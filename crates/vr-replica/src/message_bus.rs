use crate::{message::Message, state_machine::StateMachine, types::ReplicaId};

pub trait MessageBus<SM: StateMachine> {
    fn send(&self, to: ReplicaId, message: Message<SM>);

    fn broadcast(&self, to: Vec<ReplicaId>, message: Message<SM>) {
        for replica_id in to {
            self.send(replica_id, message.clone());
        }
    }
}
