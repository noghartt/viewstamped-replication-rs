use crate::message::Message;
use crate::types::{OpNumber, ReplicaId};

#[derive(Debug, PartialEq, Eq)]
pub enum Effect<I, O> {
    Send {
        to: ReplicaId,
        message: Message<I, O>,
    },
    Reply {
        client_id: u64,
        message: Message<I, O>,
    },
    Committed {
        replica: ReplicaId,
        op: OpNumber,
    },
    Prepared {
        replica: ReplicaId,
        op: OpNumber,
    },
}
