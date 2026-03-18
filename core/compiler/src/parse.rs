use std::{
    fmt::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, anyhow, bail};
use arcstr::{ArcStr, Substr};
use indexmap::IndexMap;

use crate::{
    antlr,
    ast::{
        Ast, AstMetadata, CallExpr, Decl, Expr, ModPath, Span, Statement, WorkspaceAst,
        annotated::AnnotatedAst,
    },
    compile::{StaticError, StaticErrorKind},
    config::parse_config,
};

pub struct ParseMetadata;
pub type AnnotatedParseAst = AnnotatedAst<ParseMetadata>;
pub type WorkspaceParseAst = WorkspaceAst<ParseMetadata>;

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
    if path.is_empty() {
        return Ok(PathBuf::from(root_lib));
    }
    let mut base_path = PathBuf::from(root_lib);
    base_path.pop();
    for m in &path[0..path.len() - 1] {
        base_path.push(m);
    }
    let mut direct_path = base_path.clone();
    direct_path.push(format!("{}.ar", path.last().unwrap()));
    base_path.push(path.last().unwrap());
    base_path.push("mod.ar");
    if direct_path.is_file() && base_path.is_file() {
        bail!("both mod paths exists for mod {}", path.last().unwrap());
    }
    if direct_path == root_lib {
        bail!("circular mods: {}", path.last().unwrap());
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
                                kind: StaticErrorKind::InvalidMod,
                            })
                        } else {
                            None
                        }
                    }))
            })
            .collect()
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

fn antlr_diagnostics(input: &str) -> ParseDiagnostics {
    antlr::parse_errors(input)
        .into_iter()
        .map(|err| ParseDiagnostic {
            span: err.span,
            kind: StaticErrorKind::ParseError(err.message),
        })
        .collect()
}

fn parse_result_from_antlr(
    input: ArcStr,
    path: PathBuf,
    diagnostics: &[ParseDiagnostic],
) -> ParseResult {
    if !diagnostics.is_empty() {
        let mut err = String::new();
        for diagnostic in diagnostics {
            if let Err(write_err) = writeln!(&mut err, "{}", diagnostic.kind)
                .with_context(|| "failed to write to string buffer")
            {
                return (make_backup_ast(input, path), Some(anyhow!("{write_err}")));
            }
        }
        return (
            make_backup_ast(input, path),
            Some(anyhow!(err.trim_end().to_string())),
        );
    }
    match antlr::parse_ast(input.clone(), path.clone()) {
        Ok(ast) => (ast, None),
        Err(errs) => {
            let diagnostics = errs
                .into_iter()
                .map(|err| ParseDiagnostic {
                    span: err.span,
                    kind: StaticErrorKind::ParseError(err.message),
                })
                .collect::<Vec<_>>();
            parse_result_from_antlr(input, path, &diagnostics)
        }
    }
}

pub fn parse_workspace_with_std(root_lib: impl AsRef<Path>) -> ParseOutput {
    let root_lib = root_lib.as_ref();
    let mut ast = IndexMap::new();
    let mut err = IndexMap::new();
    let root_dir = root_lib.parent().unwrap();
    if let Ok(config) = parse_config(root_dir.join("Argon.toml")) {
        for (name, mod_path) in config.mods {
            let ParseOutput { asts, errs } = parse_workspace(
                if mod_path.is_relative() {
                    root_dir.join(mod_path)
                } else {
                    mod_path
                }
                .join("lib.ar"),
            );
            ast.extend(asts.into_iter().map(|(mut k, v)| {
                k.insert(0, name.clone());
                (k, v)
            }));
            err.extend(errs);
        }
    }
    let ParseOutput { asts, errs } = parse_workspace(root_lib);
    ast.extend(asts);
    err.extend(errs);
    let std_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/std/lib.ar");
    let ParseOutput {
        asts: std_asts,
        errs: std_errs,
    } = parse_workspace(std_path);
    // TODO: fix std library overwriting user-defined std mods.
    ast.extend(std_asts.into_iter().map(|(mut k, v)| {
        k.insert(0, "std".to_string());
        (k, v)
    }));
    err.extend(std_errs);
    ParseOutput {
        asts: ast,
        errs: err,
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
        Ok(input) => {
            let input = ArcStr::from(input);
            let antlr_errs = antlr_diagnostics(&input);
            (
                parse_result_from_antlr(input.clone(), path, &antlr_errs),
                antlr_errs,
            )
        }
        Err(e) => (
            (make_backup_ast("".into(), path), Some(e.into())),
            Vec::new(),
        ),
    }
}

pub fn format_cell_input(input: &str) -> String {
    format!("cell __dummy__() {{ {input}; }}")
}

// Input should first be formatted with `format_cell_input`.
pub fn parse_cell(input: &str) -> Result<CallExpr<Substr, ParseMetadata>, anyhow::Error> {
    let errs = antlr_diagnostics(input);
    if !errs.is_empty() {
        let mut err = String::new();
        for diagnostic in errs {
            writeln!(&mut err, "{}", diagnostic.kind)
                .with_context(|| "failed to write to string buffer")?;
        }
        bail!("{}", err.trim_end());
    }
    match antlr::parse_ast(ArcStr::from(input), PathBuf::from("__cell__.ar")) {
        Ok(ast) => {
            if let Decl::Cell(c) = ast.ast.decls.into_iter().next().unwrap()
                && let Statement::Expr {
                    value: Expr::Call(call),
                    ..
                } = c.scope.stmts.into_iter().next().unwrap()
            {
                Ok(call)
            } else {
                bail!("Unable to evaluate expression.")
            }
        }
        Err(errs) => {
            let mut err = String::new();
            for err_item in errs {
                writeln!(&mut err, "{}", err_item.message)
                    .with_context(|| "failed to write to string buffer")?;
            }
            bail!("{}", err.trim_end());
        }
    }
}
