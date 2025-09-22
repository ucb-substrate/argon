use std::fmt::Write;

use anyhow::{Context, bail};
use lrlex::lrlex_mod;
use lrpar::lrpar_mod;

use crate::ast::{Ast, AstMetadata, CallExpr};

lrlex_mod!("argon.l");
lrpar_mod!("argon.y");
lrlex_mod!("cell.l");
lrpar_mod!("cell.y");

pub struct ParseMetadata;
pub type ParseAst<'a> = Ast<'a, ParseMetadata>;

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

pub fn parse(input: &str) -> Result<Ast<'_, ParseMetadata>, anyhow::Error> {
    // Get the `LexerDef` for the `argon` language.
    let lexerdef = argon_l::lexerdef();
    // Now we create a lexer with the `lexer` method with which
    // we can lex an input.
    let lexer = lexerdef.lexer(input);
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
        Some(Ok(decls)) => Ok(Ast { decls }),
        _ => bail!("Unable to evaluate expression."),
    }
}

pub fn parse_cell(input: &str) -> Result<CallExpr<'_, ParseMetadata>, anyhow::Error> {
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
