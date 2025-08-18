use serde::{Deserialize, Serialize};

type Op = Vec<String>;

#[derive(Debug, Serialize, Deserialize)]
pub struct PrepareData {
  pub op: Op,
  pub view_number: usize,
  pub op_number: usize,
  pub commit_number: usize,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct PrepareOkData {
  pub view_number: usize,
  pub op_number: usize,
  pub commit_number: usize,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
  Error {
    message: String,
  },
  Request { 
    op: Op,
    client_id: String,
    request_number: usize,
  },
  Connect {
    configuration: Vec<String>,
    current_view: usize,
    epoch: usize,
  },
  Reply {
    view_number: usize,
    request_id: usize,
    result: Option<Vec<String>>,
  },
  Prepare(PrepareData),
  PrepareOk(PrepareOkData),
}