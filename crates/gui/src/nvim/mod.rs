//! A Neovim pane, drawn from Neovim's UI protocol.

pub(crate) mod client;
mod element;
pub(crate) mod keys;
pub(crate) mod redraw;
pub(crate) mod ui;

use std::{ops::Range, process::ExitStatus};

use futures::{StreamExt, channel::mpsc::UnboundedReceiver};
use gpui::{
    App, Bounds, Context, EventEmitter, FocusHandle, Focusable, KeyDownEvent, Modifiers,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, ScrollDelta,
    ScrollWheelEvent, Size, Subscription, Task, UTF16Selection, Window, div, point, prelude::*,
};

use crate::actions::Paste;
use client::{NvimClient, NvimEvent};
use element::NvimElement;
use keys::to_nvim_keys;
use ui::UiState;

/// The key context of the pane.
pub(crate) const NVIM_CONTEXT: &str = "Nvim";

/// What the pane reports to its window.
#[derive(Debug)]
pub(crate) enum PaneEvent {
    Exited(ExitStatus),
}

/// Where the grid was last painted, for mapping the pointer to cells.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct GridLayout {
    pub origin: Point<Pixels>,
    pub cell: Size<Pixels>,
}

impl GridLayout {
    /// The `(row, col)` of the cell under `position`, clamped to a grid of
    /// `rows` × `cols`.
    pub fn cell_at(&self, position: Point<Pixels>, rows: usize, cols: usize) -> (u16, u16) {
        let offset = position - self.origin;
        let index = |offset: Pixels, size: Pixels, count: usize| {
            let index = (f32::from(offset) / f32::from(size)).floor().max(0.) as usize;
            index.min(count.saturating_sub(1)).min(u16::MAX as usize) as u16
        };
        (
            index(offset.y, self.cell.height, rows),
            index(offset.x, self.cell.width, cols),
        )
    }

    /// The bounds of the cell at `(row, col)`.
    pub fn cell_bounds(&self, row: u16, col: u16) -> Bounds<Pixels> {
        Bounds::new(
            point(
                self.origin.x + self.cell.width * f32::from(col),
                self.origin.y + self.cell.height * f32::from(row),
            ),
            self.cell,
        )
    }
}

/// Turns scroll deltas into whole wheel steps, carrying the remainder to the
/// next event.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub(crate) struct ScrollAccumulator {
    total: Point<f32>,
}

impl ScrollAccumulator {
    /// Adds `delta`, in columns and rows, and returns the whole steps
    /// completed. Positive steps scroll left and up.
    pub fn add(&mut self, delta: Point<f32>) -> Point<i32> {
        self.total.x += delta.x;
        self.total.y += delta.y;
        let steps = point(self.total.x.trunc() as i32, self.total.y.trunc() as i32);
        self.total.x -= steps.x as f32;
        self.total.y -= steps.y as f32;
        steps
    }
}

/// The letters `nvim_input_mouse` takes for held modifiers.
fn mouse_modifiers(modifiers: &Modifiers) -> String {
    [
        (modifiers.control, 'C'),
        (modifiers.shift, 'S'),
        (modifiers.alt, 'A'),
        (modifiers.platform, 'D'),
    ]
    .into_iter()
    .filter_map(|(held, letter)| held.then_some(letter))
    .collect()
}

fn mouse_button_name(button: MouseButton) -> Option<&'static str> {
    match button {
        MouseButton::Left => Some("left"),
        MouseButton::Right => Some("right"),
        MouseButton::Middle => Some("middle"),
        MouseButton::Navigate(_) => None,
    }
}

fn utf16_len(text: &str) -> usize {
    text.chars().map(char::len_utf16).sum()
}

/// The byte range of `text` covering the UTF-16 `range`.
fn byte_range(text: &str, range: &Range<usize>) -> Range<usize> {
    let offset = |target: usize| {
        let mut utf16 = 0;
        for (index, ch) in text.char_indices() {
            if utf16 >= target {
                return index;
            }
            utf16 += ch.len_utf16();
        }
        text.len()
    };
    offset(range.start)..offset(range.end)
}

/// The Neovim pane: Neovim's screen, and the input it sends back.
pub(crate) struct NvimPane {
    pub(crate) client: NvimClient,
    pub(crate) ui: UiState,
    focus_handle: FocusHandle,
    /// The grid size last sent, or `None` before attaching.
    sent_size: Option<(u16, u16)>,
    pub(crate) layout: Option<GridLayout>,
    /// Text the IME is composing.
    marked_text: Option<String>,
    /// Whether Option sends Meta on macOS.
    pub(crate) option_as_meta: bool,
    /// The held button and the cell it last reported.
    pressed: Option<(MouseButton, (u16, u16))>,
    scroll: ScrollAccumulator,
    /// The font family and size last resolved, for the `guifont` it came from.
    font: Option<(Option<String>, element::FontChoice)>,
    _events: Task<()>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<PaneEvent> for NvimPane {}

impl NvimPane {
    pub(crate) fn new(
        client: NvimClient,
        mut events: UnboundedReceiver<NvimEvent>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();
        let subscriptions = vec![
            cx.on_focus(&focus_handle, window, |pane, _, cx| {
                pane.set_focus(true, cx)
            }),
            cx.on_blur(&focus_handle, window, |pane, _, cx| {
                pane.set_focus(false, cx)
            }),
        ];
        let events = cx.spawn(async move |pane, cx| {
            while let Some(event) = events.next().await {
                if pane
                    .update(cx, |pane, cx| pane.handle_event(event, cx))
                    .is_err()
                {
                    break;
                }
            }
        });
        Self {
            client,
            ui: UiState::default(),
            focus_handle,
            sent_size: None,
            layout: None,
            marked_text: None,
            option_as_meta: false,
            pressed: None,
            scroll: ScrollAccumulator::default(),
            font: None,
            _events: events,
            _subscriptions: subscriptions,
        }
    }

    fn handle_event(&mut self, event: NvimEvent, cx: &mut Context<Self>) {
        match event {
            NvimEvent::Frame(frame) => {
                for event in frame {
                    self.ui.apply(event);
                }
                cx.notify();
            }
            NvimEvent::Exited(status) => cx.emit(PaneEvent::Exited(status)),
        }
    }

    /// Attaches at `size` the first time, and resizes afterward.
    fn set_grid_size(&mut self, size: (u16, u16)) {
        match self.sent_size {
            Some(sent) if sent == size => return,
            Some(_) => self.client.ui_try_resize(size.0, size.1),
            None => self.client.ui_attach(size.0, size.1),
        }
        self.sent_size = Some(size);
    }

    fn set_focus(&mut self, focused: bool, cx: &mut Context<Self>) {
        if self.sent_size.is_some() {
            self.client.ui_set_focus(focused);
        }
        cx.notify();
    }

    fn alt_is_meta(&self) -> bool {
        !cfg!(target_os = "macos") || self.option_as_meta
    }

    fn on_key_down(&mut self, event: &KeyDownEvent, _: &mut Window, cx: &mut Context<Self>) {
        if self.marked_text.is_some() {
            return;
        }
        if let Some(keys) = to_nvim_keys(&event.keystroke, self.alt_is_meta()) {
            self.client.input(&keys);
            cx.stop_propagation();
        }
    }

    fn paste(&mut self, _: &Paste, _: &mut Window, cx: &mut Context<Self>) {
        if let Some(text) = cx.read_from_clipboard().and_then(|item| item.text()) {
            self.client.paste(&text);
        }
    }

    /// The cell under `position`, once the grid has been painted.
    fn cell_at(&self, position: Point<Pixels>) -> Option<(u16, u16)> {
        let layout = self.layout?;
        (self.ui.height() > 0).then(|| layout.cell_at(position, self.ui.height(), self.ui.width()))
    }

    fn mouse_down(&mut self, event: &MouseDownEvent, window: &mut Window) {
        window.focus(&self.focus_handle);
        if !self.ui.mouse_enabled {
            return;
        }
        let (Some(button), Some(cell)) = (
            mouse_button_name(event.button),
            self.cell_at(event.position),
        ) else {
            return;
        };
        self.pressed = Some((event.button, cell));
        self.client.input_mouse(
            button,
            "press",
            &mouse_modifiers(&event.modifiers),
            cell.0,
            cell.1,
        );
    }

    fn mouse_move(&mut self, event: &MouseMoveEvent) {
        let Some((button, last)) = self.pressed else {
            return;
        };
        let (Some(name), Some(cell)) = (mouse_button_name(button), self.cell_at(event.position))
        else {
            return;
        };
        if cell == last || !self.ui.mouse_enabled {
            return;
        }
        self.pressed = Some((button, cell));
        self.client.input_mouse(
            name,
            "drag",
            &mouse_modifiers(&event.modifiers),
            cell.0,
            cell.1,
        );
    }

    fn mouse_up(&mut self, event: &MouseUpEvent) {
        let Some((button, last)) = self.pressed else {
            return;
        };
        if button != event.button {
            return;
        }
        self.pressed = None;
        let Some(name) = mouse_button_name(button) else {
            return;
        };
        let cell = self.cell_at(event.position).unwrap_or(last);
        self.client.input_mouse(
            name,
            "release",
            &mouse_modifiers(&event.modifiers),
            cell.0,
            cell.1,
        );
    }

    fn scroll_wheel(&mut self, event: &ScrollWheelEvent) {
        let (Some(layout), Some(cell)) = (self.layout, self.cell_at(event.position)) else {
            return;
        };
        if !self.ui.mouse_enabled {
            return;
        }
        let delta = match event.delta {
            ScrollDelta::Pixels(delta) => point(
                f32::from(delta.x) / f32::from(layout.cell.width),
                f32::from(delta.y) / f32::from(layout.cell.height),
            ),
            ScrollDelta::Lines(delta) => delta,
        };
        let steps = self.scroll.add(delta);
        let modifiers = mouse_modifiers(&event.modifiers);
        for (count, positive, negative) in [(steps.y, "up", "down"), (steps.x, "left", "right")] {
            let action = if count > 0 { positive } else { negative };
            for _ in 0..count.unsigned_abs() {
                self.client
                    .input_mouse("wheel", action, &modifiers, cell.0, cell.1);
            }
        }
    }
}

impl Focusable for NvimPane {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for NvimPane {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context(NVIM_CONTEXT)
            .track_focus(&self.focus_handle)
            .on_key_down(cx.listener(Self::on_key_down))
            .on_action(cx.listener(Self::paste))
            .size_full()
            .child(NvimElement::new(cx.entity()))
    }
}

impl gpui::EntityInputHandler for NvimPane {
    fn text_for_range(
        &mut self,
        range_utf16: Range<usize>,
        actual_range: &mut Option<Range<usize>>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<String> {
        let text = self.marked_text.as_deref().unwrap_or_default();
        let range = byte_range(text, &range_utf16);
        actual_range.replace(utf16_len(&text[..range.start])..utf16_len(&text[..range.end]));
        Some(text[range].to_owned())
    }

    fn selected_text_range(
        &mut self,
        _: bool,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<UTF16Selection> {
        let end = utf16_len(self.marked_text.as_deref().unwrap_or_default());
        Some(UTF16Selection {
            range: end..end,
            reversed: false,
        })
    }

    fn marked_text_range(&self, _: &mut Window, _: &mut Context<Self>) -> Option<Range<usize>> {
        self.marked_text.as_deref().map(|text| 0..utf16_len(text))
    }

    fn unmark_text(&mut self, _: &mut Window, cx: &mut Context<Self>) {
        self.marked_text = None;
        cx.notify();
    }

    fn replace_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.marked_text.take().is_some() {
            cx.notify();
        }
        if !text.is_empty() {
            self.client.input(&text.replace('<', "<lt>"));
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _: Option<Range<usize>>,
        text: &str,
        _: Option<Range<usize>>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.marked_text = Some(text.to_owned()).filter(|text| !text.is_empty());
        cx.notify();
    }

    fn bounds_for_range(
        &mut self,
        _: Range<usize>,
        _: Bounds<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<Bounds<Pixels>> {
        let (row, col) = self.ui.cursor;
        Some(self.layout?.cell_bounds(row, col))
    }

    fn character_index_for_point(
        &mut self,
        _: Point<Pixels>,
        _: &mut Window,
        _: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }
}

#[cfg(test)]
mod tests {
    use gpui::{ClipboardItem, TestAppContext, WindowBounds, WindowOptions, point, px, size};
    use rmpv::Value;

    use super::*;

    /// The method and arguments of each message the pane sent.
    fn sent(messages: &std::sync::mpsc::Receiver<Vec<u8>>) -> Vec<(String, Vec<Value>)> {
        messages
            .try_iter()
            .map(|bytes| {
                let message = rmpv::decode::read_value(&mut bytes.as_slice()).unwrap();
                let [kind, method, args] =
                    <[Value; 3]>::try_from(message.as_array().cloned().unwrap()).unwrap();
                assert_eq!(kind.as_u64(), Some(2), "the pane sends only notifications");
                (
                    method.as_str().unwrap().to_owned(),
                    args.as_array().cloned().unwrap(),
                )
            })
            .collect()
    }

    #[gpui::test]
    fn the_pane_attaches_at_its_size_and_sends_keys(cx: &mut TestAppContext) {
        let (client, messages) = NvimClient::for_test();
        let (_events, receiver) = futures::channel::mpsc::unbounded();
        // The test text system's cells are 7.8 × 17 px.
        let window = cx.update(|cx| {
            cx.bind_keys(crate::key_bindings(true));
            let bounds = Bounds::new(point(px(0.), px(0.)), size(px(783.), px(175.)));
            cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |window, cx| cx.new(|cx| NvimPane::new(client, receiver, window, cx)),
            )
            .unwrap()
        });
        cx.run_until_parked();
        let attach = sent(&messages);
        assert_eq!(
            attach
                .iter()
                .map(|(method, _)| method.as_str())
                .collect::<Vec<_>>(),
            ["nvim_set_client_info", "nvim_ui_attach"]
        );
        assert_eq!(attach[1].1[..2], [Value::from(100), Value::from(10)]);

        window
            .update(cx, |pane, window, _| {
                window.activate_window();
                window.focus(&pane.focus_handle);
            })
            .unwrap();
        cx.run_until_parked();
        let expected = |method: &str, args: Vec<Value>| (method.to_owned(), args);
        assert_eq!(
            sent(&messages),
            [expected("nvim_ui_set_focus", vec![true.into()])]
        );

        cx.write_to_clipboard(ClipboardItem::new_string("pasted".to_owned()));
        cx.simulate_keystrokes(window.into(), "escape ctrl-w cmd-v");
        cx.simulate_input(window.into(), "a<b");
        let input = |keys: &str| expected("nvim_input", vec![keys.into()]);
        assert_eq!(
            sent(&messages),
            [
                input("<Esc>"),
                input("<C-w>"),
                expected(
                    "nvim_paste",
                    vec!["pasted".into(), true.into(), (-1).into()]
                ),
                input("a"),
                input("<lt>"),
                input("b"),
            ]
        );

        cx.simulate_window_resize(window.into(), size(px(705.), px(175.)));
        cx.run_until_parked();
        assert_eq!(
            sent(&messages),
            [expected("nvim_ui_try_resize", vec![90.into(), 10.into()])]
        );
        window.update(cx, |_, window, _| window.refresh()).unwrap();
        cx.run_until_parked();
        assert!(
            sent(&messages).is_empty(),
            "the same size is never sent twice"
        );
    }

    #[test]
    fn pointer_positions_map_to_cells_inside_the_grid() {
        let layout = GridLayout {
            origin: point(px(100.), px(50.)),
            cell: size(px(8.), px(16.)),
        };
        assert_eq!(layout.cell_at(point(px(100.), px(50.)), 10, 20), (0, 0));
        assert_eq!(layout.cell_at(point(px(115.9), px(81.)), 10, 20), (1, 1));
        assert_eq!(layout.cell_at(point(px(124.), px(66.)), 10, 20), (1, 3));
        assert_eq!(
            layout.cell_at(point(px(50.), px(10.)), 10, 20),
            (0, 0),
            "positions above and left clamp to the first cell"
        );
        assert_eq!(
            layout.cell_at(point(px(1000.), px(1000.)), 10, 20),
            (9, 19),
            "positions below and right clamp to the last cell"
        );
        assert_eq!(
            layout.cell_bounds(1, 3),
            Bounds::new(point(px(124.), px(66.)), size(px(8.), px(16.)))
        );
    }

    #[test]
    fn scrolling_sends_whole_steps_and_carries_the_remainder() {
        let mut scroll = ScrollAccumulator::default();
        assert_eq!(scroll.add(point(0., 0.4)), point(0, 0));
        assert_eq!(scroll.add(point(0., 0.4)), point(0, 0));
        assert_eq!(
            scroll.add(point(0., 0.4)),
            point(0, 1),
            "the remainders add up to a step"
        );
        assert_eq!(scroll.add(point(-2.5, -3.)), point(-2, -2));
        assert_eq!(
            scroll.add(point(-0.5, 0.)),
            point(-1, 0),
            "negative remainders carry too"
        );
    }

    #[test]
    fn modifiers_are_letters_for_the_mouse() {
        let modifiers = Modifiers {
            control: true,
            alt: true,
            shift: true,
            platform: true,
            function: false,
        };
        assert_eq!(mouse_modifiers(&modifiers), "CSAD");
        assert_eq!(mouse_modifiers(&Modifiers::default()), "");
    }

    #[test]
    fn utf16_ranges_map_to_byte_ranges() {
        let text = "aé日😀b";
        assert_eq!(utf16_len(text), 6);
        assert_eq!(&text[byte_range(text, &(1..3))], "é日");
        assert_eq!(&text[byte_range(text, &(3..5))], "😀");
        assert_eq!(&text[byte_range(text, &(5..9))], "b");
    }
}
