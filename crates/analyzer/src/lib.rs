pub mod document;
pub mod rpc;

use std::{
    fs,
    io::Write,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    process::Stdio,
    sync::Arc,
};

use arc::Library;
use argonc::{
    ast::{Span, WorkspaceAst},
    compile::{
        self, CellArg, CompileInput, CompileOutput, ExecErrorCompileOutput,
        StaticErrorCompileOutput, VarIdTyMetadata,
    },
    parse::{self, WorkspaceParseAst},
};
use futures::prelude::*;
use indexmap::IndexMap;
use rpc::{GuiClient, LangServer};
use serde::{Deserialize, Serialize};
use tarpc::{
    context,
    server::{Channel, incoming::Incoming},
    tokio_serde::formats::Json,
};
use tokio::{
    io::{AsyncBufReadExt, BufReader},
    process::{Child, Command},
    sync::Mutex,
};
use tower_lsp_server::jsonrpc::Result;
use tower_lsp_server::ls_types::{request::Request, *};
use tower_lsp_server::{Client, LanguageServer, LspService, Server};
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use crate::document::{Document, DocumentChange};

// TODO: Allow configuration via ARGON_HOME environment variable.
pub fn default_argon_home() -> Option<PathBuf> {
    homedir::my_home()
        .ok()
        .flatten()
        .map(|home| home.join(".local/state/argon"))
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

fn compile_error_messages(output: &CompileOutput) -> Vec<String> {
    match output {
        CompileOutput::FatalParseErrors => {
            vec!["fatal parse errors encountered, unable to compile".to_string()]
        }
        CompileOutput::StaticErrors(output) => output
            .errors
            .iter()
            .map(|error| error.kind.to_string())
            .collect(),
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

async fn compile_open_cell(
    ast: &WorkspaceAst<VarIdTyMetadata>,
    cell: &str,
    lyp: &Path,
    client: &Client,
) -> Option<CompileOutput> {
    if let Err(error) = argonc::layer::read_lyp(lyp) {
        client
            .show_message(MessageType::ERROR, format!("Could not open cell: {error}"))
            .await;
        return None;
    }

    let cell_ast = match parse::parse_cell(cell) {
        Ok(cell_ast) => cell_ast,
        Err(error) => {
            client
                .show_message(MessageType::ERROR, format!("Open cell is invalid: {error}"))
                .await;
            return None;
        }
    };
    if !cell_ast.args.kwargs.is_empty() {
        client
            .show_message(
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
        client
            .show_message(
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

    Some(compile::dynamic_compile(
        ast,
        CompileInput {
            cell: &cell_path,
            args,
            lyp_file: lyp,
        },
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
                    let doc = Document::new(&ast.text, 0);
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
                    client
                        .show_message(MessageType::ERROR, error.to_string())
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
        let analysis = compile::analyze_workspace(parse::parse_workspace_with_std_and_deps(
            root_dir.join("lib.ar"),
            dependencies,
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
                    client
                        .show_message(
                            MessageType::ERROR,
                            format!("Could not open cell: {message}"),
                        )
                        .await;
                    self.compile_output = None;
                    return;
                };
                compile_open_cell(&ast, cell, lyp, client).await
            } else {
                None
            }
        } else {
            Some(CompileOutput::FatalParseErrors)
        };
        self.compile_output = o;
        if !update && let Some(output) = &self.compile_output {
            for message in compile_error_messages(output) {
                client
                    .show_message(
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

struct RpcInfoFile(PathBuf);

impl Drop for RpcInfoFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn publish_rpc_port(path: PathBuf, port: u16) -> std::io::Result<RpcInfoFile> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    writeln!(file, "{port}")?;
    file.sync_all()?;
    fs::rename(&temporary, &path).inspect_err(|_| {
        let _ = fs::remove_file(&temporary);
    })?;
    Ok(RpcInfoFile(path))
}

pub async fn main(rpc_port: Option<u16>, rpc_info: Option<PathBuf>) {
    // Start server for communication with GUI.
    let port = rpc_port.unwrap_or(0);
    let mut listener =
        match tarpc::serde_transport::tcp::listen((Ipv4Addr::LOCALHOST, port), Json::default).await
        {
            Ok(listener) => listener,
            Err(error) => {
                eprintln!("failed to bind analyzer RPC server to port {port}: {error}");
                return;
            }
        };
    let server_addr = listener.local_addr();
    let _rpc_info = if let Some(path) = rpc_info {
        match publish_rpc_port(path.clone(), server_addr.port()) {
            Ok(info) => Some(info),
            Err(error) => {
                eprintln!(
                    "failed to publish analyzer RPC port to `{}`: {error}",
                    path.display()
                );
                return;
            }
        }
    } else {
        None
    };

    // Construct actual LSP server.
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let mut ext_state = None;
    let (service, socket) = LspService::build(|client| {
        let state = State::new(server_addr, client);
        ext_state = Some(state.clone());
        Backend { state }
    })
    .custom_method("custom/startGui", Backend::start_gui)
    .custom_method("custom/openCell", Backend::open_cell)
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
            // Limit channels to 1 per IP.
            .max_channels_per_key(1, |t| {
                t.transport()
                    .peer_addr()
                    .map(|address| address.ip())
                    .unwrap_or(IpAddr::V4(Ipv4Addr::LOCALHOST))
            })
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

    // TODO: Allow configuration via ARGON_HOME environment variable.
    if let Some(log_dir) = default_argon_home() {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_env("ARGON_LOG"))
            .with_writer(tracing_appender::rolling::never(log_dir, "analyzer.log"))
            .with_ansi(false)
            .init();
    }

    // Start actual LSP server.
    Server::new(stdin, stdout, socket).serve(service).await;
}

#[cfg(test)]
mod tests {
    use std::{fs, time::SystemTime};

    use argonc::{compile::CellArg, parse};

    use super::{parse_setting, publish_rpc_port};

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
    fn rpc_info_is_published_and_removed_with_its_guard() {
        let nonce = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("argon-analyzer-rpc-{nonce}"));
        let guard = publish_rpc_port(path.clone(), 43210).unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "43210\n");
        drop(guard);
        assert!(!path.exists());
    }
}
