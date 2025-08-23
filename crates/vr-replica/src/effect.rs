use crate::clock::TimerKind;
use crate::message::Message;
use crate::types::{OpNumber, ReplicaId};

pub enum Effect<I, O> {
    Send { to: ReplicaId,  message: Message<I, O> },
    Broadcast { to: Vec<ReplicaId>, message: Message<I, O> },
    SetTimer { kind: TimerKind, at: u64 },
    CancelTimer { kind: TimerKind },
    ApplyCommited { op_number: OpNumber },
    Reply { client_id: u64, message: Message<I, O> },
}

impl<I, O> std::fmt::Debug for Effect<I, O>
where
    I: std::fmt::Debug,
    O: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Effect::Send { to, message } => write!(f, "Send {{ to: {:?}, message: {:?} }}", to, message),
            Effect::Broadcast { to, message } => write!(f, "Broadcast {{ to: {:?}, message: {:?} }}", to, message),
            Effect::SetTimer { kind, at } => write!(f, "SetTimer {{ kind: {:?}, at: {:?} }}", kind, at),
            Effect::CancelTimer { kind } => write!(f, "CancelTimer {{ kind: {:?} }}", kind),
            Effect::ApplyCommited { op_number } => write!(f, "ApplyCommited {{ op_number: {:?} }}", op_number),
            Effect::Reply { client_id, message } => write!(f, "Reply {{ client_id: {:?}, message: {:?} }}", client_id, message),
        }
    }
}
