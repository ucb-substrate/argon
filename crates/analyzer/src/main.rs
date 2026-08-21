use clap::Parser;

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// Analyzer RPC port reserved by an Argone SSH session.
    #[arg(long, hide = true)]
    rpc_port: Option<u16>,
}

#[tokio::main]
async fn main() {
    analyzer::main(Args::parse().rpc_port).await;
}
