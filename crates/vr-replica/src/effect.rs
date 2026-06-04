use crate::message::Message;
use crate::types::{OpNumber, ReplicaId};

#[derive(Debug)]
pub enum Effect<I, O> {
    Send {
        to: ReplicaId,
        message: Message<I, O>,
    },
    Reply {
        client_id: u64,
        message: Message<I, O>,
    },
    RequestReceived {
        replica: ReplicaId,
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
