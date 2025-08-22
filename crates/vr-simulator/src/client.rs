use std::collections::HashMap;

use vr_replica::state_machine::StateMachine;

use crate::simulator::NodeId;

#[derive(Debug, Clone)]
pub enum Op {
    Set(String, u64),
    Get(String),
    Del(String),
}

#[derive(Debug, Clone)]
pub struct Client {
    pub id: NodeId,
    pub state: HashMap<String, u64>,

    /// A sorted array containing the IP addresses of the replicas in the system.
    pub configuration: Vec<String>,
    /// The current view number. The primary replica is the one with the index `current_view` in the `configuration` array.
    pub current_view: usize,
    /// The current request number. For future requests, it should ensure to be greater than the previous request number.
    pub request_number: u64,
    /// The current epoch number of the replica group.
    pub epoch: usize,

    // Test related fields
    pub inflight_requests: Vec<Op>,
}

impl Client {
    pub fn new(id: NodeId) -> Self {
        Self { 
            id, 
            state: HashMap::new(), 
            configuration: vec![], 
            current_view: 0, 
            request_number: 0, 
            epoch: 0, 
            inflight_requests: vec![] 
        }
    }
}

impl StateMachine for Client {
    type Input = Op;
    type Output = ();

    fn apply(&mut self, input: Self::Input) -> Self::Output {
        match input {
            Op::Set(key, value) => {
                self.state.insert(key, value);
            },
            _ => todo!()
        }
    }
}