//! Process-local compilation session for editor integrations.
//!
//! The session owns open-document snapshots and makes those snapshots the
//! source of truth while retaining the existing one-shot compiler API. Changed
//! files are reparsed as complete files, canonical syntax fingerprints retain
//! trivia-only static results, and dynamic results remain reusable while every
//! declaration observed by their execution is semantically unchanged.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::{Path, PathBuf},
    sync::Arc,
};

use arcstr::ArcStr;
use indexmap::IndexMap;
use serde::Serialize;

use crate::{
    ast::{Decl, ModPath, WorkspaceAst, annotated::AnnotatedAst},
    cancellation::CancellationToken,
    compile::{
        self, CellArg, CompileInput, CompileOutput, StaticAnalysis, StaticError,
        StaticErrorCompileOutput, VarId, VarIdTyFrame, VarIdTyMetadata,
    },
    parse,
    workspace::WorkspaceConfig,
};

type FileRevision = (u64, u32, u64);
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
}

#[derive(Clone)]
struct StaticCache {
    revision: u64,
    config: WorkspaceConfig,
    disk_revisions: Vec<TrackedFileRevision>,
    analysis: StaticAnalysis,
    semantic: Arc<SemanticSnapshot>,
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
    environment: ExecutionEnvironment,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ExecutionEnvironment {
    config: WorkspaceConfig,
    tech_revision: Option<FileRevision>,
    gds_revisions: Vec<(PathBuf, Option<FileRevision>)>,
}

impl ExecutionRequest {
    fn new(config: &WorkspaceConfig, target: ExecutionTarget) -> Self {
        Self {
            target,
            environment: ExecutionEnvironment::new(config),
        }
    }
}

impl ExecutionEnvironment {
    fn new(config: &WorkspaceConfig) -> Self {
        Self {
            config: config.clone(),
            tech_revision: config.tech.as_deref().and_then(file_revision),
            gds_revisions: config
                .gds_imports
                .iter()
                .map(|(_, path)| (path.clone(), file_revision(path)))
                .collect(),
        }
    }
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
    name: String,
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
    output: CompileOutput,
}

#[derive(Clone)]
struct CachedCellArtifact {
    environment: ExecutionEnvironment,
    dependencies: Vec<CachedDependency>,
    artifact: compile::CellArtifact,
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
        if let Some(cache) = &self.static_cache
            && cache.revision == self.revision
            && cache.config == *config
            && cache.disk_revisions == self.tracked_file_revisions(config, &cache.analysis)
        {
            self.stats.static_cache_hits += 1;
            return true;
        }

        if self
            .static_cache
            .as_ref()
            .is_some_and(|cache| cache.config != *config)
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
            config: config.clone(),
            disk_revisions,
            analysis,
            semantic,
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

        let request = ExecutionRequest::new(
            config,
            ExecutionTarget::Cell {
                path: cell.to_vec(),
                args: args.iter().map(CellArgKey::from).collect(),
            },
        );
        if let Some(output) = self.cached_execution(&request, &snapshot) {
            self.stats.execution_cache_hits += 1;
            return Some(output);
        }

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
        let artifacts = self.reusable_cell_artifacts(&request.environment, &snapshot);
        let execution = compile::execute_cell_tracked_with_artifacts_cancellable(
            ast,
            CompileInput {
                cell: &cell_refs,
                args,
            },
            config,
            artifacts,
            cancellation,
        )?;
        self.stats.cell_artifact_cache_hits += execution.artifact_hits;
        self.stats.cell_artifact_cache_misses += execution.artifact_misses;
        self.store_cell_artifacts(
            request.environment.clone(),
            execution.artifacts.clone(),
            &snapshot,
        );
        let output = execution.output;
        self.store_execution(request, execution.dependencies, &snapshot, &output);
        Some(output)
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
        let request = ExecutionRequest::new(config, ExecutionTarget::Invocation(source.to_owned()));
        if let Some(output) = self.cached_execution(&request, &snapshot) {
            self.stats.execution_cache_hits += 1;
            return Ok(Some(output));
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
            let artifacts =
                self.reusable_cell_artifacts(&request.environment, &invocation_snapshot);
            let Some(execution) =
                compile::execute_cell_invocation_tracked_with_artifacts_cancellable(
                    invocation_analysis
                        .typed_ast
                        .as_ref()
                        .expect("invocation AST was populated"),
                    &invocation,
                    config,
                    artifacts,
                    cancellation,
                )
            else {
                return Ok(None);
            };
            self.stats.cell_artifact_cache_hits += execution.artifact_hits;
            self.stats.cell_artifact_cache_misses += execution.artifact_misses;
            self.store_cell_artifacts(
                request.environment.clone(),
                execution.artifacts.clone(),
                &invocation_snapshot,
            );
            let output = execution.output;
            self.store_execution(
                request,
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
        snapshot: &SemanticSnapshot,
    ) -> Option<CompileOutput> {
        let index = self.execution_cache.iter().position(|entry| {
            entry.request == *request && entry.dependencies_are_current(snapshot)
        })?;
        let entry = self
            .execution_cache
            .remove(index)
            .expect("cache index came from the same deque");
        let output = entry.remapped_output(snapshot)?;
        self.execution_cache.push_back(entry);
        Some(output)
    }

    fn store_execution(
        &mut self,
        request: ExecutionRequest,
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
        self.execution_cache.retain(|entry| {
            entry.request != request || !entry.same_dependency_versions(&dependencies)
        });
        if self.execution_cache.len() >= EXECUTION_CACHE_CAPACITY {
            self.execution_cache.pop_front();
            self.stats.execution_cache_evictions += 1;
        }
        self.execution_cache.push_back(ExecutionCacheEntry {
            request,
            dependencies,
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
            .filter(|entry| entry.environment == *environment)
            .filter_map(|entry| entry.remapped_artifact(snapshot))
            .collect()
    }

    fn store_cell_artifacts(
        &mut self,
        environment: ExecutionEnvironment,
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
            self.cell_artifact_cache.retain(|entry| {
                entry.environment != environment
                    || !entry.artifact.same_key(&artifact)
                    || !entry.same_dependency_versions(&dependencies)
            });
            if self.cell_artifact_cache.len() >= CELL_ARTIFACT_CACHE_CAPACITY {
                self.cell_artifact_cache.pop_front();
                self.stats.cell_artifact_cache_evictions += 1;
            }
            self.cell_artifact_cache.push_back(CachedCellArtifact {
                environment: environment.clone(),
                dependencies,
                artifact,
            });
        }
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
                        name: declaration.name.name.to_string(),
                    },
                    Decl::Fn(declaration) => DeclarationIdentity {
                        module: module.clone(),
                        kind: DeclarationKind::Function,
                        name: declaration.name.name.to_string(),
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
                                name: declaration.name.name.to_string(),
                            },
                        ),
                        Decl::Fn(declaration) => (
                            declaration.metadata.1,
                            DeclarationIdentity {
                                module: module.clone(),
                                kind: DeclarationKind::Function,
                                name: declaration.name.name.to_string(),
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

    fn remapped_output(&self, snapshot: &SemanticSnapshot) -> Option<CompileOutput> {
        let (changed_paths, remaps) = dependency_origin_remaps(&self.dependencies, snapshot)?;

        if changed_paths.is_empty() {
            return Some(self.output.clone());
        }

        let mut value = serde_json::to_value(&self.output).ok()?;
        remap_serialized_spans(&mut value, &changed_paths, &remaps).then_some(())?;
        serde_json::from_value(value).ok()
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

fn file_revision(path: &Path) -> Option<FileRevision> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some((modified.as_secs(), modified.subsec_nanos(), metadata.len()))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let target = vec!["top".to_owned()];
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(
            root.clone(),
            "cell top() {\n  let imported = inst(macro());\n}\n",
        );
        let first = compiler.compile_cell(&config, &target, Vec::new());
        compiler.set_source_text(
            root,
            "cell top() {\n  let marker = 1;\n  let imported = inst(macro());\n}\n",
        );
        let second = compiler.compile_cell(&config, &target, Vec::new());
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
            "incremental,cold_ns={},warm_ns={},isolated_edit_ns={},parse_hits={},files_reparsed={},static_hits={},static_misses={},static_unit_hits={},static_unit_misses={},execution_hits={},execution_misses={},execution_evictions={},cell_artifact_hits={},cell_artifact_misses={},cell_artifact_evictions={},retained_bytes={retained}",
            cold.as_nanos(),
            warm.as_nanos(),
            edited.as_nanos(),
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
        );
    }
}
