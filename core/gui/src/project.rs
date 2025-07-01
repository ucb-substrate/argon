use gpui::*;

use crate::{
    canvas::{test_canvas, LayoutCanvas},
    theme::THEME,
    toolbars::{SideBar, TitleBar, ToolBar},
};

pub struct LayerState {
    pub name: String,
    pub visible: bool,
}

pub struct ProjectState {
    pub layers: Vec<Entity<LayerState>>,
}

pub struct Project {
    pub state: Entity<ProjectState>,
    pub sidebar: Entity<SideBar>,
    pub canvas: Entity<LayoutCanvas>,
}

impl Project {
    pub fn new(cx: &mut Context<Self>) -> Self {
        let state = cx.new(|cx| ProjectState {
            layers: (0..100)
                .map(|i| {
                    cx.new(|_cx| LayerState {
                        name: format!("met abcdasldfkjasdlfkjasdlfkjasdf {i}"),
                        visible: true,
                    })
                })
                .collect(),
        });
        let sidebar = cx.new(|cx| SideBar::new(cx, state.clone()));
        let canvas = cx.new(|_cx| test_canvas());

        Self {
            state,
            sidebar,
            canvas,
        }
    }
}

impl Project {
    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.canvas
            .update(cx, |canvas, cx| canvas.on_mouse_move(event, window, cx));
        cx.notify();
    }

    fn on_mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.canvas
            .update(cx, |canvas, cx| canvas.on_mouse_up(event, window, cx));
    }
}

impl Render for Project {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .font_family("Zed Plex Sans")
            .size_full()
            .flex()
            .flex_col()
            .justify_start()
            .border_1()
            .border_color(THEME.divider)
            .rounded(px(10.))
            .text_sm()
            .text_color(rgb(0xffffff))
            .whitespace_nowrap()
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(cx.new(|_cx| TitleBar))
            .child(cx.new(|_cx| ToolBar))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(self.sidebar.clone())
                    .child(self.canvas.clone()),
            )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {}
