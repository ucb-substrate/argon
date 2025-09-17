use std::path::PathBuf;

use cfgrammar::yacc::YaccKind;
use lrlex::CTLexerBuilder;

fn main() {
    CTLexerBuilder::new()
        .lrpar_config(|ctp| {
            ctp.yacckind(YaccKind::Grmtools)
                .visibility(lrpar::Visibility::Public)
                .grammar_in_src_dir("argon.y")
                .unwrap()
        })
        .visibility(lrlex::Visibility::Public)
        .lexer_in_src_dir("argon.l")
        .unwrap()
        .build()
        .unwrap();
    CTLexerBuilder::new()
        .lrpar_config(|ctp| {
            ctp.yacckind(YaccKind::Grmtools)
                .visibility(lrpar::Visibility::Public)
                .grammar_in_src_dir("cell.y")
                .unwrap()
        })
        .visibility(lrlex::Visibility::Public)
        .lexer_in_src_dir("argon.l")
        .unwrap()
        .output_path(PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("cell.l.rs"))
        .mod_name("cell_l")
        .build()
        .unwrap();
}
