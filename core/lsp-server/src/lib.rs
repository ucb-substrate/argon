pub mod socket;

use std::net::SocketAddr;

use portpicker::pick_unused_port;
use socket::{GuiToLspMessage, LspFromGui, LspToGui};
use tokio::net::TcpListener;
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::sync::OnceCell;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

#[derive(Debug)]
struct Backend {
    client: Client,
    gui_w: OnceCell<LspToGui<OwnedWriteHalf>>,
}

async fn handle_gui_r(client: Client, uri: Url, gui_r: OwnedReadHalf) {
    let mut sock = LspFromGui::new(gui_r);
    let src = tokio::fs::read_to_string(uri.to_file_path().unwrap())
        .await
        .unwrap();
    let line_lengths = std::iter::once(0)
        .chain(src.lines().map(|s| s.len() + 1).scan(0, |state, x| {
            *state += x;
            Some(*state)
        }))
        .collect::<Vec<_>>();
    let char2pos = |c: usize| {
        let line_idx = match line_lengths.binary_search(&c) {
            Ok(index) | Err(index) => index,
        }.saturating_sub(1);
        Position::new(line_idx as u32, (c - line_lengths[line_idx]) as u32)
    };
    loop {
        let msg = sock.read().await;
        match msg {
            GuiToLspMessage::SelectedRect(msg) => {
                if let Some(span) = msg.span {
                    let diagnostics = vec![Diagnostic {
                        range: Range {
                            start: char2pos(span.start()),
                            end: char2pos(span.end()),
                        },
                        severity: Some(DiagnosticSeverity::INFORMATION),
                        message: "selected rect".to_string(),
                        ..Default::default()
                    }];
                    client
                        .publish_diagnostics(uri.clone(), diagnostics, None)
                        .await;
                }
            }
        }
    }
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
        self.client
            .log_message(MessageType::INFO, "server initialized!")
            .await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let port = loop {
            if let Some(port) = pick_unused_port() {
                break port;
            }
        };
        let gui_socket = TcpListener::bind(SocketAddr::new("127.0.0.1".parse().unwrap(), port))
            .await
            .unwrap();
        self.client
            .show_message(MessageType::INFO, format!("LSP listening on port {port}"))
            .await;
        let (gui_socket, _) = gui_socket.accept().await.unwrap();
        let (gui_r, gui_w) = gui_socket.into_split();
        self.gui_w.set(LspToGui::new(gui_w)).unwrap();
        let other_client = self.client.clone();
        tokio::spawn(async move {
            handle_gui_r(other_client, params.text_document.uri, gui_r).await;
        });
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

pub async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend {
        client,
        gui_w: OnceCell::new(),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
