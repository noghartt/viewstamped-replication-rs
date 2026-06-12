use std::io::Write;

use serde::Serialize;

use vr_replica::effect::Effect;
use vr_replica::message::{ClientRequest, Message};
use vr_replica::replica::Status;
use vr_replica::types::{OpNumber, ReplicaId};

use crate::types::{NodeId, NodeKind};

/// Typed trace of everything the simulator did during a run.
///
/// This is the single source of truth for a run's behavior: invariant
/// predicates (§10) and the JSONL dump (§12.0) both consume this — one
/// substrate, two consumers. `tracing` output stays human-only.
///
/// Internally tagged so each JSONL line carries `"kind": "..."` as a
/// filterable column instead of nesting under a variant key.
#[derive(Debug, Serialize)]
#[serde(tag = "kind")]
pub enum RuntimeEvent<I, O> {
    ClientRequest {
        at: u64,
        client: NodeId,
        request_number: u64,
        op: I,
    },
    MessageSent {
        at: u64,
        from: NodeKind,
        to: NodeKind,
        msg: Message<I, O>,
    },
    MessageDelivered {
        at: u64,
        to: NodeKind,
        msg: Message<I, O>,
    },
    MessageDropped {
        at: u64,
        from: NodeKind,
        to: NodeKind,
        msg: Message<I, O>,
    },
    MessageDuplicated {
        at: u64,
        from: NodeKind,
        to: NodeKind,
        msg: Message<I, O>,
    },
    EffectEmitted {
        at: u64,
        by: NodeKind,
        effect: Effect<I, O>,
    },
    ReplicaSnapshot {
        at: u64,
        node: NodeId,
        view: ReplicaId,
        op_number: usize,
        commit_number: usize,
        status: Status,
        // Full log, not just op numbers: §10.4 same_commit_same_log compares
        // prefix *contents* across replicas. Memory cost is irrelevant at our
        // scale (short runs, small clusters — see the Approach 3 rationale).
        log: Vec<(OpNumber, ClientRequest<I, O>)>,
    },
    ClientReplied {
        at: u64,
        client: NodeId,
        request_number: u64,
        result: Option<O>,
    },
}

impl<I, O> RuntimeEvent<I, O> {
    pub fn at(&self) -> u64 {
        match self {
            RuntimeEvent::ClientRequest { at, .. }
            | RuntimeEvent::MessageSent { at, .. }
            | RuntimeEvent::MessageDelivered { at, .. }
            | RuntimeEvent::MessageDropped { at, .. }
            | RuntimeEvent::MessageDuplicated { at, .. }
            | RuntimeEvent::EffectEmitted { at, .. }
            | RuntimeEvent::ReplicaSnapshot { at, .. }
            | RuntimeEvent::ClientReplied { at, .. } => *at,
        }
    }
}

/// §12.0: one JSON object per line. Streamable (a panic mid-run leaves a
/// valid prefix), tool-native (jq / pandas / DuckDB), and diffable —
/// `diff a.jsonl b.jsonl` localizes the first divergence between two runs
/// to the exact event, which is the determinism check in practice.
pub fn dump_jsonl<I: Serialize, O: Serialize>(
    events: &[RuntimeEvent<I, O>],
    w: &mut impl Write,
) -> std::io::Result<()> {
    for event in events {
        serde_json::to_writer(&mut *w, event)?;
        writeln!(w)?;
    }
    Ok(())
}
