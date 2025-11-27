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

    println!("cargo:rerun-if-changed=wrapper.h");
    println!("cargo:rerun-if-changed=build.rs");

    // // Link libraries
    println!("cargo:rustc-link-search=native=/opt/homebrew/lib");
    println!("cargo:rustc-link-lib=spqr");
    println!("cargo:rustc-link-lib=cholmod");
    println!("cargo:rustc-link-lib=suitesparseconfig");
    println!("cargo:rustc-link-lib=amd");
    println!("cargo:rustc-link-lib=colamd");
    println!("cargo:rustc-link-lib=ccolamd");
    println!("cargo:rustc-link-lib=camd");

    // Generate bindings
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg("-I/opt/homebrew/include")
        .wrap_unsafe_ops(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .allowlist_type("SuiteSparseQR_C_factorization")
        .allowlist_type("cholmod_common")
        .allowlist_type("cholmod_sparse")
        .allowlist_type("cholmod_dense")
        .allowlist_function("SuiteSparseQR_C_QR")
        .allowlist_function("SuiteSparseQR_C_factorize")
        .allowlist_function("SuiteSparseQR_C_free")
        .allowlist_function("SuiteSparseQR_C_qmult")
        .allowlist_function("SuiteSparseQR_C_solve")
        .allowlist_function("SuiteSparseQR_C_backslash_default")
        .allowlist_function("SuiteSparseQR_C_rank")
        .allowlist_function("SuiteSparseQR_C_R")
        .allowlist_function("SuiteSparseQR_C_E")
        .allowlist_function("cholmod_l_start")
        .allowlist_function("cholmod_l_finish")
        .allowlist_function("cholmod_l_free")
        .allowlist_function("cholmod_l_free_sparse")
        .allowlist_function("cholmod_l_free_dense")
        .allowlist_function("cholmod_l_allocate_sparse")
        .allowlist_function("cholmod_l_allocate_dense")
        .allowlist_function("cholmod_l_sparse_to_dense")
        .allowlist_function("cholmod_l_allocate_triplet")
        .allowlist_function("cholmod_l_free_triplet")
        .allowlist_function("cholmod_l_triplet_to_sparse")
        .allowlist_function("cholmod_l_eye")
        .allowlist_var("SPQR_ORDERING_DEFAULT")
        .allowlist_var("SPQR_DEFAULT_TOL")
        .allowlist_var("SPQR_QX")
        .allowlist_var("CHOLMOD_REAL")
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("spqr_bindings.rs"))
        .expect("Couldn't write bindings");
}
