//! Neovim, analyzer, and headless-GUI test scenarios.

use std::{net::Ipv4Addr, path::PathBuf, sync::Arc};

use analyzer::{
    ArgonConfig,
    agent::AgentClient,
    rpc::{CompilationSnapshot, CompilationUpdate, Gui, InstancePreview, LangServerClient},
    transport::{self, Role, SessionToken},
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
        object_count: usize,
    },
    Message {
        typ: tower_lsp_server::ls_types::MessageType,
        message: String,
    },
    Fit,
    WorkspacePath(Option<PathBuf>),
    WorkspaceModified(bool),
    Highlight {
        revision: u64,
        spans: Vec<Span>,
    },
    AgentActivity(Option<String>),
}

#[derive(Clone)]
struct HeadlessGui {
    events: mpsc::UnboundedSender<GuiEvent>,
    snapshot: Arc<std::sync::Mutex<Option<CompilationSnapshot>>>,
    update_gate: Arc<tokio::sync::Semaphore>,
    /// The last compiled cell's scope, used for selection-dependent requests.
    selected_scope: Arc<std::sync::Mutex<Option<Span>>>,
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

    async fn update_cell(self, _: context::Context, update: CompilationUpdate) -> bool {
        let (kind, scope, rect_count, object_count, revision) = {
            let mut previous = self.snapshot.lock().unwrap();
            let Some(snapshot) = update.materialize(previous.as_ref()) else {
                return false;
            };
            *previous = Some(snapshot.clone());
            let (kind, scope, rect_count, object_count) = snapshot_details(&snapshot.output);
            (kind, scope, rect_count, object_count, snapshot.revision)
        };
        if let Some(scope) = scope.clone() {
            *self.selected_scope.lock().expect("selected scope") = Some(scope);
        }

        self.events
            .send(GuiEvent::UpdateCell {
                revision,
                kind,
                scope,
                rect_count,
                object_count,
            })
            .expect("full-stack test should still be receiving GUI events");
        let _permit = self.update_gate.acquire().await.unwrap();
        true
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
        self.selected_scope.lock().expect("selected scope").clone()
    }

    async fn place_instance(self, _: context::Context, _: InstancePreview) {}

    async fn configure(self, _: context::Context, _: ArgonConfig) {}

    async fn activate(self, _: context::Context) {}

    async fn highlight_spans(self, _: context::Context, revision: u64, spans: Vec<Span>) {
        self.events
            .send(GuiEvent::Highlight { revision, spans })
            .expect("full-stack test should still be receiving GUI events");
    }

    async fn agent_activity(self, _: context::Context, label: Option<String>) {
        self.events
            .send(GuiEvent::AgentActivity(label))
            .expect("full-stack test should still be receiving GUI events");
    }
}

fn snapshot_details(output: &CompileOutput) -> (OutputKind, Option<Span>, usize, usize) {
    let (kind, data) = match output {
        CompileOutput::Valid(data) => (OutputKind::Data, Some(data)),
        CompileOutput::ExecErrors(output) => (OutputKind::Data, output.output.as_ref()),
        CompileOutput::StaticErrors(_) => (OutputKind::StaticErrors, None),
        CompileOutput::FatalParseErrors => (OutputKind::FatalParseErrors, None),
    };
    let (scope, rect_count, object_count) = data.map(gui_snapshot).unwrap_or((None, 0, 0));
    (kind, scope, rect_count, object_count)
}

/// The top cell's root scope, its rect count, and its total object count.
fn gui_snapshot(data: &CompiledData) -> (Option<Span>, usize, usize) {
    let Some(cell) = data.cells.get(&data.top) else {
        return (None, 0, 0);
    };
    let scope = cell.scopes.get(&cell.root).map(|scope| scope.span.clone());
    let rect_count = cell
        .objects
        .values()
        .filter(|object| object.get_rect().is_some())
        .count();
    (scope, rect_count, cell.objects.len())
}

pub struct Session {
    _directory: TempDir,
    project: PathBuf,
    ack: PathBuf,
    gui_edit_ack: PathBuf,
    diagnostic_ack: PathBuf,
    steps: PathBuf,
    analyzer_addr: std::net::SocketAddr,
    analyzer_listener: Option<tokio::net::TcpListener>,
    token: SessionToken,
    lsp_port: u16,
    lsp_listener: Option<tokio::net::TcpListener>,
    gui: HeadlessGui,
    events: mpsc::UnboundedReceiver<GuiEvent>,
    #[cfg(test)]
    update_gate: std::sync::Arc<tokio::sync::Semaphore>,
}

impl Session {
    pub async fn new(source: &str) -> Self {
        Self::with_files(source, &[]).await
    }

    /// A project whose `lib.ar` is `source`, alongside the other `files`.
    pub async fn with_files(source: &str, files: &[(&str, &str)]) -> Self {
        let directory = tempfile::tempdir().expect("create full-stack test directory");
        let project = directory.path().join("project");
        std::fs::create_dir(&project).expect("create test project");
        std::fs::write(project.join("lib.ar"), source).expect("write test source");
        for (name, contents) in files {
            std::fs::write(project.join(name), contents).expect("write test module");
        }
        let steps = directory.path().join("steps");
        std::fs::create_dir(&steps).expect("create test step directory");
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
        let update_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let gui = HeadlessGui {
            events: events_tx,
            snapshot: Default::default(),
            update_gate: update_gate.clone(),
            selected_scope: Arc::new(std::sync::Mutex::new(None)),
        };

        let ack = directory.path().join("gui.ack");
        let gui_edit_ack = directory.path().join("gui-edit.ack");
        let diagnostic_ack = directory.path().join("diagnostic.ack");
        Self {
            _directory: directory,
            project,
            ack,
            gui_edit_ack,
            diagnostic_ack,
            steps,
            analyzer_addr,
            analyzer_listener: Some(analyzer_listener),
            token: SessionToken::generate().expect("generate a session token"),
            lsp_port,
            lsp_listener: Some(lsp_listener),
            gui,
            events,
            #[cfg(test)]
            update_gate,
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
        let token = self.token.clone();
        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept Neovim LSP stream");
            let (reader, writer) = tokio::io::split(stream);
            analyzer::main_with_io_on_listener(rpc_listener, token, None, reader, writer).await;
        });
    }

    pub fn project(&self) -> &std::path::Path {
        &self.project
    }

    async fn connect(&self, role: Role) -> transport::AuthenticatedStream {
        time::timeout(TEST_TIMEOUT, async {
            loop {
                if let Ok(stream) = transport::connect(self.analyzer_addr, &self.token, role).await
                {
                    return stream;
                }
                time::sleep(std::time::Duration::from_millis(25)).await;
            }
        })
        .await
        .expect("analyzer RPC server did not start")
    }

    /// Connects the headless GUI back to the analyzer for callbacks, as the
    /// real GUI does once its window is up.
    pub async fn connect_gui(&self) {
        let stream = self.connect(Role::GuiCallback).await;
        let gui = self.gui.clone();
        tokio::spawn(async move {
            let transport = tarpc::serde_transport::new(stream, Bincode::default());
            tarpc::server::BaseChannel::with_defaults(transport)
                .execute(gui.serve())
                .for_each(|response| async move {
                    tokio::spawn(response);
                })
                .await;
        });
    }

    pub async fn connect_agent(&self) -> AgentClient {
        let stream = self.connect(Role::Agent).await;
        let transport = tarpc::serde_transport::new(stream, Bincode::default());
        AgentClient::new(tarpc::client::Config::default(), transport).spawn()
    }

    pub fn spawn_nvim(&self, mode: &str) -> tokio::process::Child {
        let mut command = nvim_command();
        command
            .current_dir(&self.project)
            .env("ARGON_TEST_LSP_PORT", self.lsp_port.to_string())
            .env("ARGON_TEST_ACK", &self.ack)
            .env("ARGON_TEST_GUI_EDIT_ACK", &self.gui_edit_ack)
            .env("ARGON_TEST_DIAGNOSTIC_ACK", &self.diagnostic_ack)
            .env("ARGON_TEST_STEPS", &self.steps)
            .env("ARGON_TEST_MODE", mode)
            .env("ARGON_TEST_READY", self.project.join("startup.ready"))
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
        let stream = self.connect(Role::Gui).await;
        let transport = tarpc::serde_transport::new(stream, Bincode::default());
        LangServerClient::new(tarpc::client::Config::default(), transport).spawn()
    }

    /// Tells the Neovim fixture that step `name` happened.
    pub fn signal(&self, name: &str) {
        std::fs::write(self.steps.join(name), "ok\n").expect("signal a test step");
    }

    /// Waits for the Neovim fixture to signal step `name`.
    pub async fn await_signal(&self, name: &str) {
        let path = self.steps.join(name);
        time::timeout(TEST_TIMEOUT, async {
            while !path.exists() {
                time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("Neovim never signalled {name}"));
    }

    /// Waits for a GUI event matching `predicate`, dropping the others.
    pub async fn wait_for_event(
        &mut self,
        mut predicate: impl FnMut(&GuiEvent) -> bool,
    ) -> GuiEvent {
        loop {
            let event = self.next_event().await;
            if predicate(&event) {
                return event;
            }
        }
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

    use analyzer::rpc::{DimensionParams, DrawSegmentConstraint, PathParams, PolygonParams};
    use argonc::compile::BasicRect;

    use super::*;
    use crate::finish_nvim;
    use analyzer::{
        ApprovalMode,
        agent::{CALL_DEADLINE, CompileStatus, CreateRequest, EditRequest, Replacement},
    };
    use serde_json::{Value, json};
    use tower_lsp_server::ls_types::MessageType;

    const FIRST_RECT: &str = "let a = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.);";
    const SECOND_RECT: &str = "let b = rect(\"met1\", x0=20., y0=0., x1=30., y1=10.);";
    const UNIT: &str = "use std::layout::rect;\n\ncell unit() {\n    let body = rect(\"met1\", x0=0., y0=0., x1=5., y1=5.);\n}\n";

    fn one_rect() -> String {
        format!("use std::layout::rect;\n\ncell top() {{\n    {FIRST_RECT}\n}}\n")
    }

    fn with_unit() -> String {
        format!(
            "mod utils;\nuse std::layout::{{inst, rect}};\n\ncell top() {{\n    let unit = inst(utils::unit());\n    eq(unit.x, 0.);\n    eq(unit.y, 0.);\n    {FIRST_RECT}\n}}\n"
        )
    }

    fn agent_context() -> context::Context {
        let mut context = context::current();
        context.deadline = std::time::Instant::now() + CALL_DEADLINE;
        context
    }

    fn replace(path: &str, old: &str, new: &str, label: &str) -> EditRequest {
        EditRequest {
            path: PathBuf::from(path),
            edits: vec![Replacement {
                old: old.to_owned(),
                new: new.to_owned(),
                replace_all: false,
            }],
            label: label.to_owned(),
        }
    }

    fn add_rect(statement: &str, label: &str) -> EditRequest {
        replace(
            "lib.ar",
            FIRST_RECT,
            &format!("{FIRST_RECT}\n    {statement}"),
            label,
        )
    }

    impl Session {
        async fn wait_for_rects(&mut self, count: usize) -> u64 {
            let GuiEvent::UpdateCell { revision, .. } = self
                .wait_for_event(|event| {
                    matches!(
                        event,
                        GuiEvent::UpdateCell { kind: OutputKind::Data, rect_count, .. }
                            if *rect_count == count
                    )
                })
                .await
            else {
                unreachable!()
            };
            revision
        }
    }

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
            session.connect_gui().await;

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
                        ..
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
            assert!(source.contains("pub let gui_rect = std::layout::rect("));
            assert!(source.contains("x0i = 1.2, y0i = 0., x1i = 10.3, y1i = 10."));
            assert!(source.contains("let editor_rect = std::layout::rect("));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn gui_connection_does_not_wait_for_initial_error_presentation() {
        assert_completes("registering GUI with initial source errors", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new("cell top() { missing; }\n").await;
            let gate = session.update_gate.clone().acquire_owned().await.unwrap();
            session.start_analyzer();
            let child = session.spawn_nvim("startup_errors");
            while !session.project.join("startup.ready").exists() {
                time::sleep(std::time::Duration::from_millis(10)).await;
            }
            // Connecting must not wait on the first presentation, including
            // when no cell is selected.
            time::timeout(std::time::Duration::from_secs(1), session.connect_gui())
                .await
                .expect("GUI connection waited for presentation");
            loop {
                if matches!(
                    session.next_event().await,
                    GuiEvent::UpdateCell {
                        kind: OutputKind::StaticErrors,
                        ..
                    }
                ) {
                    break;
                }
            }
            drop(gate);
            std::fs::write(&session.ack, "ok\n").unwrap();
            finish_nvim(child).await;
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
            session.connect_gui().await;

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
                "use std::layout::rect;\ncell top() {\n    let width = 100.;\n    let r = rect(\"met1\", x0=0., y0=0., x1=width, y1=width);\n}\ncell child(w: Float, h: Float = w, layer: String = \"met1\") {\n    let body = rect(layer, x0=0., y0=0., x1=w, y1=h);\n}\n",
            )
            .await;
            session.start_analyzer();
            let child = session.spawn_nvim("navigation");
            session.connect_gui().await;

            std::fs::write(&session.ack, "ok\n").expect("acknowledge navigation");
            finish_nvim(child).await;
        })
        .await;
    }

    /// Every statement a GUI tool inserts names layout natives by full path,
    /// so it compiles in a module without `use` declarations.
    #[tokio::test(flavor = "multi_thread")]
    async fn generated_geometry_compiles_without_imports() {
        assert_completes("waiting for generated geometry to compile", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new("cell child() {}\ncell top() {\n}\n").await;
            session.start_analyzer();
            let child = session.spawn_nvim("generated");
            let analyzer = session.connect_analyzer().await;
            session.connect_gui().await;

            // Each edit is made against the scope of the snapshot compiled
            // from the previous one, as the GUI would. Every edit adds one
            // object to the top cell, so `step` objects precede edit `step`.
            let mut last_revision = None;
            for step in 0..=5_usize {
                let scope = loop {
                    match session.next_event().await {
                        GuiEvent::UpdateCell {
                            revision,
                            kind: OutputKind::Data,
                            scope: Some(scope),
                            object_count,
                            ..
                        } if Some(revision) > last_revision && object_count == step => {
                            last_revision = Some(revision);
                            break scope;
                        }
                        GuiEvent::UpdateCell {
                            kind: kind @ (OutputKind::StaticErrors | OutputKind::FatalParseErrors),
                            ..
                        } => panic!("generated source should compile, got {kind:?}"),
                        _ => {}
                    }
                };
                let inserted = match step {
                    0 => analyzer
                        .draw_rect(
                            context::current(),
                            scope,
                            "r".to_owned(),
                            BasicRect {
                                layer: Some("met1".to_owned()),
                                x0: 0.,
                                y0: 0.,
                                x1: 10.,
                                y1: 20.,
                                construction: false,
                            },
                        )
                        .await
                        .map(|result| result.is_some()),
                    1 => analyzer
                        .draw_polygon(
                            context::current(),
                            scope,
                            "polygon0".to_owned(),
                            PolygonParams {
                                layer: "met1".to_owned(),
                                points: vec![(20., 0.), (30., 0.), (25., 10.)],
                                constraints: vec![DrawSegmentConstraint::Horizontal(1)],
                            },
                        )
                        .await
                        .map(|span| span.is_some()),
                    2 => analyzer
                        .draw_path(
                            context::current(),
                            scope,
                            "path0".to_owned(),
                            PathParams {
                                layer: "met1".to_owned(),
                                width: 2.,
                                points: vec![(40., 0.), (40., 20.)],
                                constraints: vec![DrawSegmentConstraint::Vertical(1)],
                            },
                        )
                        .await
                        .map(|span| span.is_some()),
                    3 => analyzer
                        .place_instance(context::current(), scope, "child()".to_owned(), 50., 0.)
                        .await
                        .map(|span| span.is_some()),
                    4 => analyzer
                        .draw_dimension(
                            context::current(),
                            scope,
                            DimensionParams {
                                p: "r.x1".to_owned(),
                                n: "r.x0".to_owned(),
                                value: "10.".to_owned(),
                                coord: "r.y1 + 5.".to_owned(),
                                pstop: "r.y1".to_owned(),
                                nstop: "r.y1".to_owned(),
                                horiz: "true".to_owned(),
                            },
                        )
                        .await
                        .map(|span| span.is_some()),
                    _ => break,
                };
                assert!(
                    inserted.expect("GUI edit request should reach analyzer"),
                    "edit {step} should change the source buffer"
                );
            }

            std::fs::write(&session.gui_edit_ack, "ok\n").expect("acknowledge generated edits");
            std::fs::write(&session.ack, "ok\n").expect("acknowledge generated geometry");
            finish_nvim(child).await;
            let path = session.project.join("lib.ar");
            let source = std::fs::read_to_string(&path).expect("read generated source");
            assert!(!source.contains("use "), "{source}");
            for call in [
                "std::layout::rect(",
                "std::layout::polygon(",
                "std::layout::path(",
                "std::layout::inst(child()",
                "std::layout::dimension(",
            ] {
                assert!(source.contains(call), "missing {call} in\n{source}");
            }
            let ast = argonc::parse::parse_workspace_with_std(&path).ast();
            let (_, errors) =
                argonc::compile::static_compile(&ast).expect("generated source should analyze");
            assert!(errors.errors.is_empty(), "{source}\n{:?}", errors.errors);
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
            session.connect_gui().await;
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

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_edits_land_in_neovim_and_the_gui() {
        assert_completes("waiting for an agent edit round trip", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new(&one_rect()).await;
            session.start_analyzer();
            let child = session.spawn_nvim("agent_edit");
            session.connect_gui().await;
            session.wait_for_rects(1).await;
            let agent = session.connect_agent().await;

            let status = agent.status(agent_context()).await.unwrap();
            assert!(status.gui_connected);
            assert!(!status.follow_mode);
            assert_eq!(status.approval, ApprovalMode::Never);
            assert_eq!(status.open_cell.as_deref(), Some("top()"));
            assert_eq!(status.open_files, vec![PathBuf::from("lib.ar")]);

            let report = agent
                .edit_file(agent_context(), add_rect(SECOND_RECT, "add a second rect"))
                .await
                .unwrap()
                .expect("the agent edit should apply");
            assert!(report.compiled_revision >= report.revision);
            let summary = report.summary.clone().expect("the edit should compile");
            assert_eq!(summary.status, CompileStatus::Valid);
            assert_eq!(summary.rects, 2);
            assert_eq!(summary.bbox, Some([0., 0., 30., 10.]));
            assert!(report.diagnostics.is_empty(), "{:?}", report.diagnostics);

            let file = agent
                .read_file(agent_context(), PathBuf::from("lib.ar"))
                .await
                .unwrap()
                .unwrap();
            assert!(file.from_editor);
            assert!(file.contents.contains(SECOND_RECT));
            let disk = std::fs::read_to_string(session.project().join("lib.ar")).unwrap();
            assert!(!disk.contains(SECOND_RECT), "agent edits must stay unsaved");

            let (mut activity, mut geometry, mut highlight, mut idle) =
                (false, false, false, false);
            while !(activity && geometry && highlight && idle) {
                match session.next_event().await {
                    GuiEvent::AgentActivity(Some(label)) => {
                        assert_eq!(label, "add a second rect");
                        activity = true;
                    }
                    GuiEvent::AgentActivity(None) => idle = activity,
                    GuiEvent::UpdateCell {
                        kind: OutputKind::Data,
                        rect_count: 2,
                        ..
                    } => geometry = true,
                    GuiEvent::Highlight { revision, spans } => {
                        assert_eq!(revision, report.revision);
                        let [span] = spans.as_slice() else {
                            panic!("expected one highlighted span, got {spans:?}");
                        };
                        assert!(span.path.ends_with("lib.ar"));
                        assert_eq!(
                            &file.contents[span.span.start()..span.span.end()],
                            format!("{FIRST_RECT}\n    {SECOND_RECT}")
                        );
                        highlight = true;
                    }
                    _ => {}
                }
            }

            session.signal("undo");
            session.await_signal("undone").await;
            session.wait_for_rects(1).await;
            std::fs::write(&session.ack, "ok\n").unwrap();
            finish_nvim(child).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_edits_closed_files_and_reports_problems() {
        assert_completes("waiting for agent edits to closed files", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::with_files(&with_unit(), &[("utils.ar", UNIT)]).await;
            session.start_analyzer();
            let child = session.spawn_nvim("agent_hidden");
            session.connect_gui().await;
            session.wait_for_rects(1).await;
            let agent = session.connect_agent().await;

            let report = agent
                .edit_file(
                    agent_context(),
                    replace("utils.ar", "x1=5.", "x1=17.", "widen the unit"),
                )
                .await
                .unwrap()
                .expect("editing a closed file should load it");
            assert_eq!(report.summary.unwrap().bbox, Some([0., 0., 17., 10.]));
            session.signal("edited_hidden");

            agent
                .create_file(
                    agent_context(),
                    CreateRequest {
                        path: PathBuf::from("extra.ar"),
                        contents: "cell extra() {\n}\n".to_owned(),
                        label: "add a module".to_owned(),
                    },
                )
                .await
                .unwrap()
                .expect("creating a file should open an unsaved buffer");
            session.signal("created");
            session.await_signal("hidden_checked").await;

            let probe = agent
                .compile_cell(agent_context(), "utils::unit()".to_owned())
                .await
                .unwrap()
                .unwrap();
            let summary = probe.summary.unwrap();
            assert_eq!(summary.status, CompileStatus::Valid);
            assert_eq!(summary.bbox, Some([0., 0., 17., 5.]));
            let status = agent.status(agent_context()).await.unwrap();
            assert_eq!(
                status.open_cell.as_deref(),
                Some("top()"),
                "probing a cell must not change what the GUI shows"
            );

            let broken = agent
                .edit_file(
                    agent_context(),
                    replace("utils.ar", "x1=17.", "x1=missing", "break it"),
                )
                .await
                .unwrap()
                .expect("an edit that does not compile should still apply");
            assert!(
                broken.diagnostics.iter().any(|diagnostic| {
                    diagnostic.path == std::path::Path::new("utils.ar")
                        && diagnostic.line == 4
                        && diagnostic.message.contains("missing")
                }),
                "{:?}",
                broken.diagnostics
            );

            let error = |result: Result<analyzer::agent::Report, String>| {
                result.expect_err("the edit should be refused")
            };
            let refused = |request| {
                let agent = agent.clone();
                async move { error(agent.edit_file(agent_context(), request).await.unwrap()) }
            };
            assert!(
                refused(replace("lib.ar", "0.", "1.", "ambiguous"))
                    .await
                    .contains("occurs")
            );
            assert!(
                refused(replace("lib.ar", "nowhere", "x", "missing"))
                    .await
                    .contains("not found")
            );
            assert!(
                refused(replace("Argon.toml", "name", "x", "manifest"))
                    .await
                    .contains("not an Argon source file")
            );
            assert!(
                refused(replace("/elsewhere/lib.ar", "a", "b", "outside"))
                    .await
                    .contains("outside")
            );
            let follow = agent
                .open_cell(agent_context(), "top()".to_owned())
                .await
                .unwrap()
                .unwrap_err();
            assert_eq!(follow, analyzer::agent::FOLLOW_REQUIRED);

            std::fs::write(&session.ack, "ok\n").unwrap();
            finish_nvim(child).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn approval_mode_asks_before_each_agent_edit() {
        assert_completes("waiting for approved agent edits", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new(&one_rect()).await;
            session.start_analyzer();
            let child = session.spawn_nvim("agent_approval");
            session.connect_gui().await;
            session.wait_for_rects(1).await;
            let agent = session.connect_agent().await;
            let rect = |name: &str| {
                add_rect(
                    &format!("let {name} = rect(\"met1\", x0=20., y0=0., x1=30., y1=10.);"),
                    name,
                )
            };

            session.await_signal("approval_on").await;
            assert_eq!(
                agent.status(agent_context()).await.unwrap().approval,
                ApprovalMode::Always
            );
            let rejected = agent
                .edit_file(agent_context(), rect("rejected_rect"))
                .await
                .unwrap();
            assert_eq!(
                rejected.unwrap_err(),
                "The user rejected this edit in Neovim."
            );
            agent
                .edit_file(agent_context(), rect("approved_rect"))
                .await
                .unwrap()
                .expect("an approved edit should apply");
            session.signal("approved");

            session.await_signal("approval_off").await;
            agent
                .edit_file(agent_context(), rect("unprompted_rect"))
                .await
                .unwrap()
                .expect("an edit should apply without approval");
            session.signal("unprompted");

            std::fs::write(&session.ack, "ok\n").unwrap();
            finish_nvim(child).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn agent_edits_survive_concurrent_typing() {
        assert_completes("waiting for an interrupted agent edit", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new(&one_rect()).await;
            session.start_analyzer();
            let child = session.spawn_nvim("agent_stale");
            session.connect_gui().await;
            session.wait_for_rects(1).await;
            let agent = session.connect_agent().await;
            session.await_signal("typing_armed").await;

            agent
                .edit_file(agent_context(), add_rect(SECOND_RECT, "add a second rect"))
                .await
                .unwrap()
                .expect("the edit should be replanned after the buffer changed");
            let file = agent
                .read_file(agent_context(), PathBuf::from("lib.ar"))
                .await
                .unwrap()
                .unwrap();
            assert!(file.contents.contains(SECOND_RECT));
            assert!(file.contents.contains("// typed during the agent edit"));

            std::fs::write(&session.ack, "ok\n").unwrap();
            finish_nvim(child).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn follow_mode_gates_cell_switching_and_tracks_edits() {
        assert_completes("waiting for follow mode", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::with_files(&with_unit(), &[("utils.ar", UNIT)]).await;
            session.start_analyzer();
            let child = session.spawn_nvim("agent_follow");
            session.connect_gui().await;
            session.wait_for_rects(1).await;
            let agent = session.connect_agent().await;

            let refused = agent
                .open_cell(agent_context(), "utils::unit()".to_owned())
                .await
                .unwrap();
            assert!(refused.unwrap_err().contains("follow mode"));

            session.signal("follow");
            session.await_signal("following").await;
            while !agent.status(agent_context()).await.unwrap().follow_mode {
                time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let opened = agent
                .open_cell(agent_context(), "utils::unit()".to_owned())
                .await
                .unwrap()
                .expect("follow mode should allow switching the GUI's cell");
            assert_eq!(
                opened.summary.unwrap().cell.as_deref(),
                Some("utils::unit()")
            );
            session
                .wait_for_event(|event| matches!(event, GuiEvent::Fit))
                .await;

            agent
                .edit_file(
                    agent_context(),
                    replace("utils.ar", "x1=5.", "x1=6.", "widen the unit"),
                )
                .await
                .unwrap()
                .unwrap();
            session.signal("edited_followed");
            session.await_signal("followed").await;

            std::fs::write(&session.ack, "ok\n").unwrap();
            finish_nvim(child).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn disk_changes_reach_neovim_and_the_gui() {
        assert_completes("waiting for disk changes", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::with_files(&with_unit(), &[("utils.ar", UNIT)]).await;
            session.start_analyzer();
            let child = session.spawn_nvim("watcher");
            session.connect_gui().await;
            session.wait_for_rects(1).await;
            let agent = session.connect_agent().await;
            while !agent.status(agent_context()).await.unwrap().watching_files {
                time::sleep(std::time::Duration::from_millis(10)).await;
            }

            // A closed file is reread from disk.
            std::fs::write(
                session.project().join("utils.ar"),
                UNIT.replace("x1=5.", "x1=25."),
            )
            .unwrap();
            loop {
                let report = agent.diagnostics(agent_context()).await.unwrap();
                if report.summary.and_then(|summary| summary.bbox) == Some([0., 0., 25., 10.]) {
                    break;
                }
                time::sleep(std::time::Duration::from_millis(20)).await;
            }

            // An open buffer without unsaved changes reloads.
            let lib = session.project().join("lib.ar");
            let third = "let c = rect(\"met1\", x0=40., y0=0., x1=50., y1=10.);";
            std::fs::write(
                &lib,
                with_unit().replace(FIRST_RECT, &format!("{FIRST_RECT}\n    {third}")),
            )
            .unwrap();
            session.wait_for_rects(2).await;

            // An open buffer with unsaved changes keeps them.
            session.await_signal("buffer_modified").await;
            let fourth = "let d = rect(\"met1\", x0=60., y0=0., x1=70., y1=10.);";
            std::fs::write(
                &lib,
                with_unit().replace(FIRST_RECT, &format!("{FIRST_RECT}\n    {fourth}")),
            )
            .unwrap();
            session
                .wait_for_event(|event| {
                    matches!(
                        event,
                        GuiEvent::Message { typ, message }
                            if *typ == MessageType::WARNING && message.contains("unsaved changes")
                    )
                })
                .await;
            session.signal("rewritten");

            std::fs::write(&session.ack, "ok\n").unwrap();
            finish_nvim(child).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_bridge_and_hook_use_the_running_session() {
        assert_completes("waiting for the MCP bridge", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new(&one_rect()).await;
            session.start_analyzer();
            let child = session.spawn_nvim("mcp");
            session.connect_gui().await;
            session.wait_for_rects(1).await;

            let bridge = analyzer::mcp::Bridge::new(session.project().to_path_buf());
            let call = |name: &str, arguments: Value| {
                let request = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": { "name": name, "arguments": arguments },
                });
                let bridge = &bridge;
                async move {
                    let response = bridge.handle(request).await.unwrap();
                    let result = &response["result"];
                    (
                        result["content"][0]["text"].as_str().unwrap().to_owned(),
                        result["isError"].as_bool().unwrap(),
                    )
                }
            };

            let (status, failed) = call("status", json!({})).await;
            assert!(!failed && status.contains("Open cell: top()"), "{status}");
            let (edited, failed) = call(
                "edit_file",
                json!({
                    "path": "lib.ar",
                    "label": "add a second rect",
                    "edits": [{
                        "old_string": FIRST_RECT,
                        "new_string": format!("{FIRST_RECT}\n    {SECOND_RECT}"),
                    }],
                }),
            )
            .await;
            assert!(!failed && edited.contains("2 rects"), "{edited}");
            let (read, failed) = call("read_file", json!({ "path": "lib.ar" })).await;
            assert!(!failed && read.contains("from the Neovim buffer"), "{read}");
            assert!(read.contains("let b = rect("), "{read}");

            let lib = session.project().join("lib.ar");
            let hook_input = |tool: &str, path: &std::path::Path| {
                json!({
                    "hook_event_name": "PreToolUse",
                    "tool_name": tool,
                    "tool_input": { "file_path": path },
                    "cwd": session.project(),
                })
            };
            let edit = analyzer::hook::decision(&hook_input("Edit", &lib)).await;
            assert!(edit.is_some_and(|reason| reason.contains("edit_file")));
            let read = analyzer::hook::decision(&hook_input("Read", &lib)).await;
            assert!(read.is_some_and(|reason| reason.contains("unsaved")));
            let notes = session.project().join("notes.md");
            assert_eq!(
                analyzer::hook::decision(&hook_input("Edit", &notes)).await,
                None
            );

            session.signal("save");
            session.await_signal("saved").await;
            assert_eq!(
                analyzer::hook::decision(&hook_input("Read", &lib)).await,
                None,
                "a saved file can be read from disk"
            );

            std::fs::write(&session.ack, "ok\n").unwrap();
            finish_nvim(child).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn mcp_bridge_routes_calls_to_each_running_session() {
        assert_completes("waiting for two sessions", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let extra = "let z = rect(\"met1\", x0=40., y0=0., x1=50., y1=10.);";
            let mut first = Session::new(&one_rect()).await;
            let mut second = Session::new(
                &one_rect().replace(FIRST_RECT, &format!("{FIRST_RECT}\n    {extra}")),
            )
            .await;
            first.start_analyzer();
            second.start_analyzer();
            let first_nvim = first.spawn_nvim("idle");
            let second_nvim = second.spawn_nvim("agent_target");
            first.connect_gui().await;
            second.connect_gui().await;
            first.wait_for_rects(1).await;
            second.wait_for_rects(2).await;

            // An agent whose working directory contains neither library.
            let elsewhere = tempfile::tempdir().unwrap();
            let bridge = analyzer::mcp::Bridge::new(elsewhere.path().to_path_buf());
            let call = |name: &str, arguments: Value| {
                let request = json!({
                    "jsonrpc": "2.0",
                    "id": 1,
                    "method": "tools/call",
                    "params": { "name": name, "arguments": arguments },
                });
                let bridge = &bridge;
                async move {
                    let response = bridge.handle(request).await.unwrap();
                    let result = &response["result"];
                    (
                        result["content"][0]["text"].as_str().unwrap().to_owned(),
                        result["isError"].as_bool().unwrap(),
                    )
                }
            };
            let tag = |session: &Session| {
                session
                    .project()
                    .parent()
                    .and_then(|parent| parent.file_name())
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            };

            // A file path selects the session whose workspace contains it.
            let (edited, failed) = call(
                "edit_file",
                json!({
                    "path": second.project().join("lib.ar"),
                    "label": "add a rect",
                    "edits": [{
                        "old_string": FIRST_RECT,
                        "new_string": format!("{FIRST_RECT}\n    {SECOND_RECT}"),
                    }],
                }),
            )
            .await;
            assert!(!failed && edited.contains("3 rects"), "{edited}");
            second.await_signal("edited").await;

            // `workspace` selects a session, and later calls default to it.
            let (status, failed) = call("status", json!({ "workspace": first.project() })).await;
            assert!(!failed, "{status}");
            let workspace_line = status.lines().next().unwrap();
            assert!(workspace_line.contains(&tag(&first)), "{status}");
            assert!(
                status.contains(&tag(&second)),
                "status should list the other session"
            );
            let (diagnostics, failed) = call("diagnostics", json!({})).await;
            assert!(!failed && diagnostics.contains("1 rects"), "{diagnostics}");

            // A path no session contains is refused with the sessions to choose from.
            let (refused, failed) = call(
                "read_file",
                json!({ "path": elsewhere.path().join("lib.ar") }),
            )
            .await;
            assert!(
                failed && refused.contains("No running Argon session contains"),
                "{refused}"
            );
            assert!(refused.contains(&tag(&first)) && refused.contains(&tag(&second)));

            std::fs::write(&first.ack, "ok\n").unwrap();
            std::fs::write(&second.ack, "ok\n").unwrap();
            finish_nvim(first_nvim).await;
            finish_nvim(second_nvim).await;
        })
        .await;
    }
}
