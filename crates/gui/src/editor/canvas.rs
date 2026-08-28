use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Debug,
    ops::{Add, Sub},
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicU64, Ordering},
    },
};

use analyzer::rpc::{
    DimensionParams, DrawSegmentConstraint, InitialConditionEdit, InstancePreview, PathParams,
    PolygonParams, ValueEdit,
};
use argonc::{
    ast::Span,
    compile::{self, CellId, CompiledData, ObjectId, RectInitialCondition, SolvedValue, ifmatvec},
    solver::{LinearExpr, Var},
};
use enumify::enumify;
use geometry::{dir::Dir, transform::TransformationMatrix};
use gpui::{
    App, AppContext, BorderStyle, Bounds, Context, Corners, DefiniteLength, Edges, Element, Entity,
    FillOptions, FocusHandle, Focusable, Half, InteractiveElement, IntoElement, Length,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, ParentElement,
    PathBuilder, PathStyle, Pixels, Point, Render, RenderImage, Rgba, ScrollWheelEvent,
    SharedString, Size, Style, Styled, Subscription, Task, TextRun, Window, div, pattern_slash, px,
    rgb, size, solid_background,
};
use indexmap::{IndexMap, IndexSet};
use itertools::Itertools;
use tower_lsp_server::ls_types::MessageType;

use crate::{
    actions::*,
    editor::{
        self, CompileOutputState, EditorState, LayerState, PreparedCompilationSnapshot,
        SOURCE_EDIT_REJECTED_MESSAGE, ScopeAddress, input::TextInput,
    },
    sse::SparseVec,
};

#[derive(Copy, Clone, PartialEq)]
pub enum ShapeFill {
    Stippling,
    Solid,
    Hollow,
}

const SELECT_WIDTH: Pixels = px(3.);
const DEFAULT_BORDER_WIDTH: Pixels = px(2.);
/// Above this size, retain a viewport raster for interactive redraws. GPUI can
/// replay a clean retained view, but pan/zoom dirties this immediate element;
/// transforming one sprite avoids rebuilding hundreds of thousands of quads.
const RASTER_CACHE_GEOMETRY_THRESHOLD: usize = 50_000;
/// Rasterize geometry at half resolution so complex fills remain cheap. The
/// completed pixels are expanded with nearest-neighbor sampling before GPUI
/// sees them; this preserves discrete layer colors despite GPUI's hard-coded
/// linear image sampler.
const RASTER_CACHE_RESOLUTION: f32 = 0.5;
/// Retain exact outline geometry only while rerasterizing it costs at most a
/// third of a viewport pass. Denser views use the normal background rerender:
/// scaling a completed outline field is not a valid fallback in either zoom
/// direction because isolated outlines can change width or disappear.
const OUTLINE_REPROJECTION_WORK_DIVISOR: usize = 3;
const OUTLINE_REPROJECTION_PRIMITIVE_LIMIT: usize = 50_000;
/// Minimum complete raster pixels required between opposing one-pixel borders.
/// Normal rasterization can represent a shape as soon as both borders fit;
/// genuinely narrower geometry still becomes one stable occupancy feature.
const MIN_RESOLVABLE_OUTLINE_GAP_RASTER_PIXELS: f32 = 0.;
const NAVIGATION_OVERVIEW_SIZE: Pixels = px(160.);
/// Geometry smaller than this in the interaction raster is accumulated into a
/// compact per-layer occupancy mask. Hierarchy is still fully flattened; only
/// the final sub-pixel representation becomes cheaper.
const GEOMETRY_LOD_SIZE_PX: f32 = 1.;
/// Disabled: cell-local raster tiles can turn dense bitcells into opaque
/// blocks at intermediate zoom levels. Keep the implementation isolated while
/// the flattened per-layer occupancy path remains authoritative.
const CELL_RASTER_TILES_ENABLED: bool = false;
const CELL_RASTER_TILE_MAX_SIZE: u32 = 16;
const CELL_RASTER_TILE_CACHE_LIMIT: usize = 256;
/// Keep a 5x5 field of viewport-sized rasters. The center tile is always built
/// first, followed by the two Chebyshev-distance rings around it.
const NAVIGATION_TILE_RADIUS: i32 = 2;
const TEXT_LAYOUT_SIZE: f32 = 16.;
const SCOPE_TEXT_LAYOUT_SIZE: f32 = 12.;
const MIN_READABLE_TEXT_PX: f32 = 5.;
const MAX_TEXT_PX: f32 = 64.;
const DEFAULT_DRAW_PATH_WIDTH: f32 = 20.;
const DOT_DIAMETER: Pixels = px(3.);
const DOT_SPACING: Pixels = px(7.);
/// Side length of the square drag handles drawn on unconstrained edges.
const HANDLE_SIZE: Pixels = px(12.);
/// Side length of the (larger, invisible) clickable area around each handle, so
/// the handle is easy to grab without pixel-perfect aim.
const HANDLE_HIT: Pixels = px(22.);
/// Fill / border colors of the SSE drag handles.
const HANDLE_FILL: u32 = 0x3b9dff;
const HANDLE_BORDER: u32 = 0xffffff;

/// One expression controlled by a solution-space drag and the layout-space
/// direction from which its requested displacement is taken.
#[derive(Clone)]
struct SseDragTarget {
    expr: LinearExpr,
    normal: Point<f32>,
    source: Option<SseSourceTarget>,
}

#[derive(Clone)]
struct SseSourceTarget {
    call_span: Span,
    name: String,
    value: f64,
}

type SseSourceCoordinates = HashMap<Span, Vec<(LinearExpr, String, f64)>>;

fn sse_source_target(
    coordinates: &SseSourceCoordinates,
    call_span: &Span,
    expr: &LinearExpr,
) -> Option<SseSourceTarget> {
    coordinates
        .get(call_span)?
        .iter()
        .find_map(|(candidate, name, value)| {
            (candidate == expr).then(|| SseSourceTarget {
                call_span: call_span.clone(),
                name: name.clone(),
                value: *value,
            })
        })
}

fn sourced_sse_target(
    expr: &LinearExpr,
    normal: Point<f32>,
    call_span: &Span,
    coordinates: &SseSourceCoordinates,
) -> SseDragTarget {
    SseDragTarget {
        expr: expr.clone(),
        normal,
        source: sse_source_target(coordinates, call_span, expr),
    }
}

fn sourced_corner_sse_targets(
    x: &LinearExpr,
    y: &LinearExpr,
    call_span: &Span,
    coordinates: &SseSourceCoordinates,
) -> Vec<SseDragTarget> {
    vec![
        sourced_sse_target(x, Point::new(1., 0.), call_span, coordinates),
        sourced_sse_target(y, Point::new(0., 1.), call_span, coordinates),
    ]
}

#[derive(Clone)]
struct SseHandle {
    bounds: Bounds<Pixels>,
    targets: Vec<SseDragTarget>,
}

#[derive(Clone)]
struct SseBody {
    bounds: Bounds<Pixels>,
    span: Span,
    targets: Vec<SseDragTarget>,
}

#[derive(Clone)]
struct LabeledBbox {
    rect: Rect,
    label: SharedString,
    /// Layout-space cell origin. Present for instance bboxes, absent for scopes.
    origin: Option<Point<f32>>,
}

#[derive(Clone)]
struct TextLabel {
    text: SharedString,
    position: Point<f32>,
    layer: LayerState,
}

fn corner_sse_targets(x: &LinearExpr, y: &LinearExpr) -> Vec<SseDragTarget> {
    vec![
        SseDragTarget {
            expr: x.clone(),
            normal: Point::new(1., 0.),
            source: None,
        },
        SseDragTarget {
            expr: y.clone(),
            normal: Point::new(0., 1.),
            source: None,
        },
    ]
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
enum SelectionLayer {
    Scope,
    Layout(usize),
    Overlay,
}

#[derive(Clone, Debug)]
enum SelectionOutline {
    Rect {
        bounds: Bounds<Pixels>,
        border_styles: Edges<BorderStyle>,
    },
    Polygon {
        points: Vec<Point<Pixels>>,
        edge_styles: Vec<BorderStyle>,
    },
    Polyline {
        points: Vec<Point<Pixels>>,
        segment_styles: Vec<BorderStyle>,
    },
}

fn rect_selection_outline(
    bounds: Bounds<Pixels>,
    border_styles: Edges<BorderStyle>,
) -> SelectionOutline {
    SelectionOutline::Rect {
        bounds,
        border_styles,
    }
}

#[derive(Clone, Debug)]
struct SelectionHit {
    span: Span,
    outline: SelectionOutline,
    layer: SelectionLayer,
    creation_order: Vec<u64>,
}

#[cfg(test)]
fn polygon_area(points: &[Point<Pixels>]) -> f32 {
    points
        .iter()
        .zip(points.iter().cycle().skip(1))
        .take(points.len())
        .map(|(a, b)| f32::from(a.x) * f32::from(b.y) - f32::from(b.x) * f32::from(a.y))
        .sum::<f32>()
        .abs()
        / 2.
}

fn point_in_polygon(point: Point<Pixels>, polygon: &[Point<Pixels>]) -> bool {
    if polygon.len() < 3 {
        return false;
    }

    let (px, py) = (f32::from(point.x), f32::from(point.y));
    let mut inside = false;
    for (a, b) in polygon
        .iter()
        .zip(polygon.iter().cycle().skip(1))
        .take(polygon.len())
    {
        let (ax, ay) = (f32::from(a.x), f32::from(a.y));
        let (bx, by) = (f32::from(b.x), f32::from(b.y));

        // Treat the border as part of the polygon so its visible stroke remains
        // selectable without extending the hit region to a bounding box.
        let cross = (px - ax) * (by - ay) - (py - ay) * (bx - ax);
        let scale = (bx - ax).abs().max((by - ay).abs()).max(1.);
        if cross.abs() <= f32::EPSILON * scale
            && px >= ax.min(bx)
            && px <= ax.max(bx)
            && py >= ay.min(by)
            && py <= ay.max(by)
        {
            return true;
        }

        if (ay > py) != (by > py) && px < (bx - ax) * (py - ay) / (by - ay) + ax {
            inside = !inside;
        }
    }
    inside
}

fn paint_polygon_border(
    window: &mut Window,
    points: &[Point<Pixels>],
    edge_styles: &[BorderStyle],
    width: Pixels,
    color: Rgba,
) {
    for index in 0..points.len() {
        if edge_styles.get(index) == Some(&BorderStyle::Dashed) {
            paint_dotted_segment(
                window,
                points[index],
                points[(index + 1) % points.len()],
                color,
            );
            continue;
        }
        let mut edge = PathBuilder::stroke(width);
        edge.move_to(points[index]);
        edge.line_to(points[(index + 1) % points.len()]);
        if let Ok(path) = edge.build() {
            window.paint_path(path, color);
        }
    }
}

fn dotted_segment_centers(from: Point<Pixels>, to: Point<Pixels>) -> Vec<Point<Pixels>> {
    let dx = f32::from(to.x - from.x);
    let dy = f32::from(to.y - from.y);
    let length = dx.hypot(dy);
    let count = (length / f32::from(DOT_SPACING)).ceil().max(1.) as usize;
    (0..count)
        .map(|index| {
            let t = (index as f32 + 0.5) / count as f32;
            Point::new(from.x + px(dx * t), from.y + px(dy * t))
        })
        .collect()
}

fn paint_dotted_segment(window: &mut Window, from: Point<Pixels>, to: Point<Pixels>, color: Rgba) {
    let radius = DOT_DIAMETER.half();
    for center in dotted_segment_centers(from, to) {
        let mut dot = get_paint_quad(
            Bounds::new(
                Point::new(center.x - radius, center.y - radius),
                Size::new(DOT_DIAMETER, DOT_DIAMETER),
            ),
            ShapeFill::Solid,
            color,
            color,
            Edges::all(px(0.)),
            Edges::all(BorderStyle::Solid),
        );
        dot.corner_radii = Corners::all(radius);
        window.paint_quad(dot);
    }
}

fn paint_polygon_fill(
    window: &mut Window,
    points: &[Point<Pixels>],
    layer: &LayerState,
    self_overlapping_path: bool,
) {
    if layer.fill == ShapeFill::Hollow {
        return;
    }
    let mut fill = if self_overlapping_path {
        PathBuilder::fill().with_style(PathStyle::Fill(FillOptions::non_zero()))
    } else {
        PathBuilder::fill()
    };
    fill.add_polygon(points, true);
    if let Ok(path) = fill.build() {
        let background = match layer.fill {
            ShapeFill::Solid => solid_background(layer.color),
            ShapeFill::Stippling => pattern_slash(layer.color.into(), 1., 9.),
            ShapeFill::Hollow => unreachable!(),
        };
        window.paint_path(path, background);
    }
}

fn paint_polyline(
    window: &mut Window,
    points: &[Point<Pixels>],
    segment_styles: &[BorderStyle],
    width: Pixels,
    color: Rgba,
) {
    for (index, points) in points.windows(2).enumerate() {
        if segment_styles.get(index) == Some(&BorderStyle::Dashed) {
            paint_dotted_segment(window, points[0], points[1], color);
            continue;
        }
        let mut segment = PathBuilder::stroke(width);
        segment.move_to(points[0]);
        segment.line_to(points[1]);
        if let Ok(path) = segment.build() {
            window.paint_path(path, color);
        }
    }
}

fn ordered_selection_hits(mut hits: Vec<SelectionHit>) -> Vec<SelectionHit> {
    hits.sort_by(|a, b| {
        b.layer
            .cmp(&a.layer)
            .then_with(|| b.creation_order.cmp(&a.creation_order))
    });

    // An object can contribute more than one hit box (dimensions, in
    // particular). Cycling should visit objects, not each of their hit boxes.
    let mut seen = HashSet::new();
    hits.retain(|hit| seen.insert(hit.span.clone()));
    hits
}

fn choose_selection_hit(
    hits: Vec<SelectionHit>,
    selected: Option<&Span>,
    cycle: bool,
) -> Option<SelectionHit> {
    let hits = ordered_selection_hits(hits);
    let index = if cycle {
        selected
            .and_then(|selected| hits.iter().position(|hit| hit.span == *selected))
            .map_or(0, |index| (index + 1) % hits.len().max(1))
    } else {
        0
    };
    hits.get(index).cloned()
}

/// Rectangle order for dimension-edge hit testing, matching
/// [`ordered_selection_hits`]. Scope bboxes sit below layout layers; within
/// layout, higher-z layers win, then later-created shapes.
fn ordered_dimension_rects<'a>(
    rects: &'a [(Rect, LayerState)],
    scope_rects: &'a [LabeledBbox],
) -> Vec<&'a Rect> {
    let mut candidates = rects
        .iter()
        .enumerate()
        .map(|(paint_order, (rect, layer))| {
            (
                rect,
                SelectionLayer::Layout(layer.z),
                if rect.object_path.is_empty() {
                    vec![paint_order as u64]
                } else {
                    rect.object_path
                        .iter()
                        .map(|id| id.creation_order())
                        .collect()
                },
            )
        })
        .chain(
            scope_rects
                .iter()
                .enumerate()
                .filter(|(_, bbox)| !bbox.rect.object_path.is_empty())
                .map(|(paint_order, bbox)| {
                    (
                        &bbox.rect,
                        SelectionLayer::Scope,
                        if bbox.rect.object_path.is_empty() {
                            vec![paint_order as u64]
                        } else {
                            bbox.rect
                                .object_path
                                .iter()
                                .map(|id| id.creation_order())
                                .collect()
                        },
                    )
                }),
        )
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| b.2.cmp(&a.2)));
    candidates.into_iter().map(|(rect, _, _)| rect).collect()
}

struct InitialConditionUpdate {
    span: Span,
    value: f64,
    changed: bool,
    target: Option<RectInitialCondition>,
}

#[derive(Clone, Debug)]
struct PendingSseValue {
    span: Option<Span>,
    value: f64,
}

#[derive(Clone, PartialEq, Debug)]
pub struct Rect {
    pub x0: f32,
    pub x1: f32,
    pub y0: f32,
    pub y1: f32,
    pub id: Option<Span>,
    /// Empty if not accessible.
    pub object_path: Vec<ObjectId>,
    pub border_widths: Edges<Pixels>,
    pub border_styles: Edges<BorderStyle>,
    pub cvars: Option<Edges<LinearExpr>>,
}

#[derive(Clone, Debug)]
struct Polygon {
    points: Vec<Point<f32>>,
    edge_styles: Vec<BorderStyle>,
    id: Option<Span>,
    object_path: Vec<ObjectId>,
    cvars: Option<Vec<(LinearExpr, LinearExpr)>>,
    centerline: Option<PathCenterline>,
}

#[derive(Clone, Debug)]
struct PathCenterline {
    points: Vec<Point<f32>>,
    segment_styles: Vec<BorderStyle>,
    cvars: Option<Vec<(LinearExpr, LinearExpr)>>,
}

impl Polygon {
    fn transform(&self, mat: TransformationMatrix, ofs: (f64, f64)) -> Self {
        Self {
            points: self
                .points
                .iter()
                .map(|point| {
                    let point = ifmatvec(mat, (point.x as f64, point.y as f64));
                    Point::new((point.0 + ofs.0) as f32, (point.1 + ofs.1) as f32)
                })
                .collect(),
            edge_styles: self.edge_styles.clone(),
            id: self.id.clone(),
            object_path: self.object_path.clone(),
            cvars: self.cvars.clone(),
            centerline: self.centerline.as_ref().map(|centerline| PathCenterline {
                points: centerline
                    .points
                    .iter()
                    .map(|point| {
                        let point = ifmatvec(mat, (point.x as f64, point.y as f64));
                        Point::new((point.0 + ofs.0) as f32, (point.1 + ofs.1) as f32)
                    })
                    .collect(),
                segment_styles: centerline.segment_styles.clone(),
                cvars: centerline.cvars.clone(),
            }),
        }
    }
}

fn polygon_edge_styles(
    point_count: usize,
    mut is_unconstrained: impl FnMut(usize) -> bool,
) -> Vec<BorderStyle> {
    if point_count == 0 {
        return Vec::new();
    }
    let unconstrained = (0..point_count)
        .map(&mut is_unconstrained)
        .collect::<Vec<_>>();
    (0..point_count)
        .map(|index| {
            if unconstrained[index] || unconstrained[(index + 1) % point_count] {
                BorderStyle::Dashed
            } else {
                BorderStyle::Solid
            }
        })
        .collect()
}

fn path_segment_styles(
    point_count: usize,
    mut is_unconstrained: impl FnMut(usize) -> bool,
) -> Vec<BorderStyle> {
    let unconstrained = (0..point_count)
        .map(&mut is_unconstrained)
        .collect::<Vec<_>>();
    unconstrained
        .windows(2)
        .map(|points| {
            if points[0] || points[1] {
                BorderStyle::Dashed
            } else {
                BorderStyle::Solid
            }
        })
        .collect()
}

fn snap_draw_point(origin: Point<f32>, cursor: Point<f32>) -> (Point<f32>, DrawSegmentConstraint) {
    let dx = cursor.x - origin.x;
    let dy = cursor.y - origin.y;
    let octant = (dy.atan2(dx) / std::f32::consts::FRAC_PI_4).round() as i32;
    match octant.rem_euclid(4) {
        0 => (
            Point::new(cursor.x, origin.y),
            DrawSegmentConstraint::Horizontal(0),
        ),
        2 => (
            Point::new(origin.x, cursor.y),
            DrawSegmentConstraint::Vertical(0),
        ),
        diagonal => {
            let distance = ((dx.abs() + dy.abs()) * 5.).round() / 10.;
            let x_sign = if dx < 0. { -1. } else { 1. };
            let y_sign = if diagonal == 1 { x_sign } else { -x_sign };
            let constraint = if diagonal == 1 {
                DrawSegmentConstraint::DiagonalPositive(0)
            } else {
                DrawSegmentConstraint::DiagonalNegative(0)
            };
            (
                Point::new(origin.x + x_sign * distance, origin.y + y_sign * distance),
                constraint,
            )
        }
    }
}

fn segment_constraint_with_end(
    constraint: DrawSegmentConstraint,
    end: usize,
) -> DrawSegmentConstraint {
    match constraint {
        DrawSegmentConstraint::Horizontal(_) => DrawSegmentConstraint::Horizontal(end),
        DrawSegmentConstraint::Vertical(_) => DrawSegmentConstraint::Vertical(end),
        DrawSegmentConstraint::DiagonalPositive(_) => DrawSegmentConstraint::DiagonalPositive(end),
        DrawSegmentConstraint::DiagonalNegative(_) => DrawSegmentConstraint::DiagonalNegative(end),
    }
}

fn draw_source_coordinate(value: f32, grid: f64) -> f64 {
    argonc::tech::snap(f64::from(value), grid)
}

fn format_dimension_label(value: f64, grid: f64) -> String {
    let value = compile::format_initial_condition(value, grid);
    value.strip_suffix('.').unwrap_or(&value).to_owned()
}

fn format_dimension_offset(offset: f64, quantum: f64) -> String {
    let (operator, magnitude) = if offset < 0. {
        ('-', -offset)
    } else {
        ('+', offset)
    };
    format!(
        "{operator} {}",
        compile::format_initial_condition(magnitude, quantum)
    )
}

#[derive(Clone, PartialEq, Debug)]
pub(crate) struct Edge<T> {
    pub(crate) dir: Dir,
    pub(crate) coord: T,
    pub(crate) start: T,
    pub(crate) stop: T,
}

impl<T> Edge<T> {
    fn select_bounds(&self, thickness: T) -> Bounds<T>
    where
        T: Clone + Debug + Default + PartialEq + Sub<Output = T> + Add<Output = T>,
    {
        match self.dir {
            Dir::Horiz => Bounds::new(
                Point::new(self.start.clone(), self.coord.clone() - thickness.clone()),
                Size::new(
                    self.stop.clone() - self.start.clone(),
                    thickness.clone() + thickness.clone(),
                ),
            ),
            Dir::Vert => Bounds::new(
                Point::new(self.coord.clone() - thickness.clone(), self.start.clone()),
                Size::new(
                    thickness.clone() + thickness,
                    self.stop.clone() - self.start.clone(),
                ),
            ),
        }
    }
}

impl From<compile::Rect<f64>> for Rect {
    fn from(value: compile::Rect<f64>) -> Self {
        Self {
            x0: value.x0 as f32,
            x1: value.x1 as f32,
            y0: value.y0 as f32,
            y1: value.y1 as f32,
            id: None,
            object_path: Vec::new(),
            border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
            border_styles: Edges::all(BorderStyle::Solid),
            cvars: None,
        }
    }
}

impl From<editor::Rect<(f64, Var)>> for Rect {
    fn from(value: editor::Rect<(f64, Var)>) -> Self {
        Self {
            x0: value.x0.0 as f32,
            x1: value.x1.0 as f32,
            y0: value.y0.0 as f32,
            y1: value.y1.0 as f32,
            id: None,
            object_path: Vec::new(),
            border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
            border_styles: Edges::all(BorderStyle::Solid),
            cvars: Some(Edges {
                top: LinearExpr::from(value.y1.1),
                right: LinearExpr::from(value.x1.1),
                bottom: LinearExpr::from(value.y0.1),
                left: LinearExpr::from(value.x0.1),
            }),
        }
    }
}

impl Rect {
    /// Returns the same visual rectangle with ordered coordinates. Edge-specific
    /// styling and constraint expressions move with their physical edge when a
    /// dragged edge crosses its opposite edge.
    fn normalized(mut self) -> Self {
        if self.x0 > self.x1 {
            std::mem::swap(&mut self.x0, &mut self.x1);
            std::mem::swap(&mut self.border_widths.left, &mut self.border_widths.right);
            std::mem::swap(&mut self.border_styles.left, &mut self.border_styles.right);
            if let Some(cvars) = &mut self.cvars {
                std::mem::swap(&mut cvars.left, &mut cvars.right);
            }
        }
        if self.y0 > self.y1 {
            std::mem::swap(&mut self.y0, &mut self.y1);
            std::mem::swap(&mut self.border_widths.top, &mut self.border_widths.bottom);
            std::mem::swap(&mut self.border_styles.top, &mut self.border_styles.bottom);
            if let Some(cvars) = &mut self.cvars {
                std::mem::swap(&mut cvars.top, &mut cvars.bottom);
            }
        }
        self
    }

    pub fn transform(&self, mat: TransformationMatrix, ofs: (f64, f64)) -> Self {
        let p0p = ifmatvec(mat, (self.x0 as f64, self.y0 as f64));
        let p1p = ifmatvec(mat, (self.x1 as f64, self.y1 as f64));
        Self {
            x0: (p0p.0.min(p1p.0) + ofs.0) as f32,
            y0: (p0p.1.min(p1p.1) + ofs.1) as f32,
            x1: (p0p.0.max(p1p.0) + ofs.0) as f32,
            y1: (p0p.1.max(p1p.1) + ofs.1) as f32,
            id: self.id.clone(),
            object_path: self.object_path.clone(),
            border_widths: self.border_widths,
            border_styles: self.border_styles,
            cvars: self.cvars.clone(),
        }
    }
}

pub fn intersect(a: &Bounds<Pixels>, b: &Bounds<Pixels>) -> Option<Bounds<Pixels>> {
    let origin = a.origin.max(&b.origin);
    let br = a.bottom_right().min(&b.bottom_right());
    if origin.x >= br.x || origin.y >= br.y {
        return None;
    }
    Some(Bounds::from_corners(origin, br))
}

pub struct CanvasElement {
    inner: Entity<LayoutCanvas>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct DrawRectToolState {
    p0: Option<Point<f32>>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct DrawPolygonToolState {
    points: Vec<Point<f32>>,
    constraints: Vec<DrawSegmentConstraint>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct DrawPathToolState {
    points: Vec<Point<f32>>,
    constraints: Vec<DrawSegmentConstraint>,
}

#[derive(Debug, Clone)]
pub(crate) enum DimEdge<T> {
    /// y-axis
    X0,
    /// x-axis
    Y0,
    /// edge of a rectangle
    Edge(T),
}

#[derive(Debug, Clone)]
pub(crate) struct DimensionEdge {
    path: String,
    name: String,
    /// Canvas geometry is intentionally `f32`; it is only used for painting
    /// and hit testing.
    display: Edge<f32>,
    /// Dimension defaults come from the compiler's grid-snapped `f64` output.
    exact: Edge<f64>,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct DrawDimToolState {
    pub(crate) edges: Vec<DimEdge<DimensionEdge>>,
}

#[derive(Debug, Clone)]
pub(crate) struct EditDimToolState {
    pub(crate) dim: Option<Span>,
    pub(crate) pending: Option<Box<PendingDimension>>,
    pub(crate) original_value: SharedString,
    /// `true` if entered from dimension tool
    pub(crate) dim_mode: bool,
}

impl EditDimToolState {
    fn label_position(&self, dim_hitboxes: &[DimensionHitbox]) -> Option<Point<f32>> {
        if let Some(pending) = &self.pending {
            return Some(pending.preview.label_position());
        }
        let dim = self.dim.as_ref()?;
        dim_hitboxes
            .iter()
            .find(|hitbox| &hitbox.span == dim)
            .map(|hitbox| hitbox.label_position)
    }
}

#[derive(Debug, Clone)]
pub(crate) struct PendingDimension {
    pub(crate) scope_span: Span,
    pub(crate) params: DimensionParams,
    preview: PendingDimensionPreview,
}

#[derive(Debug, Clone)]
struct PendingDimensionPreview {
    p: f32,
    n: f32,
    coord: f32,
    pstop: f32,
    nstop: f32,
    horiz: bool,
    value: String,
}

impl PendingDimensionPreview {
    fn label_position(&self) -> Point<f32> {
        if self.horiz {
            Point::new((self.p + self.n) / 2., self.coord)
        } else {
            Point::new(self.coord, (self.p + self.n) / 2.)
        }
    }
}

#[derive(Debug, Clone)]
struct DimensionHitbox {
    span: Span,
    bounds: Vec<Bounds<Pixels>>,
    value: SharedString,
    label_position: Point<f32>,
}

// TODO: potentially re-use compiler provided object IDs
#[derive(Copy, Clone, Hash, PartialEq, Eq, Debug)]
pub struct GlobalObjectId {
    scope: ScopeAddress,
    idx: usize,
}

#[derive(Debug, Default, Clone)]
pub(crate) struct SelectToolState {
    pub(crate) selected_obj: Option<Span>,
}

#[derive(Debug, Clone)]
pub(crate) struct PlaceInstanceToolState {
    invocation: String,
    scope_span: Span,
    rects: Vec<Rect>,
    polygons: Vec<Polygon>,
}

#[enumify]
#[derive(Debug, Clone)]
pub(crate) enum ToolState {
    DrawRect(DrawRectToolState),
    DrawPolygon(DrawPolygonToolState),
    DrawPath(DrawPathToolState),
    DrawDim(DrawDimToolState),
    EditDim(EditDimToolState),
    PlaceInstance(PlaceInstanceToolState),
    Select(SelectToolState),
}

impl Default for ToolState {
    fn default() -> Self {
        ToolState::Select(SelectToolState::default())
    }
}

pub struct LayoutCanvas {
    focus_handle: FocusHandle,
    text_input_focus_handle: FocusHandle,
    text_input: Entity<TextInput>,
    pub offset: Point<Pixels>,
    pub bg_style: Style,
    pub state: Entity<EditorState>,
    // SSE state
    is_sse_dragging: bool,
    // Keep displaying the final drag preview after mouse-up until the analyzer
    // sends back the result compiled from the rewritten initial conditions.
    is_sse_persisting: bool,
    pending_sse_values: Vec<PendingSseValue>,
    deferred_snapshot: Option<PreparedCompilationSnapshot>,
    sse_persist_after_revision: Option<u64>,
    sse_targets: Vec<SseDragTarget>,
    sse_delta: Point<Pixels>,
    // Drag handles and movable rectangle/instance bodies, recomputed each paint.
    sse_handles: Vec<SseHandle>,
    sse_bodies: Vec<SseBody>,
    // drag state
    is_dragging: bool,
    offset_start: Point<Pixels>,
    // shared between SSE and dragging
    drag_start: Point<Pixels>,
    mouse_position: Point<Pixels>,
    hover_hit: Option<SelectionHit>,
    shift_down: bool,
    // zoom state
    scale: f32,
    screen_bounds: Bounds<Pixels>,
    // Retained to keep the canvas's observations active.
    _subscriptions: Vec<Subscription>,
    rects: Vec<(Rect, LayerState)>,
    polygons: Vec<(Polygon, LayerState)>,
    scope_rects: Vec<LabeledBbox>,
    dim_hitboxes: Vec<DimensionHitbox>,
    raster_tiles: Option<LayoutRasterTileSet>,
    /// A different zoom level or disconnected pan is assembled here while
    /// the complete previous field remains visible. It is promoted atomically
    /// as soon as its rendered tiles cover the current viewport.
    raster_staging_tiles: Option<LayoutRasterTileSet>,
    /// Directly-created RenderImages bypass GPUI's managed image cache. Keep
    /// replaced images until the next paint can explicitly evict them from
    /// every sprite atlas, including the currently updating window.
    raster_images_to_drop: Vec<Arc<RenderImage>>,
    /// A viewport-sized image reprojected from retained semantic style planes.
    /// Unlike scaling the completed tile image, this reapplies stippling in
    /// destination pixels so its width and period remain stable during zoom.
    raster_reprojection: Option<LayoutRasterCache>,
    /// Low-resolution render of the complete displayed cell shown in the
    /// persistent hierarchy-sidebar overview pane.
    raster_overview: Option<LayoutRasterCache>,
    raster_overview_requested_revision: Option<u64>,
    raster_overview_refinement: Option<Task<()>>,
    raster_display: Option<RasterDisplayTransform>,
    /// Holds the retained raster at its last safe presentation while a zoom
    /// whose outlines are too expensive to reproject waits for an exact LOD.
    /// This must survive paint passes: recomputing `raster_display` there would
    /// silently scale the old bitmap, widening both outlines and stippling.
    raster_display_frozen: bool,
    raster_tile_target: Option<RasterTileTarget>,
    navigation_cache_active: bool,
    raster_refinement: Option<Task<()>>,
    /// Only one background raster worker runs at a time. Input events merely
    /// advance the requested generation; the worker coalesces them and always
    /// continues with the newest viewport after finishing its current image.
    raster_worker_active: bool,
    /// Monotonically identifies the newest requested raster. A slow render for
    /// an older viewport must never replace a newer, more appropriate LOD.
    raster_generation: u64,
    /// Background expanded rasters inspect this without returning to the UI
    /// thread, allowing new navigation to preempt obsolete retention work.
    raster_generation_signal: Arc<AtomicU64>,
    /// Unlike panning, a scale change makes every in-flight raster the wrong
    /// LOD. Workers check this independently so zoom immediately preempts old
    /// same-scale pan work without causing pan renders to cancel each other.
    raster_scale_signal: Arc<AtomicU64>,
    /// Changes only when layout content or its presentation changes, not for
    /// pan/zoom. It prevents a stale layer-visibility raster from entering the
    /// retained navigation atlas after a replacement has arrived.
    raster_content_revision: u64,
    raster_content_revision_signal: Arc<AtomicU64>,
    raster_output: Option<Arc<CompiledData>>,
    raster_scope_state: Option<Arc<IndexMap<editor::ScopePath, editor::ScopeState>>>,
    raster_selected_scope: Option<editor::ScopePath>,
    raster_layer_visibility: Vec<bool>,
    raster_hierarchy_depth: usize,
    raster_hide_external_geometry: bool,
    /// Conservative world-space bounds for everything the active presentation
    /// can draw. If retained tiles contain this box, zooming out may expose
    /// uncovered background but cannot expose missing geometry.
    raster_layout_bbox: Option<compile::Rect<f64>>,
    raster_dark_mode: bool,
    cell_raster_tiles: Arc<Mutex<CellRasterTileCache>>,
    raster_spatial_index: Arc<RasterSpatialIndex>,
    // True if waiting on render step to finish some initialization.
    //
    // Final bounds of layout canvas only determined in paint step.
    pending_init: bool,
}

#[derive(Clone)]
struct LayoutRasterCache {
    image: Arc<RenderImage>,
    style_planes: Option<Arc<RasterStylePlanes>>,
    texts: Arc<[TextLabel]>,
    scope_labels: Arc<[LabeledBbox]>,
    /// Logical extent covered by the raster image, including overscan.
    viewport: Size<Pixels>,
    /// Canvas size for which this cache was requested.
    screen_viewport: Size<Pixels>,
    scale: f32,
    offset: Point<Pixels>,
    content_revision: u64,
}

/// Fully composited colors for the two possible states of the shared slash
/// pattern at each retained raster pixel. Sampling these planes preserves all
/// layer ordering and outline replacement while allowing a destination raster
/// to choose a fresh, fixed-size stipple phase after a camera transform.
struct RasterStylePlanes {
    width: u32,
    height: u32,
    stipple_on: Arc<[u8]>,
    stipple_off: Arc<[u8]>,
    outline_correction: Option<Arc<RasterOutlineCorrection>>,
}

/// Exact source-raster geometry plus the fill that was underneath each visible
/// outline sample. Reprojection first restores those sparse underlay samples,
/// then strokes the exact geometry at destination resolution. Keeping source
/// coordinates as floats avoids the LOD handoff jump caused by magnifying a
/// quantized one-pixel outline mask.
struct RasterOutlineCorrection {
    geometry: RasterOutlineGeometry,
    sample_mask: Arc<[u64]>,
    samples: Arc<[RasterOutlineSample]>,
}

struct RasterOutlineGeometry {
    primitives: Arc<[RasterOutlinePrimitive]>,
    source_work: usize,
}

#[derive(Clone, Copy)]
enum RasterOutlinePrimitive {
    Rect(Bounds<Pixels>),
    Line { start: Point<f32>, stop: Point<f32> },
}

#[derive(Clone, Copy)]
struct RasterOutlineSample {
    pixel: u32,
    underlay_on: [u8; 4],
    underlay_off: [u8; 4],
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct RasterTileIndex {
    x: i32,
    y: i32,
}

#[derive(Clone)]
struct LayoutRasterTileSet {
    tiles: HashMap<RasterTileIndex, LayoutRasterCache>,
    /// Exact UI-thread rasters seed the center for first paint but must still
    /// be replaced by the fully flattened navigation traversal.
    navigation: bool,
    /// Offset used by tile (0, 0). Other tiles are translated by an integral
    /// tile stride, which keeps their raster phases mutually congruent.
    anchor_offset: Point<Pixels>,
    tile_size: Size<Pixels>,
    screen_viewport: Size<Pixels>,
    scale: f32,
    content_revision: u64,
    center: RasterTileIndex,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RasterTileTarget {
    anchor_offset: Point<Pixels>,
    tile_size: Size<Pixels>,
    screen_viewport: Size<Pixels>,
    scale: f32,
    content_revision: u64,
    center: RasterTileIndex,
    generation: u64,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RasterDisplayTransform {
    scale: f32,
    offset: Point<Pixels>,
}

#[derive(Clone, Copy)]
struct ViewportTransform {
    size: Size<Pixels>,
    screen_size: Size<Pixels>,
    scale: f32,
    offset: Point<Pixels>,
}

#[derive(Clone)]
pub(crate) struct NavigationOverviewSnapshot {
    pub image: Arc<RenderImage>,
    pub image_bounds: Bounds<Pixels>,
    pub viewport_bounds: Bounds<Pixels>,
    bounds: Bounds<Pixels>,
    display: ViewportTransform,
}

impl NavigationOverviewSnapshot {
    pub(crate) fn world_at(&self, position: Point<Pixels>) -> Point<f32> {
        navigation_overview_world_point(self.display, position, self.bounds)
    }
}

#[derive(Clone)]
struct NavigationRasterInput {
    solved_cell: CompileOutputState,
    layers: Arc<IndexMap<SharedString, LayerState>>,
    hierarchy_depth: usize,
    hide_external_geometry: bool,
    viewport: ViewportTransform,
    text_color: Rgba,
    include_text: bool,
    content_revision: u64,
    content_revision_signal: Arc<AtomicU64>,
    scale_signal: Arc<AtomicU64>,
    cell_raster_tiles: Arc<Mutex<CellRasterTileCache>>,
    spatial_index: Arc<RasterSpatialIndex>,
    use_spatial_index: bool,
    cancel_if_generation_changes: Option<(Arc<AtomicU64>, u64)>,
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct RasterBvhBounds {
    min_x: f64,
    min_y: f64,
    max_x: f64,
    max_y: f64,
}

impl RasterBvhBounds {
    fn from_points(points: impl IntoIterator<Item = (f64, f64)>) -> Option<Self> {
        let mut points = points.into_iter();
        let (x, y) = points.next()?;
        let mut bounds = Self {
            min_x: x,
            min_y: y,
            max_x: x,
            max_y: y,
        };
        for (x, y) in points {
            bounds.min_x = bounds.min_x.min(x);
            bounds.min_y = bounds.min_y.min(y);
            bounds.max_x = bounds.max_x.max(x);
            bounds.max_y = bounds.max_y.max(y);
        }
        Some(bounds)
    }

    fn union(self, other: Self) -> Self {
        Self {
            min_x: self.min_x.min(other.min_x),
            min_y: self.min_y.min(other.min_y),
            max_x: self.max_x.max(other.max_x),
            max_y: self.max_y.max(other.max_y),
        }
    }

    fn intersects(self, other: Self) -> bool {
        self.min_x <= other.max_x
            && self.max_x >= other.min_x
            && self.min_y <= other.max_y
            && self.max_y >= other.min_y
    }

    fn center(self, x_axis: bool) -> f64 {
        if x_axis {
            (self.min_x + self.max_x) / 2.
        } else {
            (self.min_y + self.max_y) / 2.
        }
    }

    fn expanded(self, amount: f64) -> Self {
        Self {
            min_x: self.min_x - amount,
            min_y: self.min_y - amount,
            max_x: self.max_x + amount,
            max_y: self.max_y + amount,
        }
    }

    fn transformed(self, mat: TransformationMatrix, offset: (f64, f64)) -> Self {
        Self::from_points(
            [
                (self.min_x, self.min_y),
                (self.min_x, self.max_y),
                (self.max_x, self.min_y),
                (self.max_x, self.max_y),
            ]
            .map(|point| {
                let point = ifmatvec(mat, point);
                (point.0 + offset.0, point.1 + offset.1)
            }),
        )
        .expect("four transformed bounds corners")
    }

    fn transformed_by_inverse_unitary(self, mat: TransformationMatrix) -> Self {
        // Rotation/reflection matrices are unitary, so their inverse is their
        // transpose. Do this explicitly: geometry's matrix inverse currently
        // omits the determinant sign and is incorrect for reflections.
        Self::from_points(
            [
                (self.min_x, self.min_y),
                (self.min_x, self.max_y),
                (self.max_x, self.min_y),
                (self.max_x, self.max_y),
            ]
            .map(|(x, y)| {
                (
                    mat[0][0] as f64 * x + mat[1][0] as f64 * y,
                    mat[0][1] as f64 * x + mat[1][1] as f64 * y,
                )
            }),
        )
        .expect("four inverse-transformed bounds corners")
    }
}

struct RasterBvhItem {
    emit_index: usize,
    bounds: RasterBvhBounds,
}

enum RasterBvhNode {
    Leaf {
        bounds: RasterBvhBounds,
        items: Box<[RasterBvhItem]>,
    },
    Branch {
        bounds: RasterBvhBounds,
        left: Box<RasterBvhNode>,
        right: Box<RasterBvhNode>,
    },
}

impl RasterBvhNode {
    const LEAF_SIZE: usize = 8;

    fn build(mut items: Vec<RasterBvhItem>) -> Option<Self> {
        let bounds = items
            .iter()
            .map(|item| item.bounds)
            .reduce(RasterBvhBounds::union)?;
        if items.len() <= Self::LEAF_SIZE {
            return Some(Self::Leaf {
                bounds,
                items: items.into_boxed_slice(),
            });
        }
        let x_axis = bounds.max_x - bounds.min_x >= bounds.max_y - bounds.min_y;
        items.sort_unstable_by(|a, b| a.bounds.center(x_axis).total_cmp(&b.bounds.center(x_axis)));
        let right = items.split_off(items.len() / 2);
        Some(Self::Branch {
            bounds,
            left: Box::new(Self::build(items).expect("non-empty BVH half")),
            right: Box::new(Self::build(right).expect("non-empty BVH half")),
        })
    }

    fn query(&self, query: RasterBvhBounds, output: &mut Vec<usize>) {
        let bounds = match self {
            Self::Leaf { bounds, .. } | Self::Branch { bounds, .. } => *bounds,
        };
        if !bounds.intersects(query) {
            return;
        }
        match self {
            Self::Leaf { items, .. } => output.extend(
                items
                    .iter()
                    .filter(|item| item.bounds.intersects(query))
                    .map(|item| item.emit_index),
            ),
            Self::Branch { left, right, .. } => {
                left.query(query, output);
                right.query(query, output);
            }
        }
    }
}

struct RasterScopeBvh {
    root: Option<RasterBvhNode>,
    unbounded: Box<[usize]>,
}

struct RasterCellSpatialIndex {
    scopes: HashMap<ScopeAddress, RasterScopeBvh>,
}

impl RasterCellSpatialIndex {
    fn build(solved: &CompileOutputState, cell: CellId) -> Self {
        let mut scopes = HashMap::new();
        let cell_info = &solved.output.cells[&cell];
        for (&scope, scope_info) in &cell_info.scopes {
            let mut items = Vec::new();
            let mut unbounded = Vec::new();
            for (emit_index, (object, _)) in scope_info.emit.iter().enumerate() {
                let bounds = match &cell_info.objects[object] {
                    SolvedValue::Rect(rect) if !rect.construction => Some(RasterBvhBounds {
                        min_x: rect.x0.0.min(rect.x1.0),
                        min_y: rect.y0.0.min(rect.y1.0),
                        max_x: rect.x0.0.max(rect.x1.0),
                        max_y: rect.y0.0.max(rect.y1.0),
                    }),
                    SolvedValue::Polygon(polygon) => {
                        RasterBvhBounds::from_points(polygon.points.iter().map(|(x, y)| (x.0, y.0)))
                    }
                    SolvedValue::Path(path) => path.bbox().map(|bbox| RasterBvhBounds {
                        min_x: bbox.x0.min(bbox.x1),
                        min_y: bbox.y0.min(bbox.y1),
                        max_x: bbox.x0.max(bbox.x1),
                        max_y: bbox.y0.max(bbox.y1),
                    }),
                    SolvedValue::Instance(instance) if !instance.construction => {
                        let child_scope = ScopeAddress {
                            cell: instance.cell,
                            scope: solved.output.cells[&instance.cell].root,
                        };
                        solved
                            .scope_paths
                            .get(&child_scope)
                            .and_then(|path| solved.state.get(path))
                            .and_then(|scope| scope.bbox.as_ref())
                            .map(|bbox| {
                                let mut instance_mat = TransformationMatrix::identity();
                                if instance.reflect {
                                    instance_mat = instance_mat.reflect_vert();
                                }
                                instance_mat = instance_mat.rotate(instance.angle);
                                RasterBvhBounds {
                                    min_x: bbox.x0.min(bbox.x1),
                                    min_y: bbox.y0.min(bbox.y1),
                                    max_x: bbox.x0.max(bbox.x1),
                                    max_y: bbox.y0.max(bbox.y1),
                                }
                                .transformed(instance_mat, (instance.x, instance.y))
                            })
                    }
                    SolvedValue::Text(text) => Some(RasterBvhBounds {
                        min_x: text.x,
                        min_y: text.y,
                        max_x: text.x,
                        max_y: text.y,
                    }),
                    _ => continue,
                };
                if let Some(bounds) = bounds {
                    items.push(RasterBvhItem { emit_index, bounds });
                } else {
                    // An instance without a solved child bbox must remain
                    // queryable; its descendants are clipped normally.
                    unbounded.push(emit_index);
                }
            }
            scopes.insert(
                ScopeAddress { cell, scope },
                RasterScopeBvh {
                    root: RasterBvhNode::build(items),
                    unbounded: unbounded.into_boxed_slice(),
                },
            );
        }
        Self { scopes }
    }
}

#[derive(Default)]
struct RasterSpatialIndex {
    cells: Mutex<HashMap<CellId, Arc<OnceLock<RasterCellSpatialIndex>>>>,
}

impl RasterSpatialIndex {
    fn query(
        &self,
        solved: &CompileOutputState,
        address: ScopeAddress,
        bounds: RasterBvhBounds,
        emit_len: usize,
    ) -> Vec<usize> {
        let cell_index = {
            let mut cells = self.cells.lock().expect("raster spatial index poisoned");
            cells
                .entry(address.cell)
                .or_insert_with(|| Arc::new(OnceLock::new()))
                .clone()
        };
        let cell_index =
            cell_index.get_or_init(|| RasterCellSpatialIndex::build(solved, address.cell));
        let Some(scope) = cell_index.scopes.get(&address) else {
            return (0..emit_len).collect();
        };
        let mut output = Vec::new();
        if let Some(root) = &scope.root {
            root.query(bounds, &mut output);
        }
        output.extend_from_slice(&scope.unbounded);
        // Raster compositing is layer based, but retaining source order also
        // keeps text and future order-dependent primitives deterministic.
        output.sort_unstable();
        output
    }
}

fn raster_viewport_world_bounds(viewport: ViewportTransform) -> RasterBvhBounds {
    let scale = viewport.scale as f64;
    let offset_x = f64::from(viewport.offset.x);
    let offset_y = f64::from(viewport.offset.y);
    let width = f64::from(viewport.size.width);
    let height = f64::from(viewport.size.height);
    RasterBvhBounds {
        min_x: -offset_x / scale,
        min_y: (offset_y - height) / scale,
        max_x: (width - offset_x) / scale,
        max_y: offset_y / scale,
    }
}

fn raster_scope_query_bounds(
    world: RasterBvhBounds,
    mat: TransformationMatrix,
    offset: (f64, f64),
    margin: f64,
) -> RasterBvhBounds {
    let translated = RasterBvhBounds {
        min_x: world.min_x - offset.0,
        min_y: world.min_y - offset.1,
        max_x: world.max_x - offset.0,
        max_y: world.max_y - offset.1,
    };
    translated
        .transformed_by_inverse_unitary(mat)
        .expanded(margin)
}

struct RasterRectPrimitive {
    bounds: Bounds<Pixels>,
    fill: ShapeFill,
    color: Rgba,
    border_color: Rgba,
}

struct RasterPolygonPrimitive {
    points: Vec<Point<f32>>,
    fill: ShapeFill,
    color: Rgba,
    border_color: Rgba,
}

struct RasterOutlineGeometryBuilder {
    primitives: Vec<RasterOutlinePrimitive>,
    source_work: usize,
    work_limit: usize,
    disabled: bool,
}

impl RasterOutlineGeometryBuilder {
    fn new(pixel_count: usize) -> Self {
        Self {
            primitives: Vec::new(),
            source_work: 0,
            work_limit: pixel_count / OUTLINE_REPROJECTION_WORK_DIVISOR,
            disabled: false,
        }
    }

    fn add_work(&mut self, work: usize) -> bool {
        if self.disabled {
            return false;
        }
        self.source_work = self.source_work.saturating_add(work);
        if self.source_work > self.work_limit
            || self.primitives.len() >= OUTLINE_REPROJECTION_PRIMITIVE_LIMIT
        {
            self.primitives.clear();
            self.disabled = true;
            return false;
        }
        true
    }

    fn rect(&mut self, bounds: Bounds<Pixels>, width: u32, height: u32) {
        let clipped_width = f32::from(bounds.size.width).abs().ceil().min(width as f32) as usize;
        let clipped_height = f32::from(bounds.size.height)
            .abs()
            .ceil()
            .min(height as f32) as usize;
        if self.add_work(2usize.saturating_mul(clipped_width.saturating_add(clipped_height))) {
            self.primitives.push(RasterOutlinePrimitive::Rect(bounds));
        }
    }

    fn line(&mut self, start: Point<f32>, stop: Point<f32>) {
        let work = (stop.x - start.x)
            .abs()
            .max((stop.y - start.y).abs())
            .ceil() as usize
            + 1;
        if self.add_work(work) {
            self.primitives
                .push(RasterOutlinePrimitive::Line { start, stop });
        }
    }

    fn finish(self) -> Option<RasterOutlineGeometry> {
        (!self.disabled).then(|| RasterOutlineGeometry {
            primitives: self.primitives.into(),
            source_work: self.source_work,
        })
    }
}

#[derive(Clone, Copy, Eq, Hash, PartialEq)]
struct CellRasterTileKey {
    address: ScopeAddress,
    orientation: TransformationMatrix,
    width: u16,
    height: u16,
    content_revision: u64,
}

struct CellRasterTile {
    width: u16,
    height: u16,
    layers: Vec<Option<Arc<[u8]>>>,
}

#[derive(Default)]
struct CellRasterTileCache {
    entries: HashMap<CellRasterTileKey, Arc<CellRasterTile>>,
    insertion_order: VecDeque<CellRasterTileKey>,
}

impl CellRasterTileCache {
    fn get(&self, key: &CellRasterTileKey) -> Option<Arc<CellRasterTile>> {
        self.entries.get(key).cloned()
    }

    fn insert(&mut self, key: CellRasterTileKey, tile: Arc<CellRasterTile>) {
        if self.entries.insert(key, tile).is_none() {
            self.insertion_order.push_back(key);
        }
        while self.entries.len() > CELL_RASTER_TILE_CACHE_LIMIT {
            if let Some(oldest) = self.insertion_order.pop_front() {
                self.entries.remove(&oldest);
            }
        }
    }

    fn clear(&mut self) {
        self.entries.clear();
        self.insertion_order.clear();
    }
}

struct RasterTilePrimitive {
    coverage: Arc<[u8]>,
    tile_width: u16,
    tile_height: u16,
    bounds: Bounds<Pixels>,
}

impl IntoElement for CanvasElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

fn get_paint_path(bounds: Bounds<Pixels>, color: Rgba, thickness: Pixels) -> PaintQuad {
    let bounds = Bounds::new(
        Point::new(
            bounds.origin.x - thickness / 2.,
            bounds.origin.y - thickness / 2.,
        ),
        Size::new(
            bounds.size.width + thickness,
            bounds.size.height + thickness,
        ),
    );
    PaintQuad {
        bounds,
        corner_radii: Corners::all(px(0.)),
        background: solid_background(color),
        border_widths: Edges::all(px(0.)),
        border_color: Rgba { a: 0., ..color }.into(),
        border_styles: Edges::all(BorderStyle::Solid),
    }
}

fn get_rect_bounds(
    r: &Rect,
    bounds: Bounds<Pixels>,
    scale: f32,
    offset: Point<Pixels>,
) -> Bounds<Pixels> {
    let x0 = r.x0.min(r.x1);
    let x1 = r.x0.max(r.x1);
    let y0 = r.y0.min(r.y1);
    let y1 = r.y0.max(r.y1);
    Bounds::new(
        Point::new(scale * px(x0), scale * px(-y1)) + offset + bounds.origin,
        Size::new(scale * px(x1 - x0), scale * px(y1 - y0)),
    )
}

fn raster_channel(value: f32) -> f32 {
    value.clamp(0., 1.)
}

#[derive(Clone, Copy)]
enum RasterComposite {
    /// Idempotent union of shapes sharing one layer.
    Union,
    /// Outlines replace the fill of their own layer.
    Replace,
}

fn layout_text_metrics(scale: f32, layout_size: f32) -> Option<(Pixels, Pixels)> {
    let size = (scale.abs() * layout_size).min(MAX_TEXT_PX);
    (size >= MIN_READABLE_TEXT_PX).then(|| (px(size), px(size * 1.125)))
}

/// Blend a GPUI color into the BGRA image format expected by `RenderImage`.
fn blend_raster_pixel(buffer: &mut [u8], pixel: usize, color: Rgba) {
    let source_alpha = raster_channel(color.a);
    if source_alpha <= 0. {
        return;
    }
    let index = pixel * 4;
    let destination_alpha = buffer[index + 3] as f32 / 255.;
    let output_alpha = source_alpha + destination_alpha * (1. - source_alpha);
    let source = [color.b, color.g, color.r];
    if output_alpha > 0. {
        for channel in 0..3 {
            let destination = buffer[index + channel] as f32 / 255.;
            let output = (raster_channel(source[channel]) * source_alpha
                + destination * destination_alpha * (1. - source_alpha))
                / output_alpha;
            buffer[index + channel] = (output * 255.).round() as u8;
        }
    }
    buffer[index + 3] = (output_alpha * 255.).round() as u8;
}

fn composite_raster_pixel(
    buffer: &mut [u8],
    pixel: usize,
    color: Rgba,
    composite: RasterComposite,
    touched: Option<&mut Vec<usize>>,
) {
    let index = pixel * 4;
    let was_transparent = buffer[index + 3] == 0;
    match composite {
        RasterComposite::Union => {
            let alpha = (raster_channel(color.a) * 255.).round() as u8;
            if alpha > buffer[index + 3] {
                buffer[index] = (raster_channel(color.b) * 255.).round() as u8;
                buffer[index + 1] = (raster_channel(color.g) * 255.).round() as u8;
                buffer[index + 2] = (raster_channel(color.r) * 255.).round() as u8;
                buffer[index + 3] = alpha;
            }
        }
        RasterComposite::Replace => {
            buffer[index] = (raster_channel(color.b) * 255.).round() as u8;
            buffer[index + 1] = (raster_channel(color.g) * 255.).round() as u8;
            buffer[index + 2] = (raster_channel(color.r) * 255.).round() as u8;
            buffer[index + 3] = (raster_channel(color.a) * 255.).round() as u8;
        }
    }
    if was_transparent
        && buffer[index + 3] != 0
        && let Some(touched) = touched
    {
        touched.push(pixel);
    }
}

struct RasterPaintTarget<'a> {
    buffer: &'a mut [u8],
    composite: RasterComposite,
    touched: Option<&'a mut Vec<usize>>,
}

impl RasterPaintTarget<'_> {
    fn pixel(&mut self, pixel: usize, color: Rgba) {
        composite_raster_pixel(
            self.buffer,
            pixel,
            color,
            self.composite,
            self.touched.as_deref_mut(),
        );
    }
}

fn blend_raster_layer(buffer: &mut [u8], layer: &mut [u8], touched: &mut Vec<usize>) {
    for pixel in touched.drain(..) {
        let source = &mut layer[pixel * 4..pixel * 4 + 4];
        if source[3] == 0 {
            continue;
        }
        if source[3] == u8::MAX {
            buffer[pixel * 4..pixel * 4 + 4].copy_from_slice(source);
        } else {
            blend_raster_pixel(
                buffer,
                pixel,
                Rgba {
                    b: source[0] as f32 / 255.,
                    g: source[1] as f32 / 255.,
                    r: source[2] as f32 / 255.,
                    a: source[3] as f32 / 255.,
                },
            );
        }
        source.fill(0);
    }
}

/// Composite a fill layer into the outline-free underlay without consuming the
/// layer. The same layer buffer is subsequently stroked and consumed by
/// `blend_raster_layer` to produce the normal completed style plane.
fn blend_raster_layer_preserving(buffer: &mut [u8], layer: &[u8], pixels: &[usize]) {
    for &pixel in pixels {
        let source = &layer[pixel * 4..pixel * 4 + 4];
        if source[3] == 0 {
            continue;
        }
        if source[3] == u8::MAX {
            buffer[pixel * 4..pixel * 4 + 4].copy_from_slice(source);
        } else {
            blend_raster_pixel(
                buffer,
                pixel,
                Rgba {
                    b: source[0] as f32 / 255.,
                    g: source[1] as f32 / 255.,
                    r: source[2] as f32 / 255.,
                    a: source[3] as f32 / 255.,
                },
            );
        }
    }
}

fn build_raster_outline_correction(
    geometry: Option<RasterOutlineGeometry>,
    outline_pixels: Option<&[usize]>,
    stipple_on: &[u8],
    stipple_off: &[u8],
    underlay_on: Option<&[u8]>,
    underlay_off: Option<&[u8]>,
) -> Option<Arc<RasterOutlineCorrection>> {
    let geometry = geometry?;
    let outline_pixels = outline_pixels?;
    let pixel_count = stipple_on.len() / 4;
    if outline_pixels.is_empty() {
        return Some(Arc::new(RasterOutlineCorrection {
            geometry,
            sample_mask: Arc::from([]),
            samples: Arc::from([]),
        }));
    }
    let underlay_on = underlay_on?;
    let underlay_off = underlay_off?;
    let mut sample_mask = vec![0_u64; pixel_count.div_ceil(64)];
    let mut samples = Vec::new();
    for &pixel in outline_pixels {
        let range = pixel * 4..pixel * 4 + 4;
        if stipple_on[range.clone()] == underlay_on[range.clone()]
            && stipple_off[range.clone()] == underlay_off[range.clone()]
        {
            continue;
        }
        sample_mask[pixel / 64] |= 1_u64 << (pixel % 64);
        samples.push(RasterOutlineSample {
            pixel: pixel as u32,
            underlay_on: underlay_on[range.clone()]
                .try_into()
                .expect("four BGRA channels"),
            underlay_off: underlay_off[range].try_into().expect("four BGRA channels"),
        });
    }
    Some(Arc::new(RasterOutlineCorrection {
        geometry,
        sample_mask: sample_mask.into(),
        samples: samples.into(),
    }))
}

fn raster_outline_sample(
    correction: &RasterOutlineCorrection,
    pixel: usize,
) -> Option<&RasterOutlineSample> {
    let word = *correction.sample_mask.get(pixel / 64)?;
    if word & (1_u64 << (pixel % 64)) == 0 {
        return None;
    }
    correction
        .samples
        .binary_search_by_key(&(pixel as u32), |sample| sample.pixel)
        .ok()
        .map(|index| &correction.samples[index])
}

fn raster_pixel_range(start: f32, stop: f32, limit: u32) -> Option<(u32, u32)> {
    let lower = start.min(stop).floor().clamp(0., limit as f32) as u32;
    let upper = start.max(stop).ceil().clamp(0., limit as f32) as u32;
    (lower < upper).then_some((lower, upper))
}

fn raster_unclipped_pixel_range(start: f32, stop: f32) -> Option<(i32, i32)> {
    let lower = start.min(stop).floor() as i32;
    let upper = start.max(stop).ceil() as i32;
    (lower < upper).then_some((lower, upper))
}

fn raster_bounds_have_resolvable_outline_gap(bounds: Bounds<Pixels>) -> bool {
    let width = f32::from(bounds.size.width).abs();
    let height = f32::from(bounds.size.height).abs();
    let resolvable_span = 2. + MIN_RESOLVABLE_OUTLINE_GAP_RASTER_PIXELS;
    width >= resolvable_span && height >= resolvable_span
}

fn raster_logical_size(width: u32, height: u32) -> Size<Pixels> {
    Size::new(
        px(width as f32 / RASTER_CACHE_RESOLUTION),
        px(height as f32 / RASTER_CACHE_RESOLUTION),
    )
}

fn expand_raster_for_display(image: image::RgbaImage) -> image::RgbaImage {
    let logical_size = raster_logical_size(image.width(), image.height());
    let width = f32::from(logical_size.width).round().max(1.) as u32;
    let height = f32::from(logical_size.height).round().max(1.) as u32;
    image::imageops::resize(&image, width, height, image::imageops::FilterType::Nearest)
}

fn raster_stipple_is_on(x: u32, y: u32, stipple_phase: i64) -> bool {
    let period = (10. * RASTER_CACHE_RESOLUTION).round().max(1.) as i64;
    (x as i64 - y as i64 - stipple_phase).rem_euclid(period) == 0
}

fn raster_from_style_planes(
    width: u32,
    height: u32,
    stipple_on: &[u8],
    stipple_off: &[u8],
    stipple_phase: i64,
) -> Option<image::RgbaImage> {
    let mut buffer = vec![0; width as usize * height as usize * 4];
    for y in 0..height {
        for x in 0..width {
            let pixel = (y * width + x) as usize;
            let source = if raster_stipple_is_on(x, y, stipple_phase) {
                stipple_on
            } else {
                stipple_off
            };
            buffer[pixel * 4..pixel * 4 + 4].copy_from_slice(&source[pixel * 4..pixel * 4 + 4]);
        }
    }
    image::RgbaImage::from_raw(width, height, buffer)
}

fn raster_stipple_phase(offset: Point<Pixels>) -> i64 {
    (f32::from(offset.x) - f32::from(offset.y)).round() as i64
}

fn mark_raster_occupancy(
    occupancy: &mut Option<Vec<u64>>,
    width: u32,
    height: u32,
    bounds: Bounds<Pixels>,
) {
    let Some((x0, x1)) = raster_pixel_range(
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.x + bounds.size.width),
        width,
    ) else {
        return;
    };
    let Some((y0, y1)) = raster_pixel_range(
        f32::from(bounds.origin.y),
        f32::from(bounds.origin.y + bounds.size.height),
        height,
    ) else {
        return;
    };
    let pixel_count = width as usize * height as usize;
    let occupancy = occupancy.get_or_insert_with(|| vec![0; pixel_count.div_ceil(64)]);
    for y in y0..y1 {
        for x in x0..x1 {
            let pixel = (y * width + x) as usize;
            occupancy[pixel / 64] |= 1_u64 << (pixel % 64);
        }
    }
}

fn paint_raster_occupancy_color(
    target: &mut RasterPaintTarget<'_>,
    occupancy: &[u64],
    pixel_count: usize,
    color: Rgba,
) {
    for (word_index, word) in occupancy.iter().enumerate() {
        let mut remaining = *word;
        while remaining != 0 {
            let bit = remaining.trailing_zeros() as usize;
            let pixel = word_index * 64 + bit;
            if pixel < pixel_count {
                target.pixel(pixel, color);
            }
            remaining &= remaining - 1;
        }
    }
}

fn mark_tile_rect(coverage: &mut [u8], width: u32, height: u32, bounds: Bounds<Pixels>) {
    let Some((x0, x1)) = raster_pixel_range(
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.x + bounds.size.width),
        width,
    ) else {
        return;
    };
    let Some((y0, y1)) = raster_pixel_range(
        f32::from(bounds.origin.y),
        f32::from(bounds.origin.y + bounds.size.height),
        height,
    ) else {
        return;
    };
    for y in y0..y1 {
        coverage[(y * width + x0) as usize..(y * width + x1) as usize].fill(u8::MAX);
    }
}

fn mark_tile_polygon(coverage: &mut [u8], width: u32, height: u32, points: &[Point<f32>]) {
    if points.len() < 3 {
        return;
    }
    let mut painted = false;
    for y in 0..height {
        let scan_y = y as f32 + 0.5;
        let mut intersections = Vec::with_capacity(points.len());
        for (start, stop) in points
            .iter()
            .zip(points.iter().cycle().skip(1))
            .take(points.len())
        {
            if (start.y <= scan_y && stop.y > scan_y) || (stop.y <= scan_y && start.y > scan_y) {
                let fraction = (scan_y - start.y) / (stop.y - start.y);
                intersections.push(start.x + fraction * (stop.x - start.x));
            }
        }
        intersections.sort_by(f32::total_cmp);
        for pair in intersections.as_chunks::<2>().0 {
            let Some((x0, x1)) = raster_pixel_range(pair[0], pair[1], width) else {
                continue;
            };
            coverage[(y * width + x0) as usize..(y * width + x1) as usize].fill(u8::MAX);
            painted = true;
        }
    }
    if !painted {
        let (min_x, max_x, min_y, max_y) = points.iter().fold(
            (
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
            ),
            |(min_x, max_x, min_y, max_y), point| {
                (
                    min_x.min(point.x),
                    max_x.max(point.x),
                    min_y.min(point.y),
                    max_y.max(point.y),
                )
            },
        );
        mark_tile_rect(
            coverage,
            width,
            height,
            Bounds::from_corners(
                Point::new(px(min_x), px(min_y)),
                Point::new(px(max_x), px(max_y)),
            ),
        );
    }
}

fn build_cell_raster_tile(
    input: &NavigationRasterInput,
    address: ScopeAddress,
    orientation: TransformationMatrix,
    width: u16,
    height: u16,
) -> Option<CellRasterTile> {
    let scope_state = &input.solved_cell.state[&input.solved_cell.scope_paths[&address]];
    let bbox = scope_state.bbox.as_ref()?;
    let p0 = ifmatvec(orientation, (bbox.x0, bbox.y0));
    let p1 = ifmatvec(orientation, (bbox.x1, bbox.y1));
    let x0 = p0.0.min(p1.0);
    let x1 = p0.0.max(p1.0);
    let y0 = p0.1.min(p1.1);
    let y1 = p0.1.max(p1.1);
    if x0 >= x1 || y0 >= y1 {
        return None;
    }
    let layer_count = input
        .layers
        .values()
        .map(|layer| layer.z)
        .max()
        .map_or(0, |z| z + 1);
    let mut coverage = (0..layer_count)
        .map(|_| vec![0; width as usize * height as usize])
        .collect::<Vec<_>>();
    let mut queue = VecDeque::from_iter([(address, orientation, (0., 0.))]);

    while let Some((address @ ScopeAddress { cell, scope }, mat, ofs)) = queue.pop_front() {
        let scope_state = &input.solved_cell.state[&input.solved_cell.scope_paths[&address]];
        if !scope_state.visible {
            continue;
        }
        let cell_info = &input.solved_cell.output.cells[&cell];
        let scope_info = &cell_info.scopes[&scope];
        for (object, _) in &scope_info.emit {
            match &cell_info.objects[object] {
                SolvedValue::Rect(rect) if !rect.construction => {
                    let Some(layer) = rect
                        .layer
                        .as_ref()
                        .and_then(|name| input.layers.get(name.as_str()))
                        .filter(|layer| layer.visible)
                    else {
                        continue;
                    };
                    let rp0 = ifmatvec(mat, (rect.x0.0, rect.y0.0));
                    let rp1 = ifmatvec(mat, (rect.x1.0, rect.y1.0));
                    let tp0 = Point::new(
                        width as f32 * ((rp0.0 + ofs.0 - x0) / (x1 - x0)) as f32,
                        height as f32 * ((y1 - rp0.1 - ofs.1) / (y1 - y0)) as f32,
                    );
                    let tp1 = Point::new(
                        width as f32 * ((rp1.0 + ofs.0 - x0) / (x1 - x0)) as f32,
                        height as f32 * ((y1 - rp1.1 - ofs.1) / (y1 - y0)) as f32,
                    );
                    mark_tile_rect(
                        &mut coverage[layer.z],
                        width as u32,
                        height as u32,
                        Bounds::from_corners(tp0.min(&tp1).map(px), tp0.max(&tp1).map(px)),
                    );
                }
                SolvedValue::Polygon(polygon) => {
                    let Some(layer) = input
                        .layers
                        .get(polygon.layer.as_str())
                        .filter(|layer| layer.visible)
                    else {
                        continue;
                    };
                    let points = polygon
                        .points
                        .iter()
                        .map(|(x, y)| {
                            let point = ifmatvec(mat, (x.0, y.0));
                            Point::new(
                                width as f32 * ((point.0 + ofs.0 - x0) / (x1 - x0)) as f32,
                                height as f32 * ((y1 - point.1 - ofs.1) / (y1 - y0)) as f32,
                            )
                        })
                        .collect::<Vec<_>>();
                    mark_tile_polygon(&mut coverage[layer.z], width as u32, height as u32, &points);
                }
                SolvedValue::Instance(instance) if !instance.construction => {
                    let mut instance_mat = TransformationMatrix::identity();
                    if instance.reflect {
                        instance_mat = instance_mat.reflect_vert();
                    }
                    instance_mat = instance_mat.rotate(instance.angle);
                    let instance_ofs = ifmatvec(mat, (instance.x, instance.y));
                    let child = instance.cell;
                    queue.push_back((
                        ScopeAddress {
                            cell: child,
                            scope: input.solved_cell.output.cells[&child].root,
                        },
                        mat * instance_mat,
                        (instance_ofs.0 + ofs.0, instance_ofs.1 + ofs.1),
                    ));
                }
                _ => {}
            }
        }
        for child in &scope_info.children {
            queue.push_back((
                ScopeAddress {
                    cell,
                    scope: *child,
                },
                mat,
                ofs,
            ));
        }
    }

    let layers = coverage
        .into_iter()
        .map(|coverage| {
            coverage
                .iter()
                .any(|value| *value != 0)
                .then(|| Arc::<[u8]>::from(coverage))
        })
        .collect::<Vec<_>>();
    layers
        .iter()
        .any(Option::is_some)
        .then_some(CellRasterTile {
            width,
            height,
            layers,
        })
}

fn cached_cell_raster_tile(
    input: &NavigationRasterInput,
    address: ScopeAddress,
    orientation: TransformationMatrix,
    width: u16,
    height: u16,
) -> Option<Arc<CellRasterTile>> {
    let key = CellRasterTileKey {
        address,
        orientation,
        width,
        height,
        content_revision: input.content_revision,
    };
    if let Some(tile) = input
        .cell_raster_tiles
        .lock()
        .expect("cell raster tile cache poisoned")
        .get(&key)
    {
        return Some(tile);
    }
    let tile = Arc::new(build_cell_raster_tile(
        input,
        address,
        orientation,
        width,
        height,
    )?);
    input
        .cell_raster_tiles
        .lock()
        .expect("cell raster tile cache poisoned")
        .insert(key, tile.clone());
    Some(tile)
}

fn paint_raster_tile(
    target: &mut RasterPaintTarget<'_>,
    raster_width: u32,
    raster_height: u32,
    primitive: &RasterTilePrimitive,
    layer: &LayerState,
) {
    let Some((x0, x1)) = raster_pixel_range(
        f32::from(primitive.bounds.origin.x),
        f32::from(primitive.bounds.origin.x + primitive.bounds.size.width),
        raster_width,
    ) else {
        return;
    };
    let Some((y0, y1)) = raster_pixel_range(
        f32::from(primitive.bounds.origin.y),
        f32::from(primitive.bounds.origin.y + primitive.bounds.size.height),
        raster_height,
    ) else {
        return;
    };
    let bounds_width = f32::from(primitive.bounds.size.width).max(f32::EPSILON);
    let bounds_height = f32::from(primitive.bounds.size.height).max(f32::EPSILON);
    let resolve_stipple_pattern = raster_tile_resolves_stipple(primitive, layer);
    let mut color = layer.color;
    if layer.fill == ShapeFill::Stippling && !resolve_stipple_pattern {
        color.a *= 0.25;
    }
    for y in y0..y1 {
        let source_y = (((y as f32 + 0.5 - f32::from(primitive.bounds.origin.y)) / bounds_height)
            * primitive.tile_height as f32)
            .floor()
            .clamp(0., primitive.tile_height as f32 - 1.) as u16;
        for x in x0..x1 {
            let source_x = (((x as f32 + 0.5 - f32::from(primitive.bounds.origin.x))
                / bounds_width)
                * primitive.tile_width as f32)
                .floor()
                .clamp(0., primitive.tile_width as f32 - 1.) as u16;
            let coverage = primitive.coverage
                [(source_y as usize * primitive.tile_width as usize) + source_x as usize];
            if coverage == 0 {
                continue;
            }
            let mut pixel_color = color;
            pixel_color.a *= coverage as f32 / u8::MAX as f32;
            if resolve_stipple_pattern {
                composite_raster_fill_pixel(
                    target,
                    raster_width,
                    x,
                    y,
                    ShapeFill::Stippling,
                    pixel_color,
                    0,
                );
            } else {
                target.pixel(
                    (y as usize * raster_width as usize) + x as usize,
                    pixel_color,
                );
            }
        }
    }
}

fn raster_tile_resolves_stipple(primitive: &RasterTilePrimitive, layer: &LayerState) -> bool {
    let stipple_period = (10. * RASTER_CACHE_RESOLUTION).round().max(1.);
    layer.fill == ShapeFill::Stippling
        && f32::from(primitive.bounds.size.width) >= stipple_period
        && f32::from(primitive.bounds.size.height) >= stipple_period
}

fn fill_raster_rect(
    target: &mut RasterPaintTarget<'_>,
    width: u32,
    height: u32,
    bounds: Bounds<Pixels>,
    fill: ShapeFill,
    color: Rgba,
    stipple_phase: i64,
) {
    let Some((x0, x1)) = raster_pixel_range(
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.x + bounds.size.width),
        width,
    ) else {
        return;
    };
    let Some((y0, y1)) = raster_pixel_range(
        f32::from(bounds.origin.y),
        f32::from(bounds.origin.y + bounds.size.height),
        height,
    ) else {
        return;
    };
    for y in y0..y1 {
        for x in x0..x1 {
            composite_raster_fill_pixel(target, width, x, y, fill, color, stipple_phase);
        }
    }
}

/// Match GPUI's global slash pattern with a fixed one-raster-pixel stroke.
/// A retained raster pixel covers two logical pixels, so keeping the sample
/// fully opaque is preferable to alpha-based sub-sampling: it gives outlines
/// and stipple strokes the same stable width and preserves layer paint order.
fn composite_raster_fill_pixel(
    target: &mut RasterPaintTarget<'_>,
    width: u32,
    x: u32,
    y: u32,
    fill: ShapeFill,
    color: Rgba,
    stipple_phase: i64,
) {
    if fill == ShapeFill::Stippling && !raster_stipple_is_on(x, y, stipple_phase) {
        return;
    }
    target.pixel((y * width + x) as usize, color);
}

fn stroke_raster_rect(
    target: &mut RasterPaintTarget<'_>,
    width: u32,
    height: u32,
    bounds: Bounds<Pixels>,
    color: Rgba,
) {
    let Some((x0, x1)) = raster_unclipped_pixel_range(
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.x + bounds.size.width),
    ) else {
        return;
    };
    let Some((y0, y1)) = raster_unclipped_pixel_range(
        f32::from(bounds.origin.y),
        f32::from(bounds.origin.y + bounds.size.height),
    ) else {
        return;
    };

    let bottom = y1 - 1;
    let right = x1 - 1;
    let clipped_x0 = x0.clamp(0, width as i32);
    let clipped_x1 = x1.clamp(0, width as i32);
    if clipped_x0 < clipped_x1 {
        for x in clipped_x0..clipped_x1 {
            if y0 >= 0 && y0 < height as i32 {
                target.pixel((y0 as u32 * width + x as u32) as usize, color);
            }
            if bottom != y0 && bottom >= 0 && bottom < height as i32 {
                target.pixel((bottom as u32 * width + x as u32) as usize, color);
            }
        }
    }
    // Exclude the two horizontal rows so corners are written exactly once.
    let clipped_y0 = (y0 + 1).clamp(0, height as i32);
    let clipped_y1 = bottom.clamp(0, height as i32);
    for y in clipped_y0..clipped_y1 {
        if x0 >= 0 && x0 < width as i32 {
            target.pixel((y as u32 * width + x0 as u32) as usize, color);
        }
        if right != x0 && right >= 0 && right < width as i32 {
            target.pixel((y as u32 * width + right as u32) as usize, color);
        }
    }
}

fn fill_raster_polygon(
    target: &mut RasterPaintTarget<'_>,
    width: u32,
    height: u32,
    points: &[Point<f32>],
    fill: ShapeFill,
    color: Rgba,
    stipple_phase: i64,
) {
    if points.len() < 3 {
        return;
    }
    let min_y = points
        .iter()
        .map(|point| point.y)
        .fold(f32::INFINITY, f32::min);
    let max_y = points
        .iter()
        .map(|point| point.y)
        .fold(f32::NEG_INFINITY, f32::max);
    let Some((y0, y1)) = raster_pixel_range(min_y, max_y, height) else {
        return;
    };
    let mut intersections = Vec::with_capacity(points.len());
    for y in y0..y1 {
        intersections.clear();
        let scan_y = y as f32 + 0.5;
        for (start, stop) in points
            .iter()
            .zip(points.iter().cycle().skip(1))
            .take(points.len())
        {
            if (start.y <= scan_y && stop.y > scan_y) || (stop.y <= scan_y && start.y > scan_y) {
                let fraction = (scan_y - start.y) / (stop.y - start.y);
                intersections.push(start.x + fraction * (stop.x - start.x));
            }
        }
        intersections.sort_by(f32::total_cmp);
        for pair in intersections.as_chunks::<2>().0 {
            let Some((x0, x1)) = raster_pixel_range(pair[0], pair[1], width) else {
                continue;
            };
            for x in x0..x1 {
                composite_raster_fill_pixel(target, width, x, y, fill, color, stipple_phase);
            }
        }
    }
}

fn stroke_raster_line(
    target: &mut RasterPaintTarget<'_>,
    width: u32,
    height: u32,
    start: Point<f32>,
    stop: Point<f32>,
    color: Rgba,
) {
    let steps = (stop.x - start.x)
        .abs()
        .max((stop.y - start.y).abs())
        .ceil() as usize;
    for step in 0..=steps {
        let fraction = if steps == 0 {
            0.
        } else {
            step as f32 / steps as f32
        };
        let x = (start.x + fraction * (stop.x - start.x)).round() as i32;
        let y = (start.y + fraction * (stop.y - start.y)).round() as i32;
        if x >= 0 && x < width as i32 && y >= 0 && y < height as i32 {
            target.pixel((y as u32 * width + x as u32) as usize, color);
        }
    }
}

fn collect_raster_outline_geometry<'a>(
    width: u32,
    height: u32,
    rects: impl IntoIterator<Item = Bounds<Pixels>>,
    polygons: impl IntoIterator<Item = &'a [Point<f32>]>,
    overlays: impl IntoIterator<Item = Bounds<Pixels>>,
) -> Option<RasterOutlineGeometry> {
    let mut builder = RasterOutlineGeometryBuilder::new(width as usize * height as usize);
    for bounds in rects.into_iter().chain(overlays) {
        builder.rect(bounds, width, height);
    }
    for points in polygons {
        for (start, stop) in points
            .iter()
            .zip(points.iter().cycle().skip(1))
            .take(points.len())
        {
            builder.line(*start, *stop);
        }
    }
    builder.finish()
}

fn build_layout_raster(
    rects: &[(Rect, LayerState)],
    polygons: &[(Polygon, LayerState)],
    scope_rects: &[LabeledBbox],
    texts: &[TextLabel],
    viewport: ViewportTransform,
    theme: &crate::theme::Theme,
    content_revision: u64,
) -> Option<LayoutRasterCache> {
    let width = (f32::from(viewport.size.width) * RASTER_CACHE_RESOLUTION)
        .ceil()
        .max(1.) as u32;
    let height = (f32::from(viewport.size.height) * RASTER_CACHE_RESOLUTION)
        .ceil()
        .max(1.) as u32;
    if width == 0 || height == 0 {
        return None;
    }
    let buffer_len = width as usize * height as usize * 4;
    let mut stipple_on = vec![0; buffer_len];
    let mut stipple_off = vec![0; buffer_len];
    let mut on_layer_buffer = vec![0; buffer_len];
    let mut off_layer_buffer = vec![0; buffer_len];
    let mut on_layer_touched = Vec::new();
    let mut off_layer_touched = Vec::new();
    let local_bounds = Bounds::new(
        Point::default(),
        Size::new(px(width as f32), px(height as f32)),
    );
    let raster_scale = viewport.scale * RASTER_CACHE_RESOLUTION;
    let raster_offset = Point::new(
        viewport.offset.x * RASTER_CACHE_RESOLUTION,
        viewport.offset.y * RASTER_CACHE_RESOLUTION,
    );
    let stipple_phase = raster_stipple_phase(raster_offset);
    let raster_rects = rects
        .iter()
        .map(|(rect, layer)| {
            (
                get_rect_bounds(rect, local_bounds, raster_scale, raster_offset),
                layer,
            )
        })
        .collect::<Vec<_>>();
    let raster_polygons = polygons
        .iter()
        .map(|(polygon, layer)| {
            let points = polygon
                .points
                .iter()
                .map(|point| {
                    Point::new(
                        raster_scale * point.x + f32::from(raster_offset.x),
                        raster_scale * -point.y + f32::from(raster_offset.y),
                    )
                })
                .collect::<Vec<_>>();
            (points, layer)
        })
        .collect::<Vec<_>>();
    let layer_count = raster_rects
        .iter()
        .map(|(_, layer)| layer.z)
        .chain(raster_polygons.iter().map(|(_, layer)| layer.z))
        .max()
        .map_or(0, |z| z + 1);
    let mut rect_layers = vec![Vec::new(); layer_count];
    let mut polygon_layers = vec![Vec::new(); layer_count];
    let mut coalesced_outline_occupancy = (0..layer_count)
        .map(|_| None::<Vec<u64>>)
        .collect::<Vec<_>>();
    let mut coalesced_outline_colors = vec![None; layer_count];
    for (index, (bounds, layer)) in raster_rects.iter().enumerate() {
        if raster_bounds_have_resolvable_outline_gap(*bounds) {
            rect_layers[layer.z].push(index);
        } else {
            mark_raster_occupancy(
                &mut coalesced_outline_occupancy[layer.z],
                width,
                height,
                *bounds,
            );
            coalesced_outline_colors[layer.z] = Some(layer.border_color);
        }
    }
    for (index, (points, layer)) in raster_polygons.iter().enumerate() {
        let (min_x, max_x, min_y, max_y) = points.iter().fold(
            (
                f32::INFINITY,
                f32::NEG_INFINITY,
                f32::INFINITY,
                f32::NEG_INFINITY,
            ),
            |(min_x, max_x, min_y, max_y), point| {
                (
                    min_x.min(point.x),
                    max_x.max(point.x),
                    min_y.min(point.y),
                    max_y.max(point.y),
                )
            },
        );
        let bounds = Bounds::from_corners(
            Point::new(px(min_x), px(min_y)),
            Point::new(px(max_x), px(max_y)),
        );
        if raster_bounds_have_resolvable_outline_gap(bounds) {
            polygon_layers[layer.z].push(index);
        } else {
            mark_raster_occupancy(
                &mut coalesced_outline_occupancy[layer.z],
                width,
                height,
                bounds,
            );
            coalesced_outline_colors[layer.z] = Some(layer.border_color);
        }
    }
    let outline_geometry = collect_raster_outline_geometry(
        width,
        height,
        rect_layers
            .iter()
            .flatten()
            .map(|index| raster_rects[*index].0),
        polygon_layers
            .iter()
            .flatten()
            .map(|index| raster_polygons[*index].0.as_slice()),
        scope_rects
            .iter()
            .map(|bbox| get_rect_bounds(&bbox.rect, local_bounds, raster_scale, raster_offset)),
    );
    let outline_pixels = outline_geometry
        .as_ref()
        .map(|geometry| raster_outline_candidate_pixels(geometry, width, height));
    let retain_outline_underlay = outline_pixels
        .as_ref()
        .is_some_and(|pixels| !pixels.is_empty());
    let mut underlay_on = retain_outline_underlay.then(|| vec![0; buffer_len]);
    let mut underlay_off = retain_outline_underlay.then(|| vec![0; buffer_len]);
    for (layer_index, (rect_layer, polygon_layer)) in
        rect_layers.iter().zip(&polygon_layers).enumerate()
    {
        // The on plane treats every stippled fill as solid. The off plane
        // omits stippled fills but retains solid fills. Selecting between the
        // two after a camera transform restores the pattern at fixed screen
        // size without losing any cross-layer compositing information.
        let mut on_fill_target = RasterPaintTarget {
            buffer: &mut on_layer_buffer,
            composite: RasterComposite::Union,
            touched: Some(&mut on_layer_touched),
        };
        if let (Some(occupancy), Some(color)) = (
            coalesced_outline_occupancy[layer_index].as_ref(),
            coalesced_outline_colors[layer_index],
        ) {
            paint_raster_occupancy_color(
                &mut on_fill_target,
                occupancy,
                width as usize * height as usize,
                color,
            );
        }
        for index in rect_layer {
            let (bounds, layer) = raster_rects[*index];
            fill_raster_rect(
                &mut on_fill_target,
                width,
                height,
                bounds,
                ShapeFill::Solid,
                layer.color,
                0,
            );
        }
        for index in polygon_layer {
            let (points, layer) = &raster_polygons[*index];
            fill_raster_polygon(
                &mut on_fill_target,
                width,
                height,
                points,
                ShapeFill::Solid,
                layer.color,
                0,
            );
        }
        let mut off_fill_target = RasterPaintTarget {
            buffer: &mut off_layer_buffer,
            composite: RasterComposite::Union,
            touched: Some(&mut off_layer_touched),
        };
        if let (Some(occupancy), Some(color)) = (
            coalesced_outline_occupancy[layer_index].as_ref(),
            coalesced_outline_colors[layer_index],
        ) {
            paint_raster_occupancy_color(
                &mut off_fill_target,
                occupancy,
                width as usize * height as usize,
                color,
            );
        }
        for index in rect_layer {
            let (bounds, layer) = raster_rects[*index];
            if layer.fill == ShapeFill::Solid {
                fill_raster_rect(
                    &mut off_fill_target,
                    width,
                    height,
                    bounds,
                    ShapeFill::Solid,
                    layer.color,
                    0,
                );
            }
        }
        for index in polygon_layer {
            let (points, layer) = &raster_polygons[*index];
            if layer.fill == ShapeFill::Solid {
                fill_raster_polygon(
                    &mut off_fill_target,
                    width,
                    height,
                    points,
                    ShapeFill::Solid,
                    layer.color,
                    0,
                );
            }
        }
        if let Some(underlay) = &mut underlay_on {
            blend_raster_layer_preserving(
                underlay,
                &on_layer_buffer,
                outline_pixels.as_deref().expect("retained outline pixels"),
            );
        }
        if let Some(underlay) = &mut underlay_off {
            blend_raster_layer_preserving(
                underlay,
                &off_layer_buffer,
                outline_pixels.as_deref().expect("retained outline pixels"),
            );
        }
        let mut on_outline_target = RasterPaintTarget {
            buffer: &mut on_layer_buffer,
            composite: RasterComposite::Replace,
            touched: Some(&mut on_layer_touched),
        };
        for index in rect_layer {
            let (bounds, layer) = raster_rects[*index];
            stroke_raster_rect(
                &mut on_outline_target,
                width,
                height,
                bounds,
                layer.border_color,
            );
        }
        for index in polygon_layer {
            let (points, layer) = &raster_polygons[*index];
            for (start, stop) in points
                .iter()
                .zip(points.iter().cycle().skip(1))
                .take(points.len())
            {
                stroke_raster_line(
                    &mut on_outline_target,
                    width,
                    height,
                    *start,
                    *stop,
                    layer.border_color,
                );
            }
        }
        let mut off_outline_target = RasterPaintTarget {
            buffer: &mut off_layer_buffer,
            composite: RasterComposite::Replace,
            touched: Some(&mut off_layer_touched),
        };
        for index in rect_layer {
            let (bounds, layer) = raster_rects[*index];
            stroke_raster_rect(
                &mut off_outline_target,
                width,
                height,
                bounds,
                layer.border_color,
            );
        }
        for index in polygon_layer {
            let (points, layer) = &raster_polygons[*index];
            for (start, stop) in points
                .iter()
                .zip(points.iter().cycle().skip(1))
                .take(points.len())
            {
                stroke_raster_line(
                    &mut off_outline_target,
                    width,
                    height,
                    *start,
                    *stop,
                    layer.border_color,
                );
            }
        }
        blend_raster_layer(&mut stipple_on, &mut on_layer_buffer, &mut on_layer_touched);
        blend_raster_layer(
            &mut stipple_off,
            &mut off_layer_buffer,
            &mut off_layer_touched,
        );
    }
    let mut on_overlay_target = RasterPaintTarget {
        buffer: &mut stipple_on,
        composite: RasterComposite::Replace,
        touched: None,
    };
    for bbox in scope_rects {
        let bounds = get_rect_bounds(&bbox.rect, local_bounds, raster_scale, raster_offset);
        stroke_raster_rect(&mut on_overlay_target, width, height, bounds, theme.text);
    }
    let mut off_overlay_target = RasterPaintTarget {
        buffer: &mut stipple_off,
        composite: RasterComposite::Replace,
        touched: None,
    };
    for bbox in scope_rects {
        let bounds = get_rect_bounds(&bbox.rect, local_bounds, raster_scale, raster_offset);
        stroke_raster_rect(&mut off_overlay_target, width, height, bounds, theme.text);
    }
    let image = expand_raster_for_display(raster_from_style_planes(
        width,
        height,
        &stipple_on,
        &stipple_off,
        stipple_phase,
    )?);
    let outline_correction = build_raster_outline_correction(
        outline_geometry,
        outline_pixels.as_deref(),
        &stipple_on,
        &stipple_off,
        underlay_on.as_deref(),
        underlay_off.as_deref(),
    );
    Some(LayoutRasterCache {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(image)])),
        style_planes: Some(Arc::new(RasterStylePlanes {
            width,
            height,
            stipple_on: stipple_on.into(),
            stipple_off: stipple_off.into(),
            outline_correction,
        })),
        texts: texts.to_vec().into(),
        scope_labels: scope_rects.to_vec().into(),
        // Use the texture's actual logical extent after rounding. Otherwise
        // an odd-sized canvas stretches every texel by a tiny amount and a
        // replacement raster appears to shift the geometry.
        viewport: raster_logical_size(width, height),
        screen_viewport: viewport.screen_size,
        scale: viewport.scale,
        offset: viewport.offset,
        content_revision,
    })
}

/// Builds the viewport raster without touching GPUI's scene or entity state.
/// This is intentionally independent from the exact/editable paint path so it
/// can run on the background executor while the UI keeps transforming the last
/// completed image at display refresh rate.
fn navigation_raster_cancelled(input: &NavigationRasterInput) -> bool {
    input.content_revision_signal.load(Ordering::Acquire) != input.content_revision
        || input.scale_signal.load(Ordering::Acquire) != input.viewport.scale.to_bits() as u64
        || input
            .cancel_if_generation_changes
            .as_ref()
            .is_some_and(|(signal, generation)| signal.load(Ordering::Acquire) != *generation)
}

fn build_navigation_raster(input: NavigationRasterInput) -> Option<LayoutRasterCache> {
    if navigation_raster_cancelled(&input) {
        return None;
    }
    let viewport = input.viewport;
    let width = (f32::from(viewport.size.width) * RASTER_CACHE_RESOLUTION)
        .ceil()
        .max(1.) as u32;
    let height = (f32::from(viewport.size.height) * RASTER_CACHE_RESOLUTION)
        .ceil()
        .max(1.) as u32;
    if width == 0 || height == 0 {
        return None;
    }

    let raster_scale = viewport.scale * RASTER_CACHE_RESOLUTION;
    let raster_offset = Point::new(
        viewport.offset.x * RASTER_CACHE_RESOLUTION,
        viewport.offset.y * RASTER_CACHE_RESOLUTION,
    );
    let stipple_phase = raster_stipple_phase(raster_offset);
    let local_bounds = Bounds::new(
        Point::default(),
        Size::new(px(width as f32), px(height as f32)),
    );
    let layer_count = input
        .layers
        .values()
        .map(|layer| layer.z)
        .max()
        .map_or(0, |z| z + 1);
    let mut rect_primitives = (0..layer_count)
        .map(|_| Vec::<RasterRectPrimitive>::new())
        .collect::<Vec<_>>();
    let mut polygon_primitives = (0..layer_count)
        .map(|_| Vec::<RasterPolygonPrimitive>::new())
        .collect::<Vec<_>>();
    let mut coalesced_outline_occupancy = (0..layer_count)
        .map(|_| None::<Vec<u64>>)
        .collect::<Vec<_>>();
    let mut tile_primitives = (0..layer_count)
        .map(|_| Vec::<RasterTilePrimitive>::new())
        .collect::<Vec<_>>();
    let mut seen_tile_candidates = HashSet::<CellRasterTileKey>::new();
    let geometry_lod_size = GEOMETRY_LOD_SIZE_PX * RASTER_CACHE_RESOLUTION;
    let mut texts = Vec::new();
    let mut scope_rects = Vec::new();
    let world_query = raster_viewport_world_bounds(viewport);
    let query_margin = if input.include_text {
        MAX_TEXT_PX as f64 / viewport.scale.abs() as f64
    } else {
        f64::from(DEFAULT_BORDER_WIDTH) / viewport.scale.abs() as f64
    };

    let selected = &input.solved_cell.state[&input.solved_cell.selected_scope].address;
    let mut queue = VecDeque::from_iter([(
        ScopeAddress {
            cell: selected.cell,
            scope: if input.hide_external_geometry {
                selected.scope
            } else {
                input.solved_cell.output.cells[&selected.cell].root
            },
        },
        TransformationMatrix::identity(),
        (0., 0.),
        0,
    )]);

    while let Some((address @ ScopeAddress { cell, scope }, mat, ofs, depth)) = queue.pop_front() {
        if navigation_raster_cancelled(&input) {
            return None;
        }
        let cell_info = &input.solved_cell.output.cells[&cell];
        let scope_info = &cell_info.scopes[&scope];
        let scope_state = &input.solved_cell.state[&input.solved_cell.scope_paths[&address]];

        if let Some(bbox) = &scope_state.bbox {
            let p0 = ifmatvec(mat, (bbox.x0, bbox.y0));
            let p1 = ifmatvec(mat, (bbox.x1, bbox.y1));
            let rect = Rect {
                x0: (p0.0.min(p1.0) + ofs.0) as f32,
                y0: (p0.1.min(p1.1) + ofs.1) as f32,
                x1: (p0.0.max(p1.0) + ofs.0) as f32,
                y1: (p0.1.max(p1.1) + ofs.1) as f32,
                id: None,
                object_path: Vec::new(),
                border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                border_styles: Edges::all(BorderStyle::Solid),
                cvars: None,
            };
            let pixel_bounds = get_rect_bounds(&rect, local_bounds, raster_scale, raster_offset);
            if depth >= input.hierarchy_depth || !scope_state.visible {
                scope_rects.push(LabeledBbox {
                    rect,
                    label: scope_state.name.clone().into(),
                    origin: None,
                });
                continue;
            }
            let tile_width = f32::from(pixel_bounds.size.width).ceil().max(1.) as u32;
            let tile_height = f32::from(pixel_bounds.size.height).ceil().max(1.) as u32;
            if CELL_RASTER_TILES_ENABLED
                && depth > 0
                && input.hierarchy_depth == usize::MAX
                && tile_width <= CELL_RASTER_TILE_MAX_SIZE
                && tile_height <= CELL_RASTER_TILE_MAX_SIZE
            {
                let key = CellRasterTileKey {
                    address,
                    orientation: mat,
                    width: tile_width as u16,
                    height: tile_height as u16,
                    content_revision: input.content_revision,
                };
                let cached = input
                    .cell_raster_tiles
                    .lock()
                    .expect("cell raster tile cache poisoned")
                    .get(&key);
                let tile = if cached.is_some() || !seen_tile_candidates.insert(key) {
                    cached.or_else(|| {
                        cached_cell_raster_tile(
                            &input,
                            address,
                            mat,
                            tile_width as u16,
                            tile_height as u16,
                        )
                    })
                } else {
                    None
                };
                if let Some(tile) = tile {
                    for (layer_index, coverage) in tile.layers.iter().enumerate() {
                        if let Some(coverage) = coverage {
                            tile_primitives[layer_index].push(RasterTilePrimitive {
                                coverage: coverage.clone(),
                                tile_width: tile.width,
                                tile_height: tile.height,
                                bounds: pixel_bounds,
                            });
                        }
                    }
                    continue;
                }
            }
        }

        let local_query = raster_scope_query_bounds(world_query, mat, ofs, query_margin);
        let emit_indices = if input.use_spatial_index {
            input.spatial_index.query(
                &input.solved_cell,
                address,
                local_query,
                scope_info.emit.len(),
            )
        } else {
            (0..scope_info.emit.len()).collect()
        };
        for (query_index, emit_index) in emit_indices.into_iter().enumerate() {
            if query_index % 256 == 0 && navigation_raster_cancelled(&input) {
                return None;
            }
            let (object, _) = &scope_info.emit[emit_index];
            match &cell_info.objects[object] {
                SolvedValue::Rect(rect) if !rect.construction => {
                    let Some(layer) = rect
                        .layer
                        .as_ref()
                        .and_then(|name| input.layers.get(name.as_str()))
                        .filter(|layer| layer.visible)
                    else {
                        continue;
                    };
                    let p0 = ifmatvec(mat, (rect.x0.0, rect.y0.0));
                    let p1 = ifmatvec(mat, (rect.x1.0, rect.y1.0));
                    let layout_rect = Rect {
                        x0: (p0.0.min(p1.0) + ofs.0) as f32,
                        y0: (p0.1.min(p1.1) + ofs.1) as f32,
                        x1: (p0.0.max(p1.0) + ofs.0) as f32,
                        y1: (p0.1.max(p1.1) + ofs.1) as f32,
                        id: None,
                        object_path: Vec::new(),
                        border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                        border_styles: Edges::all(BorderStyle::Solid),
                        cvars: None,
                    };
                    let bounds =
                        get_rect_bounds(&layout_rect, local_bounds, raster_scale, raster_offset);
                    if !bounds.intersects(&local_bounds) {
                        continue;
                    }
                    if f32::from(bounds.size.width).max(f32::from(bounds.size.height))
                        <= geometry_lod_size
                    {
                        mark_raster_occupancy(
                            &mut coalesced_outline_occupancy[layer.z],
                            width,
                            height,
                            bounds,
                        );
                        continue;
                    }
                    if !raster_bounds_have_resolvable_outline_gap(bounds) {
                        mark_raster_occupancy(
                            &mut coalesced_outline_occupancy[layer.z],
                            width,
                            height,
                            bounds,
                        );
                        continue;
                    }
                    rect_primitives[layer.z].push(RasterRectPrimitive {
                        bounds,
                        fill: layer.fill,
                        color: layer.color,
                        border_color: layer.border_color,
                    });
                }
                SolvedValue::Polygon(polygon) => {
                    let Some(layer) = input
                        .layers
                        .get(polygon.layer.as_str())
                        .filter(|layer| layer.visible)
                    else {
                        continue;
                    };
                    let points = polygon
                        .points
                        .iter()
                        .map(|(x, y)| {
                            let point = ifmatvec(mat, (x.0, y.0));
                            Point::new(
                                raster_scale * (point.0 + ofs.0) as f32
                                    + f32::from(raster_offset.x),
                                raster_scale * -(point.1 + ofs.1) as f32
                                    + f32::from(raster_offset.y),
                            )
                        })
                        .collect::<Vec<_>>();
                    let (min_x, max_x, min_y, max_y) = points.iter().fold(
                        (
                            f32::INFINITY,
                            f32::NEG_INFINITY,
                            f32::INFINITY,
                            f32::NEG_INFINITY,
                        ),
                        |(min_x, max_x, min_y, max_y), point| {
                            (
                                min_x.min(point.x),
                                max_x.max(point.x),
                                min_y.min(point.y),
                                max_y.max(point.y),
                            )
                        },
                    );
                    if max_x < 0. || min_x > width as f32 || max_y < 0. || min_y > height as f32 {
                        continue;
                    }
                    let bounds = Bounds::from_corners(
                        Point::new(px(min_x), px(min_y)),
                        Point::new(px(max_x), px(max_y)),
                    );
                    if (max_x - min_x).max(max_y - min_y) <= geometry_lod_size {
                        mark_raster_occupancy(
                            &mut coalesced_outline_occupancy[layer.z],
                            width,
                            height,
                            bounds,
                        );
                        continue;
                    }
                    if !raster_bounds_have_resolvable_outline_gap(bounds) {
                        mark_raster_occupancy(
                            &mut coalesced_outline_occupancy[layer.z],
                            width,
                            height,
                            bounds,
                        );
                        continue;
                    }
                    polygon_primitives[layer.z].push(RasterPolygonPrimitive {
                        points,
                        fill: layer.fill,
                        color: layer.color,
                        border_color: layer.border_color,
                    });
                }
                SolvedValue::Path(path) => {
                    let Some(layer) = input
                        .layers
                        .get(path.layer.as_str())
                        .filter(|layer| layer.visible)
                    else {
                        continue;
                    };
                    let Some(outline) = path.outline() else {
                        continue;
                    };
                    let points = outline
                        .into_iter()
                        .map(|point| {
                            let point = ifmatvec(mat, point);
                            Point::new(
                                raster_scale * (point.0 + ofs.0) as f32
                                    + f32::from(raster_offset.x),
                                raster_scale * -(point.1 + ofs.1) as f32
                                    + f32::from(raster_offset.y),
                            )
                        })
                        .collect::<Vec<_>>();
                    let (min_x, max_x, min_y, max_y) = points.iter().fold(
                        (
                            f32::INFINITY,
                            f32::NEG_INFINITY,
                            f32::INFINITY,
                            f32::NEG_INFINITY,
                        ),
                        |(min_x, max_x, min_y, max_y), point| {
                            (
                                min_x.min(point.x),
                                max_x.max(point.x),
                                min_y.min(point.y),
                                max_y.max(point.y),
                            )
                        },
                    );
                    if max_x < 0. || min_x > width as f32 || max_y < 0. || min_y > height as f32 {
                        continue;
                    }
                    let bounds = Bounds::from_corners(
                        Point::new(px(min_x), px(min_y)),
                        Point::new(px(max_x), px(max_y)),
                    );
                    if (max_x - min_x).max(max_y - min_y) <= geometry_lod_size
                        || !raster_bounds_have_resolvable_outline_gap(bounds)
                    {
                        mark_raster_occupancy(
                            &mut coalesced_outline_occupancy[layer.z],
                            width,
                            height,
                            bounds,
                        );
                        continue;
                    }
                    polygon_primitives[layer.z].push(RasterPolygonPrimitive {
                        points,
                        fill: layer.fill,
                        color: layer.color,
                        border_color: layer.border_color,
                    });
                }
                SolvedValue::Instance(instance) if !instance.construction => {
                    let mut instance_mat = TransformationMatrix::identity();
                    if instance.reflect {
                        instance_mat = instance_mat.reflect_vert();
                    }
                    instance_mat = instance_mat.rotate(instance.angle);
                    let instance_ofs = ifmatvec(mat, (instance.x, instance.y));
                    let child = instance.cell;
                    queue.push_back((
                        ScopeAddress {
                            cell: child,
                            scope: input.solved_cell.output.cells[&child].root,
                        },
                        mat * instance_mat,
                        (instance_ofs.0 + ofs.0, instance_ofs.1 + ofs.1),
                        depth + 1,
                    ));
                }
                SolvedValue::Text(text)
                    if input.include_text
                        && layout_text_metrics(viewport.scale, TEXT_LAYOUT_SIZE).is_some() =>
                {
                    let Some(layer) = input
                        .layers
                        .get(text.layer.as_str())
                        .filter(|layer| layer.visible)
                    else {
                        continue;
                    };
                    let position = ifmatvec(mat, (text.x, text.y));
                    let raster_position = Point::new(
                        raster_scale * (position.0 + ofs.0) as f32 + f32::from(raster_offset.x),
                        raster_scale * -(position.1 + ofs.1) as f32 + f32::from(raster_offset.y),
                    );
                    let text_margin = MAX_TEXT_PX * RASTER_CACHE_RESOLUTION;
                    if raster_position.x < -text_margin
                        || raster_position.x > width as f32 + text_margin
                        || raster_position.y < -text_margin
                        || raster_position.y > height as f32 + text_margin
                    {
                        continue;
                    }
                    texts.push(TextLabel {
                        text: text.text.clone().into(),
                        position: Point::new(
                            (position.0 + ofs.0) as f32,
                            (position.1 + ofs.1) as f32,
                        ),
                        layer: layer.clone(),
                    });
                }
                _ => {}
            }
        }

        for child in &scope_info.children {
            queue.push_back((
                ScopeAddress {
                    cell,
                    scope: *child,
                },
                mat,
                ofs,
                depth + 1,
            ));
        }
    }

    let outline_geometry = collect_raster_outline_geometry(
        width,
        height,
        rect_primitives
            .iter()
            .flatten()
            .map(|primitive| primitive.bounds),
        polygon_primitives
            .iter()
            .flatten()
            .map(|primitive| primitive.points.as_slice()),
        scope_rects
            .iter()
            .map(|bbox| get_rect_bounds(&bbox.rect, local_bounds, raster_scale, raster_offset)),
    );
    let outline_pixels = outline_geometry
        .as_ref()
        .map(|geometry| raster_outline_candidate_pixels(geometry, width, height));
    let buffer_len = width as usize * height as usize * 4;
    let mut stipple_on = vec![0; buffer_len];
    let mut stipple_off = vec![0; buffer_len];
    let mut on_layer_buffer = vec![0; buffer_len];
    let mut off_layer_buffer = vec![0; buffer_len];
    let mut on_layer_touched = Vec::new();
    let mut off_layer_touched = Vec::new();
    let retain_outline_underlay = outline_pixels
        .as_ref()
        .is_some_and(|pixels| !pixels.is_empty());
    let mut underlay_on = retain_outline_underlay.then(|| vec![0; buffer_len]);
    let mut underlay_off = retain_outline_underlay.then(|| vec![0; buffer_len]);
    let pixel_count = width as usize * height as usize;
    for (layer_index, (rect_layer, polygon_layer)) in
        rect_primitives.iter().zip(&polygon_primitives).enumerate()
    {
        if navigation_raster_cancelled(&input) {
            return None;
        }
        let layer = input
            .layers
            .values()
            .find(|layer| layer.z == layer_index && layer.visible);
        {
            let mut fill_target = RasterPaintTarget {
                buffer: &mut on_layer_buffer,
                composite: RasterComposite::Union,
                touched: Some(&mut on_layer_touched),
            };
            if let Some(occupancy) = coalesced_outline_occupancy[layer_index].as_ref()
                && let Some(layer) = layer
            {
                paint_raster_occupancy_color(
                    &mut fill_target,
                    occupancy,
                    pixel_count,
                    layer.border_color,
                );
            }
            if let Some(layer) = layer {
                for (primitive_index, primitive) in tile_primitives[layer_index].iter().enumerate()
                {
                    if primitive_index % 256 == 0 && navigation_raster_cancelled(&input) {
                        return None;
                    }
                    let mut on_layer = layer.clone();
                    if raster_tile_resolves_stipple(primitive, layer) {
                        on_layer.fill = ShapeFill::Solid;
                    }
                    paint_raster_tile(&mut fill_target, width, height, primitive, &on_layer);
                }
            }
            for (primitive_index, primitive) in rect_layer.iter().enumerate() {
                if primitive_index % 256 == 0 && navigation_raster_cancelled(&input) {
                    return None;
                }
                let RasterRectPrimitive { bounds, color, .. } = primitive;
                fill_raster_rect(
                    &mut fill_target,
                    width,
                    height,
                    *bounds,
                    ShapeFill::Solid,
                    *color,
                    0,
                );
            }
            for (primitive_index, primitive) in polygon_layer.iter().enumerate() {
                if primitive_index % 256 == 0 && navigation_raster_cancelled(&input) {
                    return None;
                }
                let RasterPolygonPrimitive { points, color, .. } = primitive;
                fill_raster_polygon(
                    &mut fill_target,
                    width,
                    height,
                    points,
                    ShapeFill::Solid,
                    *color,
                    0,
                );
            }
        }
        {
            let mut fill_target = RasterPaintTarget {
                buffer: &mut off_layer_buffer,
                composite: RasterComposite::Union,
                touched: Some(&mut off_layer_touched),
            };
            if let Some(occupancy) = coalesced_outline_occupancy[layer_index].as_ref()
                && let Some(layer) = layer
            {
                paint_raster_occupancy_color(
                    &mut fill_target,
                    occupancy,
                    pixel_count,
                    layer.border_color,
                );
            }
            if let Some(layer) = layer {
                for (primitive_index, primitive) in tile_primitives[layer_index].iter().enumerate()
                {
                    if primitive_index % 256 == 0 && navigation_raster_cancelled(&input) {
                        return None;
                    }
                    if layer.fill == ShapeFill::Solid
                        || !raster_tile_resolves_stipple(primitive, layer)
                    {
                        paint_raster_tile(&mut fill_target, width, height, primitive, layer);
                    }
                }
            }
            for (primitive_index, primitive) in rect_layer.iter().enumerate() {
                if primitive_index % 256 == 0 && navigation_raster_cancelled(&input) {
                    return None;
                }
                if primitive.fill == ShapeFill::Solid {
                    fill_raster_rect(
                        &mut fill_target,
                        width,
                        height,
                        primitive.bounds,
                        ShapeFill::Solid,
                        primitive.color,
                        0,
                    );
                }
            }
            for (primitive_index, primitive) in polygon_layer.iter().enumerate() {
                if primitive_index % 256 == 0 && navigation_raster_cancelled(&input) {
                    return None;
                }
                if primitive.fill == ShapeFill::Solid {
                    fill_raster_polygon(
                        &mut fill_target,
                        width,
                        height,
                        &primitive.points,
                        ShapeFill::Solid,
                        primitive.color,
                        0,
                    );
                }
            }
        }
        if let Some(underlay) = &mut underlay_on {
            blend_raster_layer_preserving(
                underlay,
                &on_layer_buffer,
                outline_pixels.as_deref().expect("retained outline pixels"),
            );
        }
        if let Some(underlay) = &mut underlay_off {
            blend_raster_layer_preserving(
                underlay,
                &off_layer_buffer,
                outline_pixels.as_deref().expect("retained outline pixels"),
            );
        }
        {
            let mut outline_target = RasterPaintTarget {
                buffer: &mut on_layer_buffer,
                composite: RasterComposite::Replace,
                touched: Some(&mut on_layer_touched),
            };
            for (primitive_index, primitive) in rect_layer.iter().enumerate() {
                if primitive_index % 256 == 0 && navigation_raster_cancelled(&input) {
                    return None;
                }
                stroke_raster_rect(
                    &mut outline_target,
                    width,
                    height,
                    primitive.bounds,
                    primitive.border_color,
                );
            }
            for (primitive_index, primitive) in polygon_layer.iter().enumerate() {
                if primitive_index % 256 == 0 && navigation_raster_cancelled(&input) {
                    return None;
                }
                let RasterPolygonPrimitive {
                    points,
                    border_color,
                    ..
                } = primitive;
                for (start, stop) in points
                    .iter()
                    .zip(points.iter().cycle().skip(1))
                    .take(points.len())
                {
                    stroke_raster_line(
                        &mut outline_target,
                        width,
                        height,
                        *start,
                        *stop,
                        *border_color,
                    );
                }
            }
        }
        {
            let mut outline_target = RasterPaintTarget {
                buffer: &mut off_layer_buffer,
                composite: RasterComposite::Replace,
                touched: Some(&mut off_layer_touched),
            };
            for (primitive_index, primitive) in rect_layer.iter().enumerate() {
                if primitive_index % 256 == 0 && navigation_raster_cancelled(&input) {
                    return None;
                }
                stroke_raster_rect(
                    &mut outline_target,
                    width,
                    height,
                    primitive.bounds,
                    primitive.border_color,
                );
            }
            for (primitive_index, primitive) in polygon_layer.iter().enumerate() {
                if primitive_index % 256 == 0 && navigation_raster_cancelled(&input) {
                    return None;
                }
                for (start, stop) in primitive
                    .points
                    .iter()
                    .zip(primitive.points.iter().cycle().skip(1))
                    .take(primitive.points.len())
                {
                    stroke_raster_line(
                        &mut outline_target,
                        width,
                        height,
                        *start,
                        *stop,
                        primitive.border_color,
                    );
                }
            }
        }
        blend_raster_layer(&mut stipple_on, &mut on_layer_buffer, &mut on_layer_touched);
        blend_raster_layer(
            &mut stipple_off,
            &mut off_layer_buffer,
            &mut off_layer_touched,
        );
    }
    let mut on_overlay_target = RasterPaintTarget {
        buffer: &mut stipple_on,
        composite: RasterComposite::Replace,
        touched: None,
    };
    for bbox in &scope_rects {
        let bounds = get_rect_bounds(&bbox.rect, local_bounds, raster_scale, raster_offset);
        stroke_raster_rect(
            &mut on_overlay_target,
            width,
            height,
            bounds,
            input.text_color,
        );
    }
    let mut off_overlay_target = RasterPaintTarget {
        buffer: &mut stipple_off,
        composite: RasterComposite::Replace,
        touched: None,
    };
    for bbox in &scope_rects {
        let bounds = get_rect_bounds(&bbox.rect, local_bounds, raster_scale, raster_offset);
        stroke_raster_rect(
            &mut off_overlay_target,
            width,
            height,
            bounds,
            input.text_color,
        );
    }

    let image = expand_raster_for_display(raster_from_style_planes(
        width,
        height,
        &stipple_on,
        &stipple_off,
        stipple_phase,
    )?);
    let outline_correction = build_raster_outline_correction(
        outline_geometry,
        outline_pixels.as_deref(),
        &stipple_on,
        &stipple_off,
        underlay_on.as_deref(),
        underlay_off.as_deref(),
    );
    Some(LayoutRasterCache {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(image)])),
        style_planes: Some(Arc::new(RasterStylePlanes {
            width,
            height,
            stipple_on: stipple_on.into(),
            stipple_off: stipple_off.into(),
            outline_correction,
        })),
        texts: texts.into(),
        scope_labels: scope_rects.into(),
        viewport: raster_logical_size(width, height),
        screen_viewport: viewport.screen_size,
        scale: viewport.scale,
        offset: viewport.offset,
        content_revision: input.content_revision,
    })
}

fn raster_bounds(
    cache: &LayoutRasterCache,
    bounds: Bounds<Pixels>,
    scale: f32,
    offset: Point<Pixels>,
) -> Bounds<Pixels> {
    let ratio = scale / cache.scale;
    Bounds::new(
        Point::new(
            bounds.origin.x + offset.x - cache.offset.x * ratio,
            bounds.origin.y + offset.y - cache.offset.y * ratio,
        ),
        Size::new(cache.viewport.width * ratio, cache.viewport.height * ratio),
    )
}

fn navigation_overview_layers(
    layers: &IndexMap<SharedString, LayerState>,
) -> IndexMap<SharedString, LayerState> {
    layers
        .iter()
        .filter(|(_, layer)| layer.visible)
        .map(|(name, layer)| (name.clone(), layer.clone()))
        .collect()
}

fn navigation_overview_viewport(bbox: &compile::Rect<f64>) -> ViewportTransform {
    navigation_overview_viewport_for_extents_in_size(
        bbox.x0,
        bbox.y0,
        bbox.x1,
        bbox.y1,
        Size::new(NAVIGATION_OVERVIEW_SIZE, NAVIGATION_OVERVIEW_SIZE),
    )
}

fn navigation_overview_viewport_for_extents_in_size(
    bbox_x0: f64,
    bbox_y0: f64,
    bbox_x1: f64,
    bbox_y1: f64,
    size: Size<Pixels>,
) -> ViewportTransform {
    let x0 = bbox_x0.min(bbox_x1) as f32;
    let x1 = bbox_x0.max(bbox_x1) as f32;
    let y0 = bbox_y0.min(bbox_y1) as f32;
    let y1 = bbox_y0.max(bbox_y1) as f32;
    let scale = fit_scale(size, x1 - x0, y1 - y0);
    ViewportTransform {
        size,
        screen_size: size,
        scale,
        offset: Point::new(
            px((-(x0 + x1) * scale + f32::from(size.width)) / 2.),
            px(((y1 + y0) * scale + f32::from(size.height)) / 2.),
        ),
    }
}

fn navigation_overview_display_viewport(
    bbox: &compile::Rect<f64>,
    current: ViewportTransform,
    size: Size<Pixels>,
) -> ViewportTransform {
    navigation_overview_display_viewport_for_bounds(
        RasterBvhBounds {
            min_x: bbox.x0.min(bbox.x1),
            min_y: bbox.y0.min(bbox.y1),
            max_x: bbox.x0.max(bbox.x1),
            max_y: bbox.y0.max(bbox.y1),
        },
        current,
        size,
    )
}

fn navigation_overview_display_viewport_for_bounds(
    cell: RasterBvhBounds,
    current: ViewportTransform,
    size: Size<Pixels>,
) -> ViewportTransform {
    let world = raster_viewport_world_bounds(current).union(cell);
    navigation_overview_viewport_for_extents_in_size(
        world.min_x,
        world.min_y,
        world.max_x,
        world.max_y,
        size,
    )
}

fn navigation_overview_world_bounds(
    overview: ViewportTransform,
    world: RasterBvhBounds,
    target: Bounds<Pixels>,
) -> Bounds<Pixels> {
    let target_x_scale = f32::from(target.size.width) / f32::from(overview.size.width);
    let target_y_scale = f32::from(target.size.height) / f32::from(overview.size.height);
    Bounds::new(
        Point::new(
            target.origin.x
                + (overview.offset.x + px(overview.scale * world.min_x as f32)) * target_x_scale,
            target.origin.y
                + (overview.offset.y - px(overview.scale * world.max_y as f32)) * target_y_scale,
        ),
        Size::new(
            px(overview.scale * (world.max_x - world.min_x) as f32 * target_x_scale),
            px(overview.scale * (world.max_y - world.min_y) as f32 * target_y_scale),
        ),
    )
}

fn navigation_overview_viewport_bounds(
    overview: ViewportTransform,
    current: ViewportTransform,
    target: Bounds<Pixels>,
) -> Bounds<Pixels> {
    navigation_overview_world_bounds(overview, raster_viewport_world_bounds(current), target)
}

fn navigation_overview_world_point(
    overview: ViewportTransform,
    position: Point<Pixels>,
    target: Bounds<Pixels>,
) -> Point<f32> {
    let x_scale = f32::from(target.size.width) / f32::from(overview.size.width);
    let y_scale = f32::from(target.size.height) / f32::from(overview.size.height);
    let local_x = f32::from(position.x - target.origin.x) / x_scale;
    let local_y = f32::from(position.y - target.origin.y) / y_scale;
    Point::new(
        (local_x - f32::from(overview.offset.x)) / overview.scale,
        (f32::from(overview.offset.y) - local_y) / overview.scale,
    )
}

fn minimum_centered_bounds_size(bounds: Bounds<Pixels>, minimum: Pixels) -> Bounds<Pixels> {
    let width = f32::from(bounds.size.width).max(f32::from(minimum));
    let height = f32::from(bounds.size.height).max(f32::from(minimum));
    let center = bounds.center();
    Bounds::new(
        Point::new(center.x - px(width / 2.), center.y - px(height / 2.)),
        Size::new(px(width), px(height)),
    )
}

fn clamp_bounds_to_container(bounds: Bounds<Pixels>, container: Bounds<Pixels>) -> Bounds<Pixels> {
    let width = bounds.size.width.min(container.size.width);
    let height = bounds.size.height.min(container.size.height);
    Bounds::new(
        Point::new(
            bounds
                .origin
                .x
                .clamp(container.origin.x, container.right() - width),
            bounds
                .origin
                .y
                .clamp(container.origin.y, container.bottom() - height),
        ),
        Size::new(width, height),
    )
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RasterOutlineReprojection {
    /// Restore sparse outline underlays and stroke retained float geometry.
    Exact,
    /// Dense/expensive outlines cannot be transformed without changing their
    /// apparent width or visibility, so produce a fresh exact raster.
    Rerender,
}

fn raster_reprojection_scale_is_safe(source_scale: f32, target_scale: f32) -> bool {
    // Magnifying a coarse fill plane can expose large underlay regions whose
    // internal geometry was not resolvable in the source LOD. Even exact
    // restroking cannot recover those missing shapes, so zoom-in always waits
    // for the BVH-accelerated fresh raster.
    target_scale <= source_scale
}

fn raster_outline_reprojection(
    scale_ratio: f32,
    exact_source_work: Option<usize>,
    destination_pixel_count: usize,
) -> RasterOutlineReprojection {
    let work_limit = destination_pixel_count / OUTLINE_REPROJECTION_WORK_DIVISOR;
    if exact_source_work
        .is_some_and(|work| (work as f32 * scale_ratio.max(0.)).ceil() as usize <= work_limit)
    {
        RasterOutlineReprojection::Exact
    } else {
        RasterOutlineReprojection::Rerender
    }
}

fn raster_zoom_display_transform(
    previous: Option<RasterDisplayTransform>,
    requested: Option<RasterDisplayTransform>,
    has_safe_reprojection: bool,
) -> Option<RasterDisplayTransform> {
    if has_safe_reprojection {
        requested
    } else {
        previous
    }
}

fn mark_raster_outline_pixel(
    mask: &mut [u8],
    touched: &mut Vec<usize>,
    width: u32,
    height: u32,
    x: i32,
    y: i32,
) {
    if x < 0 || x >= width as i32 || y < 0 || y >= height as i32 {
        return;
    }
    let pixel = y as usize * width as usize + x as usize;
    if mask[pixel] == 0 {
        mask[pixel] = 1;
        touched.push(pixel);
    }
}

fn mark_raster_outline_rect(
    mask: &mut [u8],
    touched: &mut Vec<usize>,
    width: u32,
    height: u32,
    bounds: Bounds<Pixels>,
) {
    let Some((x0, x1)) = raster_unclipped_pixel_range(
        f32::from(bounds.origin.x),
        f32::from(bounds.origin.x + bounds.size.width),
    ) else {
        return;
    };
    let Some((y0, y1)) = raster_unclipped_pixel_range(
        f32::from(bounds.origin.y),
        f32::from(bounds.origin.y + bounds.size.height),
    ) else {
        return;
    };
    let bottom = y1 - 1;
    let right = x1 - 1;
    for x in x0.clamp(0, width as i32)..x1.clamp(0, width as i32) {
        mark_raster_outline_pixel(mask, touched, width, height, x, y0);
        if bottom != y0 {
            mark_raster_outline_pixel(mask, touched, width, height, x, bottom);
        }
    }
    for y in (y0 + 1).clamp(0, height as i32)..bottom.clamp(0, height as i32) {
        mark_raster_outline_pixel(mask, touched, width, height, x0, y);
        if right != x0 {
            mark_raster_outline_pixel(mask, touched, width, height, right, y);
        }
    }
}

fn mark_raster_outline_line(
    mask: &mut [u8],
    touched: &mut Vec<usize>,
    width: u32,
    height: u32,
    start: Point<f32>,
    stop: Point<f32>,
) {
    let steps = (stop.x - start.x)
        .abs()
        .max((stop.y - start.y).abs())
        .ceil() as usize;
    for step in 0..=steps {
        let fraction = if steps == 0 {
            0.
        } else {
            step as f32 / steps as f32
        };
        mark_raster_outline_pixel(
            mask,
            touched,
            width,
            height,
            (start.x + fraction * (stop.x - start.x)).round() as i32,
            (start.y + fraction * (stop.y - start.y)).round() as i32,
        );
    }
}

fn raster_outline_candidate_pixels(
    geometry: &RasterOutlineGeometry,
    width: u32,
    height: u32,
) -> Vec<usize> {
    let mut mask = vec![0_u8; width as usize * height as usize];
    let mut pixels = Vec::with_capacity(geometry.source_work.min(mask.len()));
    for primitive in geometry.primitives.iter() {
        match primitive {
            RasterOutlinePrimitive::Rect(bounds) => {
                mark_raster_outline_rect(&mut mask, &mut pixels, width, height, *bounds);
            }
            RasterOutlinePrimitive::Line { start, stop } => {
                mark_raster_outline_line(&mut mask, &mut pixels, width, height, *start, *stop);
            }
        }
    }
    pixels.sort_unstable();
    pixels
}

fn raster_outline_color(
    planes: &RasterStylePlanes,
    correction: &RasterOutlineCorrection,
    source_pixel: usize,
    stipple_on: bool,
) -> Option<[u8; 4]> {
    let sample = raster_outline_sample(correction, source_pixel)?;
    let range = source_pixel * 4..source_pixel * 4 + 4;
    let (completed, underlay) = if stipple_on {
        (&planes.stipple_on[range], sample.underlay_on)
    } else {
        (&planes.stipple_off[range], sample.underlay_off)
    };
    (completed != underlay).then(|| completed.try_into().expect("four BGRA channels"))
}

fn raster_outline_color_in_footprint(
    planes: &RasterStylePlanes,
    correction: &RasterOutlineCorrection,
    source_x0: f32,
    source_x1: f32,
    source_y0: f32,
    source_y1: f32,
    stipple_on: bool,
) -> Option<[u8; 4]> {
    let x0 = source_x0.floor().clamp(0., planes.width as f32) as u32;
    let x1 = source_x1.ceil().clamp(0., planes.width as f32) as u32;
    let y0 = source_y0.floor().clamp(0., planes.height as f32) as u32;
    let y1 = source_y1.ceil().clamp(0., planes.height as f32) as u32;
    let center_x = (source_x0 + source_x1) / 2.;
    let center_y = (source_y0 + source_y1) / 2.;
    let mut closest = None::<(f32, [u8; 4])>;
    for source_y in y0..y1 {
        for source_x in x0..x1 {
            let source_pixel = (source_y * planes.width + source_x) as usize;
            let Some(color) = raster_outline_color(planes, correction, source_pixel, stipple_on)
            else {
                continue;
            };
            let distance = (source_x as f32 + 0.5 - center_x).powi(2)
                + (source_y as f32 + 0.5 - center_y).powi(2);
            if closest.is_none_or(|(closest_distance, _)| distance < closest_distance) {
                closest = Some((distance, color));
            }
        }
    }
    closest.map(|(_, color)| color)
}

fn reproject_raster_tiles(
    tiles: &LayoutRasterTileSet,
    bounds: Bounds<Pixels>,
    scale: f32,
    offset: Point<Pixels>,
) -> Option<LayoutRasterCache> {
    let width = (f32::from(bounds.size.width) * RASTER_CACHE_RESOLUTION)
        .ceil()
        .max(1.) as u32;
    let height = (f32::from(bounds.size.height) * RASTER_CACHE_RESOLUTION)
        .ceil()
        .max(1.) as u32;
    let mut output = vec![0; width as usize * height as usize * 4];
    let stipple_phase = raster_stipple_phase(Point::new(
        offset.x * RASTER_CACHE_RESOLUTION,
        offset.y * RASTER_CACHE_RESOLUTION,
    ));
    let mut texts = Vec::new();
    let mut scope_labels = Vec::new();
    let mut copied_tile = false;

    let visible_tiles = navigation_tile_order(tiles.center)
        .into_iter()
        .filter_map(|index| {
            let cache = tiles.tiles.get(&index)?;
            let target = raster_bounds(cache, bounds, scale, offset);
            target.intersects(&bounds).then_some((cache, target))
        })
        .collect::<Vec<_>>();
    let exact_source_work = visible_tiles.iter().try_fold(0_usize, |work, (cache, _)| {
        let correction = cache.style_planes.as_ref()?.outline_correction.as_ref()?;
        Some(work.saturating_add(correction.geometry.source_work))
    });
    let scale_ratio = scale / tiles.scale;
    let outline_reprojection = raster_outline_reprojection(
        scale_ratio,
        exact_source_work,
        width as usize * height as usize,
    );
    if outline_reprojection == RasterOutlineReprojection::Rerender {
        return None;
    }
    let mut outline_mask = (outline_reprojection == RasterOutlineReprojection::Exact)
        .then(|| vec![0_u8; width as usize * height as usize]);
    let mut outline_touched = Vec::new();

    for (cache, target) in visible_tiles {
        let planes = cache.style_planes.as_ref()?;
        let target_origin = target.origin - bounds.origin;
        let target_x0 = f32::from(target_origin.x) * RASTER_CACHE_RESOLUTION;
        let target_y0 = f32::from(target_origin.y) * RASTER_CACHE_RESOLUTION;
        let target_width = f32::from(target.size.width) * RASTER_CACHE_RESOLUTION;
        let target_height = f32::from(target.size.height) * RASTER_CACHE_RESOLUTION;
        let Some((x0, x1)) = raster_pixel_range(target_x0, target_x0 + target_width, width) else {
            continue;
        };
        let Some((y0, y1)) = raster_pixel_range(target_y0, target_y0 + target_height, height)
        else {
            continue;
        };
        if target_width <= 0. || target_height <= 0. {
            continue;
        }
        copied_tile = true;
        texts.extend(cache.texts.iter().cloned());
        scope_labels.extend(cache.scope_labels.iter().cloned());
        for y in y0..y1 {
            let source_y = (((y as f32 + 0.5 - target_y0) / target_height) * planes.height as f32)
                .floor()
                .clamp(0., planes.height as f32 - 1.) as u32;
            for x in x0..x1 {
                let source_x = (((x as f32 + 0.5 - target_x0) / target_width) * planes.width as f32)
                    .floor()
                    .clamp(0., planes.width as f32 - 1.) as u32;
                let source_pixel = (source_y * planes.width + source_x) as usize;
                let destination_pixel = (y * width + x) as usize;
                let stipple_on = raster_stipple_is_on(x, y, stipple_phase);
                let source = if stipple_on {
                    &planes.stipple_on
                } else {
                    &planes.stipple_off
                };
                let source_channels = if outline_reprojection == RasterOutlineReprojection::Exact
                    && let Some(sample) = planes
                        .outline_correction
                        .as_ref()
                        .and_then(|correction| raster_outline_sample(correction, source_pixel))
                {
                    if stipple_on {
                        &sample.underlay_on
                    } else {
                        &sample.underlay_off
                    }
                } else {
                    source[source_pixel * 4..source_pixel * 4 + 4]
                        .try_into()
                        .expect("four BGRA channels")
                };
                output[destination_pixel * 4..destination_pixel * 4 + 4]
                    .copy_from_slice(source_channels);
            }
        }

        if outline_reprojection == RasterOutlineReprojection::Exact {
            let correction = planes.outline_correction.as_ref()?;
            let mask = outline_mask.as_mut().expect("exact outline mask");
            let scale_x = target_width / planes.width as f32;
            let scale_y = target_height / planes.height as f32;
            for primitive in correction.geometry.primitives.iter() {
                match primitive {
                    RasterOutlinePrimitive::Rect(source) => mark_raster_outline_rect(
                        mask,
                        &mut outline_touched,
                        width,
                        height,
                        Bounds::new(
                            Point::new(
                                px(target_x0 + f32::from(source.origin.x) * scale_x),
                                px(target_y0 + f32::from(source.origin.y) * scale_y),
                            ),
                            Size::new(source.size.width * scale_x, source.size.height * scale_y),
                        ),
                    ),
                    RasterOutlinePrimitive::Line { start, stop } => mark_raster_outline_line(
                        mask,
                        &mut outline_touched,
                        width,
                        height,
                        Point::new(target_x0 + start.x * scale_x, target_y0 + start.y * scale_y),
                        Point::new(target_x0 + stop.x * scale_x, target_y0 + stop.y * scale_y),
                    ),
                }
            }
            for destination_pixel in outline_touched.drain(..) {
                mask[destination_pixel] = 0;
                let x = destination_pixel % width as usize;
                let y = destination_pixel / width as usize;
                if x < x0 as usize || x >= x1 as usize || y < y0 as usize || y >= y1 as usize {
                    continue;
                }
                let source_x0 = ((x as f32 - target_x0) / target_width) * planes.width as f32;
                let source_x1 = ((x as f32 + 1. - target_x0) / target_width) * planes.width as f32;
                let source_y0 = ((y as f32 - target_y0) / target_height) * planes.height as f32;
                let source_y1 =
                    ((y as f32 + 1. - target_y0) / target_height) * planes.height as f32;
                let Some(color) = raster_outline_color_in_footprint(
                    planes,
                    correction,
                    source_x0,
                    source_x1,
                    source_y0,
                    source_y1,
                    raster_stipple_is_on(x as u32, y as u32, stipple_phase),
                ) else {
                    continue;
                };
                output[destination_pixel * 4..destination_pixel * 4 + 4].copy_from_slice(&color);
            }
        }
    }
    if !copied_tile {
        return None;
    }

    let image = expand_raster_for_display(image::RgbaImage::from_raw(width, height, output)?);
    Some(LayoutRasterCache {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(image)])),
        style_planes: None,
        texts: texts.into(),
        scope_labels: scope_labels.into(),
        viewport: raster_logical_size(width, height),
        screen_viewport: bounds.size,
        scale,
        offset,
        content_revision: tiles.content_revision,
    })
}

fn navigation_tile_size(screen_size: Size<Pixels>) -> Size<Pixels> {
    raster_logical_size(
        (f32::from(screen_size.width) * RASTER_CACHE_RESOLUTION)
            .ceil()
            .max(1.) as u32,
        (f32::from(screen_size.height) * RASTER_CACHE_RESOLUTION)
            .ceil()
            .max(1.) as u32,
    )
}

fn navigation_tile_order(center: RasterTileIndex) -> Vec<RasterTileIndex> {
    let mut tiles = Vec::with_capacity(((NAVIGATION_TILE_RADIUS * 2 + 1).pow(2)) as usize);
    for radius in 0..=NAVIGATION_TILE_RADIUS {
        for y in -radius..=radius {
            for x in -radius..=radius {
                if x.abs().max(y.abs()) == radius {
                    tiles.push(RasterTileIndex {
                        x: center.x + x,
                        y: center.y + y,
                    });
                }
            }
        }
    }
    tiles
}

fn navigation_inner_tiles_complete(
    tiles: &HashMap<RasterTileIndex, LayoutRasterCache>,
    center: RasterTileIndex,
) -> bool {
    (-1..=1).all(|y| {
        (-1..=1).all(|x| {
            tiles.contains_key(&RasterTileIndex {
                x: center.x + x,
                y: center.y + y,
            })
        })
    })
}

fn raster_tile_offset(
    anchor_offset: Point<Pixels>,
    tile_size: Size<Pixels>,
    index: RasterTileIndex,
) -> Point<Pixels> {
    anchor_offset
        - Point::new(
            tile_size.width * index.x as f32,
            tile_size.height * index.y as f32,
        )
}

fn raster_tile_set_matches_target(tiles: &LayoutRasterTileSet, target: RasterTileTarget) -> bool {
    tiles.anchor_offset == target.anchor_offset
        && tiles.tile_size == target.tile_size
        && tiles.screen_viewport == target.screen_viewport
        && tiles.scale == target.scale
        && tiles.content_revision == target.content_revision
}

fn single_raster_tile_set(cache: LayoutRasterCache) -> LayoutRasterTileSet {
    let anchor_offset = cache.offset;
    let tile_size = cache.viewport;
    let screen_viewport = cache.screen_viewport;
    let scale = cache.scale;
    let content_revision = cache.content_revision;
    LayoutRasterTileSet {
        tiles: HashMap::from([(RasterTileIndex { x: 0, y: 0 }, cache)]),
        navigation: false,
        anchor_offset,
        tile_size,
        screen_viewport,
        scale,
        content_revision,
        center: RasterTileIndex { x: 0, y: 0 },
    }
}

fn raster_tiles_cover_bounds(
    tiles: &LayoutRasterTileSet,
    canvas: Bounds<Pixels>,
    required: Bounds<Pixels>,
    scale: f32,
    offset: Point<Pixels>,
) -> bool {
    if tiles.tiles.is_empty() || tiles.scale <= 0. {
        return false;
    }
    let ratio = scale / tiles.scale;
    let tile_width = f32::from(tiles.tile_size.width) * ratio;
    let tile_height = f32::from(tiles.tile_size.height) * ratio;
    if tile_width <= 0. || tile_height <= 0. {
        return false;
    }
    let base = Point::new(
        canvas.origin.x + offset.x - tiles.anchor_offset.x * ratio,
        canvas.origin.y + offset.y - tiles.anchor_offset.y * ratio,
    );
    let required_bottom_right = required.bottom_right();
    let min_x = ((f32::from(required.origin.x) - f32::from(base.x)) / tile_width).floor() as i32;
    let min_y = ((f32::from(required.origin.y) - f32::from(base.y)) / tile_height).floor() as i32;
    let max_x = (((f32::from(required_bottom_right.x) - f32::from(base.x)) / tile_width) - 1e-4)
        .ceil() as i32
        - 1;
    let max_y = (((f32::from(required_bottom_right.y) - f32::from(base.y)) / tile_height) - 1e-4)
        .ceil() as i32
        - 1;
    (min_y..=max_y)
        .all(|y| (min_x..=max_x).all(|x| tiles.tiles.contains_key(&RasterTileIndex { x, y })))
}

fn navigation_raster_capture_transform(tiles: &LayoutRasterTileSet) -> RasterDisplayTransform {
    RasterDisplayTransform {
        scale: tiles.scale,
        offset: raster_tile_offset(tiles.anchor_offset, tiles.tile_size, tiles.center),
    }
}

fn layout_bbox_screen_bounds(
    bbox: &compile::Rect<f64>,
    canvas: Bounds<Pixels>,
    scale: f32,
    offset: Point<Pixels>,
) -> Bounds<Pixels> {
    let x0 = bbox.x0.min(bbox.x1) as f32;
    let x1 = bbox.x0.max(bbox.x1) as f32;
    let y0 = bbox.y0.min(bbox.y1) as f32;
    let y1 = bbox.y0.max(bbox.y1) as f32;
    Bounds::new(
        Point::new(
            canvas.origin.x + offset.x + px(scale * x0),
            canvas.origin.y + offset.y - px(scale * y1),
        ),
        Size::new(px(scale * (x1 - x0)), px(scale * (y1 - y0))),
    )
}

/// Return a congruent display transform while the full overscanned image still
/// covers the canvas, or while every drawable layout coordinate is still
/// represented by a retained tile. The latter permits unlimited shrinking of
/// a fully captured layout: newly exposed pixels are known-empty background.
fn navigation_raster_transform(
    tiles: &LayoutRasterTileSet,
    canvas: Bounds<Pixels>,
    scale: f32,
    offset: Point<Pixels>,
    layout_bounds: Option<Bounds<Pixels>>,
) -> Option<RasterDisplayTransform> {
    let covers_canvas = raster_tiles_cover_bounds(tiles, canvas, canvas, scale, offset);
    let covers_layout = layout_bounds
        .is_some_and(|bounds| raster_tiles_cover_bounds(tiles, canvas, bounds, scale, offset));
    (covers_canvas || covers_layout).then_some(RasterDisplayTransform { scale, offset })
}

fn paint_should_start_navigation_worker(
    has_solved_layout: bool,
    raster_matches_viewport: bool,
    raster_worker_active: bool,
) -> bool {
    has_solved_layout && !raster_matches_viewport && !raster_worker_active
}

fn sort_initial_condition_pair(
    updates: &mut [InitialConditionUpdate],
    low_index: usize,
    high_index: usize,
) {
    if let Some((low, high)) =
        sorted_initial_condition_values(updates[low_index].value, updates[high_index].value)
    {
        updates[low_index].value = low;
        updates[high_index].value = high;
        updates[low_index].changed = true;
        updates[high_index].changed = true;
    }
}

fn fallback_value_edits(
    fallbacks: &[compile::UsedFallback],
    dv: &SparseVec,
    grid: f64,
) -> Vec<ValueEdit> {
    let mut updates = fallbacks
        .iter()
        .map(|fallback| {
            let (value, changed) =
                crate::sse::initial_condition_after_drag(&fallback.constraint, dv);
            InitialConditionUpdate {
                span: fallback.span.clone(),
                value,
                changed,
                target: fallback.initial_condition,
            }
        })
        .collect::<Vec<_>>();

    // Rectangle edges exchange their initial values when dragged across one
    // another. Polygon coordinates remain attached to their vertex index.
    let mut pairs: HashMap<ObjectId, [Option<usize>; 4]> = HashMap::new();
    for (index, update) in updates.iter().enumerate() {
        let Some(target) = update.target else {
            continue;
        };
        let (id, edge) = match target {
            RectInitialCondition::X0(id) => (id, 0),
            RectInitialCondition::X1(id) => (id, 1),
            RectInitialCondition::Y0(id) => (id, 2),
            RectInitialCondition::Y1(id) => (id, 3),
            RectInitialCondition::PolygonX(_, _)
            | RectInitialCondition::PolygonY(_, _)
            | RectInitialCondition::PathX(_, _)
            | RectInitialCondition::PathY(_, _)
            | RectInitialCondition::PathWidth(_)
            | RectInitialCondition::PathBeginExtension(_)
            | RectInitialCondition::PathEndExtension(_)
            | RectInitialCondition::InstanceX(_)
            | RectInitialCondition::InstanceY(_) => {
                continue;
            }
        };
        pairs.entry(id).or_insert([None; 4])[edge] = Some(index);
    }
    for pair in pairs.values() {
        if let (Some(x0), Some(x1)) = (pair[0], pair[1]) {
            sort_initial_condition_pair(&mut updates, x0, x1);
        }
        if let (Some(y0), Some(y1)) = (pair[2], pair[3]) {
            sort_initial_condition_pair(&mut updates, y0, y1);
        }
    }

    updates
        .into_iter()
        .filter(|update| update.changed)
        .map(|update| ValueEdit {
            span: update.span,
            value: crate::sse::format_value(update.value, grid),
        })
        .collect()
}

#[derive(Default)]
struct DragPersistenceEdits {
    values: Vec<ValueEdit>,
    initial_conditions: Vec<InitialConditionEdit>,
}

fn drag_persistence_edits(
    fallbacks: &[compile::UsedFallback],
    targets: &[SseDragTarget],
    dv: &SparseVec,
    grid: f64,
) -> DragPersistenceEdits {
    let values = fallback_value_edits(fallbacks, dv, grid);
    let mut initial_conditions = Vec::<InitialConditionEdit>::new();
    for target in targets {
        let delta = crate::sse::dot(&SparseVec::from(&target.expr), dv);
        if delta.abs() < crate::sse::EPSILON {
            continue;
        }
        let Some(source) = &target.source else {
            continue;
        };
        let edit = InitialConditionEdit {
            call_span: source.call_span.clone(),
            name: source.name.clone(),
            value: crate::sse::format_value(source.value + delta, grid),
        };
        if let Some(existing) = initial_conditions
            .iter_mut()
            .find(|existing| existing.call_span == edit.call_span && existing.name == edit.name)
        {
            existing.value = edit.value;
        } else {
            initial_conditions.push(edit);
        }
    }
    DragPersistenceEdits {
        values,
        initial_conditions,
    }
}

fn sorted_initial_condition_values(low: f64, high: f64) -> Option<(f64, f64)> {
    (low > high).then_some((high, low))
}

/// Maps a selected object's source span through the value replacements used to
/// persist an SSE drag. Compiler object IDs are regenerated, so retaining the
/// adjusted span is what lets the next compile result recognize the same rect.
fn remap_span_after_value_edits(selected: &Span, edits: &[ValueEdit]) -> Span {
    let selected_start = selected.span.start();
    let selected_end = selected.span.end();
    let mut shift_before = 0isize;
    let mut shift_within = 0isize;

    for edit in edits.iter().filter(|edit| edit.span.path == selected.path) {
        let old_start = edit.span.span.start();
        let old_end = edit.span.span.end();
        let delta = edit.value.len() as isize - (old_end - old_start) as isize;
        if old_end <= selected_start {
            shift_before += delta;
        } else if old_start < selected_end {
            shift_within += delta;
        }
    }

    let shifted_start = (selected_start as isize + shift_before) as usize;
    let shifted_end = (selected_end as isize + shift_before + shift_within) as usize;
    Span {
        path: selected.path.clone(),
        span: cfgrammar::Span::new(shifted_start, shifted_end),
    }
}

fn pending_sse_values(
    edits: &[ValueEdit],
    initial_conditions: &[InitialConditionEdit],
) -> Vec<PendingSseValue> {
    let mut pending = edits
        .iter()
        .filter_map(|edit| {
            Some(PendingSseValue {
                span: Some(remap_span_after_value_edits(&edit.span, edits)),
                value: edit.value.parse().ok()?,
            })
        })
        .collect::<Vec<_>>();
    // Missing initial-condition kwargs are returned by the analyzer as one
    // aggregate insertion edit, whose text is not itself a number. Retain the
    // requested values without a source span so an unrelated newer compile
    // cannot dismiss the optimistic drag preview before those kwargs compile.
    pending.extend(
        initial_conditions
            .iter()
            .filter_map(|condition| condition.value.parse().ok())
            .map(|value| PendingSseValue { span: None, value }),
    );
    pending
}

fn compiled_data_matches_pending_sse(data: &CompiledData, pending: &[PendingSseValue]) -> bool {
    !pending.is_empty()
        && pending.iter().all(|expected| {
            data.cells
                .values()
                .flat_map(|cell| &cell.fallback_constraints_used)
                .any(|fallback| {
                    expected
                        .span
                        .as_ref()
                        .is_none_or(|span| &fallback.span == span)
                        && (-fallback.constraint.constant - expected.value).abs() < 1e-8
                })
        })
}

fn snapshot_follows_revision(snapshot_revision: u64, revision: Option<u64>) -> bool {
    revision.is_none_or(|revision| snapshot_revision > revision)
}

fn solved_linear_after_drag(field: &(f64, LinearExpr), drag: Option<&SparseVec>) -> f64 {
    field.0
        + drag
            .map(|dv| crate::sse::dot(&SparseVec::from(&field.1), dv))
            .unwrap_or_default()
}

fn zoomed_scale(scale: f32, wheel_delta: f32) -> f32 {
    let scale = scale * (wheel_delta / 400.).exp();
    if scale.is_finite() {
        scale.clamp(f32::MIN_POSITIVE, 100.)
    } else {
        100.
    }
}

fn snap_layout_point(point: Point<f32>, grid: f64) -> Point<f32> {
    Point::new(
        argonc::tech::snap(f64::from(point.x), grid) as f32,
        argonc::tech::snap(f64::from(point.y), grid) as f32,
    )
}

fn fit_scale(viewport: Size<Pixels>, width: f32, height: f32) -> f32 {
    let width_scale = (width > 0.).then(|| f32::from(viewport.width) / width);
    let height_scale = (height > 0.).then(|| f32::from(viewport.height) / height);
    let scale = match (width_scale, height_scale) {
        (Some(width), Some(height)) => 0.9 * width.min(height),
        (Some(width), None) => 0.9 * width,
        (None, Some(height)) => 0.9 * height,
        (None, None) => 1.,
    };
    if scale.is_finite() && scale > 0. {
        scale
    } else {
        1.
    }
}

fn offset_after_horizontal_reflow(
    offset: Point<Pixels>,
    previous_bounds: Bounds<Pixels>,
    next_bounds: Bounds<Pixels>,
) -> Point<Pixels> {
    if previous_bounds.size.width <= px(0.) || previous_bounds.size.height <= px(0.) {
        return offset;
    }
    Point::new(
        offset.x + previous_bounds.origin.x - next_bounds.origin.x,
        offset.y,
    )
}

/// Flatten the solved geometry of one compiled cell into rectangles relative
/// to that cell's origin. Placement paints these as a single pointer-following
/// outline without disturbing the layout currently open in the editor.
fn instance_preview_geometry(output: &CompiledData, cell: CellId) -> (Vec<Rect>, Vec<Polygon>) {
    let mut rects = Vec::new();
    let mut polygons = Vec::new();
    let mut queue = VecDeque::from_iter([(
        cell,
        output.cells[&cell].root,
        TransformationMatrix::identity(),
        (0., 0.),
    )]);

    while let Some((cell, scope, mat, ofs)) = queue.pop_front() {
        let compiled_cell = &output.cells[&cell];
        let compiled_scope = &compiled_cell.scopes[&scope];
        let mut emitted = HashSet::new();
        for (object, _) in &compiled_scope.emit {
            if !emitted.insert(*object) {
                continue;
            }
            match &compiled_cell.objects[object] {
                SolvedValue::Rect(rect) if !rect.construction => {
                    let p0 = ifmatvec(mat, (rect.x0.0, rect.y0.0));
                    let p1 = ifmatvec(mat, (rect.x1.0, rect.y1.0));
                    rects.push(Rect {
                        x0: (p0.0.min(p1.0) + ofs.0) as f32,
                        y0: (p0.1.min(p1.1) + ofs.1) as f32,
                        x1: (p0.0.max(p1.0) + ofs.0) as f32,
                        y1: (p0.1.max(p1.1) + ofs.1) as f32,
                        id: None,
                        object_path: Vec::new(),
                        border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                        border_styles: Edges::all(BorderStyle::Dashed),
                        cvars: None,
                    });
                }
                SolvedValue::Polygon(polygon) => {
                    polygons.push(Polygon {
                        points: polygon
                            .points
                            .iter()
                            .map(|(x, y)| {
                                let point = ifmatvec(mat, (x.0, y.0));
                                Point::new((point.0 + ofs.0) as f32, (point.1 + ofs.1) as f32)
                            })
                            .collect(),
                        edge_styles: vec![BorderStyle::Dashed; polygon.points.len()],
                        id: None,
                        object_path: Vec::new(),
                        cvars: None,
                        centerline: None,
                    });
                }
                SolvedValue::Path(path) => {
                    if let Some(outline) = path.outline() {
                        let point_count = outline.len();
                        polygons.push(Polygon {
                            points: outline
                                .into_iter()
                                .map(|point| {
                                    let point = ifmatvec(mat, point);
                                    Point::new((point.0 + ofs.0) as f32, (point.1 + ofs.1) as f32)
                                })
                                .collect(),
                            edge_styles: vec![BorderStyle::Dashed; point_count],
                            id: None,
                            object_path: Vec::new(),
                            cvars: None,
                            centerline: None,
                        });
                    }
                }
                SolvedValue::Instance(instance) if !instance.construction => {
                    let mut instance_mat = TransformationMatrix::identity();
                    if instance.reflect {
                        instance_mat = instance_mat.reflect_vert();
                    }
                    instance_mat = instance_mat.rotate(instance.angle);
                    let instance_ofs = ifmatvec(mat, (instance.x, instance.y));
                    let child = instance.cell;
                    queue.push_back((
                        child,
                        output.cells[&child].root,
                        mat * instance_mat,
                        (instance_ofs.0 + ofs.0, instance_ofs.1 + ofs.1),
                    ));
                }
                _ => {}
            }
        }
        for child in &compiled_scope.children {
            queue.push_back((cell, *child, mat, ofs));
        }
    }
    (rects, polygons)
}

fn get_paint_quad(
    bounds: Bounds<Pixels>,
    fill: ShapeFill,
    color: Rgba,
    border_color: Rgba,
    border_widths: Edges<Pixels>,
    border_styles: Edges<BorderStyle>,
) -> PaintQuad {
    let bounds = Bounds::new(
        Point::new(
            bounds.origin.x - border_widths.left / 2.,
            bounds.origin.y - border_widths.top / 2.,
        ),
        Size::new(
            bounds.size.width + (border_widths.left + border_widths.right) / 2.,
            bounds.size.height + (border_widths.top + border_widths.bottom) / 2.,
        ),
    );
    let background = match fill {
        ShapeFill::Solid => solid_background(color),
        ShapeFill::Stippling => pattern_slash(color.into(), 1., 9.),
        ShapeFill::Hollow => solid_background(Rgba { a: 0., ..color }),
    };
    PaintQuad {
        bounds,
        corner_radii: Corners::all(px(0.)),
        background,
        border_widths,
        border_color: border_color.into(),
        border_styles,
    }
}

impl Element for CanvasElement {
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
        window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) -> (gpui::LayoutId, Self::RequestLayoutState) {
        let inner = self.inner.read(cx);
        let layout_id = window.request_layout(inner.bg_style.clone(), [], cx);
        (layout_id, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        _bounds: gpui::Bounds<gpui::Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut gpui::Window,
        _cx: &mut gpui::App,
    ) -> Self::PrepaintState {
    }

    fn paint(
        &mut self,
        _id: Option<&gpui::GlobalElementId>,
        _inspector_id: Option<&gpui::InspectorElementId>,
        bounds: gpui::Bounds<gpui::Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut gpui::Window,
        cx: &mut gpui::App,
    ) {
        let stale_images = self.inner.update(cx, |inner, _| {
            std::mem::take(&mut inner.raster_images_to_drop)
        });
        for image in stale_images {
            cx.drop_image(image, Some(window));
        }
        self.inner.update(cx, |inner, cx| {
            inner.offset =
                offset_after_horizontal_reflow(inner.offset, inner.screen_bounds, bounds);
            inner.screen_bounds = bounds;
            inner.update_raster_display_transform();
            if inner.pending_init {
                inner.pending_init = false;
                inner.fit_to_screen(cx);
            }
            let has_solved_layout = inner.state.read(cx).solved_cell.read(cx).is_some();
            let raster_matches_viewport = inner
                .raster_tiles
                .as_ref()
                .is_some_and(|tiles| tiles.screen_viewport == bounds.size);
            // A paint notification is also emitted after each background tile
            // finishes or is cancelled. Retargeting an already-running first
            // tile here advances its generation, causing that tile to cancel
            // and notify again forever. Navigation input still explicitly
            // retargets the worker when the viewport actually changes.
            if paint_should_start_navigation_worker(
                has_solved_layout,
                raster_matches_viewport,
                inner.raster_worker_active,
            ) {
                inner.request_navigation_raster(cx);
            }
        });
        let inner = self.inner.read(cx);
        let solved_cell = &inner.state.read(cx).solved_cell.read(cx);
        let hide_external_geometry = &inner.state.read(cx).hide_external_geometry;
        let state = inner.state.read(cx);
        let tool = state.tool.read(cx).clone();
        let select_overview = matches!(
            &tool,
            ToolState::Select(SelectToolState { selected_obj: None })
        );
        let use_raster_cache = inner.navigation_cache_active
            || select_overview
            || matches!(&tool, ToolState::DrawRect(_) | ToolState::PlaceInstance(_));
        let raster_presentation = inner
            .raster_reprojection
            .clone()
            .filter(|cache| cache.content_revision == inner.raster_content_revision)
            .map(|cache| {
                let display = RasterDisplayTransform {
                    scale: cache.scale,
                    offset: cache.offset,
                };
                (single_raster_tile_set(cache), display)
            })
            .or_else(|| {
                inner
                    .raster_tiles
                    .clone()
                    .filter(|tiles| tiles.content_revision == inner.raster_content_revision)
                    .map(|tiles| {
                        let display = inner
                            .raster_display
                            .unwrap_or_else(|| navigation_raster_capture_transform(&tiles));
                        (tiles, display)
                    })
            });
        if use_raster_cache && let Some((tiles, display)) = raster_presentation {
            let theme = state.theme();
            let bg_style = inner.bg_style.clone();
            let scale = display.scale;
            let offset = display.offset;
            let origin_coords = offset + bounds.origin;
            let navigation_cache_active = inner.navigation_cache_active;
            let hover_hit = (!navigation_cache_active)
                .then(|| inner.hover_hit.clone())
                .flatten();
            let layout_mouse_position = inner.px_to_layout(inner.mouse_position);
            let grid = solved_cell
                .as_ref()
                .map(|cell| cell.output.tech.grid_step())
                .unwrap_or(0.1);
            let snapped_layout_mouse_position = snap_layout_point(layout_mouse_position, grid);
            let draw_rect_preview =
                if let ToolState::DrawRect(DrawRectToolState { p0: Some(p0) }) = &tool {
                    let layers = state.layers.read(cx);
                    layers
                        .selected_layer
                        .as_ref()
                        .and_then(|name| layers.layers.get(name))
                        .filter(|layer| layer.visible)
                        .map(|layer| {
                            (
                                Rect {
                                    object_path: Vec::new(),
                                    x0: p0.x.min(snapped_layout_mouse_position.x),
                                    y0: p0.y.min(snapped_layout_mouse_position.y),
                                    x1: p0.x.max(snapped_layout_mouse_position.x),
                                    y1: p0.y.max(snapped_layout_mouse_position.y),
                                    id: None,
                                    border_widths: Edges::all(SELECT_WIDTH),
                                    border_styles: Edges::all(BorderStyle::Dashed),
                                    cvars: None,
                                },
                                layer.clone(),
                            )
                        })
                } else {
                    None
                };
            bg_style.paint(bounds, window, cx, |window, cx| {
                window.paint_layer(bounds, |window| {
                    window.paint_quad(get_paint_path(
                        Bounds::new(
                            Point::new(origin_coords.x, bounds.origin.y),
                            Size::new(px(0.), bounds.size.height),
                        ),
                        theme.axes,
                        DEFAULT_BORDER_WIDTH,
                    ));
                    window.paint_quad(get_paint_path(
                        Bounds::new(
                            Point::new(bounds.origin.x, origin_coords.y),
                            Size::new(bounds.size.width, px(0.)),
                        ),
                        theme.axes,
                        DEFAULT_BORDER_WIDTH,
                    ));
                    let visible_tiles = navigation_tile_order(tiles.center)
                        .into_iter()
                        .filter_map(|index| tiles.tiles.get(&index))
                        .filter(|cache| {
                            raster_bounds(cache, bounds, scale, offset).intersects(&bounds)
                        })
                        .collect::<Vec<_>>();
                    for cache in &visible_tiles {
                        window
                            .paint_image(
                                raster_bounds(cache, bounds, scale, offset),
                                Corners::all(px(0.)),
                                cache.image.clone(),
                                0,
                                false,
                            )
                            .unwrap();
                    }
                    if let Some((font_size, line_height)) =
                        layout_text_metrics(scale, TEXT_LAYOUT_SIZE)
                    {
                        let mut painted = HashSet::new();
                        for label in visible_tiles.iter().flat_map(|cache| cache.texts.iter()) {
                            let key = (
                                label.position.x.to_bits(),
                                label.position.y.to_bits(),
                                label.layer.z,
                                label.text.clone(),
                            );
                            if !painted.insert(key) {
                                continue;
                            }
                            let runs = &[TextRun {
                                len: label.text.len(),
                                font: window.text_style().font(),
                                color: label.layer.border_color.into(),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }];
                            window
                                .text_system()
                                .shape_line(label.text.clone(), font_size, runs, None)
                                .paint(
                                    Point::new(
                                        scale * px(label.position.x),
                                        scale * px(-label.position.y),
                                    ) + offset
                                        + bounds.origin,
                                    line_height,
                                    window,
                                    cx,
                                )
                                .unwrap();
                        }
                    }
                    if let Some((font_size, line_height)) =
                        layout_text_metrics(scale, SCOPE_TEXT_LAYOUT_SIZE)
                    {
                        let mut painted = HashSet::new();
                        for bbox in visible_tiles
                            .iter()
                            .flat_map(|cache| cache.scope_labels.iter())
                        {
                            let key = (
                                bbox.rect.x0.to_bits(),
                                bbox.rect.y0.to_bits(),
                                bbox.rect.x1.to_bits(),
                                bbox.rect.y1.to_bits(),
                                bbox.label.clone(),
                            );
                            if !painted.insert(key) {
                                continue;
                            }
                            let text_origin = get_rect_bounds(&bbox.rect, bounds, scale, offset)
                                .origin
                                + Point::new(px(4.), px(2.));
                            let runs = &[TextRun {
                                len: bbox.label.len(),
                                font: window.text_style().font(),
                                color: theme.text.into(),
                                background_color: None,
                                underline: None,
                                strikethrough: None,
                            }];
                            window
                                .text_system()
                                .shape_line(bbox.label.clone(), font_size, runs, None)
                                .paint(text_origin, line_height, window, cx)
                                .unwrap();
                        }
                    }
                    if let Some((preview, layer)) = &draw_rect_preview {
                        window.paint_quad(get_paint_quad(
                            get_rect_bounds(preview, bounds, scale, offset),
                            layer.fill,
                            layer.color,
                            rgb(0xffff00),
                            preview.border_widths,
                            preview.border_styles,
                        ));
                    }
                    if let ToolState::PlaceInstance(placement) = &tool {
                        let translation = (
                            snapped_layout_mouse_position.x as f64,
                            snapped_layout_mouse_position.y as f64,
                        );
                        for rect in &placement.rects {
                            let rect =
                                rect.transform(TransformationMatrix::identity(), translation);
                            window.paint_quad(get_paint_quad(
                                get_rect_bounds(&rect, bounds, scale, offset),
                                ShapeFill::Solid,
                                Rgba {
                                    a: 0.,
                                    ..rgb(0xffff00)
                                },
                                rgb(0xffff00),
                                rect.border_widths,
                                rect.border_styles,
                            ));
                        }
                        for polygon in &placement.polygons {
                            let polygon =
                                polygon.transform(TransformationMatrix::identity(), translation);
                            let points = polygon
                                .points
                                .iter()
                                .map(|point| {
                                    Point::new(scale * px(point.x), scale * px(-point.y))
                                        + offset
                                        + bounds.origin
                                })
                                .collect::<Vec<_>>();
                            let mut border = PathBuilder::stroke(DEFAULT_BORDER_WIDTH);
                            border.add_polygon(&points, true);
                            if let Ok(path) = border.build() {
                                window.paint_path(path, rgb(0xffff00));
                            }
                        }
                    }
                    if let Some(hit) = hover_hit {
                        match hit.outline {
                            SelectionOutline::Rect {
                                bounds,
                                border_styles,
                            } => window.paint_quad(get_paint_quad(
                                bounds,
                                ShapeFill::Solid,
                                Rgba {
                                    a: 0.,
                                    ..rgb(0xffff00)
                                },
                                rgb(0xffff00),
                                Edges::all(SELECT_WIDTH),
                                border_styles,
                            )),
                            SelectionOutline::Polygon {
                                points,
                                edge_styles,
                            } => paint_polygon_border(
                                window,
                                &points,
                                &edge_styles,
                                SELECT_WIDTH,
                                rgb(0xffff00),
                            ),
                            SelectionOutline::Polyline {
                                points,
                                segment_styles,
                            } => paint_polyline(
                                window,
                                &points,
                                &segment_styles,
                                SELECT_WIDTH,
                                rgb(0xffff00),
                            ),
                        }
                    }
                });
            });
            return;
        }
        if inner.navigation_cache_active && inner.raster_tiles.is_none() {
            // The first flattened raster is built off the UI thread. Avoid a
            // synchronous hierarchy traversal (and its temporary omissions)
            // while it is in flight.
            let bg_style = inner.bg_style.clone();
            bg_style.paint(bounds, window, cx, |_window, _cx| {});
            return;
        }
        let layers = state.layers.read(cx);
        let mut sse_dv = None;

        // TODO: Clean up code.
        let mut rects = Vec::new();
        let mut texts = Vec::new();
        let mut polygons = Vec::new();
        let mut dims = Vec::new();
        let mut scope_rects = Vec::new();
        let mut instance_sse_candidates = Vec::new();
        let mut select_rects = Vec::new();
        let mut source_coordinates = SseSourceCoordinates::new();
        let layout_mouse_position = inner.px_to_layout(inner.mouse_position);
        let grid = solved_cell
            .as_ref()
            .map(|cell| cell.output.tech.grid_step())
            .unwrap_or(0.1);
        let snapped_layout_mouse_position = snap_layout_point(layout_mouse_position, grid);
        if let Some(solved_cell) = solved_cell {
            let scope_address = &solved_cell.state[&solved_cell.selected_scope].address;
            let editable_cell = &solved_cell.output.cells[&scope_address.cell];
            if inner.is_sse_dragging || inner.is_sse_persisting {
                sse_dv = inner.sse_drag_delta(editable_cell);
            }
            let mut queue = VecDeque::from_iter([(
                ScopeAddress {
                    cell: scope_address.cell,
                    scope: if *hide_external_geometry {
                        scope_address.scope
                    } else {
                        solved_cell.output.cells[&scope_address.cell].root
                    },
                },
                TransformationMatrix::identity(),
                (0., 0.),
                0,
                true,
                vec![],
            )]);
            dims.extend(
                solved_cell.output.cells[&scope_address.cell]
                    .objects
                    .values()
                    .filter_map(|obj| obj.get_dimension().cloned()),
            );
            while let Some((
                curr_address @ ScopeAddress { scope, cell },
                mat,
                ofs,
                depth,
                show,
                path,
            )) = queue.pop_front()
            {
                let cell_info = &solved_cell.output.cells[&cell];
                let scope_info = &cell_info.scopes[&scope];
                let scope_state = &solved_cell.state[&solved_cell.scope_paths[&curr_address]];
                if let Some(bbox) = &scope_state.bbox {
                    let p0p = ifmatvec(mat, (bbox.x0, bbox.y0));
                    let p1p = ifmatvec(mat, (bbox.x1, bbox.y1));
                    let rect = Rect {
                        x0: (p0p.0.min(p1p.0) + ofs.0) as f32,
                        y0: (p0p.1.min(p1p.1) + ofs.1) as f32,
                        x1: (p0p.0.max(p1p.0) + ofs.0) as f32,
                        y1: (p0p.1.max(p1p.1) + ofs.1) as f32,
                        id: Some(scope_info.span.clone()),
                        object_path: Vec::new(),
                        border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                        border_styles: Edges::all(BorderStyle::Solid),
                        cvars: None,
                    };
                    let pixel_bounds = get_rect_bounds(&rect, bounds, inner.scale, inner.offset);
                    if depth > 0 && !pixel_bounds.intersects(&bounds) {
                        continue;
                    }
                    if depth >= state.hierarchy_depth || !scope_state.visible {
                        if let ToolState::Select(SelectToolState { selected_obj }) = &tool
                            && &rect.id == selected_obj
                        {
                            select_rects.push(Rect {
                                border_widths: Edges::all(SELECT_WIDTH),
                                ..rect.clone()
                            });
                        }
                        if show {
                            scope_rects.push(LabeledBbox {
                                rect,
                                label: scope_info.name.clone().into(),
                                origin: None,
                            });
                        }
                        continue;
                    }
                }
                for (obj, _) in &scope_info.emit {
                    let mut object_path = path.clone();
                    object_path.push(*obj);
                    let value = &cell_info.objects[obj];
                    match value {
                        SolvedValue::Rect(rect) => {
                            if depth == 0
                                && let Some(span) = &rect.span
                            {
                                source_coordinates.insert(
                                    span.clone(),
                                    [
                                        (&rect.x0, "x0i"),
                                        (&rect.x1, "x1i"),
                                        (&rect.y0, "y0i"),
                                        (&rect.y1, "y1i"),
                                    ]
                                    .into_iter()
                                    .map(|(coordinate, name)| {
                                        (coordinate.1.clone(), name.to_owned(), coordinate.0)
                                    })
                                    .collect(),
                                );
                            }
                            let p0p = ifmatvec(mat, (rect.x0.0, rect.y0.0));
                            let p1p = ifmatvec(mat, (rect.x1.0, rect.y1.0));
                            let layer = rect
                                .layer
                                .as_ref()
                                .and_then(|layer| layers.layers.get(layer.as_str()));
                            if let Some(layer) = layer
                                && !rect.construction
                            {
                                let (sse_dx0, sse_dx1, sse_dy0, sse_dy1) = if let Some(ref sse_dv) =
                                    sse_dv
                                {
                                    if depth == 0 {
                                        (
                                            crate::sse::dot(&SparseVec::from(&rect.x0.1), sse_dv),
                                            crate::sse::dot(&SparseVec::from(&rect.x1.1), sse_dv),
                                            crate::sse::dot(&SparseVec::from(&rect.y0.1), sse_dv),
                                            crate::sse::dot(&SparseVec::from(&rect.y1.1), sse_dv),
                                        )
                                    } else {
                                        (0., 0., 0., 0.)
                                    }
                                } else {
                                    (0., 0., 0., 0.)
                                };
                                let rect =
                                    Rect {
                                        x0: (p0p.0.min(p1p.0) + ofs.0 + sse_dx0) as f32,
                                        y0: (p0p.1.min(p1p.1) + ofs.1 + sse_dy0) as f32,
                                        x1: (p0p.0.max(p1p.0) + ofs.0 + sse_dx1) as f32,
                                        y1: (p0p.1.max(p1p.1) + ofs.1 + sse_dy1) as f32,
                                        id: rect.span.clone(),
                                        object_path,
                                        border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                                        // TODO: this is wrong for transformed rects
                                        border_styles: Edges {
                                            // TODO: check constrained status and modify widths
                                            top: if rect.y1.1.coeffs.iter().any(|(_, var)| {
                                                cell_info.unsolved_vars.contains(var)
                                            }) {
                                                BorderStyle::Dashed
                                            } else {
                                                BorderStyle::Solid
                                            },
                                            right: if rect.x1.1.coeffs.iter().any(|(_, var)| {
                                                cell_info.unsolved_vars.contains(var)
                                            }) {
                                                BorderStyle::Dashed
                                            } else {
                                                BorderStyle::Solid
                                            },
                                            bottom: if rect.y0.1.coeffs.iter().any(|(_, var)| {
                                                cell_info.unsolved_vars.contains(var)
                                            }) {
                                                BorderStyle::Dashed
                                            } else {
                                                BorderStyle::Solid
                                            },
                                            left: if rect.x0.1.coeffs.iter().any(|(_, var)| {
                                                cell_info.unsolved_vars.contains(var)
                                            }) {
                                                BorderStyle::Dashed
                                            } else {
                                                BorderStyle::Solid
                                            },
                                        },
                                        cvars: (depth == 0).then(|| Edges {
                                            left: rect.x0.1.clone(),
                                            right: rect.x1.1.clone(),
                                            bottom: rect.y0.1.clone(),
                                            top: rect.y1.1.clone(),
                                        }),
                                    }
                                    .normalized();
                                if let ToolState::Select(SelectToolState { selected_obj }) = &tool
                                    && rect.id.is_some()
                                    && &rect.id == selected_obj
                                {
                                    select_rects.push(Rect {
                                        border_widths: Edges::all(SELECT_WIDTH),
                                        ..rect.clone()
                                    });
                                }
                                if show && layer.visible {
                                    rects.push((rect, layer.clone()));
                                }
                            }
                        }
                        SolvedValue::Polygon(polygon) => {
                            if depth == 0
                                && let Some(span) = &polygon.span
                            {
                                source_coordinates.insert(
                                    span.clone(),
                                    polygon
                                        .points
                                        .iter()
                                        .enumerate()
                                        .flat_map(|(index, (x, y))| {
                                            [
                                                (x.1.clone(), format!("x{index}i"), x.0),
                                                (y.1.clone(), format!("y{index}i"), y.0),
                                            ]
                                        })
                                        .collect(),
                                );
                            }
                            let Some(layer) = layers.layers.get(polygon.layer.as_str()) else {
                                continue;
                            };
                            let edge_styles = if depth == 0 {
                                polygon_edge_styles(polygon.points.len(), |index| {
                                    let (x, y) = &polygon.points[index];
                                    x.1.coeffs
                                        .iter()
                                        .chain(&y.1.coeffs)
                                        .any(|(_, var)| cell_info.unsolved_vars.contains(var))
                                })
                            } else {
                                vec![BorderStyle::Solid; polygon.points.len()]
                            };
                            let points = polygon
                                .points
                                .iter()
                                .map(|(x, y)| {
                                    let (dx, dy) = if depth == 0 {
                                        sse_dv.as_ref().map_or((0., 0.), |sse_dv| {
                                            (
                                                crate::sse::dot(&SparseVec::from(&x.1), sse_dv),
                                                crate::sse::dot(&SparseVec::from(&y.1), sse_dv),
                                            )
                                        })
                                    } else {
                                        (0., 0.)
                                    };
                                    let point = ifmatvec(mat, (x.0 + dx, y.0 + dy));
                                    Point::new((point.0 + ofs.0) as f32, (point.1 + ofs.1) as f32)
                                })
                                .collect();
                            let polygon = Polygon {
                                points,
                                edge_styles,
                                id: polygon.span.clone(),
                                object_path,
                                cvars: (depth == 0).then(|| {
                                    polygon
                                        .points
                                        .iter()
                                        .map(|(x, y)| (x.1.clone(), y.1.clone()))
                                        .collect()
                                }),
                                centerline: None,
                            };
                            if show && layer.visible {
                                polygons.push((polygon, layer.clone()));
                            }
                        }
                        SolvedValue::Path(path) => {
                            if depth == 0
                                && let Some(span) = &path.span
                            {
                                let mut coordinates = path
                                    .points
                                    .iter()
                                    .enumerate()
                                    .flat_map(|(index, (x, y))| {
                                        [
                                            (x.1.clone(), format!("x{index}i"), x.0),
                                            (y.1.clone(), format!("y{index}i"), y.0),
                                        ]
                                    })
                                    .collect::<Vec<_>>();
                                coordinates.push((
                                    path.width.1.clone(),
                                    "widthi".to_owned(),
                                    path.width.0,
                                ));
                                coordinates.push((
                                    path.begin_extension.1.clone(),
                                    "begin_extensioni".to_owned(),
                                    path.begin_extension.0,
                                ));
                                coordinates.push((
                                    path.end_extension.1.clone(),
                                    "end_extensioni".to_owned(),
                                    path.end_extension.0,
                                ));
                                source_coordinates.insert(span.clone(), coordinates);
                            }
                            let Some(layer) = layers.layers.get(path.layer.as_str()) else {
                                continue;
                            };
                            let mut displayed = path.clone();
                            if depth == 0
                                && let Some(sse_dv) = &sse_dv
                            {
                                displayed.width.0 +=
                                    crate::sse::dot(&SparseVec::from(&path.width.1), sse_dv);
                                displayed.begin_extension.0 += crate::sse::dot(
                                    &SparseVec::from(&path.begin_extension.1),
                                    sse_dv,
                                );
                                displayed.end_extension.0 += crate::sse::dot(
                                    &SparseVec::from(&path.end_extension.1),
                                    sse_dv,
                                );
                                for ((x, y), (display_x, display_y)) in
                                    path.points.iter().zip(&mut displayed.points)
                                {
                                    display_x.0 += crate::sse::dot(&SparseVec::from(&x.1), sse_dv);
                                    display_y.0 += crate::sse::dot(&SparseVec::from(&y.1), sse_dv);
                                }
                            }
                            let Some(outline) = displayed.outline() else {
                                continue;
                            };
                            let segment_styles = if depth == 0 {
                                path_segment_styles(path.points.len(), |index| {
                                    let (x, y) = &path.points[index];
                                    x.1.coeffs
                                        .iter()
                                        .chain(&y.1.coeffs)
                                        .any(|(_, var)| cell_info.unsolved_vars.contains(var))
                                })
                            } else {
                                vec![BorderStyle::Solid; path.points.len().saturating_sub(1)]
                            };
                            let centerline = PathCenterline {
                                points: displayed
                                    .points
                                    .iter()
                                    .map(|(x, y)| {
                                        let point = ifmatvec(mat, (x.0, y.0));
                                        Point::new(
                                            (point.0 + ofs.0) as f32,
                                            (point.1 + ofs.1) as f32,
                                        )
                                    })
                                    .collect(),
                                segment_styles,
                                cvars: (depth == 0).then(|| {
                                    path.points
                                        .iter()
                                        .map(|(x, y)| (x.1.clone(), y.1.clone()))
                                        .collect()
                                }),
                            };
                            let polygon = Polygon {
                                edge_styles: vec![BorderStyle::Solid; outline.len()],
                                points: outline
                                    .into_iter()
                                    .map(|point| {
                                        let point = ifmatvec(mat, point);
                                        Point::new(
                                            (point.0 + ofs.0) as f32,
                                            (point.1 + ofs.1) as f32,
                                        )
                                    })
                                    .collect(),
                                id: path.span.clone(),
                                object_path,
                                cvars: None,
                                centerline: Some(centerline),
                            };
                            if show && layer.visible {
                                polygons.push((polygon, layer.clone()));
                            }
                        }
                        SolvedValue::Instance(inst) => {
                            if inst.construction {
                                continue;
                            }
                            if depth == 0 {
                                source_coordinates.insert(
                                    inst.span.clone(),
                                    vec![
                                        (inst.x_expr.clone(), "xi".to_owned(), inst.x),
                                        (inst.y_expr.clone(), "yi".to_owned(), inst.y),
                                    ],
                                );
                            }
                            let mut inst_mat = TransformationMatrix::identity();
                            if inst.reflect {
                                inst_mat = inst_mat.reflect_vert()
                            }
                            inst_mat = inst_mat.rotate(inst.angle);
                            let (sse_dx, sse_dy) = if depth == 0 {
                                sse_dv.as_ref().map_or((0., 0.), |sse_dv| {
                                    (
                                        crate::sse::dot(&SparseVec::from(&inst.x_expr), sse_dv),
                                        crate::sse::dot(&SparseVec::from(&inst.y_expr), sse_dv),
                                    )
                                })
                            } else {
                                (0., 0.)
                            };
                            let inst_ofs = ifmatvec(mat, (inst.x + sse_dx, inst.y + sse_dy));

                            let inst_address = ScopeAddress {
                                scope: solved_cell.output.cells[&inst.cell].root,
                                cell: inst.cell,
                            };
                            let new_mat = mat * inst_mat;
                            let new_ofs = (inst_ofs.0 + ofs.0, inst_ofs.1 + ofs.1);
                            let scope_state =
                                &solved_cell.state[&solved_cell.scope_paths[&inst_address]];
                            let mut show = show;
                            if depth + 1 >= state.hierarchy_depth || !scope_state.visible {
                                if let Some(bbox) = &scope_state.bbox {
                                    let p0p = ifmatvec(new_mat, (bbox.x0, bbox.y0));
                                    let p1p = ifmatvec(new_mat, (bbox.x1, bbox.y1));
                                    let x_unconstrained =
                                        depth == 0
                                            && inst.x_expr.coeffs.iter().any(|(_, var)| {
                                                cell_info.unsolved_vars.contains(var)
                                            });
                                    let y_unconstrained =
                                        depth == 0
                                            && inst.y_expr.coeffs.iter().any(|(_, var)| {
                                                cell_info.unsolved_vars.contains(var)
                                            });
                                    let rect = Rect {
                                        x0: (p0p.0.min(p1p.0) + new_ofs.0) as f32,
                                        y0: (p0p.1.min(p1p.1) + new_ofs.1) as f32,
                                        x1: (p0p.0.max(p1p.0) + new_ofs.0) as f32,
                                        y1: (p0p.1.max(p1p.1) + new_ofs.1) as f32,
                                        id: Some(inst.span.clone()),
                                        object_path: object_path.clone(),
                                        border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                                        border_styles: Edges {
                                            top: if y_unconstrained {
                                                BorderStyle::Dashed
                                            } else {
                                                BorderStyle::Solid
                                            },
                                            right: if x_unconstrained {
                                                BorderStyle::Dashed
                                            } else {
                                                BorderStyle::Solid
                                            },
                                            bottom: if y_unconstrained {
                                                BorderStyle::Dashed
                                            } else {
                                                BorderStyle::Solid
                                            },
                                            left: if x_unconstrained {
                                                BorderStyle::Dashed
                                            } else {
                                                BorderStyle::Solid
                                            },
                                        },
                                        cvars: None,
                                    };
                                    if let ToolState::Select(SelectToolState { selected_obj }) =
                                        &tool
                                        && rect.id.is_some()
                                        && &rect.id == selected_obj
                                    {
                                        select_rects.push(Rect {
                                            border_widths: Edges::all(SELECT_WIDTH),
                                            ..rect.clone()
                                        });
                                    }
                                    if show {
                                        if depth == 0 {
                                            instance_sse_candidates.push((
                                                rect.clone(),
                                                inst.span.clone(),
                                                Point::new(new_ofs.0 as f32, new_ofs.1 as f32),
                                                [
                                                    SseDragTarget {
                                                        expr: inst.x_expr.clone(),
                                                        normal: Point::new(1., 0.),
                                                        source: Some(SseSourceTarget {
                                                            call_span: inst.span.clone(),
                                                            name: "xi".to_owned(),
                                                            value: inst.x,
                                                        }),
                                                    },
                                                    SseDragTarget {
                                                        expr: inst.y_expr.clone(),
                                                        normal: Point::new(0., 1.),
                                                        source: Some(SseSourceTarget {
                                                            call_span: inst.span.clone(),
                                                            name: "yi".to_owned(),
                                                            value: inst.y,
                                                        }),
                                                    },
                                                ],
                                            ));
                                        }
                                        scope_rects.push(LabeledBbox {
                                            rect,
                                            label: scope_state.name.clone().into(),
                                            origin: Some(Point::new(
                                                new_ofs.0 as f32,
                                                new_ofs.1 as f32,
                                            )),
                                        });
                                    }
                                }
                                show = false;
                            }
                            queue.push_back((
                                inst_address,
                                new_mat,
                                new_ofs,
                                depth + 1,
                                show,
                                object_path,
                            ));
                        }
                        SolvedValue::Dimension(_) => {}
                        SolvedValue::Text(text) => {
                            let position = ifmatvec(mat, (text.x, text.y));
                            let layer = layers.layers.get(text.layer.as_str());
                            if let Some(layer) = layer
                                && show
                                && layer.visible
                                && layout_text_metrics(inner.scale, TEXT_LAYOUT_SIZE).is_some()
                            {
                                texts.push(TextLabel {
                                    text: text.text.clone().into(),
                                    position: Point::new(
                                        (position.0 + ofs.0) as f32,
                                        (position.1 + ofs.1) as f32,
                                    ),
                                    layer: layer.clone(),
                                });
                            }
                        }
                    }
                }
                for child in &scope_info.children {
                    let scope_address = ScopeAddress {
                        scope: *child,
                        cell,
                    };
                    queue.push_back((scope_address, mat, ofs, depth + 1, show, path.clone()));
                }
            }

            if let ToolState::DrawRect(DrawRectToolState { p0: Some(p0) }) = tool {
                let mut layer = layers.layers[layers.selected_layer.as_ref().unwrap()].clone();
                layer.border_color = rgb(0xffff00);
                rects.push((
                    Rect {
                        object_path: Vec::new(),
                        x0: p0.x.min(snapped_layout_mouse_position.x),
                        y0: p0.y.min(snapped_layout_mouse_position.y),
                        x1: p0.x.max(snapped_layout_mouse_position.x),
                        y1: p0.y.max(snapped_layout_mouse_position.y),
                        id: None,
                        border_widths: Edges::all(SELECT_WIDTH),
                        border_styles: Edges::all(BorderStyle::Dashed),
                        cvars: None,
                    },
                    layer,
                ));
            }
        }

        let rects = rects
            .into_iter()
            .sorted_by_key(|(_, layer)| layer.z)
            .collect_vec();
        let polygons = polygons
            .into_iter()
            .sorted_by_key(|(_, layer)| layer.z)
            .collect_vec();
        let scale = inner.scale;
        let offset = inner.offset;
        let content_revision = inner.raster_content_revision;
        let mut dim_hitboxes = Vec::new();
        let mut sse_handles: Vec<SseHandle> = Vec::new();
        let mut vertex_handle_points = Vec::new();
        let mut sse_bodies: Vec<SseBody> = Vec::new();
        let sse_cell = solved_cell.as_ref().map(|solved| {
            let selected = &solved.state[&solved.selected_scope].address;
            &solved.output.cells[&selected.cell]
        });
        let mut movable_corners = HashMap::new();
        for (rect, _) in &rects {
            let (Some(span), Some(cvars), Some(sse_cell)) = (&rect.id, &rect.cvars, sse_cell)
            else {
                continue;
            };
            movable_corners.insert(
                span.clone(),
                [
                    (&cvars.left, &cvars.top),
                    (&cvars.right, &cvars.top),
                    (&cvars.left, &cvars.bottom),
                    (&cvars.right, &cvars.bottom),
                ]
                .map(|(x, y)| {
                    LayoutCanvas::sse_targets_support_2d(&corner_sse_targets(x, y), sse_cell)
                }),
            );
            let targets = vec![
                sourced_sse_target(&cvars.left, Point::new(1., 0.), span, &source_coordinates),
                sourced_sse_target(&cvars.right, Point::new(1., 0.), span, &source_coordinates),
                sourced_sse_target(&cvars.bottom, Point::new(0., 1.), span, &source_coordinates),
                sourced_sse_target(&cvars.top, Point::new(0., 1.), span, &source_coordinates),
            ];
            if LayoutCanvas::sse_targets_support_2d(&targets, sse_cell) {
                sse_bodies.push(SseBody {
                    bounds: get_rect_bounds(rect, bounds, scale, offset),
                    span: span.clone(),
                    targets,
                });
            }
        }
        if let Some(sse_cell) = sse_cell {
            for (polygon, _) in &polygons {
                let Some(span) = &polygon.id else {
                    continue;
                };
                if !matches!(
                    &tool,
                    ToolState::Select(SelectToolState {
                        selected_obj: Some(selected),
                    }) if selected == span
                ) {
                    continue;
                }
                let point_sets = polygon
                    .cvars
                    .as_ref()
                    .map(|cvars| (&polygon.points, cvars))
                    .into_iter()
                    .chain(polygon.centerline.as_ref().and_then(|centerline| {
                        centerline
                            .cvars
                            .as_ref()
                            .map(|cvars| (&centerline.points, cvars))
                    }));
                for (points, cvars) in point_sets {
                    for (point, (x, y)) in points.iter().zip(cvars) {
                        let targets = LayoutCanvas::draggable_point_targets(
                            sourced_corner_sse_targets(x, y, span, &source_coordinates),
                            sse_cell,
                        );
                        if !targets.is_empty() {
                            let mid = inner.layout_to_px(*point);
                            vertex_handle_points.push(mid);
                            let hit_half = HANDLE_HIT.half();
                            sse_handles.push(SseHandle {
                                bounds: Bounds::new(
                                    Point::new(mid.x - hit_half, mid.y - hit_half),
                                    Size::new(HANDLE_HIT, HANDLE_HIT),
                                ),
                                targets,
                            });
                        }
                    }
                }
            }
        }
        if let Some(sse_cell) = sse_cell {
            for (rect, span, origin, targets) in instance_sse_candidates {
                let targets: Vec<_> = if LayoutCanvas::sse_targets_support_2d(&targets, sse_cell) {
                    targets.into_iter().collect()
                } else {
                    // A one-degree-of-freedom instance remains draggable along
                    // whichever coordinate can control that degree of freedom.
                    targets
                        .into_iter()
                        .find(|target| LayoutCanvas::sse_target_supported(target, sse_cell))
                        .into_iter()
                        .collect()
                };
                if !targets.is_empty() {
                    sse_bodies.push(SseBody {
                        bounds: get_rect_bounds(&rect, bounds, scale, offset),
                        span: span.clone(),
                        targets: targets.clone(),
                    });
                    if matches!(
                        &tool,
                        ToolState::Select(SelectToolState {
                            selected_obj: Some(selected),
                        }) if selected == &span
                    ) {
                        let mid = inner.layout_to_px(origin);
                        let hit_half = HANDLE_HIT.half();
                        sse_handles.push(SseHandle {
                            bounds: Bounds::new(
                                Point::new(mid.x - hit_half, mid.y - hit_half),
                                Size::new(HANDLE_HIT, HANDLE_HIT),
                            ),
                            targets,
                        });
                    }
                }
            }
        }
        let draw_layer = layers
            .selected_layer
            .as_ref()
            .and_then(|layer| layers.layers.get(layer))
            .cloned();
        let theme = inner.state.read(cx).theme();
        inner
            .bg_style
            .clone()
            .paint(bounds, window, cx, |window, cx| {
                window.paint_layer(bounds, |window| {
                    // Draw origin lines.
                    let origin_coords = self.inner.read(cx).layout_to_px(Point::new(0., 0.));
                    let y_axis = Edge {
                        dir: Dir::Vert,
                        coord: origin_coords.x,
                        start: bounds.origin.y,
                        stop: bounds.origin.y + bounds.size.height,
                    };
                    let x_axis = Edge {
                        dir: Dir::Horiz,
                        coord: origin_coords.y,
                        start: bounds.origin.x,
                        stop: bounds.origin.x + bounds.size.width,
                    };
                    window.paint_quad(get_paint_path(
                        y_axis.select_bounds(px(0.)),
                        theme.axes,
                        DEFAULT_BORDER_WIDTH,
                    ));
                    window.paint_quad(get_paint_path(
                        x_axis.select_bounds(px(0.)),
                        theme.axes,
                        DEFAULT_BORDER_WIDTH,
                    ));
                    for (r, l) in &rects {
                        window.paint_quad(get_paint_quad(
                            get_rect_bounds(r, bounds, scale, offset),
                            l.fill,
                            l.color,
                            l.border_color,
                            r.border_widths,
                            r.border_styles,
                        ));
                    }
                    for (polygon, layer) in &polygons {
                        let points = polygon
                            .points
                            .iter()
                            .map(|point| self.inner.read(cx).layout_to_px(*point))
                            .collect::<Vec<_>>();
                        paint_polygon_fill(
                            window,
                            &points,
                            layer,
                            polygon.centerline.is_some(),
                        );
                        paint_polygon_border(
                            window,
                            &points,
                            &polygon.edge_styles,
                            DEFAULT_BORDER_WIDTH,
                            layer.border_color,
                        );
                    }
                    if let ToolState::DrawPolygon(polygon_tool) = &tool
                        && !polygon_tool.points.is_empty()
                        && let Some(layer) = &draw_layer
                    {
                        let preview_position = if self.inner.read(cx).shift_down {
                            snap_draw_point(
                                *polygon_tool.points.last().unwrap(),
                                snapped_layout_mouse_position,
                            )
                            .0
                        } else {
                            snapped_layout_mouse_position
                        };
                        let points = polygon_tool
                            .points
                            .iter()
                            .copied()
                            .chain(std::iter::once(preview_position))
                            .map(|point| self.inner.read(cx).layout_to_px(point))
                            .collect::<Vec<_>>();
                        paint_polygon_fill(window, &points, layer, false);
                        paint_polygon_border(
                            window,
                            &points,
                            &vec![BorderStyle::Dashed; points.len()],
                            SELECT_WIDTH,
                            rgb(0xffff00),
                        );
                        for point in points.iter().take(polygon_tool.points.len()) {
                            let draw_half = HANDLE_SIZE.half();
                            window.paint_quad(get_paint_quad(
                                Bounds::new(
                                    Point::new(point.x - draw_half, point.y - draw_half),
                                    Size::new(HANDLE_SIZE, HANDLE_SIZE),
                                ),
                                ShapeFill::Solid,
                                rgb(HANDLE_FILL),
                                rgb(HANDLE_BORDER),
                                Edges::all(px(1.5)),
                                Edges::all(BorderStyle::Solid),
                            ));
                        }
                    }
                    if let ToolState::DrawPath(path_tool) = &tool
                        && !path_tool.points.is_empty()
                        && let Some(layer) = &draw_layer
                    {
                        let preview_position = if self.inner.read(cx).shift_down {
                            snap_draw_point(
                                *path_tool.points.last().unwrap(),
                                snapped_layout_mouse_position,
                            )
                            .0
                        } else {
                            snapped_layout_mouse_position
                        };
                        let centerline = path_tool
                            .points
                            .iter()
                            .copied()
                            .chain(std::iter::once(preview_position))
                            .collect::<Vec<_>>();
                        let layout_points = centerline
                            .iter()
                            .map(|point| (f64::from(point.x), f64::from(point.y)))
                            .collect::<Vec<_>>();
                        if let Some(outline) = compile::path_outline(
                            &layout_points,
                            f64::from(DEFAULT_DRAW_PATH_WIDTH),
                            0.,
                            0.,
                        ) {
                            let outline = outline
                                .into_iter()
                                .map(|(x, y)| {
                                    self.inner
                                        .read(cx)
                                        .layout_to_px(Point::new(x as f32, y as f32))
                                })
                                .collect::<Vec<_>>();
                            paint_polygon_fill(window, &outline, layer, true);
                            paint_polygon_border(
                                window,
                                &outline,
                                &vec![BorderStyle::Solid; outline.len()],
                                DEFAULT_BORDER_WIDTH,
                                layer.border_color,
                            );
                        }
                        let centerline = centerline
                            .iter()
                            .map(|point| self.inner.read(cx).layout_to_px(*point))
                            .collect::<Vec<_>>();
                        paint_polyline(
                            window,
                            &centerline,
                            &vec![BorderStyle::Dashed; centerline.len().saturating_sub(1)],
                            SELECT_WIDTH,
                            rgb(0xffff00),
                        );
                        for point in centerline.iter().take(path_tool.points.len()) {
                            let draw_half = HANDLE_SIZE.half();
                            window.paint_quad(get_paint_quad(
                                Bounds::new(
                                    Point::new(point.x - draw_half, point.y - draw_half),
                                    Size::new(HANDLE_SIZE, HANDLE_SIZE),
                                ),
                                ShapeFill::Solid,
                                rgb(HANDLE_FILL),
                                rgb(HANDLE_BORDER),
                                Edges::all(px(1.5)),
                                Edges::all(BorderStyle::Solid),
                            ));
                        }
                    }
                    for label in &texts {
                        let Some((font_size, line_height)) =
                            layout_text_metrics(scale, TEXT_LAYOUT_SIZE)
                        else {
                            break;
                        };
                        let runs = &[TextRun {
                            len: label.text.len(),
                            font: window.text_style().font(),
                            color: label.layer.border_color.into(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }];
                        window
                            .text_system()
                            .shape_line(label.text.clone(), font_size, runs, None)
                            .paint(
                                Point::new(
                                    scale * px(label.position.x),
                                    scale * px(-label.position.y),
                                ) + offset
                                    + bounds.origin,
                                line_height,
                                window,
                                cx,
                            )
                            .unwrap();
                    }
                    for bbox in &scope_rects {
                        window.paint_quad(get_paint_quad(
                            get_rect_bounds(&bbox.rect, bounds, scale, offset),
                            ShapeFill::Solid,
                            Rgba {
                                a: 0.,
                                ..theme.text
                            },
                            theme.text,
                            bbox.rect.border_widths,
                            bbox.rect.border_styles,
                        ));
                        let Some((font_size, line_height)) =
                            layout_text_metrics(scale, SCOPE_TEXT_LAYOUT_SIZE)
                        else {
                            continue;
                        };
                        let text_origin = get_rect_bounds(&bbox.rect, bounds, scale, offset).origin
                            + Point::new(px(4.), px(2.));
                        let runs = &[TextRun {
                            len: bbox.label.len(),
                            font: window.text_style().font(),
                            color: theme.text.into(),
                            background_color: None,
                            underline: None,
                            strikethrough: None,
                        }];
                        window
                            .text_system()
                            .shape_line(bbox.label.clone(), font_size, runs, None)
                            .paint(text_origin, line_height, window, cx)
                            .unwrap();
                        if let Some(origin) = bbox.origin
                            && matches!(
                                &tool,
                                ToolState::Select(SelectToolState {
                                    selected_obj: Some(selected),
                                }) if bbox.rect.id.as_ref() == Some(selected)
                            )
                        {
                            let mid = self.inner.read(cx).layout_to_px(origin);
                            let draw_half = HANDLE_SIZE.half();
                            window.paint_quad(get_paint_quad(
                                Bounds::new(
                                    Point::new(mid.x - draw_half, mid.y - draw_half),
                                    Size::new(HANDLE_SIZE, HANDLE_SIZE),
                                ),
                                ShapeFill::Solid,
                                rgb(HANDLE_FILL),
                                rgb(HANDLE_BORDER),
                                Edges::all(px(1.5)),
                                Edges::all(BorderStyle::Solid),
                            ));
                        }
                    }
                    if let ToolState::PlaceInstance(placement) = &tool {
                        for rect in &placement.rects {
                            let rect = rect.transform(
                                TransformationMatrix::identity(),
                                (
                                    snapped_layout_mouse_position.x as f64,
                                    snapped_layout_mouse_position.y as f64,
                                ),
                            );
                            window.paint_quad(get_paint_quad(
                                get_rect_bounds(&rect, bounds, scale, offset),
                                ShapeFill::Solid,
                                Rgba { a: 0., ..rgb(0xffff00) },
                                rgb(0xffff00),
                                rect.border_widths,
                                rect.border_styles,
                            ));
                        }
                        for polygon in &placement.polygons {
                            let polygon = polygon.transform(
                                TransformationMatrix::identity(),
                                (
                                    snapped_layout_mouse_position.x as f64,
                                    snapped_layout_mouse_position.y as f64,
                                ),
                            );
                            let points = polygon
                                .points
                                .iter()
                                .map(|point| self.inner.read(cx).layout_to_px(*point))
                                .collect::<Vec<_>>();
                            let mut border = PathBuilder::stroke(DEFAULT_BORDER_WIDTH);
                            border.add_polygon(&points, true);
                            if let Ok(path) = border.build() {
                                window.paint_path(path, rgb(0xffff00));
                            }
                        }
                    }
                    for mid in &vertex_handle_points {
                        let draw_half = HANDLE_SIZE.half();
                        window.paint_quad(get_paint_quad(
                            Bounds::new(
                                Point::new(mid.x - draw_half, mid.y - draw_half),
                                Size::new(HANDLE_SIZE, HANDLE_SIZE),
                            ),
                            ShapeFill::Solid,
                            rgb(HANDLE_FILL),
                            rgb(HANDLE_BORDER),
                            Edges::all(px(1.5)),
                            Edges::all(BorderStyle::Solid),
                        ));
                    }
                    // Draw edge handles and two-axis corner handles on the
                    // selected top-level rectangle.
                    if let ToolState::Select(SelectToolState {
                        selected_obj: Some(selected_obj),
                    }) = &tool
                    {
                        for (r, _) in &rects {
                            if r.id.as_ref() != Some(selected_obj) {
                                continue;
                            }
                            let Some(cvars) = &r.cvars else { continue };
                            let pb = get_rect_bounds(r, bounds, scale, offset);
                            let center = pb.center();
                            let draw_half = HANDLE_SIZE.half();
                            let hit_half = HANDLE_HIT.half();
                            let edges = [
                                (r.border_styles.left, Point::new(pb.left(), center.y), &cvars.left, Point::new(1f32, 0.)),
                                (r.border_styles.right, Point::new(pb.right(), center.y), &cvars.right, Point::new(1f32, 0.)),
                                (r.border_styles.top, Point::new(center.x, pb.top()), &cvars.top, Point::new(0., 1f32)),
                                (r.border_styles.bottom, Point::new(center.x, pb.bottom()), &cvars.bottom, Point::new(0., 1f32)),
                            ];
                            for (style, mid, expr, normal) in edges {
                                if style != BorderStyle::Dashed {
                                    continue;
                                }
                                // Draw a small visible handle, but record a
                                // larger clickable area so it is easy to grab.
                                window.paint_quad(get_paint_quad(
                                    Bounds::new(
                                        Point::new(mid.x - draw_half, mid.y - draw_half),
                                        Size::new(HANDLE_SIZE, HANDLE_SIZE),
                                    ),
                                    ShapeFill::Solid,
                                    rgb(HANDLE_FILL),
                                    rgb(HANDLE_BORDER),
                                    Edges::all(px(1.5)),
                                    Edges::all(BorderStyle::Solid),
                                ));
                                sse_handles.push(SseHandle {
                                    bounds: Bounds::new(
                                        Point::new(mid.x - hit_half, mid.y - hit_half),
                                        Size::new(HANDLE_HIT, HANDLE_HIT),
                                    ),
                                    targets: vec![sourced_sse_target(
                                        expr,
                                        normal,
                                        selected_obj,
                                        &source_coordinates,
                                    )],
                                });
                            }

                            let Some(movable) = r
                                .id
                                .as_ref()
                                .and_then(|span| movable_corners.get(span))
                            else {
                                continue;
                            };
                            let corners = [
                                (Point::new(pb.left(), pb.top()), &cvars.left, &cvars.top),
                                (Point::new(pb.right(), pb.top()), &cvars.right, &cvars.top),
                                (
                                    Point::new(pb.left(), pb.bottom()),
                                    &cvars.left,
                                    &cvars.bottom,
                                ),
                                (
                                    Point::new(pb.right(), pb.bottom()),
                                    &cvars.right,
                                    &cvars.bottom,
                                ),
                            ];
                            for ((mid, x_expr, y_expr), movable) in
                                corners.into_iter().zip(movable)
                            {
                                if !movable {
                                    continue;
                                }
                                let targets = sourced_corner_sse_targets(
                                    x_expr,
                                    y_expr,
                                    selected_obj,
                                    &source_coordinates,
                                );
                                window.paint_quad(get_paint_quad(
                                    Bounds::new(
                                        Point::new(mid.x - draw_half, mid.y - draw_half),
                                        Size::new(HANDLE_SIZE, HANDLE_SIZE),
                                    ),
                                    ShapeFill::Solid,
                                    rgb(HANDLE_FILL),
                                    rgb(HANDLE_BORDER),
                                    Edges::all(px(1.5)),
                                    Edges::all(BorderStyle::Solid),
                                ));
                                sse_handles.push(SseHandle {
                                    bounds: Bounds::new(
                                        Point::new(mid.x - hit_half, mid.y - hit_half),
                                        Size::new(HANDLE_HIT, HANDLE_HIT),
                                    ),
                                    targets,
                                });
                            }
                        }
                    }

                    let mut draw_dim =
                        |p: f32,
                         n: f32,
                         coord: f32,
                         pstop: f32,
                         nstop: f32,
                         horiz: bool,
                         value: String,
                         edit_value: Option<String>,
                         color: Rgba,
                         span: Option<&Span>| {
                            let (x0, y0, x1, y1) = if horiz {
                                (
                                    p,
                                    pstop,
                                    p,
                                    coord
                                        + if coord > pstop {
                                            5. / scale
                                        } else {
                                            -5. / scale
                                        },
                                )
                            } else {
                                (
                                    pstop,
                                    p,
                                    coord
                                        + if coord > pstop {
                                            5. / scale
                                        } else {
                                            -5. / scale
                                        },
                                    p,
                                )
                            };
                            let start_line = Rect {
                                object_path: Vec::new(),
                                x0: x0.min(x1),
                                y0: y0.min(y1),
                                x1: x0.max(x1),
                                y1: y0.max(y1),
                                id: None,
                                border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                                border_styles: Edges::all(BorderStyle::Solid),
                                cvars: None,
                            };
                            let (x0, y0, x1, y1) = if horiz {
                                (
                                    n,
                                    nstop,
                                    n,
                                    coord
                                        + if coord > nstop {
                                            5. / scale
                                        } else {
                                            -5. / scale
                                        },
                                )
                            } else {
                                (
                                    nstop,
                                    n,
                                    coord
                                        + if coord > nstop {
                                            5. / scale
                                        } else {
                                            -5. / scale
                                        },
                                    n,
                                )
                            };
                            let stop_line = Rect {
                                object_path: Vec::new(),
                                x0: x0.min(x1),
                                y0: y0.min(y1),
                                x1: x0.max(x1),
                                y1: y0.max(y1),
                                id: None,
                                border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                                border_styles: Edges::all(BorderStyle::Solid),
                                cvars: None,
                            };
                            let (x0, y0, x1, y1) = if horiz {
                                (p, coord, n, coord)
                            } else {
                                (coord, p, coord, n)
                            };
                            let dim_line = Rect {
                                object_path: Vec::new(),
                                x0: x0.min(x1),
                                y0: y0.min(y1),
                                x1: x0.max(x1),
                                y1: y0.max(y1),
                                id: None,
                                border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                                border_styles: Edges::all(BorderStyle::Solid),
                                cvars: None,
                            };
                            for r in &[start_line, stop_line, dim_line] {
                                window.paint_quad(get_paint_path(
                                    get_rect_bounds(r, bounds, scale, offset),
                                    color,
                                    DEFAULT_BORDER_WIDTH,
                                ));
                            }

                            let run_len = value.len();
                            let font_size = px(14.);
                            let runs = &[window.text_style().to_run(run_len)];
                            let label_position = Point::new((x0 + x1) / 2., (y0 + y1) / 2.);
                            let origin = self
                                .inner
                                .read(cx)
                                .layout_to_px(label_position);
                            let text = SharedString::from(value);
                            let layout =
                                window
                                    .text_system()
                                    .layout_line(&text, font_size, runs, None);
                            if let Some(span) = span {
                                dim_hitboxes.push(DimensionHitbox {
                                    span: span.clone(),
                                    bounds: vec![Bounds::new(
                                        origin,
                                        size(layout.width, font_size),
                                    )],
                                    value: edit_value.map(SharedString::from).unwrap_or(text.clone()),
                                    label_position,
                                });
                            }
                            window
                                .text_system()
                                .shape_line(text, font_size, runs, None)
                                .paint(origin, px(16.), window, cx)
                                .unwrap();
                        };

                    for dim in dims {
                        let value = solved_linear_after_drag(&dim.value, sse_dv.as_ref());
                        draw_dim(
                            solved_linear_after_drag(&dim.p, sse_dv.as_ref()) as f32,
                            solved_linear_after_drag(&dim.n, sse_dv.as_ref()) as f32,
                            solved_linear_after_drag(&dim.coord, sse_dv.as_ref()) as f32,
                            solved_linear_after_drag(&dim.pstop, sse_dv.as_ref()) as f32,
                            solved_linear_after_drag(&dim.nstop, sse_dv.as_ref()) as f32,
                            dim.horiz,
                            format_dimension_label(value, grid),
                            Some(compile::format_initial_condition(value, grid)),
                            match &tool {
                                ToolState::Select(SelectToolState {
                                    selected_obj: Some(selected),
                                })
                                | ToolState::EditDim(EditDimToolState {
                                    dim: Some(selected),
                                    ..
                                })
                                    if Some(selected) == dim.span.as_ref() =>
                                {
                                    rgb(0xffff00)
                                }
                                _ => theme.text,
                            },
                            dim.span.as_ref(),
                        );
                    }

                    if let ToolState::EditDim(EditDimToolState {
                        pending: Some(pending),
                        ..
                    }) = &tool
                    {
                        let preview = &pending.preview;
                        draw_dim(
                            preview.p,
                            preview.n,
                            preview.coord,
                            preview.pstop,
                            preview.nstop,
                            preview.horiz,
                            preview.value.clone(),
                            None,
                            rgb(0xffff00),
                            None,
                        );
                    }

                    if let ToolState::DrawDim(DrawDimToolState { edges }) = &tool {
                        // draw dimension lines
                        if edges.len() == 1 {
                            if let DimEdge::Edge(edge) = &edges[0] {
                                let display = &edge.display;
                                let coord = match display.dir {
                                    Dir::Horiz => snapped_layout_mouse_position.y,
                                    Dir::Vert => snapped_layout_mouse_position.x,
                                };
                                draw_dim(
                                    display.start,
                                    display.stop,
                                    coord,
                                    display.coord,
                                    display.coord,
                                    display.dir == Dir::Horiz,
                                    format_dimension_label(
                                        (edge.exact.stop - edge.exact.start).abs(),
                                        grid,
                                    ),
                                    None,
                                    rgb(0xff0000),
                                    None,
                                );
                            }
                        } else if edges.len() == 2 {
                            let (p, n, coord, pstop, nstop, horiz, value) =
                                match (&edges[0], &edges[1]) {
                                    (DimEdge::Edge(edge0), DimEdge::Edge(edge1)) => {
                                        let display0 = &edge0.display;
                                        let display1 = &edge1.display;
                                        let coord = match display0.dir {
                                            Dir::Horiz => snapped_layout_mouse_position.x,
                                            Dir::Vert => snapped_layout_mouse_position.y,
                                        };
                                        (
                                            display0.coord,
                                            display1.coord,
                                            coord,
                                            (display0.start + display0.stop) / 2.,
                                            (display1.start + display1.stop) / 2.,
                                            display0.dir == Dir::Vert,
                                            format_dimension_label(
                                                (edge1.exact.coord - edge0.exact.coord).abs(),
                                                grid,
                                            ),
                                        )
                                    }
                                    (DimEdge::X0 | DimEdge::Y0, DimEdge::Edge(edge))
                                    | (DimEdge::Edge(edge), DimEdge::X0 | DimEdge::Y0) => {
                                        let display = &edge.display;
                                        let coord = match display.dir {
                                            Dir::Horiz => snapped_layout_mouse_position.x,
                                            Dir::Vert => snapped_layout_mouse_position.y,
                                        };
                                        (
                                            0.,
                                            display.coord,
                                            coord,
                                            coord,
                                            (display.start + display.stop) / 2.,
                                            display.dir == Dir::Vert,
                                            format_dimension_label(edge.exact.coord.abs(), grid),
                                        )
                                    }
                                    _ => unreachable!(),
                                };
                            draw_dim(
                                p,
                                n,
                                coord,
                                pstop,
                                nstop,
                                horiz,
                                value,
                                None,
                                rgb(0xff0000),
                                None,
                            );
                        }
                        // highlight selected edges
                        for edge in edges {
                            let bounds = match edge {
                                DimEdge::Edge(edge) => {
                                    let edge = &edge.display;
                                    let (x0, y0, x1, y1) = match edge.dir {
                                        Dir::Horiz => {
                                            (edge.start, edge.coord, edge.stop, edge.coord)
                                        }
                                        Dir::Vert => {
                                            (edge.coord, edge.start, edge.coord, edge.stop)
                                        }
                                    };
                                    get_rect_bounds(
                                        &Rect {
                                            object_path: Vec::new(),
                                            x0,
                                            y0,
                                            x1,
                                            y1,
                                            id: None,
                                            border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                                            border_styles: Edges::all(BorderStyle::Solid),
                                            cvars: None,
                                        },
                                        bounds,
                                        scale,
                                        offset,
                                    )
                                }
                                DimEdge::X0 => y_axis.select_bounds(px(0.)),
                                DimEdge::Y0 => x_axis.select_bounds(px(0.)),
                            };
                            window.paint_quad(get_paint_path(bounds, rgb(0xffff00), DEFAULT_BORDER_WIDTH));
                        }
                    }
                    let inner = self.inner.read(cx);
                    // highlight hover edges
                    // TODO: reduce repeat code from on_left_mouse_down
                    match tool {
                        ToolState::DrawDim(dim_tool)
                            if dim_tool.edges.len() < 2 => {
                                let rects = ordered_dimension_rects(&rects, &scope_rects);
                                let scale = inner.scale;
                                let offset = inner.offset;
                                let mut selected = None;
                                if x_axis
                                    .select_bounds(SELECT_WIDTH)
                                    .contains(&inner.mouse_position)
                                {
                                    selected = Some(DimEdge::Y0);
                                }
                                if y_axis
                                    .select_bounds(SELECT_WIDTH)
                                    .contains(&inner.mouse_position)
                                {
                                    selected = Some(DimEdge::X0);
                                }
                                'rects: for (rect, r) in rects.into_iter().map(|r| {
                                    (
                                        r,
                                        Bounds::new(
                                            Point::new(scale * px(r.x0), scale * px(-r.y1))
                                                + offset
                                                + inner.screen_bounds.origin,
                                            Size::new(
                                                scale * px(r.x1 - r.x0),
                                                scale * px(r.y1 - r.y0),
                                            ),
                                        ),
                                    )
                                }) {
                                    for (name, edge_layout, edge_px) in [
                                        (
                                            "y0",
                                            Edge {
                                                dir: Dir::Horiz,
                                                coord: rect.y0,
                                                start: rect.x0,
                                                stop: rect.x1,
                                            },
                                            Edge {
                                                dir: Dir::Horiz,
                                                coord: r.bottom(),
                                                start: r.left(),
                                                stop: r.right(),
                                            },
                                        ),
                                        (
                                            "y1",
                                            Edge {
                                                dir: Dir::Horiz,
                                                coord: rect.y1,
                                                start: rect.x0,
                                                stop: rect.x1,
                                            },
                                            Edge {
                                                dir: Dir::Horiz,
                                                coord: r.top(),
                                                start: r.left(),
                                                stop: r.right(),
                                            },
                                        ),
                                        (
                                            "x0",
                                            Edge {
                                                dir: Dir::Vert,
                                                coord: rect.x0,
                                                start: rect.y0,
                                                stop: rect.y1,
                                            },
                                            Edge {
                                                dir: Dir::Vert,
                                                coord: r.left(),
                                                start: r.top(),
                                                stop: r.bottom(),
                                            },
                                        ),
                                        (
                                            "x1",
                                            Edge {
                                                dir: Dir::Vert,
                                                coord: rect.x1,
                                                start: rect.y0,
                                                stop: rect.y1,
                                            },
                                            Edge {
                                                dir: Dir::Vert,
                                                coord: r.right(),
                                                start: r.top(),
                                                stop: r.bottom(),
                                            },
                                        ),
                                    ] {
                                        let bounds = edge_px.select_bounds(SELECT_WIDTH);
                                        if bounds.contains(&inner.mouse_position)
                                            && !rect.object_path.is_empty()
                                        {
                                            selected =
                                                Some(DimEdge::Edge((rect, name, edge_layout)));
                                            break 'rects;
                                        }
                                    }
                                }
                                match selected {
                                    Some(DimEdge::Edge((r, _, edge))) => {
                                        let path = {
                                            let cell = inner.state.read(cx).solved_cell.read(cx);
                                            if let Some(cell) = cell
                                                && let selected_scope_addr =
                                                    cell.state[&cell.selected_scope].address
                                                && let (true, path) = find_obj_path(
                                                    &r.object_path,
                                                    cell,
                                                    selected_scope_addr,
                                                )
                                            {
                                                let path = path.join(".");
                                                Some(path)
                                            } else {
                                                None
                                            }
                                        };
                                        if path.is_some()
                                            && dim_tool
                                                .edges
                                                .first()
                                                .map(|old_edge| match old_edge {
                                                    DimEdge::X0 => Dir::Vert,
                                                    DimEdge::Y0 => Dir::Horiz,
                                                    DimEdge::Edge(edge) => edge.display.dir,
                                                } == edge.dir)
                                                .unwrap_or(true)
                                        {
                                            let (x0, y0, x1, y1) = match edge.dir {
                                                Dir::Horiz => {
                                                    (edge.start, edge.coord, edge.stop, edge.coord)
                                                }
                                                Dir::Vert => {
                                                    (edge.coord, edge.start, edge.coord, edge.stop)
                                                }
                                            };
                                            window.paint_quad(get_paint_path(
                                                get_rect_bounds(
                                                    &Rect {
                                                        object_path: Vec::new(),
                                                        x0,
                                                        y0,
                                                        x1,
                                                        y1,
                                                        id: None,
                                                        border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                                                        border_styles: Edges::all(BorderStyle::Solid),
                                                        cvars: None,
                                                    },
                                                    bounds,
                                                    scale,
                                                    offset,
                                                ),
                                                rgb(0xffff00),
                                                DEFAULT_BORDER_WIDTH,
                                            ));
                                        }
                                    }
                                    Some(DimEdge::X0) => {
                                        window.paint_quad(get_paint_path(
                                            y_axis.select_bounds(px(0.)),
                                            rgb(0xffff00),
                                            DEFAULT_BORDER_WIDTH,
                                        ));
                                    }
                                    Some(DimEdge::Y0) => {
                                        window.paint_quad(get_paint_path(
                                            x_axis.select_bounds(px(0.)),
                                            rgb(0xffff00),
                                            DEFAULT_BORDER_WIDTH,
                                        ));
                                    }
                                    _ => {}
                                }
                        }
                        ToolState::Select(select_tool) => {
                            if let Some(selected) = &select_tool.selected_obj {
                                for (polygon, _) in &polygons {
                                    if polygon.id.as_ref() != Some(selected) {
                                        continue;
                                    }
                                    if let Some(centerline) = &polygon.centerline {
                                        let points = centerline
                                            .points
                                            .iter()
                                            .map(|point| inner.layout_to_px(*point))
                                            .collect::<Vec<_>>();
                                        paint_polyline(
                                            window,
                                            &points,
                                            &centerline.segment_styles,
                                            SELECT_WIDTH,
                                            rgb(0xffff00),
                                        );
                                    } else {
                                        let points = polygon
                                            .points
                                            .iter()
                                            .map(|point| inner.layout_to_px(*point))
                                            .collect::<Vec<_>>();
                                        paint_polygon_border(
                                            window,
                                            &points,
                                            &polygon.edge_styles,
                                            SELECT_WIDTH,
                                            rgb(0xffff00),
                                        );
                                    }
                                }
                                for rect in &select_rects {
                                    window.paint_quad(get_paint_quad(
                                        get_rect_bounds(rect, bounds, scale, offset),
                                        ShapeFill::Solid,
                                        Rgba { a: 0., ..rgb(0xffff00) },
                                        rgb(0xffff00),
                                        rect.border_widths,
                                        rect.border_styles,
                                    ));
                                }
                            }
                            if let Some(hit) = inner
                                .selection_hits_at(inner.mouse_position)
                                .into_iter()
                                .next()
                            {
                                match hit.outline {
                                    SelectionOutline::Rect {
                                        bounds,
                                        border_styles,
                                    } => {
                                        window.paint_quad(get_paint_quad(
                                            bounds,
                                            ShapeFill::Solid,
                                            Rgba { a: 0., ..rgb(0xffff00) },
                                            rgb(0xffff00),
                                            Edges::all(SELECT_WIDTH),
                                            border_styles,
                                        ));
                                    }
                                    SelectionOutline::Polygon {
                                        points,
                                        edge_styles,
                                    } => paint_polygon_border(
                                        window,
                                        &points,
                                        &edge_styles,
                                        SELECT_WIDTH,
                                        rgb(0xffff00),
                                    ),
                                    SelectionOutline::Polyline {
                                        points,
                                        segment_styles,
                                    } => paint_polyline(
                                        window,
                                        &points,
                                        &segment_styles,
                                        SELECT_WIDTH,
                                        rgb(0xffff00),
                                    ),
                                }
                            }
                        }
                        _ => {}
                    }
                })
            });
        let raster_cache = (select_overview
            && rects.len() + polygons.len() + scope_rects.len() >= RASTER_CACHE_GEOMETRY_THRESHOLD)
            .then(|| {
                build_layout_raster(
                    &rects,
                    &polygons,
                    &scope_rects,
                    &texts,
                    ViewportTransform {
                        size: bounds.size,
                        screen_size: bounds.size,
                        scale,
                        offset,
                    },
                    theme,
                    content_revision,
                )
            })
            .flatten();
        let refresh_cached_raster = raster_cache.is_some() && select_overview;
        let raster_tiles = raster_cache.map(single_raster_tile_set);
        self.inner.update(cx, |inner, cx| {
            if select_overview {
                inner.resolve_raster_display_freeze();
                inner.raster_display = raster_tiles.as_ref().map(|tiles| {
                    let layout_bounds = inner
                        .raster_layout_bbox
                        .as_ref()
                        .map(|bbox| layout_bbox_screen_bounds(bbox, bounds, scale, offset));
                    navigation_raster_transform(tiles, bounds, scale, offset, layout_bounds)
                        .unwrap_or_else(|| navigation_raster_capture_transform(tiles))
                });
                if let Some(previous) = std::mem::replace(&mut inner.raster_tiles, raster_tiles) {
                    inner
                        .raster_images_to_drop
                        .extend(previous.tiles.into_values().map(|cache| cache.image));
                }
                if let Some(previous) = inner.raster_staging_tiles.take() {
                    inner
                        .raster_images_to_drop
                        .extend(previous.tiles.into_values().map(|cache| cache.image));
                }
                inner.clear_raster_reprojection();
                inner.raster_tile_target = None;
            }
            inner.rects = rects;
            inner.polygons = polygons;
            inner.scope_rects = scope_rects;
            inner.dim_hitboxes = dim_hitboxes;
            inner.sse_handles = sse_handles;
            inner.sse_bodies = sse_bodies;
            if refresh_cached_raster {
                // The first exact traversal intentionally avoids expanding
                // millions of shapes on the UI thread. Replace that initial
                // cache with the flattened background raster before the user
                // begins navigating.
                inner.request_navigation_raster(cx);
            }
        });
    }
}

impl Render for LayoutCanvas {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        let theme = self.state.read(cx).theme();
        let dimension_input = self.dimension_input_position(cx).map(|position| {
            div()
                .absolute()
                .left(position.x)
                .top(position.y)
                .w(px(140.))
                .border_1()
                .border_color(theme.divider)
                .rounded_sm()
                .overflow_hidden()
                .child(self.text_input.clone())
        });
        div()
            .flex()
            .flex_1()
            .relative()
            .key_context("LayoutCanvas")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_left_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_mouse_down))
            .on_action(cx.listener(Self::draw_rect))
            .on_action(cx.listener(Self::draw_polygon))
            .on_action(cx.listener(Self::draw_path))
            .on_action(cx.listener(Self::select_mode))
            .on_action(cx.listener(Self::draw_dim))
            .on_action(cx.listener(Self::edit_action))
            .on_action(cx.listener(Self::fit_to_screen_action))
            .on_action(cx.listener(Self::zero_hierarchy))
            .on_action(cx.listener(Self::one_hierarchy))
            .on_action(cx.listener(Self::all_hierarchy))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::finish_draw_points))
            .on_action(cx.listener(Self::dark_mode))
            .on_action(cx.listener(Self::light_mode))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_middle_mouse_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::on_middle_mouse_up))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_left_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_left_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .child(CanvasElement {
                inner: cx.entity().clone(),
            })
            .children(dimension_input)
    }
}

impl Focusable for LayoutCanvas {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl LayoutCanvas {
    pub(crate) fn new(
        cx: &mut Context<Self>,
        state: &Entity<EditorState>,
        focus_handle: FocusHandle,
        text_input_focus_handle: FocusHandle,
        text_input: Entity<TextInput>,
    ) -> Self {
        let tool = state.read(cx).tool.clone();
        LayoutCanvas {
            focus_handle,
            text_input_focus_handle,
            text_input,
            offset: Point::new(px(0.), px(0.)),
            bg_style: Style {
                size: Size {
                    width: Length::Definite(DefiniteLength::Fraction(1.)),
                    height: Length::Definite(DefiniteLength::Fraction(1.)),
                },
                ..Style::default()
            },
            is_dragging: false,
            is_sse_dragging: false,
            is_sse_persisting: false,
            pending_sse_values: Vec::new(),
            deferred_snapshot: None,
            sse_persist_after_revision: None,
            sse_targets: Vec::new(),
            sse_delta: Point::default(),
            sse_handles: Vec::new(),
            sse_bodies: Vec::new(),
            drag_start: Point::default(),
            offset_start: Point::default(),
            mouse_position: Point::default(),
            hover_hit: None,
            shift_down: false,
            scale: 1.0,
            screen_bounds: Bounds::default(),
            _subscriptions: vec![
                cx.observe(state, |canvas, _, cx| {
                    if !canvas.update_raster_presentation(cx) {
                        return;
                    }
                    canvas.raster_content_revision = canvas.raster_content_revision.wrapping_add(1);
                    canvas
                        .raster_content_revision_signal
                        .store(canvas.raster_content_revision, Ordering::Release);
                    canvas
                        .cell_raster_tiles
                        .lock()
                        .expect("cell raster tile cache poisoned")
                        .clear();
                    canvas.raster_spatial_index = Arc::new(RasterSpatialIndex::default());
                    // Presentation changes invalidate every old pixel immediately.
                    // In particular, a hidden layer must never survive in a
                    // transformed or retained navigation image.
                    if let Some(previous) = canvas.raster_tiles.take() {
                        canvas
                            .raster_images_to_drop
                            .extend(previous.tiles.into_values().map(|cache| cache.image));
                    }
                    if let Some(previous) = canvas.raster_staging_tiles.take() {
                        canvas
                            .raster_images_to_drop
                            .extend(previous.tiles.into_values().map(|cache| cache.image));
                    }
                    if let Some(previous) = canvas.raster_reprojection.take() {
                        canvas.raster_images_to_drop.push(previous.image);
                    }
                    if let Some(previous) = canvas.raster_overview.take() {
                        canvas.raster_images_to_drop.push(previous.image);
                    }
                    canvas.raster_overview_requested_revision = None;
                    canvas.raster_overview_refinement = None;
                    canvas.raster_display = None;
                    canvas.raster_display_frozen = false;
                    canvas.raster_tile_target = None;
                    canvas.navigation_cache_active = canvas.raster_output.is_some();
                    if canvas.raster_output.is_some()
                        && canvas.screen_bounds.size.width > px(0.)
                        && canvas.screen_bounds.size.height > px(0.)
                    {
                        canvas.request_navigation_raster(cx);
                    } else {
                        canvas.advance_raster_generation();
                    }
                    cx.notify();
                }),
                cx.observe(&tool, |_, _, cx| cx.notify()),
            ],
            state: state.clone(),
            rects: Vec::new(),
            polygons: Vec::new(),
            scope_rects: Vec::new(),
            dim_hitboxes: Vec::new(),
            raster_tiles: None,
            raster_staging_tiles: None,
            raster_images_to_drop: Vec::new(),
            raster_reprojection: None,
            raster_overview: None,
            raster_overview_requested_revision: None,
            raster_overview_refinement: None,
            raster_display: None,
            raster_display_frozen: false,
            raster_tile_target: None,
            navigation_cache_active: false,
            raster_refinement: None,
            raster_worker_active: false,
            raster_generation: 0,
            raster_generation_signal: Arc::new(AtomicU64::new(0)),
            raster_scale_signal: Arc::new(AtomicU64::new(1.0_f32.to_bits() as u64)),
            raster_content_revision: 0,
            raster_content_revision_signal: Arc::new(AtomicU64::new(0)),
            raster_output: None,
            raster_scope_state: None,
            raster_selected_scope: None,
            raster_layer_visibility: Vec::new(),
            raster_hierarchy_depth: usize::MAX,
            raster_hide_external_geometry: false,
            raster_layout_bbox: None,
            raster_dark_mode: true,
            cell_raster_tiles: Arc::new(Mutex::new(CellRasterTileCache::default())),
            raster_spatial_index: Arc::new(RasterSpatialIndex::default()),
            pending_init: true,
        }
    }

    /// Records the exact state that changes raster pixels. UI-only changes,
    /// such as selecting a layer in the sidebar, do not force a rebuild.
    fn update_raster_presentation(&mut self, cx: &gpui::App) -> bool {
        let (
            output,
            scope_state,
            selected_scope,
            layout_bbox,
            layer_visibility,
            hierarchy_depth,
            hide_external_geometry,
            dark_mode,
        ) = {
            let state = self.state.read(cx);
            let hide_external_geometry = state.hide_external_geometry;
            let solved_cell = state.solved_cell.read(cx);
            let (output, scope_state, selected_scope, layout_bbox) = solved_cell
                .as_ref()
                .map(|solved| {
                    let selected = solved.state[&solved.selected_scope].address;
                    let displayed = if hide_external_geometry {
                        selected
                    } else {
                        ScopeAddress {
                            cell: selected.cell,
                            scope: solved.output.cells[&selected.cell].root,
                        }
                    };
                    let layout_bbox = solved
                        .scope_paths
                        .get(&displayed)
                        .and_then(|path| solved.state.get(path))
                        .and_then(|scope| scope.bbox.clone());
                    (
                        Some(solved.output.clone()),
                        Some(solved.state.clone()),
                        Some(solved.selected_scope.clone()),
                        layout_bbox,
                    )
                })
                .unwrap_or((None, None, None, None));
            let layer_visibility = state
                .layers
                .read(cx)
                .layers
                .values()
                .map(|layer| layer.visible)
                .collect::<Vec<_>>();
            (
                output,
                scope_state,
                selected_scope,
                layout_bbox,
                layer_visibility,
                state.hierarchy_depth,
                hide_external_geometry,
                state.dark_mode,
            )
        };
        let same_output = match (&self.raster_output, &output) {
            (Some(old), Some(new)) => Arc::ptr_eq(old, new),
            (None, None) => true,
            _ => false,
        };
        let same_scope_state = match (&self.raster_scope_state, &scope_state) {
            (Some(old), Some(new)) => Arc::ptr_eq(old, new),
            (None, None) => true,
            _ => false,
        };
        let changed = !same_output
            || !same_scope_state
            || self.raster_selected_scope != selected_scope
            || self.raster_layer_visibility != layer_visibility
            || self.raster_hierarchy_depth != hierarchy_depth
            || self.raster_hide_external_geometry != hide_external_geometry
            || self.raster_dark_mode != dark_mode;
        self.raster_output = output;
        self.raster_scope_state = scope_state;
        self.raster_selected_scope = selected_scope;
        self.raster_layout_bbox = layout_bbox;
        self.raster_layer_visibility = layer_visibility;
        self.raster_hierarchy_depth = hierarchy_depth;
        self.raster_hide_external_geometry = hide_external_geometry;
        self.raster_dark_mode = dark_mode;
        changed
    }

    fn advance_raster_generation(&mut self) {
        self.raster_generation = self.raster_generation.wrapping_add(1);
        self.raster_generation_signal
            .store(self.raster_generation, Ordering::Release);
    }

    fn set_rendering(&self, rendering: bool, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            if state.rendering != rendering {
                state.rendering = rendering;
                cx.notify();
            }
        });
    }

    fn begin_navigation(&mut self) {
        self.advance_raster_generation();
        self.navigation_cache_active = true;
        self.hover_hit = None;
    }

    fn prepare_navigation_tile_target(&mut self) {
        let screen_viewport = self.screen_bounds.size;
        let tile_size = navigation_tile_size(screen_viewport);
        let raster_scale = self.scale;
        let display_ratio = self.scale / raster_scale;
        let capture_offset =
            Point::new(self.offset.x / display_ratio, self.offset.y / display_ratio);
        // Capture new LODs against the exact camera origin. Rounding this to
        // the half-resolution pixel grid gives the immediate viewport and its
        // replacement tile different quantization phases, making outlines
        // jump at handoff. Tile strides are integral raster pixels, so a
        // fractional anchor remains congruent across adjacent tiles.
        let mut anchor_offset = capture_offset;
        let mut center = RasterTileIndex { x: 0, y: 0 };
        let same_scale_navigation = self
            .raster_tile_target
            .is_some_and(|target| target.scale == raster_scale);
        let reusable_tiles = same_scale_navigation
            .then(|| {
                self.raster_staging_tiles
                    .as_ref()
                    .filter(|tiles| {
                        tiles.scale == raster_scale
                            && tiles.screen_viewport == screen_viewport
                            && tiles.tile_size == tile_size
                            && tiles.content_revision == self.raster_content_revision
                    })
                    .or_else(|| {
                        self.raster_tiles.as_ref().filter(|tiles| {
                            tiles.scale == raster_scale
                                && tiles.screen_viewport == screen_viewport
                                && tiles.tile_size == tile_size
                                && tiles.content_revision == self.raster_content_revision
                        })
                    })
            })
            .flatten();
        if let Some(tiles) = reusable_tiles {
            let candidate = RasterTileIndex {
                x: ((f32::from(tiles.anchor_offset.x) - f32::from(capture_offset.x))
                    / f32::from(tile_size.width))
                .round() as i32,
                y: ((f32::from(tiles.anchor_offset.y) - f32::from(capture_offset.y))
                    / f32::from(tile_size.height))
                .round() as i32,
            };
            let overlaps_retained_field = navigation_tile_order(candidate)
                .iter()
                .any(|index| tiles.tiles.contains_key(index));
            if overlaps_retained_field {
                anchor_offset = tiles.anchor_offset;
                center = candidate;
            }
        }

        let target = RasterTileTarget {
            anchor_offset,
            tile_size,
            screen_viewport,
            scale: raster_scale,
            content_revision: self.raster_content_revision,
            center,
            generation: self.raster_generation,
        };
        if let Some(tiles) = self
            .raster_tiles
            .as_mut()
            .filter(|tiles| raster_tile_set_matches_target(tiles, target))
        {
            let retained = navigation_tile_order(center)
                .into_iter()
                .collect::<HashSet<_>>();
            let mut discarded = Vec::new();
            tiles.tiles.retain(|index, cache| {
                if retained.contains(index) {
                    true
                } else {
                    discarded.push(cache.image.clone());
                    false
                }
            });
            tiles.center = center;
            self.raster_images_to_drop.extend(discarded);
        }
        let staging_matches = self
            .raster_staging_tiles
            .as_ref()
            .is_some_and(|tiles| raster_tile_set_matches_target(tiles, target));
        if staging_matches {
            let tiles = self.raster_staging_tiles.as_mut().expect("checked above");
            let retained = navigation_tile_order(center)
                .into_iter()
                .collect::<HashSet<_>>();
            let mut discarded = Vec::new();
            tiles.tiles.retain(|index, cache| {
                if retained.contains(index) {
                    true
                } else {
                    discarded.push(cache.image.clone());
                    false
                }
            });
            tiles.center = center;
            self.raster_images_to_drop.extend(discarded);
        } else if let Some(previous) = self.raster_staging_tiles.take() {
            self.raster_images_to_drop
                .extend(previous.tiles.into_values().map(|cache| cache.image));
        }
        self.raster_tile_target = Some(target);
        self.raster_scale_signal
            .store(target.scale.to_bits() as u64, Ordering::Release);
    }

    fn raster_display_transform_for_current_view(&self) -> Option<RasterDisplayTransform> {
        let tiles = self
            .raster_tiles
            .as_ref()
            .filter(|tiles| tiles.content_revision == self.raster_content_revision)?;
        let layout_bounds = self.raster_layout_bbox.as_ref().map(|bbox| {
            layout_bbox_screen_bounds(bbox, self.screen_bounds, self.scale, self.offset)
        });
        navigation_raster_transform(
            tiles,
            self.screen_bounds,
            self.scale,
            self.offset,
            layout_bounds,
        )
    }

    fn update_raster_display_transform(&mut self) -> bool {
        if self.raster_display_frozen {
            return false;
        }
        let Some(transform) = self.raster_display_transform_for_current_view() else {
            return false;
        };
        self.raster_display = Some(transform);
        true
    }

    pub(crate) fn navigation_overview_snapshot(
        &self,
        bounds: Bounds<Pixels>,
    ) -> Option<NavigationOverviewSnapshot> {
        if bounds.size.width <= px(0.) || bounds.size.height <= px(0.) {
            return None;
        }
        let cache = self
            .raster_overview
            .as_ref()
            .filter(|cache| cache.content_revision == self.raster_content_revision)?;
        let bbox = self.raster_layout_bbox.as_ref()?;
        let current = ViewportTransform {
            size: self.screen_bounds.size,
            screen_size: self.screen_bounds.size,
            scale: self.scale,
            offset: self.offset,
        };
        let display = navigation_overview_display_viewport(bbox, current, bounds.size);
        let cache_viewport = ViewportTransform {
            size: cache.viewport,
            screen_size: cache.screen_viewport,
            scale: cache.scale,
            offset: cache.offset,
        };
        let image_bounds = navigation_overview_world_bounds(
            display,
            raster_viewport_world_bounds(cache_viewport),
            bounds,
        );
        let viewport_bounds = clamp_bounds_to_container(
            minimum_centered_bounds_size(
                navigation_overview_viewport_bounds(display, current, bounds),
                px(3.),
            ),
            bounds,
        );
        Some(NavigationOverviewSnapshot {
            image: cache.image.clone(),
            image_bounds,
            viewport_bounds,
            bounds,
            display,
        })
    }

    pub(crate) fn center_viewport_on(&mut self, position: Point<f32>, cx: &mut Context<Self>) {
        if self.screen_bounds.size.width <= px(0.) || self.screen_bounds.size.height <= px(0.) {
            return;
        }
        self.begin_navigation();
        self.offset = Point::new(
            self.screen_bounds.size.width / 2. - px(self.scale * position.x),
            self.screen_bounds.size.height / 2. + px(self.scale * position.y),
        );
        self.hover_hit = None;
        self.update_raster_display_transform();
        self.request_navigation_raster(cx);
        cx.notify();
    }

    pub(crate) fn fit_viewport_to_world_bounds(
        &mut self,
        first: Point<f32>,
        second: Point<f32>,
        cx: &mut Context<Self>,
    ) {
        if self.screen_bounds.size.width <= px(0.) || self.screen_bounds.size.height <= px(0.) {
            return;
        }
        let x0 = first.x.min(second.x);
        let x1 = first.x.max(second.x);
        let y0 = first.y.min(second.y);
        let y1 = first.y.max(second.y);
        let previous_raster_display = self.raster_display;
        self.scale = fit_scale(self.screen_bounds.size, x1 - x0, y1 - y0).min(100.);
        self.offset = Point::new(
            px((-(x0 + x1) * self.scale + f32::from(self.screen_bounds.size.width)) / 2.),
            px(((y0 + y1) * self.scale + f32::from(self.screen_bounds.size.height)) / 2.),
        );
        self.hover_hit = None;
        let requested_raster_display = self.raster_display_transform_for_current_view();
        let has_safe_reprojection = self.refresh_raster_reprojection();
        self.raster_display = raster_zoom_display_transform(
            previous_raster_display,
            requested_raster_display,
            has_safe_reprojection,
        );
        self.raster_display_frozen = !has_safe_reprojection;
        self.request_navigation_overview(cx);
        self.request_navigation_raster(cx);
        cx.notify();
    }

    fn clear_raster_reprojection(&mut self) {
        if let Some(previous) = self.raster_reprojection.take() {
            self.raster_images_to_drop.push(previous.image);
        }
    }

    fn resolve_raster_display_freeze(&mut self) {
        self.raster_display_frozen = false;
    }

    fn request_navigation_overview(&mut self, cx: &mut Context<Self>) {
        if self
            .raster_overview
            .as_ref()
            .is_some_and(|cache| cache.content_revision == self.raster_content_revision)
            || self.raster_overview_requested_revision == Some(self.raster_content_revision)
        {
            return;
        }
        let Some(bbox) = self.raster_layout_bbox.as_ref() else {
            return;
        };
        let viewport = navigation_overview_viewport(bbox);
        let state = self.state.read(cx);
        let Some(solved_cell) = state.solved_cell.read(cx).clone() else {
            return;
        };
        let overview_layers = navigation_overview_layers(&state.layers.read(cx).layers);
        let content_revision = self.raster_content_revision;
        let input = NavigationRasterInput {
            solved_cell,
            layers: Arc::new(overview_layers),
            hierarchy_depth: state.hierarchy_depth,
            hide_external_geometry: state.hide_external_geometry,
            viewport,
            text_color: state.theme().text,
            include_text: false,
            content_revision,
            content_revision_signal: self.raster_content_revision_signal.clone(),
            scale_signal: Arc::new(AtomicU64::new(viewport.scale.to_bits() as u64)),
            cell_raster_tiles: self.cell_raster_tiles.clone(),
            spatial_index: self.raster_spatial_index.clone(),
            use_spatial_index: true,
            cancel_if_generation_changes: None,
        };
        self.raster_overview_requested_revision = Some(content_revision);
        self.raster_overview_refinement = Some(cx.spawn(async move |canvas, cx| {
            let cache = cx
                .background_spawn(async move { build_navigation_raster(input) })
                .await;
            let _ = canvas.update(cx, |canvas, cx| {
                if canvas.raster_overview_requested_revision != Some(content_revision) {
                    if let Some(cache) = cache {
                        canvas.raster_images_to_drop.push(cache.image);
                    }
                    return;
                }
                canvas.raster_overview_requested_revision = None;
                if let Some(cache) = cache {
                    if let Some(previous) = canvas.raster_overview.replace(cache) {
                        canvas.raster_images_to_drop.push(previous.image);
                    }
                    cx.notify();
                }
            });
        }));
    }

    fn refresh_raster_reprojection(&mut self) -> bool {
        let reprojection = self.raster_tiles.as_ref().and_then(|tiles| {
            if tiles.scale == self.scale
                || !raster_reprojection_scale_is_safe(tiles.scale, self.scale)
                || tiles.content_revision != self.raster_content_revision
                || tiles.screen_viewport != self.screen_bounds.size
            {
                return None;
            }
            let layout_bounds = self.raster_layout_bbox.as_ref().map(|bbox| {
                layout_bbox_screen_bounds(bbox, self.screen_bounds, self.scale, self.offset)
            });
            navigation_raster_transform(
                tiles,
                self.screen_bounds,
                self.scale,
                self.offset,
                layout_bounds,
            )?;
            reproject_raster_tiles(tiles, self.screen_bounds, self.scale, self.offset)
        });
        let Some(reprojection) = reprojection else {
            return false;
        };
        if let Some(previous) = self.raster_reprojection.replace(reprojection) {
            self.raster_images_to_drop.push(previous.image);
        }
        true
    }

    fn navigation_raster_input(
        &self,
        cx: &gpui::App,
        target: RasterTileTarget,
        index: RasterTileIndex,
    ) -> Option<NavigationRasterInput> {
        let state = self.state.read(cx);
        let solved_cell = state.solved_cell.read(cx).clone()?;
        Some(NavigationRasterInput {
            solved_cell,
            layers: Arc::new(state.layers.read(cx).layers.clone()),
            hierarchy_depth: state.hierarchy_depth,
            hide_external_geometry: state.hide_external_geometry,
            viewport: ViewportTransform {
                size: target.tile_size,
                screen_size: target.screen_viewport,
                scale: target.scale,
                offset: raster_tile_offset(target.anchor_offset, target.tile_size, index),
            },
            text_color: state.theme().text,
            include_text: true,
            content_revision: target.content_revision,
            content_revision_signal: self.raster_content_revision_signal.clone(),
            scale_signal: self.raster_scale_signal.clone(),
            cell_raster_tiles: self.cell_raster_tiles.clone(),
            spatial_index: self.raster_spatial_index.clone(),
            use_spatial_index: true,
            cancel_if_generation_changes: Some((
                self.raster_generation_signal.clone(),
                target.generation,
            )),
        })
    }

    fn next_navigation_tile(
        &self,
        cx: &gpui::App,
    ) -> Option<(RasterTileTarget, RasterTileIndex, NavigationRasterInput)> {
        let target = self.raster_tile_target?;
        if target.generation != self.raster_generation {
            return None;
        }
        let matching_tiles = self
            .raster_staging_tiles
            .as_ref()
            .filter(|tiles| tiles.navigation && raster_tile_set_matches_target(tiles, target))
            .or_else(|| {
                self.raster_tiles.as_ref().filter(|tiles| {
                    tiles.navigation && raster_tile_set_matches_target(tiles, target)
                })
            });
        let index = navigation_tile_order(target.center)
            .into_iter()
            .find(|index| !matching_tiles.is_some_and(|tiles| tiles.tiles.contains_key(index)))?;
        let input = self.navigation_raster_input(cx, target, index)?;
        Some((target, index, input))
    }

    fn install_navigation_tile(
        &mut self,
        target: RasterTileTarget,
        index: RasterTileIndex,
        cache: LayoutRasterCache,
    ) {
        if self.raster_tile_target != Some(target)
            || target.generation != self.raster_generation
            || cache.content_revision != self.raster_content_revision
        {
            self.raster_images_to_drop.push(cache.image);
            return;
        }
        let matches_active = self
            .raster_tiles
            .as_ref()
            .is_some_and(|tiles| raster_tile_set_matches_target(tiles, target));
        if matches_active {
            let tiles = self.raster_tiles.as_mut().expect("matching active tiles");
            if let Some(previous) = tiles.tiles.insert(index, cache) {
                self.raster_images_to_drop.push(previous.image);
            }
            tiles.navigation = true;
            tiles.center = target.center;
            self.resolve_raster_display_freeze();
            self.update_raster_display_transform();
            return;
        }

        let can_stage_over_active = self.raster_tiles.as_ref().is_some_and(|tiles| {
            tiles.content_revision == target.content_revision
                && tiles.screen_viewport == target.screen_viewport
        });
        if !can_stage_over_active {
            // There is no compatible previous frame to retain (initial load,
            // content invalidation, or resize), so make the center visible as
            // soon as possible instead of waiting for nine tiles.
            if index != target.center {
                self.raster_images_to_drop.push(cache.image);
                return;
            }
            let mut replacement = LayoutRasterTileSet {
                tiles: HashMap::new(),
                navigation: true,
                anchor_offset: target.anchor_offset,
                tile_size: target.tile_size,
                screen_viewport: target.screen_viewport,
                scale: target.scale,
                content_revision: target.content_revision,
                center: target.center,
            };
            replacement.tiles.insert(index, cache);
            if let Some(previous) = self.raster_tiles.replace(replacement) {
                self.raster_images_to_drop
                    .extend(previous.tiles.into_values().map(|cache| cache.image));
            }
            self.clear_raster_reprojection();
            self.resolve_raster_display_freeze();
            self.raster_display = self
                .raster_tiles
                .as_ref()
                .map(navigation_raster_capture_transform);
            return;
        }

        let staging_matches = self
            .raster_staging_tiles
            .as_ref()
            .is_some_and(|tiles| raster_tile_set_matches_target(tiles, target));
        if !staging_matches {
            if index != target.center {
                self.raster_images_to_drop.push(cache.image);
                return;
            }
            if let Some(previous) = self.raster_staging_tiles.take() {
                self.raster_images_to_drop
                    .extend(previous.tiles.into_values().map(|cache| cache.image));
            }
            self.raster_staging_tiles = Some(LayoutRasterTileSet {
                tiles: HashMap::new(),
                navigation: true,
                anchor_offset: target.anchor_offset,
                tile_size: target.tile_size,
                screen_viewport: target.screen_viewport,
                scale: target.scale,
                content_revision: target.content_revision,
                center: target.center,
            });
        }

        let staging = self
            .raster_staging_tiles
            .as_mut()
            .expect("staging tiles initialized above");
        if let Some(previous) = staging.tiles.insert(index, cache) {
            self.raster_images_to_drop.push(previous.image);
        }
        staging.center = target.center;

        let staging_covers_viewport = raster_tiles_cover_bounds(
            staging,
            self.screen_bounds,
            self.screen_bounds,
            self.scale,
            self.offset,
        );
        if !staging_covers_viewport
            && !navigation_inner_tiles_complete(&staging.tiles, staging.center)
        {
            return;
        }
        let layout_bounds = self.raster_layout_bbox.as_ref().map(|bbox| {
            layout_bbox_screen_bounds(bbox, self.screen_bounds, self.scale, self.offset)
        });
        let Some(display) = navigation_raster_transform(
            staging,
            self.screen_bounds,
            self.scale,
            self.offset,
            layout_bounds,
        ) else {
            // The target moved far enough while these tiles were rendering
            // that even the completed inner ring cannot represent the current
            // view. Keep the previous frame while the outer ring or a retarget
            // catches up.
            return;
        };
        let replacement = self
            .raster_staging_tiles
            .take()
            .expect("checked and completed above");
        if let Some(previous) = self.raster_tiles.replace(replacement) {
            self.raster_images_to_drop
                .extend(previous.tiles.into_values().map(|cache| cache.image));
        }
        self.clear_raster_reprojection();
        self.resolve_raster_display_freeze();
        self.raster_display = Some(display);
    }

    fn request_navigation_raster(&mut self, cx: &mut Context<Self>) {
        self.navigation_cache_active = true;
        if self.is_dragging
            && (self.raster_cache_has_pan_margin()
                || (self.raster_worker_active && self.raster_tiles_cover_canvas()))
        {
            return;
        }
        self.advance_raster_generation();
        self.prepare_navigation_tile_target();
        if self.raster_worker_active {
            return;
        }
        self.raster_worker_active = true;
        self.set_rendering(true, cx);
        self.raster_refinement = Some(cx.spawn(async move |canvas, cx| {
            loop {
                let Some((target, index, input)) = canvas
                    .update(cx, |canvas, cx| canvas.next_navigation_tile(cx))
                    .ok()
                    .flatten()
                else {
                    let _ = canvas.update(cx, |canvas, cx| {
                        canvas.raster_worker_active = false;
                        canvas.navigation_cache_active =
                            canvas.is_dragging || canvas.raster_staging_tiles.is_some();
                        canvas.set_rendering(false, cx);
                        cx.notify();
                    });
                    return;
                };
                let cache = cx
                    .background_spawn(async move { build_navigation_raster(input) })
                    .await;
                let _ = canvas.update(cx, |canvas, cx| {
                    if let Some(cache) = cache {
                        canvas.install_navigation_tile(target, index, cache);
                        canvas.request_navigation_overview(cx);
                    }
                    cx.notify();
                });
            }
        }));
    }

    fn raster_tiles_cover_canvas(&self) -> bool {
        self.raster_tiles.as_ref().is_some_and(|tiles| {
            tiles.content_revision == self.raster_content_revision
                && raster_tiles_cover_bounds(
                    tiles,
                    self.screen_bounds,
                    self.screen_bounds,
                    self.scale,
                    self.offset,
                )
        })
    }

    /// Recenter only after the current 5x5 field loses a half-viewport guard.
    /// Integral tile shifts retain the overlapping rows/columns, so ordinary
    /// panning schedules only the newly exposed outer ring.
    fn raster_cache_has_pan_margin(&self) -> bool {
        let Some(tiles) = self
            .raster_tiles
            .as_ref()
            .filter(|tiles| tiles.content_revision == self.raster_content_revision)
        else {
            return false;
        };
        if tiles.scale != self.scale || tiles.screen_viewport != self.screen_bounds.size {
            return false;
        }
        let guard = Size::new(tiles.tile_size.width / 2., tiles.tile_size.height / 2.);
        let guarded_canvas = Bounds::new(
            self.screen_bounds.origin - Point::new(guard.width, guard.height),
            Size::new(
                self.screen_bounds.size.width + guard.width * 2.,
                self.screen_bounds.size.height + guard.height * 2.,
            ),
        );
        raster_tiles_cover_bounds(
            tiles,
            self.screen_bounds,
            guarded_canvas,
            self.scale,
            self.offset,
        )
    }

    pub(crate) fn place_instance(&mut self, preview: InstancePreview, cx: &mut Context<Self>) {
        let (rects, polygons) = instance_preview_geometry(&preview.output, preview.cell);
        let tool = self.state.read(cx).tool.clone();
        tool.update(cx, |tool, cx| {
            *tool = ToolState::PlaceInstance(PlaceInstanceToolState {
                invocation: preview.invocation,
                scope_span: preview.scope_span,
                rects,
                polygons,
            });
            cx.notify();
        });
    }

    pub(crate) fn is_sse_dragging(&self) -> bool {
        self.is_sse_dragging
    }

    pub(crate) fn defer_snapshot(&mut self, snapshot: PreparedCompilationSnapshot) {
        self.deferred_snapshot = Some(snapshot);
    }

    pub(crate) fn take_deferred_snapshot(&mut self) -> Option<PreparedCompilationSnapshot> {
        self.deferred_snapshot.take()
    }

    fn sse_drag_delta_for_targets(
        targets: &[SseDragTarget],
        cell: &compile::CompiledCell,
        layout_delta: Point<f32>,
    ) -> Option<SparseVec> {
        let edges = targets
            .iter()
            .map(|target| SparseVec::from(&target.expr))
            .collect::<Vec<_>>();
        let deltas = targets
            .iter()
            .map(|target| {
                (target.normal.x * layout_delta.x + target.normal.y * layout_delta.y) as f64
            })
            .collect::<Vec<_>>();
        match &cell.sse_basis {
            // An empty sparse basis means there are no remaining constraint
            // rows. In that case the row-space representation is also empty,
            // and every still-unsolved variable is free to move.
            compile::SseBasis::Nullspace(vectors) if vectors.is_empty() => {
                crate::sse::drag_delta_multi(&edges, &[], &cell.unsolved_vars, &deltas)
            }
            compile::SseBasis::Nullspace(vectors) => {
                let vectors = vectors.iter().map(SparseVec::from).collect::<Vec<_>>();
                crate::sse::drag_delta_multi_nullspace(
                    &edges,
                    &vectors,
                    &cell.unsolved_vars,
                    &deltas,
                )
            }
            compile::SseBasis::Rowspace(vectors) => {
                let vectors = vectors.iter().map(SparseVec::from).collect::<Vec<_>>();
                crate::sse::drag_delta_multi(&edges, &vectors, &cell.unsolved_vars, &deltas)
            }
        }
    }

    fn sse_drag_delta(&self, cell: &compile::CompiledCell) -> Option<SparseVec> {
        let pixel_delta = (
            self.sse_delta.x.to_f64() as f32,
            self.sse_delta.y.to_f64() as f32,
        );
        Self::sse_drag_delta_for_targets(
            &self.sse_targets,
            cell,
            Point::new(
                crate::sse::edge_drag_distance(pixel_delta, (1., 0.), self.scale),
                crate::sse::edge_drag_distance(pixel_delta, (0., 1.), self.scale),
            ),
        )
    }

    fn sse_targets_support_2d(targets: &[SseDragTarget], cell: &compile::CompiledCell) -> bool {
        [Point::new(1., 0.), Point::new(0., 1.)]
            .into_iter()
            .all(|delta| Self::sse_drag_delta_for_targets(targets, cell, delta).is_some())
    }

    fn sse_target_supported(target: &SseDragTarget, cell: &compile::CompiledCell) -> bool {
        Self::sse_drag_delta_for_targets(std::slice::from_ref(target), cell, target.normal)
            .is_some()
    }

    fn draggable_point_targets(
        targets: Vec<SseDragTarget>,
        cell: &compile::CompiledCell,
    ) -> Vec<SseDragTarget> {
        if Self::sse_targets_support_2d(&targets, cell) {
            targets
        } else {
            // A point constrained in one axis (or to a one-dimensional path)
            // remains draggable through whichever coordinate controls its
            // remaining degree of freedom.
            targets
                .into_iter()
                .find(|target| Self::sse_target_supported(target, cell))
                .into_iter()
                .collect()
        }
    }

    fn selection_hits_at(&self, position: Point<Pixels>) -> Vec<SelectionHit> {
        let mut hits = Vec::new();

        for (paint_order, (rect, layer)) in self.rects.iter().enumerate() {
            let Some(span) = &rect.id else {
                continue;
            };
            let bounds = get_rect_bounds(rect, self.screen_bounds, self.scale, self.offset);
            if bounds.contains(&position) {
                hits.push(SelectionHit {
                    span: span.clone(),
                    outline: rect_selection_outline(bounds, rect.border_styles),
                    layer: SelectionLayer::Layout(layer.z),
                    creation_order: if rect.object_path.is_empty() {
                        vec![paint_order as u64]
                    } else {
                        rect.object_path
                            .iter()
                            .map(|id| id.creation_order())
                            .collect()
                    },
                });
            }
        }

        for (paint_order, (polygon, layer)) in self.polygons.iter().enumerate() {
            let Some(span) = &polygon.id else {
                continue;
            };
            let points = polygon
                .points
                .iter()
                .map(|point| self.layout_to_px(*point))
                .collect::<Vec<_>>();
            if point_in_polygon(position, &points) {
                let outline = if let Some(centerline) = &polygon.centerline {
                    SelectionOutline::Polyline {
                        points: centerline
                            .points
                            .iter()
                            .map(|point| self.layout_to_px(*point))
                            .collect(),
                        segment_styles: centerline.segment_styles.clone(),
                    }
                } else {
                    SelectionOutline::Polygon {
                        points,
                        edge_styles: polygon.edge_styles.clone(),
                    }
                };
                hits.push(SelectionHit {
                    span: span.clone(),
                    outline,
                    layer: SelectionLayer::Layout(layer.z),
                    creation_order: if polygon.object_path.is_empty() {
                        vec![paint_order as u64]
                    } else {
                        polygon
                            .object_path
                            .iter()
                            .map(|id| id.creation_order())
                            .collect()
                    },
                });
            }
        }

        for (paint_order, bbox) in self.scope_rects.iter().enumerate() {
            let Some(span) = &bbox.rect.id else {
                continue;
            };
            let bounds = get_rect_bounds(&bbox.rect, self.screen_bounds, self.scale, self.offset);
            if bounds.contains(&position) {
                hits.push(SelectionHit {
                    span: span.clone(),
                    outline: rect_selection_outline(bounds, bbox.rect.border_styles),
                    layer: SelectionLayer::Scope,
                    creation_order: if bbox.rect.object_path.is_empty() {
                        vec![paint_order as u64]
                    } else {
                        bbox.rect
                            .object_path
                            .iter()
                            .map(|id| id.creation_order())
                            .collect()
                    },
                });
            }
        }

        for (paint_order, hitbox) in self.dim_hitboxes.iter().enumerate() {
            for bounds in &hitbox.bounds {
                if bounds.contains(&position) {
                    hits.push(SelectionHit {
                        span: hitbox.span.clone(),
                        outline: SelectionOutline::Rect {
                            bounds: *bounds,
                            border_styles: Edges::all(BorderStyle::Solid),
                        },
                        layer: SelectionLayer::Overlay,
                        creation_order: vec![paint_order as u64],
                    });
                }
            }
        }

        ordered_selection_hits(hits)
    }

    pub(crate) fn fit_to_screen(&mut self, cx: &mut Context<Self>) {
        self.advance_raster_generation();
        if let Some(previous) = self.raster_tiles.take() {
            self.raster_images_to_drop
                .extend(previous.tiles.into_values().map(|cache| cache.image));
        }
        if let Some(previous) = self.raster_staging_tiles.take() {
            self.raster_images_to_drop
                .extend(previous.tiles.into_values().map(|cache| cache.image));
        }
        if let Some(previous) = self.raster_reprojection.take() {
            self.raster_images_to_drop.push(previous.image);
        }
        self.raster_display = None;
        self.raster_display_frozen = false;
        self.raster_tile_target = None;
        self.navigation_cache_active = false;
        self.raster_refinement = None;
        self.raster_worker_active = false;
        // Fitting invalidates the old worker but immediately schedules a
        // replacement on the next paint. Keep the activity handoff continuous
        // while a solved layout is still waiting to be rasterized.
        let has_solved_layout = self.state.read(cx).solved_cell.read(cx).is_some();
        self.set_rendering(has_solved_layout, cx);
        self.hover_hit = None;
        if let Some(cell) = self.state.read(cx).solved_cell.read(cx)
            && let Some(bbox) = &cell.state[&cell.selected_scope].bbox.as_ref().or_else(|| {
                let scope_address = &cell.state[&cell.selected_scope].address;
                cell.state[&cell.scope_paths[&ScopeAddress {
                    cell: scope_address.cell,
                    scope: cell.output.cells[&scope_address.cell].root,
                }]]
                    .bbox
                    .as_ref()
            })
        {
            self.scale = fit_scale(
                self.screen_bounds.size,
                (bbox.x1 - bbox.x0) as f32,
                (bbox.y1 - bbox.y0) as f32,
            );
            self.offset = Point::new(
                px((-(bbox.x0 + bbox.x1) as f32 * self.scale
                    + f32::from(self.screen_bounds.size.width))
                    / 2.),
                px(((bbox.y1 + bbox.y0) as f32 * self.scale
                    + f32::from(self.screen_bounds.size.height))
                    / 2.),
            );
        } else {
            self.offset = Point::new(px(0.), self.screen_bounds.size.height);
        }
        self.raster_scale_signal
            .store(self.scale.to_bits() as u64, Ordering::Release);
        cx.notify();
    }

    pub(crate) fn on_left_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let origin_coords = self.layout_to_px(Point::new(0., 0.));
        let y_axis = Edge {
            dir: Dir::Vert,
            coord: origin_coords.x,
            start: self.screen_bounds.origin.y,
            stop: self.screen_bounds.origin.y + self.screen_bounds.size.height,
        };
        let x_axis = Edge {
            dir: Dir::Horiz,
            coord: origin_coords.y,
            start: self.screen_bounds.origin.x,
            stop: self.screen_bounds.origin.x + self.screen_bounds.size.width,
        };
        let grid = self
            .state
            .read(cx)
            .solved_cell
            .read(cx)
            .as_ref()
            .map(|cell| cell.output.tech.grid_step())
            .unwrap_or(0.1);
        let layout_mouse_position = self.px_to_layout(event.position);
        let snapped_layout_mouse_position = snap_layout_point(layout_mouse_position, grid);
        let edit_dim = self.state.read(cx).tool.clone().update(cx, |tool, cx| {
            let mut edit_dim = false;
            match tool {
                ToolState::DrawRect(rect_tool) => {
                    let state = self.state.read(cx);
                    let layers = state.layers.read(cx);
                    if let Some(layer) = &layers.selected_layer
                        && let Some(layer_info) = layers.layers.get(layer)
                    {
                        if layer_info.visible {
                            if let Some(p0) = rect_tool.p0 {
                                rect_tool.p0 = None;
                                let p1 = snapped_layout_mouse_position;
                                let p0p = Point::new(f32::min(p0.x, p1.x), f32::min(p0.y, p1.y));
                                let p1p = Point::new(f32::max(p0.x, p1.x), f32::max(p0.y, p1.y));
                                self.state.update(cx, |state, cx| {
                                    let error: Option<SharedString> =
                                        state.solved_cell.update(cx, {
                                            |cell, cx| {
                                                if let Some(cell) = cell.as_mut() {
                                                    // TODO update in memory representation of code
                                                    // TODO add solver to gui
                                                    let scope_address =
                                                        &cell.state[&cell.selected_scope].address;
                                                    let reachable_objs =
                                                        cell.output.reachable_objs(
                                                            scope_address.cell,
                                                            scope_address.scope,
                                                        );
                                                    let names: IndexSet<_> =
                                                        reachable_objs.values().collect();
                                                    let scope = cell
                                                        .output
                                                        .cells
                                                        .get(&scope_address.cell)
                                                        .unwrap()
                                                        .scopes
                                                        .get(&scope_address.scope)
                                                        .unwrap();
                                                    let rect_name = (0..)
                                                        .map(|i| format!("rect{i}"))
                                                        .find(|name| !names.contains(name))
                                                        .unwrap();

                                                    match state.lang_server_client.draw_rect(
                                                        scope.span.clone(),
                                                        rect_name,
                                                        compile::BasicRect {
                                                            layer: state
                                                                .layers
                                                                .read(cx)
                                                                .selected_layer
                                                                .clone()
                                                                .map(|s| s.to_string()),
                                                            x0: draw_source_coordinate(p0p.x, grid),
                                                            y0: draw_source_coordinate(p0p.y, grid),
                                                            x1: draw_source_coordinate(p1p.x, grid),
                                                            y1: draw_source_coordinate(p1p.y, grid),
                                                            construction: false,
                                                        },
                                                    ) {
                                                        Ok(None) => Some(
                                                            SOURCE_EDIT_REJECTED_MESSAGE.into(),
                                                        ),
                                                        Ok(Some(_)) => None,
                                                        Err(_) => None,
                                                    }
                                                } else {
                                                    Some("no cell to edit".into())
                                                }
                                            }
                                        });
                                    if state.message.is_none()
                                        && let Some(error) = error
                                    {
                                        state.show_message(MessageType::ERROR, error);
                                    }
                                });
                            } else {
                                rect_tool.p0 = Some(snapped_layout_mouse_position);
                            }
                        } else {
                            let _ = state.lang_server_client.show_message(
                                MessageType::ERROR,
                                "Cannot draw on an invisible layer.",
                            );
                        }
                    } else {
                        let _ = state
                            .lang_server_client
                            .show_message(MessageType::ERROR, "No layer has been selected.");
                    }
                }
                ToolState::DrawPolygon(polygon_tool) => {
                    let state = self.state.read(cx);
                    let layers = state.layers.read(cx);
                    if let Some(layer) = &layers.selected_layer
                        && let Some(layer_info) = layers.layers.get(layer)
                    {
                        if layer_info.visible {
                            let cursor = snapped_layout_mouse_position;
                            let (point, constraint) = if event.modifiers.shift
                                && let Some(previous) = polygon_tool.points.last().copied()
                            {
                                let (point, constraint) = snap_draw_point(previous, cursor);
                                (point, Some(constraint))
                            } else {
                                (cursor, None)
                            };
                            if polygon_tool.points.last() != Some(&point) {
                                if let Some(constraint) = constraint {
                                    polygon_tool.constraints.push(segment_constraint_with_end(
                                        constraint,
                                        polygon_tool.points.len(),
                                    ));
                                }
                                polygon_tool.points.push(point);
                                cx.notify();
                            }
                        } else {
                            let _ = state.lang_server_client.show_message(
                                MessageType::ERROR,
                                "Cannot draw on an invisible layer.",
                            );
                        }
                    } else {
                        let _ = state
                            .lang_server_client
                            .show_message(MessageType::ERROR, "No layer has been selected.");
                    }
                }
                ToolState::DrawPath(path_tool) => {
                    let state = self.state.read(cx);
                    let layers = state.layers.read(cx);
                    if let Some(layer) = &layers.selected_layer
                        && let Some(layer_info) = layers.layers.get(layer)
                    {
                        if layer_info.visible {
                            let cursor = snapped_layout_mouse_position;
                            let (point, constraint) = if event.modifiers.shift
                                && let Some(previous) = path_tool.points.last().copied()
                            {
                                let (point, constraint) = snap_draw_point(previous, cursor);
                                (point, Some(constraint))
                            } else {
                                (cursor, None)
                            };
                            if path_tool.points.last() != Some(&point) {
                                if let Some(constraint) = constraint {
                                    path_tool.constraints.push(segment_constraint_with_end(
                                        constraint,
                                        path_tool.points.len(),
                                    ));
                                }
                                path_tool.points.push(point);
                                cx.notify();
                            }
                        } else {
                            let _ = state.lang_server_client.show_message(
                                MessageType::ERROR,
                                "Cannot draw on an invisible layer.",
                            );
                        }
                    } else {
                        let _ = state
                            .lang_server_client
                            .show_message(MessageType::ERROR, "No layer has been selected.");
                    }
                }
                ToolState::PlaceInstance(placement) => {
                    let x = argonc::tech::snap(f64::from(snapped_layout_mouse_position.x), grid);
                    let y = argonc::tech::snap(f64::from(snapped_layout_mouse_position.y), grid);
                    let result = self.state.read(cx).lang_server_client.place_instance(
                        placement.scope_span.clone(),
                        placement.invocation.clone(),
                        x,
                        y,
                    );
                    match result {
                        Ok(Some(scope_span)) => {
                            placement.scope_span = scope_span;
                            cx.notify();
                        }
                        Ok(None) => {
                            self.state.update(cx, |state, _cx| {
                                if state.message.is_none() {
                                    state.show_message(
                                        MessageType::ERROR,
                                        SOURCE_EDIT_REJECTED_MESSAGE,
                                    );
                                }
                            });
                        }
                        Err(_) => {}
                    }
                }
                ToolState::DrawDim(dim_tool) => {
                    let enter_entry_mode = if dim_tool.edges.len() < 2 {
                        let rects = ordered_dimension_rects(&self.rects, &self.scope_rects);
                        let scale = self.scale;
                        let offset = self.offset;
                        let mut selected = None;
                        if x_axis.select_bounds(SELECT_WIDTH).contains(&event.position) {
                            selected = Some(DimEdge::Y0);
                        }
                        if y_axis.select_bounds(SELECT_WIDTH).contains(&event.position) {
                            selected = Some(DimEdge::X0);
                        }
                        'rects: for (rect, r) in rects.into_iter().map(|r| {
                            (
                                r,
                                Bounds::new(
                                    Point::new(scale * px(r.x0), scale * px(-r.y1))
                                        + offset
                                        + self.screen_bounds.origin,
                                    Size::new(scale * px(r.x1 - r.x0), scale * px(r.y1 - r.y0)),
                                ),
                            )
                        }) {
                            for (name, edge_layout, edge_px) in [
                                (
                                    "y0",
                                    Edge {
                                        dir: Dir::Horiz,
                                        coord: rect.y0,
                                        start: rect.x0,
                                        stop: rect.x1,
                                    },
                                    Edge {
                                        dir: Dir::Horiz,
                                        coord: r.bottom(),
                                        start: r.left(),
                                        stop: r.right(),
                                    },
                                ),
                                (
                                    "y1",
                                    Edge {
                                        dir: Dir::Horiz,
                                        coord: rect.y1,
                                        start: rect.x0,
                                        stop: rect.x1,
                                    },
                                    Edge {
                                        dir: Dir::Horiz,
                                        coord: r.top(),
                                        start: r.left(),
                                        stop: r.right(),
                                    },
                                ),
                                (
                                    "x0",
                                    Edge {
                                        dir: Dir::Vert,
                                        coord: rect.x0,
                                        start: rect.y0,
                                        stop: rect.y1,
                                    },
                                    Edge {
                                        dir: Dir::Vert,
                                        coord: r.left(),
                                        start: r.top(),
                                        stop: r.bottom(),
                                    },
                                ),
                                (
                                    "x1",
                                    Edge {
                                        dir: Dir::Vert,
                                        coord: rect.x1,
                                        start: rect.y0,
                                        stop: rect.y1,
                                    },
                                    Edge {
                                        dir: Dir::Vert,
                                        coord: r.right(),
                                        start: r.top(),
                                        stop: r.bottom(),
                                    },
                                ),
                            ] {
                                let bounds = edge_px.select_bounds(SELECT_WIDTH);
                                if bounds.contains(&event.position) && !rect.object_path.is_empty()
                                {
                                    selected = Some(DimEdge::Edge((rect, name, edge_layout)));
                                    break 'rects;
                                }
                            }
                        }
                        let enter_entry_mode = !dim_tool.edges.is_empty();
                        match selected {
                            Some(DimEdge::Edge((r, name, edge))) => {
                                let resolved = {
                                    let cell = self.state.read(cx).solved_cell.read(cx);
                                    if let Some(cell) = cell
                                        && let selected_scope_addr =
                                            cell.state[&cell.selected_scope].address
                                        && let (true, path) =
                                            find_obj_path(&r.object_path, cell, selected_scope_addr)
                                        && let Some(exact) = exact_object_bounds(
                                            &r.object_path,
                                            cell,
                                            selected_scope_addr,
                                        )
                                        .and_then(|bounds| bounds.edge(name))
                                    {
                                        let path = path.join(".");
                                        Some((path, exact))
                                    } else {
                                        None
                                    }
                                };
                                if let Some((path, exact)) = resolved
                                    && dim_tool
                                        .edges
                                        .first()
                                        .map(|old_edge| {
                                            let old_dir = match old_edge {
                                                DimEdge::X0 => Dir::Vert,
                                                DimEdge::Y0 => Dir::Horiz,
                                                DimEdge::Edge(edge) => edge.display.dir,
                                            };
                                            old_dir == edge.dir
                                        })
                                        .unwrap_or(true)
                                {
                                    dim_tool.edges.push(DimEdge::Edge(DimensionEdge {
                                        path,
                                        name: name.to_string(),
                                        display: edge,
                                        exact,
                                    }));
                                    false
                                } else {
                                    enter_entry_mode
                                }
                            }
                            Some(DimEdge::X0) => {
                                if dim_tool
                                    .edges
                                    .first()
                                    .map(|old_edge| {
                                        let old_dir = match old_edge {
                                            DimEdge::X0 => return false,
                                            DimEdge::Y0 => return false,
                                            DimEdge::Edge(edge) => edge.display.dir,
                                        };
                                        old_dir == Dir::Vert
                                    })
                                    .unwrap_or(true)
                                {
                                    dim_tool.edges.push(DimEdge::X0);
                                }
                                false
                            }
                            Some(DimEdge::Y0) => {
                                if dim_tool
                                    .edges
                                    .first()
                                    .map(|old_edge| {
                                        let old_dir = match old_edge {
                                            DimEdge::X0 => return false,
                                            DimEdge::Y0 => return false,
                                            DimEdge::Edge(edge) => edge.display.dir,
                                        };
                                        old_dir == Dir::Horiz
                                    })
                                    .unwrap_or(true)
                                {
                                    dim_tool.edges.push(DimEdge::Y0);
                                }
                                false
                            }
                            _ => enter_entry_mode,
                        }
                    } else {
                        true
                    };
                    let state = self.state.read(cx);

                    if enter_entry_mode && let Some(cell) = state.solved_cell.read(cx) {
                        let selected_scope_addr = cell.state[&cell.selected_scope].address;
                        let grid = cell.output.tech.grid_step();

                        let pending = if dim_tool.edges.len() == 1
                            && let DimEdge::Edge(edge) = &dim_tool.edges[0]
                        {
                            let display = &edge.display;
                            let (left, right, coord, source_coord, horiz) = match display.dir {
                                Dir::Horiz => (
                                    "x0",
                                    "x1",
                                    snapped_layout_mouse_position.y,
                                    draw_source_coordinate(snapped_layout_mouse_position.y, grid),
                                    "true",
                                ),
                                Dir::Vert => (
                                    "y0",
                                    "y1",
                                    snapped_layout_mouse_position.x,
                                    draw_source_coordinate(snapped_layout_mouse_position.x, grid),
                                    "false",
                                ),
                            };

                            let distance = (edge.exact.stop - edge.exact.start).abs();
                            let value = compile::format_initial_condition(distance, grid);
                            let preview_value = format_dimension_label(distance, grid);
                            let coord_offset =
                                format_dimension_offset(source_coord - edge.exact.coord, grid);
                            Some((
                                DimensionParams {
                                    p: format!("{}.{}", edge.path, right),
                                    n: format!("{}.{}", edge.path, left),
                                    value: value.clone(),
                                    coord: format!("{}.{} {coord_offset}", edge.path, edge.name),
                                    pstop: format!("{}.{}", edge.path, edge.name),
                                    nstop: format!("{}.{}", edge.path, edge.name),
                                    horiz: horiz.to_string(),
                                },
                                value.clone(),
                                PendingDimensionPreview {
                                    p: display.stop,
                                    n: display.start,
                                    coord,
                                    pstop: display.coord,
                                    nstop: display.coord,
                                    horiz: display.dir == Dir::Horiz,
                                    value: preview_value,
                                },
                            ))
                        } else if dim_tool.edges.len() == 2 {
                            match (&dim_tool.edges[0], &dim_tool.edges[1]) {
                                (DimEdge::Edge(edge0), DimEdge::Edge(edge1)) => {
                                    let (left, right) = if edge0.exact.coord < edge1.exact.coord {
                                        (edge0, edge1)
                                    } else {
                                        (edge1, edge0)
                                    };
                                    let left_display = &left.display;
                                    let right_display = &right.display;
                                    let (start, stop, coord, source_coord, horiz) =
                                        match left_display.dir {
                                            Dir::Vert => (
                                                "y0",
                                                "y1",
                                                snapped_layout_mouse_position.y,
                                                draw_source_coordinate(
                                                    snapped_layout_mouse_position.y,
                                                    grid,
                                                ),
                                                "true",
                                            ),
                                            Dir::Horiz => (
                                                "x0",
                                                "x1",
                                                snapped_layout_mouse_position.x,
                                                draw_source_coordinate(
                                                    snapped_layout_mouse_position.x,
                                                    grid,
                                                ),
                                                "false",
                                            ),
                                        };

                                    let intended_coord_exact = (right.exact.start
                                        + right.exact.stop
                                        + left.exact.start
                                        + left.exact.stop)
                                        / 4.;
                                    let coord_offset = format_dimension_offset(
                                        source_coord - intended_coord_exact,
                                        grid / 4.,
                                    );
                                    let distance = (right.exact.coord - left.exact.coord).abs();
                                    let value = compile::format_initial_condition(distance, grid);
                                    let preview_value = format_dimension_label(distance, grid);
                                    Some((
                                        DimensionParams {
                                            p: format!("{}.{}", right.path, right.name),
                                            n: format!("{}.{}", left.path, left.name),
                                            value: value.clone(),
                                            coord: format!(
                                                "({}.{} + {}.{} + {}.{} + {}.{})/4. {coord_offset}",
                                                right.path,
                                                start,
                                                right.path,
                                                stop,
                                                left.path,
                                                start,
                                                left.path,
                                                stop,
                                            ),
                                            pstop: format!(
                                                "({}.{} + {}.{}) / 2.",
                                                right.path, start, right.path, stop,
                                            ),
                                            nstop: format!(
                                                "({}.{} + {}.{}) / 2.",
                                                left.path, start, left.path, stop,
                                            ),
                                            horiz: horiz.to_string(),
                                        },
                                        value.clone(),
                                        PendingDimensionPreview {
                                            p: right_display.coord,
                                            n: left_display.coord,
                                            coord,
                                            pstop: (right_display.start + right_display.stop) / 2.,
                                            nstop: (left_display.start + left_display.stop) / 2.,
                                            horiz: left_display.dir == Dir::Vert,
                                            value: preview_value,
                                        },
                                    ))
                                }
                                (DimEdge::X0 | DimEdge::Y0, DimEdge::Edge(edge))
                                | (DimEdge::Edge(edge), DimEdge::X0 | DimEdge::Y0) => {
                                    let display = &edge.display;
                                    let (start, stop, preview_coord, source_coord, horiz) =
                                        match display.dir {
                                            Dir::Vert => (
                                                "y0",
                                                "y1",
                                                snapped_layout_mouse_position.y,
                                                draw_source_coordinate(
                                                    snapped_layout_mouse_position.y,
                                                    grid,
                                                ),
                                                "true",
                                            ),
                                            Dir::Horiz => (
                                                "x0",
                                                "x1",
                                                snapped_layout_mouse_position.x,
                                                draw_source_coordinate(
                                                    snapped_layout_mouse_position.x,
                                                    grid,
                                                ),
                                                "false",
                                            ),
                                        };

                                    let intended_coord_exact =
                                        (edge.exact.start + edge.exact.stop) / 2.;
                                    let coord_offset = format_dimension_offset(
                                        source_coord - intended_coord_exact,
                                        grid / 2.,
                                    );
                                    let intended_coord = (display.start + display.stop) / 2.;

                                    let pnstop = format!(
                                        "({}.{} + {}.{}) / 2.",
                                        edge.path, start, edge.path, stop,
                                    );
                                    let coord_expr = format!("{pnstop} {coord_offset}");
                                    let exact_value = edge.exact.coord.abs();
                                    let value =
                                        compile::format_initial_condition(exact_value, grid);
                                    let preview_value = format_dimension_label(exact_value, grid);
                                    let (p, n, pstop, nstop, preview) = if edge.exact.coord < 0. {
                                        (
                                            "0.".to_string(),
                                            format!("{}.{}", edge.path, edge.name),
                                            coord_expr.clone(),
                                            pnstop,
                                            PendingDimensionPreview {
                                                p: 0.,
                                                n: display.coord,
                                                coord: preview_coord,
                                                pstop: preview_coord,
                                                nstop: intended_coord,
                                                horiz: display.dir == Dir::Vert,
                                                value: preview_value.clone(),
                                            },
                                        )
                                    } else {
                                        (
                                            format!("{}.{}", edge.path, edge.name),
                                            "0.".to_string(),
                                            pnstop,
                                            coord_expr.clone(),
                                            PendingDimensionPreview {
                                                p: display.coord,
                                                n: 0.,
                                                coord: preview_coord,
                                                pstop: intended_coord,
                                                nstop: preview_coord,
                                                horiz: display.dir == Dir::Vert,
                                                value: preview_value,
                                            },
                                        )
                                    };
                                    Some((
                                        DimensionParams {
                                            p,
                                            n,
                                            value: value.clone(),
                                            coord: coord_expr,
                                            pstop,
                                            nstop,
                                            horiz: horiz.to_string(),
                                        },
                                        value,
                                        preview,
                                    ))
                                }
                                _ => unreachable!(),
                            }
                        } else {
                            None
                        };
                        if let Some((params, value, preview)) = pending {
                            let scope_span = cell.output.cells[&selected_scope_addr.cell].scopes
                                [&selected_scope_addr.scope]
                                .span
                                .clone();
                            *tool = ToolState::EditDim(EditDimToolState {
                                dim: None,
                                pending: Some(Box::new(PendingDimension {
                                    scope_span,
                                    params,
                                    preview,
                                })),
                                original_value: SharedString::from(value),
                                dim_mode: true,
                            });
                            edit_dim = true;
                            cx.notify();
                        }
                    }
                }
                ToolState::Select(select_tool) => {
                    // Handles control individual edges/corners. Movable object
                    // bodies control a rectangle translation or instance origin.
                    let handle = self
                        .sse_handles
                        .iter()
                        .rev()
                        .find(|h| h.bounds.contains(&event.position))
                        .cloned();
                    if let Some(handle) = handle {
                        self.is_sse_persisting = false;
                        self.pending_sse_values.clear();
                        self.deferred_snapshot = None;
                        self.sse_persist_after_revision = None;
                        self.is_sse_dragging = true;
                        self.drag_start = event.position;
                        self.sse_delta = Point::default();
                        self.sse_targets = handle.targets;
                        cx.notify();
                    } else {
                        let selected_hit = choose_selection_hit(
                            self.selection_hits_at(event.position),
                            select_tool.selected_obj.as_ref(),
                            event.modifiers.platform,
                        );
                        if let Some(hit) = selected_hit {
                            let span = hit.span;
                            select_tool.selected_obj = Some(span.clone());
                            if let Some(body) = self
                                .sse_bodies
                                .iter()
                                .rev()
                                .find(|body| {
                                    body.span == span && body.bounds.contains(&event.position)
                                })
                                .cloned()
                            {
                                self.is_sse_persisting = false;
                                self.pending_sse_values.clear();
                                self.deferred_snapshot = None;
                                self.sse_persist_after_revision = None;
                                self.is_sse_dragging = true;
                                self.drag_start = event.position;
                                self.sse_delta = Point::default();
                                self.sse_targets = body.targets;
                            }
                            let _ = self
                                .state
                                .read(cx)
                                .lang_server_client
                                .select_rect(span.clone());
                        } else {
                            select_tool.selected_obj = None;
                        }
                        cx.notify();
                    }
                }
                _ => {}
            }
            edit_dim
        });
        if edit_dim {
            self.text_input
                .update(cx, |input, cx| input.start_dimension_edit(cx));
            window.focus(&self.text_input_focus_handle);
            window.prevent_default();
        }
    }

    fn layout_to_px(&self, pt: Point<f32>) -> Point<Pixels> {
        Point::new(self.scale * px(pt.x), self.scale * px(-pt.y))
            + self.offset
            + self.screen_bounds.origin
    }

    fn dimension_input_position(&self, cx: &App) -> Option<Point<Pixels>> {
        let ToolState::EditDim(edit) = self.state.read(cx).tool.read(cx) else {
            return None;
        };
        let global = self.layout_to_px(edit.label_position(&self.dim_hitboxes)?);
        let local = global - self.screen_bounds.origin;
        let max_x = f32::from(self.screen_bounds.size.width - px(140.)).max(0.);
        let max_y = f32::from(self.screen_bounds.size.height - px(28.)).max(0.);
        Some(Point::new(
            px(f32::from(local.x).clamp(0., max_x)),
            px(f32::from(local.y).clamp(0., max_y)),
        ))
    }

    fn px_to_layout(&self, pt: Point<Pixels>) -> Point<f32> {
        let pt = pt - self.offset - self.screen_bounds.origin;
        Point::new(f32::from(pt.x / self.scale), f32::from(-pt.y / self.scale))
    }

    pub(crate) fn draw_rect(&mut self, _: &DrawRect, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.read(cx).tool.clone().update(cx, |tool, cx| {
            if !tool.is_draw_rect() {
                *tool = ToolState::DrawRect(DrawRectToolState::default());
                cx.notify();
            }
        });
    }

    pub(crate) fn draw_polygon(
        &mut self,
        _: &DrawPolygon,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state.read(cx).tool.clone().update(cx, |tool, cx| {
            if !tool.is_draw_polygon() {
                *tool = ToolState::DrawPolygon(DrawPolygonToolState::default());
                cx.notify();
            }
        });
    }

    pub(crate) fn draw_path(&mut self, _: &DrawPath, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.read(cx).tool.clone().update(cx, |tool, cx| {
            if !tool.is_draw_path() {
                *tool = ToolState::DrawPath(DrawPathToolState::default());
                cx.notify();
            }
        });
    }

    pub(crate) fn finish_draw_points(
        &mut self,
        _: &Enter,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state.read(cx).tool.clone().update(cx, |tool, cx| {
            let (draw_points, constraints, is_path) = match tool {
                ToolState::DrawPolygon(tool) => (&mut tool.points, &mut tool.constraints, false),
                ToolState::DrawPath(tool) => (&mut tool.points, &mut tool.constraints, true),
                _ => return,
            };
            let minimum_points = if is_path { 2 } else { 3 };
            if draw_points.len() < minimum_points {
                let shape = if is_path { "path" } else { "polygon" };
                let count = if is_path { "two" } else { "three" };
                let _ = self.state.read(cx).lang_server_client.show_message(
                    MessageType::ERROR,
                    format!("A {shape} requires at least {count} points before pressing Enter."),
                );
                return;
            }
            let grid = self
                .state
                .read(cx)
                .solved_cell
                .read(cx)
                .as_ref()
                .map(|cell| cell.output.tech.grid_step())
                .unwrap_or(0.1);
            let points = draw_points
                .iter()
                .map(|point| {
                    (
                        draw_source_coordinate(point.x, grid),
                        draw_source_coordinate(point.y, grid),
                    )
                })
                .collect::<Vec<_>>();
            let source_constraints = constraints.clone();
            let Some(layer) = self
                .state
                .read(cx)
                .layers
                .read(cx)
                .selected_layer
                .as_ref()
                .map(ToString::to_string)
            else {
                let _ = self
                    .state
                    .read(cx)
                    .lang_server_client
                    .show_message(MessageType::ERROR, "No layer has been selected.");
                return;
            };

            let mut inserted = false;
            self.state.update(cx, |state, cx| {
                let error: Option<SharedString> = state.solved_cell.update(cx, |cell, _cx| {
                    let Some(cell) = cell.as_mut() else {
                        return Some("no cell to edit".into());
                    };
                    let scope_address = &cell.state[&cell.selected_scope].address;
                    let reachable_objs = cell
                        .output
                        .reachable_objs(scope_address.cell, scope_address.scope);
                    let names: IndexSet<_> = reachable_objs.values().collect();
                    let name_prefix = if is_path { "path" } else { "polygon" };
                    let object_name = (0..)
                        .map(|index| format!("{name_prefix}{index}"))
                        .find(|name| !names.contains(name))
                        .unwrap();
                    let scope_span = cell.output.cells[&scope_address.cell].scopes
                        [&scope_address.scope]
                        .span
                        .clone();
                    let result = if is_path {
                        state.lang_server_client.draw_path(
                            scope_span,
                            object_name,
                            PathParams {
                                layer: layer.clone(),
                                width: f64::from(DEFAULT_DRAW_PATH_WIDTH),
                                points: points.clone(),
                                constraints: source_constraints.clone(),
                            },
                        )
                    } else {
                        state.lang_server_client.draw_polygon(
                            scope_span,
                            object_name,
                            PolygonParams {
                                layer: layer.clone(),
                                points: points.clone(),
                                constraints: source_constraints.clone(),
                            },
                        )
                    };
                    match result {
                        Ok(Some(_)) => {
                            inserted = true;
                            None
                        }
                        Ok(None) => Some(SOURCE_EDIT_REJECTED_MESSAGE.into()),
                        Err(_) => None,
                    }
                });
                if state.message.is_none()
                    && let Some(error) = error
                {
                    state.show_message(MessageType::ERROR, error);
                }
            });
            if inserted {
                draw_points.clear();
                constraints.clear();
                cx.notify();
            }
        });
    }

    pub(crate) fn select_mode(
        &mut self,
        _: &SelectMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state.read(cx).tool.clone().update(cx, |tool, cx| {
            if !tool.is_select() {
                *tool = ToolState::Select(SelectToolState { selected_obj: None });
                cx.notify();
            }
        });
    }

    pub(crate) fn draw_dim(&mut self, _: &DrawDim, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.read(cx).tool.clone().update(cx, |tool, cx| {
            if !tool.is_draw_dim() {
                *tool = ToolState::DrawDim(DrawDimToolState::default());
                cx.notify();
            }
        });
    }

    pub(crate) fn fit_to_screen_action(
        &mut self,
        _: &Fit,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.fit_to_screen(cx);
    }

    pub(crate) fn edit_action(&mut self, _: &Edit, window: &mut Window, cx: &mut Context<Self>) {
        if let ToolState::Select(SelectToolState {
            selected_obj: Some(obj),
        }) = self.state.read(cx).tool.clone().read(cx)
            && let Some(hitbox) = self.dim_hitboxes.iter().find(|hitbox| &hitbox.span == obj)
        {
            let obj = obj.clone();
            let value = hitbox.value.clone();
            self.state.read(cx).tool.clone().update(cx, |tool, _cx| {
                *tool = ToolState::EditDim(EditDimToolState {
                    dim: Some(obj.clone()),
                    pending: None,
                    dim_mode: false,
                    original_value: value,
                })
            });
            self.text_input
                .update(cx, |input, cx| input.start_dimension_edit(cx));
            window.focus(&self.text_input_focus_handle);
            window.prevent_default();
            cx.notify();
        }
    }

    pub(crate) fn zero_hierarchy(
        &mut self,
        _: &Zero,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state.update(cx, |state, cx| {
            state.hierarchy_depth = 0;
            cx.notify();
        });
    }

    pub(crate) fn one_hierarchy(&mut self, _: &One, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.hierarchy_depth = 1;
            cx.notify();
        });
    }

    pub(crate) fn all_hierarchy(&mut self, _: &All, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.hierarchy_depth = usize::MAX;
            cx.notify();
        });
    }

    pub(crate) fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.read(cx).tool.clone().update(cx, |tool, cx| {
            match tool {
                ToolState::DrawRect(DrawRectToolState { p0: p0 @ Some(_) }) => {
                    *p0 = None;
                }
                ToolState::DrawPolygon(DrawPolygonToolState {
                    points,
                    constraints,
                }) if !points.is_empty() => {
                    points.clear();
                    constraints.clear();
                }
                ToolState::DrawPath(DrawPathToolState {
                    points,
                    constraints,
                }) if !points.is_empty() => {
                    points.clear();
                    constraints.clear();
                }
                ToolState::DrawDim(DrawDimToolState { edges }) if !edges.is_empty() => {
                    edges.clear();
                }
                ToolState::Select(SelectToolState { selected_obj }) => {
                    *selected_obj = None;
                }
                _ => {
                    *tool = ToolState::default();
                }
            }
            cx.notify();
        });
    }

    pub(crate) fn dark_mode(&mut self, _: &DarkMode, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.dark_mode = true;
            cx.notify();
        });
    }

    pub(crate) fn light_mode(
        &mut self,
        _: &LightMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state.update(cx, |state, cx| {
            state.dark_mode = false;
            cx.notify();
        });
    }

    pub(crate) fn on_middle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.begin_navigation();
        self.is_dragging = true;
        self.drag_start = event.position;
        self.offset_start = self.offset;
        self.request_navigation_overview(cx);
    }

    pub(crate) fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.mouse_position = event.position;
        self.shift_down = event.modifiers.shift;
        if self.is_dragging {
            self.offset = self.offset_start + (event.position - self.drag_start);
            self.hover_hit = None;
            self.request_navigation_overview(cx);
            self.update_raster_display_transform();
            if self.raster_reprojection.is_some() && self.refresh_raster_reprojection() {
                self.resolve_raster_display_freeze();
            }
            self.request_navigation_raster(cx);
        } else if self.is_sse_dragging {
            self.sse_delta = self.mouse_position - self.drag_start;
            self.hover_hit = None;
        } else {
            self.hover_hit = self.selection_hits_at(event.position).into_iter().next();
        }
        cx.notify();
    }

    pub(crate) fn on_middle_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_dragging = self.is_dragging;
        self.is_dragging = false;
        self.is_sse_dragging = false;
        self.sse_delta = Point::default();
        self.sse_targets.clear();
        if was_dragging {
            self.request_navigation_overview(cx);
            self.request_navigation_raster(cx);
        }
    }

    /// Computes replacements for existing fallbacks plus AST-aware requests to
    /// insert any missing geometry initial conditions on the first drag.
    fn sse_source_edits(&self, cx: &mut Context<Self>) -> DragPersistenceEdits {
        let solved = self.state.read(cx).solved_cell.read(cx);
        let Some(solved) = solved.as_ref() else {
            return DragPersistenceEdits::default();
        };
        let selected = &solved.state[&solved.selected_scope].address;
        let editable_cell = &solved.output.cells[&selected.cell];
        let Some(dv) = self.sse_drag_delta(editable_cell) else {
            return DragPersistenceEdits::default();
        };
        drag_persistence_edits(
            &editable_cell.fallback_constraints_used,
            &self.sse_targets,
            &dv,
            solved.output.tech.grid_step(),
        )
    }

    pub(crate) fn on_left_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let was_sse_dragging = self.is_sse_dragging;
        self.is_dragging = false;
        // Persist the drag by replacing existing fallbacks and inserting any
        // missing initial-condition kwargs before recompiling.
        if was_sse_dragging {
            let edits = self.sse_source_edits(cx);
            if edits.values.is_empty() && edits.initial_conditions.is_empty() {
                self.is_sse_dragging = false;
                self.pending_sse_values.clear();
                self.sse_delta = Point::default();
                self.sse_targets.clear();
            } else {
                let pending_initial_conditions = edits.initial_conditions.clone();
                match self
                    .state
                    .read(cx)
                    .lang_server_client
                    .update_values(edits.values, edits.initial_conditions)
                {
                    Ok(Some(applied_edits)) => {
                        self.is_sse_dragging = false;
                        self.is_sse_persisting = true;
                        self.pending_sse_values =
                            pending_sse_values(&applied_edits, &pending_initial_conditions);
                        self.sse_persist_after_revision = self.state.read(cx).compilation_revision;
                        // Anything deferred before the workspace edit was
                        // accepted belongs to the pre-drag source revision.
                        self.deferred_snapshot = None;
                        let selected_after_edits = {
                            let tool = self.state.read(cx).tool.read(cx);
                            match tool {
                                ToolState::Select(SelectToolState {
                                    selected_obj: Some(selected),
                                }) => Some(remap_span_after_value_edits(selected, &applied_edits)),
                                _ => None,
                            }
                        };
                        if let Some(selected) = selected_after_edits {
                            let tool = self.state.read(cx).tool.clone();
                            tool.update(cx, |tool, cx| {
                                if let ToolState::Select(select) = tool {
                                    select.selected_obj = Some(selected);
                                    cx.notify();
                                }
                            });
                        }
                    }
                    Ok(None) => {
                        self.is_sse_dragging = false;
                        self.pending_sse_values.clear();
                        self.sse_persist_after_revision = None;
                        self.sse_delta = Point::default();
                        self.sse_targets.clear();
                    }
                    Err(_) => {
                        self.is_sse_dragging = false;
                        self.pending_sse_values.clear();
                        self.sse_persist_after_revision = None;
                        self.sse_delta = Point::default();
                        self.sse_targets.clear();
                    }
                }
            }
        }
        cx.notify();
    }

    /// Ends the optimistic drag preview once a compile result based on the
    /// rewritten source has reached the GUI.
    pub(crate) fn accepts_snapshot(&self, snapshot: &PreparedCompilationSnapshot) -> bool {
        if !self.is_sse_persisting {
            return true;
        }
        if !snapshot_follows_revision(snapshot.revision, self.sse_persist_after_revision) {
            return false;
        }
        let data = match &snapshot.output {
            compile::CompileOutput::Valid(data) => Some(data),
            compile::CompileOutput::ExecErrors(output) => output.output.as_ref(),
            compile::CompileOutput::StaticErrors(_) | compile::CompileOutput::FatalParseErrors => {
                None
            }
        };
        self.pending_sse_values.is_empty()
            || data.is_some_and(|data| {
                compiled_data_matches_pending_sse(data, &self.pending_sse_values)
            })
    }

    pub(crate) fn finish_sse_persist(&mut self, cx: &mut Context<Self>) {
        if self.is_sse_persisting {
            self.is_sse_persisting = false;
            self.pending_sse_values.clear();
            self.sse_persist_after_revision = None;
            self.sse_delta = Point::default();
            self.sse_targets.clear();
            cx.notify();
        }
    }

    pub(crate) fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_dragging || self.is_sse_dragging || self.is_sse_persisting {
            // Do not allow zooming during a drag.
            return;
        }
        let new_scale = {
            let delta = event.delta.pixel_delta(px(20.));
            zoomed_scale(self.scale, f32::from(delta.y))
        };
        if new_scale == self.scale {
            return;
        }
        let previous_raster_display = self.raster_display;

        // screen = scale*world + b
        // world = (screen - b)/scale
        // (screen-b0)/scale0 = (screen-b1)/scale1
        // b1 = scale1/scale0*(b0-screen)+screen
        let a = new_scale / self.scale;
        let b0 = self.screen_bounds.origin + self.offset;
        let b1 = Point::new(a * (b0.x - event.position.x), a * (b0.y - event.position.y))
            + event.position;
        self.offset = b1 - self.screen_bounds.origin;
        self.scale = new_scale;
        let requested_raster_display = self.raster_display_transform_for_current_view();
        let has_safe_reprojection = self.refresh_raster_reprojection();
        self.raster_display = raster_zoom_display_transform(
            previous_raster_display,
            requested_raster_display,
            has_safe_reprojection,
        );
        self.raster_display_frozen = !has_safe_reprojection;
        self.request_navigation_overview(cx);
        // Retarget the exact renderer on every wheel event. The worker cancels
        // obsolete generations and immediately continues with the newest LOD.
        self.request_navigation_raster(cx);

        cx.notify();
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
struct ExactLayoutBounds {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl ExactLayoutBounds {
    fn transformed(
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        mat: TransformationMatrix,
        ofs: (f64, f64),
    ) -> Self {
        let p0 = ifmatvec(mat, (x0, y0));
        let p1 = ifmatvec(mat, (x1, y1));
        Self {
            x0: p0.0.min(p1.0) + ofs.0,
            y0: p0.1.min(p1.1) + ofs.1,
            x1: p0.0.max(p1.0) + ofs.0,
            y1: p0.1.max(p1.1) + ofs.1,
        }
    }

    fn edge(self, name: &str) -> Option<Edge<f64>> {
        match name {
            "x0" => Some(Edge {
                dir: Dir::Vert,
                coord: self.x0,
                start: self.y0,
                stop: self.y1,
            }),
            "x1" => Some(Edge {
                dir: Dir::Vert,
                coord: self.x1,
                start: self.y0,
                stop: self.y1,
            }),
            "y0" => Some(Edge {
                dir: Dir::Horiz,
                coord: self.y0,
                start: self.x0,
                stop: self.x1,
            }),
            "y1" => Some(Edge {
                dir: Dir::Horiz,
                coord: self.y1,
                start: self.x0,
                stop: self.x1,
            }),
            _ => None,
        }
    }
}

/// Resolve the same rectangle or collapsed-instance bbox that the canvas
/// displays, but retain the compiler's `f64` coordinates. This keeps source
/// dimension defaults on the compiler grid without re-snapping a rounded GUI
/// value.
fn exact_object_bounds(
    path: &[ObjectId],
    cell: &CompileOutputState,
    scope: ScopeAddress,
) -> Option<ExactLayoutBounds> {
    let mut current_scope = scope;
    let mut mat = TransformationMatrix::identity();
    let mut ofs = (0., 0.);

    for (index, object_id) in path.iter().enumerate() {
        let is_last = index + 1 == path.len();
        let object = cell.output.cells[&current_scope.cell]
            .objects
            .get(object_id)?;
        match object {
            SolvedValue::Rect(rect) if is_last => {
                return Some(ExactLayoutBounds::transformed(
                    rect.x0.0, rect.y0.0, rect.x1.0, rect.y1.0, mat, ofs,
                ));
            }
            SolvedValue::Instance(instance) => {
                let mut instance_mat = TransformationMatrix::identity();
                if instance.reflect {
                    instance_mat = instance_mat.reflect_vert();
                }
                instance_mat = instance_mat.rotate(instance.angle);
                let instance_ofs = ifmatvec(mat, (instance.x, instance.y));
                mat = mat * instance_mat;
                ofs = (instance_ofs.0 + ofs.0, instance_ofs.1 + ofs.1);
                current_scope = ScopeAddress {
                    cell: instance.cell,
                    scope: cell.output.cells[&instance.cell].root,
                };

                if is_last {
                    let scope_path = cell.scope_paths.get(&current_scope)?;
                    let bbox = cell.state.get(scope_path)?.bbox.as_ref()?;
                    return Some(ExactLayoutBounds::transformed(
                        bbox.x0, bbox.y0, bbox.x1, bbox.y1, mat, ofs,
                    ));
                }
            }
            _ => return None,
        }
    }
    None
}

pub(crate) fn find_obj_path(
    path: &[ObjectId],
    cell: &CompileOutputState,
    scope: ScopeAddress,
) -> (bool, Vec<String>) {
    let mut current_scope = scope;
    let mut string_path = Vec::new();
    let mut reachable = true;
    if path.is_empty() {
        panic!("need non-empty object path");
    }
    for obj in &path[0..path.len() - 1] {
        let mut reachable_objs = cell
            .output
            .reachable_objs(current_scope.cell, current_scope.scope);
        if let Some(name) = reachable_objs.swap_remove(obj)
            && let Some(inst) = cell.output.cells[&current_scope.cell].objects[obj].get_instance()
        {
            string_path.push(name);
            current_scope = ScopeAddress {
                cell: inst.cell,
                scope: cell.output.cells[&inst.cell].root,
            };
        } else {
            reachable = false;
            break;
        }
    }
    let obj = path.last().unwrap();
    let mut reachable_objs = cell
        .output
        .reachable_objs(current_scope.cell, current_scope.scope);
    if let Some(name) = reachable_objs.swap_remove(obj) {
        match &cell.output.cells[&current_scope.cell].objects[obj] {
            SolvedValue::Rect(_) | SolvedValue::Polygon(_) | SolvedValue::Path(_) => {
                string_path.push(name)
            }
            SolvedValue::Instance(_) => {
                string_path.push(name);
                string_path = vec![format!("bbox({})", string_path.join("."))];
            }
            _ => reachable = false,
        }
    } else {
        reachable = false;
    }
    (reachable, string_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dimension_input_uses_the_dimension_label_position() {
        let horizontal = PendingDimensionPreview {
            p: 4.,
            n: 10.,
            coord: 20.,
            pstop: 0.,
            nstop: 0.,
            horiz: true,
            value: "6.".to_owned(),
        };
        assert_eq!(horizontal.label_position(), Point::new(7., 20.));

        let vertical = PendingDimensionPreview {
            horiz: false,
            ..horizontal
        };
        assert_eq!(vertical.label_position(), Point::new(20., 7.));
    }

    #[test]
    fn raster_pixel_ranges_are_clipped_to_the_viewport() {
        assert_eq!(raster_pixel_range(-2.5, 2.2, 10), Some((0, 3)));
        assert_eq!(raster_pixel_range(8.1, 12., 10), Some((8, 10)));
        assert_eq!(raster_pixel_range(12., 14., 10), None);
        assert_eq!(raster_pixel_range(4., 4., 10), None);
    }

    #[test]
    fn outline_lod_classification_depends_on_size_not_pixel_phase() {
        let unresolved = Bounds::new(Point::new(px(1.2), px(2.2)), Size::new(px(1.7), px(20.)));
        assert!(!raster_bounds_have_resolvable_outline_gap(unresolved));
        assert!(!raster_bounds_have_resolvable_outline_gap(Bounds::new(
            Point::new(px(1.8), px(2.8)),
            unresolved.size,
        )));
        assert!(raster_bounds_have_resolvable_outline_gap(Bounds::new(
            Point::new(px(1.2), px(2.2)),
            Size::new(px(3.1), px(3.1)),
        )));
    }

    #[test]
    fn navigation_overview_uses_only_visible_layers_with_their_original_colors() {
        let (_, mut visible) = dimension_rect(0., 10., 0);
        visible.color = rgb(0xff2400);
        visible.border_color = rgb(0x123456);
        let (_, mut hidden) = dimension_rect(0., 10., 1);
        hidden.visible = false;
        let layers = IndexMap::from_iter([
            (visible.name.clone(), visible.clone()),
            (hidden.name.clone(), hidden),
        ]);

        let overview = navigation_overview_layers(&layers);

        assert_eq!(overview.len(), 1);
        assert_eq!(overview[&visible.name].color, visible.color);
        assert_eq!(overview[&visible.name].border_color, visible.border_color);
    }

    #[test]
    fn navigation_overview_maps_the_current_world_viewport() {
        let overview_bounds =
            Bounds::new(Point::new(px(22.), px(348.)), Size::new(px(160.), px(160.)));
        let overview = navigation_overview_viewport_for_extents_in_size(
            -50.,
            -50.,
            50.,
            50.,
            overview_bounds.size,
        );
        let current = ViewportTransform {
            size: Size::new(px(144.), px(144.)),
            screen_size: Size::new(px(144.), px(144.)),
            scale: 1.44,
            offset: Point::new(px(72.), px(72.)),
        };

        assert!((overview.scale - 1.44).abs() < 1e-5);
        assert_eq!(overview.offset, Point::new(px(80.), px(80.)));
        let mapped = navigation_overview_viewport_bounds(overview, current, overview_bounds);
        assert!((f32::from(mapped.origin.x) - 30.).abs() < 1e-4);
        assert!((f32::from(mapped.origin.y) - 356.).abs() < 1e-4);
        assert!((f32::from(mapped.size.width) - 144.).abs() < 1e-4);
        assert!((f32::from(mapped.size.height) - 144.).abs() < 1e-4);
    }

    #[test]
    fn navigation_overview_maps_pointer_positions_back_to_world_space() {
        let target = Bounds::new(Point::new(px(20.), px(30.)), Size::new(px(240.), px(120.)));
        let overview =
            navigation_overview_viewport_for_extents_in_size(-100., -50., 100., 50., target.size);

        let center =
            navigation_overview_world_point(overview, Point::new(px(140.), px(90.)), target);
        let macro_top_left = Point::new(
            target.origin.x + overview.offset.x - px(100. * overview.scale),
            target.origin.y + overview.offset.y - px(50. * overview.scale),
        );
        let top_left = navigation_overview_world_point(overview, macro_top_left, target);

        assert!((center.x - 0.).abs() < 1e-4);
        assert!((center.y - 0.).abs() < 1e-4);
        assert!((top_left.x + 100.).abs() < 1e-4);
        assert!((top_left.y - 50.).abs() < 1e-4);
    }

    #[test]
    fn navigation_overview_fits_the_cell_or_an_outside_macro_viewport() {
        let cell = RasterBvhBounds {
            min_x: -50.,
            min_y: -50.,
            max_x: 50.,
            max_y: 50.,
        };
        let target = Bounds::new(Point::default(), Size::new(px(160.), px(160.)));
        let cell_view =
            navigation_overview_viewport_for_extents_in_size(-50., -50., 50., 50., target.size);
        let cell_image_world = raster_viewport_world_bounds(cell_view);

        let zoomed_in = ViewportTransform {
            size: Size::new(px(20.), px(20.)),
            screen_size: Size::new(px(20.), px(20.)),
            scale: 1.,
            offset: Point::new(px(10.), px(10.)),
        };
        let zoomed_in_display =
            navigation_overview_display_viewport_for_bounds(cell, zoomed_in, target.size);
        let zoomed_in_image =
            navigation_overview_world_bounds(zoomed_in_display, cell_image_world, target);
        let zoomed_in_indicator =
            navigation_overview_viewport_bounds(zoomed_in_display, zoomed_in, target);
        assert!((f32::from(zoomed_in_image.size.width) - 160.).abs() < 1e-4);
        assert!((f32::from(zoomed_in_indicator.size.width) - 28.8).abs() < 1e-4);

        let zoomed_out = ViewportTransform {
            size: Size::new(px(200.), px(200.)),
            screen_size: Size::new(px(200.), px(200.)),
            scale: 1.,
            offset: Point::new(px(100.), px(100.)),
        };
        let zoomed_out_display =
            navigation_overview_display_viewport_for_bounds(cell, zoomed_out, target.size);
        let zoomed_out_image =
            navigation_overview_world_bounds(zoomed_out_display, cell_image_world, target);
        let zoomed_out_indicator = clamp_bounds_to_container(
            navigation_overview_viewport_bounds(zoomed_out_display, zoomed_out, target),
            target,
        );
        assert!((f32::from(zoomed_out_indicator.origin.x) - 8.).abs() < 1e-4);
        assert!((f32::from(zoomed_out_indicator.size.width) - 144.).abs() < 1e-4);
        assert!((f32::from(zoomed_out_image.origin.x) - 40.).abs() < 1e-4);
        assert!((f32::from(zoomed_out_image.size.width) - 80.).abs() < 1e-4);
        assert!(target.contains(&zoomed_out_indicator.origin));
        assert!(target.contains(&zoomed_out_indicator.bottom_right()));
    }

    #[test]
    fn raster_bvh_query_culls_items_and_preserves_emit_order() {
        let node = RasterBvhNode::build(vec![
            RasterBvhItem {
                emit_index: 8,
                bounds: RasterBvhBounds {
                    min_x: 100.,
                    min_y: 100.,
                    max_x: 110.,
                    max_y: 110.,
                },
            },
            RasterBvhItem {
                emit_index: 3,
                bounds: RasterBvhBounds {
                    min_x: 0.,
                    min_y: 0.,
                    max_x: 10.,
                    max_y: 10.,
                },
            },
            RasterBvhItem {
                emit_index: 1,
                bounds: RasterBvhBounds {
                    min_x: 4.,
                    min_y: 4.,
                    max_x: 6.,
                    max_y: 6.,
                },
            },
        ])
        .unwrap();
        let mut matches = Vec::new();
        node.query(
            RasterBvhBounds {
                min_x: 2.,
                min_y: 2.,
                max_x: 8.,
                max_y: 8.,
            },
            &mut matches,
        );
        matches.sort_unstable();

        assert_eq!(matches, vec![1, 3]);
    }

    #[test]
    fn raster_scope_query_correctly_inverts_reflection() {
        let world = RasterBvhBounds {
            min_x: 10.,
            min_y: 20.,
            max_x: 30.,
            max_y: 40.,
        };
        let reflected = TransformationMatrix::identity().reflect_vert();

        assert_eq!(
            raster_scope_query_bounds(world, reflected, (100., 200.), 0.),
            RasterBvhBounds {
                min_x: -90.,
                min_y: 160.,
                max_x: -70.,
                max_y: 180.,
            }
        );
    }

    #[test]
    fn indexed_hierarchy_raster_matches_linear_reference_pixel_for_pixel() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        std::fs::write(
            &source_path,
            r#"
cell bit() {
    let body = rect("met1", x0=0., y0=0., x1=8., y1=20.);
    let route = path("met1", 3,
        width=4.,
        x0=10., y0=0.,
        x1=30., y1=0.,
        x2=30., y2=20.,
    );
}
cell row() {
    let b0 = inst(bit(), x=0., y=0.);
    let b1 = inst(bit(), x=40., y=0.);
}
cell top() {
    let row0 = inst(row(), x=0., y=0.);
    let row1 = inst(row(), x=100., y=0., angle=90);
    let row2 = inst(row(), x=100., y=100., angle=180);
    let row3 = inst(row(), x=0., y=100., angle=270);
    let row4 = inst(row(), x=180., y=20.);
    let row5 = inst(row(), x=220., y=100., angle=90);
    let row6 = inst(row(), x=320., y=100., reflect=true);
    let row7 = inst(row(), x=420., y=100., angle=90, reflect=true);
}
"#,
        )
        .unwrap();
        let ast = argonc::parse::parse_workspace_with_std(&source_path).ast();
        let output = compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["top"],
                args: vec![],
            },
        )
        .unwrap_valid();
        let (flat_rects, flat_polygons) = instance_preview_geometry(&output, output.top);
        let solved = raster_test_compile_output_state(output);
        let layer = LayerState {
            name: "met1".into(),
            color: rgb(0x00aa44),
            fill: ShapeFill::Stippling,
            border_color: rgb(0x005522),
            visible: true,
            used: true,
            z: 0,
        };
        let layers = Arc::new(IndexMap::from_iter([(layer.name.clone(), layer)]));
        let mut viewports = vec![
            ViewportTransform {
                size: Size::new(px(128.), px(128.)),
                screen_size: Size::new(px(128.), px(128.)),
                scale: 1.,
                offset: Point::new(px(10.), px(80.)),
            },
            ViewportTransform {
                size: Size::new(px(128.), px(128.)),
                screen_size: Size::new(px(128.), px(128.)),
                scale: 4.,
                offset: Point::new(px(80.), px(80.)),
            },
            ViewportTransform {
                size: Size::new(px(128.), px(128.)),
                screen_size: Size::new(px(128.), px(128.)),
                scale: 3.,
                offset: Point::new(px(-150.), px(100.)),
            },
            ViewportTransform {
                size: Size::new(px(128.), px(128.)),
                screen_size: Size::new(px(128.), px(128.)),
                scale: 2.,
                offset: Point::new(px(-616.), px(244.)),
            },
        ];
        for index in 0..16 {
            let scale = 0.65 + index as f32 * 0.37;
            viewports.push(ViewportTransform {
                size: Size::new(px(128.), px(128.)),
                screen_size: Size::new(px(128.), px(128.)),
                scale,
                offset: Point::new(
                    px(-83.25 + index as f32 * 19.375),
                    px(41.75 + (index % 5) as f32 * 23.625),
                ),
            });
        }

        for (viewport_index, viewport) in viewports.into_iter().enumerate() {
            let indexed_input =
                raster_test_navigation_input(solved.clone(), layers.clone(), viewport, true);
            let mut linear_input = indexed_input.clone();
            linear_input.use_spatial_index = false;
            linear_input.spatial_index = Arc::new(RasterSpatialIndex::default());
            let indexed = build_navigation_raster(indexed_input).unwrap();
            let linear = build_navigation_raster(linear_input).unwrap();
            let reference = build_layout_raster(
                &flat_rects
                    .iter()
                    .cloned()
                    .map(|rect| (rect, layers["met1"].clone()))
                    .collect::<Vec<_>>(),
                &flat_polygons
                    .iter()
                    .cloned()
                    .map(|polygon| (polygon, layers["met1"].clone()))
                    .collect::<Vec<_>>(),
                &[],
                &[],
                viewport,
                &crate::theme::DARK_THEME,
                1,
            )
            .unwrap();
            if viewport.scale == 1. {
                // World (20, 0) is inside the route but outside the bit body.
                // This guards the background hierarchy renderer's path support,
                // which the immediate renderer already had.
                let planes = indexed.style_planes.as_deref().unwrap();
                let pixel = 40 * planes.width as usize + 15;
                assert_ne!(planes.stipple_on[pixel * 4 + 3], 0);
            }
            assert_raster_style_planes_equal(
                &format!("viewport {viewport_index}: indexed vs linear"),
                indexed.style_planes.as_deref().unwrap(),
                linear.style_planes.as_deref().unwrap(),
                0,
            );
            assert_raster_style_planes_equal(
                &format!("viewport {viewport_index}: indexed vs flattened"),
                indexed.style_planes.as_deref().unwrap(),
                reference.style_planes.as_deref().unwrap(),
                1,
            );
        }
    }

    #[test]
    fn raster_pixels_use_bgra_channel_order() {
        let mut pixel = [0; 4];
        blend_raster_pixel(
            &mut pixel,
            0,
            Rgba {
                r: 1.,
                g: 0.5,
                b: 0.,
                a: 1.,
            },
        );
        assert_eq!(pixel, [0, 128, 255, 255]);
    }

    #[test]
    fn retained_raster_expands_without_interpolating_layer_colors() {
        let image = image::RgbaImage::from_raw(2, 1, vec![255, 0, 0, 255, 0, 0, 255, 255]).unwrap();
        let expanded = expand_raster_for_display(image);

        assert_eq!(expanded.dimensions(), (4, 2));
        for y in 0..2 {
            assert_eq!(expanded.get_pixel(0, y).0, [255, 0, 0, 255]);
            assert_eq!(expanded.get_pixel(1, y).0, [255, 0, 0, 255]);
            assert_eq!(expanded.get_pixel(2, y).0, [0, 0, 255, 255]);
            assert_eq!(expanded.get_pixel(3, y).0, [0, 0, 255, 255]);
        }
    }

    #[test]
    fn style_planes_apply_stippling_in_destination_pixels() {
        let stipple_on = [255, 0, 0, 255].repeat(10);
        let stipple_off = vec![0; stipple_on.len()];
        let image = raster_from_style_planes(10, 1, &stipple_on, &stipple_off, 0).unwrap();

        for x in 0..10 {
            assert_eq!(
                image.get_pixel(x, 0).0[3] != 0,
                x == 0 || x == 5,
                "the half-resolution slash period must remain five pixels"
            );
        }
    }

    #[test]
    fn unresolved_stippled_rectangle_is_one_solid_lod_feature() {
        let (rect, mut layer) = dimension_rect(2., 12., 0);
        let rect = Rect {
            y0: 2.,
            y1: 3.,
            ..rect
        };
        layer.fill = ShapeFill::Stippling;
        layer.color = rgb(0xff0000);
        layer.border_color = rgb(0x0000ff);
        let cache = build_layout_raster(
            &[(rect, layer)],
            &[],
            &[],
            &[],
            ViewportTransform {
                size: Size::new(px(20.), px(20.)),
                screen_size: Size::new(px(20.), px(20.)),
                scale: 1.,
                offset: Point::new(px(0.), px(10.)),
            },
            &crate::theme::DARK_THEME,
            0,
        )
        .unwrap();
        let planes = cache.style_planes.unwrap();

        assert_eq!(planes.stipple_on, planes.stipple_off);
        assert!(
            planes
                .stipple_on
                .as_chunks::<4>()
                .0
                .contains(&[255, 0, 0, 255]),
            "the unresolved pair is retained as one opaque border-colored strip"
        );
        assert!(
            planes
                .outline_correction
                .as_ref()
                .is_some_and(|correction| correction.geometry.primitives.is_empty())
        );
    }

    #[test]
    fn tile_reprojection_restipples_instead_of_scaling_the_old_pattern() {
        let source_width = 5;
        let source_height = 5;
        let stipple_on = [255, 0, 0, 255].repeat(source_width * source_height);
        let stipple_off = vec![0; stipple_on.len()];
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(
            image::RgbaImage::new(10, 10),
        )]));
        let cache = LayoutRasterCache {
            image,
            style_planes: Some(Arc::new(RasterStylePlanes {
                width: source_width as u32,
                height: source_height as u32,
                stipple_on: stipple_on.into(),
                stipple_off: stipple_off.into(),
                outline_correction: Some(Arc::new(RasterOutlineCorrection {
                    geometry: RasterOutlineGeometry {
                        primitives: Arc::from([]),
                        source_work: 0,
                    },
                    sample_mask: Arc::from([0]),
                    samples: Arc::from([]),
                })),
            })),
            texts: Arc::from([]),
            scope_labels: Arc::from([]),
            viewport: Size::new(px(10.), px(10.)),
            screen_viewport: Size::new(px(20.), px(20.)),
            scale: 1.,
            offset: Point::default(),
            content_revision: 0,
        };
        let tiles = LayoutRasterTileSet {
            tiles: HashMap::from([(RasterTileIndex { x: 0, y: 0 }, cache)]),
            navigation: true,
            anchor_offset: Point::default(),
            tile_size: Size::new(px(10.), px(10.)),
            screen_viewport: Size::new(px(20.), px(20.)),
            scale: 1.,
            content_revision: 0,
            center: RasterTileIndex { x: 0, y: 0 },
        };
        let reprojected = reproject_raster_tiles(
            &tiles,
            Bounds::new(Point::default(), Size::new(px(20.), px(20.))),
            2.,
            Point::default(),
        )
        .unwrap();
        let bytes = reprojected.image.as_bytes(0).unwrap();
        let alpha = |x: usize, y: usize| bytes[(y * 20 + x) * 4 + 3];

        assert_eq!(alpha(0, 0), 255);
        assert_eq!(alpha(2, 0), 0);
        assert_eq!(alpha(10, 0), 255);
    }

    fn outline_test_tiles(
        width: u32,
        height: u32,
        primitives: Vec<RasterOutlinePrimitive>,
        source_work: usize,
    ) -> LayoutRasterTileSet {
        let underlay_pixel = [0, 0, 255, 255];
        let outline_pixel = [255, 0, 0, 255];
        let underlay = underlay_pixel.repeat(width as usize * height as usize);
        let geometry = RasterOutlineGeometry {
            primitives: primitives.into(),
            source_work,
        };
        let outline_pixels = raster_outline_candidate_pixels(&geometry, width, height);
        let mut completed = underlay.clone();
        for &pixel in &outline_pixels {
            completed[pixel * 4..pixel * 4 + 4].copy_from_slice(&outline_pixel);
        }
        let correction = build_raster_outline_correction(
            Some(geometry),
            Some(&outline_pixels),
            &completed,
            &completed,
            Some(&underlay),
            Some(&underlay),
        )
        .unwrap();
        let viewport = raster_logical_size(width, height);
        let cache = LayoutRasterCache {
            image: Arc::new(RenderImage::new(vec![image::Frame::new(
                image::RgbaImage::new(
                    f32::from(viewport.width) as u32,
                    f32::from(viewport.height) as u32,
                ),
            )])),
            style_planes: Some(Arc::new(RasterStylePlanes {
                width,
                height,
                stipple_on: completed.clone().into(),
                stipple_off: completed.into(),
                outline_correction: Some(correction),
            })),
            texts: Arc::from([]),
            scope_labels: Arc::from([]),
            viewport,
            screen_viewport: viewport,
            scale: 1.,
            offset: Point::default(),
            content_revision: 0,
        };
        LayoutRasterTileSet {
            tiles: HashMap::from([(RasterTileIndex { x: 0, y: 0 }, cache)]),
            navigation: true,
            anchor_offset: Point::default(),
            tile_size: viewport,
            screen_viewport: viewport,
            scale: 1.,
            content_revision: 0,
            center: RasterTileIndex { x: 0, y: 0 },
        }
    }

    #[test]
    fn exact_outline_reprojection_matches_the_replacement_lod() {
        let tiles = outline_test_tiles(
            8,
            8,
            vec![RasterOutlinePrimitive::Line {
                start: Point::new(2.25, 1.),
                stop: Point::new(2.25, 6.),
            }],
            6,
        );
        let bounds = Bounds::new(Point::default(), Size::new(px(16.), px(16.)));
        let reprojected = reproject_raster_tiles(&tiles, bounds, 2., Point::default()).unwrap();

        let mut expected = [0, 0, 255, 255].repeat(8 * 8);
        let mut target = RasterPaintTarget {
            buffer: &mut expected,
            composite: RasterComposite::Replace,
            touched: None,
        };
        stroke_raster_line(
            &mut target,
            8,
            8,
            Point::new(4.5, 2.),
            Point::new(4.5, 12.),
            rgb(0x0000ff),
        );
        let expected = expand_raster_for_display(
            image::RgbaImage::from_raw(8, 8, expected).expect("valid test raster"),
        );
        assert_eq!(reprojected.image.as_bytes(0).unwrap(), expected.as_raw());

        let bytes = reprojected.image.as_bytes(0).unwrap();
        let blue_columns = (0..16)
            .filter(|&x| bytes[(4 * 16 + x) * 4..(4 * 16 + x + 1) * 4] == [255, 0, 0, 255])
            .collect::<Vec<_>>();
        assert_eq!(
            blue_columns,
            [10, 11],
            "the outline stays two logical pixels wide"
        );
    }

    #[test]
    fn fractional_rectangle_reprojection_matches_the_replacement_lod() {
        let source_bounds =
            Bounds::new(Point::new(px(2.25), px(3.25)), Size::new(px(6.2), px(5.2)));
        let tiles = outline_test_tiles(
            16,
            16,
            vec![RasterOutlinePrimitive::Rect(source_bounds)],
            26,
        );
        let bounds = Bounds::new(Point::default(), Size::new(px(32.), px(32.)));
        let reprojected = reproject_raster_tiles(&tiles, bounds, 1.5, Point::default()).unwrap();

        let mut expected = [0, 0, 255, 255].repeat(16 * 16);
        let mut target = RasterPaintTarget {
            buffer: &mut expected,
            composite: RasterComposite::Replace,
            touched: None,
        };
        stroke_raster_rect(
            &mut target,
            16,
            16,
            Bounds::new(
                Point::new(source_bounds.origin.x * 1.5, source_bounds.origin.y * 1.5),
                Size::new(
                    source_bounds.size.width * 1.5,
                    source_bounds.size.height * 1.5,
                ),
            ),
            rgb(0x0000ff),
        );
        let expected = expand_raster_for_display(
            image::RgbaImage::from_raw(16, 16, expected).expect("valid test raster"),
        );
        assert_eq!(reprojected.image.as_bytes(0).unwrap(), expected.as_raw());
    }

    #[test]
    fn zoomed_out_outlines_coalesce_below_one_raster_pixel() {
        let tiles = outline_test_tiles(
            8,
            8,
            vec![
                RasterOutlinePrimitive::Line {
                    start: Point::new(2.1, 2.),
                    stop: Point::new(2.1, 6.),
                },
                RasterOutlinePrimitive::Line {
                    start: Point::new(2.8, 2.),
                    stop: Point::new(2.8, 6.),
                },
            ],
            10,
        );
        let reprojected = reproject_raster_tiles(
            &tiles,
            Bounds::new(Point::default(), Size::new(px(16.), px(16.))),
            0.5,
            Point::default(),
        )
        .unwrap();
        let bytes = reprojected.image.as_bytes(0).unwrap();
        let blue_columns = (0..16)
            .filter(|&x| {
                (0..16).any(|y| bytes[(y * 16 + x) * 4..(y * 16 + x + 1) * 4] == [255, 0, 0, 255])
            })
            .collect::<Vec<_>>();
        assert_eq!(blue_columns, [2, 3]);
    }

    #[test]
    fn dense_outline_policy_rerenders_instead_of_scaling_detailed_outlines() {
        let pixels = 300;
        assert!(raster_reprojection_scale_is_safe(2., 1.));
        assert_eq!(
            raster_outline_reprojection(2., Some(40), pixels),
            RasterOutlineReprojection::Exact
        );
        assert_eq!(
            raster_outline_reprojection(0.5, None, pixels),
            RasterOutlineReprojection::Rerender,
            "minification may not make isolated outlines thin or disappear"
        );
        assert_eq!(
            raster_outline_reprojection(2., None, pixels),
            RasterOutlineReprojection::Rerender
        );
        assert_eq!(
            raster_outline_reprojection(2., Some(51), pixels),
            RasterOutlineReprojection::Rerender
        );

        let previous = Some(RasterDisplayTransform {
            scale: 1.,
            offset: Point::new(px(10.), px(20.)),
        });
        let requested = Some(RasterDisplayTransform {
            scale: 2.,
            offset: Point::new(px(20.), px(40.)),
        });
        assert_eq!(
            raster_zoom_display_transform(previous, requested, false),
            previous,
            "a dense exact rerender must not scale the retained presentation"
        );
        assert_eq!(
            raster_zoom_display_transform(previous, requested, true),
            requested
        );
    }

    #[test]
    fn zoom_in_waits_for_fresh_geometry_even_when_reprojection_would_be_cheap() {
        assert_eq!(
            raster_outline_reprojection(2., Some(1), 10_000),
            RasterOutlineReprojection::Exact,
            "the low-level reprojection itself would accept this cheap outline"
        );
        assert!(!raster_reprojection_scale_is_safe(1., 2.));

        let retained = Some(RasterDisplayTransform {
            scale: 1.,
            offset: Point::new(px(10.), px(20.)),
        });
        let magnified = Some(RasterDisplayTransform {
            scale: 2.,
            offset: Point::new(px(20.), px(40.)),
        });
        assert_eq!(
            raster_zoom_display_transform(retained, magnified, false),
            retained,
            "a coarse fill plane must remain frozen until fresh geometry arrives"
        );
    }

    #[test]
    fn same_layer_overlap_is_idempotent_but_separate_layers_blend() {
        let color = Rgba {
            a: 0.5,
            ..rgb(0xff0000)
        };
        let mut first_layer = vec![0; 4];
        let mut first_touched = Vec::new();
        composite_raster_pixel(
            &mut first_layer,
            0,
            color,
            RasterComposite::Union,
            Some(&mut first_touched),
        );
        composite_raster_pixel(
            &mut first_layer,
            0,
            color,
            RasterComposite::Union,
            Some(&mut first_touched),
        );
        composite_raster_pixel(
            &mut first_layer,
            0,
            color,
            RasterComposite::Replace,
            Some(&mut first_touched),
        );
        assert_eq!(first_layer[3], 128);
        assert_eq!(first_touched, [0]);

        let mut output = vec![0; 4];
        blend_raster_layer(&mut output, &mut first_layer, &mut first_touched);
        assert_eq!(output[3], 128);
        assert_eq!(first_layer, [0; 4]);
        assert!(first_touched.is_empty());

        let mut second_layer = vec![0; 4];
        let mut second_touched = Vec::new();
        composite_raster_pixel(
            &mut second_layer,
            0,
            color,
            RasterComposite::Union,
            Some(&mut second_touched),
        );
        blend_raster_layer(&mut output, &mut second_layer, &mut second_touched);
        assert_eq!(output[3], 192);
    }

    #[test]
    fn raster_stippling_preserves_transparent_gaps() {
        let mut pixels = vec![0; 10 * 4];
        {
            let mut target = RasterPaintTarget {
                buffer: &mut pixels,
                composite: RasterComposite::Union,
                touched: None,
            };
            fill_raster_rect(
                &mut target,
                10,
                1,
                Bounds::new(Point::default(), Size::new(px(10.), px(1.))),
                ShapeFill::Stippling,
                Rgba {
                    r: 1.,
                    g: 0.,
                    b: 0.,
                    a: 1.,
                },
                0,
            );
        }
        let alphas = pixels
            .as_chunks::<4>()
            .0
            .iter()
            .map(|pixel| pixel[3])
            .collect::<Vec<_>>();
        assert_eq!(alphas, [255, 0, 0, 0, 0, 255, 0, 0, 0, 0]);
    }

    #[test]
    fn raster_stippling_is_anchored_to_layout_coordinates() {
        let period = (10. * RASTER_CACHE_RESOLUTION).round().max(1.) as i64;
        let world = (13_i64, 3_i64);
        for offset in [Point::new(px(0.), px(0.)), Point::new(px(7.), px(-4.))] {
            let phase = raster_stipple_phase(offset);
            let local_x = world.0 + f32::from(offset.x).round() as i64;
            let local_y = world.1 + f32::from(offset.y).round() as i64;
            assert_eq!(
                (local_x - local_y - phase).rem_euclid(period),
                (world.0 - world.1).rem_euclid(period)
            );
        }
    }

    #[test]
    fn navigation_tiles_are_scheduled_center_then_two_rings() {
        let center = RasterTileIndex { x: 7, y: -4 };
        let order = navigation_tile_order(center);
        assert_eq!(order.len(), 25);
        assert_eq!(order[0], center);
        assert!(
            order[1..9]
                .iter()
                .all(|index| { (index.x - center.x).abs().max((index.y - center.y).abs()) == 1 })
        );
        assert!(
            order[9..]
                .iter()
                .all(|index| { (index.x - center.x).abs().max((index.y - center.y).abs()) == 2 })
        );
        assert_eq!(order.iter().copied().collect::<HashSet<_>>().len(), 25);
    }

    #[test]
    fn zoom_level_handoff_requires_the_complete_inner_three_by_three() {
        let center = RasterTileIndex { x: 3, y: -2 };
        let order = navigation_tile_order(center);
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(
            image::RgbaImage::new(1, 1),
        )]));
        let cache = LayoutRasterCache {
            image,
            style_planes: None,
            texts: Arc::from([]),
            scope_labels: Arc::from([]),
            viewport: Size::new(px(1.), px(1.)),
            screen_viewport: Size::new(px(1.), px(1.)),
            scale: 1.,
            offset: Point::default(),
            content_revision: 0,
        };
        let mut tiles = HashMap::new();
        for index in order.iter().take(8) {
            tiles.insert(*index, cache.clone());
        }
        assert!(!navigation_inner_tiles_complete(&tiles, center));

        tiles.insert(order[8], cache);
        assert!(navigation_inner_tiles_complete(&tiles, center));
    }

    #[test]
    fn paint_does_not_restart_an_in_flight_first_tile() {
        assert!(paint_should_start_navigation_worker(true, false, false));
        assert!(!paint_should_start_navigation_worker(true, false, true));
        assert!(!paint_should_start_navigation_worker(true, true, false));
        assert!(!paint_should_start_navigation_worker(false, false, false));
    }

    #[test]
    fn five_by_five_tiles_cover_navigation_and_detect_missing_visible_tiles() {
        let image = image::RgbaImage::new(1, 1);
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(image)]));
        let tile_size = Size::new(px(100.), px(80.));
        let anchor_offset = Point::new(px(110.), px(80.));
        let cache = LayoutRasterCache {
            image,
            style_planes: None,
            texts: Arc::from([]),
            scope_labels: Arc::from([]),
            viewport: tile_size,
            screen_viewport: tile_size,
            scale: 2.,
            offset: anchor_offset,
            content_revision: 0,
        };
        let mut tiles = LayoutRasterTileSet {
            tiles: navigation_tile_order(RasterTileIndex { x: 0, y: 0 })
                .into_iter()
                .map(|index| {
                    let mut tile = cache.clone();
                    tile.offset = raster_tile_offset(anchor_offset, tile_size, index);
                    (index, tile)
                })
                .collect(),
            navigation: true,
            anchor_offset,
            tile_size,
            screen_viewport: tile_size,
            scale: 2.,
            content_revision: 0,
            center: RasterTileIndex { x: 0, y: 0 },
        };
        let canvas = Bounds::new(Point::default(), tile_size);
        assert_eq!(
            navigation_raster_capture_transform(&tiles),
            RasterDisplayTransform {
                scale: 2.,
                offset: anchor_offset,
            }
        );
        let shrunk_offset = Point::new(anchor_offset.x / 2., anchor_offset.y / 2.);
        assert!(navigation_raster_transform(&tiles, canvas, 1., shrunk_offset, None).is_some());
        assert!(
            navigation_raster_transform(
                &tiles,
                canvas,
                2.,
                anchor_offset + Point::new(px(190.), px(150.)),
                None,
            )
            .is_some()
        );

        tiles
            .tiles
            .retain(|index, _| *index == RasterTileIndex { x: 0, y: 0 });
        let contained_layout =
            Bounds::new(Point::new(px(10.), px(10.)), Size::new(px(20.), px(20.)));
        assert!(!raster_tiles_cover_bounds(
            &tiles,
            canvas,
            canvas,
            1.,
            shrunk_offset,
        ));
        assert_eq!(
            navigation_raster_transform(&tiles, canvas, 1., shrunk_offset, Some(contained_layout),),
            Some(RasterDisplayTransform {
                scale: 1.,
                offset: shrunk_offset,
            })
        );

        tiles.tiles.remove(&RasterTileIndex { x: 0, y: 0 });
        assert_eq!(
            navigation_raster_transform(&tiles, canvas, 2., anchor_offset, None),
            None
        );
    }

    #[test]
    fn fractional_rectangle_outline_stays_one_raster_pixel_wide() {
        let mut buffer = vec![0; 8 * 8 * 4];
        {
            let mut target = RasterPaintTarget {
                buffer: &mut buffer,
                composite: RasterComposite::Replace,
                touched: None,
            };
            stroke_raster_rect(
                &mut target,
                8,
                8,
                Bounds::new(Point::new(px(1.25), px(1.25)), Size::new(px(4.2), px(4.2))),
                Rgba {
                    a: 0.5,
                    ..rgb(0xff0000)
                },
            );
        }

        assert_eq!(
            buffer
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[3] != 0)
                .count(),
            16
        );
        assert!(
            buffer
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[3] != 0)
                .all(|pixel| pixel[3] == 128),
            "corners and straight edges must have identical alpha"
        );
        assert_eq!(buffer[(8 + 1) * 4 + 3], 128);
        assert_eq!(buffer[(2 * 8 + 2) * 4 + 3], 0);
    }

    #[test]
    fn rectangle_outline_does_not_turn_viewport_clipping_into_an_edge() {
        let mut buffer = vec![0; 8 * 8 * 4];
        let mut target = RasterPaintTarget {
            buffer: &mut buffer,
            composite: RasterComposite::Replace,
            touched: None,
        };
        stroke_raster_rect(
            &mut target,
            8,
            8,
            Bounds::new(Point::new(px(-5.), px(1.)), Size::new(px(9.), px(4.))),
            rgb(0xff0000),
        );

        let alpha = |x: usize, y: usize| buffer[(y * 8 + x) * 4 + 3];
        assert_eq!(alpha(0, 2), 0, "the viewport boundary is not geometry");
        assert_eq!(alpha(3, 2), 255, "the rectangle's real right edge remains");
        assert_eq!(
            alpha(0, 1),
            255,
            "the real horizontal edge is clipped normally"
        );
    }

    #[test]
    fn higher_layer_stipple_covers_lower_outlines_without_blending() {
        let width = 10;
        let height = 3;
        let mut output = vec![0; width * height * 4];
        let mut layer = vec![0; output.len()];
        let mut touched = Vec::new();
        {
            let mut target = RasterPaintTarget {
                buffer: &mut layer,
                composite: RasterComposite::Replace,
                touched: Some(&mut touched),
            };
            stroke_raster_line(
                &mut target,
                width as u32,
                height as u32,
                Point::new(0., 1.),
                Point::new(9., 1.),
                rgb(0xff0000),
            );
        }
        blend_raster_layer(&mut output, &mut layer, &mut touched);
        {
            let mut target = RasterPaintTarget {
                buffer: &mut layer,
                composite: RasterComposite::Union,
                touched: Some(&mut touched),
            };
            fill_raster_rect(
                &mut target,
                width as u32,
                height as u32,
                Bounds::new(Point::default(), Size::new(px(10.), px(3.))),
                ShapeFill::Stippling,
                rgb(0x0000ff),
                0,
            );
        }
        blend_raster_layer(&mut output, &mut layer, &mut touched);

        let pixel = |x: usize, y: usize| &output[(y * width + x) * 4..(y * width + x + 1) * 4];
        assert_eq!(pixel(1, 1), [255, 0, 0, 255]);
        assert_eq!(pixel(2, 1), [0, 0, 255, 255]);
    }

    #[test]
    fn overlapping_opaque_outlines_replace_without_bright_corners() {
        let width = 7;
        let height = 7;
        let bounds = Bounds::new(Point::new(px(1.), px(1.)), Size::new(px(5.), px(5.)));
        let mut output = vec![0; width * height * 4];
        let mut layer = vec![0; output.len()];
        let mut touched = Vec::new();
        for color in [rgb(0xff0000), rgb(0x0000ff)] {
            let mut target = RasterPaintTarget {
                buffer: &mut layer,
                composite: RasterComposite::Replace,
                touched: Some(&mut touched),
            };
            stroke_raster_rect(&mut target, width as u32, height as u32, bounds, color);
            blend_raster_layer(&mut output, &mut layer, &mut touched);
        }

        let blue = [255, 0, 0, 255];
        assert_eq!(&output[(width + 1) * 4..(width + 2) * 4], blue);
        assert_eq!(&output[(width + 3) * 4..(width + 4) * 4], blue);
        assert_eq!(&output[(3 * width + 1) * 4..(3 * width + 2) * 4], blue);
    }

    #[test]
    fn unresolved_geometry_is_a_single_opaque_outline_feature() {
        let bounds = Bounds::new(Point::new(px(1.1), px(1.1)), Size::new(px(0.2), px(0.2)));
        let mut occupancy = None;
        mark_raster_occupancy(&mut occupancy, 4, 4, bounds);
        mark_raster_occupancy(&mut occupancy, 4, 4, bounds);
        let mut buffer = vec![0; 4 * 4 * 4];

        let mut target = RasterPaintTarget {
            buffer: &mut buffer,
            composite: RasterComposite::Union,
            touched: None,
        };
        paint_raster_occupancy_color(&mut target, occupancy.as_ref().unwrap(), 16, rgb(0x0000ff));

        assert_eq!(&buffer[5 * 4..5 * 4 + 4], &[255, 0, 0, 255]);
        assert_eq!(
            buffer
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[3] != 0)
                .count(),
            1
        );
    }

    #[test]
    fn fractional_navigation_anchor_is_congruent_across_tiles() {
        let tile_size = raster_logical_size(501, 251);
        assert_eq!(tile_size, Size::new(px(1002.), px(502.)));
        let anchor = Point::new(px(1.1), px(3.7));
        let next = raster_tile_offset(anchor, tile_size, RasterTileIndex { x: 1, y: 0 });
        let anchor_raster = Point::new(
            anchor.x * RASTER_CACHE_RESOLUTION,
            anchor.y * RASTER_CACHE_RESOLUTION,
        );
        let next_raster = Point::new(
            next.x * RASTER_CACHE_RESOLUTION,
            next.y * RASTER_CACHE_RESOLUTION,
        );
        let anchor_phase = raster_stipple_phase(anchor_raster);
        let next_phase = raster_stipple_phase(next_raster);
        let period = (10. * RASTER_CACHE_RESOLUTION).round().max(1.) as i64;

        assert_eq!(
            (733_i64 - anchor_phase).rem_euclid(period),
            (232_i64 - next_phase).rem_euclid(period)
        );
    }

    #[test]
    fn tiny_stippled_cell_tiles_use_average_coverage_without_empty_phases() {
        let primitive = RasterTilePrimitive {
            coverage: Arc::<[u8]>::from(vec![u8::MAX; 4]),
            tile_width: 2,
            tile_height: 2,
            bounds: Bounds::new(Point::default(), Size::new(px(2.), px(2.))),
        };
        let mut buffer = vec![0; 2 * 2 * 4];
        let mut layer = dimension_rect(0., 1., 0).1;
        layer.fill = ShapeFill::Stippling;

        let mut target = RasterPaintTarget {
            buffer: &mut buffer,
            composite: RasterComposite::Union,
            touched: None,
        };
        paint_raster_tile(&mut target, 2, 2, &primitive, &layer);

        assert!(buffer.as_chunks::<4>().0.iter().all(|pixel| pixel[3] == 64));
    }

    #[test]
    fn subpixel_tile_polygons_still_leave_an_occupancy_hint() {
        let mut coverage = vec![0; 16];
        mark_tile_polygon(
            &mut coverage,
            4,
            4,
            &[
                Point::new(1.1, 1.1),
                Point::new(1.2, 1.1),
                Point::new(1.15, 1.2),
            ],
        );
        assert!(coverage.iter().any(|pixel| *pixel != 0));
    }

    #[test]
    fn visible_stippled_cell_tiles_keep_the_slash_pattern() {
        let primitive = RasterTilePrimitive {
            coverage: Arc::<[u8]>::from(vec![u8::MAX; 25]),
            tile_width: 5,
            tile_height: 5,
            bounds: Bounds::new(Point::default(), Size::new(px(5.), px(5.))),
        };
        let mut buffer = vec![0; 5 * 5 * 4];
        let mut layer = dimension_rect(0., 1., 0).1;
        layer.fill = ShapeFill::Stippling;

        let mut target = RasterPaintTarget {
            buffer: &mut buffer,
            composite: RasterComposite::Union,
            touched: None,
        };
        paint_raster_tile(&mut target, 5, 5, &primitive, &layer);

        assert_eq!(
            buffer
                .as_chunks::<4>()
                .0
                .iter()
                .filter(|pixel| pixel[3] != 0)
                .count(),
            5
        );
    }

    fn compile(
        ast: &argonc::parse::WorkspaceParseAst,
        input: argonc::compile::CompileInput<'_>,
    ) -> argonc::compile::CompileOutput {
        let tech = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tech/basic.tech.toml");
        argonc::compile::compile(
            ast,
            input,
            &argonc::WorkspaceConfig::default().with_tech(Some(tech)),
        )
    }

    fn raster_test_compile_output_state(output: CompiledData) -> CompileOutputState {
        fn visit_scope(
            output: &CompiledData,
            address: ScopeAddress,
            parent: Option<ScopeAddress>,
            parent_path: &[String],
            state: &mut IndexMap<editor::ScopePath, editor::ScopeState>,
            scope_paths: &mut IndexMap<ScopeAddress, editor::ScopePath>,
        ) -> (editor::ScopePath, Option<compile::Rect<f64>>) {
            let scope_info = &output.cells[&address.cell].scopes[&address.scope];
            let mut path = parent_path.to_vec();
            path.push(scope_info.name.clone());
            scope_paths.insert(address, path.clone());
            let emit = scope_info
                .emit
                .iter()
                .map(|(object, _)| *object)
                .collect::<Vec<_>>();
            let children = scope_info.children.iter().copied().collect::<Vec<_>>();
            let mut bbox = None;
            for object in emit {
                let object = output.cells[&address.cell].objects[&object].clone();
                let object_bbox = match object {
                    SolvedValue::Rect(rect) => Some(rect.to_float()),
                    SolvedValue::Polygon(polygon) => polygon.bbox(),
                    SolvedValue::Path(path) => path.bbox(),
                    SolvedValue::Instance(instance) => {
                        let child_address = ScopeAddress {
                            cell: instance.cell,
                            scope: output.cells[&instance.cell].root,
                        };
                        let (_, child_bbox) = visit_scope(
                            output,
                            child_address,
                            Some(address),
                            &path,
                            state,
                            scope_paths,
                        );
                        child_bbox.map(|child_bbox| {
                            let mut mat = TransformationMatrix::identity();
                            if instance.reflect {
                                mat = mat.reflect_vert();
                            }
                            mat = mat.rotate(instance.angle);
                            let transformed = RasterBvhBounds {
                                min_x: child_bbox.x0.min(child_bbox.x1),
                                min_y: child_bbox.y0.min(child_bbox.y1),
                                max_x: child_bbox.x0.max(child_bbox.x1),
                                max_y: child_bbox.y0.max(child_bbox.y1),
                            }
                            .transformed(mat, (instance.x, instance.y));
                            compile::Rect {
                                id: instance.id,
                                layer: None,
                                x0: transformed.min_x,
                                y0: transformed.min_y,
                                x1: transformed.max_x,
                                y1: transformed.max_y,
                                construction: true,
                                span: child_bbox.span,
                            }
                        })
                    }
                    SolvedValue::Dimension(dimension) => {
                        bbox = compile::bbox_dim_union(bbox, &dimension);
                        None
                    }
                    SolvedValue::Text(text) => {
                        bbox = compile::bbox_text_union(bbox, &text);
                        None
                    }
                };
                bbox = compile::bbox_union(bbox, object_bbox);
            }
            for child in children {
                let child_address = ScopeAddress {
                    cell: address.cell,
                    scope: child,
                };
                let (_, child_bbox) = visit_scope(
                    output,
                    child_address,
                    Some(address),
                    &path,
                    state,
                    scope_paths,
                );
                bbox = compile::bbox_union(bbox, child_bbox);
            }
            state.insert(
                path.clone(),
                editor::ScopeState {
                    name: scope_info.name.clone(),
                    address,
                    visible: true,
                    bbox: bbox.clone(),
                    parent,
                },
            );
            (path, bbox)
        }

        let output = Arc::new(output);
        let root = ScopeAddress {
            cell: output.top,
            scope: output.cells[&output.top].root,
        };
        let mut state = IndexMap::new();
        let mut scope_paths = IndexMap::new();
        let (selected_scope, _) =
            visit_scope(&output, root, None, &[], &mut state, &mut scope_paths);
        CompileOutputState {
            output,
            selected_scope,
            state: Arc::new(state),
            scope_paths: Arc::new(scope_paths),
        }
    }

    fn raster_test_navigation_input(
        solved_cell: CompileOutputState,
        layers: Arc<IndexMap<SharedString, LayerState>>,
        viewport: ViewportTransform,
        use_spatial_index: bool,
    ) -> NavigationRasterInput {
        NavigationRasterInput {
            solved_cell,
            layers,
            hierarchy_depth: usize::MAX,
            hide_external_geometry: false,
            viewport,
            text_color: rgb(0xffffff),
            include_text: false,
            content_revision: 1,
            content_revision_signal: Arc::new(AtomicU64::new(1)),
            scale_signal: Arc::new(AtomicU64::new(viewport.scale.to_bits() as u64)),
            cell_raster_tiles: Arc::new(Mutex::new(CellRasterTileCache::default())),
            spatial_index: Arc::new(RasterSpatialIndex::default()),
            use_spatial_index,
            cancel_if_generation_changes: None,
        }
    }

    fn assert_raster_style_planes_equal(
        context: &str,
        actual: &RasterStylePlanes,
        expected: &RasterStylePlanes,
        edge_inset: usize,
    ) {
        assert_eq!(
            (actual.width, actual.height),
            (expected.width, expected.height)
        );
        let actual_width = actual.width as usize;
        let actual_height = actual.height as usize;
        for (name, actual, expected) in [
            (
                "stipple-on",
                actual.stipple_on.as_ref(),
                expected.stipple_on.as_ref(),
            ),
            (
                "stipple-off",
                actual.stipple_off.as_ref(),
                expected.stipple_off.as_ref(),
            ),
        ] {
            for y in edge_inset..actual_height.saturating_sub(edge_inset) {
                for x in edge_inset..actual_width.saturating_sub(edge_inset) {
                    let pixel = y * actual_width + x;
                    for channel in 0..4 {
                        let byte = pixel * 4 + channel;
                        assert_eq!(
                            actual[byte], expected[byte],
                            "{context} {name} raster differs at pixel {pixel}, channel {channel}"
                        );
                    }
                }
            }
            assert_eq!(actual.len(), expected.len(), "{name} raster length");
        }
    }

    fn dimension_rect(x0: f32, size: f32, z: usize) -> (Rect, LayerState) {
        (
            Rect {
                x0,
                x1: x0 + size,
                y0: 0.,
                y1: size,
                id: None,
                object_path: Vec::new(),
                border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                border_styles: Edges::all(BorderStyle::Solid),
                cvars: None,
            },
            LayerState {
                name: format!("layer-{z}").into(),
                color: rgb(0),
                fill: ShapeFill::Solid,
                used: true,
                border_color: rgb(0),
                visible: true,
                z,
            },
        )
    }

    fn selection_hit(name: &str, layer: SelectionLayer, creation_order: u64) -> SelectionHit {
        let bounds = Bounds::new(Point::default(), Size::new(px(10.), px(10.)));
        SelectionHit {
            span: Span {
                path: std::path::PathBuf::from(format!("{name}.ar")),
                span: cfgrammar::Span::new(0, 1),
            },
            outline: SelectionOutline::Rect {
                bounds,
                border_styles: Edges::all(BorderStyle::Solid),
            },
            layer,
            creation_order: vec![creation_order],
        }
    }

    #[test]
    fn rectangle_selection_outline_preserves_unconstrained_edge_styles() {
        let styles = Edges {
            top: BorderStyle::Solid,
            right: BorderStyle::Dashed,
            bottom: BorderStyle::Dashed,
            left: BorderStyle::Solid,
        };
        let SelectionOutline::Rect { border_styles, .. } = rect_selection_outline(
            Bounds::new(Point::default(), Size::new(px(10.), px(10.))),
            styles,
        ) else {
            panic!("rectangle should produce a rectangular selection outline");
        };
        assert_eq!(border_styles, styles);
    }

    #[test]
    fn concave_polygon_hit_test_excludes_its_empty_bbox_region() {
        let polygon = [
            Point::new(px(0.), px(0.)),
            Point::new(px(10.), px(0.)),
            Point::new(px(10.), px(4.)),
            Point::new(px(4.), px(4.)),
            Point::new(px(4.), px(10.)),
            Point::new(px(0.), px(10.)),
        ];

        assert!(point_in_polygon(Point::new(px(2.), px(8.)), &polygon));
        assert!(point_in_polygon(Point::new(px(4.), px(8.)), &polygon));
        assert!(!point_in_polygon(Point::new(px(8.), px(8.)), &polygon));
        assert_eq!(polygon_area(&polygon), 64.);
    }

    #[test]
    fn path_segments_are_dashed_only_next_to_unconstrained_points() {
        assert_eq!(
            path_segment_styles(4, |index| index == 1),
            [BorderStyle::Dashed, BorderStyle::Dashed, BorderStyle::Solid,]
        );
        assert_eq!(
            path_segment_styles(3, |_| false),
            [BorderStyle::Solid, BorderStyle::Solid]
        );
    }

    #[test]
    fn shift_snaps_draw_points_to_octants() {
        let origin = Point::new(0., 0.);
        assert_eq!(
            snap_draw_point(origin, Point::new(8., 2.)),
            (Point::new(8., 0.), DrawSegmentConstraint::Horizontal(0))
        );
        assert_eq!(
            snap_draw_point(origin, Point::new(2., 8.)),
            (Point::new(0., 8.), DrawSegmentConstraint::Vertical(0))
        );
        assert_eq!(
            snap_draw_point(origin, Point::new(8., 5.)),
            (
                Point::new(6.5, 6.5),
                DrawSegmentConstraint::DiagonalPositive(0)
            )
        );
        assert_eq!(
            snap_draw_point(origin, Point::new(-8., 5.)),
            (
                Point::new(-6.5, 6.5),
                DrawSegmentConstraint::DiagonalNegative(0)
            )
        );
    }

    #[test]
    fn drawn_coordinates_discard_f32_representation_noise() {
        assert_eq!(draw_source_coordinate(110.3_f32, 0.1), 110.3);
        assert_eq!(draw_source_coordinate(110.37_f32, 0.25), 110.25);
    }

    #[test]
    fn dimension_labels_only_show_grid_relevant_decimals() {
        assert_eq!(format_dimension_label(500., 5.), "500");
        assert_eq!(format_dimension_label(502.5, 2.5), "502.5");
        assert_eq!(format_dimension_label(0.9050000000001, 0.005), "0.905");
    }

    #[test]
    fn dimension_offsets_are_clean_float_expressions() {
        assert_eq!(format_dimension_offset(15.0000001, 5.), "+ 15.");
        assert_eq!(format_dimension_offset(-2.5000001, 2.5), "- 2.5");
    }

    #[test]
    fn dotted_segments_keep_screen_space_spacing_when_rotated() {
        let horizontal = dotted_segment_centers(Point::default(), Point::new(px(70.), px(0.)));
        let diagonal =
            dotted_segment_centers(Point::default(), Point::new(px(49.497475), px(49.497475)));
        assert_eq!(horizontal.len(), diagonal.len());
        for centers in [&horizontal, &diagonal] {
            for pair in centers.windows(2) {
                let dx = f32::from(pair[1].x - pair[0].x);
                let dy = f32::from(pair[1].y - pair[0].y);
                assert!(dx.hypot(dy) > f32::from(DOT_DIAMETER));
            }
        }
    }

    #[test]
    fn path_centerline_points_are_draggable() {
        let source =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/path/lib.ar");
        let ast = argonc::parse::parse_workspace_with_std(&source).ast();
        let output = compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["initial_path"],
                args: vec![],
            },
        );
        let output = match output {
            compile::CompileOutput::Valid(output) => output,
            compile::CompileOutput::ExecErrors(output) => output.output.unwrap(),
            output => panic!("path fixture should compile: {output:?}"),
        };
        let cell = &output.cells[&output.top];
        let path = cell
            .objects
            .values()
            .find_map(SolvedValue::get_path)
            .expect("fixture should contain a path");
        let styles = path_segment_styles(path.points.len(), |index| {
            let (x, y) = &path.points[index];
            x.1.coeffs
                .iter()
                .chain(&y.1.coeffs)
                .any(|(_, var)| cell.unsolved_vars.contains(var))
        });
        assert_eq!(styles, [BorderStyle::Dashed, BorderStyle::Dashed]);

        let (x, y) = &path.points[2];
        let targets = LayoutCanvas::draggable_point_targets(corner_sse_targets(&x.1, &y.1), cell);
        assert_eq!(targets.len(), 2);
        assert!(LayoutCanvas::sse_targets_support_2d(&targets, cell));
    }

    #[test]
    fn polygon_edges_touching_a_one_axis_free_point_are_dashed() {
        let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/polygon/lib.ar");
        let ast = argonc::parse::parse_workspace_with_std(&source).ast();
        let output = compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["one_axis_point"],
                args: vec![],
            },
        );
        let output = match output {
            compile::CompileOutput::Valid(output) => output,
            compile::CompileOutput::ExecErrors(output) => output.output.unwrap(),
            output => panic!("one-axis polygon fixture should compile: {output:?}"),
        };
        let cell = &output.cells[&output.top];
        let polygon = cell
            .objects
            .values()
            .find_map(SolvedValue::get_polygon)
            .expect("fixture should contain a polygon");
        let styles = polygon_edge_styles(polygon.points.len(), |index| {
            let (x, y) = &polygon.points[index];
            x.1.coeffs
                .iter()
                .chain(&y.1.coeffs)
                .any(|(_, var)| cell.unsolved_vars.contains(var))
        });
        assert_eq!(
            styles,
            [BorderStyle::Solid, BorderStyle::Dashed, BorderStyle::Dashed,]
        );

        let (x, y) = &polygon.points[2];
        let targets = LayoutCanvas::draggable_point_targets(corner_sse_targets(&x.1, &y.1), cell);
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].expr, x.1);
        let drag = LayoutCanvas::sse_drag_delta_for_targets(&targets, cell, Point::new(12., 20.))
            .expect("the free x coordinate should drag");
        assert!((crate::sse::dot(&SparseVec::from(&x.1), &drag) - 12.).abs() < 1e-6);
        assert!(crate::sse::dot(&SparseVec::from(&y.1), &drag).abs() < 1e-6);
    }

    #[test]
    fn polygon_vertex_drag_moves_both_axes_and_produces_source_edits() {
        let source = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/polygon/lib.ar");
        let ast = argonc::parse::parse_workspace_with_std(&source).ast();
        let output = compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["initial_points"],
                args: vec![],
            },
        );
        let output = match output {
            compile::CompileOutput::Valid(output) => output,
            compile::CompileOutput::ExecErrors(output) => output.output.unwrap(),
            output => panic!("polygon fixture should compile: {output:?}"),
        };
        let cell = &output.cells[&output.top];
        let polygon = cell
            .objects
            .values()
            .find_map(SolvedValue::get_polygon)
            .expect("fixture should contain a polygon");
        let (x, y) = &polygon.points[2];
        let call_span = polygon.span.clone().expect("polygon should have source");
        let targets = vec![
            SseDragTarget {
                expr: x.1.clone(),
                normal: Point::new(1., 0.),
                source: Some(SseSourceTarget {
                    call_span: call_span.clone(),
                    name: "x2i".to_owned(),
                    value: x.0,
                }),
            },
            SseDragTarget {
                expr: y.1.clone(),
                normal: Point::new(0., 1.),
                source: Some(SseSourceTarget {
                    call_span,
                    name: "y2i".to_owned(),
                    value: y.0,
                }),
            },
        ];
        assert!(LayoutCanvas::sse_targets_support_2d(&targets, cell));

        let drag = LayoutCanvas::sse_drag_delta_for_targets(&targets, cell, Point::new(12.3, -4.5))
            .expect("both vertex axes should be draggable");
        assert!((crate::sse::dot(&SparseVec::from(&x.1), &drag) - 12.3).abs() < 1e-6);
        assert!((crate::sse::dot(&SparseVec::from(&y.1), &drag) + 4.5).abs() < 1e-6);

        let edits = drag_persistence_edits(&cell.fallback_constraints_used, &targets, &drag, 0.1);
        let x_fallback = cell
            .fallback_constraints_used
            .iter()
            .find(|fallback| {
                matches!(
                    fallback.initial_condition,
                    Some(RectInitialCondition::PolygonX(_, 2))
                )
            })
            .unwrap();
        let y_fallback = cell
            .fallback_constraints_used
            .iter()
            .find(|fallback| {
                matches!(
                    fallback.initial_condition,
                    Some(RectInitialCondition::PolygonY(_, 2))
                )
            })
            .unwrap();
        assert_eq!(edits.values.len(), 2);
        assert_eq!(edits.initial_conditions.len(), 2);
        assert!(
            edits
                .values
                .iter()
                .any(|edit| edit.span == x_fallback.span && edit.value == "62.3")
        );
        assert!(
            edits
                .values
                .iter()
                .any(|edit| edit.span == y_fallback.span && edit.value == "70.5")
        );

        let inserted = drag_persistence_edits(&[], &targets, &drag, 0.1);
        assert!(inserted.values.is_empty());
        assert!(inserted.initial_conditions.iter().any(|edit| {
            edit.name == "x2i"
                && edit.value == "62.3"
                && edit.call_span == polygon.span.clone().unwrap()
        }));
        assert!(inserted.initial_conditions.iter().any(|edit| {
            edit.name == "y2i"
                && edit.value == "70.5"
                && edit.call_span == polygon.span.clone().unwrap()
        }));
    }

    #[test]
    fn first_drag_requests_missing_rectangle_and_instance_initial_conditions() {
        let mut solver = argonc::solver::Solver::new();
        let rect_x0: LinearExpr = solver.new_var().into();
        let rect_y1: LinearExpr = solver.new_var().into();
        let inst_x: LinearExpr = solver.new_var().into();
        let rect_span = Span {
            path: std::path::PathBuf::from("/virtual/lib.ar"),
            span: cfgrammar::Span::new(10, 24),
        };
        let inst_span = Span {
            path: rect_span.path.clone(),
            span: cfgrammar::Span::new(30, 40),
        };
        let targets = [
            SseDragTarget {
                expr: rect_x0.clone(),
                normal: Point::new(1., 0.),
                source: Some(SseSourceTarget {
                    call_span: rect_span.clone(),
                    name: "x0i".to_owned(),
                    value: 10.,
                }),
            },
            SseDragTarget {
                expr: rect_y1.clone(),
                normal: Point::new(0., 1.),
                source: Some(SseSourceTarget {
                    call_span: rect_span.clone(),
                    name: "y1i".to_owned(),
                    value: 20.,
                }),
            },
            SseDragTarget {
                expr: inst_x.clone(),
                normal: Point::new(1., 0.),
                source: Some(SseSourceTarget {
                    call_span: inst_span.clone(),
                    name: "xi".to_owned(),
                    value: 100.,
                }),
            },
        ];
        let drag = SparseVec(
            [
                (rect_x0.coeffs[0].1, 5.),
                (rect_y1.coeffs[0].1, -3.),
                (inst_x.coeffs[0].1, 12.),
            ]
            .into_iter()
            .collect(),
        );

        let edits = drag_persistence_edits(&[], &targets, &drag, 0.1);
        assert!(edits.values.is_empty());
        assert!(edits.initial_conditions.iter().any(|edit| {
            edit.call_span == rect_span && edit.name == "x0i" && edit.value == "15."
        }));
        assert!(edits.initial_conditions.iter().any(|edit| {
            edit.call_span == rect_span && edit.name == "y1i" && edit.value == "17."
        }));
        assert!(edits.initial_conditions.iter().any(|edit| {
            edit.call_span == inst_span && edit.name == "xi" && edit.value == "112."
        }));
    }

    #[test]
    fn normalized_rect_keeps_edge_metadata_on_the_physical_edge() {
        let rect = Rect {
            x0: 20.,
            x1: 10.,
            y0: 40.,
            y1: 30.,
            id: None,
            object_path: Vec::new(),
            border_widths: Edges {
                top: px(1.),
                right: px(2.),
                bottom: px(3.),
                left: px(4.),
            },
            border_styles: Edges {
                top: BorderStyle::Dashed,
                right: BorderStyle::Solid,
                bottom: BorderStyle::Solid,
                left: BorderStyle::Dashed,
            },
            cvars: None,
        }
        .normalized();

        assert_eq!((rect.x0, rect.x1, rect.y0, rect.y1), (10., 20., 30., 40.));
        assert_eq!(rect.border_widths.left, px(2.));
        assert_eq!(rect.border_widths.right, px(4.));
        assert_eq!(rect.border_widths.top, px(3.));
        assert_eq!(rect.border_widths.bottom, px(1.));
        assert_eq!(rect.border_styles.left, BorderStyle::Solid);
        assert_eq!(rect.border_styles.right, BorderStyle::Dashed);
        assert_eq!(rect.border_styles.top, BorderStyle::Solid);
        assert_eq!(rect.border_styles.bottom, BorderStyle::Dashed);
    }

    #[test]
    fn crossed_initial_condition_values_are_sorted() {
        assert_eq!(
            sorted_initial_condition_values(150., 100.),
            Some((100., 150.))
        );
        assert_eq!(sorted_initial_condition_values(100., 150.), None);
        assert_eq!(sorted_initial_condition_values(100., 100.), None);
    }

    #[test]
    fn zoom_out_has_no_ui_scale_floor() {
        let below_old_floor = zoomed_scale(0.01, -20.);
        assert!(below_old_floor < 0.01);
        assert!(zoomed_scale(below_old_floor, -20.) < below_old_floor);
    }

    #[test]
    fn fit_scale_handles_point_and_line_bounds() {
        let viewport = Size::new(px(1000.), px(500.));
        assert_eq!(fit_scale(viewport, 0., 0.), 1.);
        assert_eq!(fit_scale(viewport, 100., 0.), 9.);
        assert_eq!(fit_scale(viewport, 0., 100.), 4.5);
        assert_eq!(fit_scale(viewport, 100., 100.), 4.5);
    }

    #[test]
    fn horizontal_canvas_reflow_keeps_the_layout_origin_stationary() {
        let previous_bounds =
            Bounds::new(Point::new(px(200.), px(80.)), Size::new(px(800.), px(600.)));
        let next_bounds = Bounds::new(Point::new(px(280.), px(80.)), Size::new(px(720.), px(600.)));
        let offset = Point::new(px(350.), px(240.));
        let next_offset = offset_after_horizontal_reflow(offset, previous_bounds, next_bounds);

        assert_eq!(
            previous_bounds.origin.x + offset.x,
            next_bounds.origin.x + next_offset.x
        );
        assert_eq!(next_offset.y, offset.y);

        let right_sidebar_reflow = Bounds::new(
            previous_bounds.origin,
            Size::new(px(700.), previous_bounds.size.height),
        );
        assert_eq!(
            offset_after_horizontal_reflow(offset, previous_bounds, right_sidebar_reflow),
            offset
        );
    }

    #[test]
    fn layout_points_snap_to_the_technology_grid() {
        assert_eq!(
            snap_layout_point(Point::new(1.12, -0.62), 0.25),
            Point::new(1., -0.5)
        );
        assert_eq!(
            snap_layout_point(Point::new(502.4, -507.6), 5.),
            Point::new(500., -510.)
        );
    }

    #[test]
    fn selected_span_tracks_value_edits_before_and_inside_rect() {
        let path = std::path::PathBuf::from("lib.ar");
        let selected = Span {
            path: path.clone(),
            span: cfgrammar::Span::new(10, 30),
        };
        let edits = [
            ValueEdit {
                span: Span {
                    path: path.clone(),
                    span: cfgrammar::Span::new(2, 5),
                },
                value: "12345".to_owned(),
            },
            ValueEdit {
                span: Span {
                    path,
                    span: cfgrammar::Span::new(15, 18),
                },
                value: "12345".to_owned(),
            },
        ];

        let remapped = remap_span_after_value_edits(&selected, &edits);
        assert_eq!(remapped.span, cfgrammar::Span::new(12, 34));
    }

    #[test]
    fn pending_drag_values_track_their_compiled_source_spans() {
        let path = std::path::PathBuf::from("lib.ar");
        let edits = [
            ValueEdit {
                span: Span {
                    path: path.clone(),
                    span: cfgrammar::Span::new(10, 19),
                },
                value: "1.2".to_owned(),
            },
            ValueEdit {
                span: Span {
                    path,
                    span: cfgrammar::Span::new(30, 32),
                },
                value: "0.".to_owned(),
            },
        ];

        let pending = pending_sse_values(&edits, &[]);
        assert_eq!(pending.len(), 2);
        assert_eq!(
            pending[0].span.as_ref().unwrap().span,
            cfgrammar::Span::new(10, 13)
        );
        assert_eq!(pending[0].value, 1.2);
        assert_eq!(
            pending[1].span.as_ref().unwrap().span,
            cfgrammar::Span::new(24, 26)
        );
        assert_eq!(pending[1].value, 0.);
    }

    #[test]
    fn pending_drag_values_include_inserted_initial_conditions() {
        let conditions = [InitialConditionEdit {
            call_span: Span {
                path: std::path::PathBuf::from("lib.ar"),
                span: cfgrammar::Span::new(10, 30),
            },
            name: "x0i".to_owned(),
            value: "12.5".to_owned(),
        }];

        let pending = pending_sse_values(&[], &conditions);
        assert_eq!(pending.len(), 1);
        assert!(pending[0].span.is_none());
        assert_eq!(pending[0].value, 12.5);
    }

    #[test]
    fn stale_compile_output_does_not_finish_a_drag_preview() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        std::fs::write(
            &source_path,
            "cell top() {\n    let shape = rect(\"met1\", x0i=1.2, y0i=0., x1i=10.3, y1i=10.)!;\n}\n",
        )
        .unwrap();
        let ast = argonc::parse::parse_workspace_with_std(&source_path).ast();
        let output = compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["top"],
                args: vec![],
            },
        );
        let output = match output {
            compile::CompileOutput::Valid(output) => output,
            compile::CompileOutput::ExecErrors(compile::ExecErrorCompileOutput {
                output: Some(output),
                ..
            }) => output,
            output => panic!("drag fixture should produce geometry: {output:?}"),
        };
        let fallback = output.cells[&output.top]
            .fallback_constraints_used
            .first()
            .expect("rectangle should use its initial conditions");
        let mut pending = vec![PendingSseValue {
            span: Some(fallback.span.clone()),
            value: -fallback.constraint.constant,
        }];

        assert!(compiled_data_matches_pending_sse(&output, &pending));
        pending[0].value += 1.;
        assert!(!compiled_data_matches_pending_sse(&output, &pending));
    }

    #[test]
    fn drag_preview_only_accepts_a_newer_compilation_revision() {
        assert!(snapshot_follows_revision(4, None));
        assert!(!snapshot_follows_revision(3, Some(3)));
        assert!(!snapshot_follows_revision(2, Some(3)));
        assert!(snapshot_follows_revision(4, Some(3)));
    }

    #[test]
    fn dimension_coordinate_follows_dragged_solver_expression() {
        let mut solver = argonc::solver::Solver::new();
        let edge = solver.new_var();
        let field = (10., LinearExpr::from(edge));
        let drag = SparseVec([(edge, 7.5)].into_iter().collect());

        assert_eq!(solved_linear_after_drag(&field, Some(&drag)), 17.5);
        assert_eq!(solved_linear_after_drag(&field, None), 10.);
    }

    #[test]
    fn dimension_defaults_keep_compiler_precision() {
        let bounds = ExactLayoutBounds {
            x0: 0.,
            y0: 50_000.005,
            x1: 10.,
            y1: 50_000.910,
        };
        let edge = bounds.edge("x0").unwrap();
        let value = compile::format_initial_condition(edge.stop - edge.start, 0.005);

        // Reducing these coordinates to f32 first produces 0.90625, which was
        // previously presented as the off-grid default `0.906`.
        assert_eq!(value, "0.905");
    }

    #[test]
    fn selection_prefers_last_created_object_on_the_same_layer() {
        let hits = vec![
            selection_hit("first", SelectionLayer::Layout(3), 0),
            selection_hit("last", SelectionLayer::Layout(3), 1),
        ];

        let selected = choose_selection_hit(hits, None, false).unwrap();
        assert_eq!(selected.span.path, std::path::PathBuf::from("last.ar"));
    }

    #[test]
    fn selection_prefers_higher_layers_before_creation_order() {
        let hits = vec![
            selection_hit("new-low", SelectionLayer::Layout(2), 100),
            selection_hit("old-high", SelectionLayer::Layout(3), 0),
        ];

        let selected = choose_selection_hit(hits, None, false).unwrap();
        assert_eq!(selected.span.path, std::path::PathBuf::from("old-high.ar"));
    }

    #[test]
    fn dimension_edges_use_normal_selection_priority() {
        let rects = vec![
            dimension_rect(10., 10., 2),
            dimension_rect(20., 100., 3),
            dimension_rect(30., 5., 3),
        ];

        let ordered = ordered_dimension_rects(&rects, &[]);

        // Higher z wins; among equal-z shapes, the later-created one wins.
        assert_eq!(
            ordered.iter().map(|rect| rect.x0).collect::<Vec<_>>(),
            [30., 20., 10.]
        );
    }

    #[test]
    fn command_click_cycles_through_hits_and_wraps() {
        let first = selection_hit("first", SelectionLayer::Layout(3), 0);
        let last = selection_hit("last", SelectionLayer::Layout(3), 1);
        let hits = vec![first.clone(), last.clone()];

        let next = choose_selection_hit(hits.clone(), Some(&last.span), true).unwrap();
        assert_eq!(next.span, first.span);
        let wrapped = choose_selection_hit(hits, Some(&first.span), true).unwrap();
        assert_eq!(wrapped.span, last.span);
    }

    #[test]
    fn instance_preview_flattens_nested_translated_geometry() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        std::fs::write(
            &source_path,
            r#"
cell leaf(width: Float) {
    let shape = rect("met1", x0=0., y0=0., x1=width, y1=5.)!;
}
cell child(width: Float) {
    let leaf_instance = inst(leaf(width), x=3., y=4.);
}
cell top() {
    let child_instance = inst(child(10.));
}
"#,
        )
        .unwrap();
        let ast = argonc::parse::parse_workspace_with_std(&source_path).ast();
        let output = compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["top"],
                args: vec![],
            },
        );
        let output = match output {
            argonc::compile::CompileOutput::Valid(output) => output,
            argonc::compile::CompileOutput::ExecErrors(
                argonc::compile::ExecErrorCompileOutput {
                    output: Some(output),
                    ..
                },
            ) => output,
            output => panic!("preview fixture should compile: {output:?}"),
        };

        let (rects, polygons) = instance_preview_geometry(&output, output.top);
        assert_eq!(rects.len(), 1);
        assert!(polygons.is_empty());
        assert_eq!(
            (rects[0].x0, rects[0].y0, rects[0].x1, rects[0].y1),
            (3., 4., 13., 9.)
        );
    }

    #[test]
    fn dimension_path_uses_bbox_for_a_collapsed_instance() {
        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        std::fs::write(
            &source_path,
            r#"
cell child() {
    let shape = rect("met1", x0=0., y0=0., x1=10., y1=5.);
}
cell top() {
    let child_instance = inst(child(), x=3., y=4.);
}
"#,
        )
        .unwrap();
        let ast = argonc::parse::parse_workspace_with_std(&source_path).ast();
        let output = compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["top"],
                args: vec![],
            },
        )
        .unwrap_valid();
        let top = output.top;
        let top_cell = &output.cells[&top];
        let top_root = top_cell.root;
        let instance = top_cell
            .objects
            .values()
            .find_map(|object| object.get_instance())
            .unwrap();
        let instance_id = instance.id;
        let child_cell = &output.cells[&instance.cell];
        let child_rect_id = child_cell
            .objects
            .values()
            .find_map(|object| object.get_rect().map(|rect| rect.id))
            .unwrap();
        let state = CompileOutputState {
            output: Arc::new(output),
            selected_scope: Vec::new(),
            state: Arc::default(),
            scope_paths: Arc::default(),
        };
        let scope = ScopeAddress {
            cell: top,
            scope: top_root,
        };

        assert_eq!(
            find_obj_path(&[instance_id], &state, scope),
            (true, vec!["bbox(child_instance)".to_owned()])
        );
        assert_eq!(
            find_obj_path(&[instance_id, child_rect_id], &state, scope),
            (true, vec!["child_instance".to_owned(), "shape".to_owned()])
        );
        assert_eq!(
            exact_object_bounds(&[instance_id, child_rect_id], &state, scope),
            Some(ExactLayoutBounds {
                x0: 3.,
                y0: 4.,
                x1: 13.,
                y1: 9.,
            })
        );
    }
}
