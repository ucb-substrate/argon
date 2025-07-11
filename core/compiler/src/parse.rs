use std::fmt::Write;

use anyhow::{bail, Context};
use lrlex::lrlex_mod;
use lrpar::lrpar_mod;

lrlex_mod!("argon.l");
lrpar_mod!("argon.y");

pub use argon_y::*;

#[derive(Debug, Clone)]
pub struct ArgonAst<'a> {
    pub decls: Vec<Decl<'a>>,
}

pub fn parse(input: &str) -> Result<ArgonAst<'_>, anyhow::Error> {
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
        Some(Ok(decls)) => Ok(ArgonAst { decls }),
        _ => bail!("Unable to evaluate expression."),
    }
}
