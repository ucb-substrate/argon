use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Debug,
    ops::{Add, Sub},
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
    BorderStyle, Bounds, Context, Corners, DefiniteLength, Edges, Element, Entity, FocusHandle,
    Focusable, Half, InteractiveElement, IntoElement, Length, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, ParentElement, PathBuilder, Pixels, Point, Render,
    Rgba, ScrollWheelEvent, SharedString, Size, Style, Styled, Subscription, TextRun, Window, div,
    pattern_slash, px, rgb, size, solid_background,
};
use indexmap::IndexSet;
use itertools::Itertools;
use tower_lsp_server::ls_types::MessageType;

use crate::{
    actions::*,
    editor::{self, CompileOutputState, EditorState, LayerState, ScopeAddress},
    sse::SparseVec,
};

#[derive(Copy, Clone, PartialEq)]
pub enum ShapeFill {
    Stippling,
    Solid,
}

const SELECT_WIDTH: Pixels = px(3.);
const DEFAULT_BORDER_WIDTH: Pixels = px(2.);
const DEFAULT_DRAW_PATH_WIDTH: f32 = 20.;
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
        let mut edge = PathBuilder::stroke(width);
        if edge_styles.get(index) == Some(&BorderStyle::Dashed) {
            edge = edge.dash_array(&[px(6.), px(5.)]);
        }
        edge.move_to(points[index]);
        edge.line_to(points[(index + 1) % points.len()]);
        if let Ok(path) = edge.build() {
            window.paint_path(path, color);
        }
    }
}

fn paint_polygon_fill(window: &mut Window, points: &[Point<Pixels>], layer: &LayerState) {
    let mut fill = PathBuilder::fill();
    fill.add_polygon(points, true);
    if let Ok(path) = fill.build() {
        let background = match layer.fill {
            ShapeFill::Solid => solid_background(layer.color),
            ShapeFill::Stippling => pattern_slash(layer.color.into(), 1., 9.),
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
        let mut segment = PathBuilder::stroke(width);
        if segment_styles.get(index) == Some(&BorderStyle::Dashed) {
            segment = segment.dash_array(&[px(6.), px(5.)]);
        }
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

fn draw_source_coordinate(value: f32) -> f64 {
    (f64::from(value) * 10.).round() / 10.
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

#[derive(Debug, Default, Clone)]
pub(crate) struct DrawDimToolState {
    pub(crate) edges: Vec<DimEdge<(String, String, Edge<f32>)>>,
}

#[derive(Debug, Clone)]
pub(crate) struct EditDimToolState {
    pub(crate) dim: Option<Span>,
    pub(crate) pending: Option<Box<PendingDimension>>,
    pub(crate) original_value: SharedString,
    /// `true` if entered from dimension tool
    pub(crate) dim_mode: bool,
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
    pub offset: Point<Pixels>,
    pub bg_style: Style,
    pub state: Entity<EditorState>,
    // SSE state
    is_sse_dragging: bool,
    // Keep displaying the final drag preview after mouse-up until the analyzer
    // sends back the result compiled from the rewritten initial conditions.
    is_sse_persisting: bool,
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
    shift_down: bool,
    // zoom state
    scale: f32,
    screen_bounds: Bounds<Pixels>,
    #[allow(unused)]
    subscriptions: Vec<Subscription>,
    rects: Vec<(Rect, LayerState)>,
    polygons: Vec<(Polygon, LayerState)>,
    scope_rects: Vec<LabeledBbox>,
    dim_hitboxes: Vec<(Span, Vec<Bounds<Pixels>>, SharedString)>,
    // True if waiting on render step to finish some initialization.
    //
    // Final bounds of layout canvas only determined in paint step.
    pending_init: bool,
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

fn fallback_value_edits(fallbacks: &[compile::UsedFallback], dv: &SparseVec) -> Vec<ValueEdit> {
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
            value: crate::sse::format_value(update.value),
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
) -> DragPersistenceEdits {
    let values = fallback_value_edits(fallbacks, dv);
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
            value: crate::sse::format_value(source.value + delta),
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
        self.inner.update(cx, |inner, cx| {
            inner.screen_bounds = bounds;
            if inner.pending_init {
                inner.pending_init = false;
                inner.fit_to_screen(cx);
            }
        });
        let inner = self.inner.read(cx);
        let solved_cell = &inner.state.read(cx).solved_cell.read(cx);
        let hide_external_geometry = &inner.state.read(cx).hide_external_geometry;
        let state = inner.state.read(cx);
        let tool = state.tool.read(cx).clone();
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
                mut show,
                path,
            )) = queue.pop_front()
            {
                let cell_info = &solved_cell.output.cells[&cell];
                let scope_info = &cell_info.scopes[&scope];
                let scope_state = &solved_cell.state[&solved_cell.scope_paths[&curr_address]];
                if depth >= state.hierarchy_depth || !scope_state.visible {
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
                    }
                    show = false;
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
                        x0: p0.x.min(layout_mouse_position.x),
                        y0: p0.y.min(layout_mouse_position.y),
                        x1: p0.x.max(layout_mouse_position.x),
                        y1: p0.y.max(layout_mouse_position.y),
                        id: None,
                        border_widths: Edges::all(SELECT_WIDTH),
                        border_styles: Edges::all(BorderStyle::Solid),
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
                        paint_polygon_fill(window, &points, layer);
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
                                layout_mouse_position,
                            )
                            .0
                        } else {
                            layout_mouse_position
                        };
                        let points = polygon_tool
                            .points
                            .iter()
                            .copied()
                            .chain(std::iter::once(preview_position))
                            .map(|point| self.inner.read(cx).layout_to_px(point))
                            .collect::<Vec<_>>();
                        paint_polygon_fill(window, &points, layer);
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
                                layout_mouse_position,
                            )
                            .0
                        } else {
                            layout_mouse_position
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
                            paint_polygon_fill(window, &outline, layer);
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
                        let font_size = px(16.);
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
                                px(18.),
                                window,
                                cx,
                            )
                            .unwrap();
                    }
                    for bbox in &scope_rects {
                        window.paint_quad(get_paint_quad(
                            get_rect_bounds(&bbox.rect, bounds, scale, offset),
                            ShapeFill::Solid,
                            Rgba { a: 0., ..theme.text },
                            theme.text,
                            bbox.rect.border_widths,
                            bbox.rect.border_styles,
                        ));
                        let font_size = px(12.);
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
                            .paint(text_origin, px(14.), window, cx)
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
                                    layout_mouse_position.x as f64,
                                    layout_mouse_position.y as f64,
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
                                    layout_mouse_position.x as f64,
                                    layout_mouse_position.y as f64,
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
                            let origin = self
                                .inner
                                .read(cx)
                                .layout_to_px(Point::new((x0 + x1) / 2., (y0 + y1) / 2.));
                            let text = SharedString::from(value);
                            let layout =
                                window
                                    .text_system()
                                    .layout_line(&text, font_size, runs, None);
                            if let Some(span) = span {
                                dim_hitboxes.push((
                                    span.clone(),
                                    vec![Bounds::new(origin, size(layout.width, font_size))],
                                    text.clone(),
                                ));
                            }
                            window
                                .text_system()
                                .shape_line(text, font_size, runs, None)
                                .paint(origin, px(16.), window, cx)
                                .unwrap();
                        };

                    for dim in dims {
                        draw_dim(
                            solved_linear_after_drag(&dim.p, sse_dv.as_ref()) as f32,
                            solved_linear_after_drag(&dim.n, sse_dv.as_ref()) as f32,
                            solved_linear_after_drag(&dim.coord, sse_dv.as_ref()) as f32,
                            solved_linear_after_drag(&dim.pstop, sse_dv.as_ref()) as f32,
                            solved_linear_after_drag(&dim.nstop, sse_dv.as_ref()) as f32,
                            dim.horiz,
                            format!(
                                "{:.3}",
                                solved_linear_after_drag(&dim.value, sse_dv.as_ref())
                            ),
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
                            rgb(0xffff00),
                            None,
                        );
                    }

                    if let ToolState::DrawDim(DrawDimToolState { edges }) = &tool {
                        // draw dimension lines
                        if edges.len() == 1 {
                            if let DimEdge::Edge((_, _, edge)) = &edges[0] {
                                let coord = match edge.dir {
                                    Dir::Horiz => layout_mouse_position.y,
                                    Dir::Vert => layout_mouse_position.x,
                                };
                                draw_dim(
                                    edge.start,
                                    edge.stop,
                                    coord,
                                    edge.coord,
                                    edge.coord,
                                    edge.dir == Dir::Horiz,
                                    format!("{:.3}", (edge.stop - edge.start).abs()),
                                    rgb(0xff0000),
                                    None,
                                );
                            }
                        } else if edges.len() == 2 {
                            let (p, n, coord, pstop, nstop, horiz, value) =
                                match (&edges[0], &edges[1]) {
                                    (
                                        DimEdge::Edge((_, _, edge0)),
                                        DimEdge::Edge((_, _, edge1)),
                                    ) => {
                                        let coord = match edge0.dir {
                                            Dir::Horiz => layout_mouse_position.x,
                                            Dir::Vert => layout_mouse_position.y,
                                        };
                                        (
                                            edge0.coord,
                                            edge1.coord,
                                            coord,
                                            (edge0.start + edge0.stop) / 2.,
                                            (edge1.start + edge1.stop) / 2.,
                                            edge0.dir == Dir::Vert,
                                            format!("{:.3}", (edge1.coord - edge0.coord).abs()),
                                        )
                                    }
                                    (DimEdge::X0 | DimEdge::Y0, DimEdge::Edge((_, _, edge)))
                                    | (DimEdge::Edge((_, _, edge)), DimEdge::X0 | DimEdge::Y0) => {
                                        let coord = match edge.dir {
                                            Dir::Horiz => layout_mouse_position.x,
                                            Dir::Vert => layout_mouse_position.y,
                                        };
                                        (
                                            0.,
                                            edge.coord,
                                            coord,
                                            coord,
                                            (edge.start + edge.stop) / 2.,
                                            edge.dir == Dir::Vert,
                                            format!("{:3}", edge.coord.abs()),
                                        )
                                    }
                                    _ => unreachable!(),
                                };
                            draw_dim(p, n, coord, pstop, nstop, horiz, value, rgb(0xff0000), None);
                        }
                        // highlight selected edges
                        for edge in edges {
                            let bounds = match edge {
                                DimEdge::Edge((_, _, edge)) => {
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
                                                    DimEdge::Edge((_, _, edge)) => edge.dir,
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
        self.inner.update(cx, |inner, cx| {
            inner.rects = rects;
            inner.polygons = polygons;
            inner.scope_rects = scope_rects;
            inner.dim_hitboxes = dim_hitboxes;
            inner.sse_handles = sse_handles;
            inner.sse_bodies = sse_bodies;
            cx.notify();
        });
    }
}

impl Render for LayoutCanvas {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl IntoElement {
        div()
            .flex()
            .flex_1()
            .key_context("LayoutCanvas")
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_left_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
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
    }
}

impl Focusable for LayoutCanvas {
    fn focus_handle(&self, _cx: &gpui::App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl LayoutCanvas {
    pub fn new(
        cx: &mut Context<Self>,
        state: &Entity<EditorState>,
        focus_handle: FocusHandle,
        text_input_focus_handle: FocusHandle,
    ) -> Self {
        LayoutCanvas {
            focus_handle,
            text_input_focus_handle,
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
            sse_targets: Vec::new(),
            sse_delta: Point::default(),
            sse_handles: Vec::new(),
            sse_bodies: Vec::new(),
            drag_start: Point::default(),
            offset_start: Point::default(),
            mouse_position: Point::default(),
            shift_down: false,
            scale: 1.0,
            screen_bounds: Bounds::default(),
            subscriptions: vec![cx.observe(state, |_, _, cx| cx.notify())],
            state: state.clone(),
            rects: Vec::new(),
            polygons: Vec::new(),
            scope_rects: Vec::new(),
            dim_hitboxes: Vec::new(),
            pending_init: true,
        }
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

        for (paint_order, (span, hitboxes, _)) in self.dim_hitboxes.iter().enumerate() {
            for bounds in hitboxes {
                if bounds.contains(&position) {
                    hits.push(SelectionHit {
                        span: span.clone(),
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
        let layout_mouse_position = self.px_to_layout(event.position);
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
                                let p1 = layout_mouse_position;
                                let p0p = Point::new(f32::min(p0.x, p1.x), f32::min(p0.y, p1.y));
                                let p1p = Point::new(f32::max(p0.x, p1.x), f32::max(p0.y, p1.y));
                                self.state.update(cx, |state, cx| {
                                    let error = state.solved_cell.update(cx, {
                                        |cell, cx| {
                                            if let Some(cell) = cell.as_mut() {
                                                // TODO update in memory representation of code
                                                // TODO add solver to gui
                                                let scope_address =
                                                    &cell.state[&cell.selected_scope].address;
                                                let reachable_objs = cell.output.reachable_objs(
                                                    scope_address.cell,
                                                    scope_address.scope,
                                                );
                                                let names: IndexSet<_> =
                                                    reachable_objs.values().collect();
                                                let scope = cell
                                                    .output
                                                    .cells
                                                    .get_mut(&scope_address.cell)
                                                    .unwrap()
                                                    .scopes
                                                    .get_mut(&scope_address.scope)
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
                                                        x0: p0p.x as f64,
                                                        y0: p0p.y as f64,
                                                        x1: p1p.x as f64,
                                                        y1: p1p.y as f64,
                                                        construction: false,
                                                    },
                                                ) {
                                                    Ok(None) => Some(
                                                        "inconsistent editor and GUI state".into(),
                                                    ),
                                                    Ok(Some(_)) => None,
                                                    Err(_) => None,
                                                }
                                            } else {
                                                Some("no cell to edit".into())
                                            }
                                        }
                                    });
                                    if state.fatal_error.is_none() {
                                        state.fatal_error = error;
                                    }
                                });
                            } else {
                                let p0 = self.px_to_layout(event.position);
                                rect_tool.p0 = Some(p0);
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
                            let cursor = Point::new(
                                (layout_mouse_position.x * 10.).round() / 10.,
                                (layout_mouse_position.y * 10.).round() / 10.,
                            );
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
                            let cursor = Point::new(
                                (layout_mouse_position.x * 10.).round() / 10.,
                                (layout_mouse_position.y * 10.).round() / 10.,
                            );
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
                    let x = ((layout_mouse_position.x as f64 * 10.).round() / 10.) + 0.;
                    let y = ((layout_mouse_position.y as f64 * 10.).round() / 10.) + 0.;
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
                            let _ = self.state.read(cx).lang_server_client.show_message(
                                MessageType::ERROR,
                                "Could not insert the instance into the source scope.",
                            );
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
                                let path = {
                                    let cell = self.state.read(cx).solved_cell.read(cx);
                                    if let Some(cell) = cell
                                        && let selected_scope_addr =
                                            cell.state[&cell.selected_scope].address
                                        && let (true, path) =
                                            find_obj_path(&r.object_path, cell, selected_scope_addr)
                                    {
                                        let path = path.join(".");
                                        Some(path)
                                    } else {
                                        None
                                    }
                                };
                                if let Some(path) = path
                                    && dim_tool
                                        .edges
                                        .first()
                                        .map(|old_edge| {
                                            let old_dir = match old_edge {
                                                DimEdge::X0 => Dir::Vert,
                                                DimEdge::Y0 => Dir::Horiz,
                                                DimEdge::Edge((_, _, edge)) => edge.dir,
                                            };
                                            old_dir == edge.dir
                                        })
                                        .unwrap_or(true)
                                {
                                    dim_tool.edges.push(DimEdge::Edge((
                                        path,
                                        name.to_string(),
                                        edge,
                                    )));
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
                                            DimEdge::Edge((_, _, edge)) => edge.dir,
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
                                            DimEdge::Edge((_, _, edge)) => edge.dir,
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

                        let pending = if dim_tool.edges.len() == 1
                            && let DimEdge::Edge(edge) = &dim_tool.edges[0]
                        {
                            let (left, right, coord, horiz) = match edge.2.dir {
                                Dir::Horiz => ("x0", "x1", layout_mouse_position.y, "true"),
                                Dir::Vert => ("y0", "y1", layout_mouse_position.x, "false"),
                            };

                            let distance = edge.2.stop - edge.2.start;
                            let value = format!("{distance:?}");
                            Some((
                                DimensionParams {
                                    p: format!("{}.{}", edge.0, right),
                                    n: format!("{}.{}", edge.0, left),
                                    value: value.clone(),
                                    coord: if coord > edge.2.coord {
                                        format!("{}.{} + {}", edge.0, edge.1, coord - edge.2.coord)
                                    } else {
                                        format!("{}.{} - {}", edge.0, edge.1, edge.2.coord - coord)
                                    },
                                    pstop: format!("{}.{}", edge.0, edge.1),
                                    nstop: format!("{}.{}", edge.0, edge.1),
                                    horiz: horiz.to_string(),
                                },
                                value,
                                PendingDimensionPreview {
                                    p: edge.2.stop,
                                    n: edge.2.start,
                                    coord,
                                    pstop: edge.2.coord,
                                    nstop: edge.2.coord,
                                    horiz: edge.2.dir == Dir::Horiz,
                                    value: format!("{distance:.3}"),
                                },
                            ))
                        } else if dim_tool.edges.len() == 2 {
                            match (&dim_tool.edges[0], &dim_tool.edges[1]) {
                                (DimEdge::Edge(edge0), DimEdge::Edge(edge1)) => {
                                    let (left, right) = if edge0.2.coord < edge1.2.coord {
                                        (edge0, edge1)
                                    } else {
                                        (edge1, edge0)
                                    };
                                    let (start, stop, coord, horiz) = match left.2.dir {
                                        Dir::Vert => ("y0", "y1", layout_mouse_position.y, "true"),
                                        Dir::Horiz => {
                                            ("x0", "x1", layout_mouse_position.x, "false")
                                        }
                                    };

                                    let intended_coord =
                                        (right.2.start + right.2.stop + left.2.start + left.2.stop)
                                            / 4.;
                                    let coord_offset = if coord > intended_coord {
                                        format!("+ {}", coord - intended_coord)
                                    } else {
                                        format!("- {}", intended_coord - coord)
                                    };
                                    let distance = right.2.coord - left.2.coord;
                                    let value = format!("{distance:?}");
                                    Some((
                                        DimensionParams {
                                            p: format!("{}.{}", right.0, right.1,),
                                            n: format!("{}.{}", left.0, left.1),
                                            value: value.clone(),
                                            coord: format!(
                                                "({}.{} + {}.{} + {}.{} + {}.{})/4. {coord_offset}",
                                                right.0,
                                                start,
                                                right.0,
                                                stop,
                                                left.0,
                                                start,
                                                left.0,
                                                stop,
                                            ),
                                            pstop: format!(
                                                "({}.{} + {}.{}) / 2.",
                                                right.0, start, right.0, stop,
                                            ),
                                            nstop: format!(
                                                "({}.{} + {}.{}) / 2.",
                                                left.0, start, left.0, stop,
                                            ),
                                            horiz: horiz.to_string(),
                                        },
                                        value,
                                        PendingDimensionPreview {
                                            p: right.2.coord,
                                            n: left.2.coord,
                                            coord,
                                            pstop: (right.2.start + right.2.stop) / 2.,
                                            nstop: (left.2.start + left.2.stop) / 2.,
                                            horiz: left.2.dir == Dir::Vert,
                                            value: format!("{distance:.3}"),
                                        },
                                    ))
                                }
                                (DimEdge::X0 | DimEdge::Y0, DimEdge::Edge(edge))
                                | (DimEdge::Edge(edge), DimEdge::X0 | DimEdge::Y0) => {
                                    let (start, stop, coord, horiz) = match edge.2.dir {
                                        Dir::Vert => ("y0", "y1", layout_mouse_position.y, "true"),
                                        Dir::Horiz => {
                                            ("x0", "x1", layout_mouse_position.x, "false")
                                        }
                                    };

                                    let intended_coord = (edge.2.start + edge.2.stop) / 2.;
                                    let coord_offset = if coord > intended_coord {
                                        format!("+ {}", coord - intended_coord)
                                    } else {
                                        format!("- {}", intended_coord - coord)
                                    };

                                    let pnstop = format!(
                                        "({}.{} + {}.{}) / 2.",
                                        edge.0, start, edge.0, stop,
                                    );
                                    let coord = format!("{pnstop} {coord_offset}");
                                    let (p, n, value, pstop, nstop, preview) = if edge.2.coord < 0.
                                    {
                                        (
                                            "0.".to_string(),
                                            format!("{}.{}", edge.0, edge.1),
                                            format!("{:?}", -edge.2.coord),
                                            coord.clone(),
                                            pnstop,
                                            PendingDimensionPreview {
                                                p: 0.,
                                                n: edge.2.coord,
                                                coord: match edge.2.dir {
                                                    Dir::Vert => layout_mouse_position.y,
                                                    Dir::Horiz => layout_mouse_position.x,
                                                },
                                                pstop: match edge.2.dir {
                                                    Dir::Vert => layout_mouse_position.y,
                                                    Dir::Horiz => layout_mouse_position.x,
                                                },
                                                nstop: intended_coord,
                                                horiz: edge.2.dir == Dir::Vert,
                                                value: format!("{:.3}", -edge.2.coord),
                                            },
                                        )
                                    } else {
                                        (
                                            format!("{}.{}", edge.0, edge.1),
                                            "0.".to_string(),
                                            format!("{:?}", edge.2.coord),
                                            pnstop,
                                            coord.clone(),
                                            PendingDimensionPreview {
                                                p: edge.2.coord,
                                                n: 0.,
                                                coord: match edge.2.dir {
                                                    Dir::Vert => layout_mouse_position.y,
                                                    Dir::Horiz => layout_mouse_position.x,
                                                },
                                                pstop: intended_coord,
                                                nstop: match edge.2.dir {
                                                    Dir::Vert => layout_mouse_position.y,
                                                    Dir::Horiz => layout_mouse_position.x,
                                                },
                                                horiz: edge.2.dir == Dir::Vert,
                                                value: format!("{:.3}", edge.2.coord),
                                            },
                                        )
                                    };
                                    Some((
                                        DimensionParams {
                                            p,
                                            n,
                                            value: value.clone(),
                                            coord,
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
            window.focus(&self.text_input_focus_handle);
            self.text_input_focus_handle
                .dispatch_action(&EditDim, window, cx);
            window.prevent_default();
        }
    }

    fn layout_to_px(&self, pt: Point<f32>) -> Point<Pixels> {
        Point::new(self.scale * px(pt.x), self.scale * px(-pt.y))
            + self.offset
            + self.screen_bounds.origin
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
            let points = draw_points
                .iter()
                .map(|point| {
                    (
                        draw_source_coordinate(point.x),
                        draw_source_coordinate(point.y),
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
                let error = state.solved_cell.update(cx, |cell, _cx| {
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
                        Ok(None) => Some("inconsistent editor and GUI state".into()),
                        Err(_) => None,
                    }
                });
                if state.fatal_error.is_none() {
                    state.fatal_error = error;
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
            && let Some((_, _, value)) = self.dim_hitboxes.iter().find(|(span, _, _)| span == obj)
        {
            let obj = obj.clone();
            self.state.read(cx).tool.clone().update(cx, |tool, _cx| {
                *tool = ToolState::EditDim(EditDimToolState {
                    dim: Some(obj.clone()),
                    pending: None,
                    dim_mode: false,
                    original_value: value.clone(),
                })
            });
            window.focus(&self.text_input_focus_handle);
            self.text_input_focus_handle
                .dispatch_action(&EditDim, window, cx);
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
        _cx: &mut Context<Self>,
    ) {
        self.is_dragging = true;
        self.drag_start = event.position;
        self.offset_start = self.offset;
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
        } else if self.is_sse_dragging {
            self.sse_delta = self.mouse_position - self.drag_start;
        }
        cx.notify();
    }

    pub(crate) fn on_middle_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.is_dragging = false;
        self.is_sse_dragging = false;
        self.sse_delta = Point::default();
        self.sse_targets.clear();
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
                self.sse_delta = Point::default();
                self.sse_targets.clear();
            } else {
                match self
                    .state
                    .read(cx)
                    .lang_server_client
                    .update_values(edits.values, edits.initial_conditions)
                {
                    Ok(Some(applied_edits)) => {
                        self.is_sse_dragging = false;
                        self.is_sse_persisting = true;
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
                        self.sse_delta = Point::default();
                        self.sse_targets.clear();
                    }
                    Err(_) => {
                        self.is_sse_dragging = false;
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
    pub(crate) fn finish_sse_persist(&mut self, cx: &mut Context<Self>) {
        if self.is_sse_persisting {
            self.is_sse_persisting = false;
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

        cx.notify();
    }
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
        assert_eq!(draw_source_coordinate(110.3_f32), 110.3);
    }

    #[test]
    fn path_centerline_points_are_draggable() {
        let source =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/path/lib.ar");
        let ast = argonc::parse::parse_workspace_with_std(&source).ast();
        let lyp = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/lyp/basic.lyp");
        let output = argonc::compile::compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["initial_path"],
                args: vec![],
                lyp_file: &lyp,
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
        let lyp = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/lyp/basic.lyp");
        let output = argonc::compile::compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["one_axis_point"],
                args: vec![],
                lyp_file: &lyp,
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
        let lyp = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/lyp/basic.lyp");
        let output = argonc::compile::compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["initial_points"],
                args: vec![],
                lyp_file: &lyp,
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

        let edits = drag_persistence_edits(&cell.fallback_constraints_used, &targets, &drag);
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

        let inserted = drag_persistence_edits(&[], &targets, &drag);
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

        let edits = drag_persistence_edits(&[], &targets, &drag);
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
    fn dimension_coordinate_follows_dragged_solver_expression() {
        let mut solver = argonc::solver::Solver::new();
        let edge = solver.new_var();
        let field = (10., LinearExpr::from(edge));
        let drag = SparseVec([(edge, 7.5)].into_iter().collect());

        assert_eq!(solved_linear_after_drag(&field, Some(&drag)), 17.5);
        assert_eq!(solved_linear_after_drag(&field, None), 10.);
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
        let lyp = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/lyp/basic.lyp");
        let output = argonc::compile::compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["top"],
                args: vec![],
                lyp_file: &lyp,
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
        let lyp = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/lyp/basic.lyp");
        let output = argonc::compile::compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["top"],
                args: vec![],
                lyp_file: &lyp,
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
            output,
            selected_scope: Vec::new(),
            state: Default::default(),
            scope_paths: Default::default(),
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
    }
}
