use crate::{message::Message, types::ReplicaId};

pub trait MessageBus<Input: Clone, Output: Clone> {
    fn send(&self, to: ReplicaId, message: Message<Input, Output>);

    fn broadcast(&self, to: Vec<ReplicaId>, message: Message<Input, Output>) {
        for replica_id in to {
            self.send(replica_id, message.clone());
        }
    }
}
