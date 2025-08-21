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
pub struct Client<T> {
    pub id: NodeId,
    pub state: T,
}

type State = HashMap<String, u64>;
impl StateMachine for Client<State> {
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