//! Process-local compilation session for editor integrations.
//!
//! The session owns open-document snapshots and makes those snapshots the
//! source of truth while retaining the existing one-shot compiler API. Changed
//! files are reparsed as complete files, canonical syntax fingerprints retain
//! trivia-only static results, and dynamic results remain reusable while every
//! declaration and external input observed by their execution is semantically
//! unchanged.

use std::{
    collections::{HashMap, HashSet, VecDeque, hash_map::DefaultHasher},
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    sync::Arc,
};

use arcstr::{ArcStr, Substr};
use indexmap::IndexMap;
use serde::Serialize;

use crate::{
    ast::{Decl, ModPath, WorkspaceAst, annotated::AnnotatedAst},
    cancellation::CancellationToken,
    compile::{
        self, CellArg, CompileInput, CompileOutput, StaticAnalysis, StaticError,
        StaticErrorCompileOutput, VarId, VarIdTyFrame, VarIdTyMetadata,
    },
    nav::NavIndex,
    parse,
    tech::{
        GdsImportTechnologyFingerprint, LayerValidationTechnologyFingerprint,
        SolverTechnologyFingerprint, Technology, TechnologyFingerprints, read_tech,
    },
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
    pub static_unit_hits: u64,
    pub static_unit_misses: u64,
    pub parse_cache_hits: u64,
    pub files_reparsed: u64,
    pub execution_cache_hits: u64,
    pub execution_cache_misses: u64,
    pub execution_cache_evictions: u64,
    pub cell_artifact_cache_hits: u64,
    pub cell_artifact_cache_misses: u64,
    pub cell_artifact_cache_evictions: u64,
    pub cell_continuation_cache_hits: u64,
    pub cell_continuation_cache_misses: u64,
    pub cell_continuation_cache_evictions: u64,
}

#[derive(Clone)]
struct StaticCache {
    revision: u64,
    environment: StaticEnvironment,
    disk_revisions: Vec<TrackedFileRevision>,
    analysis: StaticAnalysis,
    semantic: Arc<SemanticSnapshot>,
    /// Built lazily, because only editor sessions ask for it.
    ///
    /// The outer `Option` records whether the build has been attempted, so an
    /// index that was built and then judged unusable is not rebuilt from the
    /// whole typed AST on every request until the next edit.
    nav: Option<Option<Arc<NavIndex>>>,
}

/// Configuration that can affect parsing or static semantics. Technology is
/// deliberately absent: it is consumed only by dynamic execution and output
/// validation.
#[derive(Clone, PartialEq, Eq)]
struct StaticEnvironment {
    root_lib: PathBuf,
    dependencies: Vec<(String, PathBuf)>,
    gds_imports: Vec<(String, PathBuf)>,
}

impl From<&WorkspaceConfig> for StaticEnvironment {
    fn from(config: &WorkspaceConfig) -> Self {
        Self {
            root_lib: config.root_lib.clone(),
            dependencies: config.dependencies.clone(),
            gds_imports: config.gds_imports.clone(),
        }
    }
}

#[derive(Clone)]
struct StaticUnit {
    body_fingerprint: Vec<u8>,
    dependency_interfaces: Vec<(ModPath, Vec<u8>)>,
    interface_fingerprint: Vec<u8>,
    origins: Vec<cfgrammar::Span>,
    ast: AnnotatedAst<VarIdTyMetadata>,
    bindings: VarIdTyFrame,
    errors: Vec<StaticError>,
}

const EXECUTION_CACHE_CAPACITY: usize = 32;
const CELL_ARTIFACT_CACHE_CAPACITY: usize = 128;
const CELL_CONTINUATION_CACHE_CAPACITY: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CellArgKey {
    Float(u64),
    Int(i64),
    Bool(bool),
    String(String),
    Enum(String),
    Seq(Vec<CellArgKey>),
}

impl From<&CellArg> for CellArgKey {
    fn from(value: &CellArg) -> Self {
        match value {
            CellArg::Float(value) => Self::Float(value.to_bits()),
            CellArg::Int(value) => Self::Int(*value),
            CellArg::Bool(value) => Self::Bool(*value),
            CellArg::String(value) => Self::String(value.clone()),
            CellArg::Enum(value) => Self::Enum(value.clone()),
            CellArg::Seq(values) => Self::Seq(values.iter().map(Self::from).collect()),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum ExecutionTarget {
    Cell {
        path: Vec<String>,
        args: Vec<CellArgKey>,
    },
    Invocation(String),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ExecutionRequest {
    target: ExecutionTarget,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackedGdsImport {
    name: String,
    path: PathBuf,
    revision: Option<FileRevision>,
}

#[derive(Clone)]
struct CurrentTechnology {
    path: PathBuf,
    value: Technology,
    fingerprints: TechnologyFingerprints,
}

#[derive(Clone)]
struct ExecutionEnvironment {
    technology: Option<CurrentTechnology>,
    gds_imports: Vec<TrackedGdsImport>,
}

struct ExecutionContext<'a> {
    environment: &'a ExecutionEnvironment,
    snapshot: &'a SemanticSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ExternalDependencies {
    solver: Option<SolverTechnologyFingerprint>,
    gds_import: Option<GdsImportTechnologyFingerprint>,
    gds_files: Vec<TrackedGdsImport>,
}

impl ExecutionRequest {
    fn new(target: ExecutionTarget) -> Self {
        Self { target }
    }
}

impl ExecutionEnvironment {
    fn new(config: &WorkspaceConfig) -> Self {
        let technology = config.tech.as_ref().and_then(|path| {
            let value = read_tech(path).ok()?;
            let fingerprints = value.fingerprints();
            Some(CurrentTechnology {
                path: path.clone(),
                value,
                fingerprints,
            })
        });
        Self {
            technology,
            gds_imports: config
                .gds_imports
                .iter()
                .map(|(name, path)| TrackedGdsImport {
                    name: name.clone(),
                    path: path.clone(),
                    revision: file_revision(path),
                })
                .collect(),
        }
    }
}

impl ExternalDependencies {
    fn observed(
        environment: &ExecutionEnvironment,
        dependencies: &[CachedDependency],
        root: Option<&DeclarationIdentity>,
    ) -> Option<Self> {
        let technology = environment.technology.as_ref()?;
        let dependency_names = dependencies
            .iter()
            .map(|dependency| declaration_config_name(&dependency.identity))
            .collect::<HashSet<_>>();
        let gds_files = environment
            .gds_imports
            .iter()
            .filter(|import| dependency_names.contains(&import.name))
            .cloned()
            .collect::<Vec<_>>();
        let root_is_gds = root.is_some_and(|root| {
            let root = declaration_config_name(root);
            environment
                .gds_imports
                .iter()
                .any(|import| import.name == root)
        });
        let uses_solver =
            root.map_or_else(|| dependencies.len() > gds_files.len(), |_| !root_is_gds);
        Some(Self {
            solver: uses_solver.then_some(technology.fingerprints.solver),
            gds_import: (!gds_files.is_empty()).then(|| technology.fingerprints.gds_import.clone()),
            gds_files,
        })
    }

    fn is_current(&self, environment: &ExecutionEnvironment) -> bool {
        let Some(technology) = &environment.technology else {
            return false;
        };
        self.solver
            .is_none_or(|solver| solver == technology.fingerprints.solver)
            && self
                .gds_import
                .as_ref()
                .is_none_or(|gds| *gds == technology.fingerprints.gds_import)
            && self.gds_files.iter().all(|dependency| {
                environment
                    .gds_imports
                    .iter()
                    .any(|current| current == dependency)
            })
    }
}

fn declaration_config_name(identity: &DeclarationIdentity) -> String {
    if identity.module.is_empty() {
        identity.name.to_string()
    } else {
        format!("{}::{}", identity.module.join("::"), identity.name)
    }
}

fn direct_zero_argument_cell(
    ast: &WorkspaceAst<VarIdTyMetadata>,
    source: &str,
) -> Option<Vec<String>> {
    let call = parse::parse_cell(source).ok()?;
    if !call.args.posargs.is_empty() || !call.args.kwargs.is_empty() {
        return None;
    }
    let cell = call
        .func
        .path
        .iter()
        .map(|component| component.name.to_string())
        .collect::<Vec<_>>();
    let name = cell.last()?;
    let module = match cell.first().map(String::as_str) {
        Some("std") => vec!["std".to_owned()],
        Some("lib") => cell
            .iter()
            .skip(1)
            .take(cell.len().saturating_sub(2))
            .cloned()
            .collect(),
        _ => cell.iter().take(cell.len() - 1).cloned().collect(),
    };
    let exists = ast.get(&module).is_some_and(|module| {
        module.ast.decls.iter().any(
            |declaration| matches!(declaration, Decl::Cell(cell) if cell.name.name == name.as_str()),
        )
    });
    exists.then_some(cell)
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum DeclarationKind {
    Cell,
    Function,
}

/// A declaration identity is independent of traversal order and source spans.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct DeclarationIdentity {
    module: ModPath,
    kind: DeclarationKind,
    name: Substr,
}

#[derive(Clone)]
struct DeclarationSnapshot {
    fingerprint: Vec<u8>,
    source_path: PathBuf,
    source_text: ArcStr,
    origins: Vec<cfgrammar::Span>,
}

#[derive(Default)]
struct SemanticSnapshot {
    declarations: HashMap<DeclarationIdentity, DeclarationSnapshot>,
    ambiguous: HashSet<DeclarationIdentity>,
    vars: HashMap<VarId, DeclarationIdentity>,
}

#[derive(Clone)]
struct CachedDependency {
    identity: DeclarationIdentity,
    snapshot: DeclarationSnapshot,
}

#[derive(Clone)]
struct ExecutionCacheEntry {
    request: ExecutionRequest,
    dependencies: Vec<CachedDependency>,
    external_dependencies: ExternalDependencies,
    layer_validation: LayerValidationTechnologyFingerprint,
    tech_path: PathBuf,
    output: CompileOutput,
}

#[derive(Clone)]
struct CachedCellArtifact {
    external_dependencies: ExternalDependencies,
    dependencies: Vec<CachedDependency>,
    artifact: compile::CellArtifact,
}

#[derive(Clone)]
struct CachedCellContinuation {
    external_dependencies: ExternalDependencies,
    target: DeclarationIdentity,
    dependencies: Vec<CachedDependency>,
    continuation: compile::CellContinuation,
}

/// Stateful compiler used by long-lived analyzer processes.
#[derive(Default, Clone)]
pub struct IncrementalCompiler {
    revision: u64,
    sources: IndexMap<PathBuf, ArcStr>,
    parse_cache: parse::ParseCache,
    static_cache: Option<StaticCache>,
    static_units: IndexMap<ModPath, StaticUnit>,
    execution_cache: VecDeque<ExecutionCacheEntry>,
    cell_artifact_cache: VecDeque<CachedCellArtifact>,
    cell_continuation_cache: VecDeque<CachedCellContinuation>,
    /// The most recent navigation index that had content. Retained so that
    /// editor navigation keeps answering while the workspace does not
    /// type-check, which is most of the time while someone is typing.
    last_good_nav: Option<Arc<NavIndex>>,
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

    pub fn analyze_workspace(&mut self, config: &WorkspaceConfig) -> StaticAnalysis {
        self.ensure_analysis(config);
        self.static_cache
            .as_ref()
            .expect("analysis cache was populated")
            .analysis
            .clone()
    }

    /// Returns no result when a newer editor request cancels this analysis.
    pub fn analyze_workspace_cancellable(
        &mut self,
        config: &WorkspaceConfig,
        cancellation: &CancellationToken,
    ) -> Option<StaticAnalysis> {
        self.ensure_analysis_cancellable(config, Some(cancellation))
            .then(|| {
                self.static_cache
                    .as_ref()
                    .expect("analysis cache was populated")
                    .analysis
                    .clone()
            })
    }

    fn ensure_analysis(&mut self, config: &WorkspaceConfig) {
        assert!(self.ensure_analysis_cancellable(config, None));
    }

    fn ensure_analysis_cancellable(
        &mut self,
        config: &WorkspaceConfig,
        cancellation: Option<&CancellationToken>,
    ) -> bool {
        if cancellation.is_some_and(CancellationToken::is_cancelled) {
            return false;
        }
        let environment = StaticEnvironment::from(config);
        if let Some(cache) = &self.static_cache
            && cache.revision == self.revision
            && cache.environment == environment
            && cache.disk_revisions == self.tracked_file_revisions(config, &cache.analysis)
        {
            self.stats.static_cache_hits += 1;
            return true;
        }

        if self
            .static_cache
            .as_ref()
            .is_some_and(|cache| cache.environment != environment)
        {
            self.static_units.clear();
        }

        let parse_hits = self.parse_cache.hits();
        let parse_misses = self.parse_cache.misses();
        let Some(parse_output) = parse::parse_workspace_with_config_sources_and_cache_cancellable(
            config,
            &self.sources,
            &mut self.parse_cache,
            cancellation,
        ) else {
            return false;
        };
        self.stats.parse_cache_hits += self.parse_cache.hits() - parse_hits;
        self.stats.files_reparsed += self.parse_cache.misses() - parse_misses;
        let parse_errors = parse_output.static_errors();
        let ast = parse_output.ast();
        let analysis = if parse_errors.is_empty() {
            let Some((analysis, hits, misses)) =
                self.analyze_modules_incrementally(ast, cancellation)
            else {
                return false;
            };
            self.stats.static_unit_hits += hits;
            self.stats.static_unit_misses += misses;
            if hits > 0 && misses == 0 {
                self.stats.static_cache_hits += 1;
            } else {
                self.stats.static_cache_misses += 1;
            }
            analysis
        } else {
            self.stats.static_cache_misses += 1;
            compile::analyze_workspace_ast(ast, parse_errors)
        };
        let disk_revisions = self.tracked_file_revisions(config, &analysis);
        let semantic = Arc::new(SemanticSnapshot::new(&analysis));
        self.static_cache = Some(StaticCache {
            revision: self.revision,
            environment,
            disk_revisions,
            analysis,
            semantic,
            nav: None,
        });
        true
    }

    fn analyze_modules_incrementally(
        &mut self,
        ast: parse::WorkspaceParseAst,
        cancellation: Option<&CancellationToken>,
    ) -> Option<(StaticAnalysis, u64, u64)> {
        if !ast.contains_key(&vec![]) {
            return Some((
                StaticAnalysis {
                    ast,
                    typed_ast: None,
                    errors: Vec::new(),
                },
                0,
                0,
            ));
        }

        let (dependencies, dependency_errors) = compile::module_dependencies(&ast);
        if !dependency_errors.is_empty() {
            return Some((
                StaticAnalysis {
                    ast,
                    typed_ast: Some(IndexMap::new()),
                    errors: dependency_errors,
                },
                0,
                1,
            ));
        }

        let order = module_analysis_order(&ast, &dependencies);
        let mut bindings: IndexMap<ModPath, VarIdTyFrame> = IndexMap::new();
        let mut typed_ast: WorkspaceAst<VarIdTyMetadata> = IndexMap::new();
        let mut errors = Vec::new();
        let mut hits = 0;
        let mut misses = 0;

        for module in order {
            if cancellation.is_some_and(CancellationToken::is_cancelled) {
                return None;
            }
            let parsed = &ast[&module];
            let (body_fingerprint, origins) = declaration_semantics(&parsed.ast);
            let dependency_interfaces = dependencies[&module]
                .iter()
                .map(|dependency| {
                    let interface = bindings
                        .get(dependency)
                        .expect("module dependencies are analyzed first")
                        .interface_fingerprint();
                    (dependency.clone(), interface)
                })
                .collect::<Vec<_>>();

            let reused = self.static_units.get(&module).and_then(|unit| {
                (unit.body_fingerprint == body_fingerprint
                    && unit.dependency_interfaces == dependency_interfaces
                    && unit.ast.path == parsed.path)
                    .then(|| remap_static_unit(unit, parsed, &origins))?
            });

            let (module_ast, module_bindings, module_errors, interface_fingerprint) =
                if let Some((module_ast, module_errors)) = reused {
                    hits += 1;
                    let unit = &self.static_units[&module];
                    (
                        module_ast,
                        unit.bindings.clone(),
                        module_errors,
                        unit.interface_fingerprint.clone(),
                    )
                } else {
                    misses += 1;
                    let output = compile::analyze_module_cancellable(
                        &ast,
                        &module,
                        &bindings,
                        cancellation,
                    )?;
                    let interface_fingerprint = output.bindings.interface_fingerprint();
                    (
                        output.ast,
                        output.bindings,
                        output.errors,
                        interface_fingerprint,
                    )
                };

            errors.extend(module_errors.iter().cloned());
            typed_ast.insert(module.clone(), module_ast.clone());
            bindings.insert(module.clone(), module_bindings.clone());
            self.static_units.insert(
                module,
                StaticUnit {
                    body_fingerprint,
                    dependency_interfaces,
                    interface_fingerprint,
                    origins,
                    ast: module_ast,
                    bindings: module_bindings,
                    errors: module_errors,
                },
            );
        }

        self.static_units
            .retain(|module, _| ast.contains_key(module));
        Some((
            StaticAnalysis {
                ast,
                typed_ast: Some(typed_ast),
                errors,
            },
            hits,
            misses,
        ))
    }

    /// Analyzes and executes one cell, retaining results across revisions while
    /// all declarations observed by the execution remain unchanged.
    pub fn compile_cell(
        &mut self,
        config: &WorkspaceConfig,
        cell: &[String],
        args: Vec<CellArg>,
    ) -> CompileOutput {
        self.compile_cell_inner(config, cell, args, None)
            .expect("uncancellable cell compilation cannot be cancelled")
    }

    pub fn compile_cell_cancellable(
        &mut self,
        config: &WorkspaceConfig,
        cell: &[String],
        args: Vec<CellArg>,
        cancellation: &CancellationToken,
    ) -> Option<CompileOutput> {
        self.compile_cell_inner(config, cell, args, Some(cancellation))
    }

    fn compile_cell_inner(
        &mut self,
        config: &WorkspaceConfig,
        cell: &[String],
        args: Vec<CellArg>,
        cancellation: Option<&CancellationToken>,
    ) -> Option<CompileOutput> {
        if !self.ensure_analysis_cancellable(config, cancellation) {
            return None;
        }
        let snapshot = {
            let cache = self
                .static_cache
                .as_ref()
                .expect("analysis cache was populated");
            let analysis = &cache.analysis;
            if !analysis.errors.is_empty() {
                return Some(CompileOutput::StaticErrors(StaticErrorCompileOutput {
                    errors: analysis.errors.clone(),
                }));
            }
            if analysis.typed_ast.is_none() {
                return Some(CompileOutput::FatalParseErrors);
            }
            Arc::clone(&cache.semantic)
        };

        let environment = ExecutionEnvironment::new(config);
        let request = ExecutionRequest::new(ExecutionTarget::Cell {
            path: cell.to_vec(),
            args: args.iter().map(CellArgKey::from).collect(),
        });
        if let Some(output) = self.cached_execution(&request, &environment, &snapshot) {
            self.stats.execution_cache_hits += 1;
            return Some(output);
        }
        self.execute_resolved_cell(
            config,
            cell,
            args,
            request,
            ExecutionContext {
                environment: &environment,
                snapshot: &snapshot,
            },
            cancellation,
        )
    }

    fn execute_resolved_cell(
        &mut self,
        config: &WorkspaceConfig,
        cell: &[String],
        args: Vec<CellArg>,
        request: ExecutionRequest,
        context: ExecutionContext<'_>,
        cancellation: Option<&CancellationToken>,
    ) -> Option<CompileOutput> {
        self.stats.execution_cache_misses += 1;
        let cell_refs = cell.iter().map(String::as_str).collect::<Vec<_>>();
        let ast = self
            .static_cache
            .as_ref()
            .expect("analysis cache was populated")
            .analysis
            .typed_ast
            .as_ref()
            .expect("typed AST existence was checked");
        let artifacts = self.reusable_cell_artifacts(context.environment, context.snapshot);
        let continuations = self.reusable_cell_continuations(context.environment, context.snapshot);
        let execution = compile::execute_cell_tracked_with_artifacts_cancellable(
            ast,
            CompileInput {
                cell: &cell_refs,
                args,
            },
            config,
            artifacts,
            continuations,
            cancellation,
        )?;
        self.stats.cell_artifact_cache_hits += execution.artifact_hits;
        self.stats.cell_artifact_cache_misses += execution.artifact_misses;
        self.stats.cell_continuation_cache_hits += execution.continuation_hits;
        self.stats.cell_continuation_cache_misses += execution.continuation_misses;
        self.store_cell_artifacts(
            context.environment,
            execution.artifacts.clone(),
            context.snapshot,
        );
        if let Some(continuation) = execution.continuation.clone() {
            self.store_cell_continuation(context.environment, continuation, context.snapshot);
        }
        let output = execution.output;
        self.store_execution(
            request,
            context.environment,
            execution.dependencies,
            context.snapshot,
            &output,
        );
        Some(output)
    }

    /// Analyzes and executes a source-level cell invocation. A directly declared
    /// zero-argument cell uses the cached typed AST; other invocations are
    /// spliced into a clone of the current snapshot so arbitrary argument
    /// expressions are resolved and type-checked without polluting the cached
    /// editor AST.
    pub fn compile_invocation(
        &mut self,
        config: &WorkspaceConfig,
        source: &str,
    ) -> Result<CompileOutput, anyhow::Error> {
        self.compile_invocation_inner(config, source, None)
            .map(|output| output.expect("uncancellable invocation cannot be cancelled"))
    }

    pub fn compile_invocation_cancellable(
        &mut self,
        config: &WorkspaceConfig,
        source: &str,
        cancellation: &CancellationToken,
    ) -> Result<Option<CompileOutput>, anyhow::Error> {
        self.compile_invocation_inner(config, source, Some(cancellation))
    }

    fn compile_invocation_inner(
        &mut self,
        config: &WorkspaceConfig,
        source: &str,
        cancellation: Option<&CancellationToken>,
    ) -> Result<Option<CompileOutput>, anyhow::Error> {
        if !self.ensure_analysis_cancellable(config, cancellation) {
            return Ok(None);
        }
        let snapshot = {
            let cache = self
                .static_cache
                .as_ref()
                .expect("analysis cache was populated");
            let analysis = &cache.analysis;
            if !analysis.errors.is_empty() {
                return Ok(Some(CompileOutput::StaticErrors(
                    StaticErrorCompileOutput {
                        errors: analysis.errors.clone(),
                    },
                )));
            }
            Arc::clone(&cache.semantic)
        };
        let environment = ExecutionEnvironment::new(config);
        let request = ExecutionRequest::new(ExecutionTarget::Invocation(source.to_owned()));
        if let Some(output) = self.cached_execution(&request, &environment, &snapshot) {
            self.stats.execution_cache_hits += 1;
            return Ok(Some(output));
        }

        let direct_cell = self
            .static_cache
            .as_ref()
            .expect("analysis cache was populated")
            .analysis
            .typed_ast
            .as_ref()
            .and_then(|ast| direct_zero_argument_cell(ast, source));
        if let Some(cell) = direct_cell {
            return Ok(self.execute_resolved_cell(
                config,
                &cell,
                Vec::new(),
                request,
                ExecutionContext {
                    environment: &environment,
                    snapshot: &snapshot,
                },
                cancellation,
            ));
        }

        self.stats.execution_cache_misses += 1;
        let mut ast = self
            .static_cache
            .as_ref()
            .expect("analysis cache was populated")
            .analysis
            .ast
            .clone();
        let invocation = parse::splice_cell_invocation(&mut ast, source)?;
        let Some(static_result) = compile::static_compile_cancellable(&ast, cancellation) else {
            return Ok(None);
        };
        let Some((typed_ast, static_output)) = static_result else {
            return Ok(Some(CompileOutput::FatalParseErrors));
        };
        let output = if static_output.errors.is_empty() {
            let invocation_analysis = StaticAnalysis {
                ast,
                typed_ast: Some(typed_ast),
                errors: Vec::new(),
            };
            let invocation_snapshot = SemanticSnapshot::new(&invocation_analysis);
            let artifacts = self.reusable_cell_artifacts(&environment, &invocation_snapshot);
            let Some(execution) =
                compile::execute_cell_invocation_tracked_with_artifacts_cancellable(
                    invocation_analysis
                        .typed_ast
                        .as_ref()
                        .expect("invocation AST was populated"),
                    &invocation,
                    config,
                    artifacts,
                    Vec::new(),
                    cancellation,
                )
            else {
                return Ok(None);
            };
            self.stats.cell_artifact_cache_hits += execution.artifact_hits;
            self.stats.cell_artifact_cache_misses += execution.artifact_misses;
            self.store_cell_artifacts(
                &environment,
                execution.artifacts.clone(),
                &invocation_snapshot,
            );
            let output = execution.output;
            self.store_execution(
                request,
                &environment,
                execution.dependencies,
                &invocation_snapshot,
                &output,
            );
            output
        } else {
            CompileOutput::StaticErrors(static_output)
        };
        Ok(Some(output))
    }

    fn invalidate(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.stats.revision = self.revision;
    }

    fn cached_execution(
        &mut self,
        request: &ExecutionRequest,
        environment: &ExecutionEnvironment,
        snapshot: &SemanticSnapshot,
    ) -> Option<CompileOutput> {
        let index = self.execution_cache.iter().position(|entry| {
            entry.request == *request
                && entry.external_dependencies.is_current(environment)
                && entry.dependencies_are_current(snapshot)
        })?;
        let mut entry = self
            .execution_cache
            .remove(index)
            .expect("cache index came from the same deque");
        let (output, spans_were_remapped) = entry.remapped_output(snapshot)?;
        let technology = environment.technology.as_ref()?;
        let recheck_layers = entry.layer_validation != technology.fingerprints.layer_validation
            || entry.tech_path != technology.path;
        let output = compile::refresh_output_technology(
            output,
            technology.value.clone(),
            &technology.path,
            recheck_layers,
        );
        // Updating an entry whose source spans were remapped would associate
        // current spans with the old dependency snapshots. Technology-only
        // changes have no remap, so cache their refreshed validation result.
        if !spans_were_remapped {
            entry.output = output.clone();
            entry.layer_validation = technology.fingerprints.layer_validation.clone();
            entry.tech_path = technology.path.clone();
        }
        self.execution_cache.push_back(entry);
        Some(output)
    }

    fn store_execution(
        &mut self,
        request: ExecutionRequest,
        environment: &ExecutionEnvironment,
        dependency_vars: indexmap::IndexSet<VarId>,
        snapshot: &SemanticSnapshot,
        output: &CompileOutput,
    ) {
        if !matches!(
            output,
            CompileOutput::Valid(_) | CompileOutput::ExecErrors(_)
        ) {
            return;
        }
        let Some(dependencies) = cached_dependencies(dependency_vars, snapshot) else {
            return;
        };
        // An invalid lookup observes no declaration. Caching it would hide a
        // newly added declaration in a later source revision.
        if dependencies.is_empty() {
            return;
        }
        let Some(external_dependencies) =
            ExternalDependencies::observed(environment, &dependencies, None)
        else {
            return;
        };
        let Some(technology) = &environment.technology else {
            return;
        };
        self.execution_cache.retain(|entry| {
            entry.request != request
                || entry.external_dependencies != external_dependencies
                || !entry.same_dependency_versions(&dependencies)
        });
        if self.execution_cache.len() >= EXECUTION_CACHE_CAPACITY {
            self.execution_cache.pop_front();
            self.stats.execution_cache_evictions += 1;
        }
        self.execution_cache.push_back(ExecutionCacheEntry {
            request,
            dependencies,
            external_dependencies,
            layer_validation: technology.fingerprints.layer_validation.clone(),
            tech_path: technology.path.clone(),
            output: output.clone(),
        });
    }

    fn reusable_cell_artifacts(
        &self,
        environment: &ExecutionEnvironment,
        snapshot: &SemanticSnapshot,
    ) -> Vec<compile::CellArtifact> {
        self.cell_artifact_cache
            .iter()
            .filter(|entry| entry.external_dependencies.is_current(environment))
            .filter_map(|entry| entry.remapped_artifact(snapshot))
            .collect()
    }

    fn store_cell_artifacts(
        &mut self,
        environment: &ExecutionEnvironment,
        artifacts: Vec<compile::CellArtifact>,
        snapshot: &SemanticSnapshot,
    ) {
        for artifact in artifacts {
            let Some(dependencies) = cached_dependencies(artifact.dependencies.clone(), snapshot)
            else {
                continue;
            };
            if dependencies.is_empty() {
                continue;
            }
            let Some(root) = snapshot.vars.get(&artifact.root_cell()) else {
                continue;
            };
            let Some(external_dependencies) =
                ExternalDependencies::observed(environment, &dependencies, Some(root))
            else {
                continue;
            };
            self.cell_artifact_cache.retain(|entry| {
                entry.external_dependencies != external_dependencies
                    || !entry.artifact.same_key(&artifact)
                    || !entry.same_dependency_versions(&dependencies)
            });
            if self.cell_artifact_cache.len() >= CELL_ARTIFACT_CACHE_CAPACITY {
                self.cell_artifact_cache.pop_front();
                self.stats.cell_artifact_cache_evictions += 1;
            }
            self.cell_artifact_cache.push_back(CachedCellArtifact {
                external_dependencies,
                dependencies,
                artifact,
            });
        }
    }

    fn reusable_cell_continuations(
        &self,
        environment: &ExecutionEnvironment,
        snapshot: &SemanticSnapshot,
    ) -> Vec<compile::CellContinuation> {
        self.cell_continuation_cache
            .iter()
            .filter(|entry| entry.external_dependencies.is_current(environment))
            .filter(|entry| {
                entry.dependencies.iter().all(|dependency| {
                    let Some(current) = snapshot.declarations.get(&dependency.identity) else {
                        return false;
                    };
                    if dependency.identity == entry.target {
                        current.source_path == dependency.snapshot.source_path
                    } else {
                        let generated_declaration = !dependency.snapshot.origins.is_empty()
                            && dependency.snapshot.origins.iter().all(|origin| {
                                origin.start() >= dependency.snapshot.source_text.len()
                            });
                        current.fingerprint == dependency.snapshot.fingerprint
                            && current.source_path == dependency.snapshot.source_path
                            && (current.origins == dependency.snapshot.origins
                                || generated_declaration)
                    }
                })
            })
            .map(|entry| entry.continuation.clone())
            .collect()
    }

    fn store_cell_continuation(
        &mut self,
        environment: &ExecutionEnvironment,
        continuation: compile::CellContinuation,
        snapshot: &SemanticSnapshot,
    ) {
        let Some(target) = snapshot.vars.get(&continuation.cell()).cloned() else {
            return;
        };
        let Some(dependencies) = cached_dependencies(continuation.dependencies().clone(), snapshot)
        else {
            return;
        };
        if !dependencies
            .iter()
            .any(|dependency| dependency.identity == target)
        {
            return;
        }
        let Some(external_dependencies) =
            ExternalDependencies::observed(environment, &dependencies, Some(&target))
        else {
            return;
        };
        self.cell_continuation_cache.retain(|entry| {
            entry.external_dependencies != external_dependencies
                || entry.target != target
                || !entry.continuation.same_key(&continuation)
        });
        if self.cell_continuation_cache.len() >= CELL_CONTINUATION_CACHE_CAPACITY {
            self.cell_continuation_cache.pop_front();
            self.stats.cell_continuation_cache_evictions += 1;
        }
        self.cell_continuation_cache
            .push_back(CachedCellContinuation {
                external_dependencies,
                target,
                dependencies,
                continuation,
            });
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

type Offset = (usize, usize);
type OriginRemaps = HashMap<PathBuf, HashMap<Offset, Option<Offset>>>;

fn module_analysis_order(
    ast: &parse::WorkspaceParseAst,
    dependencies: &compile::ModuleDependencies,
) -> Vec<ModPath> {
    fn visit(
        module: &ModPath,
        dependencies: &compile::ModuleDependencies,
        visited: &mut HashSet<ModPath>,
        order: &mut Vec<ModPath>,
    ) {
        if !visited.insert(module.clone()) {
            return;
        }
        for dependency in &dependencies[module] {
            visit(dependency, dependencies, visited, order);
        }
        order.push(module.clone());
    }

    let mut visited = HashSet::new();
    let mut order = Vec::new();
    let std_module = vec!["std".to_owned()];
    let root_module = Vec::new();
    for module in [Some(&std_module), Some(&root_module)]
        .into_iter()
        .flatten()
        .filter(|module| ast.contains_key(*module))
        .chain(ast.keys())
    {
        visit(module, dependencies, &mut visited, &mut order);
    }
    order
}

fn remap_static_unit(
    unit: &StaticUnit,
    current: &AnnotatedAst<parse::ParseMetadata>,
    current_origins: &[cfgrammar::Span],
) -> Option<(AnnotatedAst<VarIdTyMetadata>, Vec<StaticError>)> {
    if unit.ast.text == current.text
        && unit.ast.source_text == current.source_text
        && unit.ast.generated_declarations == current.generated_declarations
    {
        return Some((unit.ast.clone(), unit.errors.clone()));
    }
    if unit.origins.len() != current_origins.len() {
        return None;
    }

    let mut path_remaps = HashMap::new();
    insert_origin_pairs(&mut path_remaps, &unit.origins, current_origins);
    let mut value = serde_json::to_value(&unit.ast.ast).ok()?;
    remap_ast_spans(&mut value, &path_remaps).then_some(())?;
    let raw: crate::ast::Ast<arcstr::Substr, VarIdTyMetadata> =
        serde_json::from_value(value).ok()?;
    let mut ast = AnnotatedAst::new(current.text.clone(), &raw, current.path.clone());
    ast.source_text = current.source_text.clone();
    ast.generated_declarations = current.generated_declarations;

    let errors = if unit.errors.is_empty() {
        Vec::new()
    } else {
        let mut changed_paths = HashSet::new();
        changed_paths.insert(current.path.clone());
        let mut remaps = HashMap::new();
        remaps.insert(current.path.clone(), path_remaps);
        let mut value = serde_json::to_value(&unit.errors).ok()?;
        remap_serialized_spans(&mut value, &changed_paths, &remaps).then_some(())?;
        serde_json::from_value(value).ok()?
    };
    Some((ast, errors))
}

fn insert_origin_pairs(
    remaps: &mut HashMap<Offset, Option<Offset>>,
    previous: &[cfgrammar::Span],
    current: &[cfgrammar::Span],
) {
    for (old, new) in previous.iter().zip(current.iter()) {
        let old = (old.start(), old.end());
        let new = (new.start(), new.end());
        remaps
            .entry(old)
            .and_modify(|mapped| {
                if *mapped != Some(new) {
                    *mapped = None;
                }
            })
            .or_insert(Some(new));
    }
}

fn remap_ast_spans(
    value: &mut serde_json::Value,
    remaps: &HashMap<Offset, Option<Offset>>,
) -> bool {
    match value {
        serde_json::Value::Object(fields) => {
            if let Some(old) = fields.get("span").and_then(serialized_span) {
                let Some(Some((start, end))) = remaps.get(&(old.start(), old.end())).copied()
                else {
                    return false;
                };
                let Some(serde_json::Value::Object(span)) = fields.get_mut("span") else {
                    return false;
                };
                span.insert("start".to_owned(), start.into());
                span.insert("end".to_owned(), end.into());
            }
            fields
                .values_mut()
                .all(|value| remap_ast_spans(value, remaps))
        }
        serde_json::Value::Array(values) => values
            .iter_mut()
            .all(|value| remap_ast_spans(value, remaps)),
        _ => true,
    }
}

fn cached_dependencies(
    dependency_vars: indexmap::IndexSet<VarId>,
    snapshot: &SemanticSnapshot,
) -> Option<Vec<CachedDependency>> {
    dependency_vars
        .into_iter()
        .map(|var| {
            let identity = snapshot.vars.get(&var)?.clone();
            let declaration = snapshot.declarations.get(&identity)?.clone();
            Some(CachedDependency {
                identity,
                snapshot: declaration,
            })
        })
        .collect()
}

fn dependency_origin_remaps(
    dependencies: &[CachedDependency],
    snapshot: &SemanticSnapshot,
) -> Option<(HashSet<PathBuf>, OriginRemaps)> {
    let mut changed_paths = HashSet::new();
    let mut remaps = HashMap::new();
    for dependency in dependencies {
        let current = snapshot.declarations.get(&dependency.identity)?;
        if current.fingerprint != dependency.snapshot.fingerprint
            || current.source_path != dependency.snapshot.source_path
        {
            return None;
        }
        if current.source_text == dependency.snapshot.source_text {
            continue;
        }
        if current.origins.len() != dependency.snapshot.origins.len() {
            return None;
        }
        let path = current.source_path.clone();
        let positions_changed = dependency
            .snapshot
            .origins
            .iter()
            .zip(&current.origins)
            .any(|(old, new)| old != new);
        insert_origin_pairs(
            remaps.entry(path).or_default(),
            &dependency.snapshot.origins,
            &current.origins,
        );
        if positions_changed {
            changed_paths.insert(current.source_path.clone());
        }
    }
    Some((changed_paths, remaps))
}

impl SemanticSnapshot {
    fn new(analysis: &StaticAnalysis) -> Self {
        let mut snapshot = Self::default();

        for (module, ast) in &analysis.ast {
            for declaration in &ast.ast.decls {
                let identity = match declaration {
                    Decl::Cell(declaration) => DeclarationIdentity {
                        module: module.clone(),
                        kind: DeclarationKind::Cell,
                        name: declaration.name.name.clone(),
                    },
                    Decl::Fn(declaration) => DeclarationIdentity {
                        module: module.clone(),
                        kind: DeclarationKind::Function,
                        name: declaration.name.name.clone(),
                    },
                    _ => continue,
                };
                if snapshot.ambiguous.contains(&identity) {
                    continue;
                }
                let (fingerprint, origins) = declaration_semantics(declaration);
                let declaration = DeclarationSnapshot {
                    fingerprint,
                    source_path: ast.path.clone(),
                    source_text: ast.source_text.clone(),
                    origins,
                };
                if snapshot
                    .declarations
                    .insert(identity.clone(), declaration)
                    .is_some()
                {
                    snapshot.declarations.remove(&identity);
                    snapshot.ambiguous.insert(identity);
                }
            }
        }

        if let Some(typed_ast) = analysis.typed_ast.as_ref() {
            for (module, ast) in typed_ast {
                for declaration in &ast.ast.decls {
                    let (var, identity) = match declaration {
                        Decl::Cell(declaration) => (
                            declaration.metadata.1,
                            DeclarationIdentity {
                                module: module.clone(),
                                kind: DeclarationKind::Cell,
                                name: declaration.name.name.clone(),
                            },
                        ),
                        Decl::Fn(declaration) => (
                            declaration.metadata.1,
                            DeclarationIdentity {
                                module: module.clone(),
                                kind: DeclarationKind::Function,
                                name: declaration.name.name.clone(),
                            },
                        ),
                        _ => continue,
                    };
                    if let Some(snapshot_declaration) = snapshot.declarations.get_mut(&identity) {
                        let typed_fingerprint =
                            compile::declaration_contract_fingerprint(declaration)
                                .expect("cells and functions have typed contracts");
                        snapshot_declaration
                            .fingerprint
                            .extend_from_slice(&(typed_fingerprint.len() as u64).to_le_bytes());
                        snapshot_declaration
                            .fingerprint
                            .extend_from_slice(&typed_fingerprint);
                        snapshot.vars.insert(var, identity);
                    }
                }
            }
        }

        snapshot
    }
}

impl ExecutionCacheEntry {
    fn same_dependency_versions(&self, dependencies: &[CachedDependency]) -> bool {
        self.dependencies.len() == dependencies.len()
            && self
                .dependencies
                .iter()
                .zip(dependencies)
                .all(|(left, right)| {
                    left.identity == right.identity
                        && left.snapshot.fingerprint == right.snapshot.fingerprint
                })
    }

    fn dependencies_are_current(&self, snapshot: &SemanticSnapshot) -> bool {
        self.dependencies.iter().all(|dependency| {
            snapshot
                .declarations
                .get(&dependency.identity)
                .is_some_and(|current| {
                    current.fingerprint == dependency.snapshot.fingerprint
                        && current.source_path == dependency.snapshot.source_path
                })
        })
    }

    fn remapped_output(&self, snapshot: &SemanticSnapshot) -> Option<(CompileOutput, bool)> {
        let (changed_paths, remaps) = dependency_origin_remaps(&self.dependencies, snapshot)?;

        if changed_paths.is_empty() {
            return Some((self.output.clone(), false));
        }

        let mut value = serde_json::to_value(&self.output).ok()?;
        remap_serialized_spans(&mut value, &changed_paths, &remaps).then_some(())?;
        Some((serde_json::from_value(value).ok()?, true))
    }
}

impl CachedCellArtifact {
    fn same_dependency_versions(&self, dependencies: &[CachedDependency]) -> bool {
        self.dependencies.len() == dependencies.len()
            && self
                .dependencies
                .iter()
                .zip(dependencies)
                .all(|(left, right)| {
                    left.identity == right.identity
                        && left.snapshot.fingerprint == right.snapshot.fingerprint
                })
    }

    fn remapped_artifact(&self, snapshot: &SemanticSnapshot) -> Option<compile::CellArtifact> {
        let (changed_paths, remaps) = dependency_origin_remaps(&self.dependencies, snapshot)?;
        if changed_paths.is_empty() {
            return Some(self.artifact.clone());
        }
        let mut artifact = self.artifact.clone();
        // Remap only cells whose scopes originate in an edited source file.
        // In particular, a GDS artifact can hold millions of objects but all
        // of its scopes point at the unchanged GDS path. Serializing the whole
        // artifact here would both deep-copy that geometry and defeat Arc
        // identity, even though there is no span in it to update.
        for cell in artifact.cells.values_mut() {
            if !cell
                .scopes
                .values()
                .any(|scope| changed_paths.contains(&scope.span.path))
            {
                continue;
            }
            let mut value = serde_json::to_value(cell.as_ref()).ok()?;
            remap_serialized_spans(&mut value, &changed_paths, &remaps).then_some(())?;
            *cell = Arc::new(serde_json::from_value(value).ok()?);
        }
        let mut errors = serde_json::to_value(&artifact.errors).ok()?;
        remap_serialized_spans(&mut errors, &changed_paths, &remaps).then_some(())?;
        artifact.errors = serde_json::from_value(errors).ok()?;
        Some(artifact)
    }
}

fn declaration_semantics<T: Serialize>(declaration: &T) -> (Vec<u8>, Vec<cfgrammar::Span>) {
    let mut value = serde_json::to_value(declaration)
        .expect("compiler AST declarations should always serialize to JSON");
    let mut origins = Vec::new();
    collect_serialized_spans(&value, &mut origins);
    remove_transient_ast_fields(&mut value);
    let fingerprint =
        serde_json::to_vec(&value).expect("compiler AST declaration JSON should always serialize");
    (fingerprint, origins)
}

fn collect_serialized_spans(value: &serde_json::Value, spans: &mut Vec<cfgrammar::Span>) {
    match value {
        serde_json::Value::Object(fields) => {
            if let Some(span) = fields.get("span").and_then(serialized_span) {
                spans.push(span);
            }
            for value in fields.values() {
                collect_serialized_spans(value, spans);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                collect_serialized_spans(value, spans);
            }
        }
        _ => {}
    }
}

fn serialized_span(value: &serde_json::Value) -> Option<cfgrammar::Span> {
    let fields = value.as_object()?;
    let start = usize::try_from(fields.get("start")?.as_u64()?).ok()?;
    let end = usize::try_from(fields.get("end")?.as_u64()?).ok()?;
    (start <= end).then(|| cfgrammar::Span::new(start, end))
}

fn remove_transient_ast_fields(value: &mut serde_json::Value) {
    match value {
        serde_json::Value::Object(fields) => {
            fields.remove("span");
            fields.remove("metadata");
            for value in fields.values_mut() {
                remove_transient_ast_fields(value);
            }
        }
        serde_json::Value::Array(values) => {
            for value in values {
                remove_transient_ast_fields(value);
            }
        }
        _ => {}
    }
}

fn remap_serialized_spans(
    value: &mut serde_json::Value,
    changed_paths: &HashSet<PathBuf>,
    remaps: &OriginRemaps,
) -> bool {
    match value {
        serde_json::Value::Object(fields) => {
            let source_span = fields
                .get("path")
                .and_then(serde_json::Value::as_str)
                .map(PathBuf::from)
                .zip(fields.get("span").and_then(serialized_span));
            if let Some((path, old)) = source_span
                && changed_paths.contains(&path)
            {
                let Some(Some((start, end))) = remaps
                    .get(&path)
                    .and_then(|path_remaps| path_remaps.get(&(old.start(), old.end())))
                    .copied()
                else {
                    return false;
                };
                let Some(serde_json::Value::Object(span)) = fields.get_mut("span") else {
                    return false;
                };
                span.insert("start".to_owned(), start.into());
                span.insert("end".to_owned(), end.into());
            }
            fields
                .values_mut()
                .all(|value| remap_serialized_spans(value, changed_paths, remaps))
        }
        serde_json::Value::Array(values) => values
            .iter_mut()
            .all(|value| remap_serialized_spans(value, changed_paths, remaps)),
        _ => true,
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_technology(
        dbu: f64,
        display_unit: u64,
        grid: u64,
        layers: &[(&str, i16, i16, &str)],
    ) -> String {
        let mut text = format!("dbu = {dbu:e}\ndisplay_unit = {display_unit}\ngrid = {grid}\n");
        for (name, gds_layer, gds_datatype, color) in layers {
            text.push_str(&format!(
                "\n[[layers]]\nname = \"{name}\"\ngds = [{gds_layer}, {gds_datatype}]\nfill = \"{color}\"\nborder = \"{color}\"\n"
            ));
        }
        text
    }

    fn compiled_data(output: &CompileOutput) -> &compile::CompiledData {
        match output {
            CompileOutput::Valid(data) => data,
            CompileOutput::ExecErrors(output) => output.output.as_ref().unwrap(),
            output => panic!("cell should produce compiled data: {output:?}"),
        }
    }

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
    fn dependency_body_edits_recheck_only_the_changed_module() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let dependency = directory.path().join("dep.ar");
        let root_source = "mod dep;\nuse dep::value;\ncell top() { let result = value(); }\n";
        std::fs::write(&root, root_source).unwrap();
        std::fs::write(&dependency, "fn value() -> Int { 1 }\n").unwrap();
        let config = WorkspaceConfig::new(&root);
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(root.clone(), root_source);
        compiler.set_source_text(dependency.clone(), "fn value() -> Int { 1 }\n");

        let first = compiler.analyze_workspace(&config);
        assert!(first.errors.is_empty(), "{:?}", first.errors);
        let before = compiler.stats();
        compiler.set_source_text(dependency.clone(), "fn value() -> Int { 2 }\n");
        let body_edit = compiler.analyze_workspace(&config);
        assert!(body_edit.errors.is_empty(), "{:?}", body_edit.errors);
        let after_body = compiler.stats();
        assert_eq!(after_body.static_unit_misses, before.static_unit_misses + 1);
        assert_eq!(after_body.static_unit_hits, before.static_unit_hits + 2);

        compiler.set_source_text(dependency, "fn value() -> Float { 2. }\n");
        let interface_edit = compiler.analyze_workspace(&config);
        assert!(
            interface_edit.errors.is_empty(),
            "{:?}",
            interface_edit.errors
        );
        let after_interface = compiler.stats();
        assert_eq!(
            after_interface.static_unit_misses,
            after_body.static_unit_misses + 2
        );
        assert_eq!(
            after_interface.static_unit_hits,
            after_body.static_unit_hits + 1
        );
    }

    #[test]
    fn cancellation_interrupts_long_cell_execution() {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let root = examples.join("range_perf/lib.ar");
        let tech = examples.join("tech/basic.tech.toml");
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech));
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(
            root,
            "cell top() { for i in std::range(500000) { let value = i; } }\n",
        );
        let analysis = compiler.analyze_workspace(&config);
        assert!(analysis.errors.is_empty(), "{:?}", analysis.errors);

        let cancellation = CancellationToken::new();
        let worker_cancellation = cancellation.clone();
        let (started, ready) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            started.send(()).unwrap();
            compiler.compile_cell_cancellable(
                &config,
                &["top".to_owned()],
                Vec::new(),
                &worker_cancellation,
            )
        });
        ready.recv().unwrap();
        std::thread::sleep(std::time::Duration::from_millis(10));
        cancellation.cancel();
        assert!(handle.join().unwrap().is_none());
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
    fn presentation_changes_reuse_geometry_and_refresh_the_output_technology() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let tech = directory.path().join("tech.toml");
        std::fs::write(
            &tech,
            test_technology(1e-9, 10, 1, &[("met1", 1, 0, "#112233")]),
        )
        .unwrap();
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech.clone()));
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(
            root,
            "cell top() { let shape = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.)!; }\n",
        );

        let first = compiler.compile_cell(&config, &["top".to_owned()], Vec::new());
        let before = compiler.stats();
        std::fs::write(
            &tech,
            test_technology(1e-9, 10, 1, &[("met1", 1, 0, "#abcdef")]),
        )
        .unwrap();
        let second = compiler.compile_cell(&config, &["top".to_owned()], Vec::new());

        let first = compiled_data(&first);
        let second = compiled_data(&second);
        assert!(Arc::ptr_eq(
            &first.cells[&first.top],
            &second.cells[&second.top]
        ));
        assert_eq!(
            second.tech.layers[0].fill_color,
            rgb::Rgb::new(0xab, 0xcd, 0xef)
        );
        assert_eq!(
            compiler.stats().execution_cache_hits,
            before.execution_cache_hits + 1
        );
        assert_eq!(
            compiler.stats().execution_cache_misses,
            before.execution_cache_misses
        );
    }

    #[test]
    fn switching_technology_files_does_not_reanalyze_source() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let first_tech = directory.path().join("first.tech.toml");
        let second_tech = directory.path().join("second.tech.toml");
        std::fs::write(
            &first_tech,
            test_technology(1e-9, 10, 1, &[("met1", 1, 0, "#112233")]),
        )
        .unwrap();
        std::fs::write(
            &second_tech,
            test_technology(1e-9, 10, 1, &[("met1", 1, 0, "#abcdef")]),
        )
        .unwrap();
        let first_config = WorkspaceConfig::new(&root).with_tech(Some(first_tech));
        let second_config = WorkspaceConfig::new(&root).with_tech(Some(second_tech));
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(
            root,
            "cell top() { let shape = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.)!; }\n",
        );

        let first = compiler.compile_cell(&first_config, &["top".to_owned()], Vec::new());
        let before = compiler.stats();
        let second = compiler.compile_cell(&second_config, &["top".to_owned()], Vec::new());

        let first = compiled_data(&first);
        let second = compiled_data(&second);
        assert!(Arc::ptr_eq(
            &first.cells[&first.top],
            &second.cells[&second.top]
        ));
        assert_eq!(
            second.tech.layers[0].fill_color,
            rgb::Rgb::new(0xab, 0xcd, 0xef)
        );
        assert_eq!(
            compiler.stats().static_cache_hits,
            before.static_cache_hits + 1
        );
        assert_eq!(
            compiler.stats().static_cache_misses,
            before.static_cache_misses
        );
        assert_eq!(
            compiler.stats().execution_cache_hits,
            before.execution_cache_hits + 1
        );
    }

    #[test]
    fn deleting_a_layer_reuses_geometry_and_only_revalidates_layers() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let tech = directory.path().join("tech.toml");
        std::fs::write(
            &tech,
            test_technology(
                1e-9,
                10,
                1,
                &[("met1", 1, 0, "#112233"), ("met2", 2, 0, "#445566")],
            ),
        )
        .unwrap();
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech.clone()));
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(
            root,
            "cell top() { let shape = rect(\"met2\", x0=0., y0=0., x1=10., y1=10.)!; }\n",
        );

        let first = compiler.compile_cell(&config, &["top".to_owned()], Vec::new());
        assert!(matches!(first, CompileOutput::Valid(_)));
        let before = compiler.stats();
        std::fs::write(
            &tech,
            test_technology(1e-9, 10, 1, &[("met1", 1, 0, "#112233")]),
        )
        .unwrap();
        let second = compiler.compile_cell(&config, &["top".to_owned()], Vec::new());

        let CompileOutput::ExecErrors(errors) = &second else {
            panic!("deleted layer should be diagnosed: {second:?}");
        };
        assert!(errors.errors.iter().any(|error| matches!(
            &error.kind,
            compile::ExecErrorKind::IllegalLayer { layer, .. } if layer == "met2"
        )));
        let first = compiled_data(&first);
        let second = compiled_data(&second);
        assert!(Arc::ptr_eq(
            &first.cells[&first.top],
            &second.cells[&second.top]
        ));
        assert_eq!(
            compiler.stats().execution_cache_hits,
            before.execution_cache_hits + 1
        );
        assert_eq!(
            compiler.stats().execution_cache_misses,
            before.execution_cache_misses
        );

        // Restoring the layer removes the cached validation error without
        // executing the cell or replacing its geometry.
        std::fs::write(
            &tech,
            test_technology(
                1e-9,
                10,
                1,
                &[("met1", 1, 0, "#112233"), ("met2", 2, 0, "#445566")],
            ),
        )
        .unwrap();
        let before_restore = compiler.stats();
        let restored = compiler.compile_cell(&config, &["top".to_owned()], Vec::new());
        assert!(matches!(restored, CompileOutput::Valid(_)));
        let restored = compiled_data(&restored);
        assert!(Arc::ptr_eq(
            &first.cells[&first.top],
            &restored.cells[&restored.top]
        ));
        assert_eq!(
            compiler.stats().execution_cache_hits,
            before_restore.execution_cache_hits + 1
        );
    }

    #[test]
    fn effective_grid_changes_invalidate_solved_source_geometry() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let tech = directory.path().join("tech.toml");
        std::fs::write(
            &tech,
            test_technology(1e-9, 10, 1, &[("met1", 1, 0, "#112233")]),
        )
        .unwrap();
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech.clone()));
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(
            root,
            "cell top() { let shape = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.)!; }\n",
        );

        let first = compiler.compile_cell(&config, &["top".to_owned()], Vec::new());
        let before = compiler.stats();
        std::fs::write(
            &tech,
            test_technology(1e-9, 10, 2, &[("met1", 1, 0, "#112233")]),
        )
        .unwrap();
        let second = compiler.compile_cell(&config, &["top".to_owned()], Vec::new());

        let first = compiled_data(&first);
        let second = compiled_data(&second);
        assert!(!Arc::ptr_eq(
            &first.cells[&first.top],
            &second.cells[&second.top]
        ));
        assert_eq!(
            compiler.stats().execution_cache_misses,
            before.execution_cache_misses + 1
        );
        assert_eq!(
            compiler.stats().cell_artifact_cache_hits,
            before.cell_artifact_cache_hits
        );
    }

    #[test]
    fn equivalent_grid_ratios_reuse_source_geometry_across_unit_changes() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let tech = directory.path().join("tech.toml");
        std::fs::write(
            &tech,
            test_technology(1e-9, 10, 1, &[("met1", 1, 0, "#112233")]),
        )
        .unwrap();
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech.clone()));
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(
            root,
            "cell top() { let shape = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.)!; }\n",
        );

        let first = compiler.compile_cell(&config, &["top".to_owned()], Vec::new());
        let before = compiler.stats();
        std::fs::write(
            &tech,
            test_technology(2e-9, 20, 2, &[("met1", 1, 0, "#112233")]),
        )
        .unwrap();
        let second = compiler.compile_cell(&config, &["top".to_owned()], Vec::new());

        let first = compiled_data(&first);
        let second = compiled_data(&second);
        assert!(Arc::ptr_eq(
            &first.cells[&first.top],
            &second.cells[&second.top]
        ));
        assert_eq!(second.tech.dbu, 2e-9);
        assert_eq!(second.tech.display_unit, 20);
        assert_eq!(
            compiler.stats().execution_cache_hits,
            before.execution_cache_hits + 1
        );
    }

    #[test]
    fn grid_changes_keep_imported_gds_artifacts_reusable() {
        use ::gds::{GdsBoundary, GdsElement, GdsLibrary, GdsPoint, GdsStruct};

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let tech = directory.path().join("tech.toml");
        let gds_path = directory.path().join("macro.gds");
        std::fs::write(
            &tech,
            test_technology(1e-9, 10, 1, &[("met1", 7, 3, "#112233")]),
        )
        .unwrap();
        let mut library = GdsLibrary::new("fixture");
        let mut structure = GdsStruct::new("macro");
        structure.elems.push(GdsElement::GdsBoundary(GdsBoundary {
            layer: 7,
            datatype: 3,
            xy: vec![
                GdsPoint::new(0, 0),
                GdsPoint::new(0, 20),
                GdsPoint::new(10, 20),
                GdsPoint::new(10, 0),
            ],
            ..Default::default()
        }));
        library.structs.push(structure);
        library.save(&gds_path).unwrap();
        let config = WorkspaceConfig::new(&root)
            .with_tech(Some(tech.clone()))
            .with_gds_imports([("macro".to_owned(), gds_path)]);
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(root, "cell top() { let imported = inst(macro()); }\n");

        let first = compiler.compile_cell(&config, &["top".to_owned()], Vec::new());
        let before = compiler.stats();
        std::fs::write(
            &tech,
            test_technology(1e-9, 10, 2, &[("met1", 7, 3, "#112233")]),
        )
        .unwrap();
        let second = compiler.compile_cell(&config, &["top".to_owned()], Vec::new());

        let first = compiled_data(&first);
        let second = compiled_data(&second);
        let imported = first.cells.keys().find(|cell| **cell != first.top).unwrap();
        assert!(Arc::ptr_eq(&first.cells[imported], &second.cells[imported]));
        assert_eq!(
            compiler.stats().execution_cache_misses,
            before.execution_cache_misses + 1
        );
        assert!(compiler.stats().cell_artifact_cache_hits > before.cell_artifact_cache_hits);
    }

    #[test]
    fn unrelated_gds_file_changes_do_not_invalidate_an_execution() {
        use ::gds::{GdsBoundary, GdsElement, GdsLibrary, GdsPoint, GdsStruct};

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let tech = directory.path().join("tech.toml");
        let used_gds = directory.path().join("used.gds");
        let unused_gds = directory.path().join("unused.gds");
        std::fs::write(
            &tech,
            test_technology(1e-9, 10, 1, &[("met1", 7, 3, "#112233")]),
        )
        .unwrap();
        let mut library = GdsLibrary::new("fixture");
        let mut structure = GdsStruct::new("used");
        structure.elems.push(GdsElement::GdsBoundary(GdsBoundary {
            layer: 7,
            datatype: 3,
            xy: vec![
                GdsPoint::new(0, 0),
                GdsPoint::new(0, 20),
                GdsPoint::new(10, 20),
                GdsPoint::new(10, 0),
            ],
            ..Default::default()
        }));
        library.structs.push(structure);
        library.save(&used_gds).unwrap();
        std::fs::write(&unused_gds, "unused GDS revision one").unwrap();
        let config = WorkspaceConfig::new(&root)
            .with_tech(Some(tech))
            .with_gds_imports([
                ("used".to_owned(), used_gds),
                ("unused".to_owned(), unused_gds.clone()),
            ]);
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(root, "cell top() { let imported = inst(used()); }\n");

        let first = compiler.compile_cell(&config, &["top".to_owned()], Vec::new());
        let before = compiler.stats();
        std::fs::write(&unused_gds, "unused GDS revision two is different").unwrap();
        let second = compiler.compile_cell(&config, &["top".to_owned()], Vec::new());

        let first = compiled_data(&first);
        let second = compiled_data(&second);
        assert!(Arc::ptr_eq(
            &first.cells[&first.top],
            &second.cells[&second.top]
        ));
        assert_eq!(
            compiler.stats().execution_cache_hits,
            before.execution_cache_hits + 1
        );
        assert_eq!(
            compiler.stats().execution_cache_misses,
            before.execution_cache_misses
        );
    }

    #[test]
    fn trivia_edits_reuse_execution_with_current_source_spans() {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let root = examples.join("immediate/lib.ar");
        let tech = examples.join("tech/basic.tech.toml");
        let source = std::fs::read_to_string(&root).unwrap();
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech));
        let cell = vec!["immediate".to_owned()];
        let mut compiler = IncrementalCompiler::new();

        compiler.set_source_text(root.clone(), source.clone());
        let _ = compiler.compile_cell(&config, &cell, Vec::new());
        let before = compiler.stats();
        let edited = format!("// unsaved editor comment\n\n{source}");
        compiler.set_source_text(root.clone(), edited.clone());
        let incremental = compiler.compile_cell(&config, &cell, Vec::new());
        assert_eq!(
            compiler.stats().execution_cache_hits,
            before.execution_cache_hits + 1
        );
        assert_eq!(
            compiler.stats().static_cache_hits,
            before.static_cache_hits + 1
        );
        assert_eq!(
            compiler.stats().static_cache_misses,
            before.static_cache_misses
        );

        let sources = IndexMap::from([(root.clone(), ArcStr::from(edited))]);
        let analysis = compile::analyze_workspace(parse::parse_workspace_with_config_and_sources(
            &config, &sources,
        ));
        let fresh = compile::execute_cell(
            analysis.typed_ast.as_ref().unwrap(),
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
    fn unrelated_declaration_edits_reuse_execution() {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let root = examples.join("immediate/lib.ar");
        let tech = examples.join("tech/basic.tech.toml");
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech));
        let cell = vec!["immediate".to_owned()];
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(
            root.clone(),
            "cell immediate() { let value = 1; }\ncell sibling() { let value = 2; }\n",
        );

        let _ = compiler.compile_cell(&config, &cell, Vec::new());
        let before = compiler.stats();
        compiler.set_source_text(
            root,
            "cell immediate() { let value = 1; }\ncell sibling() { let value = 99; }\n",
        );
        let _ = compiler.compile_cell(&config, &cell, Vec::new());

        assert_eq!(
            compiler.stats().execution_cache_hits,
            before.execution_cache_hits + 1
        );
    }

    #[test]
    fn observed_function_body_edits_invalidate_execution() {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let root = examples.join("immediate/lib.ar");
        let tech = examples.join("tech/basic.tech.toml");
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech));
        let cell = vec!["immediate".to_owned()];
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(
            root.clone(),
            "fn value() -> Int { 1 }\ncell immediate() { let value = value(); }\n",
        );

        let _ = compiler.compile_cell(&config, &cell, Vec::new());
        let before = compiler.stats();
        compiler.set_source_text(
            root,
            "fn value() -> Int { 2 }\ncell immediate() { let value = value(); }\n",
        );
        let _ = compiler.compile_cell(&config, &cell, Vec::new());

        assert_eq!(
            compiler.stats().execution_cache_misses,
            before.execution_cache_misses + 1
        );
        assert_eq!(
            compiler.stats().execution_cache_hits,
            before.execution_cache_hits
        );
    }

    #[test]
    fn typed_contract_edits_invalidate_execution_and_cell_artifacts() {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let root = examples.join("enumerations/lib.ar");
        let tech = examples.join("tech/basic.tech.toml");
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech));
        let cell = vec!["top".to_owned()];
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(
            root.clone(),
            "enum Choice { A, }\ncell top(choice: Choice) { let selected = choice; }\n",
        );
        let first = compiler.compile_cell(&config, &cell, vec![CellArg::Enum("A".to_owned())]);
        assert!(matches!(first, CompileOutput::Valid(_)));
        let before = compiler.stats();

        compiler.set_source_text(
            root,
            "enum Choice { B, }\ncell top(choice: Choice) { let selected = choice; }\n",
        );
        let second = compiler.compile_cell(&config, &cell, vec![CellArg::Enum("A".to_owned())]);
        assert!(matches!(second, CompileOutput::ExecErrors(_)));
        let after = compiler.stats();
        assert_eq!(
            after.execution_cache_misses,
            before.execution_cache_misses + 1
        );
        assert_eq!(
            after.cell_artifact_cache_hits,
            before.cell_artifact_cache_hits
        );
    }

    #[test]
    fn changed_parent_reuses_an_unchanged_child_cell_artifact() {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let root = examples.join("hierarchy/lib.ar");
        let tech = examples.join("tech/basic.tech.toml");
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech));
        let cell = vec!["top".to_owned()];
        let mut compiler = IncrementalCompiler::new();
        let child = r#"
cell leaf() {
    let shape = rect("met1", x0=0., y0=0., x1=100., y1=100.);
}

cell bot() {
    let leaf = inst(leaf());
}
"#;
        compiler.set_source_text(
            root.clone(),
            format!(
                "{child}\ncell top() {{\n    let marker = 1;\n    let child = bot();\n    let placed = inst(child);\n    let measured = placed.leaf.shape.x0;\n}}\n"
            ),
        );
        let first = compiler.compile_cell(&config, &cell, Vec::new());
        assert!(
            matches!(
                &first,
                CompileOutput::Valid(_) | CompileOutput::ExecErrors(_)
            ),
            "{first:?}"
        );
        let first_data = match &first {
            CompileOutput::Valid(data) => data,
            CompileOutput::ExecErrors(output) => output.output.as_ref().unwrap(),
            _ => unreachable!(),
        };
        let before = compiler.stats();

        compiler.set_source_text(
            root,
            format!(
                "{child}\ncell top() {{\n    let marker = 2;\n    let child = bot();\n    let placed = inst(child);\n    let measured = placed.leaf.shape.x0;\n}}\n"
            ),
        );
        let second = compiler.compile_cell(&config, &cell, Vec::new());
        assert!(matches!(
            &second,
            CompileOutput::Valid(_) | CompileOutput::ExecErrors(_)
        ));
        let second_data = match &second {
            CompileOutput::Valid(data) => data,
            CompileOutput::ExecErrors(output) => output.output.as_ref().unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(first_data.top, second_data.top);
        assert!(
            !Arc::ptr_eq(
                &first_data.cells[&first_data.top],
                &second_data.cells[&second_data.top]
            ),
            "the edited parent must be replaced"
        );
        let shared_children = first_data
            .cells
            .iter()
            .filter(|(id, cell)| {
                **id != first_data.top
                    && second_data
                        .cells
                        .get(*id)
                        .is_some_and(|next| Arc::ptr_eq(cell, next))
            })
            .count();
        assert!(
            shared_children >= 2,
            "leaf and bot should be structurally shared, found {shared_children} shared children"
        );
        let after = compiler.stats();

        assert_eq!(
            after.execution_cache_misses,
            before.execution_cache_misses + 1
        );
        assert!(
            after.cell_artifact_cache_hits > before.cell_artifact_cache_hits,
            "before={before:?}, after={after:?}"
        );
    }

    #[test]
    fn source_insertion_keeps_an_unchanged_gds_artifact_shared() {
        use ::gds::{GdsBoundary, GdsElement, GdsLibrary, GdsPoint, GdsStruct};

        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let gds_path = directory.path().join("macro.gds");
        let mut library = GdsLibrary::new("fixture");
        let mut structure = GdsStruct::new("layout_top");
        structure.elems.push(GdsElement::GdsBoundary(GdsBoundary {
            layer: 235,
            datatype: 4,
            xy: vec![
                GdsPoint::new(0, 0),
                GdsPoint::new(0, 20),
                GdsPoint::new(10, 20),
                GdsPoint::new(10, 0),
            ],
            ..Default::default()
        }));
        library.structs.push(structure);
        library.save(&gds_path).unwrap();
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = WorkspaceConfig::new(&root)
            .with_tech(Some(tech))
            .with_gds_imports([("macro".to_owned(), gds_path)]);
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(
            root.clone(),
            "cell top() {\n  let imported = inst(macro());\n}\n",
        );
        let first = compiler.compile_invocation(&config, "top()").unwrap();
        let before = compiler.stats();
        compiler.set_source_text(
            root,
            "cell top() {\n  let imported = inst(macro());\n  let added = rect(\"met1\", x0i=20., y0i=20., x1i=30., y1i=30.)!;\n}\n",
        );
        let second = compiler.compile_invocation(&config, "top()").unwrap();
        assert_eq!(
            compiler.stats().cell_continuation_cache_hits,
            before.cell_continuation_cache_hits + 1
        );
        fn data(output: &CompileOutput) -> &compile::CompiledData {
            match output {
                CompileOutput::Valid(data) => data,
                CompileOutput::ExecErrors(output) => output.output.as_ref().unwrap(),
                output => panic!("cell should compile: {output:?}"),
            }
        }
        let first = data(&first);
        let second = data(&second);
        assert_eq!(first.top, second.top);
        assert!(!Arc::ptr_eq(
            &first.cells[&first.top],
            &second.cells[&second.top]
        ));
        let imported = first.cells.keys().find(|cell| **cell != first.top).unwrap();
        assert!(Arc::ptr_eq(&first.cells[imported], &second.cells[imported]));
    }

    #[test]
    fn appended_literal_rectangle_resumes_the_solved_cell_prefix() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech));
        let initial =
            "cell top() {\n  let first = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.)!;\n}\n";
        let appended = "cell top() {\n  let first = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.)!;\n  let second = rect(\"met1\", x0i=20., y0i=20., x1i=30., y1i=30.)!;\n}\n";
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(root.clone(), initial);
        let first = compiler.compile_invocation(&config, "top()").unwrap();
        assert!(matches!(
            &first,
            CompileOutput::Valid(_) | CompileOutput::ExecErrors(_)
        ));
        let before = compiler.stats();

        compiler.set_source_text(root.clone(), appended);
        let incremental = compiler.compile_invocation(&config, "top()").unwrap();
        assert_eq!(
            compiler.stats().cell_continuation_cache_hits,
            before.cell_continuation_cache_hits + 1
        );

        let sources = IndexMap::from([(root.clone(), ArcStr::from(appended))]);
        let analysis = compile::analyze_workspace(parse::parse_workspace_with_config_and_sources(
            &config, &sources,
        ));
        assert!(analysis.errors.is_empty(), "{:?}", analysis.errors);
        let fresh = compile::execute_cell(
            analysis.typed_ast.as_ref().unwrap(),
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
            },
            &config,
        );
        fn data(output: &CompileOutput) -> &compile::CompiledData {
            match output {
                CompileOutput::Valid(data) => data,
                CompileOutput::ExecErrors(output) => output.output.as_ref().unwrap(),
                output => panic!("cell should compile: {output:?}"),
            }
        }
        fn rectangle<'a>(
            output: &'a CompileOutput,
            field: &str,
        ) -> &'a compile::Rect<(f64, crate::solver::LinearExpr)> {
            let data = data(output);
            match data.cells[&data.top].field(field) {
                Some(compile::Arrayed::Elem(compile::SolvedValue::Rect(rect))) => rect,
                value => panic!("expected rectangle field `{field}`, found {value:?}"),
            }
        }
        let old_first = rectangle(&first, "first");
        let incremental_first = rectangle(&incremental, "first");
        let incremental_second = rectangle(&incremental, "second");
        let fresh_first = rectangle(&fresh, "first");
        let fresh_second = rectangle(&fresh, "second");

        assert_eq!(old_first.id, incremental_first.id);
        assert_eq!(incremental_first.layer, fresh_first.layer);
        assert_eq!(incremental_first.x0.0, fresh_first.x0.0);
        assert_eq!(incremental_first.y0.0, fresh_first.y0.0);
        assert_eq!(incremental_first.x1.0, fresh_first.x1.0);
        assert_eq!(incremental_first.y1.0, fresh_first.y1.0);
        assert_eq!(incremental_second.layer, fresh_second.layer);
        assert_eq!(incremental_second.x0.0, fresh_second.x0.0);
        assert_eq!(incremental_second.y0.0, fresh_second.y0.0);
        assert_eq!(incremental_second.x1.0, fresh_second.x1.0);
        assert_eq!(incremental_second.y1.0, fresh_second.y1.0);

        let first_id = incremental_first.id;
        let second_id = incremental_second.id;
        let appended_again = "cell top() {\n  let first = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.)!;\n  let second = rect(\"met1\", x0i=20., y0i=20., x1i=30., y1i=30.)!;\n  let third = rect(\"met1\", x0i=40., y0i=40., x1i=50., y1i=50.)!;\n}\n";
        let before_second_append = compiler.stats();
        compiler.set_source_text(root, appended_again);
        let third = compiler.compile_invocation(&config, "top()").unwrap();
        assert_eq!(
            compiler.stats().cell_continuation_cache_hits,
            before_second_append.cell_continuation_cache_hits + 1
        );
        assert_eq!(rectangle(&third, "first").id, first_id);
        assert_eq!(rectangle(&third, "second").id, second_id);
        assert_eq!(rectangle(&third, "third").x0.0, 40.);
    }

    #[test]
    fn a_non_append_edit_does_not_resume_a_cell_checkpoint() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech));
        let target = vec!["top".to_owned()];
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(
            root.clone(),
            "cell top() {\n  let shape = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.)!;\n}\n",
        );
        let _ = compiler.compile_cell(&config, &target, Vec::new());
        let before = compiler.stats();

        compiler.set_source_text(
            root,
            "cell top() {\n  let marker = 1;\n  let shape = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.)!;\n}\n",
        );
        let _ = compiler.compile_cell(&config, &target, Vec::new());
        let after = compiler.stats();

        assert_eq!(
            after.cell_continuation_cache_hits,
            before.cell_continuation_cache_hits
        );
        assert_eq!(
            after.cell_continuation_cache_misses,
            before.cell_continuation_cache_misses + 1
        );
    }

    #[test]
    fn global_declaration_ids_do_not_depend_on_traversal_order() {
        fn ids(source: &str, root: &Path) -> (VarId, VarId) {
            let sources = IndexMap::from([(root.to_path_buf(), ArcStr::from(source))]);
            let analysis =
                compile::analyze_workspace(parse::parse_workspace_with_config_and_sources(
                    &WorkspaceConfig::new(root),
                    &sources,
                ));
            assert!(analysis.errors.is_empty(), "{:?}", analysis.errors);
            let root = &analysis.typed_ast.unwrap()[&ModPath::new()];
            let function = root
                .ast
                .decls
                .iter()
                .find_map(|declaration| match declaration {
                    Decl::Fn(declaration) if declaration.name.name == "kept" => {
                        Some(declaration.metadata.1)
                    }
                    _ => None,
                })
                .unwrap();
            let cell = root
                .ast
                .decls
                .iter()
                .find_map(|declaration| match declaration {
                    Decl::Cell(declaration) if declaration.name.name == "top" => {
                        Some(declaration.metadata.1)
                    }
                    _ => None,
                })
                .unwrap();
            (function, cell)
        }

        let root =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/immediate/lib.ar");
        let original = "fn kept() -> Int { 1 }\ncell top() { let value = kept(); }\n";
        let inserted =
            "fn added() -> Int { 0 }\nfn kept() -> Int { 1 }\ncell top() { let value = kept(); }\n";
        assert_eq!(ids(original, &root), ids(inserted, &root));
    }

    #[test]
    fn invocation_cache_survives_unrelated_source_edits() {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let root = examples.join("immediate/lib.ar");
        let tech = examples.join("tech/basic.tech.toml");
        let config = WorkspaceConfig::new(&root).with_tech(Some(tech));
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(
            root.clone(),
            "cell immediate() { let value = 1; }\ncell sibling() { let value = 2; }\n",
        );

        let _ = compiler.compile_invocation(&config, "immediate()").unwrap();
        let before = compiler.stats();
        compiler.set_source_text(
            root,
            "cell immediate() { let value = 1; }\ncell sibling() { let value = 3; }\n",
        );
        let _ = compiler.compile_invocation(&config, "immediate()").unwrap();

        assert_eq!(
            compiler.stats().execution_cache_hits,
            before.execution_cache_hits + 1
        );
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

        let append_source = format!(
            "{}  let added = rect(\"met1\", x0i=0., y0i=0., x1i=10., y1i=10.)!;\n}}\n",
            source.strip_suffix("}\n").unwrap()
        );
        compiler.set_source_text(root, append_source);
        let start = std::time::Instant::now();
        let _ = compiler.compile_cell(&config, &cell, Vec::new());
        let rectangle_append = start.elapsed();
        let retained = crate::bench_alloc::live().saturating_sub(retained_before);
        eprintln!(
            "incremental,cold_ns={},warm_ns={},isolated_edit_ns={},rectangle_append_ns={},parse_hits={},files_reparsed={},static_hits={},static_misses={},static_unit_hits={},static_unit_misses={},execution_hits={},execution_misses={},execution_evictions={},cell_artifact_hits={},cell_artifact_misses={},cell_artifact_evictions={},cell_continuation_hits={},cell_continuation_misses={},cell_continuation_evictions={},retained_bytes={retained}",
            cold.as_nanos(),
            warm.as_nanos(),
            edited.as_nanos(),
            rectangle_append.as_nanos(),
            compiler.stats().parse_cache_hits,
            compiler.stats().files_reparsed,
            compiler.stats().static_cache_hits,
            compiler.stats().static_cache_misses,
            compiler.stats().static_unit_hits,
            compiler.stats().static_unit_misses,
            compiler.stats().execution_cache_hits,
            compiler.stats().execution_cache_misses,
            compiler.stats().execution_cache_evictions,
            compiler.stats().cell_artifact_cache_hits,
            compiler.stats().cell_artifact_cache_misses,
            compiler.stats().cell_artifact_cache_evictions,
            compiler.stats().cell_continuation_cache_hits,
            compiler.stats().cell_continuation_cache_misses,
            compiler.stats().cell_continuation_cache_evictions,
        );
    }
}
