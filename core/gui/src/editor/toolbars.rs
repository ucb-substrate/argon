use std::collections::HashMap;

use compiler::compile::{CellId, SolvedValue};
use gpui::prelude::*;
use gpui::*;
use indexmap::IndexMap;
use itertools::Itertools;

use crate::{
    editor::{CompileOutputState, ScopeAddress},
    theme::THEME,
};

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
    layers: Entity<IndexMap<SharedString, LayerState>>,
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
                    .child(div().flex().child(div().flex().flex_col().children(
                        self.layers.read(cx).values().map(|layer| {
                            let layers_clone = layers_clone.clone();
                            let name = layer.name.clone();
                            div()
                                .id(SharedString::from(format!("layer_control_{}", layer.z))) // TODO: does this need to be unique? seems to work as is
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
                    ))),
            )
    }
}

pub struct HierarchySideBar {
    solved_cell: Entity<Option<CompileOutputState>>,
    #[allow(dead_code)]
    subscriptions: Vec<Subscription>,
}

impl HierarchySideBar {
    pub fn new(cx: &mut Context<Self>, state: &Entity<EditorState>) -> Self {
        let solved_cell = state.read(cx).solved_cell.clone();
        let subscriptions = vec![cx.observe(&solved_cell, |_, _, cx| cx.notify())];
        Self {
            solved_cell,
            subscriptions,
        }
    }

    fn render_scopes_helper(
        &mut self,
        cx: &mut gpui::Context<Self>,
        solved_cell: &CompileOutputState,
        scopes: &mut Vec<Stateful<Div>>,
        scope: ScopeAddress,
        depth: usize,
    ) {
        let solved_cell_clone = self.solved_cell.clone();
        let scope_state = &solved_cell.state[&scope];
        scopes.push(
            div()
                .id(SharedString::from(format!(
                    "scope_control_{}",
                    scopes.len()
                )))
                .flex()
                .on_click(move |_event, _window, cx| {
                    solved_cell_clone.update(cx, |state, cx| {
                        state
                            .as_mut()
                            .unwrap()
                            .state
                            .get_mut(&scope)
                            .unwrap()
                            .visible = !state.as_ref().unwrap().state[&scope].visible;
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
        let scope_info = &solved_cell.output.cells[&scope.cell].scopes[&scope.scope];
        for elt in scope_info.elts.clone() {
            if let SolvedValue::Instance(inst) = &elt {
                let scope = solved_cell.output.cells[&inst.cell].root;
                self.render_scopes_helper(
                    cx,
                    solved_cell,
                    scopes,
                    ScopeAddress {
                        scope,
                        cell: inst.cell,
                    },
                    depth + 1,
                );
            }
        }
        for child_scope in scope_info.children.clone() {
            self.render_scopes_helper(
                cx,
                solved_cell,
                scopes,
                ScopeAddress {
                    scope: child_scope,
                    cell: scope.cell,
                },
                depth + 1,
            );
        }
    }

    fn render_scopes(&mut self, cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        let mut scopes = Vec::new();
        if let Some(state) = self.solved_cell.read(cx).clone() {
            let scope = state.output.cells[&state.output.top].root;
            self.render_scopes_helper(
                cx,
                &state,
                &mut scopes,
                ScopeAddress {
                    scope,
                    cell: state.output.top,
                },
                0,
            );
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
