use std::collections::BTreeMap;

use vr_replica::{snapshot::ReplicaSnapshot, types::ReplicaId};

use crate::client::Op;

type Snapshot = ReplicaSnapshot<Op, Op>;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvariantViolation {
    pub invariant: &'static str,
    pub replica: ReplicaId,
    pub details: String,
}

#[derive(Debug, Default)]
pub struct StateChecker {
    snapshots: BTreeMap<ReplicaId, Snapshot>,
}

impl StateChecker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe_snapshot(&mut self, snapshot: Snapshot) -> Result<(), InvariantViolation> {
        let replica = snapshot.replica_number;

        Self::check_commit_within_log(&snapshot)?;
        Self::check_no_duplicate_requests(&snapshot)?;
        Self::check_no_duplicate_execution(&snapshot)?;
        Self::check_execution_matches_committed_prefix(&snapshot)?;
        Self::check_client_table_consistency(&snapshot)?;

        if let Some(previous) = self.snapshots.get(&replica) {
            Self::check_commit_monotonicity(previous, &snapshot)?;
            Self::check_view_monotonicity(previous, &snapshot)?;
            Self::check_epoch_monotonicity(previous, &snapshot)?;
        }

        self.check_same_committed_prefix(&snapshot)?;

        self.snapshots.insert(replica, snapshot);

        Ok(())
    }

    pub fn invariant_reply_implies_committed(
        &self,
        replica: ReplicaId,
        client_id: u64,
        request_number: usize,
    ) -> Result<(), InvariantViolation> {
        let Some(snapshot) = self.snapshots.get(&replica) else {
            return Err(InvariantViolation {
                invariant: "reply_implies_committed",
                replica,
                details: format!(
                    "replica {replica} emitted a reply before any snapshot was observed"
                ),
            });
        };

        let Some(entry) = snapshot
            .log
            .iter()
            .find(|entry| entry.client_id == client_id && entry.request_number == request_number)
        else {
            return Err(InvariantViolation {
                invariant: "reply_implies_committed",
                replica,
                details: format!(
                    "reply for client={client_id} request={request_number} \
                     does not correspond to an entry in the replica log"
                ),
            });
        };

        if entry.op_number > snapshot.commit_number {
            return Err(InvariantViolation {
                invariant: "reply_implies_committed",
                replica,
                details: format!(
                    "reply for client={client_id} request={request_number} \
                     refers to op={} but commit_number={}",
                    entry.op_number, snapshot.commit_number,
                ),
            });
        }

        Ok(())
    }

    fn check_commit_monotonicity(
        previous: &Snapshot,
        current: &Snapshot,
    ) -> Result<(), InvariantViolation> {
        if current.commit_number < previous.commit_number {
            return Err(InvariantViolation {
                invariant: "commit_monotonicity",
                replica: current.replica_number,
                details: format!(
                    "commit number regressed from {} to {}",
                    previous.commit_number, current.commit_number,
                ),
            });
        }

        Ok(())
    }

    fn check_view_monotonicity(
        previous: &Snapshot,
        current: &Snapshot,
    ) -> Result<(), InvariantViolation> {
        if current.view_number < previous.view_number {
            return Err(InvariantViolation {
                invariant: "view_monotonicity",
                replica: current.replica_number,
                details: format!(
                    "view number regressed from {} to {}",
                    previous.view_number, current.view_number,
                ),
            });
        }

        Ok(())
    }

    /// VR Revisited §4.5: a replica never moves to an older configuration epoch.
    fn check_epoch_monotonicity(
        previous: &Snapshot,
        current: &Snapshot,
    ) -> Result<(), InvariantViolation> {
        if current.epoch < previous.epoch {
            return Err(InvariantViolation {
                invariant: "epoch_monotonicity",
                replica: current.replica_number,
                details: format!(
                    "epoch regressed from {} to {}",
                    previous.epoch, current.epoch,
                ),
            });
        }

        Ok(())
    }

    fn check_no_duplicate_requests(snapshot: &Snapshot) -> Result<(), InvariantViolation> {
        use std::collections::BTreeSet;

        let mut requests = BTreeSet::new();

        for entry in &snapshot.log {
            let identity = (entry.client_id, entry.request_number);

            if !requests.insert(identity) {
                return Err(InvariantViolation {
                    invariant: "duplicate_request_in_log",
                    replica: snapshot.replica_number,
                    details: format!(
                        "client={} request={} appears more than once in the log",
                        entry.client_id, entry.request_number,
                    ),
                });
            }
        }

        Ok(())
    }

    /// VR Revisited §4.1: the client table ensures a request is executed at most once.
    fn check_no_duplicate_execution(snapshot: &Snapshot) -> Result<(), InvariantViolation> {
        use std::collections::BTreeSet;

        let mut executed = BTreeSet::new();
        for request in &snapshot.executed_requests {
            let identity = (request.client_id, request.request_number);
            if !executed.insert(identity) {
                return Err(InvariantViolation {
                    invariant: "at_most_once_execution",
                    replica: snapshot.replica_number,
                    details: format!(
                        "client={} request={} was executed more than once",
                        request.client_id, request.request_number,
                    ),
                });
            }
        }

        Ok(())
    }

    /// VR Revisited §4.1: replicas execute committed operations in log order.
    fn check_execution_matches_committed_prefix(
        snapshot: &Snapshot,
    ) -> Result<(), InvariantViolation> {
        if snapshot.executed_requests.len() != snapshot.commit_number {
            return Err(InvariantViolation {
                invariant: "execution_matches_committed_prefix",
                replica: snapshot.replica_number,
                details: format!(
                    "executed request count={} differs from commit_number={}",
                    snapshot.executed_requests.len(),
                    snapshot.commit_number,
                ),
            });
        }

        for (index, (executed, logged)) in snapshot
            .executed_requests
            .iter()
            .zip(snapshot.log.iter())
            .enumerate()
        {
            if executed.client_id != logged.client_id
                || executed.request_number != logged.request_number
                || executed.op != logged.op
            {
                return Err(InvariantViolation {
                    invariant: "execution_matches_committed_prefix",
                    replica: snapshot.replica_number,
                    details: format!(
                        "execution at op {} is client={} request={} op={:?}, but log has client={} request={} op={:?}",
                        index + 1,
                        executed.client_id,
                        executed.request_number,
                        executed.op,
                        logged.client_id,
                        logged.request_number,
                        logged.op,
                    ),
                });
            }
        }

        Ok(())
    }

    /// VR Revisited §4.1: each client-table entry tracks its latest request and result.
    fn check_client_table_consistency(snapshot: &Snapshot) -> Result<(), InvariantViolation> {
        let mut table = BTreeMap::new();
        for entry in &snapshot.client_table {
            if table.insert(entry.client_id, entry).is_some() {
                return Err(InvariantViolation {
                    invariant: "client_table_consistency",
                    replica: snapshot.replica_number,
                    details: format!("client={} appears more than once", entry.client_id),
                });
            }
        }

        let mut latest: BTreeMap<u64, (usize, &vr_replica::snapshot::LogEntrySnapshot<Op>)> =
            BTreeMap::new();
        for (index, entry) in snapshot.log.iter().enumerate() {
            let candidate = (index, entry);
            latest
                .entry(entry.client_id)
                .and_modify(|current| {
                    if entry.request_number > current.1.request_number {
                        *current = candidate;
                    }
                })
                .or_insert(candidate);
        }

        if table.len() != latest.len() {
            return Err(InvariantViolation {
                invariant: "client_table_consistency",
                replica: snapshot.replica_number,
                details: format!(
                    "client table has {} entries, but log has {} distinct clients",
                    table.len(),
                    latest.len(),
                ),
            });
        }

        for (client_id, (log_index, logged)) in latest {
            let Some(client_entry) = table.get(&client_id) else {
                return Err(InvariantViolation {
                    invariant: "client_table_consistency",
                    replica: snapshot.replica_number,
                    details: format!("client={client_id} is missing from the client table"),
                });
            };

            if client_entry.request_number != logged.request_number || client_entry.op != logged.op
            {
                return Err(InvariantViolation {
                    invariant: "client_table_consistency",
                    replica: snapshot.replica_number,
                    details: format!(
                        "client={client_id} table entry does not match its latest logged request"
                    ),
                });
            }

            let should_be_completed = log_index < snapshot.commit_number;
            if client_entry.result.is_some() != should_be_completed {
                return Err(InvariantViolation {
                    invariant: "client_table_consistency",
                    replica: snapshot.replica_number,
                    details: format!(
                        "client={client_id} request={} completion state disagrees with commit_number={}",
                        logged.request_number, snapshot.commit_number,
                    ),
                });
            }

            if should_be_completed {
                let executed = &snapshot.executed_requests[log_index];
                if client_entry.result.as_ref() != Some(&executed.result) {
                    return Err(InvariantViolation {
                        invariant: "client_table_consistency",
                        replica: snapshot.replica_number,
                        details: format!(
                            "client={client_id} request={} result disagrees with its execution",
                            logged.request_number,
                        ),
                    });
                }
            }
        }

        Ok(())
    }

    fn check_commit_within_log(snapshot: &Snapshot) -> Result<(), InvariantViolation> {
        if snapshot.commit_number > snapshot.log.len() {
            return Err(InvariantViolation {
                invariant: "commit_within_log",
                replica: snapshot.replica_number,
                details: format!(
                    "commit_number={} exceeds log length={}",
                    snapshot.commit_number,
                    snapshot.log.len(),
                ),
            });
        }

        Ok(())
    }

    fn check_same_committed_prefix(&self, current: &Snapshot) -> Result<(), InvariantViolation> {
        for (other_id, other) in &self.snapshots {
            if *other_id == current.replica_number {
                continue;
            }

            let shared_commit = current.commit_number.min(other.commit_number);

            for index in 0..shared_commit {
                let current_entry = &current.log[index];
                let other_entry = &other.log[index];

                if current_entry != other_entry {
                    return Err(InvariantViolation {
                        invariant: "same_committed_prefix",
                        replica: current.replica_number,
                        details: format!(
                            "replicas {} and {} disagree at committed op {}: \
                         current={:?}, other={:?}",
                            current.replica_number,
                            other.replica_number,
                            index + 1,
                            current_entry,
                            other_entry,
                        ),
                    });
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use vr_replica::{
        replica::Status,
        snapshot::{ClientTableEntrySnapshot, ExecutedRequestSnapshot, LogEntrySnapshot},
    };

    use super::*;

    fn snapshot(replica: ReplicaId, view_number: ReplicaId, commit_number: usize) -> Snapshot {
        let log = (1..=commit_number)
            .map(|op_number| log_entry(op_number, op_number as u64, 1))
            .collect();

        consistent_snapshot(replica, view_number, commit_number, log)
    }

    fn consistent_snapshot(
        replica: ReplicaId,
        view_number: ReplicaId,
        commit_number: usize,
        log: Vec<LogEntrySnapshot<Op>>,
    ) -> Snapshot {
        let mut client_table = BTreeMap::new();
        for entry in &log {
            client_table.insert(
                entry.client_id,
                ClientTableEntrySnapshot {
                    client_id: entry.client_id,
                    request_number: entry.request_number,
                    op: entry.op.clone(),
                    result: (entry.op_number <= commit_number).then(|| entry.op.clone()),
                },
            );
        }
        let executed_requests = log
            .iter()
            .take(commit_number)
            .map(|entry| ExecutedRequestSnapshot {
                client_id: entry.client_id,
                request_number: entry.request_number,
                op: entry.op.clone(),
                result: entry.op.clone(),
            })
            .collect();

        ReplicaSnapshot {
            replica_number: replica,
            epoch: 0,
            status: Status::Normal,
            view_number,
            op_number: log.len(),
            commit_number,
            log,
            client_table: client_table.into_values().collect(),
            executed_requests,
        }
    }

    fn snapshot_with_log(replica: ReplicaId, log: Vec<LogEntrySnapshot<Op>>) -> Snapshot {
        consistent_snapshot(replica, 0, 0, log)
    }

    fn snapshot_with_committed_log(
        replica: ReplicaId,
        commit_number: usize,
        log: Vec<LogEntrySnapshot<Op>>,
    ) -> Snapshot {
        consistent_snapshot(replica, 0, commit_number, log)
    }

    fn log_entry(op_number: usize, client_id: u64, request_number: usize) -> LogEntrySnapshot<Op> {
        LogEntrySnapshot {
            op_number,
            client_id,
            request_number,
            op: Op::Set(format!("key-{op_number}"), op_number as u64),
        }
    }

    #[test]
    fn accepts_reply_for_committed_request() {
        let mut checker = StateChecker::new();

        checker
            .observe_snapshot(snapshot_with_committed_log(0, 1, vec![log_entry(1, 7, 3)]))
            .unwrap();

        assert_eq!(checker.invariant_reply_implies_committed(0, 7, 3), Ok(()));
    }

    #[test]
    fn rejects_reply_for_uncommitted_request() {
        let mut checker = StateChecker::new();

        checker
            .observe_snapshot(snapshot_with_log(0, vec![log_entry(1, 7, 3)]))
            .unwrap();

        let violation = checker
            .invariant_reply_implies_committed(0, 7, 3)
            .unwrap_err();

        assert_eq!(violation.invariant, "reply_implies_committed");
        assert_eq!(violation.replica, 0);
    }

    #[test]
    fn rejects_reply_for_request_absent_from_log() {
        let mut checker = StateChecker::new();

        checker.observe_snapshot(snapshot(0, 0, 0)).unwrap();

        assert!(checker.invariant_reply_implies_committed(0, 7, 3).is_err());
    }

    #[test]
    fn commit_number_may_increase() {
        let mut checker = StateChecker::new();

        checker.observe_snapshot(snapshot(0, 0, 1)).unwrap();
        checker.observe_snapshot(snapshot(0, 0, 2)).unwrap();
    }

    #[test]
    fn commit_number_may_remain_equal() {
        let mut checker = StateChecker::new();

        checker.observe_snapshot(snapshot(0, 0, 1)).unwrap();
        checker.observe_snapshot(snapshot(0, 0, 1)).unwrap();
    }

    #[test]
    fn commit_number_must_not_decrease() {
        let mut checker = StateChecker::new();

        checker.observe_snapshot(snapshot(0, 0, 2)).unwrap();

        let violation = checker.observe_snapshot(snapshot(0, 0, 1)).unwrap_err();

        assert_eq!(violation.invariant, "commit_monotonicity");
        assert_eq!(violation.replica, 0);
    }

    #[test]
    fn view_number_may_increase() {
        let mut checker = StateChecker::new();

        checker.observe_snapshot(snapshot(0, 1, 0)).unwrap();
        checker.observe_snapshot(snapshot(0, 2, 0)).unwrap();
    }

    #[test]
    fn view_number_may_remain_equal() {
        let mut checker = StateChecker::new();

        checker.observe_snapshot(snapshot(0, 1, 0)).unwrap();
        checker.observe_snapshot(snapshot(0, 1, 0)).unwrap();
    }

    #[test]
    fn view_number_must_not_decrease() {
        let mut checker = StateChecker::new();

        checker.observe_snapshot(snapshot(0, 2, 0)).unwrap();

        let violation = checker.observe_snapshot(snapshot(0, 1, 0)).unwrap_err();

        assert_eq!(violation.invariant, "view_monotonicity");
        assert_eq!(violation.replica, 0);
    }

    #[test]
    fn epoch_may_increase() {
        let mut checker = StateChecker::new();
        let mut first = snapshot(0, 0, 0);
        first.epoch = 1;
        let mut second = snapshot(0, 0, 0);
        second.epoch = 2;

        checker.observe_snapshot(first).unwrap();
        checker.observe_snapshot(second).unwrap();
    }

    #[test]
    fn epoch_must_not_decrease() {
        let mut checker = StateChecker::new();
        let mut first = snapshot(0, 0, 0);
        first.epoch = 2;
        let mut second = snapshot(0, 0, 0);
        second.epoch = 1;

        checker.observe_snapshot(first).unwrap();
        let violation = checker.observe_snapshot(second).unwrap_err();

        assert_eq!(violation.invariant, "epoch_monotonicity");
        assert_eq!(violation.replica, 0);
    }

    #[test]
    fn monotonicity_is_tracked_per_replica() {
        let mut checker = StateChecker::new();

        checker.observe_snapshot(snapshot(0, 0, 5)).unwrap();
        checker.observe_snapshot(snapshot(1, 0, 1)).unwrap();

        checker.observe_snapshot(snapshot(0, 0, 6)).unwrap();
        checker.observe_snapshot(snapshot(1, 0, 2)).unwrap();
    }

    #[test]
    fn rejected_snapshot_does_not_replace_previous_snapshot() {
        let mut checker = StateChecker::new();

        checker.observe_snapshot(snapshot(0, 0, 2)).unwrap();
        checker.observe_snapshot(snapshot(0, 0, 1)).unwrap_err();

        // Must still compare against commit 2, not the rejected commit 1.
        let violation = checker.observe_snapshot(snapshot(0, 0, 1)).unwrap_err();

        assert_eq!(violation.invariant, "commit_monotonicity");
    }

    #[test]
    fn empty_log_has_no_duplicate_request() {
        let mut checker = StateChecker::new();

        checker
            .observe_snapshot(snapshot_with_log(0, vec![]))
            .unwrap();
    }

    #[test]
    fn same_request_number_from_different_clients_is_allowed() {
        let mut checker = StateChecker::new();
        let snapshot = snapshot_with_log(0, vec![log_entry(1, 10, 7), log_entry(2, 20, 7)]);

        checker.observe_snapshot(snapshot).unwrap();
    }

    #[test]
    fn different_request_numbers_from_same_client_are_allowed() {
        let mut checker = StateChecker::new();
        let snapshot = snapshot_with_log(0, vec![log_entry(1, 10, 7), log_entry(2, 10, 8)]);

        checker.observe_snapshot(snapshot).unwrap();
    }

    #[test]
    fn duplicate_request_in_first_snapshot_is_rejected() {
        let mut checker = StateChecker::new();
        let snapshot = snapshot_with_log(3, vec![log_entry(1, 10, 7), log_entry(2, 10, 7)]);

        let violation = checker.observe_snapshot(snapshot).unwrap_err();

        assert_eq!(violation.invariant, "duplicate_request_in_log");
        assert_eq!(violation.replica, 3);
        assert!(violation.details.contains("client=10"));
        assert!(violation.details.contains("request=7"));
    }

    #[test]
    fn duplicate_execution_is_rejected() {
        let mut checker = StateChecker::new();
        let mut malformed = snapshot_with_committed_log(3, 1, vec![log_entry(1, 10, 7)]);
        malformed
            .executed_requests
            .push(malformed.executed_requests[0].clone());

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "at_most_once_execution");
        assert_eq!(violation.replica, 3);
    }

    #[test]
    fn missing_execution_is_rejected() {
        let mut checker = StateChecker::new();
        let mut malformed = snapshot_with_committed_log(3, 1, vec![log_entry(1, 10, 7)]);
        malformed.executed_requests.clear();

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "execution_matches_committed_prefix");
    }

    #[test]
    fn execution_of_uncommitted_request_is_rejected() {
        let mut checker = StateChecker::new();
        let mut malformed = snapshot_with_log(3, vec![log_entry(1, 10, 7)]);
        malformed.executed_requests.push(ExecutedRequestSnapshot {
            client_id: 10,
            request_number: 7,
            op: Op::Set("key-1".into(), 1),
            result: Op::Set("key-1".into(), 1),
        });

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "execution_matches_committed_prefix");
    }

    #[test]
    fn reordered_execution_is_rejected() {
        let mut checker = StateChecker::new();
        let mut malformed =
            snapshot_with_committed_log(3, 2, vec![log_entry(1, 10, 7), log_entry(2, 20, 8)]);
        malformed.executed_requests.swap(0, 1);

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "execution_matches_committed_prefix");
    }

    #[test]
    fn wrong_executed_operation_is_rejected() {
        let mut checker = StateChecker::new();
        let mut malformed = snapshot_with_committed_log(3, 1, vec![log_entry(1, 10, 7)]);
        malformed.executed_requests[0].op = Op::Set("different".into(), 99);

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "execution_matches_committed_prefix");
    }

    #[test]
    fn missing_client_table_entry_is_rejected() {
        let mut checker = StateChecker::new();
        let mut malformed = snapshot_with_log(3, vec![log_entry(1, 10, 7)]);
        malformed.client_table.clear();

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "client_table_consistency");
    }

    #[test]
    fn extra_client_table_entry_is_rejected() {
        let mut checker = StateChecker::new();
        let mut malformed = snapshot_with_log(3, vec![log_entry(1, 10, 7)]);
        malformed.client_table.push(ClientTableEntrySnapshot {
            client_id: 20,
            request_number: 1,
            op: Op::Set("extra".into(), 1),
            result: None,
        });

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "client_table_consistency");
    }

    #[test]
    fn duplicate_client_table_entry_is_rejected() {
        let mut checker = StateChecker::new();
        let mut malformed = snapshot_with_log(3, vec![log_entry(1, 10, 7)]);
        malformed
            .client_table
            .push(malformed.client_table[0].clone());

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "client_table_consistency");
        assert!(violation.details.contains("appears more than once"));
    }

    #[test]
    fn stale_client_table_request_is_rejected() {
        let mut checker = StateChecker::new();
        let mut malformed = snapshot_with_log(3, vec![log_entry(1, 10, 7), log_entry(2, 10, 8)]);
        malformed.client_table[0].request_number = 7;

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "client_table_consistency");
    }

    #[test]
    fn mismatched_client_table_operation_is_rejected() {
        let mut checker = StateChecker::new();
        let mut malformed = snapshot_with_log(3, vec![log_entry(1, 10, 7)]);
        malformed.client_table[0].op = Op::Set("different".into(), 99);

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "client_table_consistency");
    }

    #[test]
    fn committed_client_table_entry_requires_result() {
        let mut checker = StateChecker::new();
        let mut malformed = snapshot_with_committed_log(3, 1, vec![log_entry(1, 10, 7)]);
        malformed.client_table[0].result = None;

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "client_table_consistency");
    }

    #[test]
    fn committed_client_table_result_must_match_execution() {
        let mut checker = StateChecker::new();
        let mut malformed = snapshot_with_committed_log(3, 1, vec![log_entry(1, 10, 7)]);
        malformed.client_table[0].result = Some(Op::Set("wrong-result".into(), 99));

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "client_table_consistency");
    }

    #[test]
    fn uncommitted_client_table_entry_must_not_have_result() {
        let mut checker = StateChecker::new();
        let mut malformed = snapshot_with_log(3, vec![log_entry(1, 10, 7)]);
        malformed.client_table[0].result = Some(Op::Set("result".into(), 1));

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "client_table_consistency");
    }

    #[test]
    fn equal_committed_prefixes_are_allowed() {
        let mut checker = StateChecker::new();
        let log = vec![log_entry(1, 10, 7), log_entry(2, 20, 7)];

        checker
            .observe_snapshot(snapshot_with_committed_log(0, 2, log.clone()))
            .unwrap();
        checker
            .observe_snapshot(snapshot_with_committed_log(1, 2, log))
            .unwrap();
    }

    #[test]
    fn different_uncommitted_suffixes_are_allowed() {
        let mut checker = StateChecker::new();

        checker
            .observe_snapshot(snapshot_with_committed_log(
                0,
                1,
                vec![log_entry(1, 10, 7), log_entry(2, 20, 7)],
            ))
            .unwrap();
        checker
            .observe_snapshot(snapshot_with_committed_log(
                1,
                1,
                vec![log_entry(1, 10, 7), log_entry(2, 30, 7)],
            ))
            .unwrap();
    }

    #[test]
    fn different_commit_numbers_with_equal_shared_prefix_are_allowed() {
        let mut checker = StateChecker::new();

        checker
            .observe_snapshot(snapshot_with_committed_log(
                0,
                2,
                vec![log_entry(1, 10, 7), log_entry(2, 20, 7)],
            ))
            .unwrap();
        checker
            .observe_snapshot(snapshot_with_committed_log(1, 1, vec![log_entry(1, 10, 7)]))
            .unwrap();
    }

    #[test]
    fn conflicting_committed_entries_are_rejected() {
        let mut checker = StateChecker::new();

        checker
            .observe_snapshot(snapshot_with_committed_log(
                0,
                2,
                vec![log_entry(1, 10, 7), log_entry(2, 20, 7)],
            ))
            .unwrap();
        let violation = checker
            .observe_snapshot(snapshot_with_committed_log(
                1,
                2,
                vec![log_entry(1, 10, 7), log_entry(2, 30, 7)],
            ))
            .unwrap_err();

        assert_eq!(violation.invariant, "same_committed_prefix");
        assert_eq!(violation.replica, 1);
        assert!(violation.details.contains("committed op 2"));
    }

    #[test]
    fn conflicting_committed_operations_are_rejected() {
        let mut checker = StateChecker::new();
        let first = log_entry(1, 10, 7);
        let mut conflicting = first.clone();
        conflicting.op = Op::Set("different".into(), 99);

        checker
            .observe_snapshot(snapshot_with_committed_log(0, 1, vec![first]))
            .unwrap();
        let violation = checker
            .observe_snapshot(snapshot_with_committed_log(1, 1, vec![conflicting]))
            .unwrap_err();

        assert_eq!(violation.invariant, "same_committed_prefix");
    }

    #[test]
    fn conflict_inside_differently_sized_committed_prefix_is_rejected() {
        let mut checker = StateChecker::new();

        checker
            .observe_snapshot(snapshot_with_committed_log(
                0,
                3,
                vec![
                    log_entry(1, 10, 7),
                    log_entry(2, 20, 7),
                    log_entry(3, 30, 7),
                ],
            ))
            .unwrap();
        let violation = checker
            .observe_snapshot(snapshot_with_committed_log(
                1,
                2,
                vec![log_entry(1, 10, 7), log_entry(2, 40, 7)],
            ))
            .unwrap_err();

        assert_eq!(violation.invariant, "same_committed_prefix");
        assert!(violation.details.contains("committed op 2"));
    }

    #[test]
    fn commit_number_beyond_log_is_rejected_without_panicking() {
        let mut checker = StateChecker::new();
        let malformed = snapshot_with_committed_log(3, 2, vec![log_entry(1, 10, 7)]);

        let violation = checker.observe_snapshot(malformed).unwrap_err();

        assert_eq!(violation.invariant, "commit_within_log");
        assert_eq!(violation.replica, 3);
    }

    #[test]
    fn rejected_conflicting_prefix_does_not_replace_previous_snapshot() {
        let mut checker = StateChecker::new();
        let valid_log = vec![log_entry(1, 10, 7)];

        checker
            .observe_snapshot(snapshot_with_committed_log(0, 1, valid_log.clone()))
            .unwrap();
        checker
            .observe_snapshot(snapshot_with_committed_log(1, 1, valid_log.clone()))
            .unwrap();
        checker
            .observe_snapshot(snapshot_with_committed_log(1, 1, vec![log_entry(1, 20, 7)]))
            .unwrap_err();

        checker
            .observe_snapshot(snapshot_with_committed_log(2, 1, valid_log))
            .unwrap();
    }
}
