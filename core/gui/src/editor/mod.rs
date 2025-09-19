use std::{
    collections::{HashMap, VecDeque},
    hash::{DefaultHasher, Hash, Hasher},
    net::SocketAddr,
    path::PathBuf,
};

use canvas::{LayoutCanvas, ShapeFill};
use compiler::compile::{ifmatvec, CellId, CompileOutput, SolvedValue};
use geometry::transform::TransformationMatrix;
use gpui::*;
use itertools::Itertools;
use toolbars::{HierarchySideBar, LayerSideBar, TitleBar, ToolBar};

use crate::{rpc::SyncGuiToLspClient, theme::THEME};

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

pub struct ScopeTree {
    root: Option<CellId>,
    state: HashMap<CellId, ScopeState>,
}

#[derive(Clone)]
pub struct ScopeState {
    pub name: String,
    pub visible: bool,
    pub children: Vec<CellId>,
}

pub struct EditorState {
    pub file: Option<PathBuf>,
    pub solved_cell: Option<CompileOutput>,
    pub rects: Vec<canvas::Rect>,
    pub selected_rect: Option<usize>,
    pub layers: Entity<HashMap<SharedString, LayerState>>,
    pub scopes: Entity<ScopeTree>,
    pub lsp_client: SyncGuiToLspClient,
    pub subscriptions: Vec<Subscription>,
}

pub struct Editor {
    pub state: Entity<EditorState>,
    pub hierarchy_sidebar: Entity<HierarchySideBar>,
    pub layer_sidebar: Entity<LayerSideBar>,
    pub canvas: Entity<LayoutCanvas>,
}

impl EditorState {
    pub fn update(&mut self, cx: &mut impl AppContext, file: PathBuf, solved_cell: CompileOutput) {
        self.file = Some(file);
        let mut z = 0;
        let mut queue =
            VecDeque::from_iter([(solved_cell.top, TransformationMatrix::identity(), (0., 0.))]);
        let mut layers = HashMap::new();
        let mut scopes = HashMap::new();
        let mut rects = Vec::new();
        while let Some((cell, mat, ofs)) = queue.pop_front() {
            let mut children = Vec::new();
            for value in &solved_cell.cells[&cell].values {
                match value {
                    SolvedValue::Rect(rect) => {
                        let p0p = ifmatvec(mat, (rect.x0, rect.y0));
                        let p1p = ifmatvec(mat, (rect.x1, rect.y1));
                        if let Some(layer) = &rect.layer {
                            let layer = SharedString::from(layer);
                            rects.push(canvas::Rect {
                                x0: (p0p.0.min(p1p.0) + ofs.0) as f32,
                                y0: (p0p.1.min(p1p.1) + ofs.1) as f32,
                                x1: (p0p.0.max(p1p.0) + ofs.0) as f32,
                                y1: (p0p.1.max(p1p.1) + ofs.1) as f32,
                                layer: layer.clone(),
                                scope: cell,
                                span: rect.source.clone().map(|info| info.span),
                            });
                            if !layers.contains_key(&layer) {
                                let mut s = DefaultHasher::new();
                                layer.hash(&mut s);
                                let hash = s.finish() as usize;
                                let color =
                                    rgb([0xff0000, 0x0ff000, 0x00ff00, 0x000ff0, 0x0000ff]
                                        [hash % 5]);
                                layers.insert(
                                    layer.clone(),
                                    LayerState {
                                        name: layer,
                                        color,
                                        fill: ShapeFill::Stippling,
                                        border_color: color,
                                        visible: true,
                                        z,
                                    },
                                );
                                z += 1;
                            }
                        }
                    }
                    SolvedValue::Instance(inst) => {
                        let mut inst_mat = TransformationMatrix::identity();
                        if inst.reflect {
                            inst_mat = inst_mat.reflect_vert()
                        }
                        inst_mat = inst_mat.rotate(inst.angle);
                        let inst_ofs = ifmatvec(mat, (inst.x, inst.y));

                        queue.push_back((
                            inst.cell,
                            mat * inst_mat,
                            (inst_ofs.0 + ofs.0, inst_ofs.1 + ofs.1),
                        ));
                        children.push(inst.cell);
                    }
                    _ => {}
                }
            }
            scopes.insert(
                cell,
                ScopeState {
                    name: "tmp".into(),
                    visible: true,
                    children: children.into_iter().dedup().collect(),
                },
            );
        }
        self.layers.update(cx, |old_layers, cx| {
            *old_layers = layers;
            cx.notify();
        });
        self.scopes.update(cx, |old_scopes, cx| {
            *old_scopes = ScopeTree {
                root: Some(solved_cell.top),
                state: scopes,
            };
            cx.notify();
        });
        self.rects = rects;
        self.solved_cell = Some(solved_cell);
    }
}

impl Editor {
    pub fn new(cx: &mut Context<Self>, lsp_addr: SocketAddr) -> Self {
        let lsp_client = SyncGuiToLspClient::new(cx.to_async(), lsp_addr);
        let layers = cx.new(|_cx| HashMap::new());
        let scopes = cx.new(|_cx| ScopeTree {
            root: None,
            state: HashMap::new(),
        });
        let state = cx.new(|cx| {
            let subscriptions = vec![cx.observe(&layers, |_, _, cx| cx.notify())];
            EditorState {
                file: None,
                solved_cell: None,
                rects: Vec::new(),
                selected_rect: None,
                layers,
                scopes,
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
