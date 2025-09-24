pub mod rpc;

use std::{
    net::{IpAddr, Ipv6Addr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
};

use compiler::compile::CompileOutput;
use futures::prelude::*;
use portpicker::{is_free, pick_unused_port};
use rpc::{GuiToLsp, LspServer, LspToGuiClient};
use serde::{Deserialize, Serialize};
use tarpc::{
    context,
    server::{Channel, incoming::Incoming},
    tokio_serde::formats::Json,
};
use tokio::{process::Command, sync::Mutex};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Debug, Clone)]
pub struct SharedState {
    server_addr: SocketAddr,
    editor_client: Client,
    gui_client: Arc<Mutex<Option<LspToGuiClient>>>,
}

#[derive(Debug)]
struct Backend {
    state: SharedState,
}

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
            .editor_client
            .log_message(MessageType::INFO, "server initialized!")
            .await;
    }

    async fn did_open(&self, _params: DidOpenTextDocumentParams) {}

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct OpenCellParams {
    file: PathBuf,
    cell: String,
}

#[derive(Serialize, Deserialize)]
struct SetParams {
    kv: String,
}

impl Backend {
    async fn start_gui(&self) -> Result<()> {
        self.state
            .editor_client
            .show_message(MessageType::INFO, "Starting the GUI...")
            .await;
        let server_addr = self.state.server_addr;

        tokio::spawn(async move {
            Command::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../target/debug/gui"
            ))
            .arg(format!("{server_addr}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
            .wait()
            .await
            .unwrap();
        });

        Ok(())
    }
    async fn open_cell(&self, params: OpenCellParams) -> Result<()> {
        let state = self.state.clone();
        state
            .editor_client
            .show_message(
                MessageType::INFO,
                &format!("file {:?}, cell {}", params.file, params.cell),
            )
            .await;
        tokio::spawn(async move {
            let o = Command::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../target/debug/compiler"
            ))
            .arg(&params.file)
            .arg(params.cell)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
            .wait_with_output()
            .await
            .unwrap();
            let o_str = String::from_utf8(o.stdout).unwrap();
            let o: CompileOutput = serde_json::from_str(&o_str).unwrap();
            if let Some(client) = state.gui_client.lock().await.as_mut() {
                client
                    .open_cell(context::current(), params.file, o)
                    .await
                    .unwrap();
            } else {
                state
                    .editor_client
                    .show_message(MessageType::ERROR, "No GUI connected")
                    .await;
            }
        });
        Ok(())
    }

    async fn set(&self, params: SetParams) -> Result<()> {
        let state = self.state.clone();
        // TODO: Error handling.
        let (k, v) = params.kv.split_once(" ").unwrap();
        let (k, v) = (k.to_string(), v.to_string());
        tokio::spawn(async move {
            if let Some(client) = state.gui_client.lock().await.as_mut() {
                client.set(context::current(), k, v).await.unwrap();
            }
        });
        Ok(())
    }
}

async fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(fut);
}

pub async fn main() {
    // Start server for communication with GUI.
    let port = 12345; // for debugging
    let port = if is_free(port) {
        port
    } else {
        loop {
            if let Some(port) = pick_unused_port() {
                break port;
            }
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
            editor_client: client,
            gui_client: Arc::new(Mutex::new(None)),
        };
        ext_state = Some(state.clone());
        Backend { state }
    })
    .custom_method("custom/startGui", Backend::start_gui)
    .custom_method("custom/openCell", Backend::open_cell)
    .custom_method("custom/set", Backend::set)
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
            // Limit channels to 1 per IP.
            .max_channels_per_key(1, |t| t.transport().peer_addr().unwrap().ip())
            // serve is generated by the service attribute. It takes as input any type implementing
            // the generated World trait.
            .map(|channel| {
                let server = LspServer {
                    state: state_clone.clone(),
                };
                channel.execute(server.serve()).for_each(spawn)
            })
            // Max 10 channels.
            .buffer_unordered(10)
            .for_each(|_| async {})
            .await;
    });

    state
        .editor_client
        .show_message(
            MessageType::INFO,
            format!("Server listening on port {port}"),
        )
        .await;

    // Start actual LSP server.
    Server::new(stdin, stdout, socket).serve(service).await;
}
