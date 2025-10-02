use std::{
    collections::VecDeque,
    fmt::Debug,
    ops::{Add, Sub},
};

use compiler::{
    compile::{self, ObjectId, SolvedValue, ifmatvec},
    solver::Var,
};
use enumify::enumify;
use geometry::{dir::Dir, transform::TransformationMatrix};
use gpui::{
    BorderStyle, Bounds, Context, Corners, DefiniteLength, DragMoveEvent, Edges, Element, Entity,
    FocusHandle, Focusable, InteractiveElement, IntoElement, Length, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, ParentElement, Pixels, Point, Render, Rgba,
    ScrollWheelEvent, Size, Style, Styled, Subscription, Window, div, pattern_slash, rgb, rgba,
    solid_background,
};
use indexmap::IndexSet;
use itertools::Itertools;

use crate::{
    Cancel, DrawDim, DrawRect,
    editor::{self, EditorState, LayerState, ScopeAddress},
};

#[derive(Copy, Clone, PartialEq)]
pub enum ShapeFill {
    Stippling,
    Solid,
}

#[derive(Copy, Clone, Hash, PartialEq, Eq, Debug)]
pub struct ElementId {
    scope: ScopeAddress,
    idx: usize,
}

#[enumify]
#[derive(Copy, Clone, Hash, PartialEq, Eq, Debug)]
pub enum RectId {
    Element(ElementId),
    Scope(ScopeAddress),
}

#[derive(Clone, PartialEq, Debug)]
pub struct Rect {
    pub x0: f32,
    pub x1: f32,
    pub y0: f32,
    pub y1: f32,
    pub id: Option<RectId>,
    /// Empty if not accessible.
    pub object_path: Vec<ObjectId>,
}

struct Edge<T> {
    dir: Dir,
    coord: T,
    start: T,
    stop: T,
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
            id: self.id,
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

// ~TextElement
pub struct CanvasElement {
    inner: Entity<LayoutCanvas>,
}

struct RectToolState {
    p0: Option<Point<f32>>,
}

enum DimToolState {
    SelectEdge0,
    SelectEdge1 { edge0: (Vec<ObjectId>, String) },
}

pub struct LayoutCanvas {
    focus_handle: FocusHandle,
    pub offset: Point<Pixels>,
    pub bg_style: Style,
    pub state: Entity<EditorState>,
    // drag state
    is_dragging: bool,
    drag_start: Point<Pixels>,
    offset_start: Point<Pixels>,
    // rectangle drawing state
    rect_tool: Option<RectToolState>,
    dim_tool: Option<DimToolState>,
    mouse_position: Point<Pixels>,
    // zoom state
    scale: f32,
    screen_origin: Point<Pixels>,
    #[allow(unused)]
    subscriptions: Vec<Subscription>,
    rects: Vec<(Rect, LayerState)>,
    scope_rects: Vec<Rect>,
}

impl IntoElement for CanvasElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
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
        Point::new(scale * Pixels(r.x0), scale * Pixels(r.y0)) + offset + bounds.origin,
        Size::new(scale * Pixels(r.x1 - r.x0), scale * Pixels(r.y1 - r.y0)),
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
        self.inner
            .update(cx, |inner, _cx| inner.screen_origin = bounds.origin);
        let inner = self.inner.read(cx);
        let solved_cell = &inner.state.read(cx).solved_cell.read(cx);
        let selected_rect = solved_cell.as_ref().and_then(|cell| cell.selected_rect);
        let mut selected_rect_transformations = Vec::new();
        let state = inner.state.read(cx);
        let layers = state.layers.read(cx);

        // TODO: Clean up code.
        let mut rects = Vec::new();
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
                if let Some(selected_rect) = selected_rect
                    && Some(curr_address)
                        == match selected_rect {
                            RectId::Scope(id) => {
                                solved_cell.state[&solved_cell.scope_paths[&id]].parent
                            }
                            RectId::Element(id) => Some(id.scope),
                        }
                {
                    selected_rect_transformations.push((mat, ofs));
                }
                let cell_info = &solved_cell.output.cells[&cell];
                let scope_info = &cell_info.scopes[&scope];
                let scope_state = &solved_cell.state[&solved_cell.scope_paths[&curr_address]];
                if show && (depth >= state.hierarchy_depth || !scope_state.visible) {
                    if let Some(bbox) = &scope_state.bbox {
                        let p0p = ifmatvec(mat, (bbox.x0, bbox.y0));
                        let p1p = ifmatvec(mat, (bbox.x1, bbox.y1));
                        scope_rects.push(Rect {
                            x0: (p0p.0.min(p1p.0) + ofs.0) as f32,
                            y0: (p0p.1.min(p1p.1) + ofs.1) as f32,
                            x1: (p0p.0.max(p1p.0) + ofs.0) as f32,
                            y1: (p0p.1.max(p1p.1) + ofs.1) as f32,
                            id: Some(RectId::Scope(curr_address)),
                            object_path: Vec::new(),
                        });
                    }
                    show = false;
                }
                for (i, (obj, _)) in scope_info.emit.iter().enumerate() {
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
                                    rects.push((
                                        Rect {
                                            x0: (p0p.0.min(p1p.0) + ofs.0) as f32,
                                            y0: (p0p.1.min(p1p.1) + ofs.1) as f32,
                                            x1: (p0p.0.max(p1p.0) + ofs.0) as f32,
                                            y1: (p0p.1.max(p1p.1) + ofs.1) as f32,
                                            id: Some(RectId::Element(ElementId {
                                                scope: curr_address,
                                                idx: i,
                                            })),
                                            object_path,
                                        },
                                        layer.clone(),
                                    ));
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
                                    scope_rects.push(Rect {
                                        x0: (p0p.0.min(p1p.0) + new_ofs.0) as f32,
                                        y0: (p0p.1.min(p1p.1) + new_ofs.1) as f32,
                                        x1: (p0p.0.max(p1p.0) + new_ofs.0) as f32,
                                        y1: (p0p.1.max(p1p.1) + new_ofs.1) as f32,
                                        id: Some(RectId::Element(ElementId {
                                            scope: curr_address,
                                            idx: i,
                                        })),
                                        object_path: object_path.clone(),
                                    });
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
            if let Some(selected_rect) = selected_rect {
                let r = match selected_rect {
                    RectId::Scope(id) => solved_cell.state[&solved_cell.scope_paths[&id]]
                        .bbox
                        .clone()
                        .map(|r| r.into()),
                    RectId::Element(id) => match &solved_cell.output.cells[&id.scope.cell].objects
                        [&solved_cell.output.cells[&id.scope.cell].scopes[&id.scope.scope].emit
                            [id.idx]
                            .0]
                    {
                        SolvedValue::Rect(r) => Some(r.clone().into()),
                        SolvedValue::Instance(inst) => {
                            let inst_address = ScopeAddress {
                                scope: solved_cell.output.cells[&inst.cell].root,
                                cell: inst.cell,
                            };
                            let scope_state =
                                &solved_cell.state[&solved_cell.scope_paths[&inst_address]];
                            scope_state.bbox.as_ref().map(|rect| {
                                let mut inst_mat = TransformationMatrix::identity();
                                if inst.reflect {
                                    inst_mat = inst_mat.reflect_vert()
                                }
                                inst_mat = inst_mat.rotate(inst.angle);
                                let p0p = ifmatvec(inst_mat, (rect.x0, rect.y0));
                                let p1p = ifmatvec(inst_mat, (rect.x1, rect.y1));
                                Rect {
                                    x0: (p0p.0.min(p1p.0) + inst.x) as f32,
                                    y0: (p0p.1.min(p1p.1) + inst.y) as f32,
                                    x1: (p0p.0.max(p1p.0) + inst.x) as f32,
                                    y1: (p0p.1.max(p1p.1) + inst.y) as f32,
                                    id: None,
                                    object_path: Vec::new(),
                                }
                            })
                        }
                    },
                };
                if let Some(r) = r {
                    for (mat, ofs) in selected_rect_transformations {
                        select_rects.push(r.transform(mat, ofs));
                    }
                }
            }
        }

        if let Some(RectToolState { p0: Some(p0) }) = &inner.rect_tool {
            let p1 = inner.px_to_layout(inner.mouse_position);
            rects.push((
                Rect {
                    object_path: Vec::new(),
                    x0: p0.x.min(p1.x),
                    y0: p0.y.min(p1.y),
                    x1: p0.x.max(p1.x),
                    y1: p0.y.max(p1.y),
                    id: None,
                },
                layers.layers[layers.selected_layer.as_ref().unwrap()].clone(),
            ));
        }
        let rects = rects
            .into_iter()
            .sorted_by_key(|(_, layer)| layer.z)
            .collect_vec();
        let scale = inner.scale;
        let offset = inner.offset;
        inner
            .bg_style
            .clone()
            .paint(bounds, window, cx, |window, _cx| {
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
    pub fn new(cx: &mut Context<Self>, state: &Entity<EditorState>) -> Self {
        LayoutCanvas {
            focus_handle: cx.focus_handle(),
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
            rect_tool: None,
            dim_tool: None,
            scale: 1.0,
            screen_origin: Point::default(),
            subscriptions: vec![cx.observe(state, |_, _, cx| cx.notify())],
            state: state.clone(),
            rects: Vec::new(),
            scope_rects: Vec::new(),
        }
    }

    pub(crate) fn on_left_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(ref mut rect_tool) = self.rect_tool {
            if let Some(p0) = rect_tool.p0 {
                rect_tool.p0 = None;
                let p1 = self.px_to_layout(event.position);
                let p0p = Point::new(f32::min(p0.x, p1.x), f32::min(p0.y, p1.y));
                let p1p = Point::new(f32::max(p0.x, p1.x), f32::max(p0.y, p1.y));
                self.state.update(cx, |state, cx| {
                    state.solved_cell.update(cx, {
                        |cell, cx| {
                            if let Some(cell) = cell.as_mut() {
                                // TODO update in memory representation of code
                                // TODO add solver to gui
                                let scope_address = &cell.state[&cell.selected_scope].address;
                                let reachable_objs = cell
                                    .output
                                    .reachable_objs(scope_address.cell, scope_address.scope);
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
                                    cell.file.clone(),
                                    scope.span,
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
                if self.state.read(cx).layers.read(cx).selected_layer.is_none() {
                    self.rect_tool = None;
                } else {
                    let p0 = self.px_to_layout(event.position);
                    self.rect_tool.as_mut().unwrap().p0 = Some(p0);
                }
            }
        } else if let Some(ref mut dim_tool) = self.dim_tool {
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
                        Point::new(scale * Pixels(r.x0), scale * Pixels(r.y0))
                            + offset
                            + self.screen_origin,
                        Size::new(scale * Pixels(r.x1 - r.x0), scale * Pixels(r.y1 - r.y0)),
                    ),
                )
            }) {
                for (name, edge) in [
                    (
                        "y0",
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
                            coord: r.top(),
                            start: r.left(),
                            stop: r.right(),
                        },
                    ),
                    (
                        "x0",
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
                            coord: r.right(),
                            start: r.top(),
                            stop: r.bottom(),
                        },
                    ),
                ] {
                    let bounds = edge.select_bounds(Pixels(4.));
                    if bounds.contains(&event.position) && rect.id.is_some() {
                        selected = Some((rect, name));
                        break;
                    }
                }
            }

            if let Some((r, edge)) = selected {
                match dim_tool {
                    DimToolState::SelectEdge0 => {
                        *dim_tool = DimToolState::SelectEdge1 {
                            edge0: (r.object_path.clone(), edge.to_string()),
                        };
                    }
                    DimToolState::SelectEdge1 {
                        edge0: (path0, edge0),
                    } => {
                        self.state.update(cx, |state, cx| {
                            state.solved_cell.update(cx, |cell, cx| {
                                if let Some(cell) = cell.as_mut() {
                                    let find_obj_path = |path: &[ObjectId]| {
                                        let mut current_scope = cell.selected_scope;
                                        let mut string_path = Vec::new();
                                        let mut reachable = true;
                                        if path.is_empty() {
                                            panic!("need non-empty object path");
                                        }
                                        for obj in &path[0..path.len() - 1] {
                                            let mut reachable_objs = cell.output.reachable_objs(
                                                current_scope.cell,
                                                current_scope.scope,
                                            );
                                            if let Some(name) = reachable_objs.swap_remove(obj)
                                                && let Some(inst) = cell.output.cells
                                                    [&current_scope.cell]
                                                    .objects[obj]
                                                    .get_instance()
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
                                        let mut reachable_objs = cell.output.reachable_objs(
                                            current_scope.cell,
                                            current_scope.scope,
                                        );
                                        if let Some(name) = reachable_objs.swap_remove(obj)
                                            && cell.output.cells[&current_scope.cell].objects[obj]
                                                .is_rect()
                                        {
                                            string_path.push(name);
                                        } else {
                                            reachable = false;
                                        }
                                        (reachable, string_path)
                                    };
                                    cx.notify();
                                    if let (true, path0) = find_obj_path(path0)
                                        && let (true, path1) = find_obj_path(&r.object_path)
                                    {
                                        let path0 = path0.join(".");
                                        let path1 = path1.join(".");
                                        state.lsp_client.add_eq_constraint(
                                            cell.file.clone(),
                                            cell.output.cells[&cell.selected_scope.cell].scopes
                                                [&cell.selected_scope.scope]
                                                .span,
                                            format!("{}.{}", path0, edge0),
                                            format!("{}.{}", path1, edge),
                                        );
                                    }
                                }
                            })
                        });
                        self.dim_tool = Some(DimToolState::SelectEdge0);
                    }
                }
            }
        } else {
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
                    Point::new(scale * Pixels(r.x0), scale * Pixels(r.y0))
                        + offset
                        + self.screen_origin,
                    Size::new(scale * Pixels(r.x1 - r.x0), scale * Pixels(r.y1 - r.y0)),
                );
                if rect_bounds.contains(&event.position) && r.id.is_some() {
                    selected_rect = Some(r);
                    break;
                }
            }
            if let Some(r) = selected_rect.cloned() {
                self.state.update(cx, |state, cx| {
                    state.solved_cell.update(cx, |cell, cx| {
                        if let Some(cell) = cell.as_mut() {
                            cell.selected_rect = r.id;
                            let args = r.id.and_then(|id| match id {
                                RectId::Element(id) => {
                                    let ccell = &cell.output.cells[&id.scope.cell];
                                    let (obj, emit) = &ccell.scopes[&id.scope.scope].emit[id.idx];

                                    match &ccell.objects[obj] {
                                        SolvedValue::Rect(_) => {
                                            Some((cell.file.clone(), emit.span))
                                        }
                                        _ => None,
                                    }
                                }
                                RectId::Scope(id) => Some((
                                    cell.file.clone(),
                                    cell.output.cells[&id.cell].scopes[&id.scope].span,
                                )),
                            });
                            state.lsp_client.select_rect(args);
                            cx.notify();
                        }
                    });
                });
            } else {
                self.state.update(cx, |state, cx| {
                    state.solved_cell.update(cx, |cell, cx| {
                        if let Some(cell) = cell.as_mut() {
                            cell.selected_rect = None;
                            cx.notify();
                        }
                    });
                });
            }
        }
    }

    #[allow(dead_code)]
    fn layout_to_px(&self, pt: Point<f32>) -> Point<Pixels> {
        Point::new(self.scale * Pixels(pt.x), self.scale * Pixels(pt.y))
            + self.offset
            + self.screen_origin
    }

    fn px_to_layout(&self, pt: Point<Pixels>) -> Point<f32> {
        let pt = pt - self.offset - self.screen_origin;
        Point::new(pt.x.0 / self.scale, pt.y.0 / self.scale)
    }

    pub(crate) fn draw_rect(
        &mut self,
        _: &DrawRect,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        self.dim_tool = None;
        if self.rect_tool.is_none() {
            self.rect_tool = Some(RectToolState { p0: None });
        }
    }

    pub(crate) fn draw_dim(&mut self, _: &DrawDim, _window: &mut Window, _cx: &mut Context<Self>) {
        self.rect_tool = None;
        if self.dim_tool.is_none() {
            self.dim_tool = Some(DimToolState::SelectEdge0);
        }
    }

    pub(crate) fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(rect_tool) = self.rect_tool.as_mut() {
            if rect_tool.p0.is_none() {
                self.rect_tool = None;
            } else {
                rect_tool.p0 = None;
            }
        }
        if let Some(dim_tool) = self.dim_tool.as_mut() {
            if matches!(dim_tool, DimToolState::SelectEdge0) {
                self.dim_tool = None;
            } else {
                *dim_tool = DimToolState::SelectEdge0;
            }
        }
        cx.notify();
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
        let b0 = self.screen_origin + self.offset;
        let b1 = Point::new(a * (b0.x - event.position.x), a * (b0.y - event.position.y))
            + event.position;
        self.offset = b1 - self.screen_origin;
        self.scale = new_scale;

        cx.notify();
    }
}
