use std::{
    fs,
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
}

#[derive(Debug, Clone)]
pub struct Library {
    pub name: String,
    pub manifest_path: PathBuf,
    pub root: PathBuf,
    pub lyp: Option<PathBuf>,
    pub dependencies: IndexMap<String, PathBuf>,
}

impl Library {
    pub fn load(manifest_path: impl AsRef<Path>) -> Result<Self> {
        let manifest_path = manifest_path.as_ref();
        let manifest = read_manifest(manifest_path)?;
        let directory = manifest_path.parent().unwrap_or_else(|| Path::new("."));
        let mut dependencies = IndexMap::new();
        let mut manifests = IndexSet::from_iter([manifest_key(manifest_path)]);
        collect_dependencies(&manifest, directory, &mut dependencies, &mut manifests)?;
        Ok(Self {
            name: manifest.name,
            manifest_path: manifest_path.to_path_buf(),
            root: directory.join("lib.ar"),
            lyp: manifest.lyp.map(|path| resolve(directory, path)),
            dependencies,
        })
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

fn collect_dependencies(
    manifest: &Manifest,
    directory: &Path,
    resolved: &mut IndexMap<String, PathBuf>,
    manifests: &mut IndexSet<PathBuf>,
) -> Result<()> {
    for (name, dependency) in &manifest.dependencies {
        let path = resolve(directory, dependency.clone());
        if let Some(previous) = resolved.get(name) {
            if previous != &path {
                bail!(
                    "path dependency `{name}` resolves to both `{}` and `{}`",
                    previous.display(),
                    path.display()
                );
            }
            continue;
        }
        resolved.insert(name.clone(), path.clone());

        let dependency_directory = if path.is_dir() {
            path.as_path()
        } else {
            path.parent().unwrap_or_else(|| Path::new("."))
        };
        let dependency_manifest = dependency_directory.join("Argon.toml");
        if dependency_manifest.is_file() {
            let dependency_manifest_key = manifest_key(&dependency_manifest);
            if !manifests.insert(dependency_manifest_key.clone()) {
                bail!(
                    "circular path dependency through `{}`",
                    dependency_manifest.display()
                );
            }
            let child = read_manifest(&dependency_manifest)?;
            collect_dependencies(&child, dependency_directory, resolved, manifests)?;
            manifests.shift_remove(&dependency_manifest_key);
        }
    }
    Ok(())
}

fn manifest_key(path: &Path) -> PathBuf {
    fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf())
}

fn resolve(base: &Path, path: PathBuf) -> PathBuf {
    let path = if path.is_relative() {
        base.join(path)
    } else {
        path
    };
    fs::canonicalize(&path).unwrap_or(path)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use super::{Library, Manifest};

    #[test]
    fn parses_manifest() {
        let manifest: Manifest = toml::from_str(
            r#"
                name = "test-library"
                lyp = "layers.lyp"
                [dependencies]
                dependency = "../dependency"
            "#,
        )
        .expect("manifest should parse");
        assert_eq!(manifest.name, "test-library");
        assert_eq!(
            manifest.dependencies["dependency"],
            PathBuf::from("../dependency")
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
    }
}
