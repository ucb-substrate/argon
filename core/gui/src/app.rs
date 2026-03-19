use std::{
    collections::VecDeque,
    hash::{DefaultHasher, Hash, Hasher},
    net::SocketAddr,
    sync::mpsc::Receiver,
};

use compiler::{
    ast::Span,
    compile::{
        self, CellId, CompileOutput, CompiledData, ExecErrorCompileOutput, ExecErrorKind, ObjectId,
        Rect as CompileRect, ScopeId, SolvedValue, bbox_dim_union, bbox_text_union, bbox_union,
        ifmatvec,
    },
};
use eframe::egui::{
    self, Align, Align2, CentralPanel, Color32, Context, CursorIcon, FontId, Key, Layout,
    Modifiers, PointerButton, Pos2, Rect, RichText, ScrollArea, Sense, SidePanel, Stroke,
    StrokeKind, TopBottomPanel, Ui, Vec2, Window,
};
use geometry::{dir::Dir, transform::TransformationMatrix};
use indexmap::{IndexMap, IndexSet};
use itertools::Itertools;
use lang_server::rpc::{DimensionParams, LangServerAction};
use rgb::Rgb;
use tower_lsp_server::ls_types::MessageType;

use crate::{
    rpc::{GuiEvent, SyncLangServerClient},
    theme::{Theme, dark_theme, light_theme},
};

#[derive(Clone, Copy, PartialEq, Eq)]
enum ShapeFill {
    Stippling,
}

#[derive(Clone)]
struct LayerState {
    color: Color32,
    fill: ShapeFill,
    used: bool,
    border_color: Color32,
    visible: bool,
    z: usize,
}

#[derive(Default)]
struct Layers {
    layers: IndexMap<String, LayerState>,
    selected_layer: Option<String>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq, Debug)]
struct ScopeAddress {
    scope: ScopeId,
    cell: CellId,
}

type ScopePath = Vec<String>;

#[derive(Clone, Debug)]
struct ScopeState {
    name: String,
    address: ScopeAddress,
    visible: bool,
    bbox: Option<CompileRect<f64>>,
    parent: Option<ScopeAddress>,
}

#[derive(Clone, Debug)]
struct CompileOutputState {
    output: CompiledData,
    selected_scope: ScopePath,
    state: IndexMap<ScopePath, ScopeState>,
    scope_paths: IndexMap<ScopeAddress, ScopePath>,
}

#[derive(Default)]
struct ProcessScopeState {
    layers: IndexMap<String, LayerState>,
    state: IndexMap<ScopePath, ScopeState>,
    scope_paths: IndexMap<ScopeAddress, ScopePath>,
}

#[derive(Clone)]
struct WorldRect {
    x0: f32,
    x1: f32,
    y0: f32,
    y1: f32,
    id: Option<Span>,
    object_path: Vec<ObjectId>,
    dashed_edges: [bool; 4],
}

#[derive(Clone)]
struct PaintedRect {
    world: WorldRect,
    screen: Rect,
    layer: Option<LayerState>,
    is_scope: bool,
}

#[derive(Clone)]
struct PaintedDimension {
    span: Option<Span>,
    value: String,
    p: f32,
    n: f32,
    coord: f32,
    pstop: f32,
    nstop: f32,
    horiz: bool,
    hitbox: Rect,
}

#[derive(Clone, PartialEq, Debug)]
struct LayoutEdge<T> {
    dir: Dir,
    coord: T,
    start: T,
    stop: T,
}

#[derive(Clone, Debug)]
enum DimEdgeSelection {
    X0,
    Y0,
    Edge {
        path: String,
        edge_name: String,
        edge: LayoutEdge<f32>,
    },
}

#[derive(Debug, Default, Clone)]
struct DrawRectToolState {
    p0: Option<egui::Pos2>,
}

#[derive(Debug, Default, Clone)]
struct DrawDimToolState {
    edges: Vec<DimEdgeSelection>,
}

#[derive(Debug, Clone)]
struct EditDimToolState {
    dim: Span,
    dim_mode: bool,
}

#[derive(Debug, Default, Clone)]
struct SelectToolState {
    selected_obj: Option<Span>,
}

#[derive(Debug, Clone)]
enum ToolState {
    DrawRect(DrawRectToolState),
    DrawDim(DrawDimToolState),
    EditDim(EditDimToolState),
    Select(SelectToolState),
}

impl ToolState {
    fn selected_span(&self) -> Option<&Span> {
        match self {
            ToolState::Select(state) => state.selected_obj.as_ref(),
            ToolState::EditDim(state) => Some(&state.dim),
            _ => None,
        }
    }
}

impl Default for ToolState {
    fn default() -> Self {
        Self::Select(SelectToolState::default())
    }
}

struct CanvasState {
    offset: Vec2,
    scale: f32,
    initialized: bool,
    viewport: Rect,
    mouse_pos: Option<Pos2>,
}

impl Default for CanvasState {
    fn default() -> Self {
        Self {
            offset: Vec2::ZERO,
            scale: 1.0,
            initialized: false,
            viewport: Rect::NOTHING,
            mouse_pos: None,
        }
    }
}

impl CanvasState {
    fn layout_to_screen(&self, viewport: Rect, point: Pos2) -> Pos2 {
        Pos2::new(
            viewport.left() + self.offset.x + self.scale * point.x,
            viewport.top() + self.offset.y - self.scale * point.y,
        )
    }

    fn screen_to_layout(&self, viewport: Rect, point: Pos2) -> Pos2 {
        let local = point - viewport.min.to_vec2() - self.offset;
        Pos2::new(local.x / self.scale, -local.y / self.scale)
    }
}

#[derive(Clone)]
enum InputMode {
    Command,
    EditDimension { dim: Span, dim_mode: bool },
}

#[derive(Default)]
struct InputState {
    mode: Option<InputMode>,
    text: String,
}

pub struct GuiApp {
    lang_server_client: Option<SyncLangServerClient>,
    server_rx: Option<Receiver<GuiEvent>>,
    hierarchy_depth: usize,
    dark_mode: bool,
    fatal_error: Option<String>,
    solved_cell: Option<CompileOutputState>,
    hide_external_geometry: bool,
    layers: Layers,
    tool: ToolState,
    canvas: CanvasState,
    layer_filter: String,
    hierarchy_filter: String,
    input: InputState,
}

impl GuiApp {
    pub fn new(cc: &eframe::CreationContext<'_>, lang_server_addr: SocketAddr) -> Self {
        cc.egui_ctx.set_pixels_per_point(1.0);

        let mut app = Self {
            lang_server_client: None,
            server_rx: None,
            hierarchy_depth: usize::MAX,
            dark_mode: true,
            fatal_error: None,
            solved_cell: None,
            hide_external_geometry: false,
            layers: Layers::default(),
            tool: ToolState::default(),
            canvas: CanvasState {
                scale: 1.0,
                ..Default::default()
            },
            layer_filter: String::new(),
            hierarchy_filter: String::new(),
            input: InputState::default(),
        };

        match SyncLangServerClient::new(lang_server_addr) {
            Ok((client, rx)) => {
                app.lang_server_client = Some(client);
                app.server_rx = Some(rx);
            }
            Err(err) => {
                app.fatal_error = Some(format!("failed to connect to language server: {err}"));
            }
        }

        app
    }

    fn theme(&self) -> Theme {
        if self.dark_mode {
            dark_theme()
        } else {
            light_theme()
        }
    }

    fn set_visuals(&self, ctx: &Context) {
        let mut visuals = if self.dark_mode {
            egui::Visuals::dark()
        } else {
            egui::Visuals::light()
        };
        visuals.panel_fill = self.theme().bg;
        visuals.extreme_bg_color = self.theme().input_bg;
        visuals.widgets.active.bg_fill = self.theme().selection;
        visuals.widgets.hovered.bg_fill = self.theme().selection;
        visuals.selection.bg_fill = self.theme().selection;
        ctx.set_visuals(visuals);
    }

    fn drain_server_events(&mut self) {
        let mut need_fit = false;
        let mut events = Vec::new();
        if let Some(rx) = &self.server_rx {
            while let Ok(event) = rx.try_recv() {
                events.push(event);
            }
        }
        for event in events {
            match event {
                GuiEvent::OpenCell { output, update } => {
                    self.apply_compile_output(output);
                    if update {
                        if let Some(cell) = &mut self.solved_cell {
                            let paths: IndexSet<_> = cell.state.keys().cloned().collect();
                            if !paths.contains(&cell.selected_scope) {
                                if let Some(first) = cell.state.keys().next() {
                                    cell.selected_scope = first.clone();
                                }
                            }
                        }
                    } else {
                        need_fit = true;
                    }
                }
                GuiEvent::Set { key, value } => match key.as_str() {
                    "hierarchyDepth" => {
                        self.hierarchy_depth = value.parse().unwrap_or(usize::MAX);
                    }
                    "darkMode" => {
                        if let Ok(value) = value.parse() {
                            self.dark_mode = value;
                        }
                    }
                    _ => {}
                },
            }
        }
        if need_fit {
            self.fit_to_screen();
        }
    }

    fn with_client<T>(
        &mut self,
        f: impl FnOnce(&SyncLangServerClient) -> anyhow::Result<T>,
    ) -> Option<T> {
        match self.lang_server_client.as_ref() {
            Some(client) => match f(client) {
                Ok(value) => Some(value),
                Err(err) => {
                    self.fatal_error = Some(format!("{err}"));
                    None
                }
            },
            None => {
                self.fatal_error = Some("language server client is unavailable".to_string());
                None
            }
        }
    }

    fn rgb_to_color32(color: Rgb<u8>) -> Color32 {
        Color32::from_rgb(color.r, color.g, color.b)
    }

    fn process_scope(
        &self,
        solved_cell: &CompiledData,
        scope: ScopeAddress,
        state: &mut ProcessScopeState,
        parent: Option<ScopeAddress>,
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
                        let layer_name = layer.to_string();
                        if let Some(info) = state.layers.get_mut(&layer_name) {
                            info.used = true;
                        } else {
                            let mut hasher = DefaultHasher::new();
                            layer_name.hash(&mut hasher);
                            let hash = hasher.finish() as usize;
                            let color = [
                                Color32::from_rgb(0xff, 0x00, 0x00),
                                Color32::from_rgb(0x0f, 0xf0, 0x00),
                                Color32::from_rgb(0x00, 0xff, 0x00),
                                Color32::from_rgb(0x00, 0x0f, 0xf0),
                                Color32::from_rgb(0x00, 0x00, 0xff),
                            ][hash % 5];
                            state.layers.insert(
                                layer_name.clone(),
                                LayerState {
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
                }
                SolvedValue::Instance(inst) => {
                    let inst_address = ScopeAddress {
                        scope: solved_cell.cells[&inst.cell].root,
                        cell: inst.cell,
                    };
                    self.process_scope(solved_cell, inst_address, state, Some(scope));
                    bbox = bbox_union(
                        bbox,
                        state.state[&state.scope_paths[&inst_address]]
                            .bbox
                            .as_ref()
                            .map(|rect| {
                                let mut inst_mat = TransformationMatrix::identity();
                                if inst.reflect {
                                    inst_mat = inst_mat.reflect_vert();
                                }
                                inst_mat = inst_mat.rotate(inst.angle);
                                let p0 = ifmatvec(inst_mat, (rect.x0, rect.y0));
                                let p1 = ifmatvec(inst_mat, (rect.x1, rect.y1));
                                CompileRect {
                                    layer: None,
                                    x0: p0.0.min(p1.0) + inst.x,
                                    y0: p0.1.min(p1.1) + inst.y,
                                    x1: p0.0.max(p1.0) + inst.x,
                                    y1: p0.1.max(p1.1) + inst.y,
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
                SolvedValue::Text(text) => {
                    bbox = bbox_text_union(bbox, text);
                }
            }
        }

        for child in &scope_info.children {
            let scope_address = ScopeAddress {
                scope: *child,
                cell: scope.cell,
            };
            self.process_scope(solved_cell, scope_address, state, Some(scope));
            bbox = bbox_union(
                bbox,
                state.state[&state.scope_paths[&scope_address]].bbox.clone(),
            );
        }

        let visible = self
            .solved_cell
            .as_ref()
            .and_then(|cell| cell.state.get(&scope_path).map(|scope| scope.visible))
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

    fn apply_compile_output(&mut self, output: CompileOutput) {
        let solved_cell = match output {
            CompileOutput::Valid(data) => data,
            CompileOutput::ExecErrors(ExecErrorCompileOutput {
                output: Some(data),
                errors,
            }) => {
                if errors
                    .iter()
                    .any(|error| matches!(error.kind, ExecErrorKind::InvalidCell))
                {
                    let _ = self.with_client(|client| {
                        client.show_message(MessageType::ERROR, "Open cell is invalid")
                    });
                    self.fatal_error = Some("open cell is invalid".to_string());
                    return;
                }
                data
            }
            _ => {
                self.fatal_error = Some("static compile errors encountered".to_string());
                return;
            }
        };

        let root_scope = ScopeAddress {
            scope: solved_cell.cells[&solved_cell.top].root,
            cell: solved_cell.top,
        };
        let root_scope_name = solved_cell.cells[&root_scope.cell].scopes[&root_scope.scope]
            .name
            .clone();

        let mut processed = ProcessScopeState::default();
        let old_layers = &self.layers.layers;
        for layer in &solved_cell.layers.layers {
            let name = layer.name.clone();
            let visible = old_layers
                .get(&name)
                .map(|layer| layer.visible)
                .unwrap_or(true);
            processed.layers.insert(
                name.clone(),
                LayerState {
                    color: Self::rgb_to_color32(layer.fill_color),
                    fill: ShapeFill::Stippling,
                    border_color: Self::rgb_to_color32(layer.border_color),
                    visible,
                    used: false,
                    z: processed.layers.len(),
                },
            );
        }

        self.process_scope(&solved_cell, root_scope, &mut processed, None);

        self.layers.layers = processed.layers;
        if self
            .layers
            .selected_layer
            .as_ref()
            .map(|layer| !self.layers.layers.contains_key(layer))
            .unwrap_or(true)
        {
            self.layers.selected_layer = None;
        }

        let selected_scope = self
            .solved_cell
            .as_ref()
            .and_then(|cell| {
                processed
                    .state
                    .contains_key(&cell.selected_scope)
                    .then(|| cell.selected_scope.clone())
            })
            .unwrap_or_else(|| vec![root_scope_name]);

        self.solved_cell = Some(CompileOutputState {
            output: solved_cell,
            selected_scope,
            state: processed.state,
            scope_paths: processed.scope_paths,
        });
        self.fatal_error = None;
    }

    fn fit_to_screen(&mut self) {
        let Some(cell) = &self.solved_cell else {
            return;
        };
        let Some(target) = cell.state[&cell.selected_scope].bbox.as_ref().or_else(|| {
            let scope_address = cell.state[&cell.selected_scope].address;
            cell.state[&cell.scope_paths[&ScopeAddress {
                cell: scope_address.cell,
                scope: cell.output.cells[&scope_address.cell].root,
            }]]
                .bbox
                .as_ref()
        }) else {
            self.canvas.offset = Vec2::new(0.0, self.canvas.viewport.height());
            return;
        };

        let width = (target.x1 - target.x0) as f32;
        let height = (target.y1 - target.y0) as f32;
        if width <= 0.0 || height <= 0.0 || self.canvas.viewport.width() <= 0.0 {
            return;
        }
        let scalex = self.canvas.viewport.width() / width;
        let scaley = self.canvas.viewport.height() / height;
        self.canvas.scale = 0.9 * scalex.min(scaley);
        self.canvas.offset = Vec2::new(
            (-(target.x0 + target.x1) as f32 * self.canvas.scale + self.canvas.viewport.width())
                / 2.0,
            (((target.y1 + target.y0) as f32 * self.canvas.scale) + self.canvas.viewport.height())
                / 2.0,
        );
    }

    fn handle_shortcuts(&mut self, ctx: &Context) {
        if ctx.wants_keyboard_input() {
            return;
        }
        let command = |modifiers: Modifiers, key: Key| {
            ctx.input_mut(|input| input.consume_key(modifiers, key))
        };

        if command(Modifiers::NONE, Key::R) {
            self.tool = ToolState::DrawRect(DrawRectToolState::default());
        }
        if command(Modifiers::NONE, Key::S) {
            self.tool = ToolState::Select(SelectToolState::default());
        }
        if command(Modifiers::NONE, Key::D) {
            self.tool = ToolState::DrawDim(DrawDimToolState::default());
        }
        if command(Modifiers::NONE, Key::F) {
            self.fit_to_screen();
        }
        if command(Modifiers::NONE, Key::Q) {
            if let ToolState::Select(SelectToolState {
                selected_obj: Some(span),
            }) = &self.tool
            {
                self.open_dimension_editor(span.clone(), String::new(), false);
            }
        }
        if command(Modifiers::NONE, Key::U) {
            let _ = self.with_client(|client| client.dispatch_action(LangServerAction::Undo));
        }
        if command(Modifiers::CTRL, Key::R) {
            let _ = self.with_client(|client| client.dispatch_action(LangServerAction::Redo));
        }
        if command(Modifiers::NONE, Key::Num0) {
            self.hierarchy_depth = 0;
        }
        if command(Modifiers::NONE, Key::Num1) {
            self.hierarchy_depth = 1;
        }
        if command(Modifiers::SHIFT, Key::Num8) {
            self.hierarchy_depth = usize::MAX;
        }
        if command(Modifiers::NONE, Key::Escape) {
            self.cancel();
        }
        if ctx.input(|input| {
            input
                .events
                .iter()
                .any(|event| matches!(event, egui::Event::Text(text) if text == ":"))
        }) {
            self.input.mode = Some(InputMode::Command);
            if self.input.text.is_empty() {
                self.input.text = ":".to_string();
            }
        }
    }

    fn cancel(&mut self) {
        match &mut self.tool {
            ToolState::DrawRect(state) if state.p0.is_some() => {
                state.p0 = None;
            }
            ToolState::DrawDim(state) if !state.edges.is_empty() => {
                state.edges.clear();
            }
            ToolState::Select(state) => {
                state.selected_obj = None;
            }
            ToolState::EditDim(_) => {
                self.tool = ToolState::default();
            }
            _ => {
                self.tool = ToolState::default();
            }
        }
        self.input.mode = None;
    }

    fn open_dimension_editor(&mut self, dim: Span, original_value: String, dim_mode: bool) {
        self.tool = ToolState::EditDim(EditDimToolState {
            dim: dim.clone(),
            dim_mode,
        });
        self.input.mode = Some(InputMode::EditDimension { dim, dim_mode });
        self.input.text = original_value;
    }

    fn submit_input(&mut self) {
        let Some(mode) = self.input.mode.take() else {
            return;
        };
        match mode {
            InputMode::Command => {
                let text = self.input.text.clone();
                if let Some((command, rest)) = text.split_once(' ') {
                    if command.trim_start_matches(':') == "openCell" {
                        let _ = self.with_client(|client| client.open_cell(rest.to_string()));
                    }
                }
            }
            InputMode::EditDimension { dim, dim_mode } => {
                let value = self.input.text.clone();
                match self.with_client(|client| client.edit_dimension(dim.clone(), value)) {
                    Some(Some(_)) => {
                        self.tool = if dim_mode {
                            ToolState::DrawDim(DrawDimToolState::default())
                        } else {
                            ToolState::default()
                        };
                    }
                    Some(None) => {
                        self.fatal_error = Some("inconsistent editor and GUI state".to_string());
                    }
                    None => {}
                }
            }
        }
        self.input.text.clear();
    }

    fn scene(&self) -> (Vec<PaintedRect>, Vec<PaintedDimension>) {
        let Some(solved_cell) = &self.solved_cell else {
            return (Vec::new(), Vec::new());
        };

        let selected_scope_address = solved_cell.state[&solved_cell.selected_scope].address;
        let mut queue = VecDeque::from_iter([(
            ScopeAddress {
                cell: selected_scope_address.cell,
                scope: if self.hide_external_geometry {
                    selected_scope_address.scope
                } else {
                    solved_cell.output.cells[&selected_scope_address.cell].root
                },
            },
            TransformationMatrix::identity(),
            (0.0, 0.0),
            0usize,
            true,
            Vec::<ObjectId>::new(),
        )]);

        let mut rects = Vec::new();
        let mut dims = solved_cell.output.cells[&selected_scope_address.cell]
            .objects
            .values()
            .filter_map(|obj| obj.get_dimension().cloned())
            .collect_vec();
        let mut scope_rects = Vec::new();

        while let Some((current, mat, ofs, depth, mut show, path)) = queue.pop_front() {
            let cell_info = &solved_cell.output.cells[&current.cell];
            let scope_info = &cell_info.scopes[&current.scope];
            let scope_state = &solved_cell.state[&solved_cell.scope_paths[&current]];

            if depth >= self.hierarchy_depth || !scope_state.visible {
                if let Some(bbox) = &scope_state.bbox {
                    let p0 = ifmatvec(mat, (bbox.x0, bbox.y0));
                    let p1 = ifmatvec(mat, (bbox.x1, bbox.y1));
                    scope_rects.push(WorldRect {
                        x0: (p0.0.min(p1.0) + ofs.0) as f32,
                        y0: (p0.1.min(p1.1) + ofs.1) as f32,
                        x1: (p0.0.max(p1.0) + ofs.0) as f32,
                        y1: (p0.1.max(p1.1) + ofs.1) as f32,
                        id: Some(scope_info.span.clone()),
                        object_path: Vec::new(),
                        dashed_edges: [false; 4],
                    });
                }
                show = false;
            }

            for (obj, _) in &scope_info.emit {
                let mut object_path = path.clone();
                object_path.push(*obj);
                match &cell_info.objects[obj] {
                    SolvedValue::Rect(rect) => {
                        let p0 = ifmatvec(mat, (rect.x0.0, rect.y0.0));
                        let p1 = ifmatvec(mat, (rect.x1.0, rect.y1.0));
                        if let Some(layer_name) = &rect.layer {
                            if let Some(layer) = self.layers.layers.get(layer_name.as_str()) {
                                if show && layer.visible && !rect.construction {
                                    rects.push((
                                        WorldRect {
                                            x0: (p0.0.min(p1.0) + ofs.0) as f32,
                                            y0: (p0.1.min(p1.1) + ofs.1) as f32,
                                            x1: (p0.0.max(p1.0) + ofs.0) as f32,
                                            y1: (p0.1.max(p1.1) + ofs.1) as f32,
                                            id: rect.span.clone(),
                                            object_path,
                                            dashed_edges: [
                                                rect.y0.1.coeffs.iter().any(|(_, var)| {
                                                    cell_info.unsolved_vars.contains(var)
                                                }),
                                                rect.x1.1.coeffs.iter().any(|(_, var)| {
                                                    cell_info.unsolved_vars.contains(var)
                                                }),
                                                rect.y1.1.coeffs.iter().any(|(_, var)| {
                                                    cell_info.unsolved_vars.contains(var)
                                                }),
                                                rect.x0.1.coeffs.iter().any(|(_, var)| {
                                                    cell_info.unsolved_vars.contains(var)
                                                }),
                                            ],
                                        },
                                        layer.clone(),
                                    ));
                                }
                            }
                        }
                    }
                    SolvedValue::Instance(inst) => {
                        let mut inst_mat = TransformationMatrix::identity();
                        if inst.reflect {
                            inst_mat = inst_mat.reflect_vert();
                        }
                        inst_mat = inst_mat.rotate(inst.angle);
                        let inst_ofs = ifmatvec(mat, (inst.x, inst.y));
                        let inst_address = ScopeAddress {
                            scope: solved_cell.output.cells[&inst.cell].root,
                            cell: inst.cell,
                        };
                        queue.push_back((
                            inst_address,
                            mat * inst_mat,
                            (inst_ofs.0 + ofs.0, inst_ofs.1 + ofs.1),
                            depth + 1,
                            show,
                            object_path,
                        ));
                    }
                    SolvedValue::Dimension(dim) => dims.push(dim.clone()),
                    SolvedValue::Text(_) => {}
                }
            }

            for child in &scope_info.children {
                queue.push_back((
                    ScopeAddress {
                        scope: *child,
                        cell: current.cell,
                    },
                    mat,
                    ofs,
                    depth + 1,
                    show,
                    path.clone(),
                ));
            }
        }

        let selected = self.tool.selected_span().cloned();
        let mut painted_rects = rects
            .into_iter()
            .map(|(world, layer)| PaintedRect {
                screen: self.world_rect_to_screen(&world),
                world,
                layer: Some(layer),
                is_scope: false,
            })
            .collect_vec();
        painted_rects.sort_by_key(|rect| {
            rect.layer
                .as_ref()
                .map(|layer| layer.z)
                .unwrap_or(usize::MAX)
        });
        painted_rects.extend(scope_rects.into_iter().map(|world| PaintedRect {
            screen: self.world_rect_to_screen(&world),
            world,
            layer: None,
            is_scope: true,
        }));

        let dimensions = dims
            .into_iter()
            .map(|dim| {
                let value = format!("{:.3}", dim.value);
                let center = if dim.horiz {
                    Pos2::new(((dim.p + dim.n) / 2.0) as f32, dim.coord as f32)
                } else {
                    Pos2::new(dim.coord as f32, ((dim.p + dim.n) / 2.0) as f32)
                };
                let screen_center = self.canvas.layout_to_screen(self.canvas.viewport, center);
                let text_width = value.len() as f32 * 8.0;
                let hitbox = Rect::from_center_size(screen_center, Vec2::new(text_width, 20.0));
                PaintedDimension {
                    span: dim.span.clone(),
                    value,
                    p: dim.p as f32,
                    n: dim.n as f32,
                    coord: dim.coord as f32,
                    pstop: dim.pstop as f32,
                    nstop: dim.nstop as f32,
                    horiz: dim.horiz,
                    hitbox,
                }
            })
            .collect_vec();

        if let Some(selected) = selected {
            for rect in &mut painted_rects {
                if rect.world.id.as_ref() == Some(&selected) {
                    rect.screen = rect.screen.expand(2.0);
                }
            }
        }

        (painted_rects, dimensions)
    }

    fn world_rect_to_screen(&self, rect: &WorldRect) -> Rect {
        let min = self
            .canvas
            .layout_to_screen(self.canvas.viewport, Pos2::new(rect.x0, rect.y1));
        let max = self
            .canvas
            .layout_to_screen(self.canvas.viewport, Pos2::new(rect.x1, rect.y0));
        Rect::from_min_max(min, max)
    }

    fn draw_dashed_line(&self, painter: &egui::Painter, a: Pos2, b: Pos2, stroke: Stroke) {
        let delta = b - a;
        let length = delta.length();
        if length <= 0.0 {
            return;
        }
        let dir = delta / length;
        let dash = 6.0;
        let gap = 4.0;
        let mut t = 0.0;
        while t < length {
            let start = a + dir * t;
            let end = a + dir * (t + dash).min(length);
            painter.line_segment([start, end], stroke);
            t += dash + gap;
        }
    }

    fn draw_rect_outline(&self, painter: &egui::Painter, rect: &PaintedRect, selected: bool) {
        let stroke = if selected {
            Stroke::new(3.0, Color32::YELLOW)
        } else if rect.is_scope {
            Stroke::new(2.0, self.theme().text)
        } else if let Some(layer) = &rect.layer {
            Stroke::new(2.0, layer.border_color)
        } else {
            Stroke::new(2.0, self.theme().text)
        };
        if rect.is_scope {
            painter.rect_stroke(rect.screen, 0.0, stroke, StrokeKind::Middle);
            return;
        }
        if let Some(layer) = &rect.layer {
            let fill = match layer.fill {
                ShapeFill::Stippling => layer.color.gamma_multiply(0.3),
            };
            painter.rect_filled(rect.screen, 0.0, fill);
        }
        let [bottom_dash, right_dash, top_dash, left_dash] = rect.world.dashed_edges;
        let left_top = rect.screen.left_top();
        let right_top = rect.screen.right_top();
        let left_bottom = rect.screen.left_bottom();
        let right_bottom = rect.screen.right_bottom();
        if top_dash {
            self.draw_dashed_line(painter, left_top, right_top, stroke);
        } else {
            painter.line_segment([left_top, right_top], stroke);
        }
        if right_dash {
            self.draw_dashed_line(painter, right_top, right_bottom, stroke);
        } else {
            painter.line_segment([right_top, right_bottom], stroke);
        }
        if bottom_dash {
            self.draw_dashed_line(painter, left_bottom, right_bottom, stroke);
        } else {
            painter.line_segment([left_bottom, right_bottom], stroke);
        }
        if left_dash {
            self.draw_dashed_line(painter, left_top, left_bottom, stroke);
        } else {
            painter.line_segment([left_top, left_bottom], stroke);
        }
    }

    fn draw_dimension(&self, painter: &egui::Painter, dim: &PaintedDimension, selected: bool) {
        let color = if selected {
            Color32::YELLOW
        } else {
            self.theme().text
        };
        let stroke = Stroke::new(2.0, color);
        let offset = 5.0 / self.canvas.scale.max(0.01);

        let (a0, a1) = if dim.horiz {
            (
                (
                    Pos2::new(dim.p, dim.pstop),
                    Pos2::new(
                        dim.p,
                        dim.coord
                            + if dim.coord > dim.pstop {
                                offset
                            } else {
                                -offset
                            },
                    ),
                ),
                (
                    Pos2::new(dim.n, dim.nstop),
                    Pos2::new(
                        dim.n,
                        dim.coord
                            + if dim.coord > dim.nstop {
                                offset
                            } else {
                                -offset
                            },
                    ),
                ),
            )
        } else {
            (
                (
                    Pos2::new(dim.pstop, dim.p),
                    Pos2::new(
                        dim.coord
                            + if dim.coord > dim.pstop {
                                offset
                            } else {
                                -offset
                            },
                        dim.p,
                    ),
                ),
                (
                    Pos2::new(dim.nstop, dim.n),
                    Pos2::new(
                        dim.coord
                            + if dim.coord > dim.nstop {
                                offset
                            } else {
                                -offset
                            },
                        dim.n,
                    ),
                ),
            )
        };

        let line = if dim.horiz {
            (Pos2::new(dim.p, dim.coord), Pos2::new(dim.n, dim.coord))
        } else {
            (Pos2::new(dim.coord, dim.p), Pos2::new(dim.coord, dim.n))
        };

        for (start, end) in [a0, a1, line] {
            let start = self.canvas.layout_to_screen(self.canvas.viewport, start);
            let end = self.canvas.layout_to_screen(self.canvas.viewport, end);
            painter.line_segment([start, end], stroke);
        }

        painter.text(
            dim.hitbox.center(),
            Align2::CENTER_CENTER,
            &dim.value,
            FontId::proportional(14.0),
            color,
        );
    }

    fn hovered_dim_edge(&self, rects: &[PaintedRect], mouse: Pos2) -> Option<DimEdgeSelection> {
        let Some(cell) = &self.solved_cell else {
            return None;
        };
        if let ToolState::DrawDim(state) = &self.tool {
            let axis_dir = state.edges.first().map(|edge| match edge {
                DimEdgeSelection::X0 => Dir::Vert,
                DimEdgeSelection::Y0 => Dir::Horiz,
                DimEdgeSelection::Edge { edge, .. } => edge.dir,
            });

            let y_axis_x = self
                .canvas
                .layout_to_screen(self.canvas.viewport, Pos2::new(0.0, 0.0))
                .x;
            let x_axis_y = self
                .canvas
                .layout_to_screen(self.canvas.viewport, Pos2::new(0.0, 0.0))
                .y;
            if Rect::from_center_size(
                Pos2::new(y_axis_x, mouse.y),
                Vec2::new(10.0, self.canvas.viewport.height()),
            )
            .contains(mouse)
            {
                return Some(DimEdgeSelection::X0);
            }
            if Rect::from_center_size(
                Pos2::new(mouse.x, x_axis_y),
                Vec2::new(self.canvas.viewport.width(), 10.0),
            )
            .contains(mouse)
            {
                return Some(DimEdgeSelection::Y0);
            }

            for rect in rects.iter().rev().filter(|rect| !rect.is_scope) {
                let Some(_) = rect.world.id else {
                    continue;
                };
                let edge_defs = [
                    (
                        "y0",
                        LayoutEdge {
                            dir: Dir::Horiz,
                            coord: rect.world.y0,
                            start: rect.world.x0,
                            stop: rect.world.x1,
                        },
                        Rect::from_min_max(
                            Pos2::new(rect.screen.left(), rect.screen.bottom() - 5.0),
                            Pos2::new(rect.screen.right(), rect.screen.bottom() + 5.0),
                        ),
                    ),
                    (
                        "y1",
                        LayoutEdge {
                            dir: Dir::Horiz,
                            coord: rect.world.y1,
                            start: rect.world.x0,
                            stop: rect.world.x1,
                        },
                        Rect::from_min_max(
                            Pos2::new(rect.screen.left(), rect.screen.top() - 5.0),
                            Pos2::new(rect.screen.right(), rect.screen.top() + 5.0),
                        ),
                    ),
                    (
                        "x0",
                        LayoutEdge {
                            dir: Dir::Vert,
                            coord: rect.world.x0,
                            start: rect.world.y0,
                            stop: rect.world.y1,
                        },
                        Rect::from_min_max(
                            Pos2::new(rect.screen.left() - 5.0, rect.screen.top()),
                            Pos2::new(rect.screen.left() + 5.0, rect.screen.bottom()),
                        ),
                    ),
                    (
                        "x1",
                        LayoutEdge {
                            dir: Dir::Vert,
                            coord: rect.world.x1,
                            start: rect.world.y0,
                            stop: rect.world.y1,
                        },
                        Rect::from_min_max(
                            Pos2::new(rect.screen.right() - 5.0, rect.screen.top()),
                            Pos2::new(rect.screen.right() + 5.0, rect.screen.bottom()),
                        ),
                    ),
                ];
                for (edge_name, edge, hitbox) in edge_defs {
                    if hitbox.contains(mouse) {
                        if axis_dir.is_none_or(|dir| dir == edge.dir) {
                            let selected_scope = cell.state[&cell.selected_scope].address;
                            let (reachable, path) =
                                find_obj_path(&rect.world.object_path, cell, selected_scope);
                            if reachable {
                                return Some(DimEdgeSelection::Edge {
                                    path: path.join("."),
                                    edge_name: edge_name.to_string(),
                                    edge,
                                });
                            }
                        }
                    }
                }
            }
        }
        None
    }

    fn handle_canvas_click(
        &mut self,
        mouse_screen: Pos2,
        rects: &[PaintedRect],
        dims: &[PaintedDimension],
    ) {
        let mouse_layout = self
            .canvas
            .screen_to_layout(self.canvas.viewport, mouse_screen);
        let mut tool = std::mem::take(&mut self.tool);
        match &mut tool {
            ToolState::DrawRect(state) => {
                if let Some(p0) = state.p0.take() {
                    if let Some(cell) = &self.solved_cell {
                        if let Some(layer_name) = self.layers.selected_layer.clone() {
                            let scope_address = cell.state[&cell.selected_scope].address;
                            let reachable_objs = cell
                                .output
                                .reachable_objs(scope_address.cell, scope_address.scope);
                            let names: IndexSet<_> = reachable_objs.values().collect();
                            let rect_name = (0..)
                                .map(|i| format!("rect{i}"))
                                .find(|name| !names.contains(name))
                                .unwrap();
                            let x0 = p0.x.min(mouse_layout.x) as f64;
                            let y0 = p0.y.min(mouse_layout.y) as f64;
                            let x1 = p0.x.max(mouse_layout.x) as f64;
                            let y1 = p0.y.max(mouse_layout.y) as f64;
                            let scope_span = cell.output.cells[&scope_address.cell].scopes
                                [&scope_address.scope]
                                .span
                                .clone();
                            match self.with_client(|client| {
                                client.draw_rect(
                                    scope_span,
                                    rect_name,
                                    compile::BasicRect {
                                        layer: Some(layer_name),
                                        x0,
                                        y0,
                                        x1,
                                        y1,
                                        construction: false,
                                    },
                                )
                            }) {
                                Some(Some(_)) => {}
                                Some(None) => {
                                    self.fatal_error =
                                        Some("inconsistent editor and GUI state".to_string());
                                }
                                None => {}
                            }
                        } else {
                            let _ = self.with_client(|client| {
                                client
                                    .show_message(MessageType::ERROR, "No layer has been selected.")
                            });
                        }
                    } else {
                        self.fatal_error = Some("no cell to edit".to_string());
                    }
                } else {
                    state.p0 = Some(mouse_layout);
                }
            }
            ToolState::DrawDim(state) => {
                if let Some(edge) = self.hovered_dim_edge(rects, mouse_screen) {
                    if state.edges.len() < 2 {
                        state.edges.push(edge);
                    }
                }
                if let Some(cell) = &self.solved_cell {
                    if !state.edges.is_empty() {
                        let selected_scope_addr = cell.state[&cell.selected_scope].address;
                        let scope_span = cell.output.cells[&selected_scope_addr.cell].scopes
                            [&selected_scope_addr.scope]
                            .span
                            .clone();
                        let result = if state.edges.len() == 1 {
                            match &state.edges[0] {
                                DimEdgeSelection::Edge {
                                    path,
                                    edge_name,
                                    edge,
                                } => {
                                    let (left, right, coord, horiz) = match edge.dir {
                                        Dir::Horiz => ("x0", "x1", mouse_layout.y, "true"),
                                        Dir::Vert => ("y0", "y1", mouse_layout.x, "false"),
                                    };
                                    let value = format!("{:?}", edge.stop - edge.start);
                                    self.with_client(|client| {
                                        client.draw_dimension(
                                            scope_span.clone(),
                                            DimensionParams {
                                                p: format!("{path}.{right}"),
                                                n: format!("{path}.{left}"),
                                                value: value.clone(),
                                                coord: if coord > edge.coord {
                                                    format!(
                                                        "{path}.{edge_name} + {}",
                                                        coord - edge.coord
                                                    )
                                                } else {
                                                    format!(
                                                        "{path}.{edge_name} - {}",
                                                        edge.coord - coord
                                                    )
                                                },
                                                pstop: format!("{path}.{edge_name}"),
                                                nstop: format!("{path}.{edge_name}"),
                                                horiz: horiz.to_string(),
                                            },
                                        )
                                    })
                                    .map(|span| span.map(|span| (span, value)))
                                }
                                _ => None,
                            }
                        } else if state.edges.len() == 2 {
                            match (&state.edges[0], &state.edges[1]) {
                                (
                                    DimEdgeSelection::Edge {
                                        path: path0,
                                        edge_name: edge_name0,
                                        edge: edge0,
                                    },
                                    DimEdgeSelection::Edge {
                                        path: path1,
                                        edge_name: edge_name1,
                                        edge: edge1,
                                    },
                                ) => {
                                    let (left_path, left_name, left, right_path, right_name, right) =
                                        if edge0.coord < edge1.coord {
                                            (path0, edge_name0, edge0, path1, edge_name1, edge1)
                                        } else {
                                            (path1, edge_name1, edge1, path0, edge_name0, edge0)
                                        };
                                    let (start, stop, coord, horiz) = match left.dir {
                                        Dir::Vert => ("y0", "y1", mouse_layout.y, "true"),
                                        Dir::Horiz => ("x0", "x1", mouse_layout.x, "false"),
                                    };
                                    let intended_coord =
                                        (right.start + right.stop + left.start + left.stop) / 4.0;
                                    let coord_offset = if coord > intended_coord {
                                        format!("+ {}", coord - intended_coord)
                                    } else {
                                        format!("- {}", intended_coord - coord)
                                    };
                                    let value = format!("{:?}", right.coord - left.coord);
                                    self.with_client(|client| {
                                        client.draw_dimension(
                                            scope_span.clone(),
                                            DimensionParams {
                                                p: format!("{right_path}.{right_name}"),
                                                n: format!("{left_path}.{left_name}"),
                                                value: value.clone(),
                                                coord: format!(
                                                    "({right_path}.{start} + {right_path}.{stop} + {left_path}.{start} + {left_path}.{stop})/4. {coord_offset}"
                                                ),
                                                pstop: format!(
                                                    "({right_path}.{start} + {right_path}.{stop}) / 2."
                                                ),
                                                nstop: format!(
                                                    "({left_path}.{start} + {left_path}.{stop}) / 2."
                                                ),
                                                horiz: horiz.to_string(),
                                            },
                                        )
                                    })
                                    .map(|span| span.map(|span| (span, value)))
                                }
                                (
                                    DimEdgeSelection::X0 | DimEdgeSelection::Y0,
                                    DimEdgeSelection::Edge {
                                        path,
                                        edge_name: _,
                                        edge,
                                    },
                                )
                                | (
                                    DimEdgeSelection::Edge {
                                        path,
                                        edge_name: _,
                                        edge,
                                    },
                                    DimEdgeSelection::X0 | DimEdgeSelection::Y0,
                                ) => {
                                    let (start, stop, coord, horiz) = match edge.dir {
                                        Dir::Vert => ("y0", "y1", mouse_layout.y, "true"),
                                        Dir::Horiz => ("x0", "x1", mouse_layout.x, "false"),
                                    };
                                    let intended_coord = (edge.start + edge.stop) / 2.0;
                                    let coord_offset = if coord > intended_coord {
                                        format!("+ {}", coord - intended_coord)
                                    } else {
                                        format!("- {}", intended_coord - coord)
                                    };
                                    let pnstop = format!("({path}.{start} + {path}.{stop}) / 2.");
                                    let coord_expr = format!("{pnstop} {coord_offset}");
                                    let (p, n, value, pstop, nstop) = if edge.coord < 0.0 {
                                        (
                                            "0.".to_string(),
                                            format!(
                                                "{path}.{}",
                                                if edge.dir == Dir::Vert { "x0" } else { "y0" }
                                            ),
                                            format!("{:?}", -edge.coord),
                                            coord_expr.clone(),
                                            pnstop,
                                        )
                                    } else {
                                        (
                                            format!(
                                                "{path}.{}",
                                                if edge.dir == Dir::Vert { "x0" } else { "y0" }
                                            ),
                                            "0.".to_string(),
                                            format!("{:?}", edge.coord),
                                            pnstop,
                                            coord_expr.clone(),
                                        )
                                    };
                                    self.with_client(|client| {
                                        client.draw_dimension(
                                            scope_span.clone(),
                                            DimensionParams {
                                                p,
                                                n,
                                                value: value.clone(),
                                                coord: coord_expr,
                                                pstop,
                                                nstop,
                                                horiz: horiz.to_string(),
                                            },
                                        )
                                    })
                                    .map(|span| span.map(|span| (span, value)))
                                }
                                _ => None,
                            }
                        } else {
                            None
                        };

                        if let Some(Some((span, value))) = result {
                            state.edges.clear();
                            self.open_dimension_editor(span, value, true);
                            return;
                        } else if let Some(None) = result {
                            self.fatal_error =
                                Some("inconsistent editor and GUI state".to_string());
                        }
                    }
                }
            }
            ToolState::Select(state) => {
                let selected_rect = rects
                    .iter()
                    .rev()
                    .find(|rect| rect.screen.contains(mouse_screen) && rect.world.id.is_some())
                    .and_then(|rect| rect.world.id.clone());
                let selected_dim = dims
                    .iter()
                    .find(|dim| dim.hitbox.contains(mouse_screen))
                    .and_then(|dim| dim.span.clone());
                if let Some(span) = selected_rect.or(selected_dim) {
                    state.selected_obj = Some(span.clone());
                    let _ = self.with_client(|client| client.select_rect(span));
                } else {
                    state.selected_obj = None;
                }
            }
            ToolState::EditDim(_) => {}
        }
        self.tool = tool;
    }

    fn ui_toolbar(&mut self, ui: &mut Ui) {
        let selected_tool = match self.tool {
            ToolState::Select(_) => 0,
            ToolState::DrawRect(_) => 1,
            ToolState::DrawDim(_) | ToolState::EditDim(EditDimToolState { dim_mode: true, .. }) => {
                2
            }
            ToolState::EditDim(_) => 0,
        };
        ui.horizontal(|ui| {
            if ui.button("Undo").clicked() {
                let _ = self.with_client(|client| client.dispatch_action(LangServerAction::Undo));
            }
            if ui.button("Redo").clicked() {
                let _ = self.with_client(|client| client.dispatch_action(LangServerAction::Redo));
            }
            ui.separator();
            if ui.selectable_label(selected_tool == 0, "Select").clicked() {
                self.tool = ToolState::Select(SelectToolState::default());
            }
            if ui.selectable_label(selected_tool == 1, "Rect").clicked() {
                self.tool = ToolState::DrawRect(DrawRectToolState::default());
            }
            if ui.selectable_label(selected_tool == 2, "Dim").clicked() {
                self.tool = ToolState::DrawDim(DrawDimToolState::default());
            }
            ui.separator();
            if ui.button("Fit").clicked() {
                self.fit_to_screen();
            }
            if ui.button("0").clicked() {
                self.hierarchy_depth = 0;
            }
            if ui.button("1").clicked() {
                self.hierarchy_depth = 1;
            }
            if ui.button("*").clicked() {
                self.hierarchy_depth = usize::MAX;
            }
            ui.separator();
            if ui.button("Command").clicked() {
                self.input.mode = Some(InputMode::Command);
                if self.input.text.is_empty() {
                    self.input.text = ":".to_string();
                }
            }
            ui.separator();
            if ui.selectable_label(self.dark_mode, "Dark").clicked() {
                self.dark_mode = true;
            }
            if ui.selectable_label(!self.dark_mode, "Light").clicked() {
                self.dark_mode = false;
            }
        });
    }

    fn ui_layers(&mut self, ui: &mut Ui) {
        let theme = self.theme();
        ui.heading("Layers");
        ui.add(
            egui::TextEdit::singleline(&mut self.layer_filter)
                .hint_text("Filter by name...")
                .background_color(theme.input_bg),
        );
        ui.separator();
        ScrollArea::vertical().show(ui, |ui| {
            for (name, layer) in self
                .layers
                .layers
                .iter_mut()
                .filter(|(name, _)| name.contains(&self.layer_filter))
            {
                ui.horizontal(|ui| {
                    ui.checkbox(&mut layer.visible, "");
                    let swatch = RichText::new("■").color(layer.color);
                    ui.label(swatch);
                    if ui
                        .selectable_label(self.layers.selected_layer.as_ref() == Some(name), name)
                        .clicked()
                    {
                        self.layers.selected_layer = Some(name.clone());
                    }
                });
            }
        });
    }

    fn ui_scope_node(&mut self, ui: &mut Ui, path: &ScopePath, depth: usize) {
        let hierarchy_filter = self.hierarchy_filter.clone();
        let Some(cell) = &self.solved_cell else {
            return;
        };
        let Some(scope_info) = cell.state.get(path) else {
            return;
        };
        let full_name = path.join("::");
        if !full_name.contains(&hierarchy_filter) {
            let has_descendant = cell.state.keys().any(|candidate| {
                candidate.len() > path.len()
                    && candidate[..path.len()] == path[..]
                    && candidate.join("::").contains(&hierarchy_filter)
            });
            if !has_descendant {
                return;
            }
        }
        let scope_name = scope_info.name.clone();
        let scope_visible = scope_info.visible;
        let selected = cell.selected_scope == *path;
        let children = cell
            .state
            .iter()
            .filter(|(candidate, info)| {
                info.parent.is_some()
                    && candidate.len() == path.len() + 1
                    && candidate[..path.len()] == path[..]
            })
            .map(|(candidate, _)| candidate.clone())
            .collect_vec();

        let mut new_visible = scope_visible;
        let mut select_clicked = false;

        ui.horizontal(|ui| {
            ui.add_space(depth as f32 * 12.0);
            ui.checkbox(&mut new_visible, "");
            select_clicked = ui.selectable_label(selected, scope_name).clicked();
        });
        if let Some(cell) = &mut self.solved_cell {
            if let Some(scope) = cell.state.get_mut(path) {
                scope.visible = new_visible;
            }
            if select_clicked {
                cell.selected_scope = path.clone();
            }
        }
        for child in children {
            self.ui_scope_node(ui, &child, depth + 1);
        }
    }

    fn ui_hierarchy(&mut self, ui: &mut Ui) {
        let theme = self.theme();
        ui.heading("Hierarchy");
        ui.add(
            egui::TextEdit::singleline(&mut self.hierarchy_filter)
                .hint_text("Filter by name...")
                .background_color(theme.input_bg),
        );
        ui.separator();
        ScrollArea::vertical().show(ui, |ui| {
            let Some(cell) = &self.solved_cell else {
                return;
            };
            let roots = cell
                .state
                .iter()
                .filter(|(_, state)| state.parent.is_none())
                .map(|(path, _)| path.clone())
                .collect_vec();
            for root in roots {
                self.ui_scope_node(ui, &root, 0);
            }
        });
    }

    fn ui_canvas(&mut self, ui: &mut Ui, ctx: &Context) {
        let available = ui.available_size();
        let (response, painter) = ui.allocate_painter(available, Sense::click_and_drag());
        self.canvas.viewport = response.rect;
        if !self.canvas.initialized && response.rect.width() > 0.0 {
            self.canvas.initialized = true;
            self.fit_to_screen();
        }

        self.canvas.mouse_pos = ctx.pointer_hover_pos();
        if response.hovered() {
            let scroll = ctx.input(|input| input.raw_scroll_delta.y);
            if scroll != 0.0 {
                let pointer = ctx.pointer_hover_pos().unwrap_or(response.rect.center());
                let old = self.canvas.screen_to_layout(self.canvas.viewport, pointer);
                self.canvas.scale = (self.canvas.scale * (1.0 + scroll / 400.0)).clamp(0.01, 100.0);
                let new_screen = self.canvas.layout_to_screen(self.canvas.viewport, old);
                self.canvas.offset += pointer - new_screen;
            }
            if ctx.input(|input| input.pointer.button_down(PointerButton::Secondary)) {
                self.canvas.offset += ctx.input(|input| input.pointer.delta());
            }
        }

        let (rects, dims) = self.scene();
        let selected = self.tool.selected_span().cloned();

        painter.rect_filled(response.rect, 0.0, self.theme().bg);

        let origin = self
            .canvas
            .layout_to_screen(self.canvas.viewport, Pos2::new(0.0, 0.0));
        painter.line_segment(
            [
                Pos2::new(origin.x, response.rect.top()),
                Pos2::new(origin.x, response.rect.bottom()),
            ],
            Stroke::new(2.0, self.theme().axes),
        );
        painter.line_segment(
            [
                Pos2::new(response.rect.left(), origin.y),
                Pos2::new(response.rect.right(), origin.y),
            ],
            Stroke::new(2.0, self.theme().axes),
        );

        for rect in &rects {
            self.draw_rect_outline(&painter, rect, rect.world.id.as_ref() == selected.as_ref());
        }
        for dim in &dims {
            self.draw_dimension(&painter, dim, dim.span.as_ref() == selected.as_ref());
        }

        if let ToolState::DrawRect(DrawRectToolState { p0: Some(p0) }) = &self.tool {
            if let Some(mouse) = self.canvas.mouse_pos {
                let preview = WorldRect {
                    x0: p0
                        .x
                        .min(self.canvas.screen_to_layout(self.canvas.viewport, mouse).x),
                    y0: p0
                        .y
                        .min(self.canvas.screen_to_layout(self.canvas.viewport, mouse).y),
                    x1: p0
                        .x
                        .max(self.canvas.screen_to_layout(self.canvas.viewport, mouse).x),
                    y1: p0
                        .y
                        .max(self.canvas.screen_to_layout(self.canvas.viewport, mouse).y),
                    id: None,
                    object_path: Vec::new(),
                    dashed_edges: [true; 4],
                };
                let preview = PaintedRect {
                    screen: self.world_rect_to_screen(&preview),
                    world: preview,
                    layer: self
                        .layers
                        .selected_layer
                        .as_ref()
                        .and_then(|name| self.layers.layers.get(name))
                        .cloned(),
                    is_scope: false,
                };
                self.draw_rect_outline(&painter, &preview, false);
            }
        }

        if let ToolState::DrawDim(state) = &self.tool {
            if let Some(mouse) = self.canvas.mouse_pos {
                if let Some(edge) = self.hovered_dim_edge(&rects, mouse) {
                    match edge {
                        DimEdgeSelection::Edge { edge, .. } => {
                            let (a, b) = match edge.dir {
                                Dir::Horiz => (
                                    Pos2::new(edge.start, edge.coord),
                                    Pos2::new(edge.stop, edge.coord),
                                ),
                                Dir::Vert => (
                                    Pos2::new(edge.coord, edge.start),
                                    Pos2::new(edge.coord, edge.stop),
                                ),
                            };
                            painter.line_segment(
                                [
                                    self.canvas.layout_to_screen(self.canvas.viewport, a),
                                    self.canvas.layout_to_screen(self.canvas.viewport, b),
                                ],
                                Stroke::new(3.0, Color32::YELLOW),
                            );
                        }
                        DimEdgeSelection::X0 => {
                            painter.line_segment(
                                [
                                    Pos2::new(origin.x, response.rect.top()),
                                    Pos2::new(origin.x, response.rect.bottom()),
                                ],
                                Stroke::new(3.0, Color32::YELLOW),
                            );
                        }
                        DimEdgeSelection::Y0 => {
                            painter.line_segment(
                                [
                                    Pos2::new(response.rect.left(), origin.y),
                                    Pos2::new(response.rect.right(), origin.y),
                                ],
                                Stroke::new(3.0, Color32::YELLOW),
                            );
                        }
                    }
                }
                for edge in &state.edges {
                    match edge {
                        DimEdgeSelection::Edge { edge, .. } => {
                            let (a, b) = match edge.dir {
                                Dir::Horiz => (
                                    Pos2::new(edge.start, edge.coord),
                                    Pos2::new(edge.stop, edge.coord),
                                ),
                                Dir::Vert => (
                                    Pos2::new(edge.coord, edge.start),
                                    Pos2::new(edge.coord, edge.stop),
                                ),
                            };
                            painter.line_segment(
                                [
                                    self.canvas.layout_to_screen(self.canvas.viewport, a),
                                    self.canvas.layout_to_screen(self.canvas.viewport, b),
                                ],
                                Stroke::new(3.0, Color32::YELLOW),
                            );
                        }
                        DimEdgeSelection::X0 => {
                            painter.line_segment(
                                [
                                    Pos2::new(origin.x, response.rect.top()),
                                    Pos2::new(origin.x, response.rect.bottom()),
                                ],
                                Stroke::new(3.0, Color32::YELLOW),
                            );
                        }
                        DimEdgeSelection::Y0 => {
                            painter.line_segment(
                                [
                                    Pos2::new(response.rect.left(), origin.y),
                                    Pos2::new(response.rect.right(), origin.y),
                                ],
                                Stroke::new(3.0, Color32::YELLOW),
                            );
                        }
                    }
                }
            }
        }

        if response.clicked_by(PointerButton::Primary) {
            if let Some(pos) = response.interact_pointer_pos() {
                self.handle_canvas_click(pos, &rects, &dims);
            }
        }

        if response.hovered() {
            if matches!(self.tool, ToolState::DrawRect(_) | ToolState::DrawDim(_)) {
                ui.ctx().set_cursor_icon(CursorIcon::Crosshair);
            }
        }
    }

    fn ui_input_window(&mut self, ctx: &Context) {
        let Some(mode) = self.input.mode.clone() else {
            return;
        };
        let title = match mode {
            InputMode::Command => "Command",
            InputMode::EditDimension { .. } => "Edit Dimension",
        };
        let theme = self.theme();
        Window::new(title)
            .anchor(Align2::CENTER_BOTTOM, [0.0, -24.0])
            .resizable(false)
            .collapsible(false)
            .show(ctx, |ui| {
                ui.visuals_mut().extreme_bg_color = theme.input_bg;
                let hint = match mode {
                    InputMode::Command => "Enter command...",
                    InputMode::EditDimension { .. } => "Dimension value",
                };
                let response = ui.add_sized(
                    [420.0, 24.0],
                    egui::TextEdit::singleline(&mut self.input.text)
                        .background_color(theme.input_bg)
                        .hint_text(hint),
                );
                response.request_focus();
                ui.horizontal(|ui| {
                    if ui.button("OK").clicked() {
                        ui.data_mut(|data| {
                            data.insert_temp(egui::Id::new("argon_input_submit"), true)
                        });
                    }
                    if ui.button("Cancel").clicked() {
                        ui.data_mut(|data| {
                            data.insert_temp(egui::Id::new("argon_input_cancel"), true)
                        });
                    }
                });
            });
        let submit = ctx
            .data(|data| data.get_temp::<bool>(egui::Id::new("argon_input_submit")))
            .unwrap_or(false)
            || ctx.input(|input| input.key_pressed(Key::Enter));
        let cancel = ctx
            .data(|data| data.get_temp::<bool>(egui::Id::new("argon_input_cancel")))
            .unwrap_or(false)
            || ctx.input(|input| input.key_pressed(Key::Escape));
        if submit {
            self.submit_input();
        }
        if cancel {
            if let Some(InputMode::EditDimension { dim_mode, .. }) = &self.input.mode {
                self.tool = if *dim_mode {
                    ToolState::DrawDim(DrawDimToolState::default())
                } else {
                    ToolState::default()
                };
            }
            self.input.mode = None;
            self.input.text.clear();
        }
    }
}

impl eframe::App for GuiApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.drain_server_events();
        self.set_visuals(ctx);
        self.handle_shortcuts(ctx);

        TopBottomPanel::top("title").show(ctx, |ui| {
            ui.visuals_mut().widgets.noninteractive.bg_fill = self.theme().titlebar;
            ui.horizontal(|ui| {
                ui.heading("Argon");
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if let Some(error) = &self.fatal_error {
                        ui.colored_label(self.theme().error, error);
                    }
                });
            });
        });

        TopBottomPanel::top("toolbar").show(ctx, |ui| {
            self.ui_toolbar(ui);
        });

        SidePanel::left("hierarchy")
            .default_width(220.0)
            .resizable(true)
            .show(ctx, |ui| self.ui_hierarchy(ui));

        SidePanel::right("layers")
            .default_width(220.0)
            .resizable(true)
            .show(ctx, |ui| self.ui_layers(ui));

        CentralPanel::default().show(ctx, |ui| self.ui_canvas(ui, ctx));

        self.ui_input_window(ctx);

        if self.fatal_error.is_some()
            || matches!(self.tool, ToolState::DrawRect(_) | ToolState::DrawDim(_))
        {
            ctx.request_repaint();
        }
    }
}

fn find_obj_path(
    path: &[ObjectId],
    cell: &CompileOutputState,
    scope: ScopeAddress,
) -> (bool, Vec<String>) {
    let mut current_scope = scope;
    let mut string_path = Vec::new();
    let mut reachable = true;
    if path.is_empty() {
        return (false, string_path);
    }
    for obj in &path[0..path.len() - 1] {
        let mut reachable_objs = cell
            .output
            .reachable_objs(current_scope.cell, current_scope.scope);
        if let Some(name) = reachable_objs.swap_remove(obj)
            && let Some(inst) = cell.output.cells[&current_scope.cell].objects[obj].get_instance()
        {
            string_path.push(name);
            current_scope = ScopeAddress {
                cell: inst.cell,
                scope: cell.output.cells[&inst.cell].root,
            };
        } else {
            reachable = false;
            break;
        }
    }
    let obj = path.last().unwrap();
    let mut reachable_objs = cell
        .output
        .reachable_objs(current_scope.cell, current_scope.scope);
    if let Some(name) = reachable_objs.swap_remove(obj)
        && cell.output.cells[&current_scope.cell].objects[obj].is_rect()
    {
        string_path.push(name);
    } else {
        reachable = false;
    }
    (reachable, string_path)
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use compiler::{
        compile::{CompileInput, compile},
        parse::parse_workspace_with_std,
    };
    use eframe::egui::{Pos2, Rect, Vec2};

    use super::*;

    const EXAMPLES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
    const BASIC_LYP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples/lyp/basic.lyp");

    fn compile_example(relative_path: &str, cell: &[&str]) -> CompileOutput {
        let input = Path::new(EXAMPLES_DIR).join(relative_path);
        let parsed = parse_workspace_with_std(&input);
        assert!(
            parsed.static_errors().is_empty(),
            "failed to parse {}: {:?}",
            input.display(),
            parsed.static_errors()
        );
        let ast = parsed.ast();
        compile(
            &ast,
            CompileInput {
                cell,
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        )
    }

    fn test_app() -> GuiApp {
        GuiApp {
            lang_server_client: None,
            server_rx: None,
            hierarchy_depth: usize::MAX,
            dark_mode: true,
            fatal_error: None,
            solved_cell: None,
            hide_external_geometry: false,
            layers: Layers::default(),
            tool: ToolState::default(),
            canvas: CanvasState {
                offset: Vec2::ZERO,
                scale: 1.0,
                initialized: true,
                viewport: Rect::from_min_size(Pos2::ZERO, Vec2::new(800.0, 600.0)),
                mouse_pos: None,
            },
            layer_filter: String::new(),
            hierarchy_filter: String::new(),
            input: InputState::default(),
        }
    }

    #[test]
    fn apply_compile_output_loads_layers_and_scope_tree() {
        let mut app = test_app();
        app.apply_compile_output(compile_example("hierarchy/lib.ar", &["top"]));

        let solved = app.solved_cell.as_ref().expect("compiled cell state");
        assert!(app.fatal_error.is_none());
        assert!(solved.state.len() > 1);
        assert!(
            solved
                .state
                .keys()
                .any(|path| path.join("::").contains("bot"))
        );
        assert!(app.layers.layers.contains_key("met1"));
        assert!(app.layers.layers.contains_key("met3"));
        assert_eq!(solved.selected_scope.len(), 1);
    }

    #[test]
    fn fit_to_screen_and_scene_render_dimensions_example() {
        let mut app = test_app();
        app.apply_compile_output(compile_example("dimensions/lib.ar", &["top"]));

        app.fit_to_screen();
        let (rects, dims) = app.scene();

        assert!(app.canvas.scale > 0.0);
        assert!(app.canvas.offset != Vec2::ZERO);
        assert!(
            rects
                .iter()
                .any(|rect| !rect.is_scope && rect.layer.is_some())
        );
        assert!(dims.len() >= 2);
    }

    #[test]
    fn scene_emits_scope_bbox_when_hierarchy_is_collapsed() {
        let mut app = test_app();
        app.apply_compile_output(compile_example("hierarchy/lib.ar", &["top"]));
        app.hierarchy_depth = 0;
        app.fit_to_screen();

        let (rects, _) = app.scene();

        assert!(rects.iter().any(|rect| rect.is_scope));
    }

    #[test]
    fn hovered_dim_edge_finds_rect_edge_path() {
        let mut app = test_app();
        app.apply_compile_output(compile_example("dimensions/lib.ar", &["top"]));
        app.fit_to_screen();
        app.tool = ToolState::DrawDim(DrawDimToolState::default());

        let (rects, _) = app.scene();
        let rect = rects
            .iter()
            .find(|rect| !rect.is_scope)
            .expect("painted rectangle");
        let mouse = Pos2::new(rect.screen.right(), rect.screen.center().y);

        match app.hovered_dim_edge(&rects, mouse) {
            Some(DimEdgeSelection::Edge {
                path,
                edge_name,
                edge,
            }) => {
                assert_eq!(path, "met1");
                assert_eq!(edge_name, "x1");
                assert_eq!(edge.dir, Dir::Vert);
            }
            other => panic!("expected a concrete edge hit, got {other:?}"),
        }
    }

    #[test]
    fn select_tool_click_selects_rect_even_without_rpc_client() {
        let mut app = test_app();
        app.apply_compile_output(compile_example("dimensions/lib.ar", &["top"]));
        app.fit_to_screen();

        let (rects, dims) = app.scene();
        let rect = rects
            .iter()
            .find(|rect| !rect.is_scope)
            .expect("painted rectangle");
        let expected_span = rect.world.id.clone().expect("rect span");
        let mouse = Pos2::new(rect.screen.left() + 8.0, rect.screen.top() + 8.0);

        app.handle_canvas_click(mouse, &rects, &dims);

        match &app.tool {
            ToolState::Select(state) => {
                assert_eq!(state.selected_obj.as_ref(), Some(&expected_span));
            }
            other => panic!("expected select tool, got {other:?}"),
        }
        assert_eq!(
            app.fatal_error.as_deref(),
            Some("language server client is unavailable")
        );
    }

    #[test]
    fn draw_rect_tool_first_click_captures_anchor_point() {
        let mut app = test_app();
        app.apply_compile_output(compile_example("dimensions/lib.ar", &["top"]));
        app.tool = ToolState::DrawRect(DrawRectToolState::default());

        let (rects, dims) = app.scene();
        let mouse = app
            .canvas
            .layout_to_screen(app.canvas.viewport, Pos2::new(10.0, 15.0));

        app.handle_canvas_click(mouse, &rects, &dims);

        match &app.tool {
            ToolState::DrawRect(state) => {
                assert_eq!(state.p0, Some(Pos2::new(10.0, 15.0)));
            }
            other => panic!("expected draw-rect tool, got {other:?}"),
        }
        assert!(app.fatal_error.is_none());
    }
}
