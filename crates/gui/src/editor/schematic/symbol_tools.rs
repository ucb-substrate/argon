//! The symbol view's gestures: snapping, handles, moving and reshaping
//! items, rotating and mirroring a selection, and the edits each one
//! writes.

use std::collections::HashMap;

use argonc::ast::Span;
use argonc::compile::{Align, SymbolItemKind};
use protocol::rpc::{KeywordEdit, PlacementEdit};

use super::SymbolSelection;
use super::symbol_scene::{SymbolOverrides, SymbolScene};
use super::symbols::Pt;

/// What the text tool places.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TextSpec {
    Literal(String),
    InstName,
    /// The cell parameter `name`.
    Param(String),
}

/// Snaps a point to quarter grid units, for shapes and text.
pub fn snap_quarter((x, y): Pt) -> Pt {
    ((x * 4.).round() / 4., (y * 4.).round() / 4.)
}

/// The distance from `center` to `point`, snapped to quarter grid units.
pub fn snapped_radius(center: Pt, point: Pt) -> f64 {
    ((point.0 - center.0).hypot(point.1 - center.1) * 4.).round() / 4.
}

/// The angle of `point` seen from `center`, in degrees counterclockwise
/// from the +x axis, snapped to 5 degrees.
pub fn angle_of(center: Pt, point: Pt) -> f64 {
    let angle = (point.1 - center.1).atan2(point.0 - center.0).to_degrees();
    ((angle / 5.).round() * 5.).rem_euclid(360.)
}

/// Snaps a point to whole grid units, for pins.
pub fn snap_grid((x, y): Pt) -> (i64, i64) {
    (x.round() as i64, y.round() as i64)
}

/// A `Float` literal with the fewest digits that read back as `value`.
pub fn float_literal(value: f64) -> String {
    let text = format!("{value}");
    if text.contains('.') || text.contains('e') || text.contains("inf") || text.contains("NaN") {
        text
    } else {
        format!("{text}.")
    }
}

fn points_text(points: &[Pt]) -> String {
    let points = points
        .iter()
        .map(|(x, y)| format!("({}, {})", float_literal(*x), float_literal(*y)))
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{points}]")
}

/// What a handle of a selected item reshapes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HandleKind {
    /// A point of a line or polygon.
    Point(usize),
    /// A corner of a rectangle: 0 is `(x0, y0)`, then counterclockwise.
    Corner(usize),
    /// The radius of a circle or arc, at its rightmost point.
    Radius,
    /// The start or end of an arc, on the arc.
    ArcStart,
    ArcEnd,
}

/// A handle of an item, at `at`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Handle {
    pub kind: HandleKind,
    pub at: Pt,
}

/// The handles that reshape `kind`.
pub fn handles(kind: &SymbolItemKind) -> Vec<Handle> {
    match kind {
        SymbolItemKind::Line { points } | SymbolItemKind::Polygon { points, .. } => points
            .iter()
            .enumerate()
            .map(|(index, at)| Handle {
                kind: HandleKind::Point(index),
                at: *at,
            })
            .collect(),
        SymbolItemKind::Rect { x0, y0, x1, y1, .. } => {
            [(*x0, *y0), (*x1, *y0), (*x1, *y1), (*x0, *y1)]
                .into_iter()
                .enumerate()
                .map(|(index, at)| Handle {
                    kind: HandleKind::Corner(index),
                    at,
                })
                .collect()
        }
        SymbolItemKind::Circle { x, y, r, .. } => vec![Handle {
            kind: HandleKind::Radius,
            at: (x + r, *y),
        }],
        SymbolItemKind::Arc {
            x,
            y,
            r,
            start,
            end,
        } => vec![
            Handle {
                kind: HandleKind::ArcStart,
                at: (
                    x + r * start.to_radians().cos(),
                    y + r * start.to_radians().sin(),
                ),
            },
            Handle {
                kind: HandleKind::ArcEnd,
                at: (
                    x + r * end.to_radians().cos(),
                    y + r * end.to_radians().sin(),
                ),
            },
        ],
        SymbolItemKind::Text { .. } => Vec::new(),
    }
}

/// `kind` moved by `delta`.
pub fn moved(kind: &SymbolItemKind, (dx, dy): Pt) -> SymbolItemKind {
    let shift = |(x, y): &Pt| (x + dx, y + dy);
    match kind {
        SymbolItemKind::Line { points } => SymbolItemKind::Line {
            points: points.iter().map(shift).collect(),
        },
        SymbolItemKind::Polygon { points, fill } => SymbolItemKind::Polygon {
            points: points.iter().map(shift).collect(),
            fill: *fill,
        },
        SymbolItemKind::Rect {
            x0,
            y0,
            x1,
            y1,
            fill,
        } => SymbolItemKind::Rect {
            x0: x0 + dx,
            y0: y0 + dy,
            x1: x1 + dx,
            y1: y1 + dy,
            fill: *fill,
        },
        SymbolItemKind::Circle { x, y, r, fill } => SymbolItemKind::Circle {
            x: x + dx,
            y: y + dy,
            r: *r,
            fill: *fill,
        },
        SymbolItemKind::Arc {
            x,
            y,
            r,
            start,
            end,
        } => SymbolItemKind::Arc {
            x: x + dx,
            y: y + dy,
            r: *r,
            start: *start,
            end: *end,
        },
        SymbolItemKind::Text {
            text,
            x,
            y,
            size,
            align,
        } => SymbolItemKind::Text {
            text: text.clone(),
            x: x + dx,
            y: y + dy,
            size: *size,
            align: *align,
        },
    }
}

/// `kind` with `handle` dragged to `to`.
pub fn reshaped(kind: &SymbolItemKind, handle: HandleKind, to: Pt) -> SymbolItemKind {
    let mut kind = kind.clone();
    match (&mut kind, handle) {
        (
            SymbolItemKind::Line { points } | SymbolItemKind::Polygon { points, .. },
            HandleKind::Point(index),
        ) => {
            if let Some(point) = points.get_mut(index) {
                *point = to;
            }
        }
        (SymbolItemKind::Rect { x0, y0, x1, y1, .. }, HandleKind::Corner(corner)) => {
            match corner {
                0 => (*x0, *y0) = to,
                1 => (*x1, *y0) = to,
                2 => (*x1, *y1) = to,
                _ => (*x0, *y1) = to,
            }
            if x0 > x1 {
                std::mem::swap(x0, x1);
            }
            if y0 > y1 {
                std::mem::swap(y0, y1);
            }
        }
        (SymbolItemKind::Circle { x, y, r, .. }, HandleKind::Radius) => {
            *r = ((to.0 - *x).hypot(to.1 - *y) * 4.).round() / 4.;
            if *r <= 0. {
                *r = 0.25;
            }
        }
        (
            SymbolItemKind::Arc {
                x,
                y,
                r,
                start,
                end,
            },
            handle,
        ) => {
            let angle = ((to.1 - *y).atan2(to.0 - *x).to_degrees() / 5.).round() * 5.;
            let angle = angle.rem_euclid(360.);
            match handle {
                HandleKind::Radius => {
                    *r = ((to.0 - *x).hypot(to.1 - *y) * 4.).round() / 4.;
                    if *r <= 0. {
                        *r = 0.25;
                    }
                }
                HandleKind::ArcStart => *start = angle,
                HandleKind::ArcEnd => *end = angle,
                _ => {}
            }
        }
        _ => {}
    }
    kind
}

/// A point rotated by 90 degrees counterclockwise about `center`, or
/// mirrored left to right about it when `mirror`.
fn turned((x, y): Pt, center: Pt, mirror: bool) -> Pt {
    if mirror {
        (2. * center.0 - x, y)
    } else {
        (center.0 - (y - center.1), center.1 + (x - center.0))
    }
}

/// `kind` rotated or mirrored about `center`. Mirroring reverses an arc and
/// swaps left and right alignment, so text stays beside what it labels.
pub fn turned_item(kind: &SymbolItemKind, center: Pt, mirror: bool) -> SymbolItemKind {
    let turn = |point: &Pt| turned(*point, center, mirror);
    match kind {
        SymbolItemKind::Line { points } => SymbolItemKind::Line {
            points: points.iter().map(turn).collect(),
        },
        SymbolItemKind::Polygon { points, fill } => SymbolItemKind::Polygon {
            points: points.iter().map(turn).collect(),
            fill: *fill,
        },
        SymbolItemKind::Rect {
            x0,
            y0,
            x1,
            y1,
            fill,
        } => {
            let a = turn(&(*x0, *y0));
            let b = turn(&(*x1, *y1));
            SymbolItemKind::Rect {
                x0: a.0.min(b.0),
                y0: a.1.min(b.1),
                x1: a.0.max(b.0),
                y1: a.1.max(b.1),
                fill: *fill,
            }
        }
        SymbolItemKind::Circle { x, y, r, fill } => {
            let (x, y) = turn(&(*x, *y));
            SymbolItemKind::Circle {
                x,
                y,
                r: *r,
                fill: *fill,
            }
        }
        SymbolItemKind::Arc {
            x,
            y,
            r,
            start,
            end,
        } => {
            let (x, y) = turn(&(*x, *y));
            let (start, end) = if mirror {
                (
                    (180. - end).rem_euclid(360.),
                    (180. - start).rem_euclid(360.),
                )
            } else {
                ((start + 90.).rem_euclid(360.), (end + 90.).rem_euclid(360.))
            };
            SymbolItemKind::Arc {
                x,
                y,
                r: *r,
                start,
                end,
            }
        }
        SymbolItemKind::Text {
            text,
            x,
            y,
            size,
            align,
        } => {
            let (x, y) = turn(&(*x, *y));
            let align = match (mirror, align) {
                (true, Align::Left) => Align::Right,
                (true, Align::Right) => Align::Left,
                (_, align) => *align,
            };
            SymbolItemKind::Text {
                text: text.clone(),
                x,
                y,
                size: *size,
                align,
            }
        }
    }
}

/// A pin rotated or mirrored about `center`, which is on the grid.
pub fn turned_pin((x, y): (i64, i64), center: (i64, i64), mirror: bool) -> (i64, i64) {
    let (x, y) = turned(
        (x as f64, y as f64),
        (center.0 as f64, center.1 as f64),
        mirror,
    );
    (x.round() as i64, y.round() as i64)
}

/// The center of the selection's bounds, snapped to the grid.
pub fn selection_center(scene: &SymbolScene, selection: &[SymbolSelection]) -> Option<(i64, i64)> {
    let mut bounds: Option<super::symbols::Bounds> = None;
    for selected in selection {
        match selected {
            SymbolSelection::Item(index) => {
                if let Some(item) = scene.items.get(*index) {
                    bounds = super::symbols::union(bounds, item.kind.bounds());
                }
            }
            SymbolSelection::Pin(port) => {
                if let Some(pin) = scene.pins.iter().find(|pin| pin.port == *port) {
                    super::symbols::include(
                        &mut bounds,
                        (pin.position.0 as f64, pin.position.1 as f64),
                    );
                }
            }
        }
    }
    let (x0, y0, x1, y1) = bounds?;
    Some((
        ((x0 + x1) / 2.).round() as i64,
        ((y0 + y1) / 2.).round() as i64,
    ))
}

/// The overrides that rotate or mirror the editable selection about its
/// center.
pub fn turn_overrides(
    scene: &SymbolScene,
    selection: &[SymbolSelection],
    mirror: bool,
) -> SymbolOverrides {
    let mut overrides = SymbolOverrides::default();
    let Some(center) = selection_center(scene, selection) else {
        return overrides;
    };
    let center_f = (center.0 as f64, center.1 as f64);
    for selected in selection {
        match selected {
            SymbolSelection::Item(index) => {
                if let Some(item) = scene.items.get(*index)
                    && item.editable
                    && let Some(span) = &item.span
                {
                    overrides
                        .items
                        .insert(span.clone(), turned_item(&item.kind, center_f, mirror));
                }
            }
            SymbolSelection::Pin(port) => {
                if let Some(pin) = scene.pins.iter().find(|pin| pin.port == *port)
                    && pin.editable
                    && let Some(span) = &pin.span
                {
                    overrides
                        .pins
                        .insert(span.clone(), turned_pin(pin.position, center, mirror));
                }
            }
        }
    }
    overrides
}

/// The overrides that move the editable selection by `delta`, snapped as
/// each kind of item snaps.
pub fn move_overrides(
    scene: &SymbolScene,
    selection: &[SymbolSelection],
    delta: Pt,
) -> SymbolOverrides {
    let mut overrides = SymbolOverrides::default();
    let quarter = snap_quarter(delta);
    let grid = snap_grid(delta);
    for selected in selection {
        match selected {
            SymbolSelection::Item(index) => {
                if let Some(item) = scene.items.get(*index)
                    && item.editable
                    && let Some(span) = &item.span
                {
                    overrides
                        .items
                        .insert(span.clone(), moved(&item.kind, quarter));
                }
            }
            SymbolSelection::Pin(port) => {
                if let Some(pin) = scene.pins.iter().find(|pin| pin.port == *port)
                    && pin.editable
                    && let Some(span) = &pin.span
                {
                    overrides.pins.insert(
                        span.clone(),
                        (pin.position.0 + grid.0, pin.position.1 + grid.1),
                    );
                }
            }
        }
    }
    overrides
}

/// The edits that make `overrides` permanent: every coordinate of each
/// item, and the alignment of text.
pub fn symbol_edits(overrides: &SymbolOverrides) -> Vec<PlacementEdit> {
    let keyword = |name: &str, value: String| KeywordEdit {
        name: name.to_owned(),
        value: Some(value),
    };
    let mut items = overrides.items.iter().collect::<Vec<_>>();
    items.sort_by_key(|(span, _)| (span.path.clone(), span.span.start()));
    let mut edits = Vec::new();
    for (span, kind) in items {
        let keywords = match kind {
            SymbolItemKind::Line { points } | SymbolItemKind::Polygon { points, .. } => {
                vec![keyword("points", points_text(points))]
            }
            SymbolItemKind::Rect { x0, y0, x1, y1, .. } => vec![
                keyword("x0", float_literal(*x0)),
                keyword("y0", float_literal(*y0)),
                keyword("x1", float_literal(*x1)),
                keyword("y1", float_literal(*y1)),
            ],
            SymbolItemKind::Circle { x, y, r, .. } => vec![
                keyword("x", float_literal(*x)),
                keyword("y", float_literal(*y)),
                keyword("r", float_literal(*r)),
            ],
            SymbolItemKind::Arc {
                x,
                y,
                r,
                start,
                end,
            } => vec![
                keyword("x", float_literal(*x)),
                keyword("y", float_literal(*y)),
                keyword("r", float_literal(*r)),
                keyword("start", float_literal(*start)),
                keyword("end", float_literal(*end)),
            ],
            SymbolItemKind::Text { x, y, align, .. } => vec![
                keyword("x", float_literal(*x)),
                keyword("y", float_literal(*y)),
                KeywordEdit {
                    name: "align".into(),
                    value: (*align != Align::Left)
                        .then(|| format!("std::symbol::Align::{}", align.name())),
                },
            ],
        };
        edits.push(PlacementEdit {
            call: span.clone(),
            keywords,
        });
    }
    let mut pins = overrides.pins.iter().collect::<Vec<_>>();
    pins.sort_by_key(|(span, _)| (span.path.clone(), span.span.start()));
    for (span, (x, y)) in pins {
        edits.push(PlacementEdit {
            call: span.clone(),
            keywords: vec![keyword("x", x.to_string()), keyword("y", y.to_string())],
        });
    }
    edits
}

/// Everything within `tolerance` of `point`: pins first, then items from
/// the top down, with text hit by its anchor.
pub fn hits_at(scene: &SymbolScene, point: Pt, tolerance: f64) -> Vec<SymbolSelection> {
    let mut hits = Vec::new();
    for pin in &scene.pins {
        if (pin.position.0 as f64 - point.0).abs() <= tolerance
            && (pin.position.1 as f64 - point.1).abs() <= tolerance
        {
            hits.push(SymbolSelection::Pin(pin.port));
        }
    }
    for (index, item) in scene.items.iter().enumerate().rev() {
        let hit = match &item.kind {
            SymbolItemKind::Text { x, y, size, .. } => {
                (x - point.0).abs() <= tolerance + size * 2.
                    && (y - point.1).abs() <= tolerance + size / 2.
            }
            kind => kind.bounds().is_some_and(|(x0, y0, x1, y1)| {
                point.0 >= x0 - tolerance
                    && point.0 <= x1 + tolerance
                    && point.1 >= y0 - tolerance
                    && point.1 <= y1 + tolerance
            }),
        };
        if hit {
            hits.push(SymbolSelection::Item(index));
        }
    }
    hits
}

/// `selection` with every group completed: an item a function drew brings
/// the rest of what that call drew.
pub fn expand_groups(scene: &SymbolScene, selection: Vec<SymbolSelection>) -> Vec<SymbolSelection> {
    let mut origins: HashMap<&Span, ()> = HashMap::new();
    for selected in &selection {
        let origin = match selected {
            SymbolSelection::Item(index) => scene.items.get(*index).and_then(|i| i.origin.as_ref()),
            SymbolSelection::Pin(port) => scene
                .pins
                .iter()
                .find(|pin| pin.port == *port)
                .and_then(|pin| pin.origin.as_ref()),
        };
        if let Some(origin) = origin {
            origins.insert(origin, ());
        }
    }
    let mut expanded = selection;
    for (index, item) in scene.items.iter().enumerate() {
        if item
            .origin
            .as_ref()
            .is_some_and(|o| origins.contains_key(o))
            && !expanded.contains(&SymbolSelection::Item(index))
        {
            expanded.push(SymbolSelection::Item(index));
        }
    }
    for pin in &scene.pins {
        if pin.origin.as_ref().is_some_and(|o| origins.contains_key(o))
            && !expanded.contains(&SymbolSelection::Pin(pin.port))
        {
            expanded.push(SymbolSelection::Pin(pin.port));
        }
    }
    expanded
}

/// The spans that delete the selection: an item or pin by its call, or a
/// group by its origin.
pub fn delete_targets(scene: &SymbolScene, selection: &[SymbolSelection]) -> Vec<Span> {
    let mut targets: Vec<Span> = Vec::new();
    for selected in selection {
        let (span, origin) = match selected {
            SymbolSelection::Item(index) => match scene.items.get(*index) {
                Some(item) => (item.span.clone(), item.origin.clone()),
                None => continue,
            },
            SymbolSelection::Pin(port) => match scene.pins.iter().find(|pin| pin.port == *port) {
                Some(pin) => (pin.span.clone(), pin.origin.clone()),
                None => continue,
            },
        };
        if let Some(target) = origin.or(span)
            && !targets.contains(&target)
        {
            targets.push(target);
        }
    }
    targets
}

#[cfg(test)]
mod tests {
    use argonc::compile::{CellArg, SymbolText};

    use super::*;
    use crate::editor::schematic::scene::tests::{DEVICE_INVERTER, compile_valid};
    use crate::editor::schematic::symbol_scene::build_symbol_scene;

    fn inverter_symbol() -> SymbolScene {
        let data = compile_valid(
            DEVICE_INVERTER,
            "device_inv",
            vec![CellArg::Float(2.), CellArg::Float(1.)],
        );
        build_symbol_scene(&data, data.top, "X1")
    }

    #[test]
    fn shapes_snap_to_quarters_and_pins_to_the_grid() {
        assert_eq!(snap_quarter((1.1, -0.6)), (1., -0.5));
        assert_eq!(snap_grid((1.6, -0.4)), (2, 0));
        assert_eq!(float_literal(2.), "2.");
        assert_eq!(float_literal(0.25), "0.25");
    }

    #[test]
    fn handles_follow_each_kind_of_item() {
        let rect = SymbolItemKind::Rect {
            x0: 0.,
            y0: -4.,
            x1: 4.,
            y1: 0.,
            fill: false,
        };
        let corners = handles(&rect);
        assert_eq!(corners.len(), 4);
        assert_eq!(corners[2].at, (4., 0.));
        let reshaped = reshaped(&rect, HandleKind::Corner(2), (6., 1.));
        assert!(matches!(reshaped, SymbolItemKind::Rect { x1, y1, .. } if x1 == 6. && y1 == 1.));
        // Dragging a corner past the opposite one keeps the box well formed.
        let crossed = reshaped_corner(&rect);
        assert!(matches!(crossed, SymbolItemKind::Rect { x0, x1, .. } if x0 == -1. && x1 == 0.));
        let circle = SymbolItemKind::Circle {
            x: 1.,
            y: 1.,
            r: 1.,
            fill: false,
        };
        assert_eq!(handles(&circle)[0].at, (2., 1.));
        let grown = super::reshaped(&circle, HandleKind::Radius, (1., 3.));
        assert!(matches!(grown, SymbolItemKind::Circle { r, .. } if r == 2.));
        let arc = SymbolItemKind::Arc {
            x: 0.,
            y: 0.,
            r: 2.,
            start: 0.,
            end: 90.,
        };
        let ends = handles(&arc);
        assert_eq!(ends[0].at, (2., 0.));
        assert!((ends[1].at.1 - 2.).abs() < 1e-9);
        let swung = super::reshaped(&arc, HandleKind::ArcEnd, (-2., 0.));
        assert!(matches!(swung, SymbolItemKind::Arc { end, .. } if end == 180.));
        let line = SymbolItemKind::Line {
            points: vec![(0., 0.), (2., 0.)],
        };
        let bent = super::reshaped(&line, HandleKind::Point(1), (2., 2.));
        assert!(matches!(bent, SymbolItemKind::Line { points } if points[1] == (2., 2.)));
    }

    fn reshaped_corner(rect: &SymbolItemKind) -> SymbolItemKind {
        reshaped(rect, HandleKind::Corner(2), (-1., 0.))
    }

    #[test]
    fn turning_rotates_about_the_center_and_mirroring_swaps_alignment() {
        let text = SymbolItemKind::Text {
            text: SymbolText::Literal("a".into()),
            x: 2.,
            y: 0.,
            size: 1.,
            align: Align::Left,
        };
        let mirrored = turned_item(&text, (0., 0.), true);
        assert!(
            matches!(mirrored, SymbolItemKind::Text { x, align, .. } if x == -2. && align == Align::Right)
        );
        let rotated = turned_item(&text, (0., 0.), false);
        assert!(
            matches!(rotated, SymbolItemKind::Text { x, y, align, .. } if x == 0. && y == 2. && align == Align::Left)
        );
        let arc = SymbolItemKind::Arc {
            x: 0.,
            y: 0.,
            r: 1.,
            start: 0.,
            end: 90.,
        };
        assert!(matches!(
            turned_item(&arc, (0., 0.), true),
            SymbolItemKind::Arc { start, end, .. } if start == 90. && end == 180.
        ));
        assert!(matches!(
            turned_item(&arc, (0., 0.), false),
            SymbolItemKind::Arc { start, end, .. } if start == 90. && end == 180.
        ));
        assert_eq!(turned_pin((4, 0), (0, 0), false), (0, 4));
        assert_eq!(turned_pin((4, 0), (1, 0), true), (-2, 0));
    }

    #[test]
    fn the_inverter_turns_about_its_center_and_writes_every_coordinate() {
        let scene = inverter_symbol();
        let everything = (0..scene.items.len())
            .map(SymbolSelection::Item)
            .chain(scene.pins.iter().map(|pin| SymbolSelection::Pin(pin.port)))
            .collect::<Vec<_>>();
        assert_eq!(selection_center(&scene, &everything), Some((0, 0)));
        let overrides = turn_overrides(&scene, &everything, false);
        assert_eq!(overrides.items.len(), scene.items.len());
        assert_eq!(overrides.pins.len(), 4);
        let edits = symbol_edits(&overrides);
        assert_eq!(edits.len(), scene.items.len() + 4);
        // The polygon's points are one keyword; a pin is two.
        let polygon = edits
            .iter()
            .find(|edit| edit.keywords.iter().any(|k| k.name == "points"))
            .unwrap();
        assert_eq!(polygon.keywords.len(), 1);
        let a = scene.pins.iter().find(|pin| pin.name == "a").unwrap();
        let pin_edit = edits
            .iter()
            .find(|edit| edit.call == *a.span.as_ref().unwrap())
            .unwrap();
        let values = pin_edit
            .keywords
            .iter()
            .map(|k| k.value.clone().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(values, ["0", "-4"], "(-4, 0) rotates to (0, -4)");
    }

    #[test]
    fn hits_prefer_pins_and_groups_select_together() {
        let scene = inverter_symbol();
        let hits = hits_at(&scene, (-4., 0.), 0.2);
        assert_eq!(hits.first(), Some(&SymbolSelection::Pin(0)));
        assert!(hits.len() >= 2, "the input stub line is there too");
        let text_hit = hits_at(&scene, (2.5, 2.), 0.2);
        assert!(text_hit.iter().any(|hit| matches!(hit, SymbolSelection::Item(index) if matches!(scene.items[*index].kind, SymbolItemKind::Text { .. }))));
        // The inverter draws no groups; a wrapper does.
        let data = compile_valid(
            "use std::schematic::Signal;
             cell wrapper() {
                 pub let d = Signal();
                 pub let g = Signal();
                 pub let s = Signal();
                 pub let b = Signal();
                 std::symbol::nmos(d, g, s, b);
             }",
            "wrapper",
            vec![],
        );
        let grouped = build_symbol_scene(&data, data.top, "X");
        assert!(grouped.items.iter().all(|item| item.origin.is_some()));
        let expanded = expand_groups(&grouped, vec![SymbolSelection::Item(0)]);
        assert_eq!(expanded.len(), grouped.items.len() + grouped.pins.len());
        let targets = delete_targets(&grouped, &expanded);
        assert_eq!(targets.len(), 1, "one origin for the whole group");
        assert!(
            move_overrides(&grouped, &expanded, (1., 0.))
                .items
                .is_empty(),
            "locked"
        );
    }
}
