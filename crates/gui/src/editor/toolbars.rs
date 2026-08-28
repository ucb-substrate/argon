use std::path::Path;
use std::sync::Arc;

use analyzer::rpc::LangServerAction;
use argonc::compile::SolvedValue;
use gpui::prelude::*;
use gpui::*;
use indexmap::{IndexMap, IndexSet};
use itertools::Itertools;

use crate::{
    actions::{
        DrawDim, DrawPath, DrawPolygon, DrawRect, InstantiateCommand, OpenCellCommand, Redo,
        SelectMode, Undo,
    },
    editor::{
        CompileOutputState, Layers, ScopeAddress, ScopePath,
        canvas::{EditDimToolState, LayoutCanvas, ToolState},
        input::TextInput,
    },
    theme::Theme,
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

fn workspace_title(path: Option<&Path>) -> String {
    path.map_or_else(
        || "Argon".to_owned(),
        |path| format!("Argon — {}", path.display()),
    )
}

impl Render for TitleBar {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let state = self.state.read(cx);
        let theme = state.theme();
        let mut centered_title = div()
            .relative()
            .child(workspace_title(state.workspace_path.as_deref()));
        if state.workspace_modified {
            centered_title = centered_title.child(div().absolute().left_full().ml_1().child("[+]"));
        }
        div()
            .border_color(theme.divider)
            .window_control_area(WindowControlArea::Drag)
            .p_1()
            .bg(theme.titlebar)
            .flex()
            .items_center()
            .justify_center()
            .overflow_hidden()
            .whitespace_nowrap()
            .child(centered_title)
    }
}

#[cfg(test)]
mod title_bar_tests {
    use std::path::Path;

    use super::workspace_title;

    #[test]
    fn title_shows_workspace() {
        let workspace = Path::new("/projects/inverter");
        assert_eq!(
            workspace_title(Some(workspace)),
            "Argon — /projects/inverter"
        );
        assert_eq!(workspace_title(None), "Argon");
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

/// A hover tooltip naming a control, annotated with its hotkey when it has one.
struct ToolTip {
    label: SharedString,
    hotkey: Option<SharedString>,
    theme: &'static Theme,
}

impl ToolTip {
    /// Builds the tooltip view handed to [`InteractiveElement::tooltip`].
    fn build(
        label: &'static str,
        hotkey: Option<SharedString>,
        theme: &'static Theme,
        cx: &mut App,
    ) -> AnyView {
        cx.new(|_cx| Self {
            label: label.into(),
            hotkey,
            theme,
        })
        .into()
    }
}

/// Formats the hotkey bound to `action` for display, if it has one.
///
/// Bindings are resolved in the layout canvas context, since that is where the tool
/// hotkeys are bound regardless of what currently holds focus.
fn hotkey_text(action: &dyn Action, window: &Window) -> Option<SharedString> {
    let mut context = KeyContext::new_with_defaults();
    context.add("LayoutCanvas");
    let binding = window.highest_precedence_binding_for_action_in_context(action, context)?;
    Some(
        binding
            .keystrokes()
            .iter()
            .map(ToString::to_string)
            .join(" ")
            .into(),
    )
}

impl Render for ToolTip {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        _cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let theme = self.theme;
        // Tooltips are painted as their own root element, so the editor's text styles
        // do not cascade into them.
        div()
            .font_family("Zed Plex Sans")
            .text_sm()
            .text_color(theme.text)
            .ml_2()
            .mt_2()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .rounded_sm()
            .border_1()
            .border_color(theme.divider)
            .bg(theme.bg)
            .whitespace_nowrap()
            .child(self.label.clone())
            .children(
                self.hotkey
                    .clone()
                    .map(|hotkey| div().text_xs().text_color(theme.subtext).child(hotkey)),
            )
    }
}

impl Render for ToolBar {
    fn render(
        &mut self,
        window: &mut gpui::Window,
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
                enum ToolbarItem {
                    Button {
                        id: &'static str,
                        icon: &'static str,
                        /// Tooltip label naming the button.
                        label: &'static str,
                        /// Action whose hotkey the tooltip advertises, if it has one.
                        action: Box<dyn Action>,
                        highlighted: HighlightFn,
                        on_click: OnClickFn,
                    },
                    Divider(&'static str),
                    Spacer,
                }
                let tools: [ToolbarItem; _] = [
                    ToolbarItem::Button {
                        id: "btn_undo",
                        icon: "icons/arrow-rotate-left-solid-full.svg",
                        label: "Undo",
                        action: Box::new(Undo),
                        highlighted: Box::new(|_| false),
                        on_click: Arc::new(|state, cx| {
                            let _ = state
                                .read(cx)
                                .lang_server_client
                                .dispatch_action(LangServerAction::Undo);
                        }),
                    },
                    ToolbarItem::Button {
                        id: "btn_redo",
                        icon: "icons/arrow-rotate-right-solid-full.svg",
                        label: "Redo",
                        action: Box::new(Redo),
                        highlighted: Box::new(|_| false),
                        on_click: Arc::new(|state, cx| {
                            let _ = state
                                .read(cx)
                                .lang_server_client
                                .dispatch_action(LangServerAction::Redo);
                        }),
                    },
                    ToolbarItem::Button {
                        id: "btn_open_cell",
                        icon: "icons/folder-open.svg",
                        label: "Open cell",
                        action: Box::new(OpenCellCommand),
                        highlighted: Box::new(|_| false),
                        on_click: Arc::new(|_state, cx| {
                            cx.defer(move |cx| {
                                cx.dispatch_action(&OpenCellCommand);
                            });
                        }),
                    },
                    ToolbarItem::Divider("divider_history_select"),
                    ToolbarItem::Button {
                        id: "btn_select",
                        icon: "icons/arrow-pointer-solid-full.svg",
                        label: "Select",
                        action: Box::new(SelectMode),
                        highlighted: Box::new(|tool| {
                            matches!(
                                tool,
                                ToolState::Select(_)
                                    | ToolState::EditDim(EditDimToolState {
                                        dim_mode: false,
                                        ..
                                    })
                            )
                        }),
                        on_click: Arc::new(|_state, cx| {
                            cx.defer(move |cx| {
                                cx.dispatch_action(&SelectMode);
                            });
                        }),
                    },
                    ToolbarItem::Divider("divider_select_draw"),
                    ToolbarItem::Button {
                        id: "btn_rect",
                        icon: "icons/rect.svg",
                        label: "Rectangle",
                        action: Box::new(DrawRect),
                        highlighted: Box::new(|tool| matches!(tool, ToolState::DrawRect(_))),
                        on_click: Arc::new(|_state, cx| {
                            cx.defer(move |cx| {
                                cx.dispatch_action(&DrawRect);
                            })
                        }),
                    },
                    ToolbarItem::Button {
                        id: "btn_polygon",
                        icon: "icons/polygon.svg",
                        label: "Polygon",
                        action: Box::new(DrawPolygon),
                        highlighted: Box::new(|tool| matches!(tool, ToolState::DrawPolygon(_))),
                        on_click: Arc::new(|_state, cx| {
                            cx.defer(move |cx| {
                                cx.dispatch_action(&DrawPolygon);
                            })
                        }),
                    },
                    ToolbarItem::Button {
                        id: "btn_path",
                        icon: "icons/path.svg",
                        label: "Path",
                        action: Box::new(DrawPath),
                        highlighted: Box::new(|tool| matches!(tool, ToolState::DrawPath(_))),
                        on_click: Arc::new(|_state, cx| {
                            cx.defer(move |cx| {
                                cx.dispatch_action(&DrawPath);
                            })
                        }),
                    },
                    ToolbarItem::Button {
                        id: "btn_instance",
                        icon: "icons/instance.svg",
                        label: "Place instance",
                        action: Box::new(InstantiateCommand),
                        highlighted: Box::new(|tool| matches!(tool, ToolState::PlaceInstance(_))),
                        on_click: Arc::new(|_state, cx| {
                            cx.defer(move |cx| {
                                cx.dispatch_action(&InstantiateCommand);
                            });
                        }),
                    },
                    ToolbarItem::Divider("divider_draw_constraints"),
                    ToolbarItem::Button {
                        id: "btn_dim",
                        icon: "icons/arrows-left-right-to-line-solid-full.svg",
                        label: "Dimension",
                        action: Box::new(DrawDim),
                        highlighted: Box::new(|tool| {
                            matches!(
                                tool,
                                ToolState::DrawDim(_)
                                    | ToolState::EditDim(EditDimToolState { dim_mode: true, .. })
                            )
                        }),
                        on_click: Arc::new(|_state, cx| {
                            cx.defer(move |cx| {
                                cx.dispatch_action(&DrawDim);
                            });
                        }),
                    },
                    ToolbarItem::Spacer,
                ];
                let wh = 20.;
                tools
                    .iter()
                    .map(|item| match item {
                        ToolbarItem::Button {
                            id,
                            icon,
                            label,
                            action,
                            highlighted,
                            on_click,
                        } => {
                            let on_click = on_click.clone();
                            let hotkey = hotkey_text(action.as_ref(), window);
                            div()
                                .w(px(wh + 8.))
                                .h(px(wh + 8.))
                                .flex()
                                .flex_col()
                                .items_center()
                                .child(div().flex_1())
                                .child(svg().path(*icon).w(px(wh)).h_auto().text_color(theme.text))
                                .child(div().flex_1())
                                .bg(if highlighted(self.state.read(cx).tool.read(cx)) {
                                    theme.selection
                                } else {
                                    rgba(0)
                                })
                                .id(*id)
                                .tooltip({
                                    let label = *label;
                                    move |_window, cx| {
                                        ToolTip::build(label, hotkey.clone(), theme, cx)
                                    }
                                })
                                .on_click({
                                    let state = self.state.clone();
                                    move |_, _, cx| {
                                        on_click(state.clone(), cx);
                                    }
                                })
                        }
                        ToolbarItem::Divider(id) => div()
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
                            .id(*id),
                        ToolbarItem::Spacer => div().flex_1().id("toolbar_spacer"),
                    })
                    .collect_vec()
            })
    }
}

#[cfg(test)]
mod tool_bar_tests {
    use gpui::{Context, Render, TestAppContext, Window, div, prelude::*};

    use super::hotkey_text;
    use crate::{
        actions::{DrawDim, DrawPath, DrawPolygon, DrawRect, InstantiateCommand, SelectMode, Undo},
        key_bindings,
    };

    struct EmptyView;

    impl Render for EmptyView {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
        }
    }

    #[gpui::test]
    fn tooltips_report_the_bound_tool_hotkeys(cx: &mut TestAppContext) {
        let window = cx.update(|cx| {
            cx.bind_keys(key_bindings());
            cx.open_window(Default::default(), |_, cx| cx.new(|_| EmptyView))
                .unwrap()
        });

        window
            .update(cx, |_, window, _| {
                // Only modifier-free hotkeys are asserted verbatim, since modifiers render
                // differently per platform.
                assert_eq!(hotkey_text(&DrawRect, window), Some("R".into()));
                assert_eq!(hotkey_text(&DrawPolygon, window), Some("P".into()));
                assert_eq!(hotkey_text(&SelectMode, window), Some("S".into()));
                assert_eq!(hotkey_text(&DrawDim, window), Some("D".into()));
                assert_eq!(hotkey_text(&InstantiateCommand, window), Some("I".into()));
                assert_eq!(hotkey_text(&Undo, window), Some("U".into()));
                // The path tool has no binding, so its tooltip shows the label alone.
                assert_eq!(hotkey_text(&DrawPath, window), None);
            })
            .unwrap();
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
    // Retained to keep the sidebar's observations active.
    _subscriptions: Vec<Subscription>,
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
            _subscriptions: subscriptions,
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
    // Retained to keep the sidebar's observations active.
    _subscriptions: Vec<Subscription>,
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
            _subscriptions: subscriptions,
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
                                scope_state.name,
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
