use std::{
    panic::{self, AssertUnwindSafe},
    path::PathBuf,
    sync::{Arc, Mutex, mpsc},
    thread,
};

use arc::Library;
use argonc::{
    COMPILE_STACK_SIZE, WorkspaceConfig,
    compile::{CellId, CompileOutput, StaticErrorCompileOutput},
    incremental::IncrementalCompiler,
    nav::NavIndex,
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
    pub(crate) preview_cell: Option<CellId>,
}

pub(crate) struct CompilePreview {
    pub(crate) identity: CompileIdentity,
    pub(crate) previous_cell: CellId,
    pub(crate) output: CompileOutput,
}

#[derive(Debug)]
pub(crate) struct CompileResult {
    pub(crate) identity: CompileIdentity,
    pub(crate) root_dir: PathBuf,
    pub(crate) config: WorkspaceConfig,
    pub(crate) ast: WorkspaceParseAst,
    /// Position-indexed definitions and references. `None` until the workspace
    /// has type-checked once — after that the session keeps serving the last
    /// index that had content — and on the paths that answer without reaching
    /// the compiler at all: a manifest that would not load, or an internal
    /// compiler error. None of those means "there is no index".
    pub(crate) nav: Option<Arc<NavIndex>>,
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
        preview: Option<oneshot::Sender<CompilePreview>>,
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

    #[cfg(test)]
    pub(crate) async fn compile(&self, request: CompileRequest) -> Option<CompileResult> {
        let (_, result) = self.compile_streamed(request);
        result.await.ok()
    }

    pub(crate) fn compile_streamed(
        &self,
        request: CompileRequest,
    ) -> (
        Option<oneshot::Receiver<CompilePreview>>,
        oneshot::Receiver<CompileResult>,
    ) {
        let (response, result) = oneshot::channel();
        let (preview, preview_result) = if request.preview_cell.is_some() {
            let (sender, receiver) = oneshot::channel();
            (Some(sender), Some(receiver))
        } else {
            (None, None)
        };
        self.send(Command::Compile {
            request,
            response,
            preview,
        });
        (preview_result, result)
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
            Command::Compile {
                request,
                response,
                preview,
            } => {
                // GUI drawing can enqueue many source revisions faster than a
                // large cell can compile. Apply all queued source changes, but
                // compile only the newest request: older identities cannot be
                // published once a later source revision exists. Dropping the
                // older reply channels promptly also clears their progress.
                let mut latest = (request, response, preview);
                let mut source_changes_after_latest = Vec::new();
                while let Ok(queued) = commands.try_recv() {
                    match queued {
                        Command::SetSource { .. } | Command::RemoveSource(_) => {
                            source_changes_after_latest.push(queued);
                        }
                        Command::Compile {
                            request,
                            response,
                            preview,
                        } => {
                            for change in source_changes_after_latest.drain(..) {
                                match change {
                                    Command::SetSource { path, contents } => {
                                        compiler.set_source_text(path, contents);
                                    }
                                    Command::RemoveSource(path) => {
                                        compiler.remove_source(&path);
                                    }
                                    Command::Compile { .. } => unreachable!(),
                                }
                            }
                            latest = (request, response, preview);
                        }
                    }
                }
                let (request, response, preview) = latest;
                // An internal compiler error must fail one request, not the
                // session: the panic is reported as a message on this result
                // and the worker keeps its incremental state.
                let identity = request.identity.clone();
                let root_dir = request.root_dir.clone();
                let result = panic::catch_unwind(AssertUnwindSafe(|| {
                    compile(&mut compiler, request, preview)
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
                        // The fresh session has no index yet, and this
                        // request never got far enough to ask for one.
                        // Publishing treats that as "nothing to say"
                        // rather than as an index to clear.
                        nav: None,
                        output: None,
                        messages: vec![
                            "internal compiler error; see the Argon log for details".to_owned(),
                        ],
                    }
                });
                let _ = response.send(result);
                // A source edit with no matching compile request yet belongs
                // after the request we just completed, even if it was already
                // sitting in the channel when we drained the burst.
                for change in source_changes_after_latest {
                    match change {
                        Command::SetSource { path, contents } => {
                            compiler.set_source_text(path, contents);
                        }
                        Command::RemoveSource(path) => {
                            compiler.remove_source(&path);
                        }
                        Command::Compile { .. } => unreachable!(),
                    }
                }
            }
        }
    }
}

fn compile(
    compiler: &mut IncrementalCompiler,
    request: CompileRequest,
    preview: Option<oneshot::Sender<CompilePreview>>,
) -> CompileResult {
    let CompileRequest {
        identity,
        root_dir,
        preview_cell,
    } = request;
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
                    nav: None,
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
    let nav = compiler.nav(&workspace);
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
                    nav,
                    output: None,
                    messages,
                };
            }
            if let Some((previous_cell, sender)) = preview_cell.zip(preview)
                && let Some(output) =
                    compiler.compile_cached_cell_preview(&workspace, previous_cell)
            {
                let _ = sender.send(CompilePreview {
                    identity: identity.clone(),
                    previous_cell,
                    output,
                });
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
        nav,
        output,
        messages,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn queued_burst_compiles_only_latest_source_revision() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        std::fs::write(&root, "cell top() {}\n").unwrap();
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml"),
            directory.path().join("tech.toml"),
        )
        .unwrap();
        std::fs::write(
            directory.path().join("Argon.toml"),
            "name = \"worker-burst-test\"\ntech = \"tech.toml\"\n",
        )
        .unwrap();

        let (sender, receiver) = mpsc::channel();
        let mut replies = Vec::new();
        for (revision, source) in [
            (1, "cell top() { missing; }\n"),
            (2, "cell top() { also_missing; }\n"),
            (3, "cell top() {}\n"),
        ] {
            sender
                .send(Command::SetSource {
                    path: root.clone(),
                    contents: source.to_owned(),
                })
                .unwrap();
            let (reply, result) = oneshot::channel();
            sender
                .send(Command::Compile {
                    request: CompileRequest {
                        identity: CompileIdentity {
                            revision,
                            cell: Some("top()".to_owned()),
                        },
                        root_dir: directory.path().to_path_buf(),
                        preview_cell: None,
                    },
                    response: reply,
                    preview: None,
                })
                .unwrap();
            replies.push(result);
        }
        drop(sender);
        run(receiver);
        assert!(replies.remove(0).await.is_err());
        assert!(replies.remove(0).await.is_err());
        let result = replies.remove(0).await.unwrap();
        assert_eq!(result.identity.revision, 3);
        assert!(matches!(result.output, Some(CompileOutput::Valid(_))));
    }

    #[tokio::test]
    async fn unpaired_source_change_does_not_change_previous_compile() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        std::fs::write(&root, "cell top() {}\n").unwrap();
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml"),
            directory.path().join("tech.toml"),
        )
        .unwrap();
        std::fs::write(
            directory.path().join("Argon.toml"),
            "name = \"worker-order-test\"\ntech = \"tech.toml\"\n",
        )
        .unwrap();

        let (sender, receiver) = mpsc::channel();
        sender
            .send(Command::SetSource {
                path: root.clone(),
                contents: "cell top() {}\n".to_owned(),
            })
            .unwrap();
        let (reply, result) = oneshot::channel();
        sender
            .send(Command::Compile {
                request: CompileRequest {
                    identity: CompileIdentity {
                        revision: 1,
                        cell: Some("top()".to_owned()),
                    },
                    root_dir: directory.path().to_path_buf(),
                    preview_cell: None,
                },
                response: reply,
                preview: None,
            })
            .unwrap();
        sender
            .send(Command::SetSource {
                path: root,
                contents: "cell top() { missing; }\n".to_owned(),
            })
            .unwrap();
        drop(sender);
        run(receiver);
        assert!(matches!(
            result.await.unwrap().output,
            Some(CompileOutput::Valid(_))
        ));
    }

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
                preview_cell: None,
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
                preview_cell: None,
            })
            .await
            .unwrap();
        assert_eq!(second.identity.revision, 2);
        assert!(matches!(second.output, Some(CompileOutput::Valid(_))));
    }

    #[tokio::test]
    async fn streams_parameterized_child_before_final_top_result() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let tech = directory.path().join("tech.toml");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml"),
            &tech,
        )
        .unwrap();
        std::fs::write(
            directory.path().join("Argon.toml"),
            "name = \"worker-preview-test\"\ntech = \"tech.toml\"\n",
        )
        .unwrap();
        let original = "cell leaf(w: Int) { let r = rect(\"met1\", x0=0., y0=0., x1=w as Float, y1=10.); } cell top() { let child = inst(leaf(10), x=0., y=0.); }";
        std::fs::write(&root, original).unwrap();
        let worker = CompilerWorker::new();
        let first = worker
            .compile(CompileRequest {
                identity: CompileIdentity {
                    revision: 1,
                    cell: Some("top()".to_owned()),
                },
                root_dir: directory.path().to_path_buf(),
                preview_cell: None,
            })
            .await
            .unwrap();
        let Some(CompileOutput::Valid(data)) = &first.output else {
            panic!("initial top did not compile");
        };
        let child = data
            .cells
            .iter()
            .find_map(|(id, cell)| (cell.name == "leaf").then_some(*id))
            .unwrap();
        worker.set_source_text(
            root,
            original.replace("x1=w as Float", "x1=w as Float + 1."),
        );
        let (preview, result) = worker.compile_streamed(CompileRequest {
            identity: CompileIdentity {
                revision: 2,
                cell: Some("top()".to_owned()),
            },
            root_dir: directory.path().to_path_buf(),
            preview_cell: Some(child),
        });
        let preview = preview.unwrap().await.unwrap();
        assert_eq!(preview.previous_cell, child);
        let CompileOutput::Valid(local) = &preview.output else {
            panic!("focused cell did not compile");
        };
        assert_eq!(local.cells[&local.top].name, "leaf");
        let local_x1 = local.cells[&local.top]
            .objects
            .values()
            .find_map(|object| match object {
                argonc::compile::SolvedValue::Rect(rect) => Some(rect.x1.0),
                _ => None,
            });
        assert_eq!(local_x1, Some(11.));
        let result = result.await.unwrap();
        let Some(CompileOutput::Valid(full)) = &result.output else {
            panic!("final top did not compile");
        };
        let leaf = full
            .cells
            .iter()
            .find(|(_, cell)| cell.name == "leaf")
            .unwrap()
            .0;
        let full_x1 = full.cells[leaf]
            .objects
            .values()
            .find_map(|object| match object {
                argonc::compile::SolvedValue::Rect(rect) => Some(rect.x1.0),
                _ => None,
            });
        assert_eq!(full_x1, Some(11.));
    }
}
