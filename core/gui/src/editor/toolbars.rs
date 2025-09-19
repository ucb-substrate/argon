use std::collections::HashMap;

use compiler::compile::CellId;
use gpui::prelude::*;
use gpui::*;
use itertools::Itertools;

use crate::theme::THEME;

use super::{EditorState, LayerState, ScopeTree};

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

pub struct LayerSideBar {
    layers: Entity<HashMap<SharedString, LayerState>>,
    #[allow(dead_code)]
    subscriptions: Vec<Subscription>,
}

impl LayerSideBar {
    pub fn new(cx: &mut Context<Self>, state: &Entity<EditorState>) -> Self {
        let layers = state.read(cx).layers.clone();
        let subscriptions = vec![cx.observe(&layers, |_, _, cx| cx.notify())];
        Self {
            layers,
            subscriptions,
        }
    }
}

impl Render for LayerSideBar {
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
                    .child(
                        div().flex().child(
                            div().flex().flex_col().children(
                                self.layers
                                    .read(cx)
                                    .values()
                                    .sorted_by_key(|layer| layer.z)
                                    .map(|layer| {
                                        let layers_clone = layers_clone.clone();
                                        let name = layer.name.clone();
                                        div()
                                            .id(SharedString::from(format!(
                                                "layer_control_{}",
                                                layer.z
                                            ))) // TODO: does this need to be unique? seems to work as is
                                            .flex()
                                            .on_click(move |_event, _window, cx| {
                                                layers_clone.update(cx, |state, cx| {
                                                    state.get_mut(&name).unwrap().visible =
                                                        !state[&name].visible;
                                                    cx.notify();
                                                })
                                            })
                                            .child(format!(
                                                "{} - {}",
                                                &layer.name,
                                                if layer.visible { "V" } else { "NV" }
                                            ))
                                    }),
                            ),
                        ),
                    ),
            )
    }
}

pub struct HierarchySideBar {
    scopes: Entity<ScopeTree>,
    #[allow(dead_code)]
    subscriptions: Vec<Subscription>,
}

impl HierarchySideBar {
    pub fn new(cx: &mut Context<Self>, state: &Entity<EditorState>) -> Self {
        let scopes = state.read(cx).scopes.clone();
        let subscriptions = vec![cx.observe(&scopes, |_, _, cx| cx.notify())];
        Self {
            scopes,
            subscriptions,
        }
    }

    fn render_scopes_helper(
        &mut self,
        cx: &mut gpui::Context<Self>,
        scopes: &mut Vec<Stateful<Div>>,
        scope: CellId,
        depth: usize,
    ) {
        let scopes_clone = self.scopes.clone();
        let scope_state = &self.scopes.read(cx).state[&scope];
        scopes.push(
            div()
                .id(SharedString::from(format!("scope_control_{scope}",)))
                .flex()
                .on_click(move |_event, _window, cx| {
                    scopes_clone.update(cx, |state, cx| {
                        state.state.get_mut(&scope).unwrap().visible = !state.state[&scope].visible;
                        cx.notify();
                    })
                })
                .child(format!(
                    "{}{} - {}",
                    std::iter::repeat_n("  ", depth).collect::<String>(),
                    &scope_state.name,
                    if scope_state.visible { "V" } else { "NV" }
                )),
        );
        for scope in scope_state.children.clone() {
            self.render_scopes_helper(cx, scopes, scope, depth + 1);
        }
    }

    fn render_scopes(&mut self, cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        let mut scopes = Vec::new();
        if let Some(root) = self.scopes.read(cx).root {
            self.render_scopes_helper(cx, &mut scopes, root, 0);
        }
        div().flex().flex_col().children(scopes)
    }
}

impl Render for HierarchySideBar {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        div()
            .flex()
            .flex_col()
            .h_full()
            .w(px(200.))
            .border_r_1()
            .border_color(THEME.divider)
            .bg(THEME.sidebar)
            .min_h_0()
            .child("Scopes")
            .child(
                div()
                    .flex()
                    .size_full()
                    .items_start()
                    .id("layers_scroll_vert")
                    .overflow_scroll()
                    .child(div().flex().child(self.render_scopes(cx))),
            )
    }
}
