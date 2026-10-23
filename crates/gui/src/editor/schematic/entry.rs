//! The entry tools: the wire being drawn, where ports and labels attach, and
//! the requests each gesture sends.

use argonc::compile::NodeIdx;
use protocol::rpc::{
    AddSignalRequest, ConnectRequest, KeywordEdit, PlaceRequest, PlacementEdit, SchematicTarget,
};

use super::hit::Hit;
use super::scene::{Overrides, Scene, UnroutedConnection};
use super::stretch::{self, GridPt};
use super::symbols::Pt;
use super::tools::{placement_edits, route_text};
use super::wires::segment_distance;

/// What draws the pin of a node.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Owner {
    Element(usize),
    Port(usize),
    Label(usize),
}

/// The item whose pin is `node`'s.
pub fn node_owner(scene: &Scene, node: NodeIdx) -> Option<Owner> {
    if let Some(index) = scene.ports.iter().position(|port| port.node == node) {
        return Some(Owner::Port(index));
    }
    if let Some(index) = scene.labels.iter().position(|label| label.node == node) {
        return Some(Owner::Label(index));
    }
    scene
        .elements
        .iter()
        .position(|element| element.pins.iter().any(|pin| pin.node == node))
        .map(Owner::Element)
}

/// The pin of `node`, when it is drawn.
pub fn node_pin(scene: &Scene, node: NodeIdx) -> Option<GridPt> {
    match node_owner(scene, node)? {
        Owner::Port(index) => Some(scene.ports[index].position),
        Owner::Label(index) => Some(scene.labels[index].position),
        Owner::Element(index) => scene.elements[index]
            .pins
            .iter()
            .find(|pin| pin.node == node)
            .and_then(|pin| pin.position),
    }
}

/// Where a wire starts or ends: a node, reached at its pin or along one of
/// its wires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireEnd {
    pub node: NodeIdx,
    /// How the cell body names the node.
    pub path: String,
    pub pin: GridPt,
    /// The points from `pin` (excluded) to where the drawn wire attaches
    /// (included), along an existing wire. Empty when it attaches at the pin.
    pub approach: Vec<GridPt>,
}

impl WireEnd {
    /// Where the drawn wire attaches.
    pub fn attach(&self) -> GridPt {
        self.approach.last().copied().unwrap_or(self.pin)
    }
}

/// Why something under the pointer cannot end a wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EndError {
    /// The node has no path from the cell body, so no statement can name it.
    NoPath { hit: Hit, name: String },
    /// Not a pin, port, label, or wire.
    NotAnEnd,
}

fn grid(point: Pt) -> GridPt {
    (point.0.round() as i64, point.1.round() as i64)
}

fn distance(a: Pt, b: Pt) -> f64 {
    (a.0 - b.0).hypot(a.1 - b.1)
}

/// The grid point of the wire nearest `point`, with the index of its segment.
fn nearest_on_wire(points: &[Pt], point: Pt) -> Option<(usize, GridPt)> {
    let (index, _) = points
        .windows(2)
        .enumerate()
        .map(|(index, pair)| (index, segment_distance(point, pair[0], pair[1])))
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))?;
    let (a, b) = (points[index], points[index + 1]);
    let (dx, dy) = (b.0 - a.0, b.1 - a.1);
    let length = dx * dx + dy * dy;
    let t = if length == 0. {
        0.
    } else {
        (((point.0 - a.0) * dx + (point.1 - a.1) * dy) / length).clamp(0., 1.)
    };
    Some((index, grid((a.0 + t * dx, a.1 + t * dy))))
}

/// The wire end that `hit` gives for a click at `point`.
pub fn wire_end(scene: &Scene, hit: Hit, point: Pt) -> Result<WireEnd, EndError> {
    let path = |node: NodeIdx, name: String| {
        scene.node_paths[node as usize]
            .clone()
            .ok_or(EndError::NoPath { hit, name })
    };
    match hit {
        Hit::Pin { element, terminal } => {
            let element = scene.elements.get(element).ok_or(EndError::NotAnEnd)?;
            let pin = element.pins.get(terminal).ok_or(EndError::NotAnEnd)?;
            let position = pin.position.ok_or(EndError::NotAnEnd)?;
            Ok(WireEnd {
                node: pin.node,
                path: path(pin.node, format!("{}.{}", element.name, pin.name))?,
                pin: position,
                approach: Vec::new(),
            })
        }
        Hit::Port(index) => {
            let port = scene.ports.get(index).ok_or(EndError::NotAnEnd)?;
            Ok(WireEnd {
                node: port.node,
                path: path(port.node, port.name.clone())?,
                pin: port.position,
                approach: Vec::new(),
            })
        }
        Hit::Label(index) => {
            let label = scene.labels.get(index).ok_or(EndError::NotAnEnd)?;
            Ok(WireEnd {
                node: label.node,
                path: path(label.node, label.name.clone())?,
                pin: label.position,
                approach: Vec::new(),
            })
        }
        Hit::Wire(index) => {
            let wire = scene.wires.get(index).ok_or(EndError::NotAnEnd)?;
            let (segment, at) = nearest_on_wire(&wire.points, point).ok_or(EndError::NotAnEnd)?;
            let at_f = (at.0 as f64, at.1 as f64);
            // The nearer end by length along the wire.
            let to_start = wire.points[..=segment]
                .windows(2)
                .map(|pair| distance(pair[0], pair[1]))
                .sum::<f64>()
                + distance(wire.points[segment], at_f);
            let to_end = wire.points[segment + 1..]
                .windows(2)
                .map(|pair| distance(pair[0], pair[1]))
                .sum::<f64>()
                + distance(at_f, wire.points[segment + 1]);
            let (node, pin, approach) = if to_start <= to_end {
                let mut approach = wire.points[1..=segment]
                    .iter()
                    .map(|p| grid(*p))
                    .collect::<Vec<_>>();
                approach.push(at);
                (wire.nodes[0], grid(wire.points[0]), approach)
            } else {
                let last = wire.points.len() - 1;
                let mut approach = wire.points[segment + 1..last]
                    .iter()
                    .rev()
                    .map(|p| grid(*p))
                    .collect::<Vec<_>>();
                approach.push(at);
                (wire.nodes[1], grid(wire.points[last]), approach)
            };
            let name = scene
                .nets
                .get(wire.net as usize)
                .cloned()
                .unwrap_or_default();
            Ok(WireEnd {
                node,
                path: path(node, name)?,
                pin,
                approach,
            })
        }
        Hit::Element(_) | Hit::Flyline(_) => Err(EndError::NotAnEnd),
    }
}

/// The wire end among `hits`, nearest kinds first.
pub fn end_among(scene: &Scene, hits: &[Hit], point: Pt) -> Option<Result<WireEnd, EndError>> {
    hits.iter()
        .find(|hit| {
            matches!(
                hit,
                Hit::Pin { .. } | Hit::Port(_) | Hit::Label(_) | Hit::Wire(_)
            )
        })
        .map(|hit| wire_end(scene, *hit, point))
}

/// The corner of an L from `from` to `to`: horizontal first, or vertical
/// first when `flipped`. `None` when the points share an axis.
pub fn elbow(from: GridPt, to: GridPt, flipped: bool) -> Option<GridPt> {
    if from.0 == to.0 || from.1 == to.1 {
        return None;
    }
    Some(if flipped {
        (from.0, to.1)
    } else {
        (to.0, from.1)
    })
}

/// A wire being drawn from a pin or wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireDraft {
    pub start: WireEnd,
    /// The corners each click added: the bend, when there was one, and the
    /// clicked point.
    pub clicks: Vec<Vec<GridPt>>,
    /// Whether the segment to the pointer bends vertically first.
    pub flipped: bool,
}

impl WireDraft {
    pub fn new(start: WireEnd) -> Self {
        Self {
            start,
            clicks: Vec::new(),
            flipped: false,
        }
    }

    /// The last fixed point.
    pub fn last(&self) -> GridPt {
        self.clicks
            .last()
            .and_then(|click| click.last().copied())
            .unwrap_or_else(|| self.start.attach())
    }

    fn corners(&self) -> Vec<GridPt> {
        self.clicks.iter().flatten().copied().collect()
    }

    /// Fixes a corner at `point`, bending once from the last point.
    pub fn click(&mut self, point: GridPt) {
        let last = self.last();
        if point == last {
            return;
        }
        let mut click = Vec::new();
        if let Some(bend) = elbow(last, point, self.flipped) {
            click.push(bend);
        }
        click.push(point);
        self.clicks.push(click);
    }

    /// Drops the last click. False when there is none.
    pub fn undo_click(&mut self) -> bool {
        self.clicks.pop().is_some()
    }

    /// The polyline from the start pin to `cursor`.
    pub fn preview(&self, cursor: GridPt) -> Vec<GridPt> {
        let mut points = vec![self.start.pin];
        points.extend(self.start.approach.iter().copied());
        points.extend(self.corners());
        let last = self.last();
        if let Some(bend) = elbow(last, cursor, self.flipped) {
            points.push(bend);
        }
        points.push(cursor);
        stretch::simplify(&points)
    }

    /// The route of the wire from the start to `end`.
    pub fn route_to(&self, end: &WireEnd) -> Vec<GridPt> {
        let mut points = self.preview(end.attach());
        points.extend(end.approach.iter().rev().skip(1).copied());
        points.push(end.pin);
        stretch::corners(&stretch::simplify(&points))
    }

    /// The route of the wire from the start to a new label at `at`.
    pub fn route_to_point(&self, at: GridPt) -> Vec<GridPt> {
        stretch::corners(&self.preview(at))
    }
}

/// The edits that fix the position of every unplaced element or port that
/// draws one of `nodes`, so a wire written to it stays where it was drawn.
pub fn placements_for(scene: &Scene, nodes: &[NodeIdx]) -> Vec<PlacementEdit> {
    let mut overrides = Overrides::default();
    for node in nodes {
        match node_owner(scene, *node) {
            Some(Owner::Element(index)) => {
                let element = &scene.elements[index];
                if let Some(span) = element
                    .span
                    .clone()
                    .filter(|_| element.auto_placed && element.editable)
                {
                    overrides.placements.insert(span, element.placement);
                }
            }
            Some(Owner::Port(index)) => {
                let port = &scene.ports[index];
                if port.auto_placed && port.editable {
                    overrides
                        .placements
                        .insert(port.span.clone(), port.placement);
                }
            }
            Some(Owner::Label(_)) | None => {}
        }
    }
    placement_edits(scene, &overrides)
}

/// The editable `connect` that joins exactly `a` and `b` without a route.
pub fn unrouted_between(scene: &Scene, a: NodeIdx, b: NodeIdx) -> Option<&UnroutedConnection> {
    scene.unrouted.iter().find(|connection| {
        connection.editable && (connection.nodes == [a, b] || connection.nodes == [b, a])
    })
}

/// What writing a wire sends: a new `connect`, or a route on the one that
/// already joins its ends.
#[derive(Debug, Clone, PartialEq)]
pub enum WireRequest {
    Connect(ConnectRequest),
    Route(PlaceRequest),
}

/// The request that writes a wire from `a` to `b` with `route`, fixing the
/// positions of what it touches.
fn wire_request_between(
    scene: &Scene,
    target: SchematicTarget,
    a: &WireEnd,
    b: &WireEnd,
    route: Vec<GridPt>,
) -> WireRequest {
    let mut placements = placements_for(scene, &[a.node, b.node]);
    match unrouted_between(scene, a.node, b.node) {
        Some(connection) => {
            let route = if connection.nodes[0] == a.node {
                route
            } else {
                route.into_iter().rev().collect()
            };
            placements.push(PlacementEdit {
                call: connection.span.clone(),
                keywords: vec![KeywordEdit {
                    name: "route".into(),
                    value: Some(route_text(&route)),
                }],
            });
            WireRequest::Route(PlaceRequest {
                base_revision: target.base_revision,
                edits: placements,
            })
        }
        None => WireRequest::Connect(ConnectRequest {
            target,
            a: a.path.clone(),
            b: b.path.clone(),
            route: Some(route),
            placements,
        }),
    }
}

/// The request that writes `draft` ending at `end`.
pub fn wire_request(
    scene: &Scene,
    target: SchematicTarget,
    draft: &WireDraft,
    end: &WireEnd,
) -> WireRequest {
    let route = draft.route_to(end);
    wire_request_between(scene, target, &draft.start, end, route)
}

/// The wire that replaces flyline `index`: one bend between its pins, or a
/// straight run. `None` when an end has no pin or no path.
pub fn flyline_request(
    scene: &Scene,
    target: SchematicTarget,
    index: usize,
    flipped: bool,
) -> Option<WireRequest> {
    let flyline = scene.flylines.get(index)?;
    let [Some(a), Some(b)] = flyline.nodes else {
        return None;
    };
    let end = |node: NodeIdx| -> Option<WireEnd> {
        Some(WireEnd {
            node,
            path: scene.node_paths[node as usize].clone()?,
            pin: node_pin(scene, node)?,
            approach: Vec::new(),
        })
    };
    let (a, b) = (end(a)?, end(b)?);
    let route = elbow(a.pin, b.pin, flipped).into_iter().collect();
    Some(wire_request_between(scene, target, &a, &b, route))
}

/// The polyline of the wire that would replace flyline `index`.
pub fn flyline_preview(scene: &Scene, index: usize, flipped: bool) -> Option<Vec<GridPt>> {
    let flyline = scene.flylines.get(index)?;
    let (from, to) = (grid(flyline.from), grid(flyline.to));
    let mut points = vec![from];
    points.extend(elbow(from, to, flipped));
    points.push(to);
    Some(points)
}

/// The request that ends `draft` at a new net label `name` at `at`.
pub fn label_request(
    scene: &Scene,
    target: SchematicTarget,
    draft: &WireDraft,
    name: String,
    at: GridPt,
) -> AddSignalRequest {
    // The statement connects the label to the pin, so the route runs from
    // the label.
    let route = draft.route_to_point(at).into_iter().rev().collect();
    AddSignalRequest {
        target,
        name,
        port: false,
        at,
        connect_to: Some(draft.start.path.clone()),
        route: Some(route),
        placements: placements_for(scene, &[draft.start.node]),
    }
}

/// The request that adds a port or label `name` at `point`, on the pin or
/// wire among `hits` when there is one. A label needs one.
pub fn signal_request(
    scene: &Scene,
    target: SchematicTarget,
    name: String,
    port: bool,
    hits: &[Hit],
    point: Pt,
) -> Result<AddSignalRequest, EndError> {
    let end = match end_among(scene, hits, point) {
        Some(end) => Some(end?),
        None if port => None,
        None => return Err(EndError::NotAnEnd),
    };
    Ok(AddSignalRequest {
        target,
        name,
        port,
        at: end.as_ref().map_or_else(|| grid(point), WireEnd::attach),
        connect_to: end.as_ref().map(|end| end.path.clone()),
        route: None,
        placements: end.map_or_else(Vec::new, |end| placements_for(scene, &[end.node])),
    })
}

#[cfg(test)]
mod tests {
    use argonc::compile::CellArg;

    use super::*;
    use crate::editor::schematic::scene::tests::{DEVICE_INVERTER, compile_valid};
    use crate::editor::schematic::scene::{DisplayPrefs, build_scene};

    fn inverter_scene() -> Scene {
        let data = compile_valid(
            DEVICE_INVERTER,
            "device_inv",
            vec![CellArg::Float(2.), CellArg::Float(1.)],
        );
        build_scene(&data, data.top, &DisplayPrefs::default())
    }

    fn target() -> SchematicTarget {
        SchematicTarget {
            base_revision: 1,
            cell: argonc::ast::Span {
                path: std::path::Path::new("/virtual/lib.ar").into(),
                span: cfgrammar::Span::new(0, 0),
            },
        }
    }

    #[test]
    fn elbows_bend_horizontally_first_unless_flipped() {
        assert_eq!(elbow((0, 0), (4, 2), false), Some((4, 0)));
        assert_eq!(elbow((0, 0), (4, 2), true), Some((0, 2)));
        assert_eq!(elbow((0, 0), (4, 0), false), None);
        assert_eq!(elbow((0, 0), (0, 3), true), None);
    }

    #[test]
    fn a_draft_from_a_pin_to_a_pin_writes_a_connect_with_its_bends() {
        let scene = inverter_scene();
        // The output port `out` and the drain of `mp`.
        let out = scene.ports.iter().position(|p| p.name == "out").unwrap();
        let start = wire_end(&scene, Hit::Port(out), (0., 0.)).unwrap();
        assert_eq!(start.path, "out");
        let mut draft = WireDraft::new(start);
        let pin = (
            scene.ports[out].position.0 - 2,
            scene.ports[out].position.1 + 3,
        );
        draft.click(pin);
        assert_eq!(draft.clicks.len(), 1);
        assert_eq!(draft.clicks[0].len(), 2, "a bend and the click");
        assert!(draft.undo_click());
        assert!(!draft.undo_click());

        let mp = scene.elements.iter().position(|e| e.name == "Mmp").unwrap();
        let drain = scene.elements[mp]
            .pins
            .iter()
            .position(|pin| pin.name == "d")
            .unwrap();
        let end = wire_end(
            &scene,
            Hit::Pin {
                element: mp,
                terminal: drain,
            },
            (0., 0.),
        )
        .unwrap();
        assert_eq!(end.path, "mp.d");
        let request = wire_request(&scene, target(), &draft, &end);
        let WireRequest::Connect(connect) = request else {
            panic!("a new wire between unconnected pins is a connect");
        };
        assert_eq!(connect.a, "out");
        assert_eq!(connect.b, "mp.d");
        let route = connect.route.unwrap();
        // Every segment is orthogonal.
        let mut points = vec![draft.start.pin];
        points.extend(route.iter().copied());
        points.push(end.pin);
        for pair in points.windows(2) {
            assert!(
                pair[0].0 == pair[1].0 || pair[0].1 == pair[1].1,
                "{points:?}"
            );
        }
        assert!(connect.placements.is_empty(), "everything is placed");
    }

    #[test]
    fn ending_on_a_wire_continues_along_it_to_the_nearer_end() {
        let scene = inverter_scene();
        // `connect(a, mp.g, route=[(-4, 0), (-4, 4)])`: from `a` at (-6, 0) to
        // the gate at (-2, 4).
        let gate_wire = scene
            .wires
            .iter()
            .position(|wire| wire.points.first() == Some(&(-6., 0.)) && wire.points.len() == 4)
            .unwrap();
        let end = wire_end(&scene, Hit::Wire(gate_wire), (-4.1, 3.)).unwrap();
        assert_eq!(end.attach(), (-4, 3));
        assert_eq!(end.path, "mp.g", "the gate end is nearer");
        assert_eq!(end.pin, (-2, 4));
        assert_eq!(end.approach, vec![(-4, 4), (-4, 3)]);

        let near_a = wire_end(&scene, Hit::Wire(gate_wire), (-5., 0.)).unwrap();
        assert_eq!(near_a.path, "a");
        assert_eq!(near_a.approach, vec![(-5, 0)]);
    }

    #[test]
    fn a_pin_without_a_path_is_refused_with_its_name() {
        let data = compile_valid(
            "use std::schematic::{Signal, DeviceKind, device, signals};
             cell top() {
                 pub let a = Signal(x=0, y=0);
                 for i in std::range(1) {
                     let r = device(DeviceKind::Res, signals(2), \"\", value=1.);
                 }
             }",
            "top",
            vec![],
        );
        let scene = build_scene(&data, data.top, &DisplayPrefs::default());
        let error = wire_end(
            &scene,
            Hit::Pin {
                element: 0,
                terminal: 0,
            },
            (0., 0.),
        )
        .unwrap_err();
        assert!(
            matches!(&error, EndError::NoPath { name, .. } if name == "Rr.p"),
            "{error:?}"
        );
    }

    #[test]
    fn a_label_request_runs_its_route_from_the_label() {
        let scene = inverter_scene();
        let out = scene.ports.iter().position(|p| p.name == "out").unwrap();
        let start = wire_end(&scene, Hit::Port(out), (0., 0.)).unwrap();
        let pin = start.pin;
        let mut draft = WireDraft::new(start);
        draft.click((pin.0 + 2, pin.1 + 2));
        let request = label_request(
            &scene,
            target(),
            &draft,
            "n1".into(),
            (pin.0 + 2, pin.1 + 4),
        );
        assert!(!request.port);
        assert_eq!(request.connect_to.as_deref(), Some("out"));
        assert_eq!(request.at, (pin.0 + 2, pin.1 + 4));
        // From the label down to the bend beside the pin.
        assert_eq!(request.route, Some(vec![(pin.0 + 2, pin.1)]));
    }

    #[test]
    fn a_port_on_a_pin_connects_without_a_route_and_fixes_an_unplaced_element() {
        let data = compile_valid(
            "use std::schematic::{Signal, DeviceKind, device, signals};
             cell top() {
                 let r = device(DeviceKind::Res, signals(2), \"\", value=1.);
             }",
            "top",
            vec![],
        );
        let scene = build_scene(&data, data.top, &DisplayPrefs::default());
        assert!(scene.elements[0].auto_placed);
        let hits = vec![Hit::Pin {
            element: 0,
            terminal: 0,
        }];
        let request = signal_request(&scene, target(), "in".into(), true, &hits, (0., 0.)).unwrap();
        assert!(request.port);
        assert_eq!(request.connect_to.as_deref(), Some("r.p"));
        assert_eq!(request.route, None);
        assert_eq!(request.at, scene.elements[0].pins[0].position.unwrap());
        assert_eq!(request.placements.len(), 1, "the resistor gets x= and y=");
        // A label needs a pin or wire.
        assert_eq!(
            signal_request(&scene, target(), "n".into(), false, &[], (3., 3.)).unwrap_err(),
            EndError::NotAnEnd
        );
        // A port in empty space is just a port.
        let lone = signal_request(&scene, target(), "n".into(), true, &[], (3.2, 2.8)).unwrap();
        assert_eq!(lone.at, (3, 3));
        assert_eq!(lone.connect_to, None);
    }

    #[test]
    fn wiring_a_flyline_routes_its_existing_connect_when_it_can() {
        let data = compile_valid(
            "use std::schematic::{Signal, DeviceKind, connect, device, signals};
             cell top() {
                 pub let a = Signal(x=-6, y=0);
                 let r = device(DeviceKind::Res, signals(2), \"\", value=1., x=0, y=4);
                 connect(a, r.p);
             }",
            "top",
            vec![],
        );
        let scene = build_scene(&data, data.top, &DisplayPrefs::default());
        assert_eq!(scene.flylines.len(), 1);
        let request = flyline_request(&scene, target(), 0, false).unwrap();
        let WireRequest::Route(place) = request else {
            panic!("the connect exists, so it gets a route");
        };
        assert_eq!(place.edits.len(), 1);
        assert_eq!(place.edits[0].keywords[0].name, "route");
        let preview = flyline_preview(&scene, 0, false).unwrap();
        assert_eq!(preview.len(), 3, "one bend");
    }
}
