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
    ast::{Decl, ModPath},
    compile::{
        self, CellArg, CompileInput, CompileOutput, StaticAnalysis, StaticErrorCompileOutput, VarId,
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
    pub parse_cache_hits: u64,
    pub files_reparsed: u64,
    pub execution_cache_hits: u64,
    pub execution_cache_misses: u64,
    pub execution_cache_evictions: u64,
}

#[derive(Clone)]
struct StaticCache {
    revision: u64,
    config: WorkspaceConfig,
    disk_revisions: Vec<TrackedFileRevision>,
    analysis: StaticAnalysis,
    semantic: Arc<SemanticSnapshot>,
}

const EXECUTION_CACHE_CAPACITY: usize = 32;

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
    config: WorkspaceConfig,
    tech_revision: Option<FileRevision>,
    gds_revisions: Vec<(PathBuf, Option<FileRevision>)>,
}

impl ExecutionRequest {
    fn new(config: &WorkspaceConfig, target: ExecutionTarget) -> Self {
        Self {
            target,
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

/// Stateful compiler used by long-lived analyzer processes.
#[derive(Default, Clone)]
pub struct IncrementalCompiler {
    revision: u64,
    sources: IndexMap<PathBuf, ArcStr>,
    parse_cache: parse::ParseCache,
    static_cache: Option<StaticCache>,
    execution_cache: VecDeque<ExecutionCacheEntry>,
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

    fn ensure_analysis(&mut self, config: &WorkspaceConfig) {
        if let Some(cache) = &self.static_cache
            && cache.revision == self.revision
            && cache.config == *config
            && cache.disk_revisions == self.tracked_file_revisions(config, &cache.analysis)
        {
            self.stats.static_cache_hits += 1;
            return;
        }

        let parse_hits = self.parse_cache.hits();
        let parse_misses = self.parse_cache.misses();
        let parse_output = parse::parse_workspace_with_config_sources_and_cache(
            config,
            &self.sources,
            &mut self.parse_cache,
        );
        self.stats.parse_cache_hits += self.parse_cache.hits() - parse_hits;
        self.stats.files_reparsed += self.parse_cache.misses() - parse_misses;
        let parse_errors = parse_output.static_errors();
        let ast = parse_output.ast();
        let reused = self
            .static_cache
            .as_ref()
            .filter(|cache| cache.config == *config && parse_errors.is_empty())
            .and_then(|cache| reuse_static_analysis(&cache.analysis, ast.clone()));
        let analysis = if let Some(analysis) = reused {
            self.stats.static_cache_hits += 1;
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
    }

    /// Analyzes and executes one cell, retaining results across revisions while
    /// all declarations observed by the execution remain unchanged.
    pub fn compile_cell(
        &mut self,
        config: &WorkspaceConfig,
        cell: &[String],
        args: Vec<CellArg>,
    ) -> CompileOutput {
        self.ensure_analysis(config);
        let snapshot = {
            let cache = self
                .static_cache
                .as_ref()
                .expect("analysis cache was populated");
            let analysis = &cache.analysis;
            if !analysis.errors.is_empty() {
                return CompileOutput::StaticErrors(StaticErrorCompileOutput {
                    errors: analysis.errors.clone(),
                });
            }
            if analysis.typed_ast.is_none() {
                return CompileOutput::FatalParseErrors;
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
            return output;
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
        let execution = compile::execute_cell_tracked(
            ast,
            CompileInput {
                cell: &cell_refs,
                args,
            },
            config,
        );
        let output = execution.output;
        self.store_execution(request, execution.dependencies, &snapshot, &output);
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
        let snapshot = {
            let cache = self
                .static_cache
                .as_ref()
                .expect("analysis cache was populated");
            let analysis = &cache.analysis;
            if !analysis.errors.is_empty() {
                return Ok(CompileOutput::StaticErrors(StaticErrorCompileOutput {
                    errors: analysis.errors.clone(),
                }));
            }
            Arc::clone(&cache.semantic)
        };
        let request = ExecutionRequest::new(config, ExecutionTarget::Invocation(source.to_owned()));
        if let Some(output) = self.cached_execution(&request, &snapshot) {
            self.stats.execution_cache_hits += 1;
            return Ok(output);
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
        let Some((typed_ast, static_output)) = compile::static_compile(&ast) else {
            return Ok(CompileOutput::FatalParseErrors);
        };
        let output = if static_output.errors.is_empty() {
            let invocation_analysis = StaticAnalysis {
                ast,
                typed_ast: Some(typed_ast),
                errors: Vec::new(),
            };
            let invocation_snapshot = SemanticSnapshot::new(&invocation_analysis);
            let execution = compile::execute_cell_invocation_tracked(
                invocation_analysis
                    .typed_ast
                    .as_ref()
                    .expect("invocation AST was populated"),
                &invocation,
                config,
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
        Ok(output)
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
        let Some(dependencies) = dependency_vars
            .into_iter()
            .map(|var| {
                let identity = snapshot.vars.get(&var)?.clone();
                let declaration = snapshot.declarations.get(&identity)?.clone();
                Some(CachedDependency {
                    identity,
                    snapshot: declaration,
                })
            })
            .collect::<Option<Vec<_>>>()
        else {
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

fn reuse_static_analysis(
    previous: &StaticAnalysis,
    ast: parse::WorkspaceParseAst,
) -> Option<StaticAnalysis> {
    if !workspace_semantically_equal(&previous.ast, &ast) {
        return None;
    }
    let (changed_paths, remaps) = workspace_origin_remaps(&previous.ast, &ast)?;
    let typed_ast = match previous.typed_ast.as_ref() {
        Some(previous_typed) => {
            let mut typed = IndexMap::new();
            for (module, current) in &ast {
                let previous_module = previous_typed.get(module)?;
                if !changed_paths.contains(&current.path) {
                    typed.insert(module.clone(), previous_module.clone());
                    continue;
                }

                let path_remaps = remaps.get(&current.path)?;
                let mut value = serde_json::to_value(&previous_module.ast).ok()?;
                if !remap_ast_spans(&mut value, path_remaps) {
                    return None;
                }
                let raw: crate::ast::Ast<arcstr::Substr, compile::VarIdTyMetadata> =
                    serde_json::from_value(value).ok()?;
                let mut annotated = crate::ast::annotated::AnnotatedAst::new(
                    current.text.clone(),
                    &raw,
                    current.path.clone(),
                );
                annotated.source_text = current.source_text.clone();
                annotated.generated_declarations = current.generated_declarations;
                typed.insert(module.clone(), annotated);
            }
            Some(typed)
        }
        None => None,
    };

    let errors = if previous.errors.is_empty() || changed_paths.is_empty() {
        previous.errors.clone()
    } else {
        let mut value = serde_json::to_value(&previous.errors).ok()?;
        remap_serialized_spans(&mut value, &changed_paths, &remaps).then_some(())?;
        serde_json::from_value(value).ok()?
    };

    Some(StaticAnalysis {
        ast,
        typed_ast,
        errors,
    })
}

fn workspace_semantically_equal(
    previous: &parse::WorkspaceParseAst,
    current: &parse::WorkspaceParseAst,
) -> bool {
    previous.len() == current.len()
        && previous.iter().all(|(module, previous)| {
            current.get(module).is_some_and(|current| {
                previous.path == current.path
                    && declaration_semantics(&previous.ast).0
                        == declaration_semantics(&current.ast).0
            })
        })
}

fn workspace_origin_remaps(
    previous: &parse::WorkspaceParseAst,
    current: &parse::WorkspaceParseAst,
) -> Option<(HashSet<PathBuf>, OriginRemaps)> {
    let mut changed_paths = HashSet::new();
    let mut remaps = OriginRemaps::new();
    for (module, previous) in previous {
        let current = current.get(module)?;
        if previous.source_text == current.source_text {
            continue;
        }
        let previous_origins = declaration_semantics(&previous.ast).1;
        let current_origins = declaration_semantics(&current.ast).1;
        if previous_origins.len() != current_origins.len() {
            return None;
        }
        changed_paths.insert(current.path.clone());
        insert_origin_pairs(
            remaps.entry(current.path.clone()).or_default(),
            &previous_origins,
            &current_origins,
        );
    }
    Some((changed_paths, remaps))
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
                    if snapshot.declarations.contains_key(&identity) {
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
        type Offset = (usize, usize);
        let mut changed_paths = HashSet::new();
        let mut remaps: HashMap<PathBuf, HashMap<Offset, Option<Offset>>> = HashMap::new();

        for dependency in &self.dependencies {
            let current = snapshot.declarations.get(&dependency.identity)?;
            if current.source_text == dependency.snapshot.source_text {
                continue;
            }
            if current.origins.len() != dependency.snapshot.origins.len() {
                return None;
            }
            let path = current.source_path.clone();
            changed_paths.insert(path.clone());
            let path_remaps = remaps.entry(path).or_default();
            for (old, new) in dependency
                .snapshot
                .origins
                .iter()
                .zip(current.origins.iter())
            {
                let old = (old.start(), old.end());
                let new = (new.start(), new.end());
                path_remaps
                    .entry(old)
                    .and_modify(|mapped| {
                        if *mapped != Some(new) {
                            *mapped = None;
                        }
                    })
                    .or_insert(Some(new));
            }
        }

        if changed_paths.is_empty() {
            return Some(self.output.clone());
        }

        let mut value = serde_json::to_value(&self.output).ok()?;
        remap_serialized_spans(&mut value, &changed_paths, &remaps).then_some(())?;
        serde_json::from_value(value).ok()
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
            "incremental,cold_ns={},warm_ns={},isolated_edit_ns={},parse_hits={},files_reparsed={},static_hits={},static_misses={},execution_hits={},execution_misses={},execution_evictions={},retained_bytes={retained}",
            cold.as_nanos(),
            warm.as_nanos(),
            edited.as_nanos(),
            compiler.stats().parse_cache_hits,
            compiler.stats().files_reparsed,
            compiler.stats().static_cache_hits,
            compiler.stats().static_cache_misses,
            compiler.stats().execution_cache_hits,
            compiler.stats().execution_cache_misses,
            compiler.stats().execution_cache_evictions,
        );
    }
}
