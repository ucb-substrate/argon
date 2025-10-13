use std::{borrow::Cow, net::SocketAddr};

use clap::Parser;
use editor::Editor;
use gpui::*;

use crate::actions::*;
use crate::assets::{ZED_PLEX_MONO, ZED_PLEX_SANS};

pub mod actions;
pub mod assets;
pub mod editor;
pub mod rpc;
pub mod theme;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    lsp_addr: SocketAddr,
}

pub fn main() {
    let args = Args::parse();

    Application::new().run(move |cx: &mut App| {
        // Load fonts.
        cx.text_system()
            .add_fonts(vec![
                Cow::Borrowed(ZED_PLEX_MONO),
                Cow::Borrowed(ZED_PLEX_SANS),
            ])
            .unwrap();
        // Bind keys must happen before menus to get the keybindings to show up next to menu items.
        cx.bind_keys([
            KeyBinding::new("cmd-q", Quit, None),
            KeyBinding::new("r", DrawRect, None),
            KeyBinding::new("d", DrawDim, None),
            KeyBinding::new("f", Fit, None),
            KeyBinding::new("escape", Cancel, None),
            KeyBinding::new("backspace", Backspace, None),
            KeyBinding::new("delete", Delete, None),
            KeyBinding::new("left", Left, None),
            KeyBinding::new("right", Right, None),
            KeyBinding::new("shift-left", SelectLeft, None),
            KeyBinding::new("shift-right", SelectRight, None),
            KeyBinding::new("cmd-a", SelectAll, None),
            KeyBinding::new("cmd-v", Paste, None),
            KeyBinding::new("cmd-c", Copy, None),
            KeyBinding::new("cmd-x", Cut, None),
            KeyBinding::new("home", Home, None),
            KeyBinding::new("end", End, None),
            KeyBinding::new("enter", Enter, None),
            KeyBinding::new("ctrl-cmd-space", ShowCharacterPalette, None),
        ]);
        // Register the `quit` function so it can be referenced by the `MenuItem::action` in the menu bar
        cx.on_action(quit);
        // Add menu items
        cx.set_menus(vec![Menu {
            name: "Argon".into(),
            items: vec![MenuItem::action("Quit", Quit)],
        }]);

        cx.open_window(
            WindowOptions {
                titlebar: Some(TitlebarOptions {
                    title: None,
                    appears_transparent: true,
                    traffic_light_position: None,
                }),
                focus: false,
                ..Default::default()
            },
            |window, cx| {
                window.replace_root(cx, |window, cx| Editor::new(cx, window, args.lsp_addr))
            },
        )
        .unwrap();

        cx.activate(true);
    });
}

// Define the quit function that is registered with the App
fn quit(_: &Quit, cx: &mut App) {
    println!("Gracefully quitting the application . . .");
    cx.quit();
}
