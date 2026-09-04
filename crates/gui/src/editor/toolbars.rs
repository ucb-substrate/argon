use std::path::Path;
use std::sync::Arc;

use analyzer::rpc::LangServerAction;
use argonc::compile::{CellId, CompiledData, SolvedValue};
use gpui::prelude::*;
use gpui::*;
use indexmap::{IndexMap, IndexSet};
use itertools::Itertools;

use crate::{
    actions::{
        DrawDim, DrawPath, DrawPolygon, DrawRect, InstantiateCommand, NewCellCommand,
        OpenCellCommand, Redo, RenameCellCommand, SelectMode, Undo,
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
const DEFAULT_HIERARCHY_PREVIEW_HEIGHT: f32 = 220.;
const MIN_HIERARCHY_PREVIEW_HEIGHT: f32 = 96.;
const MIN_HIERARCHY_PANEL_HEIGHT: f32 = 144.;
const HIERARCHY_PREVIEW_RESIZE_HANDLE_HEIGHT: f32 = 7.;
const NAVIGATION_OVERVIEW_BOX_DRAG_THRESHOLD: f32 = 4.;

fn small_font_size(font_size: f32) -> f32 {
    font_size * 6. / 7.
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SidebarEdge {
    Left,
    Right,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct SidebarResize(SidebarEdge);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct HierarchyPreviewResize;

#[derive(Clone, Copy)]
struct NavigationOverviewDrag {
    start: Point<Pixels>,
    current: Point<Pixels>,
}

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

fn clamp_hierarchy_preview_height(height: Pixels, sidebar_height: Pixels) -> Pixels {
    let maximum = (sidebar_height - px(MIN_HIERARCHY_PANEL_HEIGHT)).max(px(0.));
    height.clamp(px(MIN_HIERARCHY_PREVIEW_HEIGHT).min(maximum), maximum)
}

fn clamp_point_to_bounds(point: Point<Pixels>, bounds: Bounds<Pixels>) -> Point<Pixels> {
    Point::new(
        point.x.clamp(bounds.left(), bounds.right()),
        point.y.clamp(bounds.top(), bounds.bottom()),
    )
}

fn navigation_overview_drag_bounds(drag: NavigationOverviewDrag) -> Bounds<Pixels> {
    Bounds::from_corners(
        Point::new(
            drag.start.x.min(drag.current.x),
            drag.start.y.min(drag.current.y),
        ),
        Point::new(
            drag.start.x.max(drag.current.x),
            drag.start.y.max(drag.current.y),
        ),
    )
}

fn navigation_overview_drag_is_box(drag: NavigationOverviewDrag) -> bool {
    let delta = drag.current - drag.start;
    f32::from(delta.x).abs().max(f32::from(delta.y).abs()) >= NAVIGATION_OVERVIEW_BOX_DRAG_THRESHOLD
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

fn sidebar_scroll_frame(
    id: &'static str,
    content: impl IntoElement,
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
        .child(content)
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

fn workspace_title_contents(path: Option<&Path>, modified: bool) -> Div {
    let mut contents = div().flex().min_w_0().max_w_full().items_center().child(
        div()
            .min_w_0()
            .overflow_hidden()
            .text_ellipsis()
            .child(workspace_title(path)),
    );
    if modified {
        contents = contents.child(
            div()
                .debug_selector(|| "workspace_modified_indicator".to_owned())
                .ml_1()
                .flex_none()
                .child("[+]"),
        );
    }
    contents
}

impl Render for TitleBar {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let state = self.state.read(cx);
        let theme = state.theme();
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
            .child(workspace_title_contents(
                state.workspace_path.as_deref(),
                state.workspace_modified,
            ))
    }
}

#[cfg(test)]
mod title_bar_tests {
    use std::path::{Path, PathBuf};

    use gpui::{Context, Render, TestAppContext, Window, div, prelude::*, px};

    use super::{workspace_title, workspace_title_contents};

    struct TitleBarPreview {
        path: PathBuf,
    }

    impl Render for TitleBarPreview {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .debug_selector(|| "title_bar".to_owned())
                .w(px(180.))
                .overflow_hidden()
                .flex()
                .justify_center()
                .whitespace_nowrap()
                .child(workspace_title_contents(Some(&self.path), true))
        }
    }

    #[test]
    fn title_shows_workspace() {
        let workspace = Path::new("/projects/inverter");
        assert_eq!(
            workspace_title(Some(workspace)),
            "Argon — /projects/inverter"
        );
        assert_eq!(workspace_title(None), "Argon");
    }

    #[gpui::test]
    fn modified_indicator_stays_inside_a_narrow_title_bar(cx: &mut TestAppContext) {
        let (_, cx) = cx.add_window_view(|_, _| TitleBarPreview {
            path: PathBuf::from("/projects/a-workspace-name-that-is-much-wider-than-the-title-bar"),
        });

        let title_bar = cx.debug_bounds("title_bar").unwrap();
        let indicator = cx.debug_bounds("workspace_modified_indicator").unwrap();
        assert!(indicator.left() >= title_bar.left());
        assert!(indicator.right() <= title_bar.right());
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
    font_size: Option<f32>,
}

impl ToolTip {
    /// Builds the tooltip view handed to [`StatefulInteractiveElement::tooltip`].
    fn build(
        label: &'static str,
        hotkey: Option<SharedString>,
        theme: &'static Theme,
        font_size: Option<f32>,
        cx: &mut App,
    ) -> AnyView {
        cx.new(|_cx| Self {
            label: label.into(),
            hotkey,
            theme,
            font_size,
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
        let mut tooltip = div()
            .font_family("Zed Plex Sans")
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
            .children(self.hotkey.clone().map(|hotkey| {
                let hotkey = div().text_color(theme.subtext).child(hotkey);
                if let Some(font_size) = self.font_size {
                    hotkey.text_size(px(small_font_size(font_size)))
                } else {
                    hotkey.text_xs()
                }
            }));
        tooltip = if let Some(font_size) = self.font_size {
            tooltip.text_size(px(font_size))
        } else {
            tooltip.text_sm()
        };
        tooltip
    }
}

impl Render for ToolBar {
    fn render(
        &mut self,
        window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let editor_state = self.state.read(cx);
        let theme = editor_state.theme();
        let icon_size = editor_state.icon_size;
        let font_size = editor_state.font_size;
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
                        id: "btn_new_cell",
                        icon: "icons/file-circle-plus.svg",
                        label: "New cell",
                        action: Box::new(NewCellCommand),
                        highlighted: Box::new(|_| false),
                        on_click: Arc::new(|_state, cx| {
                            cx.defer(move |cx| {
                                cx.dispatch_action(&NewCellCommand);
                            });
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
                    ToolbarItem::Button {
                        id: "btn_rename_cell",
                        icon: "icons/file-pen.svg",
                        label: "Rename cell",
                        action: Box::new(RenameCellCommand),
                        highlighted: Box::new(|_| false),
                        on_click: Arc::new(|_state, cx| {
                            cx.defer(move |cx| {
                                cx.dispatch_action(&RenameCellCommand);
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
                let wh = icon_size.unwrap_or(20.);
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
                                        ToolTip::build(label, hotkey.clone(), theme, font_size, cx)
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
        actions::{
            DrawDim, DrawPath, DrawPolygon, DrawRect, InstantiateCommand, NewCellCommand,
            RenameCellCommand, SelectMode, Undo,
        },
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
                assert!(hotkey_text(&NewCellCommand, window).is_some());
                assert!(hotkey_text(&RenameCellCommand, window).is_some());
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
    list_scroll_handle: UniformListScrollHandle,
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
        let list_scroll_handle = UniformListScrollHandle::new();
        let scroll_handle = list_scroll_handle.0.borrow().base_handle.clone();
        let subscriptions = vec![
            cx.observe(&layers, |_, _, cx| cx.notify()),
            cx.observe(&name_filter, |_, _, cx| cx.notify()),
        ];
        Self {
            layers,
            name_filter,
            state,
            scroll_handle,
            list_scroll_handle,
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
        let filter = self.name_filter.read(cx).content.to_lowercase();
        let used_filter = self.state.read(cx).used_filter;
        let (layer_rows, selected_layer) = {
            let layers = self.layers.read(cx);
            (
                Arc::new(
                    layers
                        .layers
                        .values()
                        .filter(|layer| {
                            layer.name.to_lowercase().contains(&filter)
                                && (!used_filter || layer.used)
                        })
                        .cloned()
                        .collect::<Vec<_>>(),
                ),
                layers.selected_layer.clone(),
            )
        };
        let editor_state = self.editor_state.read(cx);
        let theme = editor_state.theme();
        let width = self.state.read(cx).width;
        let icon_wh = editor_state.icon_size.unwrap_or(16.);
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
        let widest_row = layer_rows
            .iter()
            .enumerate()
            .max_by_key(|(_, layer)| layer.name.chars().count())
            .map(|(index, _)| index);
        let row_count = layer_rows.len();
        let rows_for_render = layer_rows.clone();
        let layers_for_render = self.layers.clone();
        let layer_list = uniform_list(
            "layer_rows",
            row_count,
            cx.processor(
                move |_sidebar, range: std::ops::Range<usize>, _window, _cx| {
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
                    range
                        .filter_map(|index| rows_for_render.get(index))
                        .map(|layer| {
                            div()
                                .flex()
                                .min_w_full()
                                .flex_shrink_0()
                                .bg(if Some(&layer.name) == selected_layer.as_ref() {
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
                                            let layers = layers_for_render.clone();
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
                                            let layers = layers_for_render.clone();
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
                        })
                        .collect::<Vec<_>>()
                },
            ),
        )
        .with_width_from_item(widest_row)
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .track_scroll(self.list_scroll_handle.clone())
        .size_full()
        .min_h_0()
        .min_w_0();
        let layer_scroll_area = sidebar_scroll_frame(
            "layers_scroll_area",
            layer_list,
            &self.scroll_handle,
            &self.scroll_state,
            cx.entity_id(),
            theme,
        );
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
            .child(layer_scroll_area)
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
    rows_revision: u64,
    width: Pixels,
    preview_height: Pixels,
    preview_drag: Option<NavigationOverviewDrag>,
    pub(super) context_menu: Option<HierarchyContextMenu>,
}

impl Default for HierarchySideBarState {
    fn default() -> Self {
        Self {
            expanded_scopes: IndexSet::new(),
            rows_revision: 0,
            width: px(DEFAULT_SIDEBAR_WIDTH),
            preview_height: px(DEFAULT_HIERARCHY_PREVIEW_HEIGHT),
            preview_drag: None,
            context_menu: None,
        }
    }
}

#[derive(Clone, Copy)]
pub(super) struct HierarchyContextMenu {
    cell: CellId,
    position: Point<Pixels>,
}

#[derive(Clone, Copy)]
struct HierarchyRow {
    scope: ScopeAddress,
    count: usize,
    depth: usize,
}

struct HierarchyRowsCache {
    output: Arc<CompiledData>,
    rows_revision: u64,
    filter: String,
    rows: Arc<Vec<HierarchyRow>>,
    widest_row: Option<usize>,
}

pub struct HierarchySideBar {
    editor_state: Entity<EditorState>,
    tool: Entity<ToolState>,
    name_filter: Entity<TextInput>,
    pub state: Entity<HierarchySideBarState>,
    scroll_handle: ScrollHandle,
    list_scroll_handle: UniformListScrollHandle,
    scroll_state: Entity<SidebarScrollState>,
    rows_cache: Option<HierarchyRowsCache>,
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
        let subscriptions = vec![
            cx.observe(&solved_cell, |_, _, cx| cx.notify()),
            cx.observe(canvas, |_, _, cx| cx.notify()),
        ];
        let state = cx.new(|_cx| HierarchySideBarState::default());
        let scroll_state = cx.new(|_cx| SidebarScrollState::default());
        let list_scroll_handle = UniformListScrollHandle::new();
        let scroll_handle = list_scroll_handle.0.borrow().base_handle.clone();
        Self {
            editor_state: editor_state.clone(),
            tool,
            name_filter,
            state,
            scroll_handle,
            list_scroll_handle,
            scroll_state,
            rows_cache: None,
            canvas: canvas.clone(),
            _subscriptions: subscriptions,
        }
    }

    fn collect_scope_rows(
        solved_cell: &CompileOutputState,
        expanded_scopes: &IndexSet<ScopePath>,
        filter: &str,
        rows: &mut Vec<HierarchyRow>,
        scope: ScopeAddress,
        count: usize,
        depth: usize,
    ) {
        let scope_state = &solved_cell.state[&solved_cell.scope_paths[&scope]];
        let scope_path = &solved_cell.scope_paths[&scope];
        let expanded = expanded_scopes.contains(scope_path);
        if scope_state.name.to_lowercase().contains(filter) {
            rows.push(HierarchyRow {
                scope,
                count,
                depth,
            });
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
                Self::collect_scope_rows(
                    solved_cell,
                    expanded_scopes,
                    filter,
                    rows,
                    ScopeAddress { scope, cell },
                    count,
                    depth + 1,
                );
            }
            for child_scope in scope_info.children.clone() {
                Self::collect_scope_rows(
                    solved_cell,
                    expanded_scopes,
                    filter,
                    rows,
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

    fn render_scope_row(
        &mut self,
        cx: &mut Context<Self>,
        solved_cell: &CompileOutputState,
        row: HierarchyRow,
    ) -> Div {
        let HierarchyRow {
            scope,
            count,
            depth,
        } = row;
        let editor_state = self.editor_state.read(cx);
        let icon_wh = editor_state.icon_size.unwrap_or(16.);
        let theme = editor_state.theme();
        let solved_cell_entity = editor_state.solved_cell.clone();
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
        let tool = self.tool.clone();
        let scope_state = &solved_cell.state[&solved_cell.scope_paths[&scope]];
        let scope_path = solved_cell.scope_paths[&scope].clone();
        let expanded = self.state.read(cx).expanded_scopes.contains(&scope_path);
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
                    String::new()
                }
            ))
            .on_click({
                let scope_path = scope_path.clone();
                let solved_cell = solved_cell_entity.clone();
                let canvas = self.canvas.clone();
                move |_event, _window, cx| {
                    let selection_changed = solved_cell.update(cx, |state, cx| {
                        let Some(state) = state.as_mut() else {
                            return false;
                        };
                        if state.selected_scope == scope_path {
                            return false;
                        }
                        state.selected_scope = scope_path.clone();
                        cx.notify();
                        true
                    });
                    tool.update(cx, |tool, cx| {
                        *tool = ToolState::default();
                        cx.notify();
                    });
                    if selection_changed {
                        canvas.update(cx, |canvas, cx| canvas.fit_to_screen(cx));
                    }
                }
            });
        if is_cell {
            let sidebar_state = self.state.clone();
            scope_name = scope_name.on_mouse_down(MouseButton::Right, move |event, window, cx| {
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
                        let state = self.state.clone();
                        move |_event, _window, cx| {
                            state.update(cx, |state, cx| {
                                if !state.expanded_scopes.insert(scope_path.clone()) {
                                    state.expanded_scopes.swap_remove(&scope_path);
                                }
                                state.rows_revision = state.rows_revision.wrapping_add(1);
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
                        let solved_cell = solved_cell_entity;
                        move |_event, _window, cx| {
                            solved_cell.update(cx, |state, cx| {
                                if let Some(state) = state.as_mut() {
                                    let visible = state.state[&scope_path].visible;
                                    Arc::make_mut(&mut state.state)
                                        .get_mut(&scope_path)
                                        .unwrap()
                                        .visible = !visible;
                                    cx.notify();
                                }
                            })
                        }
                    }),
            )
    }

    fn cached_rows(
        &mut self,
        cx: &mut Context<Self>,
        solved_cell: &CompileOutputState,
    ) -> (Arc<Vec<HierarchyRow>>, Option<usize>) {
        let rows_revision = self.state.read(cx).rows_revision;
        let filter = self.name_filter.read(cx).content.to_lowercase();
        let cache_is_current = self.rows_cache.as_ref().is_some_and(|cache| {
            Arc::ptr_eq(&cache.output, &solved_cell.output)
                && cache.rows_revision == rows_revision
                && cache.filter == filter
        });
        if !cache_is_current {
            let expanded_scopes = self.state.read(cx).expanded_scopes.clone();
            let mut rows = Vec::new();
            let root_scope = solved_cell.output.cells[&solved_cell.output.top].root;
            Self::collect_scope_rows(
                solved_cell,
                &expanded_scopes,
                &filter,
                &mut rows,
                ScopeAddress {
                    scope: root_scope,
                    cell: solved_cell.output.top,
                },
                1,
                0,
            );
            let widest_row = rows
                .iter()
                .enumerate()
                .max_by_key(|(_, row)| {
                    let name = &solved_cell.state[&solved_cell.scope_paths[&row.scope]].name;
                    let count_width = if row.count > 1 {
                        row.count.to_string().len() + 3
                    } else {
                        0
                    };
                    row.depth * 2 + name.chars().count() + count_width
                })
                .map(|(index, _)| index);
            self.rows_cache = Some(HierarchyRowsCache {
                output: solved_cell.output.clone(),
                rows_revision,
                filter,
                rows: Arc::new(rows),
                widest_row,
            });
        }
        let cache = self.rows_cache.as_ref().unwrap();
        (cache.rows.clone(), cache.widest_row)
    }

    fn render_scopes(&mut self, cx: &mut gpui::Context<Self>) -> impl gpui::IntoElement {
        let solved_cell = self.editor_state.read(cx).solved_cell.read(cx).clone();
        let (rows, widest_row) = if let Some(solved_cell) = solved_cell.as_ref() {
            self.cached_rows(cx, solved_cell)
        } else {
            self.rows_cache = None;
            (Arc::new(Vec::new()), None)
        };
        let row_count = rows.len();
        let rows_for_render = rows.clone();
        let solved_cell_for_render = solved_cell.clone();
        let editor_state = self.editor_state.read(cx);
        let theme = editor_state.theme();
        let font_size = editor_state.font_size;
        let list = uniform_list(
            "hierarchy_rows",
            row_count,
            cx.processor(move |sidebar, range: std::ops::Range<usize>, _window, cx| {
                let Some(solved_cell) = solved_cell_for_render.as_ref() else {
                    return Vec::new();
                };
                range
                    .filter_map(|index| rows_for_render.get(index).copied())
                    .map(|row| sidebar.render_scope_row(cx, solved_cell, row))
                    .collect::<Vec<_>>()
            }),
        )
        .with_width_from_item(widest_row)
        .with_horizontal_sizing_behavior(ListHorizontalSizingBehavior::Unconstrained)
        .track_scroll(self.list_scroll_handle.clone())
        .size_full()
        .min_h_0()
        .min_w_0();
        let mut scroll_area = sidebar_scroll_frame(
            "hierarchy_scroll_area",
            list,
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
            let menu_header = div()
                .px_2()
                .pb_1()
                .text_color(theme.subtext)
                .child(cell_name);
            let menu_header = if let Some(font_size) = font_size {
                menu_header.text_size(px(small_font_size(font_size)))
            } else {
                menu_header.text_xs()
            };
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
                .child(menu_header)
                .child(action);
            scroll_area = scroll_area.child(deferred(menu).with_priority(1));
        }
        scroll_area
    }

    fn render_navigation_overview(
        &self,
        theme: &'static Theme,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let layout_canvas = self.canvas.clone();
        let sidebar_state = self.state.clone();
        let owner = cx.entity_id();
        canvas(
            |_, _, _| (),
            move |bounds, _, window, cx| {
                let snapshot = layout_canvas.read(cx).navigation_overview_snapshot(bounds);
                window.paint_layer(bounds, |window| {
                    window.paint_quad(fill(bounds, theme.bg));
                });
                if let Some(snapshot) = &snapshot {
                    window.paint_layer(bounds, |window| {
                        window
                            .paint_image(
                                snapshot.image_bounds,
                                Corners::all(px(0.)),
                                snapshot.image.clone(),
                                0,
                                false,
                            )
                            .unwrap();
                    });
                }
                window.paint_layer(bounds, |window| {
                    window.paint_quad(quad(
                        bounds,
                        Corners::all(px(0.)),
                        transparent_black(),
                        Edges::all(px(1.)),
                        theme.text,
                        Edges::all(BorderStyle::Solid),
                    ));
                });
                if let Some(snapshot) = &snapshot {
                    window.paint_layer(bounds, |window| {
                        window.paint_quad(quad(
                            snapshot.viewport_bounds,
                            Corners::all(px(0.)),
                            transparent_black(),
                            Edges::all(px(4.)),
                            theme.bg,
                            Edges::all(BorderStyle::Solid),
                        ));
                    });
                    window.paint_layer(bounds, |window| {
                        window.paint_quad(quad(
                            snapshot.viewport_bounds,
                            Corners::all(px(0.)),
                            transparent_black(),
                            Edges::all(px(2.)),
                            theme.text,
                            Edges::all(BorderStyle::Solid),
                        ));
                    });
                }
                if let Some(drag) = sidebar_state.read(cx).preview_drag
                    && navigation_overview_drag_is_box(drag)
                {
                    window.paint_layer(bounds, |window| {
                        window.paint_quad(quad(
                            navigation_overview_drag_bounds(drag),
                            Corners::all(px(0.)),
                            theme.selection,
                            Edges::all(px(2.)),
                            theme.axes,
                            Edges::all(BorderStyle::Solid),
                        ));
                    });
                }

                window.on_mouse_event({
                    let sidebar_state = sidebar_state.clone();
                    let snapshot = snapshot.clone();
                    move |event: &MouseDownEvent, phase, window, cx| {
                        if phase != DispatchPhase::Bubble
                            || event.button != MouseButton::Left
                            || !bounds.contains(&event.position)
                        {
                            return;
                        }
                        if snapshot.is_none() {
                            return;
                        }
                        window.prevent_default();
                        cx.stop_propagation();
                        let position = clamp_point_to_bounds(event.position, bounds);
                        sidebar_state.update(cx, |state, _cx| {
                            state.preview_drag = Some(NavigationOverviewDrag {
                                start: position,
                                current: position,
                            });
                        });
                        cx.notify(owner);
                    }
                });
                window.on_mouse_event({
                    let sidebar_state = sidebar_state.clone();
                    move |event: &MouseMoveEvent, phase, window, cx| {
                        if phase != DispatchPhase::Capture || !event.dragging() {
                            return;
                        }
                        if sidebar_state.read(cx).preview_drag.is_none() {
                            return;
                        }
                        window.prevent_default();
                        cx.stop_propagation();
                        let position = clamp_point_to_bounds(event.position, bounds);
                        sidebar_state.update(cx, |state, _cx| {
                            if let Some(drag) = &mut state.preview_drag {
                                drag.current = position;
                            }
                        });
                        cx.notify(owner);
                    }
                });
                window.on_mouse_event({
                    let layout_canvas = layout_canvas.clone();
                    let sidebar_state = sidebar_state.clone();
                    let snapshot = snapshot.clone();
                    move |event: &MouseUpEvent, phase, window, cx| {
                        if phase != DispatchPhase::Capture || event.button != MouseButton::Left {
                            return;
                        }
                        let Some(mut drag) = sidebar_state.read(cx).preview_drag else {
                            return;
                        };
                        drag.current = clamp_point_to_bounds(event.position, bounds);
                        sidebar_state.update(cx, |state, _cx| {
                            state.preview_drag = None;
                        });
                        window.prevent_default();
                        cx.stop_propagation();
                        cx.notify(owner);

                        let Some(snapshot) = &snapshot else {
                            return;
                        };
                        if navigation_overview_drag_is_box(drag) {
                            let first = snapshot.world_at(drag.start);
                            let second = snapshot.world_at(drag.current);
                            layout_canvas.update(cx, |canvas, cx| {
                                canvas.fit_viewport_to_world_bounds(first, second, cx);
                            });
                        } else {
                            let world = snapshot.world_at(drag.current);
                            layout_canvas.update(cx, |canvas, cx| {
                                canvas.center_viewport_on(world, cx);
                            });
                        }
                    }
                });
            },
        )
        .size_full()
    }
}

impl Render for HierarchySideBar {
    fn render(
        &mut self,
        _window: &mut gpui::Window,
        cx: &mut gpui::Context<Self>,
    ) -> impl gpui::IntoElement {
        let editor_state = self.editor_state.read(cx);
        let theme = editor_state.theme();
        let width = self.state.read(cx).width;
        let preview_height = self.state.read(cx).preview_height;
        let icon_wh = editor_state.icon_size.unwrap_or(16.);
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
                                            for state in Arc::make_mut(&mut cell.state).values_mut()
                                            {
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
                                            for state in Arc::make_mut(&mut cell.state).values_mut()
                                            {
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
                                        state.rows_revision = state.rows_revision.wrapping_add(1);
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
                                        state.rows_revision = state.rows_revision.wrapping_add(1);
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
                    .id("hierarchy_preview_resize_handle")
                    .w_full()
                    .h(px(HIERARCHY_PREVIEW_RESIZE_HANDLE_HEIGHT))
                    .flex_shrink_0()
                    .border_t_1()
                    .border_color(theme.divider)
                    .cursor_row_resize()
                    .on_mouse_down(MouseButton::Left, |_event, _window, cx| {
                        cx.stop_propagation();
                    })
                    .on_drag(HierarchyPreviewResize, |_, _, _window, cx| {
                        cx.new(|_cx| Empty)
                    }),
            )
            .child(
                div()
                    .id("hierarchy_navigation_overview")
                    .h(preview_height)
                    .w_full()
                    .flex_shrink_0()
                    .flex()
                    .flex_col()
                    .min_h_0()
                    .overflow_hidden()
                    .child(
                        div()
                            .flex_shrink_0()
                            .w_full()
                            .pb_1()
                            .text_color(theme.subtext)
                            .child("Overview"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .w_full()
                            .cursor_pointer()
                            .child(self.render_navigation_overview(theme, cx)),
                    ),
            )
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
            .on_drag_move::<HierarchyPreviewResize>({
                let state = self.state.clone();
                move |event, _window, cx| {
                    let height = clamp_hierarchy_preview_height(
                        event.bounds.bottom() - event.event.position.y,
                        event.bounds.size.height,
                    );
                    state.update(cx, |state, cx| {
                        if state.preview_height != height {
                            state.preview_height = height;
                            cx.notify();
                        }
                    });
                }
            })
    }
}

#[cfg(test)]
mod sidebar_layout_tests {
    use gpui::{Point, px};

    use super::{
        MAX_SIDEBAR_WIDTH, MIN_HIERARCHY_PANEL_HEIGHT, MIN_HIERARCHY_PREVIEW_HEIGHT,
        MIN_SIDEBAR_WIDTH, NavigationOverviewDrag, clamp_hierarchy_preview_height,
        clamp_sidebar_width, navigation_overview_drag_bounds, navigation_overview_drag_is_box,
        scrollbar_thumb_metrics,
    };

    #[test]
    fn sidebar_width_is_limited_to_usable_bounds() {
        assert_eq!(clamp_sidebar_width(px(80.)), px(MIN_SIDEBAR_WIDTH));
        assert_eq!(clamp_sidebar_width(px(320.)), px(320.));
        assert_eq!(clamp_sidebar_width(px(900.)), px(MAX_SIDEBAR_WIDTH));
    }

    #[test]
    fn hierarchy_preview_height_preserves_space_for_the_tree() {
        assert_eq!(
            clamp_hierarchy_preview_height(px(40.), px(600.)),
            px(MIN_HIERARCHY_PREVIEW_HEIGHT)
        );
        assert_eq!(clamp_hierarchy_preview_height(px(300.), px(600.)), px(300.));
        assert_eq!(
            clamp_hierarchy_preview_height(px(900.), px(600.)),
            px(600. - MIN_HIERARCHY_PANEL_HEIGHT)
        );
        assert_eq!(clamp_hierarchy_preview_height(px(200.), px(100.)), px(0.));
    }

    #[test]
    fn navigation_overview_distinguishes_clicks_from_box_drags() {
        let click = NavigationOverviewDrag {
            start: Point::new(px(20.), px(30.)),
            current: Point::new(px(23.), px(33.)),
        };
        let box_drag = NavigationOverviewDrag {
            start: Point::new(px(80.), px(90.)),
            current: Point::new(px(20.), px(30.)),
        };

        assert!(!navigation_overview_drag_is_box(click));
        assert!(navigation_overview_drag_is_box(box_drag));
        let bounds = navigation_overview_drag_bounds(box_drag);
        assert_eq!(bounds.origin, Point::new(px(20.), px(30.)));
        assert_eq!(bounds.size.width, px(60.));
        assert_eq!(bounds.size.height, px(60.));
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
