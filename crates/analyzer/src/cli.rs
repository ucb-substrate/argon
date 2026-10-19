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
    relay_socket: Option<PathBuf>,

    /// The GUI embeds this analyzer's Neovim, so never start a separate GUI.
    #[arg(long)]
    embedded_gui: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Coordinate startup with a local Argone SSH session.
    Relay,
    /// Serve the Model Context Protocol, forwarding each tool call to one of
    /// the user's running analyzers.
    Mcp {
        /// Directory to resolve relative paths against and to look for a
        /// session in first, instead of the working directory.
        #[arg(long)]
        root: Option<PathBuf>,
    },
    /// Run as a Claude Code `PreToolUse` hook that redirects `.ar` file
    /// tools to the analyzer while a session is running.
    Hook,
}

#[tokio::main]
pub async fn run() {
    let args = Args::parse();
    match args.command {
        Some(Command::Relay) => {
            if let Err(error) = run_relay().await {
                eprintln!("argon-analyzer relay: {error}");
                std::process::exit(1);
            }
        }
        Some(Command::Mcp { root }) => {
            crate::init_logging();
            if let Err(error) = crate::mcp::run(root).await {
                eprintln!("argon-analyzer mcp: {error}");
                std::process::exit(1);
            }
        }
        Some(Command::Hook) => {
            if let Err(error) = crate::hook::run().await {
                eprintln!("argon-analyzer hook: {error}");
            }
        }
        None => {
            let gui = crate::GuiLink {
                relay_socket: args.relay_socket,
                embedded: args.embedded_gui,
            };
            crate::main(args.rpc_port, gui).await
        }
    }
}

#[cfg(unix)]
async fn run_relay() -> io::Result<()> {
    let directory = tempfile::tempdir()?;
    let socket_path = directory.path().join("analyzer.sock");
    let listener = tokio::net::UnixListener::bind(&socket_path)?;
    let mut stdout = tokio::io::stdout();
    stdout
        .write_all(format!("ARGON_RELAY 1 {}\n", socket_path.display()).as_bytes())
        .await?;
    stdout.flush().await?;

    let (stream, _) = listener.accept().await?;
    let mut line = String::new();
    BufReader::new(stream).read_line(&mut line).await?;
    let (port, token) = crate::transport::parse_announcement(&line).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid analyzer RPC announcement",
        )
    })?;
    // Stdout is the SSH channel back to Argone, so the token stays private.
    stdout
        .write_all(format!("ARGON_ANALYZER 2 {port} {}\n", token.to_hex()).as_bytes())
        .await?;
    stdout.flush().await
}

#[cfg(not(unix))]
async fn run_relay() -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "relay requires a Unix-like remote host",
    ))
}
