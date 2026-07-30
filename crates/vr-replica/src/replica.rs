use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::rc::Rc;

use tracing::debug;

use crate::effect::Effect;
use crate::message::{ClientRequest, Message};
use crate::snapshot::{LogEntrySnapshot, ReplicaSnapshot};
use crate::state_machine::StateMachine;
use crate::types::{OpNumber, ReplicaId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    Normal,
    ViewChange,
    Recovering,
    Transitioning,
}

#[derive(Debug, Clone)]
pub struct Replica<Input, Output>
where
    Input: Clone + std::fmt::Debug + 'static,
    Output: Clone + std::fmt::Debug + 'static,
{
    configuration: Vec<ReplicaId>,
    pub replica_number: ReplicaId,

    pub epoch: u64,
    view_number: ReplicaId,
    pub status: Status,

    op_number: usize,
    commit_number: usize,
    log: Vec<(OpNumber, ClientRequest<Input, Output>)>,

    // TODO: Based on the paper, I do need to implement a field that tracks if the given
    // request has already been executed by the replica. If yes, I should store the result
    // which have been returned by this replica.
    client_table: BTreeMap<u64, ClientRequest<Input, Output>>,

    op_ack_table: BTreeMap<ReplicaId, OpNumber>,

    state_machine: Rc<RefCell<dyn StateMachine<Input = Input, Output = Output>>>,
}

impl<Input, Output> Replica<Input, Output>
where
    Input: Clone + std::fmt::Debug,
    Output: Clone + std::fmt::Debug,
{
    pub fn new(
        configuration: Vec<ReplicaId>,
        replica_number: ReplicaId,
        state_machine: Rc<RefCell<dyn StateMachine<Input = Input, Output = Output>>>,
    ) -> Self {
        debug!(replica_number, "creating new replica");
        let mut configuration = configuration.clone();
        configuration.sort();
        Replica {
            state_machine,
            configuration,
            replica_number,
            view_number: 0,
            op_number: 0,
            commit_number: 0,
            epoch: 0,
            status: Status::Normal,
            log: Vec::new(),
            client_table: BTreeMap::new(),
            op_ack_table: BTreeMap::new(),
        }
    }

    pub fn on_message(&mut self, message: Message<Input, Output>) -> Vec<Effect<Input, Output>> {
        match message {
            Message::Request { 0: request } => self.on_request(request),
            Message::Prepare {
                op: _,
                view_number,
                op_number,
                commit_number,
                request,
            } => self.on_prepare(request, view_number, op_number, commit_number),
            Message::PrepareOk {
                view_number,
                replica_number,
                op_number,
            } => self.on_prepare_ok(view_number, replica_number, op_number),
            m => panic!("unexpected message: {:?}", m),
        }
    }

    fn on_request(&mut self, request: ClientRequest<Input, Output>) -> Vec<Effect<Input, Output>> {
        if !self.is_primary() {
            return Vec::new();
        }

        if let Some(last_request) = self.get_last_request_from_client(request.client_id) {
            if request.request_number < last_request.request_number {
                return Vec::new();
            }

            if request.request_number == last_request.request_number {
                let reply = Message::Reply {
                    client_id: request.client_id,
                    view_number: self.view_number,
                    request_id: request.request_number,
                    result: last_request.result.clone(),
                };

                return vec![Effect::Reply {
                    client_id: request.client_id,
                    message: reply,
                }];
            }
        };

        self.op_number += 1;
        if self.log.len() + 1 == self.op_number {
            self.log.push((self.op_number, request.clone()));
        }

        self.ack_request(self.replica_number, self.op_number);

        let prepare = Message::Prepare {
            op: request.op.clone(),
            view_number: self.view_number,
            op_number: self.op_number,
            commit_number: self.commit_number,
            request: Box::new(request.clone()),
        };

        let replicas = self
            .configuration
            .clone()
            .into_iter()
            .filter(|&r| r != self.replica_number)
            .collect::<Vec<_>>();

        replicas
            .iter()
            .map(|&r| Effect::Send {
                to: r,
                message: prepare.clone(),
            })
            .collect()
    }

    // TODO: Add the implementation for the State Transfer
    fn on_prepare(
        &mut self,
        request: Box<ClientRequest<Input, Output>>,
        view_number: ReplicaId,
        op_number: usize,
        commit_number: usize,
    ) -> Vec<Effect<Input, Output>> {
        if !self.is_same_view(view_number) {
            return vec![];
        }

        let mut effects = vec![];

        if self.log.len() + 1 == op_number {
            self.log.push((op_number, *request));
            effects.push(Effect::Prepared {
                replica: self.replica_number,
                op: op_number,
            });

            self.commit_number = commit_number;

            let prepare_ok = Message::PrepareOk {
                view_number: self.view_number,
                replica_number: self.replica_number,
                op_number,
            };

            effects.push(Effect::Send {
                to: self.view_number,
                message: prepare_ok,
            });
        }

        effects
    }

    fn on_prepare_ok(
        &mut self,
        view_number: ReplicaId,
        replica_number: ReplicaId,
        op_number: usize,
    ) -> Vec<Effect<Input, Output>> {
        if !self.is_same_view(view_number) || !self.is_primary() {
            return Vec::new();
        }

        let quorum = self.get_quorum();

        self.ack_request(replica_number, op_number);

        let all_acked_ops = self
            .op_ack_table
            .values()
            .filter(|op| **op == op_number)
            .map(|op| *op)
            .collect::<Vec<usize>>();

        if all_acked_ops.len() < quorum {
            return Vec::new();
        }

        let mut effects = vec![];
        let (result, request) = self.commit_op(op_number);

        let reply = Message::Reply {
            client_id: request.client_id.clone(),
            view_number: self.view_number,
            request_id: request.request_number,
            result: Some(result),
        };

        effects.push(Effect::Reply {
            client_id: request.client_id.clone(),
            message: reply,
        });

        effects
    }

    fn on_commit(
        &mut self,
        op_number: OpNumber,
        commit_number: usize,
        view_number: ReplicaId,
    ) -> Vec<Effect<Input, Output>> {
        if !self.is_same_view(view_number) || self.is_primary() {
            return Vec::new();
        }

        if op_number == self.op_number {
            return Vec::new();
        }

        let _ = self.commit_op(op_number);

        vec![Effect::Committed {
            replica: self.replica_number,
            op: op_number,
        }]
    }

    #[inline]
    fn is_primary(&self) -> bool {
        self.view_number == self.replica_number
    }

    fn is_same_view(&self, view_number: ReplicaId) -> bool {
        self.view_number == view_number
    }

    fn get_last_request_from_client(&self, client_id: u64) -> Option<ClientRequest<Input, Output>> {
        self.client_table.get(&client_id).cloned()
    }

    fn get_quorum(&self) -> usize {
        self.configuration.len() / 2 + 1
    }

    // TODO: Validate if it needs to do more operations here
    fn commit_op(&mut self, op_number: OpNumber) -> (Output, ClientRequest<Input, Output>) {
        // TODO: Validate how exactly we should retrieve the op_number to be committed.
        // From the original implementation, seems that it does op_number - 1. Why? Not sure yet.
        let op_number = if op_number == 0 { 0 } else { op_number - 1 };
        let (_op_number, request) = self.log.get(op_number).unwrap();

        let sm = self.state_machine.clone();

        let result = sm.borrow_mut().apply(request.op.clone());
        let mut request = request.clone();

        request.result = Some(result.clone());

        self.commit_number += 1;
        self.client_table.insert(request.client_id, request.clone());

        (result, request)
    }

    fn ack_request(&mut self, replica_number: u64, op_number: usize) {
        self.op_ack_table
            .entry(replica_number)
            .and_modify(|op| *op = (*op).max(op_number))
            .or_insert(op_number);
    }

    pub fn snapshot(&self) -> ReplicaSnapshot {
        ReplicaSnapshot {
            replica_number: self.replica_number,
            status: self.status.clone(),
            view_number: self.view_number,
            op_number: self.op_number,
            commit_number: self.commit_number,
            log: self
                .log
                .iter()
                .map(|(op_number, request)| LogEntrySnapshot {
                    op_number: *op_number,
                    client_id: request.client_id,
                    request_number: request.request_number,
                })
                .collect(),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, Default)]
    struct KvState {
        state: BTreeMap<String, u64>,
    }

    type Op = String;
    impl StateMachine for KvState {
        type Input = Op;
        type Output = Op;

        fn apply(&mut self, _: Self::Input) -> Self::Output {
            String::from("applied...")
        }
    }

    #[test]
    fn snapshot_reports_initial_replica_state() {
        let state_machine = Rc::new(RefCell::new(KvState::default()));
        let replica = Replica::new(vec![0, 1, 2], 0, state_machine);

        let snapshot = replica.snapshot();

        assert_eq!(snapshot.replica_number, 0);
        assert_eq!(snapshot.status, Status::Normal);
        assert_eq!(snapshot.view_number, 0);
        assert_eq!(snapshot.op_number, 0);
        assert_eq!(snapshot.commit_number, 0);
        assert!(snapshot.log.is_empty());
    }

    #[test]
    fn snapshot_should_be_deterministic() {
        let state_machine = Rc::new(RefCell::new(KvState::default()));
        let replica = Replica::new(vec![0, 1, 2], 0, state_machine);

        let a = replica.snapshot();
        let b = replica.snapshot();

        assert_eq!(a, b);
    }

    #[test]
    fn out_of_order_ack_quorum_still_commits_in_order() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2], 0, sm.clone());

        primary.on_message(request(1, 1, "a"));
        primary.on_message(request(2, 1, "b"));

        let effects_a = primary.on_message(prepare_ok(1, 2));
        let effects_b = primary.on_message(prepare_ok(2, 2));

        assert_eq!(primary.snapshot().commit_number, 2);
        assert_eq!(sm.borrow().applied, vec!["a".to_string(), "b".to_string()]);

        let replied_to: Vec<u64> = effects_a
            .iter()
            .chain(effects_b.iter())
            .filter_map(|e| match e {
                Effect::Reply { client_id, .. } => Some(*client_id),
                _ => None,
            })
            .collect();

        assert_eq!(replied_to, vec![1, 2]);
    }

    #[test]
    fn duplicate_ack_after_quorum_executes_once() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2], 0, sm.clone());

        primary.on_message(request(1, 1, "a"));

        primary.on_message(prepare_ok(1, 1));
        primary.on_message(prepare_ok(2, 1));

        assert_eq!(primary.snapshot().commit_number, 1);
        assert_eq!(sm.borrow().applied.len(), 1);

        let effects = primary.on_message(prepare_ok(1, 1));

        assert_eq!(sm.borrow().applied.len(), 1);
        assert_eq!(primary.snapshot().commit_number, 1);
        assert!(effects.iter().all(|e| !matches!(e, Effect::Reply { .. })));
    }

    #[test]
    fn primary_counts_itself_toward_quorum() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2], 0, sm.clone());

        primary.on_message(request(1, 1, "a"));

        assert_eq!(primary.snapshot().commit_number, 0);

        let effects = primary.on_message(prepare_ok(1, 1));

        assert_eq!(primary.snapshot().commit_number, 1);
        assert_eq!(sm.borrow().applied, vec!["a".to_string()]);

        let replies = effects
            .iter()
            .filter(|e| matches!(e, Effect::Reply { .. }))
            .count();

        assert_eq!(replies, 1);
    }

    #[test]
    fn stale_ack_does_not_regress_watermark() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2, 3, 4], 0, sm.clone());

        primary.on_message(request(1, 1, "a"));
        primary.on_message(request(2, 1, "b"));

        primary.on_message(prepare_ok(1, 2));

        assert_eq!(primary.snapshot().commit_number, 0);

        primary.on_message(prepare_ok(1, 1));
        primary.on_message(prepare_ok(2, 2));

        assert_eq!(primary.snapshot().commit_number, 2);
        assert_eq!(sm.borrow().applied, vec!["a".to_string(), "b".to_string()]);
    }

    fn request(client_id: u64, request_number: usize, op: &str) -> Message<String, String> {
        Message::Request(ClientRequest {
            op: op.to_string(),
            client_id,
            request_number,
            result: None,
        })
    }

    fn prepare_ok(replica_number: ReplicaId, op_number: usize) -> Message<String, String> {
        Message::PrepareOk {
            view_number: 0,
            replica_number,
            op_number,
        }
    }

    #[derive(Debug, Default)]
    struct RecordingSm {
        applied: Vec<String>,
    }

    impl StateMachine for RecordingSm {
        type Input = String;
        type Output = String;

        fn apply(&mut self, input: Self::Input) -> Self::Output {
            self.applied.push(input.clone());
            format!("applied-{input}")
        }
    }
}
