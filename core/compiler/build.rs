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

    println!("cargo:rerun-if-env-changed=SUITESPARSE_INCLUDE_DIR");
    println!("cargo:rerun-if-env-changed=SUITESPARSE_LIB_DIR");

    let include_dir =
        env::var("SUITESPARSE_INCLUDE_DIR").unwrap_or_else(|_| "/opt/homebrew/include".to_string());
    let lib_dir =
        env::var("SUITESPARSE_LIB_DIR").unwrap_or_else(|_| "/opt/homebrew/lib".to_string());

    println!("cargo:rustc-link-search=native={}", lib_dir);
    println!("cargo:rustc-link-lib=spqr");
    println!("cargo:rustc-link-lib=cholmod");
    println!("cargo:rustc-link-lib=suitesparseconfig");
    println!("cargo:rustc-link-lib=amd");
    println!("cargo:rustc-link-lib=colamd");
    println!("cargo:rustc-link-lib=ccolamd");
    println!("cargo:rustc-link-lib=camd");

    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .clang_arg(format!("-I{}", include_dir))
        .clang_arg(format!("-I{}/suitesparse", include_dir))
        .wrap_unsafe_ops(true)
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .allowlist_function("SuiteSparseQR_C_QR")
        .allowlist_function("cholmod_l_start")
        .allowlist_function("cholmod_l_finish")
        .allowlist_function("cholmod_l_free")
        .allowlist_function("cholmod_l_free_sparse")
        .allowlist_function("cholmod_l_free_dense")
        .allowlist_function("cholmod_l_sparse_to_dense")
        .allowlist_function("cholmod_l_allocate_triplet")
        .allowlist_function("cholmod_l_free_triplet")
        .allowlist_function("cholmod_l_triplet_to_sparse")
        .allowlist_var("SPQR_ORDERING_DEFAULT")
        .allowlist_var("SPQR_DEFAULT_TOL")
        .allowlist_var("CHOLMOD_REAL")
        .generate()
        .expect("Unable to generate bindings");

    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings
        .write_to_file(out_path.join("spqr_bindings.rs"))
        .expect("Couldn't write bindings");
}
