//! Argon technology-file parsing and physical-unit conversions.

use std::{fs, path::Path};

use anyhow::{Context, Result, anyhow, bail};
use indexmap::{IndexMap, IndexSet};
use rgb::Rgb;
use serde::{Deserialize, Serialize};

/// Validated, normalized technology information used by the compiler, GDS
/// importer/exporter, solver, analyzer, and layout editor.
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
    /// Optional name of the imported or authored layer-style collection.
    pub style_name: Option<String>,
    pub layers: Vec<Layer>,
    pub custom_dither_patterns: Vec<CustomDitherPattern>,
    pub custom_line_styles: Vec<CustomLineStyle>,
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
    pub style: LayerStyle,
}

/// The complete set of per-layer presentation properties carried by KLayout
/// layer-properties files. Argon retains fields that its renderer does not yet
/// implement so adding more patterns and line styles does not require another
/// technology-file migration.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default, deny_unknown_fields)]
pub struct LayerStyle {
    /// Whether a group node starts expanded. Retained for compatibility even
    /// though Argon's current layer list is flat.
    pub expanded: bool,
    /// Frame-color brightness adjustment: -100 is black, 0 leaves the color
    /// unchanged, and 100 is white.
    pub frame_brightness: i32,
    /// Fill-color brightness adjustment, with the same scale as
    /// [`LayerStyle::frame_brightness`].
    pub fill_brightness: i32,
    /// KLayout fill-pattern reference. `I0` is solid, `I1` is clear, other
    /// `Ix` values are built-ins, and `Cx` selects a custom dither pattern.
    pub dither_pattern: String,
    /// KLayout frame-line reference. Empty or `I0` is solid, other `Ix`
    /// values are built-ins, and `Cx` selects a custom line style.
    pub line_style: String,
    /// Invalid layers are displayed but their shapes are not selectable.
    pub valid: bool,
    /// Initial visibility in the layer list.
    pub visible: bool,
    /// Use KLayout's background-dependent transparent color composition.
    pub transparent: bool,
    /// Frame width in screen pixels.
    pub width: i32,
    /// Draw the layer with small cross markers.
    pub marked: bool,
    /// Draw a diagonal X through boxes on this layer.
    pub xfill: bool,
    /// KLayout animation mode: 0 none, 1 scrolling, 2 blinking, or 3 inverse
    /// blinking.
    pub animation: i32,
}

impl Default for LayerStyle {
    fn default() -> Self {
        Self {
            expanded: false,
            frame_brightness: 0,
            fill_brightness: 0,
            dither_pattern: "C7".to_owned(),
            line_style: "C0".to_owned(),
            valid: true,
            visible: true,
            transparent: false,
            width: 1,
            marked: false,
            xfill: false,
            animation: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CustomDitherPattern {
    /// Bitmap rows where `*` is set and `.` is clear. KLayout supports row
    /// widths of 8, 16, or 32 pixels.
    pub lines: Vec<String>,
    /// Position in the pattern chooser; references use array position, not
    /// this display order.
    pub order: i32,
    /// Human-readable pattern name.
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct CustomLineStyle {
    /// Repeating line bitmap where `*` is drawn and `.` is skipped.
    pub pattern: String,
    /// Position in the line-style chooser.
    pub order: i32,
    /// Human-readable style name.
    pub name: String,
}

/// Independently comparable parts of a normalized technology. Incremental
/// compilation uses these values instead of the technology file's revision so
/// an unrelated edit (for example, changing a fill color) does not invalidate
/// solved geometry. Presentation fields are intentionally not a cache key;
/// the current [`Technology`] is attached to every returned output instead.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct TechnologyFingerprints {
    pub(crate) solver: SolverTechnologyFingerprint,
    pub(crate) layer_validation: LayerValidationTechnologyFingerprint,
    pub(crate) gds_import: GdsImportTechnologyFingerprint,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct SolverTechnologyFingerprint(u64);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct LayerValidationTechnologyFingerprint(Vec<String>);

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct GdsImportTechnologyFingerprint(Vec<u8>);

impl Technology {
    pub(crate) fn fingerprints(&self) -> TechnologyFingerprints {
        let mut layer_names = self
            .layers
            .iter()
            .map(|layer| layer.name.clone())
            .collect::<Vec<_>>();
        layer_names.sort();
        let mut pin_layers = self.pin_layers.iter().collect::<Vec<_>>();
        pin_layers.sort_by_key(|(name, _)| *name);

        let gds_import = bincode::serialize(&(
            self.dbu.to_bits(),
            self.display_unit,
            self.layers
                .iter()
                .map(|layer| (&layer.name, layer.gds_layer, layer.gds_datatype))
                .collect::<Vec<_>>(),
            pin_layers,
        ))
        .expect("normalized GDS technology inputs should always serialize");
        TechnologyFingerprints {
            solver: SolverTechnologyFingerprint(self.grid_step().to_bits()),
            layer_validation: LayerValidationTechnologyFingerprint(layer_names),
            gds_import: GdsImportTechnologyFingerprint(gds_import),
        }
    }

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

/// TOML-facing representation used only during deserialization.
///
/// This accepts conveniences such as named DBU units, alternate pin mapping
/// forms, and legacy field aliases. [`parse_tech`] converts it into the single
/// validated [`Technology`] representation used by the rest of Argon.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParsedTechnology {
    dbu: UnitValue,
    #[serde(alias = "display-unit")]
    display_unit: u64,
    #[serde(alias = "grid_size", alias = "grid-size", alias = "snap_grid")]
    grid: u64,
    #[serde(default)]
    style_name: Option<String>,
    layers: Vec<ParsedLayer>,
    #[serde(default)]
    custom_dither_patterns: Vec<CustomDitherPattern>,
    #[serde(default)]
    custom_line_styles: Vec<CustomLineStyle>,
    #[serde(default, alias = "pin-layers")]
    pin_layers: IndexMap<String, ParsedPinLayer>,
    #[serde(default)]
    pins: Vec<ParsedPinMapping>,
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

/// Serialized layer fields that need normalization before runtime use.
///
/// The techfile combines the GDS layer/datatype in one pair and represents
/// colors as hex strings. [`Layer`] keeps those as separate numeric fields and
/// parsed RGB values.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParsedLayer {
    name: String,
    #[serde(alias = "source")]
    gds: [i16; 2],
    #[serde(alias = "fill_color", alias = "fill-color")]
    fill: String,
    #[serde(alias = "border_color", alias = "border-color", alias = "frame")]
    border: String,
    #[serde(default)]
    style: LayerStyle,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
enum ParsedPinLayer {
    Label(String),
    Mapping {
        #[serde(alias = "label_layer", alias = "label-layer")]
        label: String,
    },
}

impl ParsedPinLayer {
    fn into_label(self) -> String {
        match self {
            Self::Label(label) | Self::Mapping { label } => label,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ParsedPinMapping {
    #[serde(alias = "pin_layer", alias = "pin-layer")]
    pin: String,
    #[serde(alias = "label_layer", alias = "label-layer")]
    label: String,
}

fn make_layer(
    name: String,
    gds: [i16; 2],
    fill: String,
    border: String,
    style: LayerStyle,
) -> Result<Layer> {
    if name.is_empty() {
        bail!("layer names cannot be empty");
    }
    Ok(Layer {
        name,
        gds_layer: gds[0],
        gds_datatype: gds[1],
        fill_color: parse_color(&fill)?,
        border_color: parse_color(&border)?,
        style,
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
    let input: ParsedTechnology = toml::from_str(text)
        .map_err(|error| anyhow!("could not parse technology file: {error}"))?;
    let dbu = input.dbu.scale("dbu")?;
    if input.display_unit == 0 {
        bail!("display_unit must be greater than zero DBUs");
    }
    if input.grid == 0 {
        bail!("grid must be greater than zero DBUs");
    }
    let layers = input
        .layers
        .into_iter()
        .map(|layer| make_layer(layer.name, layer.gds, layer.fill, layer.border, layer.style))
        .collect::<Result<Vec<_>>>()?;
    if layers.is_empty() {
        bail!("technology file must define at least one layer");
    }
    // A technology file has to map layer names and GDS specs bijectively.
    // Unique names keep export deterministic, and unique specs are what make
    // import unambiguous: `GdsMap::layer_name` reverse-maps an imported
    // layer/datatype pair by taking the first layer that declares it, so two
    // layers sharing one spec would silently relabel every imported shape with
    // whichever name was listed first, destroying layer identity with no
    // diagnostic. Rejecting the collision here is cheaper than teaching every
    // consumer of the reverse lookup to disambiguate.
    let mut layer_names = IndexSet::new();
    let mut layer_specs = IndexMap::new();
    for layer in &layers {
        if !layer_names.insert(layer.name.as_str()) {
            bail!("duplicate layer name `{}`", layer.name);
        }
        let spec = (layer.gds_layer, layer.gds_datatype);
        if let Some(previous) = layer_specs.insert(spec, layer.name.as_str()) {
            bail!(
                "duplicate GDS spec {}/{} on layers `{previous}` and `{}`",
                layer.gds_layer,
                layer.gds_datatype,
                layer.name
            );
        }
    }

    let mut pin_layers = input
        .pin_layers
        .into_iter()
        .map(|(pin, label)| (pin, label.into_label()))
        .collect::<IndexMap<_, _>>();
    for mapping in input.pins {
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
        display_unit: input.display_unit,
        grid: input.grid,
        style_name: input.style_name,
        layers,
        custom_dither_patterns: input.custom_dither_patterns,
        custom_line_styles: input.custom_line_styles,
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
style_name = "Test styles"

[[custom_dither_patterns]]
lines = ["*.", ".*"]
order = 1
name = "diagonal"

[[custom_line_styles]]
pattern = "**.."
order = 1
name = "dashed"

[[layers]]
name = "met1.pin"
gds = [68, 16]
fill = "#112233"
border = "#445566"

[layers.style]
frame_brightness = -10
fill_brightness = 20
dither_pattern = "C0"
line_style = "C0"
valid = true
visible = false
transparent = true
width = 3
marked = true
xfill = true
animation = 2

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
        assert_eq!(tech.layers[0].style.frame_brightness, -10);
        assert_eq!(tech.layers[0].style.dither_pattern, "C0");
        assert!(!tech.layers[0].style.visible);
        assert!(tech.layers[0].style.transparent);
        assert_eq!(tech.layers[1].style, LayerStyle::default());
        assert_eq!(tech.style_name.as_deref(), Some("Test styles"));
        assert_eq!(tech.custom_dither_patterns[0].lines, ["*.", ".*"]);
        assert_eq!(tech.custom_line_styles[0].pattern, "**..");
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
    fn rejects_layers_sharing_one_gds_spec() {
        // Import resolves a GDS layer/datatype pair back to the first layer
        // declaring it, so a shared spec would quietly rename shapes instead of
        // failing. The message has to name both layers for the author to know
        // which declaration to change.
        let error = parse_tech(&TECH.replace("gds = [68, 5]", "gds = [68, 16]"))
            .expect_err("layers sharing a GDS spec should be rejected")
            .to_string();
        assert!(error.contains("68/16"), "{error}");
        assert!(error.contains("met1.pin"), "{error}");
        assert!(error.contains("met1.label"), "{error}");
    }

    #[test]
    fn accepts_layers_sharing_only_a_gds_layer_number() {
        // Only the full pair has to be unique. Real technologies subdivide one
        // drawing layer into pin, label, and blockage layers by datatype, so
        // checking the layer number alone would reject valid PDKs.
        let tech = parse_tech(&TECH.replace("gds = [68, 5]", "gds = [68, 44]")).unwrap();
        assert_eq!(tech.layers[0].gds_layer, tech.layers[1].gds_layer);
        assert_eq!(tech.layers[1].gds_datatype, 44);
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
