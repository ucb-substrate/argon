use std::{
    hash::{DefaultHasher, Hash, Hasher},
    net::SocketAddr,
};

use canvas::{LayoutCanvas, ShapeFill};
use compiler::compile::{
    CellId, CompileOutput, CompiledData, ExecErrorCompileOutput, ExecErrorKind, Rect, ScopeId,
    SolvedValue, bbox_dim_union, bbox_union, ifmatvec,
};
use geometry::transform::TransformationMatrix;
use gpui::*;
use indexmap::{IndexMap, IndexSet};
use lsp_server::rpc::GuiToLspAction;
use rgb::Rgb;
use toolbars::{HierarchySideBar, LayerSideBar, TitleBar, ToolBar};
use tower_lsp::lsp_types::MessageType;

use crate::{
    actions::{Redo, Undo},
    editor::{canvas::ToolState, input::TextInput},
    rpc::SyncGuiToLspClient,
    theme::{DARK_THEME, LIGHT_THEME, Theme},
};

pub mod canvas;
pub mod input;
pub mod toolbars;

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
    pub output: CompiledData,
    pub selected_scope: ScopePath,
    pub state: IndexMap<ScopePath, ScopeState>,
    pub scope_paths: IndexMap<ScopeAddress, ScopePath>,
}

pub struct Layers {
    pub layers: IndexMap<SharedString, LayerState>,
    pub selected_layer: Option<SharedString>,
}

pub struct EditorState {
    pub hierarchy_depth: usize,
    pub dark_mode: bool,
    pub fatal_error: Option<SharedString>,
    pub solved_cell: Entity<Option<CompileOutputState>>,
    pub hide_external_geometry: bool,
    pub layers: Entity<Layers>,
    pub lsp_client: SyncGuiToLspClient,
    pub subscriptions: Vec<Subscription>,
    pub(crate) tool: Entity<ToolState>,
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

#[derive(Default)]
struct ProcessScopeState {
    layers: IndexMap<SharedString, LayerState>,
    state: IndexMap<ScopePath, ScopeState>,
    scope_paths: IndexMap<ScopeAddress, ScopePath>,
}

impl EditorState {
    fn theme(&self) -> &'static Theme {
        if self.dark_mode {
            &DARK_THEME
        } else {
            &LIGHT_THEME
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
                        let layer = SharedString::from(layer);
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
        let solved_cell = match output {
            CompileOutput::Valid(d) => d,
            CompileOutput::ExecErrors(ExecErrorCompileOutput {
                output: Some(d),
                errors,
            }) => {
                if errors
                    .iter()
                    .any(|e| matches!(e.kind, ExecErrorKind::InvalidCell))
                {
                    self.lsp_client
                        .show_message(MessageType::ERROR, "Open cell is invalid");
                    self.fatal_error = Some(SharedString::from("Open cell is invalid"));
                    return;
                }
                d
            }
            _ => {
                self.fatal_error = Some(SharedString::from("Static compile errors encountered"));
                return;
            }
        };
        let root_scope = ScopeAddress {
            scope: solved_cell.cells[&solved_cell.top].root,
            cell: solved_cell.top,
        };
        let root_scope_name = &solved_cell.cells[&root_scope.cell].scopes[&root_scope.scope]
            .name
            .clone();
        let mut state = ProcessScopeState::default();
        let old_layers = self.layers.read(cx);
        for layer in &solved_cell.layers.layers {
            let name = SharedString::from(layer.name.clone());
            let visible = old_layers
                .layers
                .get(&name)
                .map(|layer| layer.visible)
                .unwrap_or(true);
            state.layers.insert(
                name.clone(),
                LayerState {
                    name,
                    color: rgb_to_rgba(layer.fill_color),
                    fill: ShapeFill::Stippling,
                    border_color: rgb_to_rgba(layer.border_color),
                    visible,
                    used: false,
                    z: state.layers.len(),
                },
            );
        }
        self.process_scope(cx, &solved_cell, root_scope, &mut state, None);
        let ProcessScopeState {
            layers,
            state,
            scope_paths,
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
                output: solved_cell,
                selected_scope: old_cell
                    .as_ref()
                    .and_then(|cell| {
                        state
                            .contains_key(&cell.selected_scope)
                            .then(|| cell.selected_scope.clone())
                    })
                    .unwrap_or_else(|| vec![root_scope_name.clone()]),
                state,
                scope_paths,
            });
            cx.notify();
        });
    }
}

impl Editor {
    pub fn new(cx: &mut Context<Self>, window: &mut Window, lsp_addr: SocketAddr) -> Self {
        let lsp_client = SyncGuiToLspClient::new(cx.to_async(), lsp_addr);
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
                fatal_error: None,
                solved_cell,
                hide_external_geometry: false,
                tool,
                layers,
                subscriptions,
                lsp_client: lsp_client.clone(),
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
            .new(|cx| TextInput::new_command_prompt(cx, text_input_focus_handle, &state, &canvas));
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
        lsp_client.register_server(editor.clone());

        editor
    }

    pub fn open_cell(&self, cx: &mut AsyncApp, output: CompileOutput, update: bool) {
        self.state
            .update(cx, |state, cx| {
                state.update(cx, output);
                cx.notify();
            })
            .unwrap();
        if update {
            let state = self.state.clone();
            self.hierarchy_sidebar
                .update(cx, move |sidebar, cx| {
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
                })
                .unwrap();
        } else {
            self.canvas
                .update(cx, |canvas, cx| {
                    canvas.fit_to_screen(cx);
                    cx.notify();
                })
                .unwrap();
            self.hierarchy_sidebar
                .update(cx, |sidebar, cx| {
                    sidebar.state.update(cx, |state, cx| {
                        state.expanded_scopes.clear();
                        cx.notify();
                    });
                })
                .unwrap();
        }
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

    fn on_undo(&mut self, _: &Undo, _window: &mut Window, cx: &mut Context<Self>) {
        self.state
            .read(cx)
            .lsp_client
            .dispatch_action(GuiToLspAction::Undo);
    }

    fn on_redo(&mut self, _: &Redo, _window: &mut Window, cx: &mut Context<Self>) {
        self.state
            .read(cx)
            .lsp_client
            .dispatch_action(GuiToLspAction::Redo);
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
            .on_action(cx.listener(Self::on_redo))
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
            .child(self.title_bar.clone())
            .child(self.tool_bar.clone())
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .relative()
                    .min_h_0()
                    .child(self.hierarchy_sidebar.clone())
                    .child(div().flex_1().overflow_hidden().child(self.canvas.clone()))
                    .child(self.layer_sidebar.clone())
                    .child(
                        div()
                            .bg(rgb(0xff0000))
                            .absolute()
                            .w_full()
                            .h_full()
                            .top_0()
                            .left_0(),
                    ),
            )
            .child(self.text_input.clone())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {}
