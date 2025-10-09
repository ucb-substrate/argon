pub mod document;
pub mod import;
pub mod rpc;

use std::{
    cmp::Reverse,
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    process::Stdio,
    sync::Arc,
};

use compiler::{
    ast::{Expr, Span},
    compile::{
        self, CellArg, CompileInput, CompileOutput, ExecErrorCompileOutput,
        StaticErrorCompileOutput,
    },
    config::{Config, parse_config},
    parse::{self, WorkspaceParseAst},
};
use futures::prelude::*;
use indexmap::IndexMap;
use itertools::Itertools;
use portpicker::{is_free, pick_unused_port};
use rpc::{GuiToLsp, LspServer, LspToGuiClient};
use serde::{Deserialize, Serialize};
use tarpc::{
    context,
    server::{Channel, incoming::Incoming},
    tokio_serde::formats::Json,
};
use tokio::{process::Command, sync::Mutex};
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::{request::Request, *};
use tower_lsp::{Client, LanguageServer, LspService, Server};

use crate::{
    document::{Document, DocumentChange},
    import::ScopeAnnotationPass,
};

// TODO: finer-grained synchronization?
// TODO: Verify synchronization between GUI and editor files when appropriate.
#[derive(Debug, Clone, Default)]
pub struct StateMut {
    root_dir: Option<PathBuf>,
    config: Option<Config>,
    ast: WorkspaceParseAst,
    prev_diagnostics: IndexMap<Url, Vec<Diagnostic>>,
    compile_output: Option<CompileOutput>,
    cell: Option<String>,
    gui_client: Option<LspToGuiClient>,
    editor_files: IndexMap<Url, Document>,
}

impl StateMut {
    fn diagnostics(&self) -> IndexMap<Url, Vec<Diagnostic>> {
        let mut diagnostics = IndexMap::new();
        if let Some(o) = &self.compile_output {
            let errs = match o {
                CompileOutput::FatalParseErrors => {
                    vec![(
                        Span {
                            path: self.root_dir.as_ref().unwrap().join("lib.ar"),
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
                    .filter_map(|e| Some((e.span.as_ref()?.clone(), format!("{}", e.kind))))
                    .collect(),
                CompileOutput::Valid(_) => vec![],
            };
            for (span, message) in errs {
                let url = Url::from_file_path(&span.path).unwrap();
                if let Some(ast) = self.ast.values().find(|ast| ast.path == span.path) {
                    let doc = Document::new(&ast.text, 0);
                    diagnostics
                        .entry(url)
                        .or_insert_with(Vec::new)
                        .push(Diagnostic {
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

    async fn compile(&mut self, client: &Client) {
        // TODO: un-hardcode this.
        let root_dir = self.root_dir.as_ref().unwrap();
        self.config = parse_config(root_dir.join("Argon.toml")).ok();
        let lyp = self
            .config
            .as_ref()
            .map(|config| {
                if config.lyp.is_relative() {
                    root_dir.join(&config.lyp)
                } else {
                    config.lyp.clone()
                }
            })
            .unwrap_or_else(|| {
                PathBuf::from(concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/../../core/compiler/examples/lyp/basic.lyp"
                ))
            });
        let parse_output = parse::parse_workspace_with_std(root_dir.join("lib.ar"));
        let parse_errs = parse_output.static_errors();
        let ast = parse_output.best_effort_ast();
        self.ast = ast;
        let static_output = compile::static_compile(&self.ast);
        // If GUI is connected, must annotate scopes.
        if self.gui_client.is_some() {
            for (_, ast) in &self.ast {
                let scope_annotation = ScopeAnnotationPass::new(ast);
                let mut text_edits = scope_annotation.execute();
                text_edits.sort_by_key(|edit| Reverse(edit.range.start));
                if !text_edits.is_empty() {
                    client
                        .apply_edit(WorkspaceEdit {
                            changes: Some(HashMap::from_iter([(
                                Url::from_file_path(&ast.path).unwrap(),
                                text_edits,
                            )])),
                            document_changes: None,
                            change_annotations: None,
                        })
                        .await
                        .unwrap();

                    client
                        .send_request::<ForceSave>(ast.path.clone())
                        .await
                        .unwrap();

                    // `compile` will be reinvoked upon save.
                    return;
                }
            }
        }

        let mut o = if let Some((ast, static_output)) = static_output {
            if !static_output.errors.is_empty() {
                Some(CompileOutput::StaticErrors(static_output))
            } else if let Some(cell) = &self.cell
                && let Ok(cell_ast) = parse::parse_cell(cell)
            {
                Some(compile::dynamic_compile(
                    &ast,
                    CompileInput {
                        cell: &cell_ast
                            .func
                            .path
                            .iter()
                            .map(|ident| ident.name)
                            .collect_vec(),
                        args: cell_ast
                            .args
                            .posargs
                            .iter()
                            .map(|arg| match arg {
                                Expr::FloatLiteral(float_literal) => {
                                    CellArg::Float(float_literal.value)
                                }
                                Expr::IntLiteral(int_literal) => CellArg::Int(int_literal.value),
                                _ => panic!("must be int or float literal for now"),
                            })
                            .collect(),
                        lyp_file: &lyp,
                    },
                ))
            } else {
                None
            }
        } else {
            Some(CompileOutput::FatalParseErrors)
        };
        match &mut o {
            Some(CompileOutput::StaticErrors(e)) => e.errors.extend(parse_errs),
            o => {
                if !parse_errs.is_empty() {
                    *o = Some(CompileOutput::StaticErrors(StaticErrorCompileOutput {
                        errors: parse_errs,
                    }));
                }
            }
        }
        self.compile_output = o;
        let mut tmp = self.diagnostics();
        let mut diagnostics = tmp.clone();
        std::mem::swap(&mut self.prev_diagnostics, &mut tmp);
        for (path, _) in tmp {
            diagnostics.entry(path).or_default();
        }
        for (uri, diags) in diagnostics {
            // TODO: potentially add version number
            client.publish_diagnostics(uri, diags, None).await;
        }
        if let Some(o) = &self.compile_output
            && let Some(client) = self.gui_client.as_mut()
        {
            client
                .open_cell(context::current(), o.clone())
                .await
                .unwrap();
        } else {
            client
                .show_message(MessageType::WARNING, "No GUI connected")
                .await;
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
struct ForceSave;

impl Request for ForceSave {
    type Params = PathBuf;
    type Result = ();

    const METHOD: &'static str = "custom/forceSave";
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        self.state.state_mut.lock().await.root_dir =
            params.root_uri.map(|root| root.to_file_path().unwrap());
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
        self.state
            .editor_client
            .show_message(MessageType::INFO, "Starting the GUI...")
            .await;
        let server_addr = self.state.server_addr;

        tokio::spawn(async move {
            Command::new(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/../../target/debug/gui"
            ))
            .arg(format!("{server_addr}"))
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
            .wait()
            .await
            .unwrap();
        });

        Ok(())
    }

    /// Compiles a cell.
    async fn compile_cell(&self, cell: impl Into<String>) {
        let mut state_mut = self.state.state_mut.lock().await;
        state_mut.cell = Some(cell.into());
        state_mut.compile(&self.state.editor_client).await;
    }

    /// Compiles the current workspace and the open cell if it exists.
    async fn compile(&self) {
        let mut state_mut = self.state.state_mut.lock().await;
        state_mut.compile(&self.state.editor_client).await;
    }

    async fn open_cell(&self, params: OpenCellParams) -> Result<()> {
        let state = self.state.clone();
        state
            .editor_client
            .show_message(MessageType::INFO, &format!("cell {}", params.cell))
            .await;
        let self_clone = self.clone();
        tokio::spawn(async move {
            self_clone.compile_cell(params.cell).await;
        });
        Ok(())
    }

    async fn set(&self, params: SetParams) -> Result<()> {
        let state = self.state.clone();
        // TODO: Error handling.
        let (k, v) = params.kv.split_once(" ").unwrap();
        let (k, v) = (k.to_string(), v.to_string());
        tokio::spawn(async move {
            if let Some(client) = state.state_mut.lock().await.gui_client.as_mut() {
                client.set(context::current(), k, v).await.unwrap();
            }
        });
        Ok(())
    }
}

async fn spawn(fut: impl Future<Output = ()> + Send + 'static) {
    tokio::spawn(fut);
}

pub async fn main() {
    // Start server for communication with GUI.
    let port = 12345; // for debugging
    let port = if is_free(port) {
        port
    } else {
        loop {
            if let Some(port) = pick_unused_port() {
                break port;
            }
        }
    };
    let server_addr = (IpAddr::V4(Ipv4Addr::LOCALHOST), port).into();

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
    let state = ext_state.unwrap();

    // JSON transport is provided by the json_transport tarpc module. It makes it easy
    // to start up a serde-powered json serialization strategy over TCP.
    let mut listener = tarpc::serde_transport::tcp::listen(&server_addr, Json::default)
        .await
        .unwrap();
    listener.config_mut().max_frame_length(usize::MAX);
    let state_clone = state.clone();
    tokio::spawn(async move {
        listener
            // Ignore accept errors.
            .filter_map(|r| futures::future::ready(r.ok()))
            .map(tarpc::server::BaseChannel::with_defaults)
            // Limit channels to 1 per IP.
            .max_channels_per_key(1, |t| t.transport().peer_addr().unwrap().ip())
            // serve is generated by the service attribute. It takes as input any type implementing
            // the generated World trait.
            .map(|channel| {
                let server = LspServer {
                    state: state_clone.clone(),
                };
                channel.execute(server.serve()).for_each(spawn)
            })
            // Max 10 channels.
            .buffer_unordered(10)
            .for_each(|_| async {})
            .await;
    });

    state
        .editor_client
        .show_message(
            MessageType::INFO,
            format!("Server listening on port {port}"),
        )
        .await;

    // Start actual LSP server.
    Server::new(stdin, stdout, socket).serve(service).await;
}
