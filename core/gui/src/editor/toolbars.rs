use gpui::prelude::*;
use gpui::*;

use itertools::Itertools;

use crate::theme::THEME;

use super::{EditorState, LayerState};

pub struct TitleBar;

impl Render for TitleBar {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        div()
            .border_b_1()
            .border_color(THEME.divider)
            .window_control_area(WindowControlArea::Drag)
            .pl(px(71.))
            .bg(THEME.titlebar)
            .child("Project")
    }
}

pub struct ToolBar;

impl Render for ToolBar {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        div()
            .border_b_1()
            .border_color(THEME.divider)
            .h(px(34.))
            .bg(THEME.sidebar)
            .child("Tools")
    }
}

pub struct SideBar {
    layers: Entity<Vec<LayerState>>,
    subscriptions: Vec<Subscription>,
}

impl SideBar {
    pub fn new(cx: &mut Context<Self>, state: &Entity<EditorState>) -> Self {
        let layers = state.read(cx).layers.clone();
        let subscriptions = vec![cx.observe(&layers, |_, _, cx| cx.notify())];
        Self {
            layers: state.read(cx).layers.clone(),
            subscriptions,
        }
    }
}

impl Render for SideBar {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let layers_clone = self.layers.clone();
        div()
            .flex()
            .flex_col()
            .h_full()
            .w(px(200.))
            .border_r_1()
            .border_color(THEME.divider)
            .bg(THEME.sidebar)
            .min_h_0()
            .child("Layers")
            .child(
                div()
                    .flex()
                    .size_full()
                    .items_start()
                    .id("layers_scroll_vert")
                    .overflow_scroll()
                    .child(div().flex().child(div().flex().flex_col().children(
                        self.layers.read(cx).iter().enumerate().map(|(i, layer)| {
                            let layers_clone = layers_clone.clone();
                            div()
                                .id(SharedString::from(format!("layer_control_{i}"))) // TODO: does this need to be unique? seems to work as is
                                .flex()
                                .on_click(move |_event, _window, cx| {
                                    println!("On click {i}");
                                    layers_clone.update(cx, |state, cx| {
                                        state[i].visible = !state[i].visible;
                                        println!("Update layer state {i}");
                                        cx.notify();
                                    })
                                })
                                .child(format!(
                                    "{} - {}",
                                    &layer.name,
                                    if layer.visible { "V" } else { "NV" }
                                ))
                        }),
                    ))),
            )
    }
}
