use rand::RngExt;
use rand_chacha::ChaCha8Rng;
use rand_chacha::rand_core::SeedableRng;
use std::collections::{BTreeMap, VecDeque};
use tracing::{debug, info};

use vr_replica::message::ClientRequest;
use vr_replica::{effect::Effect, message::Message, replica::Replica};

use crate::client::{Client, Op};
use crate::events::Event;

#[derive(Clone)]
pub struct Links(pub BTreeMap<(NodeKind, NodeKind), Link>);

#[cfg(test)]
impl std::fmt::Debug for Links {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for ((a, b), l) in &self.0 {
            writeln!(f, "{:?} -> {:?} -> {:?}\n", a, b, l)?;
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

#[derive(Debug)]
enum WheelEvent<Input> {
    Deliver(NodeKind),
    ClientThink { client_id: NodeId, op: Input },
}

#[derive(Debug, Default)]
pub struct SimulatorConfig {
    pub disable_timers: bool,
    pub run_until_max_time: Option<u64>,
}

pub struct Simulator<Input: Clone + std::fmt::Debug + 'static> {
    pub now: u64,
    rng: ChaCha8Rng,
    seed: u64,
    wheel: BTreeMap<u64, Vec<WheelEvent<Input>>>,

    replicas: BTreeMap<NodeId, Replica<Input, Op>>,
    inbox: BTreeMap<NodeKind, VecDeque<Event<Input>>>,
    links: Links,

    clients: BTreeMap<NodeId, Client>,

    config: SimulatorConfig,
}

impl<Input: Clone + std::fmt::Debug + 'static> Simulator<Input> {
    pub fn new(config: Option<SimulatorConfig>) -> Self {
        Self::with_seed(config, 0)
    }

    pub fn with_seed(config: Option<SimulatorConfig>, seed: u64) -> Self {
        let rng = ChaCha8Rng::seed_from_u64(seed);
        info!(seed = seed, "Creating simulator with seed");
        Self {
            rng,
            seed,
            now: 0,
            wheel: BTreeMap::new(),
            replicas: BTreeMap::new(),
            inbox: BTreeMap::new(),
            links: Links(BTreeMap::new()),
            clients: BTreeMap::new(),
            config: config.unwrap_or_default(),
        }
    }

    pub fn run(&mut self) {
        self.run_until(self.config.run_until_max_time.unwrap_or(u64::MAX))
    }

    pub fn run_until(&mut self, max_time: u64) {
        debug!(config = ?self.config, "running simulation",);
        while let Some((&at, _)) = self.wheel.iter().next() {
            if at > max_time {
                break;
            }
            self.step()
        }
    }

    pub fn get_clients(&self) -> Vec<Client> {
        self.clients.values().cloned().collect()
    }

    pub fn get_replicas(&self) -> Vec<&Replica<Input, Op>> {
        self.replicas.values().collect()
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
        // Only schedule timers based on replica role, and not immediately at time 0
        let is_primary = r.view_number == r.replica_number;

        self.replicas.insert(id, r);
        self.inbox.insert(NodeKind::Replica(id), VecDeque::new());

        // Schedule initial timers with some delay to avoid immediate firing
        if !self.config.disable_timers {
            // let initial_delay = 100;
            // if is_primary {
            //     self.schedule(
            //         self.now + initial_delay,
            //         WheelEvent::FireTimer {
            //             node: id,
            //             kind: TimerKind::PrimaryIdleCommit,
            //         },
            //     );
            // } else {
            //     self.schedule(
            //         self.now + initial_delay,
            //         WheelEvent::FireTimer {
            //             node: id,
            //             kind: TimerKind::BackupWatchdog,
            //         },
            //     );
            // }
            todo!("Implement the disable timers configuration");
        }
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
        let Some((&at, _)) = self.wheel.iter().next() else {
            return;
        };

        let evs = self.wheel.remove(&at).unwrap();
        self.now = at;

        debug!(now = self.now, events = ?evs, "triggering step");

        for ev in evs {
            match ev {
                WheelEvent::Deliver(to) => self.deliver_one(to),
                WheelEvent::ClientThink { client_id, op } => self.client_think(client_id, op),
            }
        }
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
                debug!(event = ?ev, destination = ?dst, "deliver to replica");
                let r = self.replicas.get_mut(&dst).unwrap();
                let mut effs = match ev {
                    Event::Msg(m) => r.on_message(m.clone()),
                };
                debug!(destination = ?dst, effects = ?effs, "received effects from replicas");
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

        let current_primary = client.configuration[client.current_view as usize];
        let replica_id = NodeId(current_primary);

        debug!(destination = ?replica_id, primary = current_primary, req = ?request, "triggering client request");

        self.send(
            NodeKind::Client(client_id),
            NodeKind::Replica(replica_id),
            request,
        );
    }

    fn apply_effects(&mut self, from: NodeId, effs: &mut Vec<Effect<Input, Op>>) {
        for eff in effs.drain(..) {
            match eff {
                Effect::Send { to, message } => {
                    let from_replica = NodeKind::Replica(from);
                    let to_replica = NodeKind::Replica(NodeId(to));
                    self.send(from_replica, to_replica, message);
                }
                Effect::Reply { client_id, message } => {
                    assert!(matches!(message, Message::Reply { .. }));
                    self.send(
                        NodeKind::Replica(from),
                        NodeKind::Client(NodeId(client_id)),
                        message,
                    );
                }
            }
        }
    }

    fn send(&mut self, from: NodeKind, to: NodeKind, m: Message<Input, Op>) {
        let (up, base_ms, jitter_ms, drop_pct, dup_pct) = match self.links.0.get(&(from, to)) {
            Some(l) => (l.up, l.base_ms, l.jitter_ms, l.drop_pct, l.dup_pct),
            None => return,
        };

        if !up {
            return;
        }

        if self.rng.random_range(0..100) < drop_pct {
            debug!(from = ?from, to = ?to, "dropped");
            return;
        }

        let jitter = if jitter_ms == 0 {
            0
        } else {
            self.rng.random_range(0..=jitter_ms)
        };

        let at = self.now + base_ms + jitter;

        self.inbox
            .get_mut(&to)
            .unwrap()
            .push_back(Event::Msg(m.clone()));

        debug!(at = at, from = ?from, to = ?to, msg = ?m, "sending message");

        self.schedule(at, WheelEvent::Deliver(to));
        if self.rng.random_range(0..100) < dup_pct {
            let dup_jitter = self.rng.random_range(0..=jitter_ms.max(1));
            let at = self.now + base_ms + dup_jitter;
            debug!(at = at, from = ?from, to = ?to, "duplicated message");
            self.schedule(at, WheelEvent::Deliver(to))
        }
    }
}
