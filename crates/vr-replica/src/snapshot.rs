use crate::replica::Status;
use crate::types::{OpNumber, ReplicaId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntrySnapshot {
    pub op_number: OpNumber,
    pub client_id: u64,
    pub request_number: usize,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaSnapshot {
    pub replica_number: ReplicaId,
    pub status: Status,
    pub view_number: ReplicaId,
    pub op_number: OpNumber,
    pub commit_number: OpNumber,
    pub log: Vec<LogEntrySnapshot>,
}
