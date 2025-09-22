use compiler::compile::CellId;
use gpui::{
    BorderStyle, Bounds, Context, Corners, DefiniteLength, DragMoveEvent, Edges, Element, Entity,
    InteractiveElement, IntoElement, Length, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, PaintQuad, ParentElement, Pixels, Point, Render, ScrollWheelEvent, SharedString,
    Size, Style, Styled, Subscription, Window, div, pattern_slash, rgb, rgba, solid_background,
};
use itertools::Itertools;

use crate::editor::EditorState;

#[derive(Copy, Clone, PartialEq)]
pub enum ShapeFill {
    Stippling,
    Solid,
}

#[derive(Clone, PartialEq)]
pub struct Rect {
    pub x0: f32,
    pub x1: f32,
    pub y0: f32,
    pub y1: f32,
    pub layer: SharedString,
    pub scope: CellId,
    pub span: Option<cfgrammar::Span>,
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

// ~TextInput
pub struct LayoutCanvas {
    pub offset: Point<Pixels>,
    pub bg_style: Style,
    pub state: Entity<EditorState>,
    // drag state
    is_dragging: bool,
    drag_start: Point<Pixels>,
    offset_start: Point<Pixels>,
    // zoom state
    scale: f32,
    screen_origin: Point<Pixels>,
    #[allow(unused)]
    subscriptions: Vec<Subscription>,
}

impl IntoElement for CanvasElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
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
        let rects = inner
            .state
            .read(cx)
            .rects
            .clone()
            .into_iter()
            .map(|rect| {
                let layer = inner.state.read(cx).layers.read(cx)[&rect.layer].clone();
                let scope = inner.state.read(cx).scopes.read(cx).state[&rect.scope].clone();
                (rect, layer, scope)
            })
            .filter(|(_, layer, scope)| layer.visible && scope.visible)
            .sorted_by_key(|(_, layer, _)| layer.z)
            .collect_vec();
        let scale = inner.scale;
        let offset = inner.offset;
        inner
            .bg_style
            .clone()
            .paint(bounds, window, cx, |window, cx| {
                window.paint_layer(bounds, |window| {
                    for (r, l, _) in rects {
                        let rect_bounds = Bounds::new(
                            Point::new(scale * Pixels(r.x0), scale * Pixels(r.y0))
                                + offset
                                + bounds.origin,
                            Size::new(scale * Pixels(r.x1 - r.x0), scale * Pixels(r.y1 - r.y0)),
                        );
                        let background = match l.fill {
                            ShapeFill::Solid => solid_background(l.color),
                            ShapeFill::Stippling => pattern_slash(l.color.into(), 1., 9.),
                        };
                        if let Some(clipped) = intersect(&rect_bounds, &bounds) {
                            let left_border =
                                f32::clamp((rect_bounds.left().0 + 2.) - bounds.left().0, 0., 2.);
                            let right_border =
                                f32::clamp(bounds.right().0 - (rect_bounds.right().0 - 2.), 0., 2.);
                            let top_border =
                                f32::clamp((rect_bounds.top().0 + 2.) - bounds.top().0, 0., 2.);
                            let bot_border = f32::clamp(
                                bounds.bottom().0 - (rect_bounds.bottom().0 - 2.),
                                0.,
                                2.,
                            );
                            let mut border_widths = Edges::all(Pixels(2.));
                            border_widths.left = Pixels(left_border);
                            border_widths.right = Pixels(right_border);
                            border_widths.top = Pixels(top_border);
                            border_widths.bottom = Pixels(bot_border);
                            window.paint_quad(PaintQuad {
                                bounds: clipped,
                                corner_radii: Corners::all(Pixels(0.)),
                                background,
                                border_widths,
                                border_color: l.border_color.into(),
                                border_style: BorderStyle::Solid,
                            });
                        }
                    }
                    if let Some(selected_rect) = self.inner.read(cx).state.read(cx).selected_rect {
                        let r = &self.inner.read(cx).state.read(cx).rects[selected_rect];
                        let rect_bounds = Bounds::new(
                            Point::new(scale * Pixels(r.x0), scale * Pixels(r.y0))
                                + offset
                                + bounds.origin,
                            Size::new(scale * Pixels(r.x1 - r.x0), scale * Pixels(r.y1 - r.y0)),
                        );
                        if let Some(clipped) = intersect(&rect_bounds, &bounds) {
                            let left_border =
                                f32::clamp((rect_bounds.left().0 + 2.) - bounds.left().0, 0., 2.);
                            let right_border =
                                f32::clamp(bounds.right().0 - (rect_bounds.right().0 - 2.), 0., 2.);
                            let top_border =
                                f32::clamp((rect_bounds.top().0 + 2.) - bounds.top().0, 0., 2.);
                            let bot_border = f32::clamp(
                                bounds.bottom().0 - (rect_bounds.bottom().0 - 2.),
                                0.,
                                2.,
                            );
                            let mut border_widths = Edges::all(Pixels(2.));
                            border_widths.left = Pixels(left_border);
                            border_widths.right = Pixels(right_border);
                            border_widths.top = Pixels(top_border);
                            border_widths.bottom = Pixels(bot_border);
                            window.paint_quad(PaintQuad {
                                bounds: clipped,
                                corner_radii: Corners::all(Pixels(0.)),
                                background: solid_background(rgba(0)),
                                border_widths,
                                border_color: rgb(0xffff00).into(),
                                border_style: BorderStyle::Solid,
                            });
                        }
                    }
                })
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
            .size_full()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_left_mouse_down))
            // TODO: Uncomment once GPUI mouse movement is fixed.
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_mouse_down))
            // .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_drag_move(cx.listener(Self::on_drag_move))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .child(CanvasElement {
                inner: cx.entity().clone(),
            })
    }
}

impl LayoutCanvas {
    pub fn new(_cx: &mut Context<Self>, state: &Entity<EditorState>) -> Self {
        LayoutCanvas {
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
            scale: 1.0,
            screen_origin: Point::default(),
            subscriptions: Vec::new(),
            state: state.clone(),
        }
    }

    pub(crate) fn on_left_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let rects = self
            .state
            .read(cx)
            .rects
            .iter()
            .cloned()
            .map(|rect| {
                let layer = self.state.read(cx).layers.read(cx)[&rect.layer].clone();
                let scope = self.state.read(cx).scopes.read(cx).state[&rect.scope].clone();
                (rect, layer, scope)
            })
            .enumerate()
            .filter(|(_, (_, layer, scope))| layer.visible && scope.visible)
            .sorted_by_key(|(_, (_, layer, _))| usize::MAX - layer.z)
            .map(|(i, r)| (i, r.clone()))
            .collect_vec();
        let scale = self.scale;
        let offset = self.offset;
        for (i, (r, _, _)) in rects {
            let rect_bounds = Bounds::new(
                Point::new(scale * Pixels(r.x0), scale * Pixels(r.y0))
                    + offset
                    + self.screen_origin,
                Size::new(scale * Pixels(r.x1 - r.x0), scale * Pixels(r.y1 - r.y0)),
            );
            if rect_bounds.contains(&event.position) {
                self.state.update(cx, |state, cx| {
                    state.selected_rect = Some(i);
                    // TODO: Send message
                    state
                        .lsp_client
                        .select_rect(match (state.file.as_ref(), r.span) {
                            (Some(f), Some(s)) => Some((f.clone(), s)),
                            _ => None,
                        });
                    cx.notify();
                });
                return;
            }
        }
        self.state.update(cx, |state, cx| {
            state.selected_rect = None;
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
