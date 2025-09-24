use std::{borrow::Cow, net::SocketAddr};

use clap::Parser;
use editor::Editor;
use gpui::*;

use crate::assets::{ZED_PLEX_MONO, ZED_PLEX_SANS};

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
            KeyBinding::new("escape", Cancel, None),
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
                window.replace_root(cx, |window, cx| {
                    let editor = Editor::new(cx, args.lsp_addr);
                    window.focus(&editor.canvas.read(cx).focus_handle(cx));
                    editor
                })
            },
        )
        .unwrap();
    });
}

// Associate actions using the `actions!` macro (or `impl_actions!` macro)
actions!(Argon, [Quit, DrawRect, Cancel]);

// Define the quit function that is registered with the App
fn quit(_: &Quit, cx: &mut App) {
    println!("Gracefully quitting the application . . .");
    cx.quit();
}
