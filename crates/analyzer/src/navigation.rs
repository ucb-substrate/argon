//! `textDocument/definition` and `textDocument/references`.
//!
//! The compiler builds the index (see [`argonc::nav`]); this module maps
//! between it and the protocol: client positions to byte offsets, compiler
//! spans to `Location`s, and the embedded standard library to a real file the
//! editor can open.
//!
//! The index can be older than the buffers the editor has, deliberately: it is
//! retained across an edit that does not compile so that navigation does not
//! disappear mid-keystroke. Its offsets are therefore into the sources it was
//! built from, which it carries, and every offset crossing between those and a
//! buffer goes through the [`Alignment`] for that file.

use std::{
    collections::HashMap,
    fs,
    io::Write,
    path::{Path, PathBuf},
};

use argonc::{
    nav::{DefLocation, NavIndex},
    parse::{STD_PATH, virtual_source},
};
use tower_lsp_server::ls_types::{Location, Position, Range, Uri};

use crate::{State, argon_cache_dir, document::Document};

/// Writes the embedded standard library into `cache`.
///
/// The directory is keyed by a hash of the source, so upgrading the analyzer
/// serves the new standard library rather than a stale cached copy, and two
/// installed versions can coexist. Returns `None` if the write fails:
/// navigation into the standard library is a convenience, not something worth
/// surfacing an error for.
fn materialize_std_in(cache: &Path) -> Option<PathBuf> {
    let source = virtual_source(Path::new(STD_PATH))?;
    let directory = cache.join(format!("std/{:016x}", source_digest(source)));
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
    // The file exists from here on. Marking it read-only says "this is not
    // yours to edit"; failing to do so is not a reason to discard it.
    if let Ok(metadata) = fs::metadata(&target) {
        let mut permissions = metadata.permissions();
        permissions.set_readonly(true);
        let _ = fs::set_permissions(&target, permissions);
    }
    Some(target)
}

/// FNV-1a, spelled out rather than taken from `DefaultHasher`, whose algorithm
/// is explicitly unspecified across releases. The digest names a directory on
/// disk, so a toolchain upgrade must not silently strand the copy already
/// there and write a second one beside it.
fn source_digest(source: &str) -> u64 {
    let mut hash = 0xcbf29ce484222325_u64;
    for byte in source.bytes() {
        hash = (hash ^ u64::from(byte)).wrapping_mul(0x100000001b3);
    }
    hash
}

/// The directory the materialized standard library lives under.
///
/// Cached: resolving the home directory is a syscall, and every navigation
/// request consults this to decide whether it is looking at the standard
/// library at all.
fn std_cache_root() -> Option<&'static Path> {
    static ROOT: std::sync::OnceLock<Option<PathBuf>> = std::sync::OnceLock::new();
    ROOT.get_or_init(|| argon_cache_dir().map(|cache| cache.join("std")))
        .as_deref()
}

/// [`materialize_std_in`] under the user's cache directory, or `None` if there
/// is nowhere to write.
fn materialize_std() -> Option<PathBuf> {
    materialize_std_in(&argon_cache_dir()?)
}

/// Bytes `a` and `b` share at the start, ending on a character boundary.
fn common_prefix(a: &str, b: &str) -> usize {
    let mut end = a
        .as_bytes()
        .iter()
        .zip(b.as_bytes())
        .take_while(|(left, right)| left == right)
        .count();
    // The two texts agree byte for byte up to `end`, so backing off to a
    // boundary in one backs off to the same boundary in the other.
    while !a.is_char_boundary(end) {
        end -= 1;
    }
    end
}

/// Bytes `a` and `b` share at the end, starting on a character boundary.
fn common_suffix(a: &str, b: &str) -> usize {
    let mut len = a
        .as_bytes()
        .iter()
        .rev()
        .zip(b.as_bytes().iter().rev())
        .take_while(|(left, right)| left == right)
        .count();
    while !a.is_char_boundary(a.len() - len) {
        len -= 1;
    }
    len
}

/// Translates byte ranges between two versions of one file.
///
/// The navigation index is built from a snapshot that can lag the editor's
/// buffer, so an offset means one thing to the index and another to the
/// client. Whatever edits separate the two, the bytes they still share at the
/// start and at the end translate exactly; a range that straddles what changed
/// has no honest translation, and the answer is that there is none.
///
/// Ranges rather than lone offsets, because the single offset where the shared
/// head meets the shared tail is genuinely ambiguous — it is both the last
/// unmoved byte and the first moved one. A span has extent to settle which
/// reading applies, so a definition translates exactly. A cursor does not, and
/// always reads as the head; [`FileView::cursor`] is what keeps that arbitrary
/// choice from mattering.
#[derive(Debug, Clone, Copy)]
struct Alignment {
    /// Bytes the two versions share at the start.
    prefix: usize,
    /// Bytes they share at the end, none of them inside `prefix`.
    suffix: usize,
    indexed_len: usize,
    buffer_len: usize,
}

impl Alignment {
    fn new(indexed: &str, buffer: &str) -> Self {
        let prefix = common_prefix(indexed, buffer);
        let suffix = common_suffix(&indexed[prefix..], &buffer[prefix..]);
        Self {
            prefix,
            suffix,
            indexed_len: indexed.len(),
            buffer_len: buffer.len(),
        }
    }

    fn translate(
        range: std::ops::Range<usize>,
        prefix: usize,
        suffix: usize,
        from_len: usize,
        to_len: usize,
    ) -> Option<std::ops::Range<usize>> {
        // Wholly within the head the two versions share, so unmoved.
        if range.end <= prefix {
            return Some(range);
        }
        // Wholly within the tail they share, so moved by the size difference.
        // `common_suffix` measures what is left after `prefix`, so neither
        // subtraction can wrap.
        let tail = from_len - suffix;
        let shift = |offset: usize| offset - tail + (to_len - suffix);
        (range.start >= tail).then(|| shift(range.start)..shift(range.end))
    }

    /// The buffer range covering the same bytes as `range` in the indexed text.
    fn to_buffer(self, range: std::ops::Range<usize>) -> Option<std::ops::Range<usize>> {
        Self::translate(
            range,
            self.prefix,
            self.suffix,
            self.indexed_len,
            self.buffer_len,
        )
    }

    /// The indexed offset naming the same point as `offset` in the buffer.
    fn to_indexed(self, offset: usize) -> Option<usize> {
        Self::translate(
            offset..offset,
            self.prefix,
            self.suffix,
            self.buffer_len,
            self.indexed_len,
        )
        .map(|range| range.start)
    }
}

/// One file as the navigation index and the client each see it.
struct FileView {
    uri: Uri,
    /// The text the client's positions are in: its open buffer, or the indexed
    /// snapshot when the file is not open and the editor will read it fresh.
    buffer: Document,
    alignment: Alignment,
}

impl FileView {
    /// Byte offset in the indexed text of a position the client sent, if the
    /// identifier the index has there is one the edits left intact.
    ///
    /// The index can still hold a token the buffer no longer contains, and
    /// nothing about the offset alone reveals that: [`NavIndex::target_at`]
    /// resolves a token that merely *ends* at the offset, so a cursor left
    /// sitting where a name was deleted picks the deleted name up from either
    /// side. Mapping the token's own span back to the buffer is what settles
    /// it — a span that survives lands wholly inside text the two versions
    /// still share, and one that does not has no counterpart to answer about.
    fn cursor(&self, index: &NavIndex, path: &Path, position: Position) -> Option<usize> {
        let offset = self
            .alignment
            .to_indexed(self.buffer.position_to_offset(position)?)?;
        let (span, _) = index.target_at(path, offset)?;
        self.alignment.to_buffer(span.start()..span.end())?;
        Some(offset)
    }

    /// The location an indexed span occupies in the client's text.
    fn location(&self, span: cfgrammar::Span) -> Option<Location> {
        let range = self.alignment.to_buffer(span.start()..span.end())?;
        Some(Location {
            uri: self.uri.clone(),
            range: Range {
                start: self.buffer.offset_to_pos(range.start),
                end: self.buffer.offset_to_pos(range.end),
            },
        })
    }
}

/// The file views one navigation request has needed so far.
///
/// A `references` answer names the same file once per result, and building a
/// view indexes every line of it, so the views are built once and kept for the
/// request rather than rebuilt per span.
struct FileViews<'a> {
    state: &'a State,
    index: &'a NavIndex,
    views: HashMap<PathBuf, Option<FileView>>,
}

impl<'a> FileViews<'a> {
    fn new(state: &'a State, index: &'a NavIndex) -> Self {
        Self {
            state,
            index,
            views: HashMap::new(),
        }
    }

    async fn get(&mut self, path: &Path) -> Option<&FileView> {
        if !self.views.contains_key(path) {
            let view = self.build(path).await;
            self.views.insert(path.to_path_buf(), view);
        }
        self.views.get(path)?.as_ref()
    }

    async fn build(&self, path: &Path) -> Option<FileView> {
        let indexed = self.index.source(path)?;
        let uri = self.state.client_uri(path).await?;
        let open = {
            let source = self.state.source_state.lock().await;
            source
                .editor_files
                .get(&uri)
                .or_else(|| {
                    // A client's spelling of a URI is not the one
                    // `Uri::from_file_path` produces for the same file — the
                    // two percent-encode different characters — so fall back
                    // to comparing the paths. Missing an open buffer here
                    // would align the index against itself and answer from
                    // the snapshot instead of from what the user is looking
                    // at.
                    let file = uri.to_file_path()?;
                    source.editor_files.iter().find_map(|(open, document)| {
                        (open.to_file_path()? == file).then_some(document)
                    })
                })
                .cloned()
        };
        let buffer = open.unwrap_or_else(|| self.state.document(indexed.clone()));
        let alignment = Alignment::new(indexed, buffer.contents());
        Some(FileView {
            uri,
            buffer,
            alignment,
        })
    }
}

impl State {
    /// Path of the materialized standard library, written on first use.
    ///
    /// The write runs on a blocking thread, and only ever once: later callers
    /// read the cell rather than touching the filesystem.
    async fn std_file(&self) -> Option<&Path> {
        self.std_file
            .get_or_init(|| async {
                match tokio::task::spawn_blocking(materialize_std).await {
                    Ok(Some(path)) => Some(path),
                    // Cached for the session either way, so say so once rather
                    // than leaving std navigation quietly dead.
                    Ok(None) => {
                        tracing::warn!(
                            "could not write the standard library to the cache directory; \
                             navigation into it is unavailable for this session"
                        );
                        None
                    }
                    Err(error) => {
                        tracing::warn!(%error, "writing the standard library failed");
                        None
                    }
                }
            })
            .await
            .as_deref()
    }

    /// The compiler-facing path for a document the client named.
    ///
    /// Everything is its own path except the materialized standard library,
    /// which the compiler only knows by its virtual path.
    async fn compiler_path(&self, uri: &Uri) -> Option<PathBuf> {
        let path = uri.to_file_path()?.into_owned();
        // Only a document under the cache directory can be the materialized
        // standard library, and only that comparison needs it on disk. Every
        // other request therefore never waits on the write.
        if std_cache_root().is_some_and(|root| path.starts_with(root))
            && self.std_file().await == Some(path.as_path())
        {
            return Some(PathBuf::from(STD_PATH));
        }
        Some(path)
    }

    /// The URI a compiler path should be shown under.
    async fn client_uri(&self, path: &Path) -> Option<Uri> {
        if virtual_source(path).is_some() {
            return Uri::from_file_path(self.std_file().await?);
        }
        Uri::from_file_path(path)
    }

    async fn nav_index(&self) -> Option<std::sync::Arc<NavIndex>> {
        self.published_state.lock().await.nav.clone()
    }

    pub(crate) async fn definition(&self, uri: &Uri, position: Position) -> Option<Location> {
        let index = self.nav_index().await?;
        let path = self.compiler_path(uri).await?;
        let mut views = FileViews::new(self, &index);
        let offset = views.get(&path).await?.cursor(&index, &path, position)?;
        match &index.definition_at(&path, offset)?.location {
            DefLocation::Source(span) => views.get(&span.path).await?.location(span.span),
            DefLocation::File(path) => Some(Location {
                uri: self.client_uri(path).await?,
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
        let index = self.nav_index().await?;
        let path = self.compiler_path(uri).await?;
        let mut views = FileViews::new(self, &index);
        let offset = views.get(&path).await?.cursor(&index, &path, position)?;
        let spans = index.references_at(&path, offset, include_declaration);
        let mut locations = Vec::with_capacity(spans.len());
        for span in spans {
            if let Some(location) = views
                .get(&span.path)
                .await
                .and_then(|view| view.location(span.span))
            {
                locations.push(location);
            }
        }
        Some(locations)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::PositionEncoding;

    const FILE: &str = "/virtual/lib.ar";

    /// A real index over `source`, so the seam tests exercise the resolution
    /// the analyzer actually performs rather than a stand-in for it.
    fn index_of(source: &str) -> NavIndex {
        let root = argonc::parse::parse_source_text(source, PathBuf::from(FILE)).unwrap();
        let std = argonc::parse::parse_source_text(
            argonc::parse::STD_SOURCE,
            PathBuf::from(argonc::parse::STD_PATH),
        )
        .unwrap();
        let ast = indexmap::IndexMap::from([(Vec::new(), root), (vec!["std".to_owned()], std)]);
        let (typed, _) = argonc::compile::static_compile(&ast).expect("a root module");
        NavIndex::build(&typed)
    }

    fn view_of(indexed: &str, buffer: &str) -> FileView {
        FileView {
            uri: Uri::from_file_path(FILE).unwrap(),
            buffer: Document::new(buffer, 0, PositionEncoding::Utf8),
            alignment: Alignment::new(indexed, buffer),
        }
    }

    /// Deleting a name leaves the cursor exactly on the seam, and the index
    /// still holds the token that used to be there. Neither reading of that
    /// offset escapes it — `target_at` matches a token that ends at an offset
    /// as readily as one that contains it — so what rules the answer out is
    /// the token failing to map back, not a choice between the two sides.
    #[test]
    fn a_cursor_where_a_name_was_deleted_answers_nothing() {
        let indexed = "cell top() {\n    let width = 1.;\n    eq(width, 2.);\n}\n";
        let buffer = "cell top() {\n    let width = 1.;\n    eq(, 2.);\n}\n";
        let index = index_of(indexed);
        let view = view_of(indexed, buffer);
        let path = PathBuf::from(FILE);

        // The deletion point: the head the two versions share ends here, and
        // the tail they share begins here too.
        assert_eq!(
            view.alignment.prefix,
            view.alignment.buffer_len - view.alignment.suffix
        );
        let deleted = Position::new(2, "    eq(".len() as u32);
        let raw = view
            .alignment
            .to_indexed(view.buffer.position_to_offset(deleted).unwrap())
            .expect("the seam translates, however ambiguously");
        assert_eq!(raw, indexed.rfind("width").unwrap());

        // Reading the seam the other way would not have helped: `target_at`
        // matches a token that *ends* at an offset as readily as one that
        // contains it, so the shifted reading lands on the same deleted token.
        // What rules the answer out is the token, not a choice of side.
        let (span, _) = index.target_at(&path, raw).expect("the deleted token");
        assert_eq!(
            index.target_at(&path, span.end()).map(|(other, _)| other),
            Some(span)
        );

        assert_eq!(view.cursor(&index, &path, deleted), None);
    }

    /// The check has to reject only what the edit compromised: a name the edit
    /// left alone still answers, which is the whole point of retaining a stale
    /// index.
    #[test]
    fn a_cursor_on_a_name_the_edit_left_alone_still_answers() {
        let indexed = "cell top() {\n    let width = 1.;\n    eq(width, 2.);\n}\n";
        let buffer = "cell top() {\n    let width = 1.;\n    eq(, 2.);\n}\n";
        let index = index_of(indexed);
        let view = view_of(indexed, buffer);
        let path = PathBuf::from(FILE);

        let declaration = Position::new(1, "    let wi".len() as u32);
        let offset = view
            .cursor(&index, &path, declaration)
            .expect("the untouched declaration resolves");
        assert!(index.definition_at(&path, offset).is_some());
    }

    #[test]
    fn the_standard_library_is_written_once_and_matches_the_embedded_source() {
        let cache = tempfile::tempdir().unwrap();

        let first = materialize_std_in(cache.path()).expect("a cache directory is available");
        let expected = virtual_source(Path::new(STD_PATH)).unwrap();
        assert_eq!(fs::read_to_string(&first).unwrap(), expected);
        assert!(fs::metadata(&first).unwrap().permissions().readonly());

        // A second call reuses the file rather than rewriting a read-only one.
        assert_eq!(
            materialize_std_in(cache.path()).as_deref(),
            Some(first.as_path())
        );
    }

    /// Ranges in the unchanged head and tail of an edited file translate
    /// exactly; ones that straddle the edit translate to nothing.
    #[test]
    fn an_alignment_translates_around_an_edit() {
        let indexed = "let alpha = 1;\n";
        let buffer = "let beta = 1;\n";
        let alignment = Alignment::new(indexed, buffer);

        // `let `, before the edit.
        assert_eq!(alignment.to_buffer(0..4), Some(0..4));
        assert_eq!(alignment.to_indexed(4), Some(4));

        // ` = 1;`, after it, shifted by the byte the edit removed.
        let equals = indexed.find('=').unwrap();
        assert_eq!(
            alignment.to_buffer(equals..equals + 1),
            Some(buffer.find('=').unwrap()..buffer.find('=').unwrap() + 1)
        );
        assert_eq!(
            alignment.to_indexed(buffer.find('=').unwrap()),
            Some(equals)
        );

        // `alpha` itself, straddling the edit.
        let alpha = indexed.find("alpha").unwrap();
        assert_eq!(alignment.to_buffer(alpha..alpha + "alpha".len()), None);
        assert_eq!(alignment.to_indexed(alpha + 2), None);
    }

    #[test]
    fn an_unedited_file_aligns_with_itself() {
        let text = "cell top() {\n    let width = 100.;\n}\n";
        let alignment = Alignment::new(text, text);
        for (offset, character) in text.char_indices() {
            let end = offset + character.len_utf8();
            assert_eq!(alignment.to_buffer(offset..end), Some(offset..end));
            assert_eq!(alignment.to_indexed(offset), Some(offset));
        }
    }

    /// An edit that shifts every following offset is exactly the case that used
    /// to resolve a stale index against fresh positions. It is also the case
    /// where the shared head ends where the shared tail begins, so the range,
    /// not the offset, has to decide which side a span belongs to.
    #[test]
    fn an_insertion_at_the_top_of_a_file_shifts_everything_after_it() {
        let indexed = "cell top() {}\n";
        let buffer = "cell helper() {\ncell top() {}\n";
        let alignment = Alignment::new(indexed, buffer);
        assert_eq!(alignment.prefix, alignment.indexed_len - alignment.suffix);

        let top = indexed.find("top").unwrap();
        let moved = buffer.find("top").unwrap();
        assert_eq!(alignment.to_buffer(top..top + 3), Some(moved..moved + 3));
        assert_eq!(alignment.to_indexed(moved), Some(top));
    }

    /// The mirror image: an insertion at the end must leave the text before it
    /// where it was, even though the same offset is on both sides of the seam.
    #[test]
    fn an_insertion_at_the_end_of_a_file_leaves_earlier_spans_alone() {
        let indexed = "cell top() {}\n";
        let buffer = "cell top() {}\ncell extra() {}\n";
        let alignment = Alignment::new(indexed, buffer);

        let top = indexed.find("top").unwrap();
        assert_eq!(alignment.to_buffer(top..top + 3), Some(top..top + 3));
        assert_eq!(alignment.to_indexed(top), Some(top));
    }

    #[test]
    fn an_alignment_stays_on_character_boundaries() {
        let indexed = "let \u{b5} = 1;\n";
        let buffer = "let \u{b5}\u{1d11e} = 1;\n";
        let alignment = Alignment::new(indexed, buffer);
        // The shared prefix ends at a boundary, not inside the two-byte `\u{b5}`.
        assert_eq!(
            alignment.prefix,
            indexed.find('\u{b5}').unwrap() + '\u{b5}'.len_utf8()
        );
        let equals = indexed.find('=').unwrap();
        let moved = buffer.find('=').unwrap();
        assert_eq!(
            alignment.to_buffer(equals..equals + 1),
            Some(moved..moved + 1)
        );
    }
}
