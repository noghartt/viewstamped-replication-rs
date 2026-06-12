//! §10 Phase A harness shell. No invariants are checked yet — these tests
//! prove the recorder wiring works and the simulation boundary is sealed.

use std::cell::RefCell;
use std::collections::BTreeMap;
use std::rc::Rc;

use vr_replica::replica::Replica;
use vr_replica::state_machine::StateMachine;
use vr_simulator::client::{Client, Op};
use vr_simulator::history::{RuntimeEvent, dump_jsonl};
use vr_simulator::network::Link;
use vr_simulator::simulator::Simulator;
use vr_simulator::types::{NodeId, NodeKind};

#[derive(Debug, Default)]
struct KvState {
    state: BTreeMap<String, u64>,
}

impl StateMachine for KvState {
    type Input = Op;
    type Output = Op;

    fn apply(&mut self, input: Op) -> Op {
        match input {
            Op::Set(key, value) => {
                self.state.insert(key.clone(), value);
                Op::Set(key, value)
            }
            Op::Get(key, _) => {
                let value = self.state.get(&key).cloned();
                Op::Get(key, value)
            }
            Op::Del(key) => {
                self.state.remove(&key);
                Op::Del(key)
            }
        }
    }
}

fn setup(seed: u64, replicas: u64) -> Simulator<Op> {
    let mut sim = Simulator::with_seed(seed, None);
    let configuration: Vec<u64> = (0..replicas).collect();
    for id in 0..replicas {
        let sm = Rc::new(RefCell::new(KvState::default()));
        sim.add_replica(NodeId(id), Replica::new(configuration.clone(), id, sm));
    }
    sim.add_client(NodeId(0), Client::new(NodeId(0), configuration));
    sim
}

fn run_one(seed: u64) -> Vec<RuntimeEvent<Op, Op>> {
    let mut sim = setup(seed, 3);
    assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 7)));
    sim.run();
    sim.history
}

/// §10 10.0: the recorder records. A run with one request must leave a
/// non-empty history containing at least the request, deliveries, and a
/// reply.
#[test]
fn scaffolding_works() {
    let history = run_one(42);
    assert!(!history.is_empty(), "history must not be empty");

    let has = |pred: fn(&RuntimeEvent<Op, Op>) -> bool| history.iter().any(pred);
    assert!(has(|e| matches!(e, RuntimeEvent::ClientRequest { .. })));
    assert!(has(|e| matches!(e, RuntimeEvent::MessageSent { .. })));
    assert!(has(|e| matches!(e, RuntimeEvent::MessageDelivered { .. })));
    assert!(has(|e| matches!(e, RuntimeEvent::ReplicaSnapshot { .. })));
    assert!(has(|e| matches!(e, RuntimeEvent::ClientReplied { .. })));
}

/// §10 10.0b: same seed → byte-identical history. This is the integrity
/// check of the simulation boundary itself — if it ever fails, stop
/// everything and find the nondeterminism leak (a HashMap iteration, an
/// unseeded RNG, wall-clock time). Every other test rests on "a bug is a
/// value (a seed), not an event".
#[test]
fn determinism_same_seed_same_history() {
    let h1 = run_one(42);
    let h2 = run_one(42);
    assert_eq!(
        format!("{h1:?}"),
        format!("{h2:?}"),
        "same seed must produce identical histories"
    );
}

/// Same boundary check under active faults: jitter, drops, and duplicates
/// all draw from the seeded RNG, so they must replay identically too.
#[test]
fn determinism_holds_under_faults() {
    let run = |seed: u64| {
        let mut sim = setup(seed, 3);
        let chaotic = Link {
            partitioned: false,
            base_ms: 1,
            jitter_ms: 10,
            drop_probability: 30,
            duplication_probability: 30,
        };
        for a in 0..3u64 {
            for b in 0..3u64 {
                sim.network_mut().set_link(
                    NodeKind::Replica(NodeId(a)),
                    NodeKind::Replica(NodeId(b)),
                    chaotic.clone(),
                );
            }
        }
        assert!(sim.start_client_request(NodeId(0), Op::Set("k".into(), 7)));
        sim.run();
        sim.history
    };

    let h1 = run(7);
    let h2 = run(7);
    assert_eq!(format!("{h1:?}"), format!("{h2:?}"));
}

/// §12.0.5 round-trip smoke test: every dumped line parses back as JSON and
/// line count equals history length.
#[test]
fn jsonl_dump_round_trips() {
    let history = run_one(42);

    let mut buf = Vec::new();
    dump_jsonl(&history, &mut buf).expect("dump must succeed");

    let text = String::from_utf8(buf).expect("dump must be valid UTF-8");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), history.len(), "one line per event");

    for (i, line) in lines.iter().enumerate() {
        let value: serde_json::Value =
            serde_json::from_str(line).unwrap_or_else(|e| panic!("line {i} unparseable: {e}"));
        assert!(
            value.get("kind").is_some(),
            "line {i} must carry a `kind` tag"
        );
        assert!(value.get("at").is_some(), "line {i} must carry `at`");
    }
}
