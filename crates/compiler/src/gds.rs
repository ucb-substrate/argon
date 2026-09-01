use std::{ops::Deref, path::Path};

use ::gds::{
    GdsArrayRef, GdsBoundary, GdsElement, GdsError, GdsLayerSpec, GdsLibrary, GdsPath, GdsPoint,
    GdsStrans, GdsStruct, GdsStructRef, GdsTextElem, GdsUnits,
};
use anyhow::{Context, Result, anyhow, bail};
use arcstr::ArcStr;
use indexmap::IndexMap;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use tracing::trace;
use uniquify::Names;

use crate::{
    compile::{CellId, CompileOutput, CompiledData, ExecErrorCompileOutput, SolvedValue},
    tech::{Technology, read_tech},
};

/// The most instances one imported AREF may flatten to.
///
/// GDSII allows 32767 x 32767, which is over a billion instances; the
/// flattening loop has no other bound.
const MAX_IMPORTED_ARRAY_INSTANCES: u64 = 1 << 20;

pub struct GdsMap {
    layers: IndexMap<String, GdsLayerSpec>,
}

struct GdsExporter {
    lib: GdsLibrary,
    map: GdsMap,
    names: Names<CellId>,
    display_unit: f64,
}

impl GdsExporter {
    fn new(name: impl Into<ArcStr>, tech: &Technology) -> Self {
        let mut lib = GdsLibrary::new(name);
        // The first GDS unit is the DBU size in display (user) units; the
        // second is the DBU size in meters.
        lib.units = GdsUnits::new(1. / tech.display_unit as f64, tech.dbu);
        Self {
            lib,
            map: GdsMap::from_technology(tech),
            names: Names::new(),
            display_unit: tech.display_unit as f64,
        }
    }

    /// Converts a source coordinate to an integer GDS database unit.
    ///
    /// Checked rather than a bare `as i32`, which *saturates*: an out-of-range
    /// coordinate would otherwise be written as `2147483647` and a NaN as `0`,
    /// with the export reporting success. `check_geometry` rejects both before
    /// a run gets this far, so reaching the error here means the exporter was
    /// handed output that never passed that gate -- report it rather than
    /// inventing a number.
    fn coord_to_gds(&self, coord: f64) -> Result<i32> {
        let dbu = (coord * self.display_unit).round();
        if !dbu.is_finite() || dbu < f64::from(i32::MIN) || dbu > f64::from(i32::MAX) {
            bail!("coordinate {coord} cannot be represented in this technology's database units");
        }
        Ok(dbu as i32)
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
    Path {
        layer: String,
        name: Option<String>,
        width: f64,
        points: Vec<(f64, f64)>,
        begin_extension: f64,
        end_extension: f64,
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
    tech: &Technology,
) -> Result<ImportedGdsLibrary> {
    import_gds_with_tech(path, declared_name, tech)
}

/// GDSII data-type code for an ASCII string record.
const GDS_DTYPE_ASCII: u8 = 6;

/// Bytes of record header preceding a record's payload.
const GDS_HEADER_LEN: u64 = 4;

/// Rejects the record shapes that make the GDS reader panic or fail opaquely,
/// naming the offending byte offset.
///
/// The reader strips an optional trailing NUL with `data[len - 1]`, which
/// underflows on a zero-length string record -- an empty `STRNAME`,
/// `LIBNAME`, or text `STRING` is enough -- and it decodes strings with
/// `from_utf8`, so one Latin-1 byte in a vendor file rejects the file as a
/// whole. A compiler should never panic on file content it did not produce,
/// and a malformed input deserves a diagnostic that says which file and where.
fn validate_gds_records(path: &Path) -> Result<()> {
    use std::io::{BufReader, Read, Seek, SeekFrom};

    let mut reader = BufReader::new(
        std::fs::File::open(path)
            .with_context(|| format!("could not open imported GDS `{}`", path.display()))?,
    );
    let mut offset = 0u64;
    let mut header = [0u8; GDS_HEADER_LEN as usize];
    loop {
        match reader.read_exact(&mut header) {
            Ok(()) => {}
            // A truncated trailing header is left to the reader, which reports
            // it as a read error.
            Err(_) => return Ok(()),
        }
        let len = u64::from(u16::from_be_bytes([header[0], header[1]]));
        let dtype = header[3];
        // GDSII records are at least a header long and always even. Framing
        // errors are the reader's to report; scanning past one would only
        // misalign this walk and produce a misleading offset.
        if len < GDS_HEADER_LEN || len % 2 != 0 {
            return Ok(());
        }
        let payload = len - GDS_HEADER_LEN;
        if dtype == GDS_DTYPE_ASCII {
            if payload == 0 {
                bail!(
                    "imported GDS `{}` has an empty string record at byte {offset}",
                    path.display()
                );
            }
            let mut data = vec![0u8; payload as usize];
            if reader.read_exact(&mut data).is_err() {
                return Ok(());
            }
            if std::str::from_utf8(&data).is_err() {
                bail!(
                    "imported GDS `{}` has a non-UTF-8 string record at byte {offset}; \
                     the GDS reader cannot decode it",
                    path.display()
                );
            }
        } else if reader.seek(SeekFrom::Current(payload as i64)).is_err() {
            return Ok(());
        }
        offset += len;
    }
}

/// Renders a reader or writer failure as a sentence.
///
/// `GdsError`'s own `Display` delegates to the derived `Debug`, so an
/// unexpected end of file surfaces as
/// `Boxed(Error { kind: UnexpectedEof, message: ".." })`. Unwrapping the
/// boxed cause and naming the record position is what distinguishes a
/// truncated file from a malformed one at the point of use.
///
/// Deliberately exhaustive: a catch-all arm would silently hand the two
/// remaining variants -- and any variant the upstream crate adds -- back to
/// that same `Debug` rendering, and `GdsError::Unsupported` carries a whole
/// `GdsRecord`, whose `Debug` is an unbounded coordinate dump.
fn describe_gds_error(error: GdsError) -> anyhow::Error {
    match error {
        GdsError::Boxed(cause) => anyhow!("{cause}"),
        GdsError::Str(message) => anyhow!("{message}"),
        GdsError::RecordLen(len) => anyhow!("invalid record length {len}"),
        GdsError::InvalidDataType(code) => anyhow!("invalid record data type {code}"),
        GdsError::InvalidRecordType(code) => anyhow!("invalid record type {code}"),
        GdsError::RecordDecode(record, data, len) => {
            anyhow!("invalid {record:?} record: data type {data:?}, length {len}")
        }
        // The record itself is dropped: its `Debug` is the unbounded dump.
        GdsError::Unsupported(_, Some(context)) => {
            anyhow!("unsupported GDS feature in {context:?}")
        }
        GdsError::Unsupported(_, None) => anyhow!("unsupported GDS feature"),
        GdsError::Parse {
            msg,
            recordnum,
            bytepos,
            ..
        } => anyhow!("{msg} (record {recordnum} at byte {bytepos})"),
    }
}

fn import_gds_with_tech(
    path: &Path,
    declared_name: &str,
    tech: &Technology,
) -> Result<ImportedGdsLibrary> {
    let map = GdsMap::from_technology(tech);
    validate_gds_records(path)?;
    let library = GdsLibrary::load(path)
        .map_err(describe_gds_error)
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
    // Imported GDS coordinates use the library's DBU. Argon source and
    // compiled geometry use the local technology's display unit, itself an
    // integer number of local DBUs.
    let scale = library.units.db_unit() / tech.dbu / tech.display_unit as f64;
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
                GdsElement::GdsPath(gds_path) => {
                    elements.push(import_path(&map, path, gds_path, scale)?);
                }
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

fn import_path(
    map: &GdsMap,
    source: &Path,
    path: &GdsPath,
    scale: f64,
) -> Result<ImportedGdsElement> {
    if path.xy.len() < 2 {
        bail!(
            "imported GDS `{}` contains a path with fewer than two points",
            source.display()
        );
    }
    let width = path.width.unwrap_or(0).unsigned_abs() as f64 * scale;
    let (begin_extension, end_extension) = match path.path_type.unwrap_or(0) {
        0 => (0., 0.),
        1 => bail!(
            "imported GDS `{}` contains a rounded path (path type 1), which is not supported",
            source.display()
        ),
        2 => (width / 2., width / 2.),
        4 => (
            f64::from(path.begin_extn.unwrap_or(0)) * scale,
            f64::from(path.end_extn.unwrap_or(0)) * scale,
        ),
        path_type => bail!(
            "imported GDS `{}` contains unsupported path type {path_type}",
            source.display()
        ),
    };
    Ok(ImportedGdsElement::Path {
        layer: import_layer(map, source, path.layer, path.datatype)?,
        name: None,
        width,
        points: path
            .xy
            .iter()
            .map(|point| (f64::from(point.x) * scale, f64::from(point.y) * scale))
            .collect(),
        begin_extension,
        end_extension,
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
    // A scraped label becomes a top-level `let` in the generated cell, so it
    // must clear the reserved instance fields as well as the keywords.
    if crate::compile::RESERVED_CELL_FIELDS.contains(&ident.as_str())
        || matches!(
            ident.as_str(),
            "fn" | "if"
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
        )
    {
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
    // The array is flattened into one instance per element, so a legal
    // 32767 x 32767 AREF is 1.07e9 instances -- tens of gigabytes from a
    // few-hundred-byte input. Bound it with a diagnostic instead.
    let count = u64::from(array.cols.unsigned_abs()) * u64::from(array.rows.unsigned_abs());
    if count > MAX_IMPORTED_ARRAY_INSTANCES {
        bail!(
            "imported GDS `{}` contains a {} x {} array, which flattens to {count} instances \
             (the maximum is {MAX_IMPORTED_ARRAY_INSTANCES})",
            path.display(),
            array.cols,
            array.rows,
        );
    }
    // Widen before subtracting: an array spanning more than `i32::MAX`
    // database units wraps the difference negative, silently mirroring the
    // array in release builds (and panicking in debug).
    let dx = (
        (f64::from(array.xy[1].x) - f64::from(array.xy[0].x)) / f64::from(array.cols),
        (f64::from(array.xy[1].y) - f64::from(array.xy[0].y)) / f64::from(array.cols),
    );
    let dy = (
        (f64::from(array.xy[2].x) - f64::from(array.xy[0].x)) / f64::from(array.rows),
        (f64::from(array.xy[2].y) - f64::from(array.xy[0].y)) / f64::from(array.rows),
    );
    let cell = names.get(array.name.as_str()).copied().ok_or_else(|| {
        anyhow!(
            "imported GDS `{}` references missing structure `{}`",
            path.display(),
            array.name
        )
    })?;
    let (angle, reflect) = import_transform(path, array.strans.as_ref())?;
    for row in 0..array.rows {
        for col in 0..array.cols {
            let x = f64::from(array.xy[0].x) + f64::from(col) * dx.0 + f64::from(row) * dy.0;
            let y = f64::from(array.xy[0].y) + f64::from(col) * dx.1 + f64::from(row) * dy.1;
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
            let parent = out_path.parent().unwrap_or_else(|| Path::new("."));
            std::fs::create_dir_all(parent)?;
            // Write through a sibling temporary file and rename on success.
            // `GdsLibrary::save` streams records, so a write that fails partway
            // -- an over-long record, a full disk -- otherwise leaves a
            // truncated `.gds` at the real path, next to a `.bin` that was
            // written earlier in the run and looks perfectly valid.
            let mut builder = tempfile::Builder::new();
            builder.prefix(".argon-gds").suffix(".tmp");
            // A temporary file is created 0600 so its contents are never
            // briefly world-readable. That is the wrong mode for a build
            // artifact, so give the finished file the permissions
            // `File::create` would have.
            #[cfg(unix)]
            builder.permissions(std::fs::Permissions::from_mode(0o644));
            let temp = builder.tempfile_in(parent)?;
            exporter.lib.save(temp.path()).map_err(describe_gds_error)?;
            temp.persist(out_path)
                .map_err(|e| anyhow!("could not finalize `{}`: {e}", out_path.display()))?;
        }

        Ok(())
    }
}

impl CompiledData {
    fn cell_to_gds(&self, exporter: &mut GdsExporter, id: CellId) -> Result<()> {
        trace!("Exporting cell {id}");
        let cell = &self.cells[&id];
        let name = gds_struct_name(&cell.name);
        if name != cell.name {
            // Keep the output traceable: a reader can only map a struct back
            // to its source cell if the rewrite is recorded somewhere.
            trace!("Cell `{}` exported as GDS struct `{name}`", cell.name);
        }
        let name = exporter.names.assign_name(id, &name);
        let mut ocell = GdsStruct::new(name.to_string());
        for (_, obj) in &cell.objects {
            // Shared with `bbox`, so the two cannot disagree about which
            // geometry exists.
            if !obj.is_layout() {
                continue;
            }
            match obj {
                SolvedValue::Rect(rect) => {
                    if let Some(layer) = &rect.layer {
                        let GdsLayerSpec {
                            layer,
                            xtype: datatype,
                        } = exporter.map[layer];
                        let x0 = exporter.coord_to_gds(rect.x0.0)?;
                        let x1 = exporter.coord_to_gds(rect.x1.0)?;
                        let y0 = exporter.coord_to_gds(rect.y0.0)?;
                        let y1 = exporter.coord_to_gds(rect.y1.0)?;
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
                            Ok(GdsPoint::new(
                                exporter.coord_to_gds(x.0)?,
                                exporter.coord_to_gds(y.0)?,
                            ))
                        })
                        .collect::<Result<Vec<_>>>()?;
                    points.push(points[0].clone());
                    ocell.elems.push(GdsElement::GdsBoundary(GdsBoundary {
                        layer,
                        datatype,
                        xy: points,
                        ..Default::default()
                    }));
                }
                SolvedValue::Path(path) => {
                    let GdsLayerSpec {
                        layer,
                        xtype: datatype,
                    } = exporter.map[&path.layer];
                    // `check_geometry` has already rejected a negative width,
                    // so the absolute value only normalizes `-0.`.
                    let width = exporter.coord_to_gds(path.width.0.abs())?;
                    let begin_extension = exporter.coord_to_gds(path.begin_extension.0)?;
                    let end_extension = exporter.coord_to_gds(path.end_extension.0)?;
                    let (path_type, begin_extn, end_extn) =
                        if begin_extension == 0 && end_extension == 0 {
                            (None, None, None)
                        } else if (path.begin_extension.0 - path.width.0.abs() / 2.).abs() < 1e-9
                            && (path.end_extension.0 - path.width.0.abs() / 2.).abs() < 1e-9
                        {
                            (Some(2), None, None)
                        } else {
                            (Some(4), Some(begin_extension), Some(end_extension))
                        };
                    ocell.elems.push(GdsElement::GdsPath(GdsPath {
                        layer,
                        datatype,
                        width: Some(width),
                        path_type,
                        begin_extn,
                        end_extn,
                        xy: path
                            .points
                            .iter()
                            .map(|(x, y)| {
                                Ok(GdsPoint::new(
                                    exporter.coord_to_gds(x.0)?,
                                    exporter.coord_to_gds(y.0)?,
                                ))
                            })
                            .collect::<Result<Vec<_>>>()?,
                        ..Default::default()
                    }));
                }
                SolvedValue::Text(text) => {
                    let GdsLayerSpec {
                        layer,
                        xtype: texttype,
                    } = exporter.map[&text.layer];
                    let x = exporter.coord_to_gds(text.x)?;
                    let y = exporter.coord_to_gds(text.y)?;
                    ocell.elems.push(GdsElement::GdsTextElem(GdsTextElem {
                        string: ArcStr::from(&text.text),
                        layer,
                        texttype,
                        xy: GdsPoint::new(x, y),
                        ..Default::default()
                    }));
                }
                SolvedValue::Instance(i) => {
                    if exporter.names.name(&i.cell).is_none() {
                        self.cell_to_gds(exporter, i.cell)?;
                    }
                    ocell.elems.push(GdsElement::GdsStructRef(GdsStructRef {
                        name: exporter.names.name(&i.cell).unwrap().clone(),
                        xy: GdsPoint::new(exporter.coord_to_gds(i.x)?, exporter.coord_to_gds(i.y)?),
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

/// The longest GDSII structure name the spec allows.
const MAX_GDS_NAME_LEN: usize = 32;

/// Converts a cell's identity into a name that is legal in GDSII.
///
/// GDSII names are at most [`MAX_GDS_NAME_LEN`] characters from
/// `[A-Za-z0-9_?$]`. Argon identities are module-qualified, so `sub::inner`
/// would otherwise emit a `:` that KLayout tolerates but Virtuoso and older
/// readers do not. Collisions introduced by sanitizing or truncating are
/// resolved by the caller's uniquifier, exactly as collisions between
/// same-named cells in different modules already are.
fn gds_struct_name(identity: &str) -> String {
    let mut name: String = identity
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '?' | '$') {
                c
            } else {
                '_'
            }
        })
        .collect();
    // Keep the tail: the cell's own name is the distinguishing part of a
    // module-qualified identity.
    if name.chars().count() > MAX_GDS_NAME_LEN {
        name = name
            .chars()
            .skip(name.chars().count() - MAX_GDS_NAME_LEN)
            .collect();
    }
    if name.is_empty() {
        name.push('_');
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_map() -> GdsMap {
        GdsMap::from_iter([("met1".to_owned(), GdsLayerSpec { layer: 1, xtype: 0 })])
    }

    #[test]
    fn non_rectangular_boundary_imports_as_polygon() {
        let map = test_map();
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
    fn imports_non_rounded_path_types() {
        for (path_type, begin_extn, end_extn, expected_extensions) in [
            (None, None, None, (0., 0.)),
            (Some(2), None, None, (10., 10.)),
            (Some(4), Some(7), Some(13), (7., 13.)),
        ] {
            let element = import_path(
                &test_map(),
                Path::new("path.gds"),
                &GdsPath {
                    layer: 1,
                    datatype: 0,
                    width: Some(20),
                    path_type,
                    begin_extn,
                    end_extn,
                    xy: vec![GdsPoint::new(0, 0), GdsPoint::new(100, 50)],
                    ..Default::default()
                },
                1.,
            )
            .expect("non-rounded path should import");
            let ImportedGdsElement::Path {
                layer,
                width,
                points,
                begin_extension,
                end_extension,
                ..
            } = element
            else {
                panic!("GDS path should remain a path");
            };
            assert_eq!(layer, "met1");
            assert_eq!(width, 20.);
            assert_eq!(points, [(0., 0.), (100., 50.)]);
            assert_eq!((begin_extension, end_extension), expected_extensions);
        }
    }

    #[test]
    fn rejects_rounded_paths() {
        let error = import_path(
            &test_map(),
            Path::new("rounded.gds"),
            &GdsPath {
                layer: 1,
                datatype: 0,
                width: Some(20),
                path_type: Some(1),
                xy: vec![GdsPoint::new(0, 0), GdsPoint::new(100, 0)],
                ..Default::default()
            },
            1.,
        )
        .expect_err("rounded path should be rejected");
        assert!(error.to_string().contains("rounded path"));
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
    fn exporter_transforms_display_units_to_configured_dbus() {
        let tech = crate::tech::parse_tech(
            r##"
                dbu = "nm"
                display_unit = 1000
                grid = 1

                [[layers]]
                name = "met1"
                gds = [1, 0]
                fill = "#0000ff"
                border = "#0000ff"
            "##,
        )
        .unwrap();
        let exporter = GdsExporter::new("test", &tech);
        assert_eq!(exporter.coord_to_gds(1.25).unwrap(), 1250);
        assert_eq!(exporter.lib.units.db_unit(), 1e-9);

        // `f64 as i32` saturates, so an unchecked conversion turns any of
        // these into `i32::MAX` (or `0`, for NaN) and writes it as a real
        // coordinate. `check_geometry` rejects them before the exporter runs;
        // this is the backstop for an output that skipped that gate.
        for coord in [1e9, -1e9, f64::INFINITY, f64::NAN] {
            assert!(
                exporter.coord_to_gds(coord).is_err(),
                "{coord} must not saturate"
            );
        }
    }

    #[test]
    fn importer_transforms_gds_dbus_to_display_units() {
        let tech = crate::tech::parse_tech(
            r##"
                dbu = "nm"
                display_unit = 1000
                grid = 1

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

    fn array_ref(cols: i16, rows: i16, xy: [GdsPoint; 3]) -> GdsArrayRef {
        GdsArrayRef {
            name: "child".into(),
            xy,
            cols,
            rows,
            strans: None,
            elflags: None,
            plex: None,
            properties: Vec::new(),
        }
    }

    #[test]
    fn wide_array_pitch_does_not_wrap() {
        // The span exceeds `i32::MAX`, so subtracting before widening would
        // wrap the difference negative and mirror the array.
        let mut elements = Vec::new();
        import_array(
            Path::new("array.gds"),
            &IndexMap::from_iter([("child".to_owned(), 0usize)]),
            &array_ref(
                2,
                1,
                [
                    GdsPoint::new(-2_000_000_000, 0),
                    GdsPoint::new(2_000_000_000, 0),
                    GdsPoint::new(-2_000_000_000, 0),
                ],
            ),
            1.,
            &mut elements,
        )
        .expect("wide array should import");

        let xs = elements
            .iter()
            .map(|element| match element {
                ImportedGdsElement::Instance { x, .. } => *x,
                _ => panic!("array should flatten to instances"),
            })
            .collect::<Vec<_>>();
        assert_eq!(xs, [-2_000_000_000., 0.]);
    }

    #[test]
    fn oversized_array_is_rejected() {
        let mut elements = Vec::new();
        let error = import_array(
            Path::new("array.gds"),
            &IndexMap::from_iter([("child".to_owned(), 0usize)]),
            &array_ref(
                32767,
                32767,
                [
                    GdsPoint::new(0, 0),
                    GdsPoint::new(32767, 0),
                    GdsPoint::new(0, 32767),
                ],
            ),
            1.,
            &mut elements,
        )
        .expect_err("a billion-instance array should be rejected");
        assert!(error.to_string().contains("flattens to"), "{error}");
        assert!(elements.is_empty());
    }

    /// Writes `library` to a scratch file, runs `check`, then removes it.
    fn with_saved_library(name: &str, library: &GdsLibrary, check: impl FnOnce(&Path)) {
        let path = std::env::temp_dir().join(format!(
            "argon-{name}-{}-{}.gds",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        library.save(&path).unwrap();
        check(&path);
        std::fs::remove_file(&path).unwrap();
    }

    #[test]
    fn a_failed_gds_import_names_its_cause() {
        // The caller rendered this `anyhow::Error` with `to_string()`, which
        // shows only the outermost context -- so a missing file, a truncated
        // one, and a malformed one all reached the user as the same
        // "could not read imported GDS `..`" with the cause dropped.
        let tech = Technology {
            dbu: 1e-10,
            display_unit: 10,
            grid: 1,
            style_name: None,
            layers: Vec::new(),
            custom_dither_patterns: Vec::new(),
            custom_line_styles: Vec::new(),
            pin_layers: IndexMap::new(),
        };
        // `tempfile` cleans up on unwind; a hand-rolled directory removed by a
        // trailing statement leaks on every assertion failure.
        let dir = tempfile::tempdir().unwrap();

        let missing = dir.path().join("missing.gds");
        let error = import_gds(&missing, "top", &tech).expect_err("a missing file should fail");
        // The OS supplies the wording, so match on the classification rather
        // than on `strerror` text that differs per platform.
        assert!(
            error
                .chain()
                .filter_map(|cause| cause.downcast_ref::<std::io::Error>())
                .any(|cause| cause.kind() == std::io::ErrorKind::NotFound),
            "{error:#}"
        );

        let truncated = dir.path().join("truncated.gds");
        std::fs::write(&truncated, [0u8, 6, 0, 2]).unwrap();
        let error = import_gds(&truncated, "top", &tech).expect_err("a truncated file should fail");
        let rendered = format!("{error:#}");
        assert!(
            rendered.contains("could not read imported GDS"),
            "{rendered}"
        );
        assert!(
            rendered.contains("fill whole buffer"),
            "the reader's own cause must survive: {rendered}"
        );
    }

    #[test]
    fn empty_string_record_is_reported_not_panicked() {
        let mut library = GdsLibrary::new("fixture");
        library.structs.push(GdsStruct::new("top"));
        with_saved_library("empty-strname", &library, |path| {
            // Blank out the STRNAME payload, which the reader would index
            // out of bounds while stripping a trailing NUL.
            let original = std::fs::read(path).unwrap();
            let mut patched = Vec::with_capacity(original.len());
            let mut offset = 0;
            while offset < original.len() {
                let len = u16::from_be_bytes([original[offset], original[offset + 1]]) as usize;
                if original[offset + 2] == 0x06 {
                    patched.extend_from_slice(&[0, 4, 0x06, 0x06]);
                } else {
                    patched.extend_from_slice(&original[offset..offset + len]);
                }
                offset += len;
            }
            std::fs::write(path, patched).unwrap();

            let error = validate_gds_records(path).expect_err("empty STRNAME should be reported");
            assert!(error.to_string().contains("empty string record"), "{error}");
        });
    }

    #[test]
    fn gds_struct_names_are_sanitized_and_bounded() {
        // Module separators are illegal in GDSII, so they are rewritten rather
        // than scraped away -- `sub::inner` and `other::inner` stay distinct.
        assert_eq!(gds_struct_name("sub::inner"), "sub__inner");
        assert_eq!(gds_struct_name("other::inner"), "other__inner");
        // An imported struct name with spaces keeps all of its words.
        assert_eq!(gds_struct_name("my cell foo"), "my_cell_foo");
        assert_eq!(gds_struct_name("x y"), "x_y");
        assert_eq!(gds_struct_name("x z"), "x_z");
        // A name of only illegal characters still yields a usable name.
        assert_eq!(gds_struct_name("   "), "___");
        assert_eq!(gds_struct_name(""), "_");
        // Over-long names are truncated to the tail, which carries the cell's
        // own name.
        let long = gds_struct_name("a::really_long_module::path_and_then_a_cell_name");
        assert_eq!(long.chars().count(), MAX_GDS_NAME_LEN);
        assert!(long.ends_with("path_and_then_a_cell_name"), "{long}");
    }

    #[test]
    fn valid_library_passes_record_validation() {
        let mut library = GdsLibrary::new("fixture");
        library.structs.push(GdsStruct::new("top"));
        with_saved_library("valid-records", &library, |path| {
            validate_gds_records(path).expect("a library this crate wrote must validate");
        });
    }
}
