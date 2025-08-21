use crate::clock::TimerKind;
use crate::message::Message;
use crate::state_machine::StateMachine;
use crate::types::{OpNumber, ReplicaId};

pub enum Effect<T: StateMachine> {
    Send { to: ReplicaId,  message: Message<T> },
    Broadcast { to: Vec<String>, message: Message<T> },
    SetTimer { kind: TimerKind, at: u64 },
    CancelTimer { kind: TimerKind },
    ApplyCommited { op_number: OpNumber },
    Reply { client_id: String, message: Message<T> },
}
