//! Process-local compilation session for editor integrations.
//!
//! The session owns open-document snapshots and makes those snapshots the
//! source of truth while retaining the existing one-shot compiler API. A
//! changed file is reparsed as a complete file; later compiler passes can be
//! made finer grained without changing this public synchronization API.

use std::{
    collections::{HashMap, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};

use arcstr::ArcStr;
use indexmap::IndexMap;

use crate::{
    ast::ModPath,
    cellcache::{CellCache, CellCacheStats},
    compile::{
        self, CellArg, CompileInput, CompileOutput, CompiledData, StaticAnalysis,
        StaticErrorCompileOutput,
    },
    fingerprint::ItemIndex,
    gdscache::{GdsCache, GdsCacheStats},
    nav::NavIndex,
    parse,
    workspace::WorkspaceConfig,
};

/// Identity of a file the session reads from disk: its length and a hash of
/// its bytes.
///
/// Content, not modification time, decides freshness. A timestamp is neither
/// necessary — `touch` rewrites one without changing a single build input — nor
/// sufficient: `cp -p`, `rsync --times`, `tar -x`, and branch or snapshot
/// restores all reproduce the original timestamp over different bytes, and a
/// generated file that keeps its length then looks unchanged to the session
/// while a fresh compiler sees different geometry.
type FileRevision = (u64, u64);
type TrackedFileRevision = (PathBuf, Option<FileRevision>);

/// A byte-oriented replacement applied to the current version of a source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourceEdit {
    pub range: std::ops::Range<usize>,
    pub replacement: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum EditError {
    #[error("source snapshot does not exist: {0}")]
    MissingSource(PathBuf),
    #[error("edit range {start}..{end} is outside a {len}-byte source")]
    InvalidRange {
        start: usize,
        end: usize,
        len: usize,
    },
    #[error("edit boundary {0} is not on a UTF-8 character boundary")]
    InvalidCharBoundary(usize),
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IncrementalStats {
    pub revision: u64,
    pub static_cache_hits: u64,
    pub static_cache_misses: u64,
    pub parse_cache_hits: u64,
    pub files_reparsed: u64,
    pub execution_cache_hits: u64,
    pub execution_cache_misses: u64,
    /// Imported GDS hierarchies served from, and added to, the session cache.
    pub gds_cache: GdsCacheStats,
    /// Compiled source cells served from, and added to, the session cache.
    pub cell_cache: CellCacheStats,
}

#[derive(Clone)]
struct StaticCache {
    key: u64,
    disk_revisions: Vec<TrackedFileRevision>,
    analysis: StaticAnalysis,
    /// Built lazily, because only editor sessions ask for it.
    ///
    /// The outer `Option` records whether the build has been attempted, so an
    /// index that was built and then judged unusable is not rebuilt from the
    /// whole typed AST on every request until the next edit.
    nav: Option<Option<Arc<NavIndex>>>,
    /// Content fingerprints for this analysis, built lazily since only a
    /// session needs them.
    items: Option<Arc<ItemIndex>>,
}

/// Stateful compiler used by long-lived analyzer processes.
#[derive(Default, Clone)]
pub struct IncrementalCompiler {
    revision: u64,
    sources: IndexMap<PathBuf, ArcStr>,
    parse_cache: parse::ParseCache,
    static_cache: Option<StaticCache>,
    execution_cache: HashMap<u64, CompileOutput>,
    execution_environment: Option<u64>,
    /// The most recent navigation index that had content. Retained so that
    /// editor navigation keeps answering while the workspace does not
    /// type-check, which is most of the time while someone is typing.
    last_good_nav: Option<Arc<NavIndex>>,
    /// Compiled cells imported from GDS.
    ///
    /// Not cleared by [`Self::invalidate`]: an import is named by a `.gds`
    /// file and its own contents, so no edit to an Argon source file can
    /// change it. It is dropped only when the execution environment changes,
    /// which is the key that tracks those files.
    gds_cache: GdsCache,
    /// Compiled cells from source. Like `gds_cache`, kept across edits: an
    /// entry is named by content, so an edit that does not change a cell
    /// cannot invalidate it.
    cell_cache: CellCache,
    /// Screened digests for the technology file and imported GDS libraries.
    external_digests: ExternalDigests,
    /// Per-cell layer and geometry verdicts, retired with the cells they
    /// describe.
    check_cache: compile::CheckCache,
    /// The parsed technology, kept so that it is not re-read and re-parsed on
    /// every execution. Retired with the caches when the execution environment
    /// changes, which is the key that tracks the technology file's contents.
    tech: Option<crate::tech::Technology>,
    stats: IncrementalStats,
}

impl std::fmt::Debug for IncrementalCompiler {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IncrementalCompiler")
            .field("revision", &self.revision)
            .field("source_count", &self.sources.len())
            .field("stats", &self.stats)
            .finish()
    }
}

impl IncrementalCompiler {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn revision(&self) -> u64 {
        self.revision
    }

    pub fn stats(&self) -> IncrementalStats {
        self.stats
    }

    pub fn source(&self, path: &Path) -> Option<&str> {
        self.sources.get(path).map(ArcStr::as_str)
    }

    /// Synchronizes a complete editor document. Identical text is a no-op.
    pub fn set_source_text(&mut self, path: PathBuf, text: impl Into<ArcStr>) -> u64 {
        let text = text.into();
        if self.sources.get(&path) == Some(&text) {
            return self.revision;
        }
        self.sources.insert(path, text);
        self.invalidate();
        self.revision
    }

    /// Applies edits in order, so every range addresses the text produced by
    /// the preceding edit (matching LSP incremental-change semantics).
    pub fn apply_edits(
        &mut self,
        path: &Path,
        edits: impl IntoIterator<Item = SourceEdit>,
    ) -> Result<u64, EditError> {
        let Some(existing) = self.sources.get(path) else {
            return Err(EditError::MissingSource(path.to_path_buf()));
        };
        let mut source = existing.to_string();
        let original = source.clone();
        for edit in edits {
            if edit.range.start > edit.range.end || edit.range.end > source.len() {
                return Err(EditError::InvalidRange {
                    start: edit.range.start,
                    end: edit.range.end,
                    len: source.len(),
                });
            }
            for boundary in [edit.range.start, edit.range.end] {
                if !source.is_char_boundary(boundary) {
                    return Err(EditError::InvalidCharBoundary(boundary));
                }
            }
            source.replace_range(edit.range, &edit.replacement);
        }
        if source != original {
            self.sources
                .insert(path.to_path_buf(), ArcStr::from(source));
            self.invalidate();
        }
        Ok(self.revision)
    }

    /// Stops overriding a closed document; subsequent parses read it from disk.
    pub fn remove_source(&mut self, path: &Path) -> u64 {
        if self.sources.shift_remove(path).is_some() {
            self.invalidate();
        }
        self.revision
    }

    /// Navigation index for the current sources.
    ///
    /// Falls back to the most recent index that had content, so that a
    /// half-written edit does not take go-to-definition away. Returned behind
    /// an `Arc` because the analyzer shares one index across requests and
    /// `StaticAnalysis` is cloned on every analysis.
    pub fn nav(&mut self, config: &WorkspaceConfig) -> Option<Arc<NavIndex>> {
        self.ensure_analysis(config);
        let cache = self.static_cache.as_mut().expect("analysis cache");
        let built = match &cache.nav {
            Some(nav) => nav.clone(),
            None => {
                let built = cache.analysis.typed_ast.as_ref().and_then(|typed| {
                    // Decided before building, so an index that will not be
                    // kept is never built: that is the common case while
                    // someone is typing.
                    let coverage = NavIndex::coverage(typed);
                    // An import error or a missing root module yields an empty
                    // typed AST, which is a compile failure rather than a
                    // workspace with nothing in it. Note that it also leaves
                    // `tracked` empty, which would make the coverage check
                    // below vacuously true.
                    if coverage.is_empty() {
                        return None;
                    }
                    let tracked = typed.values().map(|module| module.path.as_path()).collect();
                    let usable = match &self.last_good_nav {
                        Some(previous) => previous.covered_by(&coverage, &tracked),
                        // Nothing better to fall back to.
                        None => true,
                    };
                    usable.then(|| Arc::new(NavIndex::build(typed)))
                });
                cache.nav = Some(built.clone());
                built
            }
        };
        if let Some(nav) = built {
            self.last_good_nav = Some(nav.clone());
            return Some(nav);
        }
        self.last_good_nav.clone()
    }

    /// The parsed technology for this environment, read at most once per epoch.
    fn ensure_tech(&mut self, config: &WorkspaceConfig) -> Option<&crate::tech::Technology> {
        if self.tech.is_none() {
            self.tech = crate::tech::read_tech(config.tech.as_deref()?).ok();
        }
        self.tech.as_ref()
    }

    /// Content fingerprints for the current sources, or `None` when nothing
    /// type-checked. Cached alongside the analysis, so repeated executions in
    /// one revision build it once.
    fn ensure_items(&mut self, config: &WorkspaceConfig) -> Option<Arc<ItemIndex>> {
        self.ensure_analysis(config);
        let cache = self.static_cache.as_mut().expect("analysis cache");
        if let Some(items) = &cache.items {
            return Some(items.clone());
        }
        let typed = cache.analysis.typed_ast.as_ref()?;
        let items = Arc::new(ItemIndex::build(typed));
        cache.items = Some(items.clone());
        Some(items)
    }

    pub fn analyze_workspace(&mut self, config: &WorkspaceConfig) -> StaticAnalysis {
        self.ensure_analysis(config);
        self.static_cache
            .as_ref()
            .expect("analysis cache was populated")
            .analysis
            .clone()
    }

    fn ensure_analysis(&mut self, config: &WorkspaceConfig) {
        let key = self.static_key(config);
        if let Some(cache) = &self.static_cache
            && cache.key == key
            && cache.disk_revisions == self.tracked_file_revisions(config, &cache.analysis)
        {
            self.stats.static_cache_hits += 1;
            return;
        }

        self.stats.static_cache_misses += 1;
        self.execution_cache.clear();
        let parse_hits = self.parse_cache.hits();
        let parse_misses = self.parse_cache.misses();
        let analysis =
            compile::analyze_workspace(parse::parse_workspace_with_config_sources_and_cache(
                config,
                &self.sources,
                &mut self.parse_cache,
            ));
        self.stats.parse_cache_hits += self.parse_cache.hits() - parse_hits;
        self.stats.files_reparsed += self.parse_cache.misses() - parse_misses;
        let disk_revisions = self.tracked_file_revisions(config, &analysis);
        self.static_cache = Some(StaticCache {
            key,
            disk_revisions,
            analysis,
            nav: None,
            items: None,
        });
    }

    /// Analyzes and executes one cell, retaining an exact-input result for
    /// repeated requests in the same source revision.
    pub fn compile_cell(
        &mut self,
        config: &WorkspaceConfig,
        cell: &[String],
        args: Vec<CellArg>,
    ) -> CompileOutput {
        self.ensure_analysis(config);
        let analysis = &self
            .static_cache
            .as_ref()
            .expect("analysis cache was populated")
            .analysis;
        if !analysis.errors.is_empty() {
            return CompileOutput::StaticErrors(StaticErrorCompileOutput {
                errors: analysis.errors.clone(),
            });
        }
        if analysis.typed_ast.is_none() {
            return CompileOutput::FatalParseErrors;
        }

        let mut hasher = DefaultHasher::new();
        let environment = execution_environment_key(
            &mut self.external_digests,
            config.tech.as_deref(),
            &config.gds_imports,
        );
        if self.execution_environment != Some(environment) {
            self.execution_cache.clear();
            // The environment key covers every import's path and contents, so
            // this is where a changed `.gds` file retires its cells. Source
            // cells go too: their geometry is solved against the technology's
            // grid, which no fingerprint sees.
            self.gds_cache.clear();
            self.cell_cache.clear();
            self.check_cache.clear();
            self.tech = None;
            self.execution_environment = Some(environment);
        }
        self.static_key(config).hash(&mut hasher);
        cell.hash(&mut hasher);
        hash_cell_args(&args, &mut hasher);
        environment.hash(&mut hasher);
        let key = hasher.finish();
        if let Some(output) = self.execution_cache.get(&key) {
            self.stats.execution_cache_hits += 1;
            return output.clone();
        }

        self.stats.execution_cache_misses += 1;
        let verify_args = args.clone();
        let items = self.ensure_items(config);
        // Cloned out so the caches below can be borrowed mutably alongside it;
        // a `Technology` is a handful of layer records.
        let tech = self.ensure_tech(config).cloned();
        let ast = self
            .static_cache
            .as_ref()
            .expect("analysis cache was populated")
            .analysis
            .typed_ast
            .as_ref()
            .expect("checked above");
        let cell_refs = cell.iter().map(String::as_str).collect::<Vec<_>>();
        let session = items.as_ref().map(|items| compile::SessionCaches {
            items,
            gds: &mut self.gds_cache,
            cells: &mut self.cell_cache,
            checks: &mut self.check_cache,
            tech: tech.as_ref(),
        });
        let output = compile::execute_cell_cached(
            ast,
            CompileInput {
                cell: &cell_refs,
                args,
            },
            config,
            session,
        );
        self.stats.cell_cache = self.cell_cache.stats();
        self.stats.gds_cache = self.gds_cache.stats();
        self.verify_against_uncached(&output, || {
            compile::execute_cell_cached(
                ast,
                CompileInput {
                    cell: &cell_refs,
                    args: verify_args,
                },
                config,
                None,
            )
        });
        self.execution_cache.insert(key, output.clone());
        output
    }

    /// Analyzes and executes a source-level cell invocation. The invocation is
    /// spliced into a clone of the current snapshot so arbitrary argument
    /// expressions are resolved and type-checked without polluting the cached
    /// editor AST.
    pub fn compile_invocation(
        &mut self,
        config: &WorkspaceConfig,
        source: &str,
    ) -> Result<CompileOutput, anyhow::Error> {
        self.ensure_analysis(config);
        let analysis = &self
            .static_cache
            .as_ref()
            .expect("analysis cache was populated")
            .analysis;
        if !analysis.errors.is_empty() {
            return Ok(CompileOutput::StaticErrors(StaticErrorCompileOutput {
                errors: analysis.errors.clone(),
            }));
        }

        let mut hasher = DefaultHasher::new();
        let environment = execution_environment_key(
            &mut self.external_digests,
            config.tech.as_deref(),
            &config.gds_imports,
        );
        if self.execution_environment != Some(environment) {
            self.execution_cache.clear();
            // The environment key covers every import's path and contents, so
            // this is where a changed `.gds` file retires its cells. Source
            // cells go too: their geometry is solved against the technology's
            // grid, which no fingerprint sees.
            self.gds_cache.clear();
            self.cell_cache.clear();
            self.check_cache.clear();
            self.tech = None;
            self.execution_environment = Some(environment);
        }
        self.static_key(config).hash(&mut hasher);
        source.hash(&mut hasher);
        environment.hash(&mut hasher);
        let key = hasher.finish();
        if let Some(output) = self.execution_cache.get(&key) {
            self.stats.execution_cache_hits += 1;
            return Ok(output.clone());
        }

        self.stats.execution_cache_misses += 1;
        let mut ast = analysis.ast.clone();
        let invocation = parse::splice_cell_invocation(&mut ast, source)?;
        let Some((typed_ast, static_output)) = compile::static_compile(&ast) else {
            return Ok(CompileOutput::FatalParseErrors);
        };
        // Built from the spliced AST rather than from `StaticCache`: that
        // second `static_compile` renumbers every `VarId`, so the cached
        // analysis's ids do not name declarations in the tree being executed.
        let items = Arc::new(ItemIndex::build(&typed_ast));
        let tech = self.ensure_tech(config).cloned();
        let output = if static_output.errors.is_empty() {
            compile::execute_cell_invocation_cached(
                &typed_ast,
                &invocation,
                config,
                Some(compile::SessionCaches {
                    items: &items,
                    gds: &mut self.gds_cache,
                    cells: &mut self.cell_cache,
                    checks: &mut self.check_cache,
                    tech: tech.as_ref(),
                }),
            )
        } else {
            CompileOutput::StaticErrors(static_output)
        };
        self.stats.gds_cache = self.gds_cache.stats();
        self.stats.cell_cache = self.cell_cache.stats();
        self.verify_against_uncached(&output, || {
            compile::execute_cell_invocation_cached(&typed_ast, &invocation, config, None)
        });
        self.execution_cache.insert(key, output.clone());
        Ok(output)
    }

    /// Recompiles the same cell with empty caches and asserts the two agree,
    /// when `ARGON_VERIFY_CELL_CACHE` is set in the environment.
    ///
    /// A dependency edge the fingerprint walker does not collect produces
    /// stale geometry with no diagnostic attached to point at it, and this is
    /// the check that turns that into a loud failure. It costs exactly what
    /// the cache saves, which is why it is opt-in.
    fn verify_against_uncached(
        &self,
        output: &CompileOutput,
        recompute: impl FnOnce() -> CompileOutput,
    ) {
        if std::env::var_os("ARGON_VERIFY_CELL_CACHE").is_none() {
            return;
        }
        let digest = |output: &CompileOutput| match output {
            CompileOutput::Valid(data) => Some(data.geometry_digest()),
            CompileOutput::ExecErrors(errors) => {
                errors.output.as_ref().map(CompiledData::geometry_digest)
            }
            _ => None,
        };
        assert_eq!(
            digest(output),
            digest(&recompute()),
            "ARGON_VERIFY_CELL_CACHE: a cached compile disagreed with an uncached \
             one, which means a dependency edge is missing from the fingerprint walker"
        );
    }

    fn invalidate(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.stats.revision = self.revision;
        self.static_cache = None;
        self.execution_cache.clear();
        // `gds_cache` and `execution_environment` deliberately survive. The
        // environment is recomputed and compared on the next execution
        // regardless, so forgetting it here would buy nothing -- and it would
        // cost everything, because a mismatch is what retires the GDS cache:
        // every edit would drop imports that no edit can invalidate.
    }

    fn static_key(&self, config: &WorkspaceConfig) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.revision.hash(&mut hasher);
        config.hash(&mut hasher);
        hasher.finish()
    }

    fn tracked_file_revisions(
        &self,
        config: &WorkspaceConfig,
        analysis: &StaticAnalysis,
    ) -> Vec<TrackedFileRevision> {
        let mut files = analysis
            .ast
            .values()
            .map(|ast| ast.path.clone())
            .filter(|path| path.extension().is_some_and(|extension| extension == "ar"))
            .collect::<Vec<_>>();
        // The parsed files above are only the modules that won resolution, so
        // watching them alone hides every change that moves a module from one
        // candidate file to the other: `foo.ar` created where a module was
        // missing silences a missing-module error, `foo.ar` created next to an
        // existing `foo/mod.ar` raises a duplicate-module error, and deleting
        // either one reverses the same step. Tracking both candidates for every
        // module in the previous analysis covers all four transitions, because
        // an absent candidate has revision `None` and gaining or losing one is
        // a change in value.
        for mod_path in analysis.ast.keys() {
            let (root_lib, module) = module_library(config, mod_path);
            let Some((name, parents)) = module.split_last() else {
                continue;
            };
            let candidates = parse::mod_candidates(root_lib, parents, name);
            files.push(candidates.direct);
            files.push(candidates.nested);
        }
        files.push(config.root_lib.clone());
        if let Some(parent) = config.root_lib.parent() {
            files.push(parent.join("Argon.toml"));
        }
        for (_, path) in &config.dependencies {
            if path.is_dir() {
                files.push(path.join("lib.ar"));
                files.push(path.join("Argon.toml"));
            } else {
                files.push(path.clone());
                if let Some(parent) = path.parent() {
                    files.push(parent.join("Argon.toml"));
                }
            }
        }
        files.sort();
        files.dedup();
        files
            .into_iter()
            .filter(|path| !self.sources.contains_key(path))
            .map(|path| {
                let revision = file_revision(&path);
                (path, revision)
            })
            .collect()
    }
}

/// Splits a workspace module key into the library whose files back it and the
/// module path within that library.
///
/// [`parse::parse_workspace_with_config_sources_and_cache`] merges every
/// dependency's modules under the dependency's name, so a key's first component
/// may name a dependency instead of a module of this workspace. A root-library
/// module that shadows a dependency's name is attributed to the dependency and
/// so contributes the wrong pair; its own file is still tracked through the
/// parsed AST, which leaves that one module no worse off than tracking no
/// candidates at all.
fn module_library<'a>(config: &WorkspaceConfig, mod_path: &'a ModPath) -> (PathBuf, &'a [String]) {
    if let Some((first, rest)) = mod_path.split_first()
        && let Some((_, path)) = config.dependencies.iter().find(|(name, _)| name == first)
    {
        // A dependency is named either by its directory or by its library file,
        // the same choice `parse` makes before walking it.
        let root_lib = if path.is_dir() {
            path.join("lib.ar")
        } else {
            path.clone()
        };
        return (root_lib, rest);
    }
    (config.root_lib.clone(), mod_path.as_slice())
}

/// Reads `path` and reduces it to a [`FileRevision`], or `None` when it does
/// not exist or cannot be read. `None` is a value like any other, so a file
/// appearing where the previous analysis found nothing invalidates the cache.
///
/// Screening with modification time and length and hashing only on a match
/// would buy nothing here: [`IncrementalCompiler::ensure_analysis`] compares
/// whole revision *values*, so the unchanged case — the one that has to be
/// cheap — must produce the hash anyway, and the only read a screen could skip
/// belongs to a file that is about to be reparsed regardless.
fn file_revision(path: &Path) -> Option<FileRevision> {
    let contents = std::fs::read(path).ok()?;
    let mut hasher = DefaultHasher::new();
    contents.hash(&mut hasher);
    // The length is implied by the hash, but carrying it costs nothing and a
    // 64-bit hash is small enough that a same-length restriction is worth
    // having between two revisions of the same file.
    Some((contents.len() as u64, hasher.finish()))
}

/// Content digests for the external files an execution environment names,
/// screened by length and modification time.
///
/// The environment key decides whether the caches are still valid, so it is
/// recomputed on every execution; hashing every declared GDS library each time
/// costs tens of milliseconds per keystroke for a workspace declaring tens of
/// megabytes, and scales with *declared* bytes rather than used ones.
///
/// Screening on `(length, modification time)` and re-reading only when one of
/// them moves is weaker than the content hashing
/// [`IncrementalCompiler::tracked_file_revisions`] does for `.ar` files: a
/// multi-megabyte GDS library is a build artifact replaced wholesale rather
/// than a source file that tools rewrite in place. The digest itself is still
/// content-derived; only the decision to *recompute* it is screened.
#[derive(Debug, Default, Clone)]
struct ExternalDigests {
    entries: HashMap<PathBuf, ScreenedDigest>,
}

#[derive(Debug, Clone)]
struct ScreenedDigest {
    len: u64,
    modified: Option<std::time::SystemTime>,
    revision: Option<FileRevision>,
}

impl ExternalDigests {
    /// The revision of `path`, re-reading it only when its length or
    /// modification time has moved since it was last read.
    fn revision(&mut self, path: &Path) -> Option<FileRevision> {
        let screen = std::fs::metadata(path)
            .ok()
            .map(|meta| (meta.len(), meta.modified().ok()));
        let Some((len, modified)) = screen else {
            // Unreadable or absent. `None` is a value like any other here, and
            // it must not be served from a stale entry.
            self.entries.remove(path);
            return None;
        };
        if let Some(cached) = self.entries.get(path)
            && cached.len == len
            && cached.modified == modified
            // A file written twice within one filesystem timestamp tick is the
            // case a screen cannot see. Refusing to trust an entry whose
            // timestamp is absent keeps that from being silent on platforms
            // that do not report one.
            && modified.is_some()
        {
            return cached.revision;
        }
        let revision = file_revision(path);
        self.entries.insert(
            path.to_path_buf(),
            ScreenedDigest {
                len,
                modified,
                revision,
            },
        );
        revision
    }
}

fn execution_environment_key(
    digests: &mut ExternalDigests,
    tech_file: Option<&Path>,
    gds_imports: &[(String, PathBuf)],
) -> u64 {
    let mut hasher = DefaultHasher::new();
    tech_file.hash(&mut hasher);
    tech_file
        .map(|path| digests.revision(path))
        .hash(&mut hasher);
    for (name, path) in gds_imports {
        name.hash(&mut hasher);
        path.hash(&mut hasher);
        digests.revision(path).hash(&mut hasher);
    }
    hasher.finish()
}

fn hash_cell_args(args: &[CellArg], hasher: &mut impl Hasher) {
    args.len().hash(hasher);
    for arg in args {
        match arg {
            CellArg::Float(value) => {
                0_u8.hash(hasher);
                value.to_bits().hash(hasher);
            }
            CellArg::Int(value) => {
                1_u8.hash(hasher);
                value.hash(hasher);
            }
            CellArg::Bool(value) => {
                2_u8.hash(hasher);
                value.hash(hasher);
            }
            CellArg::String(value) => {
                3_u8.hash(hasher);
                value.hash(hasher);
            }
            CellArg::Enum(value) => {
                4_u8.hash(hasher);
                value.hash(hasher);
            }
            CellArg::Seq(values) => {
                5_u8.hash(hasher);
                hash_cell_args(values, hasher);
            }
            CellArg::Struct { name, fields } => {
                6_u8.hash(hasher);
                name.hash(hasher);
                fields.len().hash(hasher);
                for (field, value) in fields {
                    field.hash(hasher);
                    hash_cell_args(std::slice::from_ref(value), hasher);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Writes `lib.ar` into a scratch workspace, as the CLI would see it. The
    /// directory is returned because dropping it deletes the workspace.
    fn scratch_workspace(source: &str) -> (tempfile::TempDir, WorkspaceConfig) {
        let dir = tempfile::tempdir().expect("create scratch workspace");
        let lib = dir.path().join("lib.ar");
        std::fs::write(&lib, source).expect("write scratch library");
        (dir, WorkspaceConfig::new(lib))
    }

    /// Rewrites `path` with `source` and restores the modification time it had
    /// beforehand, reproducing what `cp -p`, `rsync --times`, and `tar -x` do
    /// to a file the session has already read.
    fn write_preserving_mtime(path: &Path, source: &str) {
        let modified = std::fs::metadata(path)
            .expect("scratch file exists")
            .modified()
            .expect("platform reports modification times");
        std::fs::write(path, source).expect("rewrite scratch file");
        std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("reopen scratch file")
            .set_times(std::fs::FileTimes::new().set_modified(modified))
            .expect("restore modification time");
        assert_eq!(
            std::fs::metadata(path).unwrap().modified().unwrap(),
            modified,
            "the fixture must reproduce the original timestamp to be a fixture"
        );
    }

    #[test]
    fn edits_are_sequential_and_noops_keep_the_revision() {
        let path = PathBuf::from("lib.ar");
        let mut compiler = IncrementalCompiler::new();
        assert_eq!(compiler.set_source_text(path.clone(), "cell top() {}"), 1);
        assert_eq!(compiler.set_source_text(path.clone(), "cell top() {}"), 1);
        compiler
            .apply_edits(
                &path,
                [
                    SourceEdit {
                        range: 5..8,
                        replacement: "main".to_owned(),
                    },
                    SourceEdit {
                        range: 10..10,
                        replacement: "value: Float".to_owned(),
                    },
                ],
            )
            .unwrap();
        assert_eq!(compiler.source(&path), Some("cell main(value: Float) {}"));
        assert_eq!(compiler.revision(), 2);
    }

    #[test]
    fn analysis_uses_unsaved_snapshots_and_reuses_an_exact_revision() {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/immediate/lib.ar");
        let disk_source = std::fs::read_to_string(&root).unwrap();
        let config = WorkspaceConfig::new(&root);
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(root.clone(), "cell unsaved() {");

        let first = compiler.analyze_workspace(&config);
        assert!(!first.errors.is_empty());
        assert_eq!(std::fs::read_to_string(&root).unwrap(), disk_source);
        assert_eq!(compiler.stats().static_cache_misses, 1);

        let second = compiler.analyze_workspace(&config);
        assert!(!second.errors.is_empty());
        assert_eq!(compiler.stats().static_cache_hits, 1);
    }

    #[test]
    fn navigation_survives_an_edit_that_does_not_compile() {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/argon_library/lib.ar");
        let source = std::fs::read_to_string(&root).unwrap();
        let config = WorkspaceConfig::new(&root);
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(root.clone(), source.clone());

        let good = compiler
            .nav(&config)
            .expect("a workspace that compiles is indexed");
        let cell = source.find("cell test").unwrap() + "cell ".len();
        assert!(good.definition_at(&root, cell).is_some());

        // A syntax error leaves nothing to index. The previous index is served
        // instead, so navigation does not disappear mid-keystroke.
        compiler.set_source_text(root.clone(), "cell broken( {".to_owned());
        let stale = compiler
            .nav(&config)
            .expect("the last good index is retained");
        assert!(stale.definition_at(&root, cell).is_some());

        compiler.set_source_text(root.clone(), source);
        assert!(compiler.nav(&config).is_some());
    }

    /// An unresolvable import empties the typed AST, which is a compile
    /// failure rather than a workspace with nothing in it.
    #[test]
    fn an_unresolvable_import_keeps_the_previous_index() {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/argon_library/lib.ar");
        let source = std::fs::read_to_string(&root).unwrap();
        let config = WorkspaceConfig::new(&root);
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(root.clone(), source.clone());

        let cell = source.find("cell test").unwrap() + "cell ".len();
        assert!(
            compiler
                .nav(&config)
                .unwrap()
                .definition_at(&root, cell)
                .is_some()
        );

        // Half a module path is what an import looks like while it is being
        // typed. It reports an import error, so nothing type-checks and there
        // is nothing to index — but the workspace is not empty.
        compiler.set_source_text(root.clone(), format!("use lib::uti::test;\n{source}"));
        let stale = compiler
            .nav(&config)
            .expect("the last good index is retained");
        assert!(stale.definition_at(&root, cell).is_some());
    }

    /// A `mod` declaration that resolves to no module of its own leaves a
    /// stand-in behind that borrows the root's path. It must not be mistaken
    /// for the root's own source.
    #[test]
    fn a_module_that_resolves_to_nothing_does_not_blank_the_root() {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/argon_library/lib.ar");
        let source = std::fs::read_to_string(&root).unwrap();
        let config = WorkspaceConfig::new(&root);
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(root.clone(), source.clone());
        assert!(compiler.nav(&config).is_some());

        // `mod lib;` names the root's own file.
        let circular = format!("mod lib;\n{source}");
        compiler.set_source_text(root.clone(), circular.clone());
        let index = compiler.nav(&config).expect("an index");
        assert_eq!(index.source(&root).map(ArcStr::as_str), Some(&*circular));
        let cell = circular.find("cell test").unwrap() + "cell ".len();
        assert!(index.definition_at(&root, cell).is_some());
    }

    /// A file with nothing left in it still parses, so it must not be mistaken
    /// for one that stopped parsing and pin the index to a stale copy.
    #[test]
    fn commenting_out_a_whole_file_still_reindexes_the_workspace() {
        let directory =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/argon_library");
        let root = directory.join("lib.ar");
        let utils = directory.join("utils.ar");
        let source = std::fs::read_to_string(&root).unwrap();
        let config = WorkspaceConfig::new(&root);
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(root.clone(), source.clone());
        compiler.set_source_text(utils.clone(), std::fs::read_to_string(&utils).unwrap());
        assert!(compiler.nav(&config).is_some());

        compiler.set_source_text(utils.clone(), "// fn test() -> Float { 15. }\n".to_owned());
        let index = compiler.nav(&config).expect("an index");
        assert!(
            index.source(&utils).is_some(),
            "the commented-out file is still covered"
        );

        // The index is the fresh one, not the retained copy: a rename made
        // alongside the comment-out is reflected in it. The stale index would
        // still answer here — with the old name, at the old offsets.
        let renamed = source.replace("cell test()", "cell renamed()");
        compiler.set_source_text(root.clone(), renamed.clone());
        let index = compiler.nav(&config).expect("an index");
        let offset = renamed.find("renamed").unwrap();
        let definition = index
            .definition_at(&root, offset)
            .expect("the renamed cell resolves");
        assert_eq!(definition.name, "renamed");
        assert_eq!(index.source(&root).map(ArcStr::as_str), Some(&*renamed));
    }

    #[test]
    fn editing_one_module_reuses_other_file_parses() {
        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/argon_library/lib.ar");
        let source = std::fs::read_to_string(&root).unwrap();
        let config = WorkspaceConfig::new(&root);
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(root.clone(), source.clone());
        let first = compiler.analyze_workspace(&config);
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        let initial = compiler.stats();

        compiler.set_source_text(root.clone(), format!("// editor comment\n{source}"));
        let second = compiler.analyze_workspace(&config);
        assert!(second.errors.is_empty(), "{:?}", second.errors);
        let edited = compiler.stats();

        assert_eq!(edited.files_reparsed - initial.files_reparsed, 1);
        assert!(edited.parse_cache_hits - initial.parse_cache_hits >= 3);
    }

    #[test]
    fn repeated_execution_uses_the_session_cache() {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let root = examples.join("immediate/lib.ar");
        let tech = examples.join("tech/basic.tech.toml");
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech));
        let mut compiler = IncrementalCompiler::new();
        let cell = vec!["immediate".to_owned()];

        let first = compiler.compile_cell(&config, &cell, Vec::new());
        assert!(matches!(
            first,
            CompileOutput::Valid(_) | CompileOutput::ExecErrors(_)
        ));
        let second = compiler.compile_cell(&config, &cell, Vec::new());
        assert!(matches!(
            second,
            CompileOutput::Valid(_) | CompileOutput::ExecErrors(_)
        ));
        assert_eq!(compiler.stats().execution_cache_misses, 1);
        assert_eq!(compiler.stats().execution_cache_hits, 1);
    }

    #[test]
    fn edited_session_matches_a_fresh_compile() {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let root = examples.join("immediate/lib.ar");
        let tech = examples.join("tech/basic.tech.toml");
        let source = "cell immediate() {\n  let x0 = 31;\n  let y0 = 2;\n}\n";
        let cell = vec!["immediate".to_owned()];
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech));

        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(root.clone(), source);
        let incremental = compiler.compile_cell(&config, &cell, Vec::new());

        let sources = IndexMap::from([(root.clone(), ArcStr::from(source))]);
        let analysis = compile::analyze_workspace(parse::parse_workspace_with_config_and_sources(
            &config, &sources,
        ));
        assert!(analysis.errors.is_empty(), "{:?}", analysis.errors);
        let typed = analysis.typed_ast.unwrap();
        let fresh = compile::execute_cell(
            &typed,
            CompileInput {
                cell: &["immediate"],
                args: Vec::new(),
            },
            &config,
        );

        assert_eq!(
            bincode::serialize(&incremental).unwrap(),
            bincode::serialize(&fresh).unwrap()
        );
    }

    #[test]
    fn a_content_change_under_an_unchanged_timestamp_is_detected() {
        let (dir, config) = scratch_workspace("cell top() { let width = 11; }\n");
        let mut compiler = IncrementalCompiler::new();
        let first = compiler.analyze_workspace(&config);
        assert!(first.errors.is_empty(), "{:?}", first.errors);

        // Same length, same modification time, different program.
        write_preserving_mtime(
            &dir.path().join("lib.ar"),
            "cell top() { let width = qq; }\n",
        );

        let second = compiler.analyze_workspace(&config);
        assert!(
            !second.errors.is_empty(),
            "a rewritten library was served from the cache as clean"
        );
        assert_eq!(compiler.stats().static_cache_hits, 0);
        assert_eq!(compiler.stats().static_cache_misses, 2);
    }

    #[test]
    fn rewriting_a_file_with_the_same_contents_keeps_the_analysis() {
        // The other half of deciding freshness by content: a save with no edit
        // and a checkout that restores the same bytes both move the timestamp
        // without changing an input, and must not cost a reparse.
        let source = "cell top() { let width = 11; }\n";
        let (dir, config) = scratch_workspace(source);
        let mut compiler = IncrementalCompiler::new();
        compiler.analyze_workspace(&config);
        std::fs::write(dir.path().join("lib.ar"), source).unwrap();
        compiler.analyze_workspace(&config);

        assert_eq!(compiler.stats().static_cache_hits, 1);
        assert_eq!(compiler.stats().static_cache_misses, 1);
    }

    #[test]
    fn creating_a_missing_module_file_invalidates_the_analysis() {
        // `mod foo;` resolves to `foo/mod.ar` while `foo.ar` is absent, so the
        // file the user writes to fix the error is not the file the failed
        // parse read.
        let (dir, config) = scratch_workspace("mod foo;\n");
        let mut compiler = IncrementalCompiler::new();
        let missing = compiler.analyze_workspace(&config);
        assert!(!missing.errors.is_empty(), "the module file is absent");

        std::fs::write(dir.path().join("foo.ar"), "cell bar() {}\n").unwrap();

        let created = compiler.analyze_workspace(&config);
        assert!(created.errors.is_empty(), "{:?}", created.errors);
    }

    #[test]
    fn a_module_file_appearing_beside_mod_ar_invalidates_the_analysis() {
        let (dir, config) = scratch_workspace("mod foo;\n");
        std::fs::create_dir(dir.path().join("foo")).unwrap();
        std::fs::write(dir.path().join("foo/mod.ar"), "cell bar() {}\n").unwrap();
        let mut compiler = IncrementalCompiler::new();
        let nested = compiler.analyze_workspace(&config);
        assert!(nested.errors.is_empty(), "{:?}", nested.errors);

        // Neither file the analysis read has changed, but the module is now
        // ambiguous.
        let direct = dir.path().join("foo.ar");
        std::fs::write(&direct, "cell bar() {}\n").unwrap();
        let ambiguous = compiler.analyze_workspace(&config);
        assert!(
            !ambiguous.errors.is_empty(),
            "a module with two source files was accepted"
        );

        // Removing it again has to be as visible, or the diagnostic outlives
        // the file that caused it.
        std::fs::remove_file(&direct).unwrap();
        let resolved = compiler.analyze_workspace(&config);
        assert!(resolved.errors.is_empty(), "{:?}", resolved.errors);
    }

    /// Writes a one-rectangle GDS library and an Argon workspace instantiating
    /// it, and returns both alongside a config wired to the shared basic
    /// technology. The directory is returned because dropping it deletes them.
    fn scratch_gds_workspace(source: &str) -> (tempfile::TempDir, WorkspaceConfig, PathBuf) {
        use ::gds::{GdsBoundary, GdsElement, GdsLibrary, GdsPoint, GdsStruct};

        let dir = tempfile::tempdir().expect("create scratch workspace");
        let lib = dir.path().join("lib.ar");
        std::fs::write(&lib, source).expect("write scratch library");

        let mut library = GdsLibrary::new("fixture");
        let mut child = GdsStruct::new("child");
        child.elems.push(GdsElement::GdsBoundary(GdsBoundary {
            // Layer 235/4 is `met1` in `examples/tech/basic.tech.toml`.
            layer: 235,
            datatype: 4,
            xy: vec![
                GdsPoint::new(0, 0),
                GdsPoint::new(0, 2_000),
                GdsPoint::new(1_000, 2_000),
                GdsPoint::new(1_000, 0),
                GdsPoint::new(0, 0),
            ],
            ..Default::default()
        }));
        library.structs.push(child);
        let gds = dir.path().join("fixture.gds");
        library.save(&gds).expect("write scratch gds");

        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = WorkspaceConfig::new(lib)
            .with_tech(Some(tech))
            .with_gds_imports([("child".to_owned(), gds.clone())]);
        (dir, config, gds)
    }

    /// Solved geometry, keyed by cell name, with every id dropped.
    ///
    /// Ids depend on allocation order, which a cache hit does not reproduce,
    /// so what has to match is the geometry and the shape of the instance
    /// graph.
    fn geometry(output: &CompileOutput) -> Vec<String> {
        let data = match output {
            CompileOutput::Valid(data) => data,
            CompileOutput::ExecErrors(errors) => errors
                .output
                .as_ref()
                .expect("geometry despite diagnostics"),
            other => panic!("unexpected compile output: {other:?}"),
        };
        let mut cells = data
            .cells
            .values()
            .map(|cell| {
                let objects = cell
                    .objects
                    .values()
                    .map(|object| match object {
                        compile::SolvedValue::Rect(rect) => format!(
                            "rect {:?} {} {} {} {}",
                            rect.layer, rect.x0.0, rect.y0.0, rect.x1.0, rect.y1.0
                        ),
                        compile::SolvedValue::Instance(inst) => format!(
                            "inst {} {} {} {:?} {}",
                            data.cells[&inst.cell].name, inst.x, inst.y, inst.angle, inst.reflect
                        ),
                        other => format!("{other:?}"),
                    })
                    .collect::<Vec<_>>();
                format!("{}\n{}", cell.name, objects.join("\n"))
            })
            .collect::<Vec<_>>();
        cells.sort();
        cells.push(format!("top={}", data.cells[&data.top].name));
        cells
    }

    /// The geometry an output carries, whether or not it also reported
    /// diagnostics. A cell determined only by initial conditions is reported
    /// underconstrained but still produces every object.
    fn compiled_data(output: &CompileOutput) -> &compile::CompiledData {
        match output {
            CompileOutput::Valid(data) => data,
            CompileOutput::ExecErrors(errors) => errors
                .output
                .as_ref()
                .expect("geometry despite diagnostics"),
            other => panic!("unexpected compile output: {other:?}"),
        }
    }

    /// Every source span reachable from a compiled cell, for comparison
    /// against the same cell compiled from shifted text.
    fn spans_of(output: &CompileOutput) -> Vec<crate::ast::Span> {
        let data = compiled_data(output);
        let mut spans = Vec::new();
        for cell in data.cells.values() {
            for scope in cell.scopes.values() {
                spans.push(scope.span.clone());
                spans.extend(scope.emit.iter().map(|(_, emit)| emit.span.clone()));
            }
            for object in cell.objects.values() {
                let span = match object {
                    compile::SolvedValue::Rect(r) => r.span.clone(),
                    compile::SolvedValue::Polygon(p) => p.span.clone(),
                    compile::SolvedValue::Path(p) => p.span.clone(),
                    compile::SolvedValue::Text(t) => t.span.clone(),
                    compile::SolvedValue::Dimension(d) => d.span.clone(),
                    compile::SolvedValue::Instance(i) => Some(i.span.clone()),
                };
                spans.extend(span);
            }
            spans.extend(
                cell.fallback_constraints_used
                    .iter()
                    .map(|fallback| fallback.span.clone()),
            );
        }
        spans
    }

    /// Rebasing a cell compiled against one revision onto another makes every
    /// one of its spans point at the same *text* it originally pointed at.
    #[test]
    fn rebasing_moves_spans_onto_identical_text() {
        use crate::fingerprint::{ItemIndex, SpanRebase};

        let source = "cell leaf() {\n    \
                      let r = rect(\"met1\", x0i = 1., y0i = 2., x1i = 30., y1i = 40.);\n}\n\
                      cell top() {\n    let i = inst(leaf(), x = 0., y = 0.);\n}\n";
        let (_dir, config) = scratch_workspace(source);
        let root = config.root_lib().to_path_buf();
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = config.with_tech(Some(tech));
        let cell = vec!["top".to_owned()];

        let mut before = IncrementalCompiler::new();
        before.set_source_text(root.clone(), source);
        let old_output = before.compile_cell(&config, &cell, Vec::new());
        let old_items = ItemIndex::build(
            before
                .analyze_workspace(&config)
                .typed_ast
                .as_ref()
                .expect("it type-checks"),
        );

        // Insert above everything: identical declarations, all shifted.
        let shifted = format!("// a comment that pushes every declaration down\n{source}");
        let mut after = IncrementalCompiler::new();
        after.set_source_text(root.clone(), shifted.clone());
        let new_output = after.compile_cell(&config, &cell, Vec::new());
        let new_items = ItemIndex::build(
            after
                .analyze_workspace(&config)
                .typed_ast
                .as_ref()
                .expect("it type-checks"),
        );

        let rebase = SpanRebase::new(&old_items, &new_items)
            .expect("every declaration moved, so a translation is needed");

        let mut rebased = compiled_data(&old_output).clone();
        for cell in rebased.cells.values_mut() {
            Arc::make_mut(cell)
                .rebase_spans(&rebase)
                .expect("every span lies inside a declaration that still exists");
        }

        let slice = |text: &str, span: &crate::ast::Span| {
            text[span.span.start()..span.span.end()].to_owned()
        };
        let original = spans_of(&old_output)
            .iter()
            .map(|span| slice(source, span))
            .collect::<Vec<_>>();
        let moved = spans_of(&CompileOutput::Valid(rebased))
            .iter()
            .map(|span| slice(&shifted, span))
            .collect::<Vec<_>>();
        assert!(!original.is_empty(), "the fixture must produce spans");
        assert_eq!(
            original, moved,
            "a rebased span must select the same text from the new source"
        );

        // And the rebased spans agree with what a fresh compile of the shifted
        // text produced, which is what makes the reuse invisible.
        let fresh = spans_of(&new_output)
            .iter()
            .map(|span| slice(&shifted, span))
            .collect::<Vec<_>>();
        assert_eq!(moved, fresh);
    }

    /// A cell served from the cache must report its diagnostics exactly once,
    /// however many times the compile instantiates it.
    #[test]
    fn reuse_reports_a_cells_diagnostics_once() {
        let source = "cell child() {\n    let r = rect(\"met1\", x0i = 1., y0i = 2., x1i = 30., y1i = 40.);\n}\n\
                      cell parent() {\n    for i in std::range(3) {\n        inst(child(), x = 0., y = 0.);\n    }\n}\n";
        let (_dir, config) = scratch_workspace(source);
        let root = config.root_lib().to_path_buf();
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = config.with_tech(Some(tech));
        let cell = vec!["parent".to_owned()];

        let count = |out: &CompileOutput| match out {
            CompileOutput::ExecErrors(e) => e.errors.len(),
            CompileOutput::Valid(_) => 0,
            other => panic!("unexpected: {other:?}"),
        };

        let mut session = IncrementalCompiler::new();
        session.set_source_text(root.clone(), source);
        let first = count(&session.compile_cell(&config, &cell, Vec::new()));

        // Edit only the parent: `child` is reused on every one of the three
        // instantiations the loop performs.
        let edited = source.replace("x = 0., y = 0.", "x = 5., y = 0.");
        assert_ne!(edited, source);
        session.set_source_text(root.clone(), edited.clone());
        let second = count(&session.compile_cell(&config, &cell, Vec::new()));

        let mut fresh = IncrementalCompiler::new();
        fresh.set_source_text(root, edited);
        let uncached = count(&fresh.compile_cell(&config, &cell, Vec::new()));
        assert_eq!(
            (first, second),
            (uncached, uncached),
            "a reused cell must report exactly what a fresh compile reports"
        );
    }

    /// Reinstating leaves `CompiledData::cells` in the order a fresh execution
    /// would: children before the cells that instantiate them.
    #[test]
    fn reinstated_cells_keep_a_fresh_compiles_order() {
        let source = "cell b() { let r = rect(\"met1\", x0 = 0., y0 = 0., x1 = 1., y1 = 1.); }\n\
                      cell a() { inst(b(), x = 0., y = 0.); inst(b(), x = 2., y = 0.); }\n\
                      cell r() { inst(a(), x = 0., y = 0.); inst(b(), x = 0., y = 10.); }\n";
        let (_dir, config) = scratch_workspace(source);
        let root = config.root_lib().to_path_buf();
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = config.with_tech(Some(tech));
        let cell = vec!["r".to_owned()];

        let order = |out: &CompileOutput| match out {
            CompileOutput::Valid(d) => d.cells.values().map(|c| c.name.clone()).collect::<Vec<_>>(),
            other => panic!("unexpected: {other:?}"),
        };

        let mut session = IncrementalCompiler::new();
        session.set_source_text(root.clone(), source);
        let _ = session.compile_cell(&config, &cell, Vec::new());
        // Appending past the last declaration moves nothing and changes no
        // fingerprint, so `r` itself is served from the cell cache.
        let appended = format!("{source}// trailing\n");
        session.set_source_text(root.clone(), appended.clone());
        let reused = session.compile_cell(&config, &cell, Vec::new());
        assert!(session.stats().cell_cache.hits >= 1, "`r` must be reused");

        let mut fresh = IncrementalCompiler::new();
        fresh.set_source_text(root, appended);
        let uncached = fresh.compile_cell(&config, &cell, Vec::new());
        assert_eq!(order(&reused), order(&uncached));
    }

    /// Nothing moved, so there is nothing to translate and no walk to pay for.
    #[test]
    fn an_edit_that_moves_nothing_needs_no_rebase() {
        use crate::fingerprint::{ItemIndex, SpanRebase};

        let source = "cell leaf() { let r = rect(\"met1\", x0 = 0., y0 = 0., x1 = 1., y1 = 1.); }\n\
                      cell top() { let i = inst(leaf(), x = 0., y = 0.); }\n";
        let (_dir, config) = scratch_workspace(source);
        let root = config.root_lib().to_path_buf();
        let mut session = IncrementalCompiler::new();
        session.set_source_text(root.clone(), source);
        let items = ItemIndex::build(
            session
                .analyze_workspace(&config)
                .typed_ast
                .as_ref()
                .expect("it type-checks"),
        );
        // Appending after the last declaration moves none of them.
        session.set_source_text(root, format!("{source}// trailing\n"));
        let appended = ItemIndex::build(
            session
                .analyze_workspace(&config)
                .typed_ast
                .as_ref()
                .expect("it type-checks"),
        );
        assert!(SpanRebase::new(&items, &appended).is_none());
    }

    /// A declaration that only moved keeps its `CellId`, so a cell compiled in
    /// one revision is still recognisable in the next.
    #[test]
    fn moving_a_declaration_preserves_its_cell_id() {
        let source = "cell leaf() { let r = rect(\"met1\", x0 = 0., y0 = 0., x1 = 1., y1 = 1.); }\n\
                      cell top() { let i = inst(leaf(), x = 0., y = 0.); }\n";
        let (_dir, config) = scratch_workspace(source);
        let root = config.root_lib().to_path_buf();
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = config.with_tech(Some(tech));
        let cell = vec!["top".to_owned()];

        let ids = |output: &CompileOutput| match output {
            CompileOutput::Valid(data) => {
                let mut ids = data.cells.keys().copied().collect::<Vec<_>>();
                ids.sort_unstable();
                ids
            }
            other => panic!("unexpected compile output: {other:?}"),
        };

        let mut session = IncrementalCompiler::new();
        session.set_source_text(root.clone(), source);
        let before = ids(&session.compile_cell(&config, &cell, Vec::new()));

        // A comment above everything shifts every declaration's offsets
        // without changing what any of them mean.
        session.set_source_text(root.clone(), format!("// pushed down\n{source}"));
        let after = ids(&session.compile_cell(&config, &cell, Vec::new()));
        assert_eq!(
            before, after,
            "moving a declaration must not rename its cell"
        );

        // Editing one cell's body renames only that cell. `leaf` is untouched,
        // so its id survives; `top` instantiates it and does not.
        session.set_source_text(
            root.clone(),
            source.replace("x = 0., y = 0.", "x = 5., y = 0."),
        );
        let edited = ids(&session.compile_cell(&config, &cell, Vec::new()));
        assert_eq!(edited.len(), before.len());
        let kept = edited.iter().filter(|id| before.contains(id)).count();
        assert_eq!(kept, 1, "only the untouched leaf keeps its id");
    }

    /// Every cell reached from a named entry point is named by content, and so
    /// carries the marker bit that keeps content ids disjoint from the ones
    /// `alloc_id` hands out. A compiler *invocation* wraps the call in a
    /// generated entry cell, which is excluded -- see
    /// `ExecPass::source_cell_id`.
    #[test]
    fn source_cells_are_named_by_content() {
        let source = "cell leaf() { let r = rect(\"met1\", x0 = 0., y0 = 0., x1 = 1., y1 = 1.); }\n\
                      cell top() { let i = inst(leaf(), x = 0., y = 0.); }\n";
        let (_dir, config) = scratch_workspace(source);
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = config.with_tech(Some(tech));
        let mut session = IncrementalCompiler::new();
        session.set_source_text(config.root_lib().to_path_buf(), source);
        let output = session.compile_cell(&config, &["top".to_owned()], Vec::new());
        let CompileOutput::Valid(data) = &output else {
            panic!("unexpected compile output: {output:?}");
        };
        for id in data.cells.keys() {
            assert_ne!(
                id & crate::gdscache::CONTENT_ID_BIT,
                0,
                "every source cell is named by content"
            );
        }
    }

    /// Editing a parent must reuse the children it did not touch, and must not
    /// change what they compile to.
    #[test]
    fn editing_a_parent_reuses_unchanged_children() {
        let source = "cell child_a() { let r = rect(\"met1\", x0 = 0., y0 = 0., x1 = 1., y1 = 1.); }\n\
                      cell child_b() { let r = rect(\"met1\", x0 = 2., y0 = 0., x1 = 3., y1 = 1.); }\n\
                      cell parent() {\n    let a = inst(child_a(), x = 0., y = 0.);\n    \
                      let b = inst(child_b(), x = 10., y = 0.);\n}\n";
        let (_dir, config) = scratch_workspace(source);
        let root = config.root_lib().to_path_buf();
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = config.with_tech(Some(tech));
        let cell = vec!["parent".to_owned()];

        let mut session = IncrementalCompiler::new();
        session.set_source_text(root.clone(), source);
        let first = session.compile_cell(&config, &cell, Vec::new());
        assert_eq!(session.stats().cell_cache.hits, 0, "nothing to reuse yet");

        // Move the parent's instance. Both children are untouched.
        let edited = source.replace("x = 10., y = 0.", "x = 20., y = 0.");
        session.set_source_text(root.clone(), edited.clone());
        let second = session.compile_cell(&config, &cell, Vec::new());
        assert_eq!(
            session.stats().cell_cache.hits,
            2,
            "both unchanged children must be reused"
        );

        let mut fresh = IncrementalCompiler::new();
        fresh.set_source_text(root.clone(), edited);
        let uncached = fresh.compile_cell(&config, &cell, Vec::new());
        assert_eq!(geometry(&second), geometry(&uncached));
        assert_ne!(geometry(&first), geometry(&second), "the edit took effect");
    }

    /// A child that actually changed must be re-executed, and only it.
    #[test]
    fn changing_a_child_re_executes_only_that_child() {
        let source = "cell child_a() { let r = rect(\"met1\", x0 = 0., y0 = 0., x1 = 1., y1 = 1.); }\n\
                      cell child_b() { let r = rect(\"met1\", x0 = 2., y0 = 0., x1 = 3., y1 = 1.); }\n\
                      cell parent() {\n    let a = inst(child_a(), x = 0., y = 0.);\n    \
                      let b = inst(child_b(), x = 10., y = 0.);\n}\n";
        let (_dir, config) = scratch_workspace(source);
        let root = config.root_lib().to_path_buf();
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = config.with_tech(Some(tech));
        let cell = vec!["parent".to_owned()];

        let mut session = IncrementalCompiler::new();
        session.set_source_text(root.clone(), source);
        session.compile_cell(&config, &cell, Vec::new());

        // `child_a` changes; `child_b` does not.
        let edited = source.replace("x1 = 1., y1 = 1.); }", "x1 = 5., y1 = 1.); }");
        assert_ne!(edited, source);
        session.set_source_text(root.clone(), edited.clone());
        let second = session.compile_cell(&config, &cell, Vec::new());
        assert_eq!(
            session.stats().cell_cache.hits,
            1,
            "only the untouched child is reusable"
        );

        let mut fresh = IncrementalCompiler::new();
        fresh.set_source_text(root, edited);
        assert_eq!(
            geometry(&second),
            geometry(&fresh.compile_cell(&config, &cell, Vec::new()))
        );
    }

    /// A cell reused after the file shifted must carry spans that select the
    /// same source text -- this is the rebase path running for real, inside the
    /// cache rather than in isolation.
    #[test]
    fn reuse_after_a_shift_keeps_spans_pointing_at_the_same_text() {
        let source = "cell child() {\n    \
                      let r = rect(\"met1\", x0i = 1., y0i = 2., x1i = 30., y1i = 40.);\n}\n\
                      cell parent() {\n    let i = inst(child(), x = 0., y = 0.);\n}\n";
        let (_dir, config) = scratch_workspace(source);
        let root = config.root_lib().to_path_buf();
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = config.with_tech(Some(tech));
        let cell = vec!["parent".to_owned()];

        let mut session = IncrementalCompiler::new();
        session.set_source_text(root.clone(), source);
        session.compile_cell(&config, &cell, Vec::new());

        // Push every declaration down. `child` is unchanged, so it is reused --
        // and its recorded spans must be translated onto the new offsets.
        let shifted = format!("// pushed down\n{source}");
        session.set_source_text(root.clone(), shifted.clone());
        let reused = session.compile_cell(&config, &cell, Vec::new());
        assert!(session.stats().cell_cache.hits >= 1, "the child was reused");
        assert_eq!(session.stats().cell_cache.rebase_failures, 0);

        let mut fresh = IncrementalCompiler::new();
        fresh.set_source_text(root, shifted.clone());
        let uncached = fresh.compile_cell(&config, &cell, Vec::new());

        let slice =
            |span: &crate::ast::Span| shifted[span.span.start()..span.span.end()].to_owned();
        let reused_text = spans_of(&reused).iter().map(slice).collect::<Vec<_>>();
        let fresh_text = spans_of(&uncached).iter().map(slice).collect::<Vec<_>>();
        assert!(!reused_text.is_empty());
        assert_eq!(
            reused_text, fresh_text,
            "a reused cell's spans must select what a fresh compile's do"
        );
        assert_eq!(geometry(&reused), geometry(&uncached));
    }

    /// An edit to Argon source must not re-import the GDS it instantiates, and
    /// reusing the import must not change the compiled geometry.
    #[test]
    fn editing_source_reuses_the_imported_gds() {
        let source = "cell top() {\n    let i = inst(child(), x = 0., y = 0.);\n}\n";
        let (_dir, config, _gds) = scratch_gds_workspace(source);
        let root = config.root_lib().to_path_buf();
        let cell = vec!["top".to_owned()];

        let mut session = IncrementalCompiler::new();
        session.set_source_text(root.clone(), source);
        let first = session.compile_cell(&config, &cell, Vec::new());
        assert_eq!(session.stats().gds_cache.misses, 1);
        assert_eq!(session.stats().gds_cache.hits, 0);

        // What the GUI writes when someone draws a rectangle beside it.
        let edited = "cell top() {\n    let i = inst(child(), x = 0., y = 0.);\n    \
                      rect(\"met1\", x0i = 0., y0i = 0., x1i = 5., y1i = 5.);\n}\n";
        session.set_source_text(root.clone(), edited);
        let second = session.compile_cell(&config, &cell, Vec::new());
        assert_eq!(
            session.stats().gds_cache.hits,
            1,
            "an edit to Argon source must not re-import the GDS"
        );
        assert_eq!(session.stats().gds_cache.misses, 1);

        // The reused import agrees with one that was never cached.
        let mut fresh = IncrementalCompiler::new();
        fresh.set_source_text(root.clone(), edited);
        let uncached = fresh.compile_cell(&config, &cell, Vec::new());
        assert_eq!(geometry(&second), geometry(&uncached));
        assert_eq!(fresh.stats().gds_cache.hits, 0);

        // The edit is the only difference: one more rectangle than before.
        assert_eq!(geometry(&first).len(), geometry(&second).len());
        assert_ne!(geometry(&first), geometry(&second));
    }

    /// A same-length rewrite is still detected, because the screen also
    /// compares modification time. What [`ExternalDigests`] gives up is a
    /// rewrite that keeps *both* the length and the timestamp.
    #[test]
    fn a_same_length_gds_rewrite_is_still_detected() {
        use ::gds::{GdsBoundary, GdsElement, GdsLibrary, GdsPoint, GdsStruct};

        let source = "cell top() {\n    let i = inst(child(), x = 0., y = 0.);\n}\n";
        let (_dir, config, gds) = scratch_gds_workspace(source);
        let root = config.root_lib().to_path_buf();
        let cell = vec!["top".to_owned()];

        let mut session = IncrementalCompiler::new();
        session.set_source_text(root.clone(), source);
        let before = session.compile_cell(&config, &cell, Vec::new());
        let before_len = std::fs::metadata(&gds).unwrap().len();

        // Same structure, same record shapes, different coordinates: the file
        // keeps its length and only its contents move.
        let mut library = GdsLibrary::new("fixture");
        let mut child = GdsStruct::new("child");
        child.elems.push(GdsElement::GdsBoundary(GdsBoundary {
            layer: 235,
            datatype: 4,
            xy: vec![
                GdsPoint::new(0, 0),
                GdsPoint::new(0, 4_000),
                GdsPoint::new(3_000, 4_000),
                GdsPoint::new(3_000, 0),
                GdsPoint::new(0, 0),
            ],
            ..Default::default()
        }));
        library.structs.push(child);
        library.save(&gds).expect("rewrite scratch gds");
        assert_eq!(
            std::fs::metadata(&gds).unwrap().len(),
            before_len,
            "the fixture must keep its length to be a fixture"
        );

        let after = session.compile_cell(&config, &cell, Vec::new());
        assert_ne!(
            geometry(&before),
            geometry(&after),
            "a same-length rewrite was served from the cache as unchanged"
        );
    }

    /// The cache is keyed by import, not by source revision, so a rewritten
    /// `.gds` file must retire it even though no Argon source changed.
    #[test]
    fn rewriting_the_gds_file_retires_the_import() {
        use ::gds::{GdsBoundary, GdsElement, GdsLibrary, GdsPoint, GdsStruct};

        let source = "cell top() {\n    let i = inst(child(), x = 0., y = 0.);\n}\n";
        let (_dir, config, gds) = scratch_gds_workspace(source);
        let root = config.root_lib().to_path_buf();
        let cell = vec!["top".to_owned()];

        let mut session = IncrementalCompiler::new();
        session.set_source_text(root.clone(), source);
        let before = session.compile_cell(&config, &cell, Vec::new());

        // Same structure name, different geometry.
        let mut library = GdsLibrary::new("fixture");
        let mut child = GdsStruct::new("child");
        child.elems.push(GdsElement::GdsBoundary(GdsBoundary {
            layer: 235,
            datatype: 4,
            xy: vec![
                GdsPoint::new(0, 0),
                GdsPoint::new(0, 9_000),
                GdsPoint::new(9_000, 9_000),
                GdsPoint::new(9_000, 0),
                GdsPoint::new(0, 0),
            ],
            ..Default::default()
        }));
        library.structs.push(child);
        library.save(&gds).expect("rewrite scratch gds");

        let after = session.compile_cell(&config, &cell, Vec::new());
        assert_ne!(
            geometry(&before),
            geometry(&after),
            "a rewritten GDS was served from the cache as unchanged"
        );
        let mut fresh = IncrementalCompiler::new();
        fresh.set_source_text(root, source);
        assert_eq!(
            geometry(&after),
            geometry(&fresh.compile_cell(&config, &cell, Vec::new()))
        );
    }

    /// Edit latency for a design whose cost is in *source* cells rather than
    /// an import: `examples/multi_stress_shapes`'s `multi_shapes()` has a
    /// two-line body over two children of ten thousand rectangles each.
    ///
    /// Both children live in the same file as the parent being edited, so
    /// reusing them needs per-declaration fingerprints rather than a
    /// file-granular check. The child timings are reported alongside so the
    /// share of the edit that is reusable is visible directly.
    #[test]
    #[ignore = "incremental timing benchmark"]
    fn bench_multi_shapes_edit() {
        let dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/multi_stress_shapes");
        let root = dir.join("lib.ar");
        if !root.exists() {
            eprintln!("multi_shapes,skipped=missing_fixture");
            return;
        }
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech));
        let cell = vec!["multi_shapes".to_owned()];
        let source = std::fs::read_to_string(&root).expect("read library");

        let mut session = IncrementalCompiler::new();
        session.set_source_text(root.clone(), source.clone());
        let start = std::time::Instant::now();
        let _ = session.compile_cell(&config, &cell, Vec::new());
        let cold = start.elapsed();
        let start = std::time::Instant::now();
        let _ = session.compile_cell(&config, &cell, Vec::new());
        let warm = start.elapsed();

        let opening = "cell multi_shapes() {";
        let drawn = source.replacen(
            opening,
            &format!("{opening}\n    rect(\"met1\", x0i = 0., y0i = 0., x1i = 100., y1i = 100.);"),
            1,
        );
        assert_ne!(drawn, source, "the fixture must contain `{opening}`");
        session.set_source_text(root.clone(), drawn);
        let start = std::time::Instant::now();
        let _ = session.compile_cell(&config, &cell, Vec::new());
        let edited = start.elapsed();

        // The children on their own: the part of the edit that is reusable in
        // principle but is re-executed today. Timed standalone, so each
        // repeats the parse and static analysis the combined compile shares --
        // which makes their sum an *upper* bound on the reusable share, and is
        // why it can land a hair above 1.
        let mut children = Vec::new();
        for name in ["shapes", "shapes_loop"] {
            let mut child = IncrementalCompiler::new();
            child.set_source_text(root.clone(), source.clone());
            let start = std::time::Instant::now();
            let _ = child.compile_cell(&config, &[name.to_owned()], vec![CellArg::Int(10000)]);
            children.push((name, start.elapsed()));
        }
        let reusable = children.iter().map(|(_, dt)| dt.as_nanos()).sum::<u128>();
        eprintln!(
            "multi_shapes,cold_ns={},warm_ns={},drawn_rect_ns={},{},reusable_ns={},reusable_share_upper_bound={:.3}",
            cold.as_nanos(),
            warm.as_nanos(),
            edited.as_nanos(),
            children
                .iter()
                .map(|(name, dt)| format!("{name}_ns={}", dt.as_nanos()))
                .collect::<Vec<_>>()
                .join(","),
            reusable,
            (reusable as f64 / edited.as_nanos() as f64).min(1.0),
        );
    }

    #[test]
    #[ignore = "incremental timing benchmark"]
    fn bench_incremental_session() {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let root = examples.join("immediate/lib.ar");
        let tech = examples.join("tech/basic.tech.toml");
        let source = std::fs::read_to_string(&root).unwrap();
        let cell = vec!["immediate".to_owned()];
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech));
        let retained_before = crate::bench_alloc::live();
        let mut compiler = IncrementalCompiler::new();

        let start = std::time::Instant::now();
        let _ = compiler.compile_cell(&config, &cell, Vec::new());
        let cold = start.elapsed();
        let start = std::time::Instant::now();
        let _ = compiler.compile_cell(&config, &cell, Vec::new());
        let warm = start.elapsed();

        compiler.set_source_text(root.clone(), format!("{source}\n// isolated edit\n"));
        let start = std::time::Instant::now();
        let _ = compiler.compile_cell(&config, &cell, Vec::new());
        let edited = start.elapsed();
        let retained = crate::bench_alloc::live().saturating_sub(retained_before);
        eprintln!(
            "incremental,cold_ns={},warm_ns={},isolated_edit_ns={},parse_hits={},files_reparsed={},static_hits={},static_misses={},execution_hits={},execution_misses={},retained_bytes={retained}",
            cold.as_nanos(),
            warm.as_nanos(),
            edited.as_nanos(),
            compiler.stats().parse_cache_hits,
            compiler.stats().files_reparsed,
            compiler.stats().static_cache_hits,
            compiler.stats().static_cache_misses,
            compiler.stats().execution_cache_hits,
            compiler.stats().execution_cache_misses,
        );
    }
}
