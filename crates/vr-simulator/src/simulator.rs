use rand_chacha::ChaCha8Rng;
use rand_chacha::rand_core::SeedableRng;
use std::collections::BTreeMap;
use tracing::{debug, info, trace};

use vr_replica::effect::Effect;
use vr_replica::message::{ClientRequest, Message};
use vr_replica::replica::{Replica, Status};

use crate::client::{Client, Op};
use crate::history::{History, RuntimeEvents};
use crate::invariants::{InvariantViolation, StateChecker};
use crate::network::{Network, NetworkSendOutcome};
use crate::types::{Clients, NodeId, NodeKind, Replicas};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum EventClass {
    Fault,
    Delivery,
    Timer,
    ClientRequest,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SimulatorRunOutcome {
    Quiesced,
    TimeLimit,
    EventLimit,
}

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
        request_number: usize,
        op: Input,
    },
    HeartbeatTick {
        node: NodeId,
    },
}

impl<Input> WheelEvent<Input> {
    fn class(&self) -> EventClass {
        match self {
            WheelEvent::ClientRequest { .. } => EventClass::ClientRequest,
            WheelEvent::Deliver { .. } => EventClass::Delivery,
            Self::HeartbeatTick { .. } => EventClass::Timer,
        }
    }
}

#[derive(Debug)]
pub struct SimulatorConfig {
    pub disable_timers: bool,
    pub run_until_max_time: u64,
    pub run_until_max_events: u64,
    pub heartbeat_interval: u64,
}

impl Default for SimulatorConfig {
    fn default() -> Self {
        Self {
            disable_timers: false,
            run_until_max_time: 60_000,
            run_until_max_events: 50_000,
            heartbeat_interval: 100,
        }
    }
}

/// This is the key that maps each event on wheel. It composes by:
///
/// - Time
/// - EventClass
/// - Monotonic ID
type EventKey = (u64, EventClass, u64);

#[derive(Debug)]
pub struct Simulator<Input: Clone + std::fmt::Debug + 'static> {
    config: SimulatorConfig,
    seed: u64,
    pub now: u64,
    pub rng: ChaCha8Rng,
    network: Network,
    pub history: History<Input, Op>,
    checker: StateChecker,

    replicas: Replicas<Input, Op>,
    clients: Clients,

    wheel: BTreeMap<EventKey, WheelEvent<Input>>,
    next_event_sequence: u64,
    events_processed: u64,
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
            network: Network::new(),
            history: History::new(),
            replicas: BTreeMap::new(),
            clients: BTreeMap::new(),
            config: config.unwrap_or_default(),
            checker: StateChecker::new(),

            wheel: BTreeMap::new(),
            events_processed: 0,
            next_event_sequence: 0,
        }
    }

    pub fn run(&mut self) -> Result<SimulatorRunOutcome, InvariantViolation> {
        info!(seed = self.seed, "starting running simulation");

        loop {
            let Some(&(next_at, _, _)) = self.wheel.keys().next() else {
                return Ok(SimulatorRunOutcome::Quiesced);
            };

            if self.events_processed >= self.config.run_until_max_events {
                return Ok(SimulatorRunOutcome::EventLimit);
            }

            if next_at > self.config.run_until_max_time {
                return Ok(SimulatorRunOutcome::TimeLimit);
            }

            self.step()?;
        }
    }

    fn step(&mut self) -> Result<(), InvariantViolation> {
        let key = *self
            .wheel
            .keys()
            .next()
            .expect("step requires pending events");

        let event = self.wheel.remove(&key).expect("event key came from wheel");

        let (at, _, _) = key;
        self.now = at;

        self.events_processed = self
            .events_processed
            .checked_add(1)
            .expect("processed event count overflow");

        self.dispatch(event)
    }

    fn dispatch(&mut self, event: WheelEvent<Op>) -> Result<(), InvariantViolation> {
        match event {
            WheelEvent::ClientRequest {
                client_id,
                request_number,
                op,
            } => self.client_request(client_id, request_number, op),
            WheelEvent::Deliver { from, to, message } => self.deliver(from, to, message)?,
            WheelEvent::HeartbeatTick { node } => self.heartbeat_tick(node),
        }

        Ok(())
    }

    pub fn get_clients(&self) -> Vec<Client> {
        self.clients.values().cloned().collect()
    }

    pub fn start_client_request(&mut self, client_id: NodeId, op: Op) -> bool {
        let Some(client) = self.clients.get_mut(&client_id) else {
            return false;
        };

        if client.has_pending_request() {
            return false;
        }

        let request_number = client.lock_request_number();
        self.schedule_event(
            self.now,
            WheelEvent::ClientRequest {
                client_id,
                request_number,
                op,
            },
        );

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

    pub fn create_network_perfect_mesh(&mut self) {
        let network = Network::full_mesh_perfect(self.replicas.clone(), self.clients.clone());
        self.network = network;
    }

    fn deliver(
        &mut self,
        from: NodeKind,
        to: NodeKind,
        message: Message<Op, Op>,
    ) -> Result<(), InvariantViolation> {
        debug!(now = self.now, ?from, ?to, ?message, "delivering message");

        // The send-time NetworkRequest records the *decision* to deliver (with
        // its future `at`); this records the message actually arriving, so a
        // trace shows both ends of every hop. A dropped message has no
        // NetworkDelivered; a duplicated one has two.
        self.history.insert_history_event(
            self.now,
            RuntimeEvents::NetworkDelivered {
                from,
                to,
                message: message.clone(),
            },
        );

        match to {
            NodeKind::Replica(id) => {
                let (effects, snapshot) = {
                    let Some(replica) = self.replicas.get_mut(&id) else {
                        debug!(?id, "message to unknown replica dropped");
                        return Err(InvariantViolation {
                            invariant: "unknown_replica",
                            // TODO: Fix this to follow NodeId pattern too.
                            replica: id.0,
                            details: String::from("Attempt to sent to unnown replica ID"),
                        });
                    };

                    let effects = replica.on_message(message);
                    let snapshot = replica.snapshot();

                    (effects, snapshot)
                };

                self.checker.observe_snapshot(snapshot.clone());
                self.history
                    .insert_history_event(self.now, RuntimeEvents::ReplicaSnapshot { snapshot });

                self.apply_effects(to, effects)?;
            }
            NodeKind::Client(id) => {
                let Some(client) = self.clients.get_mut(&id) else {
                    debug!(?id, "message to unknown client dropped");
                    return Err(InvariantViolation {
                        invariant: "unknown_replica",
                        // TODO: Fix this to follow NodeId pattern too.
                        replica: id.0,
                        details: String::from("Attempt to sent to unnown replica ID"),
                    });
                };
                client.on_message(message);
            }
        }

        Ok(())
    }

    fn client_request(&mut self, client_id: NodeId, request_number: usize, op: Op) {
        let Some(client) = self.clients.get_mut(&client_id) else {
            debug!(?client_id, "request for unknown client dropped");
            return;
        };

        let request = ClientRequest {
            op,
            client_id: client.id.0,
            request_number,
            result: None,
        };

        let primary = client.believed_primary();
        self.send(
            NodeKind::Client(client_id),
            NodeKind::Replica(NodeId(primary)),
            Message::Request(request),
        );
    }

    fn apply_effects(
        &mut self,
        from: NodeKind,
        effects: Vec<Effect<Op, Op>>,
    ) -> Result<(), InvariantViolation> {
        for effect in effects {
            match effect {
                Effect::Send { to, message } => {
                    self.send(from, NodeKind::Replica(NodeId(to)), message)
                }
                Effect::Reply { client_id, message } => {
                    if let (
                        NodeKind::Replica(replica_id),
                        Message::Reply {
                            client_id,
                            request_id,
                            ..
                        },
                    ) = (from, &message)
                    {
                        self.checker.invariant_reply_implies_committed(
                            replica_id.0,
                            *client_id,
                            *request_id,
                        )?;
                    }

                    self.send(from, NodeKind::Client(NodeId(client_id)), message);
                }
                // Lifecycle effects become history records in §10 Phase A;
                // until the recorder exists they are trace-only.
                Effect::Committed { replica, .. } | Effect::Prepared { replica, .. } => {
                    trace!(at = self.now, replica = replica, "lifecycle events");
                    self.history.insert_history_event(
                        self.now,
                        RuntimeEvents::ReplicaOperation { replica, effect },
                    );
                }
            }
        }

        Ok(())
    }

    fn send(&mut self, from: NodeKind, to: NodeKind, message: Message<Op, Op>) {
        match self.network.resolve_send(from, to, self.now, &mut self.rng) {
            NetworkSendOutcome::Dropped => {
                trace!(now = self.now, ?from, ?to, ?message, "message dropped");
                self.history.insert_history_event(
                    self.now,
                    RuntimeEvents::NetworkRequest {
                        from,
                        to,
                        outcome: NetworkSendOutcome::Dropped,
                        message,
                    },
                );
            }
            NetworkSendOutcome::Delivered { at } => {
                self.history.insert_history_event(
                    self.now,
                    RuntimeEvents::NetworkRequest {
                        from,
                        to,
                        outcome: NetworkSendOutcome::Delivered { at },
                        message: message.clone(),
                    },
                );
                self.schedule_event(at, WheelEvent::Deliver { from, to, message });
            }
            NetworkSendOutcome::Duplicated { at, duplicated_at } => {
                trace!(now = self.now, ?from, ?to, ?message, "message duplicated");

                self.history.insert_history_event(
                    self.now,
                    RuntimeEvents::NetworkRequest {
                        from,
                        to,
                        outcome: NetworkSendOutcome::Duplicated { at, duplicated_at },
                        message: message.clone(),
                    },
                );

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
        assert!(
            at >= self.now,
            "cannot schedule event at {at} before now {}",
            self.now,
        );

        let class = event.class();
        let sequence = self.next_event_sequence;

        self.next_event_sequence = self
            .next_event_sequence
            .checked_add(1)
            .expect("event sequence overflow");

        let replaced = self.wheel.insert((at, class, sequence), event);

        debug_assert!(replaced.is_none())
    }

    fn heartbeat_tick(&mut self, node: NodeId) {
        if self.config.disable_timers {
            return;
        }

        let Some(replica) = self.replicas.get(&node) else {
            debug!(?node, "heartbeat for unknown replica ignored");
            return;
        };

        let snapshot = replica.snapshot();
        if snapshot.status != Status::Normal {
            return;
        }

        let replicas: Vec<NodeId> = self.replicas.keys().copied().collect();
        if replicas.is_empty() {
            return;
        }

        let primary_index = (snapshot.view_number % replicas.len() as u64) as usize;
        let primary = replicas[primary_index];

        // This may be an old timer belonging to a former primary.
        if node != primary {
            return;
        }

        let commit = Message::Commit {
            view_number: snapshot.view_number,
            commit_number: snapshot.commit_number,
        };

        for backup in replicas.into_iter().filter(|replica| *replica != node) {
            self.send(
                NodeKind::Replica(node),
                NodeKind::Replica(backup),
                commit.clone(),
            );
        }

        let next_at = self
            .now
            .checked_add(self.config.heartbeat_interval)
            .expect("virtual time overflow while scheduling heartbeat");
        self.schedule_event(next_at, WheelEvent::HeartbeatTick { node });
    }

    pub fn start_timers(&mut self) {
        if self.config.disable_timers {
            return;
        }

        assert!(
            self.config.heartbeat_interval > 0,
            "heartbeat interval must be greater than zero"
        );

        let replicas: Vec<NodeId> = self.replicas.keys().copied().collect();

        if replicas.is_empty() {
            return;
        }

        let primary = replicas[0];

        let at = self
            .now
            .checked_add(self.config.heartbeat_interval)
            .expect("virtual time overflow while scheduling initial heartbeat");

        self.schedule_event(at, WheelEvent::HeartbeatTick { node: primary });
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

        fn apply(&mut self, input: Self::Input) -> Self::Output {
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

    fn setup(seed: u64, replicas: u64, config: Option<SimulatorConfig>) -> Simulator<Op> {
        let mut sim = Simulator::with_seed(seed, config);
        let configuration: Vec<u64> = (0..replicas).collect();
        for id in 0..replicas {
            let sm = Rc::new(RefCell::new(KvState::default()));
            sim.add_replica(NodeId(id), Replica::new(configuration.clone(), id, sm));
        }
        sim.add_client(NodeId(0), Client::new(NodeId(0), configuration));
        sim
    }

    #[test]
    fn hit_event_limit_with_max_events_zero() {
        let config = SimulatorConfig {
            run_until_max_events: 0,
            ..Default::default()
        };
        let mut s = setup(42, 3, Some(config));
        s.create_network_mesh();
        s.start_client_request(NodeId(0), Op::Set("k".into(), 7));
        let output = s.run().unwrap();

        assert_eq!(output, SimulatorRunOutcome::EventLimit)
    }

    #[test]
    fn event_limit_takes_priority_over_time_limit() {
        let config = SimulatorConfig {
            run_until_max_time: 0,
            run_until_max_events: 0,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.schedule_event(1, WheelEvent::HeartbeatTick { node: NodeId(0) });

        let outcome = sim.run().unwrap();

        assert_eq!(outcome, SimulatorRunOutcome::EventLimit);
        assert_eq!(sim.events_processed, 0);
        assert_eq!(sim.now, 0);
    }

    #[test]
    fn validate_event_execute_on_max_time_limit() {
        let config = SimulatorConfig {
            run_until_max_time: 10,
            ..Default::default()
        };
        let mut s = setup(42, 3, Some(config));
        s.create_network_perfect_mesh();

        s.schedule_event(
            10,
            WheelEvent::Deliver {
                from: NodeKind::Replica(NodeId(1)),
                to: NodeKind::Replica(NodeId(0)),
                message: Message::PrepareOk {
                    view_number: 1,
                    replica_number: 1,
                    op_number: 1,
                },
            },
        );

        assert_eq!(s.now, 0);
        assert_eq!(s.events_processed, 0);
        assert_eq!(s.wheel.len(), 1);
        assert!(s.history.events().is_empty());

        let outcome = s.run().unwrap();

        assert_eq!(outcome, SimulatorRunOutcome::Quiesced);
        assert!(s.wheel.is_empty());
        assert_eq!(s.now, 10);
    }

    #[test]
    fn event_class_priority_is_stable() {
        assert!(EventClass::Fault < EventClass::Delivery);
        assert!(EventClass::Delivery < EventClass::Timer);
        assert!(EventClass::Timer < EventClass::ClientRequest);
    }

    #[test]
    fn hit_time_limit_when_max_time_reaches() {
        let config = SimulatorConfig {
            run_until_max_time: 2,
            ..Default::default()
        };
        let mut s = setup(42, 3, Some(config));
        s.create_network_mesh();
        s.start_client_request(NodeId(0), Op::Set("k".into(), 7));

        assert_eq!(s.now, 0);
        assert_eq!(s.events_processed, 0);
        assert_eq!(s.wheel.len(), 1);
        assert!(s.history.events().is_empty());

        let output = s.run().unwrap();

        assert!(s.now <= 2);
        assert_eq!(s.events_processed, 1);
        assert!(!s.wheel.is_empty());
        assert_eq!(output, SimulatorRunOutcome::TimeLimit)
    }

    #[test]
    fn delivery_precedes_client_request_at_same_time() {
        let mut sim = setup(42, 3, None);
        sim.create_network_perfect_mesh();

        assert!(sim.start_client_request(NodeId(0), Op::Set("client-request".into(), 1),));

        sim.schedule_event(
            0,
            WheelEvent::Deliver {
                from: NodeKind::Replica(NodeId(1)),
                to: NodeKind::Client(NodeId(0)),
                message: Message::Error {
                    message: "delivery".into(),
                },
            },
        );

        sim.step().unwrap();

        assert_eq!(sim.events_processed, 1);

        assert!(matches!(
            sim.history.events().first(),
            Some((
                0,
                RuntimeEvents::NetworkDelivered {
                    from: NodeKind::Replica(NodeId(1)),
                    to: NodeKind::Client(NodeId(0)),
                    ..
                }
            ))
        ));
    }

    #[test]
    fn same_time_client_requests_are_dispatched_fifo() {
        let mut sim = setup(42, 3, None);
        let configuration = vec![0, 1, 2];

        sim.add_client(NodeId(1), Client::new(NodeId(1), configuration));
        sim.create_network_perfect_mesh();

        assert!(sim.start_client_request(NodeId(0), Op::Set("first".into(), 1),));
        assert!(sim.start_client_request(NodeId(1), Op::Set("second".into(), 2),));

        sim.step().unwrap();
        sim.step().unwrap();

        let sent_keys: Vec<String> = sim
            .history
            .events()
            .iter()
            .filter_map(|(_, event)| {
                let RuntimeEvents::NetworkRequest {
                    from,
                    to,
                    message: Message::Request(request),
                    ..
                } = event
                else {
                    return None;
                };

                if !matches!(from, NodeKind::Client(_)) || *to != NodeKind::Replica(NodeId(0)) {
                    return None;
                }

                match &request.op {
                    Op::Set(key, _) => Some(key.clone()),
                    _ => None,
                }
            })
            .collect();

        assert_eq!(sent_keys, vec!["first".to_string(), "second".to_string()])
    }

    #[test]
    fn second_client_request_is_rejected_while_first_is_pending() {
        let mut sim = setup(42, 3, None);
        sim.create_network_perfect_mesh();

        assert!(sim.start_client_request(NodeId(0), Op::Set("first".into(), 1),));
        assert!(!sim.start_client_request(NodeId(0), Op::Set("second".into(), 2),));

        sim.step().unwrap();

        let sent_keys: Vec<String> = sim
            .history
            .events()
            .iter()
            .filter_map(|(_, event)| {
                let RuntimeEvents::NetworkRequest {
                    from,
                    to,
                    message: Message::Request(request),
                    ..
                } = event
                else {
                    return None;
                };

                if *from != NodeKind::Client(NodeId(0)) || *to != NodeKind::Replica(NodeId(0)) {
                    return None;
                }

                match &request.op {
                    Op::Set(key, _) => Some(key.clone()),
                    _ => None,
                }
            })
            .collect();

        assert_eq!(sent_keys, vec!["first".to_string()]);
    }

    #[test]
    fn quiescence_wins_when_last_allowed_event_drains_wheel() {
        let config = SimulatorConfig {
            run_until_max_events: 2,
            ..Default::default()
        };

        let mut sim = setup(42, 3, Some(config));

        for replica in [1, 2] {
            sim.schedule_event(
                0,
                WheelEvent::Deliver {
                    from: NodeKind::Replica(NodeId(replica)),
                    to: NodeKind::Client(NodeId(0)),
                    message: Message::Error {
                        message: format!("event from replica {replica}"),
                    },
                },
            );
        }

        let outcome = sim.run().unwrap();

        assert_eq!(sim.events_processed, 2);
        assert!(sim.wheel.is_empty());
        assert_eq!(outcome, SimulatorRunOutcome::Quiesced);
    }

    #[test]
    fn event_limit_leaves_remaining_event_pending() {
        let config = SimulatorConfig {
            run_until_max_events: 2,
            ..Default::default()
        };

        let mut sim = setup(42, 3, Some(config));

        for replica in [0, 1, 2] {
            sim.schedule_event(
                0,
                WheelEvent::Deliver {
                    from: NodeKind::Replica(NodeId(replica)),
                    to: NodeKind::Client(NodeId(0)),
                    message: Message::Error {
                        message: format!("event from replica {replica}"),
                    },
                },
            );
        }

        let outcome = sim.run().unwrap();

        assert_eq!(sim.events_processed, 2);
        assert_eq!(sim.wheel.len(), 1);
        assert_eq!(outcome, SimulatorRunOutcome::EventLimit);
    }

    #[test]
    fn enforce_hard_stop_after_hitting_max_event() {
        let config = SimulatorConfig {
            run_until_max_events: 5,
            ..Default::default()
        };
        let mut s = setup(42, 3, Some(config));
        s.create_network_perfect_mesh();
        s.start_client_request(NodeId(0), Op::Set("k".into(), 7));
        let output = s.run().unwrap();

        assert_eq!(s.events_processed, 5);
        assert!(!s.wheel.is_empty());
        assert_eq!(output, SimulatorRunOutcome::EventLimit)
    }

    #[test]
    fn same_seed_same_history() {
        let h1 = {
            let mut s = setup(42, 3, None);
            s.create_network_mesh();
            s.start_client_request(NodeId(0), Op::Set("k".into(), 7));
            s.run().unwrap();
            s.history
        };

        let h2 = {
            let mut s = setup(42, 3, None);
            s.create_network_mesh();
            s.start_client_request(NodeId(0), Op::Set("k".into(), 7));
            s.run().unwrap();
            s.history
        };

        assert_eq!(format!("{h1:?}"), format!("{h2:?}"));
    }

    #[test]
    fn records_replica_snapshot_after_delivery() {
        let mut sim = setup(42, 3, None);
        sim.create_network_perfect_mesh();
        sim.start_client_request(NodeId(0), Op::Set("k".into(), 7));
        sim.run().unwrap();

        assert!(sim.history.events().iter().any(|(_, event)| {
            matches!(
                event,
                RuntimeEvents::ReplicaSnapshot { snapshot }
                    if snapshot.replica_number == 0
                        && snapshot.op_number == 1
                        && snapshot.log.len() == 1
            )
        }));
    }

    #[test]
    fn smoke_perfect_mesh_commits_and_replies_once() {
        const SEED: u64 = 4_789_780_388_901_646_590;

        let mut sim = setup(SEED, 3, None);
        sim.create_network_perfect_mesh();
        sim.start_client_request(NodeId(0), Op::Set("k".into(), 7));
        sim.run().unwrap();

        let replica_primary = sim.replicas.get(&NodeId(0)).expect("primary should exist");
        let replica_snapshot = replica_primary.snapshot();

        assert_eq!(replica_snapshot.commit_number, 1);
        assert_eq!(replica_snapshot.op_number, 1);
        assert_eq!(replica_snapshot.log.len(), 1);

        let entry = &replica_snapshot.log[0];
        assert_eq!(entry.op_number, 1);
        assert_eq!(entry.client_id, 0);
        assert_eq!(entry.request_number, 1);

        let clients = sim.get_clients();
        assert_eq!(clients.len(), 1);

        let client = &clients[0];
        assert_eq!(client.replies_received, 1);
        assert_eq!(client.request_number, 1);
        assert_eq!(client.state.get("k"), Some(&7));
    }

    #[test]
    fn heartbeat_sends_commit_to_every_backup() {
        let config = SimulatorConfig {
            heartbeat_interval: 10,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.start_timers();

        sim.step().unwrap();

        let commits: Vec<(NodeKind, NodeKind, u64, usize)> = sim
            .history
            .events()
            .iter()
            .filter_map(|(_, event)| {
                let RuntimeEvents::NetworkRequest {
                    from,
                    to,
                    message:
                        Message::Commit {
                            view_number,
                            commit_number,
                        },
                    ..
                } = event
                else {
                    return None;
                };

                Some((*from, *to, *view_number, *commit_number))
            })
            .collect();

        assert_eq!(
            commits,
            vec![
                (
                    NodeKind::Replica(NodeId(0)),
                    NodeKind::Replica(NodeId(1)),
                    0,
                    0,
                ),
                (
                    NodeKind::Replica(NodeId(0)),
                    NodeKind::Replica(NodeId(2)),
                    0,
                    0,
                ),
            ]
        );
    }

    #[test]
    fn disabled_timers_schedule_no_heartbeat() {
        let config = SimulatorConfig {
            disable_timers: true,
            heartbeat_interval: 10,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));

        sim.start_timers();

        assert!(sim.wheel.is_empty());
    }

    #[test]
    fn heartbeat_reschedules_itself() {
        let config = SimulatorConfig {
            heartbeat_interval: 10,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.start_timers();

        sim.step().unwrap();

        assert_eq!(sim.now, 10);
        assert!(sim.wheel.iter().any(|(&(at, class, _), event)| {
            at == 20
                && class == EventClass::Timer
                && matches!(event, WheelEvent::HeartbeatTick { node: NodeId(0) })
        }));
    }

    #[test]
    fn event_limit_stops_periodic_heartbeats() {
        let config = SimulatorConfig {
            heartbeat_interval: 10,
            run_until_max_events: 3,
            ..Default::default()
        };
        let mut sim = setup(42, 1, Some(config));
        sim.create_network_perfect_mesh();
        sim.start_timers();

        let outcome = sim.run().unwrap();

        assert_eq!(outcome, SimulatorRunOutcome::EventLimit);
        assert_eq!(sim.events_processed, 3);
        assert_eq!(sim.now, 30);
        assert_eq!(sim.wheel.len(), 1);
    }

    #[test]
    fn time_limit_does_not_execute_late_heartbeat() {
        let config = SimulatorConfig {
            heartbeat_interval: 10,
            run_until_max_time: 25,
            ..Default::default()
        };
        let mut sim = setup(42, 1, Some(config));
        sim.create_network_perfect_mesh();
        sim.start_timers();

        let outcome = sim.run().unwrap();

        assert_eq!(outcome, SimulatorRunOutcome::TimeLimit);
        assert_eq!(sim.events_processed, 2);
        assert_eq!(sim.now, 20);
        assert!(sim.wheel.keys().any(|(at, _, _)| *at == 30));
    }

    #[test]
    fn heartbeat_converges_backups_to_final_commit() {
        let config = SimulatorConfig {
            heartbeat_interval: 10,
            run_until_max_time: 11,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 7)));
        sim.start_timers();

        let outcome = sim.run().unwrap();

        assert_eq!(outcome, SimulatorRunOutcome::TimeLimit);
        for replica in sim.replicas.values() {
            let snapshot = replica.snapshot();
            assert_eq!(snapshot.op_number, 1);
            assert_eq!(snapshot.commit_number, 1);
            assert_eq!(snapshot.log.len(), 1);
        }

        let clients = sim.get_clients();
        assert_eq!(clients[0].replies_received, 1);
        assert_eq!(clients[0].state.get("k"), Some(&7));
    }
}
