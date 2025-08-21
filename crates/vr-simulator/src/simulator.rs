use std::collections::{BTreeMap, HashMap, VecDeque};

use vr_replica::{clock::TimerKind, effect::Effect, message::Message, replica::Replica, state_machine::StateMachine};

use crate::{client::Client, events::Event};

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

enum WheelEvent {
    Deliver(NodeId),
    FireTimer { node: NodeId, kind: TimerKind },
    // ClientThink
}

// TODO: Add RNG
pub struct Simulator<SM: StateMachine> {
    pub now: u64,
    wheel: BTreeMap<u64, Vec<WheelEvent>>,

    replicas: HashMap<NodeId, Replica<SM>>,
    inbox: HashMap<NodeId, VecDeque<Event<SM>>>,
    links: Links,

    clients: HashMap<NodeId, Client<SM::State>>,
    client_inbox: HashMap<NodeId, VecDeque<SM::Input>>,
}

impl <SM: StateMachine> Simulator<SM> {
    pub fn new() -> Self {
        Self {
            now: 0,
            wheel: BTreeMap::new(),
            replicas: HashMap::new(),
            inbox: HashMap::new(),
            links: Links(HashMap::new()),
            clients: HashMap::new(),
            client_inbox: HashMap::new(),
        }
    }

    pub fn get_clients(&self) -> Vec<Client<SM::State>> {
        self.clients.values().cloned().collect()
    }

    pub fn get_replicas(&self) -> Vec<Replica<SM>> {
        self.replicas.values().cloned().collect()
    }

    pub fn get_links(&self) -> Links {
        self.links.clone()
    }

    pub fn add_replica(&mut self, id: NodeId, r: Replica<SM>) {
        self.replicas.insert(id, r);
        self.inbox.insert(id, VecDeque::new());
        self.schedule(self.now, WheelEvent::FireTimer { node: id, kind: TimerKind::PrimaryIdleCommit });
        self.schedule(self.now, WheelEvent::FireTimer { node: id, kind: TimerKind::BackupWatchdog });
    }

    pub fn add_client(&mut self, id: NodeId, c: Client<SM::State>) {
        self.clients.insert(id, c);
        self.client_inbox.insert(id, VecDeque::new());
    }

    pub fn set_link(&mut self, src: NodeKind, dst: NodeKind, link: Link) {
        self.links.0.insert((src, dst), link.clone());
        self.links.0.insert((dst, src), link.clone());
    }

    pub fn run(&mut self) {
        while let Some((&t, _)) = self.wheel.iter().next() {
            let evs = self.wheel.remove(&t).unwrap();
            self.now = t;
            for ev in evs {
                match ev {
                    WheelEvent::Deliver(to) => self.deliver_one(to),
                    WheelEvent::FireTimer { node, kind } => self.fire_timer(node, kind),
                }
            }
        }
    }

    fn schedule(&mut self, at: u64, event: WheelEvent) {
        self.wheel.entry(at).or_default().push(event);
    }

    fn deliver_one(&mut self, dst: NodeId) {
        if let Some(q) = self.inbox.get_mut(&dst) {
            if let Some(ev) = q.pop_front() {
                let r = self.replicas.get_mut(&dst).unwrap();
                let mut effs = match ev {
                    Event::Msg(m) => r.on_message(m, self.now),
                    Event::TimerFired(_) => r.tick(self.now),
                };
                self.apply_effects(dst, &mut effs);
            }
        }
    }

    fn fire_timer(&mut self, node: NodeId, kind: TimerKind) {
        // feed a timer-firing via the inbox so Replica::tick runs
        self.inbox.get_mut(&node).unwrap().push_back(Event::TimerFired(kind));
        self.schedule(self.now, WheelEvent::Deliver(node));
    }

    fn apply_effects(&mut self, from: NodeId, effs: &mut Vec<Effect<SM>>) {
        for eff in effs.drain(..) {
            match eff {
                Effect::Send { to, message } => self.send(from, NodeId(to), message),
                _ => todo!()
            }
        }
    }

    fn send(&mut self, from: NodeId, to: NodeId, m: Message<SM>) {
        let Some(l) = self.get_link_with_node_id(&from, &to) else {
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

    fn get_link_with_node_id(&self, src: &NodeId, dst: &NodeId) -> Option<&Link> {
        for ((a, b), l) in &self.links.0 {
            match (a, b) {
                (NodeKind::Replica(a), NodeKind::Replica(b)) if a == src && b == dst => return Some(l),
                (NodeKind::Client(a), NodeKind::Replica(b)) if a == src && b == dst => return Some(l),
                (NodeKind::Replica(a), NodeKind::Client(b)) if a == src && b == dst => return Some(l),
                (NodeKind::Client(a), NodeKind::Client(b)) if a == src && b == dst => return Some(l),
                _ => continue,
            }
        }
        None
    }
}
