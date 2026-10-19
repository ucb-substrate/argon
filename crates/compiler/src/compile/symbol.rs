//! The symbol of a cell: the shapes, text, and pins that a parent's schematic
//! draws each instance of the cell with.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, OnceLock};

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};

use crate::ast::Span;

use super::schematic::{DeviceKind, NetIdx, NodeId, ParamValue, PathSegment, Schematic};
use super::{CellId, CompileInput, CompileOutput, ExecError, ExecErrorKind, ValueId};

/// A cell's symbol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Symbol {
    /// Shapes and text, in call order.
    pub items: Vec<SymbolItem>,
    /// The pin of each port, in port order; `None` for a port without one.
    pub pins: Vec<Option<Pin>>,
}

/// A shape or text of a [`Symbol`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SymbolItem {
    pub kind: SymbolItemKind,
    /// The call that drew the item; `None` in a generated symbol.
    pub site: Option<Site>,
}

/// Coordinates are grid units; angles are degrees counterclockwise from the
/// +x axis.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SymbolItemKind {
    Line {
        points: Vec<(f64, f64)>,
    },
    Polygon {
        points: Vec<(f64, f64)>,
        fill: bool,
    },
    Rect {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        fill: bool,
    },
    Circle {
        x: f64,
        y: f64,
        r: f64,
        fill: bool,
    },
    Arc {
        x: f64,
        y: f64,
        r: f64,
        start: f64,
        end: f64,
    },
    /// Text centered vertically on `y`, with `x` at its left end, center, or
    /// right end by `align`, and `size` its height.
    Text {
        text: SymbolText,
        x: f64,
        y: f64,
        size: f64,
        align: Align,
    },
}

/// What a text item shows.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum SymbolText {
    Literal(String),
    /// The instance's name in the parent, such as `Xinv`.
    InstName,
    /// `name=value`, formatted as netlists format parameters.
    Param {
        name: String,
        value: ParamValue,
    },
}

/// Where a parent's wires end for one port.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    pub x: i64,
    pub y: i64,
    /// Where the port's name is drawn, when the call has `label=true`.
    pub label: Option<PinLabel>,
    /// The `pin` call; `None` in a generated symbol.
    pub site: Option<Site>,
}

/// The position of a pin's name, at size 1.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinLabel {
    pub x: f64,
    pub y: f64,
    pub align: Align,
}

/// How text is anchored to its `x`, mirroring `std::symbol::Align`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Align {
    #[default]
    Left,
    Center,
    Right,
}

impl Align {
    /// The alignment the `Align` variant `variant` names.
    pub fn from_variant(variant: &str) -> Option<Self> {
        match variant {
            "Left" => Some(Align::Left),
            "Center" => Some(Align::Center),
            "Right" => Some(Align::Right),
            _ => None,
        }
    }

    /// The name of the `Align` variant.
    pub fn name(self) -> &'static str {
        match self {
            Align::Left => "Left",
            Align::Center => "Center",
            Align::Right => "Right",
        }
    }
}

/// The source of a symbol item or pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Site {
    /// The `std::symbol` call.
    pub span: Span,
    /// The call in the cell's body it ran under, when it ran inside a
    /// function.
    pub origin: Option<Span>,
    /// Whether the GUI may rewrite the call.
    pub editable: bool,
}

/// An axis-aligned box as `(x0, y0, x1, y1)`.
pub type Bounds = (f64, f64, f64, f64);

/// Grows `bounds` to include `(x, y)`.
fn include(bounds: &mut Option<Bounds>, x: f64, y: f64) {
    *bounds = Some(match *bounds {
        None => (x, y, x, y),
        Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
    });
}

impl SymbolItemKind {
    /// The bounds of a shape; text has none.
    pub fn bounds(&self) -> Option<Bounds> {
        let mut bounds = None;
        match self {
            SymbolItemKind::Line { points } | SymbolItemKind::Polygon { points, .. } => {
                for (x, y) in points {
                    include(&mut bounds, *x, *y);
                }
            }
            SymbolItemKind::Rect { x0, y0, x1, y1, .. } => {
                include(&mut bounds, *x0, *y0);
                include(&mut bounds, *x1, *y1);
            }
            SymbolItemKind::Circle { x, y, r, .. } | SymbolItemKind::Arc { x, y, r, .. } => {
                include(&mut bounds, x - r, y - r);
                include(&mut bounds, x + r, y + r);
            }
            SymbolItemKind::Text { .. } => {}
        }
        bounds
    }
}

impl Symbol {
    /// The bounds of the symbol's shapes, leaving text out.
    pub fn shape_bounds(&self) -> Option<Bounds> {
        let mut bounds = None;
        for item in &self.items {
            if let Some((x0, y0, x1, y1)) = item.kind.bounds() {
                include(&mut bounds, x0, y0);
                include(&mut bounds, x1, y1);
            }
        }
        bounds
    }

    /// The bounds of the symbol's pins.
    pub fn pin_bounds(&self) -> Option<Bounds> {
        let mut bounds = None;
        for pin in self.pins.iter().flatten() {
            include(&mut bounds, pin.x as f64, pin.y as f64);
        }
        bounds
    }

    /// The size of the symbol: the bounds of its shapes and pins, leaving
    /// text out.
    pub fn bounds(&self) -> Option<Bounds> {
        let mut bounds = self.shape_bounds();
        if let Some((x0, y0, x1, y1)) = self.pin_bounds() {
            include(&mut bounds, x0, y0);
            include(&mut bounds, x1, y1);
        }
        bounds
    }
}

/// The position of the name of the pin at `(x, y)`, beside the pin and clear
/// of its stub. The pin's side is the edge of `bounds` it is nearest to, or
/// farthest beyond, with left and right winning ties.
pub fn pin_label(bounds: Bounds, x: i64, y: i64) -> PinLabel {
    let (x0, y0, x1, y1) = bounds;
    let (fx, fy) = (x as f64, y as f64);
    // How far outside each edge the pin is; negative inside the box.
    let left = x0 - fx;
    let right = fx - x1;
    let top = fy - y1;
    let bottom = y0 - fy;
    let horizontal = left.max(right);
    let vertical = top.max(bottom);
    if horizontal >= vertical {
        if left >= right {
            PinLabel {
                x: fx + 0.5,
                y: fy + 1.,
                align: Align::Left,
            }
        } else {
            PinLabel {
                x: fx - 0.5,
                y: fy + 1.,
                align: Align::Right,
            }
        }
    } else if top >= bottom {
        PinLabel {
            x: fx + 0.5,
            y: fy - 1.,
            align: Align::Left,
        }
    } else {
        PinLabel {
            x: fx + 0.5,
            y: fy + 1.,
            align: Align::Left,
        }
    }
}

/// Whether a port name looks like a supply, which the generated box puts on
/// top.
fn is_supply(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("vdd")
        || name.starts_with("vcc")
        || matches!(name.as_str(), "vpwr" | "vpb" | "avdd" | "dvdd")
}

/// Whether a port name looks like a ground, which the generated box puts on
/// the bottom.
fn is_ground(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("vss")
        || name.starts_with("gnd")
        || matches!(name.as_str(), "vgnd" | "vnb" | "agnd")
}

/// A side of the generated box.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum BoxSide {
    Left,
    Right,
    Top,
    Bottom,
}

/// The side of the generated box each of `ports` goes on: supplies on top,
/// grounds on the bottom, and the rest split between the left and right
/// sides, the first half (rounded up) on the left.
pub fn box_sides(ports: &[&str]) -> Vec<BoxSide> {
    let mut sides = vec![BoxSide::Left; ports.len()];
    let mut others = Vec::new();
    for (port, name) in ports.iter().enumerate() {
        if is_supply(name) {
            sides[port] = BoxSide::Top;
        } else if is_ground(name) {
            sides[port] = BoxSide::Bottom;
        } else {
            others.push(port);
        }
    }
    let left = others.len().div_ceil(2);
    for port in &others[left..] {
        sides[*port] = BoxSide::Right;
    }
    sides
}

/// A text item without a site.
fn text(text: SymbolText, x: f64, y: f64, align: Align) -> SymbolItem {
    SymbolItem {
        kind: SymbolItemKind::Text {
            text,
            x,
            y,
            size: 1.,
            align,
        },
        site: None,
    }
}

/// The box drawn for a cell without a symbol, with `ports` in port order and
/// `name` under it. Supplies go on top, grounds on the bottom, and the rest
/// are split between the left and right sides, the first half (rounded up)
/// on the left. The origin is the top-left corner; pins step by 2 from 2
/// units in, and each pin's name is written half a unit inside the box.
pub fn box_symbol(ports: &[&str], name: &str) -> Symbol {
    let sides = box_sides(ports);
    let on = |side: BoxSide| {
        sides
            .iter()
            .enumerate()
            .filter(|(_, s)| **s == side)
            .map(|(port, _)| port)
            .collect::<Vec<_>>()
    };
    let (top, bottom, left, right) = (
        on(BoxSide::Top),
        on(BoxSide::Bottom),
        on(BoxSide::Left),
        on(BoxSide::Right),
    );
    let w = (2 + 2 * top.len().max(bottom.len())).max(4) as i64;
    let h = (2 + 2 * left.len().max(right.len())).max(4) as i64;
    let mut pins = vec![None; ports.len()];
    let mut items = vec![SymbolItem {
        kind: SymbolItemKind::Rect {
            x0: 0.,
            y0: -(h as f64),
            x1: w as f64,
            y1: 0.,
            fill: false,
        },
        site: None,
    }];
    items.push(text(SymbolText::InstName, 0., 1., Align::Left));
    if !name.is_empty() {
        items.push(text(
            SymbolText::Literal(name.to_owned()),
            0.,
            -(h as f64) - 1.,
            Align::Left,
        ));
    }
    let mut place = |port: usize, x: i64, y: i64, label_x: f64, label_y: f64, align: Align| {
        pins[port] = Some(Pin {
            x,
            y,
            label: None,
            site: None,
        });
        items.push(text(
            SymbolText::Literal(ports[port].to_owned()),
            label_x,
            label_y,
            align,
        ));
    };
    for (index, port) in left.iter().enumerate() {
        let y = -2 - 2 * index as i64;
        place(*port, 0, y, 0.5, y as f64, Align::Left);
    }
    for (index, port) in right.iter().enumerate() {
        let y = -2 - 2 * index as i64;
        place(*port, w, y, w as f64 - 0.5, y as f64, Align::Right);
    }
    for (index, port) in top.iter().enumerate() {
        let x = 2 + 2 * index as i64;
        place(*port, x, 0, x as f64, -1., Align::Center);
    }
    for (index, port) in bottom.iter().enumerate() {
        let x = 2 + 2 * index as i64;
        place(*port, x, -h, x as f64, -(h as f64) + 1., Align::Center);
    }
    Symbol { items, pins }
}

/// The box drawn for a cell that draws no symbol, with its pin names as
/// literal text at the box's own positions.
pub fn generated_box(schematic: &Schematic, cell_name: &str) -> Symbol {
    let ports = (0..schematic.ports.len())
        .map(|port| schematic.port_name(port))
        .collect::<Vec<_>>();
    box_symbol(&ports, cell_name)
}

/// The `std::symbol` helper that draws a device of `kind` with `terminals`
/// terminals, if there is one.
fn device_helper(kind: DeviceKind, terminals: usize) -> Option<&'static str> {
    Some(match (kind, terminals) {
        (DeviceKind::Mos, 4) => "mos",
        (DeviceKind::Nmos, 4) => "nmos",
        (DeviceKind::Pmos, 4) => "pmos",
        (DeviceKind::Res, 2) => "res",
        (DeviceKind::Cap, 2) => "cap",
        (DeviceKind::Diode, 2) => "diode",
        (DeviceKind::Bjt, 3) => "bjt",
        (DeviceKind::Npn, 3) => "npn",
        (DeviceKind::Pnp, 3) => "pnp",
        (DeviceKind::Bjt, 4) => "bjt4",
        (DeviceKind::Npn, 4) => "npn4",
        (DeviceKind::Pnp, 4) => "pnp4",
        _ => return None,
    })
}

/// Compiles the symbol that `helper` draws on `terminals` fresh ports, so
/// that `std/symbol.ar` is the only place device symbols are defined.
fn compile_device_symbol(helper: &str, terminals: usize) -> Result<Symbol, String> {
    let ports = (0..terminals)
        .map(|terminal| format!("t{terminal}"))
        .collect::<Vec<_>>();
    let signals = ports
        .iter()
        .map(|port| format!("    pub let {port} = std::schematic::Signal();\n"))
        .collect::<String>();
    let source = format!(
        "cell device() {{\n{signals}    std::symbol::{helper}({});\n}}\n",
        ports.join(", ")
    );
    let root = crate::parse::parse_source_text(&source, PathBuf::from("<argon-symbol>/lib.ar"))
        .map_err(|error| format!("{error:?}"))?;
    let ast = crate::parse::with_std(root);
    let output = super::compile_without_tech(
        &ast,
        CompileInput {
            cell: &["device"],
            args: Vec::new(),
        },
    );
    match output {
        CompileOutput::Valid(data) => data.cells[&data.top]
            .symbol
            .clone()
            .ok_or_else(|| format!("`{helper}` draws nothing")),
        other => Err(format!("`{helper}` does not compile: {other:?}")),
    }
}

/// The symbol a raw device of `kind` with `terminals` terminals is drawn
/// with: the helper for its kind, compiled once per process, or a box with
/// numbered pins for a `Subckt` or a terminal count the kind does not take.
pub fn device_symbol(kind: DeviceKind, terminals: usize) -> Arc<Symbol> {
    static SYMBOLS: OnceLock<DeviceSymbols> = OnceLock::new();
    let symbols = SYMBOLS.get_or_init(Default::default);
    if let Some(symbol) = symbols.lock().expect("no poison").get(&(kind, terminals)) {
        return symbol.clone();
    }
    let symbol = device_helper(kind, terminals)
        .and_then(|helper| compile_device_symbol(helper, terminals).ok())
        .unwrap_or_else(|| {
            let names = (0..terminals).map(|t| t.to_string()).collect::<Vec<_>>();
            let names = names.iter().map(String::as_str).collect::<Vec<_>>();
            box_symbol(&names, "")
        });
    let symbol = Arc::new(symbol);
    symbols
        .lock()
        .expect("no poison")
        .insert((kind, terminals), symbol.clone());
    symbol
}

/// The device symbols compiled so far, by kind and terminal count.
type DeviceSymbols = Mutex<HashMap<(DeviceKind, usize), Arc<Symbol>>>;

/// A shape or text drawn while its cell executes.
#[derive(Debug, Clone)]
pub(super) struct ItemRecord {
    /// The drawing call's value id.
    pub(super) order: ValueId,
    pub(super) kind: SymbolItemKind,
    pub(super) site: Site,
}

/// A `pin` call that ran while its cell executes.
#[derive(Debug, Clone)]
pub(super) struct PinRecord {
    /// The call's value id.
    pub(super) order: ValueId,
    pub(super) node: NodeId,
    pub(super) x: i64,
    pub(super) y: i64,
    pub(super) label: bool,
    pub(super) site: Site,
}

/// The symbol of a cell while it executes.
#[derive(Debug, Clone, Default)]
pub(super) struct SymbolState {
    items: Vec<ItemRecord>,
    pins: Vec<PinRecord>,
}

impl SymbolState {
    pub(super) fn add_item(&mut self, record: ItemRecord) {
        self.items.push(record);
    }

    pub(super) fn add_pin(&mut self, record: PinRecord) {
        self.pins.push(record);
    }

    /// The spans of the editable calls that drew exactly one item or pin.
    fn unique_editable_spans(&self) -> HashSet<&Span> {
        let mut counts: HashMap<&Span, usize> = HashMap::new();
        let sites = self
            .items
            .iter()
            .map(|item| &item.site)
            .chain(self.pins.iter().map(|pin| &pin.site))
            .filter(|site| site.editable);
        for site in sites {
            *counts.entry(&site.span).or_default() += 1;
        }
        counts
            .into_iter()
            .filter(|(_, count)| *count == 1)
            .map(|(span, _)| span)
            .collect()
    }

    /// Builds the symbol of `cell`, checking its pins against the ports of
    /// `schematic` and reporting into `errors`. `nets` gives the net of every
    /// node, and `field_spans` the `let` that made each top-level binding.
    /// `None` when the cell made no `std::symbol` call.
    pub(super) fn emit(
        &self,
        cell: CellId,
        schematic: &Schematic,
        nets: &HashMap<NodeId, NetIdx>,
        field_spans: &IndexMap<String, Span>,
        errors: &mut Vec<ExecError>,
    ) -> Option<Symbol> {
        if self.items.is_empty() && self.pins.is_empty() {
            return None;
        }
        let unique = self.unique_editable_spans();
        let site = |site: &Site| Site {
            span: site.span.clone(),
            origin: site.origin.clone(),
            editable: site.editable && unique.contains(&site.span),
        };
        let mut error = |span: Option<Span>, kind: ExecErrorKind| {
            errors.push(ExecError { span, cell, kind });
        };

        let mut items = self.items.iter().collect::<Vec<_>>();
        items.sort_by_key(|item| item.order);
        let items = items
            .into_iter()
            .map(|item| SymbolItem {
                kind: item.kind.clone(),
                site: Some(site(&item.site)),
            })
            .collect::<Vec<_>>();

        let mut records = self.pins.iter().collect::<Vec<_>>();
        records.sort_by_key(|pin| pin.order);
        let mut placed: Vec<Option<&PinRecord>> = vec![None; schematic.ports.len()];
        let mut points = HashSet::new();
        for record in records {
            let Some(net) = nets.get(&record.node) else {
                continue;
            };
            let Some(port) = schematic.port_of(*net) else {
                error(Some(record.site.span.clone()), ExecErrorKind::PinNotOnPort);
                continue;
            };
            if placed[port].is_some() {
                error(
                    Some(record.site.span.clone()),
                    ExecErrorKind::DuplicatePin {
                        port: schematic.port_name(port).to_owned(),
                    },
                );
                continue;
            }
            if !points.insert((record.x, record.y)) {
                error(
                    Some(record.site.span.clone()),
                    ExecErrorKind::CoincidentPins {
                        x: record.x,
                        y: record.y,
                    },
                );
                continue;
            }
            placed[port] = Some(record);
        }

        let mut symbol = Symbol {
            items,
            pins: placed
                .iter()
                .map(|record| {
                    record.map(|record| Pin {
                        x: record.x,
                        y: record.y,
                        label: None,
                        site: Some(site(&record.site)),
                    })
                })
                .collect(),
        };
        // Labels follow the shapes, or the pins of a symbol without shapes.
        let bounds = symbol.shape_bounds().or_else(|| symbol.pin_bounds());
        for (pin, record) in symbol.pins.iter_mut().zip(&placed) {
            if let (Some(pin), Some(record)) = (pin, record)
                && record.label
                && let Some(bounds) = bounds
            {
                pin.label = Some(pin_label(bounds, pin.x, pin.y));
            }
        }

        for (port, pin) in symbol.pins.iter().enumerate() {
            if pin.is_some() {
                continue;
            }
            // The `pub let` that made the port, through the port's path.
            let span = schematic.port_paths[port].and_then(|path| {
                match schematic.path_segments(path).first() {
                    Some(PathSegment::Binding(name)) => field_spans.get(name).cloned(),
                    _ => None,
                }
            });
            error(
                span,
                ExecErrorKind::PortWithoutPin {
                    port: schematic.port_name(port).to_owned(),
                },
            );
        }
        Some(symbol)
    }
}
