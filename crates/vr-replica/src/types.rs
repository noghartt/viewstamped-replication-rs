use crate::message::ClientRequest;

pub type ReplicaId = u64;
pub type OpNumber = usize;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogEntry<Input, Output> {
    pub op_number: OpNumber,
    pub request: ClientRequest<Input, Output>,
}
