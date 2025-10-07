use std::{
    collections::HashMap,
    fmt::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};
use arcstr::ArcStr;
use lrlex::lrlex_mod;
use lrpar::lrpar_mod;

use crate::ast::{
    Ast, AstMetadata, CallExpr, Decl, ModPath, WorkspaceAst, annotated::AnnotatedAst,
};

lrlex_mod!("argon.l");
lrpar_mod!("argon.y");
lrlex_mod!("cell.l");
lrpar_mod!("cell.y");

pub struct ParseMetadata;
pub type ParseAst<'a> = Ast<&'a str, ParseMetadata>;
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

pub fn get_mod(root_dir: impl AsRef<Path>, path: &ModPath) -> Result<String, anyhow::Error> {
    let mut base_path = PathBuf::from(root_dir.as_ref());
    if path.is_empty() {
        base_path.push("lib.rs");
        return Ok(std::fs::read_to_string(&base_path)?);
    }
    for m in &path[0..path.len() - 1] {
        base_path.push(m);
    }
    let mut direct_path = base_path.clone();
    direct_path.push(format!("{}.ar", path.last().unwrap()));
    base_path.push(path.last().unwrap());
    base_path.push("mod.rs");
    if direct_path.exists() && base_path.exists() {
        bail!("both mod paths exists for mod {}", path.last().unwrap());
    }
    if let Ok(contents) = std::fs::read_to_string(&direct_path) {
        Ok(contents)
    } else {
        Ok(std::fs::read_to_string(&base_path)?)
    }
}

pub fn parse_workspace(root_dir: impl AsRef<Path>) -> Result<WorkspaceParseAst, anyhow::Error> {
    let root_dir = root_dir.as_ref();

    let mut stack = vec![vec![]];
    let mut workspace_ast = HashMap::new();

    while let Some(path) = stack.pop() {
        let contents = get_mod(root_dir, &path)?;
        let ast = parse(contents)?;
        for decl in &ast.ast.decls {
            if let Decl::Mod(decl) = decl {
                let mut path = path.clone();
                path.push(decl.ident.name.to_string());
                stack.push(path);
            }
        }
        workspace_ast.insert(path, ast);
    }

    // Add std library.
    let std_mod_path = vec!["std".to_string()];
    if workspace_ast.contains_key(&std_mod_path) {
        bail!("cannot name a module `std`");
    }
    let std_path = concat!(env!("CARGO_MANIFEST_DIR"), "/src/std.ar");
    let contents = std::fs::read_to_string(std_path)?;
    let ast = parse(contents)?;
    for decl in &ast.ast.decls {
        if let Decl::Mod(_) = decl {
            bail!("`std` library cannot have dependencies");
        }
    }
    workspace_ast.insert(std_mod_path, ast);

    Ok(workspace_ast)
}

pub fn parse(input: impl Into<ArcStr>) -> Result<AnnotatedAst<ParseMetadata>, anyhow::Error> {
    let input = input.into();
    // Get the `LexerDef` for the `argon` language.
    let lexerdef = argon_l::lexerdef();
    // Now we create a lexer with the `lexer` method with which
    // we can lex an input.
    let lexer = lexerdef.lexer(&input);
    // Pass the lexer to the parser and lex and parse the input.
    let (res, errs) = argon_y::parse(&lexer);
    if !errs.is_empty() {
        let mut err = String::new();
        for e in errs {
            write!(&mut err, "{}", e.pp(&lexer, &argon_y::token_epp))
                .with_context(|| "failed to write to string buffer")?;
        }
        bail!("{err}");
    }
    match res {
        Some(Ok(ast)) => Ok(AnnotatedAst::new(input.clone(), &ast)),
        _ => bail!("Unable to evaluate expression."),
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
