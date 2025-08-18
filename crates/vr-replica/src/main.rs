use tokio::net::TcpListener;
use clap::{Parser, ValueEnum};
use std::fs;
use std::process::Command;

mod replica;
mod config;
mod request;
mod message;

#[derive(ValueEnum, Clone)]
enum Mode {
    Cluster,
    Replica,
}

#[derive(Parser)]
struct Args {
    #[arg(short, long, default_value = "cluster")]
    mode: Mode,

    #[arg(short, long, default_value = "cluster.toml")]
    path: String,

    #[arg(short, long)]
    index: Option<usize>,
}

pub fn main() {
    let args = Args::parse();

    let config = fs::read_to_string(args.path.clone()).unwrap();
    let config: config::Config = toml::from_str(&config).unwrap();

    match args.mode {
        Mode::Cluster => run_cluster(config, args.path),
        Mode::Replica => {
            if let Some(index) = args.index {
                run_replica(config, index);
            } else {
                eprintln!("Replica mode requires an index");
                std::process::exit(1);
            }
        },
    }
}

fn run_cluster(config: config::Config, path: String) {
    let replica_addresses = config.replicas
        .iter()
        .map(|replica| replica.address.clone())
        .collect::<Vec<String>>();

    let exe = std::env::current_exe().expect("failed to get current executable path");
    let mut children = Vec::new();
    for index in 0..replica_addresses.len() {
        println!("Starting replica {}", index);
        let mut child = Command::new(&exe)
            .arg("--path").arg(path.clone())
            .arg("--mode").arg("replica")
            .arg("--index").arg(index.to_string())
            .spawn()
            .expect("failed to spawn replica process");

        println!("Replica {} started in process {}", index, child.id());
        children.push(child);
    }

    for mut child in children {
        let _ = child.wait();
    }
}

#[tokio::main]
async fn run_replica(config: config::Config, index: usize) {
    let replica_addresses = config.replicas
        .iter()
        .map(|replica| replica.address.clone())
        .collect::<Vec<String>>();

    let replica = replica::Replica::new(replica_addresses, index);
    let app = replica.get_router();

    let listener = TcpListener::bind(replica.address).await.unwrap();
    axum::serve(listener, app).await.unwrap();

    println!("Replica {} started", index);
}