use rand_chacha::ChaCha8Rng;
use rand_chacha::rand_core::SeedableRng;
use std::collections::BTreeMap;
use tracing::{debug, info, trace};

use vr_replica::effect::Effect;
use vr_replica::message::{ClientRequest, Message};
use vr_replica::replica::Replica;

use crate::client::{Client, Op};
use crate::network::{Network, NetworkSendOutcome};
use crate::types::{Clients, NodeId, NodeKind, Replicas};

/// The message rides IN the event: duplicating a message means scheduling the
/// same cloned event twice, which is correct by construction. (The previous
/// design — a Deliver token draining a shared inbox — made "duplicate" deliver
/// two different messages.)
#[derive(Debug)]
enum WheelEvent<Input> {
    Deliver {
        from: NodeKind,
        to: NodeKind,
        message: Message<Input, Op>,
    },
    ClientRequest {
        client_id: NodeId,
        op: Input,
    },
}

#[derive(Debug, Default)]
pub struct SimulatorConfig {
    pub disable_timers: bool,
    pub run_until_max_time: Option<u64>,
}

#[derive(Debug)]
pub struct Simulator<Input: Clone + std::fmt::Debug + 'static> {
    config: SimulatorConfig,
    seed: u64,
    pub now: u64,
    rng: ChaCha8Rng,
    wheel: BTreeMap<u64, Vec<WheelEvent<Input>>>,
    network: Network,

    replicas: Replicas<Input, Op>,
    clients: Clients,
}

impl Simulator<Op> {
    pub fn new(config: Option<SimulatorConfig>) -> Self {
        Self::with_seed(0, config)
    }

    pub fn with_seed(seed: u64, config: Option<SimulatorConfig>) -> Self {
        Self {
            seed,
            rng: ChaCha8Rng::seed_from_u64(seed),
            now: 0,
            wheel: BTreeMap::new(),
            network: Network::new(),
            replicas: BTreeMap::new(),
            clients: BTreeMap::new(),
            config: config.unwrap_or_default(),
        }
    }

    pub fn run(&mut self) {
        info!(seed = self.seed, "starting running simulation");
        // TODO: Handle scenarios like tick/heartbeart to avoid infinite loops for each simulation.
        while !self.wheel.is_empty()
            && self
                .config
                .run_until_max_time
                .is_none_or(|max| self.now < max)
        {
            self.step()
        }
    }

    pub fn get_clients(&self) -> Vec<Client> {
        self.clients.values().cloned().collect()
    }

    pub fn start_client_request(&mut self, client_id: NodeId, op: Op) -> bool {
        if !self.clients.contains_key(&client_id) {
            return false;
        }

        self.schedule_event(self.now, WheelEvent::ClientRequest { client_id, op });

        true
    }

    pub fn add_replica(&mut self, id: NodeId, r: Replica<Op, Op>) {
        self.replicas.insert(id, r);
    }

    pub fn add_client(&mut self, id: NodeId, c: Client) {
        self.clients.insert(id, c);
    }

    pub fn create_network_mesh(&mut self) {
        let network = Network::full_mesh::<Op, Op>(
            &mut self.rng,
            self.replicas.clone(),
            self.clients.clone(),
        );

        self.network = network;
    }

    pub fn step(&mut self) {
        let Some((&at, _)) = self.wheel.iter().next() else {
            return;
        };

        // Time only moves forward: an event scheduled in the past is a
        // harness bug (not a protocol bug), so it panics rather than drops.
        assert!(
            at >= self.now,
            "wheel event at {at} is before now {}",
            self.now
        );

        let evs = self.wheel.remove(&at).unwrap();
        self.now = at;

        debug!(now = self.now, events = ?evs, "triggering step");

        for ev in evs {
            match ev {
                WheelEvent::Deliver { from, to, message } => self.deliver(from, to, message),
                WheelEvent::ClientRequest { client_id, op } => self.client_request(client_id, op),
            }
        }
    }

    fn deliver(&mut self, from: NodeKind, to: NodeKind, message: Message<Op, Op>) {
        debug!(now = self.now, ?from, ?to, ?message, "delivering message");
        match to {
            NodeKind::Replica(id) => {
                let Some(replica) = self.replicas.get_mut(&id) else {
                    debug!(?id, "message to unknown replica dropped");
                    return;
                };
                let effects = replica.on_message(message);
                self.apply_effects(to, effects);
            }
            NodeKind::Client(id) => {
                let Some(client) = self.clients.get_mut(&id) else {
                    debug!(?id, "message to unknown client dropped");
                    return;
                };
                client.on_message(message);
            }
        }
    }

    fn client_request(&mut self, client_id: NodeId, op: Op) {
        let Some(client) = self.clients.get_mut(&client_id) else {
            debug!(?client_id, "request for unknown client dropped");
            return;
        };

        let request = ClientRequest {
            op,
            client_id: client.id.0,
            request_number: client.request_number as usize,
            result: None,
        };

        let primary = client.believed_primary();
        self.send(
            NodeKind::Client(client_id),
            NodeKind::Replica(NodeId(primary)),
            Message::Request(request),
        );
    }

    fn apply_effects(&mut self, from: NodeKind, effects: Vec<Effect<Op, Op>>) {
        for effect in effects {
            match effect {
                Effect::Send { to, message } => {
                    self.send(from, NodeKind::Replica(NodeId(to)), message)
                }
                Effect::Reply { client_id, message } => {
                    self.send(from, NodeKind::Client(NodeId(client_id)), message)
                }
                // Lifecycle effects become history records in §10 Phase A;
                // until the recorder exists they are trace-only.
                Effect::RequestReceived { .. }
                | Effect::Prepared { .. }
                | Effect::Committed { .. } => {
                    trace!(now = self.now, ?from, ?effect, "lifecycle effect");
                }
            }
        }
    }

    fn send(&mut self, from: NodeKind, to: NodeKind, message: Message<Op, Op>) {
        match self.network.resolve_send(from, to, self.now, &mut self.rng) {
            NetworkSendOutcome::Dropped => {
                trace!(now = self.now, ?from, ?to, ?message, "message dropped");
            }
            NetworkSendOutcome::Delivered { at } => {
                self.schedule_event(at, WheelEvent::Deliver { from, to, message });
            }
            NetworkSendOutcome::Duplicated { at, duplicated_at } => {
                trace!(now = self.now, ?from, ?to, ?message, "message duplicated");
                self.schedule_event(
                    at,
                    WheelEvent::Deliver {
                        from,
                        to,
                        message: message.clone(),
                    },
                );
                self.schedule_event(duplicated_at, WheelEvent::Deliver { from, to, message });
            }
        }
    }

    fn schedule_event(&mut self, at: u64, event: WheelEvent<Op>) {
        self.wheel.entry(at).or_default().push(event);
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use vr_replica::state_machine::StateMachine;

    use super::*;

    #[derive(Debug, Default)]
    struct KvState {
        state: BTreeMap<String, u64>,
    }

    impl StateMachine for KvState {
        type Input = Op;
        type Output = Op;

        fn apply(&mut self, input: Op) -> Op {
            match input {
                Op::Set(key, value) => {
                    self.state.insert(key.clone(), value);
                    Op::Set(key, value)
                }
                Op::Get(key, _) => {
                    let value = self.state.get(&key).cloned();
                    Op::Get(key, value)
                }
                Op::Del(key) => {
                    self.state.remove(&key);
                    Op::Del(key)
                }
            }
        }
    }

    fn setup(seed: u64, replicas: u64) -> Simulator<Op> {
        let mut sim = Simulator::with_seed(seed, None);
        let configuration: Vec<u64> = (0..replicas).collect();
        for id in 0..replicas {
            let sm = Rc::new(RefCell::new(KvState::default()));
            sim.add_replica(NodeId(id), Replica::new(configuration.clone(), id, sm));
        }
        sim.add_client(NodeId(0), Client::new(NodeId(0), configuration));
        sim
    }

    #[test]
    fn smoke_one_request_one_reply() {
        let mut sim = setup(42, 3);
        assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 7)));
        sim.run();

        let client = &sim.get_clients()[0];
        assert_eq!(client.replies_received, 1, "client state: {:?}", client);
        assert_eq!(client.state.get("k"), Some(&7));
    }
}
