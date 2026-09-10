use std::{
    borrow::Cow,
    collections::BTreeSet,
    net::{SocketAddr, TcpListener},
};

use editor::Editor;
use gpui::*;
use rust_embed::RustEmbed;
use tracing::info;

use crate::actions::*;
use crate::assets::{ZED_PLEX_MONO, ZED_PLEX_SANS};

pub mod cli;

pub mod actions;
pub mod assets;
pub mod editor;
pub mod focus;
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

pub fn run_gui(
    lang_server_addr: SocketAddr,
    gui_listen_port: Option<u16>,
    gui_register_addr: Option<SocketAddr>,
) {
    run_inner(lang_server_addr, gui_listen_port, None, gui_register_addr);
}

pub fn run_with_listener(
    lang_server_addr: SocketAddr,
    gui_listener: TcpListener,
    gui_register_addr: SocketAddr,
) {
    run_inner(
        lang_server_addr,
        None,
        Some(gui_listener),
        Some(gui_register_addr),
    );
}

fn run_inner(
    lang_server_addr: SocketAddr,
    gui_listen_port: Option<u16>,
    gui_listener: Option<TcpListener>,
    gui_register_addr: Option<SocketAddr>,
) {
    focus::initialize_target();

    analyzer::init_logging();

    Application::new()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            // Load fonts.
            cx.text_system()
                .add_fonts(vec![
                    Cow::Borrowed(ZED_PLEX_MONO),
                    Cow::Borrowed(ZED_PLEX_SANS),
                ])
                .unwrap();
            // Bind keys must happen before menus to get the keybindings to show up next to menu items.
            cx.bind_keys(key_bindings());
            // Register the `quit` function so it can be referenced by the `MenuItem::action` in the menu bar
            cx.on_action(quit);
            // Add menu items
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
                    ],
                },
                Menu {
                    name: "Edit".into(),
                    items: vec![
                        MenuItem::action("Undo", Undo),
                        MenuItem::action("Redo", Redo),
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
                    ],
                },
                Menu {
                    name: "View".into(),
                    items: vec![
                        MenuItem::action("Full Hierarchy", All),
                        MenuItem::action("Box Only", Zero),
                        MenuItem::action("Top Level Only", One),
                        MenuItem::action("Fit to Screen", Fit),
                        MenuItem::action("Dark Mode", DarkMode),
                        MenuItem::action("Light Mode", LightMode),
                    ],
                },
            ]);

            cx.open_window(editor_window_options(), |window, cx| {
                window.replace_root(cx, |window, cx| {
                    Editor::new(
                        cx,
                        window,
                        lang_server_addr,
                        gui_listen_port,
                        gui_listener,
                        gui_register_addr,
                    )
                })
            })
            .unwrap();

            focus::activate_gui(cx);
        });
}

fn key_bindings() -> Vec<KeyBinding> {
    const CANVAS_CONTEXT: &str = "LayoutCanvas && !TextInput";
    vec![
        KeyBinding::new("cmd-q", Quit, None),
        KeyBinding::new("cmd-s", Save, None),
        KeyBinding::new("r", DrawRect, Some(CANVAS_CONTEXT)),
        KeyBinding::new("p", DrawPolygon, Some(CANVAS_CONTEXT)),
        KeyBinding::new("s", SelectMode, Some(CANVAS_CONTEXT)),
        KeyBinding::new("d", DrawDim, Some(CANVAS_CONTEXT)),
        KeyBinding::new("i", InstantiateCommand, Some(CANVAS_CONTEXT)),
        KeyBinding::new("o", OpenCellCommand, Some(CANVAS_CONTEXT)),
        KeyBinding::new("cmd-n", NewCellCommand, None),
        KeyBinding::new("cmd-shift-r", RenameCellCommand, None),
        KeyBinding::new("f", Fit, Some(CANVAS_CONTEXT)),
        KeyBinding::new("q", Edit, Some(CANVAS_CONTEXT)),
        KeyBinding::new("u", Undo, Some(CANVAS_CONTEXT)),
        KeyBinding::new("ctrl-r", Redo, Some(CANVAS_CONTEXT)),
        KeyBinding::new("0", Zero, Some(CANVAS_CONTEXT)),
        KeyBinding::new("1", One, Some(CANVAS_CONTEXT)),
        KeyBinding::new("*", All, Some(CANVAS_CONTEXT)),
        KeyBinding::new("left", PanLeft, Some(CANVAS_CONTEXT)),
        KeyBinding::new("right", PanRight, Some(CANVAS_CONTEXT)),
        KeyBinding::new("up", PanUp, Some(CANVAS_CONTEXT)),
        KeyBinding::new("down", PanDown, Some(CANVAS_CONTEXT)),
        KeyBinding::new("cmd-=", ZoomIn, Some(CANVAS_CONTEXT)),
        KeyBinding::new("cmd-+", ZoomIn, Some(CANVAS_CONTEXT)),
        KeyBinding::new("cmd--", ZoomOut, Some(CANVAS_CONTEXT)),
        KeyBinding::new("ctrl-=", ZoomIn, Some(CANVAS_CONTEXT)),
        KeyBinding::new("ctrl-+", ZoomIn, Some(CANVAS_CONTEXT)),
        KeyBinding::new("ctrl--", ZoomOut, Some(CANVAS_CONTEXT)),
        KeyBinding::new("ctrl-\\", FocusInvoker, None),
        KeyBinding::new("ctrl-shift-d", ShowDiagnostics, None),
        KeyBinding::new("ctrl-shift-m", ShowMessages, None),
        KeyBinding::new(":", FocusInvokerCommandBar, Some("!TextInput")),
        KeyBinding::new("escape", Cancel, Some(CANVAS_CONTEXT)),
        KeyBinding::new("enter", Enter, Some(CANVAS_CONTEXT)),
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
    ]
}

fn editor_window_options() -> WindowOptions {
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

    use super::{actions::*, key_bindings};

    struct ShortcutTestView {
        canvas_focus: FocusHandle,
        input_focus: FocusHandle,
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
    }

    impl Render for ShortcutTestView {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
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
                        .key_context("LayoutCanvas")
                        .track_focus(&self.canvas_focus),
                )
                .child(
                    div().key_context("LayoutCanvas").child(
                        div()
                            .key_context("TextInput")
                            .track_focus(&self.input_focus),
                    ),
                )
        }
    }

    #[gpui::test]
    fn canvas_shortcuts_are_scoped_but_focus_shortcuts_are_global(cx: &mut TestAppContext) {
        let window = cx.update(|cx| {
            cx.bind_keys(key_bindings());
            cx.open_window(Default::default(), |_, cx| {
                cx.new(|cx| ShortcutTestView {
                    canvas_focus: cx.focus_handle(),
                    input_focus: cx.focus_handle(),
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
                })
            })
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
}
