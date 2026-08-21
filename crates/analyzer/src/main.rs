use std::path::PathBuf;

use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// RPC port for GUI connections. Omit or pass 0 to allocate one automatically.
    #[arg(long)]
    rpc_port: Option<u16>,

    /// File in which to publish the bound GUI RPC port.
    #[arg(long)]
    rpc_info: Option<PathBuf>,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    analyzer::main(args.rpc_port, args.rpc_info).await;
}
