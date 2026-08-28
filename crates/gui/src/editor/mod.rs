use std::{
    hash::{DefaultHasher, Hash, Hasher},
    net::SocketAddr,
    path::PathBuf,
    sync::Arc,
};

use analyzer::rpc::{
    CompilationDelta, CompilationSnapshot, CompilationUpdate, InstancePreview, LangServerAction,
};
use argonc::compile::{
    CellId, CompileOutput, CompiledData, ExecErrorCompileOutput, Rect, ScopeId, SolvedValue,
    bbox_dim_union, bbox_text_union, bbox_union, ifmatvec,
};
use canvas::{LayoutCanvas, ShapeFill};
use futures::StreamExt;
use geometry::transform::TransformationMatrix;
use gpui::*;
use indexmap::{IndexMap, IndexSet};
use rgb::Rgb;
use toolbars::{HierarchySideBar, LayerSideBar, TitleBar, ToolBar};
use tower_lsp_server::ls_types::MessageType;

use crate::{
    actions::{
        FocusInvoker, FocusInvokerCommandBar, InstantiateCommand, OpenCellCommand, Redo, Save,
        ShowMessages, Undo,
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
    /// Layers referenced by each compiled cell. Keeping this per cell lets a
    /// small edit reuse layer discovery for very large imported cells.
    pub used_layers: Arc<IndexMap<CellId, IndexSet<SharedString>>>,
}

pub struct Layers {
    pub layers: IndexMap<SharedString, LayerState>,
    pub selected_layer: Option<SharedString>,
}

pub struct EditorState {
    pub hierarchy_depth: usize,
    pub dark_mode: bool,
    pub workspace_path: Option<PathBuf>,
    pub workspace_modified: bool,
    pub compilation_revision: Option<u64>,
    /// Last materialized compiler output. Deltas are applied here before any
    /// presentation state is updated.
    pub last_output: Option<CompileOutput>,
    pub fatal_error: Option<SharedString>,
    pub message: Option<EditorMessage>,
    pub connection_error: Option<SharedString>,
    pub solved_cell: Entity<Option<CompileOutputState>>,
    pub hide_external_geometry: bool,
    pub layers: Entity<Layers>,
    pub lang_server_client: SyncLangServerClient,
    pub subscriptions: Vec<Subscription>,
    pub(crate) tool: Entity<ToolState>,
}

fn apply_compilation_delta(
    previous_revision: Option<u64>,
    previous: Option<&CompileOutput>,
    delta: CompilationDelta,
) -> Option<CompileOutput> {
    if previous_revision != Some(delta.base_revision) {
        return None;
    }
    let mut data = match previous? {
        CompileOutput::Valid(data) => data.clone(),
        CompileOutput::ExecErrors(ExecErrorCompileOutput {
            output: Some(data), ..
        }) => data.clone(),
        _ => return None,
    };
    for cell in delta.removed_cells {
        data.cells.shift_remove(&cell);
    }
    for (cell, compiled) in delta.cells {
        data.cells.insert(cell, compiled);
    }
    data.top = delta.top;
    Some(if delta.errors.is_empty() {
        CompileOutput::Valid(data)
    } else {
        CompileOutput::ExecErrors(ExecErrorCompileOutput {
            errors: delta.errors,
            output: Some(data),
        })
    })
}

fn materialize_compilation_update(
    previous_revision: Option<u64>,
    previous: Option<&CompileOutput>,
    update: CompilationUpdate,
) -> Option<CompileOutput> {
    match update {
        CompilationUpdate::Full(output) => Some(output),
        CompilationUpdate::Delta(delta) => {
            apply_compilation_delta(previous_revision, previous, delta)
        }
    }
}

#[derive(Clone, Debug)]
pub struct EditorMessage {
    pub typ: MessageType,
    pub text: SharedString,
}

fn compile_error_summary(output: &CompileOutput) -> Option<String> {
    let diagnostics = argonc::diagnostics::from_compile_output(output);
    let first = diagnostics.first()?;
    let remaining = diagnostics.len() - 1;
    Some(if remaining == 0 {
        first.message.clone()
    } else {
        format!(
            "{} (and {remaining} more error{})",
            first.message,
            if remaining == 1 { "" } else { "s" }
        )
    })
}

#[derive(Clone, Debug)]
pub struct Editor {
    pub state: Entity<EditorState>,
    pub title_bar: Entity<TitleBar>,
    pub tool_bar: Entity<ToolBar>,
    pub hierarchy_sidebar: Entity<HierarchySideBar>,
    pub layer_sidebar: Entity<LayerSideBar>,
    pub canvas: Entity<LayoutCanvas>,
    pub(crate) text_input: Entity<TextInput>,
}

fn rgb_to_rgba(color: Rgb<u8>) -> Rgba {
    rgb(((color.r as u32) << 16) | ((color.g as u32) << 8) | color.b as u32)
}

fn shape_fill(dither_pattern: &str) -> ShapeFill {
    match dither_pattern {
        "I0" => ShapeFill::Solid,
        "I1" => ShapeFill::Hollow,
        // Built-in and custom patterns are retained by the technology model.
        // Until the renderer implements each bitmap, use its existing stipple.
        _ => ShapeFill::Stippling,
    }
}

#[derive(Default)]
struct ProcessScopeState {
    layers: IndexMap<SharedString, LayerState>,
    state: IndexMap<ScopePath, ScopeState>,
    scope_paths: IndexMap<ScopeAddress, ScopePath>,
    unchanged_cells: IndexSet<CellId>,
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

fn compiled_cell_used_layers(cell: &argonc::compile::CompiledCell) -> IndexSet<SharedString> {
    let mut layers = IndexSet::new();
    for scope in cell.scopes.values() {
        for (object, _) in &scope.emit {
            let layer = match &cell.objects[object] {
                SolvedValue::Rect(rect) => rect.layer.as_deref(),
                SolvedValue::Polygon(polygon) => Some(polygon.layer.as_str()),
                SolvedValue::Path(path) => Some(path.layer.as_str()),
                SolvedValue::Text(text) => Some(text.layer.as_str()),
                SolvedValue::Instance(_) | SolvedValue::Dimension(_) => None,
            };
            if let Some(layer) = layer {
                layers.insert(SharedString::from(layer.to_owned()));
            }
        }
    }
    layers
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
            });
        }
    }
    fn process_scope(
        &self,
        cx: &App,
        solved_cell: &CompiledData,
        scope: ScopeAddress,
        state: &mut ProcessScopeState,
        parent: Option<ScopeAddress>,
    ) {
        if state.unchanged_cells.contains(&scope.cell) && state.scope_paths.contains_key(&scope) {
            return;
        }
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
                    self.process_scope(cx, solved_cell, inst_address, state, Some(scope));
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
            self.process_scope(cx, solved_cell, scope_address, state, Some(scope));
            bbox = bbox_union(
                bbox,
                state.state[&state.scope_paths[&scope_address]].bbox.clone(),
            );
        }

        let visible = self
            .solved_cell
            .read(cx)
            .as_ref()
            .and_then(|cell| Some(cell.state.get(&scope_path)?.visible))
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
    pub fn update(&mut self, cx: &mut App, output: CompileOutput) {
        self.last_output = Some(output.clone());
        let error_summary = compile_error_summary(&output).map(SharedString::from);
        let mut recoverable_error = None;
        let solved_cell = match output {
            CompileOutput::Valid(d) => d,
            CompileOutput::ExecErrors(ExecErrorCompileOutput {
                output: Some(d),
                errors,
            }) => {
                if errors.iter().any(|error| error.kind.is_invalid_cell()) {
                    self.fatal_error = error_summary;
                    return;
                }
                recoverable_error = error_summary;
                d
            }
            _ => {
                self.fatal_error = error_summary;
                return;
            }
        };
        let old_cell = self.solved_cell.read(cx).clone();
        let root_scope = ScopeAddress {
            scope: solved_cell.cells[&solved_cell.top].root,
            cell: solved_cell.top,
        };
        let root_scope_name = &solved_cell.cells[&root_scope.cell].scopes[&root_scope.scope]
            .name
            .clone();
        let mut state = ProcessScopeState::default();
        let mut used_layers = IndexMap::new();
        for (cell_id, cell) in &solved_cell.cells {
            let unchanged = old_cell
                .as_ref()
                .and_then(|old| old.output.cells.get(cell_id))
                .is_some_and(|old| Arc::ptr_eq(old, cell));
            if unchanged {
                state.unchanged_cells.insert(*cell_id);
            }
            let layers = if unchanged {
                old_cell
                    .as_ref()
                    .and_then(|old| old.used_layers.get(cell_id))
                    .cloned()
                    .unwrap_or_else(|| compiled_cell_used_layers(cell))
            } else {
                compiled_cell_used_layers(cell)
            };
            used_layers.insert(*cell_id, layers);
        }
        if let Some(old) = &old_cell {
            for (address, path) in old.scope_paths.iter() {
                if state.unchanged_cells.contains(&address.cell) {
                    state.scope_paths.insert(*address, path.clone());
                    if let Some(scope) = old.state.get(path) {
                        state.state.insert(path.clone(), scope.clone());
                    }
                }
            }
        }
        let old_layers = self.layers.read(cx);
        for layer in &solved_cell.tech.layers {
            let name = SharedString::from(layer.name.clone());
            let visible = old_layers
                .layers
                .get(&name)
                .map(|layer| layer.visible)
                .unwrap_or(layer.style.visible);
            state.layers.insert(
                name.clone(),
                LayerState {
                    name,
                    color: rgb_to_rgba(layer.fill_color),
                    fill: shape_fill(&layer.style.dither_pattern),
                    border_color: rgb_to_rgba(layer.border_color),
                    visible,
                    used: false,
                    z: state.layers.len(),
                },
            );
        }
        for layer in used_layers.values().flatten() {
            mark_layer_used(&mut state, layer);
        }
        self.process_scope(cx, &solved_cell, root_scope, &mut state, None);
        let ProcessScopeState {
            layers,
            state,
            scope_paths,
            unchanged_cells: _,
        } = state;
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
                selected_scope: old_cell
                    .as_ref()
                    .and_then(|cell| {
                        state
                            .contains_key(&cell.selected_scope)
                            .then(|| cell.selected_scope.clone())
                    })
                    .unwrap_or_else(|| vec![root_scope_name.clone()]),
                state: Arc::new(state),
                scope_paths: Arc::new(scope_paths),
                used_layers: Arc::new(used_layers),
            });
            cx.notify();
        });
        self.fatal_error = None;
        self.message = recoverable_error.map(|text| EditorMessage {
            typ: MessageType::ERROR,
            text,
        });
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
                workspace_path: None,
                workspace_modified: false,
                compilation_revision: None,
                last_output: None,
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
        let canvas = cx.new(|cx| {
            LayoutCanvas::new(
                cx,
                &state,
                canvas_focus_handle.clone(),
                text_input_focus_handle.clone(),
            )
        });
        let text_input = cx
            .new(|cx| TextInput::new_dimension_input(cx, text_input_focus_handle, &state, &canvas));
        let hierarchy_sidebar = cx.new(|cx| HierarchySideBar::new(cx, &state, &canvas));
        let layer_sidebar = cx.new(|cx| LayerSideBar::new(cx, &state, &canvas));

        let editor = Self {
            state,
            title_bar,
            tool_bar,
            hierarchy_sidebar,
            layer_sidebar,
            canvas,
            text_input,
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

    fn apply_output(&self, cx: &mut App, revision: u64, output: CompileOutput) -> bool {
        if self
            .state
            .read(cx)
            .compilation_revision
            .is_some_and(|current| revision < current)
        {
            return false;
        }
        self.state.update(cx, |state, cx| {
            state.connection_error = None;
            state.compilation_revision = Some(revision);
            state.update(cx, output);
            cx.notify();
        });
        self.canvas.update(cx, |canvas, cx| {
            canvas.finish_sse_persist(cx);
            canvas.finish_source_compilation(revision, cx);
        });
        true
    }

    pub fn fit_to_screen(&self, cx: &mut App) {
        self.canvas.update(cx, |canvas, cx| {
            canvas.fit_to_screen(cx);
            cx.notify();
        });
    }

    pub fn update_cell(&self, cx: &mut App, snapshot: CompilationSnapshot) -> bool {
        let revision = snapshot.revision;
        let output = {
            let state = self.state.read(cx);
            if state
                .compilation_revision
                .is_some_and(|current| revision < current)
            {
                return false;
            }
            let Some(output) = materialize_compilation_update(
                state.compilation_revision,
                state.last_output.as_ref(),
                snapshot.update,
            ) else {
                return false;
            };
            output
        };
        if !self.canvas.read(cx).accepts_compilation(revision, &output) {
            return false;
        }
        if self.canvas.read(cx).is_sse_dragging() {
            self.canvas.update(cx, |canvas, _| {
                canvas.defer_snapshot(CompilationSnapshot {
                    revision,
                    update: CompilationUpdate::Full(output),
                })
            });
            return true;
        }
        if !self.apply_output(cx, revision, output) {
            return false;
        }
        let state = self.state.clone();
        self.hierarchy_sidebar.update(cx, move |sidebar, cx| {
            let scope_paths: IndexSet<_> = state
                .read(cx)
                .solved_cell
                .read(cx)
                .as_ref()
                .map(|cell| cell.state.keys().cloned().collect())
                .unwrap_or_default();
            sidebar.state.update(cx, |state, _cx| {
                state
                    .expanded_scopes
                    .retain(|path| scope_paths.contains(path));
            });
            cx.notify();
        });
        true
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
        self.canvas
            .update(cx, |canvas, cx| canvas.on_mouse_move(event, window, cx));
        cx.notify();
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
        self.open_invoking_command(None, cx);
    }

    fn show_messages(&mut self, _: &ShowMessages, _window: &mut Window, cx: &mut Context<Self>) {
        self.open_invoking_command(Some("messages<CR>"), cx);
    }

    fn instantiate_command(
        &mut self,
        _: &InstantiateCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_invoking_command(Some("Argon inst "), cx);
    }

    fn open_cell_command(
        &mut self,
        _: &OpenCellCommand,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_invoking_command(Some("Argon openCell "), cx);
    }

    fn open_invoking_command(&mut self, command: Option<&str>, cx: &mut Context<Self>) {
        // Neovim may be blocked at a hit-enter or error prompt. Focus it
        // before queuing the notification so the user can clear that prompt.
        self.focus_invoker(cx);
        let _ = self
            .state
            .read(cx)
            .lang_server_client
            .open_command_bar(command.map(str::to_owned));
    }

    fn focus_invoker(&mut self, cx: &mut Context<Self>) {
        if !crate::focus::activate_invoker() {
            self.state.update(cx, |state, _cx| {
                state.fatal_error =
                    Some("could not identify the application that invoked Argone".into());
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
        div()
            .id("top")
            .track_focus(&self.canvas.focus_handle(cx))
            .on_action(cx.listener(Self::on_undo))
            .on_action(cx.listener(Self::on_save))
            .on_action(cx.listener(Self::on_redo))
            .on_action(cx.listener(Self::focus_invoking_app))
            .on_action(cx.listener(Self::focus_invoking_app_command_bar))
            .on_action(cx.listener(Self::show_messages))
            .on_action(cx.listener(Self::instantiate_command))
            .on_action(cx.listener(Self::open_cell_command))
            .font_family("Zed Plex Sans")
            .size_full()
            .flex()
            .flex_col()
            .justify_start()
            .border_1()
            .border_color(theme.divider)
            .bg(theme.bg)
            .rounded(px(10.))
            .text_sm()
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
                    .child({
                        let mut d = div()
                            .flex_1()
                            .relative()
                            .overflow_hidden()
                            .child(self.canvas.clone());

                        let displayed_error = {
                            let state = self.state.read(cx);
                            state
                                .connection_error
                                .clone()
                                .map(|error| ("Connection error", error, true, true))
                                .or_else(|| {
                                    state
                                        .fatal_error
                                        .clone()
                                        .map(|error| ("Error", error, false, true))
                                })
                                .or_else(|| {
                                    state.message.clone().map(|message| {
                                        let title = if message.typ == MessageType::ERROR {
                                            "Error"
                                        } else if message.typ == MessageType::WARNING {
                                            "Warning"
                                        } else {
                                            "Message"
                                        };
                                        (title, message.text, false, false)
                                    })
                                })
                        };
                        if let Some((title, error, is_connection_error, editing_disabled)) =
                            displayed_error
                        {
                            d = d.child(
                                div()
                                    .id("error_modal")
                                    .bg(theme.bg)
                                    .border_1()
                                    .border_color(theme.divider)
                                    .rounded_sm()
                                    .absolute()
                                    .p_2()
                                    .child(
                                        div().flex().flex_row().text_color(theme.error).child(
                                            div().flex().flex_col().child(div().flex_1()).child(
                                            svg()
                                                .path("icons/circle-exclamation-solid-full.svg")
                                                .w(px(20.))
                                                .h_auto()
                                                .mr_1()
                                                .text_color(theme.error)).child(div().flex_1())
                                        )
                                        .child(div().child(title))
                                    )
                                    .child(error)
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(theme.subtext)
                                            .child(if is_connection_error {
                                                "Editing is disabled until the connection recovers."
                                            } else if editing_disabled {
                                                "Editing is disabled until this error is fixed."
                                            } else {
                                                "The last operation was not completed."
                                            }),
                                    )
                                    .child(
                                        div()
                                            .id("show_error_details")
                                            .mt_1()
                                            .text_xs()
                                            .text_color(theme.text)
                                            .child("View details in Neovim (Ctrl-Shift-M)")
                                            .on_click(cx.listener(|editor, _, _, cx| {
                                                editor.open_invoking_command(
                                                    Some("messages<CR>"),
                                                    cx,
                                                );
                                            })),
                                    )
                                    .whitespace_normal()
                                    .top_2()
                                    .left_2()
                                    .right_2()
                            );
                        }

                        d
                    })
                    .child(self.layer_sidebar.clone()),
            )
            .child(self.text_input.clone())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {}

#[cfg(test)]
mod tests {
    use std::{path::PathBuf, sync::Arc};

    use analyzer::rpc::CompilationDelta;
    use argonc::{
        ast::Span,
        compile::{CompileOutput, StaticError, StaticErrorCompileOutput, StaticErrorKind},
        incremental::IncrementalCompiler,
    };

    use super::{apply_compilation_delta, compile_error_summary};

    #[test]
    fn compile_error_summary_shows_the_first_error_and_remaining_count() {
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

        assert_eq!(
            compile_error_summary(&output).as_deref(),
            Some("`missing` is not declared in this scope (and 1 more error)")
        );
        assert_eq!(
            compile_error_summary(&CompileOutput::FatalParseErrors).as_deref(),
            Some("fatal parse errors encountered")
        );
    }

    #[test]
    fn compilation_delta_materializes_over_shared_cells_and_checks_its_base() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("lib.ar");
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        let config = argonc::WorkspaceConfig::new(&root).with_tech(Some(tech));
        let target = vec!["top".to_owned()];
        let source = |marker| {
            format!(
                "cell child() {{ let shape = rect(\"met1\", x0=0., y0=0., x1=100., y1=100.); }}\n\
                 cell top() {{ let marker = {marker}; let child = inst(child()); }}\n"
            )
        };
        let mut compiler = IncrementalCompiler::new();
        compiler.set_source_text(root.clone(), source(1));
        let previous = compiler.compile_cell(&config, &target, Vec::new());
        compiler.set_source_text(root, source(2));
        let current = compiler.compile_cell(&config, &target, Vec::new());
        fn output_data(
            output: &CompileOutput,
        ) -> (
            &argonc::compile::CompiledData,
            Vec<argonc::compile::ExecError>,
        ) {
            match output {
                CompileOutput::Valid(data) => (data, Vec::new()),
                CompileOutput::ExecErrors(errors) => (
                    errors.output.as_ref().expect("recoverable compiler output"),
                    errors.errors.clone(),
                ),
                output => panic!("test cell should compile: {output:?}"),
            }
        }
        let (previous_data, _) = output_data(&previous);
        let (current_data, current_errors) = output_data(&current);
        let changed = current_data
            .cells
            .iter()
            .filter(|(id, cell)| {
                previous_data
                    .cells
                    .get(*id)
                    .is_none_or(|old| !Arc::ptr_eq(old, cell))
            })
            .map(|(id, cell)| (*id, cell.clone()))
            .collect();
        let delta = CompilationDelta {
            base_revision: 4,
            top: current_data.top,
            cells: changed,
            removed_cells: Vec::new(),
            errors: current_errors,
        };
        assert!(apply_compilation_delta(Some(3), Some(&previous), delta.clone()).is_none());

        let materialized = apply_compilation_delta(Some(4), Some(&previous), delta).unwrap();
        let materialized = match &materialized {
            CompileOutput::Valid(data) => data,
            CompileOutput::ExecErrors(errors) => errors.output.as_ref().unwrap(),
            _ => unreachable!(),
        };
        assert_eq!(materialized.cells.len(), current_data.cells.len());
        assert!(Arc::ptr_eq(
            &materialized.cells[&current_data.top],
            &current_data.cells[&current_data.top]
        ));
        let child = previous_data
            .cells
            .keys()
            .find(|id| **id != previous_data.top)
            .unwrap();
        assert!(Arc::ptr_eq(
            &materialized.cells[child],
            &previous_data.cells[child]
        ));
    }
}
