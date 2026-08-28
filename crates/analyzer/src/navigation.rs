//! `textDocument/definition` and `textDocument/references`.
//!
//! The compiler builds the index (see [`argonc::nav`]); this module maps
//! between it and the protocol: client positions to byte offsets, compiler
//! spans to `Location`s, and the embedded standard library to a real file the
//! editor can open.

use std::{
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use argonc::{
    nav::{DefLocation, NavIndex},
    parse::{STD_PATH, virtual_source},
};
use tower_lsp_server::ls_types::{Location, Position, Range, Uri};

use crate::{State, argon_cache_dir, span_range};

/// Writes the embedded standard library somewhere the editor can open it.
///
/// The directory is keyed by a hash of the source, so upgrading the analyzer
/// serves the new standard library rather than a stale cached copy, and two
/// installed versions can coexist. Returns `None` if there is nowhere to write
/// or the write fails: navigation into the standard library is a convenience,
/// not something worth surfacing an error for.
fn materialize_std() -> Option<PathBuf> {
    let source = virtual_source(Path::new(STD_PATH))?;
    let mut hasher = std::hash::DefaultHasher::new();
    std::hash::Hash::hash(source, &mut hasher);
    let directory =
        argon_cache_dir()?.join(format!("std/{:016x}", std::hash::Hasher::finish(&hasher)));
    let target = directory.join("lib.ar");
    if target.is_file() {
        return Some(target);
    }
    fs::create_dir_all(&directory).ok()?;
    // Write and rename so a second analyzer starting concurrently never sees a
    // half-written file.
    let mut temporary = tempfile::NamedTempFile::new_in(&directory).ok()?;
    temporary.write_all(source.as_bytes()).ok()?;
    temporary.flush().ok()?;
    temporary.persist(&target).ok()?;
    let mut permissions = fs::metadata(&target).ok()?.permissions();
    permissions.set_readonly(true);
    let _ = fs::set_permissions(&target, permissions);
    Some(target)
}

impl State {
    /// Path of the materialized standard library, written on first use.
    fn std_file(&self) -> Option<&Path> {
        self.std_file.get_or_init(materialize_std).as_deref()
    }

    /// The compiler-facing path for a document the client named.
    ///
    /// Everything is its own path except the materialized standard library,
    /// which the compiler only knows by its virtual path.
    fn compiler_path(&self, uri: &Uri) -> Option<PathBuf> {
        let path = uri.to_file_path()?.into_owned();
        if self.std_file() == Some(path.as_path()) {
            return Some(PathBuf::from(STD_PATH));
        }
        Some(path)
    }

    /// The URI a compiler path should be shown under.
    fn client_uri(&self, path: &Path) -> Option<Uri> {
        if virtual_source(path).is_some() {
            return Uri::from_file_path(self.std_file()?);
        }
        Uri::from_file_path(path)
    }

    /// Byte offset of `position` within the document the client named.
    ///
    /// Prefers the open buffer, because it is ahead of whatever last compiled.
    /// Falls back to the compiled source so that navigation still works inside
    /// a file the editor has not opened — most often the standard library.
    async fn offset_at(&self, uri: &Uri, position: Position) -> Option<(PathBuf, usize)> {
        let path = self.compiler_path(uri)?;
        if let Some(document) = self.source_state.lock().await.editor_files.get(uri) {
            return Some((path, document.position_to_offset(position)?));
        }
        let text = match virtual_source(&path) {
            Some(source) => source.to_owned(),
            None => {
                let published = self.published_state.lock().await;
                let module = published.ast.values().find(|module| module.path == path)?;
                module.source_text.to_string()
            }
        };
        let offset = self.document(text).position_to_offset(position)?;
        Some((path, offset))
    }

    async fn nav_index(&self) -> Option<std::sync::Arc<NavIndex>> {
        self.published_state.lock().await.nav.clone()
    }

    /// Converts a compiler span into a client-facing location.
    async fn location(&self, span: &argonc::ast::Span) -> Option<Location> {
        let uri = self.client_uri(&span.path)?;
        let range = match virtual_source(&span.path) {
            // The standard library is not in the workspace AST as a file on
            // disk, so range it against the source compiled into the binary.
            Some(source) => {
                let document = self.document(source);
                Range {
                    start: document.offset_to_pos(span.span.start()),
                    end: document.offset_to_pos(span.span.end()),
                }
            }
            None => {
                let published = self.published_state.lock().await;
                span_range(&published.ast, span, self.position_encoding())?
            }
        };
        Some(Location { uri, range })
    }

    pub(crate) async fn definition(&self, uri: &Uri, position: Position) -> Option<Location> {
        let (path, offset) = self.offset_at(uri, position).await?;
        let index = self.nav_index().await?;
        match &index.definition_at(&path, offset)?.location {
            DefLocation::Source(span) => self.location(span).await,
            DefLocation::File(path) => Some(Location {
                uri: self.client_uri(path)?,
                range: Range::new(Position::new(0, 0), Position::new(0, 0)),
            }),
            // A cell signature the compiler synthesized for a GDS import: it
            // resolves, but there is no source to show.
            DefLocation::Generated => None,
        }
    }

    pub(crate) async fn references(
        &self,
        uri: &Uri,
        position: Position,
        include_declaration: bool,
    ) -> Option<Vec<Location>> {
        let (path, offset) = self.offset_at(uri, position).await?;
        let index = self.nav_index().await?;
        let spans: Vec<argonc::ast::Span> = index
            .references_at(&path, offset, include_declaration)
            .into_iter()
            .cloned()
            .collect();
        let mut locations = Vec::with_capacity(spans.len());
        for span in &spans {
            if let Some(location) = self.location(span).await {
                locations.push(location);
            }
        }
        Some(locations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_standard_library_is_written_once_and_matches_the_embedded_source() {
        let cache = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test; the value is restored below.
        unsafe { std::env::set_var("XDG_CACHE_HOME", cache.path()) };

        let first = materialize_std().expect("a cache directory is available");
        let expected = virtual_source(Path::new(STD_PATH)).unwrap();
        assert_eq!(fs::read_to_string(&first).unwrap(), expected);
        assert!(fs::metadata(&first).unwrap().permissions().readonly());

        // A second call reuses the file rather than rewriting a read-only one.
        assert_eq!(materialize_std().as_deref(), Some(first.as_path()));

        unsafe { std::env::remove_var("XDG_CACHE_HOME") };
    }
}
