use crate::types::ReplicaId;

#[derive(Clone, Debug)]
pub struct ClientRequest<I, O> {
    pub op: I,
    pub client_id: u64,
    pub request_number: usize,
    pub result: Option<O>,
}

#[derive(Clone, Debug)]
pub enum Message<I, O> {
  Error {
    message: String,
  },
  Request(ClientRequest<I, O>),
  Connect {
    configuration: Vec<String>,
    current_view: usize,
    epoch: usize,
  },
  Reply {
    client_id: u64,
    view_number: ReplicaId,
    request_id: usize,
    result: Option<O>,
  },
  Prepare {
    op: I,
    view_number: ReplicaId,
    op_number: usize,
    commit_number: usize,
    request: Box<ClientRequest<I, O>>,
  },
  PrepareOk {
    view_number: ReplicaId,
    replica_number: ReplicaId,
    op_number: usize,
    commit_number: usize,
  },
  Commit {
    op_number: usize,
    commit_number: usize,
    view_number: ReplicaId,
  }
}
