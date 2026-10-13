//! The sidebars of the schematic and symbol views: the schematic tree and
//! net list, the symbol view's port list, and the properties of the
//! selection.

use std::rc::Rc;

use analyzer::rpc::CallArgument;
use argonc::compile::{Element, SymbolItemKind, SymbolText};
use gpui::{
    Context, Entity, Focusable, IntoElement, MouseButton, Render, ScrollHandle, SharedString,
    Subscription, Window, div, prelude::*, px,
};

use super::hit::Hit;
use super::symbol_tools::TextSpec;
use super::{CanvasEvent, SchematicCanvas, SymbolSelection};
use crate::editor::input::TextInput;
use crate::editor::{EditorState, View};

const SIDEBAR_WIDTH: f32 = 220.;

/// The left sidebar: elements and nets in the schematic view, ports in the
/// symbol view.
pub struct SchematicSideBar {
    editor_state: Entity<EditorState>,
    canvas: Entity<SchematicCanvas>,
    scroll_handle: ScrollHandle,
    _subscriptions: Vec<Subscription>,
}

impl SchematicSideBar {
    pub fn new(
        cx: &mut Context<Self>,
        editor_state: &Entity<EditorState>,
        canvas: &Entity<SchematicCanvas>,
    ) -> Self {
        let subscriptions = vec![
            cx.observe(editor_state, |_, _, cx| cx.notify()),
            cx.observe(canvas, |_, _, cx| cx.notify()),
        ];
        Self {
            editor_state: editor_state.clone(),
            canvas: canvas.clone(),
            scroll_handle: ScrollHandle::new(),
            _subscriptions: subscriptions,
        }
    }
}

fn row(
    id: SharedString,
    selected: bool,
    theme: &'static crate::theme::Theme,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .flex()
        .flex_row()
        .gap_2()
        .px_1()
        .min_w_full()
        .cursor_pointer()
        .bg(if selected {
            theme.selection
        } else {
            theme.sidebar
        })
        .hover(|style| style.bg(theme.hover))
}

fn header(text: &'static str) -> gpui::Div {
    div()
        .pt_2()
        .pb_1()
        .font_weight(gpui::FontWeight::BOLD)
        .child(text)
}

impl Render for SchematicSideBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let editor_state = self.editor_state.read(cx);
        let theme = editor_state.theme();
        let view = editor_state.view;
        let canvas = self.canvas.read(cx);
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        match view {
            View::Symbol => {
                rows.push(header("Ports").into_any_element());
                if let Some(scene) = canvas.symbol_scene() {
                    let selected = canvas.symbol_selection().to_vec();
                    let mut names = scene
                        .pins
                        .iter()
                        .map(|pin| (pin.port, pin.name.clone(), true))
                        .collect::<Vec<_>>();
                    for (index, name) in scene.missing.iter().enumerate() {
                        names.push((usize::MAX - index, name.clone(), false));
                    }
                    names.sort_by_key(|(port, _, _)| *port);
                    // Ports without a pin are listed by their position among
                    // the ports, so the pin tool can pick them.
                    let missing = scene.missing_ports.clone();
                    for (port, name, pinned) in names {
                        let canvas = self.canvas.clone();
                        let port = if pinned {
                            port
                        } else {
                            missing[usize::MAX - port]
                        };
                        let is_selected = selected.contains(&SymbolSelection::Pin(port));
                        let row = row(format!("port_{port}").into(), is_selected, theme)
                            .child(div().flex_1().child(name.clone()))
                            .child(
                                div()
                                    .text_color(if pinned { theme.subtext } else { theme.error })
                                    .child(if pinned { "pinned" } else { "missing" }),
                            )
                            .on_click(move |_, _, cx| {
                                canvas.update(cx, |canvas, cx| {
                                    if pinned {
                                        canvas.select_pin_and_zoom(port, cx);
                                    } else {
                                        canvas.pick_port(port, cx);
                                    }
                                });
                            });
                        rows.push(row.into_any_element());
                    }
                    if scene.generated {
                        let canvas = self.canvas.clone();
                        rows.push(
                            div()
                                .pt_2()
                                .text_color(theme.subtext)
                                .child("Generated symbol")
                                .into_any_element(),
                        );
                        rows.push(
                            div()
                                .id("create_symbol")
                                .mt_1()
                                .px_2()
                                .py_1()
                                .border_1()
                                .border_color(theme.divider)
                                .rounded_sm()
                                .cursor_pointer()
                                .hover(|style| style.bg(theme.selection))
                                .child("Create Symbol")
                                .on_click(move |_, _, cx| {
                                    canvas.update(cx, |canvas, cx| canvas.create_symbol(cx));
                                })
                                .into_any_element(),
                        );
                    }
                }
            }
            _ => {
                rows.push(header("Schematic").into_any_element());
                if let Some(scene) = canvas.scene() {
                    let selection = canvas.selection().to_vec();
                    for (index, element) in scene.elements.iter().enumerate() {
                        let canvas = self.canvas.clone();
                        let state = self.editor_state.clone();
                        let is_instance = matches!(element.element, Element::Instance(_));
                        let detail = match element.element {
                            Element::Instance(_) => element.cell_name.clone().unwrap_or_default(),
                            Element::Device(_) => {
                                element.model.clone().unwrap_or_else(|| "device".to_owned())
                            }
                        };
                        let name = element.name.clone();
                        let is_selected = selection.iter().any(|hit| {
                            matches!(hit, Hit::Element(i) | Hit::Pin { element: i, .. } if *i == index)
                        });
                        let mut row = row(format!("element_{index}").into(), is_selected, theme)
                            .child(div().child(if is_instance { "▸" } else { "•" }))
                            .child(div().child(name.clone()))
                            .child(div().text_color(theme.subtext).child(detail));
                        if !element.editable {
                            row = row.child(div().text_color(theme.locked).child("locked"));
                        }
                        row = row.on_click(move |event, _, cx| {
                            if event.click_count() == 2 && is_instance {
                                state.update(cx, |state, cx| {
                                    state.descend_schematic(&name, cx);
                                });
                            } else {
                                canvas.update(cx, |canvas, cx| {
                                    canvas.select_and_zoom(Hit::Element(index), cx);
                                });
                            }
                        });
                        rows.push(row.into_any_element());
                    }
                    rows.push(header("Nets").into_any_element());
                    let highlighted = canvas.highlighted_net();
                    let mut order = (0..scene.nets.len()).collect::<Vec<_>>();
                    order.sort_by_key(|net| !scene.port_nets.contains(&(*net as u32)));
                    for net in order {
                        let canvas = self.canvas.clone();
                        let is_port = scene.port_nets.contains(&(net as u32));
                        let is_selected = highlighted == Some(net as u32);
                        let row = row(format!("net_{net}").into(), is_selected, theme)
                            .child(div().flex_1().child(scene.nets[net].clone()))
                            .child(div().text_color(theme.subtext).child(if is_port {
                                "port"
                            } else {
                                ""
                            }))
                            .on_click(move |_, _, cx| {
                                canvas.update(cx, |canvas, cx| {
                                    let next = (canvas.highlighted_net() != Some(net as u32))
                                        .then_some(net as u32);
                                    canvas.highlight_net(next, cx);
                                });
                            });
                        rows.push(row.into_any_element());
                    }
                }
            }
        }
        div()
            .flex()
            .flex_col()
            .h_full()
            .w(px(SIDEBAR_WIDTH))
            .flex_shrink_0()
            .p_1()
            .border_r_1()
            .border_t_1()
            .border_color(theme.divider)
            .bg(theme.sidebar)
            .min_h_0()
            .child(
                div()
                    .id("schematic_sidebar_rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .children(rows),
            )
    }
}

/// What editing a property row writes.
#[derive(Debug, Clone, PartialEq)]
enum EditTarget {
    /// An argument of the selection's call.
    Argument(CallArgument),
    /// The selection's position.
    Position,
    /// The selected wire's route.
    Route,
    /// The selection's binding, with every reference to it.
    Rename,
}

/// A row of the properties panel.
struct Row {
    label: String,
    value: String,
    /// What editing the value writes, for an editable row.
    edit: Option<EditTarget>,
}

impl Row {
    fn text(label: impl Into<String>, value: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            edit: None,
        }
    }

    fn editable(label: impl Into<String>, value: impl Into<String>, edit: EditTarget) -> Self {
        Self {
            label: label.into(),
            value: value.into(),
            edit: Some(edit),
        }
    }
}

/// The device kinds the palette offers.
const PALETTE: [&str; 8] = [
    "Nmos", "Pmos", "Res", "Cap", "Diode", "Npn", "Pnp", "Subckt",
];

/// The placement keywords, which the placement row edits instead.
const PLACEMENT_KEYWORDS: [&str; 5] = ["x", "y", "angle", "reflect", "route"];

/// Reads `(x, y)` or `x, y` as a grid point.
fn parse_point(text: &str) -> Option<(i64, i64)> {
    let text = text.trim().trim_start_matches('(').trim_end_matches(')');
    let (x, y) = text.split_once(',')?;
    Some((x.trim().parse().ok()?, y.trim().parse().ok()?))
}

/// Reads `[(x, y), …]` as route corners.
fn parse_route(text: &str) -> Option<Vec<(i64, i64)>> {
    let text = text
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .trim();
    if text.is_empty() {
        return Some(Vec::new());
    }
    text.split(')')
        .map(str::trim)
        .map(|part| part.trim_start_matches(',').trim())
        .filter(|part| !part.is_empty())
        .map(|part| parse_point(&format!("{part})")))
        .collect()
}

/// The right sidebar of the schematic and symbol views: the device palette,
/// the properties of the selection, and what stops a deletion.
pub struct PropertiesSideBar {
    editor_state: Entity<EditorState>,
    canvas: Entity<SchematicCanvas>,
    scroll_handle: ScrollHandle,
    /// The editor of the row being edited.
    value_input: Entity<TextInput>,
    editing: Option<EditTarget>,
    _subscriptions: Vec<Subscription>,
}

impl PropertiesSideBar {
    pub fn new(
        cx: &mut Context<Self>,
        editor_state: &Entity<EditorState>,
        canvas: &Entity<SchematicCanvas>,
    ) -> Self {
        let canvas_focus = canvas.read(cx).focus_handle(cx);
        let this = cx.entity().downgrade();
        let submit = this.clone();
        let dismiss = this;
        let value_input = cx.new(|cx| {
            TextInput::new_prompt(
                cx.focus_handle(),
                canvas_focus,
                editor_state,
                "Value…",
                Rc::new(move |text, window, cx| {
                    let _ = submit.update(cx, |sidebar, cx| sidebar.submit_edit(text, window, cx));
                }),
                Rc::new(move |window, cx| {
                    let _ = dismiss.update(cx, |sidebar, cx| sidebar.cancel_edit(window, cx));
                }),
            )
        });
        let input_focus = value_input.read(cx).focus_handle(cx);
        canvas.update(cx, |canvas, _| canvas.properties_focus = Some(input_focus));
        let subscriptions = vec![
            cx.observe(editor_state, |_, _, cx| cx.notify()),
            cx.observe(canvas, |_, _, cx| cx.notify()),
            cx.observe(&value_input, |_, _, cx| cx.notify()),
            cx.subscribe(canvas, |sidebar, _, event, cx| match event {
                CanvasEvent::EditProperties => sidebar.edit_first_row(cx),
            }),
        ];
        Self {
            editor_state: editor_state.clone(),
            canvas: canvas.clone(),
            scroll_handle: ScrollHandle::new(),
            value_input,
            editing: None,
            _subscriptions: subscriptions,
        }
    }

    /// Starts editing the first editable row, for `q`; the canvas moves the
    /// focus.
    fn edit_first_row(&mut self, cx: &mut Context<Self>) {
        let first = self
            .properties(cx)
            .into_iter()
            .find_map(|row| row.edit.map(|edit| (edit, row.value)));
        if let Some((target, value)) = first {
            self.editing = Some(target);
            self.value_input
                .update(cx, |input, cx| input.show(&value, "Value…", cx));
            cx.notify();
        }
    }

    fn begin_edit(
        &mut self,
        target: EditTarget,
        value: &str,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editing = Some(target);
        let focus = self.value_input.update(cx, |input, cx| {
            input.show(value, "Value…", cx);
            input.focus_handle.clone()
        });
        window.focus(&focus);
        cx.notify();
    }

    fn submit_edit(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        let Some(target) = self.editing.take() else {
            return;
        };
        let text = text.trim().to_owned();
        self.canvas.update(cx, |canvas, cx| match target {
            EditTarget::Argument(argument) if !text.is_empty() => {
                canvas.set_argument(argument, text, cx);
            }
            EditTarget::Position => {
                if let Some(position) = parse_point(&text) {
                    canvas.set_position(position, cx);
                }
            }
            EditTarget::Route => {
                if let Some(corners) = parse_route(&text) {
                    canvas.set_route(corners, cx);
                }
            }
            EditTarget::Rename if !text.is_empty() => canvas.rename(text, cx),
            EditTarget::Argument(_) | EditTarget::Rename => {}
        });
        window.focus(&self.canvas.read(cx).focus_handle(cx));
        cx.notify();
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = None;
        window.focus(&self.canvas.read(cx).focus_handle(cx));
        cx.notify();
    }

    /// The rows for the selection.
    fn properties(&self, cx: &Context<Self>) -> Vec<Row> {
        let state = self.editor_state.read(cx);
        let canvas = self.canvas.read(cx);
        let mut rows = Vec::new();
        match state.view {
            View::Symbol => {
                let Some(scene) = canvas.symbol_scene() else {
                    return rows;
                };
                if canvas.symbol_selection().len() > 1 {
                    rows.push(Row::text(
                        "Selection",
                        format!("{} items", canvas.symbol_selection().len()),
                    ));
                }
                // The source arguments of the primary selection's call.
                let primary = canvas.symbol_selection().first().copied();
                let span = primary.and_then(|selected| match selected {
                    SymbolSelection::Item(index) => scene.items.get(index)?.span.clone(),
                    SymbolSelection::Pin(port) => {
                        scene.pins.iter().find(|pin| pin.port == port)?.span.clone()
                    }
                });
                let arguments = span.and_then(|span| {
                    canvas
                        .call_arguments()
                        .filter(|(call, _)| *call == span)
                        .map(|(_, arguments)| arguments)
                });
                // Every argument of a symbol call is a value the panel can
                // rewrite in place.
                let argument_rows = |rows: &mut Vec<Row>, editable: bool| {
                    let Some(arguments) = arguments else {
                        return;
                    };
                    for (index, value) in arguments.positional.iter().enumerate() {
                        let label = match index {
                            0 => "Argument",
                            _ => "Value",
                        };
                        if editable {
                            rows.push(Row::editable(
                                label,
                                value.clone(),
                                EditTarget::Argument(CallArgument::Positional(index)),
                            ));
                        } else {
                            rows.push(Row::text(label, value.clone()));
                        }
                    }
                    for (name, value) in &arguments.keywords {
                        if editable {
                            rows.push(Row::editable(
                                name.clone(),
                                value.clone(),
                                EditTarget::Argument(CallArgument::Keyword(name.clone())),
                            ));
                        } else {
                            rows.push(Row::text(name.clone(), value.clone()));
                        }
                    }
                };
                match primary {
                    Some(SymbolSelection::Pin(port)) => {
                        if let Some(pin) = scene.pins.iter().find(|pin| pin.port == port) {
                            rows.push(Row::text("Pin", pin.name.clone()));
                            rows.push(Row::text(
                                "Position",
                                format!("({}, {})", pin.position.0, pin.position.1),
                            ));
                            rows.push(Row::text("Label", pin.labeled.to_string()));
                            argument_rows(&mut rows, pin.editable);
                            if let Some(origin) = &pin.origin {
                                rows.push(Row::text("Group", group_name(origin)));
                            }
                            if !pin.editable {
                                rows.push(Row::text("Locked", lock_reason(scene.generated)));
                            }
                            rows.push(Row::text(
                                "Note",
                                "Moving a pin moves wire ends in every parent",
                            ));
                        }
                    }
                    Some(SymbolSelection::Item(index)) => {
                        if let Some(item) = scene.items.get(index) {
                            rows.push(Row::text("Item", item_name(&item.kind)));
                            if let Some((x0, y0, x1, y1)) = item.kind.bounds() {
                                rows.push(Row::text(
                                    "Bounds",
                                    format!("({x0}, {y0}) to ({x1}, {y1})"),
                                ));
                            }
                            argument_rows(&mut rows, item.editable);
                            if let Some(origin) = &item.origin {
                                rows.push(Row::text("Group", group_name(origin)));
                            }
                            if !item.editable {
                                rows.push(Row::text("Locked", lock_reason(scene.generated)));
                            }
                        }
                    }
                    None => {
                        if scene.generated {
                            rows.push(Row::text("Symbol", "Generated box"));
                        }
                        if !scene.missing.is_empty() {
                            rows.push(Row::text("Ports without pins", scene.missing.join(", ")));
                        }
                    }
                }
            }
            _ => {
                let Some(scene) = canvas.scene() else {
                    return rows;
                };
                if canvas.selection().len() > 1 {
                    rows.push(Row::text(
                        "Selection",
                        format!("{} items", canvas.selection().len()),
                    ));
                }
                let primary = canvas.selection().first().copied();
                // The source arguments of the primary selection's call.
                let arguments = primary
                    .and_then(|hit| match hit {
                        Hit::Element(index) | Hit::Pin { element: index, .. } => {
                            scene.elements.get(index).and_then(|e| e.span.clone())
                        }
                        _ => None,
                    })
                    .and_then(|span| {
                        canvas
                            .call_arguments()
                            .filter(|(call, _)| *call == span)
                            .map(|(_, arguments)| arguments)
                    });
                let position_row = |label: &str, (x, y): (i64, i64), auto: bool, editable: bool| {
                    let value = format!("({x}, {y}){}", if auto { " (auto)" } else { "" });
                    if editable {
                        Row::editable(label, value, EditTarget::Position)
                    } else {
                        Row::text(label, value)
                    }
                };
                match primary {
                    Some(Hit::Element(index) | Hit::Pin { element: index, .. }) => {
                        if let Some(element) = scene.elements.get(index) {
                            // The element name carries its kind's letter; the
                            // binding is what gets renamed.
                            let binding = element
                                .name
                                .strip_prefix(['X', 'M', 'R', 'C', 'D', 'Q'])
                                .filter(|_| element.editable && element.span.is_some())
                                .map(str::to_owned);
                            rows.push(match binding {
                                Some(binding) => Row::editable(
                                    format!("Name ({})", element.name),
                                    binding,
                                    EditTarget::Rename,
                                ),
                                None => Row::text("Name", element.name.clone()),
                            });
                            match (element.element, arguments) {
                                (Element::Instance(_), Some(arguments)) => {
                                    if let Some(cell) = &element.cell_name {
                                        rows.push(Row::text("Cell", cell.clone()));
                                    }
                                    if let Some(invocation) = arguments.positional.first() {
                                        rows.push(Row::editable(
                                            "Invocation",
                                            invocation.clone(),
                                            EditTarget::Argument(CallArgument::Positional(0)),
                                        ));
                                    }
                                }
                                (Element::Device(device), Some(arguments)) => {
                                    rows.push(Row::text("Device", format!("{device}")));
                                    match arguments.positional.get(2) {
                                        Some(model) => rows.push(Row::editable(
                                            "Model",
                                            model.clone(),
                                            EditTarget::Argument(CallArgument::Positional(2)),
                                        )),
                                        None => rows.push(Row::editable(
                                            "Model",
                                            String::new(),
                                            EditTarget::Argument(CallArgument::Positional(2)),
                                        )),
                                    }
                                    for (name, value) in &arguments.keywords {
                                        if PLACEMENT_KEYWORDS.contains(&name.as_str()) {
                                            continue;
                                        }
                                        rows.push(Row::editable(
                                            name.clone(),
                                            value.clone(),
                                            EditTarget::Argument(CallArgument::Keyword(
                                                name.clone(),
                                            )),
                                        ));
                                    }
                                }
                                (_, None) => {
                                    if let Some(cell) = &element.cell_name {
                                        rows.push(Row::text("Cell", cell.clone()));
                                    }
                                    if let Element::Device(device) = element.element {
                                        rows.push(Row::text("Device", format!("{device}")));
                                    }
                                    if let Some(model) = &element.model {
                                        rows.push(Row::text("Model", model.clone()));
                                    }
                                    for (name, value) in &element.params {
                                        rows.push(Row::text(name.clone(), value.clone()));
                                    }
                                }
                            }
                            let (x, y) = element.placement.position.unwrap_or((0, 0));
                            rows.push(position_row(
                                "Position",
                                (x, y),
                                element.auto_placed,
                                element.editable && element.span.is_some(),
                            ));
                            rows.push(Row::text(
                                "Orientation",
                                format!(
                                    "{:?}{}",
                                    element.placement.angle,
                                    if element.placement.reflect {
                                        " reflected"
                                    } else {
                                        ""
                                    }
                                ),
                            ));
                            if let Some(Hit::Pin { terminal, .. }) = primary
                                && let Some(pin) = element.pins.get(terminal)
                            {
                                rows.push(Row::text(
                                    format!("Pin {}", pin.name),
                                    scene
                                        .nets
                                        .get(pin.net as usize)
                                        .cloned()
                                        .unwrap_or_default(),
                                ));
                            }
                            if !element.editable {
                                rows.push(Row::text(
                                    "Locked",
                                    "created in a loop or function, or placed by an expression",
                                ));
                            }
                        }
                    }
                    Some(Hit::Port(index)) => {
                        if let Some(port) = scene.ports.get(index) {
                            rows.push(if port.editable {
                                Row::editable("Port", port.name.clone(), EditTarget::Rename)
                            } else {
                                Row::text("Port", port.name.clone())
                            });
                            rows.push(position_row(
                                "Position",
                                port.position,
                                port.auto_placed,
                                port.editable,
                            ));
                            if !port.editable {
                                rows.push(Row::text(
                                    "Locked",
                                    "created in a loop or function, or placed by an expression",
                                ));
                            }
                        }
                    }
                    Some(Hit::Label(index)) => {
                        if let Some(label) = scene.labels.get(index) {
                            rows.push(if label.editable {
                                Row::editable("Label", label.name.clone(), EditTarget::Rename)
                            } else {
                                Row::text("Label", label.name.clone())
                            });
                            rows.push(position_row(
                                "Position",
                                label.position,
                                false,
                                label.editable,
                            ));
                            if !label.editable {
                                rows.push(Row::text(
                                    "Locked",
                                    "created in a loop or function, or placed by an expression",
                                ));
                            }
                        }
                    }
                    Some(Hit::Wire(index)) => {
                        if let Some(wire) = scene.wires.get(index) {
                            rows.push(Row::text(
                                "Wire",
                                scene
                                    .nets
                                    .get(wire.net as usize)
                                    .cloned()
                                    .unwrap_or_default(),
                            ));
                            let corners = wire.points[1..wire.points.len() - 1]
                                .iter()
                                .map(|(x, y)| format!("({x}, {y})"))
                                .collect::<Vec<_>>()
                                .join(", ");
                            let route = format!("[{corners}]");
                            rows.push(if wire.editable {
                                Row::editable("Route", route, EditTarget::Route)
                            } else {
                                Row::text("Route", route)
                            });
                            if !wire.editable {
                                rows.push(Row::text(
                                    "Locked",
                                    "connected in a loop or function, or routed by an expression",
                                ));
                            }
                        }
                    }
                    Some(Hit::Flyline(index)) => {
                        if let Some(flyline) = scene.flylines.get(index) {
                            rows.push(Row::text(
                                "Flyline",
                                scene
                                    .nets
                                    .get(flyline.net as usize)
                                    .cloned()
                                    .unwrap_or_default(),
                            ));
                            rows.push(Row::text(
                                "Note",
                                "A connection without a wire; the wire tool draws it",
                            ));
                        }
                    }
                    None => {}
                }
                if let Some(net) = canvas.highlighted_net() {
                    rows.push(Row::text(
                        "Net",
                        scene.nets.get(net as usize).cloned().unwrap_or_default(),
                    ));
                    let conflicts = scene
                        .conflicts
                        .iter()
                        .filter(|c| c.nets.contains(&net))
                        .count();
                    if conflicts > 0 {
                        rows.push(Row::text("Conflicts", conflicts.to_string()));
                    }
                    for undrawn in scene.undrawn.iter().filter(|u| u.net == net) {
                        rows.push(Row::text("Undrawn route", undrawn.reason.clone()));
                    }
                }
                if !scene.conflicts.is_empty()
                    && canvas.selection().is_empty()
                    && canvas.highlighted_net().is_none()
                {
                    rows.push(Row::text("Conflicts", scene.conflicts.len().to_string()));
                }
            }
        }
        rows
    }

    fn button(
        id: impl Into<gpui::ElementId>,
        text: impl Into<SharedString>,
        theme: &'static crate::theme::Theme,
    ) -> gpui::Stateful<gpui::Div> {
        div()
            .id(id)
            .px_2()
            .py_1()
            .border_1()
            .border_color(theme.divider)
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(theme.selection))
            .child(text.into())
    }
}

fn lock_reason(generated: bool) -> String {
    if generated {
        "generated symbol".into()
    } else {
        "drawn in a function or placed by an expression".into()
    }
}

fn item_name(kind: &SymbolItemKind) -> &'static str {
    match kind {
        SymbolItemKind::Line { .. } => "Line",
        SymbolItemKind::Polygon { .. } => "Polygon",
        SymbolItemKind::Rect { .. } => "Rectangle",
        SymbolItemKind::Circle { .. } => "Circle",
        SymbolItemKind::Arc { .. } => "Arc",
        SymbolItemKind::Text {
            text: SymbolText::Literal(_),
            ..
        } => "Text",
        SymbolItemKind::Text {
            text: SymbolText::InstName,
            ..
        } => "Instance name",
        SymbolItemKind::Text {
            text: SymbolText::Param { .. },
            ..
        } => "Parameter",
    }
}

/// How a group is named: by the file and line of the call that drew it.
fn group_name(origin: &argonc::ast::Span) -> String {
    let file = origin
        .path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    format!("drawn by the call at {file}:{}", origin.span.start())
}

impl Render for PropertiesSideBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = self.editor_state.read(cx).theme();
        let view = self.editor_state.read(cx).view;
        let rows = self.properties(cx);
        let (name_prompt, name_input, blocked, remove_route) = {
            let canvas = self.canvas.read(cx);
            // A selected editable wire with a route can lose it.
            let remove_route = view != View::Symbol
                && canvas.selection().iter().any(|hit| {
                    matches!(hit, Hit::Wire(index) if canvas
                        .scene()
                        .is_some_and(|scene| scene.wires.get(*index).is_some_and(|wire| wire.editable)))
                });
            (
                canvas.name_prompt(),
                canvas.name_input.clone(),
                canvas.blocked_delete().to_vec(),
                remove_route,
            )
        };
        let remove_route_button = remove_route.then(|| {
            let canvas = self.canvas.clone();
            Self::button("remove_route", "Remove Route", theme)
                .mt_1()
                .on_click(move |_, _, cx| {
                    canvas.update(cx, |canvas, cx| canvas.remove_selected_route(cx));
                })
        });
        let prompt = name_prompt.map(|title| {
            div()
                .flex()
                .flex_col()
                .pb_1()
                .child(div().text_color(theme.subtext).child(title))
                .child(name_input)
        });
        let symbol_palette = (view == View::Symbol).then(|| {
            let canvas = self.canvas.clone();
            let parameters = canvas.read(cx).cell_parameters().to_vec();
            let generated = canvas
                .read(cx)
                .symbol_scene()
                .is_some_and(|scene| scene.generated);
            let inst_name_button = {
                let canvas = canvas.clone();
                Self::button("text_inst_name", "Instance name", theme).on_click(move |_, _, cx| {
                    canvas.update(cx, |canvas, cx| {
                        canvas.place_symbol_text(TextSpec::InstName, cx);
                    });
                })
            };
            let create_symbol = generated.then(|| {
                let canvas = canvas.clone();
                Self::button("create_symbol_palette", "Create Symbol", theme).on_click(
                    move |_, _, cx| {
                        canvas.update(cx, |canvas, cx| canvas.create_symbol(cx));
                    },
                )
            });
            div()
                .flex()
                .flex_col()
                .children(create_symbol.map(|button| div().flex().flex_row().pb_1().child(button)))
                .child(header("Text"))
                .child(
                    div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_1()
                        .child(inst_name_button)
                        .children(parameters.into_iter().enumerate().map(|(index, name)| {
                            let canvas = canvas.clone();
                            let label = format!("{name}=");
                            Self::button(("text_param", index), label, theme).on_click(
                                move |_, _, cx| {
                                    canvas.update(cx, |canvas, cx| {
                                        canvas.place_symbol_text(TextSpec::Param(name.clone()), cx);
                                    });
                                },
                            )
                        })),
                )
        });
        let palette = (view == View::Schematic).then(|| {
            let state = self.editor_state.clone();
            div().flex().flex_col().child(header("Devices")).child(
                div()
                    .flex()
                    .flex_row()
                    .flex_wrap()
                    .gap_1()
                    .children(PALETTE.into_iter().map(|kind| {
                        let state = state.clone();
                        Self::button(kind, kind, theme).on_click(move |_, _, cx| {
                            crate::editor::open_command(
                                &state,
                                Some(format!("Argon device {kind} ")),
                                true,
                                cx,
                            );
                        })
                    })),
            )
        });
        let blocked_rows = (!blocked.is_empty()).then(|| {
            let canvas = self.canvas.clone();
            div()
                .flex()
                .flex_col()
                .child(header("Deletion blocked"))
                .child(
                    div()
                        .text_color(theme.subtext)
                        .child("These references must go first:"),
                )
                .children(blocked.into_iter().enumerate().map(|(index, reference)| {
                    let canvas = canvas.clone();
                    let file = reference
                        .span
                        .path
                        .file_name()
                        .map(|name| name.to_string_lossy().into_owned())
                        .unwrap_or_default();
                    let span = reference.span.clone();
                    div()
                        .id(("blocking_reference", index))
                        .flex()
                        .flex_col()
                        .pb_1()
                        .cursor_pointer()
                        .hover(|style| style.bg(theme.selection))
                        .child(
                            div()
                                .text_color(theme.subtext)
                                .child(format!("{file}:{}", reference.line + 1)),
                        )
                        .child(div().child(reference.text.clone()))
                        .on_click(move |_, _, cx| {
                            canvas.read(cx).reveal(span.clone(), cx);
                        })
                }))
        });
        let editing = self.editing.clone();
        let value_input = self.value_input.clone();
        div()
            .flex()
            .flex_col()
            .h_full()
            .w(px(SIDEBAR_WIDTH))
            .flex_shrink_0()
            .p_1()
            .border_l_1()
            .border_t_1()
            .border_color(theme.divider)
            .bg(theme.sidebar)
            .min_h_0()
            .children(prompt)
            .children(palette)
            .children(symbol_palette)
            .child(header("Properties"))
            .child(
                div()
                    .id("properties_rows")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&self.scroll_handle)
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .children(rows.into_iter().enumerate().map(|(index, row)| {
                        let label = div().text_color(theme.subtext).child(row.label.clone());
                        let is_editing = row.edit.is_some() && editing == row.edit;
                        let mut element = div()
                            .id(("property", index))
                            .flex()
                            .flex_col()
                            .pb_1()
                            .child(label);
                        if is_editing {
                            element = element.child(value_input.clone());
                        } else {
                            element = element.child(div().child(row.value.clone()));
                        }
                        if let Some(target) = row.edit.filter(|_| !is_editing) {
                            let value = row.value.clone();
                            element = element
                                .cursor_pointer()
                                .hover(|style| style.bg(theme.selection))
                                .on_click(cx.listener(move |sidebar, _, window, cx| {
                                    sidebar.begin_edit(target.clone(), &value, window, cx);
                                }));
                        }
                        element
                    }))
                    .children(remove_route_button)
                    .children(blocked_rows),
            )
    }
}
