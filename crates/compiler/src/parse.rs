use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use arcstr::{ArcStr, Substr};
use indexmap::IndexMap;

use crate::{
    ast::{Ast, AstMetadata, CallExpr, Decl, ModPath, Span, WorkspaceAst, annotated::AnnotatedAst},
    compile::{StaticError, StaticErrorKind},
    parser::ParseError,
    workspace::WorkspaceConfig,
};

pub struct ParseMetadata;
pub type AnnotatedParseAst = AnnotatedAst<ParseMetadata>;
pub type WorkspaceParseAst = WorkspaceAst<ParseMetadata>;

/// Virtual path used for diagnostics originating in the embedded standard library.
pub const STD_PATH: &str = "<argon-std>/lib.ar";
/// Source text embedded into the compiler for the Argon standard library.
pub const STD_SOURCE: &str = include_str!("std/lib.ar");
/// Virtual path used for diagnostics originating in a cell invocation supplied
/// by a compiler entry point rather than by a source file.
pub const CELL_PATH: &str = "<argon-cell>";

/// Source text for a compiler-internal path that has no file on disk.
///
/// The standard library is compiled into the binary, so anything that reads a
/// source file by path — diagnostic rendering, an editor jumping to a
/// definition — has to go through here rather than the filesystem.
pub fn virtual_source(path: &Path) -> Option<&'static str> {
    (path == Path::new(STD_PATH)).then_some(STD_SOURCE)
}

impl AstMetadata for ParseMetadata {
    type Ident = ();
    type IdentPath = ();
    type EnumDecl = ();
    type StructDecl = ();
    type StructField = ();
    type CellDecl = ();
    type ConstantDecl = ();
    type LetBinding = ();
    type ForLoop = ();
    type IfExpr = ();
    type MatchExpr = ();
    type BinOpExpr = ();
    type UnaryOpExpr = ();
    type FieldAccessExpr = ();
    type IndexFieldAccessExpr = ();
    type IndexExpr = ();
    type CallExpr = ();
    type EmitExpr = ();
    type Args = ();
    type KwArgValue = ();
    type ArgDecl = ();
    type Scope = ();
    type Typ = ();
    type FnDecl = ();
    type CastExpr = ();
    type TupleExpr = ();
    type StructLitExpr = ();
}

/// The two files a `mod <name>;` declaration can name.
pub struct ModCandidates {
    /// `<name>.ar` beside the declaring module.
    pub direct: PathBuf,
    /// `<name>/mod.ar` below the declaring module.
    pub nested: PathBuf,
}

/// Both files `mod <name>;` could resolve to, whether or not either exists.
///
/// [`get_mod`] picks between them by asking the filesystem, which means a
/// module's resolution — and with it the presence of a missing-module or a
/// duplicate-module error — depends on files that the resolved path alone does
/// not name. Callers that must notice when resolution *changes*, such as an
/// incremental session deciding whether a cached parse is still valid, have to
/// watch the whole pair rather than the winner.
pub fn mod_candidates(root_lib: impl AsRef<Path>, parents: &[String], name: &str) -> ModCandidates {
    let mut nested = root_lib
        .as_ref()
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_path_buf();
    nested.extend(parents);
    let mut direct = nested.clone();
    direct.push(format!("{name}.ar"));
    nested.push(name);
    nested.push("mod.ar");
    ModCandidates { direct, nested }
}

pub fn get_mod(root_lib: impl AsRef<Path>, path: &ModPath) -> Result<PathBuf, anyhow::Error> {
    let root_lib = root_lib.as_ref();
    let Some((last, parents)) = path.split_last() else {
        return Ok(PathBuf::from(root_lib));
    };
    let ModCandidates { direct, nested } = mod_candidates(root_lib, parents, last);
    if direct.is_file() && nested.is_file() {
        bail!("both module paths exist for module `{last}`");
    }
    if direct == root_lib {
        bail!("circular module `{last}`");
    }
    if direct.is_file() {
        Ok(direct)
    } else {
        Ok(nested)
    }
}

type ParseResult = (AnnotatedParseAst, Option<anyhow::Error>);
type ParseDiagnostics = Vec<ParseDiagnostic>;
type ModSpans = Vec<(cfgrammar::Span, ModPath)>;

/// Successful per-file parses retained by an incremental compiler session.
/// Syntax-error recovery is deliberately reparsed on the next request.
#[derive(Default, Clone)]
pub struct ParseCache {
    entries: IndexMap<PathBuf, (ArcStr, AnnotatedParseAst)>,
    hits: u64,
    misses: u64,
}

impl ParseCache {
    pub fn hits(&self) -> u64 {
        self.hits
    }

    pub fn misses(&self) -> u64 {
        self.misses
    }
}

#[derive(Debug, Clone)]
pub struct ParseDiagnostic {
    span: cfgrammar::Span,
    kind: StaticErrorKind,
}

#[derive(Default)]
pub struct ParseOutput {
    pub asts: IndexMap<ModPath, ParseResult>,
    pub errs: IndexMap<PathBuf, (ParseDiagnostics, ModSpans)>,
}

impl ParseOutput {
    pub fn ast(self) -> WorkspaceParseAst {
        self.asts.into_iter().map(|(k, v)| (k, v.0)).collect()
    }
    pub fn static_errors(&self) -> Vec<StaticError> {
        self.errs
            .iter()
            .flat_map(|(path, (lex_errs, mod_errs))| {
                lex_errs
                    .iter()
                    .map(|err| StaticError {
                        span: Span {
                            path: path.clone(),
                            span: err.span,
                        },
                        kind: err.kind.clone(),
                    })
                    .chain(mod_errs.iter().filter_map(|(span, mod_path)| {
                        if self.asts.get(mod_path)?.1.is_some() {
                            Some(StaticError {
                                span: Span {
                                    path: path.clone(),
                                    span: *span,
                                },
                                kind: StaticErrorKind::InvalidMod {
                                    module: mod_path.join("::"),
                                },
                            })
                        } else {
                            None
                        }
                    }))
            })
            .chain(self.asts.values().filter_map(|(ast, error)| {
                let has_parse_diagnostics = self
                    .errs
                    .get(&ast.path)
                    .is_some_and(|(diagnostics, _)| !diagnostics.is_empty());
                (!has_parse_diagnostics)
                    .then_some(error.as_ref())
                    .flatten()
                    .map(|error| StaticError {
                        span: Span {
                            path: ast.path.clone(),
                            span: cfgrammar::Span::new(0, 0),
                        },
                        kind: StaticErrorKind::SourceError(error.to_string()),
                    })
            }))
            .collect()
    }

    fn merge(&mut self, output: Self, prefix: Option<&str>) {
        for (mut path, result) in output.asts {
            if let Some(prefix) = prefix {
                path.insert(0, prefix.to_owned());
            }
            self.asts.insert(path, result);
        }
        self.errs.extend(output.errs);
    }
}

fn make_backup_ast(input: ArcStr, path: PathBuf) -> AnnotatedParseAst {
    let input_len = input.len();
    let mut ast = AnnotatedParseAst::new(
        input,
        &Ast::<Substr, _> {
            decls: vec![],
            span: cfgrammar::Span::new(0, input_len),
        },
        path,
    );
    ast.parsed = false;
    ast
}

fn diagnostics_from_errors(errs: Vec<ParseError>) -> ParseDiagnostics {
    errs.into_iter()
        .map(|err| ParseDiagnostic {
            span: err.span,
            kind: StaticErrorKind::ParseError(err.message),
        })
        .collect()
}

fn diagnostics_message(diagnostics: &[ParseDiagnostic]) -> String {
    diagnostics
        .iter()
        .map(|diagnostic| diagnostic.kind.to_string())
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_result_from_errors(
    input: ArcStr,
    path: PathBuf,
    diagnostics: Vec<ParseDiagnostic>,
) -> (ParseResult, ParseDiagnostics) {
    let error = (!diagnostics.is_empty()).then(|| anyhow!(diagnostics_message(&diagnostics)));
    ((make_backup_ast(input, path), error), diagnostics)
}

pub fn parse_workspace_with_std(root_lib: impl AsRef<Path>) -> ParseOutput {
    parse_workspace_with_config(&WorkspaceConfig::new(root_lib.as_ref()))
}

/// Parses a library, its explicitly supplied path dependencies, and the Argon
/// standard library. This function deliberately performs no manifest or
/// configuration discovery; callers such as `arc` are responsible for
/// resolving library configuration into concrete paths.
pub fn parse_workspace_with_std_and_deps(
    root_lib: impl AsRef<Path>,
    dependencies: impl IntoIterator<Item = (String, PathBuf)>,
) -> ParseOutput {
    parse_workspace_with_config(
        &WorkspaceConfig::new(root_lib.as_ref()).with_dependencies(dependencies),
    )
}

/// Parses a resolved workspace and its embedded standard library.
pub fn parse_workspace_with_config(config: &WorkspaceConfig) -> ParseOutput {
    parse_workspace_with_config_and_sources(config, &IndexMap::new())
}

/// Parses a workspace and adds zero-argument cell declarations backed by GDS
/// files. A name such as `macros::sram` creates `sram` in module `macros`, so
/// source can refer to it as `lib::macros::sram`.
pub fn parse_workspace_with_std_deps_and_gds(
    root_lib: impl AsRef<Path>,
    dependencies: impl IntoIterator<Item = (String, PathBuf)>,
    gds_imports: impl IntoIterator<Item = (String, PathBuf)>,
) -> ParseOutput {
    parse_workspace_with_config(
        &WorkspaceConfig::new(root_lib.as_ref())
            .with_dependencies(dependencies)
            .with_gds_imports(gds_imports),
    )
}

/// Parses a resolved workspace using open editor documents as source overlays.
pub fn parse_workspace_with_config_and_sources(
    config: &WorkspaceConfig,
    sources: &IndexMap<PathBuf, ArcStr>,
) -> ParseOutput {
    parse_workspace_with_config_sources_and_cache(config, sources, &mut ParseCache::default())
}

/// Parses a workspace while taking the contents of open editor documents from
/// `sources`. Files absent from the map are read from disk. Paths in the map
/// must be the same absolute (or consistently relative) paths used by the
/// workspace and dependency roots.
pub fn parse_workspace_with_std_deps_gds_and_sources(
    root_lib: impl AsRef<Path>,
    dependencies: impl IntoIterator<Item = (String, PathBuf)>,
    gds_imports: impl IntoIterator<Item = (String, PathBuf)>,
    sources: &IndexMap<PathBuf, ArcStr>,
) -> ParseOutput {
    parse_workspace_with_config_and_sources(
        &WorkspaceConfig::new(root_lib.as_ref())
            .with_dependencies(dependencies)
            .with_gds_imports(gds_imports),
        sources,
    )
}

/// Overlay-aware workspace parsing with a reusable successful-file cache.
pub fn parse_workspace_with_config_sources_and_cache(
    config: &WorkspaceConfig,
    sources: &IndexMap<PathBuf, ArcStr>,
    cache: &mut ParseCache,
) -> ParseOutput {
    let mut output = ParseOutput::default();
    for (name, path) in &config.dependencies {
        let dep_root = if path.is_dir() {
            path.join("lib.ar")
        } else {
            path.clone()
        };
        output.merge(
            parse_workspace_with_sources(dep_root, sources, cache),
            Some(name),
        );
    }
    output.merge(
        parse_workspace_with_sources(config.root_lib(), sources, cache),
        None,
    );
    add_gds_imports(&mut output, config.gds_imports.iter().cloned());
    let std_path = PathBuf::from(STD_PATH);
    let (std_ast, std_diagnostics) = parse_source(ArcStr::from(STD_SOURCE), std_path.clone());
    // TODO: fix std library overwriting user-defined std mods.
    output.asts.insert(vec!["std".to_string()], std_ast);
    output.errs.insert(std_path, (std_diagnostics, Vec::new()));
    output
}

/// Overlay-aware workspace parsing with a reusable successful-file cache.
pub fn parse_workspace_with_std_deps_gds_sources_and_cache(
    root_lib: impl AsRef<Path>,
    dependencies: impl IntoIterator<Item = (String, PathBuf)>,
    gds_imports: impl IntoIterator<Item = (String, PathBuf)>,
    sources: &IndexMap<PathBuf, ArcStr>,
    cache: &mut ParseCache,
) -> ParseOutput {
    parse_workspace_with_config_sources_and_cache(
        &WorkspaceConfig::new(root_lib.as_ref())
            .with_dependencies(dependencies)
            .with_gds_imports(gds_imports),
        sources,
        cache,
    )
}

fn add_gds_imports(output: &mut ParseOutput, imports: impl IntoIterator<Item = (String, PathBuf)>) {
    let mut modules: IndexMap<ModPath, (String, PathBuf, usize)> = IndexMap::new();
    for (name, path) in imports {
        let mut components = name.split("::").map(str::to_owned).collect::<Vec<_>>();
        let Some(cell_name) = components.pop() else {
            continue;
        };
        let (source, _, count) = modules
            .entry(components)
            .or_insert_with(|| (String::new(), path, 0));
        source.push_str(&format!("cell {cell_name}() {{}}\n"));
        *count += 1;
    }
    for (module, (imports, import_path, import_count)) in modules {
        if let Some((existing, _)) = output.asts.get(&module) {
            let source_path = existing.path.clone();
            let source = ArcStr::from(format!("{}\n{imports}", existing.text));
            let (mut result, diagnostics) = parse_source(source, source_path.clone());
            if result.1.is_none() {
                result.0.promote_last_declarations(import_count);
            }
            result.0.source_text = existing.source_text.clone();
            output.asts.insert(module, result);
            let mod_spans = output
                .errs
                .get(&source_path)
                .map(|(_, spans)| spans.clone())
                .unwrap_or_default();
            output.errs.insert(source_path, (diagnostics, mod_spans));
        } else {
            let (mut result, diagnostics) =
                parse_source(ArcStr::from(imports), import_path.clone());
            if result.1.is_none() {
                result.0.promote_last_declarations(import_count);
            }
            result.0.source_text = ArcStr::from("");
            output.asts.insert(module, result);
            output.errs.insert(import_path, (diagnostics, Vec::new()));
        }
    }
}

/// A cell invocation spliced into the root module as a generated entry cell, so
/// that name resolution, type checking, and evaluation all treat it as ordinary
/// source. Returned by [`add_cell_invocation`].
pub struct CellInvocation {
    /// Name of the generated entry cell.
    pub entry_cell: String,
    /// Name of the generated binding holding the invocation's value.
    pub binding: String,
    /// The invocation as supplied by the caller, for diagnostics.
    pub source: String,
    /// Offsets within `source` bounding the call expression that was spliced.
    base: usize,
    call_end: usize,
    /// Offset within the root module's backing text of the spliced call.
    call_offset: usize,
    /// Offset within the root module's backing text of the generated cell.
    generated_offset: usize,
    /// Root module path, which the two offsets above index.
    path: PathBuf,
}

impl CellInvocation {
    /// Span of the spliced call within the root module. [`Self::remap`]
    /// translates it back onto the invocation.
    pub fn span(&self) -> Span {
        Span {
            path: self.path.clone(),
            span: cfgrammar::Span::new(
                self.call_offset,
                self.call_offset + (self.call_end - self.base),
            ),
        }
    }

    /// Translates a span inside the generated entry cell into a span over the
    /// invocation itself, so diagnostics point at what the caller wrote.
    /// Returns `None` for spans that are not in the generated region.
    pub fn remap(&self, span: &Span) -> Option<Span> {
        if span.path != self.path || span.span.start() < self.generated_offset {
            return None;
        }
        // Positions in the generated wrapper collapse onto the start of the
        // call, which is the closest thing the caller actually wrote.
        let onto_source = |position: usize| {
            (position.saturating_sub(self.call_offset) + self.base).min(self.source.len())
        };
        let start = onto_source(span.span.start());
        let end = onto_source(span.span.end()).max(start);
        Some(Span {
            path: PathBuf::from(CELL_PATH),
            span: cfgrammar::Span::new(start, end),
        })
    }
}

/// Splices a cell invocation such as `top(10., 20.)` into the root module of a
/// freshly parsed workspace, and refreshes that module's parse diagnostics.
///
/// See [`splice_cell_invocation`] for what the generated declaration looks like.
pub fn add_cell_invocation(
    output: &mut ParseOutput,
    invocation: &str,
) -> Result<CellInvocation, anyhow::Error> {
    let root = ModPath::new();
    let Some((existing, _)) = output.asts.get(&root) else {
        bail!("workspace has no root module");
    };
    let entry = EntryCell::new(
        output.asts.values().map(|(ast, _)| &ast.text),
        existing,
        invocation,
    )?;
    let source_path = entry.path.clone();
    let (result, diagnostics, invocation) = entry.parse();
    let mod_spans = output
        .errs
        .get(&source_path)
        .map(|(_, spans)| spans.clone())
        .unwrap_or_default();
    output.errs.insert(source_path, (diagnostics, mod_spans));
    output.asts.insert(root, result);
    Ok(invocation)
}

/// Splices a cell invocation into the root module of `asts` as a generated entry
/// cell:
///
/// ```argon
/// cell __argon_entry_0__() { let __argon_top_0__ = top(10., 20.); }
/// ```
///
/// Compiling that cell evaluates the invocation with the same passes that run
/// source files, so its arguments may be arbitrary expressions. The invocation
/// is validated with [`parse_cell`] first, which keeps its "exactly one call
/// expression" contract and its precise syntax errors.
pub fn splice_cell_invocation(
    asts: &mut WorkspaceParseAst,
    invocation: &str,
) -> Result<CellInvocation, anyhow::Error> {
    let root = ModPath::new();
    let Some(existing) = asts.get(&root) else {
        bail!("workspace has no root module");
    };
    let entry = EntryCell::new(asts.values().map(|ast| &ast.text), existing, invocation)?;
    let (result, _, invocation) = entry.parse();
    asts.insert(root, result.0);
    Ok(invocation)
}

/// A root module rewritten to declare the entry cell for an invocation.
struct EntryCell {
    /// The module's text with the generated declaration appended.
    text: ArcStr,
    path: PathBuf,
    /// Editor-visible text, which re-parsing would otherwise overwrite.
    source_text: ArcStr,
    generated_declarations: usize,
    invocation: CellInvocation,
}

impl EntryCell {
    fn new<'a>(
        texts: impl Iterator<Item = &'a ArcStr>,
        existing: &AnnotatedParseAst,
        invocation: &str,
    ) -> Result<Self, anyhow::Error> {
        let call = parse_cell(invocation)?;
        // Splice the call expression itself rather than the raw argument:
        // trailing trivia such as a line comment would otherwise swallow the
        // generated `;`.
        let base = call.span.start();
        let texts = texts.collect::<Vec<_>>();
        let unused = |name: &str| texts.iter().all(|text| !text.contains(name));
        let entry_cell = generated_name("__argon_entry_", unused);
        let binding = generated_name("__argon_top_", unused);
        let prefix = format!("cell {entry_cell}() {{ let {binding} = ");
        let generated = format!("{prefix}{}; }}\n", &invocation[base..call.span.end()]);
        let generated_offset = existing.text.len() + 1;
        Ok(Self {
            text: ArcStr::from(format!("{}\n{generated}", existing.text)),
            path: existing.path.clone(),
            source_text: existing.source_text.clone(),
            generated_declarations: existing.generated_declarations,
            invocation: CellInvocation {
                entry_cell,
                binding,
                source: invocation.to_owned(),
                base,
                call_end: call.span.end(),
                call_offset: generated_offset + prefix.len(),
                generated_offset,
                path: existing.path.clone(),
            },
        })
    }

    /// Re-parses the rewritten module, restoring the declaration order and
    /// editor-visible text that a fresh parse does not carry over.
    fn parse(self) -> (ParseResult, ParseDiagnostics, CellInvocation) {
        let (mut result, diagnostics) = parse_source(self.text, self.path);
        if result.1.is_none() {
            // Re-parsing restarts declaration order, so restore the generated
            // declarations that `add_gds_imports` promoted to the front while
            // keeping the entry cell last.
            let entry = result.0.ast.decls.pop().expect("entry cell should parse");
            result
                .0
                .promote_last_declarations(self.generated_declarations);
            result.0.ast.decls.push(entry);
        }
        result.0.source_text = self.source_text;
        (result, diagnostics, self.invocation)
    }
}

/// Builds a name with `prefix` that no module in the workspace uses.
fn generated_name(prefix: &str, unused: impl Fn(&str) -> bool) -> String {
    (0..)
        .map(|index| format!("{prefix}{index}"))
        .find(|name| unused(name))
        .expect("an unused generated name always exists")
}

pub fn parse_workspace(root_lib: impl AsRef<Path>) -> ParseOutput {
    parse_workspace_with_sources(root_lib, &IndexMap::new(), &mut ParseCache::default())
}

fn parse_workspace_with_sources(
    root_lib: impl AsRef<Path>,
    sources: &IndexMap<PathBuf, ArcStr>,
    cache: &mut ParseCache,
) -> ParseOutput {
    let root_lib = root_lib.as_ref();

    let mut stack = vec![vec![]];
    let mut workspace_ast = IndexMap::new();
    let mut workspace_errs = IndexMap::new();

    while let Some(path) = stack.pop() {
        match get_mod(root_lib, &path) {
            Ok(file_path) => {
                let (ast, errs) = parse_cached(&file_path, sources, cache);
                let mut mod_spans = Vec::new();
                for decl in &ast.0.ast.decls {
                    if let Decl::Mod(decl) = decl {
                        let mut path = path.clone();
                        path.push(decl.ident.name.to_string());
                        mod_spans.push((decl.span, path.clone()));
                        stack.push(path);
                    }
                }
                workspace_ast.insert(path, ast);
                workspace_errs.insert(file_path, (errs, mod_spans));
            }
            Err(e) => {
                // TODO: make better data structures so this dummy isn't necessary.
                //
                // A module whose file could not even be located did not come
                // from a successful parse, and it borrows the root's path for
                // want of one of its own. Marking it unparsed keeps tooling
                // that indexes by path from mistaking this empty stand-in for
                // the root module's own source.
                workspace_ast.insert(path, (make_backup_ast("".into(), root_lib.into()), Some(e)));
            }
        }
    }

    ParseOutput {
        asts: workspace_ast,
        errs: workspace_errs,
    }
}

fn parse_cached(
    path: &Path,
    sources: &IndexMap<PathBuf, ArcStr>,
    cache: &mut ParseCache,
) -> (ParseResult, ParseDiagnostics) {
    let source = match sources.get(path).cloned() {
        Some(source) => Ok(source),
        None => std::fs::read_to_string(path).map(ArcStr::from),
    };
    let source = match source {
        Ok(source) => source,
        Err(error) => {
            cache.misses += 1;
            return (
                (
                    make_backup_ast("".into(), path.to_path_buf()),
                    Some(error.into()),
                ),
                Vec::new(),
            );
        }
    };
    if let Some((cached_source, ast)) = cache.entries.get(path)
        && cached_source == &source
    {
        cache.hits += 1;
        return ((ast.clone(), None), Vec::new());
    }

    cache.misses += 1;
    let result = parse_source(source.clone(), path.to_path_buf());
    if result.0.1.is_none() {
        cache
            .entries
            .insert(path.to_path_buf(), (source, result.0.0.clone()));
    } else {
        cache.entries.shift_remove(path);
    }
    result
}

fn parse_source(input: ArcStr, path: PathBuf) -> (ParseResult, ParseDiagnostics) {
    match crate::parser::parse_ast(input.clone(), path.clone()) {
        Ok(ast) => ((ast, None), Vec::new()),
        Err(errs) => parse_result_from_errors(input, path, diagnostics_from_errors(errs)),
    }
}

/// Parse one source file from memory.
///
/// This is used by editor tooling that needs to type-check a temporary source
/// transformation without writing it to disk. The returned AST owns the source
/// text through its annotated substrings, just like a workspace parsed from
/// disk.
pub fn parse_source_text(
    input: impl Into<ArcStr>,
    path: PathBuf,
) -> Result<AnnotatedParseAst, anyhow::Error> {
    let input = input.into();
    crate::parser::parse_ast(input, path)
        .map_err(|errors| anyhow!(diagnostics_message(&diagnostics_from_errors(errors))))
}

/// Wrap a cell-body snippet (a single statement, written without its trailing
/// `;`) into a complete program by placing it in a throwaway cell:
/// `cell __dummy__() { <input>; }`. The result is intended for the whole-file
/// parser — e.g. to parse/annotate a snippet in the context of a full program.
///
/// This is **not** a preprocessing step for `parse_cell`: that function parses a
/// bare invocation directly and would reject the `cell { ... }` wrapper.
pub fn format_cell_input(input: &str) -> String {
    format!("cell __dummy__() {{ {input}; }}")
}

/// Parse a single bare cell invocation — a `callExpr` such as `top(1., 5)` — and
/// return it. The input is the invocation itself; do **not** wrap it with
/// `format_cell_input` first. Used by the analyzer to read the target
/// cell's name and literal arguments.
pub fn parse_cell(input: &str) -> Result<CallExpr<&str, ParseMetadata>, anyhow::Error> {
    match crate::parser::parse_cell(input) {
        Ok(ast) => Ok(ast),
        Err(errs) => {
            let diagnostics = diagnostics_from_errors(errs);
            bail!(diagnostics_message(&diagnostics));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::parse::{parse_cell, parse_workspace_with_std_and_deps};

    #[test]
    fn cell_invocation_parses() {
        parse_cell("test(1., 5)").expect("failed to parse cell");
    }

    #[test]
    fn dependency_modules_are_namespaced() {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let output = parse_workspace_with_std_and_deps(
            examples.join("path_dependencies/root_library/lib.ar"),
            [(
                "dependency".to_owned(),
                examples.join("path_dependencies/dependency_library"),
            )],
        );

        assert!(output.asts.contains_key(&Vec::new()));
        assert!(output.asts.contains_key(&vec!["dependency".to_owned()]));
        assert!(output.asts.contains_key(&vec!["std".to_owned()]));
    }
}
