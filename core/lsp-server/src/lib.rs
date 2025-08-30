pub mod rpc;

use std::{
    net::{IpAddr, Ipv6Addr, SocketAddr},
    process::Command,
};

use futures::{future, prelude::*};
use portpicker::pick_unused_port;
use rpc::{GuiToLsp, LspServer};
use tarpc::{
    context,
    server::{self, incoming::Incoming, Channel},
    tokio_serde::formats::Json,
};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpListener;
use tokio::sync::OnceCell;
use tokio::time;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Debug, Clone)]
struct SharedState {
    server_addr: SocketAddr,
    client: Client,
}

#[derive(Debug)]
struct Backend {
    state: SharedState,
}

// async fn handle_gui_r(client: Client, uri: Url, gui_r: OwnedReadHalf) {
//     let mut sock = LspFromGui::new(gui_r);
//     let src = tokio::fs::read_to_string(uri.to_file_path().unwrap())
//         .await
//         .unwrap();
//     let line_lengths = std::iter::once(0)
//         .chain(src.lines().map(|s| s.len() + 1).scan(0, |state, x| {
//             *state += x;
//             Some(*state)
//         }))
//         .collect::<Vec<_>>();
//     let char2pos = |c: usize| {
//         let line_idx = match line_lengths.binary_search(&c) {
//             Ok(index) | Err(index) => index,
//         }
//         .saturating_sub(1);
//         Position::new(line_idx as u32, (c - line_lengths[line_idx]) as u32)
//     };
//     loop {
//         let msg = sock.read().await;
//         match msg {
//             GuiToLspMessage::SelectedRect(msg) => {
//                 if let Some(span) = msg.span {
//                     let diagnostics = vec![Diagnostic {
//                         range: Range {
//                             start: char2pos(span.start()),
//                             end: char2pos(span.end()),
//                         },
//                         severity: Some(DiagnosticSeverity::INFORMATION),
//                         message: "selected rect".to_string(),
//                         ..Default::default()
//                     }];
//                     client
//                         .publish_diagnostics(uri.clone(), diagnostics, None)
//                         .await;
//                 }
//             }
//             _ => unimplemented!(),
//         }
//     }
// }

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        ..Default::default()
                    },
                )),
                ..Default::default()
            },
            ..Default::default()
        })
    }

    async fn initialized(&self, _: InitializedParams) {
        self.state
            .client
            .log_message(MessageType::INFO, "server initialized!")
            .await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {}

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

impl Backend {
    async fn start_gui(&self) -> Result<()> {
        self.state
            .client
            .show_message(MessageType::INFO, "Starting the GUI...")
            .await;
        let server_addr = self.state.server_addr.clone();
        tokio::spawn(async move {
            Command::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../target/debug/gui"
            ))
            .arg(format!("{}", server_addr))
            .spawn()
            .unwrap()
            .wait();
        });
        Ok(())
    }
}

pub async fn main() {
    // Start server for communication with GUI.
    let port = loop {
        if let Some(port) = pick_unused_port() {
            break port;
        }
    };
    let server_addr = (IpAddr::V6(Ipv6Addr::LOCALHOST), port).into();

    // Construct actual LSP server.
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let mut ext_state = None;
    let (service, socket) = LspService::build(|client| {
        let state = SharedState {
            server_addr,
            client,
        };
        ext_state = Some(state.clone());
        Backend { state }
    })
    .custom_method("custom/startGui", Backend::start_gui)
    .finish();
    let state = ext_state.unwrap();

    // JSON transport is provided by the json_transport tarpc module. It makes it easy
    // to start up a serde-powered json serialization strategy over TCP.
    let mut listener = tarpc::serde_transport::tcp::listen(&server_addr, Json::default)
        .await
        .unwrap();
    listener.config_mut().max_frame_length(usize::MAX);
    let state_clone = state.clone();
    tokio::spawn(async move {
        listener
            // Ignore accept errors.
            .filter_map(|r| futures::future::ready(r.ok()))
            .map(tarpc::server::BaseChannel::with_defaults)
            .map(move |channel| {
                let server = LspServer {
                    state: state_clone.clone(),
                };
                channel.execute(server.serve()).for_each(async |t| {
                    tokio::spawn(t);
                })
            })
            .for_each(|_| async {})
            .await
    });

    state
        .client
        .show_message(
            MessageType::INFO,
            format!("Server listening on port {port}"),
        )
        .await;

    // Start actual LSP server.
    Server::new(stdin, stdout, socket).serve(service).await;
}
