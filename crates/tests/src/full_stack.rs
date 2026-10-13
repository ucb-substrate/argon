//! Neovim, analyzer, and headless-GUI test scenarios.

use std::{net::Ipv4Addr, path::PathBuf, sync::Arc};

use analyzer::{
    ArgonConfig,
    rpc::{
        CellEntry, CompilationSnapshot, CompilationUpdate, DevicePreview, Gui, InstancePreview,
        LangServerClient, SchematicInstancePreview,
    },
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
        let (kind, scope, rect_count, object_count, schematic, revision) = {
            let mut previous = self.snapshot.lock().unwrap();
            let Some(snapshot) = update.materialize(previous.as_ref()) else {
                return false;
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
                    body: cell.scopes[&cell.root].span.clone(),
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
    analyzer_addr: std::net::SocketAddr,
    analyzer_listener: Option<tokio::net::TcpListener>,
    lsp_port: u16,
    lsp_listener: Option<tokio::net::TcpListener>,
    gui_addr: std::net::SocketAddr,
    events: mpsc::UnboundedReceiver<GuiEvent>,
    #[cfg(test)]
    update_gate: std::sync::Arc<tokio::sync::Semaphore>,
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
        let selected_scope = Arc::new(std::sync::Mutex::new(None));
        let mut listener =
            tarpc::serde_transport::tcp::listen((Ipv4Addr::LOCALHOST, 0), Bincode::default)
                .await
                .expect("bind headless GUI RPC listener");
        listener.config_mut().max_frame_length(usize::MAX);
        let gui_addr = listener.local_addr();
        let update_gate = std::sync::Arc::new(tokio::sync::Semaphore::new(1));
        let server_update_gate = update_gate.clone();
        tokio::spawn(async move {
            listener
                .filter_map(|connection| futures::future::ready(connection.ok()))
                .map(tarpc::server::BaseChannel::with_defaults)
                .map(move |channel| {
                    let server = HeadlessGui {
                        events: events_tx.clone(),
                        snapshot: Default::default(),
                        update_gate: server_update_gate.clone(),
                        selected_scope: selected_scope.clone(),
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

    use analyzer::rpc::{DimensionParams, DrawSegmentConstraint, PathParams, PolygonParams};
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
            analyzer
                .register(context::current(), session.gui_addr())
                .await
                .expect("register headless GUI");

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
            analyzer
                .register(context::current(), session.gui_addr())
                .await
                .expect("register headless GUI");

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
            analyzer
                .register(context::current(), session.gui_addr())
                .await
                .expect("register headless GUI");

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
    async fn gui_registration_does_not_wait_for_initial_error_presentation() {
        assert_completes("registering GUI with initial source errors", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new("cell top() { missing; }\n").await;
            let gate = session.update_gate.clone().acquire_owned().await.unwrap();
            session.start_analyzer();
            let child = session.spawn_nvim("startup_errors");
            while !session.project.join("startup.ready").exists() {
                time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let analyzer = session.connect_analyzer().await;
            // A real GUI cannot service initial snapshot callbacks until its
            // constructor returns. Registration must acknowledge independently
            // of that first presentation, including when no cell is selected.
            time::timeout(
                std::time::Duration::from_secs(1),
                analyzer.register(context::current(), session.gui_addr()),
            )
            .await
            .expect("GUI registration waited for presentation")
            .expect("register headless GUI");
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
                "use std::layout::rect;\ncell top() {\n    let width = 100.;\n    let r = rect(\"met1\", x0=0., y0=0., x1=width, y1=width);\n}\ncell child(w: Float, h: Float = w, layer: String = \"met1\") {\n    let body = rect(layer, x0=0., y0=0., x1=w, y1=h);\n}\n",
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
            analyzer
                .register(context::current(), session.gui_addr())
                .await
                .expect("register headless GUI");

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

    #[tokio::test(flavor = "multi_thread")]
    async fn library_cells_reach_the_gui_before_a_cell_is_open() {
        assert_completes("waiting for the library's cells in the GUI", async {
            let _guard = FULL_STACK_LOCK.lock().await;
            let mut session = Session::new("cell top() {\n}\n\ncell inv(w: Float) {\n}\n").await;
            session.start_analyzer();
            let child = session.spawn_nvim("cells");
            let analyzer = session.connect_analyzer().await;
            analyzer
                .register(context::current(), session.gui_addr())
                .await
                .expect("register headless GUI");

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
}
