//! The service agents use to read and edit the workspace.
//!
//! Agent edits are applied to Neovim's buffers rather than to disk, so the
//! user sees each one as it lands, can undo it, and decides when to save.

use std::{
    path::{Component, Path, PathBuf},
    sync::atomic::Ordering,
    time::Duration,
};

use argonc::{
    ast::Span,
    compile::{CompileOutput, SolvedValue},
};
use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use tarpc::context;
use tokio::time::Instant;
use tower_lsp_server::{
    NotCancellable, OngoingProgress, Unbounded,
    ls_types::{Diagnostic, ProgressToken, Range, TextEdit, Uri, request::Request},
};

use crate::{
    ApprovalMode, Backend, SourceState,
    compiler_worker::{CompileIdentity, CompileRequest},
    document::{Document, PositionEncoding},
};

/// How long the bridge lets one call run, which bounds an approval prompt.
pub const CALL_DEADLINE: Duration = Duration::from_secs(600);

/// How long Neovim may take to report a change the analyzer asked for.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long an edit waits for the compilation that includes it.
const COMPILE_TIMEOUT: Duration = Duration::from_secs(120);

/// Attempts at an edit whose buffer changed between planning and applying.
const MAX_EDIT_ATTEMPTS: usize = 3;

pub const FOLLOW_REQUIRED: &str = "The GUI's cell can only be switched while the user has follow mode on (`:Argon follow` in Neovim). Use compile_cell to check a cell without changing the GUI.";

/// One find-and-replace within a file.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Replacement {
    pub old: String,
    pub new: String,
    /// Replace every occurrence instead of requiring exactly one.
    pub replace_all: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EditRequest {
    pub path: PathBuf,
    /// Matched against the file as it was before any of them applied.
    pub edits: Vec<Replacement>,
    /// Shown to the user while the edit is applied.
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateRequest {
    pub path: PathBuf,
    pub contents: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileContents {
    pub path: PathBuf,
    pub contents: String,
    /// Whether the text came from a Neovim buffer, which may be unsaved.
    pub from_editor: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentDiagnostic {
    pub path: PathBuf,
    /// One-based.
    pub line: u32,
    /// One-based, in the editor's position encoding.
    pub column: u32,
    pub message: String,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum CompileStatus {
    Valid,
    ExecErrors,
    StaticErrors,
    ParseErrors,
}

/// The shape of a compiled cell, so an agent can tell whether an edit did
/// what it meant without seeing the layout.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CompileSummary {
    pub cell: Option<String>,
    pub status: CompileStatus,
    /// Distinct cells in the compiled hierarchy.
    pub cells: usize,
    pub rects: usize,
    pub polygons: usize,
    pub paths: usize,
    pub instances: usize,
    /// Coordinates the solver left to initial conditions in the top cell.
    pub unsolved_vars: usize,
    /// `[x0, y0, x1, y1]` of the top cell's layout geometry.
    pub bbox: Option<[f64; 4]>,
}

impl CompileSummary {
    pub(crate) fn new(cell: Option<&str>, output: &CompileOutput) -> Self {
        let (status, data) = match output {
            CompileOutput::Valid(data) => (CompileStatus::Valid, Some(data)),
            CompileOutput::ExecErrors(errors) => {
                (CompileStatus::ExecErrors, errors.output.as_ref())
            }
            CompileOutput::StaticErrors(_) => (CompileStatus::StaticErrors, None),
            CompileOutput::FatalParseErrors => (CompileStatus::ParseErrors, None),
        };
        let mut summary = Self {
            cell: cell.map(str::to_owned),
            status,
            cells: 0,
            rects: 0,
            polygons: 0,
            paths: 0,
            instances: 0,
            unsolved_vars: 0,
            bbox: None,
        };
        let Some(data) = data else {
            return summary;
        };
        summary.cells = data.cells.len();
        if let Some(top) = data.cells.get(&data.top) {
            for object in top.objects.values() {
                match object {
                    SolvedValue::Rect(_) => summary.rects += 1,
                    SolvedValue::Polygon(_) => summary.polygons += 1,
                    SolvedValue::Path(_) => summary.paths += 1,
                    SolvedValue::Instance(_) => summary.instances += 1,
                    _ => {}
                }
            }
            summary.unsolved_vars = top.unsolved_vars.len();
        }
        summary.bbox = data
            .layout_bbox(data.top)
            .map(|bbox| [bbox.x0, bbox.y0, bbox.x1, bbox.y1]);
        summary
    }
}

/// The latest committed compilation and the cell it was for.
#[derive(Debug, Clone)]
pub(crate) struct LatestCompile {
    pub(crate) cell: Option<String>,
    pub(crate) output: CompileOutput,
}

/// What the workspace looks like after an agent call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    /// The source revision the call produced or observed.
    pub revision: u64,
    /// The revision `summary` and `diagnostics` describe. Older than
    /// `revision` when compilation did not finish in time.
    pub compiled_revision: u64,
    pub summary: Option<CompileSummary>,
    pub diagnostics: Vec<AgentDiagnostic>,
    pub messages: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatus {
    pub root: Option<PathBuf>,
    pub open_cell: Option<String>,
    pub gui_connected: bool,
    pub follow_mode: bool,
    pub approval: ApprovalMode,
    /// Whether any Neovim buffer in the workspace has unsaved changes.
    pub workspace_modified: bool,
    /// Whether files changed on disk outside Neovim are picked up.
    pub watching_files: bool,
    pub open_files: Vec<PathBuf>,
    pub revision: u64,
    pub compiled_revision: u64,
}

#[tarpc::service]
pub trait Agent {
    async fn status() -> AgentStatus;
    /// The file as the user currently has it, unsaved edits included.
    async fn read_file(path: PathBuf) -> Result<FileContents, String>;
    async fn edit_file(request: EditRequest) -> Result<Report, String>;
    /// Creates a new source file as an unsaved Neovim buffer.
    async fn create_file(request: CreateRequest) -> Result<Report, String>;
    async fn diagnostics() -> Report;
    /// Compiles a cell without changing what the GUI shows.
    async fn compile_cell(cell: String) -> Result<Report, String>;
    /// Shows a cell in the GUI. Allowed only in follow mode.
    async fn open_cell(cell: String) -> Result<Report, String>;
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct EnsureBufferParams {
    uri: Uri,
    create: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct EnsureBufferResult {
    ok: bool,
    #[serde(default)]
    message: Option<String>,
}

/// Asks Neovim to load a file into a buffer attached to the analyzer.
enum EnsureBuffer {}

impl Request for EnsureBuffer {
    type Params = EnsureBufferParams;
    type Result = EnsureBufferResult;
    const METHOD: &'static str = "custom/ensureBuffer";
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub(crate) struct ApplyAgentEditParams {
    uri: Uri,
    /// The buffer version the edits were planned against.
    version: i32,
    edits: Vec<TextEdit>,
    /// Where the replacement text lands once applied.
    highlights: Vec<Range>,
    label: String,
    confirm: bool,
    /// The whole edited file, sent only when confirming, for the preview.
    new_text: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub(crate) enum ApplyAgentEditResult {
    Applied { version: i32 },
    Stale { version: i32 },
    Rejected,
    Failed { message: String },
}

/// Asks Neovim to apply an agent edit if its buffer is still at the planned
/// version, confirming with the user first when `confirm` is set.
enum ApplyAgentEdit {}

impl Request for ApplyAgentEdit {
    type Params = ApplyAgentEditParams;
    type Result = ApplyAgentEditResult;
    const METHOD: &'static str = "custom/applyAgentEdit";
}

#[derive(Debug, Deserialize)]
pub(crate) struct FollowModeParams {
    enabled: bool,
}

/// A resolved edit, ready to send to Neovim.
#[derive(Debug, Clone)]
pub(crate) struct EditPlan {
    version: i32,
    edits: Vec<TextEdit>,
    highlights: Vec<Range>,
    /// Byte spans of the replacement text in the edited file.
    spans: Vec<cfgrammar::Span>,
    new_text: String,
}

/// Every start offset of `pattern` in `text`, overlapping ones included.
fn occurrences(text: &str, pattern: &str) -> Vec<usize> {
    let mut starts = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find(pattern) {
        let start = from + found;
        starts.push(start);
        from = start + text[start..].chars().next().map_or(1, char::len_utf8);
    }
    starts
}

/// Resolves find-and-replace edits against `document`.
pub(crate) fn plan_edit(
    document: &Document,
    replacements: &[Replacement],
    encoding: PositionEncoding,
) -> Result<EditPlan, String> {
    if replacements.is_empty() {
        return Err("No edits were given.".to_owned());
    }
    let text = document.contents();
    let mut matches = Vec::new();
    for (index, replacement) in replacements.iter().enumerate() {
        let number = index + 1;
        if replacement.old.is_empty() {
            return Err(format!(
                "Edit {number}: old_string is empty. Use create_file to write a new file."
            ));
        }
        if replacement.old == replacement.new {
            return Err(format!(
                "Edit {number}: old_string and new_string are identical."
            ));
        }
        let starts = occurrences(text, &replacement.old);
        match starts.len() {
            0 => {
                return Err(format!(
                    "Edit {number}: old_string was not found. Read the file again; it may have changed."
                ));
            }
            count if count > 1 && !replacement.replace_all => {
                return Err(format!(
                    "Edit {number}: old_string occurs {count} times. Include more surrounding text to make it unique, or set replace_all."
                ));
            }
            _ => {}
        }
        for start in starts {
            matches.push((
                start,
                start + replacement.old.len(),
                number,
                &replacement.new,
            ));
        }
    }
    matches.sort_by_key(|&(start, ..)| start);
    for pair in matches.windows(2) {
        let (_, end, first, _) = pair[0];
        let (start, _, second, _) = pair[1];
        if end > start {
            return Err(if first == second {
                format!("Edit {first}: old_string matches overlapping text.")
            } else {
                format!("Edits {first} and {second} overlap.")
            });
        }
    }

    let mut new_text = String::with_capacity(text.len());
    let mut spans = Vec::with_capacity(matches.len());
    let mut copied = 0;
    for &(start, end, _, new) in &matches {
        new_text.push_str(&text[copied..start]);
        let span_start = new_text.len();
        new_text.push_str(new);
        spans.push(cfgrammar::Span::new(span_start, new_text.len()));
        copied = end;
    }
    new_text.push_str(&text[copied..]);

    let edited = Document::new(new_text.clone(), 0, encoding);
    Ok(EditPlan {
        version: document.version(),
        edits: matches
            .iter()
            .map(|&(start, end, _, new)| TextEdit {
                range: Range::new(document.offset_to_pos(start), document.offset_to_pos(end)),
                new_text: new.clone(),
            })
            .collect(),
        highlights: spans
            .iter()
            .map(|span| {
                Range::new(
                    edited.offset_to_pos(span.start()),
                    edited.offset_to_pos(span.end()),
                )
            })
            .collect(),
        spans,
        new_text,
    })
}

/// Fills a blank document, such as a new buffer, with `contents`.
pub(crate) fn plan_insert(
    document: &Document,
    contents: &str,
    encoding: PositionEncoding,
) -> Result<EditPlan, String> {
    let existing = document.contents();
    if !existing.trim().is_empty() {
        return Err("The file already has contents; use edit_file instead.".to_owned());
    }
    let edited = Document::new(contents.to_owned(), 0, encoding);
    Ok(EditPlan {
        version: document.version(),
        edits: vec![TextEdit {
            range: Range::new(
                document.offset_to_pos(0),
                document.offset_to_pos(existing.len()),
            ),
            new_text: contents.to_owned(),
        }],
        highlights: vec![Range::new(
            edited.offset_to_pos(0),
            edited.offset_to_pos(contents.len()),
        )],
        spans: vec![cfgrammar::Span::new(0, contents.len())],
        new_text: contents.to_owned(),
    })
}

/// Removes `.` and `..` components without touching the file system.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
            other => normalized.push(other),
        }
    }
    normalized
}

/// A stable identity for a file that may not exist yet.
fn identity(path: &Path) -> PathBuf {
    if let Ok(path) = path.canonicalize() {
        return path;
    }
    match (path.parent().map(Path::canonicalize), path.file_name()) {
        (Some(Ok(parent)), Some(name)) => parent.join(name),
        _ => path.to_path_buf(),
    }
}

/// `path` relative to `root` when it lies inside it.
fn display_path(root: Option<&Path>, path: &Path) -> PathBuf {
    let Some(root) = root else {
        return path.to_path_buf();
    };
    if let Ok(relative) = path.strip_prefix(root) {
        return relative.to_path_buf();
    }
    identity(path)
        .strip_prefix(identity(root))
        .map(Path::to_path_buf)
        .unwrap_or_else(|_| path.to_path_buf())
}

fn agent_diagnostics(
    root: Option<&Path>,
    diagnostics: &IndexMap<Uri, Vec<Diagnostic>>,
) -> Vec<AgentDiagnostic> {
    diagnostics
        .iter()
        .flat_map(|(uri, diagnostics)| {
            let path = uri
                .to_file_path()
                .map(|path| display_path(root, &path))
                .unwrap_or_else(|| PathBuf::from(uri.as_str()));
            diagnostics.iter().map(move |diagnostic| AgentDiagnostic {
                path: path.clone(),
                line: diagnostic.range.start.line + 1,
                column: diagnostic.range.start.character + 1,
                message: diagnostic.message.clone(),
            })
        })
        .collect()
}

/// The editor's URI for `path`, if Neovim has it open under any name.
pub(crate) fn open_uri(source: &SourceState, path: &Path) -> Option<Uri> {
    let target = identity(path);
    source
        .editor_files
        .keys()
        .find(|uri| {
            uri.to_file_path()
                .is_some_and(|open| identity(&open) == target)
        })
        .cloned()
}

struct AgentActivity {
    progress: OngoingProgress<Unbounded, NotCancellable>,
}

impl Backend {
    fn root(&self) -> Result<PathBuf, String> {
        self.state
            .root_dir
            .get()
            .cloned()
            .ok_or_else(|| "The analyzer has not opened a workspace yet.".to_owned())
    }

    /// An agent-supplied path, relative to the workspace root unless absolute.
    fn resolve(&self, path: &Path) -> Result<PathBuf, String> {
        let root = self.root()?;
        Ok(normalize(&if path.is_absolute() {
            path.to_path_buf()
        } else {
            root.join(path)
        }))
    }

    /// Whether `path` is in the workspace or one of the modules it compiles.
    async fn in_workspace(&self, path: &Path) -> bool {
        let Ok(root) = self.root() else {
            return false;
        };
        let target = identity(path);
        if target.starts_with(identity(&root)) {
            return true;
        }
        let ast = self.state.published_state.lock().await.ast.clone();
        ast.values().any(|module| identity(&module.path) == target)
    }

    async fn check_editable(&self, path: &Path) -> Result<(), String> {
        if path.extension().is_none_or(|extension| extension != "ar") {
            return Err(format!(
                "{} is not an Argon source file. Only .ar files are edited through Neovim.",
                path.display()
            ));
        }
        if !self.in_workspace(path).await {
            return Err(format!(
                "{} is outside this Argon workspace.",
                path.display()
            ));
        }
        Ok(())
    }

    /// Waits until `found` returns a value for the source state, which is
    /// rechecked each time an editor document changes.
    async fn wait_for_source<T>(&self, found: impl Fn(&SourceState) -> Option<T>) -> Option<T> {
        let mut changes = self.state.source_changes.subscribe();
        let deadline = Instant::now() + SETTLE_TIMEOUT;
        loop {
            if let Some(value) = found(&*self.state.source_state.lock().await) {
                return Some(value);
            }
            match tokio::time::timeout_at(deadline, changes.changed()).await {
                Ok(Ok(())) => {}
                _ => return None,
            }
        }
    }

    async fn wait_for_version(&self, uri: &Uri, version: i32) -> bool {
        self.wait_for_source(|source| {
            source
                .editor_files
                .get(uri)
                .filter(|document| document.version() >= version)
                .map(|_| ())
        })
        .await
        .is_some()
    }

    async fn wait_for_compiled(&self, revision: u64) -> bool {
        let mut compiled = self.state.compiled_revisions.subscribe();
        tokio::time::timeout(COMPILE_TIMEOUT, compiled.wait_for(|&done| done >= revision))
            .await
            .is_ok_and(|result| result.is_ok())
    }

    /// Makes sure Neovim has `path` open in a buffer attached to this analyzer.
    async fn ensure_buffer(&self, path: &Path, create: bool) -> Result<Uri, String> {
        if let Some(uri) = open_uri(&*self.state.source_state.lock().await, path) {
            return Ok(uri);
        }
        if create {
            if path.exists() {
                return Err(format!(
                    "{} already exists; use edit_file instead.",
                    path.display()
                ));
            }
            if !path.parent().is_some_and(Path::is_dir) {
                return Err(format!(
                    "The directory for {} does not exist.",
                    path.display()
                ));
            }
        } else if !path.is_file() {
            return Err(format!("{} does not exist.", path.display()));
        }
        let uri = Uri::from_file_path(path)
            .ok_or_else(|| format!("{} is not a valid file path.", path.display()))?;
        let response = self
            .state
            .editor_client
            .send_request::<EnsureBuffer>(EnsureBufferParams { uri, create })
            .await
            .map_err(|error| format!("Could not ask Neovim to open {}: {error}", path.display()))?;
        if !response.ok {
            return Err(response
                .message
                .unwrap_or_else(|| format!("Neovim could not open {}.", path.display())));
        }
        self.wait_for_source(|source| open_uri(source, path))
            .await
            .ok_or_else(|| format!("Neovim opened {} but did not attach it.", path.display()))
    }

    /// Applies the plan `plan_for` makes against the current buffer text,
    /// replanning if the buffer moved on first. Returns the source revision
    /// that includes the edit.
    async fn apply_agent_edit(
        &self,
        uri: &Uri,
        label: &str,
        plan_for: impl Fn(&Document) -> Result<EditPlan, String>,
    ) -> Result<(EditPlan, u64), String> {
        for _ in 0..MAX_EDIT_ATTEMPTS {
            let plan = {
                let source = self.state.source_state.lock().await;
                let document = source
                    .editor_files
                    .get(uri)
                    .ok_or_else(|| "Neovim closed the file before the edit.".to_owned())?;
                plan_for(document)?
            };
            let confirm = self.state.config().agent.approval == ApprovalMode::Always;
            self.state.begin_pending_edit(uri).await;
            let result = self
                .state
                .editor_client
                .send_request::<ApplyAgentEdit>(ApplyAgentEditParams {
                    uri: uri.clone(),
                    version: plan.version,
                    edits: plan.edits.clone(),
                    highlights: plan.highlights.clone(),
                    label: label.to_owned(),
                    confirm,
                    new_text: confirm.then(|| plan.new_text.clone()),
                })
                .await;
            match result {
                Ok(ApplyAgentEditResult::Applied { version }) => {
                    if !self.wait_for_version(uri, version).await {
                        self.state.end_pending_edit(uri).await;
                        return Err(
                            "Neovim applied the edit but never reported the change.".to_owned()
                        );
                    }
                    let revision = self.state.source_state.lock().await.revision;
                    return Ok((plan, revision));
                }
                Ok(ApplyAgentEditResult::Stale { version }) => {
                    self.state.end_pending_edit(uri).await;
                    self.wait_for_version(uri, version).await;
                }
                Ok(ApplyAgentEditResult::Rejected) => {
                    self.state.end_pending_edit(uri).await;
                    return Err("The user rejected this edit in Neovim.".to_owned());
                }
                Ok(ApplyAgentEditResult::Failed { message }) => {
                    self.state.end_pending_edit(uri).await;
                    return Err(format!("Neovim could not apply the edit: {message}"));
                }
                Err(error) => {
                    self.state.end_pending_edit(uri).await;
                    return Err(format!("Could not reach Neovim: {error}"));
                }
            }
        }
        Err(
            "The file kept changing while the edit was applied. Read it again and retry."
                .to_owned(),
        )
    }

    async fn report(&self, revision: u64) -> Report {
        let root = self.state.root_dir.get().cloned();
        let published = self.state.published_state.lock().await;
        Report {
            revision,
            compiled_revision: published.compiled_revision,
            summary: published
                .latest
                .as_ref()
                .map(|latest| CompileSummary::new(latest.cell.as_deref(), &latest.output)),
            diagnostics: agent_diagnostics(root.as_deref(), &published.prev_diagnostics),
            messages: published.messages.clone(),
        }
    }

    /// Waits for the edit's compilation, outlines what it changed in the
    /// GUI, and reports the result.
    async fn settle(&self, path: &Path, plan: &EditPlan, revision: u64) -> Report {
        if self.wait_for_compiled(revision).await {
            self.highlight_in_gui(revision, path, &plan.spans).await;
        }
        self.report(revision).await
    }

    async fn highlight_in_gui(&self, revision: u64, path: &Path, spans: &[cfgrammar::Span]) {
        let Some(connection) = self.state.gui_connection().await else {
            return;
        };
        // Compiled spans name files the way the compiler found them.
        let target = identity(path);
        let path = self
            .state
            .published_state
            .lock()
            .await
            .ast
            .values()
            .find(|module| identity(&module.path) == target)
            .map_or_else(|| path.to_path_buf(), |module| module.path.clone());
        let spans = spans
            .iter()
            .map(|&span| Span {
                path: path.clone(),
                span,
            })
            .collect();
        let result = connection
            .client
            .highlight_spans(context::current(), revision, spans)
            .await;
        self.handle_gui_result(&connection, result).await;
    }

    async fn begin_agent_activity(&self, label: &str) -> AgentActivity {
        let id = self
            .state
            .next_compilation_activity_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let token = ProgressToken::String(format!("argon-agent-{id}"));
        let _ = self
            .state
            .editor_client
            .create_work_done_progress(token.clone())
            .await;
        let progress = self
            .state
            .editor_client
            .progress(token, "Argon agent")
            .with_message(label)
            .begin()
            .await;
        self.publish_agent_activity(Some(label.to_owned())).await;
        AgentActivity { progress }
    }

    async fn finish_agent_activity(&self, activity: AgentActivity) {
        activity.progress.finish().await;
        self.publish_agent_activity(None).await;
    }

    async fn publish_agent_activity(&self, label: Option<String>) {
        if let Some(connection) = self.state.gui_connection().await {
            let result = connection
                .client
                .agent_activity(context::current(), label)
                .await;
            self.handle_gui_result(&connection, result).await;
        }
    }

    pub(crate) async fn follow_mode(&self, params: FollowModeParams) -> crate::Result<()> {
        self.state
            .follow_mode
            .store(params.enabled, Ordering::Release);
        Ok(())
    }

    async fn edit_with(
        &self,
        path: PathBuf,
        label: String,
        create: bool,
        plan_for: impl Fn(&Document) -> Result<EditPlan, String>,
    ) -> Result<Report, String> {
        let path = self.resolve(&path)?;
        self.check_editable(&path).await?;
        // One edit at a time, so each plans against text the last one settled.
        let _serial = self.state.agent_edits.lock().await;
        let activity = self.begin_agent_activity(&label).await;
        let result = async {
            let uri = self.ensure_buffer(&path, create).await?;
            let (plan, revision) = self.apply_agent_edit(&uri, &label, &plan_for).await?;
            Ok(self.settle(&path, &plan, revision).await)
        }
        .await;
        self.finish_agent_activity(activity).await;
        result
    }
}

impl Agent for Backend {
    async fn status(self, _: context::Context) -> AgentStatus {
        let root = self.state.root_dir.get().cloned();
        let gui_connected = self.state.gui_connection().await.is_some();
        let watching_files = self
            .state
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .is_some();
        let source = self.state.source_state.lock().await;
        let compiled_revision = self.state.published_state.lock().await.compiled_revision;
        AgentStatus {
            open_cell: source.cell.clone(),
            gui_connected,
            follow_mode: self.state.follow_mode.load(Ordering::Acquire),
            approval: self.state.config().agent.approval,
            workspace_modified: source.workspace_modified,
            watching_files,
            open_files: source
                .editor_files
                .keys()
                .filter_map(|uri| uri.to_file_path())
                .map(|path| display_path(root.as_deref(), &path))
                .collect(),
            revision: source.revision,
            compiled_revision,
            root,
        }
    }

    async fn read_file(self, _: context::Context, path: PathBuf) -> Result<FileContents, String> {
        let path = self.resolve(&path)?;
        if !self.in_workspace(&path).await {
            return Err(format!(
                "{} is outside this Argon workspace.",
                path.display()
            ));
        }
        let root = self.state.root_dir.get().cloned();
        let display = display_path(root.as_deref(), &path);
        {
            let source = self.state.source_state.lock().await;
            if let Some(uri) = open_uri(&source, &path) {
                return Ok(FileContents {
                    path: display,
                    contents: source.editor_files[&uri].contents().to_owned(),
                    from_editor: true,
                });
            }
        }
        let contents = tokio::fs::read_to_string(&path)
            .await
            .map_err(|error| format!("Could not read {}: {error}", path.display()))?;
        Ok(FileContents {
            path: display,
            contents,
            from_editor: false,
        })
    }

    async fn edit_file(self, _: context::Context, request: EditRequest) -> Result<Report, String> {
        let encoding = self.state.position_encoding();
        let edits = request.edits;
        self.edit_with(request.path, request.label, false, |document| {
            plan_edit(document, &edits, encoding)
        })
        .await
    }

    async fn create_file(
        self,
        _: context::Context,
        request: CreateRequest,
    ) -> Result<Report, String> {
        let encoding = self.state.position_encoding();
        let contents = request.contents;
        self.edit_with(request.path, request.label, true, |document| {
            plan_insert(document, &contents, encoding)
        })
        .await
    }

    async fn diagnostics(self, _: context::Context) -> Report {
        let revision = self.state.source_state.lock().await.revision;
        self.report(revision).await
    }

    async fn compile_cell(self, _: context::Context, cell: String) -> Result<Report, String> {
        let root_dir = self.root()?;
        let revision = self.state.source_state.lock().await.revision;
        let result = self
            .state
            .compiler
            .compile(CompileRequest {
                identity: CompileIdentity {
                    revision,
                    cell: Some(cell.clone()),
                },
                root_dir: root_dir.clone(),
            })
            .await
            .ok_or_else(|| "The compiler is unavailable.".to_owned())?;
        let diagnostics = crate::diagnostics(
            &result.root_dir,
            &result.ast,
            result.output.as_ref(),
            self.state.position_encoding(),
        );
        Ok(Report {
            revision,
            compiled_revision: revision,
            summary: result
                .output
                .as_ref()
                .map(|output| CompileSummary::new(Some(&cell), output)),
            diagnostics: agent_diagnostics(Some(&root_dir), &diagnostics),
            messages: result.messages,
        })
    }

    async fn open_cell(self, _: context::Context, cell: String) -> Result<Report, String> {
        if !self.state.follow_mode.load(Ordering::Acquire) {
            return Err(FOLLOW_REQUIRED.to_owned());
        }
        let identity = {
            let mut source = self.state.source_state.lock().await;
            source.cell = Some(cell);
            source.compile_identity()
        };
        if let Some(connection) = self.update_cell(identity.clone()).await {
            let result = connection.client.fit(context::current()).await;
            self.handle_gui_result(&connection, result).await;
        }
        Ok(self.report(identity.revision).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn document(text: &str) -> Document {
        Document::new(text.to_owned(), 7, PositionEncoding::Utf8)
    }

    fn replacement(old: &str, new: &str) -> Replacement {
        Replacement {
            old: old.to_owned(),
            new: new.to_owned(),
            replace_all: false,
        }
    }

    #[test]
    fn plans_edits_against_the_original_text() {
        let text = "cell top() {\n    let a = 1.;\n    let b = 2.;\n}\n";
        let plan = plan_edit(
            &document(text),
            &[
                replacement("let b = 2.;", "let b = 20.;"),
                replacement("1.", "10."),
            ],
            PositionEncoding::Utf8,
        )
        .unwrap();
        assert_eq!(plan.version, 7);
        assert_eq!(
            plan.new_text,
            "cell top() {\n    let a = 10.;\n    let b = 20.;\n}\n"
        );
        assert_eq!(plan.edits.len(), 2);
        assert_eq!(
            plan.edits[0].range.start,
            tower_lsp_server::ls_types::Position::new(1, 12)
        );
        assert_eq!(
            &plan.new_text[plan.spans[0].start()..plan.spans[0].end()],
            "10."
        );
        assert_eq!(
            &plan.new_text[plan.spans[1].start()..plan.spans[1].end()],
            "let b = 20.;"
        );
        assert_eq!(
            plan.highlights[1].start,
            tower_lsp_server::ls_types::Position::new(2, 4)
        );
    }

    #[test]
    fn rejects_missing_ambiguous_and_overlapping_edits() {
        let doc = document("aaa bb bb");
        let error =
            |edits: &[Replacement]| plan_edit(&doc, edits, PositionEncoding::Utf8).unwrap_err();
        assert!(error(&[replacement("zz", "y")]).contains("not found"));
        assert!(error(&[replacement("bb", "c")]).contains("occurs 2 times"));
        assert!(error(&[replacement("aa", "c")]).contains("occurs 2 times"));
        assert!(error(&[replacement("aaa", "x"), replacement("a b", "y")]).contains("overlap"));
        assert!(error(&[replacement("", "x")]).contains("create_file"));
        assert!(error(&[]).contains("No edits"));
        let all = plan_edit(
            &doc,
            &[Replacement {
                replace_all: true,
                ..replacement("bb", "c")
            }],
            PositionEncoding::Utf8,
        )
        .unwrap();
        assert_eq!(all.new_text, "aaa c c");
        assert!(
            plan_edit(
                &doc,
                &[Replacement {
                    replace_all: true,
                    ..replacement("aa", "c")
                }],
                PositionEncoding::Utf8,
            )
            .unwrap_err()
            .contains("overlapping")
        );
    }

    #[test]
    fn insertion_requires_a_blank_document() {
        let plan = plan_insert(&document("\n"), "cell a() {}\n", PositionEncoding::Utf8).unwrap();
        assert_eq!(
            plan.edits[0].range.start,
            tower_lsp_server::ls_types::Position::new(0, 0)
        );
        assert_eq!(plan.new_text, "cell a() {}\n");
        assert_eq!(plan.spans, vec![cfgrammar::Span::new(0, 12)]);
        assert_eq!(
            plan.highlights[0].start,
            tower_lsp_server::ls_types::Position::new(0, 0)
        );
        assert!(plan_insert(&document("x"), "y", PositionEncoding::Utf8).is_err());
    }

    #[test]
    fn paths_are_normalized_and_shown_relative_to_the_root() {
        assert_eq!(
            normalize(Path::new("/a/./b/../c.ar")),
            PathBuf::from("/a/c.ar")
        );
        assert_eq!(
            display_path(
                Some(Path::new("/work/lib")),
                Path::new("/work/lib/src/a.ar")
            ),
            PathBuf::from("src/a.ar")
        );
        assert_eq!(
            display_path(Some(Path::new("/work/lib")), Path::new("/elsewhere/a.ar")),
            PathBuf::from("/elsewhere/a.ar")
        );
    }
}
