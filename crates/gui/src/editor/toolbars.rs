use std::path::Path;
use std::sync::Arc;

use analyzer::rpc::LangServerAction;
use argonc::compile::{CellId, SolvedValue};
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

const DEFAULT_SIDEBAR_WIDTH: f32 = 200.;
const MIN_SIDEBAR_WIDTH: f32 = 180.;
const MAX_SIDEBAR_WIDTH: f32 = 600.;
const SIDEBAR_RESIZE_HANDLE_WIDTH: f32 = 6.;
const SIDEBAR_SCROLLBAR_WIDTH: f32 = 10.;
const SIDEBAR_SCROLLBAR_MIN_THUMB: f32 = 24.;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarEdge {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SidebarResize(SidebarEdge);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarScrollAxis {
    Horizontal,
    Vertical,
}

#[derive(Clone, Copy, Debug)]
struct SidebarScrollDrag {
    axis: SidebarScrollAxis,
    grab_offset: Pixels,
}

#[derive(Default)]
struct SidebarScrollState {
    drag: Option<SidebarScrollDrag>,
}

fn clamp_sidebar_width(width: Pixels) -> Pixels {
    width.clamp(px(MIN_SIDEBAR_WIDTH), px(MAX_SIDEBAR_WIDTH))
}

fn axis_size(size: Size<Pixels>, axis: SidebarScrollAxis) -> Pixels {
    match axis {
        SidebarScrollAxis::Horizontal => size.width,
        SidebarScrollAxis::Vertical => size.height,
    }
}

fn axis_position(position: Point<Pixels>, axis: SidebarScrollAxis) -> Pixels {
    match axis {
        SidebarScrollAxis::Horizontal => position.x,
        SidebarScrollAxis::Vertical => position.y,
    }
}

fn scrollbar_thumb_metrics(
    viewport_length: Pixels,
    max_offset: Pixels,
    current_offset: Pixels,
    track_length: Pixels,
) -> Option<(Pixels, Pixels)> {
    if viewport_length <= px(0.) || max_offset <= px(0.) || track_length <= px(0.) {
        return None;
    }

    let content_length = viewport_length + max_offset;
    let minimum_thumb_length = px(SIDEBAR_SCROLLBAR_MIN_THUMB).min(track_length);
    let thumb_length = (track_length * (viewport_length / content_length))
        .clamp(minimum_thumb_length, track_length);
    let thumb_travel = track_length - thumb_length;
    let progress = (-current_offset / max_offset).clamp(0., 1.);
    Some((thumb_travel * progress, thumb_length))
}

fn scrollbar_thumb_bounds(
    scroll_handle: &ScrollHandle,
    track_bounds: Bounds<Pixels>,
    axis: SidebarScrollAxis,
) -> Option<Bounds<Pixels>> {
    let viewport_length = axis_size(scroll_handle.bounds().size, axis);
    let max_offset = axis_size(scroll_handle.max_offset(), axis);
    let current_offset = axis_position(scroll_handle.offset(), axis);
    let track_length = axis_size(track_bounds.size, axis);
    let (thumb_offset, thumb_length) =
        scrollbar_thumb_metrics(viewport_length, max_offset, current_offset, track_length)?;
    let mut thumb_bounds = track_bounds;
    match axis {
        SidebarScrollAxis::Horizontal => {
            thumb_bounds.origin.x += thumb_offset;
            thumb_bounds.size.width = thumb_length;
        }
        SidebarScrollAxis::Vertical => {
            thumb_bounds.origin.y += thumb_offset;
            thumb_bounds.size.height = thumb_length;
        }
    }
    Some(thumb_bounds)
}

fn scroll_to_scrollbar_position(
    scroll_handle: &ScrollHandle,
    track_bounds: Bounds<Pixels>,
    axis: SidebarScrollAxis,
    cursor_position: Point<Pixels>,
    grab_offset: Pixels,
) {
    let Some(thumb_bounds) = scrollbar_thumb_bounds(scroll_handle, track_bounds, axis) else {
        return;
    };
    let track_length = axis_size(track_bounds.size, axis);
    let thumb_length = axis_size(thumb_bounds.size, axis);
    let thumb_travel = track_length - thumb_length;
    if thumb_travel <= px(0.) {
        return;
    }

    let thumb_start = (axis_position(cursor_position, axis)
        - axis_position(track_bounds.origin, axis)
        - grab_offset)
        .clamp(px(0.), thumb_travel);
    let max_offset = axis_size(scroll_handle.max_offset(), axis);
    let new_axis_offset = -max_offset * (thumb_start / thumb_travel);
    let mut new_offset = scroll_handle.offset();
    match axis {
        SidebarScrollAxis::Horizontal => new_offset.x = new_axis_offset,
        SidebarScrollAxis::Vertical => new_offset.y = new_axis_offset,
    }
    scroll_handle.set_offset(new_offset);
}

fn sidebar_scrollbar(
    axis: SidebarScrollAxis,
    scroll_handle: ScrollHandle,
    scroll_state: Entity<SidebarScrollState>,
    owner: EntityId,
    theme: &'static Theme,
) -> impl IntoElement {
    let track_color = Hsla::from(theme.divider).opacity(0.35);
    let thumb_color = Hsla::from(theme.subtext).opacity(0.7);
    canvas(
        |_, _, _| (),
        move |track_bounds, _, window, _cx| {
            window.paint_quad(fill(track_bounds, track_color));
            let thumb_bounds = scrollbar_thumb_bounds(&scroll_handle, track_bounds, axis);
            if let Some(thumb_bounds) = thumb_bounds {
                window.paint_quad(fill(thumb_bounds, thumb_color));
            } else {
                window.paint_quad(fill(track_bounds, thumb_color.opacity(0.35)));
            }

            window.on_mouse_event({
                let scroll_handle = scroll_handle.clone();
                let scroll_state = scroll_state.clone();
                move |event: &MouseDownEvent, phase, window, cx| {
                    if phase != DispatchPhase::Bubble
                        || event.button != MouseButton::Left
                        || !track_bounds.contains(&event.position)
                    {
                        return;
                    }
                    let Some(thumb_bounds) =
                        scrollbar_thumb_bounds(&scroll_handle, track_bounds, axis)
                    else {
                        return;
                    };
                    window.prevent_default();
                    cx.stop_propagation();
                    let in_thumb = thumb_bounds.contains(&event.position);
                    let grab_offset = if in_thumb {
                        axis_position(event.position, axis)
                            - axis_position(thumb_bounds.origin, axis)
                    } else {
                        axis_size(thumb_bounds.size, axis) / 2.
                    };
                    if !in_thumb {
                        scroll_to_scrollbar_position(
                            &scroll_handle,
                            track_bounds,
                            axis,
                            event.position,
                            grab_offset,
                        );
                    }
                    scroll_state.update(cx, |state, _cx| {
                        state.drag = Some(SidebarScrollDrag { axis, grab_offset });
                    });
                    cx.notify(owner);
                }
            });
            window.on_mouse_event({
                let scroll_handle = scroll_handle.clone();
                let scroll_state = scroll_state.clone();
                move |event: &MouseMoveEvent, phase, _window, cx| {
                    if phase != DispatchPhase::Capture || !event.dragging() {
                        return;
                    }
                    let Some(drag) = scroll_state.read(cx).drag else {
                        return;
                    };
                    if drag.axis != axis {
                        return;
                    }
                    scroll_to_scrollbar_position(
                        &scroll_handle,
                        track_bounds,
                        axis,
                        event.position,
                        drag.grab_offset,
                    );
                    cx.notify(owner);
                }
            });
            window.on_mouse_event({
                let scroll_state = scroll_state.clone();
                move |event: &MouseUpEvent, phase, _window, cx| {
                    if phase != DispatchPhase::Capture || event.button != MouseButton::Left {
                        return;
                    }
                    if scroll_state
                        .read(cx)
                        .drag
                        .is_some_and(|drag| drag.axis == axis)
                    {
                        scroll_state.update(cx, |state, _cx| state.drag = None);
                        cx.notify(owner);
                    }
                }
            });
        },
    )
    .size_full()
}

fn sidebar_scroll_area(
    id: &'static str,
    content: Div,
    scroll_handle: &ScrollHandle,
    scroll_state: &Entity<SidebarScrollState>,
    owner: EntityId,
    theme: &'static Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .relative()
        .flex_1()
        .min_h_0()
        .min_w_0()
        .child(
            content
                .id(SharedString::from(format!("{id}_content")))
                .size_full()
                .min_h_0()
                .min_w_0()
                .overflow_scroll()
                .scrollbar_width(px(SIDEBAR_SCROLLBAR_WIDTH))
                .track_scroll(scroll_handle),
        )
        .child(
            div()
                .absolute()
                .top_0()
                .right_0()
                .h_full()
                .w(px(SIDEBAR_SCROLLBAR_WIDTH))
                .cursor_default()
                .child(sidebar_scrollbar(
                    SidebarScrollAxis::Vertical,
                    scroll_handle.clone(),
                    scroll_state.clone(),
                    owner,
                    theme,
                )),
        )
        .child(
            div()
                .absolute()
                .bottom_0()
                .left_0()
                .w_full()
                .h(px(SIDEBAR_SCROLLBAR_WIDTH))
                .cursor_default()
                .child(sidebar_scrollbar(
                    SidebarScrollAxis::Horizontal,
                    scroll_handle.clone(),
                    scroll_state.clone(),
                    owner,
                    theme,
                )),
        )
}

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

pub struct LayerSideBarState {
    used_filter: bool,
    width: Pixels,
}

impl Default for LayerSideBarState {
    fn default() -> Self {
        Self {
            used_filter: false,
            width: px(DEFAULT_SIDEBAR_WIDTH),
        }
    }
}

pub struct LayerSideBar {
    layers: Entity<Layers>,
    name_filter: Entity<TextInput>,
    state: Entity<LayerSideBarState>,
    scroll_handle: ScrollHandle,
    scroll_state: Entity<SidebarScrollState>,
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
        let scroll_state = cx.new(|_cx| SidebarScrollState::default());
        let subscriptions = vec![
            cx.observe(&layers, |_, _, cx| cx.notify()),
            cx.observe(&name_filter, |_, _, cx| cx.notify()),
        ];
        Self {
            layers,
            name_filter,
            state,
            scroll_handle: ScrollHandle::new(),
            scroll_state,
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
        let width = self.state.read(cx).width;
        let icon_wh = 16.;
        let icon_div = || {
            div()
                .w(px(icon_wh + 8.))
                .h(px(icon_wh + 8.))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .items_center()
                .child(div().flex_1())
        };
        div()
            .flex()
            .flex_col()
            .relative()
            .h_full()
            .w(width)
            .flex_shrink_0()
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
            .child(sidebar_scroll_area(
                "layers_scroll_area",
                div().flex().flex_col().items_start().children(
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
                                .min_w_full()
                                .flex_shrink_0()
                                .bg(if Some(&layer.name) == layers.selected_layer.as_ref() {
                                    theme.selection
                                } else {
                                    theme.sidebar
                                })
                                .child(
                                    div()
                                        .id(SharedString::from(format!("layer_select_{}", layer.z)))
                                        .flex_grow()
                                        .flex_shrink_0()
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
                                                    state.layers.get_mut(&name).unwrap().visible =
                                                        !state.layers[&name].visible;
                                                    cx.notify();
                                                })
                                            }
                                        }),
                                )
                        }),
                ),
                &self.scroll_handle,
                &self.scroll_state,
                cx.entity_id(),
                theme,
            ))
            .child(
                div()
                    .id("layers_resize_handle")
                    .absolute()
                    .top_0()
                    .left(px(-SIDEBAR_RESIZE_HANDLE_WIDTH / 2.))
                    .h_full()
                    .w(px(SIDEBAR_RESIZE_HANDLE_WIDTH))
                    .cursor_col_resize()
                    .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                        cx.stop_propagation();
                    })
                    .on_drag(SidebarResize(SidebarEdge::Left), |_, _, _window, cx| {
                        cx.new(|_cx| Empty)
                    }),
            )
            .on_drag_move::<SidebarResize>({
                let state = self.state.clone();
                move |event, _window, cx| {
                    if event.drag(cx).0 != SidebarEdge::Left {
                        return;
                    }
                    let width = clamp_sidebar_width(event.bounds.right() - event.event.position.x);
                    state.update(cx, |state, cx| {
                        if state.width != width {
                            state.width = width;
                            cx.notify();
                        }
                    });
                }
            })
    }
}

pub struct HierarchySideBarState {
    pub expanded_scopes: IndexSet<ScopePath>,
    width: Pixels,
    pub(super) context_menu: Option<HierarchyContextMenu>,
}

impl Default for HierarchySideBarState {
    fn default() -> Self {
        Self {
            expanded_scopes: IndexSet::new(),
            width: px(DEFAULT_SIDEBAR_WIDTH),
            context_menu: None,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct HierarchyContextMenu {
    cell: CellId,
    position: Point<Pixels>,
}

pub struct HierarchySideBar {
    editor_state: Entity<EditorState>,
    tool: Entity<ToolState>,
    name_filter: Entity<TextInput>,
    pub state: Entity<HierarchySideBarState>,
    scroll_handle: ScrollHandle,
    scroll_state: Entity<SidebarScrollState>,
    canvas: Entity<LayoutCanvas>,
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
        let scroll_state = cx.new(|_cx| SidebarScrollState::default());
        Self {
            editor_state: editor_state.clone(),
            tool,
            name_filter,
            state,
            scroll_handle: ScrollHandle::new(),
            scroll_state,
            canvas: canvas.clone(),
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
                .flex_shrink_0()
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
            let is_cell = solved_cell.output.cells[&scope.cell].root == scope.scope;
            let mut scope_name = div()
                .id(SharedString::from(format!("scope_select_{scope:?}")))
                .flex_grow()
                .flex_shrink_0()
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
                });
            if is_cell {
                let sidebar_state = self.state.clone();
                scope_name =
                    scope_name.on_mouse_down(MouseButton::Right, move |event, window, cx| {
                        window.prevent_default();
                        cx.stop_propagation();
                        sidebar_state.update(cx, |state, cx| {
                            state.context_menu = Some(HierarchyContextMenu {
                                cell: scope.cell,
                                position: event.position,
                            });
                            cx.notify();
                        });
                    });
            }
            scopes.push(
                div()
                    .flex()
                    .min_w_full()
                    .flex_shrink_0()
                    .bg(
                        if scope == solved_cell.state[&solved_cell.selected_scope].address {
                            theme.selection
                        } else {
                            theme.sidebar
                        },
                    )
                    .child(div().w(px(12. * depth as f32)).flex_shrink_0())
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
                    .child(scope_name)
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
        let theme = self.editor_state.read(cx).theme();
        let mut scroll_area = sidebar_scroll_area(
            "hierarchy_scroll_area",
            div().flex().flex_col().items_start().children(scopes),
            &self.scroll_handle,
            &self.scroll_state,
            cx.entity_id(),
            theme,
        );

        let context_menu = self.state.read(cx).context_menu;
        if let Some(context_menu) = context_menu {
            let (cell_name, is_current_top) = {
                let editor_state = self.editor_state.read(cx);
                let solved_cell = editor_state.solved_cell.read(cx);
                let Some(solved_cell) = solved_cell.as_ref() else {
                    return scroll_area;
                };
                let Some(cell) = solved_cell.output.cells.get(&context_menu.cell) else {
                    return scroll_area;
                };
                (
                    cell.scopes[&cell.root].name.clone(),
                    solved_cell.output.top == context_menu.cell,
                )
            };
            let bounds = self.scroll_handle.bounds();
            let menu_width = px(180.);
            let menu_height = px(58.);
            let local_position = point(
                (context_menu.position.x - bounds.origin.x)
                    .clamp(px(0.), (bounds.size.width - menu_width).max(px(0.))),
                (context_menu.position.y - bounds.origin.y)
                    .clamp(px(0.), (bounds.size.height - menu_height).max(px(0.))),
            );
            let mut action =
                div()
                    .id("set_hierarchy_top_cell")
                    .px_2()
                    .py_1()
                    .child(if is_current_top {
                        "Current top cell"
                    } else {
                        "Set as top cell"
                    });
            if is_current_top {
                action = action.text_color(theme.subtext);
            } else {
                let editor_state = self.editor_state.clone();
                let canvas = self.canvas.clone();
                let sidebar_state = self.state.clone();
                let cell = context_menu.cell;
                action = action
                    .cursor_pointer()
                    .hover(|style| style.bg(theme.selection))
                    .on_click(move |_event, _window, cx| {
                        sidebar_state.update(cx, |state, cx| {
                            state.context_menu = None;
                            cx.notify();
                        });
                        let changed =
                            editor_state.update(cx, |state, cx| state.set_top_cell(cell, cx));
                        if changed {
                            canvas.update(cx, |canvas, cx| {
                                canvas.fit_to_screen(cx);
                                cx.notify();
                            });
                        }
                    });
            }
            let menu = div()
                .id("hierarchy_context_menu")
                .absolute()
                .left(local_position.x)
                .top(local_position.y)
                .w(menu_width)
                .py_1()
                .rounded_sm()
                .border_1()
                .border_color(theme.divider)
                .shadow_md()
                .bg(theme.bg)
                .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                    cx.stop_propagation();
                })
                .on_mouse_down(MouseButton::Right, |_event, window, cx| {
                    window.prevent_default();
                    cx.stop_propagation();
                })
                .child(
                    div()
                        .px_2()
                        .pb_1()
                        .text_xs()
                        .text_color(theme.subtext)
                        .child(cell_name),
                )
                .child(action);
            scroll_area = scroll_area.child(deferred(menu).with_priority(1));
        }
        scroll_area
    }
}

impl Render for HierarchySideBar {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let theme = self.editor_state.read(cx).theme();
        let width = self.state.read(cx).width;
        let icon_wh = 16.;
        let icon_div = || {
            div()
                .w(px(icon_wh + 8.))
                .h(px(icon_wh + 8.))
                .flex_shrink_0()
                .flex()
                .flex_col()
                .items_center()
                .child(div().flex_1())
        };
        div()
            .flex()
            .flex_col()
            .relative()
            .h_full()
            .w(width)
            .flex_shrink_0()
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
            .child(
                div()
                    .id("hierarchy_resize_handle")
                    .absolute()
                    .top_0()
                    .right(px(-SIDEBAR_RESIZE_HANDLE_WIDTH / 2.))
                    .h_full()
                    .w(px(SIDEBAR_RESIZE_HANDLE_WIDTH))
                    .cursor_col_resize()
                    .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                        cx.stop_propagation();
                    })
                    .on_drag(SidebarResize(SidebarEdge::Right), |_, _, _window, cx| {
                        cx.new(|_cx| Empty)
                    }),
            )
            .on_mouse_down(MouseButton::Left, {
                let state = self.state.clone();
                move |_event, _window, cx| {
                    if state.read(cx).context_menu.is_some() {
                        state.update(cx, |state, cx| {
                            state.context_menu = None;
                            cx.notify();
                        });
                    }
                }
            })
            .on_drag_move::<SidebarResize>({
                let state = self.state.clone();
                move |event, _window, cx| {
                    if event.drag(cx).0 != SidebarEdge::Right {
                        return;
                    }
                    let width = clamp_sidebar_width(event.event.position.x - event.bounds.left());
                    state.update(cx, |state, cx| {
                        if state.width != width {
                            state.width = width;
                            cx.notify();
                        }
                    });
                }
            })
    }
}

#[cfg(test)]
mod sidebar_layout_tests {
    use gpui::px;

    use super::{
        MAX_SIDEBAR_WIDTH, MIN_SIDEBAR_WIDTH, clamp_sidebar_width, scrollbar_thumb_metrics,
    };

    #[test]
    fn sidebar_width_is_limited_to_usable_bounds() {
        assert_eq!(clamp_sidebar_width(px(80.)), px(MIN_SIDEBAR_WIDTH));
        assert_eq!(clamp_sidebar_width(px(320.)), px(320.));
        assert_eq!(clamp_sidebar_width(px(900.)), px(MAX_SIDEBAR_WIDTH));
    }

    #[test]
    fn scrollbar_thumb_tracks_the_visible_fraction_and_offset() {
        assert_eq!(
            scrollbar_thumb_metrics(px(100.), px(100.), px(-50.), px(100.)),
            Some((px(25.), px(50.)))
        );
        assert_eq!(
            scrollbar_thumb_metrics(px(100.), px(0.), px(0.), px(100.)),
            None
        );
        assert_eq!(
            scrollbar_thumb_metrics(px(10.), px(10.), px(-5.), px(10.)),
            Some((px(0.), px(10.)))
        );
    }
}
