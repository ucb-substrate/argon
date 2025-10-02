use std::{
    hash::{DefaultHasher, Hash, Hasher},
    net::SocketAddr,
    path::PathBuf,
};

use canvas::{LayoutCanvas, ShapeFill};
use compiler::compile::{
    ifmatvec, CellId, CompileOutput, Rect, ScopeId, SolvedValue, ValidCompileOutput,
};
use geometry::transform::TransformationMatrix;
use gpui::*;
use indexmap::IndexMap;
use rgb::Rgb;
use toolbars::{HierarchySideBar, LayerSideBar, TitleBar, ToolBar};

use crate::{editor::canvas::RectId, rpc::SyncGuiToLspClient, theme::THEME};

pub mod canvas;
pub mod toolbars;

#[derive(Clone)]
pub struct LayerState {
    pub name: SharedString,
    pub color: Rgba,
    pub fill: ShapeFill,
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
    pub file: PathBuf,
    pub output: ValidCompileOutput,
    pub selected_scope: ScopePath,
    pub selected_rect: Option<RectId>,
    pub state: IndexMap<ScopePath, ScopeState>,
    pub scope_paths: IndexMap<ScopeAddress, ScopePath>,
}

pub struct Layers {
    pub layers: IndexMap<SharedString, LayerState>,
    pub selected_layer: Option<SharedString>,
}

pub struct EditorState {
    pub hierarchy_depth: usize,
    pub solved_cell: Entity<Option<CompileOutputState>>,
    pub layers: Entity<Layers>,
    pub lsp_client: SyncGuiToLspClient,
    pub subscriptions: Vec<Subscription>,
}

pub struct Editor {
    pub state: Entity<EditorState>,
    pub hierarchy_sidebar: Entity<HierarchySideBar>,
    pub layer_sidebar: Entity<LayerSideBar>,
    pub canvas: Entity<LayoutCanvas>,
}

fn bbox_union(b1: Option<Rect<f64>>, b2: Option<Rect<f64>>) -> Option<Rect<f64>> {
    match (b1, b2) {
        (Some(r1), Some(r2)) => Some(Rect {
            layer: None,
            x0: r1.x0.min(r2.x0),
            y0: r1.y0.min(r2.y0),
            x1: r1.x1.max(r2.x1),
            y1: r1.y1.max(r2.y1),
            id: r1.id,
        }),
        (Some(r), None) | (None, Some(r)) => Some(r),
        (None, None) => None,
    }
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
    fn process_scope(
        &self,
        cx: &App,
        solved_cell: &ValidCompileOutput,
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
                        if !state.layers.contains_key(&layer) {
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
                                }
                            }),
                    );
                }
                _ => (),
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
    pub fn update(&mut self, cx: &mut App, file: PathBuf, solved_cell: CompileOutput) {
        let solved_cell = solved_cell.unwrap_valid();
        let root_scope = ScopeAddress {
            scope: solved_cell.cells[&solved_cell.top].root,
            cell: solved_cell.top,
        };
        let root_scope_name = &solved_cell.cells[&root_scope.cell].scopes[&root_scope.scope]
            .name
            .clone();
        let mut state = ProcessScopeState::default();
        for layer in &solved_cell.layers.layers {
            let name = SharedString::from(layer.name.clone());
            state.layers.insert(
                name.clone(),
                LayerState {
                    name,
                    color: rgb_to_rgba(layer.fill_color),
                    fill: ShapeFill::Stippling,
                    border_color: rgb_to_rgba(layer.border_color),
                    visible: true,
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
                file,
                output: solved_cell,
                selected_scope: old_cell
                    .as_ref()
                    .and_then(|cell| {
                        cell.state
                            .contains_key(&cell.selected_scope)
                            .then(|| cell.selected_scope.clone())
                    })
                    .unwrap_or_else(|| vec![root_scope_name.clone()]),
                selected_rect: None,
                state,
                scope_paths,
            });
            cx.notify();
        });
    }
}

impl Editor {
    pub fn new(cx: &mut Context<Self>, lsp_addr: SocketAddr) -> Self {
        let lsp_client = SyncGuiToLspClient::new(cx.to_async(), lsp_addr);
        let solved_cell = cx.new(|_cx| None);
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
                solved_cell,
                layers,
                subscriptions,
                lsp_client: lsp_client.clone(),
            }
        });
        lsp_client.register_server(state.clone());
        let hierarchy_sidebar = cx.new(|cx| HierarchySideBar::new(cx, &state));
        let layer_sidebar = cx.new(|cx| LayerSideBar::new(cx, &state));
        let canvas = cx.new(|cx| LayoutCanvas::new(cx, &state));

        Self {
            state,
            hierarchy_sidebar,
            layer_sidebar,
            canvas,
        }
    }
}

impl Editor {
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
}

impl Render for Editor {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .font_family("Zed Plex Sans")
            .size_full()
            .flex()
            .flex_col()
            .justify_start()
            .border_1()
            .border_color(THEME.divider)
            .rounded(px(10.))
            .text_sm()
            .text_color(rgb(0xffffff))
            .whitespace_nowrap()
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .child(cx.new(|_cx| TitleBar))
            .child(cx.new(|_cx| ToolBar))
            .child(
                div()
                    .flex()
                    .flex_row()
                    .flex_1()
                    .min_h_0()
                    .child(self.hierarchy_sidebar.clone())
                    .child(self.canvas.clone())
                    .child(self.layer_sidebar.clone()),
            )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {}
