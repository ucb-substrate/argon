use std::{
    collections::HashSet,
    hash::{DefaultHasher, Hash, Hasher},
    net::SocketAddr,
};

use canvas::{LayoutCanvas, ShapeFill};
use compiler::compile::{CompiledCell, Rect, SolvedValue};
use gpui::*;
use itertools::Itertools;
use toolbars::{SideBar, TitleBar, ToolBar};

use crate::{project::Project, rpc::SyncGuiToLspClient, theme::THEME};

pub mod canvas;
pub mod toolbars;

pub struct LayerState {
    pub name: String,
    pub color: Rgba,
    pub fill: ShapeFill,
    pub border_color: Rgba,
    pub visible: bool,
    pub z: usize,
}

pub struct EditorState {
    pub solved_cell: CompiledCell,
    pub rects: Vec<canvas::Rect>,
    pub selected_rect: Option<usize>,
    pub layers: Vec<Entity<LayerState>>,
    pub lsp_client: SyncGuiToLspClient,
    pub subscriptions: Vec<Subscription>,
}

pub struct Editor {
    pub state: Entity<EditorState>,
    pub project: Option<Entity<Project>>,
    pub sidebar: Entity<SideBar>,
    pub canvas: Entity<LayoutCanvas>,
}

fn get_rects(
    cx: &mut App,
    solved_cell: &CompiledCell,
    layers: &[Entity<LayerState>],
) -> Vec<canvas::Rect> {
    solved_cell
        .values
        .iter()
        .filter_map(|v| v.get_rect().cloned())
        .flat_map(|rect| {
            let mut rects = Vec::new();
            let layer = layers
                .iter()
                .map(|layer| (layer.clone(), layer.read(cx)))
                .find(|(_, layer)| {
                    if let Some(rect_layer) = &rect.layer {
                        &layer.name == rect_layer
                    } else {
                        false
                    }
                });
            if let Some((id, layer)) = layer {
                rects.push(canvas::Rect {
                    x0: rect.x0 as f32,
                    y0: rect.y0 as f32,
                    x1: rect.x1 as f32,
                    y1: rect.y1 as f32,
                    color: layer.color,
                    fill: layer.fill,
                    border_color: layer.border_color,
                    layer: id.clone(),
                    span: rect.source.clone().map(|info| info.span),
                });
            }
            rects
        })
        .collect()
}

impl Editor {
    pub fn new(cx: &mut Context<Self>, lsp_addr: SocketAddr) -> Self {
        let lsp_client = SyncGuiToLspClient::new(cx.background_executor().clone(), lsp_addr);
        let solved_cell = CompiledCell {
            values: vec![
                SolvedValue::Rect(Rect {
                    layer: Some("Met1".to_string()),
                    x0: 0.,
                    y0: 0.,
                    x1: 100.,
                    y1: 100.,
                    source: None,
                }),
                SolvedValue::Rect(Rect {
                    layer: Some("Via1".to_string()),
                    x0: 10.,
                    y0: 10.,
                    x1: 90.,
                    y1: 90.,
                    source: None,
                }),
                SolvedValue::Rect(Rect {
                    layer: Some("Met2".to_string()),
                    x0: 5.,
                    y0: 5.,
                    x1: 95.,
                    y1: 95.,
                    source: None,
                }),
            ],
        };
        let layers: HashSet<_> = solved_cell
            .values
            .iter()
            .filter_map(|value| value.get_rect()?.layer.clone())
            .collect();
        let layers: Vec<_> = layers
            .into_iter()
            .sorted()
            .enumerate()
            .map(|(z, name)| {
                let mut s = DefaultHasher::new();
                name.hash(&mut s);
                let hash = s.finish() as usize;
                let color = rgb([0xff0000, 0x0ff000, 0x00ff00, 0x000ff0, 0x0000ff][hash % 5]);
                cx.new(|_cx| LayerState {
                    name,
                    color,
                    fill: ShapeFill::Stippling,
                    border_color: color,
                    visible: true,
                    z,
                })
            })
            .collect();
        let rects = get_rects(cx, &solved_cell, &layers);
        let state = cx.new(|cx| {
            let subscriptions = layers
                .iter()
                .map(|layer| {
                    cx.observe(layer, |_, _, cx| {
                        println!("project notified");
                        cx.notify();
                    })
                })
                .collect();
            EditorState {
                solved_cell: solved_cell.clone(),
                rects,
                selected_rect: None,
                layers,
                subscriptions,
                lsp_client,
            }
        });

        let sidebar = cx.new(|cx| SideBar::new(cx, &state));
        let canvas = cx.new(|cx| LayoutCanvas::new(cx, &state));

        Self {
            state,
            project: None,
            sidebar,
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
                    .child(self.sidebar.clone())
                    .child(self.canvas.clone()),
            )
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {}
