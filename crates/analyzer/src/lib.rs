pub mod document;
pub mod rpc;

pub mod cli;

use std::{
    env, fs, io,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use arc::Library;
use argonc::{
    ast::{Span, WorkspaceAst},
    compile::{
        self, Arrayed, CellArg, CompileInput, CompileOutput, ExecErrorCompileOutput,
        StaticErrorCompileOutput, VarIdTyMetadata,
    },
    parse::{self, WorkspaceParseAst},
};
use futures::prelude::*;
use indexmap::IndexMap;
use rpc::{GuiClient, InstancePreview, LangServer, insert_statement};
use serde::{Deserialize, Serialize};
use tarpc::{context, server::Channel, tokio_serde::formats::Json};
use tokio::{
    io::{AsyncBufReadExt, AsyncRead, AsyncWrite, BufReader},
    process::{Child, Command},
    sync::Mutex,
};
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{request::Request, *};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::document::{Document, DocumentChange};

const DEFAULT_LOG_LEVEL: &str = "error";
const LOG_FILE: &str = "argon.log";

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct ArgonConfig {
    log: LogConfig,
}

#[derive(Debug, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct LogConfig {
    level: String,
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
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(tracing_appender::rolling::never(log_dir, LOG_FILE))
        .with_ansi(false)
        .try_init();
}

// TODO: finer-grained synchronization?
// TODO: Verify synchronization between GUI and editor files when appropriate.
#[derive(Debug, Default)]
pub struct StateMut {
    gui: Option<Child>,
    root_dir: Option<PathBuf>,
    config: Option<Library>,
    ast: WorkspaceParseAst,
    prev_diagnostics: IndexMap<Uri, Vec<Diagnostic>>,
    compile_output: Option<CompileOutput>,
    cell: Option<String>,
    gui_client: Option<GuiClient>,
    editor_files: IndexMap<Uri, Document>,
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

fn parse_setting(input: &str) -> Option<(&str, &str)> {
    let separator = input.find(char::is_whitespace)?;
    let (key, value) = input.split_at(separator);
    let value = value.trim_start();
    (!key.is_empty() && !value.is_empty()).then_some((key, value))
}

async fn show_message_in_editor_and_gui(
    client: &Client,
    mut gui_client: Option<GuiClient>,
    typ: MessageType,
    message: impl Into<String>,
) {
    let message = message.into();
    client.show_message(typ, message.clone()).await;
    if typ != MessageType::LOG
        && let Some(gui_client) = gui_client.as_mut()
    {
        let _ = gui_client
            .show_message(context::current(), typ, message)
            .await;
    }
}

async fn compile_open_cell(
    ast: &WorkspaceAst<VarIdTyMetadata>,
    cell: &str,
    lyp: &Path,
    gds_imports: &[(String, PathBuf)],
    client: &Client,
    gui_client: Option<GuiClient>,
) -> Option<CompileOutput> {
    if let Err(error) = argonc::layer::read_lyp(lyp) {
        show_message_in_editor_and_gui(
            client,
            gui_client,
            MessageType::ERROR,
            format!("Could not open cell: {error}"),
        )
        .await;
        return None;
    }

    let cell_ast = match parse::parse_cell(cell) {
        Ok(cell_ast) => cell_ast,
        Err(error) => {
            show_message_in_editor_and_gui(
                client,
                gui_client,
                MessageType::ERROR,
                format!("Open cell is invalid: {error}"),
            )
            .await;
            return None;
        }
    };
    if !cell_ast.args.kwargs.is_empty() {
        show_message_in_editor_and_gui(
            client,
            gui_client,
            MessageType::ERROR,
            "Open cell does not support keyword arguments yet",
        )
        .await;
        return None;
    }
    let Some(args) = cell_ast
        .args
        .posargs
        .iter()
        .map(CellArg::from_literal)
        .collect::<Option<Vec<_>>>()
    else {
        show_message_in_editor_and_gui(
            client,
            gui_client,
            MessageType::ERROR,
            "Open cell arguments must be integer, float, boolean, or empty-list literals",
        )
        .await;
        return None;
    };
    let cell_path = cell_ast
        .func
        .path
        .iter()
        .map(|ident| ident.name)
        .collect::<Vec<_>>();

    Some(compile::dynamic_compile_with_gds(
        ast,
        CompileInput {
            cell: &cell_path,
            args,
            lyp_file: lyp,
        },
        gds_imports,
    ))
}

impl StateMut {
    fn diagnostics(&self) -> IndexMap<Uri, Vec<Diagnostic>> {
        let mut diagnostics = IndexMap::new();
        let Some(root_dir) = &self.root_dir else {
            return diagnostics;
        };
        if let Some(o) = &self.compile_output {
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
                if let Some(ast) = self.ast.values().find(|ast| ast.path == span.path)
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

    async fn compile(&mut self, client: &Client, update: bool) {
        let Some(root_dir) = &self.root_dir else {
            return;
        };
        let manifest_path = root_dir.join("Argon.toml");
        self.config = if manifest_path.is_file() {
            match Library::load(&manifest_path) {
                Ok(config) => Some(config),
                Err(error) => {
                    show_message_in_editor_and_gui(
                        client,
                        self.gui_client.clone(),
                        MessageType::ERROR,
                        error.to_string(),
                    )
                    .await;
                    self.compile_output = None;
                    return;
                }
            }
        } else {
            None
        };
        let lyp = self.config.as_ref().and_then(|config| config.lyp.clone());
        let dependencies = self
            .config
            .as_ref()
            .map(|config| config.dependencies.clone())
            .unwrap_or_default();
        let gds_imports = self
            .config
            .as_ref()
            .map(|config| {
                config
                    .gds
                    .iter()
                    .map(|(name, path)| (name.clone(), path.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let analysis = compile::analyze_workspace(parse::parse_workspace_with_std_deps_and_gds(
            root_dir.join("lib.ar"),
            dependencies,
            gds_imports.clone(),
        ));
        self.ast = analysis.ast;

        let o = if let Some(ast) = analysis.typed_ast {
            if !analysis.errors.is_empty() {
                Some(CompileOutput::StaticErrors(StaticErrorCompileOutput {
                    errors: analysis.errors,
                }))
            } else if let Some(cell) = &self.cell {
                let Some(lyp) = lyp.as_deref() else {
                    let message = if manifest_path.is_file() {
                        format!(
                            "`{}` does not set `lyp`; add `lyp = \"path/to/layers.lyp\"`",
                            manifest_path.display()
                        )
                    } else {
                        format!(
                            "no library manifest found at `{}`; create it and set `lyp = \"path/to/layers.lyp\"`",
                            manifest_path.display()
                        )
                    };
                    show_message_in_editor_and_gui(
                        client,
                        self.gui_client.clone(),
                        MessageType::ERROR,
                        format!("Could not open cell: {message}"),
                    )
                    .await;
                    self.compile_output = None;
                    return;
                };
                compile_open_cell(
                    &ast,
                    cell,
                    lyp,
                    &gds_imports,
                    client,
                    self.gui_client.clone(),
                )
                .await
            } else {
                None
            }
        } else {
            Some(CompileOutput::FatalParseErrors)
        };
        self.compile_output = o;
        if !update && let Some(output) = &self.compile_output {
            for message in blocking_compile_error_messages(output) {
                show_message_in_editor_and_gui(
                    client,
                    self.gui_client.clone(),
                    MessageType::ERROR,
                    format!("Could not open cell: {message}"),
                )
                .await;
            }
        }
        let mut diagnostics = self.diagnostics();
        let previous = std::mem::replace(&mut self.prev_diagnostics, diagnostics.clone());
        for uri in previous.into_keys() {
            diagnostics.entry(uri).or_default();
        }
        for (uri, diagnostics) in diagnostics {
            // TODO: potentially add version number
            client.publish_diagnostics(uri, diagnostics, None).await;
        }
        if let Some(o) = &self.compile_output
            && let Some(gui_client) = self.gui_client.as_mut()
            && let Err(e) = gui_client
                .open_cell(context::current(), o.clone(), update)
                .await
        {
            client
                .show_message(MessageType::ERROR, format!("{e}"))
                .await;
            self.gui_client = None;
        }
    }
}

#[derive(Debug, Clone)]
pub struct State {
    server_addr: SocketAddr,
    editor_client: Client,
    state_mut: Arc<Mutex<StateMut>>,
}

impl State {
    fn new(server_addr: SocketAddr, editor_client: Client) -> Self {
        Self {
            server_addr,
            editor_client,
            state_mut: Default::default(),
        }
    }
}

#[derive(Debug, Clone)]
struct Backend {
    state: State,
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
struct ForceSave;

impl Request for ForceSave {
    type Params = PathBuf;
    type Result = ();

    const METHOD: &'static str = "custom/forceSave";
}

#[derive(Debug, Clone, Copy)]
struct FocusEditor;

impl Request for FocusEditor {
    type Params = Option<String>;
    type Result = ();

    const METHOD: &'static str = "custom/focusEditor";
}

impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        #[allow(deprecated)]
        {
            self.state.state_mut.lock().await.root_dir = params
                .root_uri
                .and_then(|root| root.to_file_path().map(|path| path.into_owned()));
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
        self.compile().await;
    }

    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let mut state_mut = self.state.state_mut.lock().await;
        let doc = Document::new(params.text_document.text, params.text_document.version);
        state_mut.editor_files.insert(params.text_document.uri, doc);
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let mut state_mut = self.state.state_mut.lock().await;
        if let Some(doc) = state_mut.editor_files.get_mut(&params.text_document.uri) {
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
        } else {
            // optional: log error, or handle missing document
        }
    }

    async fn did_save(&self, _: DidSaveTextDocumentParams) {
        self.compile().await;
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let mut state_mut = self.state.state_mut.lock().await;
        state_mut
            .editor_files
            .swap_remove(&params.text_document.uri);
    }

    async fn shutdown(&self) -> Result<()> {
        if let Some(gui) = self.state.state_mut.lock().await.gui.as_mut() {
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
struct SetParams {
    kv: String,
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
        let mut state_mut = self.state.state_mut.lock().await;
        if let Some(gui_client) = &state_mut.gui_client {
            self.state
                .editor_client
                .show_message(MessageType::LOG, "Attempting to contact existing GUI...")
                .await;
            if gui_client.activate(context::current()).await.is_ok() {
                self.state
                    .editor_client
                    .show_message(MessageType::LOG, "Connected to existing GUI!")
                    .await;
                return Ok(());
            }
            self.state
                .editor_client
                .show_message(MessageType::LOG, "Failed to contact existing GUI.")
                .await;
        }
        if let Some(mut gui) = state_mut.gui.take() {
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
                        tokio::spawn(async move {
                            let reader = BufReader::new(stderr);
                            let mut lines = reader.lines();

                            while let Ok(Some(line)) = lines.next_line().await {
                                error!("{}", line);
                                state
                                    .editor_client
                                    .show_message(MessageType::ERROR, line)
                                    .await;
                            }
                        });
                    }
                    state.state_mut.lock().await.gui = Some(child);
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

    /// Compiles a cell.
    async fn compile_cell(&self, cell: impl Into<String>) {
        let mut state_mut = self.state.state_mut.lock().await;
        state_mut.cell = Some(cell.into());
        state_mut.compile(&self.state.editor_client, false).await;
    }

    /// Compiles the current workspace and the open cell if it exists.
    async fn compile(&self) {
        let mut state_mut = self.state.state_mut.lock().await;
        state_mut.compile(&self.state.editor_client, true).await;
    }

    async fn open_cell(&self, params: OpenCellParams) -> Result<()> {
        let state = self.state.clone();
        state
            .editor_client
            .show_message(MessageType::LOG, &format!("cell {}", params.cell))
            .await;
        self.compile_cell(params.cell).await;
        Ok(())
    }

    async fn instantiate(&self, params: InstantiateParams) -> Result<()> {
        let Some(gui_client) = self.state.state_mut.lock().await.gui_client.clone() else {
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    "Start the Argon GUI before placing an instance",
                )
                .await;
            return Ok(());
        };
        let selected_scope = match gui_client.selected_scope(context::current()).await {
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
                self.state.state_mut.lock().await.gui_client = None;
                self.state
                    .editor_client
                    .show_message(
                        MessageType::ERROR,
                        format!(
                            "The Argon GUI is not connected; start it before placing an instance ({error})"
                        ),
                    )
                    .await;
                return Ok(());
            }
        };
        let state_mut = self.state.state_mut.lock().await;
        if !rpc::editor_buffers_are_current(&state_mut) {
            drop(state_mut);
            self.state
                .editor_client
                .show_message(MessageType::ERROR, rpc::OUT_OF_SYNC_MESSAGE)
                .await;
            return Ok(());
        }
        let selected_scope = Some(selected_scope).filter(|span| {
            state_mut
                .ast
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
        let Some((module_path, ast)) = state_mut
            .ast
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
                state_mut
                    .ast
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
        let mut preview_ast = state_mut.ast.clone();
        preview_ast.insert(module_path, preview_module);
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
        let Some(open_cell) = state_mut.cell.clone() else {
            self.state
                .editor_client
                .show_message(MessageType::ERROR, "Open a cell before placing an instance")
                .await;
            return Ok(());
        };
        let Some(lyp) = state_mut
            .config
            .as_ref()
            .and_then(|config| config.lyp.clone())
        else {
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    "The Argon library does not configure an LYP file",
                )
                .await;
            return Ok(());
        };
        let gds_imports = state_mut
            .config
            .as_ref()
            .map(|config| {
                config
                    .gds
                    .iter()
                    .map(|(name, path)| (name.clone(), path.clone()))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        let Some(compiled) = compile_open_cell(
            &typed_ast,
            &open_cell,
            &lyp,
            &gds_imports,
            &self.state.editor_client,
            state_mut.gui_client.clone(),
        )
        .await
        else {
            return Ok(());
        };
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
        drop(state_mut);
        if let Err(error) = gui_client.place_instance(context::current(), preview).await {
            self.state
                .editor_client
                .show_message(
                    MessageType::ERROR,
                    format!("Could not contact the GUI: {error}"),
                )
                .await;
        }
        Ok(())
    }

    async fn set(&self, params: SetParams) -> Result<()> {
        let Some((key, value)) = parse_setting(&params.kv) else {
            self.state
                .editor_client
                .show_message(MessageType::ERROR, "Expected a setting in KEY VALUE form")
                .await;
            return Ok(());
        };
        let (key, value) = (key.to_owned(), value.to_owned());
        let state = self.state.clone();
        tokio::spawn(async move {
            let mut state_mut = state.state_mut.lock().await;
            if let Some(client) = state_mut.gui_client.as_mut()
                && let Err(e) = client.set(context::current(), key, value).await
            {
                state
                    .editor_client
                    .show_message(MessageType::ERROR, format!("{e}"))
                    .await;
                state_mut.gui_client = None;
            }
        });
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
    .custom_method("custom/set", Backend::set)
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

    use argonc::{
        compile::{self, CellArg, CompileInput, CompileOutput, ExecErrorCompileOutput},
        parse,
    };

    use super::{DEFAULT_LOG_LEVEL, parse_config, parse_setting, preview_instance_cell};

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
    fn open_cell_accepts_boolean_literals() {
        let call = parse::parse_cell("fet1v8(true, 150., 5)").expect("cell should parse");
        let arg = CellArg::from_literal(&call.args.posargs[0]).expect("boolean should convert");
        assert!(matches!(arg, CellArg::Bool(true)));
    }

    #[test]
    fn setting_requires_a_key_and_value() {
        assert_eq!(parse_setting("grid 10"), Some(("grid", "10")));
        assert_eq!(parse_setting("grid   10 20"), Some(("grid", "10 20")));
        assert_eq!(parse_setting("grid"), None);
        assert_eq!(parse_setting("grid   "), None);
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
        let lyp = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/lyp/basic.lyp");
        let output = compile::dynamic_compile(
            &typed,
            CompileInput {
                cell: &["top"],
                args: vec![CellArg::Float(42.)],
                lyp_file: &lyp,
            },
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
