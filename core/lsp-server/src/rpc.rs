use std::{collections::HashMap, net::SocketAddr, path::PathBuf};

use cfgrammar::Span;
use compiler::compile::{BasicRect, CompileOutput};

use tarpc::{context, tokio_serde::formats::Json};
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, MessageType, Position, Range, TextEdit, Url, WorkspaceEdit,
};

use crate::{State, document::DocumentChange};

#[tarpc::service]
pub trait GuiToLsp {
    async fn register(addr: SocketAddr);
    async fn select_rect(span: Option<(PathBuf, Span)>);
    async fn draw_rect(file: PathBuf, scope_span: Span, var_name: String, rect: BasicRect<f64>);
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

    async fn draw_rect(
        self,
        _: tarpc::context::Context,
        file: PathBuf,
        scope_span: Span,
        var_name: String,
        rect: BasicRect<f64>,
    ) {
        let mut state_mut = self.state.state_mut.lock().await;
        let url = Url::from_file_path(&file).unwrap();
        if let Some(doc) = state_mut.gui_files.get(&url)
            && let Some(scope) = doc.ast.span2scope.get(&scope_span)
        {
            let edit = if let Some(tail) = &scope.tail {
                let start = doc.offset_to_pos(tail.span().start());
                TextEdit {
                    range: Range::new(start, start),
                    new_text: format!(
                        "let {var_name} = rect({}x0i = {}, y0i = {}, x1i = {}, y1i = {})!;\n{}",
                        rect.layer
                            .map(|layer| format!("{layer}, "))
                            .unwrap_or_default(),
                        rect.x0,
                        rect.y0,
                        rect.x1,
                        rect.y1,
                        // TODO: handle different types of indentation, or enforce that gui
                        // reformats file before editing.
                        std::iter::repeat_n(' ', start.character as usize).collect::<String>()
                    ),
                }
            } else {
                let start = doc.offset_to_pos(scope.span.start());
                let stop = doc.offset_to_pos(scope.span.end());
                let line = doc.substr(Position::new(stop.line, 0)..stop);
                let trimmed = line.trim_start();
                let whitespace = &line[..line.len() - trimmed.len()];
                let insert_loc = doc.offset_to_pos(scope.span.end() - 1);
                TextEdit {
                    range: Range::new(insert_loc, insert_loc),
                    new_text: format!(
                        "{}let {var_name} = rect({}x0i = {}, y0i = {}, x1i = {}, y1i = {})!;\n{whitespace}",
                        if start.line != stop.line {
                            "    "
                        } else {
                            "\n"
                        },
                        rect.layer
                            .map(|layer| format!("\"{layer}\", "))
                            .unwrap_or_default(),
                        rect.x0,
                        rect.y0,
                        rect.x1,
                        rect.y1,
                    ),
                }
            };

            if let Some(file) = state_mut.editor_files.get(&url)
                && file.contents() != doc.contents()
            {
                self.state
                    .editor_client
                    .show_message(
                        MessageType::ERROR,
                        "Editor buffer state is inconsistent with GUI state.",
                    )
                    .await;
                return;
            }
            let version = doc.version() + 1;
            let cell = doc.cell.clone();
            state_mut.gui_files.get_mut(&url).unwrap().apply_changes(
                vec![DocumentChange {
                    range: Some(edit.range),
                    patch: edit.new_text.clone(),
                }],
                version,
            );

            self.state
                .editor_client
                .apply_edit(WorkspaceEdit {
                    changes: Some(HashMap::from_iter([(url, vec![edit])])),
                    document_changes: None,
                    change_annotations: None,
                })
                .await
                .unwrap();

            let o = state_mut.compile_gui_cell(&file, &cell);
            if let Some(gui_client) = &mut state_mut.gui_client {
                gui_client
                    .open_cell(context::current(), file, o)
                    .await
                    .unwrap();
            }
        }
    }
}
