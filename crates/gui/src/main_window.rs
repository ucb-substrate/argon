//! The window root: the title bar, the Neovim pane, and the editor.

use std::{any::Any, net::SocketAddr, process::ExitStatus};

use analyzer::transport::SessionToken;
use futures::{StreamExt, channel::mpsc::UnboundedReceiver};
use gpui::{
    App, Context, CursorStyle, Entity, FocusHandle, Focusable, Global, MouseButton, MouseMoveEvent,
    Pixels, Render, SharedString, Subscription, Task, Window, WindowHandle, div, prelude::*, px,
};

use crate::{
    actions::{
        FocusInvoker, NewCellCommand, Quit, RenameCellCommand, Save, ShowDiagnostics, ShowMessages,
        ToggleNeovim,
    },
    editor::{Editor, EditorState, EditorTasks, Invoker, toolbars::placeholder_title_bar},
    editor_window_options,
    nvim::{NvimPane, PaneEvent},
    theme::DARK_THEME,
};

/// The pane's default share of the window's width.
const DEFAULT_SPLIT: f32 = 0.4;
const DIVIDER_WIDTH: Pixels = px(4.);
const MIN_EDITOR_WIDTH: Pixels = px(320.);
/// The fewest columns the pane keeps while the divider is dragged.
const MIN_PANE_COLUMNS: f32 = 20.;
/// The cell width assumed before the pane has been painted.
const FALLBACK_CELL_WIDTH: Pixels = px(8.);

/// An analyzer's RPC address and session token, or why none arrived.
pub(crate) type Announcement = Result<(SocketAddr, SessionToken), String>;

/// Where analyzer announcements come from. Dropping it releases what it
/// owns, such as the relay socket's directory or SSH processes.
pub(crate) struct AnnouncementSource {
    receiver: Option<UnboundedReceiver<Announcement>>,
    _resources: Box<dyn Any>,
}

impl AnnouncementSource {
    pub(crate) fn new(receiver: UnboundedReceiver<Announcement>, resources: impl Any) -> Self {
        Self {
            receiver: Some(receiver),
            _resources: Box::new(resources),
        }
    }

    /// Binds `analyzer.sock` in a new private directory and accepts
    /// announcements on it for the life of the process.
    #[cfg(unix)]
    pub(crate) fn bind_relay() -> std::io::Result<(Self, std::path::PathBuf)> {
        let directory = tempfile::Builder::new()
            .prefix("argone-")
            .tempdir_in(crate::cli::short_temp_root())?;
        let path = directory.path().join("analyzer.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path)?;
        let (sender, receiver) = futures::channel::mpsc::unbounded();
        std::thread::spawn(move || serve_relay(listener, sender));
        Ok((Self::new(receiver, directory), path))
    }
}

/// Forwards each analyzer's `PORT TOKEN` line until the receiver is gone.
#[cfg(unix)]
fn serve_relay(
    listener: std::os::unix::net::UnixListener,
    sender: futures::channel::mpsc::UnboundedSender<Announcement>,
) {
    use std::io::{BufRead, BufReader};

    const READ_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(error) => {
                tracing::warn!("could not accept an analyzer announcement: {error}");
                continue;
            }
        };
        let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
        let mut line = String::new();
        if let Err(error) = BufReader::new(stream).read_line(&mut line) {
            tracing::warn!("could not read an analyzer announcement: {error}");
            continue;
        }
        let Some((port, token)) = analyzer::transport::parse_announcement(&line) else {
            tracing::warn!("ignored an invalid analyzer announcement");
            continue;
        };
        let addr = SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port));
        if sender.unbounded_send(Ok((addr, token))).is_err() {
            return;
        }
    }
}

/// The window of `argone gui`, kept so that it can be reopened.
struct StandaloneWindow(Entity<MainWindow>);

impl Global for StandaloneWindow {}

/// Reopens the window of `argone gui` if it was closed.
pub(crate) fn reopen_standalone(editor: &Editor, cx: &mut App) {
    if !cx.windows().is_empty() {
        return;
    }
    let Some(main) = cx
        .try_global::<StandaloneWindow>()
        .map(|main| main.0.clone())
    else {
        return;
    };
    let editor = editor.clone();
    let _ = cx.open_window(editor_window_options(), |window, cx| {
        editor.focus_canvas(window, cx);
        main
    });
}

/// The pane's share of `width`, leaving the pane `min_pane` and the editor
/// its minimum width. The pane wins when both do not fit.
fn clamp_split(split: f32, width: Pixels, min_pane: Pixels) -> f32 {
    let width = f32::from(width).max(1.);
    let max = (width - f32::from(MIN_EDITOR_WIDTH) - f32::from(DIVIDER_WIDTH)) / width;
    let min = f32::from(min_pane) / width;
    split.min(max).max(min).clamp(0., 1.)
}

/// The root of a GUI window.
pub(crate) struct MainWindow {
    /// Focused when neither the pane nor a canvas can be.
    focus_handle: FocusHandle,
    editor: Option<(Entity<Editor>, EditorTasks)>,
    nvim: Option<Entity<NvimPane>>,
    nvim_visible: bool,
    /// The pane's share of the width.
    split: f32,
    dragging_divider: bool,
    _announcements: Option<AnnouncementSource>,
    /// Shown in place of the editor until there is one.
    status: Option<SharedString>,
    /// Shown in place of the pane once Neovim has exited unsuccessfully.
    nvim_exit: Option<SharedString>,
    editor_state: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
    _announcement_task: Option<Task<()>>,
}

impl MainWindow {
    fn empty(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            editor: None,
            nvim: None,
            nvim_visible: true,
            split: DEFAULT_SPLIT,
            dragging_divider: false,
            _announcements: None,
            status: None,
            nvim_exit: None,
            editor_state: None,
            _subscriptions: Vec::new(),
            _announcement_task: None,
        }
    }

    /// The window of `argone gui`, connected to the analyzer at `addr`.
    pub(crate) fn standalone(
        addr: SocketAddr,
        token: SessionToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut main = Self::empty(cx);
        main.connect(addr, token, window, cx);
        main
    }

    /// Keeps this window for [`reopen_standalone`].
    pub(crate) fn keep_standalone(main: &Entity<Self>, cx: &mut App) {
        cx.set_global(StandaloneWindow(main.clone()));
    }

    /// A window with the Neovim pane, which connects to each analyzer that
    /// `announcements` reports.
    pub(crate) fn embedded(
        pane: Entity<NvimPane>,
        mut announcements: AnnouncementSource,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let receiver = announcements.receiver.take();
        let task = cx.spawn_in(window, async move |main, cx| {
            let Some(mut receiver) = receiver else {
                return;
            };
            while let Some(announcement) = receiver.next().await {
                let announced = main.update_in(cx, |main, window, cx| {
                    main.announced(announcement, window, cx)
                });
                if announced.is_err() {
                    break;
                }
            }
        });
        let subscriptions = vec![
            cx.subscribe_in(&pane, window, |main, _, event, window, cx| match event {
                PaneEvent::Exited(status) => main.nvim_exited(*status, window, cx),
            }),
            cx.on_app_quit(|main, _| {
                main._announcements.take();
                async {}
            }),
        ];
        window.focus(&pane.focus_handle(cx));
        Self {
            nvim: Some(pane),
            _announcements: Some(announcements),
            status: Some("Starting the analyzer…".into()),
            _subscriptions: subscriptions,
            _announcement_task: Some(task),
            ..Self::empty(cx)
        }
    }

    fn announced(
        &mut self,
        announcement: Announcement,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match announcement {
            Ok((addr, token)) => self.connect(addr, token, window, cx),
            Err(error) => {
                self.status = Some(error.into());
                cx.notify();
            }
        }
    }

    /// Replaces the editor with one connected to the analyzer at `addr`.
    fn connect(
        &mut self,
        addr: SocketAddr,
        token: SessionToken,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let invoker = match self.nvim {
            Some(_) => Invoker::Pane {
                window: window.window_handle(),
                main: cx.weak_entity(),
            },
            None => Invoker::App,
        };
        let pane_focused = self
            .pane_focus(cx)
            .is_some_and(|focus| focus.contains_focused(window, cx));
        self.editor = None;
        self.editor_state = None;
        match Editor::new(cx, window, addr, token, invoker) {
            Ok((editor, tasks)) => {
                if self.nvim.is_some() && !pane_focused {
                    editor.focus_canvas(window, cx);
                }
                let state = editor.state.clone();
                self.editor_state = Some(cx.observe(&state, |main, state, cx| {
                    main.copy_editor_settings(&state, cx)
                }));
                self.editor = Some((cx.new(|_| editor), tasks));
                self.copy_editor_settings(&state, cx);
                self.status = None;
            }
            Err(error) => {
                tracing::error!("could not connect to the analyzer at {addr}: {error:#}");
                self.status = Some(format!("Could not connect to the analyzer: {error:#}").into());
            }
        }
        cx.notify();
    }

    fn copy_editor_settings(&mut self, state: &Entity<EditorState>, cx: &mut Context<Self>) {
        let option_as_meta = state.read(cx).option_as_meta;
        if let Some(pane) = &self.nvim {
            pane.update(cx, |pane, _| pane.option_as_meta = option_as_meta);
        }
    }

    fn pane_focus(&self, cx: &App) -> Option<FocusHandle> {
        self.nvim.as_ref().map(|pane| pane.focus_handle(cx))
    }

    /// Shows the pane and focuses it.
    pub(crate) fn focus_nvim(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.nvim_exit.is_some() {
            return;
        }
        let Some(focus) = self.pane_focus(cx) else {
            return;
        };
        self.nvim_visible = true;
        window.focus(&focus);
        cx.notify();
    }

    /// Focuses the canvas, or the window itself when there is no editor.
    fn focus_canvas(&self, window: &mut Window, cx: &App) {
        match &self.editor {
            Some((editor, _)) => editor.read(cx).focus_canvas(window, cx),
            None => window.focus(&self.focus_handle),
        }
    }

    /// Makes every Quit go through [`Self::request_quit`] of `window`, even
    /// when the window isn't active.
    pub(crate) fn route_quit(window: WindowHandle<Self>, cx: &mut App) {
        cx.on_action(move |_: &Quit, cx| {
            // Deferred so that this also works while `window` is being updated.
            cx.defer(move |cx| {
                if window
                    .update(cx, |main, window, cx| main.request_quit(window, cx))
                    .is_err()
                {
                    cx.quit();
                }
            });
        });
    }

    /// Asks Neovim to quit, or quits at once if it has exited. Returns
    /// whether the window may close.
    pub(crate) fn request_quit(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        match &self.nvim {
            Some(pane) if self.nvim_exit.is_none() => {
                let client = pane.read(cx).client.clone();
                self.focus_nvim(window, cx);
                client.input("<C-\\><C-N>:confirm qa<CR>");
                false
            }
            _ => {
                cx.quit();
                true
            }
        }
    }

    fn nvim_exited(&mut self, status: ExitStatus, window: &mut Window, cx: &mut Context<Self>) {
        if status.success() {
            cx.quit();
            return;
        }
        let message = match status.code() {
            Some(code) => format!("Neovim exited with status {code}"),
            None => format!("Neovim exited ({status})"),
        };
        tracing::error!("{message}");
        self.nvim_exit = Some(message.into());
        self.nvim_visible = true;
        self.focus_canvas(window, cx);
        cx.notify();
    }

    fn toggle_nvim(&mut self, _: &ToggleNeovim, window: &mut Window, cx: &mut Context<Self>) {
        if self.nvim.is_none() {
            return;
        }
        if self.nvim_visible {
            self.nvim_visible = false;
            self.focus_canvas(window, cx);
        } else {
            self.nvim_visible = true;
            self.focus_nvim(window, cx);
        }
        cx.notify();
    }

    fn focus_invoker(&mut self, _: &FocusInvoker, window: &mut Window, cx: &mut Context<Self>) {
        self.focus_canvas(window, cx);
    }

    /// Runs `f` on the editor, if there is one.
    fn with_editor(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
        f: impl FnOnce(&mut Editor, &mut Window, &mut Context<Editor>),
    ) {
        if let Some((editor, _)) = &self.editor {
            editor.update(cx, |editor, cx| f(editor, window, cx));
        }
    }

    fn save(&mut self, action: &Save, window: &mut Window, cx: &mut Context<Self>) {
        self.with_editor(window, cx, |editor, window, cx| {
            editor.on_save(action, window, cx)
        });
    }

    fn show_diagnostics(
        &mut self,
        action: &ShowDiagnostics,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_editor(window, cx, |editor, window, cx| {
            editor.show_diagnostics(action, window, cx)
        });
    }

    fn show_messages(
        &mut self,
        action: &ShowMessages,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_editor(window, cx, |editor, window, cx| {
            editor.show_messages(action, window, cx)
        });
    }

    fn new_cell(&mut self, action: &NewCellCommand, window: &mut Window, cx: &mut Context<Self>) {
        self.with_editor(window, cx, |editor, window, cx| {
            editor.new_cell_command(action, window, cx)
        });
    }

    fn rename_cell(
        &mut self,
        action: &RenameCellCommand,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.with_editor(window, cx, |editor, window, cx| {
            editor.rename_cell_command(action, window, cx)
        });
    }

    fn min_pane_width(&self, cx: &App) -> Pixels {
        let cell_width = self
            .nvim
            .as_ref()
            .and_then(|pane| pane.read(cx).layout)
            .map_or(FALLBACK_CELL_WIDTH, |layout| layout.cell.width);
        cell_width * MIN_PANE_COLUMNS
    }

    fn drag_divider(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.dragging_divider {
            return;
        }
        if event.pressed_button != Some(MouseButton::Left) {
            self.dragging_divider = false;
            return;
        }
        let width = window.viewport_size().width;
        self.split = clamp_split(event.position.x / width, width, self.min_pane_width(cx));
        cx.notify();
    }
}

impl Render for MainWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let state = self
            .editor
            .as_ref()
            .map(|(editor, _)| editor.read(cx).state.clone());
        let (theme, font_size) = match &state {
            Some(state) => {
                let state = state.read(cx);
                (state.theme(), state.font_size)
            }
            None => (&*DARK_THEME, None),
        };
        let title_bar = match &self.editor {
            Some((editor, _)) => editor.read(cx).title_bar.clone().into_any_element(),
            None => placeholder_title_bar(theme).into_any_element(),
        };
        let message = |text: SharedString| {
            div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .text_color(theme.subtext)
                .whitespace_normal()
                .child(text)
        };
        let editor_slot = div().flex_1().min_w_0().h_full().child(match &self.editor {
            Some((editor, _)) => editor.clone().into_any_element(),
            None => message(self.status.clone().unwrap_or_default()).into_any_element(),
        });

        let mut body = div().flex().flex_row().flex_1().min_h_0();
        if let Some(pane) = self.nvim.as_ref().filter(|_| self.nvim_visible) {
            let viewport = window.viewport_size().width;
            let width = viewport * clamp_split(self.split, viewport, self.min_pane_width(cx));
            let pane = match &self.nvim_exit {
                Some(exit) => message(exit.clone()).into_any_element(),
                None => pane.clone().into_any_element(),
            };
            body = body
                .child(
                    div()
                        .w(width)
                        .h_full()
                        .flex_none()
                        .overflow_hidden()
                        .child(pane),
                )
                .child(
                    div()
                        .id("nvim_divider")
                        .w(DIVIDER_WIDTH)
                        .h_full()
                        .flex_none()
                        .bg(theme.divider)
                        .cursor(CursorStyle::ResizeLeftRight)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|main, _, _, cx| {
                                main.dragging_divider = true;
                                cx.stop_propagation();
                            }),
                        ),
                );
        }
        body = body.child(editor_slot);

        let root = div()
            .id("main_window")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::focus_invoker))
            .on_action(cx.listener(Self::save))
            .on_action(cx.listener(Self::show_diagnostics))
            .on_action(cx.listener(Self::show_messages))
            .on_action(cx.listener(Self::new_cell))
            .on_action(cx.listener(Self::rename_cell))
            .on_action(cx.listener(Self::toggle_nvim))
            .on_mouse_move(cx.listener(Self::drag_divider))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|main, _, _, _| main.dragging_divider = false),
            )
            .font_family("Zed Plex Sans")
            .size_full()
            .flex()
            .flex_col()
            .border_1()
            .border_color(theme.divider)
            .bg(theme.bg)
            .rounded(px(10.))
            .text_color(theme.text)
            .overflow_hidden()
            .whitespace_nowrap()
            .child(title_bar)
            .child(body);
        match font_size {
            Some(font_size) => root.text_size(px(font_size)),
            None => root.text_sm(),
        }
    }
}

#[cfg(test)]
mod tests {
    use gpui::{TestAppContext, px};

    use super::*;
    use crate::nvim::client::NvimClient;

    #[gpui::test]
    fn quitting_asks_neovim_and_hiding_it_keeps_the_window_usable(cx: &mut TestAppContext) {
        let (client, messages) = NvimClient::for_test();
        let (_events, events) = futures::channel::mpsc::unbounded();
        let (_announcer, announcements) = futures::channel::mpsc::unbounded();
        let window = cx.update(|cx| {
            cx.bind_keys(crate::key_bindings(true));
            let window = cx
                .open_window(Default::default(), |window, cx| {
                    let pane = cx.new(|cx| NvimPane::new(client, events, window, cx));
                    let announcements = AnnouncementSource::new(announcements, ());
                    cx.new(|cx| MainWindow::embedded(pane, announcements, window, cx))
                })
                .unwrap();
            MainWindow::route_quit(window, cx);
            window
        });
        cx.run_until_parked();
        messages.try_iter().for_each(drop);

        // The window isn't active, so only global handlers see this.
        cx.update(|cx| cx.dispatch_action(&Quit));
        cx.run_until_parked();
        let inputs = messages
            .try_iter()
            .map(|bytes| rmpv::decode::read_value(&mut bytes.as_slice()).unwrap())
            .filter(|message| message[1].as_str() == Some("nvim_input"))
            .map(|message| message[2][0].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(inputs, ["<C-\\><C-N>:confirm qa<CR>"]);

        // Without an editor, hiding Neovim must leave a way to show it again.
        cx.dispatch_action(window.into(), ToggleNeovim);
        assert!(!window.update(cx, |main, _, _| main.nvim_visible).unwrap());
        cx.dispatch_action(window.into(), ToggleNeovim);
        assert!(window.update(cx, |main, _, _| main.nvim_visible).unwrap());
    }

    #[test]
    fn the_divider_keeps_both_sides_usable() {
        let width = px(1000.);
        let min_pane = px(160.);
        assert_eq!(clamp_split(0.4, width, min_pane), 0.4);
        assert_eq!(clamp_split(0.05, width, min_pane), 0.16);
        assert_eq!(clamp_split(0.95, width, min_pane), 0.676);
        assert_eq!(
            clamp_split(0.5, px(400.), min_pane),
            0.4,
            "the pane keeps its columns in a narrow window"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_relay_forwards_announcements() {
        use std::io::Write;

        let (mut source, path) = AnnouncementSource::bind_relay().unwrap();
        let mut receiver = source.receiver.take().unwrap();
        let token = SessionToken::generate().unwrap();
        let mut stream = std::os::unix::net::UnixStream::connect(&path).unwrap();
        writeln!(stream, "1234 {}", token.to_hex()).unwrap();
        drop(stream);

        let announcement = futures::executor::block_on(receiver.next()).unwrap();
        assert_eq!(
            announcement,
            Ok((SocketAddr::from(([127, 0, 0, 1], 1234)), token))
        );
        let directory = path.parent().unwrap().to_owned();
        drop(source);
        assert!(
            !directory.exists(),
            "dropping the source removes its directory"
        );
    }
}
