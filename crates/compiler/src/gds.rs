use std::{io::BufReader, ops::Deref, path::Path};

use ::gds::{
    GdsArrayRef, GdsBoundary, GdsElement, GdsLayerSpec, GdsLibrary, GdsPoint, GdsStrans, GdsStruct,
    GdsStructRef, GdsTextElem, GdsUnits,
};
use anyhow::{Context, Result, anyhow, bail};
use arcstr::ArcStr;
use indexmap::IndexMap;
use tracing::trace;
use uniquify::Names;

use crate::compile::{CellId, CompileOutput, CompiledData, ExecErrorCompileOutput, SolvedValue};

pub struct GdsMap {
    layers: IndexMap<String, GdsLayerSpec>,
}

struct GdsExporter {
    lib: GdsLibrary,
    map: GdsMap,
    names: Names<CellId>,
}

impl GdsExporter {
    fn new(name: impl Into<ArcStr>, map: GdsMap, units: GdsUnits) -> Self {
        let mut lib = GdsLibrary::new(name);
        lib.units = units;
        Self {
            lib,
            map,
            names: Names::new(),
        }
    }

    fn coord_to_gds(&self, coord: f64) -> i32 {
        (coord * 1e-9 / self.lib.units.db_unit()).round() as i32
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
    pub fn from_lyp(path: impl AsRef<Path>) -> Result<Self> {
        let lyp = klayout_lyp::from_reader(BufReader::new(std::fs::File::open(path)?))?;
        Ok(GdsMap::from_iter(
            lyp.layers
                .into_iter()
                .map(|layer_prop| {
                    let (layer, datatype) = parse_layer_source(&layer_prop.source)?;
                    Ok((
                        layer_prop.name,
                        GdsLayerSpec {
                            layer,
                            xtype: datatype,
                        },
                    ))
                })
                .collect::<Result<Vec<_>>>()?,
        ))
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
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
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
    lyp_path: &Path,
) -> Result<ImportedGdsLibrary> {
    let library = GdsLibrary::load(path)
        .map_err(|error| anyhow!("{error}"))
        .with_context(|| format!("could not read imported GDS `{}`", path.display()))?;
    let map = GdsMap::from_lyp(lyp_path)
        .with_context(|| format!("could not map layers for GDS `{}`", path.display()))?;
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
    let scale = library.units.db_unit() / 1e-9;
    let coord = |value: i32| f64::from(value) * scale;
    let mut structs = Vec::with_capacity(library.structs.len());
    for structure in &library.structs {
        let mut elements = Vec::new();
        for element in &structure.elems {
            match element {
                GdsElement::GdsBoundary(boundary) => {
                    elements.push(import_rect(
                        &map,
                        path,
                        boundary.layer,
                        boundary.datatype,
                        &boundary.xy,
                        scale,
                    )?);
                }
                GdsElement::GdsBox(gds_box) => {
                    elements.push(import_rect(
                        &map,
                        path,
                        gds_box.layer,
                        gds_box.boxtype,
                        &gds_box.xy,
                        scale,
                    )?);
                }
                GdsElement::GdsTextElem(text) => {
                    let (angle, reflect) = import_transform(path, text.strans.as_ref())?;
                    if angle != 0. || reflect {
                        bail!(
                            "imported GDS `{}` contains transformed text, which is not supported",
                            path.display()
                        );
                    }
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
                    "imported GDS `{}` contains a path element; only rectangular boundaries, boxes, text, and cell references are supported",
                    path.display()
                ),
                GdsElement::GdsNode(_) => bail!(
                    "imported GDS `{}` contains a node element, which is not supported",
                    path.display()
                ),
            }
        }
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
                "imported GDS `{}` uses layer {layer}/{datatype}, which is absent from the LYP file",
                path.display()
            )
        })
}

fn import_rect(
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
    if x0 == x1
        || y0 == y1
        || points
            .iter()
            .any(|point| ![x0, x1].contains(&point.x) || ![y0, y1].contains(&point.y))
    {
        bail!(
            "imported GDS `{}` contains a non-rectangular boundary, which is not supported",
            path.display()
        );
    }
    Ok(ImportedGdsElement::Rect {
        layer: import_layer(map, path, layer, datatype)?,
        x0: f64::from(x0) * scale,
        y0: f64::from(y0) * scale,
        x1: f64::from(x1) * scale,
        y1: f64::from(y1) * scale,
    })
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
    pub fn to_gds(&self, map: GdsMap, units: GdsUnits, out_path: impl AsRef<Path>) -> Result<()> {
        let out_path = out_path.as_ref();
        trace!("Exporting to gds at {out_path:?}");
        let mut exporter = GdsExporter::new("TOP", map, units);
        if let CompileOutput::Valid(output)
        | CompileOutput::ExecErrors(ExecErrorCompileOutput {
            errors: _,
            output: Some(output),
        }) = self
        {
            output.cell_to_gds(&mut exporter, output.top)?;
        }
        if let Some(parent) = out_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        exporter.lib.save(out_path).map_err(|e| anyhow!("{e}"))?;

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
                            ],
                            ..Default::default()
                        }));
                    }
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

fn parse_layer_source(source: &str) -> Result<(i16, i16)> {
    let (layer, datatype) = source
        .split_once('/')
        .ok_or_else(|| anyhow!("parse error"))?;
    let datatype = datatype
        .split_once('@')
        .map_or(datatype, |(datatype, _)| datatype);
    Ok((layer.parse()?, datatype.parse()?))
}

fn parse_cell_name(name: &str) -> Result<&str> {
    name.rsplit("cell ")
        .next()
        .and_then(|suffix| suffix.split_whitespace().next())
        .ok_or_else(|| anyhow!("parse error"))
}
