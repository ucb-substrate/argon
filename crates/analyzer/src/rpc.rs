//! RPC types shared by the analyzer and Argone.

use std::{collections::HashMap, net::SocketAddr, path::PathBuf};

use argonc::{
    ast::Span,
    compile::{BasicRect, CellId, CompileOutput, CompiledData},
};

use serde::{Deserialize, Serialize};
use tarpc::tokio_serde::formats::Json;
use tower_lsp_server::ls_types::{
    Diagnostic, DiagnosticSeverity, MessageType, Position, Range, ShowDocumentParams, TextEdit,
    Uri, WorkspaceEdit,
};

use crate::{ForceSave, Redo, State, StateMut, Undo, document::Document};

/// A single source rewrite: replace the text at `span` with `value`. Used to
/// persist solution-space-exploration drags by updating initial-condition
/// values (e.g. the `100.` in `x1i=100.`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueEdit {
    pub span: Span,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionParams {
    pub p: String,
    pub n: String,
    pub value: String,
    pub coord: String,
    pub pstop: String,
    pub nstop: String,
    pub horiz: String,
}

/// A solved cell and the source location where placing it will insert an
/// instance. The GUI keeps this separate from the currently open layout and
/// uses it only for the cursor-following placement outline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstancePreview {
    pub output: CompiledData,
    pub cell: CellId,
    pub invocation: String,
    pub scope_span: Span,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LangServerAction {
    Undo,
    Redo,
}

#[tarpc::service]
pub trait LangServer {
    async fn register(addr: SocketAddr);
    async fn select_rect(span: Span);
    async fn draw_rect(scope_span: Span, var_name: String, rect: BasicRect<f64>) -> Option<Span>;
    async fn place_instance(scope_span: Span, invocation: String, x: f64, y: f64) -> Option<Span>;
    async fn draw_dimension(scope_span: Span, params: DimensionParams) -> Option<Span>;
    async fn edit_dimension(span: Span, value: String) -> Option<Span>;
    async fn update_values(edits: Vec<ValueEdit>) -> bool;
    async fn add_eq_constraint(scope_span: Span, lhs: String, rhs: String);
    async fn open_cell(cell: String);
    async fn show_message(typ: MessageType, message: String);
    async fn dispatch_action(action: LangServerAction);
    async fn focus_editor(command: Option<String>);
}

#[tarpc::service]
pub trait Gui {
    async fn open_cell(cell: CompileOutput, update: bool);
    async fn selected_scope() -> Option<Span>;
    async fn place_instance(preview: InstancePreview);
    async fn set(key: String, value: String);
    async fn activate();
}

pub(crate) const OUT_OF_SYNC_MESSAGE: &str = "Editor buffer state is inconsistent with GUI state.";

pub(crate) fn editor_buffers_are_current(state: &StateMut) -> bool {
    state.ast.values().all(|ast| {
        Uri::from_file_path(&ast.path)
            .and_then(|uri| state.editor_files.get(&uri))
            .is_none_or(|document| document.contents() == ast.text)
    })
}

pub(crate) struct SourceInsertion {
    pub(crate) offset: usize,
    pub(crate) edit: TextEdit,
    pub(crate) tracked_span: cfgrammar::Span,
}

pub(crate) fn insert_statement(
    document: &Document,
    scope_span: cfgrammar::Span,
    tail_start: Option<usize>,
    statement: &str,
    tracked: std::ops::Range<usize>,
) -> SourceInsertion {
    let (offset, prefix, suffix) = if let Some(offset) = tail_start {
        let position = document.offset_to_pos(offset);
        (
            offset,
            String::new(),
            format!("\n{}", " ".repeat(position.character as usize)),
        )
    } else {
        let start = document.offset_to_pos(scope_span.start());
        let stop = document.offset_to_pos(scope_span.end());
        let line = document.substr(Position::new(stop.line, 0)..stop);
        let indentation = &line[..line.len() - line.trim_start().len()];
        (
            scope_span.end() - 1,
            if start.line == stop.line {
                "\n".to_owned()
            } else {
                "    ".to_owned()
            },
            format!("\n{indentation}"),
        )
    };
    let position = document.offset_to_pos(offset);
    let tracked_start = offset + prefix.len() + tracked.start;

    SourceInsertion {
        offset,
        edit: TextEdit {
            range: Range::new(position, position),
            new_text: format!("{prefix}{statement}{suffix}"),
        },
        tracked_span: cfgrammar::Span::new(tracked_start, offset + prefix.len() + tracked.end),
    }
}

fn instance_placement_expression(invocation: &str, x: f64, y: f64) -> String {
    format!("inst({invocation}, xi={x:?}, yi={y:?})")
}

impl State {
    async fn apply_source_changes(
        &self,
        changes: HashMap<Uri, Vec<TextEdit>>,
        paths: impl IntoIterator<Item = PathBuf>,
        focus: Option<Uri>,
    ) -> bool {
        let result: Result<(), String> = async {
            if let Some(uri) = focus {
                self.editor_client
                    .show_document(ShowDocumentParams {
                        uri,
                        external: None,
                        take_focus: None,
                        selection: None,
                    })
                    .await
                    .map_err(|error| format!("could not show source document: {error}"))?;
            }

            let response = self
                .editor_client
                .apply_edit(WorkspaceEdit {
                    changes: Some(changes),
                    document_changes: None,
                    change_annotations: None,
                })
                .await
                .map_err(|error| format!("could not apply source edit: {error}"))?;
            if !response.applied {
                return Err(response
                    .failure_reason
                    .unwrap_or_else(|| "editor rejected source edit".to_owned()));
            }

            for path in paths {
                self.editor_client
                    .send_request::<ForceSave>(path)
                    .await
                    .map_err(|error| format!("could not save edited source: {error}"))?;
            }
            Ok(())
        }
        .await;

        if let Err(error) = result {
            self.editor_client
                .show_message(MessageType::ERROR, error)
                .await;
            false
        } else {
            true
        }
    }

    async fn apply_source_edit(&self, uri: Uri, path: PathBuf, edit: TextEdit) -> bool {
        self.apply_source_changes(
            HashMap::from([(uri.clone(), vec![edit])]),
            [path],
            Some(uri),
        )
        .await
    }
}

impl LangServer for State {
    async fn register(self, _: tarpc::context::Context, addr: SocketAddr) -> () {
        let mut transport = tarpc::serde_transport::tcp::connect(addr, Json::default);
        transport.config_mut().max_frame_length(usize::MAX);
        let gui_client = match transport.await {
            Ok(transport) => GuiClient::new(tarpc::client::Config::default(), transport).spawn(),
            Err(error) => {
                self.editor_client
                    .show_message(
                        MessageType::ERROR,
                        format!("Could not connect to the GUI: {error}"),
                    )
                    .await;
                return;
            }
        };
        let mut state_mut = self.state_mut.lock().await;
        state_mut.gui_client = Some(gui_client);
        state_mut.compile(&self.editor_client, false).await;
    }

    async fn select_rect(self, _: tarpc::context::Context, span: Span) {
        // TODO: check that vim file is in sync with GUI file.
        let state_mut = self.state_mut.lock().await;
        if let Some(ast) = state_mut.ast.values().find(|ast| ast.path == span.path) {
            let doc = Document::new(&ast.text, 0);
            let Some(url) = Uri::from_file_path(&span.path) else {
                return;
            };
            let diagnostics = vec![Diagnostic {
                range: Range {
                    start: doc.offset_to_pos(span.span.start()),
                    end: doc.offset_to_pos(span.span.end()),
                },
                severity: Some(DiagnosticSeverity::INFORMATION),
                message: "selected rect".to_string(),
                ..Default::default()
            }];
            self.editor_client
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
    ) -> Option<Span> {
        let state_mut = self.state_mut.lock().await;
        if !editor_buffers_are_current(&state_mut) {
            drop(state_mut);
            self.editor_client
                .show_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                .await;
            return None;
        }
        let url = Uri::from_file_path(&scope_span.path)?;
        let ast = state_mut
            .ast
            .values()
            .find(|ast| ast.path == scope_span.path)?;
        let scope = ast.span2scope.get(&scope_span)?;
        let document = Document::new(&ast.text, 0);
        let expression = format!(
            "rect({}x0i = {}, y0i = {}, x1i = {}, y1i = {})",
            rect.layer
                .as_ref()
                .map(|layer| format!("\"{layer}\", "))
                .unwrap_or_default(),
            rect.x0,
            rect.y0,
            rect.x1,
            rect.y1,
        );
        let prefix = format!("let {var_name} = ");
        let insertion = insert_statement(
            &document,
            scope.span,
            scope.tail.as_ref().map(|tail| tail.span().start()),
            &format!("{prefix}{expression}!;"),
            prefix.len()..prefix.len() + expression.len(),
        );
        let span = Span {
            path: scope_span.path.clone(),
            span: insertion.tracked_span,
        };
        drop(state_mut);

        self.apply_source_edit(url, scope_span.path, insertion.edit)
            .await
            .then_some(span)
    }

    async fn place_instance(
        self,
        _: tarpc::context::Context,
        scope_span: Span,
        invocation: String,
        x: f64,
        y: f64,
    ) -> Option<Span> {
        let state_mut = self.state_mut.lock().await;
        if !editor_buffers_are_current(&state_mut) {
            drop(state_mut);
            self.editor_client
                .show_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                .await;
            return None;
        }
        let url = Uri::from_file_path(&scope_span.path)?;
        let ast = state_mut
            .ast
            .values()
            .find(|ast| ast.path == scope_span.path)?;
        let scope = ast.span2scope.get(&scope_span)?;
        let document = Document::new(&ast.text, 0);
        let names = scope
            .stmts
            .iter()
            .filter_map(|statement| match statement {
                argonc::ast::Statement::LetBinding(binding) => Some(binding.name.name.as_str()),
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let var_name = (0..)
            .map(|index| format!("inst{index}"))
            .find(|name| !names.contains(name.as_str()))?;
        let expression = instance_placement_expression(&invocation, x, y);
        let prefix = format!("let {var_name} = ");
        let insertion = insert_statement(
            &document,
            scope.span,
            scope.tail.as_ref().map(|tail| tail.span().start()),
            &format!("{prefix}{expression};"),
            prefix.len()..prefix.len() + expression.len(),
        );
        // Keep the placement tool anchored to this scope after the edit. Its
        // start is unchanged, while its end moves by the inserted source text.
        let updated_scope_span = Span {
            path: scope_span.path.clone(),
            span: cfgrammar::Span::new(
                scope.span.start(),
                scope.span.end() + insertion.edit.new_text.len(),
            ),
        };
        drop(state_mut);

        self.apply_source_edit(url, scope_span.path, insertion.edit)
            .await
            .then_some(updated_scope_span)
    }

    async fn draw_dimension(
        self,
        _: tarpc::context::Context,
        scope_span: Span,
        params: DimensionParams,
    ) -> Option<Span> {
        let state_mut = self.state_mut.lock().await;
        if !editor_buffers_are_current(&state_mut) {
            drop(state_mut);
            self.editor_client
                .show_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                .await;
            return None;
        }
        let url = Uri::from_file_path(&scope_span.path)?;
        let ast = state_mut
            .ast
            .values()
            .find(|ast| ast.path == scope_span.path)?;
        let scope = ast.span2scope.get(&scope_span)?;
        let document = Document::new(&ast.text, 0);
        let expression = format!(
            "dimension({}, {}, {}, {}, {}, {}, {})",
            params.p,
            params.n,
            params.value,
            params.coord,
            params.pstop,
            params.nstop,
            params.horiz
        );
        let insertion = insert_statement(
            &document,
            scope.span,
            scope.tail.as_ref().map(|tail| tail.span().start()),
            &format!("{expression};"),
            0..expression.len(),
        );
        let span = Span {
            path: scope_span.path.clone(),
            span: insertion.tracked_span,
        };
        drop(state_mut);

        self.apply_source_edit(url, scope_span.path, insertion.edit)
            .await
            .then_some(span)
    }

    async fn edit_dimension(
        self,
        _: tarpc::context::Context,
        span: Span,
        value: String,
    ) -> Option<Span> {
        let state_mut = self.state_mut.lock().await;
        if !editor_buffers_are_current(&state_mut) {
            drop(state_mut);
            self.editor_client
                .show_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                .await;
            return None;
        }
        let url = Uri::from_file_path(&span.path)?;
        let ast = state_mut.ast.values().find(|ast| ast.path == span.path)?;
        let call = ast.span2call.get(&span)?;
        let old_value = call.args.posargs.get(2)?;
        let document = Document::new(&ast.text, 0);
        let edit = TextEdit {
            range: Range::new(
                document.offset_to_pos(old_value.span().start()),
                document.offset_to_pos(old_value.span().end()),
            ),
            new_text: value.clone(),
        };
        let updated_span = Span {
            path: span.path.clone(),
            span: cfgrammar::Span::new(
                old_value.span().start(),
                old_value.span().start() + value.len(),
            ),
        };
        drop(state_mut);

        self.apply_source_edit(url, span.path, edit)
            .await
            .then_some(updated_span)
    }

    /// Rewrites the value text at each given span in a single workspace edit,
    /// then saves (triggering recompilation). Used to persist SSE drags so the
    /// dragged layout survives recompilation instead of snapping back.
    async fn update_values(self, _: tarpc::context::Context, edits: Vec<ValueEdit>) -> bool {
        if edits.is_empty() {
            return true;
        }
        let state_mut = self.state_mut.lock().await;
        if !editor_buffers_are_current(&state_mut) {
            drop(state_mut);
            self.editor_client
                .show_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                .await;
            return false;
        }

        // Build one WorkspaceEdit grouping all rewrites per file. Edits within a
        // file are sorted by descending start offset so they can be applied
        // back-to-front without invalidating each other's offsets.
        let mut pending: HashMap<Uri, Vec<(usize, TextEdit)>> = HashMap::new();
        let mut paths = Vec::new();
        for ValueEdit { span, value } in edits {
            if let Some(ast) = state_mut.ast.values().find(|ast| ast.path == span.path)
                && let Some(uri) = Uri::from_file_path(&span.path)
            {
                let doc = Document::new(&ast.text, 0);
                let start = doc.offset_to_pos(span.span.start());
                let stop = doc.offset_to_pos(span.span.end());
                pending.entry(uri).or_default().push((
                    span.span.start(),
                    TextEdit {
                        range: Range::new(start, stop),
                        new_text: value,
                    },
                ));
                if !paths.contains(&span.path) {
                    paths.push(span.path.clone());
                }
            }
        }
        if pending.is_empty() {
            return false;
        }
        let changes = pending
            .into_iter()
            .map(|(uri, mut edits)| {
                edits.sort_by(|(left, _), (right, _)| right.cmp(left));
                (uri, edits.into_iter().map(|(_, edit)| edit).collect())
            })
            .collect();
        drop(state_mut);

        self.apply_source_changes(changes, paths, None).await
    }

    async fn add_eq_constraint(
        self,
        _: tarpc::context::Context,
        scope_span: Span,
        lhs: String,
        rhs: String,
    ) {
        let state_mut = self.state_mut.lock().await;
        if !editor_buffers_are_current(&state_mut) {
            drop(state_mut);
            self.editor_client
                .show_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                .await;
            return;
        }
        let Some(url) = Uri::from_file_path(&scope_span.path) else {
            return;
        };
        let Some(ast) = state_mut
            .ast
            .values()
            .find(|ast| ast.path == scope_span.path)
        else {
            return;
        };
        let Some(scope) = ast.span2scope.get(&scope_span) else {
            return;
        };
        let document = Document::new(&ast.text, 0);
        let insertion = insert_statement(
            &document,
            scope.span,
            scope.tail.as_ref().map(|tail| tail.span().start()),
            &format!("eq({lhs}, {rhs});"),
            0..0,
        );
        drop(state_mut);

        self.apply_source_edit(url, scope_span.path, insertion.edit)
            .await;
    }

    async fn open_cell(self, _: tarpc::context::Context, cell: String) {
        self.editor_client
            .show_message(MessageType::INFO, &format!("cell {}", cell))
            .await;
        tokio::spawn(async move {
            let mut state_mut = self.state_mut.lock().await;
            state_mut.cell = Some(cell);
            state_mut.compile(&self.editor_client, false).await;
        });
    }

    async fn show_message(self, _: tarpc::context::Context, typ: MessageType, message: String) {
        self.editor_client.show_message(typ, message).await;
    }

    async fn dispatch_action(self, _: tarpc::context::Context, action: LangServerAction) {
        let result = match action {
            LangServerAction::Undo => self.editor_client.send_request::<Undo>(()).await,
            LangServerAction::Redo => self.editor_client.send_request::<Redo>(()).await,
        };
        if let Err(error) = result {
            self.editor_client
                .show_message(
                    MessageType::ERROR,
                    format!("Could not dispatch editor action: {error}"),
                )
                .await;
        }
    }

    async fn focus_editor(self, _: tarpc::context::Context, command: Option<String>) {
        if let Err(error) = self
            .editor_client
            .send_request::<crate::FocusEditor>(command)
            .await
        {
            self.editor_client
                .show_message(
                    MessageType::ERROR,
                    format!("Could not focus the editor: {error}"),
                )
                .await;
        }
    }
}

#[cfg(test)]
mod tests {
    use tower_lsp_server::ls_types::Position;

    use super::{Document, insert_statement, instance_placement_expression};

    #[test]
    fn placed_instances_use_initial_conditions() {
        assert_eq!(
            instance_placement_expression("child(10.)", 12.5, -4.0),
            "inst(child(10.), xi=12.5, yi=-4.0)"
        );
    }

    #[test]
    fn inserts_a_statement_before_an_existing_tail() {
        let source = "cell top() { x }\n";
        let scope_start = source.find('{').expect("scope should start");
        let scope_end = source.find('}').expect("scope should end") + 1;
        let tail_start = source.find('x').expect("tail should exist");
        let statement = "let r = rect()!;";
        let expression_start = "let r = ".len();
        let insertion = insert_statement(
            &Document::new(source, 0),
            cfgrammar::Span::new(scope_start, scope_end),
            Some(tail_start),
            statement,
            expression_start..expression_start + "rect()".len(),
        );

        assert_eq!(
            insertion.edit.new_text,
            format!("{statement}\n{}", " ".repeat(tail_start))
        );
        assert_eq!(
            insertion.tracked_span,
            cfgrammar::Span::new(
                tail_start + expression_start,
                tail_start + expression_start + "rect()".len(),
            )
        );
    }

    #[test]
    fn indents_a_statement_in_an_empty_multiline_scope() {
        let source = "cell top() {\n}\n";
        let scope_start = source.find('{').expect("scope should start");
        let closing_brace = source.find('}').expect("scope should end");
        let insertion = insert_statement(
            &Document::new(source, 0),
            cfgrammar::Span::new(scope_start, closing_brace + 1),
            None,
            "eq(x, y);",
            0..0,
        );

        assert_eq!(insertion.edit.range.start, Position::new(1, 0));
        assert_eq!(insertion.edit.new_text, "    eq(x, y);\n");
    }
}
