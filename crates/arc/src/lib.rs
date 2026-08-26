use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use indexmap::{IndexMap, IndexSet};
use serde::Deserialize;

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

    use super::{Library, Manifest};

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
