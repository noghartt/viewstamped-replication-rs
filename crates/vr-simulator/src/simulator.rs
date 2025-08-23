use std::collections::{BTreeMap, HashMap, VecDeque};

use vr_replica::message::ClientRequest;
use vr_replica::{clock::TimerKind, effect::Effect, message::Message, replica::Replica};

use crate::client::{Client, Op};
use crate::events::Event;

#[derive(Clone)]
pub struct Links(pub HashMap<(NodeKind, NodeKind), Link>);

#[cfg(test)]
impl std::fmt::Debug for Links {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for ((a, b), l) in &self.0 {
            write!(f, "{:?} -> {:?} -> {:?}\n", a, b, l)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum NodeKind {
    Client(NodeId),
    Replica(NodeId),
}

#[derive(Debug, Clone)]
pub struct Link {
    pub up: bool,
    pub base_ms: u64,
    pub jitter_ms: u64,
    pub drop_pct: u8,
    pub dup_pct: u8,
}

enum WheelEvent<Input> {
    Deliver(NodeKind),
    FireTimer { node: NodeId, kind: TimerKind },
    ClientThink { client_id: NodeId, op: Input },
}

// TODO: Add RNG
pub struct Simulator<Input: Clone + 'static> {
    pub now: u64,
    wheel: BTreeMap<u64, Vec<WheelEvent<Input>>>,

    replicas: HashMap<NodeId, Replica<Input, Op>>,
    inbox: HashMap<NodeKind, VecDeque<Event<Input>>>,
    links: Links,

    clients: HashMap<NodeId, Client>,
}

impl <Input: Clone + 'static> Simulator<Input> {
    pub fn new() -> Self {
        Self {
            now: 0,
            wheel: BTreeMap::new(),
            replicas: HashMap::new(),
            inbox: HashMap::new(),
            links: Links(HashMap::new()),
            clients: HashMap::new(),
        }
    }

    pub fn get_clients(&self) -> Vec<Client> {
        self.clients.values().cloned().collect()
    }

    pub fn get_replicas(&self) -> Vec<&Replica<Input, Op>> {
        self.replicas.values().map(|r| r).collect()
    }

    pub fn get_links(&self) -> Links {
        self.links.clone()
    }

    pub fn start_client_request(&mut self, client_id: NodeId, op: Input) -> bool {
        if self.clients.get_mut(&client_id).is_none() {
            return false;
        };

        self.schedule(self.now, WheelEvent::ClientThink { client_id, op });
        
        true
    }

    pub fn add_replica(&mut self, id: NodeId, r: Replica<Input, Op>) {
        self.replicas.insert(id, r);
        self.inbox.insert(NodeKind::Replica(id), VecDeque::new());
        self.schedule(self.now, WheelEvent::FireTimer { node: id, kind: TimerKind::PrimaryIdleCommit });
        self.schedule(self.now, WheelEvent::FireTimer { node: id, kind: TimerKind::BackupWatchdog });
    }

    pub fn add_client(&mut self, id: NodeId, c: Client) {
        self.clients.insert(id, c);
        self.inbox.insert(NodeKind::Client(id), VecDeque::new());
    }

    pub fn set_link(&mut self, src: NodeKind, dst: NodeKind, link: Link) {
        self.links.0.insert((src, dst), link.clone());
        self.links.0.insert((dst, src), link.clone());
    }

    pub fn step(&mut self) {
        let Some((&at, evs)) = self.wheel.iter().next() else {
            return;
        };

        let evs = self.wheel.remove(&at).unwrap();
        self.now = at;

        for ev in evs {
            match ev {
                WheelEvent::Deliver(to) => self.deliver_one(to),
                WheelEvent::FireTimer { node, kind } => self.fire_timer(NodeKind::Replica(node), kind),
                WheelEvent::ClientThink { client_id, op } => self.client_think(client_id, op),
            }
        }
    }

    pub fn run(&mut self) {
        while let Some((&_, _)) = self.wheel.iter().next() {
            self.step()
        }
    }

    fn do_step(&mut self, at: u64) {
    }

    fn schedule(&mut self, at: u64, event: WheelEvent<Input>) {
        self.wheel.entry(at).or_default().push(event);
    }

    fn deliver_one(&mut self, dst: NodeKind) {
        match dst {
            NodeKind::Replica(id) => self.deliver_to_replica(id),
            NodeKind::Client(id) => self.deliver_to_client(id),
        }
    }

    fn deliver_to_replica(&mut self, dst: NodeId) {
        if let Some(q) = self.inbox.get_mut(&NodeKind::Replica(dst)) {
            if let Some(ev) = q.pop_front() {
                let r = self.replicas.get_mut(&dst).unwrap();
                let mut effs = match ev {
                    Event::Msg(m) => r.on_message(m.clone(), self.now),
                    Event::TimerFired(_) => r.tick(self.now),
                };
                self.apply_effects(dst, &mut effs);
            }
        }
    }

    fn deliver_to_client(&mut self, dst: NodeId) {
        if let Some(q) = self.inbox.get_mut(&NodeKind::Client(dst)) {
            if let Some(ev) = q.pop_front() {
                let c = self.clients.get_mut(&dst).unwrap();
                c.on_message(ev);
            }
        }
    }

    fn fire_timer(&mut self, node: NodeKind, kind: TimerKind) {
        // feed a timer-firing via the inbox so Replica::tick runs
        self.inbox.get_mut(&node).unwrap().push_back(Event::TimerFired(kind));
        self.schedule(self.now, WheelEvent::Deliver(node.clone()));
    }

    fn client_think(&mut self, client_id: NodeId, op: Input) {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return;
        };

        let request = Message::Request::<Input, Op>(ClientRequest {
            client_id: client_id.0,
            op,
            request_number: 0,
            result: None,
        });
        
        let replica_id = NodeId(client.current_view);
        self.send(NodeKind::Client(client_id), NodeKind::Replica(replica_id), request);
    }

    fn apply_effects(&mut self, from: NodeId, effs: &mut Vec<Effect<Input, Op>>) {
        for eff in effs.drain(..) {
            match eff {
                Effect::Send { to, message } => {
                    let from_replica = NodeKind::Replica(from);
                    let to_replica = NodeKind::Replica(NodeId(to));
                    self.send(from_replica, to_replica, message);
                },
                Effect::Reply { client_id, message } => {
                    assert!(matches!(message, Message::Reply { .. }));
                    self.send(NodeKind::Replica(from), NodeKind::Client(NodeId(client_id)), message);
                },
                _ => todo!()
            }
        }
    }

    fn send(&mut self, from: NodeKind, to: NodeKind, m: Message<Input, Op>) {
        let Some(l) = self.links.0.get(&(from, to)) else {
            return;
        };

        if !l.up {
            return;
        }

        let base_ms = l.base_ms;
        // TODO: Add jitter, drop, and RNG
        self.inbox.get_mut(&to).unwrap().push_back(Event::Msg(m.clone()));
        let at = self.now + base_ms;
        self.schedule(at, WheelEvent::Deliver(to));
    }
}
