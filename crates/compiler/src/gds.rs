use std::{ops::Deref, path::Path};

use ::gds::{
    GdsArrayRef, GdsBoundary, GdsElement, GdsLayerSpec, GdsLibrary, GdsPoint, GdsStrans, GdsStruct,
    GdsStructRef, GdsTextElem, GdsUnits,
};
use anyhow::{Context, Result, anyhow, bail};
use arcstr::ArcStr;
use indexmap::IndexMap;
use tracing::trace;
use uniquify::Names;

use crate::{
    compile::{CellId, CompileOutput, CompiledData, ExecErrorCompileOutput, SolvedValue},
    tech::{Technology, read_tech},
};

pub struct GdsMap {
    layers: IndexMap<String, GdsLayerSpec>,
}

struct GdsExporter {
    lib: GdsLibrary,
    map: GdsMap,
    names: Names<CellId>,
    entry_unit: f64,
}

impl GdsExporter {
    fn new(name: impl Into<ArcStr>, tech: &Technology) -> Self {
        let mut lib = GdsLibrary::new(name);
        // The first GDS unit is the DBU size in display (user) units; the
        // second is the DBU size in meters.
        lib.units = GdsUnits::new(tech.dbu / tech.display_unit, tech.dbu);
        Self {
            lib,
            map: GdsMap::from_technology(tech),
            names: Names::new(),
            entry_unit: tech.entry_unit,
        }
    }

    fn coord_to_gds(&self, coord: f64) -> i32 {
        (coord * self.entry_unit / self.lib.units.db_unit()).round() as i32
    }
}

impl FromIterator<(String, GdsLayerSpec)> for GdsMap {
    fn from_iter<T: IntoIterator<Item = (String, GdsLayerSpec)>>(iter: T) -> Self {
        Self {
            layers: IndexMap::from_iter(iter),
        }
    }
}

impl Deref for GdsMap {
    type Target = IndexMap<String, GdsLayerSpec>;

    fn deref(&self) -> &Self::Target {
        &self.layers
    }
}

impl GdsMap {
    pub fn from_tech(path: impl AsRef<Path>) -> Result<Self> {
        Ok(Self::from_technology(&read_tech(path)?))
    }

    pub fn from_technology(tech: &Technology) -> Self {
        GdsMap::from_iter(tech.layers.iter().map(|layer| {
            (
                layer.name.clone(),
                GdsLayerSpec {
                    layer: layer.gds_layer,
                    xtype: layer.gds_datatype,
                },
            )
        }))
    }

    fn layer_name(&self, layer: i16, datatype: i16) -> Option<&str> {
        self.layers.iter().find_map(|(name, spec)| {
            (spec.layer == layer && spec.xtype == datatype).then_some(name.as_str())
        })
    }
}

#[derive(Debug)]
pub(crate) struct ImportedGdsLibrary {
    pub(crate) structs: Vec<ImportedGdsStruct>,
    pub(crate) top: usize,
}

#[derive(Debug)]
pub(crate) struct ImportedGdsStruct {
    pub(crate) name: String,
    pub(crate) elements: Vec<ImportedGdsElement>,
}

#[derive(Debug)]
pub(crate) enum ImportedGdsElement {
    Rect {
        layer: String,
        name: Option<String>,
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
    },
    Polygon {
        layer: String,
        name: Option<String>,
        points: Vec<(f64, f64)>,
    },
    Text {
        layer: String,
        text: String,
        x: f64,
        y: f64,
    },
    Instance {
        cell: usize,
        x: f64,
        y: f64,
        angle: f64,
        reflect: bool,
    },
}

pub(crate) fn import_gds(
    path: &Path,
    declared_name: &str,
    tech_path: &Path,
) -> Result<ImportedGdsLibrary> {
    let tech = read_tech(tech_path)
        .with_context(|| format!("could not map layers for GDS `{}`", path.display()))?;
    import_gds_with_tech(path, declared_name, &tech)
}

fn import_gds_with_tech(
    path: &Path,
    declared_name: &str,
    tech: &Technology,
) -> Result<ImportedGdsLibrary> {
    let map = GdsMap::from_technology(tech);
    let library = GdsLibrary::load(path)
        .map_err(|error| anyhow!("{error}"))
        .with_context(|| format!("could not read imported GDS `{}`", path.display()))?;
    let names = library
        .structs
        .iter()
        .enumerate()
        .map(|(index, structure)| (structure.name.to_string(), index))
        .collect::<IndexMap<_, _>>();
    let mut referenced = std::collections::HashSet::new();
    for structure in &library.structs {
        for element in &structure.elems {
            match element {
                GdsElement::GdsStructRef(reference) => {
                    referenced.insert(reference.name.to_string());
                }
                GdsElement::GdsArrayRef(reference) => {
                    referenced.insert(reference.name.to_string());
                }
                _ => {}
            }
        }
    }
    let top_candidates = library
        .structs
        .iter()
        .enumerate()
        .filter(|(_, structure)| !referenced.contains(structure.name.as_str()))
        .map(|(index, _)| index)
        .collect::<Vec<_>>();
    let top = (top_candidates.len() == 1)
        .then_some(top_candidates[0])
        .or_else(|| names.get(declared_name).copied())
        .ok_or_else(|| {
            anyhow!(
                "imported GDS `{}` has no unique top structure and no structure named `{declared_name}`",
                path.display()
            )
        })?;
    // GDS coordinates are integer DBUs. Argon source and compiled geometry
    // use entry units, so imported coordinates must account for both scales.
    let scale = library.units.db_unit() / tech.entry_unit;
    let coord = |value: i32| f64::from(value) * scale;
    let mut structs = Vec::with_capacity(library.structs.len());
    for structure in &library.structs {
        let mut elements = Vec::new();
        for element in &structure.elems {
            match element {
                GdsElement::GdsBoundary(boundary) => {
                    elements.push(import_boundary(
                        &map,
                        path,
                        boundary.layer,
                        boundary.datatype,
                        &boundary.xy,
                        scale,
                    )?);
                }
                GdsElement::GdsBox(gds_box) => {
                    elements.push(import_boundary(
                        &map,
                        path,
                        gds_box.layer,
                        gds_box.boxtype,
                        &gds_box.xy,
                        scale,
                    )?);
                }
                GdsElement::GdsTextElem(text) => {
                    // Text transforms affect glyph presentation, not the label's
                    // anchor. Argon retains text as an annotation at that anchor.
                    let layer = import_layer(&map, path, text.layer, text.texttype)?;
                    elements.push(ImportedGdsElement::Text {
                        layer,
                        text: text.string.to_string(),
                        x: coord(text.xy.x),
                        y: coord(text.xy.y),
                    });
                }
                GdsElement::GdsStructRef(reference) => elements.push(import_reference(
                    path,
                    &names,
                    &reference.name,
                    reference.xy.x,
                    reference.xy.y,
                    reference.strans.as_ref(),
                    scale,
                )?),
                GdsElement::GdsArrayRef(array) => {
                    import_array(path, &names, array, scale, &mut elements)?;
                }
                GdsElement::GdsPath(_) => bail!(
                    "imported GDS `{}` contains a path element; only boundaries, boxes, text, and cell references are supported",
                    path.display()
                ),
                GdsElement::GdsNode(_) => bail!(
                    "imported GDS `{}` contains a node element, which is not supported",
                    path.display()
                ),
            }
        }
        name_pin_shapes(&mut elements, &tech.pin_layers);
        structs.push(ImportedGdsStruct {
            name: structure.name.to_string(),
            elements,
        });
    }
    Ok(ImportedGdsLibrary { structs, top })
}

fn import_layer(map: &GdsMap, path: &Path, layer: i16, datatype: i16) -> Result<String> {
    map.layer_name(layer, datatype)
        .map(str::to_owned)
        .ok_or_else(|| {
            anyhow!(
                "imported GDS `{}` uses layer {layer}/{datatype}, which is absent from the technology file",
                path.display()
            )
        })
}

fn import_boundary(
    map: &GdsMap,
    path: &Path,
    layer: i16,
    datatype: i16,
    points: &[GdsPoint],
    scale: f64,
) -> Result<ImportedGdsElement> {
    let Some(x0) = points.iter().map(|point| point.x).min() else {
        bail!(
            "imported GDS `{}` contains an empty boundary",
            path.display()
        );
    };
    let x1 = points.iter().map(|point| point.x).max().unwrap();
    let y0 = points.iter().map(|point| point.y).min().unwrap();
    let y1 = points.iter().map(|point| point.y).max().unwrap();
    let layer = import_layer(map, path, layer, datatype)?;
    if x0 != x1
        && y0 != y1
        && points
            .iter()
            .all(|point| [x0, x1].contains(&point.x) && [y0, y1].contains(&point.y))
    {
        return Ok(ImportedGdsElement::Rect {
            layer,
            name: None,
            x0: f64::from(x0) * scale,
            y0: f64::from(y0) * scale,
            x1: f64::from(x1) * scale,
            y1: f64::from(y1) * scale,
        });
    }

    let mut polygon_points = points
        .iter()
        .map(|point| (f64::from(point.x) * scale, f64::from(point.y) * scale))
        .collect::<Vec<_>>();
    if polygon_points.first() == polygon_points.last() {
        polygon_points.pop();
    }
    if polygon_points.len() < 3 {
        bail!(
            "imported GDS `{}` contains a boundary with fewer than three points",
            path.display()
        );
    }
    Ok(ImportedGdsElement::Polygon {
        layer,
        name: None,
        points: polygon_points,
    })
}

/// Associate label text with pin shapes using the technology's explicit
/// pin-layer mappings.
fn name_pin_shapes(elements: &mut [ImportedGdsElement], pin_layers: &IndexMap<String, String>) {
    let labels = elements
        .iter()
        .filter_map(|element| match element {
            ImportedGdsElement::Text { layer, text, x, y } => {
                Some((layer.clone(), text.clone(), *x, *y))
            }
            _ => None,
        })
        .collect::<Vec<_>>();

    for element in elements {
        match element {
            ImportedGdsElement::Rect {
                layer,
                name,
                x0,
                y0,
                x1,
                y1,
            } => {
                let Some(label_layer) = pin_layers.get(layer) else {
                    continue;
                };
                *name = labels.iter().find_map(|(layer, text, x, y)| {
                    (layer == label_layer && *x >= *x0 && *x <= *x1 && *y >= *y0 && *y <= *y1)
                        .then(|| argon_ident(text))
                });
            }
            ImportedGdsElement::Polygon {
                layer,
                name,
                points,
            } => {
                let Some(label_layer) = pin_layers.get(layer) else {
                    continue;
                };
                *name = labels.iter().find_map(|(layer, text, x, y)| {
                    (layer == label_layer && point_in_polygon((*x, *y), points))
                        .then(|| argon_ident(text))
                });
            }
            _ => {}
        }
    }
}

fn point_in_polygon(point: (f64, f64), polygon: &[(f64, f64)]) -> bool {
    let mut inside = false;
    for ((x0, y0), (x1, y1)) in polygon
        .iter()
        .copied()
        .zip(polygon.iter().copied().cycle().skip(1))
        .take(polygon.len())
    {
        if (y0 > point.1) != (y1 > point.1) && point.0 < (x1 - x0) * (point.1 - y0) / (y1 - y0) + x0
        {
            inside = !inside;
        }
    }
    inside
}

fn argon_ident(label: &str) -> String {
    let mut ident = String::with_capacity(label.len() + 1);
    for ch in label.chars() {
        if ch == '_' || ch.is_ascii_alphanumeric() {
            ident.push(ch);
        } else {
            ident.push('_');
        }
    }
    if ident.is_empty() || ident.as_bytes()[0].is_ascii_digit() {
        ident.insert(0, '_');
    }
    if matches!(
        ident.as_str(),
        "x" | "y"
            | "fn"
            | "if"
            | "as"
            | "in"
            | "let"
            | "for"
            | "mod"
            | "use"
            | "enum"
            | "cell"
            | "true"
            | "else"
            | "match"
            | "const"
            | "false"
            | "struct"
    ) {
        ident.push('_');
    }
    ident
}

fn import_reference(
    path: &Path,
    names: &IndexMap<String, usize>,
    name: &str,
    x: i32,
    y: i32,
    transform: Option<&GdsStrans>,
    scale: f64,
) -> Result<ImportedGdsElement> {
    let cell = names.get(name).copied().ok_or_else(|| {
        anyhow!(
            "imported GDS `{}` references missing structure `{name}`",
            path.display()
        )
    })?;
    let (angle, reflect) = import_transform(path, transform)?;
    Ok(ImportedGdsElement::Instance {
        cell,
        x: f64::from(x) * scale,
        y: f64::from(y) * scale,
        angle,
        reflect,
    })
}

fn import_array(
    path: &Path,
    names: &IndexMap<String, usize>,
    array: &GdsArrayRef,
    scale: f64,
    elements: &mut Vec<ImportedGdsElement>,
) -> Result<()> {
    if array.cols <= 0 || array.rows <= 0 {
        bail!("imported GDS `{}` contains an empty array", path.display());
    }
    let dx = (
        f64::from(array.xy[1].x - array.xy[0].x) / f64::from(array.cols),
        f64::from(array.xy[1].y - array.xy[0].y) / f64::from(array.cols),
    );
    let dy = (
        f64::from(array.xy[2].x - array.xy[0].x) / f64::from(array.rows),
        f64::from(array.xy[2].y - array.xy[0].y) / f64::from(array.rows),
    );
    for row in 0..array.rows {
        for col in 0..array.cols {
            let x = f64::from(array.xy[0].x) + f64::from(col) * dx.0 + f64::from(row) * dy.0;
            let y = f64::from(array.xy[0].y) + f64::from(col) * dx.1 + f64::from(row) * dy.1;
            let cell = names.get(array.name.as_str()).copied().ok_or_else(|| {
                anyhow!(
                    "imported GDS `{}` references missing structure `{}`",
                    path.display(),
                    array.name
                )
            })?;
            let (angle, reflect) = import_transform(path, array.strans.as_ref())?;
            elements.push(ImportedGdsElement::Instance {
                cell,
                x: x * scale,
                y: y * scale,
                angle,
                reflect,
            });
        }
    }
    Ok(())
}

fn import_transform(path: &Path, transform: Option<&GdsStrans>) -> Result<(f64, bool)> {
    let Some(transform) = transform else {
        return Ok((0., false));
    };
    if transform.mag.is_some_and(|mag| (mag - 1.).abs() > 1e-9) {
        bail!(
            "imported GDS `{}` contains a magnified cell reference, which is not supported",
            path.display()
        );
    }
    let angle = transform.angle.unwrap_or(0.).rem_euclid(360.);
    if ![0., 90., 180., 270.]
        .iter()
        .any(|candidate| f64::abs(angle - candidate) < 1e-9)
    {
        bail!(
            "imported GDS `{}` contains a non-Manhattan cell reference, which is not supported",
            path.display()
        );
    }
    Ok((angle, transform.reflected))
}

impl CompileOutput {
    pub fn to_gds(&self, out_path: impl AsRef<Path>) -> Result<()> {
        let out_path = out_path.as_ref();
        trace!("Exporting to gds at {out_path:?}");
        if let CompileOutput::Valid(output)
        | CompileOutput::ExecErrors(ExecErrorCompileOutput {
            errors: _,
            output: Some(output),
        }) = self
        {
            let mut exporter = GdsExporter::new("TOP", &output.tech);
            output.cell_to_gds(&mut exporter, output.top)?;
            if let Some(parent) = out_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            exporter.lib.save(out_path).map_err(|e| anyhow!("{e}"))?;
        }

        Ok(())
    }
}

impl CompiledData {
    fn cell_to_gds(&self, exporter: &mut GdsExporter, id: CellId) -> Result<()> {
        trace!("Exporting cell {id}");
        let cell = &self.cells[&id];
        let name = &cell.scopes[&cell.root].name;
        let name = parse_cell_name(name)?;
        let name = exporter.names.assign_name(id, name);
        let mut ocell = GdsStruct::new(name.to_string());
        for (_, obj) in &cell.objects {
            match obj {
                SolvedValue::Rect(rect) if !rect.construction => {
                    if let Some(layer) = &rect.layer {
                        let GdsLayerSpec {
                            layer,
                            xtype: datatype,
                        } = exporter.map[layer];
                        let x0 = exporter.coord_to_gds(rect.x0.0);
                        let x1 = exporter.coord_to_gds(rect.x1.0);
                        let y0 = exporter.coord_to_gds(rect.y0.0);
                        let y1 = exporter.coord_to_gds(rect.y1.0);
                        ocell.elems.push(GdsElement::GdsBoundary(GdsBoundary {
                            layer,
                            datatype,
                            xy: vec![
                                GdsPoint::new(x0, y0),
                                GdsPoint::new(x0, y1),
                                GdsPoint::new(x1, y1),
                                GdsPoint::new(x1, y0),
                                GdsPoint::new(x0, y0),
                            ],
                            ..Default::default()
                        }));
                    }
                }
                SolvedValue::Polygon(polygon) => {
                    let GdsLayerSpec {
                        layer,
                        xtype: datatype,
                    } = exporter.map[&polygon.layer];
                    let mut points = polygon
                        .points
                        .iter()
                        .map(|(x, y)| {
                            GdsPoint::new(exporter.coord_to_gds(x.0), exporter.coord_to_gds(y.0))
                        })
                        .collect::<Vec<_>>();
                    points.push(points[0].clone());
                    ocell.elems.push(GdsElement::GdsBoundary(GdsBoundary {
                        layer,
                        datatype,
                        xy: points,
                        ..Default::default()
                    }));
                }
                SolvedValue::Text(text) => {
                    let GdsLayerSpec {
                        layer,
                        xtype: texttype,
                    } = exporter.map[&text.layer];
                    let x = exporter.coord_to_gds(text.x);
                    let y = exporter.coord_to_gds(text.y);
                    ocell.elems.push(GdsElement::GdsTextElem(GdsTextElem {
                        string: ArcStr::from(&text.text),
                        layer,
                        texttype,
                        xy: GdsPoint::new(x, y),
                        ..Default::default()
                    }));
                }
                SolvedValue::Instance(i) if !i.construction => {
                    if exporter.names.name(&i.cell).is_none() {
                        self.cell_to_gds(exporter, i.cell)?;
                    }
                    ocell.elems.push(GdsElement::GdsStructRef(GdsStructRef {
                        name: exporter.names.name(&i.cell).unwrap().clone(),
                        xy: GdsPoint::new(exporter.coord_to_gds(i.x), exporter.coord_to_gds(i.y)),
                        strans: Some(GdsStrans {
                            reflected: i.reflect,
                            abs_mag: false,
                            abs_angle: false,
                            mag: None,
                            angle: Some(i.angle.degrees()),
                        }),
                        ..Default::default()
                    }));
                }
                _ => {}
            }
        }
        exporter.lib.structs.push(ocell);
        Ok(())
    }
}

fn parse_cell_name(name: &str) -> Result<&str> {
    name.rsplit("cell ")
        .next()
        .and_then(|suffix| suffix.split_whitespace().next())
        .ok_or_else(|| anyhow!("parse error"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_rectangular_boundary_imports_as_polygon() {
        let map = GdsMap::from_iter([("met1".to_owned(), GdsLayerSpec { layer: 1, xtype: 0 })]);
        let element = import_boundary(
            &map,
            Path::new("polygon.gds"),
            1,
            0,
            &[
                GdsPoint::new(0, 0),
                GdsPoint::new(100, 0),
                GdsPoint::new(50, 75),
                GdsPoint::new(0, 0),
            ],
            1.,
        )
        .expect("polygon boundary should import");

        let ImportedGdsElement::Polygon { layer, points, .. } = element else {
            panic!("non-rectangular boundary should remain a polygon");
        };
        assert_eq!(layer, "met1");
        assert_eq!(points, [(0., 0.), (100., 0.), (50., 75.)]);
    }

    #[test]
    fn pin_shape_uses_contained_matching_layer_label() {
        let mut elements = vec![
            ImportedGdsElement::Rect {
                layer: "ports".to_owned(),
                name: None,
                x0: 0.,
                y0: 0.,
                x1: 100.,
                y1: 100.,
            },
            ImportedGdsElement::Text {
                layer: "port_names".to_owned(),
                text: "VDD".to_owned(),
                x: 50.,
                y: 50.,
            },
            ImportedGdsElement::Text {
                layer: "met2.label".to_owned(),
                text: "wrong_layer".to_owned(),
                x: 50.,
                y: 50.,
            },
        ];

        name_pin_shapes(
            &mut elements,
            &IndexMap::from_iter([("ports".to_owned(), "port_names".to_owned())]),
        );

        let ImportedGdsElement::Rect { name, .. } = &elements[0] else {
            unreachable!();
        };
        assert_eq!(name.as_deref(), Some("VDD"));
    }

    #[test]
    fn exporter_transforms_entry_units_to_configured_dbus() {
        let tech = crate::tech::parse_tech(
            r##"
                dbu = "nm"
                display_unit = "um"
                entry_unit = "um"
                grid = 0.001

                [[layers]]
                name = "met1"
                gds = [1, 0]
                fill = "#0000ff"
                border = "#0000ff"
            "##,
        )
        .unwrap();
        let exporter = GdsExporter::new("test", &tech);
        assert_eq!(exporter.coord_to_gds(1.25), 1250);
        assert_eq!(exporter.lib.units.db_unit(), 1e-9);
    }

    #[test]
    fn importer_transforms_gds_dbus_to_entry_units() {
        let tech = crate::tech::parse_tech(
            r##"
                dbu = "nm"
                display_unit = "um"
                entry_unit = "um"
                grid = 0.001

                [[layers]]
                name = "met1"
                gds = [1, 0]
                fill = "#0000ff"
                border = "#0000ff"
            "##,
        )
        .unwrap();
        let mut library = GdsLibrary::new("fixture");
        let mut structure = GdsStruct::new("top");
        structure.elems.push(GdsElement::GdsBoundary(GdsBoundary {
            layer: 1,
            datatype: 0,
            xy: vec![
                GdsPoint::new(0, 0),
                GdsPoint::new(0, 2_000),
                GdsPoint::new(1_000, 2_000),
                GdsPoint::new(1_000, 0),
                GdsPoint::new(0, 0),
            ],
            ..Default::default()
        }));
        library.structs.push(structure);
        let path = std::env::temp_dir().join(format!(
            "argon-dbu-transform-{}-{}.gds",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        library.save(&path).unwrap();

        let imported = import_gds_with_tech(&path, "top", &tech).unwrap();
        let ImportedGdsElement::Rect { x1, y1, .. } = imported.structs[0].elements[0] else {
            panic!("fixture boundary should import as a rectangle");
        };
        assert_eq!(x1, 1.);
        assert_eq!(y1, 2.);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pin_label_is_made_into_an_argon_identifier() {
        assert_eq!(argon_ident("data out[0]"), "data_out_0_");
        assert_eq!(argon_ident("1V8"), "_1V8");
        assert_eq!(argon_ident("in"), "in_");
        assert_eq!(argon_ident("x"), "x_");
    }
}
