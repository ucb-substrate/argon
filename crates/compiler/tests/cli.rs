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

#[test]
fn execution_accepts_a_boolean_cell_argument() {
    let source = temp_source("bool-root", "");
    let dependency = temp_source(
        "bool-dependency",
        r#"cell device(enabled: Bool, w: Float, count: Int) {
    if enabled {
        rect("met1", x0=0., y0=0., w=w, h=10.);
    } else {
        rect("met1", x0=0., y0=0., w=w, h=20.);
    };
}
"#,
    );
    let lyp = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/lyp/basic.lyp");
    let artifact_path = std::env::temp_dir().join("argonc-bool.bin");
    let output = Command::new(env!("CARGO_BIN_EXE_argonc"))
        .arg(source)
        .arg("--dependency")
        .arg(format!("devices={}", dependency.display()))
        .arg("--cell")
        .arg("devices::device(true, 150., 5)")
        .arg("--lyp")
        .arg(lyp)
        .arg("--output")
        .arg(artifact_path)
        .output()
        .expect("argonc should run");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn invalid_cell_argument_type_is_reported_cleanly() {
    let source = temp_source("invalid-argument-root", "");
    let dependency = temp_source(
        "invalid-argument-dependency",
        "cell device(enabled: Bool, w: Float, count: Int) {}\n",
    );
    let lyp = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/lyp/basic.lyp");
    let output = Command::new(env!("CARGO_BIN_EXE_argonc"))
        .arg(source)
        .arg("--dependency")
        .arg(format!("devices={}", dependency.display()))
        .arg("--cell")
        .arg("devices::device(1, 150., 5)")
        .arg("--lyp")
        .arg(lyp)
        .arg("--output")
        .arg(std::env::temp_dir().join("argonc-invalid-argument.bin"))
        .output()
        .expect("argonc should run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains("invalid cell argument 1: expected Bool, found Int"),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked at"), "{stderr}");
}

#[test]
fn missing_lyp_is_reported_with_its_path() {
    let source = temp_source("missing-lyp", "cell top() {}\n");
    let missing = source
        .parent()
        .expect("source should have a parent")
        .join("missing.lyp");
    let output = Command::new(env!("CARGO_BIN_EXE_argonc"))
        .arg(source)
        .arg("--cell")
        .arg("top()")
        .arg("--lyp")
        .arg(&missing)
        .output()
        .expect("argonc should run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("could not read LYP file"), "{stderr}");
    assert!(stderr.contains(&missing.display().to_string()), "{stderr}");
    assert!(!stderr.contains("panicked at"), "{stderr}");
}

#[test]
fn malformed_lyp_is_reported_with_its_path() {
    let source = temp_source("malformed-lyp", "cell top() {}\n");
    let malformed = source
        .parent()
        .expect("source should have a parent")
        .join("malformed.lyp");
    fs::write(&malformed, "not XML").expect("malformed LYP should be written");
    let output = Command::new(env!("CARGO_BIN_EXE_argonc"))
        .arg(source)
        .arg("--cell")
        .arg("top()")
        .arg("--lyp")
        .arg(&malformed)
        .output()
        .expect("argonc should run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("could not parse LYP file"), "{stderr}");
    assert!(
        stderr.contains(&malformed.display().to_string()),
        "{stderr}"
    );
    assert!(!stderr.contains("panicked at"), "{stderr}");
}

#[test]
fn standard_library_errors_show_embedded_source_lines() {
    let source = temp_source(
        "std-diagnostic",
        r#"cell top() {
    let r = crect(layer="missing.drawing", x0=0., y0=0., w=10., h=10.);
    std::array(r, 2, 20., 0.);
}
"#,
    );
    let lyp = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/lyp/basic.lyp");
    let output = Command::new(env!("CARGO_BIN_EXE_argonc"))
        .arg(source)
        .arg("--cell")
        .arg("top()")
        .arg("--lyp")
        .arg(&lyp)
        .output()
        .expect("argonc should run");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(
        stderr.contains(&format!(
            "rectangle uses layer `missing.drawing`, which is not defined in LYP file `{}`",
            lyp.display()
        )),
        "{stderr}"
    );
    assert!(stderr.contains("--> <argon-std>/lib.ar:"), "{stderr}");
    assert!(
        stderr.contains("let first_rect = rect(r.layer);"),
        "{stderr}"
    );
    assert!(!stderr.contains("<argon-std>/lib.ar:1:1"), "{stderr}");
}

#[test]
fn help_uses_dependency_terminology() {
    let output = Command::new(env!("CARGO_BIN_EXE_argonc"))
        .arg("--help")
        .output()
        .expect("argonc should show help");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success());
    assert!(stdout.contains("--dependency"), "{stdout}");
    assert!(!stdout.contains("--extern"), "{stdout}");
}
