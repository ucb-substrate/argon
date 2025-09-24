use std::{net::SocketAddr, path::PathBuf};

use cfgrammar::Span;
use compiler::compile::CompileOutput;

use tarpc::tokio_serde::formats::Json;
use tower_lsp::lsp_types::{Diagnostic, DiagnosticSeverity, Position, Range, Url};

use crate::SharedState;

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
    pub state: SharedState,
}

impl GuiToLsp for LspServer {
    async fn register(self, _: tarpc::context::Context, addr: SocketAddr) -> () {
        *self.state.gui_client.lock().await = Some({
            let mut transport = tarpc::serde_transport::tcp::connect(addr, Json::default);
            transport.config_mut().max_frame_length(usize::MAX);

            LspToGuiClient::new(tarpc::client::Config::default(), transport.await.unwrap()).spawn()
        });
    }

    async fn select_rect(self, _: tarpc::context::Context, span: Option<(PathBuf, Span)>) {
        if let Some((file, span)) = &span {
            let src = tokio::fs::read_to_string(&file).await.unwrap();
            let line_lengths = std::iter::once(0)
                .chain(src.lines().map(|s| s.len() + 1).scan(0, |state, x| {
                    *state += x;
                    Some(*state)
                }))
                .collect::<Vec<_>>();
            let char2pos = |c: usize| {
                let line_idx = match line_lengths.binary_search(&c) {
                    Ok(index) | Err(index) => index,
                }
                .saturating_sub(1);
                Position::new(line_idx as u32, (c - line_lengths[line_idx]) as u32)
            };
            let diagnostics = vec![Diagnostic {
                range: Range {
                    start: char2pos(span.start()),
                    end: char2pos(span.end()),
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
