//! What the window shows before a cell is open.

use analyzer::rpc::{CellEntry, RecentCell};
use gpui::{prelude::FluentBuilder, *};

use crate::{
    actions::{NewCellCommand, OpenCellCommand},
    editor::{Editor, toolbars::binding_text},
    theme::Theme,
};

/// The editor's key context while the start screen is shown.
pub(crate) const START_SCREEN_CONTEXT: &str = "StartScreen";

const START_SCREEN_WIDTH: f32 = 440.;
const BUTTON_ICON_SIZE: f32 = 16.;
/// The height the library's cells keep when recent cells crowd them.
const MIN_LIBRARY_HEIGHT: f32 = 128.;

fn start_button(
    id: &'static str,
    icon: &'static str,
    label: &'static str,
    hotkey: Option<SharedString>,
    theme: &'static Theme,
) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .px_3()
        .py_2()
        .rounded_md()
        .border_1()
        .border_color(theme.divider)
        .bg(theme.input_bg)
        .cursor_pointer()
        .hover(|style| style.bg(theme.selection))
        .child(
            svg()
                .path(icon)
                .w(px(BUTTON_ICON_SIZE))
                .h(px(BUTTON_ICON_SIZE))
                .text_color(theme.text),
        )
        .child(label)
        .children(hotkey.map(|hotkey| div().text_color(theme.subtext).child(hotkey)))
}

/// A row of a start screen list, selected in tests as `{id}_{index}`.
fn cell_row(id: &'static str, index: usize, theme: &'static Theme) -> Stateful<Div> {
    div()
        .id((id, index))
        .debug_selector(move || format!("{id}_{index}"))
        .flex()
        .flex_row()
        .flex_none()
        .gap_1()
        .px_2()
        .py_1()
        .rounded_sm()
        .overflow_hidden()
        .cursor_pointer()
        .hover(|style| style.bg(theme.selection))
}

impl Editor {
    /// Buttons to open or create a cell, above the recently opened cells and
    /// the library's cells.
    pub(crate) fn render_start_screen(
        &self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let state = self.state.read(cx);
        let theme = state.theme();
        let font_size = state.font_size;
        let cells = state.cells.clone();
        let recent_cells = state.recent_cells.clone();
        let mut context = KeyContext::new_with_defaults();
        context.add(START_SCREEN_CONTEXT);
        let hotkey = |action: &dyn Action| binding_text(action, window, context.clone());

        let heading = div().text_color(theme.text).child("No cell open");
        let heading = match font_size {
            Some(font_size) => heading.text_size(px(font_size * 1.4)),
            None => heading.text_xl(),
        };
        let buttons = div()
            .flex()
            .flex_row()
            .flex_wrap()
            .gap_2()
            .child(
                start_button(
                    "start_open_cell",
                    "icons/folder-open.svg",
                    "Open cell…",
                    hotkey(&OpenCellCommand),
                    theme,
                )
                .on_click(|_, window, cx| window.dispatch_action(Box::new(OpenCellCommand), cx)),
            )
            .child(
                start_button(
                    "start_new_cell",
                    "icons/file-circle-plus.svg",
                    "New cell…",
                    hotkey(&NewCellCommand),
                    theme,
                )
                .on_click(|_, window, cx| window.dispatch_action(Box::new(NewCellCommand), cx)),
            );
        // The recent cells shrink and scroll when the window is too short for
        // both lists.
        let recents = (!recent_cells.is_empty()).then(|| {
            let rows = recent_cells.into_iter().enumerate().map(|(index, recent)| {
                // Without the library's cells, nothing is known to be missing.
                let available = cells
                    .as_ref()
                    .is_none_or(|cells| cells.iter().any(|cell| cell.path == recent.cell));
                cell_row("start_recent", index, theme)
                    .text_color(if available { theme.text } else { theme.subtext })
                    .child(div().min_w_0().truncate().child(recent.invocation.clone()))
                    .on_click(cx.listener(move |editor, _, _, cx| {
                        editor.open_recent_cell(&recent, cx);
                    }))
            });
            div()
                .debug_selector(|| "start_recents".to_owned())
                .min_h_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(div().text_color(theme.subtext).child("Recently opened"))
                .child(
                    div()
                        .id("start_recents")
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .overflow_y_scroll()
                        .children(rows),
                )
        });
        let cell_list = cells.map(|cells| {
            let section = div()
                .flex_1()
                .min_h(px(MIN_LIBRARY_HEIGHT))
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_color(theme.subtext)
                        .child("Cells in this library"),
                );
            if cells.is_empty() {
                return section.child(
                    div()
                        .px_2()
                        .py_1()
                        .text_color(theme.subtext)
                        .child("None yet. Create one to get started."),
                );
            }
            section.child(
                div()
                    .id("start_cells")
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .overflow_y_scroll()
                    .children(cells.into_iter().enumerate().map(|(index, cell)| {
                        let parameters = (!cell.parameters.is_empty())
                            .then(|| format!("({})", cell.parameters.join(", ")));
                        cell_row("start_cell", index, theme)
                            .child(div().flex_none().child(cell.path.clone()))
                            .when_some(parameters, |row, parameters| {
                                row.child(
                                    div()
                                        .min_w_0()
                                        .overflow_hidden()
                                        .text_ellipsis()
                                        .text_color(theme.subtext)
                                        .child(parameters),
                                )
                            })
                            .on_click(cx.listener(move |editor, _, _, cx| {
                                editor.open_library_cell(&cell, cx);
                            }))
                    })),
            )
        });

        div()
            .id("start_screen")
            .debug_selector(|| "start_screen".to_owned())
            .flex_1()
            .min_h_0()
            .flex()
            .flex_col()
            .items_center()
            .px_4()
            .bg(theme.bg)
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .w(px(START_SCREEN_WIDTH))
                    .max_w_full()
                    .flex()
                    .flex_col()
                    .gap_4()
                    .pt(px(72.))
                    .pb_4()
                    .child(
                        div().flex().flex_col().gap_1().child(heading).child(
                            div()
                                .text_color(theme.subtext)
                                .child("Open a cell or create a new one to start editing."),
                        ),
                    )
                    .child(buttons)
                    .children(recents)
                    .children(cell_list),
            )
            .into_any_element()
    }

    /// Opens `cell` if it needs no arguments; otherwise asks for them at the
    /// invoking editor's command line.
    fn open_library_cell(&mut self, cell: &CellEntry, cx: &mut Context<Self>) {
        if cell.requires_arguments {
            self.open_invoking_command(Some(&format!("Argon openCell {}(", cell.path)), true, cx);
        } else {
            let _ = self
                .state
                .read(cx)
                .lang_server_client
                .open_cell(format!("{}()", cell.path));
        }
    }

    /// Reopens `recent` exactly as it was invoked.
    fn open_recent_cell(&self, recent: &RecentCell, cx: &mut Context<Self>) {
        let _ = self
            .state
            .read(cx)
            .lang_server_client
            .open_cell(recent.invocation.clone());
    }
}
