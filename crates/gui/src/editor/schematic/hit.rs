//! Hit testing against a scene, in grid units.

use super::scene::Scene;
use super::symbols::Pt;
use super::wires::segment_distance;

/// Something under the pointer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Hit {
    /// An element, by its index in the scene.
    Element(usize),
    /// A terminal pin of an element.
    Pin {
        element: usize,
        terminal: usize,
    },
    Port(usize),
    Label(usize),
    /// A wire, by its index in the scene.
    Wire(usize),
    Flyline(usize),
}

/// Everything within `tolerance` grid units of `point`, nearest kinds of
/// item first: pins and labels, then wires, then elements, then flylines.
pub fn hits_at(scene: &Scene, point: Pt, tolerance: f64) -> Vec<Hit> {
    let mut hits = Vec::new();
    let near = |p: (i64, i64)| {
        (p.0 as f64 - point.0).abs() <= tolerance && (p.1 as f64 - point.1).abs() <= tolerance
    };
    for (index, glyph) in scene.ports.iter().enumerate() {
        if near(glyph.position) {
            hits.push(Hit::Port(index));
        }
    }
    for (index, label) in scene.labels.iter().enumerate() {
        if near(label.position) {
            hits.push(Hit::Label(index));
        }
    }
    for (element, scene_element) in scene.elements.iter().enumerate() {
        for (terminal, pin) in scene_element.pins.iter().enumerate() {
            if pin.position.is_some_and(near) {
                hits.push(Hit::Pin { element, terminal });
            }
        }
    }
    for (index, wire) in scene.wires.iter().enumerate() {
        let on_wire = wire
            .points
            .windows(2)
            .any(|pair| segment_distance(point, pair[0], pair[1]) <= tolerance);
        if on_wire {
            hits.push(Hit::Wire(index));
        }
    }
    // Later elements are drawn on top, so they are hit first.
    for (index, element) in scene.elements.iter().enumerate().rev() {
        let (x0, y0, x1, y1) = element.bounds;
        if point.0 >= x0 - tolerance
            && point.0 <= x1 + tolerance
            && point.1 >= y0 - tolerance
            && point.1 <= y1 + tolerance
        {
            hits.push(Hit::Element(index));
        }
    }
    for (index, flyline) in scene.flylines.iter().enumerate() {
        if segment_distance(point, flyline.from, flyline.to) <= tolerance {
            hits.push(Hit::Flyline(index));
        }
    }
    hits
}
