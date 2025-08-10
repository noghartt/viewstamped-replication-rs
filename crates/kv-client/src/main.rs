use std::io::{stdin, stdout, Write};

use vr_proxy::Proxy;
use clap::Parser;

#[derive(Parser)]
struct Args {
    #[clap(short, long)]
    addr: String,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    let proxy = Proxy::new(&args.addr).await.unwrap();

    println!("Connected to proxy! {}", proxy.id);

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

        match line.trim() {
            "exit" => break,
            _ => {
                println!("ERROR: Unknown command");
                continue;
           },
        }
    }
}
