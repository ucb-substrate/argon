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
}
