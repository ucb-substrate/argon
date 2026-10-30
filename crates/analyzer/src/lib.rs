pub mod agent;
mod cell_edit;
mod cell_list;
mod command_completion;
mod compiler_worker;
pub mod document;
pub mod hook;
pub mod mcp;
mod navigation;
mod recents;
pub mod rpc;
mod schematic_edit;
pub mod session;
pub mod transport;
mod watcher;

pub mod cli;

use std::{
    collections::HashMap,
    env, fs, io,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use arc::Library;
use argonc::{
    WorkspaceConfig,
    ast::{ModPath, Span},
    compile::{
        self, Arrayed, CompileOutput, DeviceKind, ExecErrorCompileOutput, StaticErrorCompileOutput,
    },
    diagnostics,
    nav::NavIndex,
    parse::{self, AnnotatedParseAst, WorkspaceParseAst},
};
use futures::prelude::*;
use indexmap::IndexMap;
use rpc::{
    CompilationSnapshot, CompressedCompilationUpdate, DevicePreview, FocusedCellPreview, GuiClient,
    InstancePreview, LangServer, SchematicInstancePreview, insert_statement,
};
use serde::{Deserialize, Serialize};
use tarpc::{context, server::Channel};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader},
    process::{Child, Command},
    sync::Mutex,
};
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{notification::Notification, request::Request, *};
use tower_lsp_server::{
    Client, LanguageServer, LspService, NotCancellable, OngoingProgress, Server, Unbounded,
};
use tracing::{error, info};
use tracing_subscriber::{
    EnvFilter, Registry, layer::SubscriberExt, reload, util::SubscriberInitExt,
};

use crate::command_completion::{CommandCompletion, CommandCompletionParams};
use crate::compiler_worker::{
    CompileIdentity, CompilePreview, CompileRequest, CompileResult, CompilerWorker,
    INTERNAL_COMPILER_ERROR_MESSAGE,
};
use crate::document::{Document, DocumentChange, PositionEncoding};

const LOG_FILE: &str = "argon.log";

// tarpc gives every new context a ten-second deadline. A compilation update can
// contain a complete native layout, and both transferring it and preparing its
// GUI hierarchy are legitimately unbounded with respect to that default. tarpc
// has no deadline-free context, so give snapshot delivery an effectively
// unlimited horizon. Connection loss still fails the request immediately.
const GUI_SNAPSHOT_DEADLINE: Duration = Duration::from_secs(365 * 24 * 60 * 60);

fn gui_snapshot_context() -> context::Context {
    let mut context = context::current();
    context.deadline = Instant::now() + GUI_SNAPSHOT_DEADLINE;
    context
}

fn completion_trigger_characters() -> Vec<String> {
    // Neovim's native LSP completion uses the server's trigger characters for
    // automatic completion. Argon identifiers are ASCII, so advertising the
    // identifier alphabet gives completion on each typed prefix as well as on
    // member/path separators.
    (b'a'..=b'z')
        .chain(b'A'..=b'Z')
        .chain(*b"_.:")
        .map(|byte| char::from(byte).to_string())
        .collect()
}

fn read_unpoisoned<T>(lock: &RwLock<T>) -> RwLockReadGuard<'_, T> {
    lock.read().unwrap_or_else(|poisoned| {
        // Configuration updates replace the whole value, so no partially
        // mutated configuration needs to be rolled back after a panic.
        lock.clear_poison();
        poisoned.into_inner()
    })
}

fn write_unpoisoned<T>(lock: &RwLock<T>) -> RwLockWriteGuard<'_, T> {
    lock.write().unwrap_or_else(|poisoned| {
        lock.clear_poison();
        poisoned.into_inner()
    })
}
static LOG_RELOAD: OnceLock<reload::Handle<EnvFilter, Registry>> = OnceLock::new();

pub use protocol::config::*;

/// Argon's own subdirectory of one XDG base directory.
///
/// `variable` is the XDG environment variable naming the base directory, and
/// `home_fallback` the path relative to the home directory that the XDG
/// specification falls back to when it is unset or empty.
fn argon_xdg_dir(variable: &str, home_fallback: &str) -> Option<PathBuf> {
    env::var_os(variable)
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            homedir::my_home()
                .ok()
                .flatten()
                .map(|home| home.join(home_fallback))
        })
        .map(|directory| directory.join("argon"))
}

pub fn argon_config_path() -> Option<PathBuf> {
    argon_xdg_dir("XDG_CONFIG_HOME", ".config").map(|directory| directory.join("config.toml"))
}

pub fn argon_state_dir() -> Option<PathBuf> {
    argon_xdg_dir("XDG_STATE_HOME", ".local/state")
}

/// Creates `directory` and its missing parents readable only by the current
/// user, as the XDG base directory specification asks. Existing directories
/// keep their permissions.
pub(crate) fn create_private_dir_all(directory: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(directory)
    }
    #[cfg(not(unix))]
    {
        fs::create_dir_all(directory)
    }
}

pub fn argon_cache_dir() -> Option<PathBuf> {
    argon_xdg_dir("XDG_CACHE_HOME", ".cache")
}

pub fn argon_log_path() -> Option<PathBuf> {
    argon_state_dir().map(|directory| directory.join(LOG_FILE))
}

/// The progress message shown for the whole compilation of `identity`.
fn compilation_message(identity: &CompileIdentity) -> String {
    identity.cell.as_deref().map_or_else(
        || "Compiling workspace".to_owned(),
        |cell| format!("Compiling {cell}"),
    )
}

fn read_config() -> std::result::Result<ArgonConfig, String> {
    let Some(path) = argon_config_path() else {
        return Ok(ArgonConfig::default());
    };
    let text = match fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ArgonConfig::default());
        }
        Err(error) => {
            return Err(format!("could not read '{}': {error}", path.display()));
        }
    };
    parse_config(&text).map_err(|error| format!("could not parse '{}': {error}", path.display()))
}

fn parse_config(text: &str) -> std::result::Result<ArgonConfig, toml::de::Error> {
    toml::from_str(text)
}

fn config_with_key(
    config: &ArgonConfig,
    key: &str,
    value: Option<&str>,
) -> std::result::Result<ArgonConfig, String> {
    let segments = key.split('.').collect::<Vec<_>>();
    if segments.is_empty()
        || segments.iter().any(|segment| {
            segment.is_empty()
                || !segment
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        })
    {
        return Err(format!("invalid configuration key '{key}'"));
    }

    let serialized = toml::to_string(config)
        .map_err(|error| format!("could not serialize current configuration: {error}"))?;
    let mut document = serialized
        .parse::<toml::Table>()
        .map_err(|error| format!("could not inspect current configuration: {error}"))?;
    let mut table = &mut document;
    for segment in &segments[..segments.len() - 1] {
        let entry = table
            .entry((*segment).to_owned())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        table = entry
            .as_table_mut()
            .ok_or_else(|| format!("configuration key '{segment}' is not a table"))?;
    }

    let leaf = segments[segments.len() - 1];
    if let Some(value) = value {
        let assignment = format!("value = {value}");
        let parsed = match assignment.parse::<toml::Table>() {
            Ok(mut table) => table
                .remove("value")
                .expect("the parsed assignment contains its value"),
            Err(_) if !value.is_empty() && !value.chars().any(char::is_whitespace) => {
                toml::Value::String(value.to_owned())
            }
            Err(error) => {
                return Err(format!("invalid TOML value for '{key}': {error}"));
            }
        };
        table.insert(leaf.to_owned(), parsed);
    } else if table.remove(leaf).is_none() {
        return Err(format!("configuration key '{key}' is not set"));
    }

    toml::Value::Table(document)
        .try_into()
        .map_err(|error| format!("invalid value for configuration key '{key}': {error}"))
}

fn write_config(path: &Path, config: &ArgonConfig) -> std::result::Result<(), String> {
    let text = toml::to_string_pretty(config)
        .map_err(|error| format!("could not serialize configuration: {error}"))?;
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent).map_err(|error| {
            format!(
                "could not create configuration directory '{}': {error}",
                parent.display()
            )
        })?;
    }
    fs::write(path, text).map_err(|error| {
        format!(
            "could not write configuration '{}': {error}",
            path.display()
        )
    })
}

pub fn init_logging() {
    let config = read_config().unwrap_or_else(|error| {
        eprintln!("argon: {error}; using log level '{DEFAULT_LOG_LEVEL}'");
        ArgonConfig::default()
    });
    let filter = EnvFilter::try_new(&config.log.level).unwrap_or_else(|error| {
        eprintln!(
            "argon: invalid log level '{}': {error}; using '{DEFAULT_LOG_LEVEL}'",
            config.log.level
        );
        EnvFilter::new(DEFAULT_LOG_LEVEL)
    });
    let Some(log_dir) = argon_state_dir() else {
        return;
    };
    if let Err(error) = create_private_dir_all(&log_dir) {
        eprintln!(
            "argon: could not create log directory '{}': {error}",
            log_dir.display()
        );
        return;
    }
    let (filter, reload_handle) = reload::Layer::new(filter);
    let subscriber = tracing_subscriber::registry().with(filter).with(
        tracing_subscriber::fmt::layer()
            .with_writer(tracing_appender::rolling::never(log_dir, LOG_FILE))
            .with_ansi(false),
    );
    if subscriber.try_init().is_ok() {
        let _ = LOG_RELOAD.set(reload_handle);
    }
}

pub fn reload_log_filter(level: &str) -> std::result::Result<(), String> {
    let filter = EnvFilter::try_new(level).map_err(|error| error.to_string())?;
    if let Some(handle) = LOG_RELOAD.get() {
        handle.reload(filter).map_err(|error| error.to_string())?;
    }
    Ok(())
}

/// How long the editor may take to report all of an analyzer edit it applied.
/// Neovim reports the edits after the first one together, once its change
/// debounce passes.
pub(crate) const PENDING_EDIT_TIMEOUT: Duration = Duration::from_secs(1);

#[derive(Debug, Default)]
pub(crate) struct SourceState {
    pub(crate) revision: u64,
    pub(crate) cell: Option<String>,
    /// An invocation the user opened, to record as recent once it compiles.
    pub(crate) pending_recent: Option<String>,
    pub(crate) editor_files: IndexMap<Uri, Document>,
    /// The text each file will have once the editor reports all of the
    /// analyzer's edit to it.
    pub(crate) pending_workspace_edits: IndexMap<Uri, String>,
    pub(crate) workspace_modified: bool,
}

/// What a change to an editor file means for an analyzer edit to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditProgress {
    /// No analyzer edit to the file is in flight.
    Unrelated,
    /// The editor has reported part of the edit, and the rest is to come.
    Partial,
    /// The editor has reported all of the edit.
    Complete,
}

impl SourceState {
    fn advance_revision(&mut self) -> u64 {
        self.revision = self.revision.saturating_add(1);
        self.revision
    }

    /// How much of the analyzer's edit to `uri` the editor has reported,
    /// retiring the edit once all of it has arrived.
    fn edit_progress(&mut self, uri: &Uri) -> EditProgress {
        let Some(expected) = self.pending_workspace_edits.get(uri) else {
            return EditProgress::Unrelated;
        };
        if self
            .editor_files
            .get(uri)
            .is_none_or(|document| document.contents() != expected)
        {
            return EditProgress::Partial;
        }
        self.pending_workspace_edits.shift_remove(uri);
        EditProgress::Complete
    }

    fn compile_identity(&self) -> CompileIdentity {
        CompileIdentity {
            revision: self.revision,
            cell: self.cell.clone(),
        }
    }
}

#[derive(Debug, Default)]
pub(crate) struct PublishedState {
    pub(crate) config: WorkspaceConfig,
    pub(crate) ast: Arc<WorkspaceParseAst>,
    /// Latest usable navigation index.
    ///
    /// This can be older than `ast`: the compiler keeps serving the last index
    /// that covered the workspace so navigation survives a broken edit, and
    /// decides on its own when that index is superseded. The index carries the
    /// sources its offsets are into, so a stale one stays readable without
    /// retaining the parse it came from.
    pub(crate) nav: Option<Arc<NavIndex>>,
    pub(crate) prev_diagnostics: IndexMap<Uri, Vec<Diagnostic>>,
    pub(crate) compiled_revision: u64,
    pub(crate) compiled_cell: Option<String>,
    /// Compiler messages that accompanied the latest committed result.
    pub(crate) messages: Vec<String>,
    /// The latest committed result, summarized on demand for agents.
    pub(crate) latest: Option<agent::LatestCompile>,
}

#[derive(Clone, Debug)]
struct GuiConnection {
    id: u64,
    client: GuiClient,
    snapshot: Arc<tokio::sync::Mutex<Option<CompilationSnapshot>>>,
    /// The library's cells as last sent to this GUI.
    cells: Arc<tokio::sync::Mutex<Option<Vec<rpc::CellEntry>>>>,
}

#[derive(Debug, Default)]
struct GuiState {
    connection: Option<GuiConnection>,
    process: Option<Child>,
    next_connection_id: u64,
}

struct CompilationActivity {
    id: u64,
    gui_connection: Option<GuiConnection>,
    editor_progress: OngoingProgress<Unbounded, NotCancellable>,
}

fn is_gui_disconnected(error: &tarpc::client::RpcError) -> bool {
    matches!(
        error,
        tarpc::client::RpcError::Shutdown
            | tarpc::client::RpcError::Send(_)
            | tarpc::client::RpcError::Channel(_)
    )
}

/// Errors that actually prevent the GUI from displaying a compiled cell.
/// Execution diagnostics may still contain usable output (most notably an
/// underconstrained cell with initial-condition fallbacks), so those are
/// published as diagnostics without also claiming that the cell did not open.
/// Most error messages folded into one editor popup.
///
/// The popup is a single line, and a broken file reports an error per
/// remaining token, so the whole list would otherwise arrive as one
/// multi-megabyte notification.
const MAX_POPUP_MESSAGES: usize = 10;

fn blocking_compile_error_messages(output: &CompileOutput) -> Vec<String> {
    let mut messages = blocking_compile_error_messages_uncapped(output);
    if messages.len() > MAX_POPUP_MESSAGES {
        let dropped = messages.len() - MAX_POPUP_MESSAGES;
        messages.truncate(MAX_POPUP_MESSAGES);
        messages.push(format!(
            "... and {dropped} more error{}",
            if dropped == 1 { "" } else { "s" }
        ));
    }
    messages
}

fn blocking_compile_error_messages_uncapped(output: &CompileOutput) -> Vec<String> {
    match output {
        CompileOutput::FatalParseErrors => {
            vec!["fatal parse errors encountered, unable to compile".to_string()]
        }
        CompileOutput::StaticErrors(output) => output
            .errors
            .iter()
            .map(|error| error.kind.to_string())
            .collect(),
        CompileOutput::ExecErrors(output) if output.output.is_some() => Vec::new(),
        CompileOutput::ExecErrors(output) => output
            .errors
            .iter()
            .map(|error| error.kind.to_string())
            .collect(),
        CompileOutput::Valid(_) => Vec::new(),
    }
}

fn workspace_config(root_lib: PathBuf, library: Option<&Library>) -> WorkspaceConfig {
    let Some(library) = library else {
        return WorkspaceConfig::new(root_lib);
    };
    WorkspaceConfig::new(root_lib)
        .with_dependencies(
            library
                .dependencies
                .iter()
                .map(|(name, path)| (name.clone(), path.clone())),
        )
        .with_tech(library.tech.clone())
        .with_gds_imports(
            library
                .gds
                .iter()
                .map(|(name, path)| (name.clone(), path.clone())),
        )
}

/// Runs `f` on its own thread with the compiler's stack, so that a deep
/// compile neither overflows nor blocks the async runtime. Returns `None` if
/// `f` panicked or the thread could not start.
async fn on_compile_stack<T: Send + 'static>(f: impl FnOnce() -> T + Send + 'static) -> Option<T> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    let spawned = std::thread::Builder::new()
        .name("argon-command-compile".to_owned())
        .stack_size(argonc::COMPILE_STACK_SIZE)
        .spawn(move || {
            let _ = sender.send(f());
        });
    if let Err(error) = spawned {
        error!("could not start a compilation thread: {error}");
        return None;
    }
    let result = receiver.await.ok();
    if result.is_none() {
        error!("internal compiler error while running an editor command");
    }
    result
}

/// Compiles `open_cell` with `preview_source` in place of `module`, the
/// module at `module_path`, or returns a message saying why it could not.
fn compile_preview_workspace(
    workspace_ast: &WorkspaceParseAst,
    module_path: ModPath,
    module: &AnnotatedParseAst,
    preview_source: String,
    open_cell: &str,
    workspace: &WorkspaceConfig,
    describe: &str,
) -> std::result::Result<compile::CompiledData, String> {
    let mut preview_module = parse::parse_source_text(preview_source, module.path.to_path_buf())
        .map_err(|error| format!("Could not {describe}: {error}"))?;
    preview_module.promote_last_declarations(module.generated_declarations);
    let mut preview_ast = workspace_ast.clone();
    preview_ast.insert(module_path, preview_module);
    // Recompile the open cell against the preview. Splicing its invocation
    // before static analysis lets argument expressions use normal name
    // resolution and type checking.
    let invocation = parse::splice_cell_invocation(&mut preview_ast, open_cell)
        .map_err(|error| format!("Open cell is invalid: {error}"))?;
    let (typed_ast, static_output) = compile::static_compile(&preview_ast)
        .ok_or_else(|| "Could not analyze the preview".to_owned())?;
    if !static_output.errors.is_empty() {
        let message = static_output
            .errors
            .iter()
            .map(|error| error.kind.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        return Err(format!("Could not {describe}: {message}"));
    }
    if workspace.tech.is_none() {
        return Err("The Argon library does not configure a technology file".to_owned());
    }
    match compile::execute_cell_invocation(&typed_ast, &invocation, workspace) {
        CompileOutput::Valid(output) => Ok(output),
        CompileOutput::ExecErrors(ExecErrorCompileOutput {
            output: Some(output),
            errors,
        }) if !errors.iter().any(|error| error.kind.is_invalid_cell()) => Ok(output),
        output => {
            let messages = blocking_compile_error_messages(&output).join("\n");
            Err(format!("Could not {describe}: {messages}"))
        }
    }
}

/// Ranges compiler spans against the workspace files they came from.
///
/// Building a [`Document`] indexes every line of a file, so the one built for
/// a file is kept for the rest of the batch: a single compile routinely
/// reports many errors against the same handful of files.
struct SpanRanger<'a> {
    ast: &'a WorkspaceParseAst,
    encoding: PositionEncoding,
    documents: HashMap<PathBuf, Option<Document>>,
}

impl<'a> SpanRanger<'a> {
    fn new(ast: &'a WorkspaceParseAst, encoding: PositionEncoding) -> Self {
        Self {
            ast,
            encoding,
            documents: HashMap::new(),
        }
    }

    /// Range of `span` within the editor-visible text of the file that produced
    /// it.
    ///
    /// Returns `None` when the file is not part of the compiled workspace or
    /// when the span starts beyond the editor-visible source. Generated
    /// declarations (GDS import stubs, spliced entry cells) are appended past
    /// that boundary, so this is what keeps them from being reported at a
    /// position no buffer has.
    fn range(&mut self, span: &Span) -> Option<Range> {
        let ast = self.ast;
        let encoding = self.encoding;
        let document = self
            .documents
            .entry(span.path.to_path_buf())
            .or_insert_with(|| {
                let module = ast.values().find(|module| module.path == span.path)?;
                Some(Document::new(&module.source_text, 0, encoding))
            })
            .as_ref()?;
        if span.span.start() > document.contents().len() {
            return None;
        }
        Some(Range {
            start: document.offset_to_pos(span.span.start()),
            end: document.offset_to_pos(span.span.end()),
        })
    }
}

fn diagnostics(
    root_dir: &Path,
    ast: &WorkspaceParseAst,
    output: Option<&CompileOutput>,
    encoding: PositionEncoding,
) -> IndexMap<Uri, Vec<Diagnostic>> {
    let mut diagnostics: IndexMap<Uri, Vec<Diagnostic>> = IndexMap::new();
    if let Some(o) = output {
        let mut errs = match o {
            CompileOutput::FatalParseErrors => {
                vec![(
                    Span {
                        path: root_dir.join("lib.ar").into(),
                        span: cfgrammar::Span::new(0, 0),
                    },
                    "fatal parse errors encountered, unable to compile".to_string(),
                )]
            }
            CompileOutput::StaticErrors(StaticErrorCompileOutput { errors }) => errors
                .iter()
                .map(|e| (e.span.clone(), format!("{}", e.kind)))
                .collect(),
            CompileOutput::ExecErrors(ExecErrorCompileOutput { errors, .. }) => errors
                .iter()
                .map(|e| {
                    (
                        e.span.clone().unwrap_or_else(|| Span {
                            path: root_dir.join("lib.ar").into(),
                            span: cfgrammar::Span::new(0, 0),
                        }),
                        format!("{}", e.kind),
                    )
                })
                .collect(),
            CompileOutput::Valid(_) => vec![],
        };
        // The same bound the CLI applies. One unbalanced delimiter reports an
        // error per remaining token -- thousands for a 36 KB file -- and this
        // list is rebuilt and published on every keystroke.
        let dropped = diagnostics::condense_spanned(&mut errs);
        if dropped > 0 {
            errs.push((
                Span {
                    path: root_dir.join("lib.ar").into(),
                    span: cfgrammar::Span::new(0, 0),
                },
                format!(
                    "... and {dropped} more error{}",
                    if dropped == 1 { "" } else { "s" }
                ),
            ));
        }
        let mut ranger = SpanRanger::new(ast, encoding);
        for (span, message) in errs {
            if let Some(range) = ranger.range(&span)
                && let Some(url) = Uri::from_file_path(&span.path)
            {
                diagnostics.entry(url).or_default().push(Diagnostic {
                    range,
                    severity: Some(DiagnosticSeverity::ERROR),
                    message,
                    ..Default::default()
                });
            }
        }
    }
    diagnostics
}

#[derive(Debug, Clone)]
pub struct State {
    server_addr: SocketAddr,
    editor_client: Client,
    root_dir: Arc<OnceLock<PathBuf>>,
    /// Negotiated in `initialize`; UTF-16 until the client tells us otherwise.
    position_encoding: Arc<OnceLock<PositionEncoding>>,
    /// The directory the embedded standard library is written to on first
    /// use, so the editor can open it. `None` if it could not be written.
    ///
    /// An async cell because writing it is filesystem work that has to happen
    /// off the runtime's worker threads, and because requests arriving while
    /// the write is in flight should await it rather than repeat it.
    std_dir: Arc<tokio::sync::OnceCell<Option<PathBuf>>>,
    pub(crate) source_state: Arc<Mutex<SourceState>>,
    pub(crate) published_state: Arc<Mutex<PublishedState>>,
    compiler: CompilerWorker,
    /// Serializes GUI drags and remembers the edits they applied.
    pub(crate) drag_edits: Arc<Mutex<rpc::DragEditHistory>>,
    gui: Arc<Mutex<GuiState>>,
    next_compilation_activity_id: Arc<AtomicU64>,
    app_config: Arc<RwLock<ArgonConfig>>,
    /// Secret that every RPC connection must present.
    token: transport::SessionToken,
    session_file: Arc<std::sync::Mutex<Option<session::SessionFile>>>,
    /// Bumped whenever an editor document opens, changes, or closes.
    source_changes: Arc<tokio::sync::watch::Sender<u64>>,
    /// The source revision of the latest committed compilation.
    compiled_revisions: Arc<tokio::sync::watch::Sender<u64>>,
    /// Whether the editor has a follow window for agent edits.
    follow_mode: Arc<std::sync::atomic::AtomicBool>,
    /// Serializes agent edits so each resolves against settled text.
    agent_edits: Arc<Mutex<()>>,
    watcher: Arc<std::sync::Mutex<Option<watcher::WorkspaceWatcher>>>,
    /// Whether the GUI and this analyzer's editor share a window.
    embedded: bool,
    /// The library's recently opened invocations, most recent first. Held
    /// while the recents file is updated, so updates don't interleave.
    recents: Arc<Mutex<Vec<String>>>,
    /// The document the analyzer last edited through the editor, which GUI
    /// undo and redo apply to.
    last_source_edit: Arc<std::sync::Mutex<Option<Uri>>>,
}

impl State {
    fn new(
        server_addr: SocketAddr,
        editor_client: Client,
        token: transport::SessionToken,
        embedded: bool,
    ) -> Self {
        let config = read_config().unwrap_or_else(|error| {
            error!("{error}; using default configuration");
            ArgonConfig::default()
        });
        Self {
            server_addr,
            editor_client,
            root_dir: Default::default(),
            position_encoding: Default::default(),
            std_dir: Default::default(),
            source_state: Default::default(),
            published_state: Default::default(),
            compiler: CompilerWorker::new(),
            drag_edits: Default::default(),
            gui: Default::default(),
            next_compilation_activity_id: Default::default(),
            app_config: Arc::new(RwLock::new(config)),
            token,
            session_file: Default::default(),
            source_changes: Arc::new(tokio::sync::watch::Sender::new(0)),
            compiled_revisions: Arc::new(tokio::sync::watch::Sender::new(0)),
            follow_mode: Default::default(),
            agent_edits: Default::default(),
            watcher: Default::default(),
            embedded,
            recents: Default::default(),
            last_source_edit: Default::default(),
        }
    }

    pub(crate) fn notify_source_changed(&self) {
        self.source_changes
            .send_modify(|count| *count = count.wrapping_add(1));
    }

    /// Waits up to `timeout` for `found` to return a value for the source
    /// state, which is rechecked each time an editor document changes.
    pub(crate) async fn wait_for_source<T>(
        &self,
        timeout: Duration,
        found: impl Fn(&SourceState) -> Option<T>,
    ) -> Option<T> {
        let mut changes = self.source_changes.subscribe();
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            if let Some(value) = found(&*self.source_state.lock().await) {
                return Some(value);
            }
            match tokio::time::timeout_at(deadline, changes.changed()).await {
                Ok(Ok(())) => {}
                _ => return None,
            }
        }
    }

    fn apply_config(&self, config: ArgonConfig) {
        *write_unpoisoned(&self.app_config) = config;
    }

    fn config(&self) -> ArgonConfig {
        read_unpoisoned(&self.app_config).clone()
    }

    pub(crate) fn position_encoding(&self) -> PositionEncoding {
        self.position_encoding.get().copied().unwrap_or_default()
    }

    /// A read-only document over compiled source text, for converting spans
    /// into client-facing ranges. The version is meaningless for these.
    pub(crate) fn document(&self, contents: impl Into<arcstr::ArcStr>) -> Document {
        Document::new(contents, 0, self.position_encoding())
    }

    async fn is_latest_compile_request(&self, identity: &CompileIdentity) -> bool {
        let source = self.source_state.lock().await;
        source.compile_identity() == *identity
    }

    /// Whether the cell `identity` compiles is still the open one, at any
    /// source revision.
    async fn is_open_cell(&self, identity: &CompileIdentity) -> bool {
        self.source_state.lock().await.cell == identity.cell
    }

    async fn gui_connection(&self) -> Option<GuiConnection> {
        self.gui.lock().await.connection.clone()
    }

    async fn begin_compilation(&self, identity: &CompileIdentity) -> CompilationActivity {
        let id = self
            .next_compilation_activity_id
            .fetch_add(1, Ordering::Relaxed)
            .saturating_add(1);
        let gui_connection = self.gui_connection().await;
        if let Some(connection) = &gui_connection
            && let Err(error) = connection
                .client
                .compilation_started(context::current(), id)
                .await
            && is_gui_disconnected(&error)
        {
            self.clear_gui_connection(connection.id).await;
        }

        let token = ProgressToken::String(format!("argon-compilation-{id}"));
        // This is server-initiated progress, so reserve the token before
        // publishing its begin notification. Older clients may reject the
        // request but can still consume the standard $/progress messages.
        let _ = self
            .editor_client
            .create_work_done_progress(token.clone())
            .await;
        let editor_progress = self
            .editor_client
            .progress(token, "Argon compilation")
            .with_message(compilation_message(identity))
            .begin()
            .await;
        CompilationActivity {
            id,
            gui_connection,
            editor_progress,
        }
    }

    async fn finish_compilation(&self, activity: CompilationActivity) {
        activity.editor_progress.finish().await;
        let Some(connection) = activity.gui_connection else {
            return;
        };
        if let Err(error) = connection
            .client
            .compilation_finished(context::current(), activity.id)
            .await
            && is_gui_disconnected(&error)
        {
            self.clear_gui_connection(connection.id).await;
        }
    }

    async fn install_gui_connection(&self, client: GuiClient) -> GuiConnection {
        let mut gui = self.gui.lock().await;
        gui.next_connection_id = gui.next_connection_id.saturating_add(1);
        let connection = GuiConnection {
            id: gui.next_connection_id,
            client,
            snapshot: Arc::default(),
            cells: Arc::default(),
        };
        gui.connection = Some(connection.clone());
        connection
    }

    /// Records this analyzer in the user's session directory so local
    /// clients, such as the agent bridge, can find and authenticate to it.
    fn publish_session(&self, root_dir: &Path) {
        let info = session::SessionInfo {
            pid: std::process::id(),
            root: root_dir.to_path_buf(),
            port: self.server_addr.port(),
            token: self.token.to_hex(),
        };
        match session::publish(&info) {
            Ok(file) => {
                *self.session_file.lock().unwrap_or_else(|p| p.into_inner()) = Some(file);
            }
            Err(error) => error!("could not publish the analyzer session: {error}"),
        }
    }

    fn retire_session(&self) {
        self.session_file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        self.watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
    }

    async fn take_gui_process(&self) -> Option<Child> {
        self.gui.lock().await.process.take()
    }

    async fn set_gui_process(&self, process: Child) {
        self.gui.lock().await.process = Some(process);
    }

    pub(crate) async fn current_editor_ast(&self) -> Option<Arc<WorkspaceParseAst>> {
        let source = self.source_state.lock().await;
        let compiled = self.published_state.lock().await;
        rpc::editor_buffers_are_current(&source, &compiled).then(|| compiled.ast.clone())
    }

    pub(crate) async fn technology_grid(&self) -> f64 {
        let tech = self.published_state.lock().await.config.tech.clone();
        tech.and_then(|path| argonc::tech::read_tech(path).ok())
            .map(|tech| tech.grid_step())
            .unwrap_or(0.1)
    }

    async fn clear_gui_connection(&self, id: u64) {
        let mut gui = self.gui.lock().await;
        if gui
            .connection
            .as_ref()
            .is_some_and(|current| current.id == id)
        {
            gui.connection = None;
        }
    }

    async fn publish_workspace_modified(&self, modified: bool, connection: Option<GuiConnection>) {
        let Some(connection) = connection else {
            return;
        };
        if let Err(error) = connection
            .client
            .workspace_modified(context::current(), modified)
            .await
        {
            self.editor_client
                .show_message(MessageType::ERROR, format!("{error}"))
                .await;
            if is_gui_disconnected(&error) {
                self.clear_gui_connection(connection.id).await;
            }
        }
    }

    /// Sends the library's cells to the GUI if they changed since the last send.
    async fn publish_cells(&self, connection: Option<GuiConnection>) {
        let Some(connection) = connection else {
            return;
        };
        let mut sent = connection.cells.lock().await;
        let cells = {
            let compiled = self.published_state.lock().await;
            cell_list::library_cells(&compiled.ast, &compiled.config, sent.as_deref())
        };
        let Some(cells) = cells.filter(|cells| sent.as_ref() != Some(cells)) else {
            return;
        };
        match connection
            .client
            .set_cells(context::current(), cells.clone())
            .await
        {
            Ok(()) => *sent = Some(cells),
            Err(error) => {
                if is_gui_disconnected(&error) {
                    self.clear_gui_connection(connection.id).await;
                }
            }
        }
    }

    /// Sends the library's recent cells to the GUI.
    async fn publish_recent_cells(&self, connection: Option<GuiConnection>) {
        let Some(connection) = connection else {
            return;
        };
        // Holding the list while sending keeps a stale list from arriving last.
        let recents = self.recents.lock().await;
        let cells = recents.iter().cloned().map(rpc::RecentCell::new).collect();
        if let Err(error) = connection
            .client
            .set_recent_cells(context::current(), cells)
            .await
            && is_gui_disconnected(&error)
        {
            self.clear_gui_connection(connection.id).await;
        }
    }

    /// Replaces the recent cells with what `update` returns for the library
    /// root, off the async runtime, then sends them to the GUI.
    async fn update_recents(&self, update: impl FnOnce(&Path) -> Vec<String> + Send + 'static) {
        let Some(root_dir) = self.root_dir.get().cloned() else {
            return;
        };
        {
            let mut recents = self.recents.lock().await;
            match tokio::task::spawn_blocking(move || update(&root_dir)).await {
                Ok(cells) => *recents = cells,
                Err(error) => error!("could not update the recent cells: {error}"),
            }
        }
        self.publish_recent_cells(self.gui_connection().await).await;
    }

    async fn publish_workspace_path(&self, connection: Option<GuiConnection>) -> bool {
        let Some(connection) = connection else {
            return true;
        };
        let result = connection
            .client
            .set_workspace_path(context::current(), self.root_dir.get().cloned())
            .await;
        if let Err(error) = result {
            self.editor_client
                .show_message(
                    MessageType::ERROR,
                    format!("Could not configure the GUI workspace: {error}"),
                )
                .await;
            if is_gui_disconnected(&error) {
                self.clear_gui_connection(connection.id).await;
                return false;
            }
        }
        true
    }
}

#[derive(Debug, Clone)]
struct Backend {
    state: State,
}

#[derive(Deserialize)]
struct WorkspaceModifiedParams {
    modified: bool,
}

#[derive(Deserialize)]
struct SetConfigParams {
    key: String,
    value: Option<String>,
}

#[derive(Deserialize)]
struct SaveConfigParams {
    path: Option<PathBuf>,
}

impl Backend {
    async fn activate_config(&self, config: ArgonConfig) -> std::result::Result<(), String> {
        reload_log_filter(&config.log.level)
            .map_err(|error| format!("invalid log level '{}': {error}", config.log.level))?;
        self.state.apply_config(config.clone());
        let Some(connection) = self.state.gui_connection().await else {
            return Ok(());
        };
        if let Err(error) = connection
            .client
            .configure(context::current(), config)
            .await
        {
            if is_gui_disconnected(&error) {
                self.state.clear_gui_connection(connection.id).await;
            }
            return Err(format!("could not configure GUI: {error}"));
        }
        Ok(())
    }

    fn compile_after_debounce(&self, identity: CompileIdentity) {
        let delay = self.state.config().analyzer.compile_debounce_ms;
        self.compile_after(identity, Duration::from_millis(delay));
    }

    /// Compiles `identity` after `delay`, unless the source or cell changes
    /// first.
    fn compile_after(&self, identity: CompileIdentity, delay: Duration) {
        let backend = self.clone();
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            if backend.state.is_latest_compile_request(&identity).await {
                backend.update_cell(identity).await;
            }
        });
    }

    /// Compiles `identity` after an editor document changed: at once when the
    /// change completes an analyzer edit, and after the debounce otherwise. A
    /// partly reported edit waits for the rest, up to `PENDING_EDIT_TIMEOUT`.
    ///
    /// The compilation runs on its own task, so a didChange notification
    /// releases its LSP service slot as soon as the source is recorded; the
    /// GUI update can be much slower than the edit.
    fn compile_after_change(&self, progress: EditProgress, identity: CompileIdentity) {
        match progress {
            EditProgress::Complete => self.compile_after(identity, Duration::ZERO),
            EditProgress::Partial => self.compile_after(identity, PENDING_EDIT_TIMEOUT),
            EditProgress::Unrelated => self.compile_after_debounce(identity),
        }
    }

    /// Compile and publish diagnostics for an exact source/cell identity.
    /// GUI presentation is deliberately handled by the caller.
    async fn compile_snapshot(
        &self,
        identity: CompileIdentity,
        preview_cell: Option<compile::CellId>,
    ) -> Option<CompilationSnapshot> {
        let (preview, result) = {
            let source = self.state.source_state.lock().await;
            if source.compile_identity() != identity {
                return None;
            }
            let root_dir = self.state.root_dir.get().cloned()?;
            self.state.compiler.compile_streamed(CompileRequest {
                identity: identity.clone(),
                root_dir,
                preview_cell,
                publish: true,
            })
        };
        if let Some(preview) = preview {
            let backend = self.clone();
            tokio::spawn(async move {
                if let Ok(preview) = preview.await {
                    backend.send_cell_preview(preview).await;
                }
            });
        }
        let result = result.await.ok()?;
        if !self.state.is_latest_compile_request(&result.identity).await {
            return None;
        }
        self.publish_compilation(result).await
    }

    async fn publish_compilation(&self, mut result: CompileResult) -> Option<CompilationSnapshot> {
        let identity = result.identity.clone();
        if !self.state.is_latest_compile_request(&identity).await {
            return None;
        }
        let mut current_diagnostics = diagnostics(
            &result.root_dir,
            &result.ast,
            result.output.as_ref(),
            self.state.position_encoding(),
        );
        // Cells are shared, so this copy costs one pointer per cell.
        let latest = result.output.clone().map(|output| agent::LatestCompile {
            cell: identity.cell.clone(),
            output,
        });
        // The compiled GDS can contain millions of geometry values. Move it
        // into the RPC snapshot after diagnostics have borrowed it instead of
        // deep-cloning the complete layout immediately before serialization.
        let snapshot = result.output.take().map(|output| CompilationSnapshot {
            revision: identity.revision,
            output,
        });
        let recent = {
            // The freshness check and commit are atomic with respect to source
            // and cell changes because they take this same lock.
            let mut source = self.state.source_state.lock().await;
            let mut compiled = self.state.published_state.lock().await;
            if source.compile_identity() != identity {
                return None;
            }
            let previous =
                std::mem::replace(&mut compiled.prev_diagnostics, current_diagnostics.clone());
            for uri in previous.into_keys() {
                current_diagnostics.entry(uri).or_default();
            }
            compiled.config = result.config;
            compiled.ast = Arc::new(result.ast);
            // The compiler only ever answers `None` before the workspace has
            // type-checked once, or on a path that never asked it (a manifest
            // that would not load, an internal compiler error). None of those
            // is a reason to throw away an index that is still good.
            if let Some(nav) = result.nav {
                compiled.nav = Some(nav);
            }
            compiled.compiled_revision = identity.revision;
            compiled.compiled_cell = identity.cell.clone();
            compiled.messages = result.messages.clone();
            compiled.latest = latest;
            let drawable = snapshot
                .as_ref()
                .is_some_and(|snapshot| snapshot.data().is_some());
            if drawable && source.pending_recent == identity.cell {
                source.pending_recent.take()
            } else {
                None
            }
        };
        self.state
            .compiled_revisions
            .send_replace(identity.revision);
        self.state
            .publish_cells(self.state.gui_connection().await)
            .await;
        if let Some(invocation) = recent {
            self.state
                .update_recents(move |root| recents::record(root, &invocation))
                .await;
        }

        for message in result.messages {
            if !self.state.is_latest_compile_request(&identity).await {
                return None;
            }
            self.state.report_message(MessageType::ERROR, message).await;
        }
        for (uri, diagnostics) in current_diagnostics {
            if !self.state.is_latest_compile_request(&identity).await {
                return None;
            }
            self.state
                .editor_client
                .publish_diagnostics(uri, diagnostics, None)
                .await;
        }
        let snapshot = snapshot?;
        if !self.state.is_latest_compile_request(&identity).await {
            return None;
        }
        Some(snapshot)
    }

    async fn send_cell_update(
        &self,
        identity: &CompileIdentity,
        snapshot: CompilationSnapshot,
        progress: &OngoingProgress<Unbounded, NotCancellable>,
        pipeline_started: Instant,
    ) -> Option<GuiConnection> {
        if !self.state.is_latest_compile_request(identity).await {
            return None;
        }
        let connection = self.state.gui_connection().await?;
        if !self.state.is_latest_compile_request(identity).await {
            return None;
        }
        let geometry = match &snapshot.output {
            CompileOutput::Valid(data) => Some(data),
            CompileOutput::ExecErrors(output) => output.output.as_ref(),
            CompileOutput::FatalParseErrors | CompileOutput::StaticErrors(_) => None,
        };
        let detail = geometry.map(|data| {
            let scopes: usize = data.cells.values().map(|cell| cell.scopes.len()).sum();
            let objects: usize = data.cells.values().map(|cell| cell.objects.len()).sum();
            format!(" ({} scopes, {} objects)", scopes, objects)
        });
        progress
            .report(format!(
                "Compressing layout snapshot{}; compilation took {:.1}s",
                detail.as_deref().unwrap_or_default(),
                pipeline_started.elapsed().as_secs_f64(),
            ))
            .await;
        // Serialize updates per connection so each delta names an acknowledged
        // base. Reconnection creates a fresh cache; IDs alone never prove reuse.
        let mut previous = connection.snapshot.lock().await;
        if !self.state.is_latest_compile_request(identity).await {
            return None;
        }
        let update = rpc::CompilationUpdate::new(snapshot.clone(), previous.as_ref());
        let compression_started = Instant::now();
        let update = self.compress_gui_update(update).await?;
        // A rapid source burst may supersede this snapshot while its large
        // delta is being compressed. Do not occupy the serialized GUI RPC
        // lane with a revision that can no longer be displayed.
        if !self.state.is_latest_compile_request(identity).await {
            return None;
        }
        let compressed_mib = update.encoded_len() as f64 / (1024. * 1024.);
        progress
            .report(format!(
                "Transferring {:.1} MiB and preparing layout in GUI; compression took {:.1}s",
                compressed_mib,
                compression_started.elapsed().as_secs_f64(),
            ))
            .await;
        let gui_started = Instant::now();
        let result = self
            .await_gui_update(&connection, update, progress, gui_started)
            .await?;
        if result.accepted {
            let gui_seconds = gui_started.elapsed().as_secs_f64();
            let other_seconds = (gui_seconds
                - result.decode_seconds
                - result.materialize_seconds
                - result.prepare_seconds)
                .max(0.);
            progress
                .report(format!(
                    "Layout accepted in {gui_seconds:.1}s: transfer/dispatch {other_seconds:.1}s, decode {:.1}s, materialize {:.1}s, hierarchy {:.1}s ({:.1}s total)",
                    result.decode_seconds,
                    result.materialize_seconds,
                    result.prepare_seconds,
                    pipeline_started.elapsed().as_secs_f64(),
                ))
                .await;
        } else {
            progress
                .report("GUI requested a full snapshot; recompressing layout")
                .await;
            let retry_started = Instant::now();
            let full = rpc::CompilationUpdate::new(snapshot.clone(), None);
            let full = self.compress_gui_update(full).await?;
            progress
                .report(format!(
                    "Transferring {:.1} MiB full layout and preparing it in GUI",
                    full.encoded_len() as f64 / (1024. * 1024.),
                ))
                .await;
            let result = self
                .await_gui_update(&connection, full, progress, retry_started)
                .await?;
            if !result.accepted {
                return None;
            }
            progress
                .report(format!(
                    "Layout accepted; full retry {:.1}s (decode {:.1}s, materialize {:.1}s, hierarchy {:.1}s; {:.1}s total)",
                    retry_started.elapsed().as_secs_f64(),
                    result.decode_seconds,
                    result.materialize_seconds,
                    result.prepare_seconds,
                    pipeline_started.elapsed().as_secs_f64(),
                ))
                .await;
        }
        *previous = Some(snapshot);
        drop(previous);
        Some(connection)
    }

    async fn await_gui_update(
        &self,
        connection: &GuiConnection,
        update: CompressedCompilationUpdate,
        progress: &OngoingProgress<Unbounded, NotCancellable>,
        started: Instant,
    ) -> Option<rpc::GuiUpdateResult> {
        let mut request = Box::pin(
            connection
                .client
                .update_cell(gui_snapshot_context(), update),
        );
        let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        heartbeat.tick().await;
        let result = loop {
            tokio::select! {
                result = &mut request => break result,
                _ = heartbeat.tick() => {
                    progress.report(format!(
                        "GUI is receiving/decompressing/preparing layout ({:.0}s elapsed)",
                        started.elapsed().as_secs_f64(),
                    )).await;
                }
            }
        };
        self.handle_gui_result(connection, result).await
    }

    async fn compress_gui_update(
        &self,
        update: rpc::CompilationUpdate,
    ) -> Option<CompressedCompilationUpdate> {
        match tokio::task::spawn_blocking(move || CompressedCompilationUpdate::encode(&update))
            .await
        {
            Ok(Ok(update)) => Some(update),
            result => {
                let error = match result {
                    Ok(Err(error)) => error,
                    Err(error) => error.to_string(),
                    Ok(Ok(_)) => unreachable!(),
                };
                self.state
                    .editor_client
                    .show_message(
                        MessageType::ERROR,
                        format!("Could not encode the GUI snapshot: {error}"),
                    )
                    .await;
                None
            }
        }
    }

    async fn handle_gui_result<T>(
        &self,
        connection: &GuiConnection,
        result: std::result::Result<T, tarpc::client::RpcError>,
    ) -> Option<T> {
        match result {
            Ok(value) => Some(value),
            Err(error) => {
                self.state
                    .editor_client
                    .show_message(
                        MessageType::ERROR,
                        format!("Could not contact the GUI: {error}"),
                    )
                    .await;
                if is_gui_disconnected(&error) {
                    self.state.clear_gui_connection(connection.id).await;
                }
                None
            }
        }
    }

    async fn update_cell(&self, identity: CompileIdentity) -> Option<GuiConnection> {
        let pipeline_started = Instant::now();
        let activity = self.state.begin_compilation(&identity).await;
        let preview_cell = if identity.cell.is_some()
            && self.state.published_state.lock().await.compiled_cell == identity.cell
        {
            if let Some(connection) = self.state.gui_connection().await {
                // The query is only an optimization. A busy GUI must never
                // delay the mandatory top-level compilation for an RPC's
                // ordinary ten-second deadline.
                tokio::time::timeout(
                    Duration::from_millis(200),
                    connection.client.selected_cell(context::current()),
                )
                .await
                .ok()
                .and_then(|result| result.ok())
                .flatten()
            } else {
                None
            }
        } else {
            None
        };
        let result = async {
            // The compiler runs on its dedicated worker thread, so the async
            // language-server task remains free to publish a heartbeat. Large
            // cells used to leave Neovim showing an undifferentiated spinner
            // for tens of seconds, making healthy evaluation indistinguishable
            // from a wedged worker.
            let mut compilation = Box::pin(self.compile_snapshot(identity.clone(), preview_cell));
            let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
            heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            // `interval`'s first tick is immediate; the begin notification
            // already describes that instant.
            heartbeat.tick().await;
            let snapshot = loop {
                tokio::select! {
                    snapshot = &mut compilation => break snapshot?,
                    _ = heartbeat.tick() => {
                        activity.editor_progress.report(format!(
                            "{} ({:.0}s elapsed)",
                            compilation_message(&identity),
                            pipeline_started.elapsed().as_secs_f64(),
                        )).await;
                    }
                }
            };
            self.send_cell_update(
                &identity,
                snapshot,
                &activity.editor_progress,
                pipeline_started,
            )
            .await
        }
        .await;
        self.state.finish_compilation(activity).await;
        result
    }

    async fn send_cell_preview(&self, preview: CompilePreview) {
        if !self
            .state
            .is_latest_compile_request(&preview.identity)
            .await
        {
            return;
        }
        if !matches!(
            preview.output,
            CompileOutput::Valid(_) | CompileOutput::ExecErrors(_)
        ) {
            return;
        }
        let Some(connection) = self.state.gui_connection().await else {
            return;
        };
        // Cells the GUI already holds are sent by reference. The lock keeps an
        // in-flight update from replacing that base while the delta is built;
        // if one lands before the preview arrives, the GUI rejects the preview.
        let update = rpc::CompilationUpdate::new(
            CompilationSnapshot {
                revision: preview.identity.revision,
                output: preview.output,
            },
            connection.snapshot.lock().await.as_ref(),
        );
        let Some(update) = self.compress_gui_update(update).await else {
            return;
        };
        if !self
            .state
            .is_latest_compile_request(&preview.identity)
            .await
        {
            return;
        }
        let request = FocusedCellPreview {
            previous_cell: preview.previous_cell,
            update,
        };
        if let Err(error) = connection
            .client
            .preview_cell(gui_snapshot_context(), request)
            .await
            && is_gui_disconnected(&error)
        {
            self.state.clear_gui_connection(connection.id).await;
        }
    }

    async fn open_cell_view(&self, identity: CompileIdentity) {
        let Some(connection) = self.update_cell(identity.clone()).await else {
            return;
        };
        // An edit made once the layout arrived, such as a GUI drawing, leaves
        // the cell open; only opening another cell cancels the fit.
        if !self.state.is_open_cell(&identity).await {
            return;
        }
        let result = connection.client.fit(context::current()).await;
        if self.handle_gui_result(&connection, result).await.is_none() {
            return;
        }
        if self.state.is_open_cell(&identity).await {
            let result = connection.client.activate(context::current()).await;
            self.handle_gui_result(&connection, result).await;
        }
    }

    async fn workspace_modified(&self, params: WorkspaceModifiedParams) -> Result<()> {
        let gui_client = {
            let mut source = self.state.source_state.lock().await;
            if source.workspace_modified == params.modified {
                return Ok(());
            }
            source.workspace_modified = params.modified;
            drop(source);
            self.state.gui_connection().await
        };
        self.state
            .publish_workspace_modified(params.modified, gui_client)
            .await;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy)]
struct Undo;

impl Request for Undo {
    type Params = rpc::HistoryParams;
    type Result = ();

    const METHOD: &'static str = "custom/undo";
}

#[derive(Debug, Clone, Copy)]
struct Redo;

impl Request for Redo {
    type Params = rpc::HistoryParams;
    type Result = ();

    const METHOD: &'static str = "custom/redo";
}

#[derive(Debug, Clone, Copy)]
struct Save;

impl Request for Save {
    type Params = ();
    type Result = ();

    const METHOD: &'static str = "custom/save";
}

#[derive(Debug, Clone, Copy)]
struct FocusEditor;

impl Notification for FocusEditor {
    type Params = rpc::FocusEditorParams;

    const METHOD: &'static str = "custom/focusEditor";
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        let encoding = PositionEncoding::negotiate(
            params
                .capabilities
                .general
                .as_ref()
                .and_then(|general| general.position_encodings.as_deref()),
        );
        let _ = self.state.position_encoding.set(encoding);
        let root_uri = params
            .workspace_folders
            .and_then(|folders| folders.into_iter().next())
            .map(|folder| folder.uri);
        #[expect(
            deprecated,
            reason = "root_uri remains necessary for LSP clients without workspaceFolders support"
        )]
        let root_uri = root_uri.or(params.root_uri);
        let root_dir = root_uri.and_then(|root| root.to_file_path().map(|path| path.into_owned()));
        if let Some(root_dir) = root_dir {
            let _ = self.state.root_dir.set(root_dir.clone());
            self.state.update_recents(recents::load).await;
            self.state.publish_session(&root_dir);
            // A recursive watch walks the whole tree on some platforms.
            let backend = self.clone();
            let watched = root_dir.clone();
            tokio::task::spawn_blocking(move || backend.start_watcher(&watched));
            let connection = self.state.gui_connection().await;
            self.state.publish_workspace_path(connection).await;
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                position_encoding: Some(encoding.kind()),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                completion_provider: Some(CompletionOptions {
                    trigger_characters: Some(completion_trigger_characters()),
                    resolve_provider: Some(false),
                    ..Default::default()
                }),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                signature_help_provider: Some(SignatureHelpOptions {
                    trigger_characters: Some(vec!["(".to_owned(), ",".to_owned(), "=".to_owned()]),
                    retrigger_characters: Some(vec![",".to_owned()]),
                    ..Default::default()
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                document_highlight_provider: Some(OneOf::Left(true)),
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::INCREMENTAL),
                        save: Some(TextDocumentSyncSaveOptions::Supported(true)),
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
        self.update_current().await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let mut source = self.state.source_state.lock().await;
        let doc = Document::new(
            params.text_document.text,
            params.text_document.version,
            self.state.position_encoding(),
        );
        let uri = params.text_document.uri;
        if let Some(path) = uri.to_file_path().map(|path| path.into_owned()) {
            self.state
                .compiler
                .set_source_text(path, doc.contents().to_owned());
        }
        source.editor_files.insert(uri.clone(), doc);
        source.advance_revision();
        let progress = source.edit_progress(&uri);
        let identity = source.compile_identity();
        drop(source);
        self.state.notify_source_changed();
        self.compile_after_change(progress, identity);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let mut source = self.state.source_state.lock().await;
        let mut changed_source = None;
        if let Some(doc) = source.editor_files.get_mut(&params.text_document.uri) {
            let previous = doc.contents().to_owned();
            // apply each change
            doc.apply_changes(
                params
                    .content_changes
                    .into_iter()
                    .map(|change| DocumentChange {
                        range: change.range,
                        patch: change.text,
                    })
                    .collect(),
                params.text_document.version,
            );
            let contents = doc.contents().to_owned();
            if contents != previous
                && let Some(path) = params
                    .text_document
                    .uri
                    .to_file_path()
                    .map(|path| path.into_owned())
            {
                changed_source = Some((path, contents));
            }
        } else {
            // optional: log error, or handle missing document
        }
        if let Some((path, contents)) = changed_source {
            self.state.compiler.set_source_text(path, contents);
            source.advance_revision();
        }
        let progress = source.edit_progress(&params.text_document.uri);
        let identity = source.compile_identity();
        drop(source);
        self.state.notify_source_changed();
        self.compile_after_change(progress, identity);
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let position = params.text_document_position_params;
        Ok(self
            .state
            .definition(&position.text_document.uri, position.position)
            .await
            .map(GotoDefinitionResponse::Scalar))
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let position = params.text_document_position;
        Ok(self
            .state
            .references(
                &position.text_document.uri,
                position.position,
                params.context.include_declaration,
            )
            .await)
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let position = params.text_document_position;
        Ok(self
            .state
            .completion(&position.text_document.uri, position.position)
            .await)
    }

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let position = params.text_document_position_params;
        Ok(self
            .state
            .hover(&position.text_document.uri, position.position)
            .await)
    }

    async fn signature_help(&self, params: SignatureHelpParams) -> Result<Option<SignatureHelp>> {
        let position = params.text_document_position_params;
        Ok(self
            .state
            .signature_help(&position.text_document.uri, position.position)
            .await)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        Ok(self
            .state
            .document_symbols(&params.text_document.uri)
            .await
            .map(DocumentSymbolResponse::Nested))
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        let position = params.text_document_position_params;
        Ok(self
            .state
            .document_highlights(&position.text_document.uri, position.position)
            .await)
    }

    async fn did_save(&self, _: DidSaveTextDocumentParams) {
        let revision = self.state.source_state.lock().await.revision;
        let compiled_revision = self.state.published_state.lock().await.compiled_revision;
        // Saving persists the buffer; it only needs to compile when the latest
        // source revision has not already produced a committed result.
        if revision != compiled_revision {
            self.update_current().await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let mut source = self.state.source_state.lock().await;
        source.editor_files.swap_remove(&params.text_document.uri);
        if let Some(path) = params
            .text_document
            .uri
            .to_file_path()
            .map(|path| path.into_owned())
        {
            self.state.compiler.remove_source(path);
        }
        source.advance_revision();
        let identity = source.compile_identity();
        drop(source);
        self.state.notify_source_changed();
        self.compile_after_debounce(identity);
    }

    async fn shutdown(&self) -> Result<()> {
        self.state.retire_session();
        if let Some(mut gui) = self.state.take_gui_process().await {
            let _ = gui.kill().await;
        }
        Ok(())
    }
}

#[derive(Serialize, Deserialize)]
struct OpenCellParams {
    cell: String,
}

#[derive(Serialize, Deserialize)]
struct InstantiateParams {
    cell: String,
}

#[derive(Serialize, Deserialize)]
struct DeviceCommandParams {
    /// The command's argument text: `<Kind> [<count>] <model and parameters>`.
    args: String,
}

#[derive(Serialize, Deserialize)]
struct CellNameParams {
    name: String,
    uri: Uri,
}

const PREVIEW_BINDING_PREFIX: &str = "__argon_preview_instance";

/// The instance the schematic preview placed at `call`, with what the GUI
/// draws for it.
fn schematic_instance_preview(
    output: &compile::CompiledData,
    call: &Span,
    invocation: String,
) -> Option<SchematicInstancePreview> {
    let instance = output.cells.values().find_map(|cell| {
        cell.schematic
            .instances
            .iter()
            .find(|instance| instance.span.as_ref() == Some(call))
    })?;
    let child = output.cells.get(&instance.cell)?;
    let symbol = child
        .symbol
        .clone()
        .unwrap_or_else(|| compile::generated_box(&child.schematic, &child.name));
    let pin_names = (0..child.schematic.ports.len())
        .map(|port| child.schematic.port_name(port).to_owned())
        .collect();
    Some(SchematicInstancePreview {
        invocation,
        cell_name: child.name.clone(),
        symbol,
        pin_names,
    })
}

/// Reads `<Kind> [<count>] <model and parameters>`: the kind by its
/// variant name in any case, the terminal count when the kind needs one,
/// and the rest as argument text.
fn parse_device_command(text: &str) -> std::result::Result<(DeviceKind, usize, String), String> {
    let text = text.trim();
    let (kind_name, rest) = text.split_once(char::is_whitespace).unwrap_or((text, ""));
    if kind_name.is_empty() {
        return Err("expected a device kind, such as `Nmos`".to_owned());
    }
    let kind = DeviceKind::ALL
        .into_iter()
        .find(|kind| kind.name().eq_ignore_ascii_case(kind_name))
        .ok_or_else(|| {
            format!(
                "`{kind_name}` is not a device kind; expected one of {}",
                DeviceKind::ALL
                    .iter()
                    .map(|kind| kind.name())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    let rest = rest.trim_start();
    let (count, args) = match rest.split_once(char::is_whitespace).unwrap_or((rest, "")) {
        (first, args) if first.parse::<usize>().is_ok() => {
            (Some(first.parse::<usize>().expect("checked")), args)
        }
        _ => (None, rest),
    };
    let terminals = match (count, kind.base()) {
        (Some(count), _) => count,
        (None, DeviceKind::Mos) => 4,
        (None, DeviceKind::Res | DeviceKind::Cap | DeviceKind::Diode) => 2,
        (None, DeviceKind::Bjt) => 3,
        (None, _) => {
            return Err(format!(
                "a `{}` device needs a terminal count: `:Argon device {} 3 \"model\"`",
                kind.name(),
                kind.name()
            ));
        }
    };
    kind.check_terminals(terminals)
        .map_err(|error| format!("a `{}` device {error}", kind.name()))?;
    let args = schematic_edit::device_args(args);
    let call = format!(
        "std::schematic::device(std::schematic::DeviceKind::{}, std::schematic::signals({terminals}), {args})",
        kind.name()
    );
    parse::parse_cell(&call).map_err(|error| format!("the arguments do not parse: {error}"))?;
    Ok((kind, terminals, args))
}

fn preview_instance_cell(
    output: &compile::CompiledData,
    preview_binding: &str,
) -> Option<compile::CellId> {
    output.cells.values().find_map(|cell| {
        cell.scopes.values().find_map(|scope| {
            scope.bindings.iter().find_map(|(_, (name, objects))| {
                if name != preview_binding {
                    return None;
                }
                let Arrayed::Elem(object) = objects else {
                    return None;
                };
                cell.objects[object]
                    .get_instance()
                    .map(|instance| instance.cell)
            })
        })
    })
}

impl Backend {
    /// The RPC address and session token, for an editor that hosts the GUI
    /// itself and relays its connections.
    async fn gui_endpoint(&self) -> Result<GuiEndpoint> {
        Ok(GuiEndpoint {
            port: self.state.server_addr.port(),
            token: self.state.token.to_hex(),
        })
    }

    async fn start_gui(&self) -> Result<()> {
        if let Some(connection) = self.state.gui_connection().await {
            self.state
                .editor_client
                .log_message(MessageType::INFO, "Attempting to contact existing GUI...")
                .await;
            match connection.client.activate(context::current()).await {
                Ok(()) => {
                    self.state
                        .editor_client
                        .log_message(MessageType::INFO, "Connected to existing GUI!")
                        .await;
                    return Ok(());
                }
                Err(error) if is_gui_disconnected(&error) => {
                    self.state.clear_gui_connection(connection.id).await;
                    self.state
                        .editor_client
                        .log_message(MessageType::INFO, "Failed to contact existing GUI.")
                        .await;
                }
                Err(error) => {
                    self.state
                        .editor_client
                        .show_message(
                            MessageType::ERROR,
                            format!("Could not activate the GUI: {error}"),
                        )
                        .await;
                    return Ok(());
                }
            }
        }
        if self.state.embedded {
            self.state
                .editor_client
                .log_message(MessageType::INFO, "The GUI is still connecting")
                .await;
            return Ok(());
        }
        let old_gui = self.state.take_gui_process().await;
        if let Some(mut gui) = old_gui {
            let _ = gui.kill().await;
        }

        self.state
            .editor_client
            .log_message(MessageType::INFO, "Starting the GUI...")
            .await;
        let state = self.state.clone();

        tokio::spawn(async move {
            match Command::new("argone")
                .arg("gui")
                .arg(format!("{}", state.server_addr))
                .env(transport::TOKEN_ENV, state.token.to_hex())
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
            {
                Ok(mut child) => {
                    if let Some(stdout) = child.stdout.take() {
                        tokio::spawn(async move {
                            let reader = BufReader::new(stdout);
                            let mut lines = reader.lines();

                            while let Ok(Some(line)) = lines.next_line().await {
                                info!("{}", line);
                            }
                        });
                    }
                    if let Some(stderr) = child.stderr.take() {
                        let stderr_state = state.clone();
                        tokio::spawn(async move {
                            let reader = BufReader::new(stderr);
                            let mut lines = reader.lines();

                            while let Ok(Some(line)) = lines.next_line().await {
                                error!("{}", line);
                                stderr_state
                                    .editor_client
                                    .show_message(MessageType::ERROR, line)
                                    .await;
                            }
                        });
                    }
                    state.set_gui_process(child).await;
                }
                Err(err) => {
                    let message =
                        format!("failed to start argone: {err}. Is it installed and on PATH?");
                    error!("{message}");
                    state
                        .editor_client
                        .show_message(MessageType::ERROR, message)
                        .await;
                }
            }
        });

        Ok(())
    }

    async fn select_and_open_cell(&self, cell: impl Into<String>) {
        let cell = cell.into();
        let identity = {
            let mut source = self.state.source_state.lock().await;
            source.pending_recent = Some(cell.clone());
            source.cell = Some(cell);
            source.compile_identity()
        };
        self.open_cell_view(identity).await;
    }

    async fn update_current(&self) {
        let identity = self.state.source_state.lock().await.compile_identity();
        self.update_cell(identity).await;
    }

    async fn open_current(&self) {
        let identity = self.state.source_state.lock().await.compile_identity();
        self.open_cell_view(identity).await;
    }

    async fn command_completion(
        &self,
        params: CommandCompletionParams,
    ) -> Result<CommandCompletion> {
        Ok(self.state.command_completion(params).await)
    }

    async fn open_cell(&self, params: OpenCellParams) -> Result<()> {
        let state = self.state.clone();
        state
            .editor_client
            .show_message(MessageType::LOG, &format!("cell {}", params.cell))
            .await;
        self.select_and_open_cell(params.cell).await;
        Ok(())
    }

    async fn new_cell(&self, params: CellNameParams) -> Result<()> {
        let Some(source_path) = params.uri.to_file_path().map(|path| path.into_owned()) else {
            self.state
                .report_message(
                    MessageType::ERROR,
                    "The active buffer does not have a file path",
                )
                .await;
            return Ok(());
        };
        let Some(ast) = self.state.current_editor_ast().await else {
            self.state
                .report_message(MessageType::ERROR, rpc::OUT_OF_SYNC_MESSAGE)
                .await;
            return Ok(());
        };
        let edit = match cell_edit::new_cell_edit(
            &ast,
            &source_path,
            &params.name,
            self.state.position_encoding(),
        ) {
            Ok(edit) => edit,
            Err(error) => {
                self.state.report_message(MessageType::ERROR, error).await;
                return Ok(());
            }
        };
        let invocation = format!("{}()", params.name);
        let previous = {
            let mut source = self.state.source_state.lock().await;
            (
                source.cell.replace(invocation.clone()),
                source.pending_recent.replace(invocation),
            )
        };
        if !self.state.apply_source_edit(params.uri, edit).await {
            let mut source = self.state.source_state.lock().await;
            (source.cell, source.pending_recent) = previous;
        }
        Ok(())
    }

    async fn rename_cell(&self, params: CellNameParams) -> Result<()> {
        let Some(ast) = self.state.current_editor_ast().await else {
            self.state
                .report_message(MessageType::ERROR, rpc::OUT_OF_SYNC_MESSAGE)
                .await;
            return Ok(());
        };
        let current_invocation = self.state.source_state.lock().await.cell.clone();
        let Some(current_invocation) = current_invocation else {
            self.state
                .report_message(MessageType::ERROR, "Open a cell before renaming it")
                .await;
            return Ok(());
        };
        let rename = {
            let invocation = current_invocation.clone();
            let name = params.name.clone();
            let encoding = self.state.position_encoding();
            on_compile_stack(move || {
                cell_edit::rename_cell_edits(&ast, &invocation, &name, encoding)
            })
            .await
        };
        let mut rename = match rename {
            Some(Ok(rename)) => rename,
            Some(Err(error)) => {
                self.state.report_message(MessageType::ERROR, error).await;
                return Ok(());
            }
            None => {
                self.state
                    .report_message(
                        MessageType::ERROR,
                        format!("Could not rename the cell: {INTERNAL_COMPILER_ERROR_MESSAGE}"),
                    )
                    .await;
                return Ok(());
            }
        };
        let renamed = |invocation: &str| {
            rename
                .rename_invocation(invocation)
                .unwrap_or_else(|| invocation.to_owned())
        };
        let previous = {
            let mut source = self.state.source_state.lock().await;
            let pending_recent = source.pending_recent.as_deref().map(renamed);
            (
                source.cell.replace(renamed(&current_invocation)),
                std::mem::replace(&mut source.pending_recent, pending_recent),
            )
        };
        let changes = std::mem::take(&mut rename.changes);
        if !self.state.apply_source_changes(changes, None).await {
            let mut source = self.state.source_state.lock().await;
            (source.cell, source.pending_recent) = previous;
            return Ok(());
        }
        self.state
            .update_recents(move |root| {
                recents::rewrite(root, |invocation| rename.rename_invocation(invocation))
            })
            .await;
        Ok(())
    }

    /// Compiles the open cell with `expression` bound to a fresh name at the
    /// end of `scope_span`, which must be the body of a cell or a scope in
    /// one. Returns the output, the binding, and the span of the expression
    /// in the preview source, or reports why the preview failed.
    async fn compile_preview(
        &self,
        workspace_ast: &Arc<WorkspaceParseAst>,
        workspace: &WorkspaceConfig,
        open_cell: Option<String>,
        scope_span: &Span,
        expression: &str,
        describe: &str,
    ) -> Option<(compile::CompiledData, String, cfgrammar::Span)> {
        let Some((module_path, ast)) = workspace_ast
            .iter()
            .find(|(_, ast)| ast.path == scope_span.path)
            .map(|(module_path, ast)| (module_path.clone(), ast.clone()))
        else {
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    "The selected scope is not part of this Argon workspace",
                )
                .await;
            return None;
        };
        let Some(scope) = ast.span2scope.get(scope_span) else {
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    "The scope selected in the Argon GUI is not part of the current workspace",
                )
                .await;
            return None;
        };
        let document = self.state.document(&ast.source_text);
        let preview_binding = (0..)
            .map(|index| format!("{PREVIEW_BINDING_PREFIX}{index}"))
            .find(|name| {
                workspace_ast
                    .values()
                    .all(|ast| !ast.text.contains(name.as_str()))
            })
            .expect("an unused preview binding always exists");
        let statement = format!("let {preview_binding} = {expression};");
        let tracked = statement.len() - expression.len() - 1..statement.len() - 1;
        let insertion = insert_statement(
            &document,
            scope.span,
            scope.tail.as_ref().map(|tail| tail.span().start()),
            &statement,
            tracked,
        );
        let mut preview_source = ast.text.to_string();
        preview_source.insert_str(insertion.offset, &insertion.edit.new_text);
        let Some(open_cell) = open_cell else {
            self.state
                .editor_client
                .show_message(MessageType::ERROR, "Open a cell before placing an instance")
                .await;
            return None;
        };
        let compiled = {
            let workspace_ast = workspace_ast.clone();
            let workspace = workspace.clone();
            let describe = describe.to_owned();
            on_compile_stack(move || {
                compile_preview_workspace(
                    &workspace_ast,
                    module_path,
                    &ast,
                    preview_source,
                    &open_cell,
                    &workspace,
                    &describe,
                )
            })
            .await
        };
        let output = match compiled {
            Some(Ok(output)) => output,
            Some(Err(message)) => {
                self.state
                    .editor_client
                    .show_message(MessageType::ERROR, message)
                    .await;
                return None;
            }
            None => {
                self.state
                    .editor_client
                    .show_message(
                        MessageType::ERROR,
                        format!("Could not {describe}: {INTERNAL_COMPILER_ERROR_MESSAGE}"),
                    )
                    .await;
                return None;
            }
        };
        Some((output, preview_binding, insertion.tracked_span))
    }

    /// The body of the cell the schematic view shows, from the GUI.
    async fn schematic_target(&self, connection: &GuiConnection, action: &str) -> Option<Span> {
        match connection.client.schematic_target(context::current()).await {
            Ok(Some(target)) => Some(target),
            Ok(None) => {
                self.state
                    .editor_client
                    .show_message(
                        MessageType::ERROR,
                        format!("Open a cell in the Argon GUI before {action}"),
                    )
                    .await;
                None
            }
            Err(error) => {
                let disconnected = is_gui_disconnected(&error);
                if disconnected {
                    self.state.clear_gui_connection(connection.id).await;
                }
                self.state
                    .editor_client
                    .show_message(
                        MessageType::ERROR,
                        if disconnected {
                            format!(
                                "The Argon GUI is not connected; start it before {action} ({error})"
                            )
                        } else {
                            format!("Could not query the GUI's schematic: {error}")
                        },
                    )
                    .await;
                None
            }
        }
    }

    /// The published workspace, open cell, and configuration, when the editor
    /// buffers match what was compiled.
    async fn preview_sources(
        &self,
    ) -> Option<(Arc<WorkspaceParseAst>, Option<String>, WorkspaceConfig)> {
        let source = self.state.source_state.lock().await;
        let compiled = self.state.published_state.lock().await;
        if !rpc::editor_buffers_are_current(&source, &compiled) {
            drop(compiled);
            drop(source);
            self.state
                .editor_client
                .show_message(MessageType::ERROR, rpc::OUT_OF_SYNC_MESSAGE)
                .await;
            return None;
        }
        Some((
            compiled.ast.clone(),
            source.cell.clone(),
            compiled.config.clone(),
        ))
    }

    /// `:Argon sinst <invocation>`: previews a schematic instance of a cell
    /// in the cell on screen.
    async fn schematic_instantiate(&self, params: InstantiateParams) -> Result<()> {
        let Some(connection) = self.state.gui_connection().await else {
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    "Start the Argon GUI before placing an instance",
                )
                .await;
            return Ok(());
        };
        let Some(target) = self
            .schematic_target(&connection, "placing an instance")
            .await
        else {
            return Ok(());
        };
        let Some((workspace_ast, open_cell, workspace)) = self.preview_sources().await else {
            return Ok(());
        };
        let Some((output, _, tracked)) = self
            .compile_preview(
                &workspace_ast,
                &workspace,
                open_cell,
                &target,
                &format!("std::schematic::inst({})", params.cell),
                &format!("instantiate `{}`", params.cell),
            )
            .await
        else {
            return Ok(());
        };
        let tracked = Span {
            path: target.path.clone(),
            span: tracked,
        };
        let Some(preview) = schematic_instance_preview(&output, &tracked, params.cell) else {
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    "The preview instance did not execute while compiling the open cell",
                )
                .await;
            return Ok(());
        };
        let result = connection
            .client
            .place_schematic_instance(context::current(), preview)
            .await;
        self.handle_gui_result(&connection, result).await;
        Ok(())
    }

    /// `:Argon device <Kind> [<count>] <model and parameters>`: previews a
    /// device in the cell on screen.
    async fn device(&self, params: DeviceCommandParams) -> Result<()> {
        let (kind, terminals, args) = match parse_device_command(&params.args) {
            Ok(parsed) => parsed,
            Err(error) => {
                self.state
                    .editor_client
                    .show_message(MessageType::ERROR, format!("Argon device: {error}"))
                    .await;
                return Ok(());
            }
        };
        let Some(connection) = self.state.gui_connection().await else {
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    "Start the Argon GUI before placing a device",
                )
                .await;
            return Ok(());
        };
        let Some(target) = self.schematic_target(&connection, "placing a device").await else {
            return Ok(());
        };
        let Some((workspace_ast, open_cell, workspace)) = self.preview_sources().await else {
            return Ok(());
        };
        let expression = format!(
            "std::schematic::device(std::schematic::DeviceKind::{}, std::schematic::signals({terminals}), {args})",
            kind.name()
        );
        let Some((output, _, tracked)) = self
            .compile_preview(
                &workspace_ast,
                &workspace,
                open_cell,
                &target,
                &expression,
                &format!("place a `{}` device", kind.name()),
            )
            .await
        else {
            return Ok(());
        };
        let tracked = Span {
            path: target.path.clone(),
            span: tracked,
        };
        let evaluated = output.cells.values().any(|cell| {
            cell.schematic
                .devices
                .iter()
                .any(|device| device.span.as_ref() == Some(&tracked))
        });
        if !evaluated {
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    "The preview device did not execute while compiling the open cell",
                )
                .await;
            return Ok(());
        }
        let result = connection
            .client
            .place_device(
                context::current(),
                DevicePreview {
                    kind,
                    terminals,
                    args,
                },
            )
            .await;
        self.handle_gui_result(&connection, result).await;
        Ok(())
    }

    async fn instantiate(&self, params: InstantiateParams) -> Result<()> {
        let Some(connection) = self.state.gui_connection().await else {
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    "Start the Argon GUI before placing an instance",
                )
                .await;
            return Ok(());
        };
        let selected_scope = match connection.client.selected_scope(context::current()).await {
            Ok(Some(scope)) => scope,
            Ok(None) => {
                self.state
                    .editor_client
                    .show_message(
                        MessageType::ERROR,
                        "Select a destination scope in the Argon GUI before placing an instance",
                    )
                    .await;
                return Ok(());
            }
            Err(error) => {
                let disconnected = is_gui_disconnected(&error);
                if disconnected {
                    self.state.clear_gui_connection(connection.id).await;
                }
                self.state
                    .editor_client
                    .show_message(
                        MessageType::ERROR,
                        if disconnected {
                            format!(
                                "The Argon GUI is not connected; start it before placing an instance ({error})"
                            )
                        } else {
                            format!("Could not query the selected GUI scope: {error}")
                        },
                    )
                    .await;
                return Ok(());
            }
        };
        let source = self.state.source_state.lock().await;
        let compiled = self.state.published_state.lock().await;
        if !rpc::editor_buffers_are_current(&source, &compiled) {
            drop(compiled);
            drop(source);
            self.state
                .editor_client
                .show_message(MessageType::ERROR, rpc::OUT_OF_SYNC_MESSAGE)
                .await;
            return Ok(());
        }
        let workspace_ast = compiled.ast.clone();
        let open_cell = source.cell.clone();
        let workspace = compiled.config.clone();
        drop(compiled);
        drop(source);
        let selected_scope = Some(selected_scope).filter(|span| {
            workspace_ast
                .values()
                .find(|ast| ast.path == span.path)
                .is_some_and(|ast| ast.span2scope.contains_key(span))
        });
        let Some(scope_span) = selected_scope else {
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    "The scope selected in the Argon GUI is not part of the current workspace",
                )
                .await;
            return Ok(());
        };
        let Some((output, preview_binding, _)) = self
            .compile_preview(
                &workspace_ast,
                &workspace,
                open_cell,
                &scope_span,
                &format!("std::layout::inst({})", params.cell),
                &format!("instantiate `{}`", params.cell),
            )
            .await
        else {
            return Ok(());
        };
        let Some(cell) = preview_instance_cell(&output, &preview_binding) else {
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    "The preview scope did not execute while compiling the open cell",
                )
                .await;
            return Ok(());
        };
        let preview = InstancePreview {
            geometry: output.preview_geometry(cell),
            invocation: params.cell,
            scope_span,
        };
        let result = connection
            .client
            .place_instance(context::current(), preview)
            .await;
        self.handle_gui_result(&connection, result).await;
        Ok(())
    }

    async fn reload_config(&self) -> Result<()> {
        let config = match read_config() {
            Ok(config) => config,
            Err(error) => {
                self.state
                    .editor_client
                    .show_message(MessageType::ERROR, error)
                    .await;
                return Ok(());
            }
        };
        if let Err(error) = self.activate_config(config).await {
            self.state
                .editor_client
                .show_message(MessageType::ERROR, error)
                .await;
            return Ok(());
        }
        let path = argon_config_path()
            .map(|path| path.display().to_string())
            .unwrap_or_else(|| "the default configuration".to_owned());
        self.state
            .editor_client
            .show_message(
                MessageType::INFO,
                format!("Reloaded Argon configuration from {path}"),
            )
            .await;
        Ok(())
    }

    async fn set_config(&self, params: SetConfigParams) -> Result<()> {
        let config =
            match config_with_key(&self.state.config(), &params.key, params.value.as_deref()) {
                Ok(config) => config,
                Err(error) => {
                    self.state
                        .editor_client
                        .show_message(MessageType::ERROR, error)
                        .await;
                    return Ok(());
                }
            };
        if let Err(error) = self.activate_config(config).await {
            self.state
                .editor_client
                .show_message(MessageType::ERROR, error)
                .await;
            return Ok(());
        }
        let value = params
            .value
            .as_deref()
            .map_or_else(|| "default".to_owned(), str::to_owned);
        self.state
            .editor_client
            .show_message(
                MessageType::INFO,
                format!("Set Argon configuration {} = {value}", params.key),
            )
            .await;
        Ok(())
    }

    async fn save_config(&self, params: SaveConfigParams) -> Result<()> {
        let path = match params.path.or_else(argon_config_path) {
            Some(path) => path,
            None => {
                self.state
                    .editor_client
                    .show_message(
                        MessageType::ERROR,
                        "Could not determine an Argon configuration path",
                    )
                    .await;
                return Ok(());
            }
        };
        if let Err(error) = write_config(&path, &self.state.config()) {
            self.state
                .editor_client
                .show_message(MessageType::ERROR, error)
                .await;
            return Ok(());
        }
        self.state
            .editor_client
            .show_message(
                MessageType::INFO,
                format!("Saved Argon configuration to {}", path.display()),
            )
            .await;
        Ok(())
    }
}

async fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(fut);
}

/// Most authenticated RPC connections served at once.
const MAX_RPC_CONNECTIONS: usize = 10;

#[cfg(unix)]
fn announce_rpc_port(path: &Path, port: u16, token: &transport::SessionToken) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(path)?;
    writeln!(stream, "{port} {}", token.to_hex())?;
    stream.flush()
}

#[cfg(not(unix))]
fn announce_rpc_port(_: &Path, _: u16, _: &transport::SessionToken) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "relay requires a Unix-like remote host",
    ))
}

/// Accepts RPC connections, serving each one the service its role asked for
/// once it has presented the session token.
async fn serve_rpc(listener: tokio::net::TcpListener, state: State) {
    let slots = Arc::new(tokio::sync::Semaphore::new(MAX_RPC_CONNECTIONS));
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(error) => {
                error!("could not accept an RPC connection: {error}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let state = state.clone();
        let slots = slots.clone();
        tokio::spawn(async move {
            let (role, stream) = match transport::accept(stream, &state.token).await {
                Ok(accepted) => accepted,
                Err(error) => {
                    info!("rejected an RPC connection: {error}");
                    return;
                }
            };
            let Ok(_slot) = slots.acquire_owned().await else {
                return;
            };
            match role {
                transport::Role::Gui => {
                    let transport =
                        tarpc::serde_transport::new(stream, transport::Postcard::default());
                    tarpc::server::BaseChannel::with_defaults(transport)
                        .execute(state.serve())
                        .for_each(spawn)
                        .await;
                }
                transport::Role::GuiCallback => {
                    let transport =
                        tarpc::serde_transport::new(stream, transport::Postcard::default());
                    let client =
                        GuiClient::new(tarpc::client::Config::default(), transport).spawn();
                    state.connect_gui(client).await;
                }
                transport::Role::Agent => {
                    let transport =
                        tarpc::serde_transport::new(stream, transport::Postcard::default());
                    tarpc::server::BaseChannel::with_defaults(transport)
                        .execute(agent::Agent::serve(Backend { state }))
                        .for_each(spawn)
                        .await;
                }
            }
        });
    }
}

/// Where the GUI connects, for an editor that relays its connections.
#[derive(Debug, Serialize)]
struct GuiEndpoint {
    port: u16,
    /// The session token, in hex.
    token: String,
}

/// How the analyzer finds and launches its GUI.
#[derive(Clone, Debug, Default)]
pub struct GuiLink {
    /// Unix socket through which to report the RPC port and token.
    pub relay_socket: Option<PathBuf>,
    /// Whether the GUI and this analyzer's editor share a window.
    pub embedded: bool,
}

pub async fn main(rpc_port: Option<u16>, gui: GuiLink) {
    main_with_io(rpc_port, gui, tokio::io::stdin(), tokio::io::stdout()).await;
}

/// Runs the analyzer over an arbitrary LSP transport.
///
/// The production binary supplies stdin and stdout. Tests can supply a TCP
/// stream without launching a second Cargo target.
pub async fn main_with_io<I, O>(rpc_port: Option<u16>, gui: GuiLink, stdin: I, stdout: O)
where
    I: AsyncRead + Unpin,
    O: AsyncWrite,
{
    // Start server for communication with GUI.
    let port = rpc_port.unwrap_or(0);
    let listener = match tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, port)).await {
        Ok(listener) => listener,
        Err(error) => {
            eprintln!("failed to bind analyzer RPC server to port {port}: {error}");
            return;
        }
    };
    let token = match transport::SessionToken::generate() {
        Ok(token) => token,
        Err(error) => {
            eprintln!("failed to generate the analyzer session token: {error}");
            return;
        }
    };
    main_with_io_on_listener(listener, token, gui, stdin, stdout).await;
}

/// Runs the analyzer using pre-bound GUI RPC and LSP transports.
///
/// Supplying the listener and token lets tests reserve the RPC address
/// without a bind-release-bind race and authenticate their own clients.
pub async fn main_with_io_on_listener<I, O>(
    rpc_listener: tokio::net::TcpListener,
    token: transport::SessionToken,
    gui: GuiLink,
    stdin: I,
    stdout: O,
) where
    I: AsyncRead + Unpin,
    O: AsyncWrite,
{
    let server_addr = match rpc_listener.local_addr() {
        Ok(addr) => addr,
        Err(error) => {
            eprintln!("failed to read the analyzer RPC address: {error}");
            return;
        }
    };
    if let Some(path) = &gui.relay_socket
        && let Err(error) = announce_rpc_port(path, server_addr.port(), &token)
    {
        eprintln!(
            "failed to announce analyzer RPC port through `{}`: {error}",
            path.display()
        );
        return;
    }

    let mut ext_state = None;
    let (service, socket) = LspService::build(|client| {
        let state = State::new(server_addr, client, token, gui.embedded);
        ext_state = Some(state.clone());
        Backend { state }
    })
    .custom_method("custom/startGui", Backend::start_gui)
    .custom_method("custom/guiEndpoint", Backend::gui_endpoint)
    .custom_method("custom/openCell", Backend::open_cell)
    .custom_method("custom/newCell", Backend::new_cell)
    .custom_method("custom/renameCell", Backend::rename_cell)
    .custom_method("custom/inst", Backend::instantiate)
    .custom_method("custom/sinst", Backend::schematic_instantiate)
    .custom_method("custom/device", Backend::device)
    .custom_method("custom/commandCompletion", Backend::command_completion)
    .custom_method("custom/reloadConfig", Backend::reload_config)
    .custom_method("custom/setConfig", Backend::set_config)
    .custom_method("custom/saveConfig", Backend::save_config)
    .custom_method("custom/workspaceModified", Backend::workspace_modified)
    .custom_method("custom/followMode", Backend::follow_mode)
    .finish();
    let Some(state) = ext_state else {
        eprintln!("failed to initialize analyzer state");
        return;
    };
    tokio::spawn(serve_rpc(rpc_listener, state.clone()));

    state
        .editor_client
        .show_message(
            MessageType::LOG,
            format!("Server listening on port {}", server_addr.port()),
        )
        .await;

    init_logging();

    // Start actual LSP server.
    Server::new(stdin, stdout, socket).serve(service).await;
    state.retire_session();
}

#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::net::UnixListener;
    use std::sync::{Arc, RwLock};

    use argonc::{
        compile::{self, CellArg, CompileInput, CompileOutput, ExecErrorCompileOutput},
        parse,
    };

    use super::{
        ArgonConfig, DEFAULT_LOG_LEVEL, DeviceKind, GuiConfig, SourceState, config_with_key,
        is_gui_disconnected, parse_config, preview_instance_cell, read_unpoisoned, write_config,
        write_unpoisoned,
    };

    #[test]
    fn only_transport_errors_disconnect_the_gui() {
        assert!(is_gui_disconnected(&tarpc::client::RpcError::Shutdown));
        assert!(!is_gui_disconnected(
            &tarpc::client::RpcError::DeadlineExceeded
        ));
    }

    fn poison_rwlock(lock: &Arc<RwLock<i32>>) {
        let lock = lock.clone();
        let result = std::thread::spawn(move || {
            let _guard = write_unpoisoned(&lock);
            panic!("poison test lock");
        })
        .join();
        assert!(result.is_err());
    }

    #[test]
    fn configuration_lock_recovers_from_poisoning() {
        let lock = Arc::new(RwLock::new(1));
        poison_rwlock(&lock);
        assert_eq!(*read_unpoisoned(&lock), 1);

        poison_rwlock(&lock);
        *write_unpoisoned(&lock) = 2;
        assert_eq!(*read_unpoisoned(&lock), 2);
    }

    #[test]
    fn compilation_identity_changes_only_with_source_or_cell() {
        let mut source = SourceState::default();
        let initial = source.compile_identity();
        assert_eq!(initial, source.compile_identity());

        source.advance_revision();
        let edited = source.compile_identity();
        assert_ne!(initial, edited);

        source.cell = Some("top()".to_owned());
        let opened = source.compile_identity();
        assert_ne!(edited, opened);
        assert_eq!(opened, source.compile_identity());
    }

    #[test]
    fn logging_config_defaults_to_errors() {
        let config = parse_config("").expect("empty config should use defaults");
        assert_eq!(config.log.level, DEFAULT_LOG_LEVEL);
    }

    #[test]
    fn logging_config_reads_filter_and_rejects_unknown_keys() {
        let config =
            parse_config("[log]\nlevel = \"analyzer=debug,argone=trace\"\n").expect("valid config");
        assert_eq!(config.log.level, "analyzer=debug,argone=trace");
        assert!(parse_config("[log]\nunknown = true\n").is_err());
    }

    #[test]
    fn runtime_configuration_is_loaded_from_toml() {
        let config = parse_config(
            "[analyzer]\ncompile_debounce_ms = 0\n\
             [gui]\ndark_mode = false\nhierarchy_depth = 3\nicon_size = 18\nfont_size = 15.5\n",
        )
        .expect("valid runtime configuration");
        assert_eq!(config.analyzer.compile_debounce_ms, 0);
        assert!(!config.gui.dark_mode);
        assert_eq!(config.gui.hierarchy_depth, Some(3));
        assert_eq!(config.gui.icon_size, Some(18.));
        assert_eq!(config.gui.font_size, Some(15.5));
        assert!(parse_config("[gui]\nunknown = true\n").is_err());
        assert!(parse_config("[gui]\nicon_size = 0\n").is_err());
        assert!(parse_config("[gui]\nfont_size = 257\n").is_err());
    }

    #[test]
    fn runtime_configuration_round_trips_through_rpc_encoding() {
        for config in [
            ArgonConfig::default(),
            ArgonConfig {
                gui: GuiConfig {
                    icon_size: Some(18.),
                    font_size: Some(15.5),
                    ..GuiConfig::default()
                },
                ..ArgonConfig::default()
            },
        ] {
            let encoded = postcard::to_stdvec(&config).unwrap();
            let decoded: ArgonConfig = postcard::from_bytes(&encoded).unwrap();
            assert_eq!(decoded.gui.icon_size, config.gui.icon_size);
            assert_eq!(decoded.gui.font_size, config.gui.font_size);
        }
    }

    #[test]
    fn runtime_configuration_updates_dotted_keys_without_changing_other_values() {
        let config = ArgonConfig::default();
        let config = config_with_key(&config, "gui.hierarchy_depth", Some("3")).unwrap();
        let config = config_with_key(&config, "gui.dark_mode", Some("false")).unwrap();
        let config = config_with_key(&config, "gui.icon_size", Some("22")).unwrap();
        let config = config_with_key(&config, "gui.font_size", Some("13.5")).unwrap();
        let config = config_with_key(&config, "log.level", Some("analyzer=debug")).unwrap();

        assert_eq!(config.gui.hierarchy_depth, Some(3));
        assert!(!config.gui.dark_mode);
        assert_eq!(config.gui.icon_size, Some(22.));
        assert_eq!(config.gui.font_size, Some(13.5));
        assert_eq!(config.log.level, "analyzer=debug");
        assert_eq!(config.analyzer.compile_debounce_ms, 150);
    }

    #[test]
    fn runtime_configuration_validates_keys_and_types_and_can_reset_a_key() {
        let config =
            config_with_key(&ArgonConfig::default(), "gui.hierarchy_depth", Some("2")).unwrap();
        let config = config_with_key(&config, "gui.hierarchy_depth", None).unwrap();
        assert_eq!(config.gui.hierarchy_depth, None);
        assert!(config_with_key(&config, "gui.unknown", Some("true")).is_err());
        assert!(config_with_key(&config, "gui.dark_mode", Some("2")).is_err());
        assert!(config_with_key(&config, "gui.icon_size", Some("0")).is_err());
        assert!(config_with_key(&config, "gui.font_size", Some("300")).is_err());
        assert!(config_with_key(&config, "gui..dark_mode", Some("true")).is_err());
    }

    #[test]
    fn live_configuration_can_be_saved_and_loaded() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("nested/config.toml");
        let config =
            config_with_key(&ArgonConfig::default(), "gui.hierarchy_depth", Some("4")).unwrap();

        write_config(&path, &config).unwrap();
        let loaded = parse_config(&std::fs::read_to_string(path).unwrap()).unwrap();
        assert_eq!(loaded.gui.hierarchy_depth, Some(4));
        assert_eq!(loaded.analyzer.compile_debounce_ms, 150);
    }

    #[tokio::test]
    async fn preview_compile_reports_deep_recursion_instead_of_overflowing() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        let source = "use std::layout::{inst, rect};\n\
             cell forever(n: Int) {\n\
                 let r = rect(\"met1\", x0=0., y0=0., x1=1., y1=1.);\n\
                 let again = inst(forever(n + 1));\n\
             }\n";
        std::fs::write(&source_path, source).unwrap();
        let ast = parse::parse_workspace_with_std(&source_path).ast();
        let (module_path, module) = ast
            .iter()
            .find(|(_, module)| module.source_text.as_str() == source)
            .map(|(module_path, module)| (module_path.clone(), module.clone()))
            .unwrap();
        let tech = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tech/basic.tech.toml");
        let config = argonc::WorkspaceConfig::new(&source_path).with_tech(Some(tech));
        // The test thread's stack is far smaller than this compile needs.
        let message = super::on_compile_stack(move || {
            super::compile_preview_workspace(
                &ast,
                module_path,
                &module,
                module.text.to_string(),
                "forever(0)",
                &config,
                "place an instance",
            )
        })
        .await
        .expect("the compile should not panic")
        .expect_err("unbounded recursion should not compile");
        assert!(message.contains("recursion limit"), "{message}");
    }

    #[test]
    fn open_cell_evaluates_expression_arguments() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        std::fs::write(
            &source_path,
            "use std::layout::rect;\n\
             fn double(x: Float) -> Float { 2. * x }\n\
             cell top(w: Float) {\n\
                 let r = rect(\"met1\", x0=0., y0=0., x1=w, y1=10.);\n\
             }\n",
        )
        .unwrap();
        let mut ast = parse::parse_workspace_with_std(&source_path).ast();
        let invocation = parse::splice_cell_invocation(&mut ast, "top(double(25.))")
            .expect("invocation should splice");
        let (typed, errors) = compile::static_compile(&ast).unwrap();
        assert!(errors.errors.is_empty(), "{:?}", errors.errors);
        let tech = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tech/basic.tech.toml");
        let config = argonc::WorkspaceConfig::new(&source_path).with_tech(Some(tech));
        let output = compile::execute_cell_invocation(&typed, &invocation, &config);
        let CompileOutput::Valid(output) = output else {
            panic!("open cell should compile: {output:?}");
        };
        let top = &output.cells[&output.top];
        let rect = top
            .objects
            .values()
            .find_map(|object| object.get_rect())
            .expect("top should emit a rect");
        assert_eq!(rect.x1.0, 50.);
    }

    #[test]
    fn the_device_command_reads_its_kind_count_and_arguments() {
        let (kind, terminals, args) =
            super::parse_device_command("Nmos \"nch\", w=1., l=0.15").unwrap();
        assert_eq!(kind, DeviceKind::Nmos);
        assert_eq!(terminals, 4);
        assert_eq!(args, "\"nch\", w=1., l=0.15");
        let (kind, terminals, args) = super::parse_device_command("subckt 3 \"opamp\"").unwrap();
        assert_eq!(kind, DeviceKind::Subckt);
        assert_eq!(terminals, 3);
        assert_eq!(args, "\"opamp\"");
        let (kind, terminals, args) = super::parse_device_command("Res value=1.").unwrap();
        assert_eq!((kind, terminals), (DeviceKind::Res, 2));
        assert_eq!(args, "\"\", value=1.", "a missing model is empty");
        let (_, terminals, _) = super::parse_device_command("Npn 4 \"q\"").unwrap();
        assert_eq!(terminals, 4);
        assert!(super::parse_device_command("").is_err());
        assert!(super::parse_device_command("Triode").is_err());
        assert!(
            super::parse_device_command("Subckt \"x\"").is_err(),
            "needs a count"
        );
        assert!(
            super::parse_device_command("Nmos 3 \"nch\"").is_err(),
            "four terminals"
        );
        assert!(super::parse_device_command("Nmos \"nch\", w=").is_err());
    }

    /// The preview binding is spelled as the analyzer inserts it, which needs
    /// no `use` declaration.
    #[test]
    fn preview_instance_can_use_a_value_from_its_scope() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        std::fs::write(
            &source_path,
            "cell child(width: Float) {}\n\
             cell top(width: Float) {\n\
                 let preview_binding = std::layout::inst(child(width));\n\
             }\n",
        )
        .unwrap();
        let ast = parse::parse_workspace_with_std(&source_path).ast();
        let (typed, errors) = compile::static_compile(&ast).unwrap();
        assert!(errors.errors.is_empty(), "{:?}", errors.errors);
        let tech = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tech/basic.tech.toml");
        let output = compile::execute_cell(
            &typed,
            CompileInput {
                cell: &["top"],
                args: vec![CellArg::Float(42.)],
            },
            &argonc::WorkspaceConfig::default().with_tech(Some(tech)),
        );
        let output = match output {
            CompileOutput::Valid(output) => output,
            CompileOutput::ExecErrors(ExecErrorCompileOutput {
                output: Some(output),
                ..
            }) => output,
            output => panic!("preview should compile: {output:?}"),
        };

        assert!(preview_instance_cell(&output, "preview_binding").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn private_dirs_are_owner_only_and_existing_parents_are_left_alone() {
        use std::{fs, os::unix::fs::PermissionsExt, path::Path};

        let root = tempfile::tempdir().unwrap();
        let existing = root.path().join("existing");
        fs::create_dir(&existing).unwrap();
        fs::set_permissions(&existing, fs::Permissions::from_mode(0o755)).unwrap();
        let directory = existing.join("state/argon");
        super::create_private_dir_all(&directory).unwrap();
        let mode = |path: &Path| fs::metadata(path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&existing), 0o755);
        assert_eq!(mode(&existing.join("state")), 0o700);
        assert_eq!(mode(&directory), 0o700);
    }

    #[tokio::test]
    async fn an_embedded_analyzer_waits_for_its_gui_instead_of_starting_one() {
        use futures::StreamExt;

        let mut state = None;
        let (_service, socket) = tower_lsp_server::LspService::build(|client| {
            let token = super::transport::SessionToken::generate().unwrap();
            let backend = super::Backend {
                state: super::State::new("127.0.0.1:1".parse().unwrap(), client, token, true),
            };
            state = Some(backend.state.clone());
            backend
        })
        .finish();
        tokio::spawn(socket.for_each(|_| async {}));
        let state = state.unwrap();

        super::Backend {
            state: state.clone(),
        }
        .start_gui()
        .await
        .unwrap();
        assert!(state.take_gui_process().await.is_none());
    }

    #[cfg(unix)]
    #[test]
    fn rpc_port_and_token_are_announced_through_a_unix_socket() {
        use std::io::Read;

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("rpc.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let receiver = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut port = String::new();
            stream.read_to_string(&mut port).unwrap();
            port
        });
        let token = super::transport::SessionToken::generate().unwrap();
        super::announce_rpc_port(&path, 43210, &token).unwrap();
        assert_eq!(
            receiver.join().unwrap(),
            format!("43210 {}\n", token.to_hex())
        );
    }

    mod pending_edits {
        use std::{collections::HashMap, time::Duration};

        use futures::{SinkExt, StreamExt};
        use serde_json::{Value, json};
        use tokio::sync::mpsc;
        use tower::{Service, ServiceExt};
        use tower_lsp_server::{
            LanguageServer, LspService,
            jsonrpc::{Id, Request, Response},
            ls_types::{
                ApplyWorkspaceEditParams, DidChangeTextDocumentParams, DidOpenTextDocumentParams,
                Position, Range, TextDocumentContentChangeEvent, TextDocumentItem, TextEdit, Uri,
                VersionedTextDocumentIdentifier,
            },
        };

        use crate::{ArgonConfig, Backend, PENDING_EDIT_TIMEOUT, State, transport::SessionToken};

        const BEFORE: &str = "cell top() {\n    let a = 1.;\n    let b = 2.;\n}\n";
        const AFTER: &str = "cell top() {\n    let a = 3.;\n    let b = 4.;\n}\n";

        /// An analyzer with `BEFORE` open as `lib.ar` in a workspace that
        /// compiles, and the editor's side of its connection. The editor
        /// answers every request but `workspace/applyEdit`, which the test
        /// answers.
        struct Editor {
            backend: Backend,
            uri: Uri,
            workspace_edits: mpsc::UnboundedReceiver<(Id, ApplyWorkspaceEditParams)>,
            responses: mpsc::UnboundedSender<Response>,
            _service: LspService<Backend>,
            _directory: tempfile::TempDir,
        }

        impl Editor {
            async fn open() -> Self {
                let directory = tempfile::tempdir().unwrap();
                std::fs::copy(
                    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                        .join("../../examples/tech/basic.tech.toml"),
                    directory.path().join("tech.toml"),
                )
                .unwrap();
                std::fs::write(
                    directory.path().join("Argon.toml"),
                    "name = \"pending-edits\"\ntech = \"tech.toml\"\n",
                )
                .unwrap();
                let path = directory.path().join("lib.ar");
                std::fs::write(&path, BEFORE).unwrap();

                let mut backend = None;
                let (mut service, mut socket) = LspService::build(|client| {
                    let token = SessionToken::generate().unwrap();
                    let built = Backend {
                        state: State::new("127.0.0.1:1".parse().unwrap(), client, token, false),
                    };
                    backend = Some(built.clone());
                    built
                })
                .finish();
                let backend = backend.unwrap();
                let initialize = Request::build("initialize")
                    .params(json!({ "capabilities": {} }))
                    .id(1)
                    .finish();
                service
                    .ready()
                    .await
                    .unwrap()
                    .call(initialize)
                    .await
                    .unwrap();
                // Only the analyzer's own edits compile within a test.
                let mut config = ArgonConfig::default();
                config.analyzer.compile_debounce_ms = 60_000;
                backend.state.apply_config(config);
                backend
                    .state
                    .root_dir
                    .set(directory.path().to_path_buf())
                    .unwrap();
                backend.state.source_state.lock().await.cell = Some("top()".to_owned());

                let (edits, workspace_edits) = mpsc::unbounded_channel();
                let (responses, mut outgoing) = mpsc::unbounded_channel::<Response>();
                tokio::spawn(async move {
                    loop {
                        tokio::select! {
                            request = socket.next() => {
                                let Some(request) = request else {
                                    break;
                                };
                                let (method, id, params) = request.into_parts();
                                match id {
                                    Some(id) if method == "workspace/applyEdit" => {
                                        let params = serde_json::from_value(params.unwrap()).unwrap();
                                        let _ = edits.send((id, params));
                                    }
                                    Some(id) => {
                                        let _ = socket.send(Response::from_ok(id, Value::Null)).await;
                                    }
                                    None => {}
                                }
                            }
                            Some(response) = outgoing.recv() => {
                                let _ = socket.send(response).await;
                            }
                        }
                    }
                });

                let uri = Uri::from_file_path(&path).unwrap();
                backend
                    .did_open(DidOpenTextDocumentParams {
                        text_document: TextDocumentItem {
                            uri: uri.clone(),
                            language_id: "argon".to_owned(),
                            version: 0,
                            text: BEFORE.to_owned(),
                        },
                    })
                    .await;
                Self {
                    backend,
                    uri,
                    workspace_edits,
                    responses,
                    _service: service,
                    _directory: directory,
                }
            }

            /// Asks the editor to set `a` to 3 and `b` to 4 in one edit,
            /// returning whether it applied and the source revision once the
            /// request returns.
            fn edit(&self) -> tokio::task::JoinHandle<(bool, u64)> {
                let replacement = |line| TextEdit {
                    range: Range::new(Position::new(line, 12), Position::new(line, 14)),
                    new_text: if line == 1 { "3." } else { "4." }.to_owned(),
                };
                let changes =
                    HashMap::from([(self.uri.clone(), vec![replacement(1), replacement(2)])]);
                let state = self.backend.state.clone();
                tokio::spawn(async move {
                    let applied = state.apply_source_changes(changes, None).await;
                    (applied, state.source_state.lock().await.revision)
                })
            }

            /// The edits of the next workspace edit the analyzer asks for.
            async fn workspace_edit(&mut self) -> (Id, Vec<TextEdit>) {
                let (id, params) = self.workspace_edits.recv().await.unwrap();
                let mut changes = params.edit.changes.unwrap();
                (id, changes.remove(&self.uri).unwrap())
            }

            fn applied(&self, id: Id) {
                self.responses
                    .send(Response::from_ok(id, json!({ "applied": true })))
                    .unwrap();
            }

            /// Reports `edit` as the change that brought the buffer to `version`.
            async fn change(&self, version: i32, edit: &TextEdit) {
                self.backend
                    .did_change(DidChangeTextDocumentParams {
                        text_document: VersionedTextDocumentIdentifier {
                            uri: self.uri.clone(),
                            version,
                        },
                        content_changes: vec![TextDocumentContentChangeEvent {
                            range: Some(edit.range),
                            range_length: None,
                            text: edit.new_text.clone(),
                        }],
                    })
                    .await;
            }

            async fn compiled_revision(&self) -> u64 {
                self.backend
                    .state
                    .published_state
                    .lock()
                    .await
                    .compiled_revision
            }
        }

        #[tokio::test]
        async fn an_edit_reported_in_parts_compiles_once_all_of_it_arrives() {
            let mut editor = Editor::open().await;
            let edit = editor.edit();
            let (id, edits) = editor.workspace_edit().await;
            // Like Neovim, report the edit nearest the end at once, answer,
            // and report the rest after a debounce.
            editor.change(1, &edits[1]).await;
            assert_eq!(editor.compiled_revision().await, 0);
            editor.applied(id);
            tokio::time::sleep(Duration::from_millis(50)).await;
            editor.change(2, &edits[0]).await;

            // Revision 1 opened the file and revision 2 had half the edit.
            assert_eq!(edit.await.unwrap(), (true, 3));
            let mut compiled = editor.backend.state.compiled_revisions.subscribe();
            tokio::time::timeout(
                PENDING_EDIT_TIMEOUT * 10,
                compiled.wait_for(|&revision| revision >= 3),
            )
            .await
            .unwrap()
            .unwrap();
            assert_eq!(editor.compiled_revision().await, 3);
            let published = editor.backend.state.published_state.lock().await;
            assert!(
                published
                    .ast
                    .values()
                    .any(|module| module.source_text.as_str() == AFTER)
            );
        }

        #[tokio::test]
        async fn an_edit_never_reported_in_full_is_withdrawn_and_compiled_as_is() {
            let mut editor = Editor::open().await;
            let edit = editor.edit();
            let (id, edits) = editor.workspace_edit().await;
            editor.change(1, &edits[1]).await;
            editor.applied(id);

            assert_eq!(edit.await.unwrap(), (true, 2));
            let state = &editor.backend.state;
            assert!(
                state
                    .source_state
                    .lock()
                    .await
                    .pending_workspace_edits
                    .is_empty()
            );
            let mut compiled = state.compiled_revisions.subscribe();
            tokio::time::timeout(
                PENDING_EDIT_TIMEOUT * 10,
                compiled.wait_for(|&revision| revision >= 2),
            )
            .await
            .unwrap()
            .unwrap();
        }
    }
}
