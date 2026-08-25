//! Neovim, analyzer, and headless-GUI test scenarios.

use std::{net::Ipv4Addr, path::PathBuf, time::Instant};

use analyzer::rpc::{Gui, InstancePreview, LangServerClient};
use argonc::{
    ast::Span,
    compile::{BasicRect, CompileOutput, CompiledData},
};
use futures::prelude::*;
use tarpc::{context, server::Channel, tokio_serde::formats::Json};
use tempfile::TempDir;
use tokio::{sync::mpsc, time};

use crate::{TEST_TIMEOUT, finish_nvim, nvim_command, repository_root};

// Process-heavy scenarios share ports and startup deadlines, so keep them
// serial even though Cargo runs Rust tests in parallel by default.
static FULL_STACK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum OutputKind {
    Data,
    StaticErrors,
    FatalParseErrors,
}

#[derive(Debug)]
enum GuiEvent {
    OpenCell {
        kind: OutputKind,
        update: bool,
        scope: Option<Span>,
        rect_count: usize,
    },
}

#[derive(Clone)]
struct HeadlessGui {
    events: mpsc::UnboundedSender<GuiEvent>,
}

impl Gui for HeadlessGui {
    async fn open_cell(self, _: context::Context, output: CompileOutput, update: bool) {
        let (kind, data) = match &output {
            CompileOutput::Valid(data) => (OutputKind::Data, Some(data)),
            CompileOutput::ExecErrors(output) => (OutputKind::Data, output.output.as_ref()),
            CompileOutput::StaticErrors(_) => (OutputKind::StaticErrors, None),
            CompileOutput::FatalParseErrors => (OutputKind::FatalParseErrors, None),
        };
        let (scope, rect_count) = data.map(gui_snapshot).unwrap_or((None, 0));
        self.events
            .send(GuiEvent::OpenCell {
                kind,
                update,
                scope,
                rect_count,
            })
            .expect("full-stack test should still be receiving GUI events");
    }

    async fn selected_scope(self, _: context::Context) -> Option<Span> {
        None
    }

    async fn place_instance(self, _: context::Context, _: InstancePreview) {}

    async fn set(self, _: context::Context, _: String, _: String) {}

    async fn activate(self, _: context::Context) {}
}

fn gui_snapshot(data: &CompiledData) -> (Option<Span>, usize) {
    let Some(cell) = data.cells.get(&data.top) else {
        return (None, 0);
    };
    let scope = cell.scopes.get(&cell.root).map(|scope| scope.span.clone());
    let rect_count = cell
        .objects
        .values()
        .filter(|object| object.get_rect().is_some())
        .count();
    (scope, rect_count)
}

struct Session {
    _directory: TempDir,
    project: PathBuf,
    ack: PathBuf,
    gui_edit_ack: PathBuf,
    diagnostic_ack: PathBuf,
    analyzer_port: u16,
    lsp_port: u16,
    lsp_listener: Option<tokio::net::TcpListener>,
    gui_addr: std::net::SocketAddr,
    events: mpsc::UnboundedReceiver<GuiEvent>,
}

impl Session {
    async fn new(source: &str) -> Self {
        let directory = tempfile::tempdir().expect("create full-stack test directory");
        let project = directory.path().join("project");
        std::fs::create_dir(&project).expect("create test project");
        std::fs::write(project.join("lib.ar"), source).expect("write test source");
        std::fs::write(
            project.join("Argon.toml"),
            "name = \"full-stack-test\"\nlyp = \"layers.lyp\"\n",
        )
        .expect("write test manifest");
        std::fs::copy(
            repository_root().join("examples/lyp/basic.lyp"),
            project.join("layers.lyp"),
        )
        .expect("copy test layer properties");

        let analyzer_port = unused_port();
        let lsp_listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind analyzer LSP listener");
        let lsp_port = lsp_listener
            .local_addr()
            .expect("read analyzer LSP address")
            .port();
        let (events_tx, events) = mpsc::unbounded_channel();
        let mut listener =
            tarpc::serde_transport::tcp::listen((Ipv4Addr::LOCALHOST, 0), Json::default)
                .await
                .expect("bind headless GUI RPC listener");
        listener.config_mut().max_frame_length(usize::MAX);
        let gui_addr = listener.local_addr();
        tokio::spawn(async move {
            listener
                .filter_map(|connection| futures::future::ready(connection.ok()))
                .map(tarpc::server::BaseChannel::with_defaults)
                .map(move |channel| {
                    let server = HeadlessGui {
                        events: events_tx.clone(),
                    };
                    channel
                        .execute(server.serve())
                        .for_each(|response| async move {
                            tokio::spawn(response);
                        })
                })
                .buffer_unordered(10)
                .for_each(|_| async {})
                .await;
        });

        let ack = directory.path().join("gui.ack");
        let gui_edit_ack = directory.path().join("gui-edit.ack");
        let diagnostic_ack = directory.path().join("diagnostic.ack");
        Self {
            _directory: directory,
            project,
            ack,
            gui_edit_ack,
            diagnostic_ack,
            analyzer_port,
            lsp_port,
            lsp_listener: Some(lsp_listener),
            gui_addr,
            events,
        }
    }

    fn start_analyzer(&mut self) {
        let listener = self
            .lsp_listener
            .take()
            .expect("analyzer should only be started once");
        let rpc_port = self.analyzer_port;
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept Neovim LSP stream");
            let (reader, writer) = tokio::io::split(stream);
            analyzer::main_with_io(Some(rpc_port), None, reader, writer).await;
        });
    }

    fn spawn_nvim(&self, mode: &str) -> tokio::process::Child {
        let mut command = nvim_command();
        command
            .current_dir(&self.project)
            .env("ARGON_TEST_LSP_PORT", self.lsp_port.to_string())
            .env("ARGON_TEST_ACK", &self.ack)
            .env("ARGON_TEST_GUI_EDIT_ACK", &self.gui_edit_ack)
            .env("ARGON_TEST_DIAGNOSTIC_ACK", &self.diagnostic_ack)
            .env("ARGON_TEST_MODE", mode)
            .arg("--cmd")
            .arg(format!(
                "set runtimepath+={}",
                repository_root().display()
            ))
            .arg("--cmd")
            .arg("lua vim.g.argon={cmd=vim.lsp.rpc.connect('127.0.0.1', tonumber(vim.env.ARGON_TEST_LSP_PORT))}")
            .arg("--cmd")
            .arg("filetype plugin on")
            .arg("lib.ar")
            .arg("-l")
            .arg(repository_root().join("crates/tests/fixtures/nvim/full_stack.lua"));
        command.spawn().expect("start headless Neovim")
    }

    async fn connect_analyzer(&self) -> LangServerClient {
        let deadline = Instant::now() + TEST_TIMEOUT;
        loop {
            let mut transport = tarpc::serde_transport::tcp::connect(
                (Ipv4Addr::LOCALHOST, self.analyzer_port),
                Json::default,
            );
            transport.config_mut().max_frame_length(usize::MAX);
            if let Ok(transport) = transport.await {
                return LangServerClient::new(tarpc::client::Config::default(), transport).spawn();
            }
            assert!(
                Instant::now() < deadline,
                "analyzer RPC server did not start"
            );
            time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    async fn next_event(&mut self) -> GuiEvent {
        time::timeout(TEST_TIMEOUT, self.events.recv())
            .await
            .expect("timed out waiting for GUI event")
            .expect("headless GUI event stream closed")
    }
}

fn unused_port() -> u16 {
    std::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .expect("reserve analyzer port")
        .local_addr()
        .expect("read analyzer port")
        .port()
}

#[tokio::test(flavor = "multi_thread")]
async fn gui_edit_roundtrip() {
    let _guard = FULL_STACK_LOCK.lock().await;
    let mut session = Session::new("cell top() {\n}\n").await;
    session.start_analyzer();
    let child = session.spawn_nvim("roundtrip");
    let analyzer = session.connect_analyzer().await;
    analyzer
        .register(context::current(), session.gui_addr)
        .await
        .expect("register headless GUI");

    let mut drew_rect = false;
    let mut saw_editor_update = false;
    while !saw_editor_update {
        match session.next_event().await {
            GuiEvent::OpenCell {
                kind: OutputKind::Data,
                update,
                scope,
                rect_count,
            } if !drew_rect => {
                let scope = scope.expect("compiled top cell should expose its root scope");
                let inserted = analyzer
                    .draw_rect(
                        context::current(),
                        scope,
                        "gui_rect".to_owned(),
                        BasicRect {
                            layer: Some("met1".to_owned()),
                            x0: 0.0,
                            y0: 0.0,
                            x1: 10.0,
                            y1: 10.0,
                            construction: false,
                        },
                    )
                    .await
                    .expect("GUI draw request should reach analyzer");
                assert!(inserted.is_some(), "GUI draw should edit the source buffer");
                assert!(!update);
                assert_eq!(rect_count, 0);
                drew_rect = true;
            }
            GuiEvent::OpenCell {
                kind: OutputKind::Data,
                update: true,
                rect_count: 1,
                ..
            } => {
                std::fs::write(&session.gui_edit_ack, "ok\n")
                    .expect("acknowledge compiled GUI edit");
            }
            GuiEvent::OpenCell {
                kind: OutputKind::Data,
                update: true,
                rect_count,
                ..
            } if rect_count >= 2 => saw_editor_update = true,
            _ => {}
        }
    }

    std::fs::write(&session.ack, "ok\n").expect("acknowledge GUI observations");
    finish_nvim(child).await;
    let source =
        std::fs::read_to_string(session.project.join("lib.ar")).expect("read round-tripped source");
    assert!(source.contains("let gui_rect = rect("));
    assert!(source.contains("let editor_rect = rect("));
}

#[tokio::test(flavor = "multi_thread")]
async fn diagnostic_recovery() {
    let _guard = FULL_STACK_LOCK.lock().await;
    let mut session = Session::new("cell top() {\n    missing;\n}\n").await;
    session.start_analyzer();
    let child = session.spawn_nvim("diagnostics");
    let analyzer = session.connect_analyzer().await;
    analyzer
        .register(context::current(), session.gui_addr)
        .await
        .expect("register headless GUI");

    let mut saw_errors = false;
    let mut saw_recovery = false;
    while !saw_recovery {
        match session.next_event().await {
            GuiEvent::OpenCell {
                kind: OutputKind::StaticErrors,
                ..
            } => {
                saw_errors = true;
                std::fs::write(&session.diagnostic_ack, "ok\n")
                    .expect("acknowledge GUI diagnostics");
            }
            GuiEvent::OpenCell {
                kind: OutputKind::Data,
                ..
            } if saw_errors => saw_recovery = true,
            _ => {}
        }
    }

    std::fs::write(&session.ack, "ok\n").expect("acknowledge diagnostic recovery");
    finish_nvim(child).await;
}
