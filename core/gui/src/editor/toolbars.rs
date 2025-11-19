use std::sync::Arc;

use compiler::compile::SolvedValue;
use gpui::prelude::*;
use gpui::*;
use indexmap::{IndexMap, IndexSet};
use itertools::Itertools;
use lsp_server::rpc::GuiToLspAction;

use crate::{
    actions::{DrawDim, DrawRect, SelectMode},
    editor::{
        CompileOutputState, Layers, ScopeAddress, ScopePath,
        canvas::{EditDimToolState, LayoutCanvas, ToolState},
        input::TextInput,
    },
};

use super::EditorState;

pub struct TitleBar {
    state: Entity<EditorState>,
}

impl TitleBar {
    pub fn new(state: &Entity<EditorState>) -> Self {
        Self {
            state: state.clone(),
        }
    }
}

impl Render for TitleBar {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let theme = self.state.read(cx).theme();
        div()
            .border_color(theme.divider)
            .window_control_area(WindowControlArea::Drag)
            .p_1()
            .bg(theme.titlebar)
            .text_center()
            .child("Argon")
    }
}

pub struct ToolBar {
    state: Entity<EditorState>,
}

impl ToolBar {
    pub fn new(state: &Entity<EditorState>) -> Self {
        Self {
            state: state.clone(),
        }
    }
}

impl Render for ToolBar {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let theme = self.state.read(cx).theme();
        div()
            .border_color(theme.divider)
            .p_2()
            .bg(theme.bg)
            .flex()
            .flex_row()
            .children({
                type HighlightFn = Box<dyn Fn(&ToolState) -> bool>;
                type OnClickFn = Arc<dyn Fn(Entity<EditorState>, &mut App)>;
                let tools: [Option<(&'static str, &'static str, HighlightFn, OnClickFn)>; _] = [
                    Some((
                        "btn_undo",
                        "icons/arrow-rotate-left-solid-full.svg",
                        Box::new(|_| false),
                        Arc::new(|state, cx| {
                            state
                                .read(cx)
                                .lsp_client
                                .dispatch_action(GuiToLspAction::Undo);
                        }),
                    )),
                    Some((
                        "btn_redo",
                        "icons/arrow-rotate-right-solid-full.svg",
                        Box::new(|_| false),
                        Arc::new(|state, cx| {
                            state
                                .read(cx)
                                .lsp_client
                                .dispatch_action(GuiToLspAction::Redo);
                        }),
                    )),
                    None,
                    Some((
                        "btn_select",
                        "icons/arrow-pointer-solid-full.svg",
                        Box::new(|tool| {
                            matches!(
                                tool,
                                ToolState::Select(_)
                                    | ToolState::EditDim(EditDimToolState {
                                        dim_mode: false,
                                        ..
                                    })
                            )
                        }),
                        Arc::new(|_state, cx| {
                            cx.defer(move |cx| {
                                cx.dispatch_action(&SelectMode);
                            });
                        }),
                    )),
                    Some((
                        "btn_rect",
                        "icons/rect.svg",
                        Box::new(|tool| matches!(tool, ToolState::DrawRect(_))),
                        Arc::new(|_state, cx| {
                            cx.defer(move |cx| {
                                cx.dispatch_action(&DrawRect);
                            })
                        }),
                    )),
                    Some((
                        "btn_dim",
                        "icons/arrows-left-right-to-line-solid-full.svg",
                        Box::new(|tool| {
                            matches!(
                                tool,
                                ToolState::DrawDim(_)
                                    | ToolState::EditDim(EditDimToolState { dim_mode: true, .. })
                            )
                        }),
                        Arc::new(|_state, cx| {
                            cx.defer(move |cx| {
                                cx.dispatch_action(&DrawDim);
                            });
                        }),
                    )),
                ];
                let wh = 20.;
                tools
                    .iter()
                    .map(|path| {
                        if let Some((id, path, highlighted, on_click)) = path {
                            let on_click = on_click.clone();
                            div()
                                .w(px(wh + 8.))
                                .h(px(wh + 8.))
                                .flex()
                                .flex_col()
                                .items_center()
                                .child(div().flex_1())
                                .child(svg().path(*path).w(px(wh)).h_auto().text_color(theme.text))
                                .child(div().flex_1())
                                .bg(if highlighted(self.state.read(cx).tool.read(cx)) {
                                    theme.selection
                                } else {
                                    rgba(0)
                                })
                                .id(*id)
                                .on_click({
                                    let state = self.state.clone();
                                    move |_, _, cx| {
                                        on_click(state.clone(), cx);
                                    }
                                })
                        } else {
                            div()
                                .flex()
                                .flex_row()
                                .child(
                                    div()
                                        .w_2()
                                        .h(px(wh + 8.))
                                        .border_r_1()
                                        .border_color(theme.divider),
                                )
                                .child(div().w_2())
                                .id("dummy") // TODO: fix?
                        }
                    })
                    .collect_vec()
            })
            .child(div().flex_1())
    }
}

#[derive(Default)]
pub struct LayerSideBarState {
    used_filter: bool,
}

pub struct LayerSideBar {
    layers: Entity<Layers>,
    name_filter: Entity<TextInput>,
    state: Entity<LayerSideBarState>,
    editor_state: Entity<EditorState>,
    #[allow(dead_code)]
    subscriptions: Vec<Subscription>,
}

impl LayerSideBar {
    pub fn new(
        cx: &mut Context<Self>,
        editor_state: &Entity<EditorState>,
        canvas: &Entity<LayoutCanvas>,
    ) -> Self {
        let layers = editor_state.read(cx).layers.clone();
        let name_filter =
            cx.new(|cx| TextInput::new_filter(cx, cx.focus_handle(), editor_state, canvas));
        let state = cx.new(|_cx| LayerSideBarState::default());
        let subscriptions = vec![
            cx.observe(&layers, |_, _, cx| cx.notify()),
            cx.observe(&name_filter, |_, _, cx| cx.notify()),
        ];
        Self {
            layers,
            name_filter,
            state,
            editor_state: editor_state.clone(),
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
        let theme = self.editor_state.read(cx).theme();
        let icon_wh = 16.;
        let icon_div = || {
            div()
                .w(px(icon_wh + 8.))
                .h(px(icon_wh + 8.))
                .flex()
                .flex_col()
                .items_center()
                .child(div().flex_1())
        };
        div()
            .flex()
            .flex_col()
            .h_full()
            .w(px(200.))
            .p_1()
            .border_l_1()
            .border_t_1()
            .border_color(theme.divider)
            .bg(theme.sidebar)
            .min_h_0()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_center()
                    .child("Layers")
                    .child(div().flex_1())
                    .child(
                        icon_div()
                            .child(
                                svg()
                                    .path("icons/eye-solid-full.svg")
                                    .w(px(icon_wh))
                                    .h_auto()
                                    .text_color(theme.text),
                            )
                            .child(div().flex_1())
                            .id("all_visible_hierarchy_btn")
                            .on_click({
                                let layers = self.layers.clone();
                                move |_event, _window, cx| {
                                    layers.update(cx, |state, cx| {
                                        for (_, layer) in state.layers.iter_mut() {
                                            layer.visible = true;
                                        }
                                        cx.notify();
                                    })
                                }
                            }),
                    )
                    .child(
                        icon_div()
                            .child(
                                svg()
                                    .path("icons/eye-slash-solid-full.svg")
                                    .w(px(icon_wh))
                                    .h_auto()
                                    .text_color(theme.text),
                            )
                            .child(div().flex_1())
                            .id("none_visible_hierarchy_btn")
                            .on_click({
                                let layers = self.layers.clone();
                                move |_event, _window, cx| {
                                    layers.update(cx, |state, cx| {
                                        for (_, layer) in state.layers.iter_mut() {
                                            layer.visible = false;
                                        }
                                        cx.notify();
                                    })
                                }
                            }),
                    )
                    .child(
                        icon_div()
                            .child(
                                svg()
                                    .path(if self.state.read(cx).used_filter {
                                        "icons/filter-solid-full.svg"
                                    } else {
                                        "icons/filter-circle-xmark-solid-full.svg"
                                    })
                                    .w(px(icon_wh))
                                    .h_auto()
                                    .text_color(theme.text),
                            )
                            .child(div().flex_1())
                            .id("filter_used_btn")
                            .on_click({
                                let state = self.state.clone();
                                move |_event, _window, cx| {
                                    state.update(cx, |state, cx| {
                                        state.used_filter = !state.used_filter;
                                        cx.notify();
                                    })
                                }
                            }),
                    ),
            )
            .child(self.name_filter.clone())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .items_start()
                    .id("layers_scroll_vert")
                    .overflow_y_scroll()
                    .children(
                        layers
                            .layers
                            .values()
                            .filter(|layer| {
                                layer
                                    .name
                                    .to_lowercase()
                                    .contains(&self.name_filter.read(cx).content.to_lowercase())
                                    && (!self.state.read(cx).used_filter || layer.used)
                            })
                            .map(|layer| {
                                div()
                                    .flex()
                                    .w_full()
                                    .bg(if Some(&layer.name) == layers.selected_layer.as_ref() {
                                        theme.selection
                                    } else {
                                        theme.sidebar
                                    })
                                    .child(
                                        div()
                                            .id(SharedString::from(format!(
                                                "layer_select_{}",
                                                layer.z
                                            )))
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
                                        icon_div()
                                            .child(
                                                svg()
                                                    .path(if layer.visible {
                                                        "icons/eye-solid-full.svg"
                                                    } else {
                                                        "icons/eye-slash-solid-full.svg"
                                                    })
                                                    .w(px(icon_wh))
                                                    .h_auto()
                                                    .text_color(theme.text),
                                            )
                                            .child(div().flex_1())
                                            .id(SharedString::from(format!(
                                                "layer_control_{}",
                                                layer.z
                                            )))
                                            .on_click({
                                                let layers = self.layers.clone();
                                                let name = layer.name.clone();
                                                move |_event, _window, cx| {
                                                    layers.update(cx, |state, cx| {
                                                        state
                                                            .layers
                                                            .get_mut(&name)
                                                            .unwrap()
                                                            .visible = !state.layers[&name].visible;
                                                        cx.notify();
                                                    })
                                                }
                                            }),
                                    )
                            }),
                    ),
            )
    }
}

#[derive(Default)]
pub struct HierarchySideBarState {
    pub expanded_scopes: IndexSet<ScopePath>,
}

pub struct HierarchySideBar {
    editor_state: Entity<EditorState>,
    tool: Entity<ToolState>,
    name_filter: Entity<TextInput>,
    pub state: Entity<HierarchySideBarState>,
    #[allow(dead_code)]
    subscriptions: Vec<Subscription>,
}

impl HierarchySideBar {
    pub fn new(
        cx: &mut Context<Self>,
        editor_state: &Entity<EditorState>,
        canvas: &Entity<LayoutCanvas>,
    ) -> Self {
        let solved_cell = editor_state.read(cx).solved_cell.clone();
        let tool = editor_state.read(cx).tool.clone();
        let name_filter =
            cx.new(|cx| TextInput::new_filter(cx, cx.focus_handle(), editor_state, canvas));
        let subscriptions = vec![cx.observe(&solved_cell, |_, _, cx| cx.notify())];
        let state = cx.new(|_cx| HierarchySideBarState::default());
        Self {
            editor_state: editor_state.clone(),
            tool,
            name_filter,
            state,
            subscriptions,
        }
    }

    fn render_scopes_helper(
        &mut self,
        cx: &mut Context<Self>,
        solved_cell: &CompileOutputState,
        scopes: &mut Vec<Div>,
        scope: ScopeAddress,
        count: usize,
        depth: usize,
    ) {
        let icon_wh = 16.;
        let icon_div = || {
            div()
                .w(px(icon_wh + 8.))
                .h(px(icon_wh + 8.))
                .flex()
                .flex_col()
                .items_center()
                .child(div().flex_1())
        };
        let solved_cell_clone_1 = self.editor_state.read(cx).solved_cell.clone();
        let solved_cell_clone_2 = self.editor_state.read(cx).solved_cell.clone();
        let tool_clone = self.tool.clone();
        let scope_state = &solved_cell.state[&solved_cell.scope_paths[&scope]];
        let scope_path = solved_cell.scope_paths[&scope].clone();
        let self_entity = cx.entity();
        let expanded = self.state.read(cx).expanded_scopes.contains(&scope_path);
        let theme = self.editor_state.read(cx).theme();
        if scope_state
            .name
            .to_lowercase()
            .contains(&self.name_filter.read(cx).content.to_lowercase())
        {
            scopes.push(
                div()
                    .flex()
                    .w_full()
                    .bg(
                        if scope == solved_cell.state[&solved_cell.selected_scope].address {
                            theme.selection
                        } else {
                            theme.sidebar
                        },
                    )
                    .child(div().w(px(12. * depth as f32)))
                    .child(
                        icon_div()
                            .child(
                                svg()
                                    .path(if expanded {
                                        "icons/angle-down-solid-full.svg"
                                    } else {
                                        "icons/angle-right-solid-full.svg"
                                    })
                                    .w(px(icon_wh))
                                    .h_auto()
                                    .text_color(theme.text),
                            )
                            .child(div().flex_1())
                            .id(SharedString::from(format!("scope_collapse_{scope:?}",)))
                            .on_click({
                                let scope_path = scope_path.clone();
                                move |_event, _window, cx| {
                                    self_entity.read(cx).state.clone().update(cx, |state, cx| {
                                        if !state.expanded_scopes.insert(scope_path.clone()) {
                                            state.expanded_scopes.swap_remove(&scope_path);
                                        }
                                        cx.notify();
                                    });
                                }
                            }),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("scope_select_{scope:?}")))
                            .flex_1()
                            .overflow_hidden()
                            .child(format!(
                                "{}{}",
                                &scope_state.name,
                                if count > 1 {
                                    format!(" ({count})")
                                } else {
                                    "".to_string()
                                }
                            ))
                            .on_click({
                                let scope_path = scope_path.clone();
                                move |_event, _window, cx| {
                                    solved_cell_clone_1.update(cx, |state, cx| {
                                        if let Some(state) = state.as_mut() {
                                            state.selected_scope = scope_path.clone();
                                            cx.notify();
                                        }
                                    });
                                    tool_clone.update(cx, |tool, cx| {
                                        *tool = ToolState::default();
                                        cx.notify();
                                    });
                                }
                            }),
                    )
                    .child(
                        icon_div()
                            .child(
                                svg()
                                    .path(if scope_state.visible {
                                        "icons/eye-solid-full.svg"
                                    } else {
                                        "icons/eye-slash-solid-full.svg"
                                    })
                                    .w(px(icon_wh))
                                    .h_auto()
                                    .text_color(theme.text),
                            )
                            .child(div().flex_1())
                            .id(SharedString::from(format!("scope_control_{scope:?}",)))
                            .on_click({
                                let scope_path = scope_path.clone();
                                move |_event, _window, cx| {
                                    solved_cell_clone_2.update(cx, |state, cx| {
                                        if let Some(state) = state.as_mut() {
                                            state.state.get_mut(&scope_path).unwrap().visible =
                                                !state.state[&scope_path].visible;
                                            cx.notify();
                                        }
                                    })
                                }
                            }),
                    ),
            );
        }
        let scope_info = &solved_cell.output.cells[&scope.cell].scopes[&scope.scope];
        let mut cells = IndexMap::new();
        for (obj, _) in scope_info.emit.iter() {
            let elt = &solved_cell.output.cells[&scope.cell].objects[obj];
            if let SolvedValue::Instance(inst) = elt {
                *cells.entry(inst.cell).or_insert(0) += 1;
            }
        }

        if expanded {
            for (cell, count) in cells {
                let scope = solved_cell.output.cells[&cell].root;
                self.render_scopes_helper(
                    cx,
                    solved_cell,
                    scopes,
                    ScopeAddress { scope, cell },
                    count,
                    depth + 1,
                );
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
                    1,
                    depth + 1,
                );
            }
        }
    }

    fn render_scopes(&mut self, cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        let mut scopes = Vec::new();
        if let Some(state) = self.editor_state.read(cx).solved_cell.read(cx).clone() {
            let scope = state.output.cells[&state.output.top].root;
            self.render_scopes_helper(
                cx,
                &state,
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
        let theme = self.editor_state.read(cx).theme();
        let icon_wh = 16.;
        let icon_div = || {
            div()
                .w(px(icon_wh + 8.))
                .h(px(icon_wh + 8.))
                .flex()
                .flex_col()
                .items_center()
                .child(div().flex_1())
        };
        div()
            .flex()
            .flex_col()
            .h_full()
            .w(px(200.))
            .p_1()
            .border_r_1()
            .border_t_1()
            .border_color(theme.divider)
            .bg(theme.sidebar)
            .min_h_0()
            .child(
                div()
                    .flex()
                    .flex_row()
                    .justify_center()
                    .child("Scopes")
                    .child(div().flex_1())
                    .child(
                        icon_div()
                            .child(
                                svg()
                                    .path("icons/eye-solid-full.svg")
                                    .w(px(icon_wh))
                                    .h_auto()
                                    .text_color(theme.text),
                            )
                            .child(div().flex_1())
                            .id("all_visible_hierarchy_btn")
                            .on_click({
                                let solved_cell = self.editor_state.read(cx).solved_cell.clone();
                                move |_event, _window, cx| {
                                    solved_cell.update(cx, |cell, cx| {
                                        if let Some(cell) = cell {
                                            for state in cell.state.values_mut() {
                                                state.visible = true;
                                            }
                                        }
                                        cx.notify();
                                    })
                                }
                            }),
                    )
                    .child(
                        icon_div()
                            .child(
                                svg()
                                    .path("icons/eye-slash-solid-full.svg")
                                    .w(px(icon_wh))
                                    .h_auto()
                                    .text_color(theme.text),
                            )
                            .child(div().flex_1())
                            .id("none_visible_hierarchy_btn")
                            .on_click({
                                let solved_cell = self.editor_state.read(cx).solved_cell.clone();
                                move |_event, _window, cx| {
                                    solved_cell.update(cx, |cell, cx| {
                                        if let Some(cell) = cell {
                                            for state in cell.state.values_mut() {
                                                state.visible = false;
                                            }
                                        }
                                        cx.notify();
                                    })
                                }
                            }),
                    )
                    .child(
                        icon_div()
                            .child(
                                svg()
                                    .path("icons/angles-down-solid-full.svg")
                                    .w(px(icon_wh))
                                    .h_auto()
                                    .text_color(theme.text),
                            )
                            .child(div().flex_1())
                            .id("none_collapse_hierarchy_btn")
                            .on_click({
                                let self_entity = cx.entity();
                                let solved_cell = self.editor_state.read(cx).solved_cell.clone();
                                move |_event, _window, cx| {
                                    let mut scope_paths = IndexSet::new();
                                    if let Some(cell) = solved_cell.read(cx) {
                                        for path in cell.state.keys() {
                                            scope_paths.insert(path.clone());
                                        }
                                    }
                                    self_entity.read(cx).state.clone().update(cx, |state, cx| {
                                        state.expanded_scopes = scope_paths;
                                        cx.notify();
                                    });
                                }
                            }),
                    )
                    .child(
                        icon_div()
                            .child(
                                svg()
                                    .path("icons/angles-up-solid-full.svg")
                                    .w(px(icon_wh))
                                    .h_auto()
                                    .text_color(theme.text),
                            )
                            .child(div().flex_1())
                            .id("all_collapse_hierarchy_btn")
                            .on_click({
                                let self_entity = cx.entity();
                                move |_event, _window, cx| {
                                    self_entity.read(cx).state.clone().update(cx, |state, cx| {
                                        state.expanded_scopes.clear();
                                        cx.notify();
                                    });
                                }
                            }),
                    )
                    .child(
                        icon_div()
                            .child(
                                svg()
                                    .path(if self.editor_state.read(cx).hide_external_geometry {
                                        "icons/bug-solid-full.svg"
                                    } else {
                                        "icons/bug-slash-solid-full.svg"
                                    })
                                    .w(px(icon_wh))
                                    .h_auto()
                                    .text_color(theme.text),
                            )
                            .child(div().flex_1())
                            .id("hide_external_geometry")
                            .on_click({
                                let editor_state = self.editor_state.clone();
                                move |_event, _window, cx| {
                                    editor_state.update(cx, |state, cx| {
                                        state.hide_external_geometry =
                                            !state.hide_external_geometry;
                                        cx.notify();
                                    })
                                }
                            }),
                    ),
            )
            .child(self.name_filter.clone())
            .child(self.render_scopes(cx))
    }
}
