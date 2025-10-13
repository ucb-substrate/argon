use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct Config {
    #[serde(default)]
    pub lyp: Option<PathBuf>,
    /// Additional modules to add to the current crate.
    #[serde(default)]
    pub mods: HashMap<String, PathBuf>,
}

pub fn parse_config(manifest_path: impl AsRef<Path>) -> anyhow::Result<Config> {
    Ok(toml::from_str(&std::fs::read_to_string(manifest_path)?)?)
}
