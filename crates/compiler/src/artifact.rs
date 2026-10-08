use std::{
    fs,
    io::{BufWriter, Write},
    path::Path,
};

use anyhow::{Context, Result, bail};

use crate::compile::CompileOutput;

const MAGIC: &[u8; 8] = b"ARGON\0\0\x01";

/// Writes a successful compiler result in Argon's versioned binary format.
pub fn write(output: &CompileOutput, path: impl AsRef<Path>) -> Result<()> {
    let path = path.as_ref();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("could not create `{}`", parent.display()))?;
    }
    // Streamed to the file: the artifact can be larger than the output it
    // encodes, so it is never held in memory whole.
    let file =
        fs::File::create(path).with_context(|| format!("could not write `{}`", path.display()))?;
    let mut writer = BufWriter::new(file);
    writer
        .write_all(MAGIC)
        .with_context(|| format!("could not write `{}`", path.display()))?;
    bincode::serialize_into(&mut writer, output).context("could not serialize compiler output")?;
    writer
        .flush()
        .with_context(|| format!("could not write `{}`", path.display()))
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
