use rand_chacha::ChaCha8Rng;
use rand_chacha::rand_core::SeedableRng;
use std::collections::{BTreeMap, BTreeSet};
use tracing::{debug, info, trace};

use vr_replica::effect::Effect;
use vr_replica::message::{ClientRequest, Message};
use vr_replica::replica::{Replica, Status};
use vr_replica::transition::{MessageSender, ProtocolObservation, Transition};

use crate::client::{Client, Op, PendingRequest};
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
    ClientRetryTick {
        client_id: NodeId,
        request_number: usize,
        generation: u64,
    },
    WatchdogTick {
        node: NodeId,
        generation: u64,
    },
}

impl<Input> WheelEvent<Input> {
    fn class(&self) -> EventClass {
        match self {
            Self::ClientRequest { .. } => EventClass::ClientRequest,
            Self::Deliver { .. } => EventClass::Delivery,
            Self::HeartbeatTick { .. } => EventClass::Timer,
            Self::ClientRetryTick { .. } => EventClass::Timer,
            Self::WatchdogTick { .. } => EventClass::Timer,
        }
    }
}

#[derive(Debug)]
pub struct SimulatorConfig {
    pub disable_timers: bool,
    pub run_until_max_time: u64,
    pub run_until_max_events: u64,
    pub heartbeat_interval: u64,
    pub client_retry_interval: u64,
    pub watchdog_interval: u64,
}

impl Default for SimulatorConfig {
    fn default() -> Self {
        Self {
            disable_timers: false,
            run_until_max_time: 60_000,
            run_until_max_events: 50_000,
            heartbeat_interval: 100,
            client_retry_interval: 1_000,
            watchdog_interval: 500,
        }
    }
}

/// This is the key that maps each event on wheel. It composes by:
///
/// - Time
/// - EventClass
/// - Monotonic ID
type EventKey = (u64, EventClass, u64);

#[derive(Debug, Clone, Copy)]
struct ReplicaMonitor {
    generation: u64,
    expired: bool,
}

#[derive(Debug)]
pub struct Simulator<Input: Clone + std::fmt::Debug + 'static> {
    config: SimulatorConfig,
    seed: u64,
    pub now: u64,
    pub rng: ChaCha8Rng,
    network: Network,
    pub history: History<Input, Op>,
    checker: StateChecker,
    fault_free_network: bool,

    replicas: Replicas<Input, Op>,
    clients: Clients,
    monitors: BTreeMap<NodeId, ReplicaMonitor>,

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
            monitors: BTreeMap::new(),
            config: config.unwrap_or_default(),
            checker: StateChecker::new(),
            fault_free_network: false,

            wheel: BTreeMap::new(),
            events_processed: 0,
            next_event_sequence: 0,
        }
    }

    pub fn run(&mut self) -> Result<SimulatorRunOutcome, InvariantViolation> {
        info!(seed = self.seed, "starting running simulation");

        loop {
            let Some(&(next_at, _, _)) = self.wheel.keys().next() else {
                return self.finish_run(SimulatorRunOutcome::Quiesced);
            };

            if self.events_processed >= self.config.run_until_max_events {
                return self.finish_run(SimulatorRunOutcome::EventLimit);
            }

            if next_at > self.config.run_until_max_time {
                return self.finish_run(SimulatorRunOutcome::TimeLimit);
            }

            self.step()?;
        }
    }

    fn finish_run(
        &self,
        outcome: SimulatorRunOutcome,
    ) -> Result<SimulatorRunOutcome, InvariantViolation> {
        if self.fault_free_network {
            self.check_client_progress()?;
            self.check_final_convergence()?;
        }

        Ok(outcome)
    }

    fn check_client_progress(&self) -> Result<(), InvariantViolation> {
        let mut invoked = BTreeSet::new();
        let mut completed = BTreeSet::new();

        for (_, event) in self.history.events() {
            match event {
                RuntimeEvents::ClientInvoked {
                    client,
                    request_number,
                    ..
                } => {
                    invoked.insert((*client, *request_number));
                }
                RuntimeEvents::ClientCompleted {
                    client,
                    request_number,
                    ..
                } => {
                    completed.insert((*client, *request_number));
                }
                _ => {}
            }
        }

        if let Some((client, request_number)) = invoked.difference(&completed).next() {
            return Err(InvariantViolation {
                invariant: "fault_free_progress",
                replica: client.0,
                details: format!(
                    "client={} request={} was invoked but did not complete before the run ended",
                    client.0, request_number,
                ),
            });
        }

        Ok(())
    }

    fn check_final_convergence(&self) -> Result<(), InvariantViolation> {
        let Some((baseline_id, baseline_replica)) = self.replicas.first_key_value() else {
            return Ok(());
        };
        let baseline = baseline_replica.snapshot();
        let Some(baseline_prefix) = baseline.log.get(..baseline.commit_number) else {
            return Err(InvariantViolation {
                invariant: "final_convergence",
                replica: baseline_id.0,
                details: format!(
                    "replica {} ended with commit_number={} beyond log length={}",
                    baseline_id.0,
                    baseline.commit_number,
                    baseline.log.len(),
                ),
            });
        };

        for (replica_id, replica) in self.replicas.iter().skip(1) {
            let snapshot = replica.snapshot();

            if snapshot.commit_number != baseline.commit_number {
                return Err(InvariantViolation {
                    invariant: "final_convergence",
                    replica: replica_id.0,
                    details: format!(
                        "replica {} ended at commit_number={}, but replica {} ended at commit_number={}",
                        replica_id.0, snapshot.commit_number, baseline_id.0, baseline.commit_number,
                    ),
                });
            }

            let Some(committed_prefix) = snapshot.log.get(..snapshot.commit_number) else {
                return Err(InvariantViolation {
                    invariant: "final_convergence",
                    replica: replica_id.0,
                    details: format!(
                        "replica {} ended with commit_number={} beyond log length={}",
                        replica_id.0,
                        snapshot.commit_number,
                        snapshot.log.len(),
                    ),
                });
            };

            if committed_prefix != baseline_prefix {
                return Err(InvariantViolation {
                    invariant: "final_convergence",
                    replica: replica_id.0,
                    details: format!(
                        "replica {} has a different committed prefix than replica {} at commit_number={}",
                        replica_id.0, baseline_id.0, baseline.commit_number,
                    ),
                });
            }
        }

        Ok(())
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
            } => {
                self.client_request(client_id, request_number, op);
            }
            WheelEvent::Deliver { from, to, message } => self.deliver(from, to, message)?,
            WheelEvent::HeartbeatTick { node } => self.heartbeat_tick(node)?,
            WheelEvent::ClientRetryTick {
                client_id,
                request_number,
                generation,
            } => self.client_retry_tick(client_id, request_number, generation),
            WheelEvent::WatchdogTick { node, generation } => self.watchdog_tick(node, generation),
        }

        Ok(())
    }

    pub fn get_clients(&self) -> Vec<Client> {
        self.clients.values().cloned().collect()
    }

    pub fn start_client_request(&mut self, client_id: NodeId, op: Op) -> bool {
        let Some(pending) = self
            .clients
            .get_mut(&client_id)
            .and_then(|client| client.try_begin_request(op))
        else {
            return false;
        };

        self.schedule_event(
            self.now,
            WheelEvent::ClientRequest {
                client_id,
                request_number: pending.request_number,
                op: pending.op.clone(),
            },
        );

        self.history.insert_history_event(
            self.now,
            RuntimeEvents::ClientInvoked {
                client: client_id,
                request_number: pending.request_number,
                op: pending.op.clone(),
            },
        );

        if !self.config.disable_timers {
            self.schedule_retry(client_id, &pending);
        }

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
        self.fault_free_network = false;
    }

    pub fn create_network_perfect_mesh(&mut self) {
        let network = Network::full_mesh_perfect(self.replicas.clone(), self.clients.clone());
        self.network = network;
        self.fault_free_network = true;
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
                let (transition, snapshot) = {
                    let Some(replica) = self.replicas.get_mut(&id) else {
                        debug!(?id, "message to unknown replica dropped");
                        return Err(InvariantViolation {
                            invariant: "unknown_replica",
                            // TODO: Fix this to follow NodeId pattern too.
                            replica: id.0,
                            details: String::from("Attempt to sent to unnown replica ID"),
                        });
                    };

                    let sender = match from {
                        NodeKind::Replica(id) => MessageSender::Replica(id.0),
                        NodeKind::Client(id) => MessageSender::Client(id.0),
                    };
                    let transition = replica.on_message_from(sender, message);
                    let snapshot = replica.snapshot();

                    (transition, snapshot)
                };

                self.history.insert_history_event(
                    self.now,
                    RuntimeEvents::ReplicaSnapshot {
                        snapshot: snapshot.clone(),
                    },
                );

                self.checker.observe_snapshot(snapshot)?;

                self.apply_transition(to, transition)?;
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

                if let Some((request_number, result)) = client.on_message(message) {
                    self.history.insert_history_event(
                        self.now,
                        RuntimeEvents::ClientCompleted {
                            client: client.id,
                            request_number,
                            result,
                        },
                    );
                }
            }
        }

        Ok(())
    }

    fn apply_transition(
        &mut self,
        from: NodeKind,
        transition: Transition<Op, Op>,
    ) -> Result<(), InvariantViolation> {
        let Transition {
            effects,
            observations,
        } = transition;

        for observation in observations {
            self.history
                .insert_history_event(self.now, RuntimeEvents::ProtocolObserved { observation });

            match observation {
                ProtocolObservation::PrimaryActivityAccepted { replica, .. } => {
                    self.reset_watchdog(NodeId(replica));
                }
            }
        }

        self.apply_effects(from, effects)
    }

    fn client_request(&mut self, client_id: NodeId, request_number: usize, op: Op) {
        let Some(client) = self.clients.get(&client_id) else {
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

    fn heartbeat_tick(&mut self, node: NodeId) -> Result<(), InvariantViolation> {
        if self.config.disable_timers {
            return Ok(());
        }

        let Some(replica) = self.replicas.get(&node) else {
            debug!(?node, "heartbeat for unknown replica ignored");
            return Ok(());
        };

        let snapshot = replica.snapshot();
        if snapshot.status != Status::Normal {
            return Ok(());
        }

        let replicas: Vec<NodeId> = self.replicas.keys().copied().collect();
        if replicas.is_empty() {
            return Ok(());
        }

        let primary_index = (snapshot.view_number % replicas.len() as u64) as usize;
        let primary = replicas[primary_index];

        // This may be an old timer belonging to a former primary.
        if node != primary {
            return Ok(());
        }

        let effects = replica.heartbeat_effects();
        self.apply_effects(NodeKind::Replica(node), effects)?;

        let next_at = self
            .now
            .checked_add(self.config.heartbeat_interval)
            .expect("virtual time overflow while scheduling heartbeat");
        self.schedule_event(next_at, WheelEvent::HeartbeatTick { node });

        Ok(())
    }

    pub fn start_timers(&mut self) {
        if self.config.disable_timers {
            return;
        }

        assert!(
            self.config.heartbeat_interval > 0,
            "heartbeat interval must be greater than zero"
        );
        assert!(
            self.config.watchdog_interval > 0,
            "watchdog interval must be greater than zero"
        );

        let replicas: Vec<NodeId> = self.replicas.keys().copied().collect();

        if replicas.is_empty() {
            return;
        }

        let primary = replicas[0];

        for backup in replicas.iter().copied().filter(|node| *node != primary) {
            if self.monitors.contains_key(&backup) {
                continue;
            }

            self.monitors.insert(
                backup,
                ReplicaMonitor {
                    generation: 0,
                    expired: false,
                },
            );
            self.schedule_watchdog(backup, 0);
        }

        let at = self
            .now
            .checked_add(self.config.heartbeat_interval)
            .expect("virtual time overflow while scheduling initial heartbeat");

        self.schedule_event(at, WheelEvent::HeartbeatTick { node: primary });
    }

    fn client_retry_tick(&mut self, client_id: NodeId, request_number: usize, generation: u64) {
        let Some(pending) = self
            .clients
            .get(&client_id)
            .and_then(|client| client.pending_retry(request_number, generation))
        else {
            return;
        };

        self.history.insert_history_event(
            self.now,
            RuntimeEvents::ClientRetried {
                client: client_id,
                request_number,
                generation,
            },
        );

        self.client_request(client_id, request_number, pending.op.clone());
        self.schedule_retry(client_id, &pending);
    }

    fn schedule_retry(&mut self, client_id: NodeId, pending: &PendingRequest) {
        assert!(
            self.config.client_retry_interval > 0,
            "client retry interval must be greater than zero"
        );
        let at = self
            .now
            .checked_add(self.config.client_retry_interval)
            .expect("virtual time overflow while scheduling client retry");
        self.schedule_event(
            at,
            WheelEvent::ClientRetryTick {
                client_id,
                request_number: pending.request_number,
                generation: pending.retry_generation,
            },
        );
    }

    fn reset_watchdog(&mut self, node: NodeId) {
        let Some(monitor) = self.monitors.get_mut(&node) else {
            return;
        };

        monitor.generation = monitor
            .generation
            .checked_add(1)
            .expect("watchdog generation overflow");
        monitor.expired = false;
        let generation = monitor.generation;

        self.history
            .insert_history_event(self.now, RuntimeEvents::WatchdogReset { node, generation });
        self.schedule_watchdog(node, generation);
    }

    fn schedule_watchdog(&mut self, node: NodeId, generation: u64) {
        let at = self
            .now
            .checked_add(self.config.watchdog_interval)
            .expect("virtual time overflow while scheduling watchdog");
        self.schedule_event(at, WheelEvent::WatchdogTick { node, generation });
    }

    fn watchdog_tick(&mut self, node: NodeId, generation: u64) {
        let Some(monitor) = self.monitors.get_mut(&node) else {
            return;
        };

        if monitor.generation != generation || monitor.expired {
            return;
        }

        monitor.expired = true;
        self.history.insert_history_event(
            self.now,
            RuntimeEvents::WatchdogExpired { node, generation },
        );
    }
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    use vr_replica::state_machine::StateMachine;

    use crate::network::Link;

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
        assert_eq!(s.wheel.len(), 2);
        assert!(matches!(
            s.history.events(),
            [(
                0,
                RuntimeEvents::ClientInvoked {
                    client: NodeId(0),
                    request_number: 1,
                    ..
                }
            )]
        ));

        let output = s.run().unwrap();

        assert!(s.now <= 2);
        assert_eq!(s.events_processed, 1);
        assert!(!s.wheel.is_empty());
        assert_eq!(output, SimulatorRunOutcome::TimeLimit)
    }

    #[test]
    fn time_limit_checks_fault_free_progress() {
        let config = SimulatorConfig {
            run_until_max_time: 0,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 7)));

        let violation = sim.run().unwrap_err();

        assert_eq!(violation.invariant, "fault_free_progress");
        assert_eq!(sim.events_processed, 1);
        assert!(!sim.wheel.is_empty());
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
            sim.history.events().get(1),
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
        let violation = s.run().unwrap_err();

        assert_eq!(s.events_processed, 5);
        assert!(!s.wheel.is_empty());
        assert_eq!(violation.invariant, "fault_free_progress");
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
        let config = SimulatorConfig {
            heartbeat_interval: 10,
            run_until_max_time: 11,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.start_client_request(NodeId(0), Op::Set("k".into(), 7));
        sim.start_timers();
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
    fn quiescent_fault_free_run_checks_final_convergence() {
        let mut sim = setup(42, 3, None);
        sim.create_network_perfect_mesh();
        assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 7)));

        let violation = sim.run().unwrap_err();

        assert_eq!(violation.invariant, "final_convergence");
        assert_eq!(sim.clients[&NodeId(0)].replies_received, 1);
        assert!(sim.wheel.is_empty());
    }

    #[test]
    fn smoke_perfect_mesh_commits_and_replies_once() {
        const SEED: u64 = 4_789_780_388_901_646_590;

        let config = SimulatorConfig {
            heartbeat_interval: 10,
            run_until_max_time: 11,
            ..Default::default()
        };
        let mut sim = setup(SEED, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.start_client_request(NodeId(0), Op::Set("k".into(), 7));
        sim.start_timers();

        assert_eq!(sim.run().unwrap(), SimulatorRunOutcome::TimeLimit);

        let replica_primary = sim.replicas.get(&NodeId(0)).expect("primary should exist");
        let replica_snapshot = replica_primary.snapshot();

        assert_eq!(replica_snapshot.commit_number, 1);
        assert_eq!(replica_snapshot.op_number, 1);
        assert_eq!(replica_snapshot.epoch, 0);
        assert_eq!(replica_snapshot.log.len(), 1);

        let entry = &replica_snapshot.log[0];
        assert_eq!(entry.op_number, 1);
        assert_eq!(entry.client_id, 0);
        assert_eq!(entry.request_number, 1);
        assert_eq!(entry.op, Op::Set("k".into(), 7));
        assert_eq!(replica_snapshot.executed_requests.len(), 1);
        assert_eq!(replica_snapshot.executed_requests[0].client_id, 0);
        assert_eq!(replica_snapshot.executed_requests[0].request_number, 1);
        assert_eq!(
            replica_snapshot.executed_requests[0].op,
            Op::Set("k".into(), 7)
        );
        assert_eq!(
            replica_snapshot.executed_requests[0].result,
            Op::Set("k".into(), 7)
        );
        assert_eq!(replica_snapshot.client_table.len(), 1);
        assert_eq!(replica_snapshot.client_table[0].client_id, 0);
        assert_eq!(replica_snapshot.client_table[0].request_number, 1);
        assert_eq!(
            replica_snapshot.client_table[0].result,
            Some(Op::Set("k".into(), 7))
        );

        let clients = sim.get_clients();
        assert_eq!(clients.len(), 1);

        let client = &clients[0];
        assert_eq!(client.replies_received, 1);
        assert_eq!(client.request_number, 1);
        assert_eq!(client.state.get("k"), Some(&7));

        assert!(sim.history.events().iter().any(|(_, event)| {
            matches!(
                event,
                RuntimeEvents::ClientInvoked {
                    client: NodeId(0),
                    request_number: 1,
                    op: Op::Set(key, 7),
                } if key == "k"
            )
        }));
        assert!(sim.history.events().iter().any(|(_, event)| {
            matches!(
                event,
                RuntimeEvents::ClientCompleted {
                    client: NodeId(0),
                    request_number: 1,
                    result: Op::Set(key, 7),
                } if key == "k"
            )
        }));
    }

    #[test]
    fn sequential_requests_from_one_client_preserve_client_table_and_execute_once() {
        let config = SimulatorConfig {
            heartbeat_interval: 10,
            run_until_max_time: 21,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.start_timers();
        assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 1)));

        while sim.clients[&NodeId(0)].has_pending_request() {
            sim.step().unwrap();
        }

        assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 2)));
        assert_eq!(sim.run().unwrap(), SimulatorRunOutcome::TimeLimit);

        for replica in sim.replicas.values() {
            let snapshot = replica.snapshot();
            assert_eq!(snapshot.commit_number, 2);
            assert_eq!(snapshot.executed_requests.len(), 2);
            assert_eq!(snapshot.client_table.len(), 1);
            assert_eq!(snapshot.client_table[0].request_number, 2);
            assert_eq!(
                snapshot.client_table[0].result,
                Some(Op::Set("k".into(), 2))
            );
        }

        assert_eq!(sim.clients[&NodeId(0)].replies_received, 2);
        assert_eq!(sim.clients[&NodeId(0)].state.get("k"), Some(&2));
    }

    #[test]
    fn retry_recovers_first_dropped_request_without_second_invocation() {
        let config = SimulatorConfig {
            heartbeat_interval: 10,
            client_retry_interval: 5,
            run_until_max_time: 11,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.network.set_link(
            NodeKind::Client(NodeId(0)),
            NodeKind::Replica(NodeId(0)),
            Link {
                drop_probability: 100,
                ..Default::default()
            },
        );
        sim.start_timers();
        assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 7)));

        sim.step().unwrap();
        sim.network.set_link(
            NodeKind::Client(NodeId(0)),
            NodeKind::Replica(NodeId(0)),
            Link::default(),
        );

        assert_eq!(sim.run().unwrap(), SimulatorRunOutcome::TimeLimit);

        let invocations = sim
            .history
            .events()
            .iter()
            .filter(|(_, event)| matches!(event, RuntimeEvents::ClientInvoked { .. }))
            .count();
        let retries = sim
            .history
            .events()
            .iter()
            .filter(|(_, event)| matches!(event, RuntimeEvents::ClientRetried { .. }))
            .count();
        let completions = sim
            .history
            .events()
            .iter()
            .filter(|(_, event)| matches!(event, RuntimeEvents::ClientCompleted { .. }))
            .count();

        assert_eq!(invocations, 1);
        assert_eq!(retries, 1);
        assert_eq!(completions, 1);
        for replica in sim.replicas.values() {
            assert_eq!(replica.snapshot().executed_requests.len(), 1);
        }
    }

    #[test]
    fn heartbeat_retransmission_recovers_quorum_critical_dropped_prepare() {
        let config = SimulatorConfig {
            heartbeat_interval: 5,
            client_retry_interval: 50,
            watchdog_interval: 50,
            run_until_max_time: 11,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.fault_free_network = false;
        sim.network.set_link(
            NodeKind::Replica(NodeId(0)),
            NodeKind::Replica(NodeId(1)),
            Link {
                drop_probability: 100,
                ..Default::default()
            },
        );
        sim.network.set_link(
            NodeKind::Replica(NodeId(0)),
            NodeKind::Replica(NodeId(2)),
            Link {
                drop_probability: 100,
                ..Default::default()
            },
        );
        sim.network.set_link(
            NodeKind::Replica(NodeId(2)),
            NodeKind::Replica(NodeId(0)),
            Link {
                drop_probability: 100,
                ..Default::default()
            },
        );
        sim.start_timers();
        assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 7)));

        sim.step().unwrap();
        sim.step().unwrap();
        sim.network.set_link(
            NodeKind::Replica(NodeId(0)),
            NodeKind::Replica(NodeId(1)),
            Link::default(),
        );

        assert_eq!(sim.run().unwrap(), SimulatorRunOutcome::TimeLimit);

        assert_eq!(sim.clients[&NodeId(0)].replies_received, 1);
        assert_eq!(sim.replicas[&NodeId(0)].snapshot().commit_number, 1);
        assert_eq!(sim.replicas[&NodeId(1)].snapshot().commit_number, 1);
        assert_eq!(sim.replicas[&NodeId(2)].snapshot().commit_number, 0);
        assert!(sim.history.events().iter().any(|(at, event)| {
            *at == 5
                && matches!(
                    event,
                    RuntimeEvents::NetworkRequest {
                        from: NodeKind::Replica(NodeId(0)),
                        to: NodeKind::Replica(NodeId(1)),
                        message: Message::Prepare { op_number: 1, .. },
                        ..
                    }
                )
        }));
    }

    #[test]
    fn later_prepare_triggers_state_transfer_and_backup_converges() {
        let config = SimulatorConfig {
            heartbeat_interval: 10,
            client_retry_interval: 50,
            watchdog_interval: 50,
            run_until_max_time: 11,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.fault_free_network = false;
        sim.network.set_link(
            NodeKind::Replica(NodeId(0)),
            NodeKind::Replica(NodeId(1)),
            Link {
                drop_probability: 100,
                ..Default::default()
            },
        );
        sim.start_timers();
        assert!(sim.start_client_request(NodeId(0), Op::Set("a".into(), 1)));

        while sim.clients[&NodeId(0)].has_pending_request() {
            sim.step().unwrap();
        }

        sim.network.set_link(
            NodeKind::Replica(NodeId(0)),
            NodeKind::Replica(NodeId(1)),
            Link::default(),
        );
        assert!(sim.start_client_request(NodeId(0), Op::Set("b".into(), 2)));

        assert_eq!(sim.run().unwrap(), SimulatorRunOutcome::TimeLimit);

        assert_eq!(sim.clients[&NodeId(0)].replies_received, 2);
        for replica in sim.replicas.values() {
            let snapshot = replica.snapshot();
            assert_eq!(snapshot.op_number, 2);
            assert_eq!(snapshot.commit_number, 2);
            assert_eq!(snapshot.log.len(), 2);
            assert_eq!(snapshot.executed_requests.len(), 2);
        }
        assert!(sim.history.events().iter().any(|(_, event)| {
            matches!(
                event,
                RuntimeEvents::NetworkDelivered {
                    message: Message::GetState { .. },
                    ..
                }
            )
        }));
        assert!(sim.history.events().iter().any(|(_, event)| {
            matches!(
                event,
                RuntimeEvents::NetworkDelivered {
                    message: Message::NewState { .. },
                    ..
                }
            )
        }));
    }

    #[test]
    fn retry_recovers_first_dropped_reply_from_cached_result() {
        let config = SimulatorConfig {
            heartbeat_interval: 10,
            client_retry_interval: 5,
            run_until_max_time: 11,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.network.set_link(
            NodeKind::Replica(NodeId(0)),
            NodeKind::Client(NodeId(0)),
            Link {
                drop_probability: 100,
                ..Default::default()
            },
        );
        sim.start_timers();
        assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 7)));

        while !sim.history.events().iter().any(|(_, event)| {
            matches!(
                event,
                RuntimeEvents::NetworkRequest {
                    from: NodeKind::Replica(NodeId(0)),
                    to: NodeKind::Client(NodeId(0)),
                    outcome: NetworkSendOutcome::Dropped,
                    message: Message::Reply { .. },
                }
            )
        }) {
            sim.step().unwrap();
        }

        sim.network.set_link(
            NodeKind::Replica(NodeId(0)),
            NodeKind::Client(NodeId(0)),
            Link::default(),
        );
        assert_eq!(sim.run().unwrap(), SimulatorRunOutcome::TimeLimit);

        let replies_sent = sim
            .history
            .events()
            .iter()
            .filter(|(_, event)| {
                matches!(
                    event,
                    RuntimeEvents::NetworkRequest {
                        from: NodeKind::Replica(NodeId(0)),
                        to: NodeKind::Client(NodeId(0)),
                        message: Message::Reply { .. },
                        ..
                    }
                )
            })
            .count();
        let completions = sim
            .history
            .events()
            .iter()
            .filter(|(_, event)| matches!(event, RuntimeEvents::ClientCompleted { .. }))
            .count();

        assert_eq!(replies_sent, 2);
        assert_eq!(completions, 1);
        for replica in sim.replicas.values() {
            let snapshot = replica.snapshot();
            assert_eq!(snapshot.log.len(), 1);
            assert_eq!(snapshot.executed_requests.len(), 1);
        }
    }

    #[test]
    fn stale_retry_tick_after_completion_is_silent_and_does_not_reschedule() {
        let config = SimulatorConfig {
            client_retry_interval: 5,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 7)));

        while sim.clients[&NodeId(0)].has_pending_request() {
            sim.step().unwrap();
        }

        let history_len = sim.history.events().len();
        let wheel_len = sim.wheel.len();
        assert!(sim.wheel.values().any(|event| {
            matches!(
                event,
                WheelEvent::ClientRetryTick {
                    client_id: NodeId(0),
                    request_number: 1,
                    generation: 0,
                }
            )
        }));

        sim.step().unwrap();

        assert_eq!(sim.now, 5);
        assert_eq!(sim.history.events().len(), history_len);
        assert_eq!(sim.wheel.len(), wheel_len - 1);
        assert!(
            !sim.history
                .events()
                .iter()
                .any(|(_, event)| { matches!(event, RuntimeEvents::ClientRetried { .. }) })
        );
    }

    #[test]
    fn duplicated_retry_request_and_reply_still_execute_once() {
        let config = SimulatorConfig {
            heartbeat_interval: 10,
            client_retry_interval: 5,
            run_until_max_time: 11,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.network.set_link(
            NodeKind::Client(NodeId(0)),
            NodeKind::Replica(NodeId(0)),
            Link {
                drop_probability: 100,
                ..Default::default()
            },
        );
        sim.start_timers();
        assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 7)));

        sim.step().unwrap();
        sim.network.set_link(
            NodeKind::Client(NodeId(0)),
            NodeKind::Replica(NodeId(0)),
            Link {
                duplication_probability: 100,
                ..Default::default()
            },
        );
        sim.network.set_link(
            NodeKind::Replica(NodeId(0)),
            NodeKind::Client(NodeId(0)),
            Link {
                duplication_probability: 100,
                ..Default::default()
            },
        );

        assert_eq!(sim.run().unwrap(), SimulatorRunOutcome::TimeLimit);

        let request_deliveries = sim
            .history
            .events()
            .iter()
            .filter(|(_, event)| {
                matches!(
                    event,
                    RuntimeEvents::NetworkDelivered {
                        from: NodeKind::Client(NodeId(0)),
                        to: NodeKind::Replica(NodeId(0)),
                        message: Message::Request(_),
                    }
                )
            })
            .count();
        let reply_deliveries = sim
            .history
            .events()
            .iter()
            .filter(|(_, event)| {
                matches!(
                    event,
                    RuntimeEvents::NetworkDelivered {
                        from: NodeKind::Replica(NodeId(0)),
                        to: NodeKind::Client(NodeId(0)),
                        message: Message::Reply { .. },
                    }
                )
            })
            .count();

        assert_eq!(request_deliveries, 2);
        assert_eq!(reply_deliveries, 2);
        assert_eq!(sim.clients[&NodeId(0)].replies_received, 1);
        for replica in sim.replicas.values() {
            let snapshot = replica.snapshot();
            assert_eq!(snapshot.log.len(), 1);
            assert_eq!(snapshot.executed_requests.len(), 1);
        }
    }

    #[test]
    fn same_seed_same_retry_history() {
        let run = |seed| {
            let config = SimulatorConfig {
                heartbeat_interval: 10,
                client_retry_interval: 5,
                run_until_max_time: 11,
                ..Default::default()
            };
            let mut sim = setup(seed, 3, Some(config));
            sim.create_network_perfect_mesh();
            sim.network.set_link(
                NodeKind::Client(NodeId(0)),
                NodeKind::Replica(NodeId(0)),
                Link {
                    drop_probability: 100,
                    ..Default::default()
                },
            );
            sim.start_timers();
            assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 7)));

            sim.step().unwrap();
            sim.network.set_link(
                NodeKind::Client(NodeId(0)),
                NodeKind::Replica(NodeId(0)),
                Link::default(),
            );
            sim.run().unwrap();

            assert!(
                sim.history
                    .events()
                    .iter()
                    .any(|(_, event)| { matches!(event, RuntimeEvents::ClientRetried { .. }) })
            );
            format!("{:?}", sim.history)
        };

        assert_eq!(run(42), run(42));
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
    fn start_timers_schedules_watchdogs_for_backups_only() {
        let mut sim = setup(42, 3, None);
        sim.create_network_perfect_mesh();

        sim.start_timers();

        assert_eq!(sim.monitors.len(), 2);
        assert!(!sim.monitors.contains_key(&NodeId(0)));
        assert!(sim.monitors.contains_key(&NodeId(1)));
        assert!(sim.monitors.contains_key(&NodeId(2)));
        assert_eq!(
            sim.wheel
                .values()
                .filter(|event| matches!(event, WheelEvent::WatchdogTick { .. }))
                .count(),
            2
        );
    }

    #[test]
    fn accepted_prepare_resets_backup_watchdog() {
        let config = SimulatorConfig {
            heartbeat_interval: 100,
            watchdog_interval: 50,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.start_timers();
        assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 7)));

        for _ in 0..20 {
            if sim.monitors[&NodeId(1)].generation > 0 {
                break;
            }
            sim.step().unwrap();
        }

        assert_eq!(sim.monitors[&NodeId(1)].generation, 1);
        assert!(!sim.monitors[&NodeId(1)].expired);
        assert!(sim.history.events().iter().any(|(_, event)| {
            matches!(
                event,
                RuntimeEvents::WatchdogReset {
                    node: NodeId(1),
                    generation: 1,
                }
            )
        }));
    }

    #[test]
    fn prepare_gap_still_resets_watchdog_as_primary_activity() {
        let config = SimulatorConfig {
            heartbeat_interval: 100,
            watchdog_interval: 50,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.start_timers();
        let request = ClientRequest {
            op: Op::Set("k".into(), 7),
            client_id: 0,
            request_number: 1,
            result: None,
        };
        sim.schedule_event(
            1,
            WheelEvent::Deliver {
                from: NodeKind::Replica(NodeId(0)),
                to: NodeKind::Replica(NodeId(1)),
                message: Message::Prepare {
                    op: request.op.clone(),
                    view_number: 0,
                    op_number: 2,
                    commit_number: 0,
                    request: Box::new(request),
                },
            },
        );

        sim.step().unwrap();

        assert_eq!(sim.monitors[&NodeId(1)].generation, 1);
        assert!(sim.replicas[&NodeId(1)].snapshot().log.is_empty());
    }

    #[test]
    fn stale_watchdog_tick_is_ignored() {
        let config = SimulatorConfig {
            heartbeat_interval: 100,
            watchdog_interval: 10,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.start_timers();
        sim.reset_watchdog(NodeId(1));

        sim.step().unwrap();

        assert_eq!(sim.now, 10);
        assert_eq!(sim.monitors[&NodeId(1)].generation, 1);
        assert!(!sim.monitors[&NodeId(1)].expired);
        assert!(!sim.history.events().iter().any(|(_, event)| {
            matches!(
                event,
                RuntimeEvents::WatchdogExpired {
                    node: NodeId(1),
                    ..
                }
            )
        }));
    }

    #[test]
    fn current_watchdog_generation_expires_once() {
        let config = SimulatorConfig {
            heartbeat_interval: 100,
            watchdog_interval: 10,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.start_timers();

        sim.step().unwrap();
        sim.watchdog_tick(NodeId(1), 0);

        let expirations = sim
            .history
            .events()
            .iter()
            .filter(|(_, event)| {
                matches!(
                    event,
                    RuntimeEvents::WatchdogExpired {
                        node: NodeId(1),
                        generation: 0,
                    }
                )
            })
            .count();
        assert_eq!(expirations, 1);
        assert!(sim.monitors[&NodeId(1)].expired);
    }

    #[test]
    fn healthy_heartbeats_prevent_watchdog_expiration() {
        let config = SimulatorConfig {
            heartbeat_interval: 5,
            watchdog_interval: 12,
            run_until_max_time: 16,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.start_timers();

        assert_eq!(sim.run().unwrap(), SimulatorRunOutcome::TimeLimit);

        assert!(sim.monitors.values().all(|monitor| !monitor.expired));
        assert!(
            sim.history
                .events()
                .iter()
                .any(|(_, event)| { matches!(event, RuntimeEvents::WatchdogReset { .. }) })
        );
        assert!(
            !sim.history
                .events()
                .iter()
                .any(|(_, event)| { matches!(event, RuntimeEvents::WatchdogExpired { .. }) })
        );
    }

    #[test]
    fn same_seed_same_watchdog_history() {
        let run = |seed| {
            let config = SimulatorConfig {
                heartbeat_interval: 5,
                watchdog_interval: 12,
                run_until_max_time: 16,
                ..Default::default()
            };
            let mut sim = setup(seed, 3, Some(config));
            sim.create_network_perfect_mesh();
            sim.start_timers();
            sim.run().unwrap();
            format!("{:?}", sim.history)
        };

        assert_eq!(run(42), run(42));
    }

    #[test]
    fn dropped_heartbeats_expire_only_the_silent_backup() {
        let config = SimulatorConfig {
            heartbeat_interval: 5,
            watchdog_interval: 12,
            run_until_max_time: 12,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.network.set_link(
            NodeKind::Replica(NodeId(0)),
            NodeKind::Replica(NodeId(1)),
            Link {
                drop_probability: 100,
                ..Default::default()
            },
        );
        sim.start_timers();

        assert_eq!(sim.run().unwrap(), SimulatorRunOutcome::TimeLimit);

        assert!(sim.monitors[&NodeId(1)].expired);
        assert!(!sim.monitors[&NodeId(2)].expired);
        assert!(sim.history.events().iter().any(|(_, event)| {
            matches!(
                event,
                RuntimeEvents::WatchdogExpired {
                    node: NodeId(1),
                    generation: 0,
                }
            )
        }));
    }

    #[test]
    fn wrong_sender_commit_does_not_reset_watchdog() {
        let config = SimulatorConfig {
            heartbeat_interval: 100,
            watchdog_interval: 50,
            ..Default::default()
        };
        let mut sim = setup(42, 3, Some(config));
        sim.create_network_perfect_mesh();
        sim.start_timers();
        sim.schedule_event(
            1,
            WheelEvent::Deliver {
                from: NodeKind::Replica(NodeId(2)),
                to: NodeKind::Replica(NodeId(1)),
                message: Message::Commit {
                    view_number: 0,
                    commit_number: 0,
                },
            },
        );

        sim.step().unwrap();

        assert_eq!(sim.monitors[&NodeId(1)].generation, 0);
        assert!(!sim.history.events().iter().any(|(_, event)| {
            matches!(
                event,
                RuntimeEvents::WatchdogReset {
                    node: NodeId(1),
                    ..
                }
            )
        }));
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
