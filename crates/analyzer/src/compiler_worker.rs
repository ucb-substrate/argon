use std::{
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
};

use arc::Library;
use argonc::{
    COMPILE_STACK_SIZE, WorkspaceConfig,
    compile::{CompileOutput, StaticErrorCompileOutput},
    incremental::IncrementalCompiler,
    parse::WorkspaceParseAst,
};
use tokio::sync::oneshot;
use tracing::error;

use crate::workspace_config;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CompileIdentity {
    pub(crate) revision: u64,
    pub(crate) cell: Option<String>,
}

#[derive(Debug)]
pub(crate) struct CompileRequest {
    pub(crate) identity: CompileIdentity,
    pub(crate) root_dir: PathBuf,
}

#[derive(Debug)]
pub(crate) struct CompileResult {
    pub(crate) identity: CompileIdentity,
    pub(crate) root_dir: PathBuf,
    pub(crate) config: WorkspaceConfig,
    pub(crate) ast: WorkspaceParseAst,
    pub(crate) output: Option<CompileOutput>,
    pub(crate) messages: Vec<String>,
}

enum Command {
    SetSource {
        path: PathBuf,
        contents: String,
    },
    RemoveSource(PathBuf),
    Compile {
        request: CompileRequest,
        response: oneshot::Sender<CompileResult>,
    },
}

/// A serial command queue whose dedicated thread exclusively owns the
/// process-local incremental compilation session.
///
/// The thread is respawned if it dies. A compiler panic would otherwise end
/// compilation for the rest of the editor session: the receiver would drop,
/// and every later send fails silently, so the editor would simply stop
/// getting diagnostics with no indication why.
#[derive(Clone, Debug)]
pub(crate) struct CompilerWorker {
    commands: Arc<Mutex<mpsc::Sender<Command>>>,
}

impl CompilerWorker {
    pub(crate) fn new() -> Self {
        Self {
            commands: Arc::new(Mutex::new(spawn_worker())),
        }
    }

    /// Sends `command`, respawning the worker once if the current one has died.
    fn send(&self, command: Command) -> bool {
        let mut commands = match self.commands.lock() {
            Ok(commands) => commands,
            Err(poisoned) => poisoned.into_inner(),
        };
        let Err(returned) = commands.send(command) else {
            return true;
        };
        error!("incremental compiler worker died; restarting it");
        *commands = spawn_worker();
        commands.send(returned.0).is_ok()
    }

    pub(crate) fn set_source_text(&self, path: PathBuf, contents: String) {
        self.send(Command::SetSource { path, contents });
    }

    pub(crate) fn remove_source(&self, path: PathBuf) {
        self.send(Command::RemoveSource(path));
    }

    pub(crate) async fn compile(&self, request: CompileRequest) -> Option<CompileResult> {
        let (response, result) = oneshot::channel();
        if !self.send(Command::Compile { request, response }) {
            return None;
        }
        result.await.ok()
    }
}

fn spawn_worker() -> mpsc::Sender<Command> {
    let (commands, receiver) = mpsc::channel();
    thread::Builder::new()
        .name("argon-compiler".to_owned())
        // Compilation recurses natively for inlined `fn` calls and nested cell
        // instantiation; on the default stack a deep hierarchy aborts the whole
        // language server rather than reporting a recursion-limit diagnostic.
        .stack_size(COMPILE_STACK_SIZE)
        .spawn(move || run(receiver))
        .expect("spawn incremental compiler worker");
    commands
}

fn run(commands: mpsc::Receiver<Command>) {
    let mut compiler = IncrementalCompiler::new();
    while let Ok(command) = commands.recv() {
        match command {
            Command::SetSource { path, contents } => {
                compiler.set_source_text(path, contents);
            }
            Command::RemoveSource(path) => {
                compiler.remove_source(&path);
            }
            Command::Compile { request, response } => {
                // An internal compiler error must fail one request, not the
                // session: the panic is reported as a message on this result
                // and the worker keeps its incremental state.
                let identity = request.identity.clone();
                let root_dir = request.root_dir.clone();
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    compile(&mut compiler, request)
                }))
                .unwrap_or_else(|_| {
                    error!("internal compiler error while compiling {root_dir:?}");
                    // The panic may have left the incremental session
                    // half-updated, so start a fresh one: losing the caches
                    // costs a rebuild, keeping poisoned state costs
                    // correctness.
                    compiler = IncrementalCompiler::new();
                    CompileResult {
                        identity,
                        config: WorkspaceConfig::new(root_dir.join("lib.ar")),
                        root_dir,
                        ast: WorkspaceParseAst::default(),
                        output: None,
                        messages: vec![
                            "internal compiler error; see the Argon log for details".to_owned(),
                        ],
                    }
                });
                let _ = response.send(result);
            }
        }
    }
}

fn compile(compiler: &mut IncrementalCompiler, request: CompileRequest) -> CompileResult {
    let CompileRequest { identity, root_dir } = request;
    let manifest_path = root_dir.join("Argon.toml");
    let library = if manifest_path.is_file() {
        match Library::load(&manifest_path) {
            Ok(library) => Some(library),
            Err(error) => {
                return CompileResult {
                    identity,
                    config: WorkspaceConfig::new(root_dir.join("lib.ar")),
                    root_dir,
                    ast: WorkspaceParseAst::default(),
                    output: None,
                    messages: vec![error.to_string()],
                };
            }
        }
    } else {
        None
    };
    let workspace = workspace_config(root_dir.join("lib.ar"), library.as_ref());
    let analysis = compiler.analyze_workspace(&workspace);
    let ast = analysis.ast;
    let mut messages = Vec::new();

    let output = if analysis.typed_ast.is_some() {
        if !analysis.errors.is_empty() {
            Some(CompileOutput::StaticErrors(StaticErrorCompileOutput {
                errors: analysis.errors,
            }))
        } else if let Some(cell) = identity.cell.as_deref() {
            if workspace.tech.is_none() {
                let message = if manifest_path.is_file() {
                    format!(
                        "`{}` does not set `tech`; add `tech = \"path/to/tech.toml\"`",
                        manifest_path.display()
                    )
                } else {
                    format!(
                        "no library manifest found at `{}`; create it and set `tech = \"path/to/tech.toml\"`",
                        manifest_path.display()
                    )
                };
                messages.push(format!("Could not open cell: {message}"));
                return CompileResult {
                    identity,
                    root_dir,
                    config: workspace,
                    ast,
                    output: None,
                    messages,
                };
            }
            match compiler.compile_invocation(&workspace, cell) {
                Ok(output) => Some(output),
                Err(error) => {
                    messages.push(format!("Open cell is invalid: {error}"));
                    None
                }
            }
        } else {
            None
        }
    } else {
        Some(CompileOutput::FatalParseErrors)
    };

    CompileResult {
        identity,
        root_dir,
        config: workspace,
        ast,
        output,
        messages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn source_updates_and_compiles_are_processed_in_queue_order() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let tech = directory.path().join("tech.toml");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml"),
            &tech,
        )
        .unwrap();
        std::fs::write(&root, "cell top() {}\n").unwrap();
        std::fs::write(
            directory.path().join("Argon.toml"),
            "name = \"worker-test\"\ntech = \"tech.toml\"\n",
        )
        .unwrap();

        let worker = CompilerWorker::new();
        worker.set_source_text(root.clone(), "cell top() { missing; }\n".to_owned());
        let first = worker
            .compile(CompileRequest {
                identity: CompileIdentity {
                    revision: 1,
                    cell: Some("top()".to_owned()),
                },
                root_dir: directory.path().to_path_buf(),
            })
            .await
            .unwrap();
        assert_eq!(first.identity.revision, 1);
        assert!(matches!(first.output, Some(CompileOutput::StaticErrors(_))));

        worker.set_source_text(root, "cell top() {}\n".to_owned());
        let second = worker
            .compile(CompileRequest {
                identity: CompileIdentity {
                    revision: 2,
                    cell: Some("top()".to_owned()),
                },
                root_dir: directory.path().to_path_buf(),
            })
            .await
            .unwrap();
        assert_eq!(second.identity.revision, 2);
        assert!(matches!(second.output, Some(CompileOutput::Valid(_))));
    }
}
