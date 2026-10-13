//! Picks up workspace files that change on disk outside Neovim.

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
    /// The buffer has unsaved changes, so Neovim kept them.
    Modified,
    Closed,
}

/// Asks Neovim to reread a file that changed on disk, unless its buffer has
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
    let hidden_or_output = relative.components().any(|component| {
        let name = component.as_os_str().to_string_lossy();
        name.starts_with('.') || name == "target"
    });
    !hidden_or_output
        && path
            .extension()
            .is_some_and(|extension| extension == "ar" || extension == "toml" || extension == "gds")
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
        let watcher = match watcher {
            Ok(watcher) => watcher,
            Err(error) => {
                error!("could not watch {} for changes: {error}", root.display());
                return;
            }
        };
        *self
            .state
            .watcher
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(WorkspaceWatcher { _watcher: watcher });

        let backend = self.clone();
        let roots = workspace_roots(root);
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
                        .filter(|path| is_relevant(&roots, path)),
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
            // A missing file is Neovim's to report, and matching text is
            // Neovim's own write.
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
                                "{} changed on disk, but Neovim has unsaved changes to it, so it was not reloaded.",
                                name.display()
                            ),
                        )
                        .await;
                }
                Ok(_) => {}
                Err(error) => error!("could not ask Neovim to reload {}: {error}", path.display()),
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
        assert!(!relevant("/tmp/.hidden/lib/.git/index"));
        assert!(!relevant("/tmp/.hidden/lib/.lib.ar.swp"));
        assert!(!relevant("/tmp/.hidden/lib/notes.md"));
        assert!(!relevant("/elsewhere/lib.ar"));
    }
}
