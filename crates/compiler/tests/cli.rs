use std::{
    fs,
    path::PathBuf,
    process::Command,
    time::{SystemTime, UNIX_EPOCH},
};

use argonc::{artifact, compile::CompileOutput};

fn temp_source(name: &str, source: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock should be after the Unix epoch")
        .as_nanos();
    let directory = std::env::temp_dir().join(format!("argonc-{name}-{nonce}"));
    fs::create_dir_all(&directory).expect("temporary directory should be created");
    let path = directory.join("lib.ar");
    fs::write(&path, source).expect("temporary source should be written");
    path
}

#[test]
fn checks_a_source_file() {
    let source = temp_source("valid", "cell top() {}\n");
    let output = Command::new(env!("CARGO_BIN_EXE_argonc"))
        .arg(source)
        .arg("--check")
        .output()
        .expect("argonc should run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn unsupported_source_is_a_diagnostic_not_a_panic() {
    let source = temp_source("unsupported", "struct Point {}\n");
    let output = Command::new(env!("CARGO_BIN_EXE_argonc"))
        .arg(source)
        .arg("--check")
        .output()
        .expect("argonc should run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("error: error during parsing"), "{stderr}");
    assert!(!stderr.contains("panicked at"), "{stderr}");
}

#[test]
fn missing_input_is_reported_cleanly() {
    let output = Command::new(env!("CARGO_BIN_EXE_argonc"))
        .arg("/path/that/does/not/exist/lib.ar")
        .arg("--check")
        .output()
        .expect("argonc should run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("could not load source"), "{stderr}");
    assert!(!stderr.contains("panicked at"), "{stderr}");
}

#[test]
fn execution_writes_binary_output_without_gds_by_default() {
    let source = temp_source(
        "run",
        "cell top() { let r = rect(\"met1\", x0=0., y0=0., x1=10., y1=20.); }\n",
    );
    let directory = source.parent().expect("source should have a parent");
    let artifact_path = directory.join("top.bin");
    let implicit_gds_path = source.with_extension("gds");
    let lyp = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/lyp/basic.lyp");
    let output = Command::new(env!("CARGO_BIN_EXE_argonc"))
        .arg(&source)
        .arg("--cell")
        .arg("top()")
        .arg("--lyp")
        .arg(lyp)
        .arg("--output")
        .arg(&artifact_path)
        .output()
        .expect("argonc should run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(matches!(
        artifact::read(artifact_path).expect("artifact should decode"),
        CompileOutput::Valid(_)
    ));
    assert!(!implicit_gds_path.exists());
}
