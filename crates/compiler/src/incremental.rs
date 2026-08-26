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
};

use arcstr::ArcStr;
use indexmap::IndexMap;

use crate::{
    compile::{
        self, CellArg, CompileInput, CompileOutput, StaticAnalysis, StaticErrorCompileOutput,
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
}

#[derive(Clone)]
struct StaticCache {
    key: u64,
    disk_revisions: Vec<TrackedFileRevision>,
    analysis: StaticAnalysis,
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
        let Some(ast) = analysis.typed_ast.as_ref() else {
            return CompileOutput::FatalParseErrors;
        };

        let mut hasher = DefaultHasher::new();
        let environment = execution_environment_key(config.lyp.as_deref(), &config.gds_imports);
        if self.execution_environment != Some(environment) {
            self.execution_cache.clear();
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
        let cell_refs = cell.iter().map(String::as_str).collect::<Vec<_>>();
        let output = compile::dynamic_compile_with_config(
            ast,
            CompileInput {
                cell: &cell_refs,
                args,
            },
            config,
        );
        self.execution_cache.insert(key, output.clone());
        output
    }

    fn invalidate(&mut self) {
        self.revision = self.revision.saturating_add(1);
        self.stats.revision = self.revision;
        self.static_cache = None;
        self.execution_cache.clear();
        self.execution_environment = None;
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

fn file_revision(path: &Path) -> Option<FileRevision> {
    let metadata = std::fs::metadata(path).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?;
    Some((modified.as_secs(), modified.subsec_nanos(), metadata.len()))
}

fn execution_environment_key(lyp_file: Option<&Path>, gds_imports: &[(String, PathBuf)]) -> u64 {
    let mut hasher = DefaultHasher::new();
    lyp_file.hash(&mut hasher);
    lyp_file.and_then(file_revision).hash(&mut hasher);
    for (name, path) in gds_imports {
        name.hash(&mut hasher);
        path.hash(&mut hasher);
        file_revision(path).hash(&mut hasher);
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
            CellArg::Seq(values) => {
                3_u8.hash(hasher);
                hash_cell_args(values, hasher);
            }
        }
    }
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
        let lyp = examples.join("lyp/basic.lyp");
        let config = WorkspaceConfig::new(&root).with_lyp(Some(lyp));
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
        let lyp = examples.join("lyp/basic.lyp");
        let source = "cell immediate() {\n  let x0 = 31;\n  let y0 = 2;\n}\n";
        let cell = vec!["immediate".to_owned()];
        let config = WorkspaceConfig::new(&root).with_lyp(Some(lyp));

        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(root.clone(), source);
        let incremental = compiler.compile_cell(&config, &cell, Vec::new());

        let sources = IndexMap::from([(root.clone(), ArcStr::from(source))]);
        let analysis = compile::analyze_workspace(parse::parse_workspace_with_config_and_sources(
            &config, &sources,
        ));
        assert!(analysis.errors.is_empty(), "{:?}", analysis.errors);
        let typed = analysis.typed_ast.unwrap();
        let fresh = compile::dynamic_compile_with_config(
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
        let lyp = examples.join("lyp/basic.lyp");
        let source = std::fs::read_to_string(&root).unwrap();
        let cell = vec!["immediate".to_owned()];
        let config = WorkspaceConfig::new(&root).with_lyp(Some(lyp));
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
