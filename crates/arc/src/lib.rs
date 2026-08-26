use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use indexmap::{IndexMap, IndexSet};
use serde::Deserialize;

pub mod cli;

const MANIFEST_FILE: &str = "Argon.toml";
const SOURCE_FILE: &str = "lib.ar";
const LYP_FILE: &str = "layers.lyp";

const STARTER_SOURCE: &str = r#"cell top() {
    text("Hello world!", "text.label", 0., 0.);
}
"#;

const DEFAULT_LYP: &str = r#"<?xml version="1.0" encoding="utf-8"?>
<layer-properties>
 <name>Default Layer Properties</name>
 <properties>
  <frame-color>#0000ff</frame-color>
  <fill-color>#0000ff</fill-color>
  <frame-brightness>0</frame-brightness>
  <fill-brightness>0</fill-brightness>
  <dither-pattern>C7</dither-pattern>
  <line-style>C0</line-style>
  <valid>true</valid>
  <visible>true</visible>
  <transparent>false</transparent>
  <width>1</width>
  <marked>false</marked>
  <xfill>false</xfill>
  <animation>0</animation>
  <name>met1</name>
  <source>1/0@1</source>
 </properties>
 <properties>
  <frame-color>#ff00ff</frame-color>
  <fill-color>#ff00ff</fill-color>
  <frame-brightness>0</frame-brightness>
  <fill-brightness>0</fill-brightness>
  <dither-pattern>C7</dither-pattern>
  <line-style>C0</line-style>
  <valid>true</valid>
  <visible>true</visible>
  <transparent>false</transparent>
  <width>1</width>
  <marked>false</marked>
  <xfill>false</xfill>
  <animation>0</animation>
  <name>met2</name>
  <source>2/0@1</source>
 </properties>
 <properties>
  <frame-color>#0080ff</frame-color>
  <fill-color>#0080ff</fill-color>
  <frame-brightness>0</frame-brightness>
  <fill-brightness>0</fill-brightness>
  <dither-pattern>C7</dither-pattern>
  <line-style>C0</line-style>
  <valid>true</valid>
  <visible>true</visible>
  <transparent>false</transparent>
  <width>1</width>
  <marked>false</marked>
  <xfill>false</xfill>
  <animation>0</animation>
  <name>text.label</name>
  <source>10/0@1</source>
 </properties>
</layer-properties>
"#;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Human-readable library name shown by `arc`.
    pub name: String,
    /// KLayout layer-properties file, relative to the manifest directory.
    #[serde(default)]
    pub lyp: Option<PathBuf>,
    /// Path library dependencies.
    #[serde(default)]
    pub dependencies: IndexMap<String, PathBuf>,
    /// GDS files imported as zero-argument cells.
    #[serde(default)]
    pub gds: IndexMap<String, PathBuf>,
}

#[derive(Debug, Clone)]
pub struct Library {
    pub name: String,
    pub manifest_path: PathBuf,
    pub root: PathBuf,
    pub lyp: Option<PathBuf>,
    pub dependencies: IndexMap<String, PathBuf>,
    pub gds: IndexMap<String, PathBuf>,
}

#[derive(Debug, Default)]
pub struct FormatReport {
    pub files: Vec<PathBuf>,
    pub changed: Vec<PathBuf>,
}

impl Library {
    pub fn load(manifest_path: impl AsRef<Path>) -> Result<Self> {
        let manifest_path = manifest_path.as_ref();
        let manifest = read_manifest(manifest_path)?;
        let directory = manifest_path.parent().unwrap_or_else(|| Path::new("."));
        let dependencies = DependencyResolver::new(manifest_path).resolve(&manifest, directory)?;
        let gds = manifest
            .gds
            .into_iter()
            .map(|(name, path)| Ok((validate_cell_path(name)?, resolve_path(directory, path))))
            .collect::<Result<_>>()?;
        Ok(Self {
            name: manifest.name,
            manifest_path: manifest_path.to_path_buf(),
            root: directory.join("lib.ar"),
            lyp: manifest.lyp.map(|path| resolve_path(directory, path)),
            dependencies,
            gds,
        })
    }

    pub fn directory(&self) -> &Path {
        self.manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
    }

    pub fn target_path(&self, file_name: impl AsRef<Path>) -> PathBuf {
        self.directory().join("target").join(file_name)
    }
}

/// Create a new, self-contained Argon workspace at path.
pub fn create_workspace(path: impl AsRef<Path>, name: Option<&str>) -> Result<Library> {
    let path = path.as_ref();
    let name = name
        .map(str::to_owned)
        .or_else(|| {
            path.file_name()
                .and_then(|name| name.to_str())
                .map(str::to_owned)
        })
        .ok_or_else(|| {
            anyhow::anyhow!(
                "could not determine a workspace name from '{}'; pass --name",
                path.display()
            )
        })?;
    validate_workspace_name(&name)?;

    if path.exists() {
        bail!("destination '{}' already exists", path.display());
    }

    fs::create_dir_all(path)
        .with_context(|| format!("could not create workspace '{}'", path.display()))?;
    let manifest_path = path.join(MANIFEST_FILE);
    fs::write(
        &manifest_path,
        format!("name = {name:?}\nlyp = {LYP_FILE:?}\n"),
    )
    .with_context(|| format!("could not write '{}'", manifest_path.display()))?;
    fs::write(path.join(SOURCE_FILE), STARTER_SOURCE)
        .with_context(|| format!("could not write '{}'", path.join(SOURCE_FILE).display()))?;
    fs::write(path.join(LYP_FILE), DEFAULT_LYP)
        .with_context(|| format!("could not write '{}'", path.join(LYP_FILE).display()))?;

    Library::load(manifest_path)
}

pub fn format_path(path: impl AsRef<Path>, check: bool) -> Result<FormatReport> {
    let path = path.as_ref();
    let mut files = Vec::new();
    collect_argon_files(path, &mut files)?;
    format_files(files, check)
}

/// Find the nearest Argon manifest in `start` or one of its parent directories.
pub fn find_manifest_path(start: impl AsRef<Path>) -> Result<PathBuf> {
    let start = start.as_ref();
    let mut directory = if start.is_absolute() {
        start.to_path_buf()
    } else {
        env::current_dir()
            .context("could not determine the current directory")?
            .join(start)
    };
    if directory.is_file() {
        directory.pop();
    }

    loop {
        let candidate = directory.join(MANIFEST_FILE);
        if candidate.is_file() {
            return Ok(candidate);
        }
        if !directory.pop() {
            bail!(
                "could not find {MANIFEST_FILE} in '{}' or any parent directory",
                start.display()
            );
        }
    }
}

/// Format the Argon source files owned by one manifest-defined workspace.
pub fn format_workspace(manifest_path: impl AsRef<Path>, check: bool) -> Result<FormatReport> {
    let manifest_path = manifest_path.as_ref();
    read_manifest(manifest_path)?;
    let directory = manifest_path.parent().unwrap_or_else(|| Path::new("."));
    let mut files = Vec::new();
    collect_workspace_argon_files(directory, &mut files)?;
    format_files(files, check)
}

fn format_files(mut files: Vec<PathBuf>, check: bool) -> Result<FormatReport> {
    files.sort();

    let mut changed = Vec::new();
    for file in &files {
        let source = fs::read_to_string(file)
            .with_context(|| format!("could not read '{}'", file.display()))?;
        let formatted = format_source(&source);
        if source != formatted {
            changed.push(file.clone());
            if !check {
                fs::write(file, formatted)
                    .with_context(|| format!("could not write '{}'", file.display()))?;
            }
        }
    }
    Ok(FormatReport { files, changed })
}

pub fn format_source(source: &str) -> String {
    let normalized = source.replace("\r\n", "\n").replace('\r', "\n");
    let mut output = Vec::new();
    let mut delimiters = Vec::new();
    let mut in_string = false;

    for line in normalized.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            if output.last().is_some_and(|line: &String| !line.is_empty()) {
                output.push(String::new());
            }
            continue;
        }

        let leading_closers = leading_closing_delimiters(trimmed, &delimiters, in_string);
        let remaining = &delimiters[..delimiters.len().saturating_sub(leading_closers)];
        let depth = if trimmed.starts_with('}') && !in_string {
            remaining
                .iter()
                .filter(|delimiter| **delimiter == '{')
                .count()
        } else {
            indentation_depth(remaining)
        };
        output.push(format!("{}{trimmed}", " ".repeat(depth * 4)));
        update_delimiters(trimmed, &mut delimiters, &mut in_string);
    }

    while output.last().is_some_and(String::is_empty) {
        output.pop();
    }
    if output.is_empty() {
        String::new()
    } else {
        format!("{}\n", output.join("\n"))
    }
}

fn collect_argon_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let metadata =
        fs::metadata(path).with_context(|| format!("could not access '{}'", path.display()))?;
    if metadata.is_file() {
        if path.extension().is_some_and(|extension| extension == "ar") {
            files.push(path.to_path_buf());
        } else {
            bail!("'{}' is not an Argon source file", path.display());
        }
        return Ok(());
    }
    if !metadata.is_dir() {
        bail!("'{}' is not a file or directory", path.display());
    }

    let mut entries = fs::read_dir(path)
        .with_context(|| format!("could not read directory '{}'", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("could not read directory '{}'", path.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry
            .file_type()
            .with_context(|| format!("could not inspect '{}'", entry.path().display()))?;
        if file_type.is_dir() {
            if entry.file_name() != ".git" && entry.file_name() != "target" {
                collect_argon_files(&entry.path(), files)?;
            }
        } else if file_type.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "ar")
        {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn collect_workspace_argon_files(path: &Path, files: &mut Vec<PathBuf>) -> Result<()> {
    let mut entries = fs::read_dir(path)
        .with_context(|| format!("could not read directory '{}'", path.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("could not read directory '{}'", path.display()))?;
    entries.sort_by_key(|entry| entry.file_name());
    for entry in entries {
        let file_type = entry
            .file_type()
            .with_context(|| format!("could not inspect '{}'", entry.path().display()))?;
        if file_type.is_dir() {
            let name = entry.file_name();
            if name == ".git" || name == "target" {
                continue;
            }
            let nested_workspace = entry.path().join(MANIFEST_FILE).is_file();
            if !nested_workspace {
                collect_workspace_argon_files(&entry.path(), files)?;
            }
        } else if file_type.is_file()
            && entry
                .path()
                .extension()
                .is_some_and(|extension| extension == "ar")
        {
            files.push(entry.path());
        }
    }
    Ok(())
}

fn indentation_depth(delimiters: &[char]) -> usize {
    let brace_count = delimiters
        .iter()
        .filter(|delimiter| **delimiter == '{')
        .count();
    let continuation_count = delimiters
        .iter()
        .rposition(|delimiter| *delimiter == '{')
        .map_or(delimiters.len(), |brace| delimiters.len() - brace - 1);
    brace_count + continuation_count
}

fn leading_closing_delimiters(line: &str, delimiters: &[char], in_string: bool) -> usize {
    if in_string {
        return 0;
    }
    let mut stack_index = delimiters.len();
    let mut closers = 0;
    for ch in line.chars() {
        if ch.is_whitespace() {
            continue;
        }
        let Some(open) = stack_index
            .checked_sub(1)
            .and_then(|index| delimiters.get(index))
        else {
            break;
        };
        if closes(*open, ch) {
            stack_index -= 1;
            closers += 1;
        } else {
            break;
        }
    }
    closers
}

fn update_delimiters(line: &str, delimiters: &mut Vec<char>, in_string: &mut bool) {
    let mut chars = line.chars().peekable();
    let mut escaped = false;
    while let Some(ch) = chars.next() {
        if *in_string {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                *in_string = false;
            }
            continue;
        }
        if ch == '/' && chars.peek() == Some(&'/') {
            break;
        }
        match ch {
            '"' => *in_string = true,
            '{' | '(' | '[' => delimiters.push(ch),
            '}' | ')' | ']' if delimiters.last().is_some_and(|open| closes(*open, ch)) => {
                delimiters.pop();
            }
            _ => {}
        }
    }
}

fn closes(open: char, close: char) -> bool {
    matches!((open, close), ('{', '}') | ('(', ')') | ('[', ']'))
}

fn validate_workspace_name(name: &str) -> Result<()> {
    if name.is_empty()
        || !name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_')
    {
        bail!(
            "invalid workspace name '{name}'; names may contain only ASCII letters, numbers, -, and _"
        );
    }
    Ok(())
}

fn read_manifest(path: &Path) -> Result<Manifest> {
    let text = fs::read_to_string(path)
        .with_context(|| format!("could not read manifest `{}`", path.display()))?;
    let manifest: Manifest = toml::from_str(&text)
        .with_context(|| format!("could not parse manifest `{}`", path.display()))?;
    if manifest.name.trim().is_empty() {
        bail!(
            "library name in manifest `{}` cannot be empty",
            path.display()
        );
    }
    Ok(manifest)
}

struct DependencyResolver {
    dependencies: IndexMap<String, PathBuf>,
    active_manifests: IndexSet<PathBuf>,
}

impl DependencyResolver {
    fn new(root_manifest: &Path) -> Self {
        Self {
            dependencies: IndexMap::new(),
            active_manifests: IndexSet::from_iter([manifest_key(root_manifest)]),
        }
    }

    fn resolve(
        mut self,
        manifest: &Manifest,
        directory: &Path,
    ) -> Result<IndexMap<String, PathBuf>> {
        self.collect(manifest, directory)?;
        Ok(self.dependencies)
    }

    fn collect(&mut self, manifest: &Manifest, directory: &Path) -> Result<()> {
        for (name, dependency) in &manifest.dependencies {
            let path = resolve_path(directory, dependency.clone());
            if let Some(previous) = self.dependencies.get(name) {
                if previous != &path {
                    bail!(
                        "path dependency `{name}` resolves to both `{}` and `{}`",
                        previous.display(),
                        path.display()
                    );
                }
                continue;
            }
            self.dependencies.insert(name.clone(), path.clone());

            let dependency_directory = if path.is_dir() {
                path.as_path()
            } else {
                path.parent().unwrap_or_else(|| Path::new("."))
            };
            let dependency_manifest = dependency_directory.join("Argon.toml");
            if dependency_manifest.is_file() {
                let key = manifest_key(&dependency_manifest);
                if !self.active_manifests.insert(key.clone()) {
                    bail!(
                        "circular path dependency through `{}`",
                        dependency_manifest.display()
                    );
                }
                let child = read_manifest(&dependency_manifest)?;
                self.collect(&child, dependency_directory)?;
                self.active_manifests.shift_remove(&key);
            }
        }
        Ok(())
    }
}

fn manifest_key(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolve_path(base: &Path, path: PathBuf) -> PathBuf {
    let path = expand_home(path);
    let path = if path.is_relative() {
        base.join(path)
    } else {
        path
    };
    fs::canonicalize(&path).unwrap_or(path)
}

fn expand_home(path: PathBuf) -> PathBuf {
    let Some(path_text) = path.to_str() else {
        return path;
    };
    let Some(suffix) = path_text.strip_prefix("~/") else {
        return path;
    };
    env::var_os("HOME")
        .map(PathBuf::from)
        .map(|home| home.join(suffix))
        .unwrap_or(path)
}

fn validate_cell_path(name: String) -> Result<String> {
    if name.split("::").all(is_identifier) {
        Ok(name)
    } else {
        bail!("invalid GDS cell path `{name}`")
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use argonc::{
        compile::{CompileInput, compile},
        parse::parse_workspace_with_std,
    };

    use super::{
        Library, Manifest, create_workspace, find_manifest_path, format_path, format_source,
        format_workspace,
    };

    #[test]
    fn formats_nested_delimiters_without_reading_strings_or_comments() {
        let source = r#"
cell top() {
  // A comment containing } does not close the cell.
 let values = list(
 "a { string",
 cons(
 1,
 []));
}
"#;
        assert_eq!(
            format_source(source),
            r#"cell top() {
    // A comment containing } does not close the cell.
    let values = list(
        "a { string",
        cons(
            1,
            []));
}
"#
        );
    }

    #[test]
    fn formatting_check_does_not_write_files() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        fs::write(
            &source_path,
            "cell top() {\n  text(\"hi\", \"text\", 0., 0.);  \n}\n",
        )
        .unwrap();

        let report = format_path(directory.path(), true).unwrap();
        assert_eq!(report.files.as_slice(), std::slice::from_ref(&source_path));
        assert_eq!(
            report.changed.as_slice(),
            std::slice::from_ref(&source_path)
        );
        assert!(fs::read_to_string(&source_path).unwrap().contains("  text"));

        let report = format_path(directory.path(), false).unwrap();
        assert_eq!(
            report.changed.as_slice(),
            std::slice::from_ref(&source_path)
        );
        assert_eq!(
            fs::read_to_string(&source_path).unwrap(),
            "cell top() {\n    text(\"hi\", \"text\", 0., 0.);\n}\n"
        );
        assert!(
            format_path(directory.path(), true)
                .unwrap()
                .changed
                .is_empty()
        );
    }

    #[test]
    fn formatting_is_scoped_to_the_nearest_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let workspace = directory.path().join("workspace");
        let modules = workspace.join("modules");
        let nested = workspace.join("nested");
        let target = workspace.join("target");
        for path in [&modules, &nested, &target] {
            fs::create_dir_all(path).unwrap();
        }
        fs::write(workspace.join("Argon.toml"), "name = \"workspace\"\n").unwrap();
        fs::write(workspace.join("lib.ar"), "cell top() {  \n}\n").unwrap();
        fs::write(modules.join("device.ar"), "cell device() {  \n}\n").unwrap();
        fs::write(nested.join("Argon.toml"), "name = \"nested\"\n").unwrap();
        fs::write(nested.join("lib.ar"), "cell nested() {  \n}\n").unwrap();
        fs::write(target.join("generated.ar"), "cell generated() {  \n}\n").unwrap();

        let manifest = find_manifest_path(&modules).unwrap();
        assert_eq!(manifest, workspace.join("Argon.toml"));
        let report = format_workspace(manifest, false).unwrap();
        assert_eq!(
            report.files,
            vec![workspace.join("lib.ar"), modules.join("device.ar")]
        );
        assert_eq!(
            fs::read_to_string(nested.join("lib.ar")).unwrap(),
            "cell nested() {  \n}\n"
        );
        assert_eq!(
            fs::read_to_string(target.join("generated.ar")).unwrap(),
            "cell generated() {  \n}\n"
        );
    }

    #[test]
    fn creates_runnable_starter_workspace() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let workspace = directory.path().join("hello-argon");
        let library =
            create_workspace(&workspace, None).expect("starter workspace should be created");

        assert_eq!(library.name, "hello-argon");
        assert_eq!(
            fs::read_to_string(workspace.join("Argon.toml")).unwrap(),
            "name = \"hello-argon\"\nlyp = \"layers.lyp\"\n"
        );
        assert_eq!(
            fs::read_to_string(&library.root).unwrap(),
            "cell top() {\n    text(\"Hello world!\", \"text.label\", 0., 0.);\n}\n"
        );

        let lyp = library
            .lyp
            .as_ref()
            .expect("starter manifest should set lyp");
        let parsed = parse_workspace_with_std(&library.root);
        assert!(parsed.static_errors().is_empty());
        let output = compile(
            &parsed.ast(),
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: lyp,
            },
        )
        .unwrap_valid();
        let label = output.cells[&output.top]
            .objects
            .values()
            .find_map(|object| object.get_text())
            .expect("top should emit a text label");
        assert_eq!(label.text, "Hello world!");
        assert_eq!(label.layer, "text.label");
        assert_eq!((label.x, label.y), (0., 0.));
    }

    #[test]
    fn refuses_to_overwrite_an_existing_destination() {
        let directory = tempfile::tempdir().expect("temporary directory should be created");
        let workspace = directory.path().join("existing");
        fs::create_dir(&workspace).unwrap();
        fs::write(workspace.join("keep"), "user data").unwrap();

        let error =
            create_workspace(&workspace, None).expect_err("existing destination must be rejected");
        assert!(error.to_string().contains("already exists"));
        assert_eq!(
            fs::read_to_string(workspace.join("keep")).unwrap(),
            "user data"
        );
    }

    fn example_library_dirs(directory: &std::path::Path, output: &mut Vec<PathBuf>) {
        if directory.join("lib.ar").is_file() {
            output.push(directory.to_path_buf());
        }
        for entry in fs::read_dir(directory).expect("example directory should be readable") {
            let path = entry.expect("example entry should be readable").path();
            if path.is_dir() {
                example_library_dirs(&path, output);
            }
        }
    }

    #[test]
    fn every_example_library_has_a_gui_manifest() {
        let examples = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples");
        let mut libraries = Vec::new();
        example_library_dirs(&examples, &mut libraries);
        assert!(!libraries.is_empty());

        for directory in libraries {
            let manifest = directory.join("Argon.toml");
            assert!(manifest.is_file(), "missing `{}`", manifest.display());
            let library = Library::load(&manifest)
                .unwrap_or_else(|error| panic!("invalid `{}`: {error:#}", manifest.display()));
            let lyp = library
                .lyp
                .unwrap_or_else(|| panic!("`{}` does not set `lyp`", manifest.display()));
            assert!(lyp.is_file(), "missing LYP `{}`", lyp.display());
        }
    }

    #[test]
    fn parses_manifest() {
        let manifest: Manifest = toml::from_str(
            r#"
                name = "test-library"
                lyp = "layers.lyp"
                [dependencies]
                dependency = "../dependency"
                [gds]
                "macros::sram" = "~/layout/sram.gds"
            "#,
        )
        .expect("manifest should parse");
        assert_eq!(manifest.name, "test-library");
        assert_eq!(
            manifest.dependencies["dependency"],
            PathBuf::from("../dependency")
        );
        assert_eq!(
            manifest.gds["macros::sram"],
            PathBuf::from("~/layout/sram.gds")
        );
    }

    #[test]
    fn rejects_unsupported_manifest_formats() {
        let mods = toml::from_str::<Manifest>(
            r#"
                name = "test-library"
                [mods]
                dependency = "../dependency"
            "#,
        );
        assert!(mods.is_err());

        let detailed_dependency = toml::from_str::<Manifest>(
            r#"
                name = "test-library"
                [dependencies]
                dependency = { path = "../dependency" }
            "#,
        );
        assert!(detailed_dependency.is_err());
    }

    #[test]
    fn resolves_transitive_path_dependencies() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("arc-manifest-{nonce}"));
        let app = root.join("app");
        let first = root.join("first");
        let second = root.join("second");
        for directory in [&app, &first, &second] {
            fs::create_dir_all(directory).expect("test library should be created");
        }
        fs::write(
            app.join("Argon.toml"),
            "name = \"app\"\n[dependencies]\nfirst = \"../first\"\n",
        )
        .expect("root manifest should be written");
        fs::write(
            first.join("Argon.toml"),
            "name = \"first\"\n[dependencies]\nsecond = \"../second\"\n",
        )
        .expect("dependency manifest should be written");

        let library = Library::load(app.join("Argon.toml")).expect("library should resolve");
        assert_eq!(library.name, "app");
        assert_eq!(
            library.dependencies["first"],
            fs::canonicalize(first).expect("dependency path should canonicalize")
        );
        assert_eq!(
            library.dependencies["second"],
            fs::canonicalize(second).expect("dependency path should canonicalize")
        );
        assert_eq!(
            library.target_path("argon.bin"),
            app.join("target/argon.bin")
        );
    }

    #[test]
    fn rejects_circular_path_dependencies() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after Unix epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!("arc-cycle-{nonce}"));
        let app = root.join("app");
        let dependency = root.join("dependency");
        for directory in [&app, &dependency] {
            fs::create_dir_all(directory).expect("test library should be created");
        }
        fs::write(
            app.join("Argon.toml"),
            "name = \"app\"\n[dependencies]\ndependency = \"../dependency\"\n",
        )
        .expect("root manifest should be written");
        fs::write(
            dependency.join("Argon.toml"),
            "name = \"dependency\"\n[dependencies]\napp = \"../app\"\n",
        )
        .expect("dependency manifest should be written");

        let error =
            Library::load(app.join("Argon.toml")).expect_err("dependency cycle should be rejected");
        assert!(error.to_string().contains("circular path dependency"));
    }
}
