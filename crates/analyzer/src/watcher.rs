//! Picks up workspace files that change on disk outside the editor.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use indexmap::IndexSet;
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde::{Deserialize, Serialize};
use tower_lsp_server::ls_types::{MessageType, Uri, request::Request};
use tracing::error;

use crate::{Backend, agent};
use arc::Library;

/// How long to gather a burst of events, such as an editor's atomic save.
const DEBOUNCE: Duration = Duration::from_millis(100);

/// Keeps the operating-system watch alive.
pub(crate) struct WorkspaceWatcher {
    _watcher: RecommendedWatcher,
}

impl std::fmt::Debug for WorkspaceWatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("WorkspaceWatcher")
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub(crate) struct ReloadBufferParams {
    uri: Uri,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "lowercase")]
pub(crate) enum ReloadBufferResult {
    Reloaded,
    Unchanged,
    /// The buffer has unsaved changes, so the editor kept them.
    Modified,
    Closed,
}

/// Asks the editor to reread a file that changed on disk, unless its buffer has
/// unsaved changes.
enum ReloadBuffer {}

impl Request for ReloadBuffer {
    type Params = ReloadBufferParams;
    type Result = ReloadBufferResult;
    const METHOD: &'static str = "custom/reloadBuffer";
}

/// `path` relative to the workspace, which events may name through either
/// the root as given or its canonical form.
fn relative_to<'a>(roots: &[PathBuf], path: &'a Path) -> Option<&'a Path> {
    roots.iter().find_map(|root| path.strip_prefix(root).ok())
}

/// Whether a change to `path` can affect compilation.
fn is_relevant(roots: &[PathBuf], path: &Path) -> bool {
    let Some(relative) = relative_to(roots, path) else {
        return false;
    };
    let mut names = relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy());
    let Some(extension) = path.extension() else {
        return false;
    };
    // A WASM module is built into a `target` directory, but cargo's
    // intermediate copies in `deps` are not the ones a manifest names.
    if extension == "wasm" {
        return !names.any(|name| name.starts_with('.') || name == "deps");
    }
    let hidden_or_output = names.any(|name| name.starts_with('.') || name == "target");
    !hidden_or_output && (extension == "ar" || extension == "toml" || extension == "gds")
}

/// The manifest's WASM modules, which may be built outside the workspace.
fn wasm_modules(root: &Path) -> Vec<PathBuf> {
    let manifest = root.join("Argon.toml");
    if !manifest.is_file() {
        return Vec::new();
    }
    Library::load(&manifest)
        .map(|library| library.wasm.into_values().collect())
        .unwrap_or_default()
}

/// The directory to watch for `module` to change, and whether to watch it
/// recursively: its own directory, or its nearest existing ancestor until it
/// is first built. `None` when the workspace watch already covers it.
fn module_watch(roots: &[PathBuf], module: &Path) -> Option<(PathBuf, RecursiveMode)> {
    let parent = module.parent()?;
    let directory = parent.ancestors().find(|directory| directory.is_dir())?;
    let directory = directory.canonicalize().ok()?;
    if roots.iter().any(|root| directory.starts_with(root)) {
        return None;
    }
    let mode = if parent.is_dir() {
        RecursiveMode::NonRecursive
    } else {
        RecursiveMode::Recursive
    };
    Some((directory, mode))
}

/// Whether `path` names one of `modules`, which may be spelled differently.
fn is_module(modules: &[PathBuf], path: &Path) -> bool {
    let canonical_parent =
        |path: &Path| path.parent().and_then(|parent| parent.canonicalize().ok());
    path.extension()
        .is_some_and(|extension| extension == "wasm")
        && modules.iter().any(|module| {
            module == path
                || (module.file_name() == path.file_name()
                    && canonical_parent(module).is_some_and(|parent| {
                        canonical_parent(path).is_some_and(|other| other == parent)
                    }))
        })
}

fn workspace_roots(root: &Path) -> Vec<PathBuf> {
    let mut roots = vec![root.to_path_buf()];
    if let Ok(canonical) = root.canonicalize()
        && canonical != root
    {
        roots.push(canonical);
    }
    roots
}

impl Backend {
    pub(crate) fn start_watcher(&self, root: &Path) {
        let (events, mut received) = tokio::sync::mpsc::unbounded_channel();
        let watcher = notify::recommended_watcher(move |event| {
            let _ = events.send(event);
        })
        .and_then(|mut watcher| {
            watcher.watch(root, RecursiveMode::Recursive)?;
            Ok(watcher)
        });
        let mut watcher = match watcher {
            Ok(watcher) => watcher,
            Err(error) => {
                error!("could not watch {} for changes: {error}", root.display());
                return;
            }
        };
        let roots = workspace_roots(root);
        let modules = wasm_modules(root);
        for (directory, mode) in modules
            .iter()
            .filter_map(|module| module_watch(&roots, module))
        {
            if let Err(error) = watcher.watch(&directory, mode) {
                error!(
                    "could not watch {} for changes: {error}",
                    directory.display()
                );
            }
        }
        *self
            .state
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(WorkspaceWatcher { _watcher: watcher });

        let backend = self.clone();
        tokio::spawn(async move {
            let collect = |paths: &mut IndexSet<PathBuf>, event: notify::Result<notify::Event>| {
                let Ok(event) = event else {
                    return;
                };
                if matches!(event.kind, EventKind::Access(_)) {
                    return;
                }
                paths.extend(
                    event
                        .paths
                        .into_iter()
                        .filter(|path| is_relevant(&roots, path) || is_module(&modules, path)),
                );
            };
            while let Some(event) = received.recv().await {
                let mut paths = IndexSet::new();
                collect(&mut paths, event);
                tokio::time::sleep(DEBOUNCE).await;
                while let Ok(event) = received.try_recv() {
                    collect(&mut paths, event);
                }
                if !paths.is_empty() {
                    backend.disk_changed(paths).await;
                }
            }
        });
    }

    /// Reloads open buffers whose files changed and recompiles for closed
    /// ones, which the compiler rereads from disk.
    async fn disk_changed(&self, paths: IndexSet<PathBuf>) {
        let mut closed_file_changed = false;
        for path in paths {
            let open = {
                let source = self.state.source_state.lock().await;
                agent::open_uri(&source, &path)
                    .map(|uri| (uri.clone(), source.editor_files[&uri].contents().to_owned()))
            };
            let Some((uri, buffer)) = open else {
                closed_file_changed = true;
                continue;
            };
            // A missing file is the editor's to report, and matching text is
            // the editor's own write.
            match tokio::fs::read_to_string(&path).await {
                Ok(disk) if disk != buffer => {}
                _ => continue,
            }
            match self
                .state
                .editor_client
                .send_request::<ReloadBuffer>(ReloadBufferParams { uri })
                .await
            {
                Ok(ReloadBufferResult::Modified) => {
                    let roots = self
                        .state
                        .root_dir
                        .get()
                        .map(|root| workspace_roots(root))
                        .unwrap_or_default();
                    let name = relative_to(&roots, &path).unwrap_or(&path);
                    self.state
                        .report_message(
                            MessageType::WARNING,
                            format!(
                                "{} changed on disk, but the editor has unsaved changes to it, so it was not reloaded.",
                                name.display()
                            ),
                        )
                        .await;
                }
                Ok(_) => {}
                Err(error) => error!(
                    "could not ask the editor to reload {}: {error}",
                    path.display()
                ),
            }
        }
        if closed_file_changed {
            let identity = {
                let mut source = self.state.source_state.lock().await;
                source.advance_revision();
                source.compile_identity()
            };
            self.compile_after_debounce(identity);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modules_outside_the_workspace_are_watched_by_directory() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().canonicalize().unwrap();
        let root = base.join("lib");
        let release = base.join("target/wasm32-unknown-unknown/release");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(base.join("target")).unwrap();
        let roots = workspace_roots(&root);
        let module = root.join("../target/wasm32-unknown-unknown/release/sizing.wasm");

        // Until the module is first built, its nearest existing ancestor is.
        let (directory, mode) = module_watch(&roots, &module).unwrap();
        assert_eq!(directory, base.join("target"));
        assert_eq!(mode, RecursiveMode::Recursive);

        std::fs::create_dir_all(&release).unwrap();
        let (directory, mode) = module_watch(&roots, &module).unwrap();
        assert_eq!(directory, release);
        assert_eq!(mode, RecursiveMode::NonRecursive);
        assert!(module_watch(&roots, &root.join("target/sizing.wasm")).is_none());

        let modules = [module];
        assert!(is_module(&modules, &release.join("sizing.wasm")));
        assert!(!is_module(&modules, &release.join("other.wasm")));
        assert!(!is_module(&modules, &release.join("sizing.d")));
    }

    #[test]
    fn only_sources_and_inputs_outside_hidden_and_output_directories_matter() {
        let roots = [
            PathBuf::from("/tmp/.hidden/lib"),
            PathBuf::from("/private/tmp/.hidden/lib"),
        ];
        let relevant = |path: &str| is_relevant(&roots, Path::new(path));
        assert!(relevant("/tmp/.hidden/lib/lib.ar"));
        assert!(relevant("/private/tmp/.hidden/lib/src/cells.ar"));
        assert!(relevant("/tmp/.hidden/lib/Argon.toml"));
        assert!(relevant("/tmp/.hidden/lib/layout/sram.gds"));
        assert!(!relevant("/tmp/.hidden/lib/target/argon.gds"));
        assert!(relevant(
            "/tmp/.hidden/lib/routines/target/wasm32-unknown-unknown/release/routines.wasm"
        ));
        assert!(!relevant(
            "/tmp/.hidden/lib/routines/target/wasm32-unknown-unknown/release/deps/routines.wasm"
        ));
        assert!(!relevant("/tmp/.hidden/lib/.git/index"));
        assert!(!relevant("/tmp/.hidden/lib/.lib.ar.swp"));
        assert!(!relevant("/tmp/.hidden/lib/notes.md"));
        assert!(!relevant("/elsewhere/lib.ar"));
    }
}
