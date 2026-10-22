//! The GUI running in a browser page, which relays its analyzer connections
//! and holds the editor it hands focus to.

pub mod host;

use gpui::{
    App, Context, Entity, IntoElement, Render, SharedString, Window, WindowBounds, WindowOptions,
    div, prelude::*, px, size,
};
use wasm_bindgen::prelude::*;

use crate::{
    editor::{Editor, EditorTasks, Invoker},
    rpc::Endpoint,
    theme::DARK_THEME,
};

/// Starts the GUI. It posts messages to the page through `post`, and the
/// page hands it messages through [`receive`].
#[wasm_bindgen]
pub fn start(post: js_sys::Function) {
    gpui_platform::web_init();
    host::set_post(post);
    crate::run_web(|window, cx| cx.new(|cx| WebRoot::new(window, cx)));
}

/// Handles one message from the page.
#[wasm_bindgen]
pub fn receive(message: JsValue) {
    host::receive(message);
}

/// The window root in the browser: the editor, or why there is none.
pub(crate) struct WebRoot {
    editor: Option<(Entity<Editor>, EditorTasks)>,
    status: SharedString,
}

impl WebRoot {
    fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let invoker = Invoker::Host {
            window: window.window_handle(),
        };
        match Editor::new(cx, window, Endpoint, invoker) {
            Ok((editor, tasks)) => {
                editor.focus_canvas(window, cx);
                Self {
                    editor: Some((cx.new(|_| editor), tasks)),
                    status: SharedString::default(),
                }
            }
            Err(error) => Self {
                editor: None,
                status: format!("Could not start the editor: {error:#}").into(),
            },
        }
    }
}

impl Render for WebRoot {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (theme, font_size) = match &self.editor {
            Some((editor, _)) => {
                let state = editor.read(cx).state.read(cx);
                (state.theme(), state.font_size)
            }
            None => (&*DARK_THEME, None),
        };
        let root = div()
            .font_family(crate::assets::SANS_FAMILY)
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.bg)
            .text_color(theme.text)
            .overflow_hidden()
            .whitespace_nowrap();
        let root = match &self.editor {
            Some((editor, _)) => root.child(editor.clone()),
            None => root
                .items_center()
                .justify_center()
                .text_color(theme.subtext)
                .child(self.status.clone()),
        };
        match font_size {
            Some(font_size) => root.text_size(px(font_size)),
            None => root.text_sm(),
        }
    }
}

/// The window the browser GUI opens, filling the page.
pub(crate) fn window_options(cx: &App) -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(gpui::Bounds::centered(
            None,
            size(px(1280.), px(800.)),
            cx,
        ))),
        focus: true,
        ..WindowOptions::default()
    }
}
