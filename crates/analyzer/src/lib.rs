mod compiler_worker;
pub mod document;
pub mod rpc;

pub mod cli;

use std::{
    env, fs, io,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, OnceLock, RwLock, RwLockReadGuard, RwLockWriteGuard},
    time::Duration,
};

use arc::Library;
use argonc::{
    WorkspaceConfig,
    ast::{Span, WorkspaceAst},
    compile::{
        self, Arrayed, CompileOutput, ExecErrorCompileOutput, StaticErrorCompileOutput,
        VarIdTyMetadata,
    },
    parse::{self, CellInvocation, WorkspaceParseAst},
};
use futures::prelude::*;
use indexmap::IndexMap;
use rpc::{CompilationSnapshot, GuiClient, InstancePreview, LangServer, insert_statement};
use serde::{Deserialize, Serialize};
use tarpc::{context, server::Channel, tokio_serde::formats::Json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader},
    process::{Child, Command},
    sync::Mutex,
};
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{notification::Notification, request::Request, *};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};
use tracing::{error, info};
use tracing_subscriber::{
    EnvFilter, Registry, layer::SubscriberExt, reload, util::SubscriberInitExt,
};

use crate::compiler_worker::{CompileIdentity, CompileRequest, CompileResult, CompilerWorker};
use crate::document::{Document, DocumentChange};

const DEFAULT_LOG_LEVEL: &str = "error";
const LOG_FILE: &str = "argon.log";

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

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ArgonConfig {
    pub analyzer: AnalyzerConfig,
    pub gui: GuiConfig,
    pub log: LogConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct AnalyzerConfig {
    pub compile_debounce_ms: u64,
}

impl Default for AnalyzerConfig {
    fn default() -> Self {
        Self {
            compile_debounce_ms: 150,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct GuiConfig {
    pub dark_mode: bool,
    /// Maximum rendered hierarchy depth. Omitted means unlimited.
    pub hierarchy_depth: Option<usize>,
}

impl Default for GuiConfig {
    fn default() -> Self {
        Self {
            dark_mode: true,
            hierarchy_depth: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct LogConfig {
    pub level: String,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            level: DEFAULT_LOG_LEVEL.to_owned(),
        }
    }
}

pub fn argon_config_path() -> Option<PathBuf> {
    env::var_os("XDG_CONFIG_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            homedir::my_home()
                .ok()
                .flatten()
                .map(|home| home.join(".config"))
        })
        .map(|directory| directory.join("argon/config.toml"))
}

pub fn argon_state_dir() -> Option<PathBuf> {
    env::var_os("XDG_STATE_HOME")
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
        .or_else(|| {
            homedir::my_home()
                .ok()
                .flatten()
                .map(|home| home.join(".local/state"))
        })
        .map(|directory| directory.join("argon"))
}

pub fn argon_log_path() -> Option<PathBuf> {
    argon_state_dir().map(|directory| directory.join(LOG_FILE))
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
    if let Err(error) = fs::create_dir_all(&log_dir) {
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

#[derive(Debug, Default)]
pub(crate) struct SourceState {
    pub(crate) revision: u64,
    pub(crate) cell: Option<String>,
    pub(crate) editor_files: IndexMap<Uri, Document>,
    pub(crate) pending_workspace_edits: IndexMap<Uri, usize>,
    pub(crate) workspace_modified: bool,
}

impl SourceState {
    fn advance_revision(&mut self) -> u64 {
        self.revision = self.revision.saturating_add(1);
        self.revision
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
    pub(crate) prev_diagnostics: IndexMap<Uri, Vec<Diagnostic>>,
    pub(crate) compiled_revision: u64,
}

#[derive(Clone, Debug)]
struct GuiConnection {
    id: u64,
    client: GuiClient,
}

#[derive(Debug, Default)]
struct GuiState {
    connection: Option<GuiConnection>,
    process: Option<Child>,
    next_connection_id: u64,
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
fn blocking_compile_error_messages(output: &CompileOutput) -> Vec<String> {
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

fn compile_open_cell(
    ast: &WorkspaceAst<VarIdTyMetadata>,
    invocation: &CellInvocation,
    config: &WorkspaceConfig,
) -> CompileOutput {
    compile::execute_cell_invocation(ast, invocation, config)
}

fn diagnostics(
    root_dir: &Path,
    ast: &WorkspaceParseAst,
    output: Option<&CompileOutput>,
) -> IndexMap<Uri, Vec<Diagnostic>> {
    let mut diagnostics: IndexMap<Uri, Vec<Diagnostic>> = IndexMap::new();
    if let Some(o) = output {
        let errs = match o {
            CompileOutput::FatalParseErrors => {
                vec![(
                    Span {
                        path: root_dir.join("lib.ar"),
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
                            path: root_dir.join("lib.ar"),
                            span: cfgrammar::Span::new(0, 0),
                        }),
                        format!("{}", e.kind),
                    )
                })
                .collect(),
            CompileOutput::Valid(_) => vec![],
        };
        for (span, message) in errs {
            // Generated entry declarations are beyond the editor-visible
            // source. Their diagnostics are reported as analyzer messages.
            if let Some(ast) = ast.values().find(|ast| ast.path == span.path)
                && span.span.start() <= ast.source_text.len()
                && let Some(url) = Uri::from_file_path(&span.path)
            {
                let doc = Document::new(&ast.source_text, 0);
                diagnostics.entry(url).or_default().push(Diagnostic {
                    range: Range {
                        start: doc.offset_to_pos(span.span.start()),
                        end: doc.offset_to_pos(span.span.end()),
                    },
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
    pub(crate) source_state: Arc<Mutex<SourceState>>,
    pub(crate) published_state: Arc<Mutex<PublishedState>>,
    compiler: CompilerWorker,
    gui: Arc<Mutex<GuiState>>,
    app_config: Arc<RwLock<ArgonConfig>>,
}

impl State {
    fn new(server_addr: SocketAddr, editor_client: Client) -> Self {
        let config = read_config().unwrap_or_else(|error| {
            error!("{error}; using default configuration");
            ArgonConfig::default()
        });
        Self {
            server_addr,
            editor_client,
            root_dir: Default::default(),
            source_state: Default::default(),
            published_state: Default::default(),
            compiler: CompilerWorker::new(),
            gui: Default::default(),
            app_config: Arc::new(RwLock::new(config)),
        }
    }

    fn apply_config(&self, config: ArgonConfig) {
        *write_unpoisoned(&self.app_config) = config;
    }

    fn config(&self) -> ArgonConfig {
        read_unpoisoned(&self.app_config).clone()
    }

    async fn is_latest_compile_request(&self, identity: &CompileIdentity) -> bool {
        let source = self.source_state.lock().await;
        source.compile_identity() == *identity
    }

    async fn gui_connection(&self) -> Option<GuiConnection> {
        self.gui.lock().await.connection.clone()
    }

    async fn install_gui_connection(&self, client: GuiClient) -> GuiConnection {
        let mut gui = self.gui.lock().await;
        gui.next_connection_id = gui.next_connection_id.saturating_add(1);
        let connection = GuiConnection {
            id: gui.next_connection_id,
            client,
        };
        gui.connection = Some(connection.clone());
        connection
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
        let backend = self.clone();
        let delay = self.state.config().analyzer.compile_debounce_ms;
        tokio::spawn(async move {
            if delay > 0 {
                tokio::time::sleep(Duration::from_millis(delay)).await;
            }
            if backend.state.is_latest_compile_request(&identity).await {
                backend.update_cell(identity).await;
            }
        });
    }

    /// Compile and publish diagnostics for an exact source/cell identity.
    /// GUI presentation is deliberately handled by the caller.
    async fn compile_snapshot(&self, identity: CompileIdentity) -> Option<CompilationSnapshot> {
        let request = {
            let source = self.state.source_state.lock().await;
            if source.compile_identity() != identity {
                return None;
            }
            let root_dir = self.state.root_dir.get().cloned()?;
            CompileRequest {
                identity: identity.clone(),
                root_dir,
            }
        };
        let result = self.state.compiler.compile(request).await?;
        if !self.state.is_latest_compile_request(&result.identity).await {
            return None;
        }
        self.publish_compilation(result).await
    }

    async fn publish_compilation(&self, result: CompileResult) -> Option<CompilationSnapshot> {
        let identity = result.identity.clone();
        if !self.state.is_latest_compile_request(&identity).await {
            return None;
        }
        let mut current_diagnostics =
            diagnostics(&result.root_dir, &result.ast, result.output.as_ref());
        let snapshot = result.output.clone().map(|output| CompilationSnapshot {
            revision: identity.revision,
            output,
        });
        {
            // The freshness check and commit are atomic with respect to source
            // and cell changes because they take this same lock.
            let source = self.state.source_state.lock().await;
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
            compiled.compiled_revision = identity.revision;
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
    ) -> Option<GuiConnection> {
        if !self.state.is_latest_compile_request(identity).await {
            return None;
        }
        let connection = self.state.gui_connection().await?;
        if !self.state.is_latest_compile_request(identity).await {
            return None;
        }
        let result = connection
            .client
            .update_cell(context::current(), snapshot)
            .await;
        self.handle_gui_result(&connection, result).await?;
        Some(connection)
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
        let snapshot = self.compile_snapshot(identity.clone()).await?;
        self.send_cell_update(&identity, snapshot).await
    }

    async fn open_cell_view(&self, identity: CompileIdentity) {
        let Some(connection) = self.update_cell(identity.clone()).await else {
            return;
        };
        if !self.state.is_latest_compile_request(&identity).await {
            return;
        }
        let result = connection.client.fit(context::current()).await;
        if self.handle_gui_result(&connection, result).await.is_none() {
            return;
        }
        if self.state.is_latest_compile_request(&identity).await {
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
    type Params = ();
    type Result = ();

    const METHOD: &'static str = "custom/undo";
}

#[derive(Debug, Clone, Copy)]
struct Redo;

impl Request for Redo {
    type Params = ();
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
    type Params = Option<String>;

    const METHOD: &'static str = "custom/focusEditor";
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
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
            let _ = self.state.root_dir.set(root_dir);
            let connection = self.state.gui_connection().await;
            self.state.publish_workspace_path(connection).await;
        }
        Ok(InitializeResult {
            capabilities: ServerCapabilities {
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
        let doc = Document::new(params.text_document.text, params.text_document.version);
        let uri = params.text_document.uri;
        if let Some(path) = uri.to_file_path().map(|path| path.into_owned()) {
            self.state
                .compiler
                .set_source_text(path, doc.contents().to_owned());
        }
        source.editor_files.insert(uri, doc);
        source.advance_revision();
        let identity = source.compile_identity();
        drop(source);
        self.compile_after_debounce(identity);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let mut source = self.state.source_state.lock().await;
        let analyzer_edit = source
            .pending_workspace_edits
            .contains_key(&params.text_document.uri);
        if let Some(count) = source
            .pending_workspace_edits
            .get_mut(&params.text_document.uri)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                source
                    .pending_workspace_edits
                    .shift_remove(&params.text_document.uri);
            }
        }
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
        let identity = source.compile_identity();
        drop(source);
        if analyzer_edit {
            self.update_cell(identity).await;
        } else {
            self.compile_after_debounce(identity);
        }
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
        self.compile_after_debounce(identity);
    }

    async fn shutdown(&self) -> Result<()> {
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

const PREVIEW_BINDING_PREFIX: &str = "__argon_preview_instance";

fn preview_instance_cell(
    output: &compile::CompiledData,
    preview_binding: &str,
) -> Option<compile::CellId> {
    output.cells.values().find_map(|cell| {
        cell.scopes.values().find_map(|scope| {
            scope.bindings.values().find_map(|(name, objects)| {
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
    async fn start_gui(&self) -> Result<()> {
        if let Some(connection) = self.state.gui_connection().await {
            self.state
                .editor_client
                .show_message(MessageType::LOG, "Attempting to contact existing GUI...")
                .await;
            match connection.client.activate(context::current()).await {
                Ok(()) => {
                    self.state
                        .editor_client
                        .show_message(MessageType::LOG, "Connected to existing GUI!")
                        .await;
                    return Ok(());
                }
                Err(error) if is_gui_disconnected(&error) => {
                    self.state.clear_gui_connection(connection.id).await;
                    self.state
                        .editor_client
                        .show_message(MessageType::LOG, "Failed to contact existing GUI.")
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
        let old_gui = self.state.take_gui_process().await;
        if let Some(mut gui) = old_gui {
            let _ = gui.kill().await;
        }

        self.state
            .editor_client
            .show_message(MessageType::LOG, "Starting the GUI...")
            .await;
        let state = self.state.clone();

        tokio::spawn(async move {
            match Command::new("argone")
                .arg("gui")
                .arg(format!("{}", state.server_addr))
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
        let identity = {
            let mut source = self.state.source_state.lock().await;
            source.cell = Some(cell.into());
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

    async fn open_cell(&self, params: OpenCellParams) -> Result<()> {
        let state = self.state.clone();
        state
            .editor_client
            .show_message(MessageType::LOG, &format!("cell {}", params.cell))
            .await;
        self.select_and_open_cell(params.cell).await;
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
            return Ok(());
        };
        let document = Document::new(&ast.source_text, 0);
        let scope = &ast.span2scope[&scope_span];
        let preview_binding = (0..)
            .map(|index| format!("{PREVIEW_BINDING_PREFIX}{index}"))
            .find(|name| {
                workspace_ast
                    .values()
                    .all(|ast| !ast.text.contains(name.as_str()))
            })
            .expect("an unused preview binding always exists");
        let statement = format!("let {preview_binding} = inst({});", params.cell);
        let insertion = insert_statement(
            &document,
            scope.span,
            scope.tail.as_ref().map(|tail| tail.span().start()),
            &statement,
            0..statement.len(),
        );
        let mut preview_source = ast.text.to_string();
        preview_source.insert_str(insertion.offset, &insertion.edit.new_text);
        let mut preview_module =
            match parse::parse_source_text(preview_source, scope_span.path.clone()) {
                Ok(ast) => ast,
                Err(error) => {
                    self.state
                        .editor_client
                        .show_message(
                            MessageType::ERROR,
                            format!("Could not instantiate `{}`: {error}", params.cell),
                        )
                        .await;
                    return Ok(());
                }
            };
        preview_module.promote_last_declarations(ast.generated_declarations);
        let mut preview_ast = (*workspace_ast).clone();
        preview_ast.insert(module_path, preview_module);
        let Some(open_cell) = open_cell else {
            self.state
                .editor_client
                .show_message(MessageType::ERROR, "Open a cell before placing an instance")
                .await;
            return Ok(());
        };
        // Recompile the open cell against the preview. Splicing its invocation
        // before static analysis lets argument expressions use normal name
        // resolution and type checking.
        let invocation = match parse::splice_cell_invocation(&mut preview_ast, &open_cell) {
            Ok(invocation) => invocation,
            Err(error) => {
                self.state
                    .editor_client
                    .show_message(MessageType::ERROR, format!("Open cell is invalid: {error}"))
                    .await;
                return Ok(());
            }
        };
        let Some((typed_ast, static_output)) = compile::static_compile(&preview_ast) else {
            self.state
                .editor_client
                .show_message(MessageType::ERROR, "Could not analyze the preview instance")
                .await;
            return Ok(());
        };
        if !static_output.errors.is_empty() {
            let message = static_output
                .errors
                .iter()
                .map(|error| error.kind.to_string())
                .collect::<Vec<_>>()
                .join("\n");
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    format!("Could not instantiate `{}`: {message}", params.cell),
                )
                .await;
            return Ok(());
        }
        if workspace.tech.is_none() {
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    "The Argon library does not configure a technology file",
                )
                .await;
            return Ok(());
        }
        let compiled = compile_open_cell(&typed_ast, &invocation, &workspace);
        let output = match compiled {
            CompileOutput::Valid(output) => output,
            CompileOutput::ExecErrors(ExecErrorCompileOutput {
                output: Some(output),
                errors,
            }) if !errors.iter().any(|error| error.kind.is_invalid_cell()) => output,
            output => {
                let messages = blocking_compile_error_messages(&output).join("\n");
                self.state
                    .editor_client
                    .show_message(
                        MessageType::ERROR,
                        format!("Could not instantiate `{}`: {messages}", params.cell),
                    )
                    .await;
                return Ok(());
            }
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
            output,
            cell,
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

    async fn reload_config(&self, _: ()) -> Result<()> {
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

#[cfg(unix)]
fn announce_rpc_port(path: &Path, port: u16) -> io::Result<()> {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let mut stream = UnixStream::connect(path)?;
    writeln!(stream, "{port}")?;
    stream.flush()
}

#[cfg(not(unix))]
fn announce_rpc_port(_: &Path, _: u16) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "relay requires a Unix-like remote host",
    ))
}

pub async fn main(rpc_port: Option<u16>, relay_socket: Option<PathBuf>) {
    main_with_io(
        rpc_port,
        relay_socket,
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await;
}

/// Runs the analyzer over an arbitrary LSP transport.
///
/// The production binary supplies stdin and stdout. Tests can supply a TCP
/// stream without launching a second Cargo target.
pub async fn main_with_io<I, O>(
    rpc_port: Option<u16>,
    relay_socket: Option<PathBuf>,
    stdin: I,
    stdout: O,
) where
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
    main_with_io_on_listener(listener, relay_socket, stdin, stdout).await;
}

/// Runs the analyzer using pre-bound GUI RPC and LSP transports.
///
/// Supplying the listener lets tests reserve the RPC address without a
/// bind-release-bind race.
pub async fn main_with_io_on_listener<I, O>(
    rpc_listener: tokio::net::TcpListener,
    relay_socket: Option<PathBuf>,
    stdin: I,
    stdout: O,
) where
    I: AsyncRead + Unpin,
    O: AsyncWrite,
{
    let mut listener =
        match tarpc::serde_transport::tcp::listen_on(rpc_listener, Json::default).await {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("failed to configure analyzer RPC listener: {error}");
                return;
            }
        };
    let server_addr = listener.local_addr();
    if let Some(path) = relay_socket
        && let Err(error) = announce_rpc_port(&path, server_addr.port())
    {
        eprintln!(
            "failed to announce analyzer RPC port through `{}`: {error}",
            path.display()
        );
        return;
    }

    let mut ext_state = None;
    let (service, socket) = LspService::build(|client| {
        let state = State::new(server_addr, client);
        ext_state = Some(state.clone());
        Backend { state }
    })
    .custom_method("custom/startGui", Backend::start_gui)
    .custom_method("custom/openCell", Backend::open_cell)
    .custom_method("custom/inst", Backend::instantiate)
    .custom_method("custom/reloadConfig", Backend::reload_config)
    .custom_method("custom/setConfig", Backend::set_config)
    .custom_method("custom/saveConfig", Backend::save_config)
    .custom_method("custom/workspaceModified", Backend::workspace_modified)
    .finish();
    let Some(state) = ext_state else {
        eprintln!("failed to initialize analyzer state");
        return;
    };
    listener.config_mut().max_frame_length(usize::MAX);
    let state_clone = state.clone();
    tokio::spawn(async move {
        listener
            // Ignore accept errors.
            .filter_map(|r| futures::future::ready(r.ok()))
            .map(tarpc::server::BaseChannel::with_defaults)
            // serve is generated by the service attribute. It takes as input any type implementing
            // the generated World trait.
            .map(|channel| channel.execute(state_clone.clone().serve()).for_each(spawn))
            // Max 10 channels.
            .buffer_unordered(10)
            .for_each(|_| async {})
            .await;
    });

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
        ArgonConfig, DEFAULT_LOG_LEVEL, SourceState, config_with_key, is_gui_disconnected,
        parse_config, preview_instance_cell, read_unpoisoned, write_config, write_unpoisoned,
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
             [gui]\ndark_mode = false\nhierarchy_depth = 3\n",
        )
        .expect("valid runtime configuration");
        assert_eq!(config.analyzer.compile_debounce_ms, 0);
        assert!(!config.gui.dark_mode);
        assert_eq!(config.gui.hierarchy_depth, Some(3));
        assert!(parse_config("[gui]\nunknown = true\n").is_err());
    }

    #[test]
    fn runtime_configuration_updates_dotted_keys_without_changing_other_values() {
        let config = ArgonConfig::default();
        let config = config_with_key(&config, "gui.hierarchy_depth", Some("3")).unwrap();
        let config = config_with_key(&config, "gui.dark_mode", Some("false")).unwrap();
        let config = config_with_key(&config, "log.level", Some("analyzer=debug")).unwrap();

        assert_eq!(config.gui.hierarchy_depth, Some(3));
        assert!(!config.gui.dark_mode);
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

    #[test]
    fn open_cell_evaluates_expression_arguments() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        std::fs::write(
            &source_path,
            "fn double(x: Float) -> Float { 2. * x }\n\
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
    fn preview_instance_can_use_a_value_from_its_scope() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        std::fs::write(
            &source_path,
            "cell child(width: Float) {}\n\
             cell top(width: Float) {\n\
                 let preview_binding = inst(child(width));\n\
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
    fn rpc_port_is_announced_through_a_unix_socket() {
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
        super::announce_rpc_port(&path, 43210).unwrap();
        assert_eq!(receiver.join().unwrap(), "43210\n");
    }
}
