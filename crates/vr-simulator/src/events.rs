use vr_replica::{clock::TimerKind, message::Message, state_machine::StateMachine};

pub enum Event<SM: StateMachine> {
    Msg(Message<SM>),        
    TimerFired(TimerKind),     
}

pub enum ClientEvent<SM: StateMachine> {
    Reply { client_id: String, request_id: u64, result: SM::Output },
}
