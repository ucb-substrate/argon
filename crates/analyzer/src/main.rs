use std::{io, path::PathBuf};

use clap::{Parser, Subcommand};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

#[derive(Debug, Parser)]
#[command(version, about)]
struct Args {
    /// RPC port for GUI connections. Omit or pass 0 to allocate one automatically.
    #[arg(long)]
    rpc_port: Option<u16>,

    /// Unix socket through which to report the bound GUI RPC port.
    #[arg(long)]
    rendezvous_socket: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Coordinate startup with a local Argone SSH session.
    Rendezvous,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();
    if matches!(args.command, Some(Command::Rendezvous)) {
        if let Err(error) = run_rendezvous().await {
            eprintln!("argon-analyzer rendezvous: {error}");
            std::process::exit(1);
        }
    } else {
        analyzer::main(args.rpc_port, args.rendezvous_socket).await;
    }
}

#[cfg(unix)]
async fn run_rendezvous() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let socket_path = directory.path().join("analyzer.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path)?;
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(format!("ARGON_RENDEZVOUS 1 {}\n", socket_path.display()).as_bytes())
        .await?;
    stdout.flush().await?;

    let (stream, _) = listener.accept().await?;
    let mut port = String::new();
    BufReader::new(stream).read_line(&mut port).await?;
    let port = port
        .trim()
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid analyzer RPC port"))?;
    stdout
        .write_all(format!("ARGON_ANALYZER 1 {port}\n").as_bytes())
        .await?;
    stdout.flush().await
}

#[cfg(not(unix))]
async fn run_rendezvous() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "rendezvous requires a Unix-like remote host",
    ))
}
