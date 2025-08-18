use std::io::{stdin, stdout, Write};

use state::State;
use vr_proxy::Proxy;
use clap::Parser;

mod state;

#[derive(Parser)]
struct Args {
    #[clap(short, long)]
    addr: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let proxy = Proxy::new(&args.addr).await.unwrap();
    let mut state = State::new(proxy);

    loop {
        print!("KV Client> ");
        stdout().flush().unwrap();

        let Some(lines) = stdin().lines().next() else {
            continue;
        };

        let Ok(line) = lines else {
            println!("Error reading line");
            break;
        };

        let input = line;
        let parts = input.trim().split_whitespace().map(|s| s.to_string()).collect::<Vec<String>>();
        let Some(command) = parts.first() else {
            println!("ERROR: Invalid command");
            continue;
        };

        match command.to_lowercase().as_str() {
            "exit" => break,
            "set" => {
                let key = parts[1].clone();
                let value = parts[2].clone();
                println!("Setting key: {} to value: {}", key, value);
                let result = state.proxy.send_write_request(key, value).await;
                if let Err(e) = result {
                    println!("ERROR: {}", e);
                }
            }
            _ => {
                println!("ERROR: Unknown command");
                continue;
           },
        }
    }
}
