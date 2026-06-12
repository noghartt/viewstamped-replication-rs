use std::collections::BTreeMap;

use vr_replica::message::Message;

use crate::types::NodeId;

#[derive(Debug, Clone)]
pub enum Op {
    Set(String, u64),
    Get(String, Option<u64>),
    Del(String),
}

#[derive(Debug, Clone)]
pub struct Client {
    pub id: NodeId,
    pub state: BTreeMap<String, u64>,
    pub replies_received: u64,

    /// A sorted array containing the IP addresses of the replicas in the system.
    pub configuration: Vec<u64>,
    /// The current view number. The primary replica is the one with the index `current_view` in the `configuration` array.
    pub current_view: u64,
    /// The current request number. For future requests, it should ensure to be greater than the previous request number.
    pub request_number: u64,
    /// The current epoch number of the replica group.
    pub epoch: usize,
}

impl Client {
    pub fn new(id: NodeId, configuration: Vec<u64>) -> Self {
        Self {
            id,
            state: BTreeMap::new(),
            replies_received: 0,
            configuration,
            current_view: 0,
            request_number: 0,
            epoch: 0,
        }
    }

    pub fn believed_primary(&self) -> u64 {
        self.configuration[(self.current_view as usize) % self.configuration.len()]
    }

    pub fn on_message<I: std::fmt::Debug>(&mut self, message: Message<I, Op>) {
        match message {
            Message::Reply { result, .. } => {
                self.replies_received += 1;
                // TODO: Not sure if we should update the request_number only on reply.
                self.request_number += 1;
                if let Some(op) = result {
                    self.apply_op(op);
                }
            }
            // VR's rule for unexpected messages is ignore-and-drop; a panic
            // here would kill an entire seed campaign on one stray message.
            other => tracing::debug!(?other, "client ignoring unexpected message"),
        }
    }

    fn apply_op(&mut self, op: Op) {
        match op {
            Op::Set(key, value) => {
                self.state.insert(key, value);
            }
            Op::Get(_, _) => {}
            Op::Del(key) => {
                self.state.remove(&key);
            }
        }
    }
}
