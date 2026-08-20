//! Analyzer runtime configuration and bundled defaults.

use std::path::PathBuf;

const DEFAULT_LYP: &[u8] = include_bytes!("../../../pdks/sky130/sky130.lyp");

// TODO: Allow configuration via ARGON_HOME environment variable.
pub fn default_argon_home() -> Option<PathBuf> {
    Some(homedir::my_home().ok()??.join(".local/state/argon"))
}

/// Materialize the bundled default layer map outside Cargo's source cache.
pub fn default_lyp_path() -> Option<PathBuf> {
    let dir = default_argon_home().unwrap_or_else(|| std::env::temp_dir().join("argon"));
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join("default-sky130.lyp");
    if std::fs::read(&path).ok().as_deref() != Some(DEFAULT_LYP) {
        std::fs::write(&path, DEFAULT_LYP).ok()?;
    }
    Some(path)
}
