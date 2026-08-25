use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail};
use arcstr::{ArcStr, Substr};
use indexmap::IndexMap;

use crate::{
    ast::{Ast, AstMetadata, CallExpr, Decl, ModPath, Span, WorkspaceAst, annotated::AnnotatedAst},
    compile::{StaticError, StaticErrorKind},
    parser::ParseError,
};

pub struct ParseMetadata;
pub type AnnotatedParseAst = AnnotatedAst<ParseMetadata>;
pub type WorkspaceParseAst = WorkspaceAst<ParseMetadata>;

/// Virtual path used for diagnostics originating in the embedded standard library.
pub const STD_PATH: &str = "<argon-std>/lib.ar";
/// Source text embedded into the compiler for the Argon standard library.
pub const STD_SOURCE: &str = include_str!("std/lib.ar");

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
    type ComparisonExpr = ();
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
}

pub fn get_mod(root_lib: impl AsRef<Path>, path: &ModPath) -> Result<PathBuf, anyhow::Error> {
    let root_lib = root_lib.as_ref();
    let Some(last) = path.last() else {
        return Ok(PathBuf::from(root_lib));
    };
    let mut base_path = root_lib
        .parent()
        .unwrap_or_else(|| Path::new(""))
        .to_path_buf();
    for m in &path[0..path.len() - 1] {
        base_path.push(m);
    }
    let mut direct_path = base_path.clone();
    direct_path.push(format!("{last}.ar"));
    base_path.push(last);
    base_path.push("mod.ar");
    if direct_path.is_file() && base_path.is_file() {
        bail!("both module paths exist for module `{last}`");
    }
    if direct_path == root_lib {
        bail!("circular module `{last}`");
    }
    if direct_path.is_file() {
        Ok(direct_path)
    } else {
        Ok(base_path)
    }
}

type ParseResult = (AnnotatedParseAst, Option<anyhow::Error>);
type ParseDiagnostics = Vec<ParseDiagnostic>;
type ModSpans = Vec<(cfgrammar::Span, ModPath)>;

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
    AnnotatedParseAst::new(
        input,
        &Ast::<Substr, _> {
            decls: vec![],
            span: cfgrammar::Span::new(0, input_len),
        },
        path,
    )
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
    parse_workspace_with_std_and_deps(root_lib, std::iter::empty::<(String, PathBuf)>())
}

/// Parses a library, its explicitly supplied path dependencies, and the Argon
/// standard library. This function deliberately performs no manifest or
/// configuration discovery; callers such as `arc` are responsible for
/// resolving library configuration into concrete paths.
pub fn parse_workspace_with_std_and_deps(
    root_lib: impl AsRef<Path>,
    dependencies: impl IntoIterator<Item = (String, PathBuf)>,
) -> ParseOutput {
    parse_workspace_with_std_deps_and_gds(
        root_lib,
        dependencies,
        std::iter::empty::<(String, PathBuf)>(),
    )
}

/// Parses a workspace and adds zero-argument cell declarations backed by GDS
/// files. A name such as `macros::sram` creates `sram` in module `macros`, so
/// source can refer to it as `lib::macros::sram`.
pub fn parse_workspace_with_std_deps_and_gds(
    root_lib: impl AsRef<Path>,
    dependencies: impl IntoIterator<Item = (String, PathBuf)>,
    gds_imports: impl IntoIterator<Item = (String, PathBuf)>,
) -> ParseOutput {
    let root_lib = root_lib.as_ref();
    let mut output = ParseOutput::default();
    for (name, path) in dependencies {
        let dep_root = if path.is_dir() {
            path.join("lib.ar")
        } else {
            path
        };
        output.merge(parse_workspace(dep_root), Some(&name));
    }
    output.merge(parse_workspace(root_lib), None);
    add_gds_imports(&mut output, gds_imports);
    let std_path = PathBuf::from(STD_PATH);
    let (std_ast, std_diagnostics) = parse_source(ArcStr::from(STD_SOURCE), std_path.clone());
    // TODO: fix std library overwriting user-defined std mods.
    output.asts.insert(vec!["std".to_string()], std_ast);
    output.errs.insert(std_path, (std_diagnostics, Vec::new()));
    output
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

pub fn parse_workspace(root_lib: impl AsRef<Path>) -> ParseOutput {
    let root_lib = root_lib.as_ref();

    let mut stack = vec![vec![]];
    let mut workspace_ast = IndexMap::new();
    let mut workspace_errs = IndexMap::new();

    while let Some(path) = stack.pop() {
        match get_mod(root_lib, &path) {
            Ok(file_path) => {
                let (ast, errs) = parse(&file_path);
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
                workspace_ast.insert(
                    path,
                    (
                        // TODO: make better data structures so this dummy isn't necessary.
                        AnnotatedParseAst::new(
                            "".into(),
                            &Ast::<Substr, _> {
                                decls: vec![],
                                span: cfgrammar::Span::new(0, 0),
                            },
                            root_lib.into(),
                        ),
                        Some(e),
                    ),
                );
            }
        }
    }

    ParseOutput {
        asts: workspace_ast,
        errs: workspace_errs,
    }
}

fn parse(path: impl Into<PathBuf>) -> (ParseResult, ParseDiagnostics) {
    let path = path.into();
    match std::fs::read_to_string(&path) {
        Ok(input) => parse_source(ArcStr::from(input), path),
        Err(e) => (
            (make_backup_ast("".into(), path), Some(e.into())),
            Vec::new(),
        ),
    }
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
/// parser `parser::parse_ast` — e.g. to parse/annotate a snippet in the context
/// of a full program.
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
