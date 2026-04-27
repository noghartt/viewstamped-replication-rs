use vr_replica::message::Message;

use crate::client::Op;

#[derive(Debug)]
pub enum Event<Input: Clone> {
    Msg(Message<Input, Op>),
}
