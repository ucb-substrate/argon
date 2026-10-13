//! Painting a schematic or symbol scene with gpui.

use argonc::compile::Align;
use gpui::{
    App, Bounds, ContentMask, Corners, Element, Font, Hsla, IntoElement, PaintQuad, Pixels, Point,
    Rgba, SharedString, TextRun, Window, fill, point, px, rgba,
};
use gpui::{Half, PathBuilder, Size};

use super::hit::Hit;
use super::scene::Scene;
use super::symbol_scene::SymbolScene;
use super::symbols::{Bounds as WorldBounds, PlacedSymbol, PlacedText, Pt, Shape};
use super::{SchematicCanvas, SymbolSelection};
use crate::editor::viewport::Viewport;
use crate::theme::Theme;
use argonc::compile::SymbolItemKind;

/// Below this many pixels per grid unit, elements are drawn as boxes.
pub const BOXES_ONLY_SCALE: f32 = 4.;
const WIRE_WIDTH: Pixels = px(2.);
const SHAPE_WIDTH: Pixels = px(1.5);
const PIN_SIZE: Pixels = px(6.);
const JUNCTION_DIAMETER: Pixels = px(7.);
const MIN_TEXT_PX: f32 = 5.;
const MAX_TEXT_PX: f32 = 64.;
const DOT_SPACING: f32 = 7.;
const DOT_DIAMETER: Pixels = px(3.);

/// The canvas as an element: it owns no state, and paints what the entity
/// holds.
pub struct SchematicElement {
    pub canvas: gpui::Entity<SchematicCanvas>,
}

impl IntoElement for SchematicElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for SchematicElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<gpui::ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let style = self.canvas.read(cx).bg_style.clone();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.canvas
            .update(cx, |canvas, cx| canvas.before_paint(bounds, cx));
        let frame = self.canvas.read(cx).frame(cx);
        window.with_content_mask(Some(ContentMask { bounds }), |window| {
            paint_frame(&frame, bounds, window, cx);
        });
    }
}

/// What one paint of the canvas shows.
pub struct Frame {
    pub theme: &'static Theme,
    pub viewport: Viewport,
    pub content: FrameContent,
    pub selection: Vec<Hit>,
    pub hover: Option<Hit>,
    pub highlighted_net: Option<argonc::compile::NetIdx>,
    pub symbol_selection: Vec<SymbolSelection>,
    /// Whether an edit is drawn before its compiled result arrived.
    pub pending: bool,
    /// What a tool draws under the pointer, and new items whose compiled
    /// result has not arrived.
    pub overlays: Vec<Overlay>,
    /// The handles of the symbol selection.
    pub handles: Vec<Pt>,
}

/// Something drawn over the scene: a tool's preview, or a new item waiting
/// for its compiled result.
#[derive(Debug, Clone, PartialEq)]
pub enum Overlay {
    Symbol(PlacedSymbol),
    Wire(Vec<Pt>),
    Signal { at: Pt, name: String, port: bool },
    Shape(Shape),
    Text(PlacedText),
    Pin { at: Pt, name: String },
}

pub enum FrameContent {
    /// Nothing is compiled yet.
    Nothing,
    Schematic(std::sync::Arc<Scene>),
    Symbol(std::sync::Arc<SymbolScene>),
}

fn to_px(viewport: Viewport, bounds: Bounds<Pixels>, (x, y): Pt) -> Point<Pixels> {
    viewport.to_px(bounds, (x as f32, y as f32))
}

fn stroke(window: &mut Window, points: &[Point<Pixels>], width: Pixels, color: Rgba) {
    if points.len() < 2 {
        return;
    }
    let mut path = PathBuilder::stroke(width);
    path.move_to(points[0]);
    for point in &points[1..] {
        path.line_to(*point);
    }
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn fill_polygon(window: &mut Window, points: &[Point<Pixels>], color: Rgba) {
    if points.len() < 3 {
        return;
    }
    let mut path = PathBuilder::fill();
    path.add_polygon(points, true);
    if let Ok(path) = path.build() {
        window.paint_path(path, color);
    }
}

fn circle_points(center: Point<Pixels>, radius: Pixels) -> Vec<Point<Pixels>> {
    const SEGMENTS: usize = 32;
    (0..SEGMENTS)
        .map(|index| {
            let angle = index as f32 / SEGMENTS as f32 * std::f32::consts::TAU;
            point(
                center.x + radius * angle.cos(),
                center.y - radius * angle.sin(),
            )
        })
        .collect()
}

fn arc_points(center: Point<Pixels>, radius: Pixels, start: f64, end: f64) -> Vec<Point<Pixels>> {
    let mut sweep = (end - start).rem_euclid(360.);
    if sweep == 0. {
        sweep = 360.;
    }
    let segments = ((sweep / 360. * 32.).ceil() as usize).max(2);
    (0..=segments)
        .map(|index| {
            let angle = (start + sweep * index as f64 / segments as f64).to_radians() as f32;
            point(
                center.x + radius * angle.cos(),
                center.y - radius * angle.sin(),
            )
        })
        .collect()
}

fn dot(window: &mut Window, center: Point<Pixels>, diameter: Pixels, color: Rgba) {
    let radius = diameter.half();
    let mut quad = fill(
        Bounds::new(
            point(center.x - radius, center.y - radius),
            Size::new(diameter, diameter),
        ),
        color,
    );
    quad.corner_radii = Corners::all(radius);
    window.paint_quad(quad);
}

fn dotted(window: &mut Window, from: Point<Pixels>, to: Point<Pixels>, color: Rgba) {
    let dx = f32::from(to.x - from.x);
    let dy = f32::from(to.y - from.y);
    let length = dx.hypot(dy);
    let count = (length / DOT_SPACING).ceil().max(1.) as usize;
    for index in 0..count {
        let t = (index as f32 + 0.5) / count as f32;
        dot(
            window,
            point(from.x + px(dx * t), from.y + px(dy * t)),
            DOT_DIAMETER,
            color,
        );
    }
}

fn square(window: &mut Window, center: Point<Pixels>, size: Pixels, color: Rgba) {
    let half = size.half();
    window.paint_quad(fill(
        Bounds::new(
            point(center.x - half, center.y - half),
            Size::new(size, size),
        ),
        color,
    ));
}

fn outline(window: &mut Window, bounds: Bounds<Pixels>, color: Rgba, width: Pixels) {
    let quad = PaintQuad {
        bounds,
        corner_radii: Corners::all(px(0.)),
        background: rgba(0).into(),
        border_widths: gpui::Edges::all(width),
        border_color: color.into(),
        border_styles: gpui::Edges::all(gpui::BorderStyle::Solid),
    };
    window.paint_quad(quad);
}

/// A text run of `len` bytes in `color`.
fn run(len: usize, font: Font, color: Rgba) -> TextRun {
    TextRun {
        len,
        font,
        color: Hsla::from(color),
        background_color: None,
        underline: None,
        strikethrough: None,
    }
}

/// Paints `text` anchored at `anchor`, which is its vertical center and its
/// left end, center, or right end by `align`; vertical text runs bottom to
/// top.
#[expect(
    clippy::too_many_arguments,
    reason = "one anchor, its typography, and a color"
)]
pub fn paint_text(
    window: &mut Window,
    cx: &mut App,
    text: &str,
    anchor: Point<Pixels>,
    font_px: f32,
    align: Align,
    vertical: bool,
    color: Rgba,
) {
    if !(MIN_TEXT_PX..=MAX_TEXT_PX).contains(&font_px) || text.is_empty() {
        return;
    }
    let font = window.text_style().font();
    let font_size = px(font_px);
    let line_height = px(font_px * 1.2);
    if !vertical {
        let shaped = window.text_system().shape_line(
            SharedString::from(text.to_owned()),
            font_size,
            &[run(text.len(), font, color)],
            None,
        );
        let x = match align {
            Align::Left => anchor.x,
            Align::Center => anchor.x - shaped.width.half(),
            Align::Right => anchor.x - shaped.width,
        };
        let origin = point(x, anchor.y - line_height.half());
        let _ = shaped.paint(origin, line_height, window, cx);
        return;
    }
    // Stack the characters upward.
    let chars = text.chars().collect::<Vec<_>>();
    let total = line_height * chars.len() as f32;
    let bottom = match align {
        Align::Left => anchor.y,
        Align::Center => anchor.y + total.half(),
        Align::Right => anchor.y + total,
    };
    for (index, character) in chars.iter().enumerate() {
        let text = character.to_string();
        let shaped = window.text_system().shape_line(
            SharedString::from(text.clone()),
            font_size,
            &[run(text.len(), font.clone(), color)],
            None,
        );
        let origin = point(
            anchor.x - shaped.width.half(),
            bottom - line_height * (index as f32 + 1.),
        );
        let _ = shaped.paint(origin, line_height, window, cx);
    }
}

fn paint_placed_text(
    window: &mut Window,
    cx: &mut App,
    viewport: Viewport,
    bounds: Bounds<Pixels>,
    text: &PlacedText,
    color: Rgba,
) {
    paint_text(
        window,
        cx,
        &text.text,
        to_px(viewport, bounds, text.anchor),
        text.size as f32 * viewport.scale,
        text.align,
        text.vertical,
        color,
    );
}

fn paint_shape(
    window: &mut Window,
    viewport: Viewport,
    bounds: Bounds<Pixels>,
    shape: &Shape,
    color: Rgba,
) {
    match shape {
        Shape::Line(points) => {
            let points = points
                .iter()
                .map(|p| to_px(viewport, bounds, *p))
                .collect::<Vec<_>>();
            stroke(window, &points, SHAPE_WIDTH, color);
        }
        Shape::Polygon { points, fill } => {
            let mut points = points
                .iter()
                .map(|p| to_px(viewport, bounds, *p))
                .collect::<Vec<_>>();
            if *fill {
                fill_polygon(window, &points, color);
            }
            if let Some(first) = points.first().copied() {
                points.push(first);
            }
            stroke(window, &points, SHAPE_WIDTH, color);
        }
        Shape::Circle { center, r, fill } => {
            let center = to_px(viewport, bounds, *center);
            let mut points = circle_points(center, px(*r as f32 * viewport.scale));
            if *fill {
                fill_polygon(window, &points, color);
            }
            points.push(points[0]);
            stroke(window, &points, SHAPE_WIDTH, color);
        }
        Shape::Arc {
            center,
            r,
            start,
            end,
        } => {
            let center = to_px(viewport, bounds, *center);
            let points = arc_points(center, px(*r as f32 * viewport.scale), *start, *end);
            stroke(window, &points, SHAPE_WIDTH, color);
        }
    }
}

fn world_bounds_px(
    viewport: Viewport,
    bounds: Bounds<Pixels>,
    (x0, y0, x1, y1): WorldBounds,
) -> Bounds<Pixels> {
    let top_left = to_px(viewport, bounds, (x0, y1));
    let bottom_right = to_px(viewport, bounds, (x1, y0));
    Bounds::from_corners(top_left, bottom_right)
}

fn intersects(a: Bounds<Pixels>, b: Bounds<Pixels>) -> bool {
    a.origin.x <= b.origin.x + b.size.width
        && b.origin.x <= a.origin.x + a.size.width
        && a.origin.y <= b.origin.y + b.size.height
        && b.origin.y <= a.origin.y + a.size.height
}

fn paint_placed_symbol(
    window: &mut Window,
    cx: &mut App,
    viewport: Viewport,
    bounds: Bounds<Pixels>,
    symbol: &PlacedSymbol,
    color: Rgba,
    pin_color: Rgba,
) {
    for shape in &symbol.shapes {
        paint_shape(window, viewport, bounds, shape, color);
    }
    for pin in symbol.pins.iter().flatten() {
        square(
            window,
            to_px(viewport, bounds, (pin.0 as f64, pin.1 as f64)),
            PIN_SIZE,
            pin_color,
        );
    }
    for text in &symbol.texts {
        paint_placed_text(window, cx, viewport, bounds, text, color);
    }
}

fn paint_axes(window: &mut Window, viewport: Viewport, bounds: Bounds<Pixels>, theme: &Theme) {
    let origin = to_px(viewport, bounds, (0., 0.));
    let color = Rgba {
        a: 0.5,
        ..theme.axes
    };
    if origin.x >= bounds.origin.x && origin.x <= bounds.origin.x + bounds.size.width {
        window.paint_quad(fill(
            Bounds::new(
                point(origin.x, bounds.origin.y),
                Size::new(px(1.), bounds.size.height),
            ),
            color,
        ));
    }
    if origin.y >= bounds.origin.y && origin.y <= bounds.origin.y + bounds.size.height {
        window.paint_quad(fill(
            Bounds::new(
                point(bounds.origin.x, origin.y),
                Size::new(bounds.size.width, px(1.)),
            ),
            color,
        ));
    }
}

fn paint_hint(
    window: &mut Window,
    cx: &mut App,
    bounds: Bounds<Pixels>,
    theme: &Theme,
    hint: &str,
) {
    paint_text(
        window,
        cx,
        hint,
        bounds.center(),
        14.,
        Align::Center,
        false,
        theme.subtext,
    );
}

/// Paints `frame` within `bounds`.
pub fn paint_frame(frame: &Frame, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    let theme = frame.theme;
    window.paint_quad(fill(bounds, theme.bg));
    match &frame.content {
        FrameContent::Nothing => {}
        FrameContent::Schematic(scene) => paint_scene(frame, scene, bounds, window, cx),
        FrameContent::Symbol(scene) => paint_symbol_scene(frame, scene, bounds, window, cx),
    }
}

fn paint_scene(
    frame: &Frame,
    scene: &Scene,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let theme = frame.theme;
    let viewport = frame.viewport;
    paint_axes(window, viewport, bounds, theme);
    if scene.empty {
        paint_hint(
            window,
            cx,
            bounds,
            theme,
            "No schematic yet: press p to add a port or i to place an instance",
        );
        return;
    }
    let boxes_only = viewport.scale < BOXES_ONLY_SCALE;
    let highlighted = |net: argonc::compile::NetIdx| frame.highlighted_net == Some(net);

    for (index, element) in scene.elements.iter().enumerate() {
        let element_bounds = world_bounds_px(viewport, bounds, element.bounds);
        if !intersects(element_bounds, bounds) {
            continue;
        }
        let selected = frame.selection.contains(&Hit::Element(index));
        let hovered = frame.hover == Some(Hit::Element(index));
        if selected || hovered {
            window.paint_quad(fill(
                element_bounds,
                if selected {
                    theme.selection
                } else {
                    theme.hover
                },
            ));
        }
        let color = if element.editable {
            theme.symbol
        } else {
            theme.locked
        };
        if boxes_only {
            outline(window, element_bounds, color, px(1.));
            continue;
        }
        paint_placed_symbol(
            window,
            cx,
            viewport,
            bounds,
            &element.symbol,
            color,
            theme.pin,
        );
        // Key parameters beside a device, since its helper knows none.
        if !element.params.is_empty() {
            let (x1, y0) = (element.bounds.2, element.bounds.0);
            let text = element
                .params
                .iter()
                .map(|(name, value)| format!("{name}={value}"))
                .collect::<Vec<_>>()
                .join(" ");
            paint_text(
                window,
                cx,
                &text,
                to_px(viewport, bounds, (x1 + 0.5, y0 + 0.5)),
                0.8 * viewport.scale,
                Align::Left,
                false,
                theme.subtext,
            );
        }
        for (terminal, pin) in element.pins.iter().enumerate() {
            let Some(position) = pin.position else {
                continue;
            };
            let hit = Hit::Pin {
                element: index,
                terminal,
            };
            if frame.hover == Some(hit) || frame.selection.contains(&hit) || highlighted(pin.net) {
                let center = to_px(viewport, bounds, (position.0 as f64, position.1 as f64));
                dot(window, center, PIN_SIZE * 2., theme.hover);
            }
        }
    }

    // Wires, junctions, and flylines.
    for segment in &scene.segments {
        let color = if highlighted(segment.net) {
            theme.axes
        } else {
            theme.wire
        };
        let a = to_px(viewport, bounds, segment.a);
        let b = to_px(viewport, bounds, segment.b);
        stroke(window, &[a, b], WIRE_WIDTH, color);
    }
    for (index, wire) in scene.wires.iter().enumerate() {
        let selected = frame.selection.contains(&Hit::Wire(index));
        let hovered = frame.hover == Some(Hit::Wire(index));
        if !(selected || hovered) && wire.editable {
            continue;
        }
        let points = wire
            .points
            .iter()
            .map(|p| to_px(viewport, bounds, *p))
            .collect::<Vec<_>>();
        let color = if selected {
            Rgba {
                a: 1.,
                ..theme.selection
            }
        } else if hovered {
            Rgba {
                a: 1.,
                ..theme.hover
            }
        } else {
            theme.locked
        };
        stroke(window, &points, WIRE_WIDTH * 2., color);
    }
    for junction in &scene.junctions {
        dot(
            window,
            to_px(viewport, bounds, junction.at),
            JUNCTION_DIAMETER,
            theme.wire,
        );
    }
    for (index, flyline) in scene.flylines.iter().enumerate() {
        let color = if frame.selection.contains(&Hit::Flyline(index))
            || frame.hover == Some(Hit::Flyline(index))
        {
            Rgba {
                a: 1.,
                ..theme.selection
            }
        } else if highlighted(flyline.net) {
            theme.axes
        } else {
            theme.flyline
        };
        dotted(
            window,
            to_px(viewport, bounds, flyline.from),
            to_px(viewport, bounds, flyline.to),
            color,
        );
    }

    // Ports and labels.
    for (index, port) in scene.ports.iter().enumerate() {
        let center = to_px(
            viewport,
            bounds,
            (port.position.0 as f64, port.position.1 as f64),
        );
        let selected = frame.selection.contains(&Hit::Port(index));
        if selected || frame.hover == Some(Hit::Port(index)) || highlighted(port.net) {
            dot(
                window,
                center,
                PIN_SIZE * 2.5,
                if selected {
                    theme.selection
                } else {
                    theme.hover
                },
            );
        }
        let color = if port.editable {
            theme.pin
        } else {
            theme.locked
        };
        // A port pin: a diamond with its name above.
        let radius = PIN_SIZE;
        let diamond = [
            point(center.x, center.y - radius),
            point(center.x + radius, center.y),
            point(center.x, center.y + radius),
            point(center.x - radius, center.y),
        ];
        fill_polygon(window, &diamond, color);
        paint_text(
            window,
            cx,
            &port.name,
            point(center.x, center.y - radius - px(0.5 * viewport.scale)),
            0.9 * viewport.scale,
            Align::Center,
            false,
            theme.symbol,
        );
    }
    for (index, label) in scene.labels.iter().enumerate() {
        let center = to_px(
            viewport,
            bounds,
            (label.position.0 as f64, label.position.1 as f64),
        );
        let selected = frame.selection.contains(&Hit::Label(index));
        if selected || frame.hover == Some(Hit::Label(index)) || highlighted(label.net) {
            dot(
                window,
                center,
                PIN_SIZE * 2.5,
                if selected {
                    theme.selection
                } else {
                    theme.hover
                },
            );
        }
        let color = if label.editable {
            theme.wire
        } else {
            theme.locked
        };
        // A net label: a tick with the name beside it.
        stroke(
            window,
            &[center, point(center.x, center.y - px(0.6 * viewport.scale))],
            SHAPE_WIDTH,
            color,
        );
        paint_text(
            window,
            cx,
            &label.name,
            point(
                center.x + px(0.3 * viewport.scale),
                center.y - px(0.6 * viewport.scale),
            ),
            0.9 * viewport.scale,
            Align::Left,
            false,
            theme.symbol,
        );
    }

    paint_overlays(frame, bounds, window, cx);

    if frame.pending {
        paint_text(
            window,
            cx,
            "Applying edit…",
            point(
                bounds.origin.x + bounds.size.width.half(),
                bounds.origin.y + px(20.),
            ),
            13.,
            Align::Center,
            false,
            theme.subtext,
        );
    }

    // Conflicts last, so they are never hidden.
    for conflict in &scene.conflicts {
        let center = to_px(viewport, bounds, conflict.at);
        let r = px(6.);
        stroke(
            window,
            &[
                point(center.x - r, center.y - r),
                point(center.x + r, center.y + r),
            ],
            WIRE_WIDTH,
            theme.conflict,
        );
        stroke(
            window,
            &[
                point(center.x - r, center.y + r),
                point(center.x + r, center.y - r),
            ],
            WIRE_WIDTH,
            theme.conflict,
        );
    }
}

/// Paints the tool previews and ghosts, translucent.
fn paint_overlays(frame: &Frame, bounds: Bounds<Pixels>, window: &mut Window, cx: &mut App) {
    let theme = frame.theme;
    let viewport = frame.viewport;
    let faint = |color: Rgba| Rgba { a: 0.55, ..color };
    for overlay in &frame.overlays {
        match overlay {
            Overlay::Symbol(symbol) => paint_placed_symbol(
                window,
                cx,
                viewport,
                bounds,
                symbol,
                faint(theme.symbol),
                faint(theme.pin),
            ),
            Overlay::Wire(points) => {
                let points = points
                    .iter()
                    .map(|p| to_px(viewport, bounds, *p))
                    .collect::<Vec<_>>();
                stroke(window, &points, WIRE_WIDTH, faint(theme.wire));
                for point in &points {
                    dot(window, *point, PIN_SIZE, faint(theme.wire));
                }
            }
            Overlay::Shape(shape) => {
                paint_shape(window, viewport, bounds, shape, faint(theme.symbol))
            }
            Overlay::Text(text) => {
                paint_placed_text(window, cx, viewport, bounds, text, faint(theme.symbol));
            }
            Overlay::Pin { at, name } => {
                let center = to_px(viewport, bounds, *at);
                square(window, center, PIN_SIZE, faint(theme.pin));
                paint_text(
                    window,
                    cx,
                    name,
                    point(center.x, center.y - PIN_SIZE - px(0.3 * viewport.scale)),
                    0.8 * viewport.scale,
                    Align::Center,
                    false,
                    faint(theme.subtext),
                );
            }
            Overlay::Signal { at, name, port } => {
                let center = to_px(viewport, bounds, *at);
                let radius = PIN_SIZE;
                if *port {
                    let diamond = [
                        point(center.x, center.y - radius),
                        point(center.x + radius, center.y),
                        point(center.x, center.y + radius),
                        point(center.x - radius, center.y),
                    ];
                    fill_polygon(window, &diamond, faint(theme.pin));
                } else {
                    stroke(
                        window,
                        &[center, point(center.x, center.y - px(0.6 * viewport.scale))],
                        SHAPE_WIDTH,
                        faint(theme.wire),
                    );
                }
                paint_text(
                    window,
                    cx,
                    name,
                    point(center.x, center.y - radius - px(0.5 * viewport.scale)),
                    0.9 * viewport.scale,
                    Align::Center,
                    false,
                    faint(theme.symbol),
                );
            }
        }
    }
}

fn paint_symbol_scene(
    frame: &Frame,
    scene: &SymbolScene,
    bounds: Bounds<Pixels>,
    window: &mut Window,
    cx: &mut App,
) {
    let theme = frame.theme;
    let viewport = frame.viewport;
    paint_axes(window, viewport, bounds, theme);
    // The origin mark.
    let origin = to_px(viewport, bounds, (0., 0.));
    let r = px(5.);
    stroke(
        window,
        &[point(origin.x - r, origin.y), point(origin.x + r, origin.y)],
        SHAPE_WIDTH,
        theme.axes,
    );
    stroke(
        window,
        &[point(origin.x, origin.y - r), point(origin.x, origin.y + r)],
        SHAPE_WIDTH,
        theme.axes,
    );

    // Items map to the placed shapes and texts in order.
    let (mut shape_index, mut text_index) = (0, 0);
    for (index, item) in scene.items.iter().enumerate() {
        let selected = frame
            .symbol_selection
            .contains(&SymbolSelection::Item(index));
        let color = if scene.generated || !item.editable {
            theme.locked
        } else {
            theme.symbol
        };
        match item.kind {
            SymbolItemKind::Text { .. } => {
                if let Some(text) = scene.placed.texts.get(text_index) {
                    if selected {
                        let anchor = to_px(viewport, bounds, text.anchor);
                        dot(window, anchor, PIN_SIZE * 2.5, theme.selection);
                    }
                    paint_placed_text(window, cx, viewport, bounds, text, color);
                }
                text_index += 1;
            }
            _ => {
                if let Some(shape) = scene.placed.shapes.get(shape_index) {
                    if selected && let Some(shape_bounds) = shape.bounds() {
                        window.paint_quad(fill(
                            world_bounds_px(viewport, bounds, shape_bounds),
                            theme.selection,
                        ));
                    }
                    paint_shape(window, viewport, bounds, shape, color);
                }
                shape_index += 1;
            }
        }
    }
    // Pin labels follow the items' texts.
    let color = if scene.generated {
        theme.locked
    } else {
        theme.symbol
    };
    for text in scene.placed.texts.iter().skip(text_index) {
        paint_placed_text(window, cx, viewport, bounds, text, color);
    }
    for pin in &scene.pins {
        let center = to_px(
            viewport,
            bounds,
            (pin.position.0 as f64, pin.position.1 as f64),
        );
        if frame
            .symbol_selection
            .contains(&SymbolSelection::Pin(pin.port))
        {
            dot(window, center, PIN_SIZE * 2.5, theme.selection);
        }
        square(
            window,
            center,
            PIN_SIZE,
            if pin.editable {
                theme.pin
            } else {
                theme.locked
            },
        );
        // Every pin shows its port's name here, labeled or not.
        if !pin.labeled {
            paint_text(
                window,
                cx,
                &pin.name,
                point(center.x, center.y - PIN_SIZE - px(0.3 * viewport.scale)),
                0.8 * viewport.scale,
                Align::Center,
                false,
                theme.subtext,
            );
        }
    }
    for handle in &frame.handles {
        let center = to_px(viewport, bounds, *handle);
        square(window, center, PIN_SIZE, theme.axes);
    }
    paint_overlays(frame, bounds, window, cx);
    if scene.generated {
        paint_text(
            window,
            cx,
            "Generated symbol: the cell draws none of its own",
            point(
                bounds.origin.x + bounds.size.width.half(),
                bounds.origin.y + px(20.),
            ),
            13.,
            Align::Center,
            false,
            theme.subtext,
        );
    }
}
