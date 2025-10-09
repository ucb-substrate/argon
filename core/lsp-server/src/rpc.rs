use std::{collections::HashMap, net::SocketAddr, path::PathBuf};

use compiler::{
    ast::Span,
    compile::{BasicRect, CompileOutput},
};

use tarpc::{context, tokio_serde::formats::Json};
use tower_lsp::lsp_types::{
    Diagnostic, DiagnosticSeverity, MessageType, Position, Range, ShowDocumentParams, TextEdit,
    Url, WorkspaceEdit,
};

use crate::{
    ForceSave, State,
    document::{Document, DocumentChange},
};

#[tarpc::service]
pub trait GuiToLsp {
    async fn register(addr: SocketAddr);
    async fn select_rect(span: Span);
    async fn draw_rect(scope_span: Span, var_name: String, rect: BasicRect<f64>);
    async fn add_eq_constraint(scope_span: Span, lhs: String, rhs: String);
}

#[tarpc::service]
pub trait LspToGui {
    async fn open_cell(cell: CompileOutput);
    async fn set(key: String, value: String);
}

#[derive(Clone)]
pub struct LspServer {
    pub state: State,
}

impl GuiToLsp for LspServer {
    async fn register(self, _: tarpc::context::Context, addr: SocketAddr) -> () {
        let gui_client = {
            let mut transport = tarpc::serde_transport::tcp::connect(addr, Json::default);
            transport.config_mut().max_frame_length(usize::MAX);

            LspToGuiClient::new(tarpc::client::Config::default(), transport.await.unwrap()).spawn()
        };
        let mut state_mut = self.state.state_mut.lock().await;
        if let Some(o) = &state_mut.compile_output {
            gui_client
                .open_cell(context::current(), o.clone())
                .await
                .unwrap();
        }
        state_mut.gui_client = Some(gui_client);
    }

    async fn select_rect(self, _: tarpc::context::Context, span: Span) {
        // TODO: check that vim file is in sync with GUI file.
        let url = Url::from_file_path(&span.path).unwrap();
        if let Some(doc) = self.state.state_mut.lock().await.editor_files.get(&url) {
            let diagnostics = vec![Diagnostic {
                range: Range {
                    start: doc.offset_to_pos(span.span.start()),
                    end: doc.offset_to_pos(span.span.end()),
                },
                severity: Some(DiagnosticSeverity::INFORMATION),
                message: "selected rect".to_string(),
                ..Default::default()
            }];
            self.state
                .editor_client
                .publish_diagnostics(url, diagnostics, None)
                .await;
        }
    }

    async fn draw_rect(
        self,
        _: tarpc::context::Context,
        scope_span: Span,
        var_name: String,
        rect: BasicRect<f64>,
    ) {
        // TODO: check if editor file is up to date with ast.
        let mut state_mut = self.state.state_mut.lock().await;
        let url = Url::from_file_path(&scope_span.path).unwrap();
        let doc = state_mut
            .editor_files
            .get(&url)
            .map::<Result<_, std::io::Error>, _>(|doc| Ok(doc.clone()))
            .unwrap_or_else(|| Ok(Document::new(std::fs::read_to_string(&scope_span.path)?, 0)));
        if let Ok(doc) = doc
            && let Some(scope) = state_mut
                .ast
                .values()
                .find(|ast| ast.path == scope_span.path)
                .as_ref()
                .and_then(|ast| ast.span2scope.get(&scope_span))
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
            if let Some(doc) = state_mut.editor_files.get_mut(&url) {
                let version = doc.version() + 1;
                doc.apply_changes(
                    vec![DocumentChange {
                        range: Some(edit.range),
                        patch: edit.new_text.clone(),
                    }],
                    version,
                );
            }

            self.state
                .editor_client
                .show_document(ShowDocumentParams {
                    uri: url.clone(),
                    external: None,
                    take_focus: None,
                    selection: None,
                })
                .await
                .unwrap();

            self.state
                .editor_client
                .apply_edit(WorkspaceEdit {
                    changes: Some(HashMap::from_iter([(url, vec![edit])])),
                    document_changes: None,
                    change_annotations: None,
                })
                .await
                .unwrap();

            self.state
                .editor_client
                .send_request::<ForceSave>(scope_span.path.clone())
                .await
                .unwrap();
        }
    }

    async fn add_eq_constraint(
        self,
        _: tarpc::context::Context,
        scope_span: Span,
        lhs: String,
        rhs: String,
    ) {
        // TODO: check if editor file is up to date with ast.
        let mut state_mut = self.state.state_mut.lock().await;
        let url = Url::from_file_path(&scope_span.path).unwrap();
        let doc = state_mut
            .editor_files
            .get(&url)
            .map::<Result<_, std::io::Error>, _>(|doc| Ok(doc.clone()))
            .unwrap_or_else(|| Ok(Document::new(std::fs::read_to_string(&scope_span.path)?, 0)));
        if let Ok(doc) = doc
            && let Some(scope) = state_mut
                .ast
                .values()
                .find(|ast| ast.path == scope_span.path)
                .as_ref()
                .and_then(|ast| ast.span2scope.get(&scope_span))
        {
            let edit = if let Some(tail) = &scope.tail {
                let start = doc.offset_to_pos(tail.span().start());
                TextEdit {
                    range: Range::new(start, start),
                    new_text: format!(
                        "eq({}, {});\n{}",
                        lhs,
                        rhs,
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
                        "{}eq({}, {});\n{whitespace}",
                        if start.line != stop.line {
                            "    "
                        } else {
                            "\n"
                        },
                        lhs,
                        rhs
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

            if let Some(doc) = state_mut.editor_files.get_mut(&url) {
                let version = doc.version() + 1;
                doc.apply_changes(
                    vec![DocumentChange {
                        range: Some(edit.range),
                        patch: edit.new_text.clone(),
                    }],
                    version,
                );
            }

            self.state
                .editor_client
                .show_document(ShowDocumentParams {
                    uri: url.clone(),
                    external: None,
                    take_focus: None,
                    selection: None,
                })
                .await
                .unwrap();

            self.state
                .editor_client
                .apply_edit(WorkspaceEdit {
                    changes: Some(HashMap::from_iter([(url, vec![edit])])),
                    document_changes: None,
                    change_annotations: None,
                })
                .await
                .unwrap();

            self.state
                .editor_client
                .send_request::<ForceSave>(scope_span.path.clone())
                .await
                .unwrap();
        }
    }
}
