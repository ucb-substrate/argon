//! Authenticated connections to the analyzer's RPC listener.
//!
//! Every connection opens with a JSON `Hello` frame carrying the session token
//! and the client's role. Only after the analyzer accepts it does the stream
//! carry tarpc traffic, so an unauthenticated peer never reaches a service.

use std::{fmt, io, net::SocketAddr, time::Duration};

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tokio::net::TcpStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

/// Environment variable that hands the session token to a launched client.
pub const TOKEN_ENV: &str = "ARGON_SESSION_TOKEN";

/// Version of the handshake and of the RPC services behind it.
pub const PROTOCOL_VERSION: u32 = 1;

/// How long a peer may take to complete the handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounds what an unauthenticated peer can make the analyzer buffer.
const MAX_HANDSHAKE_FRAME: usize = 4096;

const TOKEN_BYTES: usize = 32;

/// A stream that has completed the handshake, ready for a tarpc transport.
pub type AuthenticatedStream = Framed<TcpStream, LengthDelimitedCodec>;

/// The secret that every RPC connection to one analyzer must present.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionToken([u8; TOKEN_BYTES]);

impl SessionToken {
    pub fn generate() -> io::Result<Self> {
        let mut bytes = [0; TOKEN_BYTES];
        getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
        Ok(Self(bytes))
    }

    pub fn to_hex(&self) -> String {
        self.0.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        let text = text.trim();
        if text.len() != TOKEN_BYTES * 2 || !text.is_ascii() {
            return None;
        }
        let mut bytes = [0; TOKEN_BYTES];
        for (index, byte) in bytes.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&text[2 * index..2 * index + 2], 16).ok()?;
        }
        Some(Self(bytes))
    }

    /// Reads the token from [`TOKEN_ENV`].
    pub fn from_env() -> Option<Self> {
        std::env::var(TOKEN_ENV)
            .ok()
            .and_then(|text| Self::from_hex(&text))
    }

    /// Compares in constant time, so response timing reveals nothing about the token.
    fn matches(&self, other: &Self) -> bool {
        self.0
            .iter()
            .zip(other.0.iter())
            .fold(0u8, |difference, (a, b)| difference | (a ^ b))
            == 0
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(..)")
    }
}

/// What a connection is for, which decides the service it is served.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    /// The GUI calling the analyzer.
    Gui,
    /// The analyzer calling the GUI, over a connection the GUI opened.
    GuiCallback,
    /// An agent bridge calling the analyzer.
    Agent,
}

#[derive(Serialize, Deserialize)]
struct Hello {
    version: u32,
    token: String,
    role: Role,
}

#[derive(Serialize, Deserialize)]
enum HelloReply {
    Accepted,
    Rejected(String),
}

fn handshake_codec() -> LengthDelimitedCodec {
    LengthDelimitedCodec::builder()
        .max_frame_length(MAX_HANDSHAKE_FRAME)
        .new_codec()
}

fn into_rpc_stream(mut framed: AuthenticatedStream) -> AuthenticatedStream {
    framed.codec_mut().set_max_frame_length(usize::MAX);
    framed
}

async fn send_json(framed: &mut AuthenticatedStream, value: &impl Serialize) -> io::Result<()> {
    let bytes = serde_json::to_vec(value).map_err(io::Error::other)?;
    framed.send(Bytes::from(bytes)).await
}

async fn receive_json<T: for<'de> Deserialize<'de>>(
    framed: &mut AuthenticatedStream,
) -> io::Result<T> {
    let frame = framed.next().await.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed during the handshake",
        )
    })??;
    serde_json::from_slice(&frame)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

/// Connects to an analyzer and authenticates as `role`.
pub async fn connect(
    addr: SocketAddr,
    token: &SessionToken,
    role: Role,
) -> io::Result<AuthenticatedStream> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let stream = TcpStream::connect(addr).await?;
        let mut framed = Framed::new(stream, handshake_codec());
        send_json(
            &mut framed,
            &Hello {
                version: PROTOCOL_VERSION,
                token: token.to_hex(),
                role,
            },
        )
        .await?;
        match receive_json(&mut framed).await? {
            HelloReply::Accepted => Ok(into_rpc_stream(framed)),
            HelloReply::Rejected(reason) => Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the analyzer rejected the connection: {reason}"),
            )),
        }
    })
    .await
    .map_err(|_| {
        io::Error::new(
            io::ErrorKind::TimedOut,
            format!("timed out connecting to the analyzer at {addr}"),
        )
    })?
}

/// Authenticates an accepted connection, returning the role it asked for.
pub async fn accept(
    stream: TcpStream,
    token: &SessionToken,
) -> io::Result<(Role, AuthenticatedStream)> {
    accept_within(stream, token, HANDSHAKE_TIMEOUT).await
}

async fn accept_within(
    stream: TcpStream,
    token: &SessionToken,
    limit: Duration,
) -> io::Result<(Role, AuthenticatedStream)> {
    let mut framed = Framed::new(stream, handshake_codec());
    let role = tokio::time::timeout(limit, async {
        let hello: Hello = receive_json(&mut framed).await?;
        let rejection = if !SessionToken::from_hex(&hello.token).is_some_and(|t| t.matches(token))
        {
            Some("invalid session token".to_owned())
        } else if hello.version != PROTOCOL_VERSION {
            Some(format!(
                "protocol version {} is not supported; this analyzer speaks version {PROTOCOL_VERSION}",
                hello.version
            ))
        } else {
            None
        };
        if let Some(reason) = rejection {
            let _ = send_json(&mut framed, &HelloReply::Rejected(reason.clone())).await;
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
        }
        send_json(&mut framed, &HelloReply::Accepted).await?;
        Ok(hello.role)
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "handshake timed out"))??;
    Ok((role, into_rpc_stream(framed)))
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use futures::SinkExt;
    use tokio::net::TcpListener;

    use super::*;

    async fn listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    #[test]
    fn tokens_round_trip_through_hex_and_never_print() {
        let token = SessionToken::generate().unwrap();
        assert_eq!(SessionToken::from_hex(&token.to_hex()), Some(token.clone()));
        assert_ne!(SessionToken::generate().unwrap(), token);
        assert!(!format!("{token:?}").contains(&token.to_hex()));
        assert_eq!(SessionToken::from_hex("abc"), None);
        assert_eq!(SessionToken::from_hex(&"g".repeat(64)), None);
    }

    #[tokio::test]
    async fn accepts_the_session_token_and_reports_the_role() {
        let (listener, addr) = listener().await;
        let token = SessionToken::generate().unwrap();
        let server_token = token.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accept(stream, &server_token).await.map(|(role, _)| role)
        });
        connect(addr, &token, Role::Agent).await.unwrap();
        assert_eq!(server.await.unwrap().unwrap(), Role::Agent);
    }

    #[tokio::test]
    async fn rejects_a_wrong_token() {
        let (listener, addr) = listener().await;
        let token = SessionToken::generate().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accept(stream, &token).await.map(|(role, _)| role)
        });
        let error = connect(addr, &SessionToken::generate().unwrap(), Role::Gui)
            .await
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(error.to_string().contains("invalid session token"));
        assert_eq!(
            server.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn rejects_a_peer_that_skips_the_handshake() {
        let (listener, addr) = listener().await;
        let token = SessionToken::generate().unwrap();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accept_within(stream, &token, Duration::from_millis(100))
                .await
                .map(|(role, _)| role)
        });
        let _silent = TcpStream::connect(addr).await.unwrap();
        assert_eq!(
            server.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::TimedOut
        );
    }

    #[tokio::test]
    async fn rejects_oversized_and_malformed_hellos() {
        let (listener, addr) = listener().await;
        let token = SessionToken::generate().unwrap();
        let server = tokio::spawn(async move {
            let mut results = Vec::new();
            for _ in 0..2 {
                let (stream, _) = listener.accept().await.unwrap();
                results.push(accept(stream, &token).await.map(|(role, _)| role));
            }
            results
        });
        let mut oversized = Framed::new(
            TcpStream::connect(addr).await.unwrap(),
            LengthDelimitedCodec::new(),
        );
        let _ = oversized
            .send(Bytes::from(vec![b' '; MAX_HANDSHAKE_FRAME + 1]))
            .await;
        let mut malformed = Framed::new(
            TcpStream::connect(addr).await.unwrap(),
            LengthDelimitedCodec::new(),
        );
        malformed
            .send(Bytes::from_static(b"not json"))
            .await
            .unwrap();
        let results = server.await.unwrap();
        assert!(results.iter().all(Result::is_err));
    }

    #[tokio::test]
    async fn rejects_a_mismatched_protocol_version_with_a_reason() {
        let (listener, addr) = listener().await;
        let token = SessionToken::generate().unwrap();
        let client_token = token.clone();
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accept(stream, &token).await.map(|(role, _)| role)
        });
        let mut framed = Framed::new(TcpStream::connect(addr).await.unwrap(), handshake_codec());
        send_json(
            &mut framed,
            &Hello {
                version: PROTOCOL_VERSION + 1,
                token: client_token.to_hex(),
                role: Role::Gui,
            },
        )
        .await
        .unwrap();
        let HelloReply::Rejected(reason) = receive_json(&mut framed).await.unwrap() else {
            panic!("a mismatched version should be rejected");
        };
        assert!(reason.contains("protocol version"));
        assert!(server.await.unwrap().is_err());
    }
}
