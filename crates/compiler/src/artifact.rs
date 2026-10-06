use std::{fs, path::Path};

use anyhow::{Context, Result, bail};

use crate::compile::CompileOutput;

const MAGIC: &[u8; 8] = b"ARGON\0\0\x02";

/// Writes a successful compiler result in Argon's versioned binary format.
pub fn write(output: &CompileOutput, path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    let payload = bincode::serialize(output).context("could not serialize compiler output")?;
    let mut bytes = Vec::with_capacity(MAGIC.len() + payload.len());
    bytes.extend_from_slice(MAGIC);
    bytes.extend_from_slice(&payload);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create `{}`", parent.display()))?;
    }
    fs::write(path, bytes).with_context(|| format!("could not write `{}`", path.display()))
}

/// Reads an Argon binary compiler-output artifact.
pub fn read(path: impl AsRef<Path>) -> Result<CompileOutput> {
    let path = path.as_ref();
    let bytes = fs::read(path).with_context(|| format!("could not read `{}`", path.display()))?;
    let Some(payload) = bytes.strip_prefix(MAGIC) else {
        bail!("`{}` is not an Argon compiler artifact", path.display());
    };
    bincode::deserialize(payload).context("could not deserialize compiler output")
}
