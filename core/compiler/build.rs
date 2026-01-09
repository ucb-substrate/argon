use std::env;
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

    let mut build = cc::Build::new();

        build
            .cpp(true)               // Use C++ compiler
            .std("c++14")            // Eigen requires C++14 or newer
            .file("src/eigen_qr.cpp") // Path to your C++ file
            .flag("-O3");


    if std::path::Path::new("/opt/homebrew/include/eigen3").exists() {
        build.include("/opt/homebrew/include/eigen3");
    } else {
        build.include("/usr/local/include/eigen3");
    }

    build.compile("eigen_qr");

    println!("cargo:rerun-if-changed=src/eigen_qr.cpp");
    println!("cargo:rerun-if-changed=build.rs");
}
