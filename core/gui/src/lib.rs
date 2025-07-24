use std::collections::HashMap;
use std::net::TcpStream;
use std::path::PathBuf;
use std::{borrow::Cow, net::SocketAddr};

use clap::Parser;
use gpui::*;
use itertools::Itertools;
use project::Project;
use socket::GuiToLsp;

use crate::assets::{ZED_PLEX_MONO, ZED_PLEX_SANS};

pub mod assets;
pub mod canvas;
pub mod project;
pub mod socket;
pub mod text;
pub mod theme;
pub mod toolbars;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    file: PathBuf,
    cell: String,
    params: Vec<String>,
    #[arg(long)]
    lsp_addr: Option<SocketAddr>,
}

pub fn main() {
    let args = Args::parse();
    let mut params = HashMap::new();
    for p in args.params {
        let terms = p.split('=').collect_vec();
        assert_eq!(
            terms.len(),
            2,
            "param values must follow `name=value` syntax"
        );
        let v = terms[1]
            .parse()
            .expect("failed to parse param value as i64");
        params.insert(terms[0].to_string(), v);
    }
    let lsp_client = args
        .lsp_addr
        .map(|addr| GuiToLsp::new(TcpStream::connect(addr).unwrap()));

    Application::new().run(|cx: &mut App| {
        // Load fonts.
        cx.text_system()
            .add_fonts(vec![
                Cow::Borrowed(ZED_PLEX_MONO),
                Cow::Borrowed(ZED_PLEX_SANS),
            ])
            .unwrap();
        // Bring the menu bar to the foreground (so you can see the menu bar)
        cx.activate(true);
        // Bind keys must happen before menus to get the keybindings to show up next to menu items.
        cx.bind_keys([KeyBinding::new("cmd-q", Quit, None)]);
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
                ..Default::default()
            },
            |window, cx| {
                window.replace_root(cx, |_window, cx| {
                    Project::new(cx, args.file, args.cell, params, lsp_client)
                })
            },
        )
        .unwrap();
    });
}

// Associate actions using the `actions!` macro (or `impl_actions!` macro)
actions!(Argon, [Quit]);

// Define the quit function that is registered with the App
fn quit(_: &Quit, cx: &mut App) {
    println!("Gracefully quitting the application . . .");
    cx.quit();
}
