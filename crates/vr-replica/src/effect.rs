use crate::message::Message;
use crate::types::{OpNumber, ReplicaId};

pub enum Effect<I, O> {
    Send {
        to: ReplicaId,
        message: Message<I, O>,
    },
    Reply {
        client_id: u64,
        message: Message<I, O>,
    },
}

impl<I, O> std::fmt::Debug for Effect<I, O>
where
    I: std::fmt::Debug,
    O: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effect::Send { to, message } => {
                write!(f, "Send {{ to: {:?}, message: {:?} }}", to, message)
            }
            Effect::Reply { client_id, message } => write!(
                f,
                "Reply {{ client_id: {:?}, message: {:?} }}",
                client_id, message
            ),
        }
    }
}
