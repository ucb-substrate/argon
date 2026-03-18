use std::net::SocketAddr;

use clap::Parser;
use tracing::error;
use tracing_subscriber::EnvFilter;

use crate::app::GuiApp;
use lang_server::config::default_argon_home;

mod app;
mod rpc;
mod theme;

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    lang_server_addr: SocketAddr,
}

pub fn main() {
    let args = Args::parse();

    if let Some(log_dir) = default_argon_home() {
        tracing_subscriber::fmt()
            .with_env_filter(EnvFilter::from_env("ARGON_LOG"))
            .with_writer(tracing_appender::rolling::never(log_dir, "gui.log"))
            .with_ansi(false)
            .init();
    }

    let native_options = eframe::NativeOptions {
        viewport: eframe::egui::ViewportBuilder::default()
            .with_inner_size([1600.0, 1000.0])
            .with_min_inner_size([960.0, 640.0])
            .with_title("Argon"),
        ..Default::default()
    };

    if let Err(err) = eframe::run_native(
        "Argon",
        native_options,
        Box::new(move |cc| Ok(Box::new(GuiApp::new(cc, args.lang_server_addr)))),
    ) {
        error!("failed to start gui: {err}");
        std::process::exit(1);
    }
}
