use std::{fs, path::PathBuf, process::Command};

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest_dir.parent().and_then(|p| p.parent()).unwrap();
    let grammar_dir = manifest_dir.join("grammar");
    let toolchain_dir = repo_root.parent().unwrap().join(".argon-toolchain");
    let output_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("antlr");
    let antlr_jar = std::env::var_os("ARGON_ANTLR_JAR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            toolchain_dir
                .join("antlr4-tool")
                .join("tool")
                .join("target")
                .join("antlr4-4.8-2-SNAPSHOT-complete.jar")
        });

    assert!(
        antlr_jar.is_file(),
        "missing ANTLR tool jar at {}. Run ./scripts/build-compiler.sh, ./scripts/bootstrap-antlr.sh, or set ARGON_ANTLR_JAR.",
        antlr_jar.display()
    );

    fs::create_dir_all(&output_dir).unwrap();

    let status = Command::new("java")
        .current_dir(&grammar_dir)
        .arg("-cp")
        .arg(&antlr_jar)
        .arg("org.antlr.v4.Tool")
        .arg("-Dlanguage=Rust")
        .arg("-visitor")
        .arg("-o")
        .arg(&output_dir)
        .arg("Argon.g4")
        .status()
        .expect("failed to start ANTLR tool");

    assert!(status.success(), "ANTLR tool failed");

    fs::write(
        output_dir.join("mod.rs"),
        "pub mod argonlexer;\npub mod argonlistener;\n#[allow(unused_parens)]\npub mod argonparser;\npub mod argonvisitor;\n",
    )
    .unwrap();

    println!("cargo:rerun-if-changed=grammar/Argon.g4");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-env-changed=ARGON_ANTLR_JAR");
}
