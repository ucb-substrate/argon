use std::{fs::File, io::BufReader, path::Path};

use anyhow::{Context, Result, anyhow};
use klayout_lyp::KlayoutLayerProperties;
use rgb::Rgb;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LayerProperties {
    pub layers: Vec<Layer>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layer {
    pub name: String,
    pub fill_color: Rgb<u8>,
    pub border_color: Rgb<u8>,
}

impl From<KlayoutLayerProperties> for LayerProperties {
    fn from(value: KlayoutLayerProperties) -> Self {
        Self {
            layers: value
                .layers
                .into_iter()
                .map(|l| Layer {
                    name: l.name,
                    fill_color: l.fill_color,
                    border_color: l.frame_color,
                })
                .collect(),
        }
    }
}

/// Load a KLayout layer-properties file while preserving useful path and parser
/// context for command-line and editor diagnostics.
pub fn read_lyp(path: impl AsRef<Path>) -> Result<LayerProperties> {
    let path = path.as_ref();
    let file = File::open(path)
        .with_context(|| format!("could not read LYP file `{}`", path.display()))?;
    let properties = klayout_lyp::from_reader(BufReader::new(file))
        .map_err(|error| anyhow!("could not parse LYP file `{}`: {error}", path.display()))?;
    Ok(properties.into())
}
