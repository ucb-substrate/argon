//! The schematic view's gestures: what a drag moves, and the preview and
//! source edits a gesture produces.

use std::collections::HashMap;
use std::sync::Arc;

use analyzer::rpc::{KeywordEdit, PlacementEdit};
use argonc::compile::{NodeIdx, Placement};
use geometry::transform::Rotation;

use super::hit::Hit;
use super::scene::{Overrides, Scene};
use super::stretch::{self, GridPt};
use super::symbols::{Pt, place_symbol};

/// What a drag in the select tool moves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DragKind {
    /// The selected elements, ports, and labels.
    Items,
    /// One segment of a wire, sideways.
    Segment { wire: usize, segment: usize },
}

/// A drag in progress between world points, over the scene it began on.
#[derive(Debug, Clone)]
pub struct Drag {
    pub kind: DragKind,
    pub start: Pt,
    pub current: Pt,
    /// The scene without the drag's preview.
    pub base: Arc<Scene>,
    /// The selection in `base`.
    pub selection: Vec<Hit>,
}

impl Drag {
    /// How far the pointer moved, in whole grid units.
    pub fn delta(&self) -> GridPt {
        grid((self.current.0 - self.start.0, self.current.1 - self.start.1))
    }

    /// The preview of the drag, or `None` until it moves a grid unit.
    pub fn preview(&self) -> Option<Overrides> {
        let delta = self.delta();
        if delta == (0, 0) {
            return None;
        }
        Some(match self.kind {
            DragKind::Items => move_overrides(&self.base, &self.selection, delta),
            DragKind::Segment { wire, segment } => {
                segment_overrides(&self.base, wire, segment, delta)
            }
        })
    }
}

fn grid(point: (f64, f64)) -> GridPt {
    (point.0.round() as i64, point.1.round() as i64)
}

fn add(a: GridPt, b: GridPt) -> GridPt {
    (a.0 + b.0, a.1 + b.1)
}

/// Rewrites the routes of the editable wires whose ends moved by `moved`.
/// A locked route is left alone, so its ends just follow the pins.
fn stretch_wires(scene: &Scene, moved: &HashMap<NodeIdx, GridPt>, overrides: &mut Overrides) {
    for wire in &scene.wires {
        if !wire.editable {
            continue;
        }
        let [a, b] = wire.nodes;
        let points = wire.points.iter().map(|p| grid(*p)).collect::<Vec<_>>();
        let rewritten = match (moved.get(&a).copied(), moved.get(&b).copied()) {
            (None, None) => continue,
            (Some(start), Some(end)) if start == end => stretch::move_both(&points, start),
            (Some(start), Some(end)) => {
                stretch::move_end(&stretch::move_start(&points, start), end)
            }
            (Some(start), None) => stretch::move_start(&points, start),
            (None, Some(end)) => stretch::move_end(&points, end),
        };
        overrides
            .routes
            .insert(wire.span.clone(), Some(stretch::corners(&rewritten)));
    }
}

/// The preview of moving the selected elements, ports, and labels by
/// `delta`, with their wires stretched.
pub fn move_overrides(scene: &Scene, selected: &[Hit], delta: GridPt) -> Overrides {
    let mut overrides = Overrides::default();
    if delta == (0, 0) {
        return overrides;
    }
    let mut moved = HashMap::new();
    for hit in selected {
        match *hit {
            Hit::Element(index) | Hit::Pin { element: index, .. } => {
                let Some(element) = scene.elements.get(index) else {
                    continue;
                };
                let Some(span) = element.span.clone().filter(|_| element.editable) else {
                    continue;
                };
                let origin = element.placement.position.unwrap_or((0, 0));
                overrides.placements.insert(
                    span,
                    Placement {
                        position: Some(add(origin, delta)),
                        ..element.placement
                    },
                );
                for pin in &element.pins {
                    moved.insert(pin.node, delta);
                }
            }
            Hit::Port(index) => {
                let Some(port) = scene.ports.get(index).filter(|port| port.editable) else {
                    continue;
                };
                overrides.placements.insert(
                    port.span.clone(),
                    Placement {
                        position: Some(add(port.position, delta)),
                        ..port.placement
                    },
                );
                moved.insert(port.node, delta);
            }
            Hit::Label(index) => {
                let Some(label) = scene.labels.get(index).filter(|label| label.editable) else {
                    continue;
                };
                overrides.placements.insert(
                    label.span.clone(),
                    Placement {
                        position: Some(add(label.position, delta)),
                        ..label.placement
                    },
                );
                moved.insert(label.node, delta);
            }
            Hit::Wire(_) | Hit::Flyline(_) => {}
        }
    }
    stretch_wires(scene, &moved, &mut overrides);
    overrides
}

/// The preview of rotating the selection by 90 degrees, or of mirroring it
/// when `mirror`, each element about its own origin, with wires stretched
/// to the pins' new places.
pub fn orient_overrides(scene: &Scene, selected: &[Hit], mirror: bool) -> Overrides {
    let mut overrides = Overrides::default();
    let mut moved = HashMap::new();
    let reorient = |placement: Placement| {
        if mirror {
            Placement {
                reflect: !placement.reflect,
                ..placement
            }
        } else {
            Placement {
                angle: placement.angle + Rotation::R90,
                ..placement
            }
        }
    };
    for hit in selected {
        match *hit {
            Hit::Element(index) | Hit::Pin { element: index, .. } => {
                let Some(element) = scene.elements.get(index) else {
                    continue;
                };
                let Some(span) = element.span.clone().filter(|_| element.editable) else {
                    continue;
                };
                let placement = reorient(element.placement);
                let placed = place_symbol(
                    &element.raw_symbol,
                    &placement,
                    &element.name,
                    &element.pin_names,
                );
                for (terminal, pin) in element.pins.iter().enumerate() {
                    if let (Some(old), Some(Some(new))) = (pin.position, placed.pins.get(terminal))
                    {
                        moved.insert(pin.node, (new.0 - old.0, new.1 - old.1));
                    }
                }
                overrides.placements.insert(span, placement);
            }
            Hit::Port(index) => {
                if let Some(port) = scene.ports.get(index).filter(|port| port.editable) {
                    let placement = Placement {
                        angle: port.placement.angle + Rotation::R90,
                        ..port.placement
                    };
                    overrides.placements.insert(port.span.clone(), placement);
                }
            }
            Hit::Label(index) => {
                if let Some(label) = scene.labels.get(index).filter(|label| label.editable) {
                    let placement = Placement {
                        angle: label.placement.angle + Rotation::R90,
                        ..label.placement
                    };
                    overrides.placements.insert(label.span.clone(), placement);
                }
            }
            Hit::Wire(_) | Hit::Flyline(_) => {}
        }
    }
    stretch_wires(scene, &moved, &mut overrides);
    overrides
}

/// Whether two axis-aligned segments run along each other.
fn overlaps(a: (GridPt, GridPt), b: (GridPt, GridPt)) -> bool {
    let horizontal = |s: (GridPt, GridPt)| s.0.1 == s.1.1;
    let vertical = |s: (GridPt, GridPt)| s.0.0 == s.1.0;
    if horizontal(a) && horizontal(b) && a.0.1 == b.0.1 {
        let (a0, a1) = (a.0.0.min(a.1.0), a.0.0.max(a.1.0));
        let (b0, b1) = (b.0.0.min(b.1.0), b.0.0.max(b.1.0));
        return a0 < b1 && b0 < a1;
    }
    if vertical(a) && vertical(b) && a.0.0 == b.0.0 {
        let (a0, a1) = (a.0.1.min(a.1.1), a.0.1.max(a.1.1));
        let (b0, b1) = (b.0.1.min(b.1.1), b.0.1.max(b.1.1));
        return a0 < b1 && b0 < a1;
    }
    a == b
}

/// The preview of dragging segment `segment` of wire `wire` sideways by
/// `delta`: every editable route that runs along it moves with it.
pub fn segment_overrides(scene: &Scene, wire: usize, segment: usize, delta: GridPt) -> Overrides {
    let mut overrides = Overrides::default();
    let Some(target) = scene.wires.get(wire) else {
        return overrides;
    };
    let points = target.points.iter().map(|p| grid(*p)).collect::<Vec<_>>();
    let (Some(p), Some(q)) = (points.get(segment), points.get(segment + 1)) else {
        return overrides;
    };
    let dragged = (*p, *q);
    for (index, other) in scene.wires.iter().enumerate() {
        if !other.editable {
            continue;
        }
        let other_points = other.points.iter().map(|p| grid(*p)).collect::<Vec<_>>();
        let along = if index == wire {
            Some(segment)
        } else {
            other_points
                .windows(2)
                .position(|pair| overlaps((pair[0], pair[1]), dragged))
        };
        if let Some(along) = along {
            let moved = stretch::move_segment(&other_points, along, delta);
            overrides
                .routes
                .insert(other.span.clone(), Some(stretch::corners(&moved)));
        }
    }
    overrides
}

/// The route keyword's source text for `corners`.
pub fn route_text(corners: &[GridPt]) -> String {
    let corners = corners
        .iter()
        .map(|(x, y)| format!("({x}, {y})"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{corners}]")
}

/// A rotation in degrees counterclockwise.
pub fn degrees(angle: Rotation) -> i64 {
    match angle {
        Rotation::R0 => 0,
        Rotation::R90 => 90,
        Rotation::R180 => 180,
        Rotation::R270 => 270,
    }
}

/// The source edits that make `overrides` permanent. A signal's call takes
/// no `reflect`, so the scene says which spans are signals.
pub fn placement_edits(scene: &Scene, overrides: &Overrides) -> Vec<PlacementEdit> {
    let signal_spans = scene
        .ports
        .iter()
        .map(|port| &port.span)
        .chain(scene.labels.iter().map(|label| &label.span))
        .collect::<Vec<_>>();
    let mut edits = Vec::new();
    let mut placements = overrides.placements.iter().collect::<Vec<_>>();
    placements.sort_by_key(|(span, _)| (span.path.clone(), span.span.start()));
    for (span, placement) in placements {
        let (x, y) = placement.position.unwrap_or((0, 0));
        let mut keywords = vec![
            KeywordEdit {
                name: "x".into(),
                value: Some(x.to_string()),
            },
            KeywordEdit {
                name: "y".into(),
                value: Some(y.to_string()),
            },
            KeywordEdit {
                name: "angle".into(),
                value: (placement.angle != Rotation::R0)
                    .then(|| degrees(placement.angle).to_string()),
            },
        ];
        if !signal_spans.contains(&span) {
            keywords.push(KeywordEdit {
                name: "reflect".into(),
                value: placement.reflect.then(|| "true".into()),
            });
        }
        edits.push(PlacementEdit {
            call: span.clone(),
            keywords,
        });
    }
    let mut routes = overrides.routes.iter().collect::<Vec<_>>();
    routes.sort_by_key(|(span, _)| (span.path.clone(), span.span.start()));
    for (span, route) in routes {
        edits.push(PlacementEdit {
            call: span.clone(),
            keywords: vec![KeywordEdit {
                name: "route".into(),
                value: route.as_deref().map(route_text),
            }],
        });
    }
    edits
}

/// The edits that write the current position of every unplaced, editable
/// element and port, so that nothing moves until the user moves it.
pub fn fix_positions(scene: &Scene) -> Overrides {
    let mut overrides = Overrides::default();
    for element in &scene.elements {
        if let Some(span) = element
            .span
            .clone()
            .filter(|_| element.auto_placed && element.editable)
        {
            overrides.placements.insert(span, element.placement);
        }
    }
    for port in scene
        .ports
        .iter()
        .filter(|port| port.auto_placed && port.editable)
    {
        overrides
            .placements
            .insert(port.span.clone(), port.placement);
    }
    overrides
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

    #[test]
    fn moving_a_device_moves_its_pins_and_stretches_its_wires() {
        let scene = inverter_scene();
        let overrides = move_overrides(&scene, &[Hit::Element(0)], (2, 0));
        let mp = &scene.elements[0];
        assert_eq!(
            overrides.placements[mp.span.as_ref().unwrap()].position,
            Some((2, 4))
        );
        // The four wires to `mp` are rewritten; the four to `mn` are not.
        assert_eq!(overrides.routes.len(), 4, "{:?}", overrides.routes);
        // `connect(a, mp.g, route=[(-4, 0), (-4, 4)])`: the gate moved right, so
        // the corner beside it keeps the segment horizontal.
        let gate_wire = &scene.wires[0];
        assert_eq!(
            overrides.routes[&gate_wire.span],
            Some(vec![(-4, 0), (-4, 4)])
        );
        // `connect(vdd, mp.s, route=[])`: a straight wire gets a corner, and
        // the segment at the moved pin keeps its direction.
        let vdd_wire = &scene.wires[4];
        assert_eq!(overrides.routes[&vdd_wire.span], Some(vec![(2, 8)]));
        let edits = placement_edits(&scene, &overrides);
        let device_edit = edits
            .iter()
            .find(|e| &e.call == mp.span.as_ref().unwrap())
            .unwrap();
        let keywords = device_edit
            .keywords
            .iter()
            .map(|k| (k.name.as_str(), k.value.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(
            keywords,
            [
                ("x", Some("2")),
                ("y", Some("4")),
                ("angle", None),
                ("reflect", None)
            ]
        );
        assert_eq!(route_text(&[(0, 6)]), "[(0, 6)]");
        assert_eq!(route_text(&[]), "[]");
    }

    #[test]
    fn moving_two_connected_elements_moves_their_wire() {
        let scene = inverter_scene();
        // Both devices and the ports they share wires with.
        let selected = [Hit::Element(0), Hit::Element(1), Hit::Port(1)];
        let overrides = move_overrides(&scene, &selected, (0, 1));
        // `connect(out, mp.d, route=[(0, 0)])` moves whole.
        let out_wire = &scene.wires[2];
        assert_eq!(overrides.routes[&out_wire.span], Some(vec![(0, 1)]));
    }

    #[test]
    fn rotating_rewrites_the_angle_and_the_wires_to_the_moved_pins() {
        let scene = inverter_scene();
        let overrides = orient_overrides(&scene, &[Hit::Element(1)], false);
        let mn = &scene.elements[1];
        let placement = overrides.placements[mn.span.as_ref().unwrap()];
        assert_eq!(placement.angle, Rotation::R90);
        assert_eq!(overrides.routes.len(), 4);
        let mirrored = orient_overrides(&scene, &[Hit::Element(1)], true);
        assert!(mirrored.placements[mn.span.as_ref().unwrap()].reflect);
        let edits = placement_edits(&scene, &overrides);
        let keywords = edits[0]
            .keywords
            .iter()
            .map(|k| (k.name.as_str(), k.value.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(
            keywords,
            [
                ("x", Some("0")),
                ("y", Some("-4")),
                ("angle", Some("90")),
                ("reflect", None)
            ]
        );
    }

    #[test]
    fn a_segment_drag_rewrites_every_route_along_it() {
        let scene = inverter_scene();
        // The vertical run of the first `a` wire, from (-4, 0) to (-4, 4).
        let overrides = segment_overrides(&scene, 0, 1, (-1, 0));
        assert_eq!(
            overrides.routes[&scene.wires[0].span],
            Some(vec![(-5, 0), (-5, 4)])
        );
        // The shared horizontal run from (-6, 0) to (-4, 0), dragged down,
        // moves both `a` wires and bends at the port.
        let overrides = segment_overrides(&scene, 0, 0, (0, -1));
        assert_eq!(
            overrides.routes[&scene.wires[0].span],
            Some(vec![(-6, -1), (-4, -1), (-4, 4)])
        );
        assert_eq!(
            overrides.routes[&scene.wires[1].span],
            Some(vec![(-6, -1), (-4, -1), (-4, -4)])
        );
    }

    #[test]
    fn fixing_positions_writes_only_unplaced_editable_items() {
        let placed = inverter_scene();
        assert!(fix_positions(&placed).is_empty());
        let data = compile_valid(DEVICE_INVERTER, "parent", Vec::new());
        let scene = build_scene(&data, data.top, &DisplayPrefs::default());
        let overrides = fix_positions(&scene);
        // The unplaced instance and the unplaced `out` port.
        assert_eq!(overrides.placements.len(), 2);
        assert!(
            overrides
                .placements
                .contains_key(scene.elements[1].span.as_ref().unwrap())
        );
        assert!(overrides.placements.contains_key(&scene.ports[1].span));
    }
}
