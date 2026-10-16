use std::{
    hash::{DefaultHasher, Hash, Hasher},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use analyzer::rpc::{
    CellEntry, CompilationSnapshot, DevicePreview, InstancePreview, LangServerAction, RecentCell,
    SchematicInstancePreview,
};
use argonc::compile::{CellId, CompileOutput, CompiledData, ExecErrorCompileOutput, Rect, ScopeId};
use canvas::{LayoutCanvas, ShapeFill, StipplePattern};
use futures::StreamExt;
use gpui::{prelude::FluentBuilder, *};
use indexmap::{IndexMap, IndexSet};
use rgb::Rgb;
use schematic::{
    SchematicCanvas,
    sidebar::{PropertiesSideBar, SchematicSideBar},
};
use start_screen::START_SCREEN_CONTEXT;
use toolbars::{HierarchySideBar, LayerSideBar, TitleBar, ToolBar};
use tower_lsp_server::ls_types::MessageType;

use crate::{
    actions::{
        DeviceCommand, ExportNetlist, FocusInvoker, FocusInvokerCommandBar, InstantiateCommand,
        NewCellCommand, NextView, OpenCellCommand, PreviousView, Redo, RenameCellCommand, Save,
        ShowDiagnostics, ShowMessages, Undo, ViewLayout, ViewSchematic, ViewSymbol,
    },
    editor::{canvas::ToolState, input::TextInput},
    main_window::MainWindow,
    rpc::SyncLangServerClient,
    theme::{DARK_THEME, LIGHT_THEME, Theme},
};

pub mod canvas;
mod hierarchy;
pub mod input;
pub mod schematic;
pub(crate) mod start_screen;
pub mod toolbars;
pub mod viewport;

pub(crate) const SOURCE_EDIT_REJECTED_MESSAGE: &str =
    "Could not apply the source edit. Press Ctrl-Shift-M to view the detailed error in Neovim.";

/// The view of the cell on screen that the window shows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum View {
    #[default]
    Layout,
    Schematic,
    Symbol,
}

impl View {
    /// The identifier the view's canvas adds to its key context.
    pub fn key_context(self) -> &'static str {
        match self {
            View::Layout => "LayoutCanvas",
            View::Schematic => "SchematicCanvas",
            View::Symbol => "SymbolCanvas",
        }
    }

    /// `v`: the next view around the cycle layout, schematic, symbol.
    pub fn next(self) -> Self {
        match self {
            View::Layout => View::Schematic,
            View::Schematic => View::Symbol,
            View::Symbol => View::Layout,
        }
    }

    /// `shift-v`: the previous view around the same cycle.
    pub fn previous(self) -> Self {
        match self {
            View::Layout => View::Symbol,
            View::Schematic => View::Layout,
            View::Symbol => View::Schematic,
        }
    }
}

/// One step from the open cell to the cell on screen.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RootStep {
    /// A schematic instance, by its element name.
    Schematic(String),
    /// A layout instance, by its scope path in the cell above it.
    Layout(ScopePath),
}

impl RootStep {
    fn describe(&self) -> String {
        match self {
            RootStep::Schematic(name) => format!("instance `{name}`"),
            RootStep::Layout(path) => {
                format!("scope `{}`", path.last().map(String::as_str).unwrap_or(""))
            }
        }
    }
}

/// Where focus goes when the GUI hands off to Neovim.
#[derive(Clone, Default)]
pub(crate) enum Invoker {
    /// The application that launched the GUI.
    #[default]
    App,
    /// The Neovim pane in this window.
    Pane {
        window: AnyWindowHandle,
        main: WeakEntity<MainWindow>,
    },
}

/// The tasks that serve an [`Editor`]. Dropping them disconnects it from the
/// analyzer.
pub(crate) struct EditorTasks {
    _exec: Task<()>,
    _callbacks: Task<()>,
}

/// How long objects an agent edit changed stay outlined.
const AGENT_HIGHLIGHT_DURATION: Duration = Duration::from_millis(2000);

/// Source spans an agent edit changed, outlined once the GUI shows
/// `revision`.
#[derive(Clone, Debug)]
pub struct AgentHighlight {
    pub revision: u64,
    pub spans: Vec<argonc::ast::Span>,
    generation: u64,
}

#[derive(Clone)]
pub struct LayerState {
    pub name: SharedString,
    pub color: Rgba,
    pub fill: ShapeFill,
    pub used: bool,
    pub border_color: Rgba,
    pub visible: bool,
    pub z: usize,
}

#[derive(Clone, Debug)]
pub struct ScopeState {
    pub name: String,
    pub address: ScopeAddress,
    pub visible: bool,
    pub bbox: Option<Rect<f64>>,
    pub parent: Option<ScopeAddress>,
}

pub type ScopePath = Vec<String>;

#[derive(Clone, Copy, Hash, PartialEq, Eq, Debug)]
pub struct ScopeAddress {
    pub scope: ScopeId,
    pub cell: CellId,
}

#[derive(Clone, Debug)]
pub struct CompileOutputState {
    pub output: Arc<CompiledData>,
    pub selected_scope: ScopePath,
    pub state: Arc<imbl::HashMap<ScopePath, ScopeState>>,
    pub scope_paths: Arc<imbl::HashMap<ScopeAddress, ScopePath>>,
    hierarchy: Arc<hierarchy::PreparedHierarchy>,
}

pub struct Layers {
    pub layers: IndexMap<SharedString, LayerState>,
    pub selected_layer: Option<SharedString>,
}

pub struct EditorState {
    pub hierarchy_depth: usize,
    pub dark_mode: bool,
    pub icon_size: Option<f32>,
    pub font_size: Option<f32>,
    pub workspace_path: Option<PathBuf>,
    pub workspace_modified: bool,
    pub compilation_activities: IndexSet<u64>,
    snapshot_preparations: IndexSet<u64>,
    latest_snapshot_preparation: Option<u64>,
    pub rendering: bool,
    pub compilation_revision: Option<u64>,
    pub compilation_error: Option<EditorMessage>,
    pub fatal_error: Option<EditorMessage>,
    pub message: Option<EditorMessage>,
    pub connection_error: Option<SharedString>,
    /// What an agent is doing right now, if anything.
    pub agent_activity: Option<SharedString>,
    pub agent_highlight: Option<AgentHighlight>,
    agent_highlight_generation: u64,
    pub solved_cell: Entity<Option<CompileOutputState>>,
    pub hide_external_geometry: bool,
    pub layers: Entity<Layers>,
    pub lang_server_client: SyncLangServerClient,
    pub subscriptions: Vec<Subscription>,
    pub(crate) tool: Entity<ToolState>,
    pub view: View,
    /// Which schematic tool is active, for the toolbar.
    pub schematic_tool: schematic::ToolKind,
    /// Which symbol tool is active, for the toolbar.
    pub symbol_tool: schematic::SymbolToolKind,
    pub show_flylines: bool,
    /// The steps from the open cell to the cell on screen, kept across
    /// snapshots by name.
    pub root_path: Vec<RootStep>,
    /// The open cell, which the root path descends from.
    pub(crate) base_top: Option<CellId>,
    /// The library's cells, offered on the start screen. `None` until the
    /// analyzer sends them.
    pub cells: Option<Vec<CellEntry>>,
    /// The cell invocations recently opened in the library, most recent first.
    pub recent_cells: Vec<RecentCell>,
    /// Whether Option acts as Meta in the Neovim pane on macOS.
    pub option_as_meta: bool,
    pub(crate) invoker: Invoker,
}

#[derive(Clone, Debug)]
pub struct EditorMessage {
    pub typ: MessageType,
    pub text: SharedString,
    pub details: MessageDetails,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageDetails {
    Diagnostics,
    Messages,
}

fn compilation_error_message(output: &CompileOutput) -> Option<EditorMessage> {
    (!matches!(output, CompileOutput::Valid(_))).then(|| EditorMessage {
        typ: MessageType::ERROR,
        text: "Compilation errors".into(),
        details: MessageDetails::Diagnostics,
    })
}

fn activity_status_label(is_compiling: bool, is_rendering: bool) -> Option<&'static str> {
    if is_compiling {
        Some("Compiling")
    } else if is_rendering {
        Some("Rendering")
    } else {
        None
    }
}

#[derive(Clone, Debug)]
pub struct Editor {
    pub state: Entity<EditorState>,
    pub title_bar: Entity<TitleBar>,
    pub tool_bar: Entity<ToolBar>,
    pub hierarchy_sidebar: Entity<HierarchySideBar>,
    pub layer_sidebar: Entity<LayerSideBar>,
    pub canvas: Entity<LayoutCanvas>,
    pub schematic_canvas: Entity<SchematicCanvas>,
    pub schematic_sidebar: Entity<SchematicSideBar>,
    pub properties_sidebar: Entity<PropertiesSideBar>,
}

fn rgb_to_rgba(color: Rgb<u8>) -> Rgba {
    rgb(((color.r as u32) << 16) | ((color.g as u32) << 8) | color.b as u32)
}

fn shape_fill(
    dither_pattern: &str,
    custom_patterns: &[argonc::tech::CustomDitherPattern],
) -> ShapeFill {
    match dither_pattern {
        "I0" => ShapeFill::Solid,
        "I1" => ShapeFill::Hollow,
        reference if reference.starts_with('C') => reference[1..]
            .parse::<usize>()
            .ok()
            .and_then(|index| custom_patterns.get(index))
            .and_then(|pattern| StipplePattern::from_lines(&pattern.lines))
            .map(|pattern| match pattern.coverage() {
                0. => ShapeFill::Hollow,
                1. => ShapeFill::Solid,
                _ => ShapeFill::Pattern(pattern),
            })
            .unwrap_or(ShapeFill::Stippling),
        // KLayout built-ins other than solid and clear do not carry their
        // bitmap in the technology file, so retain the legacy slash fallback.
        _ => ShapeFill::Stippling,
    }
}

#[derive(Default)]
struct ProcessScopeState {
    layers: IndexMap<SharedString, LayerState>,
    state: imbl::HashMap<ScopePath, ScopeState>,
    scope_paths: imbl::HashMap<ScopeAddress, ScopePath>,
}

#[derive(Default)]
pub(crate) struct CompilationPreparationContext {
    layers: IndexMap<SharedString, LayerState>,
    selected_scope: Option<ScopePath>,
    scope_state: Option<Arc<imbl::HashMap<ScopePath, ScopeState>>>,
    previous: Option<CompileOutputState>,
}

struct PreparedCompileOutput {
    layers: IndexMap<SharedString, LayerState>,
    selected_scope: ScopePath,
    state: imbl::HashMap<ScopePath, ScopeState>,
    scope_paths: imbl::HashMap<ScopeAddress, ScopePath>,
    hierarchy: Arc<hierarchy::PreparedHierarchy>,
}

pub(crate) struct PreparedCompilationSnapshot {
    pub(crate) revision: u64,
    pub(crate) output: CompileOutput,
    compilation_error: Option<EditorMessage>,
    prepared_output: Option<PreparedCompileOutput>,
}

fn mark_layer_used(state: &mut ProcessScopeState, layer: &str) {
    if let Some(layer_info) = state.layers.get_mut(layer) {
        layer_info.used = true;
        return;
    }
    let layer = SharedString::from(layer.to_owned());
    let mut hasher = DefaultHasher::new();
    layer.hash(&mut hasher);
    let hash = hasher.finish() as usize;
    let color = rgb([0xff0000, 0x0ff000, 0x00ff00, 0x000ff0, 0x0000ff][hash % 5]);
    state.layers.insert(
        layer.clone(),
        LayerState {
            name: layer,
            color,
            fill: ShapeFill::Stippling,
            border_color: color,
            visible: true,
            used: true,
            z: state.layers.len(),
        },
    );
}

impl EditorState {
    pub(crate) fn theme(&self) -> &'static Theme {
        if self.dark_mode {
            &DARK_THEME
        } else {
            &LIGHT_THEME
        }
    }

    /// The agent highlight to draw, once the layout it describes is shown.
    pub(crate) fn visible_agent_highlight(&self) -> Option<&[argonc::ast::Span]> {
        let highlight = self.agent_highlight.as_ref()?;
        (self.compilation_revision? >= highlight.revision).then_some(highlight.spans.as_slice())
    }

    pub fn show_message(&mut self, typ: MessageType, message: impl Into<SharedString>) {
        if typ != MessageType::LOG {
            self.message = Some(EditorMessage {
                typ,
                text: message.into(),
                details: MessageDetails::Messages,
            });
        }
    }
    /// Re-root the current compiled hierarchy at an already compiled child cell.
    ///
    /// This keeps the exact parameterization represented by the selected cell ID;
    /// reconstructing a source invocation from the hierarchy label would lose it.
    pub(crate) fn set_top_cell(&mut self, cell: CellId, cx: &mut App) -> bool {
        let Some(mut output) = self
            .solved_cell
            .read(cx)
            .as_ref()
            .map(|state| state.output.as_ref().clone())
        else {
            return false;
        };
        if output.top == cell || !output.cells.contains_key(&cell) {
            return false;
        }
        output.top = cell;
        self.update(cx, CompileOutput::Valid(output));
        true
    }

    /// The instance name the symbol view shows: the last descend step's, or
    /// `X?` at the open cell.
    pub(crate) fn symbol_inst_name(&self) -> String {
        match self.root_path.last() {
            Some(RootStep::Schematic(name)) => name.clone(),
            Some(RootStep::Layout(path)) => path.last().cloned().unwrap_or_else(|| "X?".into()),
            None => "X?".into(),
        }
    }

    /// The cell `step` reaches from `from`, which must be the cell on screen
    /// for a layout step.
    fn resolve_step(&self, cx: &App, step: &RootStep, from: CellId) -> Option<CellId> {
        let solved = self.solved_cell.read(cx);
        let solved = solved.as_ref()?;
        match step {
            RootStep::Schematic(name) => solved
                .output
                .cells
                .get(&from)?
                .schematic
                .instances
                .iter()
                .find(|instance| instance.name == *name)
                .map(|instance| instance.cell),
            RootStep::Layout(path) => {
                if solved.output.top != from {
                    return None;
                }
                solved
                    .scope_paths
                    .iter()
                    .find(|(_, scope_path)| *scope_path == path)
                    .map(|(address, _)| address.cell)
            }
        }
    }

    /// Re-roots the compiled hierarchy at the end of the root path, dropping
    /// the steps that no longer resolve.
    pub(crate) fn apply_root_path(&mut self, cx: &mut App) {
        let Some(mut cell) = self.base_top else {
            return;
        };
        let steps = self.root_path.clone();
        let mut failed = None;
        for (index, step) in steps.iter().enumerate() {
            self.set_top_cell(cell, cx);
            match self.resolve_step(cx, step, cell) {
                Some(next) => cell = next,
                None => {
                    failed = Some(index);
                    break;
                }
            }
        }
        self.set_top_cell(cell, cx);
        if let Some(index) = failed {
            let step = self.root_path[index].describe();
            self.root_path.truncate(index);
            self.show_message(
                MessageType::WARNING,
                format!("{step} is gone; showing the cell above it"),
            );
        }
    }

    /// Descends into the schematic instance `name` of the cell on screen.
    pub(crate) fn descend_schematic(&mut self, name: &str, cx: &mut App) -> bool {
        let Some(top) = self
            .solved_cell
            .read(cx)
            .as_ref()
            .map(|solved| solved.output.top)
        else {
            return false;
        };
        let step = RootStep::Schematic(name.to_owned());
        let Some(cell) = self.resolve_step(cx, &step, top) else {
            return false;
        };
        self.root_path.push(step);
        self.set_top_cell(cell, cx);
        true
    }

    /// Descends into the layout instance of `cell` in the cell on screen.
    pub(crate) fn descend_layout(&mut self, cell: CellId, cx: &mut App) -> bool {
        let path = {
            let solved = self.solved_cell.read(cx);
            let Some(solved) = solved.as_ref() else {
                return false;
            };
            let Some(compiled) = solved.output.cells.get(&cell) else {
                return false;
            };
            let address = ScopeAddress {
                cell,
                scope: compiled.root,
            };
            solved.scope_paths.get(&address).cloned()
        };
        let Some(path) = path else {
            return false;
        };
        if !self.set_top_cell(cell, cx) {
            return false;
        }
        self.root_path.push(RootStep::Layout(path));
        true
    }

    /// Returns to the cell above the cell on screen.
    pub(crate) fn return_up(&mut self, cx: &mut App) -> bool {
        if self.root_path.pop().is_none() {
            return false;
        }
        self.apply_root_path(cx);
        true
    }

    fn compilation_preparation_context(&self, cx: &App) -> CompilationPreparationContext {
        let old_cell = self.solved_cell.read(cx);
        CompilationPreparationContext {
            layers: self.layers.read(cx).layers.clone(),
            selected_scope: old_cell.as_ref().map(|cell| cell.selected_scope.clone()),
            scope_state: old_cell.as_ref().map(|cell| cell.state.clone()),
            previous: old_cell.clone(),
        }
    }

    pub fn update(&mut self, cx: &mut App, output: CompileOutput) {
        let context = self.compilation_preparation_context(cx);
        let prepared = prepare_compilation_snapshot(
            CompilationSnapshot {
                revision: self.compilation_revision.unwrap_or_default(),
                output,
            },
            context,
        );
        self.apply_prepared_output(cx, prepared);
    }

    fn apply_prepared_output(&mut self, cx: &mut App, prepared: PreparedCompilationSnapshot) {
        let PreparedCompilationSnapshot {
            output,
            compilation_error,
            prepared_output,
            ..
        } = prepared;
        // Compilation status belongs to the accepted snapshot, rather than to
        // the general message queue. In particular, a valid snapshot must
        // always remove a banner left by an earlier failed compilation.
        self.compilation_error = compilation_error;
        let solved_cell = match output {
            CompileOutput::Valid(d) => d,
            CompileOutput::ExecErrors(ExecErrorCompileOutput {
                output: Some(d),
                errors,
            }) => {
                if errors.iter().any(|error| error.kind.is_invalid_cell()) {
                    return;
                }
                d
            }
            _ => return,
        };
        let Some(PreparedCompileOutput {
            layers,
            selected_scope,
            state,
            scope_paths,
            hierarchy,
        }) = prepared_output
        else {
            return;
        };
        self.layers.update(cx, |old_layers, cx| {
            old_layers.layers = layers;
            if old_layers
                .selected_layer
                .as_ref()
                .map(|selected_layer| !old_layers.layers.contains_key(selected_layer))
                .unwrap_or(true)
            {
                old_layers.selected_layer = None;
            }
            cx.notify();
        });
        self.solved_cell.update(cx, |old_cell, cx| {
            *old_cell = Some(CompileOutputState {
                output: Arc::new(solved_cell),
                selected_scope,
                state: Arc::new(state),
                scope_paths: Arc::new(scope_paths),
                hierarchy,
            });
            cx.notify();
        });
        self.message = None;
    }
}

pub(crate) fn prepare_compilation_snapshot(
    snapshot: CompilationSnapshot,
    context: CompilationPreparationContext,
) -> PreparedCompilationSnapshot {
    let compilation_error = compilation_error_message(&snapshot.output);
    let solved_cell = match &snapshot.output {
        CompileOutput::Valid(data) => Some(data),
        CompileOutput::ExecErrors(ExecErrorCompileOutput {
            output: Some(data),
            errors,
        }) if !errors.iter().any(|error| error.kind.is_invalid_cell()) => Some(data),
        _ => None,
    };
    let prepared_output = solved_cell.map(|solved_cell| {
        let root_scope = ScopeAddress {
            scope: solved_cell.cells[&solved_cell.top].root,
            cell: solved_cell.top,
        };
        let root_scope_name = solved_cell.cells[&root_scope.cell].scopes[&root_scope.scope]
            .name
            .clone();
        let mut state = ProcessScopeState::default();
        for layer in &solved_cell.tech.layers {
            let name = SharedString::from(layer.name.clone());
            let visible = context
                .layers
                .get(&name)
                .map(|layer| layer.visible)
                .unwrap_or(layer.style.visible);
            state.layers.insert(
                name.clone(),
                LayerState {
                    name,
                    color: rgb_to_rgba(layer.fill_color),
                    fill: shape_fill(
                        &layer.style.dither_pattern,
                        &solved_cell.tech.custom_dither_patterns,
                    ),
                    border_color: rgb_to_rgba(layer.border_color),
                    visible,
                    used: false,
                    z: state.layers.len(),
                },
            );
        }
        let hierarchy = hierarchy::prepare(
            solved_cell,
            root_scope,
            &mut state,
            context.scope_state.as_deref(),
            context.previous.as_ref(),
        );
        let ProcessScopeState {
            layers,
            state,
            scope_paths,
        } = state;
        let selected_scope = context
            .selected_scope
            .filter(|selected_scope| state.contains_key(selected_scope))
            .unwrap_or_else(|| vec![root_scope_name]);
        PreparedCompileOutput {
            layers,
            selected_scope,
            state,
            scope_paths,
            hierarchy: Arc::new(hierarchy),
        }
    });
    PreparedCompilationSnapshot {
        revision: snapshot.revision,
        output: snapshot.output,
        compilation_error,
        prepared_output,
    }
}

impl Editor {
    /// Connects to the analyzer at `lang_server_addr` and builds the views.
    pub(crate) fn new(
        cx: &mut App,
        window: &mut Window,
        lang_server_addr: SocketAddr,
        token: analyzer::transport::SessionToken,
        invoker: Invoker,
    ) -> anyhow::Result<(Self, EditorTasks)> {
        let (lang_server_client, mut rx) =
            SyncLangServerClient::new(cx.to_async(), lang_server_addr, token)?;
        let solved_cell = cx.new(|_cx| None);
        let tool = cx.new(|_cx| ToolState::default());
        let layers = cx.new(|_cx| Layers {
            layers: IndexMap::new(),
            selected_layer: None,
        });
        let state = cx.new(|cx| {
            let subscriptions = vec![
                cx.observe(&solved_cell, |_, _, cx| cx.notify()),
                cx.observe(&layers, |_, _, cx| cx.notify()),
            ];
            EditorState {
                hierarchy_depth: usize::MAX,
                dark_mode: true,
                icon_size: None,
                font_size: None,
                workspace_path: None,
                workspace_modified: false,
                compilation_activities: IndexSet::new(),
                snapshot_preparations: IndexSet::new(),
                latest_snapshot_preparation: None,
                rendering: false,
                compilation_revision: None,
                compilation_error: None,
                fatal_error: None,
                message: None,
                connection_error: None,
                agent_activity: None,
                agent_highlight: None,
                agent_highlight_generation: 0,
                solved_cell,
                hide_external_geometry: false,
                tool,
                layers,
                subscriptions,
                lang_server_client: lang_server_client.clone(),
                view: View::Layout,
                schematic_tool: schematic::ToolKind::Select,
                symbol_tool: schematic::SymbolToolKind::Select,
                show_flylines: true,
                root_path: Vec::new(),
                base_top: None,
                cells: None,
                recent_cells: Vec::new(),
                option_as_meta: false,
                invoker: invoker.clone(),
            }
        });
        let title_bar = cx.new(|_cx| TitleBar::new(&state));
        let tool_bar = cx.new(|_cx| ToolBar::new(&state));
        let canvas_focus_handle = cx.focus_handle();
        let text_input_focus_handle = cx.focus_handle();
        if matches!(invoker, Invoker::App) {
            window.focus(&canvas_focus_handle);
        }
        let text_input = cx.new(|cx| {
            TextInput::new_dimension_input(
                cx,
                text_input_focus_handle.clone(),
                canvas_focus_handle.clone(),
                &state,
            )
        });
        let canvas = cx.new(|cx| {
            LayoutCanvas::new(
                cx,
                &state,
                canvas_focus_handle.clone(),
                text_input_focus_handle.clone(),
                text_input.clone(),
            )
        });
        let hierarchy_sidebar = cx.new(|cx| HierarchySideBar::new(cx, &state, &canvas));
        let layer_sidebar = cx.new(|cx| LayerSideBar::new(cx, &state, &canvas));
        let schematic_focus_handle = cx.focus_handle();
        let schematic_canvas =
            cx.new(|cx| SchematicCanvas::new(cx, &state, schematic_focus_handle));
        let schematic_sidebar = cx.new(|cx| SchematicSideBar::new(cx, &state, &schematic_canvas));
        let properties_sidebar = cx.new(|cx| PropertiesSideBar::new(cx, &state, &schematic_canvas));

        let editor = Self {
            state,
            title_bar,
            tool_bar,
            hierarchy_sidebar,
            layer_sidebar,
            canvas,
            schematic_canvas,
            schematic_sidebar,
            properties_sidebar,
        };
        let exec = cx.spawn({
            let editor = editor.clone();
            async move |app| {
                while let Some(exec) = rx.next().await {
                    exec(&editor, app);
                }
            }
        });
        let tasks = EditorTasks {
            _exec: exec,
            _callbacks: lang_server_client.serve_callbacks(),
        };
        Ok((editor, tasks))
    }

    fn apply_snapshot(&self, cx: &mut App, snapshot: PreparedCompilationSnapshot) -> bool {
        if self
            .state
            .read(cx)
            .compilation_revision
            .is_some_and(|revision| snapshot.revision < revision)
        {
            return false;
        }
        let base_top = match &snapshot.output {
            CompileOutput::Valid(data) => Some(data.top),
            CompileOutput::ExecErrors(ExecErrorCompileOutput {
                output: Some(data), ..
            }) => Some(data.top),
            _ => None,
        };
        self.state.update(cx, |state, cx| {
            state.connection_error = None;
            state.compilation_revision = Some(snapshot.revision);
            // A later revision may have moved the source the highlight names.
            if state
                .agent_highlight
                .as_ref()
                .is_some_and(|highlight| highlight.revision < snapshot.revision)
            {
                state.agent_highlight = None;
            }
            if snapshot.prepared_output.is_some() {
                // Keep the status animation alive between hierarchy preparation
                // and the first raster worker started by the next paint.
                state.rendering = true;
            }
            state.apply_prepared_output(cx, snapshot);
            if base_top.is_some() {
                state.base_top = base_top;
                state.apply_root_path(cx);
            }
            cx.notify();
        });
        self.canvas
            .update(cx, |canvas, cx| canvas.finish_sse_persist(cx));
        true
    }

    /// Fits the active view.
    pub fn fit_to_screen(&self, cx: &mut App) {
        match self.state.read(cx).view {
            View::Layout => self.canvas.update(cx, |canvas, cx| {
                canvas.fit_to_screen(cx);
                cx.notify();
            }),
            View::Schematic | View::Symbol => self
                .schematic_canvas
                .update(cx, |canvas, cx| canvas.fit_to_screen(cx)),
        }
    }

    /// The focus handle of the active view's canvas.
    fn active_focus_handle(&self, cx: &App) -> FocusHandle {
        match self.state.read(cx).view {
            View::Layout => self.canvas.focus_handle(cx),
            View::Schematic | View::Symbol => self.schematic_canvas.focus_handle(cx),
        }
    }

    /// Focuses the active view's canvas.
    pub(crate) fn focus_canvas(&self, window: &mut Window, cx: &App) {
        window.focus(&self.active_focus_handle(cx));
    }

    /// Brings the editor forward: its canvas when Neovim is in the same
    /// window, otherwise the whole application.
    pub(crate) fn activate(&self, cx: &mut App) {
        match self.state.read(cx).invoker.clone() {
            Invoker::App => crate::focus::activate_gui(cx),
            Invoker::Pane { window, .. } => {
                let _ = window.update(cx, |_, window, cx| self.focus_canvas(window, cx));
            }
        }
    }

    fn set_view(&mut self, view: View, window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.view = view;
            cx.notify();
        });
        let focus = self.active_focus_handle(cx);
        window.focus(&focus);
        cx.notify();
    }

    fn view_layout(&mut self, _: &ViewLayout, window: &mut Window, cx: &mut Context<Self>) {
        self.set_view(View::Layout, window, cx);
    }

    fn view_schematic(&mut self, _: &ViewSchematic, window: &mut Window, cx: &mut Context<Self>) {
        self.set_view(View::Schematic, window, cx);
    }

    fn view_symbol(&mut self, _: &ViewSymbol, window: &mut Window, cx: &mut Context<Self>) {
        self.set_view(View::Symbol, window, cx);
    }

    fn next_view(&mut self, _: &NextView, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.state.read(cx).view.next();
        self.set_view(view, window, cx);
    }

    fn previous_view(&mut self, _: &PreviousView, window: &mut Window, cx: &mut Context<Self>) {
        let view = self.state.read(cx).view.previous();
        self.set_view(view, window, cx);
    }

    pub fn set_compilation_active(&self, cx: &mut App, activity_id: u64, active: bool) {
        self.state.update(cx, |state, cx| {
            if active {
                state.compilation_activities.insert(activity_id);
            } else {
                state.compilation_activities.shift_remove(&activity_id);
            }
            cx.notify();
        });
    }

    pub(crate) fn begin_snapshot_preparation(
        &self,
        cx: &mut App,
        preparation_id: u64,
    ) -> CompilationPreparationContext {
        let context = self.state.read(cx).compilation_preparation_context(cx);
        self.state.update(cx, |state, cx| {
            state.snapshot_preparations.insert(preparation_id);
            state.latest_snapshot_preparation = Some(
                state
                    .latest_snapshot_preparation
                    .map_or(preparation_id, |latest| latest.max(preparation_id)),
            );
            cx.notify();
        });
        context
    }

    pub(crate) fn finish_snapshot_preparation(
        &self,
        cx: &mut App,
        preparation_id: u64,
        snapshot: PreparedCompilationSnapshot,
    ) {
        let is_latest = self.state.update(cx, |state, cx| {
            state
                .snapshot_preparations
                .retain(|pending| *pending > preparation_id);
            let is_latest = state.latest_snapshot_preparation == Some(preparation_id);
            if is_latest {
                state.latest_snapshot_preparation = None;
            }
            cx.notify();
            is_latest
        });
        if is_latest {
            self.update_cell(cx, snapshot);
        }
    }

    fn update_cell(&self, cx: &mut App, snapshot: PreparedCompilationSnapshot) {
        if self.canvas.read(cx).is_sse_dragging() {
            self.canvas
                .update(cx, |canvas, _| canvas.defer_snapshot(snapshot));
            return;
        }
        if !self.canvas.read(cx).accepts_snapshot(&snapshot) || !self.apply_snapshot(cx, snapshot) {
            return;
        }
        let state = self.state.clone();
        self.hierarchy_sidebar.update(cx, move |sidebar, cx| {
            let scope_state = state
                .read(cx)
                .solved_cell
                .read(cx)
                .as_ref()
                .map(|cell| cell.state.clone());
            sidebar.state.update(cx, |state, _cx| {
                state.expanded_scopes.retain(|path| {
                    scope_state
                        .as_ref()
                        .is_some_and(|scope_state| scope_state.contains_key(path))
                });
                state.context_menu = None;
            });
            cx.notify();
        });
    }

    pub fn highlight_agent_edit(&self, cx: &mut App, revision: u64, spans: Vec<argonc::ast::Span>) {
        let generation = self.state.update(cx, |state, cx| {
            state.agent_highlight_generation += 1;
            let generation = state.agent_highlight_generation;
            state.agent_highlight = Some(AgentHighlight {
                revision,
                spans,
                generation,
            });
            cx.notify();
            generation
        });
        self.canvas.update(cx, |_, cx| cx.notify());
        let state = self.state.clone();
        let canvas = self.canvas.clone();
        cx.spawn(async move |cx| {
            cx.background_executor()
                .timer(AGENT_HIGHLIGHT_DURATION)
                .await;
            let _ = state.update(cx, |state, cx| {
                if state
                    .agent_highlight
                    .as_ref()
                    .is_some_and(|highlight| highlight.generation == generation)
                {
                    state.agent_highlight = None;
                    cx.notify();
                }
            });
            let _ = canvas.update(cx, |_, cx| cx.notify());
        })
        .detach();
    }

    pub fn set_agent_activity(&self, cx: &mut App, label: Option<String>) {
        self.state.update(cx, |state, cx| {
            state.agent_activity = label.map(SharedString::from);
            cx.notify();
        });
    }

    pub fn set_workspace_modified(&self, cx: &mut App, modified: bool) {
        self.state.update(cx, |state, cx| {
            state.workspace_modified = modified;
            cx.notify();
        });
        self.title_bar.update(cx, |_, cx| cx.notify());
    }

    pub fn set_workspace_path(&self, cx: &mut App, path: Option<PathBuf>) {
        self.state.update(cx, |state, cx| {
            state.workspace_path = path;
            cx.notify();
        });
        self.title_bar.update(cx, |_, cx| cx.notify());
    }

    pub fn place_instance(&self, cx: &mut App, preview: InstancePreview) {
        self.canvas
            .update(cx, |canvas, cx| canvas.place_instance(preview, cx));
    }

    /// Starts placing a schematic instance, in the schematic view.
    pub fn place_schematic_instance(&self, cx: &mut App, preview: SchematicInstancePreview) {
        self.show_schematic_view(cx);
        self.schematic_canvas
            .update(cx, |canvas, cx| canvas.place_instance(preview, cx));
    }

    /// Starts placing a device, in the schematic view.
    pub fn place_device(&self, cx: &mut App, preview: DevicePreview) {
        self.show_schematic_view(cx);
        self.schematic_canvas
            .update(cx, |canvas, cx| canvas.place_device(preview, cx));
    }

    fn show_schematic_view(&self, cx: &mut App) {
        self.state.update(cx, |state, cx| {
            if state.view != View::Schematic {
                state.view = View::Schematic;
                cx.notify();
            }
        });
    }

    /// The body of the cell the schematic view shows.
    pub fn schematic_target(&self, cx: &App) -> Option<argonc::ast::Span> {
        let state = self.state.read(cx);
        let solved = state.solved_cell.read(cx);
        let solved = solved.as_ref()?;
        let cell = solved.output.cells.get(&solved.output.top)?;
        Some(cell.scopes.get(&cell.root)?.span.clone())
    }

    pub fn selected_scope_span(&self, cx: &App) -> Option<argonc::ast::Span> {
        let state = self.state.read(cx);
        let solved = state.solved_cell.read(cx);
        let solved = solved.as_ref()?;
        let scope = solved.state.get(&solved.selected_scope)?.address;
        Some(
            solved
                .output
                .cells
                .get(&scope.cell)?
                .scopes
                .get(&scope.scope)?
                .span
                .clone(),
        )
    }

    fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // This root listener keeps layout drags alive after the pointer leaves
        // the canvas, but toolbar/sidebar motion must not enter layout hit
        // testing.
        if self.state.read(cx).view != View::Layout {
            return;
        }
        if !self
            .canvas
            .read(cx)
            .should_handle_pointer_move(event.position)
        {
            return;
        }
        self.canvas
            .update(cx, |canvas, cx| canvas.on_mouse_move(event, window, cx));
    }

    fn on_left_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if self.state.read(cx).view != View::Layout {
            return;
        }
        let deferred = self
            .canvas
            .update(cx, |canvas, _| canvas.take_deferred_snapshot());
        if let Some(snapshot) = deferred {
            self.update_cell(cx, snapshot);
        }
    }

    fn on_undo(&mut self, _: &Undo, _window: &mut Window, cx: &mut Context<Self>) {
        let _ = self
            .state
            .read(cx)
            .lang_server_client
            .dispatch_action(LangServerAction::Undo);
    }

    pub(crate) fn on_save(&mut self, _: &Save, _window: &mut Window, cx: &mut Context<Self>) {
        let _ = self
            .state
            .read(cx)
            .lang_server_client
            .dispatch_action(LangServerAction::Save);
    }

    fn on_redo(&mut self, _: &Redo, _window: &mut Window, cx: &mut Context<Self>) {
        let _ = self
            .state
            .read(cx)
            .lang_server_client
            .dispatch_action(LangServerAction::Redo);
    }

    fn focus_invoking_app(
        &mut self,
        _: &FocusInvoker,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        focus_invoker(&self.state, cx);
    }

    fn focus_invoking_app_command_bar(
        &mut self,
        _: &FocusInvokerCommandBar,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_invoking_command(None, true, cx);
    }

    pub(crate) fn show_messages(
        &mut self,
        _: &ShowMessages,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_invoking_command(Some("messages<CR>"), false, cx);
    }

    pub(crate) fn show_diagnostics(
        &mut self,
        _: &ShowDiagnostics,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_invoking_command(Some("Argon diagnostics<CR>"), false, cx);
    }

    fn instantiate_command(
        &mut self,
        _: &InstantiateCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let command = match self.state.read(cx).view {
            View::Layout => "Argon inst ",
            View::Schematic | View::Symbol => "Argon sinst ",
        };
        self.open_invoking_command(Some(command), true, cx);
    }

    fn device_command(&mut self, _: &DeviceCommand, _window: &mut Window, cx: &mut Context<Self>) {
        self.open_invoking_command(Some("Argon device Nmos "), true, cx);
    }

    /// Writes the netlist of the cell on screen where the user says; a
    /// `.scs` file gets Spectre, anything else SPICE.
    fn export_netlist(&mut self, _: &ExportNetlist, _window: &mut Window, cx: &mut Context<Self>) {
        let (data, name) = {
            let state = self.state.read(cx);
            let solved = state.solved_cell.read(cx);
            match solved.as_ref() {
                Some(solved) => (
                    solved.output.clone(),
                    solved
                        .output
                        .cells
                        .get(&solved.output.top)
                        .map(|cell| cell.name.clone())
                        .unwrap_or_else(|| "netlist".into()),
                ),
                None => {
                    self.state.update(cx, |state, cx| {
                        state.show_message(
                            MessageType::ERROR,
                            "Open a cell before exporting its netlist",
                        );
                        cx.notify();
                    });
                    return;
                }
            }
        };
        let directory = self
            .state
            .read(cx)
            .workspace_path
            .as_ref()
            .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
            .unwrap_or_else(|| std::env::current_dir().unwrap_or_default());
        let receiver = cx.prompt_for_new_path(&directory, Some(&format!("{name}.sp")));
        let state = self.state.clone();
        cx.spawn(async move |_, cx| {
            let Ok(Ok(Some(path))) = receiver.await else {
                return;
            };
            let format = if path.extension().is_some_and(|extension| extension == "scs") {
                argonc::netlist::NetlistFormat::Spectre
            } else {
                argonc::netlist::NetlistFormat::Spice
            };
            let netlist = argonc::netlist::netlist(
                &data,
                format,
                &argonc::netlist::NetlistOptions::default(),
            );
            let message = match std::fs::write(&path, netlist) {
                Ok(()) => (MessageType::INFO, format!("Wrote {}", path.display())),
                Err(error) => (
                    MessageType::ERROR,
                    format!("Could not write {}: {error}", path.display()),
                ),
            };
            let _ = state.update(cx, |state, cx| {
                state.show_message(message.0, message.1);
                cx.notify();
            });
        })
        .detach();
    }

    fn open_cell_command(
        &mut self,
        _: &OpenCellCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_invoking_command(Some("Argon openCell "), true, cx);
    }

    pub(crate) fn new_cell_command(
        &mut self,
        _: &NewCellCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_invoking_command(Some("Argon newCell "), true, cx);
    }

    pub(crate) fn rename_cell_command(
        &mut self,
        _: &RenameCellCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_invoking_command(Some("Argon renameCell "), true, cx);
    }

    fn open_invoking_command(
        &mut self,
        command: Option<&str>,
        return_to_gui: bool,
        cx: &mut Context<Self>,
    ) {
        open_command(&self.state, command.map(str::to_owned), return_to_gui, cx);
    }
}

/// Opens `command` at the invoking editor's command line, returning focus
/// to the GUI afterward when `return_to_gui`.
pub(crate) fn open_command(
    state: &Entity<EditorState>,
    command: Option<String>,
    return_to_gui: bool,
    cx: &mut App,
) {
    // A returning workflow may be blocked at a hit-enter or error prompt.
    // Focus Neovim first so the user can clear it before the notification.
    if return_to_gui {
        focus_invoker(state, cx);
    }
    let _ = state
        .read(cx)
        .lang_server_client
        .open_command_bar(command, return_to_gui);
    if !return_to_gui {
        // Leave the final activation request pointed at Neovim. Commands
        // configured this way intentionally do not bounce back to Argon.
        focus_invoker(state, cx);
    }
}

fn focus_invoker(state: &Entity<EditorState>, cx: &mut App) {
    if let Invoker::Pane { window, main } = state.read(cx).invoker.clone() {
        // Deferred so that this also works while `window` is being updated.
        cx.defer(move |cx| {
            let _ = window.update(cx, |_, window, cx| {
                let _ = main.update(cx, |main, cx| main.focus_nvim(window, cx));
            });
        });
        return;
    }
    if !crate::focus::activate_invoker() {
        state.update(cx, |state, _cx| {
            state.fatal_error = Some(EditorMessage {
                typ: MessageType::ERROR,
                text: "could not identify the application that invoked Argone".into(),
                details: MessageDetails::Messages,
            });
        });
    }
}

impl Editor {
    fn theme(&self, cx: &mut Context<Self>) -> &'static Theme {
        self.state.read(cx).theme()
    }
}

impl Render for Editor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme(cx);
        let font_size = self.state.read(cx).font_size;
        let view = self.state.read(cx).view;
        // A schematic-only cell has nothing to show in the layout view.
        let layout_hint = {
            let state = self.state.read(cx);
            let solved = state.solved_cell.read(cx);
            solved
                .as_ref()
                .and_then(|solved| solved.output.cells.get(&solved.output.top))
                .filter(|cell| {
                    cell.objects.is_empty()
                        && !(cell.schematic.nodes.is_empty() && cell.schematic.instances.is_empty())
                })
                .map(|_| "No layout yet: press r to draw a rectangle, or v for the schematic")
        };
        let agent_activity = self.state.read(cx).agent_activity.clone();
        let (displayed_status, activity_label) = {
            let state = self.state.read(cx);
            (
                state
                    .connection_error
                    .clone()
                    .map(|error| {
                        (
                            EditorMessage {
                                typ: MessageType::ERROR,
                                text: error,
                                details: MessageDetails::Messages,
                            },
                            true,
                        )
                    })
                    .or_else(|| state.compilation_error.clone().map(|error| (error, false)))
                    .or_else(|| state.fatal_error.clone().map(|error| (error, false)))
                    .or_else(|| state.message.clone().map(|message| (message, false))),
                activity_status_label(
                    !state.compilation_activities.is_empty(),
                    state.rendering || !state.snapshot_preparations.is_empty(),
                ),
            )
        };
        let mut status_fills_space = false;
        let mut status_bar = div()
            .id("status_bar")
            .border_t_1()
            .border_color(theme.divider)
            .bg(theme.bg)
            .px_2()
            .py_1()
            .min_h(px(27.))
            .flex()
            .flex_row()
            .items_center()
            .gap_2();
        if let Some((message, is_connection_error)) = displayed_status {
            let details = message.details;
            let is_compilation_error = details == MessageDetails::Diagnostics;
            let title = if is_compilation_error {
                "Compilation errors"
            } else if is_connection_error {
                "Connection error"
            } else if message.typ == MessageType::ERROR {
                "Error"
            } else if message.typ == MessageType::WARNING {
                "Warning"
            } else {
                "Message"
            };
            let status_text = if is_compilation_error {
                SharedString::from(title)
            } else {
                SharedString::from(format!("{title}: {}", message.text))
            };
            let action_label = if is_compilation_error {
                "Open diagnostics (Ctrl-Shift-D)"
            } else {
                "View messages (Ctrl-Shift-M)"
            };
            let mut status_message = div()
                .overflow_x_hidden()
                .text_ellipsis()
                .text_color(theme.error)
                .child(status_text);
            if !is_compilation_error {
                status_message = status_message.flex_1();
                status_fills_space = true;
            }
            status_bar = status_bar
                .child(
                    svg()
                        .path("icons/circle-exclamation-solid-full.svg")
                        .w(px(14.))
                        .h_auto()
                        .text_color(theme.error),
                )
                .child(status_message)
                .child(
                    div()
                        .id("show_status_details")
                        .text_color(theme.text)
                        .cursor_pointer()
                        .child(action_label)
                        .on_click(cx.listener(move |editor, _, _, cx| {
                            if details == MessageDetails::Diagnostics {
                                editor.open_invoking_command(
                                    Some("Argon diagnostics<CR>"),
                                    false,
                                    cx,
                                );
                            } else {
                                editor.open_invoking_command(Some("messages<CR>"), false, cx);
                            }
                        })),
                );
        }
        if let Some(hint) = self.schematic_canvas.read(cx).hint() {
            status_bar = status_bar.child(
                div()
                    .id("schematic_tool_hint")
                    .text_color(theme.subtext)
                    .text_ellipsis()
                    .child(hint),
            );
        }
        if !status_fills_space {
            status_bar = status_bar.child(div().flex_1());
        }
        if let Some(agent_activity) = agent_activity {
            status_bar = status_bar.child(
                div()
                    .id("agent_activity")
                    .flex_shrink()
                    .min_w_0()
                    .overflow_x_hidden()
                    .text_ellipsis()
                    .text_color(theme.subtext)
                    .child(SharedString::from(format!("Agent: {agent_activity}"))),
            );
        }
        // Keep the activity row's intrinsic height even while idle. Adding
        // the label used to shrink the canvas, invalidating every raster tile;
        // completing that raster hid the label and restarted the cycle.
        // An invisible, non-animated row also respects custom font sizes.
        let activity_icon = svg()
            .path("icons/arrow-rotate-right-solid-full.svg")
            .w(px(14.))
            .h_auto()
            .text_color(theme.subtext);
        let activity_icon = if activity_label.is_some() {
            activity_icon
                .with_animation(
                    "compilation_spinner",
                    Animation::new(Duration::from_millis(800)).repeat(),
                    |icon, delta| {
                        icon.with_transformation(Transformation::rotate(percentage(delta)))
                    },
                )
                .into_any_element()
        } else {
            activity_icon.into_any_element()
        };
        status_bar = status_bar.child(
            div()
                .id("compilation_status")
                .flex()
                .flex_row()
                .flex_shrink_0()
                .items_center()
                .gap_1()
                .text_color(theme.subtext)
                .when(activity_label.is_none(), |row| row.invisible())
                .child(activity_icon)
                .child(activity_label.unwrap_or("Rendering")),
        );
        // Until a cell opens, there is nothing for the tools or sidebars to act on.
        let start_screen = self.state.read(cx).solved_cell.read(cx).is_none();
        let body = if start_screen {
            self.render_start_screen(window, cx)
        } else {
            match view {
                View::Layout => div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(self.hierarchy_sidebar.clone())
                    .child(
                        div()
                            .flex_1()
                            .relative()
                            .overflow_hidden()
                            .child(self.canvas.clone())
                            .children(layout_hint.map(|hint| {
                                div()
                                    .absolute()
                                    .top_2()
                                    .left_0()
                                    .right_0()
                                    .flex()
                                    .justify_center()
                                    .text_color(theme.subtext)
                                    .child(hint)
                            })),
                    )
                    .child(self.layer_sidebar.clone()),
                View::Schematic | View::Symbol => div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(self.schematic_sidebar.clone())
                    .child(
                        div()
                            .flex_1()
                            .relative()
                            .overflow_hidden()
                            .child(self.schematic_canvas.clone()),
                    )
                    .child(self.properties_sidebar.clone()),
            }
            .into_any_element()
        };
        let active_focus = self.active_focus_handle(cx);
        let mut root = div()
            .id("top")
            .track_focus(&active_focus)
            .when(start_screen, |root| root.key_context(START_SCREEN_CONTEXT))
            .on_action(cx.listener(Self::view_layout))
            .on_action(cx.listener(Self::view_schematic))
            .on_action(cx.listener(Self::view_symbol))
            .on_action(cx.listener(Self::next_view))
            .on_action(cx.listener(Self::previous_view))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::focus_invoking_app))
            .on_action(cx.listener(Self::focus_invoking_app_command_bar))
            .on_action(cx.listener(Self::show_diagnostics))
            .on_action(cx.listener(Self::show_messages))
            .on_action(cx.listener(Self::instantiate_command))
            .on_action(cx.listener(Self::device_command))
            .on_action(cx.listener(Self::export_netlist))
            .on_action(cx.listener(Self::open_cell_command))
            .on_action(cx.listener(Self::new_cell_command))
            .on_action(cx.listener(Self::rename_cell_command))
            .font_family("Zed Plex Sans")
            .size_full()
            .flex()
            .flex_col()
            .justify_start()
            .bg(theme.bg)
            .text_color(theme.text)
            .overflow_hidden()
            .whitespace_nowrap()
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_left_mouse_up))
            .when(!start_screen, |root| root.child(self.tool_bar.clone()))
            .child(body)
            .child(status_bar);
        root = if let Some(font_size) = font_size {
            root.text_size(px(font_size))
        } else {
            root.text_sm()
        };
        root
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use argonc::{
        ast::Span,
        compile::{CompileOutput, StaticError, StaticErrorCompileOutput, StaticErrorKind},
    };

    use argonc::tech::CustomDitherPattern;

    use super::{
        MessageDetails, ShapeFill, View, activity_status_label, compilation_error_message,
        shape_fill,
    };

    #[test]
    fn v_cycles_the_views_and_shift_v_cycles_back() {
        let cycle = [View::Layout, View::Schematic, View::Symbol];
        for (i, view) in cycle.iter().enumerate() {
            assert_eq!(view.next(), cycle[(i + 1) % 3]);
            assert_eq!(view.previous(), cycle[(i + 2) % 3]);
            assert_eq!(view.next().previous(), *view);
        }
    }

    #[test]
    fn compilation_status_hands_off_to_rendering_without_an_idle_state() {
        assert_eq!(activity_status_label(true, true), Some("Compiling"));
        assert_eq!(activity_status_label(false, true), Some("Rendering"));
        assert_eq!(activity_status_label(false, false), None);
    }

    #[test]
    fn compilation_error_message_does_not_embed_individual_diagnostics() {
        let span = Span {
            path: PathBuf::from("lib.ar"),
            span: cfgrammar::Span::new(0, 1),
        };
        let output = CompileOutput::StaticErrors(StaticErrorCompileOutput {
            errors: vec![
                StaticError {
                    span: span.clone(),
                    kind: StaticErrorKind::UndeclaredVar {
                        name: "missing".to_owned(),
                    },
                },
                StaticError {
                    span,
                    kind: StaticErrorKind::InvalidKwArg,
                },
            ],
        });

        let message = compilation_error_message(&output).expect("compilation should have failed");
        assert_eq!(message.text.as_ref(), "Compilation errors");
        assert_eq!(message.details, MessageDetails::Diagnostics);

        let message = compilation_error_message(&CompileOutput::FatalParseErrors)
            .expect("compilation should have failed");
        assert_eq!(message.text.as_ref(), "Compilation errors");
        assert_eq!(message.details, MessageDetails::Diagnostics);

        let directory = tempfile::tempdir().unwrap();
        let source_path = directory.path().join("lib.ar");
        std::fs::write(&source_path, "cell top() {}\n").unwrap();
        let ast = argonc::parse::parse_workspace_with_std(&source_path).ast();
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let valid = argonc::compile::compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["top"],
                args: vec![],
            },
            &argonc::WorkspaceConfig::default().with_tech(Some(tech)),
        );
        assert!(matches!(&valid, CompileOutput::Valid(_)));
        assert!(compilation_error_message(&valid).is_none());
    }

    #[test]
    fn custom_dither_references_use_technology_array_positions() {
        let patterns = vec![
            CustomDitherPattern {
                lines: vec!["........".to_owned(); 8],
                order: 90,
                name: "blank".to_owned(),
            },
            CustomDitherPattern {
                lines: vec!["********".to_owned(); 8],
                order: 10,
                name: "solid".to_owned(),
            },
            CustomDitherPattern {
                lines: (0..8)
                    .map(|y| {
                        let mut row = [b'.'; 8];
                        row[y] = b'*';
                        String::from_utf8(row.to_vec()).unwrap()
                    })
                    .collect(),
                order: 1,
                name: "diagonal".to_owned(),
            },
        ];

        assert_eq!(shape_fill("C0", &patterns), ShapeFill::Hollow);
        assert_eq!(shape_fill("C1", &patterns), ShapeFill::Solid);
        assert!(matches!(shape_fill("C2", &patterns), ShapeFill::Pattern(_)));
        assert_eq!(
            shape_fill("C9", &patterns),
            ShapeFill::Stippling,
            "missing custom references retain the legacy fallback"
        );
    }

    #[test]
    fn every_sky130_custom_dither_reference_resolves_to_its_bitmap() {
        let tech = argonc::tech::read_tech(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../pdks/sky130/sky130.tech.toml"),
        )
        .unwrap();
        let custom_layers = tech
            .layers
            .iter()
            .filter(|layer| layer.style.dither_pattern.starts_with('C'))
            .collect::<Vec<_>>();

        assert!(!custom_layers.is_empty());
        for layer in custom_layers {
            assert_ne!(
                shape_fill(&layer.style.dither_pattern, &tech.custom_dither_patterns,),
                ShapeFill::Stippling,
                "{} references an unavailable custom bitmap",
                layer.name
            );
        }
    }
}
