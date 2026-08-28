use std::{
    path::PathBuf,
    sync::{Arc, mpsc},
    thread,
};

use arc::Library;
use argonc::{
    WorkspaceConfig,
    compile::{CompileOutput, StaticErrorCompileOutput},
    incremental::IncrementalCompiler,
    nav::NavIndex,
    parse::WorkspaceParseAst,
};
use tokio::sync::oneshot;

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
    /// Position-indexed definitions and references. `None` only until the
    /// workspace has type-checked once; after that the session keeps serving
    /// the last index that had content.
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
    },
}

/// A serial command queue whose dedicated thread exclusively owns the
/// process-local incremental compilation session.
#[derive(Clone, Debug)]
pub(crate) struct CompilerWorker {
    commands: mpsc::Sender<Command>,
}

impl CompilerWorker {
    pub(crate) fn new() -> Self {
        let (commands, receiver) = mpsc::channel();
        thread::Builder::new()
            .name("argon-compiler".to_owned())
            .spawn(move || run(receiver))
            .expect("spawn incremental compiler worker");
        Self { commands }
    }

    pub(crate) fn set_source_text(&self, path: PathBuf, contents: String) {
        let _ = self.commands.send(Command::SetSource { path, contents });
    }

    pub(crate) fn remove_source(&self, path: PathBuf) {
        let _ = self.commands.send(Command::RemoveSource(path));
    }

    pub(crate) async fn compile(&self, request: CompileRequest) -> Option<CompileResult> {
        let (response, result) = oneshot::channel();
        self.commands
            .send(Command::Compile { request, response })
            .ok()?;
        result.await.ok()
    }
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
                let _ = response.send(compile(&mut compiler, request));
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
