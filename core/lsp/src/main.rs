use std::net::{IpAddr, SocketAddr};

use portpicker::pick_unused_port;
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::process::Command;
use tokio::sync::OnceCell;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

pub mod socket;

#[derive(Debug)]
struct Backend {
    client: Client,
    gui_socket: OnceCell<TcpStream>,
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, _: InitializeParams) -> Result<InitializeResult> {
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(true),
                        })),
                        ..Default::default()
                    },
                )),
                definition_provider: Some(OneOf::Left(true)),
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
        self.client.log_message(MessageType::INFO, "abcde").await;
        let port = loop {
            if let Some(port) = pick_unused_port() {
                break port;
            }
        };
        let gui_socket = TcpListener::bind(SocketAddr::new("127.0.0.1".parse().unwrap(), port))
            .await
            .unwrap();
        self.client.log_message(MessageType::INFO, "abcde").await;
        self.client
            .show_message(MessageType::INFO, format!("LSP listening on port {port}"))
            .await;
        let (mut gui_socket, _) = gui_socket.accept().await.unwrap();
        // self.gui_socket.set(gui_socket).unwrap();
        let other_client = self.client.clone();
        tokio::spawn(async move {
            loop {
                let mut buf = [0; 512];
                let n = gui_socket.read(&mut buf).await.unwrap();

                if n > 0 {
                    other_client
                        .show_message(MessageType::INFO, str::from_utf8(&buf[..n]).unwrap())
                        .await;
                }
            }
        });
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.client.show_message(MessageType::INFO, "test").await;
        Ok(Some(GotoDefinitionResponse::Scalar(Location::new(
            params.text_document_position_params.text_document.uri,
            Range::new(Position::new(0, 0), Position::new(0, 0)),
        ))))
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

#[tokio::main]
async fn main() {
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let (service, socket) = LspService::new(|client| Backend {
        client,
        gui_socket: OnceCell::new(),
    });
    Server::new(stdin, stdout, socket).serve(service).await;
}
