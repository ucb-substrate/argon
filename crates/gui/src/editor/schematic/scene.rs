//! The drawing of a cell's schematic: a pure function of the compiled data
//! and the display preferences, in grid units.

use std::collections::HashMap;

use argonc::ast::Span;
use argonc::compile::{
    CellId, CompiledData, Element, NetIdx, NodeIdx, NodeSource, Placement, box_sides,
    device_symbol, generated_box,
};
use argonc::netlist::format_param;

use super::place::{port_position, slot_pitch, slot_position, slot_rows};
use super::symbols::{Bounds, PlacedSymbol, Pt, include, place_symbol, union};
use super::wires::{NetItem, NetItemKind, analyze_nets};

/// Session display preferences the scene depends on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayPrefs {
    pub show_flylines: bool,
}

/// Placements and routes drawn in place of the source's, by the span of the
/// call they belong to: a gesture in progress, or one whose edit is pending.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Overrides {
    pub placements: HashMap<Span, Placement>,
    /// A route, or `None` for a connection whose route is removed.
    pub routes: HashMap<Span, Option<Vec<(i64, i64)>>>,
}

impl Overrides {
    pub fn is_empty(&self) -> bool {
        self.placements.is_empty() && self.routes.is_empty()
    }
}

impl Default for DisplayPrefs {
    fn default() -> Self {
        Self {
            show_flylines: true,
        }
    }
}

/// Everything the schematic view draws for one cell.
#[derive(Debug, Clone, Default)]
pub struct Scene {
    pub elements: Vec<SceneElement>,
    pub ports: Vec<PortGlyph>,
    pub labels: Vec<NetLabel>,
    /// One wire per routed `connect`, as its source draws it.
    pub wires: Vec<Wire>,
    /// The wires' segments with collinear overlaps of one net merged, which
    /// is what gets painted.
    pub segments: Vec<NetSegment>,
    pub junctions: Vec<Junction>,
    pub conflicts: Vec<Conflict>,
    pub flylines: Vec<Flyline>,
    pub undrawn: Vec<UndrawnRoute>,
    /// The `connect`s without a route.
    pub unrouted: Vec<UnroutedConnection>,
    /// How the cell body names each node, for the statements the tools
    /// write.
    pub node_paths: Vec<Option<String>>,
    /// The name of each net.
    pub nets: Vec<String>,
    /// The nets that are ports, in port order.
    pub port_nets: Vec<NetIdx>,
    pub bounds: Option<Bounds>,
    /// Whether the cell has no schematic content at all.
    pub empty: bool,
}

/// An instance or device, drawn with its symbol.
#[derive(Debug, Clone)]
pub struct SceneElement {
    pub element: Element,
    pub name: String,
    pub cell: Option<CellId>,
    pub cell_name: Option<String>,
    pub model: Option<String>,
    /// Formatted device parameters.
    pub params: Vec<(String, String)>,
    pub placement: Placement,
    pub auto_placed: bool,
    pub editable: bool,
    pub span: Option<Span>,
    pub symbol: PlacedSymbol,
    /// The symbol before placement, to place again under another placement.
    pub raw_symbol: std::sync::Arc<argonc::compile::Symbol>,
    /// The port or terminal names the symbol's pin labels show.
    pub pin_names: Vec<String>,
    /// The bounds of the symbol's shapes and pins; a point for an empty one.
    pub bounds: Bounds,
    pub pins: Vec<ElementPin>,
}

impl SceneElement {
    pub fn origin(&self) -> Pt {
        let (x, y) = self.placement.position.unwrap_or((0, 0));
        (x as f64, y as f64)
    }
}

/// A terminal of an element.
#[derive(Debug, Clone)]
pub struct ElementPin {
    pub node: NodeIdx,
    pub net: NetIdx,
    pub name: String,
    /// Where the pin is; `None` for a port without a pin in the child's
    /// symbol.
    pub position: Option<(i64, i64)>,
}

/// A port pin: a placed or auto-placed signal on a port.
#[derive(Debug, Clone)]
pub struct PortGlyph {
    pub node: NodeIdx,
    pub net: NetIdx,
    pub port: usize,
    pub name: String,
    pub position: (i64, i64),
    pub placement: Placement,
    pub auto_placed: bool,
    pub editable: bool,
    pub span: Span,
}

/// A net label: a placed signal on an internal net.
#[derive(Debug, Clone)]
pub struct NetLabel {
    pub node: NodeIdx,
    pub net: NetIdx,
    pub name: String,
    pub position: (i64, i64),
    pub placement: Placement,
    pub editable: bool,
    pub span: Span,
}

/// A routed `connect`, from pin to pin through its corners.
#[derive(Debug, Clone)]
pub struct Wire {
    pub connection: usize,
    pub net: NetIdx,
    /// The nodes at its two ends.
    pub nodes: [NodeIdx; 2],
    pub points: Vec<Pt>,
    pub editable: bool,
    pub span: Span,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct NetSegment {
    pub net: NetIdx,
    pub a: Pt,
    pub b: Pt,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Junction {
    pub net: NetIdx,
    pub at: Pt,
}

/// A point where the drawing shows a connection the netlist lacks.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Conflict {
    pub nets: [NetIdx; 2],
    pub at: Pt,
}

/// A connection that no wire draws, between two islands of one net.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Flyline {
    pub net: NetIdx,
    pub from: Pt,
    pub to: Pt,
    /// The nodes at each end, when the ends are pins.
    pub nodes: [Option<NodeIdx>; 2],
}

/// A `connect` without a route, which flylines show.
#[derive(Debug, Clone, PartialEq)]
pub struct UnroutedConnection {
    pub connection: usize,
    pub nodes: [NodeIdx; 2],
    pub editable: bool,
    pub span: Span,
}

/// A route that ends at a node without a pin, so it is not drawn.
#[derive(Debug, Clone, PartialEq)]
pub struct UndrawnRoute {
    pub connection: usize,
    pub net: NetIdx,
    pub reason: String,
}

/// Builds the scene of `cell` in `data`.
pub fn build_scene(data: &CompiledData, cell: CellId, prefs: &DisplayPrefs) -> Scene {
    build_scene_with(data, cell, prefs, &Overrides::default())
}

/// Builds the scene of `cell` in `data`, with `overrides` in place of the
/// source's placements and routes.
pub fn build_scene_with(
    data: &CompiledData,
    cell: CellId,
    prefs: &DisplayPrefs,
    overrides: &Overrides,
) -> Scene {
    let Some(compiled) = data.cells.get(&cell) else {
        return Scene {
            empty: true,
            ..Scene::default()
        };
    };
    let schematic = &compiled.schematic;
    let mut scene = Scene {
        nets: schematic.nets.iter().map(|net| net.name.clone()).collect(),
        port_nets: schematic.ports.clone(),
        empty: schematic.nodes.is_empty() && schematic.instances.is_empty(),
        node_paths: schematic
            .nodes
            .iter()
            .map(|node| node.path.map(|path| schematic.path_text(path)))
            .collect(),
        ..Scene::default()
    };
    if scene.empty {
        return scene;
    }

    // Symbols first, since the slot pitch depends on the largest one.
    struct Prepared {
        element: Element,
        symbol: std::sync::Arc<argonc::compile::Symbol>,
        pin_names: Vec<String>,
        size: f64,
    }
    let prepared = schematic
        .elements
        .iter()
        .map(|element| match *element {
            Element::Instance(index) => {
                let instance = &schematic.instances[index];
                let child = data.cells.get(&instance.cell);
                let symbol = match child.and_then(|child| child.symbol.clone()) {
                    Some(symbol) => std::sync::Arc::new(symbol),
                    None => std::sync::Arc::new(match child {
                        Some(child) => generated_box(&child.schematic, &child.name),
                        None => generated_box(&Default::default(), ""),
                    }),
                };
                let pin_names = child
                    .map(|child| {
                        (0..child.schematic.ports.len())
                            .map(|port| child.schematic.port_name(port).to_owned())
                            .collect()
                    })
                    .unwrap_or_default();
                let size = symbol_size(&symbol);
                Prepared {
                    element: *element,
                    symbol,
                    pin_names,
                    size,
                }
            }
            Element::Device(index) => {
                let device = &schematic.devices[index];
                let count = device.terminals.len();
                let symbol = device_symbol(device.kind, count);
                let pin_names = (0..count)
                    .map(|terminal| device.kind.terminal_label(terminal, count))
                    .collect();
                let size = symbol_size(&symbol);
                Prepared {
                    element: *element,
                    symbol,
                    pin_names,
                    size,
                }
            }
        })
        .collect::<Vec<_>>();
    let largest = prepared.iter().map(|p| p.size).fold(0., f64::max);
    let pitch = slot_pitch(largest);
    let rows = slot_rows(prepared.len());

    for (k, prepared) in prepared.into_iter().enumerate() {
        let (name, span, source_placement, editable, nodes, cell_id) = match prepared.element {
            Element::Instance(index) => {
                let instance = &schematic.instances[index];
                (
                    instance.name.clone(),
                    instance.span.clone(),
                    instance.placement,
                    instance.editable,
                    instance.terminal_nodes.clone(),
                    Some(instance.cell),
                )
            }
            Element::Device(index) => {
                let device = &schematic.devices[index];
                (
                    device.name.clone(),
                    device.span.clone(),
                    device.placement,
                    device.editable,
                    device.terminal_nodes.clone(),
                    None,
                )
            }
        };
        let source = span
            .as_ref()
            .and_then(|span| overrides.placements.get(span).copied())
            .or(source_placement)
            .unwrap_or_default();
        let auto_placed = source.position.is_none();
        let placement = Placement {
            position: Some(source.position.unwrap_or_else(|| slot_position(k, pitch))),
            ..source
        };
        let symbol = place_symbol(&prepared.symbol, &placement, &name, &prepared.pin_names);
        let origin = placement.position.unwrap_or((0, 0));
        let origin = (origin.0 as f64, origin.1 as f64);
        let bounds = symbol
            .bounds()
            .unwrap_or((origin.0, origin.1, origin.0, origin.1));
        let pins = nodes
            .iter()
            .enumerate()
            .map(|(terminal, node)| ElementPin {
                node: *node,
                net: schematic.nodes[*node as usize].net,
                name: prepared
                    .pin_names
                    .get(terminal)
                    .cloned()
                    .unwrap_or_default(),
                position: symbol.pins.get(terminal).copied().flatten(),
            })
            .collect();
        let (cell_name, model, params) = match prepared.element {
            Element::Instance(_) => (
                cell_id
                    .and_then(|cell| data.cells.get(&cell))
                    .map(|c| c.name.clone()),
                None,
                Vec::new(),
            ),
            Element::Device(index) => {
                let device = &schematic.devices[index];
                let mut params = Vec::new();
                if let Some(value) = &device.value {
                    params.push(("value".to_owned(), format_param(value)));
                }
                params.extend(
                    device
                        .params
                        .iter()
                        .map(|(name, value)| (name.clone(), format_param(value))),
                );
                (None, device.model.clone(), params)
            }
        };
        scene.elements.push(SceneElement {
            element: prepared.element,
            name,
            cell: cell_id,
            cell_name,
            model,
            params,
            placement,
            auto_placed,
            editable,
            span,
            symbol,
            raw_symbol: prepared.symbol,
            pin_names: prepared.pin_names,
            bounds,
            pins,
        });
    }

    // Ports and labels: the cell's own signals.
    let port_names = (0..schematic.ports.len())
        .map(|port| schematic.port_name(port))
        .collect::<Vec<_>>();
    let sides = box_sides(&port_names);
    let mut side_counts: HashMap<argonc::compile::BoxSide, usize> = HashMap::new();
    let mut port_auto_position = vec![None; schematic.ports.len()];
    for (port, side) in sides.iter().enumerate() {
        let index = side_counts.entry(*side).or_default();
        port_auto_position[port] = Some(port_position(*side, *index, pitch, rows));
        *index += 1;
    }
    let mut port_has_glyph = vec![false; schematic.ports.len()];
    // Placed signals first, so an unplaced one never takes the auto slot of
    // a port that is already drawn.
    for pass in 0..2 {
        for (index, node) in schematic.nodes.iter().enumerate() {
            let NodeSource::Signal {
                span,
                placement,
                editable,
            } = &node.source
            else {
                continue;
            };
            let source = overrides
                .placements
                .get(span)
                .copied()
                .or(*placement)
                .unwrap_or_default();
            let placed = source.position.is_some();
            if placed != (pass == 0) {
                continue;
            }
            // A port pin needs a `pub` field; a private signal on a port's
            // net is a label like any other.
            let port = schematic.port_of(node.net).filter(|_| node.public);
            match (port, source.position) {
                (Some(port), Some(position)) => {
                    port_has_glyph[port] = true;
                    scene.ports.push(PortGlyph {
                        node: index as NodeIdx,
                        net: node.net,
                        port,
                        name: port_names[port].to_owned(),
                        position,
                        placement: source,
                        auto_placed: false,
                        editable: *editable,
                        span: span.clone(),
                    });
                }
                (Some(port), None) => {
                    if port_has_glyph[port] {
                        continue;
                    }
                    port_has_glyph[port] = true;
                    let position = port_auto_position[port].expect("every port has a slot");
                    scene.ports.push(PortGlyph {
                        node: index as NodeIdx,
                        net: node.net,
                        port,
                        name: port_names[port].to_owned(),
                        position,
                        placement: Placement {
                            position: Some(position),
                            ..source
                        },
                        auto_placed: true,
                        editable: *editable,
                        span: span.clone(),
                    });
                }
                (None, Some(position)) => scene.labels.push(NetLabel {
                    node: index as NodeIdx,
                    net: node.net,
                    name: scene.nets[node.net as usize].clone(),
                    position,
                    placement: source,
                    editable: *editable,
                    span: span.clone(),
                }),
                (None, None) => {}
            }
        }
    }

    // The pin of every node, and where its flylines end.
    let mut node_pin: Vec<Option<Pt>> = vec![None; schematic.nodes.len()];
    let mut node_anchor: Vec<Option<Pt>> = vec![None; schematic.nodes.len()];
    for element in &scene.elements {
        for pin in &element.pins {
            let position = pin.position.map(|(x, y)| (x as f64, y as f64));
            node_pin[pin.node as usize] = position;
            node_anchor[pin.node as usize] = position.or(Some(element.origin()));
        }
    }
    for glyph in &scene.ports {
        let position = (glyph.position.0 as f64, glyph.position.1 as f64);
        node_pin[glyph.node as usize] = Some(position);
        node_anchor[glyph.node as usize] = Some(position);
    }
    for label in &scene.labels {
        let position = (label.position.0 as f64, label.position.1 as f64);
        node_pin[label.node as usize] = Some(position);
        node_anchor[label.node as usize] = Some(position);
    }

    // Wires: exactly the routes the source stores.
    for (index, connection) in schematic.connections.iter().enumerate() {
        let route = match overrides.routes.get(&connection.span) {
            Some(route) => route.as_ref(),
            None => connection.route.as_ref(),
        };
        let Some(route) = route else {
            scene.unrouted.push(UnroutedConnection {
                connection: index,
                nodes: connection.nodes,
                editable: connection.editable,
                span: connection.span.clone(),
            });
            continue;
        };
        let [a, b] = connection.nodes;
        let net = schematic.nodes[a as usize].net;
        let (Some(start), Some(end)) = (node_pin[a as usize], node_pin[b as usize]) else {
            let pinless = if node_pin[a as usize].is_none() { a } else { b };
            let reason = match &schematic.nodes[pinless as usize].source {
                NodeSource::Signal { .. } => "ends at a signal that is not placed".to_owned(),
                NodeSource::Terminal { .. } | NodeSource::DeviceTerminal { .. } => {
                    "ends at a terminal whose port has no pin in the child's symbol".to_owned()
                }
            };
            scene.undrawn.push(UndrawnRoute {
                connection: index,
                net,
                reason,
            });
            continue;
        };
        let mut points = Vec::with_capacity(route.len() + 2);
        points.push(start);
        points.extend(route.iter().map(|(x, y)| (*x as f64, *y as f64)));
        points.push(end);
        scene.wires.push(Wire {
            connection: index,
            net,
            nodes: [a, b],
            points,
            editable: connection.editable,
            span: connection.span.clone(),
        });
    }

    // Items of each net, for merging, junctions, conflicts, and flylines.
    let mut items = Vec::new();
    for element in &scene.elements {
        for pin in &element.pins {
            match pin.position {
                Some((x, y)) => items.push(NetItem {
                    net: pin.net,
                    kind: NetItemKind::Pin {
                        at: (x as f64, y as f64),
                        node: pin.node,
                    },
                }),
                None => items.push(NetItem {
                    net: pin.net,
                    kind: NetItemKind::Anchor {
                        at: element.origin(),
                        node: pin.node,
                    },
                }),
            }
        }
    }
    for glyph in &scene.ports {
        items.push(NetItem {
            net: glyph.net,
            kind: NetItemKind::Named {
                at: (glyph.position.0 as f64, glyph.position.1 as f64),
                node: glyph.node,
            },
        });
    }
    for label in &scene.labels {
        items.push(NetItem {
            net: label.net,
            kind: NetItemKind::Named {
                at: (label.position.0 as f64, label.position.1 as f64),
                node: label.node,
            },
        });
    }
    for wire in &scene.wires {
        for pair in wire.points.windows(2) {
            items.push(NetItem {
                net: wire.net,
                kind: NetItemKind::Segment {
                    a: pair[0],
                    b: pair[1],
                },
            });
        }
    }
    let analysis = analyze_nets(&items, scene.nets.len());
    scene.segments = analysis.segments;
    scene.junctions = analysis.junctions;
    scene.conflicts = analysis.conflicts;
    scene.flylines = if prefs.show_flylines {
        analysis.flylines
    } else {
        Vec::new()
    };

    let mut bounds = None;
    for element in &scene.elements {
        bounds = union(bounds, Some(element.bounds));
    }
    for glyph in &scene.ports {
        include(
            &mut bounds,
            (glyph.position.0 as f64, glyph.position.1 as f64),
        );
    }
    for label in &scene.labels {
        include(
            &mut bounds,
            (label.position.0 as f64, label.position.1 as f64),
        );
    }
    for wire in &scene.wires {
        for point in &wire.points {
            include(&mut bounds, *point);
        }
    }
    scene.bounds = bounds;
    scene
}

/// The size of a symbol: the larger side of the bounds of its shapes and
/// pins.
fn symbol_size(symbol: &argonc::compile::Symbol) -> f64 {
    match symbol.bounds() {
        Some((x0, y0, x1, y1)) => (x1 - x0).max(y1 - y0),
        None => 0.,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::path::PathBuf;

    use argonc::compile::{CellArg, CompileInput, CompileOutput, CompiledData};

    use super::*;

    /// `cell(args)` of `source`, compiled in a scratch workspace.
    pub(crate) fn compile(source: &str, cell: &str, args: Vec<CellArg>) -> CompileOutput {
        let directory = tempfile::tempdir().unwrap();
        let lib = directory.path().join("lib.ar");
        std::fs::write(&lib, source).unwrap();
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = argonc::WorkspaceConfig::new(&lib).with_tech(Some(tech));
        let ast = argonc::parse::parse_workspace_with_config(&config).ast();
        argonc::compile::compile(
            &ast,
            CompileInput {
                cell: &[cell],
                args,
            },
            &config,
        )
    }

    pub(crate) fn compile_valid(source: &str, cell: &str, args: Vec<CellArg>) -> CompiledData {
        match compile(source, cell, args) {
            CompileOutput::Valid(data) => data,
            other => panic!("{other:#?}"),
        }
    }

    pub(crate) const DEVICE_INVERTER: &str = r#"
use std::schematic::{DeviceKind, Signal, connect, device, signals};
use std::symbol::{circle, inst_name, line, param, pin, polygon};

cell device_inv(pw: Float, nw: Float) {
    pub let a = Signal(x=-6, y=0);
    pub let out = Signal(x=4, y=0);
    pub let vdd = Signal(x=0, y=8);
    pub let vss = Signal(x=0, y=-8);

    let mp = device(DeviceKind::Pmos, signals(4), "pch", w=pw, l=0.15, x=0, y=4);
    let mn = device(DeviceKind::Nmos, signals(4), "nch", w=nw, l=0.15, x=0, y=-4);

    connect(a, mp.g, route=[(-4, 0), (-4, 4)]);
    connect(a, mn.g, route=[(-4, 0), (-4, -4)]);
    connect(out, mp.d, route=[(0, 0)]);
    connect(out, mn.d, route=[(0, 0)]);
    connect(vdd, mp.s, route=[]);
    connect(vdd, mp.b, route=[(2, 8)]);
    connect(vss, mn.s, route=[]);
    connect(vss, mn.b, route=[(2, -8)]);

    polygon([(-2., 2.), (-2., -2.), (2., 0.)]);
    circle(x=2.25, y=0., r=0.25);
    line([(-4., 0.), (-2., 0.)]);
    line([(2.5, 0.), (4., 0.)]);
    line([(0., 1.), (0., 4.)]);
    line([(0., -1.), (0., -4.)]);
    pin(a, x=-4, y=0);
    pin(out, x=4, y=0);
    pin(vdd, x=0, y=4, label=true);
    pin(vss, x=0, y=-4, label=true);
    inst_name(x=2., y=2.);
    param("pw", pw, x=2., y=-2.);
    param("nw", nw, x=2., y=-3.);
}

cell parent() {
    pub let in0 = Signal(x=-10, y=0);
    pub let out = Signal();
    let inv = std::schematic::inst(device_inv(2., 1.), x=0, y=0);
    let other = std::schematic::inst(device_inv(2., 1.), angle=90);
    connect(in0, inv.a, route=[]);
    connect(inv.out, other.a);
    connect(other.out, out);
}
"#;

    fn scene_of(source: &str, cell: &str, args: Vec<CellArg>) -> Scene {
        let data = compile_valid(source, cell, args);
        build_scene(&data, data.top, &DisplayPrefs::default())
    }

    #[test]
    fn the_device_inverter_is_drawn_as_its_source_says() {
        let scene = scene_of(
            DEVICE_INVERTER,
            "device_inv",
            vec![CellArg::Float(2.), CellArg::Float(1.)],
        );
        assert!(!scene.empty);
        assert_eq!(scene.nets, ["a", "out", "vdd", "vss"]);
        let names = scene
            .elements
            .iter()
            .map(|e| e.name.as_str())
            .collect::<Vec<_>>();
        assert_eq!(names, ["Mmp", "Mmn"]);
        let mp = &scene.elements[0];
        assert_eq!(mp.placement.position, Some((0, 4)));
        assert!(!mp.auto_placed && mp.editable);
        // The PMOS symbol has its source on top, facing `vdd`.
        let pins = mp
            .pins
            .iter()
            .map(|pin| (pin.name.as_str(), pin.position))
            .collect::<Vec<_>>();
        assert_eq!(
            pins,
            [
                ("d", Some((0, 2))),
                ("g", Some((-2, 4))),
                ("s", Some((0, 6))),
                ("b", Some((2, 4))),
            ]
        );
        assert_eq!(
            mp.params,
            [
                ("w".to_owned(), "2".to_owned()),
                ("l".to_owned(), "0.15".to_owned())
            ]
        );
        let mn = &scene.elements[1];
        let pins = mn.pins.iter().map(|pin| pin.position).collect::<Vec<_>>();
        assert_eq!(
            pins,
            [Some((0, -2)), Some((-2, -4)), Some((0, -6)), Some((2, -4))]
        );

        // Four placed ports, no labels, every connection routed.
        let ports = scene
            .ports
            .iter()
            .map(|port| (port.name.as_str(), port.position))
            .collect::<Vec<_>>();
        assert_eq!(
            ports,
            [
                ("a", (-6, 0)),
                ("out", (4, 0)),
                ("vdd", (0, 8)),
                ("vss", (0, -8))
            ]
        );
        assert!(scene.labels.is_empty());
        assert_eq!(scene.wires.len(), 8);
        assert!(scene.undrawn.is_empty());
        assert_eq!(
            scene.wires[0].points,
            [(-6., 0.), (-4., 0.), (-4., 4.), (-2., 4.)]
        );
        assert_eq!(scene.wires[4].points, [(0., 8.), (0., 6.)]);
        // The two `a` wires share a segment, drawn once with a junction.
        let a_segments = scene.segments.iter().filter(|s| s.net == 0).count();
        assert_eq!(a_segments, 4, "{:?}", scene.segments);
        assert!(scene.junctions.contains(&Junction {
            net: 0,
            at: (-4., 0.)
        }));
        assert!(scene.junctions.contains(&Junction {
            net: 1,
            at: (0., 0.)
        }));
        assert!(scene.flylines.is_empty(), "{:?}", scene.flylines);
        assert!(scene.conflicts.is_empty(), "{:?}", scene.conflicts);
        assert_eq!(scene.bounds, Some((-6., -8., 4., 8.)));
    }

    #[test]
    fn a_parent_draws_the_child_with_its_symbol_and_flylines() {
        let scene = scene_of(DEVICE_INVERTER, "parent", Vec::new());
        let inv = &scene.elements[0];
        assert_eq!(inv.name, "Xinv");
        assert_eq!(inv.cell_name.as_deref(), Some("device_inv"));
        // The child's own symbol: pins where its `pin` calls put them.
        let pins = inv
            .pins
            .iter()
            .map(|pin| (pin.name.as_str(), pin.position))
            .collect::<Vec<_>>();
        assert_eq!(
            pins,
            [
                ("a", Some((-4, 0))),
                ("out", Some((4, 0))),
                ("vdd", Some((0, 4))),
                ("vss", Some((0, -4))),
            ]
        );
        assert!(inv.symbol.texts.iter().any(|t| t.text == "Xinv"));
        assert!(inv.symbol.texts.iter().any(|t| t.text == "pw=2"));
        assert!(
            inv.symbol
                .texts
                .iter()
                .any(|t| t.text == "vdd" && t.anchor == (0.5, 3.))
        );

        // The second instance is unplaced: it takes slot 1 and is rotated.
        let other = &scene.elements[1];
        assert!(other.auto_placed);
        assert_eq!(other.placement.angle, geometry::transform::Rotation::R90);
        let pitch = slot_pitch(8.);
        assert_eq!(other.placement.position, Some(slot_position(1, pitch)));
        // Rotated 90 degrees, `a` at (-4, 0) lands below the origin.
        let (ox, oy) = other.placement.position.unwrap();
        assert_eq!(other.pins[0].position, Some((ox, oy - 4)));

        // `in0` is placed; `out` is not, so it goes to the right column.
        assert_eq!(scene.ports[0].name, "in0");
        assert!(!scene.ports[0].auto_placed);
        assert_eq!(scene.ports[1].name, "out");
        assert!(scene.ports[1].auto_placed);
        // One routed wire, two connections left as flylines.
        assert_eq!(scene.wires.len(), 1);
        assert_eq!(scene.flylines.len(), 2, "{:?}", scene.flylines);
        let out_flyline = scene
            .flylines
            .iter()
            .find(|f| f.net == scene.ports[1].net)
            .expect("the unrouted `out` connection");
        assert!(
            out_flyline.from
                == (
                    scene.ports[1].position.0 as f64,
                    scene.ports[1].position.1 as f64
                )
                || out_flyline.to
                    == (
                        scene.ports[1].position.0 as f64,
                        scene.ports[1].position.1 as f64
                    )
        );
        let hidden = build_scene(
            &compile_valid(DEVICE_INVERTER, "parent", Vec::new()),
            compile_valid(DEVICE_INVERTER, "parent", Vec::new()).top,
            &DisplayPrefs {
                show_flylines: false,
            },
        );
        assert!(hidden.flylines.is_empty());
    }

    #[test]
    fn a_cell_without_a_symbol_is_drawn_as_a_generated_box() {
        let source = r#"
use std::schematic::Signal;
cell leaf() {
    pub let a = Signal();
    pub let vdd = Signal();
    pub let vss = Signal();
    pub let out = Signal();
}
cell top() {
    let l = std::schematic::inst(leaf(), x=0, y=0);
    let m = std::schematic::inst(leaf());
}
"#;
        let scene = scene_of(source, "top", Vec::new());
        let l = &scene.elements[0];
        assert_eq!(
            l.pins.iter().map(|p| p.position).collect::<Vec<_>>(),
            [Some((0, -2)), Some((2, 0)), Some((2, -4)), Some((4, -2))]
        );
        assert!(l.symbol.texts.iter().any(|t| t.text == "leaf"));
        assert!(l.symbol.texts.iter().any(|t| t.text == "Xl"));
        // Nothing connects the two instances, so no flylines.
        assert!(scene.flylines.is_empty());
        assert!(scene.ports.is_empty());
    }

    #[test]
    fn empty_and_partial_schematics_are_scenes_too() {
        let empty = scene_of("cell top() {}", "top", Vec::new());
        assert!(empty.empty && empty.bounds.is_none());

        let source = r#"
use std::schematic::{DeviceKind, Signal, connect, device, signals};
cell top() {
    pub let a = Signal(x=0, y=0);
    let r = device(DeviceKind::Res, signals(2), "", value=1., x=4, y=0);
    connect(a, r.p, route=[(0, 2)]);
    let bad = device(DeviceKind::Res, signals(2), "", x=8, y=0);
}
"#;
        let output = compile(source, "top", Vec::new());
        let CompileOutput::ExecErrors(output) = output else {
            panic!("{output:#?}");
        };
        let data = output.output.expect("a partial output");
        let scene = build_scene(&data, data.top, &DisplayPrefs::default());
        // The invalid device still has a handle, so it is drawn; the valid
        // one has its wire.
        assert_eq!(scene.elements.len(), 2);
        assert_eq!(scene.wires.len(), 1);
    }

    #[test]
    fn routes_to_unplaced_signals_are_not_drawn() {
        let source = r#"
use std::schematic::{Signal, connect};
cell top() {
    pub let a = Signal(x=0, y=0);
    let hidden = Signal();
    connect(a, hidden, route=[(2, 0)]);
    let l = Signal(x=4, y=4);
    connect(l, hidden);
}
"#;
        let scene = scene_of(source, "top", Vec::new());
        assert_eq!(scene.wires.len(), 0);
        assert_eq!(scene.undrawn.len(), 1);
        assert_eq!(
            scene.undrawn[0].reason,
            "ends at a signal that is not placed"
        );
        // The placed internal signal is a label named after its net, and the
        // net's port pin and label are joined by name.
        assert_eq!(scene.labels.len(), 1);
        assert_eq!(scene.labels[0].name, "a");
        assert!(scene.flylines.is_empty());
    }
}
