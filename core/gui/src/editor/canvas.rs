use std::{
    collections::VecDeque,
    fmt::Debug,
    ops::{Add, Sub},
};

use compiler::{
    ast::Span,
    compile::{self, ObjectId, SolvedValue, ifmatvec},
    solver::Var,
};
use enumify::enumify;
use geometry::{dir::Dir, transform::TransformationMatrix};
use gpui::{
    BorderStyle, Bounds, Context, Corners, DefiniteLength, DragMoveEvent, Edges, Element, Entity,
    FocusHandle, Focusable, InteractiveElement, IntoElement, Length, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, ParentElement, Pixels, Point, Render, Rgba,
    ScrollWheelEvent, SharedString, Size, Style, Styled, Subscription, Window, div, pattern_slash,
    px, rgb, size, solid_background,
};
use indexmap::IndexSet;
use itertools::Itertools;
use lang_server::rpc::DimensionParams;
use tower_lsp_server::lsp_types::MessageType;

use crate::{
    actions::*,
    editor::{self, CompileOutputState, EditorState, LayerState, ScopeAddress},
};

#[derive(Copy, Clone, PartialEq)]
pub enum ShapeFill {
    Stippling,
    Solid,
}

const SELECT_WIDTH: Pixels = px(3.);
const DEFAULT_BORDER_WIDTH: Pixels = px(2.);

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
        }
    }
}

impl Rect {
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
    pub(crate) dim: Span,
    pub(crate) original_value: SharedString,
    /// `true` if entered from dimension tool
    pub(crate) dim_mode: bool,
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

#[enumify]
#[derive(Debug, Clone)]
pub(crate) enum ToolState {
    DrawRect(DrawRectToolState),
    DrawDim(DrawDimToolState),
    EditDim(EditDimToolState),
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
    // drag state
    is_dragging: bool,
    drag_start: Point<Pixels>,
    offset_start: Point<Pixels>,
    mouse_position: Point<Pixels>,
    // zoom state
    scale: f32,
    screen_bounds: Bounds<Pixels>,
    #[allow(unused)]
    subscriptions: Vec<Subscription>,
    rects: Vec<(Rect, LayerState)>,
    scope_rects: Vec<Rect>,
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
    Bounds::new(
        Point::new(scale * px(r.x0), scale * px(-r.y1)) + offset + bounds.origin,
        Size::new(scale * px(r.x1 - r.x0), scale * px(r.y1 - r.y0)),
    )
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

        // TODO: Clean up code.
        let mut rects = Vec::new();
        let mut dims = Vec::new();
        let mut scope_rects = Vec::new();
        let mut select_rects = Vec::new();
        let layout_mouse_position = inner.px_to_layout(inner.mouse_position);
        if let Some(solved_cell) = solved_cell {
            let scope_address = &solved_cell.state[&solved_cell.selected_scope].address;
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
                            scope_rects.push(rect);
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
                            let p0p = ifmatvec(mat, (rect.x0.0, rect.y0.0));
                            let p1p = ifmatvec(mat, (rect.x1.0, rect.y1.0));
                            let layer = rect
                                .layer
                                .as_ref()
                                .and_then(|layer| layers.layers.get(layer.as_str()));
                            if let Some(layer) = layer
                                && !rect.construction
                            {
                                let rect =
                                    Rect {
                                        x0: (p0p.0.min(p1p.0) + ofs.0) as f32,
                                        y0: (p0p.1.min(p1p.1) + ofs.1) as f32,
                                        x1: (p0p.0.max(p1p.0) + ofs.0) as f32,
                                        y1: (p0p.1.max(p1p.1) + ofs.1) as f32,
                                        id: rect.span.clone(),
                                        object_path,
                                        border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
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
                                    };
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
                        SolvedValue::Instance(inst) => {
                            let mut inst_mat = TransformationMatrix::identity();
                            if inst.reflect {
                                inst_mat = inst_mat.reflect_vert()
                            }
                            inst_mat = inst_mat.rotate(inst.angle);
                            let inst_ofs = ifmatvec(mat, (inst.x, inst.y));

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
                                    let rect = Rect {
                                        x0: (p0p.0.min(p1p.0) + new_ofs.0) as f32,
                                        y0: (p0p.1.min(p1p.1) + new_ofs.1) as f32,
                                        x1: (p0p.0.max(p1p.0) + new_ofs.0) as f32,
                                        y1: (p0p.1.max(p1p.1) + new_ofs.1) as f32,
                                        id: Some(inst.span.clone()),
                                        object_path: object_path.clone(),
                                        border_widths: Edges::all(DEFAULT_BORDER_WIDTH),
                                        border_styles: Edges::all(BorderStyle::Solid),
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
                                        scope_rects.push(rect);
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
                        SolvedValue::Text(_) => {}
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
                    },
                    layers.layers[layers.selected_layer.as_ref().unwrap()].clone(),
                ));
            }
        }

        let rects = rects
            .into_iter()
            .sorted_by_key(|(_, layer)| layer.z)
            .collect_vec();
        let scale = inner.scale;
        let offset = inner.offset;
        let mut dim_hitboxes = Vec::new();
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
                    for r in &scope_rects {
                        window.paint_quad(get_paint_quad(
                            get_rect_bounds(r, bounds, scale, offset),
                            ShapeFill::Solid,
                            Rgba { a: 0., ..theme.text },
                            theme.text,
                            r.border_widths,
                            r.border_styles,
                        ));
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
                            dim.p as f32,
                            dim.n as f32,
                            dim.coord as f32,
                            dim.pstop as f32,
                            dim.nstop as f32,
                            dim.horiz,
                            format!("{:.3}", dim.value), // TODO: show actual expression
                            match &tool {
                                ToolState::Select(SelectToolState {
                                    selected_obj: Some(selected),
                                })
                                | ToolState::EditDim(EditDimToolState { dim: selected, .. })
                                    if Some(selected) == dim.span.as_ref() =>
                                {
                                    rgb(0xffff00)
                                }
                                _ => theme.text,
                            },
                            dim.span.as_ref(),
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
                        ToolState::DrawDim(dim_tool) => {
                            if dim_tool.edges.len() < 2 {
                                let rects = rects
                                    .iter()
                                    .rev()
                                    .sorted_by_key(|(_, layer)| usize::MAX - layer.z)
                                    .map(|(r, _)| r);
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
                                for (rect, r) in rects.map(|r| {
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
                                            && rect.id.is_some()
                                        {
                                            selected =
                                                Some(DimEdge::Edge((rect, name, edge_layout)));
                                            break;
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
                        }
                        ToolState::Select(_) => {
                            let rects = inner
                                .rects
                                .iter()
                                .rev()
                                .sorted_by_key(|(_, layer)| usize::MAX - layer.z)
                                .map(|(r, _)| r);
                            let scale = inner.scale;
                            let offset = inner.offset;
                            for hitbox in rects
                                .chain(scope_rects.iter())
                                .filter_map(|r| {
                                    r.id.as_ref().map(|_| {
                                        Bounds::new(
                                            Point::new(scale * px(r.x0), scale * px(-r.y1))
                                                + offset
                                                + inner.screen_bounds.origin,
                                            Size::new(
                                                scale * px(r.x1 - r.x0),
                                                scale * px(r.y1 - r.y0),
                                            ),
                                        )
                                    })
                                })
                                .chain(
                                    inner
                                        .dim_hitboxes
                                        .iter()
                                        .flat_map(|(_, hitboxes, _)| hitboxes.iter().copied()),
                                )
                            {
                                if hitbox.contains(&inner.mouse_position) {
                                    window.paint_quad(get_paint_quad(
                                        hitbox,
                                        ShapeFill::Solid,
                                        Rgba { a: 0., ..rgb(0xffff00) },
                                        rgb(0xffff00),
                                        Edges::all(SELECT_WIDTH),
                                Edges::all(BorderStyle::Solid),
                                    ));
                                    break;
                                }
                            }
                        }
                        _ => {}
                    }
                })
            });
        self.inner.update(cx, |inner, cx| {
            inner.rects = rects;
            inner.scope_rects = scope_rects;
            inner.dim_hitboxes = dim_hitboxes;
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
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_left_mouse_down))
            // TODO: Uncomment once GPUI mouse movement is fixed.
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            // .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_action(cx.listener(Self::draw_rect))
            .on_action(cx.listener(Self::select_mode))
            .on_action(cx.listener(Self::draw_dim))
            .on_action(cx.listener(Self::edit_action))
            .on_action(cx.listener(Self::fit_to_screen_action))
            .on_action(cx.listener(Self::zero_hierarchy))
            .on_action(cx.listener(Self::one_hierarchy))
            .on_action(cx.listener(Self::all_hierarchy))
            .on_action(cx.listener(Self::command_action))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::dark_mode))
            .on_action(cx.listener(Self::light_mode))
            .on_drag_move(cx.listener(Self::on_drag_move))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::on_mouse_up))
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
            drag_start: Point::default(),
            offset_start: Point::default(),
            mouse_position: Point::default(),
            scale: 1.0,
            screen_bounds: Bounds::default(),
            subscriptions: vec![cx.observe(state, |_, _, cx| cx.notify())],
            state: state.clone(),
            rects: Vec::new(),
            scope_rects: Vec::new(),
            dim_hitboxes: Vec::new(),
            pending_init: true,
        }
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
            let scalex = self.screen_bounds.size.width / (bbox.x1 - bbox.x0) as f32;
            let scaley = self.screen_bounds.size.height / (bbox.y1 - bbox.y0) as f32;
            self.scale = 0.9 * f32::from(scalex.min(scaley));
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
                                                    Err(e) => Some(format!("{e}").into()),
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
                            let res = state.lang_server_client.show_message(
                                MessageType::ERROR,
                                "Cannot draw on an invisible layer.",
                            );
                            if let Err(e) = res {
                                self.state.update(cx, |state, _cx| {
                                    state.fatal_error = Some(format!("{e}").into());
                                });
                            }
                        }
                    } else {
                        let res = state
                            .lang_server_client
                            .show_message(MessageType::ERROR, "No layer has been selected.");
                        if let Err(e) = res {
                            self.state.update(cx, |state, _cx| {
                                state.fatal_error = Some(format!("{e}").into());
                            });
                        }
                    }
                }
                ToolState::DrawDim(dim_tool) => {
                    let enter_entry_mode = if dim_tool.edges.len() < 2 {
                        let rects = self
                            .rects
                            .iter()
                            .rev()
                            .sorted_by_key(|(_, layer)| usize::MAX - layer.z)
                            .map(|(r, _)| r);
                        let scale = self.scale;
                        let offset = self.offset;
                        let mut selected = None;
                        if x_axis.select_bounds(SELECT_WIDTH).contains(&event.position) {
                            selected = Some(DimEdge::Y0);
                        }
                        if y_axis.select_bounds(SELECT_WIDTH).contains(&event.position) {
                            selected = Some(DimEdge::X0);
                        }
                        for (rect, r) in rects.map(|r| {
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
                                if bounds.contains(&event.position) && rect.id.is_some() {
                                    selected = Some(DimEdge::Edge((rect, name, edge_layout)));
                                    break;
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

                        let span_value = if dim_tool.edges.len() == 1
                            && let DimEdge::Edge(edge) = &dim_tool.edges[0]
                        {
                            let (left, right, coord, horiz) = match edge.2.dir {
                                Dir::Horiz => ("x0", "x1", layout_mouse_position.y, "true"),
                                Dir::Vert => ("y0", "y1", layout_mouse_position.x, "false"),
                            };

                            let value = format!("{:?}", edge.2.stop - edge.2.start);
                            let res = state.lang_server_client.draw_dimension(
                                cell.output.cells[&selected_scope_addr.cell].scopes
                                    [&selected_scope_addr.scope]
                                    .span
                                    .clone(),
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
                            );
                            if let Some(error) = match &res {
                                Ok(None) => Some("inconsistent editor and GUI state".into()),
                                Ok(Some(_)) => None,
                                Err(e) => Some(format!("{e}").into()),
                            } {
                                self.state.update(cx, |state, _cx| {
                                    state.fatal_error = Some(error);
                                });
                            }

                            res.unwrap_or_default().map(|span| (span, value))
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
                                    let value = format!("{:?}", right.2.coord - left.2.coord);
                                    let res = state.lang_server_client.draw_dimension(
                                        cell.output.cells[&selected_scope_addr.cell].scopes
                                            [&selected_scope_addr.scope]
                                            .span
                                            .clone(),
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
                                    );
                                    if let Some(error) = match &res {
                                        Ok(None) => {
                                            Some("inconsistent editor and GUI state".into())
                                        }
                                        Ok(Some(_)) => None,
                                        Err(e) => Some(format!("{e}").into()),
                                    } {
                                        self.state.update(cx, |state, _cx| {
                                            state.fatal_error = Some(error);
                                        });
                                    }

                                    res.unwrap_or_default().map(|span| (span, value))
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
                                    let (p, n, value, pstop, nstop) = if edge.2.coord < 0. {
                                        (
                                            "0.".to_string(),
                                            format!("{}.{}", edge.0, edge.1),
                                            format!("{:?}", -edge.2.coord),
                                            coord.clone(),
                                            pnstop,
                                        )
                                    } else {
                                        (
                                            format!("{}.{}", edge.0, edge.1),
                                            "0.".to_string(),
                                            format!("{:?}", edge.2.coord),
                                            pnstop,
                                            coord.clone(),
                                        )
                                    };
                                    let res = state.lang_server_client.draw_dimension(
                                        cell.output.cells[&selected_scope_addr.cell].scopes
                                            [&selected_scope_addr.scope]
                                            .span
                                            .clone(),
                                        DimensionParams {
                                            p,
                                            n,
                                            value: value.clone(),
                                            coord,
                                            pstop,
                                            nstop,
                                            horiz: horiz.to_string(),
                                        },
                                    );
                                    if let Some(error) = match &res {
                                        Ok(None) => {
                                            Some("inconsistent editor and GUI state".into())
                                        }
                                        Ok(Some(_)) => None,
                                        Err(e) => Some(format!("{e}").into()),
                                    } {
                                        self.state.update(cx, |state, _cx| {
                                            state.fatal_error = Some(error);
                                        });
                                    }

                                    res.unwrap_or_default().map(|span| (span, value))
                                }
                                _ => unreachable!(),
                            }
                        } else {
                            None
                        };
                        if let Some((span, value)) = span_value {
                            *tool = ToolState::EditDim(EditDimToolState {
                                dim: span,
                                original_value: SharedString::from(value),
                                dim_mode: true,
                            });
                            edit_dim = true;
                            cx.notify();
                        }
                    }
                }
                ToolState::Select(select_tool) => {
                    let rects = self
                        .rects
                        .iter()
                        .rev()
                        .sorted_by_key(|(_, layer)| usize::MAX - layer.z)
                        .map(|(r, _)| r);
                    let scale = self.scale;
                    let offset = self.offset;
                    let mut selected_obj = None;
                    for (span, bounds) in rects
                        .chain(self.scope_rects.iter())
                        .filter_map(|r| {
                            let rect_bounds = Bounds::new(
                                Point::new(scale * px(r.x0), scale * px(-r.y1))
                                    + offset
                                    + self.screen_bounds.origin,
                                Size::new(scale * px(r.x1 - r.x0), scale * px(r.y1 - r.y0)),
                            );
                            Some((r.id.as_ref()?, rect_bounds))
                        })
                        .chain(self.dim_hitboxes.iter().flat_map(|(span, hitboxes, _)| {
                            hitboxes.iter().map(|hitbox| (span, *hitbox)).collect_vec()
                        }))
                    {
                        if bounds.contains(&event.position) {
                            selected_obj = Some(span);
                            break;
                        }
                    }
                    if let Some(span) = selected_obj {
                        select_tool.selected_obj = Some(span.clone());
                        if let Err(e) = self
                            .state
                            .read(cx)
                            .lang_server_client
                            .select_rect(span.clone())
                        {
                            self.state.update(cx, |state, cx| {
                                state.fatal_error =
                                    Some(format!("Editing disabled due to error {e}").into());
                                cx.notify();
                            });
                        }
                    } else {
                        select_tool.selected_obj = None;
                    }
                    cx.notify();
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
                    dim: obj.clone(),
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

    pub(crate) fn command_action(
        &mut self,
        _: &Command,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        window.focus(&self.text_input_focus_handle);
        window.prevent_default();
        cx.notify();
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

    pub(crate) fn on_mouse_down(
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
        if self.is_dragging {
            self.offset = self.offset_start + (event.position - self.drag_start);
        }
        cx.notify();
    }

    pub(crate) fn on_drag_move(
        &mut self,
        _event: &DragMoveEvent<()>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.is_dragging = false;
    }

    pub(crate) fn on_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.is_dragging = false;
    }

    pub(crate) fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_dragging {
            // Do not allow zooming during a drag.
            return;
        }
        let new_scale = {
            let delta = event.delta.pixel_delta(px(20.));
            let ns = self.scale + f32::from(delta.y) / 400.;
            f32::clamp(ns, 0.01, 100.)
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
    if let Some(name) = reachable_objs.swap_remove(obj)
        && cell.output.cells[&current_scope.cell].objects[obj].is_rect()
    {
        string_path.push(name);
    } else {
        reachable = false;
    }
    (reachable, string_path)
}
