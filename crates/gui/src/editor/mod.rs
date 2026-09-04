use std::{
    hash::{DefaultHasher, Hash, Hasher},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use analyzer::rpc::{CompilationSnapshot, InstancePreview, LangServerAction};
use argonc::compile::{
    CellId, CompileOutput, CompiledData, ExecErrorCompileOutput, Rect, ScopeId, SolvedValue,
    bbox_dim_union, bbox_text_union, bbox_union, ifmatvec,
};
use canvas::{LayoutCanvas, ShapeFill, StipplePattern};
use futures::StreamExt;
use geometry::transform::TransformationMatrix;
use gpui::*;
use indexmap::{IndexMap, IndexSet};
use rgb::Rgb;
use toolbars::{HierarchySideBar, LayerSideBar, TitleBar, ToolBar};
use tower_lsp_server::ls_types::MessageType;

use crate::{
    actions::{
        FocusInvoker, FocusInvokerCommandBar, InstantiateCommand, NewCellCommand, OpenCellCommand,
        Redo, RenameCellCommand, Save, ShowDiagnostics, ShowMessages, Undo,
    },
    editor::{canvas::ToolState, input::TextInput},
    rpc::SyncLangServerClient,
    theme::{DARK_THEME, LIGHT_THEME, Theme},
};

pub mod canvas;
pub mod input;
pub mod toolbars;

pub(crate) const SOURCE_EDIT_REJECTED_MESSAGE: &str =
    "Could not apply the source edit. Press Ctrl-Shift-M to view the detailed error in Neovim.";

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
    pub state: Arc<IndexMap<ScopePath, ScopeState>>,
    pub scope_paths: Arc<IndexMap<ScopeAddress, ScopePath>>,
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
    pub solved_cell: Entity<Option<CompileOutputState>>,
    pub hide_external_geometry: bool,
    pub layers: Entity<Layers>,
    pub lang_server_client: SyncLangServerClient,
    pub subscriptions: Vec<Subscription>,
    pub(crate) tool: Entity<ToolState>,
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
    state: IndexMap<ScopePath, ScopeState>,
    scope_paths: IndexMap<ScopeAddress, ScopePath>,
}

pub(crate) struct CompilationPreparationContext {
    layers: IndexMap<SharedString, LayerState>,
    selected_scope: Option<ScopePath>,
    scope_state: Option<Arc<IndexMap<ScopePath, ScopeState>>>,
}

struct PreparedCompileOutput {
    layers: IndexMap<SharedString, LayerState>,
    selected_scope: ScopePath,
    state: IndexMap<ScopePath, ScopeState>,
    scope_paths: IndexMap<ScopeAddress, ScopePath>,
}

pub(crate) struct PreparedCompilationSnapshot {
    pub(crate) revision: u64,
    pub(crate) output: CompileOutput,
    compilation_error: Option<EditorMessage>,
    prepared_output: Option<PreparedCompileOutput>,
}

fn mark_layer_used(state: &mut ProcessScopeState, layer: &str) {
    let layer = SharedString::from(layer.to_owned());
    if let Some(layer_info) = state.layers.get_mut(&layer) {
        layer_info.used = true;
    } else {
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
}

impl EditorState {
    fn theme(&self) -> &'static Theme {
        if self.dark_mode {
            &DARK_THEME
        } else {
            &LIGHT_THEME
        }
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
    fn process_scope(
        solved_cell: &CompiledData,
        scope: ScopeAddress,
        state: &mut ProcessScopeState,
        parent: Option<ScopeAddress>,
        old_scope_state: Option<&IndexMap<ScopePath, ScopeState>>,
    ) {
        let scope_info = &solved_cell.cells[&scope.cell].scopes[&scope.scope];
        let mut scope_path = if let Some(parent) = &parent {
            state.scope_paths[parent].clone()
        } else {
            vec![]
        };
        scope_path.push(scope_info.name.clone());
        state.scope_paths.insert(scope, scope_path.clone());
        let mut bbox = None;
        for (obj, _) in &scope_info.emit {
            let value = &solved_cell.cells[&scope.cell].objects[obj];
            match value {
                SolvedValue::Rect(rect) => {
                    bbox = bbox_union(bbox, Some(rect.to_float()));
                    if let Some(layer) = &rect.layer {
                        mark_layer_used(state, layer);
                    }
                }
                SolvedValue::Polygon(polygon) => {
                    bbox = bbox_union(bbox, polygon.bbox());
                    let layer = SharedString::from(&polygon.layer);
                    if let Some(layer_info) = state.layers.get_mut(&layer) {
                        layer_info.used = true;
                    } else {
                        let mut s = DefaultHasher::new();
                        layer.hash(&mut s);
                        let hash = s.finish() as usize;
                        let color =
                            rgb([0xff0000, 0x0ff000, 0x00ff00, 0x000ff0, 0x0000ff][hash % 5]);
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
                }
                SolvedValue::Path(path) => {
                    bbox = bbox_union(bbox, path.bbox());
                    let layer = SharedString::from(&path.layer);
                    if let Some(layer_info) = state.layers.get_mut(&layer) {
                        layer_info.used = true;
                    } else {
                        let mut s = DefaultHasher::new();
                        layer.hash(&mut s);
                        let hash = s.finish() as usize;
                        let color =
                            rgb([0xff0000, 0x0ff000, 0x00ff00, 0x000ff0, 0x0000ff][hash % 5]);
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
                }
                SolvedValue::Instance(inst) => {
                    let inst_address = ScopeAddress {
                        scope: solved_cell.cells[&inst.cell].root,
                        cell: inst.cell,
                    };
                    Self::process_scope(
                        solved_cell,
                        inst_address,
                        state,
                        Some(scope),
                        old_scope_state,
                    );
                    bbox = bbox_union(
                        bbox,
                        state.state[&state.scope_paths[&inst_address]]
                            .bbox
                            .as_ref()
                            .map(|rect| {
                                let mut inst_mat = TransformationMatrix::identity();
                                if inst.reflect {
                                    inst_mat = inst_mat.reflect_vert()
                                }
                                inst_mat = inst_mat.rotate(inst.angle);
                                let p0p = ifmatvec(inst_mat, (rect.x0, rect.y0));
                                let p1p = ifmatvec(inst_mat, (rect.x1, rect.y1));
                                Rect {
                                    layer: None,
                                    x0: p0p.0.min(p1p.0) + inst.x,
                                    y0: p0p.1.min(p1p.1) + inst.y,
                                    x1: p0p.0.max(p1p.0) + inst.x,
                                    y1: p0p.1.max(p1p.1) + inst.y,
                                    id: inst.id,
                                    construction: true,
                                    span: rect.span.clone(),
                                }
                            }),
                    );
                }
                SolvedValue::Dimension(dim) => {
                    bbox = bbox_dim_union(bbox, dim);
                }
                SolvedValue::Text(t) => {
                    bbox = bbox_text_union(bbox, t);
                    mark_layer_used(state, &t.layer);
                }
            }
        }

        for child in &scope_info.children {
            let scope_address = ScopeAddress {
                scope: *child,
                cell: scope.cell,
            };
            Self::process_scope(
                solved_cell,
                scope_address,
                state,
                Some(scope),
                old_scope_state,
            );
            bbox = bbox_union(
                bbox,
                state.state[&state.scope_paths[&scope_address]].bbox.clone(),
            );
        }

        let visible = old_scope_state
            .and_then(|state| state.get(&scope_path).map(|scope| scope.visible))
            .unwrap_or(true);
        state.state.insert(
            scope_path,
            ScopeState {
                name: scope_info.name.clone(),
                address: scope,
                visible,
                bbox,
                parent,
            },
        );
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

    fn compilation_preparation_context(&self, cx: &App) -> CompilationPreparationContext {
        let old_cell = self.solved_cell.read(cx);
        CompilationPreparationContext {
            layers: self.layers.read(cx).layers.clone(),
            selected_scope: old_cell.as_ref().map(|cell| cell.selected_scope.clone()),
            scope_state: old_cell.as_ref().map(|cell| cell.state.clone()),
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
        EditorState::process_scope(
            solved_cell,
            root_scope,
            &mut state,
            None,
            context.scope_state.as_deref(),
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
    pub fn new(
        cx: &mut Context<Self>,
        window: &mut Window,
        lang_server_addr: SocketAddr,
        gui_listen_port: Option<u16>,
        gui_listener: Option<std::net::TcpListener>,
        gui_register_addr: Option<SocketAddr>,
    ) -> Self {
        let (lang_server_client, mut rx) =
            SyncLangServerClient::new(cx.to_async(), lang_server_addr);
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
                solved_cell,
                hide_external_geometry: false,
                tool,
                layers,
                subscriptions,
                lang_server_client: lang_server_client.clone(),
            }
        });
        let title_bar = cx.new(|_cx| TitleBar::new(&state));
        let tool_bar = cx.new(|_cx| ToolBar::new(&state));
        let canvas_focus_handle = cx.focus_handle();
        let text_input_focus_handle = cx.focus_handle();
        window.focus(&canvas_focus_handle);
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

        let editor = Self {
            state,
            title_bar,
            tool_bar,
            hierarchy_sidebar,
            layer_sidebar,
            canvas,
        };
        cx.to_async()
            .spawn({
                let editor = editor.clone();
                async move |app| loop {
                    if let Some(exec) = rx.next().await {
                        exec(&editor, app);
                    }
                }
            })
            .detach();
        lang_server_client.register_server(gui_listen_port, gui_listener, gui_register_addr);

        editor
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
        self.state.update(cx, |state, cx| {
            state.connection_error = None;
            state.compilation_revision = Some(snapshot.revision);
            if snapshot.prepared_output.is_some() {
                // Keep the status animation alive between hierarchy preparation
                // and the first raster worker started by the next paint.
                state.rendering = true;
            }
            state.apply_prepared_output(cx, snapshot);
            cx.notify();
        });
        self.canvas
            .update(cx, |canvas, cx| canvas.finish_sse_persist(cx));
        true
    }

    pub fn fit_to_screen(&self, cx: &mut App) {
        self.canvas.update(cx, |canvas, cx| {
            canvas.fit_to_screen(cx);
            cx.notify();
        });
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
        // This root listener keeps drags alive after the pointer leaves the
        // canvas, but toolbar/sidebar motion must not enter layout hit testing.
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

    fn on_save(&mut self, _: &Save, _window: &mut Window, cx: &mut Context<Self>) {
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
        self.focus_invoker(cx);
    }

    fn focus_invoking_app_command_bar(
        &mut self,
        _: &FocusInvokerCommandBar,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_invoking_command(None, true, cx);
    }

    fn show_messages(&mut self, _: &ShowMessages, _window: &mut Window, cx: &mut Context<Self>) {
        self.open_invoking_command(Some("messages<CR>"), false, cx);
    }

    fn show_diagnostics(
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
        self.open_invoking_command(Some("Argon inst "), true, cx);
    }

    fn open_cell_command(
        &mut self,
        _: &OpenCellCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_invoking_command(Some("Argon openCell "), true, cx);
    }

    fn new_cell_command(
        &mut self,
        _: &NewCellCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_invoking_command(Some("Argon newCell "), true, cx);
    }

    fn rename_cell_command(
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
        // A returning workflow may be blocked at a hit-enter or error prompt.
        // Focus Neovim first so the user can clear it before the notification.
        if return_to_gui {
            self.focus_invoker(cx);
        }
        let _ = self
            .state
            .read(cx)
            .lang_server_client
            .open_command_bar(command.map(str::to_owned), return_to_gui);
        if !return_to_gui {
            // Leave the final activation request pointed at Neovim. Commands
            // configured this way intentionally do not bounce back to Argon.
            self.focus_invoker(cx);
        }
    }

    fn focus_invoker(&mut self, cx: &mut Context<Self>) {
        if !crate::focus::activate_invoker() {
            self.state.update(cx, |state, _cx| {
                state.fatal_error = Some(EditorMessage {
                    typ: MessageType::ERROR,
                    text: "could not identify the application that invoked Argone".into(),
                    details: MessageDetails::Messages,
                });
            });
        }
    }

    fn theme(&self, cx: &mut Context<Self>) -> &'static Theme {
        self.state.read(cx).theme()
    }
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.theme(cx);
        let font_size = self.state.read(cx).font_size;
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
        if let Some(activity_label) = activity_label {
            if !status_fills_space {
                status_bar = status_bar.child(div().flex_1());
            }
            status_bar = status_bar.child(
                div()
                    .id("compilation_status")
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_1()
                    .text_color(theme.subtext)
                    .child(
                        svg()
                            .path("icons/arrow-rotate-right-solid-full.svg")
                            .w(px(14.))
                            .h_auto()
                            .text_color(theme.subtext)
                            .with_animation(
                                "compilation_spinner",
                                Animation::new(Duration::from_millis(800)).repeat(),
                                |icon, delta| {
                                    icon.with_transformation(Transformation::rotate(percentage(
                                        delta,
                                    )))
                                },
                            ),
                    )
                    .child(activity_label),
            );
        }
        let mut root = div()
            .id("top")
            .track_focus(&self.canvas.focus_handle(cx))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::focus_invoking_app))
            .on_action(cx.listener(Self::focus_invoking_app_command_bar))
            .on_action(cx.listener(Self::show_diagnostics))
            .on_action(cx.listener(Self::show_messages))
            .on_action(cx.listener(Self::instantiate_command))
            .on_action(cx.listener(Self::open_cell_command))
            .on_action(cx.listener(Self::new_cell_command))
            .on_action(cx.listener(Self::rename_cell_command))
            .font_family("Zed Plex Sans")
            .size_full()
            .flex()
            .flex_col()
            .justify_start()
            .border_1()
            .border_color(theme.divider)
            .bg(theme.bg)
            .rounded(px(10.))
            .text_color(theme.text)
            .overflow_hidden()
            .whitespace_nowrap()
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_left_mouse_up))
            .child(self.title_bar.clone())
            .child(self.tool_bar.clone())
            .child(
                div()
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
                            .child(self.canvas.clone()),
                    )
                    .child(self.layer_sidebar.clone()),
            )
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
        MessageDetails, ShapeFill, activity_status_label, compilation_error_message, shape_fill,
    };

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
