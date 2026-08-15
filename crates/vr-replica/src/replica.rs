use std::cell::RefCell;
use std::collections::BTreeMap;
use std::fmt::Debug;
use std::rc::Rc;

use tracing::debug;

use crate::effect::Effect;
use crate::message::{ClientRequest, Message};
use crate::snapshot::{
    ClientTableEntrySnapshot, ExecutedRequestSnapshot, LogEntrySnapshot, ReplicaSnapshot,
};
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
    executed_requests: Vec<ExecutedRequestSnapshot<Input, Output>>,

    op_ack_table: BTreeMap<ReplicaId, OpNumber>,

    state_machine: Rc<RefCell<dyn StateMachine<Input = Input, Output = Output>>>,
}

impl<Input, Output> Replica<Input, Output>
where
    Input: Clone + PartialEq + std::fmt::Debug,
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
            executed_requests: Vec::new(),
            op_ack_table: BTreeMap::new(),
        }
    }

    pub fn on_message(&mut self, message: Message<Input, Output>) -> Vec<Effect<Input, Output>> {
        match message {
            Message::Request { 0: request } => self.on_request(request),
            Message::Prepare {
                op,
                view_number,
                op_number,
                commit_number,
                request,
            } => self.on_prepare(op, request, view_number, op_number, commit_number),
            Message::PrepareOk {
                view_number,
                replica_number,
                op_number,
            } => self.on_prepare_ok(view_number, replica_number, op_number),
            Message::Commit {
                view_number,
                commit_number,
            } => self.on_commit(commit_number, view_number),
            m => {
                debug!(
                    ?m,
                    replica_number = self.replica_number,
                    "no mapped message"
                );

                return Vec::new();
            }
        }
    }

    fn on_request(&mut self, request: ClientRequest<Input, Output>) -> Vec<Effect<Input, Output>> {
        if self.status != Status::Normal || !self.is_primary() {
            return Vec::new();
        }

        if let Some(last_request) = self.get_last_request_from_client(request.client_id) {
            if request.request_number < last_request.request_number {
                return Vec::new();
            }

            if last_request.result.is_none() {
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

        debug_assert_eq!(self.log.len(), self.op_number);

        self.op_number += 1;
        self.log.push((self.op_number, request.clone()));
        self.client_table.insert(request.client_id, request.clone());
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

    fn on_prepare(
        &mut self,
        op: Input,
        request: Box<ClientRequest<Input, Output>>,
        view_number: ReplicaId,
        op_number: usize,
        commit_number: usize,
    ) -> Vec<Effect<Input, Output>> {
        if self.status != Status::Normal || self.is_primary() || !self.is_same_view(view_number) {
            return Vec::new();
        }

        if op != request.op {
            return Vec::new();
        }

        let mut effects = vec![];

        if self.log.len() + 1 == op_number {
            debug_assert_eq!(self.op_number, self.log.len());

            self.log.push((op_number, *request.clone()));
            effects.push(Effect::Prepared {
                replica: self.replica_number,
                op: op_number,
            });

            self.op_number = op_number;
            self.client_table.insert(request.client_id, *request);

            let target = commit_number.min(self.log.len());
            let committed = self.commit_up_to(target);

            committed.iter().for_each(|(op_number, _, _)| {
                effects.push(Effect::Committed {
                    replica: self.replica_number,
                    op: *op_number,
                })
            });

            let prepare_ok = Message::PrepareOk {
                view_number: self.view_number,
                replica_number: self.replica_number,
                op_number,
            };

            effects.push(Effect::Send {
                to: self.view_number,
                message: prepare_ok,
            });
        } else if op_number <= self.log.len() {
            let (_, stored) = &self.log[op_number - 1];
            let matches = stored.client_id == request.client_id
                && stored.request_number == request.request_number
                && stored.op == request.op;

            if matches {
                effects.push(Effect::Send {
                    to: self.view_number,
                    message: Message::PrepareOk {
                        view_number: self.view_number,
                        replica_number: self.replica_number,
                        op_number,
                    },
                });
            }
        }

        effects
    }

    fn on_prepare_ok(
        &mut self,
        view_number: ReplicaId,
        replica_number: ReplicaId,
        op_number: usize,
    ) -> Vec<Effect<Input, Output>> {
        if self.status != Status::Normal
            || !self.is_primary()
            || !self.is_same_view(view_number)
            || op_number > self.op_number
        {
            return Vec::new();
        }

        if !self.configuration.contains(&replica_number) {
            return Vec::new();
        }

        self.ack_request(replica_number, op_number);

        let committed = self.commit_up_to(self.commit_point());

        debug_assert!(self.commit_number <= self.op_number);

        committed
            .into_iter()
            .map(|(_, result, request)| {
                let reply = Message::Reply {
                    client_id: request.client_id.clone(),
                    view_number: self.view_number,
                    request_id: request.request_number,
                    result: Some(result),
                };

                Effect::Reply {
                    client_id: request.client_id,
                    message: reply,
                }
            })
            .collect()
    }

    fn on_commit(
        &mut self,
        commit_number: usize,
        view_number: ReplicaId,
    ) -> Vec<Effect<Input, Output>> {
        if self.status != Status::Normal || self.is_primary() || !self.is_same_view(view_number) {
            return Vec::new();
        }

        let target = commit_number.min(self.log.len());

        self.commit_up_to(target)
            .into_iter()
            .map(|(op_number, _, _)| Effect::Committed {
                replica: self.replica_number,
                op: op_number,
            })
            .collect()
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
    fn commit_op(&mut self, op_number: OpNumber) -> (usize, Output, ClientRequest<Input, Output>) {
        debug_assert_eq!(op_number, self.commit_number + 1);

        let (_, request) = self.log.get(op_number - 1).unwrap();

        let sm = self.state_machine.clone();

        let result = sm.borrow_mut().apply(request.op.clone());
        let mut request = request.clone();

        self.executed_requests.push(ExecutedRequestSnapshot {
            client_id: request.client_id,
            request_number: request.request_number,
            op: request.op.clone(),
            result: result.clone(),
        });

        request.result = Some(result.clone());

        self.commit_number = op_number;
        let should_update_client_table = self
            .client_table
            .get(&request.client_id)
            .is_none_or(|latest| latest.request_number <= request.request_number);
        if should_update_client_table {
            self.client_table.insert(request.client_id, request.clone());
        }

        (op_number, result, request)
    }

    fn ack_request(&mut self, replica_number: u64, op_number: usize) {
        self.op_ack_table
            .entry(replica_number)
            .and_modify(|op| *op = (*op).max(op_number))
            .or_insert(op_number);
    }

    fn commit_point(&self) -> usize {
        let mut acked_ops: Vec<usize> = self
            .configuration
            .iter()
            .map(|r| self.op_ack_table.get(r).copied().unwrap_or(0))
            .collect();
        acked_ops.sort_unstable_by(|a, b| b.cmp(a)); // descending
        acked_ops[self.get_quorum() - 1]
    }

    fn commit_up_to(
        &mut self,
        target: usize,
    ) -> Vec<(usize, Output, ClientRequest<Input, Output>)> {
        let mut committed = vec![];
        while self.commit_number < target {
            committed.push(self.commit_op(self.commit_number + 1));
        }
        committed
    }

    pub fn snapshot(&self) -> ReplicaSnapshot<Input, Output> {
        ReplicaSnapshot {
            replica_number: self.replica_number,
            epoch: self.epoch,
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
                    op: request.op.clone(),
                })
                .collect(),
            client_table: self
                .client_table
                .iter()
                .map(|(client_id, request)| ClientTableEntrySnapshot {
                    client_id: *client_id,
                    request_number: request.request_number,
                    op: request.op.clone(),
                    result: request.result.clone(),
                })
                .collect(),
            executed_requests: self.executed_requests.clone(),
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
        assert_eq!(snapshot.epoch, 0);
        assert_eq!(snapshot.status, Status::Normal);
        assert_eq!(snapshot.view_number, 0);
        assert_eq!(snapshot.op_number, 0);
        assert_eq!(snapshot.commit_number, 0);
        assert!(snapshot.log.is_empty());
        assert!(snapshot.client_table.is_empty());
        assert!(snapshot.executed_requests.is_empty());
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
        assert_eq!(
            primary.snapshot().executed_requests,
            vec![
                ExecutedRequestSnapshot {
                    client_id: 1,
                    request_number: 1,
                    op: "a".to_string(),
                    result: "applied-a".to_string(),
                },
                ExecutedRequestSnapshot {
                    client_id: 2,
                    request_number: 1,
                    op: "b".to_string(),
                    result: "applied-b".to_string(),
                },
            ]
        );
        assert_eq!(primary.snapshot().log[0].op, "a");
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

    #[test]
    fn test_duplicate_messages_before_commit() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2], 0, sm.clone());

        let effects = primary.on_message(request(1, 1, "a"));

        assert_eq!(primary.snapshot().op_number, 1);
        assert_eq!(primary.snapshot().commit_number, 0);
        assert_eq!(primary.snapshot().log.len(), 1);
        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::Send { .. }))
                .count(),
            2
        );
        assert!(sm.borrow().applied.is_empty());

        let effects = primary.on_message(request(1, 1, "a"));

        assert!(effects.is_empty());
        assert_eq!(primary.snapshot().op_number, 1);
        assert_eq!(primary.snapshot().commit_number, 0);
        assert_eq!(primary.snapshot().log.len(), 1);
        assert!(sm.borrow().applied.is_empty());

        let commit_effects = primary.on_message(prepare_ok(1, 1));

        assert_eq!(primary.snapshot().commit_number, 1);
        assert_eq!(primary.snapshot().log.len(), 1);
        assert_eq!(sm.borrow().applied, vec!["a".to_string()]);
        assert_eq!(
            commit_effects
                .iter()
                .filter(|effect| matches!(effect, Effect::Reply { .. }))
                .count(),
            1
        );
    }

    #[test]
    fn duplicate_request_after_commit_returns_cached_reply_without_execution() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2], 0, sm.clone());

        primary.on_message(request(1, 1, "a"));
        primary.on_message(prepare_ok(1, 1));

        assert_eq!(sm.borrow().applied, vec!["a".to_string()]);

        let effects = primary.on_message(request(1, 1, "a"));

        assert_eq!(primary.snapshot().op_number, 1);
        assert_eq!(primary.snapshot().commit_number, 1);
        assert_eq!(primary.snapshot().log.len(), 1);
        assert_eq!(sm.borrow().applied, vec!["a".to_string()]);

        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::Reply { .. }))
                .count(),
            1
        );

        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, Effect::Send { .. }))
        );

        match effects.as_slice() {
            [
                Effect::Reply {
                    client_id,
                    message:
                        Message::Reply {
                            request_id, result, ..
                        },
                },
            ] => {
                assert_eq!(*client_id, 1);
                assert_eq!(*request_id, 1);
                assert_eq!(result.as_deref(), Some("applied-a"));
            }
            other => panic!("expected one cached reply, got {other:?}"),
        }
    }

    #[test]
    fn older_request_number_is_ignored() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2], 0, sm.clone());

        primary.on_message(request(1, 2, "newer"));

        let effects = primary.on_message(request(1, 1, "older"));

        assert!(effects.is_empty());
        assert_eq!(primary.snapshot().op_number, 1);
        assert_eq!(primary.snapshot().log.len(), 1);
        assert!(sm.borrow().applied.is_empty());
    }

    #[test]
    fn newer_request_is_appended_after_previous_request_completes() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2], 0, sm.clone());

        primary.on_message(request(1, 1, "a"));
        primary.on_message(prepare_ok(1, 1));

        let effects = primary.on_message(request(1, 2, "b"));

        assert_eq!(primary.snapshot().op_number, 2);
        assert_eq!(primary.snapshot().commit_number, 1);
        assert_eq!(primary.snapshot().log.len(), 2);
        assert_eq!(sm.borrow().applied, vec!["a".to_string()]);

        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::Send { .. }))
                .count(),
            2
        );

        primary.on_message(prepare_ok(1, 2));

        assert_eq!(primary.snapshot().commit_number, 2);
        assert_eq!(sm.borrow().applied, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn backup_updates_state_after_appending_prepare() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());

        let req: ClientRequest<String, String> = ClientRequest {
            client_id: 1,
            op: String::from("a"),
            request_number: 1,
            result: None,
        };

        let eff = backup.on_message(prepare(String::from("a"), 1, 0, req.clone()));

        assert_eq!(eff.len(), 2);

        let first_effect = eff.first().unwrap();
        assert_eq!(*first_effect, Effect::Prepared { op: 1, replica: 1 });

        let last_effect = eff.last().unwrap();
        assert_eq!(
            *last_effect,
            Effect::Send {
                to: 0,
                message: Message::PrepareOk {
                    view_number: 0,
                    replica_number: 1,
                    op_number: 1,
                }
            }
        );

        assert_eq!(backup.log.len(), 1);
        assert_eq!(backup.op_number, 1);
        assert_eq!(backup.commit_number, 0);
        assert_eq!(backup.client_table.get(&1), Some(&req));
    }

    #[test]
    fn backup_executes_piggybacked_commit_range_in_order() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());

        let req: ClientRequest<String, String> = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 7,
            result: None,
        };

        backup.on_message(prepare(String::from("a"), 1, 0, req.clone()));
        backup.on_message(prepare(
            String::from("b"),
            2,
            0,
            ClientRequest {
                op: String::from("b"),
                client_id: 20,
                ..req.clone()
            },
        ));

        let effects = backup.on_message(prepare(
            String::from("c"),
            3,
            2,
            ClientRequest {
                op: String::from("c"),
                client_id: 30,
                ..req
            },
        ));

        let snapshot = backup.snapshot();

        assert_eq!(snapshot.op_number, 3);
        assert_eq!(snapshot.log.len(), 3);
        assert_eq!(snapshot.commit_number, 2);
        assert_eq!(sm.borrow().applied, vec!["a".to_string(), "b".to_string()]);

        let committed: Vec<usize> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Committed { op, .. } => Some(*op),
                _ => None,
            })
            .collect();

        assert_eq!(committed, vec![1, 2]);
        assert_eq!(
            backup
                .client_table
                .get(&10)
                .and_then(|request| request.result.as_deref()),
            Some("applied-a")
        );
        assert_eq!(
            backup
                .client_table
                .get(&20)
                .and_then(|request| request.result.as_deref()),
            Some("applied-b")
        );
        assert_eq!(
            backup
                .client_table
                .get(&30)
                .and_then(|request| request.result.as_deref()),
            None
        );
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, Effect::Reply { .. }))
        );

        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(effect, Effect::Prepared { op: 3, .. }))
                .count(),
            1
        );

        assert_eq!(
            effects
                .iter()
                .filter(|effect| matches!(
                    effect,
                    Effect::Send {
                        message: Message::PrepareOk { op_number: 3, .. },
                        ..
                    }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn committing_older_request_does_not_regress_backup_client_table() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());
        let first = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 1,
            result: None,
        };
        let second = ClientRequest {
            client_id: 10,
            op: String::from("b"),
            request_number: 2,
            result: None,
        };

        backup.on_message(prepare(String::from("a"), 1, 0, first));
        backup.on_message(prepare(String::from("b"), 2, 1, second));

        let snapshot = backup.snapshot();
        assert_eq!(snapshot.commit_number, 1);
        assert_eq!(snapshot.client_table.len(), 1);
        assert_eq!(snapshot.client_table[0].request_number, 2);
        assert_eq!(snapshot.client_table[0].op, "b");
        assert_eq!(snapshot.client_table[0].result, None);
        assert_eq!(sm.borrow().applied, vec!["a".to_string()]);
    }

    #[test]
    fn stale_prepare_commit_does_not_regress_backup() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());

        let req: ClientRequest<String, String> = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 7,
            result: None,
        };

        backup.on_message(prepare(String::from("a"), 1, 0, req.clone()));
        backup.on_message(prepare(
            String::from("b"),
            2,
            1,
            ClientRequest {
                op: String::from("b"),
                client_id: 20,
                ..req.clone()
            },
        ));

        let effects = backup.on_message(prepare(
            String::from("c"),
            3,
            0,
            ClientRequest {
                op: String::from("c"),
                client_id: 30,
                ..req
            },
        ));

        assert!(
            !effects
                .iter()
                .any(|e| matches!(e, Effect::Committed { .. }))
        );

        assert_eq!(backup.snapshot().commit_number, 1);
        assert_eq!(sm.borrow().applied, vec!["a".to_string()]);
        assert!(
            effects
                .iter()
                .any(|effect| matches!(effect, Effect::Prepared { replica: 1, op: 3 }))
        );
        assert!(effects.iter().any(|effect| matches!(
            effect,
            Effect::Send {
                to: 0,
                message: Message::PrepareOk {
                    view_number: 0,
                    replica_number: 1,
                    op_number: 3,
                }
            }
        )));
    }

    #[test]
    fn prepare_commit_beyond_local_log_is_clamped() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());

        let req: ClientRequest<String, String> = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 7,
            result: None,
        };

        let effects = backup.on_message(prepare(String::from("a"), 1, 10, req));

        let snapshot = backup.snapshot();
        assert_eq!(snapshot.op_number, 1);
        assert_eq!(snapshot.log.len(), 1);
        assert_eq!(snapshot.commit_number, 1);
        assert_eq!(sm.borrow().applied, vec!["a".to_string()]);

        let committed: Vec<usize> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Committed { op, .. } => Some(*op),
                _ => None,
            })
            .collect();
        assert_eq!(committed, vec![1]);
    }

    #[test]
    fn duplicate_prepare_does_not_append_or_execute_twice() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());
        let req = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 7,
            result: None,
        };

        backup.on_message(prepare(String::from("a"), 1, 1, req.clone()));
        let duplicate_effects = backup.on_message(prepare(String::from("a"), 1, 1, req));

        let snapshot = backup.snapshot();
        assert_eq!(snapshot.op_number, 1);
        assert_eq!(snapshot.log.len(), 1);
        assert_eq!(snapshot.commit_number, 1);
        assert_eq!(sm.borrow().applied, vec!["a".to_string()]);
        assert_eq!(
            duplicate_effects,
            vec![Effect::Send {
                to: 0,
                message: Message::PrepareOk {
                    view_number: 0,
                    replica_number: 1,
                    op_number: 1,
                },
            }]
        );
    }

    #[test]
    fn conflicting_duplicate_prepare_is_ignored() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());
        let req = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 7,
            result: None,
        };

        backup.on_message(prepare(String::from("a"), 1, 0, req));
        let before = backup.snapshot();
        let effects = backup.on_message(prepare(
            String::from("conflict"),
            1,
            0,
            ClientRequest {
                client_id: 10,
                op: String::from("conflict"),
                request_number: 7,
                result: None,
            },
        ));

        assert!(effects.is_empty());
        assert_eq!(backup.snapshot(), before);
        assert!(sm.borrow().applied.is_empty());
    }

    #[test]
    fn prepare_with_log_gap_is_ignored() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());
        let before = backup.snapshot();

        let effects = backup.on_message(prepare(
            String::from("b"),
            2,
            0,
            ClientRequest {
                client_id: 20,
                op: String::from("b"),
                request_number: 7,
                result: None,
            },
        ));

        assert!(effects.is_empty());
        assert_eq!(backup.snapshot(), before);
        assert!(backup.client_table.is_empty());
        assert!(sm.borrow().applied.is_empty());
    }

    #[test]
    fn prepare_with_mismatched_operation_fields_is_ignored() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());
        let before = backup.snapshot();
        let message = Message::Prepare {
            op: String::from("outer"),
            view_number: 0,
            op_number: 1,
            commit_number: 0,
            request: Box::new(ClientRequest {
                client_id: 10,
                op: String::from("inner"),
                request_number: 7,
                result: None,
            }),
        };

        let effects = backup.on_message(message);

        assert!(effects.is_empty());
        assert_eq!(backup.snapshot(), before);
        assert!(backup.client_table.is_empty());
        assert!(sm.borrow().applied.is_empty());
    }

    #[test]
    fn commit_executes_available_prefix() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());
        let req = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 7,
            result: None,
        };

        backup.on_message(prepare(String::from("a"), 1, 0, req.clone()));
        backup.on_message(prepare(
            String::from("b"),
            2,
            0,
            ClientRequest {
                client_id: 20,
                op: String::from("b"),
                ..req
            },
        ));

        let effects = backup.on_message(commit(0, 2));

        assert_eq!(backup.snapshot().commit_number, 2);
        assert_eq!(sm.borrow().applied, vec!["a".to_string(), "b".to_string()]);
        assert_eq!(
            backup
                .client_table
                .get(&10)
                .and_then(|request| request.result.as_deref()),
            Some("applied-a")
        );
        assert_eq!(
            backup
                .client_table
                .get(&20)
                .and_then(|request| request.result.as_deref()),
            Some("applied-b")
        );

        let committed: Vec<usize> = effects
            .iter()
            .filter_map(|effect| match effect {
                Effect::Committed { op, .. } => Some(*op),
                _ => None,
            })
            .collect();
        assert_eq!(committed, vec![1, 2]);
        assert!(
            effects
                .iter()
                .all(|effect| !matches!(effect, Effect::Reply { .. }))
        );
    }

    #[test]
    fn duplicate_commit_is_no_op() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());
        let req = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 7,
            result: None,
        };

        backup.on_message(prepare(String::from("a"), 1, 0, req));
        backup.on_message(commit(0, 1));
        let duplicate_effects = backup.on_message(commit(0, 1));

        assert_eq!(backup.snapshot().commit_number, 1);
        assert_eq!(sm.borrow().applied, vec!["a".to_string()]);
        assert!(duplicate_effects.is_empty());
    }

    #[test]
    fn stale_commit_does_not_regress_backup() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());
        let req = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 7,
            result: None,
        };

        backup.on_message(prepare(String::from("a"), 1, 0, req.clone()));
        backup.on_message(prepare(
            String::from("b"),
            2,
            0,
            ClientRequest {
                client_id: 20,
                op: String::from("b"),
                ..req
            },
        ));
        backup.on_message(commit(0, 2));
        let stale_effects = backup.on_message(commit(0, 1));

        assert_eq!(backup.snapshot().commit_number, 2);
        assert_eq!(sm.borrow().applied, vec!["a".to_string(), "b".to_string()]);
        assert!(stale_effects.is_empty());
    }

    #[test]
    fn commit_beyond_log_is_clamped() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());
        let req = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 7,
            result: None,
        };

        backup.on_message(prepare(String::from("a"), 1, 0, req));
        let effects = backup.on_message(commit(0, 10));

        assert_eq!(backup.snapshot().commit_number, 1);
        assert_eq!(sm.borrow().applied, vec!["a".to_string()]);
        assert_eq!(effects, vec![Effect::Committed { replica: 1, op: 1 }]);
    }

    #[test]
    fn wrong_view_commit_is_ignored() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());
        let req = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 7,
            result: None,
        };

        backup.on_message(prepare(String::from("a"), 1, 0, req));
        let effects = backup.on_message(commit(1, 1));

        assert_eq!(backup.snapshot().commit_number, 0);
        assert!(sm.borrow().applied.is_empty());
        assert!(effects.is_empty());
    }

    #[test]
    fn primary_ignores_commit() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2], 0, sm.clone());

        primary.on_message(request(10, 7, "a"));
        let effects = primary.on_message(commit(0, 1));

        assert_eq!(primary.snapshot().op_number, 1);
        assert_eq!(primary.snapshot().commit_number, 0);
        assert!(sm.borrow().applied.is_empty());
        assert!(effects.is_empty());
    }

    #[test]
    fn non_normal_primary_ignores_request() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2], 0, sm.clone());
        primary.status = Status::ViewChange;
        let before = primary.snapshot();

        let effects = primary.on_message(request(10, 7, "a"));

        assert!(effects.is_empty());
        assert_eq!(primary.snapshot(), before);
        assert!(primary.client_table.is_empty());
        assert!(primary.op_ack_table.is_empty());
        assert!(sm.borrow().applied.is_empty());
    }

    #[test]
    fn primary_ignores_prepare() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2], 0, sm.clone());
        let before = primary.snapshot();
        let req = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 7,
            result: None,
        };

        let effects = primary.on_message(prepare(String::from("a"), 1, 0, req));

        assert!(effects.is_empty());
        assert_eq!(primary.snapshot(), before);
        assert!(primary.client_table.is_empty());
        assert!(sm.borrow().applied.is_empty());
    }

    #[test]
    fn non_normal_backup_ignores_prepare() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());
        backup.status = Status::Recovering;
        let before = backup.snapshot();
        let req = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 7,
            result: None,
        };

        let effects = backup.on_message(prepare(String::from("a"), 1, 0, req));

        assert!(effects.is_empty());
        assert_eq!(backup.snapshot(), before);
        assert!(backup.client_table.is_empty());
        assert!(sm.borrow().applied.is_empty());
    }

    #[test]
    fn non_normal_primary_ignores_prepare_ok() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2], 0, sm.clone());
        primary.on_message(request(10, 7, "a"));
        primary.status = Status::ViewChange;
        let before = primary.snapshot();
        let ack_table_before = primary.op_ack_table.clone();

        let effects = primary.on_message(prepare_ok(1, 1));

        assert!(effects.is_empty());
        assert_eq!(primary.snapshot(), before);
        assert_eq!(primary.op_ack_table, ack_table_before);
        assert!(sm.borrow().applied.is_empty());
    }

    #[test]
    fn non_normal_backup_ignores_commit() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut backup = Replica::new(vec![0, 1, 2], 1, sm.clone());
        let req = ClientRequest {
            client_id: 10,
            op: String::from("a"),
            request_number: 7,
            result: None,
        };
        backup.on_message(prepare(String::from("a"), 1, 0, req));
        backup.status = Status::Recovering;
        let before = backup.snapshot();

        let effects = backup.on_message(commit(0, 1));

        assert!(effects.is_empty());
        assert_eq!(backup.snapshot(), before);
        assert!(sm.borrow().applied.is_empty());
    }

    #[test]
    fn ahead_prepare_ok_is_ignored() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2], 0, sm.clone());
        primary.on_message(request(10, 7, "a"));
        let before = primary.snapshot();
        let ack_table_before = primary.op_ack_table.clone();

        let effects = primary.on_message(prepare_ok(1, 2));

        assert!(effects.is_empty());
        assert_eq!(primary.snapshot(), before);
        assert_eq!(primary.op_ack_table, ack_table_before);
        assert!(sm.borrow().applied.is_empty());
    }

    #[test]
    fn prepare_ok_from_non_member_is_ignored() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut primary = Replica::new(vec![0, 1, 2], 0, sm.clone());
        primary.on_message(request(10, 7, "a"));
        let before = primary.snapshot();
        let ack_table_before = primary.op_ack_table.clone();

        let effects = primary.on_message(prepare_ok(99, 1));

        assert!(effects.is_empty());
        assert_eq!(primary.snapshot(), before);
        assert_eq!(primary.op_ack_table, ack_table_before);
        assert!(sm.borrow().applied.is_empty());
    }

    #[test]
    fn unexpected_message_is_ignored() {
        let sm = Rc::new(RefCell::new(RecordingSm::default()));
        let mut replica = Replica::new(vec![0, 1, 2], 1, sm.clone());
        let before = replica.snapshot();

        let effects = replica.on_message(Message::Error {
            message: String::from("unexpected"),
        });

        assert!(effects.is_empty());
        assert_eq!(replica.snapshot(), before);
        assert!(replica.client_table.is_empty());
        assert!(replica.op_ack_table.is_empty());
        assert!(sm.borrow().applied.is_empty());
    }

    fn request(client_id: u64, request_number: usize, op: &str) -> Message<String, String> {
        Message::Request(ClientRequest {
            op: op.to_string(),
            client_id,
            request_number,
            result: None,
        })
    }

    fn prepare<I, O>(
        op: I,
        op_number: usize,
        commit_number: usize,
        request: ClientRequest<I, O>,
    ) -> Message<I, O> {
        Message::Prepare {
            op: op,
            view_number: 0,
            op_number,
            commit_number: commit_number,
            request: Box::new(request),
        }
    }

    fn prepare_ok(replica_number: ReplicaId, op_number: usize) -> Message<String, String> {
        Message::PrepareOk {
            view_number: 0,
            replica_number,
            op_number,
        }
    }

    fn commit(view_number: ReplicaId, commit_number: OpNumber) -> Message<String, String> {
        Message::Commit {
            view_number,
            commit_number,
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
