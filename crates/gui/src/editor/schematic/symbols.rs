//! A compiled symbol under a placement: its shapes, pins, and text in the
//! parent's grid, with text kept upright.

use argonc::compile::{Align, Placement, Symbol, SymbolItemKind, SymbolText};
use argonc::netlist::format_param;
use geometry::transform::Rotation;

/// A point in grid units, with y pointing up.
pub type Pt = (f64, f64);

/// An axis-aligned box as `(x0, y0, x1, y1)` in grid units.
pub type Bounds = (f64, f64, f64, f64);

/// Grows `bounds` to include `(x, y)`.
pub fn include(bounds: &mut Option<Bounds>, (x, y): Pt) {
    *bounds = Some(match *bounds {
        None => (x, y, x, y),
        Some((x0, y0, x1, y1)) => (x0.min(x), y0.min(y), x1.max(x), y1.max(y)),
    });
}

/// The union of two optional boxes.
pub fn union(a: Option<Bounds>, b: Option<Bounds>) -> Option<Bounds> {
    let mut bounds = a;
    if let Some((x0, y0, x1, y1)) = b {
        include(&mut bounds, (x0, y0));
        include(&mut bounds, (x1, y1));
    }
    bounds
}

/// Rotates and reflects a symbol-space vector as `placement` does, without
/// translating it.
fn orient(placement: &Placement, (x, y): Pt) -> Pt {
    let (x, y) = if placement.reflect { (x, -y) } else { (x, y) };
    match placement.angle {
        Rotation::R0 => (x, y),
        Rotation::R90 => (-y, x),
        Rotation::R180 => (-x, -y),
        Rotation::R270 => (y, -x),
    }
}

/// Transforms a symbol-space point into the parent's grid.
pub fn place_point(placement: &Placement, point: Pt) -> Pt {
    let (x, y) = orient(placement, point);
    let (ox, oy) = placement.position.unwrap_or((0, 0));
    (x + ox as f64, y + oy as f64)
}

/// Transforms a symbol-space grid point into the parent's grid. Pins stay on
/// integer points under every placement.
pub fn place_grid_point(placement: &Placement, (x, y): (i64, i64)) -> (i64, i64) {
    let (px, py) = place_point(placement, (x as f64, y as f64));
    (px.round() as i64, py.round() as i64)
}

/// A shape of a placed symbol, in the parent's grid.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    Line(Vec<Pt>),
    Polygon {
        points: Vec<Pt>,
        fill: bool,
    },
    Circle {
        center: Pt,
        r: f64,
        fill: bool,
    },
    /// Counterclockwise from `start` to `end`, in degrees.
    Arc {
        center: Pt,
        r: f64,
        start: f64,
        end: f64,
    },
}

impl Shape {
    pub fn bounds(&self) -> Option<Bounds> {
        let mut bounds = None;
        match self {
            Shape::Line(points) | Shape::Polygon { points, .. } => {
                for point in points {
                    include(&mut bounds, *point);
                }
            }
            Shape::Circle { center, r, .. } | Shape::Arc { center, r, .. } => {
                include(&mut bounds, (center.0 - r, center.1 - r));
                include(&mut bounds, (center.0 + r, center.1 + r));
            }
        }
        bounds
    }
}

/// Text of a placed symbol. It is never drawn upside down or mirrored: the
/// anchor follows the placement, and the alignment flips so that the text
/// still reads left to right, or bottom to top when `vertical`.
#[derive(Debug, Clone, PartialEq)]
pub struct PlacedText {
    pub text: String,
    pub anchor: Pt,
    pub size: f64,
    pub align: Align,
    pub vertical: bool,
}

/// Places text anchored at the symbol-space point `anchor`.
pub fn place_text(
    placement: &Placement,
    text: String,
    anchor: Pt,
    size: f64,
    align: Align,
) -> PlacedText {
    // The direction the text would run in after the transform.
    let (dx, dy) = orient(placement, (1., 0.));
    let vertical = dx.abs() < dy.abs();
    let reversed = if vertical { dy < 0. } else { dx < 0. };
    let align = if reversed {
        match align {
            Align::Left => Align::Right,
            Align::Right => Align::Left,
            Align::Center => Align::Center,
        }
    } else {
        align
    };
    PlacedText {
        text,
        anchor: place_point(placement, anchor),
        size,
        align,
        vertical,
    }
}

/// A symbol in the parent's grid.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct PlacedSymbol {
    pub shapes: Vec<Shape>,
    /// The pin of each port, in port order.
    pub pins: Vec<Option<(i64, i64)>>,
    pub texts: Vec<PlacedText>,
}

impl PlacedSymbol {
    /// The bounds of the shapes and pins, leaving text out.
    pub fn bounds(&self) -> Option<Bounds> {
        let mut bounds = None;
        for shape in &self.shapes {
            bounds = union(bounds, shape.bounds());
        }
        for (x, y) in self.pins.iter().flatten() {
            include(&mut bounds, (*x as f64, *y as f64));
        }
        bounds
    }
}

/// Places `symbol`, showing `inst_name` for its instance-name items and
/// `pin_names` beside its labeled pins.
pub fn place_symbol(
    symbol: &Symbol,
    placement: &Placement,
    inst_name: &str,
    pin_names: &[String],
) -> PlacedSymbol {
    let mut placed = PlacedSymbol::default();
    for item in &symbol.items {
        match &item.kind {
            SymbolItemKind::Line { points } => placed.shapes.push(Shape::Line(
                points.iter().map(|p| place_point(placement, *p)).collect(),
            )),
            SymbolItemKind::Polygon { points, fill } => placed.shapes.push(Shape::Polygon {
                points: points.iter().map(|p| place_point(placement, *p)).collect(),
                fill: *fill,
            }),
            SymbolItemKind::Rect {
                x0,
                y0,
                x1,
                y1,
                fill,
            } => placed.shapes.push(Shape::Polygon {
                points: [(*x0, *y0), (*x1, *y0), (*x1, *y1), (*x0, *y1)]
                    .into_iter()
                    .map(|p| place_point(placement, p))
                    .collect(),
                fill: *fill,
            }),
            SymbolItemKind::Circle { x, y, r, fill } => placed.shapes.push(Shape::Circle {
                center: place_point(placement, (*x, *y)),
                r: *r,
                fill: *fill,
            }),
            SymbolItemKind::Arc {
                x,
                y,
                r,
                start,
                end,
            } => {
                let (start, end) = place_arc_angles(placement, *start, *end);
                placed.shapes.push(Shape::Arc {
                    center: place_point(placement, (*x, *y)),
                    r: *r,
                    start,
                    end,
                });
            }
            SymbolItemKind::Text {
                text,
                x,
                y,
                size,
                align,
            } => {
                let text = match text {
                    SymbolText::Literal(text) => text.clone(),
                    SymbolText::InstName => inst_name.to_owned(),
                    SymbolText::Param { name, value } => format!("{name}={}", format_param(value)),
                };
                placed
                    .texts
                    .push(place_text(placement, text, (*x, *y), *size, *align));
            }
        }
    }
    for (port, pin) in symbol.pins.iter().enumerate() {
        let Some(pin) = pin else {
            placed.pins.push(None);
            continue;
        };
        placed
            .pins
            .push(Some(place_grid_point(placement, (pin.x, pin.y))));
        if let Some(label) = &pin.label {
            let name = pin_names.get(port).cloned().unwrap_or_default();
            placed.texts.push(place_text(
                placement,
                name,
                (label.x, label.y),
                1.,
                label.align,
            ));
        }
    }
    placed
}

/// The angles of an arc under `placement`: rotated by the placement's angle,
/// and reversed when the placement reflects.
fn place_arc_angles(placement: &Placement, start: f64, end: f64) -> (f64, f64) {
    let rotation = match placement.angle {
        Rotation::R0 => 0.,
        Rotation::R90 => 90.,
        Rotation::R180 => 180.,
        Rotation::R270 => 270.,
    };
    if placement.reflect {
        (-end + rotation, -start + rotation)
    } else {
        (start + rotation, end + rotation)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn placement(x: i64, y: i64, angle: Rotation, reflect: bool) -> Placement {
        Placement {
            position: Some((x, y)),
            angle,
            reflect,
        }
    }

    #[test]
    fn points_rotate_counterclockwise_about_the_origin_then_move() {
        let p = placement(10, 20, Rotation::R90, false);
        assert_eq!(place_point(&p, (1., 0.)), (10., 21.));
        assert_eq!(place_grid_point(&p, (0, 2)), (8, 20));
        let r = placement(0, 0, Rotation::R0, true);
        assert_eq!(place_point(&r, (1., 2.)), (1., -2.));
        let both = placement(0, 0, Rotation::R90, true);
        // Reflect first, then rotate.
        assert_eq!(place_point(&both, (1., 2.)), (2., 1.));
    }

    #[test]
    fn text_stays_upright_and_flips_its_alignment() {
        let upright = place_text(
            &placement(0, 0, Rotation::R0, false),
            "a".into(),
            (1., 1.),
            1.,
            Align::Left,
        );
        assert!(!upright.vertical);
        assert_eq!(upright.align, Align::Left);

        let flipped = place_text(
            &placement(0, 0, Rotation::R180, false),
            "a".into(),
            (1., 1.),
            1.,
            Align::Left,
        );
        assert!(!flipped.vertical);
        assert_eq!(flipped.align, Align::Right);
        assert_eq!(flipped.anchor, (-1., -1.));

        let up = place_text(
            &placement(0, 0, Rotation::R90, false),
            "a".into(),
            (0., 0.),
            1.,
            Align::Left,
        );
        assert!(up.vertical);
        assert_eq!(up.align, Align::Left, "runs bottom to top already");

        let down = place_text(
            &placement(0, 0, Rotation::R270, false),
            "a".into(),
            (0., 0.),
            1.,
            Align::Left,
        );
        assert!(down.vertical);
        assert_eq!(down.align, Align::Right, "would run top to bottom");

        let mirrored = place_text(
            &placement(0, 0, Rotation::R0, true),
            "a".into(),
            (0., 0.),
            1.,
            Align::Right,
        );
        assert!(!mirrored.vertical);
        assert_eq!(mirrored.align, Align::Right, "reflection keeps x");
    }

    #[test]
    fn a_placed_symbol_resolves_its_placeholders() {
        let symbol = argonc::compile::box_symbol(&["a", "vdd"], "inv");
        let placed = place_symbol(
            &symbol,
            &placement(4, 0, Rotation::R0, false),
            "Xinv",
            &["a".into(), "vdd".into()],
        );
        assert_eq!(placed.pins, [Some((4, -2)), Some((6, 0))]);
        assert!(
            placed
                .texts
                .iter()
                .any(|t| t.text == "Xinv" && t.anchor == (4., 1.))
        );
        assert!(placed.texts.iter().any(|t| t.text == "inv"));
        assert_eq!(placed.bounds(), Some((4., -4., 8., 0.)));
    }
}
