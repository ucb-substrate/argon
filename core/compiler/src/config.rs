use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct Config {
    pub lyp: PathBuf,
}

pub fn parse_config(manifest_path: impl AsRef<Path>) -> anyhow::Result<Config> {
    Ok(toml::from_str(&std::fs::read_to_string(manifest_path)?)?)
}
