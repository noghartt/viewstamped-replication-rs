use crate::state_machine::StateMachine;
use crate::types::ReplicaId;

#[derive(Clone, Debug)]
pub struct ClientRequest<SM: StateMachine> {
    pub op: SM::Input,
    pub client_id: String,
    pub request_number: usize,
    pub result: Option<SM::Output>,
}

pub enum Message<SM: StateMachine> {
  Error {
    message: String,
  },
  Request(ClientRequest<SM>),
  Connect {
    configuration: Vec<String>,
    current_view: usize,
    epoch: usize,
  },
  Reply {
    view_number: ReplicaId,
    request_id: usize,
    result: Option<SM::Output>,
  },
  Prepare {
    op: SM::Input,
    view_number: ReplicaId,
    op_number: usize,
    commit_number: usize,
    request: Box<ClientRequest<SM>>,
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
