use std::{
    fs,
    path::{Path, PathBuf},
    process::Command,
    time::SystemTime,
};

const ANTLR_JAR_NAME: &str = "antlr4-4.8-2-SNAPSHOT-complete.jar";
const GENERATED_FILES: [&str; 5] = [
    "argonlexer.rs",
    "argonlistener.rs",
    "argonparser.rs",
    "argonvisitor.rs",
    "mod.rs",
];

fn main() {
    let manifest_dir = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo_root = manifest_dir.parent().and_then(|p| p.parent()).unwrap();
    let grammar_dir = manifest_dir.join("grammar");
    let antlr_dir = repo_root.join("antlr4");
    let output_dir = PathBuf::from(std::env::var("OUT_DIR").unwrap()).join("antlr");
    let grammar = grammar_dir.join("Argon.g4");
    let antlr_jar = antlr_dir.join("tool").join("target").join(ANTLR_JAR_NAME);

    let jar_inputs = [
        antlr_dir.join("pom.xml"),
        antlr_dir.join("tool").join("pom.xml"),
        antlr_dir.join("runtime").join("Java").join("pom.xml"),
        antlr_dir.join("runtime").join("Java").join("src"),
        antlr_dir.join("tool").join("src"),
        antlr_dir
            .join("tool")
            .join("resources")
            .join("org")
            .join("antlr")
            .join("v4")
            .join("tool")
            .join("templates")
            .join("codegen")
            .join("Rust"),
    ];

    if output_is_stale(&antlr_jar, &jar_inputs) {
        let status = Command::new("bash")
            .current_dir(&antlr_dir)
            .arg("-c")
            .arg("mvn -pl tool -am -DskipTests package")
            .status()
            .expect("failed to compile ANTLR tool");
        assert!(status.success(), "ANTLR tool compilation failed");
    }

    assert!(
        antlr_jar.is_file(),
        "missing ANTLR tool jar at {}.",
        antlr_jar.display()
    );

    fs::create_dir_all(&output_dir).unwrap();

    if generated_parser_is_stale(&output_dir, &[grammar.clone(), antlr_jar.clone()]) {
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
    }

    fs::write(
        output_dir.join("mod.rs"),
        "pub mod argonlexer;\npub mod argonlistener;\n#[allow(unused_parens)]\npub mod argonparser;\npub mod argonvisitor;\n",
    )
    .unwrap();

    println!("cargo:rerun-if-changed=grammar/Argon.g4");
    println!("cargo:rerun-if-changed=build.rs");
    println!(
        "cargo:rerun-if-changed={}",
        antlr_dir.join("pom.xml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        antlr_dir.join("tool").join("pom.xml").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        antlr_dir
            .join("runtime")
            .join("Java")
            .join("pom.xml")
            .display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        antlr_dir.join("runtime").join("Java").join("src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        antlr_dir.join("tool").join("src").display()
    );
    println!(
        "cargo:rerun-if-changed={}",
        antlr_dir
            .join("tool")
            .join("resources")
            .join("org")
            .join("antlr")
            .join("v4")
            .join("tool")
            .join("templates")
            .join("codegen")
            .join("Rust")
            .display()
    );
}

fn generated_parser_is_stale(output_dir: &Path, inputs: &[PathBuf]) -> bool {
    GENERATED_FILES
        .iter()
        .map(|file| output_dir.join(file))
        .any(|output| output_is_stale(&output, inputs))
}

fn output_is_stale(output: &Path, inputs: &[PathBuf]) -> bool {
    let Ok(output_time) = modified_time(output) else {
        return true;
    };
    newest_mtime(inputs).is_some_and(|input_time| input_time > output_time)
}

fn newest_mtime(paths: &[PathBuf]) -> Option<SystemTime> {
    paths
        .iter()
        .filter_map(|path| newest_path_mtime(path).ok().flatten())
        .max()
}

fn newest_path_mtime(path: &Path) -> std::io::Result<Option<SystemTime>> {
    if path.is_file() {
        return modified_time(path).map(Some);
    }
    if path.is_dir() {
        let mut newest = None;
        for entry in fs::read_dir(path)? {
            let entry = entry?;
            if let Some(entry_time) = newest_path_mtime(&entry.path())? {
                newest =
                    Some(newest.map_or(entry_time, |current: SystemTime| current.max(entry_time)));
            }
        }
        return Ok(newest);
    }
    Ok(None)
}

fn modified_time(path: &Path) -> std::io::Result<SystemTime> {
    fs::metadata(path)?.modified()
}
