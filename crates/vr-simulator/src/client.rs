use std::collections::HashMap;

use vr_replica::{message::Message, state_machine::StateMachine};

use crate::{events::Event, simulator::NodeId};

#[derive(Debug, Clone)]
pub enum Op {
    Set(String, u64),
    Get(String, Option<u64>),
    Del(String),
}

#[derive(Debug, Clone)]
pub struct Client {
    pub id: NodeId,
    pub state: HashMap<String, u64>,

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
            state: HashMap::new(), 
            configuration, 
            current_view: 0, 
            request_number: 0, 
            epoch: 0, 
        }
    }

    pub fn on_message<I: Clone + 'static>(&mut self, ev: Event<I>) -> () {
        match ev {
            Event::Msg(m) if matches!(m, Message::Reply { .. }) => {
                let Message::Reply { result, .. } = m else {
                    panic!("Unexpected message");
                };

                // TODO: Not sure if we should update the request_number only on reply.
                self.request_number += 1;
                if let Some(op) = result {
                    self.apply_op(op);
                }
            },
            _ => panic!("Unexpected message"),
        }
    }

    fn apply_op(&mut self, op: Op) -> () {
        match op {
            Op::Set(key, value) => {
                self.state.insert(key, value);
            },
            _ => todo!()
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