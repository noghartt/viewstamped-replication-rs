use vr_replica::{clock::TimerKind, message::Message};

use crate::client::Op;

pub enum Event<Input: Clone> {
    Msg(Message<Input, Op>),        
    TimerFired(TimerKind),
}
