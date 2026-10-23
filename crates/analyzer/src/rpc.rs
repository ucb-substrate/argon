//! The analyzer's side of the GUI RPC services.

pub use protocol::rpc::*;

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use arcstr::Substr;
use argonc::{
    ast::{CellDecl, Decl, Span},
    compile::{BasicRect, CompiledData, DeviceKind},
    parse::{AnnotatedParseAst, ParseMetadata, WorkspaceParseAst},
};
use tarpc::context;
use tower_lsp_server::ls_types::{
    Diagnostic, DiagnosticSeverity, MessageType, Position, Range, ShowDocumentParams, TextEdit,
    Uri, WorkspaceEdit,
};

use crate::{
    Backend, PENDING_EDIT_TIMEOUT, PublishedState, Redo, Save, SourceState, State, Undo,
    document::Document, schematic_edit,
};

/// Applies non-overlapping replacements to one file's text.
fn apply_value_edits(text: &str, path: &Path, edits: &[ValueEdit]) -> String {
    let mut edits = edits
        .iter()
        .filter(|edit| *edit.span.path == *path)
        .collect::<Vec<_>>();
    edits.sort_by_key(|edit| std::cmp::Reverse(edit.span.span.start()));
    let mut text = text.to_owned();
    for edit in edits {
        text.replace_range(edit.span.span.start()..edit.span.span.end(), &edit.value);
    }
    text
}

/// Drag edits the analyzer applied on top of one compile revision, so a later
/// drag whose spans were recorded against that revision can still be placed.
#[derive(Debug, Default)]
pub(crate) struct DragEditHistory {
    base_revision: Option<u64>,
    /// Applied batches in order, each in the coordinates of the text before it.
    batches: Vec<Vec<ValueEdit>>,
    /// Each edited file's text after the batches.
    texts: HashMap<PathBuf, String>,
}

impl DragEditHistory {
    /// The batches that carry spans recorded against `base_revision` onto the
    /// current text, or `None` unless that text is exactly what they produced.
    fn batches_since(
        &self,
        base_revision: u64,
        current: impl Fn(&Path) -> Option<String>,
    ) -> Option<&[Vec<ValueEdit>]> {
        (self.base_revision == Some(base_revision)
            && self
                .texts
                .iter()
                .all(|(path, text)| current(path).as_deref() == Some(text.as_str())))
        .then_some(&self.batches)
    }

    /// Records a batch applied to `texts`, starting afresh from `base_revision`
    /// when the batch was placed without earlier ones.
    fn record(
        &mut self,
        base_revision: u64,
        fresh: bool,
        texts: &HashMap<PathBuf, String>,
        batch: Vec<ValueEdit>,
    ) {
        if fresh {
            *self = Self {
                base_revision: Some(base_revision),
                ..Self::default()
            };
        }
        for (path, text) in texts {
            if batch.iter().any(|edit| *edit.span.path == **path) {
                self.texts
                    .insert(path.clone(), apply_value_edits(text, path, &batch));
            }
        }
        self.batches.push(batch);
    }
}

/// How long a drag waits for the analyzer's own earlier edits to reach the editor.
const DRAG_EDIT_SETTLE_TIMEOUT: Duration = Duration::from_secs(1);

pub(crate) const OUT_OF_SYNC_MESSAGE: &str = "Editor buffer state is inconsistent with GUI state.";
pub(crate) const STALE_DRAG_MESSAGE: &str =
    "The source changed while dragging, so the drag was not applied. Drag again.";
pub(crate) const STALE_SNAPSHOT_MESSAGE: &str =
    "The schematic changed since this gesture began; it was not applied.";
pub(crate) const PENDING_EDIT_MESSAGE: &str =
    "An earlier edit is still being applied; try again in a moment.";
pub(crate) const READ_ONLY_GENERATED_SOURCE_MESSAGE: &str =
    "Imported GDS cells are read-only in the GUI. Open an Argon source cell to edit geometry.";

const NOT_IN_WORKSPACE_MESSAGE: &str =
    "The selected GUI object is no longer part of the current Argon workspace.";

pub(crate) fn source_edit_error(ast: &WorkspaceParseAst, span: &Span) -> Option<&'static str> {
    let Some(ast) = ast.values().find(|ast| ast.path == span.path) else {
        return Some(NOT_IN_WORKSPACE_MESSAGE);
    };
    (span.span.end() > ast.source_text.len()).then_some(READ_ONLY_GENERATED_SOURCE_MESSAGE)
}

/// Re-find a scope after queued edits. A scope is safe to match by the same
/// opening offset when the prefix is unchanged, or by a shifted offset when
/// the complete suffix beginning at that scope is unchanged. If neither side
/// anchors it, an old byte offset could select a different scope.
fn scope_after_insertion(
    ast: &AnnotatedParseAst,
    old_scope: &Span,
    old_source: &str,
) -> Option<Span> {
    let old_start = old_scope.span.start();
    let current = ast.source_text.as_str();
    let prefix_unchanged = old_start < old_source.len()
        && old_start < current.len()
        && old_source.as_bytes()[..=old_start] == current.as_bytes()[..=old_start];
    let start = if prefix_unchanged {
        old_start
    } else {
        let suffix = old_source.get(old_start..)?;
        current.strip_suffix(suffix).map(str::len)?
    };
    if prefix_unchanged && ast.span2scope.contains_key(old_scope) {
        return Some(old_scope.clone());
    }
    let mut matching = ast
        .span2scope
        .keys()
        .filter(|span| span.path == old_scope.path && span.span.start() == start);
    let scope = matching.next()?.clone();
    matching.next().is_none().then_some(scope)
}

fn available_rect_name(requested: String, used: &std::collections::HashSet<&str>) -> String {
    if !used.contains(requested.as_str()) {
        return requested;
    }
    (0..)
        .map(|index| format!("rect{index}"))
        .find(|name| !used.contains(name.as_str()))
        .expect("a finite scope has a free rectangle name")
}

pub(crate) fn editor_buffers_are_current(source: &SourceState, compiled: &PublishedState) -> bool {
    compiled.ast.values().all(|ast| {
        Uri::from_file_path(&ast.path)
            .map(|uri| {
                source.pending_workspace_edits.contains_key(&uri)
                    || source
                        .editor_files
                        .get(&uri)
                        .is_none_or(|document| document.contents() == ast.source_text)
            })
            .unwrap_or(true)
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
        let line = document
            .substr(Position::new(stop.line, 0)..stop)
            .unwrap_or_default();
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

/// The start of a `let` binding `name` in `scope`. Bindings at the top level
/// of a cell are `pub`, so that a parent can read them through an instance.
fn let_prefix(ast: &AnnotatedParseAst, scope: cfgrammar::Span, name: &str) -> String {
    let cell_body = ast
        .ast
        .decls
        .iter()
        .any(|decl| matches!(decl, Decl::Cell(cell) if cell.scope.span == scope));
    if cell_body {
        format!("pub let {name} = ")
    } else {
        format!("let {name} = ")
    }
}

fn polygon_expression(polygon: &PolygonParams) -> String {
    let coordinates = polygon
        .points
        .iter()
        .enumerate()
        .map(|(index, (x, y))| format!("x{index}i = {x:?}, y{index}i = {y:?},"))
        .collect::<Vec<_>>()
        .join("\n        ");
    format!(
        "std::layout::polygon({:?}, {},\n        {}\n    )",
        polygon.layer,
        polygon.points.len(),
        coordinates
    )
}

fn path_expression(path: &PathParams) -> String {
    let coordinates = path
        .points
        .iter()
        .enumerate()
        .map(|(index, (x, y))| format!("x{index}i = {x:?}, y{index}i = {y:?},"))
        .collect::<Vec<_>>()
        .join("\n        ");
    format!(
        "std::layout::path({:?}, {},\n        widthi = {:?},\n        {}\n    )",
        path.layer,
        path.points.len(),
        path.width,
        coordinates
    )
}

fn segment_constraint_statements(variable: &str, constraints: &[DrawSegmentConstraint]) -> String {
    constraints
        .iter()
        .filter_map(|constraint| {
            let end = match *constraint {
                DrawSegmentConstraint::Horizontal(end)
                | DrawSegmentConstraint::Vertical(end)
                | DrawSegmentConstraint::DiagonalPositive(end)
                | DrawSegmentConstraint::DiagonalNegative(end) => end,
            };
            let start = end.checked_sub(1)?;
            let expression = match *constraint {
                DrawSegmentConstraint::Horizontal(end) => {
                    format!("{variable}.y{end}, {variable}.y{start}")
                }
                DrawSegmentConstraint::Vertical(end) => {
                    format!("{variable}.x{end}, {variable}.x{start}")
                }
                DrawSegmentConstraint::DiagonalPositive(end) => format!(
                    "{variable}.x{end} - {variable}.x{start}, {variable}.y{end} - {variable}.y{start}"
                ),
                DrawSegmentConstraint::DiagonalNegative(end) => format!(
                    "{variable}.x{end} - {variable}.x{start}, {variable}.y{start} - {variable}.y{end}"
                ),
            };
            Some(format!("\n    eq({expression});"))
        })
        .collect()
}

fn missing_initial_condition_edit(
    source: &str,
    call_span: &Span,
    values: Vec<(String, String)>,
) -> Option<ValueEdit> {
    let (range, value) = schematic_edit::ensure_keywords(source, call_span.span, &values)?;
    Some(ValueEdit {
        span: Span {
            path: call_span.path.clone(),
            span: cfgrammar::Span::new(range.start, range.end),
        },
        value,
    })
}

fn instance_placement_expression(invocation: &str, x: f64, y: f64) -> String {
    format!("std::layout::inst({invocation}, xi={x:?}, yi={y:?})")
}

impl State {
    pub(crate) async fn report_message(&self, typ: MessageType, message: impl Into<String>) {
        let message = message.into();
        self.editor_client.show_message(typ, message.clone()).await;
        if typ != MessageType::LOG
            && let Some(connection) = self.gui_connection().await
        {
            let _ = connection
                .client
                .show_message(context::current(), typ, message)
                .await;
        }
    }

    /// Waits for the analyzer's own earlier edits to reach `paths`, then
    /// returns the batches that carry spans recorded against `base_revision`
    /// onto the current text, whether none were needed, and that text.
    async fn drag_edit_base(
        &self,
        history: &DragEditHistory,
        base_revision: u64,
        paths: &HashSet<PathBuf>,
    ) -> Option<(Vec<Vec<ValueEdit>>, bool, HashMap<PathBuf, String>)> {
        let paths = paths
            .iter()
            .chain(history.texts.keys())
            .cloned()
            .collect::<HashSet<_>>();
        let uris = paths
            .iter()
            .filter_map(Uri::from_file_path)
            .collect::<Vec<_>>();
        let deadline = tokio::time::Instant::now() + DRAG_EDIT_SETTLE_TIMEOUT;
        loop {
            {
                let source = self.source_state.lock().await;
                if !uris
                    .iter()
                    .any(|uri| source.pending_workspace_edits.contains_key(uri))
                {
                    let compiled = self.published_state.lock().await;
                    let text = |path: &Path| {
                        Uri::from_file_path(path)
                            .and_then(|uri| source.editor_files.get(&uri))
                            .map(|document| document.contents().to_string())
                            .or_else(|| {
                                compiled
                                    .ast
                                    .values()
                                    .find(|ast| *ast.path == *path)
                                    .map(|ast| ast.source_text.to_string())
                            })
                    };
                    let placed = if source.revision == base_revision {
                        Some((Vec::new(), true))
                    } else {
                        history
                            .batches_since(base_revision, text)
                            .map(|batches| (batches.to_vec(), false))
                    };
                    if let Some((batches, fresh)) = placed {
                        let texts = paths
                            .iter()
                            .filter_map(|path| Some((path.clone(), text(path)?)))
                            .collect();
                        return Some((batches, fresh, texts));
                    }
                    // Only the analyzer's own edits, still arriving, could
                    // make the text match.
                    if history.base_revision != Some(base_revision) {
                        return None;
                    }
                }
            }
            if tokio::time::Instant::now() >= deadline {
                return None;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    /// Marks an analyzer edit to `uri` as in flight until the editor reports
    /// `expected`, the file's text with all of the edit applied.
    pub(crate) async fn begin_pending_edit(&self, uri: &Uri, expected: String) {
        self.source_state
            .lock()
            .await
            .pending_workspace_edits
            .insert(uri.clone(), expected);
    }

    /// Withdraws the in-flight edit that leaves `uri` as `expected`, unless
    /// it has completed or a newer edit replaced it. Returns whether it was
    /// still in flight.
    pub(crate) async fn end_pending_edit(&self, uri: &Uri, expected: &str) -> bool {
        let mut state = self.source_state.lock().await;
        let in_flight = state
            .pending_workspace_edits
            .get(uri)
            .is_some_and(|pending| pending == expected);
        if in_flight {
            state.pending_workspace_edits.shift_remove(uri);
        }
        in_flight
    }

    /// The text each file that `changes` alters will have once the editor
    /// applies them, to its buffer or, when it has none, to the file on disk.
    async fn edited_texts(&self, changes: &HashMap<Uri, Vec<TextEdit>>) -> Vec<(Uri, String)> {
        let mut texts = Vec::new();
        for (uri, edits) in changes {
            let open = self
                .source_state
                .lock()
                .await
                .editor_files
                .get(uri)
                .cloned();
            let document = match open {
                Some(document) => document,
                None => {
                    let Some(path) = uri.to_file_path() else {
                        continue;
                    };
                    let Ok(text) = tokio::fs::read_to_string(&path).await else {
                        continue;
                    };
                    self.document(text)
                }
            };
            let edited = document.edited(edits);
            if edited != document.contents() {
                texts.push((uri.clone(), edited));
            }
        }
        texts
    }

    /// Waits until the editor reports all of each edit in `pending`,
    /// withdrawing the ones it has not reported in time.
    async fn wait_for_pending_edits(&self, pending: &[(Uri, String)]) {
        let in_flight = |source: &SourceState| {
            pending
                .iter()
                .any(|(uri, expected)| source.pending_workspace_edits.get(uri) == Some(expected))
        };
        let arrived = self
            .wait_for_source(PENDING_EDIT_TIMEOUT, |source| {
                (!in_flight(source)).then_some(())
            })
            .await;
        if arrived.is_none() {
            for (uri, expected) in pending {
                self.end_pending_edit(uri, expected).await;
            }
        }
    }

    /// The document GUI undo and redo apply to.
    pub(crate) fn history_params(&self) -> HistoryParams {
        HistoryParams {
            uri: self
                .last_source_edit
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone(),
        }
    }

    /// Applies `changes` through the editor as one edit, returning once the
    /// editor has reported all of it.
    pub(crate) async fn apply_source_changes(
        &self,
        changes: HashMap<Uri, Vec<TextEdit>>,
        focus: Option<Uri>,
    ) -> bool {
        let edited = match &focus {
            Some(uri) => Some(uri.clone()),
            None if changes.len() == 1 => changes.keys().next().cloned(),
            None => None,
        };
        if edited.is_some() {
            *self
                .last_source_edit
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = edited;
        }
        let pending = self.edited_texts(&changes).await;
        for (uri, expected) in &pending {
            self.begin_pending_edit(uri, expected.clone()).await;
        }
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

            Ok(())
        }
        .await;

        if let Err(error) = result {
            for (uri, expected) in &pending {
                self.end_pending_edit(uri, expected).await;
            }
            self.report_message(MessageType::ERROR, error).await;
            false
        } else {
            self.wait_for_pending_edits(&pending).await;
            true
        }
    }

    pub(crate) async fn apply_source_edit(&self, uri: Uri, edit: TextEdit) -> bool {
        self.apply_source_changes(HashMap::from([(uri.clone(), vec![edit])]), Some(uri))
            .await
    }

    /// The published workspace and the snapshot the GUI holds, when a
    /// schematic edit drawn on `base_revision` that touches `spans` is safe:
    /// that revision is the published one, the editor buffers match it, and
    /// none of the touched files has an edit still being applied.
    pub(crate) async fn schematic_edit_context<'s>(
        &self,
        base_revision: u64,
        spans: impl Iterator<Item = &'s Span>,
    ) -> std::result::Result<(Arc<WorkspaceParseAst>, CompilationSnapshot), &'static str> {
        let spans = spans.collect::<Vec<_>>();
        let workspace_ast = {
            let source = self.source_state.lock().await;
            let compiled = self.published_state.lock().await;
            if compiled.compiled_revision != base_revision {
                return Err(STALE_SNAPSHOT_MESSAGE);
            }
            if !editor_buffers_are_current(&source, &compiled) {
                return Err(OUT_OF_SYNC_MESSAGE);
            }
            for span in &spans {
                if let Some(error) = source_edit_error(&compiled.ast, span) {
                    return Err(error);
                }
                let pending = Uri::from_file_path(&span.path)
                    .is_some_and(|uri| source.pending_workspace_edits.contains_key(&uri));
                if pending {
                    return Err(PENDING_EDIT_MESSAGE);
                }
            }
            compiled.ast.clone()
        };
        let connection = self
            .gui_connection()
            .await
            .ok_or("The GUI is not connected.")?;
        let snapshot = connection.snapshot.lock().await.clone();
        match snapshot {
            Some(snapshot) if snapshot.revision == base_revision => Ok((workspace_ast, snapshot)),
            _ => Err(STALE_SNAPSHOT_MESSAGE),
        }
    }

    /// [`Self::schematic_edit_context`], reporting a refusal to the editor
    /// and the GUI.
    async fn schematic_context<'s>(
        &self,
        base_revision: u64,
        spans: impl Iterator<Item = &'s Span>,
    ) -> Option<(Arc<WorkspaceParseAst>, CompilationSnapshot)> {
        match self.schematic_edit_context(base_revision, spans).await {
            Ok(context) => Some(context),
            Err(message) => {
                self.report_message(MessageType::ERROR, message).await;
                None
            }
        }
    }

    /// Applies `replacements` through the editor as one edit, returning the
    /// source revision afterward. Nothing to apply is a success.
    async fn apply_replacements(
        &self,
        workspace_ast: &WorkspaceParseAst,
        mut replacements: schematic_edit::FileReplacements,
    ) -> Option<u64> {
        let mut changes: HashMap<Uri, Vec<TextEdit>> = HashMap::new();
        for (path, replacements) in &mut replacements {
            replacements.sort_by_key(|replacement| std::cmp::Reverse(replacement.0.start));
            let (Some(ast), Some(uri)) = (
                schematic_edit::module_at(workspace_ast, path),
                Uri::from_file_path(path),
            ) else {
                continue;
            };
            let document = self.document(&ast.source_text);
            let edits = replacements
                .iter()
                .map(|(range, text)| TextEdit {
                    range: Range::new(
                        document.offset_to_pos(range.start),
                        document.offset_to_pos(range.end),
                    ),
                    new_text: text.clone(),
                })
                .collect::<Vec<_>>();
            if !edits.is_empty() {
                changes.insert(uri, edits);
            }
        }
        if !changes.is_empty() && !self.apply_source_changes(changes, None).await {
            return None;
        }
        Some(self.source_state.lock().await.revision)
    }

    /// Adds `statements` to the end of the body of the cell at `target`, with
    /// `placements` applied in the same edit. The receipt holds the span of
    /// the call in each statement.
    async fn insert_statements(
        &self,
        workspace_ast: &WorkspaceParseAst,
        data: &CompiledData,
        target: &SchematicTarget,
        statements: Result<Vec<String>, String>,
        placements: &[PlacementEdit],
    ) -> Option<EditReceipt> {
        let result = statements.and_then(|statements| {
            let (ast, cell) = cell_body(workspace_ast, data, &target.cell)?;
            let (mut replacements, _) = placement_replacements(workspace_ast, data, placements)?;
            let insertion =
                schematic_edit::insert_statements(&ast.source_text, &cell.scope, &statements);
            let file = replacements
                .entry(target.cell.path.to_path_buf())
                .or_default();
            let spans = statements
                .iter()
                .zip(&insertion.statements)
                .map(|(statement, range)| {
                    let call = schematic_edit::call_range(statement);
                    Span {
                        path: target.cell.path.clone(),
                        span: schematic_edit::inserted_span(
                            &insertion.replacement,
                            &(range.start + call.start..range.start + call.end),
                            file,
                        ),
                    }
                })
                .collect::<Vec<_>>();
            file.push(insertion.replacement);
            Ok((replacements, spans))
        });
        let (replacements, spans) = match result {
            Ok(result) => result,
            Err(message) => {
                self.report_message(MessageType::ERROR, message).await;
                return None;
            }
        };
        let revision = self.apply_replacements(workspace_ast, replacements).await?;
        Some(EditReceipt { revision, spans })
    }
}

/// The module and declaration of the cell whose body is `cell`, when the
/// snapshot compiled it.
fn cell_body<'a>(
    workspace_ast: &'a WorkspaceParseAst,
    data: &CompiledData,
    cell: &Span,
) -> std::result::Result<(&'a AnnotatedParseAst, &'a CellDecl<Substr, ParseMetadata>), String> {
    let ast = schematic_edit::module_at(workspace_ast, &cell.path)
        .ok_or_else(|| OUT_OF_SYNC_MESSAGE.to_owned())?;
    let decl = schematic_edit::cell_with_body(ast, cell.span)
        .ok_or_else(|| STALE_SNAPSHOT_MESSAGE.to_owned())?;
    let compiled = data.cells.values().any(|compiled| {
        compiled.scopes.contains_key(&compiled.root) && *compiled.scope_span(compiled.root) == *cell
    });
    if !compiled {
        return Err(STALE_SNAPSHOT_MESSAGE.to_owned());
    }
    Ok((ast, decl))
}

/// A compiled cell whose body is `cell`; any set of arguments will do for
/// the ports and symbol.
fn compiled_cell<'a>(
    data: &'a CompiledData,
    cell: &Span,
) -> std::result::Result<&'a argonc::compile::CompiledCell, String> {
    data.cells
        .values()
        .map(|compiled| &**compiled)
        .find(|compiled| {
            compiled.scopes.contains_key(&compiled.root)
                && *compiled.scope_span(compiled.root) == *cell
        })
        .ok_or_else(|| STALE_SNAPSHOT_MESSAGE.to_owned())
}

/// Checks a device the GUI wants to place, returning its kind.
fn check_device(
    kind: &str,
    terminals: usize,
    args: &str,
) -> std::result::Result<DeviceKind, String> {
    let kind = DeviceKind::from_variant(kind)
        .ok_or_else(|| format!("`{kind}` is not a `std::schematic::DeviceKind`."))?;
    kind.check_terminals(terminals)
        .map_err(|error| format!("A `{}` device {error}.", kind.name()))?;
    let call = schematic_edit::device_statement(
        "d",
        kind.name(),
        terminals,
        args,
        &ElementPlacement::default(),
    );
    let call = &call[schematic_edit::call_range(&call)];
    argonc::parse::parse_cell(call)
        .map_err(|error| format!("The device arguments do not parse: {error}"))?;
    Ok(kind)
}

/// The keyword rewrites of `edits`, per file, with the call each edit names.
fn placement_replacements(
    workspace_ast: &WorkspaceParseAst,
    data: &CompiledData,
    edits: &[PlacementEdit],
) -> std::result::Result<(schematic_edit::FileReplacements, Vec<Span>), String> {
    let mut per_file: schematic_edit::FileReplacements = HashMap::new();
    let mut calls = Vec::with_capacity(edits.len());
    for edit in edits {
        let ast = schematic_edit::module_at(workspace_ast, &edit.call.path)
            .ok_or_else(|| OUT_OF_SYNC_MESSAGE.to_owned())?;
        let (Some(call), Some((kind, editable))) = (
            ast.span2call.get(&edit.call),
            schematic_edit::site_of(data, &edit.call),
        ) else {
            return Err(STALE_SNAPSHOT_MESSAGE.to_owned());
        };
        if !editable {
            return Err(format!(
                "This {} cannot be edited from the GUI.",
                kind.describe()
            ));
        }
        if let Some(keyword) = edit
            .keywords
            .iter()
            .find(|keyword| !kind.keywords().contains(&keyword.name.as_str()))
        {
            return Err(format!(
                "`{}` is not a keyword of a {}.",
                keyword.name,
                kind.describe()
            ));
        }
        // The points of a line or polygon are its first argument.
        let (points, keywords): (Vec<_>, Vec<_>) = edit
            .keywords
            .iter()
            .cloned()
            .partition(|keyword| keyword.name == schematic_edit::POINTS);
        let mut replacements =
            schematic_edit::keyword_replacements(&ast.source_text, call, &keywords);
        for points in points {
            let value = points
                .value
                .ok_or_else(|| "A line or polygon keeps its points.".to_owned())?;
            replacements.extend(schematic_edit::argument_replacements(
                &ast.source_text,
                call,
                &CallArgument::Positional(0),
                &value,
            )?);
        }
        per_file
            .entry(edit.call.path.to_path_buf())
            .or_default()
            .extend(replacements);
        calls.push(Span {
            path: edit.call.path.clone(),
            span: call.span,
        });
    }
    Ok((per_file, calls))
}

impl State {
    /// Adopts a GUI that connected back for callbacks, replacing any earlier
    /// one, and sends it the session's configuration and current cell.
    pub(crate) async fn connect_gui(self, gui_client: GuiClient) {
        let connection = self.install_gui_connection(gui_client.clone()).await;
        let modified = self.source_state.lock().await.workspace_modified;
        if let Err(error) = gui_client
            .configure(context::current(), self.config())
            .await
        {
            self.editor_client
                .show_message(
                    MessageType::ERROR,
                    format!("Could not configure the GUI: {error}"),
                )
                .await;
            if crate::is_gui_disconnected(&error) {
                self.clear_gui_connection(connection.id).await;
                return;
            }
        }
        if !self.publish_workspace_path(Some(connection.clone())).await {
            return;
        }
        self.publish_workspace_modified(modified, Some(connection.clone()))
            .await;
        self.publish_cells(Some(connection.clone())).await;
        self.publish_recent_cells(Some(connection)).await;
        Backend { state: self }.open_current().await;
    }
}

impl LangServer for State {
    async fn select_span(self, _: tarpc::context::Context, span: Span) {
        // TODO: check that vim file is in sync with GUI file.
        let compiled = self.published_state.lock().await;
        if let Some(ast) = compiled.ast.values().find(|ast| ast.path == span.path) {
            let doc = self.document(&ast.source_text);
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
            drop(compiled);
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
    ) -> Option<RectangleEditResult> {
        let published_source = {
            let published = self.published_state.lock().await;
            if let Some(error) = source_edit_error(&published.ast, &scope_span) {
                drop(published);
                self.report_message(MessageType::ERROR, error).await;
                return None;
            }
            published
                .ast
                .values()
                .find(|ast| ast.path == scope_span.path)?
                .source_text
                .clone()
        };
        let url = Uri::from_file_path(&scope_span.path)?;
        // The editor can acknowledge an insertion before its didChange event
        // reaches the analyzer. Wait for that local source update, not for the
        // entire layout to compile, before inserting the next queued shape.
        let wait_started = std::time::Instant::now();
        let source_text = loop {
            let source = self.source_state.lock().await;
            if !source.pending_workspace_edits.contains_key(&url) {
                break source
                    .editor_files
                    .get(&url)
                    .map(|document| document.contents().to_owned())
                    .unwrap_or_else(|| published_source.to_string());
            }
            drop(source);
            if wait_started.elapsed() >= std::time::Duration::from_secs(3) {
                self.report_message(
                    MessageType::ERROR,
                    "Timed out waiting for the editor to apply the previous drawing.",
                )
                .await;
                return None;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        };
        let ast = match argonc::parse::parse_source_text(source_text, scope_span.path.to_path_buf())
        {
            Ok(ast) => ast,
            Err(error) => {
                self.report_message(MessageType::ERROR, error.to_string())
                    .await;
                return None;
            }
        };
        let current_scope = match scope_after_insertion(&ast, &scope_span, &published_source) {
            Some(scope) => scope,
            None => {
                self.report_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                    .await;
                return None;
            }
        };
        let scope = &ast.span2scope[&current_scope];
        let used_names = scope
            .stmts
            .iter()
            .filter_map(|statement| match statement {
                argonc::ast::Statement::LetBinding(binding) => Some(binding.name.name.as_str()),
                _ => None,
            })
            .collect::<std::collections::HashSet<_>>();
        let var_name = available_rect_name(var_name, &used_names);
        let document = self.document(&ast.source_text);
        let grid = self.technology_grid().await;
        let expression = format!(
            "std::layout::rect({}x0i = {}, y0i = {}, x1i = {}, y1i = {})",
            rect.layer
                .as_ref()
                .map(|layer| format!("\"{layer}\", "))
                .unwrap_or_default(),
            argonc::compile::format_initial_condition(rect.x0, grid),
            argonc::compile::format_initial_condition(rect.y0, grid),
            argonc::compile::format_initial_condition(rect.x1, grid),
            argonc::compile::format_initial_condition(rect.y1, grid),
        );
        let prefix = let_prefix(&ast, scope.span, &var_name);
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
        if !self.apply_source_edit(url, insertion.edit).await {
            return None;
        }
        Some(RectangleEditResult {
            span,
            revision: self.source_state.lock().await.revision,
            name: var_name,
        })
    }

    async fn draw_polygon(
        self,
        _: tarpc::context::Context,
        scope_span: Span,
        var_name: String,
        polygon: PolygonParams,
    ) -> Option<Span> {
        if polygon.points.len() < 3 {
            return None;
        }
        let Some(workspace_ast) = self.current_editor_ast().await else {
            self.report_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                .await;
            return None;
        };
        if let Some(error) = source_edit_error(&workspace_ast, &scope_span) {
            self.report_message(MessageType::ERROR, error).await;
            return None;
        }
        let url = Uri::from_file_path(&scope_span.path)?;
        let ast = workspace_ast
            .values()
            .find(|ast| ast.path == scope_span.path)?;
        let scope = ast.span2scope.get(&scope_span)?;
        let document = self.document(&ast.source_text);
        let expression = polygon_expression(&polygon);
        let prefix = let_prefix(ast, scope.span, &var_name);
        let constraints = segment_constraint_statements(&var_name, &polygon.constraints);
        let insertion = insert_statement(
            &document,
            scope.span,
            scope.tail.as_ref().map(|tail| tail.span().start()),
            &format!("{prefix}{expression}!;{constraints}"),
            prefix.len()..prefix.len() + expression.len(),
        );
        let span = Span {
            path: scope_span.path.clone(),
            span: insertion.tracked_span,
        };
        self.apply_source_edit(url, insertion.edit)
            .await
            .then_some(span)
    }

    async fn draw_path(
        self,
        _: tarpc::context::Context,
        scope_span: Span,
        var_name: String,
        path: PathParams,
    ) -> Option<Span> {
        if path.points.len() < 2 || !path.width.is_finite() || path.width <= 0. {
            return None;
        }
        let Some(workspace_ast) = self.current_editor_ast().await else {
            self.report_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                .await;
            return None;
        };
        if let Some(error) = source_edit_error(&workspace_ast, &scope_span) {
            self.report_message(MessageType::ERROR, error).await;
            return None;
        }
        let url = Uri::from_file_path(&scope_span.path)?;
        let ast = workspace_ast
            .values()
            .find(|ast| ast.path == scope_span.path)?;
        let scope = ast.span2scope.get(&scope_span)?;
        let document = self.document(&ast.source_text);
        let expression = path_expression(&path);
        let prefix = let_prefix(ast, scope.span, &var_name);
        let constraints = segment_constraint_statements(&var_name, &path.constraints);
        let insertion = insert_statement(
            &document,
            scope.span,
            scope.tail.as_ref().map(|tail| tail.span().start()),
            &format!("{prefix}{expression}!;{constraints}"),
            prefix.len()..prefix.len() + expression.len(),
        );
        let span = Span {
            path: scope_span.path.clone(),
            span: insertion.tracked_span,
        };
        self.apply_source_edit(url, insertion.edit)
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
        let Some(workspace_ast) = self.current_editor_ast().await else {
            self.report_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                .await;
            return None;
        };
        if let Some(error) = source_edit_error(&workspace_ast, &scope_span) {
            self.report_message(MessageType::ERROR, error).await;
            return None;
        }
        let url = Uri::from_file_path(&scope_span.path)?;
        let ast = workspace_ast
            .values()
            .find(|ast| ast.path == scope_span.path)?;
        let scope = ast.span2scope.get(&scope_span)?;
        let document = self.document(&ast.source_text);
        let names = scope
            .stmts
            .iter()
            .flat_map(|statement| statement.let_names())
            .map(|name| name.name.as_str())
            .collect::<std::collections::HashSet<_>>();
        let var_name = (0..)
            .map(|index| format!("inst{index}"))
            .find(|name| !names.contains(name.as_str()))?;
        let expression = instance_placement_expression(&invocation, x, y);
        let prefix = let_prefix(ast, scope.span, &var_name);
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
        self.apply_source_edit(url, insertion.edit)
            .await
            .then_some(updated_scope_span)
    }

    async fn draw_dimension(
        self,
        _: tarpc::context::Context,
        scope_span: Span,
        params: DimensionParams,
    ) -> Option<Span> {
        let Some(workspace_ast) = self.current_editor_ast().await else {
            self.report_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                .await;
            return None;
        };
        if let Some(error) = source_edit_error(&workspace_ast, &scope_span) {
            self.report_message(MessageType::ERROR, error).await;
            return None;
        }
        let url = Uri::from_file_path(&scope_span.path)?;
        let ast = workspace_ast
            .values()
            .find(|ast| ast.path == scope_span.path)?;
        let scope = ast.span2scope.get(&scope_span)?;
        let document = self.document(&ast.source_text);
        let expression = format!(
            "std::layout::dimension({}, {}, {}, {}, {}, {}, {})",
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
        self.apply_source_edit(url, insertion.edit)
            .await
            .then_some(span)
    }

    async fn edit_dimension(
        self,
        _: tarpc::context::Context,
        span: Span,
        value: String,
    ) -> Option<Span> {
        let Some(workspace_ast) = self.current_editor_ast().await else {
            self.report_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                .await;
            return None;
        };
        if let Some(error) = source_edit_error(&workspace_ast, &span) {
            self.report_message(MessageType::ERROR, error).await;
            return None;
        }
        let url = Uri::from_file_path(&span.path)?;
        let ast = workspace_ast.values().find(|ast| ast.path == span.path)?;
        let call = ast.span2call.get(&span)?;
        let old_value = call.args.posargs.get(2)?;
        let document = self.document(&ast.source_text);
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
        self.apply_source_edit(url, edit)
            .await
            .then_some(updated_span)
    }

    /// Rewrites existing initial-condition values and inserts any missing
    /// geometry kwargs in one workspace edit. The resulting `didChange`
    /// recompiles the dirty in-memory buffers without saving them.
    async fn update_values(
        self,
        _: tarpc::context::Context,
        mut edits: Vec<ValueEdit>,
        mut initial_conditions: Vec<InitialConditionEdit>,
        base_revision: Option<u64>,
    ) -> Option<Vec<ValueEdit>> {
        if edits.is_empty() && initial_conditions.is_empty() {
            return Some(Vec::new());
        }
        // Drags are placed one at a time, each onto the text the one before
        // it left. Spans recorded against any other text would overwrite the
        // wrong bytes.
        let mut history = self.drag_edits.lock().await;
        let paths = edits
            .iter()
            .map(|edit| edit.span.path.to_path_buf())
            .chain(
                initial_conditions
                    .iter()
                    .map(|initial_condition| initial_condition.call_span.path.to_path_buf()),
            )
            .collect::<HashSet<_>>();
        let placed = match base_revision {
            Some(base_revision) => self.drag_edit_base(&history, base_revision, &paths).await,
            None => None,
        };
        let (Some(base_revision), Some((batches, fresh, texts))) = (base_revision, placed) else {
            self.report_message(MessageType::ERROR, STALE_DRAG_MESSAGE)
                .await;
            return None;
        };
        for batch in &batches {
            for edit in &mut edits {
                edit.span = remap_span_after_value_edits(&edit.span, batch);
            }
            for initial_condition in &mut initial_conditions {
                initial_condition.call_span =
                    remap_span_after_value_edits(&initial_condition.call_span, batch);
            }
        }
        let workspace_ast = self.published_state.lock().await.ast.clone();
        let source_error = edits
            .iter()
            .map(|edit| &edit.span)
            .chain(
                initial_conditions
                    .iter()
                    .map(|initial_condition| &initial_condition.call_span),
            )
            .find_map(|span| {
                if !workspace_ast.values().any(|ast| ast.path == span.path) {
                    return Some(NOT_IN_WORKSPACE_MESSAGE);
                }
                (span.span.end() > texts.get(&*span.path).map_or(0, String::len))
                    .then_some(READ_ONLY_GENERATED_SOURCE_MESSAGE)
            });
        if let Some(error) = source_error {
            self.report_message(MessageType::ERROR, error).await;
            return None;
        }

        // Resolve "ensure kwarg" requests against the current text. Existing
        // kwargs become ordinary value replacements. Missing kwargs sharing a
        // call are collected into one insertion so edit ordering is defined.
        let mut asts = HashMap::<PathBuf, AnnotatedParseAst>::new();
        let mut missing: HashMap<Span, Vec<(String, String)>> = HashMap::new();
        for InitialConditionEdit {
            call_span,
            name,
            value,
        } in initial_conditions
        {
            if !asts.contains_key(&*call_span.path) {
                let parsed = texts.get(&*call_span.path).and_then(|text| {
                    argonc::parse::parse_source_text(text.clone(), call_span.path.to_path_buf())
                        .ok()
                });
                let Some(ast) = parsed else {
                    self.report_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                        .await;
                    return None;
                };
                asts.insert(call_span.path.to_path_buf(), ast);
            }
            let Some(call) = asts[&*call_span.path].span2call.get(&call_span) else {
                continue;
            };
            if let Some(kwarg) = call
                .args
                .kwargs
                .iter()
                .find(|kwarg| kwarg.name.name == name)
            {
                let span = Span {
                    path: call_span.path.clone(),
                    span: kwarg.value.span(),
                };
                if !edits.iter().any(|edit| edit.span == span) {
                    edits.push(ValueEdit { span, value });
                }
            } else {
                let values = missing.entry(call_span).or_default();
                if let Some(existing) = values
                    .iter_mut()
                    .find(|(existing_name, _)| existing_name == &name)
                {
                    existing.1 = value;
                } else {
                    values.push((name, value));
                }
            }
        }

        for (call_span, values) in missing {
            if let Some(text) = texts.get(&*call_span.path)
                && let Some(edit) = missing_initial_condition_edit(text, &call_span, values)
            {
                edits.push(edit);
            }
        }

        // The same existing kwarg may be reached through dependent drag
        // targets. Keep one deterministic replacement for each source span.
        let mut deduplicated = Vec::<ValueEdit>::new();
        for edit in edits {
            if let Some(existing) = deduplicated
                .iter_mut()
                .find(|existing| existing.span == edit.span)
            {
                existing.value = edit.value;
            } else {
                deduplicated.push(edit);
            }
        }
        let edits = deduplicated;

        // Build one WorkspaceEdit grouping all rewrites per file. Edits within a
        // file are sorted by descending start offset so they can be applied
        // back-to-front without invalidating each other's offsets.
        let mut pending: HashMap<Uri, Vec<(usize, TextEdit)>> = HashMap::new();
        for ValueEdit { span, value } in &edits {
            if let Some(text) = texts.get(&*span.path)
                && let Some(uri) = Uri::from_file_path(&span.path)
            {
                let doc = self.document(text.as_str());
                let start = doc.offset_to_pos(span.span.start());
                let stop = doc.offset_to_pos(span.span.end());
                pending.entry(uri).or_default().push((
                    span.span.start(),
                    TextEdit {
                        range: Range::new(start, stop),
                        new_text: value.clone(),
                    },
                ));
            }
        }
        if pending.is_empty() {
            return None;
        }
        let changes = pending
            .into_iter()
            .map(|(uri, mut edits)| {
                edits.sort_by(|(left, _), (right, _)| right.cmp(left));
                (uri, edits.into_iter().map(|(_, edit)| edit).collect())
            })
            .collect();
        if !self.apply_source_changes(changes, None).await {
            return None;
        }
        history.record(base_revision, fresh, &texts, edits.clone());
        Some(edits)
    }

    /// Rewrites the placement and route keywords of schematic calls in one
    /// workspace edit, after checking that the GUI drew the gesture on the
    /// published snapshot and that every call is the one it came from.
    async fn place(self, _: tarpc::context::Context, request: PlaceRequest) -> Option<EditReceipt> {
        let (workspace_ast, snapshot) = self
            .schematic_context(
                request.base_revision,
                request.edits.iter().map(|edit| &edit.call),
            )
            .await?;
        let data = snapshot.data()?;
        let (replacements, calls) =
            match placement_replacements(&workspace_ast, data, &request.edits) {
                Ok(replacements) => replacements,
                Err(message) => {
                    self.report_message(MessageType::ERROR, message).await;
                    return None;
                }
            };
        let spans = calls
            .iter()
            .map(|call| Span {
                path: call.path.clone(),
                span: schematic_edit::span_after(call.span, &replacements[&*call.path]),
            })
            .collect();
        let revision = self
            .apply_replacements(&workspace_ast, replacements)
            .await?;
        Some(EditReceipt { revision, spans })
    }

    async fn add_signal(
        self,
        _: tarpc::context::Context,
        request: AddSignalRequest,
    ) -> Option<EditReceipt> {
        let touched = std::iter::once(&request.target.cell)
            .chain(request.placements.iter().map(|edit| &edit.call));
        let (workspace_ast, snapshot) = self
            .schematic_context(request.target.base_revision, touched)
            .await?;
        let data = snapshot.data()?;
        let statements = (|| {
            let (_, cell) = cell_body(&workspace_ast, data, &request.target.cell)?;
            let what = if request.port {
                "a port"
            } else {
                "a net label"
            };
            schematic_edit::check_new_name(&request.name, what, &schematic_edit::cell_names(cell))?;
            let mut statements = vec![schematic_edit::signal_statement(
                &request.name,
                request.port,
                request.at,
            )];
            if let Some(path) = &request.connect_to {
                schematic_edit::check_path(path)?;
                statements.push(schematic_edit::connect_statement(
                    &request.name,
                    path,
                    request.route.as_deref(),
                ));
            }
            Ok(statements)
        })();
        self.insert_statements(
            &workspace_ast,
            data,
            &request.target,
            statements,
            &request.placements,
        )
        .await
    }

    async fn place_schematic_instance(
        self,
        _: tarpc::context::Context,
        request: PlaceSchematicInstanceRequest,
    ) -> Option<EditReceipt> {
        let (workspace_ast, snapshot) = self
            .schematic_context(
                request.target.base_revision,
                std::iter::once(&request.target.cell),
            )
            .await?;
        let data = snapshot.data()?;
        let statements = (|| {
            let (_, cell) = cell_body(&workspace_ast, data, &request.target.cell)?;
            let base = schematic_edit::invoked_cell_name(&request.invocation)?;
            let name = schematic_edit::fresh_name(&base, &schematic_edit::cell_names(cell));
            Ok(vec![schematic_edit::instance_statement(
                &name,
                &request.invocation,
                &request.placement,
            )])
        })();
        self.insert_statements(&workspace_ast, data, &request.target, statements, &[])
            .await
    }

    async fn place_device(
        self,
        _: tarpc::context::Context,
        request: PlaceDeviceRequest,
    ) -> Option<EditReceipt> {
        let (workspace_ast, snapshot) = self
            .schematic_context(
                request.target.base_revision,
                std::iter::once(&request.target.cell),
            )
            .await?;
        let data = snapshot.data()?;
        let statements = (|| {
            let (_, cell) = cell_body(&workspace_ast, data, &request.target.cell)?;
            let kind = check_device(&request.kind, request.terminals, &request.args)?;
            let name = schematic_edit::fresh_name(
                &kind.name().to_lowercase(),
                &schematic_edit::cell_names(cell),
            );
            Ok(vec![schematic_edit::device_statement(
                &name,
                kind.name(),
                request.terminals,
                &request.args,
                &request.placement,
            )])
        })();
        self.insert_statements(&workspace_ast, data, &request.target, statements, &[])
            .await
    }

    async fn connect(
        self,
        _: tarpc::context::Context,
        request: ConnectRequest,
    ) -> Option<EditReceipt> {
        let touched = std::iter::once(&request.target.cell)
            .chain(request.placements.iter().map(|edit| &edit.call));
        let (workspace_ast, snapshot) = self
            .schematic_context(request.target.base_revision, touched)
            .await?;
        let data = snapshot.data()?;
        let statements = (|| {
            cell_body(&workspace_ast, data, &request.target.cell)?;
            schematic_edit::check_path(&request.a)?;
            schematic_edit::check_path(&request.b)?;
            Ok(vec![schematic_edit::connect_statement(
                &request.a,
                &request.b,
                request.route.as_deref(),
            )])
        })();
        self.insert_statements(
            &workspace_ast,
            data,
            &request.target,
            statements,
            &request.placements,
        )
        .await
    }

    async fn delete(
        self,
        _: tarpc::context::Context,
        request: DeleteRequest,
    ) -> Option<DeleteOutcome> {
        let (workspace_ast, snapshot) = self
            .schematic_context(request.base_revision, request.targets.iter())
            .await?;
        let data = snapshot.data()?;
        let index = self.nav_index().await;
        let plan = match schematic_edit::plan_deletion(
            &workspace_ast,
            data,
            index.as_deref(),
            &request.targets,
        ) {
            Ok(plan) => plan,
            Err(message) => {
                self.report_message(MessageType::ERROR, message).await;
                return None;
            }
        };
        if !plan.blocking.is_empty() {
            return Some(DeleteOutcome::Blocked(plan.blocking));
        }
        let revision = self
            .apply_replacements(&workspace_ast, plan.replacements)
            .await?;
        Some(DeleteOutcome::Applied(EditReceipt {
            revision,
            spans: Vec::new(),
        }))
    }

    async fn set_argument(
        self,
        _: tarpc::context::Context,
        request: SetArgumentRequest,
    ) -> Option<EditReceipt> {
        let (workspace_ast, snapshot) = self
            .schematic_context(request.base_revision, std::iter::once(&request.call))
            .await?;
        let data = snapshot.data()?;
        let result = (|| {
            let ast = schematic_edit::module_at(&workspace_ast, &request.call.path)
                .ok_or_else(|| OUT_OF_SYNC_MESSAGE.to_owned())?;
            let (Some(call), Some((kind, editable))) = (
                ast.span2call.get(&request.call),
                schematic_edit::site_of(data, &request.call),
            ) else {
                return Err(STALE_SNAPSHOT_MESSAGE.to_owned());
            };
            if !editable {
                return Err(format!(
                    "This {} cannot be edited from the GUI.",
                    kind.describe()
                ));
            }
            schematic_edit::check_expression(&request.value)?;
            let replacements = schematic_edit::argument_replacements(
                &ast.source_text,
                call,
                &request.argument,
                &request.value,
            )?;
            let span = schematic_edit::span_after(call.span, &replacements);
            Ok((
                HashMap::from([(request.call.path.to_path_buf(), replacements)]),
                Span {
                    path: request.call.path.clone(),
                    span,
                },
            ))
        })();
        let (replacements, span) = match result {
            Ok(result) => result,
            Err(message) => {
                self.report_message(MessageType::ERROR, message).await;
                return None;
            }
        };
        let revision = self
            .apply_replacements(&workspace_ast, replacements)
            .await?;
        Some(EditReceipt {
            revision,
            spans: vec![span],
        })
    }

    async fn call_arguments(
        self,
        _: tarpc::context::Context,
        base_revision: u64,
        call: Span,
    ) -> Option<CallArguments> {
        let compiled = self.published_state.lock().await;
        if compiled.compiled_revision != base_revision {
            return None;
        }
        let ast = schematic_edit::module_at(&compiled.ast, &call.path)?;
        let call = ast.span2call.get(&call)?;
        Some(schematic_edit::call_arguments(&ast.source_text, call))
    }

    async fn add_symbol_items(
        self,
        _: tarpc::context::Context,
        request: AddSymbolItemsRequest,
    ) -> Option<EditReceipt> {
        let (workspace_ast, snapshot) = self
            .schematic_context(
                request.target.base_revision,
                std::iter::once(&request.target.cell),
            )
            .await?;
        let data = snapshot.data()?;
        let statements = (|| {
            let (_, cell) = cell_body(&workspace_ast, data, &request.target.cell)?;
            let compiled = compiled_cell(data, &request.target.cell)?;
            request
                .items
                .iter()
                .map(|item| schematic_edit::symbol_statement(item, cell, &compiled.schematic))
                .collect::<Result<Vec<_>, _>>()
        })();
        self.insert_statements(&workspace_ast, data, &request.target, statements, &[])
            .await
    }

    async fn create_symbol(
        self,
        _: tarpc::context::Context,
        request: CreateSymbolRequest,
    ) -> Option<EditReceipt> {
        let (workspace_ast, snapshot) = self
            .schematic_context(
                request.target.base_revision,
                std::iter::once(&request.target.cell),
            )
            .await?;
        let data = snapshot.data()?;
        let statements = (|| {
            cell_body(&workspace_ast, data, &request.target.cell)?;
            let compiled = compiled_cell(data, &request.target.cell)?;
            schematic_edit::create_symbol_statements(compiled)
        })();
        self.insert_statements(&workspace_ast, data, &request.target, statements, &[])
            .await
    }

    async fn rename(
        self,
        _: tarpc::context::Context,
        request: RenameRequest,
    ) -> Option<EditReceipt> {
        let (workspace_ast, snapshot) = self
            .schematic_context(request.base_revision, std::iter::once(&request.site))
            .await?;
        let data = snapshot.data()?;
        let index = self.nav_index().await;
        let replacements = match schematic_edit::rename_replacements(
            &workspace_ast,
            data,
            index.as_deref(),
            &request.site,
            &request.name,
        ) {
            Ok(replacements) => replacements,
            Err(message) => {
                self.report_message(MessageType::ERROR, message).await;
                return None;
            }
        };
        let span = Span {
            path: request.site.path.clone(),
            span: schematic_edit::span_after(
                request.site.span,
                replacements
                    .get(&*request.site.path)
                    .map_or(&[][..], Vec::as_slice),
            ),
        };
        let revision = self
            .apply_replacements(&workspace_ast, replacements)
            .await?;
        Some(EditReceipt {
            revision,
            spans: vec![span],
        })
    }

    async fn copy_statements(
        self,
        _: tarpc::context::Context,
        base_revision: u64,
        targets: Vec<Span>,
    ) -> Option<Vec<ClipboardItem>> {
        let (workspace_ast, snapshot) = self
            .schematic_context(base_revision, targets.iter())
            .await?;
        let data = snapshot.data()?;
        match schematic_edit::copy_statements(&workspace_ast, data, &targets) {
            Ok(items) => Some(items),
            Err(message) => {
                self.report_message(MessageType::ERROR, message).await;
                None
            }
        }
    }

    async fn paste(self, _: tarpc::context::Context, request: PasteRequest) -> Option<EditReceipt> {
        let (workspace_ast, snapshot) = self
            .schematic_context(
                request.target.base_revision,
                std::iter::once(&request.target.cell),
            )
            .await?;
        let data = snapshot.data()?;
        let statements = (|| {
            let (_, cell) = cell_body(&workspace_ast, data, &request.target.cell)?;
            schematic_edit::paste_statements(
                &request.items,
                request.offset,
                request.symbol_offset,
                &schematic_edit::cell_names(cell),
            )
        })();
        self.insert_statements(&workspace_ast, data, &request.target, statements, &[])
            .await
    }

    async fn cell_parameters(
        self,
        _: tarpc::context::Context,
        base_revision: u64,
        cell: Span,
    ) -> Option<Vec<String>> {
        let compiled = self.published_state.lock().await;
        if compiled.compiled_revision != base_revision {
            return None;
        }
        let ast = schematic_edit::module_at(&compiled.ast, &cell.path)?;
        let decl = schematic_edit::cell_with_body(ast, cell.span)?;
        Some(
            decl.args
                .iter()
                .map(|arg| arg.name.name.to_string())
                .collect(),
        )
    }

    async fn add_eq_constraint(
        self,
        _: tarpc::context::Context,
        scope_span: Span,
        lhs: String,
        rhs: String,
    ) {
        let Some(workspace_ast) = self.current_editor_ast().await else {
            self.report_message(MessageType::ERROR, OUT_OF_SYNC_MESSAGE)
                .await;
            return;
        };
        if let Some(error) = source_edit_error(&workspace_ast, &scope_span) {
            self.report_message(MessageType::ERROR, error).await;
            return;
        }
        let Some(url) = Uri::from_file_path(&scope_span.path) else {
            return;
        };
        let Some(ast) = workspace_ast
            .values()
            .find(|ast| ast.path == scope_span.path)
        else {
            return;
        };
        let Some(scope) = ast.span2scope.get(&scope_span) else {
            return;
        };
        let document = self.document(&ast.source_text);
        let insertion = insert_statement(
            &document,
            scope.span,
            scope.tail.as_ref().map(|tail| tail.span().start()),
            &format!("eq({lhs}, {rhs});"),
            0..0,
        );
        self.apply_source_edit(url, insertion.edit).await;
    }

    async fn open_cell(self, _: tarpc::context::Context, cell: String) {
        self.editor_client
            .show_message(MessageType::LOG, &format!("cell {}", cell))
            .await;
        tokio::spawn(async move {
            Backend { state: self }.select_and_open_cell(cell).await;
        });
    }

    async fn show_message(self, _: tarpc::context::Context, typ: MessageType, message: String) {
        self.editor_client.show_message(typ, message).await;
    }

    async fn dispatch_action(self, _: tarpc::context::Context, action: LangServerAction) {
        let result = match action {
            LangServerAction::Save => self.editor_client.send_request::<Save>(()).await,
            LangServerAction::Undo => {
                let params = self.history_params();
                self.editor_client.send_request::<Undo>(params).await
            }
            LangServerAction::Redo => {
                let params = self.history_params();
                self.editor_client.send_request::<Redo>(params).await
            }
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

    async fn focus_editor(self, _: tarpc::context::Context, params: FocusEditorParams) {
        self.editor_client
            .send_notification::<crate::FocusEditor>(params)
            .await;
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn rapid_rectangles_get_unique_source_names_even_with_a_stale_layout() {
        let mut names = std::collections::HashSet::<String>::new();
        for _ in 0..100 {
            // The GUI may keep requesting the same name while a large top
            // cell is still evaluating. The source is authoritative.
            let used = names.iter().map(String::as_str).collect();
            let name = super::available_rect_name("rect61".to_owned(), &used);
            assert!(names.insert(name));
        }
        assert!(names.contains("rect61"));
        assert_eq!(names.len(), 100);
    }

    #[test]
    fn queued_rectangle_relocates_scope_without_top_level_compilation() {
        let path = std::path::PathBuf::from("lib.ar");
        let original = "cell top() { let first = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.); }";
        let parsed = argonc::parse::parse_source_text(original, path.clone()).unwrap();
        let scope = parsed.span2scope.keys().next().unwrap().clone();
        let edited = original.replace(
            " }",
            " let second = rect(\"met1\", x0=2., y0=2., x1=3., y1=3.); }",
        );
        let current = argonc::parse::parse_source_text(edited, path.clone()).unwrap();
        let relocated = super::scope_after_insertion(&current, &scope, original).unwrap();
        assert_eq!(relocated.span.start(), scope.span.start());
        assert!(relocated.span.end() > scope.span.end());

        let shifted = format!("// shifted\n{original}");
        let shifted = argonc::parse::parse_source_text(shifted, path.clone()).unwrap();
        let relocated = super::scope_after_insertion(&shifted, &scope, original).unwrap();
        assert_eq!(
            relocated.span.start(),
            scope.span.start() + "// shifted\n".len()
        );

        let ambiguous = format!("// shifted\n{original}\n// changed tail");
        let ambiguous = argonc::parse::parse_source_text(ambiguous, path).unwrap();
        assert!(super::scope_after_insertion(&ambiguous, &scope, original).is_none());
    }

    #[test]
    fn compilation_delta_preserves_identity_order_and_removes_old_cells() {
        use super::{CompilationSnapshot, CompilationUpdate, CompressedCompilationUpdate};
        use std::sync::Arc;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("lib.ar");
        std::fs::write(
            &source,
            r#"
use std::layout::{inst, rect};
cell leaf() { let r = rect("met1", x0=0., y0=0., x1=10., y1=5.); }
cell top() { let a = inst(leaf(), x=0., y=0.); }
"#,
        )
        .unwrap();
        let config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../examples/tech/basic.tech.toml"),
        ));
        let mut compiler = argonc::incremental::IncrementalCompiler::new();
        let base = CompilationSnapshot {
            revision: 1,
            output: compiler.compile_cell(&config, &["top".into()], vec![]),
        };
        let roundtrip = |update: CompilationUpdate| -> CompilationUpdate {
            let raw_len = postcard::to_stdvec(&update).unwrap().len();
            let compressed = CompressedCompilationUpdate::encode(&update).unwrap();
            assert!(compressed.encoded_len() > 0);
            assert!(compressed.encoded_len() < raw_len);
            compressed.decode().unwrap()
        };
        let received_base = roundtrip(CompilationUpdate::new(base.clone(), None))
            .materialize(None)
            .unwrap();
        let mut edited = base.clone();
        edited.revision = 2;
        let data = edited.data_mut().unwrap();
        // The same ID with new data must be sent; an ID is never a cache key
        // without immutable identity. This also exercises removal of a cell.
        let top = data.top;
        let top_cell = Arc::make_mut(data.cells.get_mut(&top).unwrap());
        top_cell.name = "updated".into();
        top_cell.objects.clear();
        for scope in top_cell.scopes.values_mut() {
            scope.emit.clear();
        }
        let update = CompilationUpdate::new(edited.clone(), Some(&base));
        assert_eq!(update.snapshot.data().unwrap().cells.len(), 1);
        let received = roundtrip(update.clone())
            .materialize(Some(&received_base))
            .unwrap();
        assert_eq!(
            received.data().unwrap().cells.keys().collect::<Vec<_>>(),
            edited.data().unwrap().cells.keys().collect::<Vec<_>>()
        );
        for (id, cell) in &received.data().unwrap().cells {
            if *id != top {
                assert!(Arc::ptr_eq(cell, &received_base.data().unwrap().cells[id]));
            }
        }
        assert_eq!(received.data().unwrap().cells[&top].name, "updated");
        assert!(roundtrip(update.clone()).materialize(None).is_none());
        assert!(roundtrip(update).materialize(Some(&received)).is_none());
        edited.revision = 3;
        edited.data_mut().unwrap().cells.retain(|id, _| *id == top);
        let received = roundtrip(CompilationUpdate::new(edited.clone(), Some(&base)))
            .materialize(Some(&received_base))
            .unwrap();
        assert_eq!(received.data().unwrap().cells.len(), 1);
        assert_eq!(
            received.data().unwrap().geometry_digest(),
            edited.data().unwrap().geometry_digest()
        );
    }

    #[test]
    fn drag_history_places_spans_only_on_the_text_its_edits_produced() {
        use super::{
            DragEditHistory, Span, ValueEdit, apply_value_edits, remap_span_after_value_edits,
        };
        let path = std::path::PathBuf::from("lib.ar");
        let text = "rect(x0i = 0., y0i = 0.)".to_owned();
        let span = |text: &str, kwarg: &str| {
            let start = text.find(kwarg).unwrap() + kwarg.len();
            let len = text[start..].find([',', ')']).unwrap();
            Span {
                path: path.clone().into(),
                span: cfgrammar::Span::new(start, start + len),
            }
        };
        let edit = |text: &str, kwarg: &str, value: &str| ValueEdit {
            span: span(text, kwarg),
            value: value.to_owned(),
        };
        let mut history = DragEditHistory::default();
        let texts = std::collections::HashMap::from([(path.clone(), text.clone())]);
        history.record(1, true, &texts, vec![edit(&text, "x0i = ", "12345.")]);
        let edited = "rect(x0i = 12345., y0i = 0.)";
        assert_eq!(apply_value_edits(&text, &path, &history.batches[0]), edited);

        let batches = history
            .batches_since(1, |_| Some(edited.to_owned()))
            .unwrap();
        let y0 = batches.iter().fold(span(&text, "y0i = "), |span, batch| {
            remap_span_after_value_edits(&span, batch)
        });
        assert_eq!(y0, span(edited, "y0i = "));
        // Text anyone else changed, or a different base, cannot be mapped.
        assert!(
            history
                .batches_since(1, |_| Some(format!("// typed\n{edited}")))
                .is_none()
        );
        assert!(
            history
                .batches_since(2, |_| Some(edited.to_owned()))
                .is_none()
        );
    }

    #[test]
    fn a_focused_preview_is_a_delta_against_the_full_snapshot() {
        use super::{CompilationSnapshot, CompilationUpdate, CompressedCompilationUpdate};
        use std::sync::Arc;
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("lib.ar");
        let original = r#"
use std::layout::{inst, rect};
cell leaf() { let r = rect("met1", x0=0., y0=0., x1=10., y1=5.); }
cell mid() { let a = inst(leaf(), x=0., y=0.); }
cell top() { let m = inst(mid(), x=0., y=0.); }
"#;
        std::fs::write(&source, original).unwrap();
        let config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../examples/tech/basic.tech.toml"),
        ));
        let mut compiler = argonc::incremental::IncrementalCompiler::new();
        let base = CompilationSnapshot {
            revision: 1,
            output: compiler.compile_cell(&config, &["top".into()], vec![]),
        };
        let named = |snapshot: &CompilationSnapshot, name: &str| {
            snapshot
                .data()
                .unwrap()
                .cells
                .iter()
                .find(|(_, cell)| cell.name.ends_with(name))
                .map(|(id, cell)| (*id, cell.clone()))
                .unwrap()
        };
        compiler.set_source_text(
            source,
            original.replace("inst(leaf(), x=0.", "inst(leaf(), x=5."),
        );
        let preview = CompilationSnapshot {
            revision: 2,
            output: compiler
                .compile_cached_cell_preview(&config, named(&base, "mid").0)
                .unwrap(),
        };
        let update = CompilationUpdate::new(preview.clone(), Some(&base));
        // The unchanged leaf is named by ID rather than sent again.
        assert_eq!(update.snapshot.data().unwrap().cells.len(), 1);
        let encoded = || CompressedCompilationUpdate::encode(&update).unwrap();
        let received = encoded()
            .decode()
            .unwrap()
            .materialize(Some(&base))
            .unwrap();
        assert_eq!(
            received.data().unwrap().geometry_digest(),
            preview.data().unwrap().geometry_digest()
        );
        assert!(Arc::ptr_eq(
            &named(&received, "leaf").1,
            &named(&base, "leaf").1
        ));
        assert!(
            encoded()
                .decode()
                .unwrap()
                .materialize(Some(&preview))
                .is_none()
        );
    }

    use argonc::{WorkspaceConfig, parse};
    use tower_lsp_server::ls_types::{Position, Uri};

    use super::{
        Document, DrawSegmentConstraint, PathBuf, PathParams, PolygonParams,
        READ_ONLY_GENERATED_SOURCE_MESSAGE, editor_buffers_are_current, insert_statement,
        instance_placement_expression, let_prefix, missing_initial_condition_edit, path_expression,
        polygon_expression, segment_constraint_statements, source_edit_error,
    };
    use crate::{PublishedState, SourceState, document::PositionEncoding};

    #[test]
    fn generated_gds_declarations_do_not_make_editor_buffers_stale() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        let source = "cell top() {}\n";
        std::fs::write(&source_path, source).unwrap();
        let config = WorkspaceConfig::new(&source_path)
            .with_gds_imports([("ring_osc".to_owned(), directory.path().join("ring_osc.gds"))]);
        let ast = parse::parse_workspace_with_config(&config).ast();
        let uri = Uri::from_file_path(&source_path).unwrap();
        let compiled = PublishedState {
            ast: std::sync::Arc::new(ast),
            ..Default::default()
        };
        let mut source_state = SourceState::default();
        source_state
            .editor_files
            .insert(uri, Document::new(source, 1, PositionEncoding::Utf8));

        assert!(editor_buffers_are_current(&source_state, &compiled));

        let source_scope = compiled
            .ast
            .values()
            .find(|ast| *ast.path == *source_path)
            .and_then(|ast| ast.span2scope.keys().next())
            .cloned()
            .expect("source cell should have a scope");
        assert_eq!(source_edit_error(&compiled.ast, &source_scope), None);

        let imported_scope = compiled
            .ast
            .values()
            .find(|ast| *ast.path == *source_path)
            .and_then(|ast| {
                ast.span2scope
                    .keys()
                    .find(|span| span.span.end() > ast.source_text.len())
            })
            .cloned()
            .expect("GDS declaration should have a generated scope");
        assert_eq!(
            source_edit_error(&compiled.ast, &imported_scope),
            Some(READ_ONLY_GENERATED_SOURCE_MESSAGE)
        );
    }

    #[test]
    fn accepted_workspace_edits_are_not_reported_as_out_of_sync() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        std::fs::write(&source_path, "cell top() {}\n").unwrap();
        let ast = parse::parse_workspace_with_std(&source_path).ast();
        let uri = Uri::from_file_path(&source_path).unwrap();
        let compiled = PublishedState {
            ast: std::sync::Arc::new(ast),
            ..Default::default()
        };
        let mut source_state = SourceState::default();
        source_state.editor_files.insert(
            uri.clone(),
            Document::new("cell top() { rect(); }\n", 2, PositionEncoding::Utf8),
        );
        assert!(!editor_buffers_are_current(&source_state, &compiled));

        source_state
            .pending_workspace_edits
            .insert(uri, "cell top() { rect(); }\n".to_owned());
        assert!(editor_buffers_are_current(&source_state, &compiled));
    }

    /// Inserts `statement` into the body of `top`, in a module with no `use`
    /// declarations, the way the drawing requests do, and asserts that the
    /// result type-checks.
    fn assert_compiles_without_imports(statement: &str) {
        let path = PathBuf::from("/virtual/lib.ar");
        let source = "cell child(w: Float) {}\ncell top() {\n}\n";
        let ast = parse::parse_source_text(source, path.clone()).unwrap();
        let body = source.rfind('{').unwrap();
        let scope = ast
            .span2scope
            .values()
            .find(|scope| scope.span.start() == body)
            .expect("the body of top should be indexed");
        let insertion = insert_statement(
            &Document::new(source, 0, PositionEncoding::Utf8),
            scope.span,
            None,
            statement,
            0..0,
        );
        let mut edited = source.to_owned();
        edited.insert_str(insertion.offset, &insertion.edit.new_text);
        let root = parse::parse_source_text(edited.clone(), path).unwrap();
        let (_, errors) = argonc::compile::static_compile(&parse::with_std(root)).unwrap();
        assert!(errors.errors.is_empty(), "{edited}\n{:?}", errors.errors);
    }

    #[test]
    fn place_rewrites_symbol_coordinates_and_the_points_of_a_line() {
        use super::{KeywordEdit, PlacementEdit, placement_replacements};
        use crate::schematic_edit::testing::compile;
        let compiled = compile(
            "use std::schematic::Signal;
             use std::symbol::{line, pin, rect, text};
             cell inv() {
                 pub let a = Signal(x=-6, y=0);
                 line([(-4., 0.), (-2., 0.)]);
                 rect(x0=-2., y0=-2., x1=2., y1=2.);
                 text(\"inv\", x=0., y=3.);
                 pin(a, x=-4, y=0);
             }
             cell top() {
                 let u = std::schematic::inst(inv(), x=0, y=0);
             }",
        );
        let keyword = |name: &str, value: &str| KeywordEdit {
            name: name.to_owned(),
            value: Some(value.to_owned()),
        };
        let edits = vec![
            PlacementEdit {
                call: compiled.site("line("),
                keywords: vec![keyword("points", "[(-4., 0.5), (-2., 0.5)]")],
            },
            PlacementEdit {
                call: compiled.site("rect("),
                keywords: vec![keyword("x0", "-3."), keyword("fill", "true")],
            },
            PlacementEdit {
                call: compiled.site("text("),
                keywords: vec![
                    keyword("x", "1."),
                    keyword("align", "std::symbol::Align::Right"),
                ],
            },
            PlacementEdit {
                call: compiled.site("pin("),
                keywords: vec![keyword("x", "-5"), keyword("label", "true")],
            },
        ];
        let (replacements, calls) =
            placement_replacements(&compiled.workspace, &compiled.data, &edits).unwrap();
        assert_eq!(calls.len(), 4);
        let mut file = replacements[&compiled.path].clone();
        file.sort_by_key(|replacement| std::cmp::Reverse(replacement.0.start));
        let edited = crate::schematic_edit::apply_replacements(compiled.source(), &file);
        assert!(
            edited.contains("line([(-4., 0.5), (-2., 0.5)]);"),
            "{edited}"
        );
        assert!(
            edited.contains("rect(x0=-3., y0=-2., x1=2., y1=2., fill=true);"),
            "{edited}"
        );
        assert!(
            edited.contains("text(\"inv\", x=1., y=3., align=std::symbol::Align::Right);"),
            "{edited}"
        );
        assert!(
            edited.contains("pin(a, x=-5, y=0, label=true);"),
            "{edited}"
        );
        // A keyword of another kind is refused, and so is dropping the points.
        let wrong = vec![PlacementEdit {
            call: compiled.site("line("),
            keywords: vec![keyword("x0", "1.")],
        }];
        assert!(placement_replacements(&compiled.workspace, &compiled.data, &wrong).is_err());
        let dropped = vec![PlacementEdit {
            call: compiled.site("line("),
            keywords: vec![KeywordEdit {
                name: "points".into(),
                value: None,
            }],
        }];
        assert!(placement_replacements(&compiled.workspace, &compiled.data, &dropped).is_err());
    }

    #[test]
    fn placed_instances_use_initial_conditions() {
        let expression = instance_placement_expression("child(10.)", 12.5, -4.0);
        assert_eq!(
            expression,
            "std::layout::inst(child(10.), xi=12.5, yi=-4.0)"
        );
        assert_compiles_without_imports(&format!("pub let inst0 = {expression};"));
    }

    #[test]
    fn drawn_polygons_use_numbered_initial_conditions() {
        let expression = polygon_expression(&PolygonParams {
            layer: "met1".to_owned(),
            points: vec![(0., 1.), (20.5, 1.), (10., 30.)],
            constraints: vec![],
        });
        assert_eq!(
            expression,
            "std::layout::polygon(\"met1\", 3,\n        x0i = 0.0, y0i = 1.0,\n        x1i = 20.5, y1i = 1.0,\n        x2i = 10.0, y2i = 30.0,\n    )"
        );
        let constraints =
            segment_constraint_statements("polygon0", &[DrawSegmentConstraint::Horizontal(1)]);
        assert_compiles_without_imports(&format!("pub let polygon0 = {expression}!;{constraints}"));
    }

    #[test]
    fn drawn_paths_use_numbered_initial_conditions() {
        let expression = path_expression(&PathParams {
            layer: "met1".to_owned(),
            width: 20.,
            points: vec![(0., 1.), (20.5, 1.), (10., 30.)],
            constraints: vec![],
        });
        assert_eq!(
            expression,
            "std::layout::path(\"met1\", 3,\n        widthi = 20.0,\n        x0i = 0.0, y0i = 1.0,\n        x1i = 20.5, y1i = 1.0,\n        x2i = 10.0, y2i = 30.0,\n    )"
        );
        let constraints =
            segment_constraint_statements("path0", &[DrawSegmentConstraint::Vertical(2)]);
        assert_compiles_without_imports(&format!("pub let path0 = {expression}!;{constraints}"));
    }

    #[test]
    fn drawn_segment_constraints_reference_adjacent_fields() {
        assert_eq!(
            segment_constraint_statements(
                "path0",
                &[
                    DrawSegmentConstraint::Horizontal(1),
                    DrawSegmentConstraint::Vertical(2),
                    DrawSegmentConstraint::DiagonalPositive(3),
                    DrawSegmentConstraint::DiagonalNegative(4),
                ],
            ),
            "\n    eq(path0.y1, path0.y0);\n    eq(path0.x2, path0.x1);\n    eq(path0.x3 - path0.x2, path0.y3 - path0.y2);\n    eq(path0.x4 - path0.x3, path0.y3 - path0.y4);"
        );
    }

    #[test]
    fn inserts_missing_initial_conditions_into_geometry_calls() {
        fn insert(source: &str, call: &str, values: &[(&str, &str)]) -> String {
            let start = source.find(call).unwrap();
            let call_span = argonc::ast::Span {
                path: std::path::Path::new("/virtual/lib.ar").into(),
                span: cfgrammar::Span::new(start, start + call.len()),
            };
            let edit = missing_initial_condition_edit(
                source,
                &call_span,
                values
                    .iter()
                    .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                    .collect(),
            )
            .unwrap();
            let mut updated = source.to_owned();
            updated.replace_range(edit.span.span.start()..edit.span.span.end(), &edit.value);
            parse::parse_source_text(updated.clone(), call_span.path.to_path_buf()).unwrap();
            updated
        }

        assert_eq!(
            insert(
                "cell top() { let p = polygon(\"met1\", 3); }",
                "polygon(\"met1\", 3)",
                &[("x2i", "12.5"), ("y2i", "7.")],
            ),
            "cell top() { let p = polygon(\"met1\", 3, x2i=12.5, y2i=7.); }"
        );
        assert_eq!(
            insert(
                "cell top() { let r = crect(); }",
                "crect()",
                &[("x0i", "4."), ("y0i", "5.")],
            ),
            "cell top() { let r = crect(x0i=4., y0i=5.); }"
        );
        assert_eq!(
            insert(
                "cell top() { let i = inst(child()); }",
                "inst(child())",
                &[("xi", "40."), ("yi", "10.")],
            ),
            "cell top() { let i = inst(child(), xi=40., yi=10.); }"
        );
        assert_eq!(
            insert(
                "cell top() {\n    let r = rect(\"met1\",\n        x0=0.,\n    );\n}",
                "rect(\"met1\",\n        x0=0.,\n    )",
                &[("x1i", "20."), ("y1i", "30.")],
            ),
            "cell top() {\n    let r = rect(\"met1\",\n        x0=0.,\n        x1i=20., y1i=30.,\n    );\n}"
        );
    }

    #[test]
    fn only_a_binding_at_the_top_level_of_a_cell_is_public() {
        let source = "cell top() {\n    for i in std::range(2) {\n    }\n}\nfn f() {\n}\n";
        let ast = parse::parse_source_text(source, PathBuf::from("/virtual/lib.ar"))
            .expect("source should parse");
        let prefix_at = |brace: usize| {
            let scope = ast
                .span2scope
                .values()
                .find(|scope| scope.span.start() == brace)
                .expect("scope should be indexed");
            let_prefix(&ast, scope.span, "r")
        };

        assert_eq!(prefix_at(source.find('{').unwrap()), "pub let r = ");
        assert_eq!(prefix_at(source.find("2) {").unwrap() + 3), "let r = ");
        assert_eq!(prefix_at(source.find("f() {").unwrap() + 4), "let r = ");
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
            &Document::new(source, 0, PositionEncoding::Utf8),
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
            &Document::new(source, 0, PositionEncoding::Utf8),
            cfgrammar::Span::new(scope_start, closing_brace + 1),
            None,
            "eq(x, y);",
            0..0,
        );

        assert_eq!(insertion.edit.range.start, Position::new(1, 0));
        assert_eq!(insertion.edit.new_text, "    eq(x, y);\n");
    }
}
