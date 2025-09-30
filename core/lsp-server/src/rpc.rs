use std::{net::SocketAddr, path::PathBuf};

use cfgrammar::Span;
use compiler::compile::CompileOutput;

use tarpc::tokio_serde::formats::Json;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Range, Url};

use crate::State;

#[tarpc::service]
pub trait GuiToLsp {
    async fn register(addr: SocketAddr);
    async fn select_rect(span: Option<(PathBuf, Span)>);
}

#[tarpc::service]
pub trait LspToGui {
    async fn open_cell(file: PathBuf, cell: CompileOutput);
    async fn set(key: String, value: String);
}

#[derive(Clone)]
pub struct LspServer {
    pub state: State,
}

impl GuiToLsp for LspServer {
    async fn register(self, _: tarpc::context::Context, addr: SocketAddr) -> () {
        self.state.state_mut.lock().await.gui_client = Some({
            let mut transport = tarpc::serde_transport::tcp::connect(addr, Json::default);
            transport.config_mut().max_frame_length(usize::MAX);

            LspToGuiClient::new(tarpc::client::Config::default(), transport.await.unwrap()).spawn()
        });
    }

    async fn select_rect(self, _: tarpc::context::Context, span: Option<(PathBuf, Span)>) {
        if let Some((file, span)) = &span {
            // TODO: check that vim file is in sync with GUI file.
            if let Some(doc) = self
                .state
                .state_mut
                .lock()
                .await
                .gui_files
                .get(&Url::from_file_path(file).unwrap())
            {
                let diagnostics = vec![Diagnostic {
                    range: Range {
                        start: doc.offset_to_pos(span.start()),
                        end: doc.offset_to_pos(span.end()),
                    },
                    severity: Some(DiagnosticSeverity::INFORMATION),
                    message: "selected rect".to_string(),
                    ..Default::default()
                }];
                self.state
                    .editor_client
                    .publish_diagnostics(Url::from_file_path(file).unwrap(), diagnostics, None)
                    .await;
            }
        }
    }
}
