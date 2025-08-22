use vr_replica::{clock::TimerKind, message::Message, state_machine::StateMachine};

pub enum Event<Input: Clone, Output: Clone> {
    Msg(Message<Input, Output>),        
    TimerFired(TimerKind),     
}

pub enum ClientEvent<SM: StateMachine> {
    Reply { client_id: String, request_id: u64, result: SM::Output },
}
