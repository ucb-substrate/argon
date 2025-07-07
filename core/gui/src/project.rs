use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;

use compiler::compile::{compile, CompileInput};
use compiler::parse::parse;
use compiler::solver::{Rect, SolvedCell};
use gpui::*;

use crate::{
    canvas::{test_canvas, LayoutCanvas, ShapeFill},
    text::TextDisplay,
    theme::THEME,
    toolbars::{SideBar, TitleBar, ToolBar},
};

pub struct LayerState {
    pub name: String,
    pub color: Rgba,
    pub fill: ShapeFill,
    pub border_color: Rgba,
    pub visible: bool,
}

pub struct ProjectState {
    pub path: PathBuf,
    pub code: String,
    pub cell: String,
    pub params: HashMap<String, f64>,
    pub solved_cell: SolvedCell,
    pub layers: Vec<Entity<LayerState>>,
    pub subscriptions: Vec<Subscription>,
}

pub struct Project {
    pub state: Entity<ProjectState>,
    pub sidebar: Entity<SideBar>,
    pub canvas: Entity<LayoutCanvas>,
}

impl Project {
    pub fn new(
        cx: &mut Context<Self>,
        path: PathBuf,
        cell: String,
        params: HashMap<String, f64>,
    ) -> Self {
        let code = std::fs::read_to_string(&path).expect("failed to read file");
        let ast = parse(&code).expect("failed to parse Argon");
        let params_ref = params.iter().map(|(k, v)| (k.as_str(), *v)).collect();
        let solved_cell = compile(CompileInput {
            cell: &cell,
            ast: &ast,
            params: params_ref,
        })
        .expect("failed to compiler Argon");
        let layers: HashSet<_> = solved_cell
            .rects
            .iter()
            .map(|Rect { layer, .. }| layer.clone().unwrap().to_string())
            .collect();
        let layers: Vec<_> = layers
            .into_iter()
            .map(|name| {
                let mut s = DefaultHasher::new();
                name.hash(&mut s);
                let hash = s.finish() as usize;
                cx.new(|_cx| LayerState {
                    name,
                    color: rgb([0xff0000, 0x00ff00, 0x0000ff][hash % 3]),
                    fill: ShapeFill::Stippling,
                    border_color: rgb([0xff0000, 0x00ff00, 0x0000ff][hash % 3]),
                    visible: true,
                })
            })
            .collect();
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
            ProjectState {
                path,
                code,
                cell,
                params,
                solved_cell: solved_cell.clone(),
                layers,
                subscriptions,
            }
        });

        let sidebar = cx.new(|cx| SideBar::new(cx, state.clone()));
        let canvas = cx.new(|cx| LayoutCanvas::new(cx, state.clone()));

        Self {
            state,
            sidebar,
            canvas,
        }
    }
}

impl Project {
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

    fn on_mouse_up(&mut self, event: &MouseUpEvent, window: &mut Window, cx: &mut Context<Self>) {
        self.canvas
            .update(cx, |canvas, cx| canvas.on_mouse_up(event, window, cx));
    }
}

impl Render for Project {
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
            .child(cx.new(|_cx| TextDisplay {
                text: "cell via {\n  let x = Rect(0, 0, 100, 100)!\n}\n".to_string(),
                highlight_span: Some(5..8),
            }))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Event {}
