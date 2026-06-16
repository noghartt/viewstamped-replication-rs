use vr_replica::message::Message;

use crate::{network::NetworkSendOutcome, types::NodeKind};

#[derive(Debug)]
pub enum RuntimeEvents<Input, Output> {
    NetworkRequest {
        from: NodeKind,
        to: NodeKind,
        outcome: NetworkSendOutcome,
        message: Message<Input, Output>,
    },
}

#[derive(Debug)]
pub struct History<Input, Output> {
    events: Vec<RuntimeEvents<Input, Output>>,
}

impl<Input, Output> History<Input, Output> {
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    pub fn insert_history_event(&mut self, event: RuntimeEvents<Input, Output>) {
        self.events.push(event);
    }
}
