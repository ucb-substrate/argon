//! Argon technology-file parsing and physical-unit conversions.

use std::{fs, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use indexmap::{IndexMap, IndexSet};
use rgb::Rgb;
use serde::{Deserialize, Serialize};

/// All technology information needed by the compiler and layout editor.
///
/// [`Technology::dbu`] is the physical size of one GDS database unit in
/// meters. All other length configuration is expressed as an integer number of
/// DBUs. Argon source coordinates use the configured display unit.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Technology {
    pub dbu: f64,
    /// Number of DBUs in one Argon source/display coordinate unit.
    pub display_unit: u64,
    /// Snap-grid spacing in DBUs.
    pub grid: u64,
    pub layers: Vec<Layer>,
    /// Pin-shape layer name to the text-label layer carrying the pin name.
    pub pin_layers: IndexMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Layer {
    pub name: String,
    pub gds_layer: i16,
    pub gds_datatype: i16,
    pub fill_color: Rgb<u8>,
    pub border_color: Rgb<u8>,
}

impl Technology {
    /// Snap-grid spacing in Argon source/display coordinate units.
    pub fn grid_step(&self) -> f64 {
        self.grid as f64 / self.display_unit as f64
    }

    /// Convert a coordinate in GDS database units to source/display units.
    pub fn dbu_to_display(&self, value: i32) -> f64 {
        f64::from(value) / self.display_unit as f64
    }

    /// Convert an Argon source/display coordinate to an integer GDS coordinate.
    pub fn display_to_dbu(&self, value: f64) -> i32 {
        (value * self.display_unit as f64).round() as i32
    }

    /// Snap a source/display coordinate to the nearest configured grid point.
    pub fn snap(&self, value: f64) -> f64 {
        snap(value, self.grid_step())
    }
}

/// Snap `value` to `grid`, normalizing negative zero along the way.
pub fn snap(value: f64, grid: f64) -> f64 {
    let snapped = (value / grid).round() * grid;

    // Multiplying by a decimal grid can expose floating-point noise (for
    // example, 3 * 0.1). Round once more at the grid's decimal precision so
    // source rewrites remain clean and stable.
    let mut scaled_grid = grid.abs();
    let mut decimal_places = 0;
    while decimal_places < 12
        && (scaled_grid - scaled_grid.round()).abs() > f64::EPSILON * scaled_grid.abs().max(1.) * 4.
    {
        scaled_grid *= 10.;
        decimal_places += 1;
    }
    let decimal_scale = 10_f64.powi(decimal_places);
    if snapped.is_finite() && (snapped * decimal_scale).is_finite() {
        (snapped * decimal_scale).round() / decimal_scale + 0.0
    } else {
        snapped + 0.0
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TechnologyFile {
    dbu: UnitValue,
    #[serde(alias = "display-unit")]
    display_unit: u64,
    #[serde(alias = "grid_size", alias = "grid-size", alias = "snap_grid")]
    grid: u64,
    layers: LayerList,
    #[serde(default, alias = "pin-layers")]
    pin_layers: IndexMap<String, PinLayerValue>,
    #[serde(default)]
    pins: Vec<PinMappingFile>,
}

/// Accept either an SI scale (`1e-9`) or a familiar unit name (`"nm"`).
#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum UnitValue {
    Scale(f64),
    Name(String),
}

impl UnitValue {
    fn scale(self, field: &str) -> Result<f64> {
        let scale = match self {
            Self::Scale(scale) => scale,
            Self::Name(name) => match name.trim().to_ascii_lowercase().as_str() {
                "m" | "meter" | "meters" => 1.,
                "mm" | "millimeter" | "millimeters" => 1e-3,
                "um" | "micron" | "microns" | "micrometer" | "micrometers" => 1e-6,
                "nm" | "nanometer" | "nanometers" => 1e-9,
                "pm" | "picometer" | "picometers" => 1e-12,
                _ => bail!("unknown {field} `{name}`"),
            },
        };
        if !scale.is_finite() || scale <= 0. {
            bail!("{field} must be finite and greater than zero");
        }
        Ok(scale)
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum LayerList {
    List(Vec<LayerFile>),
    Map(IndexMap<String, NamedLayerFile>),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LayerFile {
    name: String,
    #[serde(alias = "source")]
    gds: [i16; 2],
    #[serde(alias = "fill_color", alias = "fill-color")]
    fill: String,
    #[serde(alias = "border_color", alias = "border-color", alias = "frame")]
    border: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct NamedLayerFile {
    #[serde(alias = "source")]
    gds: [i16; 2],
    #[serde(alias = "fill_color", alias = "fill-color")]
    fill: String,
    #[serde(alias = "border_color", alias = "border-color", alias = "frame")]
    border: String,
}

impl LayerList {
    fn into_layers(self) -> Result<Vec<Layer>> {
        match self {
            Self::List(layers) => layers
                .into_iter()
                .map(|layer| make_layer(layer.name, layer.gds, layer.fill, layer.border))
                .collect(),
            Self::Map(layers) => layers
                .into_iter()
                .map(|(name, layer)| make_layer(name, layer.gds, layer.fill, layer.border))
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum PinLayerValue {
    Label(String),
    Mapping {
        #[serde(alias = "label_layer", alias = "label-layer")]
        label: String,
    },
}

impl PinLayerValue {
    fn into_label(self) -> String {
        match self {
            Self::Label(label) | Self::Mapping { label } => label,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PinMappingFile {
    #[serde(alias = "pin_layer", alias = "pin-layer")]
    pin: String,
    #[serde(alias = "label_layer", alias = "label-layer")]
    label: String,
}

fn make_layer(name: String, gds: [i16; 2], fill: String, border: String) -> Result<Layer> {
    if name.is_empty() {
        bail!("layer names cannot be empty");
    }
    Ok(Layer {
        name,
        gds_layer: gds[0],
        gds_datatype: gds[1],
        fill_color: parse_color(&fill)?,
        border_color: parse_color(&border)?,
    })
}

fn parse_color(color: &str) -> Result<Rgb<u8>> {
    let hex = color
        .strip_prefix('#')
        .ok_or_else(|| anyhow!("color `{color}` must have the form #RRGGBB"))?;
    if hex.len() != 6 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("color `{color}` must have the form #RRGGBB");
    }
    Ok(Rgb::new(
        u8::from_str_radix(&hex[0..2], 16)?,
        u8::from_str_radix(&hex[2..4], 16)?,
        u8::from_str_radix(&hex[4..6], 16)?,
    ))
}

/// Parse an Argon TOML technology file from a string.
pub fn parse_tech(text: &str) -> Result<Technology> {
    let file: TechnologyFile = toml::from_str(text)
        .map_err(|error| anyhow!("could not parse technology file: {error}"))?;
    let dbu = file.dbu.scale("dbu")?;
    if file.display_unit == 0 {
        bail!("display_unit must be greater than zero DBUs");
    }
    if file.grid == 0 {
        bail!("grid must be greater than zero DBUs");
    }
    let layers = file.layers.into_layers()?;
    if layers.is_empty() {
        bail!("technology file must define at least one layer");
    }
    let mut layer_names = IndexSet::new();
    for layer in &layers {
        if !layer_names.insert(layer.name.as_str()) {
            bail!("duplicate layer name `{}`", layer.name);
        }
    }

    let mut pin_layers = file
        .pin_layers
        .into_iter()
        .map(|(pin, label)| (pin, label.into_label()))
        .collect::<IndexMap<_, _>>();
    for mapping in file.pins {
        if pin_layers
            .insert(mapping.pin.clone(), mapping.label)
            .is_some()
        {
            bail!("duplicate pin-layer mapping for `{}`", mapping.pin);
        }
    }
    for (pin, label) in &pin_layers {
        if !layer_names.contains(pin.as_str()) {
            bail!("pin-layer mapping refers to undefined layer `{pin}`");
        }
        if !layer_names.contains(label.as_str()) {
            bail!("pin-layer mapping refers to undefined label layer `{label}`");
        }
    }

    Ok(Technology {
        dbu,
        display_unit: file.display_unit,
        grid: file.grid,
        layers,
        pin_layers,
    })
}

/// Load and validate an Argon TOML technology file.
pub fn read_tech(path: impl AsRef<Path>) -> Result<Technology> {
    let path = path.as_ref();
    let text = fs::read_to_string(path)
        .with_context(|| format!("could not read technology file `{}`", path.display()))?;
    parse_tech(&text).map_err(|error| {
        anyhow!(
            "could not parse technology file `{}`: {error}",
            path.display()
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use approx::assert_relative_eq;

    const TECH: &str = r##"
dbu = 1e-9
display_unit = 1000
grid = 250

[[layers]]
name = "met1.pin"
gds = [68, 16]
fill = "#112233"
border = "#445566"

[[layers]]
name = "met1.label"
gds = [68, 5]
fill = "#112233"
border = "#445566"

[pin_layers]
"met1.pin" = "met1.label"
"##;

    #[test]
    fn parses_units_layers_and_pin_mappings() {
        let tech = parse_tech(TECH).unwrap();
        assert_eq!(tech.layers.len(), 2);
        assert_eq!(tech.layers[0].gds_layer, 68);
        assert_eq!(tech.layers[0].fill_color, Rgb::new(0x11, 0x22, 0x33));
        assert_eq!(tech.pin_layers["met1.pin"], "met1.label");
        assert_eq!(tech.display_unit, 1000);
    }

    #[test]
    fn transforms_between_display_and_database_units() {
        let tech = parse_tech(TECH).unwrap();
        assert_relative_eq!(tech.dbu_to_display(1250), 1.25, epsilon = 1e-12);
        assert_eq!(tech.display_to_dbu(2.5), 2500);
        assert_relative_eq!(tech.grid_step(), 0.25, epsilon = 1e-12);
    }

    #[test]
    fn snaps_to_configured_grid() {
        let tech = parse_tech(TECH).unwrap();
        assert_relative_eq!(tech.snap(1.13), 1.25, epsilon = 1e-12);
        assert_eq!(tech.snap(-0.01).to_bits(), 0_f64.to_bits());
        assert_eq!(snap(0.26, 0.1), 0.3);
    }

    #[test]
    fn rejects_invalid_references_and_scales() {
        assert!(parse_tech(&TECH.replace("1e-9", "0.")).is_err());
        assert!(parse_tech(&TECH.replace("display_unit = 1000", "display_unit = 0")).is_err());
        assert!(parse_tech(&TECH.replace("grid = 250", "grid = 0")).is_err());
        assert!(parse_tech(&TECH.replace("grid = 250", "entry_unit = 1\ngrid = 250")).is_err());
        assert!(
            parse_tech(&TECH.replace(
                "\"met1.pin\" = \"met1.label\"",
                "\"met1.pin\" = \"missing\""
            ))
            .is_err()
        );
    }
}
