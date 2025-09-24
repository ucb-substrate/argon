use std::{
    hash::{DefaultHasher, Hash, Hasher},
    net::SocketAddr,
    path::PathBuf,
};

use canvas::{LayoutCanvas, ShapeFill};
use compiler::compile::{
    CellId, CompileOutput, Rect, ScopeId, SolvedValue, ValidCompileOutput, ifmatvec,
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
    pub visible: bool,
    pub bbox: Option<Rect<f64>>,
    pub parent: Option<ScopeAddress>,
}

#[derive(Clone, Copy, Hash, PartialEq, Eq, Debug)]
pub struct ScopeAddress {
    pub scope: ScopeId,
    pub cell: CellId,
}

#[derive(Clone, Debug)]
pub struct CompileOutputState {
    pub file: PathBuf,
    pub output: ValidCompileOutput,
    pub selected_scope: ScopeAddress,
    pub selected_rect: Option<RectId>,
    pub state: IndexMap<ScopeAddress, ScopeState>,
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
            source: None,
        }),
        (Some(r), None) | (None, Some(r)) => Some(r),
        (None, None) => None,
    }
}

fn process_scope(
    solved_cell: &ValidCompileOutput,
    scope: ScopeAddress,
    z: &mut usize,
    layers: &mut IndexMap<SharedString, LayerState>,
    state: &mut IndexMap<ScopeAddress, ScopeState>,
    parent: Option<ScopeAddress>,
) {
    let scope_info = &solved_cell.cells[&scope.cell].scopes[&scope.scope];
    let mut bbox = None;
    for value in &scope_info.elts {
        match value {
            SolvedValue::Rect(rect) => {
                bbox = bbox_union(bbox, Some(rect.clone()));
                if let Some(layer) = &rect.layer {
                    let layer = SharedString::from(layer);
                    if !layers.contains_key(&layer) {
                        let mut s = DefaultHasher::new();
                        layer.hash(&mut s);
                        let hash = s.finish() as usize;
                        let color =
                            rgb([0xff0000, 0x0ff000, 0x00ff00, 0x000ff0, 0x0000ff][hash % 5]);
                        layers.insert(
                            layer.clone(),
                            LayerState {
                                name: layer,
                                color,
                                fill: ShapeFill::Stippling,
                                border_color: color,
                                visible: true,
                                z: *z,
                            },
                        );
                        *z += 1;
                    }
                }
            }
            SolvedValue::Instance(inst) => {
                let inst_address = ScopeAddress {
                    scope: solved_cell.cells[&inst.cell].root,
                    cell: inst.cell,
                };
                process_scope(solved_cell, inst_address, z, layers, state, Some(scope));
                bbox = bbox_union(
                    bbox,
                    state[&inst_address].bbox.as_ref().map(|rect| {
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
                            source: None,
                        }
                    }),
                );
            }
            _ => {}
        }
    }
    for child in &scope_info.children {
        let scope_address = ScopeAddress {
            scope: *child,
            cell: scope.cell,
        };
        process_scope(solved_cell, scope_address, z, layers, state, Some(scope));
        bbox = bbox_union(bbox, state[&scope_address].bbox.clone());
    }
    state.insert(
        scope,
        ScopeState {
            name: scope_info.name.clone(),
            visible: true,
            bbox,
            parent,
        },
    );
}

fn rgb_to_rgba(color: Rgb<u8>) -> Rgba {
    rgb(((color.r as u32) << 16) | ((color.g as u32) << 8) | color.b as u32)
}

impl EditorState {
    pub fn update(&mut self, cx: &mut impl AppContext, file: PathBuf, solved_cell: CompileOutput) {
        let solved_cell = solved_cell.unwrap_valid();
        let selected_scope = ScopeAddress {
            scope: solved_cell.cells[&solved_cell.top].root,
            cell: solved_cell.top,
        };
        let mut layers = IndexMap::new();
        for (z, layer) in solved_cell.layers.layers.iter().enumerate() {
            let name = SharedString::from(layer.name.clone());
            layers.insert(
                name.clone(),
                LayerState {
                    name,
                    color: rgb_to_rgba(layer.fill_color),
                    fill: ShapeFill::Stippling,
                    border_color: rgb_to_rgba(layer.border_color),
                    visible: true,
                    z,
                },
            );
        }
        let mut z = layers.len();
        let mut state = IndexMap::new();
        process_scope(
            &solved_cell,
            selected_scope,
            &mut z,
            &mut layers,
            &mut state,
            None,
        );
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
                selected_scope,
                selected_rect: None,
                state,
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
