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
