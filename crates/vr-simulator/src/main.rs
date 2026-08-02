use std::collections::HashMap;
use std::sync::Once;
use std::{cell::RefCell, rc::Rc};

use clap::{Args, Parser};

mod client;
mod history;
mod invariants;
mod network;
mod simulator;
mod types;

use rand::RngExt;
use simulator::Simulator;
use types::NodeId;
use vr_replica::{replica::Replica, state_machine::StateMachine};

use crate::client::{Client, Op};
use crate::invariants::InvariantViolation;
use crate::simulator::{SimulatorConfig, SimulatorRunOutcome};

#[derive(Parser, Debug)]
#[command(
    version,
    about = "Run deterministic Viewstamped Replication simulations"
)]
struct Cli {
    #[command(flatten)]
    modes: Modes,

    #[command(flatten)]
    config: CliConfig,
}

#[derive(Args, Debug)]
// Creates a group where only 1 argument is allowed, and at least 1 is required
#[group(required = true, multiple = false)]
struct Modes {
    #[arg(short, long, value_name = "SEED")]
    seed: Option<u64>,

    #[arg(
        long = "max-samples",
        value_name = "COUNT",
        value_parser = clap::value_parser!(u64).range(1..)
    )]
    max_samples: Option<u64>,
}

// TODO: Fix the CLI configs to match the SimulatorConfig implementation.
#[derive(Args, Clone, Debug)]
struct CliConfig {
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u64).range(1..))]
    replicas: u64,

    #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u64).range(1..))]
    clients: u64,

    #[arg(long = "max-time")]
    run_until_max_time: Option<u64>,

    #[arg(long = "max-events")]
    run_until_max_events: Option<u64>,

    #[arg(default_value_t = 1)]
    link_base_ms: u64,

    #[arg(default_value_t = 0)]
    link_jitter_ms: u64,

    #[arg(default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=100))]
    link_drop_pct: u8,

    #[arg(default_value_t = 0, value_parser = clap::value_parser!(u8).range(0..=100))]
    link_dup_pct: u8,

    #[arg(long, default_value_t = false)]
    disable_timers: bool,

    #[arg(long, default_value_t = false)]
    history: bool,
}

enum Mode {
    Single(u64),
    MaxSamples(u64),
}

fn main() -> std::process::ExitCode {
    init_tracing();

    let args = Cli::parse();

    let mode = get_mode(args.modes);
    let result = match mode {
        Mode::Single(seed) => run_single_simulation(seed, &args.config),
        Mode::MaxSamples(max_samples) => run_max_samples_simulations(max_samples, &args.config),
    };

    match result {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(_) => std::process::ExitCode::FAILURE,
    }
}

fn run_single_simulation(seed: u64, config: &CliConfig) -> Result<(), InvariantViolation> {
    let mut simulator = setup_simulation(seed, config);
    match simulator.run() {
        Ok(outcome) => {
            print_simulation_summary(seed, &simulator, outcome, config.history);
            Ok(())
        }
        Err(violation) => {
            eprintln!("simulation failed");
            eprintln!("seed={seed}");
            eprintln!("invariant={}", violation.invariant);
            eprintln!("replica={}", violation.replica);
            eprintln!("details={}", violation.details);
            eprintln!();
            eprintln!("{}", simulator.history);

            Err(violation)
        }
    }
}

fn run_max_samples_simulations(
    max_samples: u64,
    config: &CliConfig,
) -> Result<(), InvariantViolation> {
    for sample in 0..max_samples {
        let seed = rand::random();

        println!("running sample={}/{} seed={seed}", sample + 1, max_samples);

        run_single_simulation(seed, config)?;
    }

    Ok(())
}

fn setup_simulation(seed: u64, config: &CliConfig) -> Simulator<Op> {
    let simulator_config = SimulatorConfig {
        disable_timers: config.disable_timers,
        run_until_max_time: config.run_until_max_time,
        run_until_max_events: config.run_until_max_events,
    };

    let mut simulator = Simulator::with_seed(seed, Some(simulator_config));
    let replica_ids = replica_ids(config.replicas);
    let client_ids = client_ids(config.clients);
    let replica_configuration = replica_ids.iter().map(|id| id.0).collect::<Vec<_>>();

    for replica_id in &replica_ids {
        let state_machine = Rc::new(RefCell::new(ReplicaState::default()));
        let replica = Replica::new(replica_configuration.clone(), replica_id.0, state_machine);
        simulator.add_replica(*replica_id, replica);
    }

    for client_id in &client_ids {
        let client = Client::new(*client_id, replica_configuration.clone());
        simulator.add_client(*client_id, client);
    }

    simulator.create_network_mesh();

    start_seeded_workload(&mut simulator, &client_ids);

    simulator
}

fn get_mode(modes: Modes) -> Mode {
    match modes {
        Modes {
            seed: Some(seed),
            max_samples: None,
        } => Mode::Single(seed),
        Modes {
            seed: None,
            max_samples: Some(max_samples),
        } => Mode::MaxSamples(max_samples),
        _ => panic!("You should pick either single or simulation mode."),
    }
}

fn replica_ids(count: u64) -> Vec<NodeId> {
    (0..count).map(NodeId).collect()
}

fn client_ids(count: u64) -> Vec<NodeId> {
    (0..count).map(NodeId).collect()
}

fn start_seeded_workload(simulator: &mut Simulator<Op>, clients: &[NodeId]) {
    for client_id in clients {
        let key = format!("client-{}", client_id.0);
        // NOTE: I'm not sure if we need the .clone() here.
        let started = simulator.start_client_request(
            *client_id,
            Op::Set(key, simulator.rng.clone().random_range(1..100)),
        );

        assert!(
            started,
            "client {:?} should exist before workload setup",
            client_id
        );
    }
}

fn print_simulation_summary(
    seed: u64,
    simulator: &Simulator<Op>,
    outcome: SimulatorRunOutcome,
    log_history: bool,
) {
    println!(
        "finished simulation outcome={:?} seed={seed} now={}",
        outcome, simulator.now,
    );

    for client in simulator.get_clients() {
        println!("client={} state={:?}", client.id.0, client.state);
    }

    if log_history {
        println!("{}", simulator.history);
    }
}

#[derive(Debug, Default)]
struct ReplicaState {
    state: HashMap<String, u64>,
}

impl StateMachine for ReplicaState {
    type Input = Op;
    type Output = Op;

    fn apply(&mut self, input: Self::Input) -> Self::Output {
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

fn get_log_level() -> tracing::Level {
    let Ok(log_env) = std::env::var("LOG_LEVEL") else {
        return tracing::Level::INFO;
    };

    match log_env.as_str() {
        "debug" => tracing::Level::DEBUG,
        "error" => tracing::Level::ERROR,
        "warn" => tracing::Level::WARN,
        "trace" => tracing::Level::TRACE,
        _ => tracing::Level::INFO,
    }
}

static TRACING: Once = Once::new();
fn init_tracing() {
    TRACING.call_once(|| {
        tracing_subscriber::fmt()
            .with_max_level(get_log_level())
            .init()
    });
}
