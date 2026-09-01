//! Neovim, analyzer, and headless-GUI test scenarios.

use std::{net::Ipv4Addr, path::PathBuf};

use analyzer::{
    ArgonConfig,
    rpc::{CompilationSnapshot, Gui, InstancePreview, LangServerClient},
};
use argonc::{
    ast::Span,
    compile::{CompileOutput, CompiledData},
};
use futures::prelude::*;
use tarpc::{context, server::Channel, tokio_serde::formats::Bincode};
use tempfile::TempDir;
use tokio::{sync::mpsc, time};

use crate::{TEST_TIMEOUT, nvim_command, repository_root};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    Data,
    StaticErrors,
    FatalParseErrors,
}

#[derive(Debug)]
pub enum GuiEvent {
    CompilationStarted(u64),
    CompilationFinished(u64),
    UpdateCell {
        revision: u64,
        kind: OutputKind,
        scope: Option<Span>,
        rect_count: usize,
    },
    Message {
        typ: tower_lsp_server::ls_types::MessageType,
        message: String,
    },
    Fit,
    WorkspacePath(Option<PathBuf>),
    WorkspaceModified(bool),
}

#[derive(Clone)]
struct HeadlessGui {
    events: mpsc::UnboundedSender<GuiEvent>,
}

impl Gui for HeadlessGui {
    async fn compilation_started(self, _: context::Context, activity_id: u64) {
        self.events
            .send(GuiEvent::CompilationStarted(activity_id))
            .expect("full-stack test should still be receiving GUI events");
    }

    async fn compilation_finished(self, _: context::Context, activity_id: u64) {
        self.events
            .send(GuiEvent::CompilationFinished(activity_id))
            .expect("full-stack test should still be receiving GUI events");
    }

    async fn update_cell(self, _: context::Context, snapshot: CompilationSnapshot) {
        let (kind, scope, rect_count) = snapshot_details(&snapshot.output);
        self.events
            .send(GuiEvent::UpdateCell {
                revision: snapshot.revision,
                kind,
                scope,
                rect_count,
            })
            .expect("full-stack test should still be receiving GUI events");
    }

    async fn show_message(
        self,
        _: context::Context,
        typ: tower_lsp_server::ls_types::MessageType,
        message: String,
    ) {
        self.events
            .send(GuiEvent::Message { typ, message })
            .expect("full-stack test should still be receiving GUI events");
    }

    async fn fit(self, _: context::Context) {
        self.events
            .send(GuiEvent::Fit)
            .expect("full-stack test should still be receiving GUI events");
    }

    async fn set_workspace_path(self, _: context::Context, path: Option<PathBuf>) {
        self.events
            .send(GuiEvent::WorkspacePath(path))
            .expect("full-stack test should still be receiving GUI events");
    }

    async fn workspace_modified(self, _: context::Context, modified: bool) {
        self.events
            .send(GuiEvent::WorkspaceModified(modified))
            .expect("full-stack test should still be receiving GUI events");
    }

    async fn selected_scope(self, _: context::Context) -> Option<Span> {
        None
    }

    async fn place_instance(self, _: context::Context, _: InstancePreview) {}

    async fn configure(self, _: context::Context, _: ArgonConfig) {}

    async fn activate(self, _: context::Context) {}
}

fn snapshot_details(output: &CompileOutput) -> (OutputKind, Option<Span>, usize) {
    let (kind, data) = match output {
        CompileOutput::Valid(data) => (OutputKind::Data, Some(data)),
        CompileOutput::ExecErrors(output) => (OutputKind::Data, output.output.as_ref()),
        CompileOutput::StaticErrors(_) => (OutputKind::StaticErrors, None),
        CompileOutput::FatalParseErrors => (OutputKind::FatalParseErrors, None),
    };
    let (scope, rect_count) = data.map(gui_snapshot).unwrap_or((None, 0));
    (kind, scope, rect_count)
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

pub struct Session {
    _directory: TempDir,
    project: PathBuf,
    ack: PathBuf,
    gui_edit_ack: PathBuf,
    diagnostic_ack: PathBuf,
    analyzer_addr: std::net::SocketAddr,
    analyzer_listener: Option<tokio::net::TcpListener>,
    lsp_port: u16,
    lsp_listener: Option<tokio::net::TcpListener>,
    gui_addr: std::net::SocketAddr,
    events: mpsc::UnboundedReceiver<GuiEvent>,
}

impl Session {
    pub async fn new(source: &str) -> Self {
        let directory = tempfile::tempdir().expect("create full-stack test directory");
        let project = directory.path().join("project");
        std::fs::create_dir(&project).expect("create test project");
        std::fs::write(project.join("lib.ar"), source).expect("write test source");
        std::fs::write(
            project.join("Argon.toml"),
            "name = \"full-stack-test\"\ntech = \"tech.toml\"\n",
        )
        .expect("write test manifest");
        std::fs::copy(
            repository_root().join("examples/tech/basic.tech.toml"),
            project.join("tech.toml"),
        )
        .expect("copy test technology");

        let analyzer_listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind analyzer GUI RPC listener");
        let analyzer_addr = analyzer_listener
            .local_addr()
            .expect("read analyzer GUI RPC address");
        let lsp_listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
            .await
            .expect("bind analyzer LSP listener");
        let lsp_port = lsp_listener
            .local_addr()
            .expect("read analyzer LSP address")
            .port();
        let (events_tx, events) = mpsc::unbounded_channel();
        let mut listener =
            tarpc::serde_transport::tcp::listen((Ipv4Addr::LOCALHOST, 0), Bincode::default)
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
            analyzer_addr,
            analyzer_listener: Some(analyzer_listener),
            lsp_port,
            lsp_listener: Some(lsp_listener),
            gui_addr,
            events,
        }
    }

    pub fn start_analyzer(&mut self) {
        let listener = self
            .lsp_listener
            .take()
            .expect("analyzer should only be started once");
        let rpc_listener = self
            .analyzer_listener
            .take()
            .expect("analyzer should only be started once");
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept Neovim LSP stream");
            let (reader, writer) = tokio::io::split(stream);
            analyzer::main_with_io_on_listener(rpc_listener, None, reader, writer).await;
        });
    }

    pub fn gui_addr(&self) -> std::net::SocketAddr {
        self.gui_addr
    }

    pub fn spawn_nvim(&self, mode: &str) -> tokio::process::Child {
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

    pub async fn connect_analyzer(&self) -> LangServerClient {
        time::timeout(TEST_TIMEOUT, async {
            loop {
                let mut transport =
                    tarpc::serde_transport::tcp::connect(self.analyzer_addr, Bincode::default);
                transport.config_mut().max_frame_length(usize::MAX);
                if let Ok(transport) = transport.await {
                    return LangServerClient::new(tarpc::client::Config::default(), transport)
                        .spawn();
                }
                time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("analyzer RPC server did not start")
    }

    pub async fn next_event(&mut self) -> GuiEvent {
        self.events
            .recv()
            .await
            .expect("headless GUI event stream closed")
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashSet, future::Future};

    use argonc::compile::BasicRect;

    use super::*;
    use crate::finish_nvim;

    // Process-heavy scenarios share ports and startup deadlines, so keep them
    // serial even though Cargo runs Rust tests in parallel by default.
    static FULL_STACK_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

    async fn assert_completes(description: &str, future: impl Future<Output = ()>) {
        time::timeout(TEST_TIMEOUT, future)
            .await
            .unwrap_or_else(|_| panic!("timed out {description}"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn gui_edit_roundtrip() {
        assert_completes("waiting for GUI/editor round trip", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new("cell top() {\n}\n").await;
            session.start_analyzer();
            let child = session.spawn_nvim("roundtrip");
            let analyzer = session.connect_analyzer().await;
            analyzer
                .register(context::current(), session.gui_addr())
                .await
                .expect("register headless GUI");

            let mut drew_rect = false;
            let mut saw_editor_update = false;
            let mut saw_workspace_modified = false;
            let mut saw_workspace_path = false;
            let mut saw_fit = false;
            let mut active_compilations = HashSet::new();
            let mut saw_compilation_update = false;
            let mut saw_compilation_finish = false;
            let mut opened_revision = None;
            while !(saw_editor_update
                && saw_workspace_modified
                && saw_workspace_path
                && saw_fit
                && saw_compilation_update
                && saw_compilation_finish)
            {
                match session.next_event().await {
                    GuiEvent::CompilationStarted(activity_id) => {
                        active_compilations.insert(activity_id);
                    }
                    GuiEvent::CompilationFinished(activity_id) => {
                        saw_compilation_finish |= active_compilations.remove(&activity_id);
                    }
                    GuiEvent::UpdateCell {
                        revision,
                        kind: OutputKind::Data,
                        scope,
                        rect_count,
                    } if !drew_rect => {
                        saw_compilation_update |= !active_compilations.is_empty();
                        let scope = scope.expect("compiled top cell should expose its root scope");
                        let inserted = analyzer
                            .draw_rect(
                                context::current(),
                                scope,
                                "gui_rect".to_owned(),
                                BasicRect {
                                    layer: Some("met1".to_owned()),
                                    x0: 1.2000000476837158,
                                    y0: -0.04,
                                    x1: 10.349,
                                    y1: 10.0,
                                    construction: false,
                                },
                            )
                            .await
                            .expect("GUI draw request should reach analyzer");
                        assert!(inserted.is_some(), "GUI draw should edit the source buffer");
                        assert_eq!(rect_count, 0);
                        opened_revision = Some(revision);
                        drew_rect = true;
                    }
                    GuiEvent::UpdateCell {
                        revision,
                        kind: OutputKind::Data,
                        rect_count: 1,
                        ..
                    } => {
                        saw_compilation_update |= !active_compilations.is_empty();
                        assert!(Some(revision) > opened_revision);
                        std::fs::write(&session.gui_edit_ack, "ok\n")
                            .expect("acknowledge compiled GUI edit");
                    }
                    GuiEvent::UpdateCell {
                        kind: OutputKind::Data,
                        rect_count,
                        ..
                    } if rect_count >= 2 => {
                        saw_compilation_update |= !active_compilations.is_empty();
                        saw_editor_update = true;
                    }
                    GuiEvent::Fit => saw_fit = true,
                    GuiEvent::WorkspacePath(Some(path)) => {
                        assert_eq!(
                            path.canonicalize()
                                .expect("canonicalize GUI workspace path"),
                            session
                                .project
                                .canonicalize()
                                .expect("canonicalize test workspace path")
                        );
                        saw_workspace_path = true;
                    }
                    GuiEvent::WorkspaceModified(true) => saw_workspace_modified = true,
                    _ => {}
                }
            }

            std::fs::write(&session.ack, "ok\n").expect("acknowledge GUI observations");
            finish_nvim(child).await;
            let source = std::fs::read_to_string(session.project.join("lib.ar"))
                .expect("read round-tripped source");
            assert!(source.contains("let gui_rect = rect("));
            assert!(source.contains("x0i = 1.2, y0i = 0., x1i = 10.3, y1i = 10."));
            assert!(source.contains("let editor_rect = rect("));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn diagnostic_recovery() {
        assert_completes("waiting for diagnostic recovery", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new("cell top() {\n    missing;\n}\n").await;
            session.start_analyzer();
            let child = session.spawn_nvim("diagnostics");
            let analyzer = session.connect_analyzer().await;
            analyzer
                .register(context::current(), session.gui_addr())
                .await
                .expect("register headless GUI");

            let mut saw_errors = false;
            let mut saw_recovery = false;
            while !saw_recovery {
                match session.next_event().await {
                    GuiEvent::UpdateCell {
                        kind: OutputKind::StaticErrors,
                        ..
                    } => {
                        saw_errors = true;
                        std::fs::write(&session.diagnostic_ack, "ok\n")
                            .expect("acknowledge GUI diagnostics");
                    }
                    GuiEvent::UpdateCell {
                        kind: OutputKind::Data,
                        ..
                    } if saw_errors => saw_recovery = true,
                    _ => {}
                }
            }

            std::fs::write(&session.ack, "ok\n").expect("acknowledge diagnostic recovery");
            finish_nvim(child).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn definitions_and_references_are_served_to_neovim() {
        assert_completes("waiting for navigation requests", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new(
                "cell top() {\n    let width = 100.;\n    let r = rect(\"met1\", x0=0., y0=0., x1=width, y1=width);\n}\n",
            )
            .await;
            session.start_analyzer();
            let child = session.spawn_nvim("navigation");
            let analyzer = session.connect_analyzer().await;
            analyzer
                .register(context::current(), session.gui_addr())
                .await
                .expect("register headless GUI");

            std::fs::write(&session.ack, "ok\n").expect("acknowledge navigation");
            finish_nvim(child).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn analyzer_errors_are_mirrored_to_the_gui() {
        assert_completes("waiting for analyzer error in GUI", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new("cell top() {\n}\n").await;
            session.start_analyzer();
            let child = session.spawn_nvim("rpc_errors");
            let analyzer = session.connect_analyzer().await;
            analyzer
                .register(context::current(), session.gui_addr())
                .await
                .expect("register headless GUI");
            analyzer
                .open_cell(context::current(), "top(".to_owned())
                .await
                .expect("invalid open-cell request should reach analyzer");

            loop {
                if let GuiEvent::Message { typ, message } = session.next_event().await
                    && typ == tower_lsp_server::ls_types::MessageType::ERROR
                    && message.contains("Open cell is invalid")
                {
                    break;
                }
            }

            std::fs::write(&session.ack, "ok\n").expect("acknowledge mirrored GUI error");
            finish_nvim(child).await;
        })
        .await;
    }
}
