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

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Coordinate startup with a local Argone SSH session.
    Relay,
    /// Serve the Model Context Protocol, forwarding to the running analyzer
    /// whose workspace contains the working directory.
    Mcp {
        /// Directory to find a session for, instead of the working directory.
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
        None => crate::main(args.rpc_port, args.relay_socket).await,
    }
}

/// Parses the `PORT TOKEN` line the analyzer writes to the relay socket.
fn parse_port_and_token(line: &str) -> Option<(u16, String)> {
    let mut fields = line.split_whitespace();
    let port = fields
        .next()?
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)?;
    let token = crate::transport::SessionToken::from_hex(fields.next()?)?;
    fields.next().is_none().then(|| (port, token.to_hex()))
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
    let (port, token) = parse_port_and_token(&line).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid analyzer RPC announcement",
        )
    })?;
    // Stdout is the SSH channel back to Argone, so the token stays private.
    stdout
        .write_all(format!("ARGON_ANALYZER 2 {port} {token}\n").as_bytes())
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

#[cfg(test)]
mod tests {
    use super::parse_port_and_token;

    #[test]
    fn relay_announcements_carry_a_port_and_a_token() {
        let token = "ab".repeat(32);
        assert_eq!(
            parse_port_and_token(&format!("1234 {token}\n")),
            Some((1234, token.clone()))
        );
        assert_eq!(parse_port_and_token("1234\n"), None);
        assert_eq!(parse_port_and_token(&format!("0 {token}")), None);
        assert_eq!(parse_port_and_token("1234 not-a-token"), None);
        assert_eq!(parse_port_and_token(&format!("1234 {token} extra")), None);
    }
}
