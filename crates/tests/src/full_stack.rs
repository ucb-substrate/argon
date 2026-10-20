//! Neovim, analyzer, and headless-GUI test scenarios.

use std::{net::Ipv4Addr, path::PathBuf, sync::Arc};

use analyzer::{
    ArgonConfig,
    agent::AgentClient,
    rpc::{
        CellEntry, CompilationSnapshot, CompressedCompilationUpdate, DevicePreview,
        FocusedCellPreview, Gui, GuiUpdateResult, InstancePreview, LangServerClient, RecentCell,
        SchematicInstancePreview,
    },
    transport::{self, Role, SessionToken},
};
use argonc::{
    ast::Span,
    compile::{CompileOutput, CompiledData},
};
use futures::prelude::*;
use tarpc::{context, server::Channel};
use tempfile::TempDir;
use tokio::{sync::mpsc, time};

use crate::{TEST_TIMEOUT, nvim_command, repository_root};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    Data,
    StaticErrors,
    FatalParseErrors,
}

/// An element's name, placement, and the span of its call.
pub type ElementSummary = (String, Option<(i64, i64)>, Option<Span>);
/// A connection's route and the span of its call.
pub type ConnectionSummary = (Option<Vec<(i64, i64)>>, Span);
/// A signal's net name and the span of its call.
pub type SignalSummary = (String, Span);
/// A pin of a cell's symbol: its port's name, its position, and the span
/// of its `pin` call.
pub type PinSummary = (String, (i64, i64), Option<Span>);

/// A compiled cell's symbol: whether the cell draws one, and its pins.
#[derive(Debug, Clone, PartialEq)]
pub struct CellSummary {
    pub name: String,
    /// The cell body.
    pub body: Span,
    pub has_symbol: bool,
    pub pins: Vec<PinSummary>,
}

/// What the top cell's schematic shows: its nets, each element with its
/// placement and the span of its call, each connection's route and span,
/// and each signal's net and span.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SchematicSummary {
    pub nets: Vec<String>,
    pub elements: Vec<ElementSummary>,
    pub connections: Vec<ConnectionSummary>,
    pub signals: Vec<SignalSummary>,
    /// Every compiled cell, the top one included.
    pub cells: Vec<CellSummary>,
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
        schematic: SchematicSummary,
    },
    Message {
        typ: tower_lsp_server::ls_types::MessageType,
        message: String,
    },
    Fit,
    WorkspacePath(Option<PathBuf>),
    WorkspaceModified(bool),
    /// A schematic instance the `sinst` command prepared for placement.
    SchematicInstancePreview(SchematicInstancePreview),
    /// A device the `device` command prepared for placement.
    DevicePreview(DevicePreview),
    /// The library's cells.
    Cells(Vec<CellEntry>),
    /// The library's recently opened cells.
    RecentCells(Vec<RecentCell>),
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

    async fn update_cell(
        self,
        _: context::Context,
        update: CompressedCompilationUpdate,
    ) -> GuiUpdateResult {
        let Ok(update) = update.decode() else {
            return GuiUpdateResult::default();
        };
        let (kind, scope, rect_count, object_count, schematic, revision) = {
            let mut previous = self.snapshot.lock().unwrap();
            let Some(snapshot) = update.materialize(previous.as_ref()) else {
                return GuiUpdateResult::default();
            };
            *previous = Some(snapshot.clone());
            let (kind, scope, rect_count, object_count, schematic) =
                snapshot_details(&snapshot.output);
            (
                kind,
                scope,
                rect_count,
                object_count,
                schematic,
                snapshot.revision,
            )
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
                schematic,
            })
            .expect("full-stack test should still be receiving GUI events");
        let _permit = self.update_gate.acquire().await.unwrap();
        GuiUpdateResult {
            accepted: true,
            ..GuiUpdateResult::default()
        }
    }

    async fn preview_cell(self, _: context::Context, _: FocusedCellPreview) -> GuiUpdateResult {
        GuiUpdateResult {
            accepted: true,
            ..GuiUpdateResult::default()
        }
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

    async fn selected_cell(self, _: context::Context) -> Option<argonc::compile::CellId> {
        None
    }

    async fn place_instance(self, _: context::Context, _: InstancePreview) {}

    async fn schematic_target(self, _: context::Context) -> Option<Span> {
        self.selected_scope.lock().expect("selected scope").clone()
    }

    async fn place_schematic_instance(
        self,
        _: context::Context,
        preview: SchematicInstancePreview,
    ) {
        self.events
            .send(GuiEvent::SchematicInstancePreview(preview))
            .expect("full-stack test should still be receiving GUI events");
    }

    async fn place_device(self, _: context::Context, preview: DevicePreview) {
        self.events
            .send(GuiEvent::DevicePreview(preview))
            .expect("full-stack test should still be receiving GUI events");
    }

    async fn configure(self, _: context::Context, _: ArgonConfig) {}

    async fn activate(self, _: context::Context) {}

    async fn set_cells(self, _: context::Context, cells: Vec<CellEntry>) {
        self.events
            .send(GuiEvent::Cells(cells))
            .expect("full-stack test should still be receiving GUI events");
    }

    async fn set_recent_cells(self, _: context::Context, cells: Vec<RecentCell>) {
        self.events
            .send(GuiEvent::RecentCells(cells))
            .expect("full-stack test should still be receiving GUI events");
    }

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

fn snapshot_details(
    output: &CompileOutput,
) -> (OutputKind, Option<Span>, usize, usize, SchematicSummary) {
    let (kind, data) = match output {
        CompileOutput::Valid(data) => (OutputKind::Data, Some(data)),
        CompileOutput::ExecErrors(output) => (OutputKind::Data, output.output.as_ref()),
        CompileOutput::StaticErrors(_) => (OutputKind::StaticErrors, None),
        CompileOutput::FatalParseErrors => (OutputKind::FatalParseErrors, None),
    };
    let (scope, rect_count, object_count) = data.map(gui_snapshot).unwrap_or((None, 0, 0));
    let schematic = data.map(schematic_summary).unwrap_or_default();
    (kind, scope, rect_count, object_count, schematic)
}

/// The top cell's schematic, as the GUI would draw it.
fn schematic_summary(data: &CompiledData) -> SchematicSummary {
    let Some(cell) = data.cells.get(&data.top) else {
        return SchematicSummary::default();
    };
    let schematic = &cell.schematic;
    let position = |placement: Option<argonc::compile::Placement>| {
        placement.and_then(|placement| placement.position)
    };
    SchematicSummary {
        nets: schematic.nets.iter().map(|net| net.name.clone()).collect(),
        elements: schematic
            .elements
            .iter()
            .map(|element| match *element {
                argonc::compile::Element::Instance(index) => {
                    let instance = &schematic.instances[index];
                    (
                        instance.name.clone(),
                        position(instance.placement),
                        instance.span.clone(),
                    )
                }
                argonc::compile::Element::Device(index) => {
                    let device = &schematic.devices[index];
                    (
                        device.name.clone(),
                        position(device.placement),
                        device.span.clone(),
                    )
                }
            })
            .collect(),
        connections: schematic
            .connections
            .iter()
            .map(|connection| (connection.route.clone(), connection.span.clone()))
            .collect(),
        signals: schematic
            .nodes
            .iter()
            .filter_map(|node| match &node.source {
                argonc::compile::NodeSource::Signal { span, .. } => {
                    Some((schematic.nets[node.net as usize].name.clone(), span.clone()))
                }
                _ => None,
            })
            .collect(),
        cells: data
            .cells
            .values()
            .map(|cell| {
                let symbol = cell
                    .symbol
                    .clone()
                    .unwrap_or_else(|| argonc::compile::generated_box(&cell.schematic, &cell.name));
                CellSummary {
                    name: cell.name.clone(),
                    body: cell.scope_span(cell.root).clone(),
                    has_symbol: cell.symbol.is_some(),
                    pins: symbol
                        .pins
                        .iter()
                        .enumerate()
                        .filter_map(|(port, pin)| {
                            let pin = pin.as_ref()?;
                            Some((
                                cell.schematic.port_name(port).to_owned(),
                                (pin.x, pin.y),
                                pin.site.as_ref().map(|site| site.span.clone()),
                            ))
                        })
                        .collect(),
                }
            })
            .collect(),
    }
}

/// The top cell's root scope, its rect count, and its total object count.
fn gui_snapshot(data: &CompiledData) -> (Option<Span>, usize, usize) {
    let Some(cell) = data.cells.get(&data.top) else {
        return (None, 0, 0);
    };
    let scope = cell
        .scopes
        .get(&cell.root)
        .map(|_| cell.scope_span(cell.root).clone());
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
            analyzer::main_with_io_on_listener(
                rpc_listener,
                token,
                analyzer::GuiLink::default(),
                reader,
                writer,
            )
            .await;
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
            let transport = tarpc::serde_transport::new(stream, transport::Postcard::default());
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
        let transport = tarpc::serde_transport::new(stream, transport::Postcard::default());
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
        let transport = tarpc::serde_transport::new(stream, transport::Postcard::default());
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
        /// Waits for the GUI to be sent exactly `invocations` as recent cells.
        async fn wait_for_recents(&mut self, invocations: &[&str]) -> Vec<RecentCell> {
            let GuiEvent::RecentCells(cells) = self
                .wait_for_event(|event| {
                    matches!(
                        event,
                        GuiEvent::RecentCells(cells)
                            if cells.iter().map(|cell| cell.invocation.as_str()).eq(invocations.iter().copied())
                    )
                })
                .await
            else {
                unreachable!()
            };
            cells
        }

        /// The invocations in the library's recents file.
        fn stored_recents(&self) -> Vec<String> {
            let contents =
                std::fs::read_to_string(self.project.join(".argon/recents.json")).unwrap();
            let file: Value = serde_json::from_str(&contents).unwrap();
            file["cells"]
                .as_array()
                .unwrap()
                .iter()
                .map(|cell| cell["invocation"].as_str().unwrap().to_owned())
                .collect()
        }

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

    async fn draw_burst(analyzer: &LangServerClient, scope: &Span) {
        time::timeout(std::time::Duration::from_secs(2), async {
            for i in 0..20 {
                let x = (i * 20) as f64;
                let inserted = analyzer
                    .draw_rect(
                        context::current(),
                        scope.clone(),
                        "burst".to_owned(),
                        BasicRect {
                            layer: Some("met1".to_owned()),
                            x0: x,
                            y0: 0.,
                            x1: x + 10.,
                            y1: 10.,
                            construction: false,
                        },
                    )
                    .await
                    .expect("GUI drawing request should reach analyzer");
                assert!(inserted.is_some(), "drawing {i} should edit the buffer");
            }
        })
        .await
        .expect("20 GUI drawings were delayed by Neovim or GUI compilation");
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

    const SCHEMATIC_SOURCE: &str = "use std::schematic::{DeviceKind, Signal, connect, device, signals};
                                     cell top() {
                                             pub let a = Signal(x=-6, y=0);
                                             pub let vdd = Signal(x=0, y=8);
                                             let mp = device(DeviceKind::Pmos, signals(4), \"pch\", w=1., l=0.15, x=0, y=4);
                                             connect(a, mp.g, route=[(-4, 0), (-4, 4)]);
                                             connect(vdd, mp.s, route=[]);
                                     }
";

    /// Moving a device through the `place` RPC writes its placement and its
    /// wires' routes as one edit, which one undo in Neovim reverts whole, and
    /// a gesture drawn on a stale snapshot is refused.
    #[tokio::test(flavor = "multi_thread")]
    async fn schematic_move_roundtrip() {
        use analyzer::rpc::{KeywordEdit, PlaceRequest, PlacementEdit};

        assert_completes("moving a schematic device and undoing it", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new(SCHEMATIC_SOURCE).await;
            session.start_analyzer();
            let child = session.spawn_nvim("schematic_move");
            let analyzer = session.connect_analyzer().await;
            session.connect_gui().await;

            let mut moved = false;
            let mut saw_move = false;
            let mut saw_revert = false;
            let mut stale_refused = false;
            while !(saw_move && saw_revert && stale_refused) {
                match session.next_event().await {
                    GuiEvent::UpdateCell {
                        revision,
                        kind: OutputKind::Data,
                        schematic,
                        ..
                    } if !moved => {
                        let (name, position, span) = schematic.elements[0].clone();
                        assert_eq!(name, "Mmp");
                        assert_eq!(position, Some((0, 4)));
                        let span = span.expect("a device placed in source has a span");
                        let keyword = |name: &str, value: Option<&str>| KeywordEdit {
                            name: name.to_owned(),
                            value: value.map(str::to_owned),
                        };
                        let edits = vec![
                            PlacementEdit {
                                call: span,
                                keywords: vec![keyword("x", Some("2")), keyword("y", Some("4"))],
                            },
                            PlacementEdit {
                                call: schematic.connections[1].1.clone(),
                                keywords: vec![keyword("route", Some("[(2, 8)]"))],
                            },
                        ];
                        // Drawn on an older snapshot: refused, and the GUI is told.
                        let stale = analyzer
                            .place(
                                context::current(),
                                PlaceRequest {
                                    base_revision: revision.wrapping_sub(1),
                                    edits: edits.clone(),
                                },
                            )
                            .await
                            .expect("place request should reach analyzer");
                        assert!(stale.is_none(), "a stale gesture must be refused");
                        let receipt = analyzer
                            .place(
                                context::current(),
                                PlaceRequest {
                                    base_revision: revision,
                                    edits,
                                },
                            )
                            .await
                            .expect("place request should reach analyzer")
                            .expect("the move should be applied");
                        assert_eq!(receipt.spans.len(), 2);
                        moved = true;
                    }
                    GuiEvent::UpdateCell {
                        kind: OutputKind::Data,
                        schematic,
                        ..
                    } if moved && !saw_move => {
                        if schematic.elements[0].1 == Some((2, 4)) {
                            assert_eq!(schematic.connections[1].0, Some(vec![(2, 8)]));
                            assert_eq!(schematic.connections[0].0, Some(vec![(-4, 0), (-4, 4)]));
                            saw_move = true;
                            std::fs::write(
                                &session.gui_edit_ack,
                                "ok
",
                            )
                            .expect("acknowledge compiled GUI edit");
                        }
                    }
                    GuiEvent::UpdateCell {
                        kind: OutputKind::Data,
                        schematic,
                        ..
                    } if saw_move && !saw_revert => {
                        if schematic.elements[0].1 == Some((0, 4)) {
                            assert_eq!(schematic.connections[1].0, Some(vec![]));
                            saw_revert = true;
                        }
                    }
                    GuiEvent::Message { message, .. }
                        if message.contains("changed since this gesture began") =>
                    {
                        stale_refused = true;
                    }
                    _ => {}
                }
            }

            std::fs::write(
                &session.ack,
                "ok
",
            )
            .expect("acknowledge GUI observations");
            finish_nvim(child).await;
            let source = std::fs::read_to_string(session.project.join("lib.ar"))
                .expect("read round-tripped source");
            assert_eq!(source, SCHEMATIC_SOURCE, "one undo reverts the whole move");
        })
        .await;
    }

    const ENTRY_SOURCE: &str = "cell child() {
    pub let p = std::schematic::Signal(x=0, y=0);
}
cell top() {
    pub let b = std::schematic::Signal(x=4, y=0);
    let c = b;
}
";

    /// Placing a device, adding ports, wiring them, and deleting the device
    /// through the entry RPCs each write one edit that Neovim undoes whole;
    /// a deletion that another statement blocks is refused with that
    /// reference; and `:Argon device` and `:Argon sinst` preview in the GUI.
    #[tokio::test(flavor = "multi_thread")]
    async fn schematic_entry_roundtrip() {
        use analyzer::rpc::{
            AddSignalRequest, ConnectRequest, DeleteOutcome, DeleteRequest, ElementPlacement,
            PlaceDeviceRequest, SchematicTarget,
        };

        assert_completes("entering a schematic and undoing it", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new(ENTRY_SOURCE).await;
            session.start_analyzer();
            let child = session.spawn_nvim("schematic_entry");
            let analyzer = session.connect_analyzer().await;
            session.connect_gui().await;

            let mut step = 0;
            let mut device_preview = None;
            let mut instance_preview = None;
            while !(step == 6 && device_preview.is_some() && instance_preview.is_some()) {
                match session.next_event().await {
                    GuiEvent::UpdateCell {
                        revision,
                        kind: OutputKind::Data,
                        scope: Some(scope),
                        schematic,
                        ..
                    } => {
                        let target = SchematicTarget {
                            base_revision: revision,
                            cell: scope,
                        };
                        match step {
                            0 => {
                                // `b` is read by `let c = b;`, so it cannot go.
                                let b = schematic
                                    .signals
                                    .iter()
                                    .find(|(net, _)| net == "b")
                                    .expect("b is a signal")
                                    .1
                                    .clone();
                                let outcome = analyzer
                                    .delete(
                                        context::current(),
                                        DeleteRequest {
                                            base_revision: revision,
                                            targets: vec![b],
                                        },
                                    )
                                    .await
                                    .expect("delete request should reach analyzer")
                                    .expect("a blocked deletion still answers");
                                let DeleteOutcome::Blocked(references) = outcome else {
                                    panic!("deleting `b` must be blocked");
                                };
                                assert_eq!(references.len(), 1);
                                assert_eq!(references[0].text, "let c = b;");
                                analyzer
                                    .place_device(
                                        context::current(),
                                        PlaceDeviceRequest {
                                            target,
                                            kind: "Nmos".into(),
                                            terminals: 4,
                                            args: "\"nch\", w=1., l=0.15".into(),
                                            placement: ElementPlacement {
                                                at: (0, -4),
                                                angle: 0,
                                                reflect: false,
                                            },
                                        },
                                    )
                                    .await
                                    .expect("place request should reach analyzer")
                                    .expect("the device should be placed");
                                step = 1;
                            }
                            1 if schematic.elements.len() == 1 => {
                                assert_eq!(schematic.elements[0].0, "Mnmos0");
                                assert_eq!(schematic.elements[0].1, Some((0, -4)));
                                analyzer
                                    .add_signal(
                                        context::current(),
                                        AddSignalRequest {
                                            target,
                                            name: "a".into(),
                                            port: true,
                                            at: (-6, 0),
                                            connect_to: Some("nmos0.g".into()),
                                            route: Some(vec![(-6, -4)]),
                                            placements: Vec::new(),
                                        },
                                    )
                                    .await
                                    .expect("add_signal request should reach analyzer")
                                    .expect("the port should be added");
                                step = 2;
                            }
                            2 if schematic.nets.iter().any(|net| net == "a") => {
                                assert_eq!(schematic.connections.len(), 1);
                                assert_eq!(schematic.connections[0].0, Some(vec![(-6, -4)]));
                                analyzer
                                    .add_signal(
                                        context::current(),
                                        AddSignalRequest {
                                            target,
                                            name: "vss".into(),
                                            port: true,
                                            at: (0, -8),
                                            connect_to: None,
                                            route: None,
                                            placements: Vec::new(),
                                        },
                                    )
                                    .await
                                    .expect("add_signal request should reach analyzer")
                                    .expect("the port should be added");
                                step = 3;
                            }
                            3 if schematic.nets.iter().any(|net| net == "vss") => {
                                analyzer
                                    .connect(
                                        context::current(),
                                        ConnectRequest {
                                            target,
                                            a: "vss".into(),
                                            b: "nmos0.s".into(),
                                            route: Some(Vec::new()),
                                            placements: Vec::new(),
                                        },
                                    )
                                    .await
                                    .expect("connect request should reach analyzer")
                                    .expect("the wire should be written");
                                step = 4;
                            }
                            4 if schematic.connections.len() == 2 => {
                                assert_eq!(schematic.connections[1].0, Some(Vec::new()));
                                let device = schematic.elements[0].2.clone().expect("placed");
                                let outcome = analyzer
                                    .delete(
                                        context::current(),
                                        DeleteRequest {
                                            base_revision: revision,
                                            targets: vec![device],
                                        },
                                    )
                                    .await
                                    .expect("delete request should reach analyzer")
                                    .expect("the device should be deleted");
                                assert!(matches!(outcome, DeleteOutcome::Applied(_)));
                                step = 5;
                            }
                            5 if schematic.elements.is_empty() => {
                                assert!(schematic.connections.is_empty());
                                assert_eq!(schematic.nets, ["b", "a", "vss"]);
                                std::fs::write(&session.gui_edit_ack, "ok\n")
                                    .expect("acknowledge compiled GUI edits");
                                step = 6;
                            }
                            _ => {}
                        }
                    }
                    GuiEvent::DevicePreview(preview) => {
                        assert_eq!(preview.kind, argonc::compile::DeviceKind::Nmos);
                        assert_eq!(preview.terminals, 4);
                        assert_eq!(preview.args, "\"nch\", w=1., l=0.15");
                        device_preview = Some(preview);
                    }
                    GuiEvent::SchematicInstancePreview(preview) => {
                        assert_eq!(preview.cell_name, "child");
                        assert_eq!(preview.pin_names, ["p"]);
                        assert_eq!(preview.invocation, "child()");
                        instance_preview = Some(preview);
                    }
                    _ => {}
                }
            }
            std::fs::write(&session.diagnostic_ack, "ok\n").expect("acknowledge previews");
            std::fs::write(&session.ack, "ok\n").expect("acknowledge GUI observations");
            finish_nvim(child).await;
            let source = std::fs::read_to_string(session.project.join("lib.ar"))
                .expect("read round-tripped source");
            assert_eq!(
                source, ENTRY_SOURCE,
                "one undo per gesture restores the source"
            );
        })
        .await;
    }

    const SYMBOL_SOURCE: &str = "cell inv() {
    pub let a = std::schematic::Signal(x=-6, y=0);
    pub let out = std::schematic::Signal(x=6, y=0);
}
cell top() {
    pub let a = std::schematic::Signal(x=-6, y=-2);
    pub let out = std::schematic::Signal(x=12, y=-2);
    pub let u = std::schematic::inst(inv(), x=0, y=0);
    std::schematic::connect(a, u.a, route=[]);
    std::schematic::connect(out, u.out, route=[]);
}
";

    /// Create Symbol in a child writes its generated box with the same pins,
    /// so the parent's wires end where they did; moving a pin then moves
    /// the parent's wire end with it; and each edit is one undo step.
    #[tokio::test(flavor = "multi_thread")]
    async fn symbol_edit_roundtrip() {
        use analyzer::rpc::{
            CreateSymbolRequest, KeywordEdit, PlaceRequest, PlacementEdit, SchematicTarget,
        };

        assert_completes("creating and editing a symbol", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new(SYMBOL_SOURCE).await;
            session.start_analyzer();
            let child = session.spawn_nvim("symbol_edit");
            let analyzer = session.connect_analyzer().await;
            session.connect_gui().await;

            let mut step = 0;
            let mut generated_pins = Vec::new();
            while step < 3 {
                let GuiEvent::UpdateCell {
                    revision,
                    kind: OutputKind::Data,
                    schematic,
                    ..
                } = session.next_event().await
                else {
                    continue;
                };
                let inv = schematic.cells.iter().find(|cell| cell.name == "inv");
                let Some(inv) = inv else {
                    continue;
                };
                match step {
                    0 => {
                        assert!(!inv.has_symbol);
                        generated_pins = inv
                            .pins
                            .iter()
                            .map(|(name, at, _)| (name.clone(), *at))
                            .collect();
                        assert_eq!(
                            generated_pins,
                            [("a".to_owned(), (0, -2)), ("out".to_owned(), (4, -2))]
                        );
                        assert_eq!(schematic.connections.len(), 2);
                        analyzer
                            .create_symbol(
                                context::current(),
                                CreateSymbolRequest {
                                    target: SchematicTarget {
                                        base_revision: revision,
                                        cell: inv.body.clone(),
                                    },
                                },
                            )
                            .await
                            .expect("create_symbol request should reach analyzer")
                            .expect("the symbol should be written");
                        step = 1;
                    }
                    1 if inv.has_symbol => {
                        let pins = inv
                            .pins
                            .iter()
                            .map(|(name, at, _)| (name.clone(), *at))
                            .collect::<Vec<_>>();
                        assert_eq!(pins, generated_pins, "the drawn pins are the box's");
                        assert_eq!(schematic.connections.len(), 2, "the parent's wires stay");
                        let pin_a = inv.pins[0].2.clone().expect("a drawn pin has a call");
                        analyzer
                            .place(
                                context::current(),
                                PlaceRequest {
                                    base_revision: revision,
                                    edits: vec![PlacementEdit {
                                        call: pin_a,
                                        keywords: vec![KeywordEdit {
                                            name: "y".into(),
                                            value: Some("-3".into()),
                                        }],
                                    }],
                                },
                            )
                            .await
                            .expect("place request should reach analyzer")
                            .expect("the pin should move");
                        step = 2;
                    }
                    2 if inv.pins[0].1 == (0, -3) => {
                        // The parent's routes are untouched; its wire end
                        // follows the pin because routes end at pins.
                        assert_eq!(schematic.connections[0].0, Some(Vec::new()));
                        std::fs::write(&session.gui_edit_ack, "ok\n")
                            .expect("acknowledge compiled GUI edits");
                        step = 3;
                    }
                    _ => {}
                }
            }
            std::fs::write(&session.ack, "ok\n").expect("acknowledge GUI observations");
            finish_nvim(child).await;
            let source = std::fs::read_to_string(session.project.join("lib.ar"))
                .expect("read round-tripped source");
            assert_eq!(
                source, SYMBOL_SOURCE,
                "one undo per gesture restores the source"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rapid_gui_drawings_do_not_wait_for_gui_compilation() {
        assert_completes("drawing while GUI compilation is blocked", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new("cell top() {\n}\n").await;
            session.start_analyzer();
            let child = session.spawn_nvim("rapid_drawing");
            let analyzer = session.connect_analyzer().await;
            session.connect_gui().await;

            let scope = loop {
                if let GuiEvent::UpdateCell {
                    kind: OutputKind::Data,
                    scope: Some(scope),
                    ..
                } = session.next_event().await
                {
                    break scope;
                }
            };
            // Block only subsequent GUI updates. Editor edits should still
            // acknowledge promptly even when the GUI cannot accept a snapshot.
            loop {
                if matches!(session.next_event().await, GuiEvent::CompilationFinished(_)) {
                    break;
                }
            }
            let gate = session.update_gate.clone().acquire_owned().await.unwrap();
            draw_burst(&analyzer, &scope).await;
            drop(gate);
            std::fs::write(&session.ack, "ok\n").expect("acknowledge GUI observations");
            finish_nvim(child).await;
            let source = std::fs::read_to_string(session.project.join("lib.ar"))
                .expect("read edited source");
            assert_eq!(source.matches("= std::layout::rect(").count(), 20);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn bulk_delete_after_drawing_burst_skips_stale_compilations() {
        assert_completes("bulk deleting after a drawing burst", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new("cell top() {\n}\n").await;
            session.start_analyzer();
            let child = session.spawn_nvim("bulk_delete_after_burst");
            let analyzer = session.connect_analyzer().await;
            session.connect_gui().await;
            let (opened_revision, scope) = loop {
                if let GuiEvent::UpdateCell {
                    revision,
                    kind: OutputKind::Data,
                    scope: Some(scope),
                    ..
                } = session.next_event().await
                {
                    break (revision, scope);
                }
            };
            loop {
                if matches!(session.next_event().await, GuiEvent::CompilationFinished(_)) {
                    break;
                }
            }
            let gate = session.update_gate.clone().acquire_owned().await.unwrap();
            draw_burst(&analyzer, &scope).await;
            time::timeout(TEST_TIMEOUT, async {
                while !session.project.join("startup.ready").exists() {
                    time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .expect("Neovim did not bulk-delete the drawings");
            let started = std::time::Instant::now();
            drop(gate);
            time::timeout(std::time::Duration::from_secs(2), async {
                loop {
                    if matches!(
                        session.next_event().await,
                        GuiEvent::UpdateCell {
                            revision,
                            kind: OutputKind::Data,
                            rect_count: 0,
                            ..
                        } if revision > opened_revision
                    ) {
                        break;
                    }
                }
            })
            .await
            .expect("newest bulk deletion waited for stale GUI/compiler work");
            eprintln!(
                "bulk_delete_after_burst_ms={}",
                started.elapsed().as_millis()
            );
            std::fs::write(&session.ack, "ok\n").expect("acknowledge GUI observations");
            finish_nvim(child).await;
            let source = std::fs::read_to_string(session.project.join("lib.ar"))
                .expect("read edited source");
            assert_eq!(source.matches("= std::layout::rect(").count(), 0);
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
    async fn sse_edits_from_one_layout_are_placed_after_each_other() {
        assert_completes("placing consecutive SSE edits", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let source = "use std::layout::rect;\ncell top() {\n    let r = rect(\"met1\", x0i = 0., y0i = 0., x1i = 10., y1i = 10.)!;\n}\n";
            let mut session = Session::new(source).await;
            session.start_analyzer();
            let child = session.spawn_nvim("sse_edits");
            let analyzer = session.connect_analyzer().await;
            session.connect_gui().await;
            let (revision, path) = loop {
                if let GuiEvent::UpdateCell {
                    revision,
                    kind: OutputKind::Data,
                    scope: Some(scope),
                    ..
                } = session.next_event().await
                {
                    break (revision, scope.path);
                }
            };
            // Every edit names spans in the layout compiled at `revision`, as
            // a GUI does while the drags before it are still compiling.
            let update = |kwarg: &str, value: &str, base_revision: u64| {
                let start = source.find(kwarg).unwrap() + kwarg.len();
                let len = source[start..].find([',', ')']).unwrap();
                let edit = analyzer::rpc::ValueEdit {
                    span: Span {
                        path: path.clone(),
                        span: cfgrammar::Span::new(start, start + len),
                    },
                    value: value.to_owned(),
                };
                let analyzer = analyzer.clone();
                async move {
                    analyzer
                        .update_values(context::current(), vec![edit], vec![], Some(base_revision))
                        .await
                        .expect("SSE edit should reach analyzer")
                }
            };

            // Lengthening `x0i` shifts every later span in the file.
            assert!(update("x0i = ", "12345.", revision).await.is_some());
            // Sent before that edit has compiled.
            assert!(update("y0i = ", "7.", revision).await.is_some());
            loop {
                if let GuiEvent::UpdateCell {
                    revision: edited,
                    kind: OutputKind::Data,
                    ..
                } = session.next_event().await
                    && edited > revision
                {
                    break;
                }
            }
            // Sent after the analyzer has compiled the edited source.
            assert!(update("x1i = ", "20.", revision).await.is_some());
            assert!(
                update("y1i = ", "3.", revision + 1000).await.is_none(),
                "a layout the analyzer did not edit from is rejected"
            );

            std::fs::write(&session.gui_edit_ack, "ok\n").expect("request the edited source");
            std::fs::write(&session.ack, "ok\n").expect("acknowledge GUI observations");
            finish_nvim(child).await;
            let edited = std::fs::read_to_string(session.project.join("lib.ar"))
                .expect("read edited source");
            assert!(
                edited.contains("x0i = 12345., y0i = 7., x1i = 20., y1i = 10."),
                "{edited}"
            );
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
    async fn library_cells_reach_the_gui_before_a_cell_is_open() {
        assert_completes("waiting for the library's cells in the GUI", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new("cell top() {\n}\n\ncell inv(w: Float) {\n}\n").await;
            session.start_analyzer();
            let child = session.spawn_nvim("cells");
            let analyzer = session.connect_analyzer().await;
            session.connect_gui().await;

            let expected = vec![
                CellEntry {
                    path: "top".to_owned(),
                    parameters: Vec::new(),
                    requires_arguments: false,
                },
                CellEntry {
                    path: "inv".to_owned(),
                    parameters: vec!["w".to_owned()],
                    requires_arguments: true,
                },
            ];
            loop {
                if let GuiEvent::Cells(cells) = session.next_event().await
                    && cells == expected
                {
                    break;
                }
            }

            analyzer
                .open_cell(context::current(), "top()".to_owned())
                .await
                .expect("open-cell request should reach analyzer");
            loop {
                if let GuiEvent::UpdateCell {
                    kind: OutputKind::Data,
                    scope: Some(_),
                    ..
                } = session.next_event().await
                {
                    break;
                }
            }

            std::fs::write(&session.ack, "ok\n").expect("acknowledge the opened cell");
            finish_nvim(child).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recent_cells_follow_user_opens() {
        assert_completes("waiting for recently opened cells", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new("cell top() {\n}\n\ncell inv(n: Int) {\n}\n").await;
            session.start_analyzer();
            let child = session.spawn_nvim("recents");
            let analyzer = session.connect_analyzer().await;
            session.connect_gui().await;

            // Neovim opens `top()` once it starts.
            session.wait_for_recents(&["top()"]).await;
            session.signal("open_inv");
            let recents = session.wait_for_recents(&["inv(2)", "top()"]).await;
            assert_eq!(recents[0].cell, "inv");
            assert_eq!(session.stored_recents(), ["inv(2)", "top()"]);
            assert_eq!(
                std::fs::read_to_string(session.project().join(".argon/.gitignore")).unwrap(),
                "*\n"
            );

            // An invocation that does not compile is not recorded.
            session.signal("open_nope");
            loop {
                match session.next_event().await {
                    GuiEvent::RecentCells(cells) => {
                        assert!(cells.iter().all(|cell| cell.invocation != "nope()"));
                    }
                    GuiEvent::UpdateCell {
                        kind: OutputKind::StaticErrors,
                        ..
                    } => break,
                    _ => {}
                }
            }
            assert_eq!(session.stored_recents(), ["inv(2)", "top()"]);

            // Nor is an agent's open, even of an invocation the user opened
            // that compiles once the agent adds its cell.
            session.await_signal("following").await;
            let agent = session.connect_agent().await;
            while !agent.status(agent_context()).await.unwrap().follow_mode {
                time::sleep(std::time::Duration::from_millis(10)).await;
            }
            agent
                .open_cell(agent_context(), "nope()".to_owned())
                .await
                .unwrap()
                .expect("follow mode should allow switching the GUI's cell");
            let report = agent
                .edit_file(
                    agent_context(),
                    replace(
                        "lib.ar",
                        "cell top() {\n}\n",
                        "cell top() {\n}\n\ncell nope() {\n}\n",
                        "add nope",
                    ),
                )
                .await
                .unwrap()
                .expect("the agent edit should apply");
            assert_eq!(report.summary.unwrap().status, CompileStatus::Valid);

            session.signal("new_cell");
            session
                .wait_for_recents(&["fresh()", "inv(2)", "top()"])
                .await;

            // Another spelling of a recorded invocation replaces it.
            analyzer
                .open_cell(context::current(), "inv( 2 )".to_owned())
                .await
                .expect("open-cell request should reach analyzer");
            session
                .wait_for_recents(&["inv( 2 )", "fresh()", "top()"])
                .await;

            session.signal("rename_cell");
            let renamed = ["unit( 2 )", "fresh()", "top()"];
            let recents = session.wait_for_recents(&renamed).await;
            assert_eq!(recents[0].cell, "unit");
            assert_eq!(session.stored_recents(), renamed);

            session.connect_gui().await;
            session.wait_for_recents(&renamed).await;

            std::fs::write(&session.ack, "ok\n").unwrap();
            finish_nvim(child).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn recent_cells_are_loaded_when_the_analyzer_starts() {
        assert_completes("waiting for stored recent cells", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new("cell top() {\n}\n\ncell inv(n: Int) {\n}\n").await;
            let directory = session.project().join(".argon");
            std::fs::create_dir(&directory).unwrap();
            let stored = json!({
                "cells": [{ "invocation": "lib::inv(2)" }, { "invocation": "top()" }]
            });
            std::fs::write(directory.join("recents.json"), stored.to_string()).unwrap();
            session.start_analyzer();
            let child = session.spawn_nvim("cells");
            session.connect_gui().await;

            let recents = session.wait_for_recents(&["lib::inv(2)", "top()"]).await;
            assert_eq!(recents[0].cell, "inv");
            assert_eq!(recents[1].cell, "top");

            std::fs::write(&session.ack, "ok\n").unwrap();
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
