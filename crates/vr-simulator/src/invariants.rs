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

    pub fn observe_snapshot(&mut self, snapshot: ReplicaSnapshot) {
        self.snapshots.insert(snapshot.replica_number, snapshot);
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
}

#[cfg(test)]
mod tests {
    use vr_replica::{replica::Status, snapshot::LogEntrySnapshot};

    use super::*;

    #[test]
    fn accepts_reply_for_committed_request() {
        let mut checker = StateChecker::new();

        checker.observe_snapshot(ReplicaSnapshot {
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
        });

        assert_eq!(checker.invariant_reply_implies_committed(0, 7, 3), Ok(()));
    }

    #[test]
    fn rejects_reply_for_uncommitted_request() {
        let mut checker = StateChecker::new();

        checker.observe_snapshot(ReplicaSnapshot {
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
        });

        let violation = checker
            .invariant_reply_implies_committed(0, 7, 3)
            .unwrap_err();

        assert_eq!(violation.invariant, "reply_implies_committed");
        assert_eq!(violation.replica, 0);
    }

    #[test]
    fn rejects_reply_for_request_absent_from_log() {
        let mut checker = StateChecker::new();

        checker.observe_snapshot(ReplicaSnapshot {
            replica_number: 0,
            status: Status::Normal,
            view_number: 0,
            op_number: 0,
            commit_number: 0,
            log: vec![],
        });

        assert!(checker.invariant_reply_implies_committed(0, 7, 3).is_err());
    }
}
