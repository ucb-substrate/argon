use std::{io::BufReader, ops::Deref, path::Path};

use anyhow::{Result, anyhow};
use gds21::{
    GdsBoundary, GdsElement, GdsLayerSpec, GdsLibrary, GdsPoint, GdsStrans, GdsStruct, GdsStructRef,
};
use indexmap::IndexMap;
use regex::Regex;
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
    fn new(name: impl Into<String>, map: GdsMap) -> Self {
        Self {
            lib: GdsLibrary::new(name),
            map,
            names: Names::new(),
        }
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
                    let re = Regex::new(r"(\d*)/(\d*)@\d*")?;
                    let caps = re
                        .captures(&layer_prop.source)
                        .ok_or_else(|| anyhow!("parse error"))?;
                    let layer = caps
                        .get(1)
                        .ok_or_else(|| anyhow!("parse error"))?
                        .as_str()
                        .parse()?;
                    let datatype = caps
                        .get(2)
                        .ok_or_else(|| anyhow!("parse error"))?
                        .as_str()
                        .parse()?;
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
}

impl CompileOutput {
    pub fn to_gds(&self, map: GdsMap, out_path: impl AsRef<Path>) -> Result<()> {
        let out_path = out_path.as_ref();
        let mut exporter = GdsExporter::new("TOP", map);
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
        let cell = &self.cells[&id];
        let name = &cell.scopes[&cell.root].name;
        let re = Regex::new(r".*cell ([a-zA-Z0-9_]*)")?;
        let caps = re.captures(name).ok_or_else(|| anyhow!("parse error"))?;
        let name = caps.get(1).ok_or_else(|| anyhow!("parse error"))?.as_str();
        let name = exporter.names.assign_name(id, name);
        let mut ocell = GdsStruct::new(name.to_string());
        for (_, obj) in &cell.objects {
            match obj {
                SolvedValue::Rect(rect) => {
                    if rect.construction {
                        continue;
                    }
                    if let Some(layer) = &rect.layer {
                        let GdsLayerSpec {
                            layer,
                            xtype: datatype,
                        } = exporter.map[layer];
                        let x0 = rect.x0.0 as i32;
                        let x1 = rect.x1.0 as i32;
                        let y0 = rect.y0.0 as i32;
                        let y1 = rect.y1.0 as i32;
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
                SolvedValue::Instance(i) => {
                    self.cell_to_gds(exporter, i.cell)?;
                    ocell.elems.push(GdsElement::GdsStructRef(GdsStructRef {
                        name: exporter.names.name(&i.cell).unwrap().to_string(),
                        xy: GdsPoint::new(i.x as i32, i.y as i32),
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
