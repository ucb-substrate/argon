use std::{path::PathBuf, sync::mpsc, thread};

use arc::Library;
use argonc::{
    compile::{CompileOutput, StaticErrorCompileOutput},
    incremental::IncrementalCompiler,
    parse::WorkspaceParseAst,
};
use tokio::sync::oneshot;

use crate::{open_cell_input, workspace_config};

#[derive(Debug)]
pub(crate) struct CompileRequest {
    pub(crate) revision: u64,
    pub(crate) root_dir: PathBuf,
    pub(crate) cell: Option<String>,
}

#[derive(Debug)]
pub(crate) struct CompileResult {
    pub(crate) revision: u64,
    pub(crate) root_dir: PathBuf,
    pub(crate) config: Option<Library>,
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
    let manifest_path = request.root_dir.join("Argon.toml");
    let config = if manifest_path.is_file() {
        match Library::load(&manifest_path) {
            Ok(config) => Some(config),
            Err(error) => {
                return CompileResult {
                    revision: request.revision,
                    root_dir: request.root_dir,
                    config: None,
                    ast: WorkspaceParseAst::default(),
                    output: None,
                    messages: vec![error.to_string()],
                };
            }
        }
    } else {
        None
    };
    let lyp = config.as_ref().and_then(|config| config.lyp.clone());
    let workspace = workspace_config(request.root_dir.join("lib.ar"), config.as_ref());
    let analysis = compiler.analyze_workspace(&workspace);
    let ast = analysis.ast;
    let mut messages = Vec::new();

    let output = if analysis.typed_ast.is_some() {
        if !analysis.errors.is_empty() {
            Some(CompileOutput::StaticErrors(StaticErrorCompileOutput {
                errors: analysis.errors,
            }))
        } else if let Some(cell) = request.cell {
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
                messages.push(format!("Could not open cell: {message}"));
                return CompileResult {
                    revision: request.revision,
                    root_dir: request.root_dir,
                    config,
                    ast,
                    output: None,
                    messages,
                };
            };
            match open_cell_input(&cell, lyp) {
                Ok((cell_path, args)) => Some(compiler.compile_cell(&workspace, &cell_path, args)),
                Err(message) => {
                    messages.push(message);
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
        revision: request.revision,
        root_dir: request.root_dir,
        config,
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
        let lyp = directory.path().join("layers.lyp");
        std::fs::copy(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/lyp/basic.lyp"),
            &lyp,
        )
        .unwrap();
        std::fs::write(&root, "cell top() {}\n").unwrap();
        std::fs::write(
            directory.path().join("Argon.toml"),
            "name = \"worker-test\"\nlyp = \"layers.lyp\"\n",
        )
        .unwrap();

        let worker = CompilerWorker::new();
        worker.set_source_text(root.clone(), "cell top() { missing; }\n".to_owned());
        let first = worker
            .compile(CompileRequest {
                revision: 1,
                root_dir: directory.path().to_path_buf(),
                cell: Some("top()".to_owned()),
            })
            .await
            .unwrap();
        assert_eq!(first.revision, 1);
        assert!(matches!(first.output, Some(CompileOutput::StaticErrors(_))));

        worker.set_source_text(root, "cell top() {}\n".to_owned());
        let second = worker
            .compile(CompileRequest {
                revision: 2,
                root_dir: directory.path().to_path_buf(),
                cell: Some("top()".to_owned()),
            })
            .await
            .unwrap();
        assert_eq!(second.revision, 2);
        assert!(matches!(second.output, Some(CompileOutput::Valid(_))));
    }
}
