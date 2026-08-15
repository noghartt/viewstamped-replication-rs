use std::collections::BTreeMap;

use vr_replica::{snapshot::ReplicaSnapshot, types::ReplicaId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InvariantViolation {
    pub invariant: &'static str,
    pub replica: ReplicaId,
    pub details: String,
}

#[derive(Debug, Default)]
pub struct StateChecker {
    snapshots: BTreeMap<ReplicaId, ReplicaSnapshot>,
}

impl StateChecker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn observe_snapshot(
        &mut self,
        snapshot: ReplicaSnapshot,
    ) -> Result<(), InvariantViolation> {
        let replica = snapshot.replica_number;

        Self::check_commit_within_log(&snapshot)?;
        Self::check_no_duplicate_requests(&snapshot)?;

        if let Some(previous) = self.snapshots.get(&replica) {
            Self::check_commit_monotonicity(previous, &snapshot)?;
            Self::check_view_monotonicity(previous, &snapshot)?;
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
        previous: &ReplicaSnapshot,
        current: &ReplicaSnapshot,
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
        previous: &ReplicaSnapshot,
        current: &ReplicaSnapshot,
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

    fn check_no_duplicate_requests(snapshot: &ReplicaSnapshot) -> Result<(), InvariantViolation> {
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

    fn check_commit_within_log(snapshot: &ReplicaSnapshot) -> Result<(), InvariantViolation> {
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

    fn check_same_committed_prefix(
        &self,
        current: &ReplicaSnapshot,
    ) -> Result<(), InvariantViolation> {
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
    use vr_replica::{replica::Status, snapshot::LogEntrySnapshot};

    use super::*;

    fn snapshot(
        replica: ReplicaId,
        view_number: ReplicaId,
        commit_number: usize,
    ) -> ReplicaSnapshot {
        let log = (1..=commit_number)
            .map(|op_number| log_entry(op_number, op_number as u64, 1))
            .collect();

        ReplicaSnapshot {
            replica_number: replica,
            status: Status::Normal,
            view_number,
            op_number: commit_number,
            commit_number,
            log,
        }
    }

    fn snapshot_with_log(replica: ReplicaId, log: Vec<LogEntrySnapshot>) -> ReplicaSnapshot {
        ReplicaSnapshot {
            replica_number: replica,
            status: Status::Normal,
            view_number: 0,
            op_number: log.len(),
            commit_number: 0,
            log,
        }
    }

    fn snapshot_with_committed_log(
        replica: ReplicaId,
        commit_number: usize,
        log: Vec<LogEntrySnapshot>,
    ) -> ReplicaSnapshot {
        ReplicaSnapshot {
            replica_number: replica,
            status: Status::Normal,
            view_number: 0,
            op_number: log.len(),
            commit_number,
            log,
        }
    }

    fn log_entry(op_number: usize, client_id: u64, request_number: usize) -> LogEntrySnapshot {
        LogEntrySnapshot {
            op_number,
            client_id,
            request_number,
        }
    }

    #[test]
    fn accepts_reply_for_committed_request() {
        let mut checker = StateChecker::new();

        checker
            .observe_snapshot(ReplicaSnapshot {
                replica_number: 0,
                status: Status::Normal,
                view_number: 0,
                op_number: 1,
                commit_number: 1,
                log: vec![LogEntrySnapshot {
                    op_number: 1,
                    client_id: 7,
                    request_number: 3,
                }],
            })
            .unwrap();

        assert_eq!(checker.invariant_reply_implies_committed(0, 7, 3), Ok(()));
    }

    #[test]
    fn rejects_reply_for_uncommitted_request() {
        let mut checker = StateChecker::new();

        checker
            .observe_snapshot(ReplicaSnapshot {
                replica_number: 0,
                status: Status::Normal,
                view_number: 0,
                op_number: 1,
                commit_number: 0,
                log: vec![LogEntrySnapshot {
                    op_number: 1,
                    client_id: 7,
                    request_number: 3,
                }],
            })
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

        checker
            .observe_snapshot(ReplicaSnapshot {
                replica_number: 0,
                status: Status::Normal,
                view_number: 0,
                op_number: 0,
                commit_number: 0,
                log: vec![],
            })
            .unwrap();

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
