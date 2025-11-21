use std::path::PathBuf;
use std::{borrow::Cow, net::SocketAddr};

use clap::Parser;
use editor::Editor;
use gpui::*;
use lang_server::config::default_argon_home;
use tracing::info;
use tracing_subscriber::EnvFilter;

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

struct Assets {
    base: PathBuf,
}

impl AssetSource for Assets {
    fn load(&self, path: &str) -> Result<Option<std::borrow::Cow<'static, [u8]>>> {
        std::fs::read(self.base.join(path))
            .map(|data| Some(std::borrow::Cow::Owned(data)))
            .map_err(|err| err.into())
    }

    fn list(&self, path: &str) -> Result<Vec<SharedString>> {
        std::fs::read_dir(self.base.join(path))
            .map(|entries| {
                entries
                    .filter_map(|entry| {
                        entry
                            .ok()
                            .and_then(|entry| entry.file_name().into_string().ok())
                            .map(SharedString::from)
                    })
                    .collect()
            })
            .map_err(|err| err.into())
    }
}

pub fn main() {
    let args = Args::parse();

    // TODO: Allow configuration via ARGON_HOME environment variable.
    if let Some(log_dir) = default_argon_home() {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_env("ARGON_LOG"))
            .with_writer(tracing_appender::rolling::never(log_dir, "gui.log"))
            .with_ansi(false)
            .init();
    }

    Application::new()
        .with_assets(Assets {
            base: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets"),
        })
        .run(move |cx: &mut App| {
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
                KeyBinding::new("s", SelectMode, None),
                KeyBinding::new("d", DrawDim, None),
                KeyBinding::new("f", Fit, None),
                KeyBinding::new("q", Edit, None),
                KeyBinding::new("u", Undo, None),
                KeyBinding::new("ctrl-r", Redo, None),
                KeyBinding::new("0", Zero, None),
                KeyBinding::new("1", One, None),
                KeyBinding::new("*", All, None),
                KeyBinding::new(":", Command, None),
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
            cx.set_menus(vec![
                Menu {
                    name: "Argon".into(),
                    items: vec![MenuItem::action("Quit", Quit)],
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
                        MenuItem::action("Dim", DrawDim),
                        MenuItem::action("Edit", Edit),
                        MenuItem::action("Command Prompt", Command),
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
    info!("Gracefully quitting the application . . .");
    cx.quit();
}
