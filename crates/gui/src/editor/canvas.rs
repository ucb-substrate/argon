use std::{
    collections::{HashMap, HashSet, VecDeque},
    fmt::Debug,
    ops::{Add, Sub},
    sync::{Arc, Mutex},
};

use analyzer::rpc::{
    DimensionParams, InitialConditionEdit, InstancePreview, PolygonParams, ValueEdit,
};
use argonc::{
    ast::Span,
    compile::{self, CellId, CompiledData, ObjectId, RectInitialCondition, SolvedValue, ifmatvec},
    solver::{LinearExpr, Var},
};
use enumify::enumify;
use geometry::{dir::Dir, transform::TransformationMatrix};
use gpui::{
    AppContext, BorderStyle, Bounds, ContentMask, Context, Corners, DefiniteLength, Edges, Element,
    Entity, FocusHandle, Focusable, Half, InteractiveElement, IntoElement, Length, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, ParentElement, PathBuilder, Pixels,
    Point, Render, RenderImage, Rgba, ScrollWheelEvent, SharedString, Size, Style, Styled,
    Subscription, Task, TextRun, Window, div, pattern_slash, px, rgb, size, solid_background,
};
use indexmap::{IndexMap, IndexSet};
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
/// Relative detail ranks for retained rasters. Hierarchy traversal is the same
/// in both modes; the coarse pass only aggregates slightly larger sub-pixel
/// shapes and omits text.
const NORMAL_NAVIGATION_DETAIL_RANK: f32 = 64. * 64.;
const INTERACTIVE_NAVIGATION_DETAIL_RANK: f32 = 256. * 256.;
/// Above this size, retain a viewport raster for interactive redraws. GPUI can
/// replay a clean retained view, but pan/zoom dirties this immediate element;
/// transforming one sprite avoids rebuilding hundreds of thousands of quads.
const RASTER_CACHE_GEOMETRY_THRESHOLD: usize = 50_000;
/// The interaction cache is deliberately half resolution. Its geometry is
/// shown only between exact vector rebuilds, and quartering its pixel count
/// keeps cache generation bounded even for long overlapping routes.
const RASTER_CACHE_RESOLUTION: f32 = 0.5;
/// Keep several completed viewport rasters as a small world-space atlas. When
/// navigation outruns raster generation, previously visible regions remain
/// available instead of disappearing with the old viewport.
const RASTER_CACHE_HISTORY_LIMIT: usize = 8;
/// Geometry smaller than this in the interaction raster is accumulated into a
/// compact per-layer occupancy mask. Hierarchy is still fully flattened; only
/// the final sub-pixel representation becomes cheaper.
const COARSE_GEOMETRY_LOD_SIZE_PX: f32 = 2.;
const NORMAL_GEOMETRY_LOD_SIZE_PX: f32 = 1.;
/// Repeated cells below this projected raster size are flattened once into an
/// exact-size tile and stamped for each instance. Larger cells expand into
/// ordinary geometry, so detail comes into focus without an instance cutoff.
const CELL_RASTER_TILE_MAX_SIZE: u32 = 16;
const CELL_RASTER_TILE_CACHE_LIMIT: usize = 256;
const NAVIGATION_OVERSCAN: Pixels = px(192.);
const NAVIGATION_OVERSCAN_GUARD: Pixels = px(48.);
const TEXT_LAYOUT_SIZE: f32 = 16.;
const SCOPE_TEXT_LAYOUT_SIZE: f32 = 12.;
const MIN_READABLE_TEXT_PX: f32 = 5.;
const MAX_TEXT_PX: f32 = 64.;
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
    area: f32,
    outline: SelectionOutline,
    layer: SelectionLayer,
    paint_order: usize,
}

fn selection_hit_area(hit: &SelectionHit) -> f32 {
    hit.area
}

fn bounds_area(bounds: Bounds<Pixels>) -> f32 {
    f32::from(bounds.size.width).abs() * f32::from(bounds.size.height).abs()
}

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

fn ordered_selection_hits(mut hits: Vec<SelectionHit>) -> Vec<SelectionHit> {
    hits.sort_by(|a, b| {
        b.layer
            .cmp(&a.layer)
            .then_with(|| selection_hit_area(a).total_cmp(&selection_hit_area(b)))
            .then_with(|| b.paint_order.cmp(&a.paint_order))
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
/// layout, higher-z layers win, then smaller shapes, then later paint order.
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
                (rect.x1 - rect.x0).abs() * (rect.y1 - rect.y0).abs(),
                paint_order,
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
                        (bbox.rect.x1 - bbox.rect.x0).abs() * (bbox.rect.y1 - bbox.rect.y0).abs(),
                        paint_order,
                    )
                }),
        )
        .collect::<Vec<_>>();
    candidates.sort_by(|a, b| {
        b.1.cmp(&a.1)
            .then_with(|| a.2.total_cmp(&b.2))
            .then_with(|| b.3.cmp(&a.3))
    });
    candidates.into_iter().map(|(rect, _, _, _)| rect).collect()
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
    hover_hit: Option<SelectionHit>,
    // zoom state
    scale: f32,
    screen_bounds: Bounds<Pixels>,
    #[allow(unused)]
    subscriptions: Vec<Subscription>,
    rects: Vec<(Rect, LayerState)>,
    polygons: Vec<(Polygon, LayerState)>,
    scope_rects: Vec<LabeledBbox>,
    dim_hitboxes: Vec<(Span, Vec<Bounds<Pixels>>, SharedString)>,
    raster_cache: Option<LayoutRasterCache>,
    raster_cache_history: VecDeque<LayoutRasterCache>,
    navigation_cache_active: bool,
    raster_refinement: Option<Task<()>>,
    /// Only one background raster worker runs at a time. Input events merely
    /// advance the requested generation; the worker coalesces them and always
    /// continues with the newest viewport after finishing its current image.
    raster_worker_active: bool,
    /// Monotonically identifies the newest requested raster. A slow render for
    /// an older viewport must never replace a newer, more appropriate LOD.
    raster_generation: u64,
    /// Changes only when layout content or its presentation changes, not for
    /// pan/zoom. It prevents a stale layer-visibility raster from entering the
    /// retained navigation atlas after a replacement has arrived.
    raster_content_revision: u64,
    raster_output: Option<Arc<CompiledData>>,
    cell_raster_tiles: Arc<Mutex<CellRasterTileCache>>,
    // True if waiting on render step to finish some initialization.
    //
    // Final bounds of layout canvas only determined in paint step.
    pending_init: bool,
}

#[derive(Clone)]
struct LayoutRasterCache {
    image: Arc<RenderImage>,
    texts: Arc<[TextLabel]>,
    scope_labels: Arc<[LabeledBbox]>,
    /// Logical extent covered by the raster image, including overscan.
    viewport: Size<Pixels>,
    /// Canvas size for which this cache was requested.
    screen_viewport: Size<Pixels>,
    scale: f32,
    offset: Point<Pixels>,
    /// Smaller thresholds retain more hierarchy detail. This lets an older,
    /// detailed image remain authoritative while a coarse navigation image
    /// fills only newly exposed screen edges.
    lod_area_px: f32,
    content_revision: u64,
}

#[derive(Clone, Copy)]
struct ViewportTransform {
    size: Size<Pixels>,
    screen_size: Size<Pixels>,
    scale: f32,
    offset: Point<Pixels>,
}

#[derive(Clone)]
struct NavigationRasterInput {
    solved_cell: CompileOutputState,
    layers: Arc<IndexMap<SharedString, LayerState>>,
    hierarchy_depth: usize,
    hide_external_geometry: bool,
    viewport: ViewportTransform,
    text_color: Rgba,
    lod_area_px: f32,
    include_text: bool,
    content_revision: u64,
    cell_raster_tiles: Arc<Mutex<CellRasterTileCache>>,
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

fn raster_pixel_range(start: f32, stop: f32, limit: u32) -> Option<(u32, u32)> {
    let lower = start.min(stop).floor().clamp(0., limit as f32) as u32;
    let upper = start.max(stop).ceil().clamp(0., limit as f32) as u32;
    (lower < upper).then_some((lower, upper))
}

fn raster_logical_size(width: u32, height: u32) -> Size<Pixels> {
    Size::new(
        px(width as f32 / RASTER_CACHE_RESOLUTION),
        px(height as f32 / RASTER_CACHE_RESOLUTION),
    )
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

fn paint_raster_occupancy(
    buffer: &mut [u8],
    occupancy: &[u64],
    pixel_count: usize,
    layer: &LayerState,
) {
    let mut color = layer.color;
    if layer.fill == ShapeFill::Stippling {
        // Below one screen pixel the slash pattern has no stable phase. Its
        // average coverage produces the same solid-looking density as other
        // flattened sub-pixel geometry.
        color.a *= 0.25;
    }
    for (word_index, word) in occupancy.iter().enumerate() {
        let mut remaining = *word;
        while remaining != 0 {
            let bit = remaining.trailing_zeros() as usize;
            let pixel = word_index * 64 + bit;
            if pixel < pixel_count {
                blend_raster_pixel(buffer, pixel, color);
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
        for pair in intersections.chunks_exact(2) {
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
    buffer: &mut [u8],
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
    let stipple_period = (10. * RASTER_CACHE_RESOLUTION).round().max(1.);
    let resolve_stipple_pattern = layer.fill == ShapeFill::Stippling
        && f32::from(primitive.bounds.size.width) >= stipple_period
        && f32::from(primitive.bounds.size.height) >= stipple_period;
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
                blend_raster_fill_pixel(
                    buffer,
                    raster_width,
                    x,
                    y,
                    ShapeFill::Stippling,
                    pixel_color,
                );
            } else {
                blend_raster_pixel(
                    buffer,
                    (y as usize * raster_width as usize) + x as usize,
                    pixel_color,
                );
            }
        }
    }
}

fn fill_raster_rect(
    buffer: &mut [u8],
    width: u32,
    height: u32,
    bounds: Bounds<Pixels>,
    fill: ShapeFill,
    color: Rgba,
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
            blend_raster_fill_pixel(buffer, width, x, y, fill, color);
        }
    }
}

/// Match GPUI's global 45-degree 1px/9px slash pattern in the retained raster.
/// At half resolution one raster pixel covers two logical pixels, so use a
/// half-alpha sample every five pixels to preserve the pattern's coverage.
fn blend_raster_fill_pixel(
    buffer: &mut [u8],
    width: u32,
    x: u32,
    y: u32,
    fill: ShapeFill,
    mut color: Rgba,
) {
    if fill == ShapeFill::Stippling {
        let period = (10. * RASTER_CACHE_RESOLUTION).round().max(1.) as i64;
        if (x as i64 - y as i64).rem_euclid(period) != 0 {
            return;
        }
        color.a *= RASTER_CACHE_RESOLUTION;
    }
    blend_raster_pixel(buffer, (y * width + x) as usize, color);
}

fn stroke_raster_rect(
    buffer: &mut [u8],
    width: u32,
    height: u32,
    bounds: Bounds<Pixels>,
    color: Rgba,
) {
    let one = px(1.);
    fill_raster_rect(
        buffer,
        width,
        height,
        Bounds::new(bounds.origin, Size::new(bounds.size.width, one)),
        ShapeFill::Solid,
        color,
    );
    fill_raster_rect(
        buffer,
        width,
        height,
        Bounds::new(
            Point::new(bounds.origin.x, bounds.origin.y + bounds.size.height - one),
            Size::new(bounds.size.width, one),
        ),
        ShapeFill::Solid,
        color,
    );
    fill_raster_rect(
        buffer,
        width,
        height,
        Bounds::new(bounds.origin, Size::new(one, bounds.size.height)),
        ShapeFill::Solid,
        color,
    );
    fill_raster_rect(
        buffer,
        width,
        height,
        Bounds::new(
            Point::new(bounds.origin.x + bounds.size.width - one, bounds.origin.y),
            Size::new(one, bounds.size.height),
        ),
        ShapeFill::Solid,
        color,
    );
}

fn fill_raster_polygon(
    buffer: &mut [u8],
    width: u32,
    height: u32,
    points: &[Point<f32>],
    fill: ShapeFill,
    color: Rgba,
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
        for pair in intersections.chunks_exact(2) {
            let Some((x0, x1)) = raster_pixel_range(pair[0], pair[1], width) else {
                continue;
            };
            for x in x0..x1 {
                blend_raster_fill_pixel(buffer, width, x, y, fill, color);
            }
        }
    }
}

fn stroke_raster_line(
    buffer: &mut [u8],
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
            blend_raster_pixel(buffer, (y as u32 * width + x as u32) as usize, color);
        }
    }
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
    let mut buffer = vec![0; width as usize * height as usize * 4];
    let local_bounds = Bounds::new(
        Point::default(),
        Size::new(px(width as f32), px(height as f32)),
    );
    let raster_scale = viewport.scale * RASTER_CACHE_RESOLUTION;
    let raster_offset = Point::new(
        viewport.offset.x * RASTER_CACHE_RESOLUTION,
        viewport.offset.y * RASTER_CACHE_RESOLUTION,
    );
    for (rect, layer) in rects {
        let bounds = get_rect_bounds(rect, local_bounds, raster_scale, raster_offset);
        fill_raster_rect(&mut buffer, width, height, bounds, layer.fill, layer.color);
        stroke_raster_rect(&mut buffer, width, height, bounds, layer.border_color);
    }
    for (polygon, layer) in polygons {
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
        fill_raster_polygon(&mut buffer, width, height, &points, layer.fill, layer.color);
        for (start, stop) in points
            .iter()
            .zip(points.iter().cycle().skip(1))
            .take(points.len())
        {
            stroke_raster_line(
                &mut buffer,
                width,
                height,
                *start,
                *stop,
                layer.border_color,
            );
        }
    }
    for bbox in scope_rects {
        let bounds = get_rect_bounds(&bbox.rect, local_bounds, raster_scale, raster_offset);
        stroke_raster_rect(&mut buffer, width, height, bounds, theme.text);
    }
    let image = image::RgbaImage::from_raw(width, height, buffer)?;
    Some(LayoutRasterCache {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(image)])),
        texts: texts.to_vec().into(),
        scope_labels: scope_rects.to_vec().into(),
        // Use the texture's actual logical extent after rounding. Otherwise
        // an odd-sized canvas stretches every texel by a tiny amount and a
        // replacement raster appears to shift the geometry.
        viewport: raster_logical_size(width, height),
        screen_viewport: viewport.screen_size,
        scale: viewport.scale,
        offset: viewport.offset,
        lod_area_px: 0.,
        content_revision,
    })
}

/// Builds the viewport raster without touching GPUI's scene or entity state.
/// This is intentionally independent from the exact/editable paint path so it
/// can run on the background executor while the UI keeps transforming the last
/// completed image at display refresh rate.
fn build_navigation_raster(input: NavigationRasterInput) -> Option<LayoutRasterCache> {
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
    let mut low_detail_occupancy = (0..layer_count)
        .map(|_| None::<Vec<u64>>)
        .collect::<Vec<_>>();
    let mut tile_primitives = (0..layer_count)
        .map(|_| Vec::<RasterTilePrimitive>::new())
        .collect::<Vec<_>>();
    let mut seen_tile_candidates = HashSet::<CellRasterTileKey>::new();
    let geometry_lod_size = if input.lod_area_px > NORMAL_NAVIGATION_DETAIL_RANK {
        COARSE_GEOMETRY_LOD_SIZE_PX
    } else {
        NORMAL_GEOMETRY_LOD_SIZE_PX
    } * RASTER_CACHE_RESOLUTION;
    let mut texts = Vec::new();
    let mut scope_rects = Vec::new();

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
            if depth > 0 && !pixel_bounds.intersects(&local_bounds) {
                continue;
            }
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
            if depth > 0
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
                            &mut low_detail_occupancy[layer.z],
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
                            &mut low_detail_occupancy[layer.z],
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

    let mut buffer = vec![0; width as usize * height as usize * 4];
    let pixel_count = width as usize * height as usize;
    for (layer_index, (rect_layer, polygon_layer)) in rect_primitives
        .into_iter()
        .zip(polygon_primitives)
        .enumerate()
    {
        if let Some(occupancy) = low_detail_occupancy[layer_index].take()
            && let Some(layer) = input
                .layers
                .values()
                .find(|layer| layer.z == layer_index && layer.visible)
        {
            paint_raster_occupancy(&mut buffer, &occupancy, pixel_count, layer);
        }
        if let Some(layer) = input
            .layers
            .values()
            .find(|layer| layer.z == layer_index && layer.visible)
        {
            for primitive in &tile_primitives[layer_index] {
                paint_raster_tile(&mut buffer, width, height, primitive, layer);
            }
        }
        for primitive in rect_layer {
            let RasterRectPrimitive {
                bounds,
                fill,
                color,
                border_color,
            } = primitive;
            fill_raster_rect(&mut buffer, width, height, bounds, fill, color);
            stroke_raster_rect(&mut buffer, width, height, bounds, border_color);
        }
        for primitive in polygon_layer {
            let RasterPolygonPrimitive {
                points,
                fill,
                color,
                border_color,
            } = primitive;
            fill_raster_polygon(&mut buffer, width, height, &points, fill, color);
            for (start, stop) in points
                .iter()
                .zip(points.iter().cycle().skip(1))
                .take(points.len())
            {
                stroke_raster_line(&mut buffer, width, height, *start, *stop, border_color);
            }
        }
    }
    for bbox in &scope_rects {
        let bounds = get_rect_bounds(&bbox.rect, local_bounds, raster_scale, raster_offset);
        stroke_raster_rect(&mut buffer, width, height, bounds, input.text_color);
    }

    let image = image::RgbaImage::from_raw(width, height, buffer)?;
    Some(LayoutRasterCache {
        image: Arc::new(RenderImage::new(vec![image::Frame::new(image)])),
        texts: texts.into(),
        scope_labels: scope_rects.into(),
        viewport: raster_logical_size(width, height),
        screen_viewport: viewport.screen_size,
        scale: viewport.scale,
        offset: viewport.offset,
        lod_area_px: input.lod_area_px,
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

fn align_navigation_raster_offset(value: Pixels) -> Pixels {
    px((f32::from(value) * RASTER_CACHE_RESOLUTION).round() / RASTER_CACHE_RESOLUTION)
}

fn same_raster_view(a: &LayoutRasterCache, b: &LayoutRasterCache) -> bool {
    a.viewport == b.viewport
        && a.screen_viewport == b.screen_viewport
        && a.scale == b.scale
        && a.offset == b.offset
}

/// Subtract one axis-aligned cover from a region. The result contains at most
/// four non-overlapping strips and preserves every pixel outside the cover.
fn subtract_raster_cover(region: Bounds<Pixels>, cover: Bounds<Pixels>) -> Vec<Bounds<Pixels>> {
    let Some(overlap) = intersect(&region, &cover) else {
        return vec![region];
    };
    let region_br = region.bottom_right();
    let overlap_br = overlap.bottom_right();
    let mut remainder = Vec::with_capacity(4);
    let mut push = |origin: Point<Pixels>, bottom_right: Point<Pixels>| {
        if origin.x < bottom_right.x && origin.y < bottom_right.y {
            remainder.push(Bounds::from_corners(origin, bottom_right));
        }
    };
    push(region.origin, Point::new(region_br.x, overlap.origin.y));
    push(Point::new(region.origin.x, overlap_br.y), region_br);
    push(
        Point::new(region.origin.x, overlap.origin.y),
        Point::new(overlap.origin.x, overlap_br.y),
    );
    push(
        Point::new(overlap_br.x, overlap.origin.y),
        Point::new(region_br.x, overlap_br.y),
    );
    remainder
}

fn uncovered_raster_regions(
    cache_bounds: Bounds<Pixels>,
    newer_bounds: &[Bounds<Pixels>],
    canvas_bounds: Bounds<Pixels>,
) -> Vec<Bounds<Pixels>> {
    let Some(visible_bounds) = intersect(&cache_bounds, &canvas_bounds) else {
        return Vec::new();
    };
    newer_bounds
        .iter()
        .fold(vec![visible_bounds], |regions, newer| {
            regions
                .into_iter()
                .flat_map(|region| subtract_raster_cover(region, *newer))
                .collect()
        })
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
            let has_solved_layout = inner.state.read(cx).solved_cell.read(cx).is_some();
            let raster_matches_viewport = inner
                .raster_cache
                .as_ref()
                .is_some_and(|cache| cache.screen_viewport == bounds.size);
            if has_solved_layout && !raster_matches_viewport && !inner.raster_worker_active {
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
        let use_raster_cache = inner.navigation_cache_active || select_overview;
        if use_raster_cache && let Some(cache) = inner.raster_cache.clone() {
            let theme = state.theme();
            let bg_style = inner.bg_style.clone();
            let scale = inner.scale;
            let offset = inner.offset;
            let origin_coords = inner.layout_to_px(Point::new(0., 0.));
            let navigation_cache_active = inner.navigation_cache_active;
            let hover_hit = (!navigation_cache_active)
                .then(|| inner.hover_hit.clone())
                .flatten();
            let mut completed_caches = inner
                .raster_cache_history
                .iter()
                .cloned()
                .collect::<Vec<_>>();
            completed_caches.push(cache.clone());
            let mut completed_caches = completed_caches.into_iter().enumerate().collect::<Vec<_>>();
            completed_caches.sort_by(|(a_index, a), (b_index, b)| {
                a.lod_area_px
                    .total_cmp(&b.lod_area_px)
                    .then_with(|| b_index.cmp(a_index))
            });
            let mut higher_priority_bounds = Vec::with_capacity(completed_caches.len());
            let mut cache_paints = Vec::with_capacity(completed_caches.len());
            for (_, completed) in completed_caches {
                let image_bounds = raster_bounds(&completed, bounds, scale, offset);
                let regions =
                    uncovered_raster_regions(image_bounds, &higher_priority_bounds, bounds);
                if !regions.is_empty() {
                    cache_paints.push((completed, image_bounds, regions));
                }
                higher_priority_bounds.push(image_bounds);
            }
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
                    // More detailed rasters win even if a coarse navigation
                    // image is newer; the coarse image fills newly exposed
                    // edges until refinement arrives. Masks keep translucent
                    // and stippled pixels from accumulating at overlaps.
                    for (completed, image_bounds, regions) in cache_paints.iter().rev() {
                        for region in regions {
                            window.with_content_mask(
                                Some(ContentMask { bounds: *region }),
                                |window| {
                                    window
                                        .paint_image(
                                            *image_bounds,
                                            Corners::all(px(0.)),
                                            completed.image.clone(),
                                            0,
                                            false,
                                        )
                                        .unwrap();
                                },
                            );
                        }
                    }
                    if !navigation_cache_active {
                        for label in cache.texts.iter() {
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
                        for bbox in cache.scope_labels.iter() {
                            let Some((font_size, line_height)) =
                                layout_text_metrics(scale, SCOPE_TEXT_LAYOUT_SIZE)
                            else {
                                break;
                            };
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
                        }
                    }
                });
            });
            return;
        }
        if inner.navigation_cache_active && inner.raster_cache.is_none() {
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
                rects.push((
                    Rect {
                        object_path: Vec::new(),
                        x0: p0.x.min(layout_mouse_position.x),
                        y0: p0.y.min(layout_mouse_position.y),
                        x1: p0.x.max(layout_mouse_position.x),
                        y1: p0.y.max(layout_mouse_position.y),
                        id: None,
                        border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                        border_styles: Edges::all(BorderStyle::Dashed),
                        cvars: None,
                    },
                    layers.layers[layers.selected_layer.as_ref().unwrap()].clone(),
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
        let mut polygon_handle_points = Vec::new();
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
                let (Some(span), Some(cvars)) = (&polygon.id, &polygon.cvars) else {
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
                for (point, (x, y)) in polygon.points.iter().zip(cvars) {
                    let targets = LayoutCanvas::draggable_point_targets(
                        sourced_corner_sse_targets(x, y, span, &source_coordinates),
                        sse_cell,
                    );
                    if !targets.is_empty() {
                        let mid = inner.layout_to_px(*point);
                        polygon_handle_points.push(mid);
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
                        let mut fill = PathBuilder::fill();
                        fill.add_polygon(&points, true);
                        if let Ok(path) = fill.build() {
                            let background = match layer.fill {
                                ShapeFill::Solid => solid_background(layer.color),
                                ShapeFill::Stippling => {
                                    pattern_slash(layer.color.into(), 1., 9.)
                                }
                            };
                            window.paint_path(path, background);
                        }
                        let selected = matches!(
                            &tool,
                            ToolState::Select(SelectToolState {
                                selected_obj: Some(selected),
                            }) if polygon.id.as_ref() == Some(selected)
                        );
                        let border_width = if selected {
                            SELECT_WIDTH
                        } else {
                            DEFAULT_BORDER_WIDTH
                        };
                        let border_color = if selected {
                            rgb(0xffff00)
                        } else {
                            layer.border_color
                        };
                        paint_polygon_border(
                            window,
                            &points,
                            &polygon.edge_styles,
                            border_width,
                            border_color,
                        );
                    }
                    for mid in &polygon_handle_points {
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
                    if let ToolState::DrawPolygon(polygon_tool) = &tool
                        && !polygon_tool.points.is_empty()
                    {
                        let points = polygon_tool
                            .points
                            .iter()
                            .copied()
                            .chain(std::iter::once(layout_mouse_position))
                            .map(|point| self.inner.read(cx).layout_to_px(point))
                            .collect::<Vec<_>>();
                        let mut preview = PathBuilder::stroke(DEFAULT_BORDER_WIDTH);
                        preview.add_polygon(&points, false);
                        if let Ok(path) = preview.build() {
                            window.paint_path(path, rgb(0xffff00));
                        }
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
                    for r in &select_rects {
                        window.paint_quad(get_paint_quad(
                            get_rect_bounds(r, bounds, scale, offset),
                            ShapeFill::Solid,
                            Rgba { a: 0., ..rgb(0xffff00) },
                            rgb(0xffff00),
                            r.border_widths,
                            r.border_styles,
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
                        ToolState::Select(_) => {
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
                                }
                            }
                        }
                        _ => {}
                    }
                })
            });
        let raster_cache = (rects.len() + polygons.len() + scope_rects.len()
            >= RASTER_CACHE_GEOMETRY_THRESHOLD)
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
        let refresh_cached_overview = raster_cache.is_some() && select_overview;
        self.inner.update(cx, |inner, cx| {
            inner.raster_cache_history.clear();
            inner.raster_cache = raster_cache;
            inner.rects = rects;
            inner.polygons = polygons;
            inner.scope_rects = scope_rects;
            inner.dim_hitboxes = dim_hitboxes;
            inner.sse_handles = sse_handles;
            inner.sse_bodies = sse_bodies;
            if refresh_cached_overview {
                // The first exact traversal intentionally avoids expanding
                // millions of shapes on the UI thread. Replace that initial
                // cache with the flattened background overview before the user
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
            .on_action(cx.listener(Self::select_mode))
            .on_action(cx.listener(Self::draw_dim))
            .on_action(cx.listener(Self::edit_action))
            .on_action(cx.listener(Self::fit_to_screen_action))
            .on_action(cx.listener(Self::zero_hierarchy))
            .on_action(cx.listener(Self::one_hierarchy))
            .on_action(cx.listener(Self::all_hierarchy))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::finish_polygon))
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
            hover_hit: None,
            scale: 1.0,
            screen_bounds: Bounds::default(),
            subscriptions: vec![cx.observe(state, |canvas, _, cx| {
                let next_output = canvas
                    .state
                    .read(cx)
                    .solved_cell
                    .read(cx)
                    .as_ref()
                    .map(|solved| solved.output.clone());
                let same_output = canvas
                    .raster_output
                    .as_ref()
                    .zip(next_output.as_ref())
                    .is_some_and(|(old, new)| Arc::ptr_eq(old, new));
                canvas.raster_content_revision = canvas.raster_content_revision.wrapping_add(1);
                canvas
                    .cell_raster_tiles
                    .lock()
                    .expect("cell raster tile cache poisoned")
                    .clear();
                canvas.raster_output = next_output;
                if same_output && canvas.raster_cache.is_some() {
                    // Layer visibility, hierarchy visibility/depth, and theme
                    // changes can replace the current image atomically. Keep
                    // painting it until the background result is complete.
                    canvas.request_navigation_raster(cx);
                } else {
                    canvas.raster_generation = canvas.raster_generation.wrapping_add(1);
                    canvas.raster_cache = None;
                    canvas.raster_cache_history.clear();
                    canvas.navigation_cache_active = false;
                    canvas.raster_refinement = None;
                    canvas.raster_worker_active = false;
                    cx.notify();
                }
            })],
            state: state.clone(),
            rects: Vec::new(),
            polygons: Vec::new(),
            scope_rects: Vec::new(),
            dim_hitboxes: Vec::new(),
            raster_cache: None,
            raster_cache_history: VecDeque::new(),
            navigation_cache_active: false,
            raster_refinement: None,
            raster_worker_active: false,
            raster_generation: 0,
            raster_content_revision: 0,
            raster_output: None,
            cell_raster_tiles: Arc::new(Mutex::new(CellRasterTileCache::default())),
            pending_init: true,
        }
    }

    fn begin_navigation(&mut self) {
        self.raster_generation = self.raster_generation.wrapping_add(1);
        self.navigation_cache_active = true;
        self.hover_hit = None;
    }

    fn install_navigation_raster(&mut self, cache: LayoutRasterCache) {
        if let Some(previous) = self.raster_cache.replace(cache) {
            let current = self.raster_cache.as_ref().unwrap();
            if previous.content_revision != current.content_revision {
                self.raster_cache_history.clear();
                return;
            }
            if same_raster_view(&previous, current) {
                return;
            }
            self.raster_cache_history
                .retain(|historic| !same_raster_view(historic, &previous));
            self.raster_cache_history.push_back(previous);
            while self.raster_cache_history.len() > RASTER_CACHE_HISTORY_LIMIT {
                self.raster_cache_history.pop_front();
            }
        }
    }

    fn navigation_raster_input(
        &self,
        cx: &gpui::App,
        lod_area_px: f32,
        include_text: bool,
    ) -> Option<NavigationRasterInput> {
        let state = self.state.read(cx);
        let solved_cell = state.solved_cell.read(cx).clone()?;
        let screen_size = self.screen_bounds.size;
        let raw_offset = self.offset + Point::new(NAVIGATION_OVERSCAN, NAVIGATION_OVERSCAN);
        Some(NavigationRasterInput {
            solved_cell,
            layers: Arc::new(state.layers.read(cx).layers.clone()),
            hierarchy_depth: state.hierarchy_depth,
            hide_external_geometry: state.hide_external_geometry,
            viewport: ViewportTransform {
                size: Size::new(
                    screen_size.width + NAVIGATION_OVERSCAN * 2.,
                    screen_size.height + NAVIGATION_OVERSCAN * 2.,
                ),
                screen_size,
                scale: self.scale,
                offset: Point::new(
                    align_navigation_raster_offset(raw_offset.x),
                    align_navigation_raster_offset(raw_offset.y),
                ),
            },
            text_color: state.theme().text,
            lod_area_px,
            include_text,
            content_revision: self.raster_content_revision,
            cell_raster_tiles: self.cell_raster_tiles.clone(),
        })
    }

    fn request_navigation_raster(&mut self, cx: &mut Context<Self>) {
        self.navigation_cache_active = true;
        if self.is_dragging && !self.raster_worker_active && self.raster_cache_has_pan_margin() {
            return;
        }
        self.raster_generation = self.raster_generation.wrapping_add(1);
        if self.raster_worker_active {
            return;
        }
        self.raster_worker_active = true;
        self.raster_refinement = Some(cx.spawn(async move |canvas, cx| {
            let mut refinement_generation = None;
            loop {
                let Some((generation, is_refinement, input)) = canvas
                    .update(cx, |canvas, cx| {
                        let generation = canvas.raster_generation;
                        let is_refinement = refinement_generation == Some(generation);
                        canvas
                            .navigation_raster_input(
                                cx,
                                if is_refinement {
                                    NORMAL_NAVIGATION_DETAIL_RANK
                                } else {
                                    INTERACTIVE_NAVIGATION_DETAIL_RANK
                                },
                                is_refinement,
                            )
                            .map(|input| (generation, is_refinement, input))
                    })
                    .ok()
                    .flatten()
                else {
                    let _ = canvas.update(cx, |canvas, cx| {
                        canvas.raster_worker_active = false;
                        canvas.navigation_cache_active = canvas.is_dragging;
                        cx.notify();
                    });
                    return;
                };
                let cache = cx
                    .background_spawn(async move { build_navigation_raster(input) })
                    .await;
                let continue_with_newer_viewport = canvas
                    .update(cx, |canvas, cx| {
                        // Applying an intermediate image is intentional: GPUI
                        // can transform it immediately, so newly exposed areas
                        // become populated during a drag instead of only after
                        // mouse-up. This single sequential worker guarantees
                        // that images can never arrive out of order.
                        let generation_is_current = canvas.raster_generation == generation;
                        if (!is_refinement || generation_is_current)
                            && let Some(cache) = cache
                        {
                            canvas.install_navigation_raster(cache);
                            cx.notify();
                        }
                        if !generation_is_current {
                            (true, None)
                        } else if !is_refinement && !canvas.is_dragging {
                            // Publish a coarse image first, then immediately
                            // replace it with normal detail for the same
                            // viewport. A newer input cancels only this refine
                            // pass, never the already-visible coarse result.
                            (true, Some(generation))
                        } else {
                            canvas.raster_worker_active = false;
                            canvas.navigation_cache_active = canvas.is_dragging;
                            (false, None)
                        }
                    })
                    .unwrap_or((false, None));
                refinement_generation = continue_with_newer_viewport.1;
                if !continue_with_newer_viewport.0 {
                    return;
                }
            }
        }));
    }

    /// Keep translating the existing image while its overscan still covers
    /// the canvas. Besides avoiding unnecessary hierarchy walks, this keeps a
    /// single raster sampling phase throughout most of a pan, so geometry
    /// cannot wobble as same-scale replacement images arrive.
    fn raster_cache_has_pan_margin(&self) -> bool {
        let Some(cache) = self
            .raster_cache
            .as_ref()
            .filter(|cache| cache.content_revision == self.raster_content_revision)
        else {
            return false;
        };
        if cache.scale != self.scale || cache.screen_viewport != self.screen_bounds.size {
            return false;
        }
        let image_bounds = raster_bounds(cache, self.screen_bounds, self.scale, self.offset);
        let canvas_bottom_right = self.screen_bounds.bottom_right();
        let image_bottom_right = image_bounds.bottom_right();
        image_bounds.origin.x <= self.screen_bounds.origin.x - NAVIGATION_OVERSCAN_GUARD
            && image_bounds.origin.y <= self.screen_bounds.origin.y - NAVIGATION_OVERSCAN_GUARD
            && image_bottom_right.x >= canvas_bottom_right.x + NAVIGATION_OVERSCAN_GUARD
            && image_bottom_right.y >= canvas_bottom_right.y + NAVIGATION_OVERSCAN_GUARD
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
                    area: bounds_area(bounds),
                    outline: rect_selection_outline(bounds, rect.border_styles),
                    layer: SelectionLayer::Layout(layer.z),
                    paint_order,
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
                hits.push(SelectionHit {
                    span: span.clone(),
                    area: polygon_area(&points),
                    outline: SelectionOutline::Polygon {
                        points,
                        edge_styles: polygon.edge_styles.clone(),
                    },
                    layer: SelectionLayer::Layout(layer.z),
                    paint_order,
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
                    area: bounds_area(bounds),
                    outline: rect_selection_outline(bounds, bbox.rect.border_styles),
                    layer: SelectionLayer::Scope,
                    paint_order,
                });
            }
        }

        for (paint_order, (span, hitboxes, _)) in self.dim_hitboxes.iter().enumerate() {
            for bounds in hitboxes {
                if bounds.contains(&position) {
                    hits.push(SelectionHit {
                        span: span.clone(),
                        area: bounds_area(*bounds),
                        outline: SelectionOutline::Rect {
                            bounds: *bounds,
                            border_styles: Edges::all(BorderStyle::Solid),
                        },
                        layer: SelectionLayer::Overlay,
                        paint_order,
                    });
                }
            }
        }

        ordered_selection_hits(hits)
    }

    pub(crate) fn fit_to_screen(&mut self, cx: &mut Context<Self>) {
        self.raster_generation = self.raster_generation.wrapping_add(1);
        self.raster_cache = None;
        self.raster_cache_history.clear();
        self.navigation_cache_active = false;
        self.raster_refinement = None;
        self.raster_worker_active = false;
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
                            let point = Point::new(
                                (layout_mouse_position.x * 10.).round() / 10.,
                                (layout_mouse_position.y * 10.).round() / 10.,
                            );
                            if polygon_tool.points.last() != Some(&point) {
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

    pub(crate) fn finish_polygon(
        &mut self,
        _: &Enter,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state.read(cx).tool.clone().update(cx, |tool, cx| {
            let ToolState::DrawPolygon(polygon_tool) = tool else {
                return;
            };
            if polygon_tool.points.len() < 3 {
                let _ = self.state.read(cx).lang_server_client.show_message(
                    MessageType::ERROR,
                    "A polygon requires at least three points before pressing Enter.",
                );
                return;
            }
            let points = polygon_tool
                .points
                .iter()
                .map(|point| (f64::from(point.x), f64::from(point.y)))
                .collect::<Vec<_>>();
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
                    let polygon_name = (0..)
                        .map(|index| format!("polygon{index}"))
                        .find(|name| !names.contains(name))
                        .unwrap();
                    let scope_span = cell.output.cells[&scope_address.cell].scopes
                        [&scope_address.scope]
                        .span
                        .clone();
                    match state.lang_server_client.draw_polygon(
                        scope_span,
                        polygon_name,
                        PolygonParams {
                            layer: layer.clone(),
                            points: points.clone(),
                        },
                    ) {
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
                polygon_tool.points.clear();
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
                ToolState::DrawPolygon(DrawPolygonToolState { points }) if !points.is_empty() => {
                    points.clear();
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
        self.begin_navigation();
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
        if self.is_dragging {
            self.offset = self.offset_start + (event.position - self.drag_start);
            self.hover_hit = None;
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
        self.request_navigation_raster(cx);

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
            SolvedValue::Rect(_) | SolvedValue::Polygon(_) => string_path.push(name),
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
    fn raster_pixel_ranges_are_clipped_to_the_viewport() {
        assert_eq!(raster_pixel_range(-2.5, 2.2, 10), Some((0, 3)));
        assert_eq!(raster_pixel_range(8.1, 12., 10), Some((8, 10)));
        assert_eq!(raster_pixel_range(12., 14., 10), None);
        assert_eq!(raster_pixel_range(4., 4., 10), None);
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
    fn raster_stippling_preserves_transparent_gaps() {
        let mut pixels = vec![0; 10 * 4];
        fill_raster_rect(
            &mut pixels,
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
        );
        let alphas = pixels
            .chunks_exact(4)
            .map(|pixel| pixel[3])
            .collect::<Vec<_>>();
        assert_eq!(alphas, [128, 0, 0, 0, 0, 128, 0, 0, 0, 0]);
    }

    #[test]
    fn raster_cache_overlap_keeps_only_uncovered_regions() {
        let old = Bounds::new(Point::default(), Size::new(px(100.), px(100.)));
        let newer = Bounds::new(Point::new(px(25.), px(20.)), Size::new(px(50.), px(60.)));
        let regions = uncovered_raster_regions(old, &[newer], old);

        assert!(regions.iter().all(|region| !region.intersects(&newer)));
        let remaining_area = regions
            .iter()
            .map(|region| f32::from(region.size.width) * f32::from(region.size.height))
            .sum::<f32>();
        assert_eq!(remaining_area, 10_000. - 3_000.);
    }

    #[test]
    fn raster_cache_regions_are_clipped_to_the_canvas() {
        let cache = Bounds::new(Point::new(px(-20.), px(10.)), Size::new(px(80.), px(80.)));
        let canvas = Bounds::new(Point::default(), Size::new(px(50.), px(50.)));

        assert_eq!(
            uncovered_raster_regions(cache, &[], canvas),
            [Bounds::new(
                Point::new(px(0.), px(10.)),
                Size::new(px(50.), px(40.)),
            )]
        );
    }

    #[test]
    fn subpixel_geometry_is_deduplicated_in_layer_occupancy() {
        let bounds = Bounds::new(Point::new(px(1.1), px(1.1)), Size::new(px(0.2), px(0.2)));
        let mut occupancy = None;
        mark_raster_occupancy(&mut occupancy, 4, 4, bounds);
        mark_raster_occupancy(&mut occupancy, 4, 4, bounds);
        let mut buffer = vec![0; 4 * 4 * 4];
        let mut layer = dimension_rect(0., 1., 0).1;
        layer.fill = ShapeFill::Stippling;

        paint_raster_occupancy(&mut buffer, occupancy.as_ref().unwrap(), 16, &layer);

        assert_eq!(buffer[5 * 4 + 3], 64);
        assert_eq!(
            buffer.chunks_exact(4).filter(|pixel| pixel[3] != 0).count(),
            1
        );
    }

    #[test]
    fn navigation_raster_offset_stays_on_the_half_resolution_pixel_grid() {
        assert_eq!(align_navigation_raster_offset(px(0.9)), px(0.));
        assert_eq!(align_navigation_raster_offset(px(1.1)), px(2.));
        assert_eq!(align_navigation_raster_offset(px(3.1)), px(4.));
        assert_eq!(
            raster_logical_size(501, 251),
            Size::new(px(1002.), px(502.))
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

        paint_raster_tile(&mut buffer, 2, 2, &primitive, &layer);

        assert!(buffer.chunks_exact(4).all(|pixel| pixel[3] == 64));
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

        paint_raster_tile(&mut buffer, 5, 5, &primitive, &layer);

        assert_eq!(
            buffer.chunks_exact(4).filter(|pixel| pixel[3] != 0).count(),
            5
        );
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

    fn selection_hit(name: &str, layer: SelectionLayer, size: f32) -> SelectionHit {
        let bounds = Bounds::new(Point::default(), Size::new(px(size), px(size)));
        SelectionHit {
            span: Span {
                path: std::path::PathBuf::from(format!("{name}.ar")),
                span: cfgrammar::Span::new(0, 1),
            },
            area: bounds_area(bounds),
            outline: SelectionOutline::Rect {
                bounds,
                border_styles: Edges::all(BorderStyle::Solid),
            },
            layer,
            paint_order: 0,
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
    fn selection_prefers_smallest_hit_box_on_the_same_layer() {
        let hits = vec![
            selection_hit("large", SelectionLayer::Layout(3), 100.),
            selection_hit("small", SelectionLayer::Layout(3), 10.),
        ];

        let selected = choose_selection_hit(hits, None, false).unwrap();
        assert_eq!(selected.span.path, std::path::PathBuf::from("small.ar"));
    }

    #[test]
    fn selection_prefers_higher_layers_before_area() {
        let hits = vec![
            selection_hit("small-low", SelectionLayer::Layout(2), 10.),
            selection_hit("large-high", SelectionLayer::Layout(3), 100.),
        ];

        let selected = choose_selection_hit(hits, None, false).unwrap();
        assert_eq!(
            selected.span.path,
            std::path::PathBuf::from("large-high.ar")
        );
    }

    #[test]
    fn dimension_edges_use_normal_selection_priority() {
        let rects = vec![
            dimension_rect(10., 10., 2),
            dimension_rect(20., 100., 3),
            dimension_rect(30., 5., 3),
        ];

        let ordered = ordered_dimension_rects(&rects, &[]);

        // Higher z wins even when larger; among equal-z shapes, smaller wins.
        assert_eq!(
            ordered.iter().map(|rect| rect.x0).collect::<Vec<_>>(),
            [30., 20., 10.]
        );
    }

    #[test]
    fn command_click_cycles_through_hits_and_wraps() {
        let small = selection_hit("small", SelectionLayer::Layout(3), 10.);
        let large = selection_hit("large", SelectionLayer::Layout(3), 100.);
        let hits = vec![large.clone(), small.clone()];

        let next = choose_selection_hit(hits.clone(), Some(&small.span), true).unwrap();
        assert_eq!(next.span, large.span);
        let wrapped = choose_selection_hit(hits, Some(&large.span), true).unwrap();
        assert_eq!(wrapped.span, small.span);
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
    }
}
