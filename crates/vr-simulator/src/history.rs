use std::fmt;

use vr_replica::{effect::Effect, message::Message, types::ReplicaId};

use crate::{
    network::NetworkSendOutcome,
    types::{NodeId, NodeKind},
};

#[derive(Debug)]
pub enum RuntimeEvents<
    Input: Clone + std::fmt::Debug + 'static,
    Output: Clone + std::fmt::Debug + 'static,
> {
    NetworkRequest {
        from: NodeKind,
        to: NodeKind,
        outcome: NetworkSendOutcome,
        message: Message<Input, Output>,
    },
    ReplicaOperation {
        replica: ReplicaId,
        effect: Effect<Input, Output>,
    },
}

#[derive(Debug)]
pub struct History<
    Input: Clone + std::fmt::Debug + 'static,
    Output: Clone + std::fmt::Debug + 'static,
> {
    events: Vec<(u64, RuntimeEvents<Input, Output>)>,
}

impl<Input: Clone + std::fmt::Debug + 'static, Output: Clone + std::fmt::Debug + 'static>
    History<Input, Output>
{
    pub fn new() -> Self {
        Self { events: Vec::new() }
    }

    pub fn insert_history_event(&mut self, at: u64, event: RuntimeEvents<Input, Output>) {
        self.events.push((at, event));
    }
}

impl<Input: Clone + std::fmt::Debug + 'static, Output: Clone + std::fmt::Debug + 'static> Default
    for History<Input, Output>
{
    fn default() -> Self {
        Self::new()
    }
}

/// Renders a node as a short tag: `C0` for clients, `R0` for replicas.
fn node_tag(node: &NodeKind) -> String {
    match node {
        NodeKind::Client(id) => format!("C{}", id.0),
        NodeKind::Replica(id) => format!("R{}", id.0),
    }
}

fn effect_tag<Input: fmt::Debug, Output: fmt::Debug>(effect: &Effect<Input, Output>) -> String {
    match effect {
        Effect::Committed { op, .. } => format!("Committed#{}", op),
        Effect::Prepared { op, .. } => format!("Prepared#{}", op),
        _ => format!("TODO / Effect: {:?}", effect),
    }
}

/// One-line summary of a message — the variant name plus the fields that
/// matter when reading a trace, not the full `Debug` dump.
fn message_label<I: fmt::Debug, O: fmt::Debug>(message: &Message<I, O>) -> String {
    match message {
        Message::Error { message } => format!("Error({message})"),
        Message::Request(req) => {
            format!("Request#{} {:?}", req.request_number, req.op)
        }
        Message::Connect { current_view, .. } => format!("Connect(view={current_view})"),
        Message::Reply { request_id, .. } => format!("Reply#{request_id}"),
        Message::Prepare {
            op_number,
            commit_number,
            ..
        } => format!("Prepare(op={op_number}, commit={commit_number})"),
        Message::PrepareOk {
            op_number,
            replica_number,
            ..
        } => format!("PrepareOk(op={op_number}, from=R{replica_number})"),
    }
}

/// Describes what the network decided to do with a message.
fn outcome_label(outcome: &NetworkSendOutcome) -> String {
    match outcome {
        NetworkSendOutcome::Dropped => "DROPPED".to_string(),
        NetworkSendOutcome::Delivered { at } => format!("deliver @{at}"),
        NetworkSendOutcome::Duplicated { at, duplicated_at } => {
            format!("deliver @{at} +dup @{duplicated_at}")
        }
    }
}

impl<Input: Clone + fmt::Debug + 'static, Output: Clone + fmt::Debug + 'static> fmt::Display
    for History<Input, Output>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(
            f,
            "═══ Simulation History ({} events) ═══",
            self.events.len()
        )?;

        for (at, event) in &self.events {
            match event {
                RuntimeEvents::NetworkRequest {
                    from,
                    to,
                    outcome,
                    message,
                } => {
                    writeln!(
                        f,
                        "[t={at:>5}]  {:>3} ──▶ {:<3}  {:<32}  {}",
                        node_tag(from),
                        node_tag(to),
                        message_label(message),
                        outcome_label(outcome),
                    )?;
                }

                RuntimeEvents::ReplicaOperation { replica, effect } => {
                    writeln!(
                        f,
                        "[t={at:>5}]  {:>3} {:>21}",
                        node_tag(&NodeKind::Replica(NodeId(*replica))),
                        format!("● {}", effect_tag(effect))
                    )?;
                }
            }
        }

        Ok(())
    }
}
