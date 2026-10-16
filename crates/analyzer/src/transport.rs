//! Authenticated connections to the analyzer's RPC listener.
//!
//! Every connection opens with a challenge-response handshake in which the
//! analyzer and the client each prove they hold the session token without
//! sending it. Only after both proofs check out does the stream carry tarpc
//! traffic, so neither an unauthenticated client nor an impostor listening on
//! the analyzer's port learns the token or gets trusted.

use std::{fmt, io, net::SocketAddr, time::Duration};

use bytes::Bytes;
use futures::{SinkExt, StreamExt};
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;
use tokio::net::TcpStream;
use tokio_util::codec::{Framed, LengthDelimitedCodec};

/// Environment variable that hands the session token to a launched client.
pub const TOKEN_ENV: &str = "ARGON_SESSION_TOKEN";

/// Version of the handshake and of the RPC services behind it.
pub const PROTOCOL_VERSION: u32 = 2;

/// How long a peer may take to complete the handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);

/// Bounds what an unauthenticated peer can make the analyzer buffer.
const MAX_HANDSHAKE_FRAME: usize = 4096;

/// Length of the token, the handshake nonces and the proofs.
const SECRET_BYTES: usize = 32;

type Secret = [u8; SECRET_BYTES];

/// A stream that has completed the handshake, ready for a tarpc transport.
pub type AuthenticatedStream = Framed<TcpStream, LengthDelimitedCodec>;

fn random_secret() -> io::Result<Secret> {
    let mut bytes = [0; SECRET_BYTES];
    getrandom::fill(&mut bytes).map_err(|error| io::Error::other(error.to_string()))?;
    Ok(bytes)
}

fn to_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn from_hex(text: &str) -> Option<Secret> {
    let text = text.trim();
    if text.len() != SECRET_BYTES * 2 || !text.is_ascii() {
        return None;
    }
    let mut bytes = [0; SECRET_BYTES];
    for (index, byte) in bytes.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[2 * index..2 * index + 2], 16).ok()?;
    }
    Some(bytes)
}

/// The secret that every RPC connection to one analyzer must prove it holds.
#[derive(Clone, PartialEq, Eq)]
pub struct SessionToken(Secret);

impl SessionToken {
    pub fn generate() -> io::Result<Self> {
        random_secret().map(Self)
    }

    pub fn to_hex(&self) -> String {
        to_hex(&self.0)
    }

    pub fn from_hex(text: &str) -> Option<Self> {
        from_hex(text).map(Self)
    }

    /// Reads the token from [`TOKEN_ENV`].
    pub fn from_env() -> Option<Self> {
        std::env::var(TOKEN_ENV)
            .ok()
            .and_then(|text| Self::from_hex(&text))
    }
}

impl fmt::Debug for SessionToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SessionToken(..)")
    }
}

/// Parses the `PORT TOKEN` line an analyzer writes to its relay socket.
pub fn parse_announcement(line: &str) -> Option<(u16, SessionToken)> {
    let mut fields = line.split_whitespace();
    let port = fields
        .next()?
        .parse::<u16>()
        .ok()
        .filter(|port| *port != 0)?;
    let token = SessionToken::from_hex(fields.next()?)?;
    fields.next().is_none().then_some((port, token))
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

impl Role {
    fn tag(self) -> u8 {
        match self {
            Role::Gui => 0,
            Role::GuiCallback => 1,
            Role::Agent => 2,
        }
    }
}

/// Which side of the handshake a proof comes from.
#[derive(Debug, Clone, Copy)]
enum Prover {
    Analyzer,
    Client,
}

/// What one handshake's proofs are bound to, so no proof is valid in
/// another handshake or from the other side.
struct Transcript {
    role: Role,
    client_nonce: Secret,
    analyzer_nonce: Secret,
}

impl Transcript {
    fn mac(&self, token: &SessionToken, prover: Prover) -> Hmac<Sha256> {
        let mut mac =
            Hmac::<Sha256>::new_from_slice(&token.0).expect("HMAC accepts keys of any length");
        mac.update(match prover {
            Prover::Analyzer => b"argon-analyzer",
            Prover::Client => b"argon-client",
        });
        mac.update(&PROTOCOL_VERSION.to_be_bytes());
        mac.update(&[self.role.tag()]);
        mac.update(&self.client_nonce);
        mac.update(&self.analyzer_nonce);
        mac
    }

    fn prove(&self, token: &SessionToken, prover: Prover) -> String {
        to_hex(&self.mac(token, prover).finalize().into_bytes())
    }

    /// Checks `proof` in constant time.
    fn verify(&self, token: &SessionToken, prover: Prover, proof: &str) -> bool {
        from_hex(proof).is_some_and(|proof| self.mac(token, prover).verify_slice(&proof).is_ok())
    }
}

/// The field that every protocol version's first frame carries.
#[derive(Deserialize)]
struct Versioned {
    version: u32,
}

/// The client's opening frame.
#[derive(Serialize, Deserialize)]
struct Hello {
    version: u32,
    role: Role,
    nonce: String,
}

/// The analyzer's answer to a [`Hello`], proving it holds the token.
#[derive(Serialize, Deserialize)]
enum HelloReply {
    Challenge { nonce: String, proof: String },
    Rejected(String),
}

/// The client's answer to a challenge, proving it holds the token.
#[derive(Serialize, Deserialize)]
struct Proof {
    proof: String,
}

#[derive(Serialize, Deserialize)]
enum ProofReply {
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

async fn receive_frame(framed: &mut AuthenticatedStream) -> io::Result<bytes::BytesMut> {
    framed.next().await.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "connection closed during the handshake",
        )
    })?
}

fn parse<T: for<'de> Deserialize<'de>>(frame: &[u8]) -> io::Result<T> {
    serde_json::from_slice(frame).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

async fn receive_json<T: for<'de> Deserialize<'de>>(
    framed: &mut AuthenticatedStream,
) -> io::Result<T> {
    parse(&receive_frame(framed).await?)
}

fn rejected(reason: String) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("the analyzer rejected the connection: {reason}"),
    )
}

/// Connects to an analyzer and authenticates as `role`, after checking that
/// the listener holds `token`.
pub async fn connect(
    addr: SocketAddr,
    token: &SessionToken,
    role: Role,
) -> io::Result<AuthenticatedStream> {
    tokio::time::timeout(HANDSHAKE_TIMEOUT, async {
        let stream = TcpStream::connect(addr).await?;
        let mut framed = Framed::new(stream, handshake_codec());
        let client_nonce = random_secret()?;
        send_json(
            &mut framed,
            &Hello {
                version: PROTOCOL_VERSION,
                role,
                nonce: to_hex(&client_nonce),
            },
        )
        .await?;
        let reply = receive_json(&mut framed).await.map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "the analyzer closed the connection during the handshake; it may be an older version of Argon",
                )
            } else {
                error
            }
        })?;
        let (analyzer_nonce, proof) = match reply {
            HelloReply::Challenge { nonce, proof } => (nonce, proof),
            HelloReply::Rejected(reason) => return Err(rejected(reason)),
        };
        let transcript = Transcript {
            role,
            client_nonce,
            analyzer_nonce: from_hex(&analyzer_nonce).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "malformed analyzer nonce")
            })?,
        };
        if !transcript.verify(token, Prover::Analyzer, &proof) {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                format!("the process listening at {addr} does not hold this session token"),
            ));
        }
        send_json(
            &mut framed,
            &Proof {
                proof: transcript.prove(token, Prover::Client),
            },
        )
        .await?;
        match receive_json(&mut framed).await? {
            ProofReply::Accepted => Ok(into_rpc_stream(framed)),
            ProofReply::Rejected(reason) => Err(rejected(reason)),
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
        let frame = receive_frame(&mut framed).await?;
        let Versioned { version } = parse(&frame)?;
        if version != PROTOCOL_VERSION {
            let reason = format!(
                "protocol version {version} is not supported; this analyzer speaks version {PROTOCOL_VERSION}"
            );
            let _ = send_json(&mut framed, &HelloReply::Rejected(reason.clone())).await;
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
        }
        let hello: Hello = parse(&frame)?;
        let transcript = Transcript {
            role: hello.role,
            client_nonce: from_hex(&hello.nonce).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "malformed client nonce")
            })?,
            analyzer_nonce: random_secret()?,
        };
        send_json(
            &mut framed,
            &HelloReply::Challenge {
                nonce: to_hex(&transcript.analyzer_nonce),
                proof: transcript.prove(token, Prover::Analyzer),
            },
        )
        .await?;
        let Proof { proof } = receive_json(&mut framed).await?;
        if !transcript.verify(token, Prover::Client, &proof) {
            let reason = "invalid session token".to_owned();
            let _ = send_json(&mut framed, &ProofReply::Rejected(reason.clone())).await;
            return Err(io::Error::new(io::ErrorKind::PermissionDenied, reason));
        }
        send_json(&mut framed, &ProofReply::Accepted).await?;
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
    use tokio::{net::TcpListener, task::JoinHandle};

    use super::*;

    #[test]
    fn announcements_carry_a_port_and_a_token() {
        let token = "ab".repeat(32);
        assert_eq!(
            parse_announcement(&format!("1234 {token}\n")),
            Some((1234, SessionToken::from_hex(&token).unwrap()))
        );
        assert_eq!(parse_announcement("1234\n"), None);
        assert_eq!(parse_announcement(&format!("0 {token}")), None);
        assert_eq!(parse_announcement("1234 not-a-token"), None);
        assert_eq!(parse_announcement(&format!("1234 {token} extra")), None);
    }

    async fn listener() -> (TcpListener, SocketAddr) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        (listener, addr)
    }

    /// Runs the analyzer's side of the handshake for one connection.
    fn accept_once(listener: TcpListener, token: SessionToken) -> JoinHandle<io::Result<Role>> {
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            accept(stream, &token).await.map(|(role, _)| role)
        })
    }

    async fn raw_client(addr: SocketAddr) -> AuthenticatedStream {
        Framed::new(TcpStream::connect(addr).await.unwrap(), handshake_codec())
    }

    fn transcript(role: Role) -> Transcript {
        Transcript {
            role,
            client_nonce: random_secret().unwrap(),
            analyzer_nonce: random_secret().unwrap(),
        }
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

    #[test]
    fn proofs_are_bound_to_the_token_the_side_and_the_handshake() {
        let token = SessionToken::generate().unwrap();
        let transcript = transcript(Role::Gui);
        let proof = transcript.prove(&token, Prover::Client);
        assert!(transcript.verify(&token, Prover::Client, &proof));
        assert!(!transcript.verify(&token, Prover::Analyzer, &proof));
        assert!(!transcript.verify(&SessionToken::generate().unwrap(), Prover::Client, &proof));
        let other_nonce = Transcript {
            analyzer_nonce: random_secret().unwrap(),
            ..transcript
        };
        assert!(!other_nonce.verify(&token, Prover::Client, &proof));
        let other_role = Transcript {
            role: Role::Agent,
            ..transcript
        };
        assert!(!other_role.verify(&token, Prover::Client, &proof));
        assert!(!transcript.verify(&token, Prover::Client, "not hex"));
    }

    #[tokio::test]
    async fn accepts_the_session_token_and_reports_the_role() {
        let (listener, addr) = listener().await;
        let token = SessionToken::generate().unwrap();
        let server = accept_once(listener, token.clone());
        connect(addr, &token, Role::Agent).await.unwrap();
        assert_eq!(server.await.unwrap().unwrap(), Role::Agent);
    }

    #[tokio::test]
    async fn rejects_a_client_without_the_token() {
        let (listener, addr) = listener().await;
        let server = accept_once(listener, SessionToken::generate().unwrap());
        let mut client = raw_client(addr).await;
        let client_nonce = random_secret().unwrap();
        send_json(
            &mut client,
            &Hello {
                version: PROTOCOL_VERSION,
                role: Role::Agent,
                nonce: to_hex(&client_nonce),
            },
        )
        .await
        .unwrap();
        let HelloReply::Challenge { nonce, .. } = receive_json(&mut client).await.unwrap() else {
            panic!("the analyzer should challenge a current client");
        };
        let transcript = Transcript {
            role: Role::Agent,
            client_nonce,
            analyzer_nonce: from_hex(&nonce).unwrap(),
        };
        let guess = SessionToken::generate().unwrap();
        send_json(
            &mut client,
            &Proof {
                proof: transcript.prove(&guess, Prover::Client),
            },
        )
        .await
        .unwrap();
        let ProofReply::Rejected(reason) = receive_json(&mut client).await.unwrap() else {
            panic!("a proof made with the wrong token should be rejected");
        };
        assert!(reason.contains("invalid session token"));
        assert_eq!(
            server.await.unwrap().unwrap_err().kind(),
            io::ErrorKind::PermissionDenied
        );
    }

    #[tokio::test]
    async fn refuses_a_listener_without_the_token_and_never_sends_it() {
        let (listener, addr) = listener().await;
        let token = SessionToken::generate().unwrap();
        let impostor = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut framed = Framed::new(stream, handshake_codec());
            let hello: Hello = receive_json(&mut framed).await.unwrap();
            let transcript = Transcript {
                role: hello.role,
                client_nonce: from_hex(&hello.nonce).unwrap(),
                analyzer_nonce: random_secret().unwrap(),
            };
            let guess = SessionToken::generate().unwrap();
            send_json(
                &mut framed,
                &HelloReply::Challenge {
                    nonce: to_hex(&transcript.analyzer_nonce),
                    proof: transcript.prove(&guess, Prover::Analyzer),
                },
            )
            .await
            .unwrap();
            let mut received = vec![serde_json::to_string(&hello).unwrap()];
            while let Some(Ok(frame)) = framed.next().await {
                received.push(String::from_utf8_lossy(&frame).into_owned());
            }
            received
        });
        let error = connect(addr, &token, Role::Gui).await.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert!(
            error
                .to_string()
                .contains("does not hold this session token")
        );
        let received = impostor.await.unwrap();
        assert_eq!(received.len(), 1, "the client should stop after the hello");
        assert!(!received[0].contains(&token.to_hex()));
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
    async fn tells_a_version_one_client_why_it_was_rejected() {
        let (listener, addr) = listener().await;
        let token = SessionToken::generate().unwrap();
        let server = accept_once(listener, token.clone());
        let mut client = raw_client(addr).await;
        send_json(
            &mut client,
            &serde_json::json!({ "version": 1, "token": token.to_hex(), "role": "Gui" }),
        )
        .await
        .unwrap();
        // Version 1 clients read a rejection as `{"Rejected": reason}`.
        let reply: serde_json::Value = receive_json(&mut client).await.unwrap();
        let reason = reply["Rejected"].as_str().unwrap();
        assert!(reason.contains("protocol version 1 is not supported"));
        assert!(server.await.unwrap().is_err());
    }
}
