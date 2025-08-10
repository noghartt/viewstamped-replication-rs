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

    println!("Connected to proxy");
}
