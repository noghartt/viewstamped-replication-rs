use crate::replica::Status;
use crate::types::{OpNumber, ReplicaId};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutedRequestSnapshot<Input, Output> {
    pub client_id: u64,
    pub request_number: usize,
    pub op: Input,
    pub result: Output,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntrySnapshot<Input> {
    pub op_number: OpNumber,
    pub client_id: u64,
    pub request_number: usize,
    pub op: Input,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientTableEntrySnapshot<Input, Output> {
    pub client_id: u64,
    pub request_number: usize,
    pub op: Input,
    pub result: Option<Output>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReplicaSnapshot<Input, Output> {
    pub replica_number: ReplicaId,
    pub epoch: u64,
    pub status: Status,
    pub view_number: ReplicaId,
    pub op_number: OpNumber,
    pub commit_number: OpNumber,
    pub log: Vec<LogEntrySnapshot<Input>>,
    pub client_table: Vec<ClientTableEntrySnapshot<Input, Output>>,
    pub executed_requests: Vec<ExecutedRequestSnapshot<Input, Output>>,
}
