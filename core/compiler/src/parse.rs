use std::{
    fmt::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use arcstr::ArcStr;
use indexmap::IndexMap;
use lrlex::{DefaultLexerTypes, lrlex_mod};
use lrpar::{LexParseError, NonStreamingLexer, Span, lrpar_mod};

use crate::ast::{
    Ast, AstMetadata, CallExpr, Decl, ModPath, WorkspaceAst, annotated::AnnotatedAst,
};

lrlex_mod!("argon.l");
lrpar_mod!("argon.y");
lrlex_mod!("cell.l");
lrpar_mod!("cell.y");

pub struct ParseMetadata;
pub type ParseAst<'a> = Ast<&'a str, ParseMetadata>;
pub type AnnotatedParseAst = AnnotatedAst<ParseMetadata>;
pub type WorkspaceParseAst = WorkspaceAst<ParseMetadata>;

impl AstMetadata for ParseMetadata {
    type Ident = ();
    type EnumDecl = ();
    type StructDecl = ();
    type StructField = ();
    type CellDecl = ();
    type ConstantDecl = ();
    type LetBinding = ();
    type IfExpr = ();
    type BinOpExpr = ();
    type UnaryOpExpr = ();
    type ComparisonExpr = ();
    type FieldAccessExpr = ();
    type EnumValue = ();
    type CallExpr = ();
    type EmitExpr = ();
    type Args = ();
    type KwArgValue = ();
    type ArgDecl = ();
    type Scope = ();
    type Typ = ();
    type VarExpr = ();
    type FnDecl = ();
    type CastExpr = ();
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

type ParseResult = Result<AnnotatedParseAst, anyhow::Error>;
type LexParseErrors = Vec<LexParseError<u32, DefaultLexerTypes>>;
type ModSpans = Vec<(Span, ModPath)>;

pub struct ParseOutput {
    pub asts: IndexMap<ModPath, ParseResult>,
    pub errs: IndexMap<PathBuf, (LexParseErrors, ModSpans)>,
}

impl ParseOutput {
    pub fn unwrap_asts(self) -> WorkspaceParseAst {
        self.asts
            .into_iter()
            .map(|(k, v)| (k, v.unwrap()))
            .collect()
    }
    pub fn best_effort_ast(self) -> WorkspaceParseAst {
        self.asts
            .into_iter()
            .filter_map(|(k, v)| Some((k, v.ok()?)))
            .collect()
    }
}

pub fn parse_workspace_with_std(root_lib: impl AsRef<Path>) -> ParseOutput {
    let ParseOutput { mut asts, mut errs } = parse_workspace(root_lib);
    let std_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/std/lib.ar");
    let ParseOutput {
        asts: std_asts,
        errs: std_errs,
    } = parse_workspace(std_path);
    // TODO: fix std library overwriting user-defined std mods.
    asts.extend(std_asts.into_iter().map(|(mut k, v)| {
        k.insert(0, "std".to_string());
        (k, v)
    }));
    errs.extend(std_errs);
    ParseOutput { asts, errs }
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
                if let Ok(ast) = &ast {
                    for decl in &ast.ast.decls {
                        if let Decl::Mod(decl) = decl {
                            let mut path = path.clone();
                            path.push(decl.ident.name.to_string());
                            mod_spans.push((decl.span, path.clone()));
                            stack.push(path);
                        }
                    }
                }
                workspace_ast.insert(path, ast);
                workspace_errs.insert(file_path, (errs, mod_spans));
            }
            Err(e) => {
                workspace_ast.insert(path, Err(e));
            }
        }
    }

    ParseOutput {
        asts: workspace_ast,
        errs: workspace_errs,
    }
}

fn parse_inner(
    input: ArcStr,
    path: PathBuf,
    res: Option<Result<ParseAst<'_>, ()>>,
    lexer: &dyn NonStreamingLexer<DefaultLexerTypes>,
    errs: &[LexParseError<u32, DefaultLexerTypes>],
) -> Result<AnnotatedAst<ParseMetadata>, anyhow::Error> {
    if !errs.is_empty() {
        let mut err = String::new();
        for e in errs {
            write!(&mut err, "{}", e.pp(lexer, &argon_y::token_epp))
                .with_context(|| "failed to write to string buffer")?;
        }
        bail!("{err}")
    }
    match res {
        Some(Ok(ast)) => Ok(AnnotatedAst::new(input, &ast, path)),
        _ => bail!("Unable to evaluate expression."),
    }
}

pub fn parse(path: impl Into<PathBuf>) -> (ParseResult, LexParseErrors) {
    let path = path.into();
    match std::fs::read_to_string(&path) {
        Ok(input) => {
            let input = ArcStr::from(input);
            // Get the `LexerDef` for the `argon` language.
            let lexerdef = argon_l::lexerdef();
            // Now we create a lexer with the `lexer` method with which
            // we can lex an input.
            let lexer = lexerdef.lexer(&input);
            // Pass the lexer to the parser and lex and parse the input.
            let (res, errs) = argon_y::parse(&lexer);
            (parse_inner(input.clone(), path, res, &lexer, &errs), errs)
        }
        Err(e) => (Err(e.into()), Vec::new()),
    }
}

pub fn parse_cell(input: &str) -> Result<CallExpr<&'_ str, ParseMetadata>, anyhow::Error> {
    // Get the `LexerDef` for the `argon` language.
    let lexerdef = cell_l::lexerdef();
    // Now we create a lexer with the `lexer` method with which
    // we can lex an input.
    let lexer = lexerdef.lexer(input);
    // Pass the lexer to the parser and lex and parse the input.
    let (res, errs) = cell_y::parse(&lexer);
    if !errs.is_empty() {
        let mut err = String::new();
        for e in errs {
            write!(&mut err, "{}", e.pp(&lexer, &cell_y::token_epp))
                .with_context(|| "failed to write to string buffer")?;
        }
        bail!("{err}");
    }
    match res {
        Some(Ok(expr)) => Ok(expr),
        _ => bail!("Unable to evaluate expression."),
    }
}
