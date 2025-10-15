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
    App, AppContext, BorderStyle, Bounds, Context, Corners, DefiniteLength, DragMoveEvent, Edges,
    Element, Entity, FocusHandle, Focusable, InteractiveElement, IntoElement, Length, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, ParentElement, Pixels, Point, Render,
    Rgba, ScrollWheelEvent, SharedString, Size, Style, Styled, Subscription, Window, div,
    pattern_slash, rgb, rgba, solid_background,
};
use indexmap::IndexSet;
use itertools::Itertools;
use lsp_server::rpc::DimensionParams;

use crate::{
    actions::{All, Cancel, DrawDim, DrawRect, Fit, One, Zero},
    editor::{self, CompileOutputState, EditorState, LayerState, ScopeAddress},
};

#[derive(Copy, Clone, PartialEq)]
pub enum ShapeFill {
    Stippling,
    Solid,
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

#[derive(Debug, Clone)]
pub(crate) struct DrawRectToolState {
    p0: Option<Point<f32>>,
}

#[derive(Debug, Clone)]
pub(crate) struct DrawDimToolState {
    pub(crate) edges: Vec<(String, String, Edge<f32>)>,
}

#[derive(Debug, Clone)]
pub(crate) struct EditDimToolState {
    pub(crate) dim: Span,
}

// TODO: potentially re-use compiler provided object IDs
#[derive(Copy, Clone, Hash, PartialEq, Eq, Debug)]
pub struct GlobalObjectId {
    scope: ScopeAddress,
    idx: usize,
}

#[derive(Debug, Clone)]
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
        ToolState::Select(SelectToolState { selected_obj: None })
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
    pub(crate) tool: Entity<ToolState>,
    mouse_position: Point<Pixels>,
    // zoom state
    scale: f32,
    screen_bounds: Bounds<Pixels>,
    #[allow(unused)]
    subscriptions: Vec<Subscription>,
    rects: Vec<(Rect, LayerState)>,
    scope_rects: Vec<Rect>,
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

fn get_paint_path(
    r: &Rect,
    bounds: Bounds<Pixels>,
    scale: f32,
    offset: Point<Pixels>,
    color: Rgba,
    thickness: Pixels,
) -> Option<PaintQuad> {
    let rect_bounds = Bounds::new(
        Point::new(
            scale * Pixels(r.x0) - thickness / 2.,
            scale * Pixels(-r.y1) - thickness / 2.,
        ) + offset
            + bounds.origin,
        Size::new(
            scale * Pixels(r.x1 - r.x0) + thickness,
            scale * Pixels(r.y1 - r.y0) + thickness,
        ),
    );
    intersect(&rect_bounds, &bounds).map(|clipped| PaintQuad {
        bounds: clipped,
        corner_radii: Corners::all(Pixels(0.)),
        background: solid_background(color),
        border_widths: Edges::all(Pixels(0.)),
        border_color: rgba(0).into(),
        border_style: BorderStyle::Solid,
    })
}

fn get_paint_quad(
    r: &Rect,
    bounds: Bounds<Pixels>,
    scale: f32,
    offset: Point<Pixels>,
    fill: ShapeFill,
    color: Rgba,
    border_color: Rgba,
) -> Option<PaintQuad> {
    let rect_bounds = Bounds::new(
        Point::new(
            scale * Pixels(r.x0) - Pixels(1.),
            scale * Pixels(-r.y1) - Pixels(1.),
        ) + offset
            + bounds.origin,
        Size::new(
            scale * Pixels(r.x1 - r.x0) + Pixels(2.),
            scale * Pixels(r.y1 - r.y0) + Pixels(2.),
        ),
    );
    let background = match fill {
        ShapeFill::Solid => solid_background(color),
        ShapeFill::Stippling => pattern_slash(color.into(), 1., 9.),
    };
    if let Some(clipped) = intersect(&rect_bounds, &bounds) {
        let left_border = f32::clamp((rect_bounds.left().0 + 2.) - bounds.left().0, 0., 2.);
        let right_border = f32::clamp(bounds.right().0 - (rect_bounds.right().0 - 2.), 0., 2.);
        let top_border = f32::clamp((rect_bounds.top().0 + 2.) - bounds.top().0, 0., 2.);
        let bot_border = f32::clamp(bounds.bottom().0 - (rect_bounds.bottom().0 - 2.), 0., 2.);
        let mut border_widths = Edges::all(Pixels(2.));
        border_widths.left = Pixels(left_border);
        border_widths.right = Pixels(right_border);
        border_widths.top = Pixels(top_border);
        border_widths.bottom = Pixels(bot_border);
        Some(PaintQuad {
            bounds: clipped,
            corner_radii: Corners::all(Pixels(0.)),
            background,
            border_widths,
            border_color: border_color.into(),
            border_style: BorderStyle::Solid,
        })
    } else {
        None
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
        let tool = inner.tool.read(cx).clone();
        let state = inner.state.read(cx);
        let layers = state.layers.read(cx);

        // TODO: Clean up code.
        let mut rects = Vec::new();
        let mut dims = Vec::new();
        let mut scope_rects = Vec::new();
        let mut select_rects = Vec::new();
        if let Some(solved_cell) = solved_cell {
            let scope_address = &solved_cell.state[&solved_cell.selected_scope].address;
            let mut queue = VecDeque::from_iter([(
                ScopeAddress {
                    cell: scope_address.cell,
                    scope: solved_cell.output.cells[&scope_address.cell].root,
                },
                TransformationMatrix::identity(),
                (0., 0.),
                0,
                true,
                vec![],
            )]);
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
                if show && (depth >= state.hierarchy_depth || !scope_state.visible) {
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
                        };
                        if let ToolState::Select(SelectToolState { selected_obj }) =
                            inner.tool.read(cx)
                            && &rect.id == selected_obj
                        {
                            select_rects.push(rect.clone());
                        }
                        scope_rects.push(rect);
                    }
                    show = false;
                }
                for (obj, _) in &scope_info.emit {
                    let mut object_path = path.clone();
                    object_path.push(*obj);
                    let value = &cell_info.objects[obj];
                    match value {
                        SolvedValue::Rect(rect) => {
                            if show {
                                let p0p = ifmatvec(mat, (rect.x0.0, rect.y0.0));
                                let p1p = ifmatvec(mat, (rect.x1.0, rect.y1.0));
                                let layer = rect
                                    .layer
                                    .as_ref()
                                    .and_then(|layer| layers.layers.get(layer.as_str()));
                                if let Some(layer) = layer
                                    && layer.visible
                                {
                                    let rect = Rect {
                                        x0: (p0p.0.min(p1p.0) + ofs.0) as f32,
                                        y0: (p0p.1.min(p1p.1) + ofs.1) as f32,
                                        x1: (p0p.0.max(p1p.0) + ofs.0) as f32,
                                        y1: (p0p.1.max(p1p.1) + ofs.1) as f32,
                                        id: rect.span.clone(),
                                        object_path,
                                    };
                                    if let ToolState::Select(SelectToolState { selected_obj }) =
                                        inner.tool.read(cx)
                                        && &rect.id == selected_obj
                                    {
                                        select_rects.push(rect.clone());
                                    }
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
                            if show && (depth + 1 >= state.hierarchy_depth || !scope_state.visible)
                            {
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
                                    };
                                    if let ToolState::Select(SelectToolState { selected_obj }) =
                                        inner.tool.read(cx)
                                        && &rect.id == selected_obj
                                    {
                                        select_rects.push(rect.clone());
                                    }
                                    scope_rects.push(rect);
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
                    }
                }
                dims.extend(
                    cell_info
                        .objects
                        .values()
                        .filter_map(|obj| obj.get_dimension().cloned()),
                );
                for child in &scope_info.children {
                    let scope_address = ScopeAddress {
                        scope: *child,
                        cell,
                    };
                    queue.push_back((scope_address, mat, ofs, depth + 1, show, path.clone()));
                }
            }

            let layout_mouse_position = inner.px_to_layout(inner.mouse_position);
            if let ToolState::DrawRect(DrawRectToolState { p0: Some(p0) }) = tool {
                rects.push((
                    Rect {
                        object_path: Vec::new(),
                        x0: p0.x.min(layout_mouse_position.x),
                        y0: p0.y.min(layout_mouse_position.y),
                        x1: p0.x.max(layout_mouse_position.x),
                        y1: p0.y.max(layout_mouse_position.y),
                        id: None,
                    },
                    layers.layers[layers.selected_layer.as_ref().unwrap()].clone(),
                ));
            }
        }

        let layout_mouse_position = inner.px_to_layout(inner.mouse_position);
        let rects = rects
            .into_iter()
            .sorted_by_key(|(_, layer)| layer.z)
            .collect_vec();
        let scale = inner.scale;
        let offset = inner.offset;
        inner
            .bg_style
            .clone()
            .paint(bounds, window, cx, |window, cx| {
                window.paint_layer(bounds, |window| {
                    for (r, l) in &rects {
                        if let Some(quad) = get_paint_quad(
                            r,
                            bounds,
                            scale,
                            offset,
                            l.fill,
                            l.color,
                            l.border_color,
                        ) {
                            window.paint_quad(quad.clone());
                        }
                    }
                    for r in &scope_rects {
                        if let Some(quad) = get_paint_quad(
                            r,
                            bounds,
                            scale,
                            offset,
                            ShapeFill::Solid,
                            rgba(0),
                            rgb(0xffffff),
                        ) {
                            window.paint_quad(quad);
                        }
                    }
                    for r in &select_rects {
                        if let Some(quad) = get_paint_quad(
                            r,
                            bounds,
                            scale,
                            offset,
                            ShapeFill::Solid,
                            rgba(0),
                            rgb(0xffff00),
                        ) {
                            window.paint_quad(quad);
                        }
                    }

                    let draw_dim = |window: &mut Window,
                                    cx: &mut App,
                                    p: f32,
                                    n: f32,
                                    coord: f32,
                                    pstop: f32,
                                    nstop: f32,
                                    horiz: bool,
                                    value: String,
                                    color: Rgba| {
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
                        };
                        for r in &[start_line, stop_line, dim_line] {
                            if let Some(quad) =
                                get_paint_path(r, bounds, scale, offset, color, Pixels(2.))
                            {
                                window.paint_quad(quad);
                            }
                        }

                        let run_len = value.len();
                        let text_origin = self
                            .inner
                            .read(cx)
                            .layout_to_px(Point::new((x0 + x1) / 2., (y0 + y1) / 2.));
                        window
                            .text_system()
                            .shape_line(
                                SharedString::from(value),
                                Pixels(14.),
                                &[window.text_style().to_run(run_len)],
                            )
                            .paint(text_origin, Pixels(16.), window, cx)
                            .unwrap();
                    };

                    for dim in dims {
                        draw_dim(
                            window,
                            cx,
                            dim.p.0 as f32,
                            dim.n.0 as f32,
                            dim.coord.0 as f32,
                            dim.pstop.0 as f32,
                            dim.nstop.0 as f32,
                            dim.horiz,
                            format!("{:.3}", dim.value.0), // TODO: show actual expression
                            match &tool {
                                ToolState::Select(SelectToolState {
                                    selected_obj: Some(selected),
                                })
                                | ToolState::EditDim(EditDimToolState { dim: selected })
                                    if Some(selected) == dim.span.as_ref() =>
                                {
                                    rgb(0xff0000)
                                }
                                _ => rgb(0xffffff),
                            },
                        );
                    }

                    if let ToolState::DrawDim(DrawDimToolState { edges }) = &tool {
                        // draw dimension lines
                        if edges.len() == 1 {
                            let edge = &edges[0].2;
                            let coord = match edge.dir {
                                Dir::Horiz => layout_mouse_position.y,
                                Dir::Vert => layout_mouse_position.x,
                            };
                            draw_dim(
                                window,
                                cx,
                                edge.start,
                                edge.stop,
                                coord,
                                edge.coord,
                                edge.coord,
                                edge.dir == Dir::Horiz,
                                format!("{:.3}", (edge.stop - edge.start).abs()),
                                rgb(0xff0000),
                            );
                        } else if edges.len() == 2 {
                            let edge0 = &edges[0].2;
                            let edge1 = &edges[1].2;
                            let coord = match edge0.dir {
                                Dir::Horiz => layout_mouse_position.x,
                                Dir::Vert => layout_mouse_position.y,
                            };
                            draw_dim(
                                window,
                                cx,
                                edge0.coord,
                                edge1.coord,
                                coord,
                                (edge0.start + edge0.stop) / 2.,
                                (edge1.start + edge1.stop) / 2.,
                                edge0.dir == Dir::Vert,
                                format!("{:.3}", (edge1.coord - edge0.coord).abs()),
                                rgb(0xff0000),
                            );
                        }
                        // highlight selected edges
                        for (_, _, edge) in edges {
                            let (x0, y0, x1, y1) = match edge.dir {
                                Dir::Horiz => (edge.start, edge.coord, edge.stop, edge.coord),
                                Dir::Vert => (edge.coord, edge.start, edge.coord, edge.stop),
                            };
                            if let Some(quad) = get_paint_path(
                                &Rect {
                                    object_path: Vec::new(),
                                    x0,
                                    y0,
                                    x1,
                                    y1,
                                    id: None,
                                },
                                bounds,
                                scale,
                                offset,
                                rgb(0xffff00),
                                Pixels(2.),
                            ) {
                                window.paint_quad(quad);
                            }
                        }
                    }
                })
            });
        self.inner.update(cx, |inner, cx| {
            inner.rects = rects;
            inner.scope_rects = scope_rects;
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
            .on_action(cx.listener(Self::draw_dim))
            .on_action(cx.listener(Self::fit_to_screen_action))
            .on_action(cx.listener(Self::zero_hierarchy))
            .on_action(cx.listener(Self::one_hierarchy))
            .on_action(cx.listener(Self::all_hierarchy))
            .on_action(cx.listener(Self::cancel))
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
            offset: Point::new(Pixels(0.), Pixels(0.)),
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
            tool: cx.new(|_cx| ToolState::default()),
            scale: 1.0,
            screen_bounds: Bounds::default(),
            subscriptions: vec![cx.observe(state, |_, _, cx| cx.notify())],
            state: state.clone(),
            rects: Vec::new(),
            scope_rects: Vec::new(),
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
            let scalex = self.screen_bounds.size.width.0 / (bbox.x1 - bbox.x0) as f32;
            let scaley = self.screen_bounds.size.height.0 / (bbox.y1 - bbox.y0) as f32;
            self.scale = 0.9 * scalex.min(scaley);
            self.offset = Point::new(
                Pixels(
                    (-(bbox.x0 + bbox.x1) as f32 * self.scale + self.screen_bounds.size.width.0)
                        / 2.,
                ),
                Pixels(
                    ((bbox.y1 + bbox.y0) as f32 * self.scale + self.screen_bounds.size.height.0)
                        / 2.,
                ),
            );
        } else {
            self.offset = Point::new(Pixels(0.), self.screen_bounds.size.height);
        }
    }

    pub(crate) fn on_left_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let layout_mouse_position = self.px_to_layout(event.position);
        self.tool.update(cx, |tool, cx| {
            match tool {
                ToolState::DrawRect(rect_tool) => {
                    if let Some(p0) = rect_tool.p0 {
                        rect_tool.p0 = None;
                        let p1 = layout_mouse_position;
                        let p0p = Point::new(f32::min(p0.x, p1.x), f32::min(p0.y, p1.y));
                        let p1p = Point::new(f32::max(p0.x, p1.x), f32::max(p0.y, p1.y));
                        self.state.update(cx, |state, cx| {
                            state.solved_cell.update(cx, {
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
                                        let names: IndexSet<_> = reachable_objs.values().collect();
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

                                        state.lsp_client.draw_rect(
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
                                            },
                                        );
                                    }
                                }
                            });
                        });
                    } else {
                        // TODO: error handling.
                        if self.state.read(cx).layers.read(cx).selected_layer.is_some() {
                            let p0 = self.px_to_layout(event.position);
                            rect_tool.p0 = Some(p0);
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
                        for (rect, r) in rects.map(|r| {
                            (
                                r,
                                Bounds::new(
                                    Point::new(scale * Pixels(r.x0), scale * Pixels(-r.y1))
                                        + offset
                                        + self.screen_bounds.origin,
                                    Size::new(
                                        scale * Pixels(r.x1 - r.x0),
                                        scale * Pixels(r.y1 - r.y0),
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
                                let bounds = edge_px.select_bounds(Pixels(4.));
                                if bounds.contains(&event.position) && rect.id.is_some() {
                                    selected = Some((rect, name, edge_layout));
                                }
                            }
                        }
                        let enter_entry_mode = !dim_tool.edges.is_empty();
                        if let Some((r, name, edge)) = selected {
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
                                    .map(|(_, _, edge0)| edge0.dir == edge.dir)
                                    .unwrap_or(true)
                            {
                                dim_tool.edges.push((path, name.to_string(), edge));
                                false
                            } else {
                                enter_entry_mode
                            }
                        } else {
                            enter_entry_mode
                        }
                    } else {
                        true
                    };
                    // TODO: add dimension constraint to code here instead of in input.rs
                    let state = self.state.read(cx);

                    if enter_entry_mode && let Some(cell) = state.solved_cell.read(cx) {
                        let selected_scope_addr = cell.state[&cell.selected_scope].address;

                        let span = if dim_tool.edges.len() == 1 {
                            let (left, right, coord, horiz) = match dim_tool.edges[0].2.dir {
                                Dir::Horiz => ("x0", "x1", layout_mouse_position.y, "true"),
                                Dir::Vert => ("y0", "y1", layout_mouse_position.x, "false"),
                            };

                            state.lsp_client.draw_dimension(
                                cell.output.cells[&selected_scope_addr.cell].scopes
                                    [&selected_scope_addr.scope]
                                    .span
                                    .clone(),
                                DimensionParams {
                                    p: format!("{}.{}", dim_tool.edges[0].0, right),
                                    n: format!("{}.{}", dim_tool.edges[0].0, left),
                                    value: format!(
                                        "{:?}",
                                        dim_tool.edges[0].2.stop - dim_tool.edges[0].2.start
                                    ),
                                    coord: if coord > dim_tool.edges[0].2.coord {
                                        format!(
                                            "{}.{} + {}",
                                            dim_tool.edges[0].0,
                                            dim_tool.edges[0].1,
                                            coord - dim_tool.edges[0].2.coord
                                        )
                                    } else {
                                        format!(
                                            "{}.{} - {}",
                                            dim_tool.edges[0].0,
                                            dim_tool.edges[0].1,
                                            dim_tool.edges[0].2.coord - coord
                                        )
                                    },
                                    pstop: format!(
                                        "{}.{}",
                                        dim_tool.edges[0].0, dim_tool.edges[0].1
                                    ),
                                    nstop: format!(
                                        "{}.{}",
                                        dim_tool.edges[0].0, dim_tool.edges[0].1
                                    ),
                                    horiz: horiz.to_string(),
                                },
                            )
                        } else if dim_tool.edges.len() == 2 {
                            let (left, right) =
                                if dim_tool.edges[0].2.coord < dim_tool.edges[1].2.coord {
                                    (0, 1)
                                } else {
                                    (1, 0)
                                };
                            let (start, stop, coord, horiz) = match dim_tool.edges[0].2.dir {
                                Dir::Vert => ("y0", "y1", layout_mouse_position.y, "true"),
                                Dir::Horiz => ("x0", "x1", layout_mouse_position.x, "false"),
                            };

                            let intended_coord = (dim_tool.edges[right].2.start
                                + dim_tool.edges[right].2.stop
                                + dim_tool.edges[left].2.start
                                + dim_tool.edges[left].2.stop)
                                / 4.;
                            let coord_offset = if coord > intended_coord {
                                format!("+ {}", coord - intended_coord)
                            } else {
                                format!("- {}", intended_coord - coord)
                            };
                            state.lsp_client.draw_dimension(
                                cell.output.cells[&selected_scope_addr.cell].scopes
                                    [&selected_scope_addr.scope]
                                    .span
                                    .clone(),
                                DimensionParams {
                                    p: format!(
                                        "{}.{}",
                                        dim_tool.edges[right].0, dim_tool.edges[right].1,
                                    ),
                                    n: format!(
                                        "{}.{}",
                                        dim_tool.edges[left].0, dim_tool.edges[left].1
                                    ),
                                    value: format!(
                                        "{:?}",
                                        dim_tool.edges[right].2.coord
                                            - dim_tool.edges[left].2.coord
                                    ),
                                    coord: format!(
                                        "({}.{} + {}.{} + {}.{} + {}.{})/4. {coord_offset}",
                                        dim_tool.edges[right].0,
                                        start,
                                        dim_tool.edges[right].0,
                                        stop,
                                        dim_tool.edges[left].0,
                                        start,
                                        dim_tool.edges[left].0,
                                        stop,
                                    ),
                                    pstop: format!(
                                        "({}.{} + {}.{}) / 2.",
                                        dim_tool.edges[right].0,
                                        start,
                                        dim_tool.edges[right].0,
                                        stop,
                                    ),
                                    nstop: format!(
                                        "({}.{} + {}.{}) / 2.",
                                        dim_tool.edges[left].0, start, dim_tool.edges[left].0, stop,
                                    ),
                                    horiz: horiz.to_string(),
                                },
                            )
                        } else {
                            None
                        };
                        if let Some(span) = span {
                            *tool = ToolState::EditDim(EditDimToolState { dim: span });
                            window.focus(&self.text_input_focus_handle);
                            window.prevent_default();
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
                    let mut selected_rect = None;
                    for r in rects.chain(self.scope_rects.iter()) {
                        let rect_bounds = Bounds::new(
                            Point::new(scale * Pixels(r.x0), scale * Pixels(-r.y1))
                                + offset
                                + self.screen_bounds.origin,
                            Size::new(scale * Pixels(r.x1 - r.x0), scale * Pixels(r.y1 - r.y0)),
                        );
                        if rect_bounds.contains(&event.position) && r.id.is_some() {
                            selected_rect = Some(r);
                            break;
                        }
                    }
                    if let Some(r) = selected_rect.cloned() {
                        select_tool.selected_obj = r.id.clone();
                        if let Some(span) = &r.id {
                            self.state.read(cx).lsp_client.select_rect(span.clone());
                        }
                    } else {
                        select_tool.selected_obj = None;
                    }
                    cx.notify();
                }
                _ => {
                    // TODO: implement EditDim tool
                }
            }
        });
    }

    #[allow(dead_code)]
    fn layout_to_px(&self, pt: Point<f32>) -> Point<Pixels> {
        Point::new(self.scale * Pixels(pt.x), self.scale * Pixels(-pt.y))
            + self.offset
            + self.screen_bounds.origin
    }

    fn px_to_layout(&self, pt: Point<Pixels>) -> Point<f32> {
        let pt = pt - self.offset - self.screen_bounds.origin;
        Point::new(pt.x.0 / self.scale, -pt.y.0 / self.scale)
    }

    pub(crate) fn draw_rect(&mut self, _: &DrawRect, _window: &mut Window, cx: &mut Context<Self>) {
        self.tool.update(cx, |tool, cx| {
            if !tool.is_draw_rect() {
                *tool = ToolState::DrawRect(DrawRectToolState { p0: None });
                cx.notify();
            }
        });
    }

    pub(crate) fn draw_dim(&mut self, _: &DrawDim, _window: &mut Window, cx: &mut Context<Self>) {
        self.tool.update(cx, |tool, cx| {
            if !tool.is_draw_dim() {
                *tool = ToolState::DrawDim(DrawDimToolState { edges: Vec::new() });
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
        self.tool.update(cx, |tool, cx| {
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

    #[allow(unused)]
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

    #[allow(unused)]
    pub(crate) fn on_drag_move(
        &mut self,
        _event: &DragMoveEvent<()>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.is_dragging = false;
    }

    #[allow(unused)]
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
            let delta = event.delta.pixel_delta(Pixels(20.));
            let ns = self.scale + delta.y.0 / 400.;
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
