use crate::clock::TimerKind;
use crate::message::Message;
use crate::types::{OpNumber, ReplicaId};

pub enum Effect<I, O> {
    Send { to: ReplicaId,  message: Message<I, O> },
    Broadcast { to: Vec<String>, message: Message<I, O> },
    SetTimer { kind: TimerKind, at: u64 },
    CancelTimer { kind: TimerKind },
    ApplyCommited { op_number: OpNumber },
    Reply { client_id: String, message: Message<I, O> },
}
