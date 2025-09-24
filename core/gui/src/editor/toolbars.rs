use compiler::compile::SolvedValue;
use gpui::prelude::*;
use gpui::*;
use indexmap::IndexMap;

use crate::{
    editor::{CompileOutputState, Layers, ScopeAddress},
    theme::THEME,
};

use super::EditorState;

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
    layers: Entity<Layers>,
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
        let layers = self.layers.read(cx);
        div()
            .flex()
            .flex_col()
            .h_full()
            .w(px(200.))
            .border_l_1()
            .border_color(THEME.divider)
            .bg(THEME.sidebar)
            .min_h_0()
            .child("Layers")
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .items_start()
                    .id("layers_scroll_vert")
                    .overflow_y_scroll()
                    .children(layers.layers.values().map(|layer| {
                        div()
                            .flex()
                            .w_full()
                            .bg(if Some(&layer.name) == layers.selected_layer.as_ref() {
                                rgba(0x00000099)
                            } else {
                                rgba(0)
                            })
                            .child(
                                div()
                                    .id(SharedString::from(format!("layer_select_{}", layer.z)))
                                    .flex_1()
                                    .overflow_hidden()
                                    .child(layer.name.clone())
                                    .on_click({
                                        let layers = self.layers.clone();
                                        let name = layer.name.clone();
                                        move |_event, _window, cx| {
                                            layers.update(cx, |state, cx| {
                                                state.selected_layer = Some(name.clone());
                                                cx.notify();
                                            })
                                        }
                                    }),
                            )
                            .child(
                                div()
                                    .child(if layer.visible { "--V" } else { "NV" })
                                    .id(SharedString::from(format!("layer_control_{}", layer.z)))
                                    .on_click({
                                        let layers = self.layers.clone();
                                        let name = layer.name.clone();
                                        move |_event, _window, cx| {
                                            layers.update(cx, |state, cx| {
                                                state.layers.get_mut(&name).unwrap().visible =
                                                    !state.layers[&name].visible;
                                                cx.notify();
                                            })
                                        }
                                    }),
                            )
                    })),
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
        solved_cell: &CompileOutputState,
        scopes: &mut Vec<Div>,
        scope: ScopeAddress,
        count: usize,
        depth: usize,
    ) {
        let solved_cell_clone_1 = self.solved_cell.clone();
        let solved_cell_clone_2 = self.solved_cell.clone();
        let scope_state = &solved_cell.state[&scope];
        scopes.push(
            div()
                .flex()
                .w_full()
                .bg(if scope == solved_cell.selected_scope {
                    rgba(0x00000099)
                } else {
                    rgba(0)
                })
                .child(
                    div()
                        .id(SharedString::from(format!("scope_select_{scope:?}")))
                        .flex_1()
                        .overflow_hidden()
                        .child(format!(
                            "{}{}{}",
                            std::iter::repeat_n("  ", depth).collect::<String>(),
                            &scope_state.name,
                            if count > 1 {
                                format!(" ({count})")
                            } else {
                                "".to_string()
                            }
                        ))
                        .on_click(move |_event, _window, cx| {
                            solved_cell_clone_1.update(cx, |state, cx| {
                                if let Some(state) = state.as_mut() {
                                    state.selected_scope = scope;
                                    cx.notify();
                                }
                            })
                        }),
                )
                .child(
                    div()
                        .child(if scope_state.visible { "--V" } else { "NV" })
                        .id(SharedString::from(format!("scope_control_{scope:?}",)))
                        .on_click(move |_event, _window, cx| {
                            solved_cell_clone_2.update(cx, |state, cx| {
                                if let Some(state) = state.as_mut() {
                                    state.state.get_mut(&scope).unwrap().visible =
                                        !state.state[&scope].visible;
                                    cx.notify();
                                }
                            })
                        }),
                ),
        );
        let scope_info = &solved_cell.output.cells[&scope.cell].scopes[&scope.scope];
        let mut cells = IndexMap::new();
        for elt in scope_info.elts.clone() {
            if let SolvedValue::Instance(inst) = &elt {
                *cells.entry(inst.cell).or_insert(0) += 1;
            }
        }

        for (cell, count) in cells {
            let scope = solved_cell.output.cells[&cell].root;
            self.render_scopes_helper(
                solved_cell,
                scopes,
                ScopeAddress { scope, cell },
                count,
                depth + 1,
            );
        }
        for child_scope in scope_info.children.clone() {
            self.render_scopes_helper(
                solved_cell,
                scopes,
                ScopeAddress {
                    scope: child_scope,
                    cell: scope.cell,
                },
                1,
                depth + 1,
            );
        }
    }

    fn render_scopes(&mut self, cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        let mut scopes = Vec::new();
        if let Some(state) = self.solved_cell.read(cx) {
            let scope = state.output.cells[&state.output.top].root;
            self.render_scopes_helper(
                state,
                &mut scopes,
                ScopeAddress {
                    scope,
                    cell: state.output.top,
                },
                1,
                0,
            );
        }
        div()
            .flex()
            .flex_col()
            .w_full()
            .id("layers_scroll_vert")
            .overflow_y_scroll()
            .children(scopes)
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
            .child(self.render_scopes(cx))
    }
}
