use std::{borrow::Cow, collections::BTreeSet, net::SocketAddr};

use analyzer::transport::SessionToken;

use editor::start_screen::START_SCREEN_CONTEXT;
use gpui::*;
use rust_embed::RustEmbed;
use tracing::info;

use crate::actions::*;
use crate::assets::{
    ZED_PLEX_MONO, ZED_PLEX_MONO_BOLD, ZED_PLEX_MONO_BOLD_ITALIC, ZED_PLEX_MONO_ITALIC,
    ZED_PLEX_SANS,
};
use crate::main_window::MainWindow;
use crate::nvim::NVIM_CONTEXT;

pub mod cli;

pub mod actions;
pub mod assets;
pub mod editor;
pub mod focus;
mod main_window;
mod nvim;
pub mod rpc;
pub mod sse;
pub mod theme;

#[derive(RustEmbed)]
#[folder = "assets/"]
struct Assets;

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        Ok(Self::get(path).map(|asset| Cow::Owned(asset.data.into_owned())))
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        let path = path.trim_matches('/');
        let prefix = (!path.is_empty()).then(|| format!("{path}/"));
        let entries = Self::iter()
            .filter_map(|asset_path| {
                let relative = match &prefix {
                    Some(prefix) => asset_path.strip_prefix(prefix)?,
                    None => asset_path.as_ref(),
                };
                relative.split('/').next().map(str::to_string)
            })
            .collect::<BTreeSet<_>>();
        Ok(entries.into_iter().map(SharedString::from).collect())
    }
}

/// Loads fonts, binds keys, and sets the menus. `embedded` is whether the
/// window holds a Neovim pane.
fn init_app(cx: &mut App, embedded: bool) {
    cx.text_system()
        .add_fonts(
            [
                ZED_PLEX_MONO,
                ZED_PLEX_MONO_BOLD,
                ZED_PLEX_MONO_ITALIC,
                ZED_PLEX_MONO_BOLD_ITALIC,
                ZED_PLEX_SANS,
            ]
            .into_iter()
            .map(Cow::Borrowed)
            .collect(),
        )
        .unwrap();
    // Bind keys must happen before menus to get the keybindings to show up next to menu items.
    cx.bind_keys(key_bindings(embedded));
    // Register the `quit` function so it can be referenced by the `MenuItem::action` in the menu bar
    if !embedded {
        cx.on_action(quit);
    }
    let mut view_items = vec![
        MenuItem::action("Layout", ViewLayout),
        MenuItem::action("Schematic", ViewSchematic),
        MenuItem::action("Symbol", ViewSymbol),
    ];
    if embedded {
        view_items.push(MenuItem::action("Neovim", ToggleNeovim));
    }
    view_items.extend([
        MenuItem::separator(),
        MenuItem::action("Descend into Instance", Descend),
        MenuItem::action("Return", Return),
        MenuItem::action("Flylines", ToggleFlylines),
        MenuItem::action("Fix Positions", FixPositions),
        MenuItem::action("Remove Route", RemoveRoute),
        MenuItem::separator(),
        MenuItem::action("Full Hierarchy", All),
        MenuItem::action("Box Only", Zero),
        MenuItem::action("Top Level Only", One),
        MenuItem::action("Fit to Screen", Fit),
        MenuItem::action("Dark Mode", DarkMode),
        MenuItem::action("Light Mode", LightMode),
    ]);
    cx.set_menus(vec![
        Menu {
            name: "Argon".into(),
            items: vec![MenuItem::action("Quit", Quit)],
        },
        Menu {
            name: "File".into(),
            items: vec![
                MenuItem::action("New Cell…", NewCellCommand),
                MenuItem::action("Open Cell…", OpenCellCommand),
                MenuItem::action("Rename Cell…", RenameCellCommand),
                MenuItem::separator(),
                MenuItem::action("Save", Save),
                MenuItem::action("Export Netlist…", ExportNetlist),
            ],
        },
        Menu {
            name: "Edit".into(),
            items: vec![
                MenuItem::action("Undo", Undo),
                MenuItem::action("Redo", Redo),
                MenuItem::separator(),
                MenuItem::action("Copy", Copy),
                MenuItem::action("Paste", Paste),
                MenuItem::action("Delete", Delete),
            ],
        },
        Menu {
            name: "Tools".into(),
            items: vec![
                MenuItem::action("Rect", DrawRect),
                MenuItem::action("Polygon", DrawPolygon),
                MenuItem::action("Path", DrawPath),
                MenuItem::action("Dim", DrawDim),
                MenuItem::action("Edit", Edit),
                MenuItem::separator(),
                MenuItem::action("Wire", Wire),
                MenuItem::action("Port", PlacePort),
                MenuItem::action("Net Label", PlaceLabel),
                MenuItem::action("Device…", DeviceCommand),
                MenuItem::action("Instance…", InstantiateCommand),
                MenuItem::separator(),
                MenuItem::action("Symbol Line", SymbolLine),
                MenuItem::action("Symbol Circle", SymbolCircle),
                MenuItem::action("Symbol Arc", SymbolArc),
                MenuItem::action("Symbol Text…", SymbolText),
                MenuItem::action("Symbol Pin", SymbolPin),
                MenuItem::action("Create Symbol", CreateSymbol),
            ],
        },
        Menu {
            name: "View".into(),
            items: view_items,
        },
    ]);
}

/// Runs the GUI on its own, connected to the analyzer at `lang_server_addr`.
pub fn run_gui(lang_server_addr: SocketAddr, token: SessionToken) {
    focus::initialize_target();

    analyzer::init_logging();

    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            init_app(cx, false);
            cx.open_window(editor_window_options(), |window, cx| {
                let main = cx.new(|cx| MainWindow::standalone(lang_server_addr, token, window, cx));
                MainWindow::keep_standalone(&main, cx);
                main
            })
            .unwrap();

            focus::activate_gui(cx);
        });
}

/// Runs the GUI with the Neovim that `client` talks to in a pane, connecting
/// to each analyzer that `announcements` reports.
#[cfg(unix)]
pub(crate) fn run_embedded(
    client: nvim::client::NvimClient,
    events: futures::channel::mpsc::UnboundedReceiver<nvim::client::NvimEvent>,
    announcements: main_window::AnnouncementSource,
) {
    analyzer::init_logging();

    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            init_app(cx, true);
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                    None,
                    size(px(1440.), px(900.)),
                    cx,
                ))),
                focus: true,
                ..editor_window_options()
            };
            let window = cx
                .open_window(options, |window, cx| {
                    let pane = cx.new(|cx| nvim::NvimPane::new(client, events, window, cx));
                    let main = cx.new(|cx| MainWindow::embedded(pane, announcements, window, cx));
                    let weak = main.downgrade();
                    window.on_window_should_close(cx, move |window, cx| {
                        weak.update(cx, |main, cx| main.request_quit(window, cx))
                            .unwrap_or(true)
                    });
                    main
                })
                .unwrap();
            MainWindow::route_quit(window, cx);
            cx.activate(true);
        });
}

/// The key bindings. `embedded` is whether the window holds a Neovim pane,
/// which moves the focus key from `ctrl-\` to `cmd-\`.
fn key_bindings(embedded: bool) -> Vec<KeyBinding> {
    // Every canvas handles these; tool letters stay per canvas.
    const CANVAS: &str = "Canvas && !TextInput";
    const LAYOUT: &str = "LayoutCanvas && !TextInput";
    const SCHEMATIC: &str = "SchematicCanvas && !TextInput";
    const SYMBOL: &str = "SymbolCanvas && !TextInput";
    let focus_key = if embedded { "cmd-\\" } else { "ctrl-\\" };
    vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-s", Save, None),
        KeyBinding::new("r", DrawRect, Some(LAYOUT)),
        KeyBinding::new("p", DrawPolygon, Some(LAYOUT)),
        KeyBinding::new("s", SelectMode, Some(LAYOUT)),
        KeyBinding::new("d", DrawDim, Some(LAYOUT)),
        KeyBinding::new("i", InstantiateCommand, Some(LAYOUT)),
        KeyBinding::new("q", Edit, Some(LAYOUT)),
        KeyBinding::new("0", Zero, Some(LAYOUT)),
        KeyBinding::new("1", One, Some(LAYOUT)),
        KeyBinding::new("*", All, Some(LAYOUT)),
        KeyBinding::new("s", SelectMode, Some(SCHEMATIC)),
        KeyBinding::new("i", InstantiateCommand, Some(SCHEMATIC)),
        KeyBinding::new("e", Descend, Some(SCHEMATIC)),
        KeyBinding::new("shift-e", Return, Some(SCHEMATIC)),
        KeyBinding::new("r", Rotate, Some(SCHEMATIC)),
        KeyBinding::new("shift-r", Mirror, Some(SCHEMATIC)),
        KeyBinding::new("w", Wire, Some(SCHEMATIC)),
        KeyBinding::new("p", PlacePort, Some(SCHEMATIC)),
        KeyBinding::new("l", PlaceLabel, Some(SCHEMATIC)),
        KeyBinding::new("m", DeviceCommand, Some(SCHEMATIC)),
        KeyBinding::new("/", FlipBend, Some(SCHEMATIC)),
        KeyBinding::new("q", Edit, Some(SCHEMATIC)),
        KeyBinding::new("delete", Delete, Some(SCHEMATIC)),
        KeyBinding::new("backspace", Backspace, Some(SCHEMATIC)),
        KeyBinding::new("enter", Enter, Some(SCHEMATIC)),
        KeyBinding::new("cmd-c", Copy, Some(SCHEMATIC)),
        KeyBinding::new("cmd-v", Paste, Some(SCHEMATIC)),
        KeyBinding::new("s", SelectMode, Some(SYMBOL)),
        KeyBinding::new("l", SymbolLine, Some(SYMBOL)),
        KeyBinding::new("p", DrawPolygon, Some(SYMBOL)),
        KeyBinding::new("r", DrawRect, Some(SYMBOL)),
        KeyBinding::new("c", SymbolCircle, Some(SYMBOL)),
        KeyBinding::new("a", SymbolArc, Some(SYMBOL)),
        KeyBinding::new("t", SymbolText, Some(SYMBOL)),
        KeyBinding::new("n", SymbolPin, Some(SYMBOL)),
        KeyBinding::new("tab", NextPort, Some(SYMBOL)),
        KeyBinding::new("x", SymbolRotate, Some(SYMBOL)),
        KeyBinding::new("shift-x", SymbolMirror, Some(SYMBOL)),
        KeyBinding::new("q", Edit, Some(SYMBOL)),
        KeyBinding::new("delete", Delete, Some(SYMBOL)),
        KeyBinding::new("backspace", Backspace, Some(SYMBOL)),
        KeyBinding::new("enter", Enter, Some(SYMBOL)),
        KeyBinding::new("cmd-c", Copy, Some(SYMBOL)),
        KeyBinding::new("cmd-v", Paste, Some(SYMBOL)),
        KeyBinding::new("o", OpenCellCommand, Some(CANVAS)),
        KeyBinding::new("o", OpenCellCommand, Some(START_SCREEN_CONTEXT)),
        KeyBinding::new("cmd-n", NewCellCommand, None),
        KeyBinding::new("cmd-shift-r", RenameCellCommand, None),
        KeyBinding::new("f", Fit, Some(CANVAS)),
        KeyBinding::new("u", Undo, Some(CANVAS)),
        KeyBinding::new("ctrl-r", Redo, Some(CANVAS)),
        KeyBinding::new("v", NextView, Some(CANVAS)),
        KeyBinding::new("shift-v", PreviousView, Some(CANVAS)),
        KeyBinding::new("left", PanLeft, Some(CANVAS)),
        KeyBinding::new("right", PanRight, Some(CANVAS)),
        KeyBinding::new("up", PanUp, Some(CANVAS)),
        KeyBinding::new("down", PanDown, Some(CANVAS)),
        KeyBinding::new("cmd-=", ZoomIn, Some(CANVAS)),
        KeyBinding::new("cmd-+", ZoomIn, Some(CANVAS)),
        KeyBinding::new("cmd--", ZoomOut, Some(CANVAS)),
        KeyBinding::new("ctrl-=", ZoomIn, Some(CANVAS)),
        KeyBinding::new("ctrl-+", ZoomIn, Some(CANVAS)),
        KeyBinding::new("ctrl--", ZoomOut, Some(CANVAS)),
        KeyBinding::new(focus_key, FocusInvoker, None),
        KeyBinding::new("ctrl-shift-d", ShowDiagnostics, None),
        KeyBinding::new("ctrl-shift-m", ShowMessages, None),
        KeyBinding::new(":", FocusInvokerCommandBar, Some("!TextInput && !Nvim")),
        KeyBinding::new("escape", Cancel, Some(CANVAS)),
        KeyBinding::new("enter", Enter, Some(CANVAS)),
        KeyBinding::new("backspace", Backspace, Some(CANVAS)),
        KeyBinding::new("escape", Cancel, Some("TextInput")),
        KeyBinding::new("backspace", Backspace, Some("TextInput")),
        KeyBinding::new("delete", Delete, Some("TextInput")),
        KeyBinding::new("left", Left, Some("TextInput")),
        KeyBinding::new("right", Right, Some("TextInput")),
        KeyBinding::new("shift-left", SelectLeft, Some("TextInput")),
        KeyBinding::new("shift-right", SelectRight, Some("TextInput")),
        KeyBinding::new("cmd-a", SelectAll, Some("TextInput")),
        KeyBinding::new("cmd-v", Paste, Some("TextInput")),
        KeyBinding::new("cmd-c", Copy, Some("TextInput")),
        KeyBinding::new("cmd-x", Cut, Some("TextInput")),
        KeyBinding::new("home", Home, Some("TextInput")),
        KeyBinding::new("end", End, Some("TextInput")),
        KeyBinding::new("enter", Enter, Some("TextInput")),
        KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, Some("TextInput")),
        KeyBinding::new("cmd-v", Paste, Some(NVIM_CONTEXT)),
    ]
}

pub(crate) fn editor_window_options() -> WindowOptions {
    WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: None,
            appears_transparent: true,
            traffic_light_position: None,
        }),
        focus: false,
        ..Default::default()
    }
}

// Define the quit function that is registered with the App
fn quit(_: &Quit, cx: &mut App) {
    info!("Gracefully quitting the application . . .");
    cx.quit();
}

#[cfg(test)]
mod tests {
    use gpui::{Context, FocusHandle, Render, TestAppContext, Window, div, prelude::*};

    use super::{NVIM_CONTEXT, START_SCREEN_CONTEXT, actions::*, key_bindings};

    struct ShortcutTestView {
        canvas_focus: FocusHandle,
        schematic_focus: FocusHandle,
        symbol_focus: FocusHandle,
        input_focus: FocusHandle,
        start_focus: FocusHandle,
        nvim_focus: FocusHandle,
        next_view_count: usize,
        previous_view_count: usize,
        rotate_count: usize,
        mirror_count: usize,
        descend_count: usize,
        return_count: usize,
        fit_count: usize,
        select_count: usize,
        undo_count: usize,
        draw_rect_count: usize,
        draw_polygon_count: usize,
        command_bar_count: usize,
        instantiate_count: usize,
        open_cell_count: usize,
        new_cell_count: usize,
        rename_cell_count: usize,
        focus_invoker_count: usize,
        show_diagnostics_count: usize,
        show_messages_count: usize,
        save_count: usize,
        pan_count: usize,
        zoom_in_count: usize,
        zoom_out_count: usize,
        zero_count: usize,
        one_count: usize,
        all_count: usize,
        backspace_count: usize,
        symbol_line_count: usize,
        symbol_pin_count: usize,
        symbol_rotate_count: usize,
        symbol_mirror_count: usize,
    }

    impl ShortcutTestView {
        fn new(cx: &mut Context<Self>) -> Self {
            ShortcutTestView {
                canvas_focus: cx.focus_handle(),
                schematic_focus: cx.focus_handle(),
                symbol_focus: cx.focus_handle(),
                input_focus: cx.focus_handle(),
                start_focus: cx.focus_handle(),
                nvim_focus: cx.focus_handle(),
                next_view_count: 0,
                previous_view_count: 0,
                rotate_count: 0,
                mirror_count: 0,
                descend_count: 0,
                return_count: 0,
                fit_count: 0,
                select_count: 0,
                undo_count: 0,
                draw_rect_count: 0,
                draw_polygon_count: 0,
                command_bar_count: 0,
                instantiate_count: 0,
                open_cell_count: 0,
                new_cell_count: 0,
                rename_cell_count: 0,
                focus_invoker_count: 0,
                show_diagnostics_count: 0,
                show_messages_count: 0,
                save_count: 0,
                pan_count: 0,
                zoom_in_count: 0,
                zoom_out_count: 0,
                zero_count: 0,
                one_count: 0,
                all_count: 0,
                backspace_count: 0,
                symbol_line_count: 0,
                symbol_pin_count: 0,
                symbol_rotate_count: 0,
                symbol_mirror_count: 0,
            }
        }
    }

    impl Render for ShortcutTestView {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .on_action(cx.listener(|view, _: &NextView, _, _| view.next_view_count += 1))
                .on_action(
                    cx.listener(|view, _: &PreviousView, _, _| view.previous_view_count += 1),
                )
                .on_action(cx.listener(|view, _: &Rotate, _, _| view.rotate_count += 1))
                .on_action(cx.listener(|view, _: &Mirror, _, _| view.mirror_count += 1))
                .on_action(cx.listener(|view, _: &Descend, _, _| view.descend_count += 1))
                .on_action(cx.listener(|view, _: &Return, _, _| view.return_count += 1))
                .on_action(cx.listener(|view, _: &Fit, _, _| view.fit_count += 1))
                .on_action(cx.listener(|view, _: &SelectMode, _, _| view.select_count += 1))
                .on_action(cx.listener(|view, _: &Undo, _, _| view.undo_count += 1))
                .on_action(cx.listener(|view, _: &Save, _, _| view.save_count += 1))
                .on_action(cx.listener(|view, _: &DrawRect, _, _| view.draw_rect_count += 1))
                .on_action(cx.listener(|view, _: &DrawPolygon, _, _| view.draw_polygon_count += 1))
                .on_action(cx.listener(|view, _: &PanLeft, _, _| view.pan_count += 1))
                .on_action(cx.listener(|view, _: &PanRight, _, _| view.pan_count += 1))
                .on_action(cx.listener(|view, _: &PanUp, _, _| view.pan_count += 1))
                .on_action(cx.listener(|view, _: &PanDown, _, _| view.pan_count += 1))
                .on_action(cx.listener(|view, _: &ZoomIn, _, _| view.zoom_in_count += 1))
                .on_action(cx.listener(|view, _: &ZoomOut, _, _| view.zoom_out_count += 1))
                .on_action(cx.listener(|view, _: &Zero, _, _| view.zero_count += 1))
                .on_action(cx.listener(|view, _: &One, _, _| view.one_count += 1))
                .on_action(cx.listener(|view, _: &All, _, _| view.all_count += 1))
                .on_action(cx.listener(|view, _: &Backspace, _, _| view.backspace_count += 1))
                .on_action(cx.listener(|view, _: &SymbolLine, _, _| view.symbol_line_count += 1))
                .on_action(cx.listener(|view, _: &SymbolPin, _, _| view.symbol_pin_count += 1))
                .on_action(
                    cx.listener(|view, _: &SymbolRotate, _, _| view.symbol_rotate_count += 1),
                )
                .on_action(
                    cx.listener(|view, _: &SymbolMirror, _, _| view.symbol_mirror_count += 1),
                )
                .on_action(
                    cx.listener(|view, _: &FocusInvokerCommandBar, _, _| {
                        view.command_bar_count += 1
                    }),
                )
                .on_action(
                    cx.listener(|view, _: &InstantiateCommand, _, _| view.instantiate_count += 1),
                )
                .on_action(cx.listener(|view, _: &OpenCellCommand, _, _| view.open_cell_count += 1))
                .on_action(cx.listener(|view, _: &NewCellCommand, _, _| view.new_cell_count += 1))
                .on_action(
                    cx.listener(|view, _: &RenameCellCommand, _, _| view.rename_cell_count += 1),
                )
                .on_action(
                    cx.listener(|view, _: &FocusInvoker, _, _| view.focus_invoker_count += 1),
                )
                .on_action(
                    cx.listener(|view, _: &ShowDiagnostics, _, _| view.show_diagnostics_count += 1),
                )
                .on_action(
                    cx.listener(|view, _: &ShowMessages, _, _| view.show_messages_count += 1),
                )
                .child(
                    div()
                        .key_context("Canvas LayoutCanvas")
                        .track_focus(&self.canvas_focus),
                )
                .child(
                    div()
                        .key_context("Canvas SchematicCanvas")
                        .track_focus(&self.schematic_focus),
                )
                .child(
                    div()
                        .key_context("Canvas SymbolCanvas")
                        .track_focus(&self.symbol_focus),
                )
                .child(
                    div().key_context("Canvas LayoutCanvas").child(
                        div()
                            .key_context("TextInput")
                            .track_focus(&self.input_focus),
                    ),
                )
                .child(
                    div()
                        .key_context(START_SCREEN_CONTEXT)
                        .track_focus(&self.start_focus),
                )
                .child(
                    div()
                        .key_context(NVIM_CONTEXT)
                        .track_focus(&self.nvim_focus),
                )
        }
    }

    #[gpui::test]
    fn the_start_screen_opens_and_creates_cells_but_has_no_tools(cx: &mut TestAppContext) {
        let window = cx.update(|cx| {
            cx.bind_keys(key_bindings(false));
            cx.open_window(Default::default(), |_, cx| cx.new(ShortcutTestView::new))
                .unwrap()
        });

        window
            .update(cx, |view, window, _| window.focus(&view.start_focus))
            .unwrap();
        cx.simulate_keystrokes(*window, "o cmd-n r v f");
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.open_cell_count, 1);
                assert_eq!(view.new_cell_count, 1);
                assert_eq!(view.draw_rect_count, 0);
                assert_eq!(view.next_view_count, 0);
                assert_eq!(view.fit_count, 0);
            })
            .unwrap();
    }

    #[gpui::test]
    fn view_shortcuts_follow_the_focused_canvas(cx: &mut TestAppContext) {
        let window = cx.update(|cx| {
            cx.bind_keys(key_bindings(false));
            cx.open_window(Default::default(), |_, cx| cx.new(ShortcutTestView::new))
                .unwrap()
        });
        let keys = "v shift-v f s e shift-e r shift-r cmd-n p l n x shift-x";

        // The layout canvas: `e` and `shift-e` are not bound there.
        window
            .update(cx, |view, window, _| window.focus(&view.canvas_focus))
            .unwrap();
        cx.simulate_keystrokes(*window, keys);
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.next_view_count, 1);
                assert_eq!(view.previous_view_count, 1);
                assert_eq!(view.fit_count, 1);
                assert_eq!(view.select_count, 1);
                assert_eq!(view.descend_count, 0);
                assert_eq!(view.return_count, 0);
                assert_eq!(view.draw_rect_count, 1);
                assert_eq!(view.rotate_count, 0);
                assert_eq!(view.new_cell_count, 1);
            })
            .unwrap();

        // The schematic canvas: descend and return, but no layout tools.
        window
            .update(cx, |view, window, _| window.focus(&view.schematic_focus))
            .unwrap();
        cx.simulate_keystrokes(*window, keys);
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.next_view_count, 2);
                assert_eq!(view.previous_view_count, 2);
                assert_eq!(view.fit_count, 2);
                assert_eq!(view.select_count, 2);
                assert_eq!(view.descend_count, 1);
                assert_eq!(view.return_count, 1);
                assert_eq!(view.draw_rect_count, 1);
                assert_eq!(view.rotate_count, 1);
                assert_eq!(view.mirror_count, 1);
                assert_eq!(view.new_cell_count, 2);
            })
            .unwrap();

        // The symbol canvas: the common keys, plus its own drawing letters.
        // `p` and `r` are the polygon and rectangle there, and `x` turns.
        window
            .update(cx, |view, window, _| window.focus(&view.symbol_focus))
            .unwrap();
        cx.simulate_keystrokes(*window, keys);
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.next_view_count, 3);
                assert_eq!(view.previous_view_count, 3);
                assert_eq!(view.fit_count, 3);
                assert_eq!(view.select_count, 3);
                assert_eq!(view.descend_count, 1);
                assert_eq!(view.draw_rect_count, 2);
                assert_eq!(view.draw_polygon_count, 2);
                assert_eq!(view.rotate_count, 1, "`r` draws a rectangle here");
                assert_eq!(view.symbol_line_count, 1);
                assert_eq!(view.symbol_pin_count, 1);
                assert_eq!(view.symbol_rotate_count, 1);
                assert_eq!(view.symbol_mirror_count, 1);
                assert_eq!(view.new_cell_count, 3);
            })
            .unwrap();

        // A text field keeps the letters, but the global shortcuts work.
        window
            .update(cx, |view, window, _| window.focus(&view.input_focus))
            .unwrap();
        cx.simulate_keystrokes(*window, keys);
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.next_view_count, 3);
                assert_eq!(view.fit_count, 3);
                assert_eq!(view.new_cell_count, 4);
            })
            .unwrap();
    }

    #[gpui::test]
    fn backspace_is_bound_in_the_canvas_as_well_as_text_inputs(cx: &mut TestAppContext) {
        let window = cx.update(|cx| {
            cx.bind_keys(key_bindings(false));
            cx.open_window(Default::default(), |_, cx| cx.new(ShortcutTestView::new))
                .unwrap()
        });

        window
            .update(cx, |view, window, _| window.focus(&view.input_focus))
            .unwrap();
        cx.simulate_keystrokes(*window, "backspace");
        window
            .update(cx, |view, _, _| assert_eq!(view.backspace_count, 1))
            .unwrap();

        window
            .update(cx, |view, window, _| window.focus(&view.canvas_focus))
            .unwrap();
        cx.simulate_keystrokes(*window, "backspace");
        window
            .update(cx, |view, _, _| assert_eq!(view.backspace_count, 2))
            .unwrap();
    }

    #[gpui::test]
    fn canvas_shortcuts_are_scoped_but_focus_shortcuts_are_global(cx: &mut TestAppContext) {
        let window = cx.update(|cx| {
            cx.bind_keys(key_bindings(false));
            cx.open_window(Default::default(), |_, cx| cx.new(ShortcutTestView::new))
                .unwrap()
        });

        window
            .update(cx, |view, window, _| window.focus(&view.input_focus))
            .unwrap();
        cx.simulate_keystrokes(
            *window,
            "u r p i o 0 1 * left right up down cmd-= cmd-+ cmd-- ctrl-= ctrl-+ ctrl-- ctrl-shift-d ctrl-shift-m : ctrl-\\ cmd-s cmd-n cmd-shift-r",
        );
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.undo_count, 0);
                assert_eq!(view.draw_rect_count, 0);
                assert_eq!(view.draw_polygon_count, 0);
                assert_eq!(view.command_bar_count, 0);
                assert_eq!(view.instantiate_count, 0);
                assert_eq!(view.open_cell_count, 0);
                assert_eq!(view.new_cell_count, 1);
                assert_eq!(view.rename_cell_count, 1);
                assert_eq!(view.zero_count, 0);
                assert_eq!(view.one_count, 0);
                assert_eq!(view.all_count, 0);
                assert_eq!(view.focus_invoker_count, 1);
                assert_eq!(view.show_diagnostics_count, 1);
                assert_eq!(view.show_messages_count, 1);
                assert_eq!(view.save_count, 1);
                assert_eq!(view.pan_count, 0);
                assert_eq!(view.zoom_in_count, 0);
                assert_eq!(view.zoom_out_count, 0);
            })
            .unwrap();

        window
            .update(cx, |view, window, _| window.focus(&view.canvas_focus))
            .unwrap();
        cx.simulate_keystrokes(
            *window,
            "u r p i o 0 1 * left right up down cmd-= cmd-+ cmd-- ctrl-= ctrl-+ ctrl-- ctrl-shift-d ctrl-shift-m : ctrl-\\ cmd-s cmd-n cmd-shift-r",
        );
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.undo_count, 1);
                assert_eq!(view.draw_rect_count, 1);
                assert_eq!(view.draw_polygon_count, 1);
                assert_eq!(view.command_bar_count, 1);
                assert_eq!(view.instantiate_count, 1);
                assert_eq!(view.open_cell_count, 1);
                assert_eq!(view.new_cell_count, 2);
                assert_eq!(view.rename_cell_count, 2);
                assert_eq!(view.zero_count, 1);
                assert_eq!(view.one_count, 1);
                assert_eq!(view.all_count, 1);
                assert_eq!(view.focus_invoker_count, 2);
                assert_eq!(view.show_diagnostics_count, 2);
                assert_eq!(view.show_messages_count, 2);
                assert_eq!(view.save_count, 2);
                assert_eq!(view.pan_count, 4);
                assert_eq!(view.zoom_in_count, 4);
                assert_eq!(view.zoom_out_count, 2);
            })
            .unwrap();
    }

    #[gpui::test]
    fn neovim_receives_canvas_letters_and_the_command_key(cx: &mut TestAppContext) {
        let window = cx.update(|cx| {
            cx.bind_keys(key_bindings(true));
            cx.open_window(Default::default(), |_, cx| cx.new(ShortcutTestView::new))
                .unwrap()
        });

        window
            .update(cx, |view, window, _| window.focus(&view.nvim_focus))
            .unwrap();
        cx.simulate_keystrokes(*window, "u r p i o 0 1 * v f left : cmd-s ctrl-shift-d");
        window
            .update(cx, |view, _, _| {
                assert_eq!(view.command_bar_count, 0, "`:` belongs to Neovim");
                assert_eq!(view.undo_count, 0);
                assert_eq!(view.draw_rect_count, 0);
                assert_eq!(view.draw_polygon_count, 0);
                assert_eq!(view.instantiate_count, 0);
                assert_eq!(view.open_cell_count, 0);
                assert_eq!(view.zero_count, 0);
                assert_eq!(view.one_count, 0);
                assert_eq!(view.all_count, 0);
                assert_eq!(view.next_view_count, 0);
                assert_eq!(view.fit_count, 0);
                assert_eq!(view.pan_count, 0);
                assert_eq!(view.save_count, 1, "global shortcuts still work");
                assert_eq!(view.show_diagnostics_count, 1);
            })
            .unwrap();
    }

    #[gpui::test]
    fn the_focus_key_depends_on_whether_neovim_is_embedded(cx: &mut TestAppContext) {
        for (embedded, focus_key, other_key) in
            [(true, "cmd-\\", "ctrl-\\"), (false, "ctrl-\\", "cmd-\\")]
        {
            let window = cx.update(|cx| {
                cx.clear_key_bindings();
                cx.bind_keys(key_bindings(embedded));
                cx.open_window(Default::default(), |_, cx| cx.new(ShortcutTestView::new))
                    .unwrap()
            });
            for focus in [
                |view: &ShortcutTestView| view.canvas_focus.clone(),
                |view: &ShortcutTestView| view.nvim_focus.clone(),
            ] {
                window
                    .update(cx, |view, window, _| window.focus(&focus(view)))
                    .unwrap();
                cx.simulate_keystrokes(*window, other_key);
                cx.simulate_keystrokes(*window, focus_key);
            }
            window
                .update(cx, |view, _, _| {
                    assert_eq!(
                        view.focus_invoker_count, 2,
                        "only {focus_key} focuses the invoker when embedded is {embedded}"
                    );
                })
                .unwrap();
        }
    }
}
