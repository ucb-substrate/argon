use gpui::{
    div, pattern_slash, rgb, solid_background, BorderStyle, Bounds, Context, Corners,
    DefiniteLength, Edges, Element, Entity, InteractiveElement, IntoElement, Length, MouseButton,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, ParentElement, Pixels, Point, Render,
    Rgba, ScrollWheelEvent, Size, Style, Styled, Window,
};

#[derive(Copy, Clone, PartialEq)]
pub enum ShapeFill {
    Stippling,
    Solid,
}

#[derive(Copy, Clone, PartialEq)]
pub struct Rect {
    pub x0: f32,
    pub x1: f32,
    pub y0: f32,
    pub y1: f32,
    pub color: Rgba,
    pub fill: ShapeFill,
    pub border_color: Rgba,
}

// ~TextElement
pub struct CanvasElement {
    inner: Entity<LayoutCanvas>,
}

// ~TextInput
pub struct LayoutCanvas {
    pub offset: Point<Pixels>,
    pub rects: Vec<Rect>,
    pub bg_style: Style,
    // drag state
    is_dragging: bool,
    drag_start: Point<Pixels>,
    offset_start: Point<Pixels>,
    // zoom state
    scale: f32,
    screen_origin: Point<Pixels>,
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
        let rects = inner.rects.clone();
        let scale = inner.scale;
        let offset = inner.offset;
        inner
            .bg_style
            .clone()
            .paint(bounds, window, cx, |window, _cx| {
                window.paint_layer(bounds, |window| {
                    for r in rects {
                        let bounds = Bounds::new(
                            Point::new(scale * Pixels(r.x0), scale * Pixels(r.y0))
                                + offset
                                + bounds.origin,
                            Size::new(scale * Pixels(r.x1 - r.x0), scale * Pixels(r.y1 - r.y0)),
                        );
                        let background = match r.fill {
                            ShapeFill::Solid => solid_background(r.color),
                            ShapeFill::Stippling => pattern_slash(r.color.into(), 1., 9.),
                        };
                        window.paint_quad(PaintQuad {
                            bounds,
                            corner_radii: Corners::all(Pixels(0.)),
                            background,
                            border_widths: Edges::all(Pixels(2.)),
                            border_color: r.border_color.into(),
                            border_style: BorderStyle::Solid,
                        });
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
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .child(CanvasElement {
                inner: cx.entity().clone(),
            })
    }
}

pub(crate) fn test_canvas() -> LayoutCanvas {
    LayoutCanvas {
        rects: vec![
            Rect {
                x0: 0.0,
                y0: 0.0,
                x1: 100.,
                y1: 40.,
                color: rgb(0xff),
                fill: ShapeFill::Stippling,
                border_color: rgb(0xff),
            },
            Rect {
                x0: 70.,
                y0: 10.,
                x1: 90.,
                y1: 30.,
                color: rgb(0x5e00e6),
                fill: ShapeFill::Solid,
                border_color: rgb(0x5e00e6),
            },
            Rect {
                x0: 60.,
                y0: 0.,
                x1: 100.,
                y1: 100.,
                color: rgb(0xff00ff),
                fill: ShapeFill::Stippling,
                border_color: rgb(0xff00ff),
            },
        ],
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
    }
}

impl LayoutCanvas {
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
