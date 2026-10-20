//! The schematic and symbol views of the open cell: one canvas that draws a
//! scene built from the compiled result and never writes files itself.

pub mod entry;
pub mod hit;
pub mod paint;
pub mod place;
#[cfg(test)]
mod render_tests;
pub mod scene;
pub mod sidebar;
pub mod stretch;
pub mod symbol_scene;
pub mod symbol_tools;
pub mod symbols;
pub mod tools;
pub mod wires;

use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;

use analyzer::rpc::{
    AddSignalRequest, AddSymbolItemsRequest, BlockingReference, CallArgument, CallArguments,
    ClipboardItem, CreateSymbolRequest, DeleteOutcome, DeleteRequest, DevicePreview, EditReceipt,
    ElementPlacement, NewSymbolItem, PasteRequest, PlaceDeviceRequest, PlaceRequest,
    PlaceSchematicInstanceRequest, RenameRequest, SchematicInstancePreview, SchematicTarget,
    SetArgumentRequest,
};
use argonc::ast::Span;
use argonc::compile::{
    Align, CompiledData, Element, NetIdx, Placement, SymbolItemKind, SymbolText, device_symbol,
};
use gpui::{
    App, Bounds, Context, DefiniteLength, Entity, EventEmitter, FocusHandle, Focusable,
    IntoElement, Length, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point,
    Render, ScrollWheelEvent, Size, Style, Subscription, Task, Window, div, prelude::*, px,
};
use tower_lsp_server::ls_types::MessageType;

use self::entry::{EndError, WireDraft, WireRequest};
use self::hit::{Hit, hits_at};
use self::paint::{Frame, FrameContent, Overlay, SchematicElement};
use self::scene::{DisplayPrefs, Overrides, Scene, build_scene_with};
use self::stretch::GridPt;
use self::symbol_scene::{SymbolOverrides, SymbolScene, build_symbol_scene_with};
use self::symbol_tools::{HandleKind, TextSpec};
use self::symbols::{Bounds as WorldBounds, PlacedSymbol, Pt, Shape, place_symbol, place_text};
use self::tools::{Drag, DragKind};
use crate::actions::{
    Backspace, Cancel, Copy, CreateSymbol, DarkMode, Delete, Descend, DrawPolygon, DrawRect, Edit,
    Enter, Fit, FixPositions, FlipBend, LightMode, Mirror, NextPort, PanDown, PanLeft, PanRight,
    PanUp, Paste, PlaceLabel, PlacePort, RemoveRoute, Return, Rotate, SelectMode, SymbolArc,
    SymbolCircle, SymbolLine, SymbolMirror, SymbolPin, SymbolRotate,
    SymbolText as SymbolTextAction, ToggleFlylines, Wire, ZoomIn, ZoomOut,
};
use crate::editor::input::TextInput;
use crate::editor::viewport::Viewport;
use crate::editor::{EditorState, View};

/// The result of a schematic edit sent to the analyzer.
type EditFuture = Pin<Box<dyn Future<Output = anyhow::Result<Option<EditReceipt>>>>>;

const KEYBOARD_PAN_STEP: Pixels = px(64.);
const KEYBOARD_ZOOM_FACTOR: f32 = 1.2;
const FIT_MARGIN: f32 = 24.;
/// Hit tolerance in pixels.
const HIT_TOLERANCE: f32 = 6.;

/// What is selected in the symbol view.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SymbolSelection {
    /// A shape, by its index among the placed shapes.
    Item(usize),
    /// The pin of a port.
    Pin(usize),
}

/// Identifies what a scene was built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SceneKey {
    output: *const CompiledData,
    cell: argonc::compile::CellId,
    prefs: DisplayPrefs,
    /// Bumped whenever the overrides change.
    generation: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct SymbolSceneKey {
    output: *const CompiledData,
    cell: argonc::compile::CellId,
    inst_name: String,
    /// Bumped whenever the overrides change.
    generation: u64,
}

/// An edit the analyzer accepted, drawn over each snapshot until one
/// compiled from the rewritten source arrives.
#[derive(Debug, Clone, Default)]
struct PendingEdit {
    overrides: Overrides,
    symbol_overrides: SymbolOverrides,
    /// New items the edit adds, drawn as ghosts.
    ghosts: Vec<Overlay>,
    /// The GUI's revision when the gesture ended; a later one retires it.
    after_revision: Option<u64>,
}

/// Which symbol tool is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SymbolToolKind {
    #[default]
    Select,
    Line,
    Polygon,
    Rect,
    Circle,
    Arc,
    Text,
    Pin,
}

/// The active tool of the symbol view and the clicks it has so far.
#[derive(Debug, Clone, PartialEq)]
pub enum SymbolTool {
    Select,
    Line {
        points: Vec<Pt>,
    },
    Polygon {
        points: Vec<Pt>,
    },
    Rect {
        start: Option<Pt>,
    },
    Circle {
        center: Option<Pt>,
    },
    Arc {
        center: Option<Pt>,
        start: Option<Pt>,
    },
    /// Text placed at a click, once it has content.
    Text {
        text: Option<TextSpec>,
    },
    /// A pin for `port`, which is the next port without one; `placed` are
    /// the ports whose pins wait for their compiled result.
    Pin {
        port: Option<usize>,
        placed: Vec<usize>,
    },
}

impl SymbolTool {
    pub fn kind(&self) -> SymbolToolKind {
        match self {
            SymbolTool::Select => SymbolToolKind::Select,
            SymbolTool::Line { .. } => SymbolToolKind::Line,
            SymbolTool::Polygon { .. } => SymbolToolKind::Polygon,
            SymbolTool::Rect { .. } => SymbolToolKind::Rect,
            SymbolTool::Circle { .. } => SymbolToolKind::Circle,
            SymbolTool::Arc { .. } => SymbolToolKind::Arc,
            SymbolTool::Text { .. } => SymbolToolKind::Text,
            SymbolTool::Pin { .. } => SymbolToolKind::Pin,
        }
    }
}

/// What a drag in the symbol view's select tool does.
#[derive(Debug, Clone, Copy, PartialEq)]
enum SymbolDragKind {
    /// Moves the selection.
    Move,
    /// Reshapes item `item` by one of its handles.
    Handle { item: usize, handle: HandleKind },
}

/// A symbol view drag in progress, over the symbol scene it began on.
#[derive(Debug, Clone)]
struct SymbolDrag {
    kind: SymbolDragKind,
    start: Pt,
    current: Pt,
    /// The symbol scene without the drag's preview.
    base: Arc<SymbolScene>,
    /// The selection in `base`.
    selection: Vec<SymbolSelection>,
    /// What a release selects if the drag never changed anything.
    click: Option<Vec<SymbolSelection>>,
}

impl SymbolDrag {
    /// The preview of the drag, or `None` until it changes anything.
    fn preview(&self) -> Option<SymbolOverrides> {
        let delta = (self.current.0 - self.start.0, self.current.1 - self.start.1);
        match self.kind {
            SymbolDragKind::Move => {
                if symbol_tools::snap_quarter(delta) == (0., 0.) {
                    return None;
                }
                Some(symbol_tools::move_overrides(
                    &self.base,
                    &self.selection,
                    delta,
                ))
            }
            SymbolDragKind::Handle { item, handle } => {
                if self.current == self.start {
                    return None;
                }
                let item = self.base.items.get(item)?;
                let span = item.span.clone()?;
                let to = symbol_tools::snap_quarter(self.current);
                let mut overrides = SymbolOverrides::default();
                overrides
                    .items
                    .insert(span, symbol_tools::reshaped(&item.kind, handle, to));
                Some(overrides)
            }
        }
    }
}

/// Which schematic tool is active.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ToolKind {
    #[default]
    Select,
    Instance,
    Device,
    Wire,
    Port,
    Label,
}

/// The active tool of the schematic view and what it holds.
#[derive(Debug, Clone)]
pub enum Tool {
    Select,
    /// An instance following the pointer, under `orientation`.
    PlaceInstance {
        preview: SchematicInstancePreview,
        orientation: Placement,
    },
    PlaceDevice {
        preview: DevicePreview,
        orientation: Placement,
    },
    /// A wire being drawn, once it has a start.
    Wire {
        draft: Option<WireDraft>,
        /// Whether a wire previewed over a flyline bends vertically first.
        flipped: bool,
    },
    /// A port, placed once it has a name.
    Port {
        name: Option<String>,
    },
    /// A net label, placed once it has a name.
    Label {
        name: Option<String>,
    },
}

impl Tool {
    pub fn kind(&self) -> ToolKind {
        match self {
            Tool::Select => ToolKind::Select,
            Tool::PlaceInstance { .. } => ToolKind::Instance,
            Tool::PlaceDevice { .. } => ToolKind::Device,
            Tool::Wire { .. } => ToolKind::Wire,
            Tool::Port { .. } => ToolKind::Port,
            Tool::Label { .. } => ToolKind::Label,
        }
    }

    /// Whether the tool is waiting for a name from the prompt.
    fn wants_name(&self) -> bool {
        matches!(self, Tool::Port { name: None } | Tool::Label { name: None })
    }
}

/// What the canvas tells its sidebars.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CanvasEvent {
    /// Start editing the first editable property of the selection.
    EditProperties,
}

pub struct SchematicCanvas {
    focus_handle: FocusHandle,
    pub state: Entity<EditorState>,
    pub bg_style: Style,
    viewport: Viewport,
    symbol_viewport: Viewport,
    screen_bounds: Bounds<Pixels>,
    scene: Option<Arc<Scene>>,
    scene_key: Option<SceneKey>,
    scene_task: Option<Task<()>>,
    symbol_scene: Option<Arc<SymbolScene>>,
    symbol_scene_key: Option<SymbolSceneKey>,
    symbol_scene_task: Option<Task<()>>,
    selection: Vec<Hit>,
    /// The selection by the spans of its calls, which survive a rebuild.
    selection_spans: Vec<Span>,
    hover: Option<Hit>,
    highlighted_net: Option<NetIdx>,
    symbol_selection: Vec<SymbolSelection>,
    symbol_tool: SymbolTool,
    symbol_drag: Option<SymbolDrag>,
    symbol_drag_overrides: Option<SymbolOverrides>,
    /// The parameters of the cell on screen, for the text tool.
    cell_parameters: Vec<String>,
    /// The cell `cell_parameters` were fetched for.
    cell_parameters_for: Option<argonc::compile::CellId>,
    cell_parameters_task: Option<Task<()>>,
    is_dragging: bool,
    drag_start: Point<Pixels>,
    offset_start: Point<Pixels>,
    /// A select-tool drag of items or a wire segment.
    drag: Option<Drag>,
    drag_overrides: Option<Overrides>,
    pending: Option<PendingEdit>,
    /// One gesture at a time: editing pauses until the edit's RPC returns.
    edit_in_flight: bool,
    /// The source revision of the last accepted edit, until a snapshot that
    /// new arrives. The analyzer refuses edits drawn on an older snapshot.
    awaited_revision: Option<u64>,
    overrides_generation: u64,
    edit_task: Option<Task<()>>,
    /// Fit the next scene of each view on its first paint.
    pending_fit: bool,
    pending_symbol_fit: bool,
    tool: Tool,
    /// The pointer's grid position over the canvas.
    cursor: Option<GridPt>,
    /// The pointer's world position over the canvas.
    cursor_world: Option<Pt>,
    /// Whether the symbol view is shown, as of the last paint or event.
    in_symbol_view: bool,
    /// A wire ended in empty space, waiting for the label's name.
    label_prompt: Option<(WireDraft, GridPt)>,
    /// The name prompt of the port and label tools, shown by the properties
    /// panel.
    pub(crate) name_input: Entity<TextInput>,
    /// The focus of the properties panel's editor, for `q`.
    pub(crate) properties_focus: Option<FocusHandle>,
    /// The source arguments of the primary selection's call.
    call_arguments: Option<(Span, CallArguments)>,
    call_arguments_task: Option<Task<()>>,
    /// The references that stopped the last deletion.
    blocked_delete: Vec<BlockingReference>,
    /// The statements copied last, with the corner of their bounds that
    /// pasting puts at the pointer.
    clipboard: Option<(Vec<ClipboardItem>, Pt)>,
    _subscriptions: Vec<Subscription>,
}

impl EventEmitter<CanvasEvent> for SchematicCanvas {}

impl SchematicCanvas {
    pub(crate) fn new(
        cx: &mut Context<Self>,
        state: &Entity<EditorState>,
        focus_handle: FocusHandle,
    ) -> Self {
        let subscriptions = vec![cx.observe(state, |canvas, _, cx| {
            canvas.refresh(cx);
            cx.notify();
        })];
        let canvas_focus = focus_handle.clone();
        let this = cx.entity().downgrade();
        let submit = this.clone();
        let dismiss = this;
        let name_input = cx.new(|cx| {
            TextInput::new_prompt(
                cx.focus_handle(),
                canvas_focus,
                state,
                "Name…",
                Rc::new(move |text, window, cx| {
                    let _ = submit.update(cx, |canvas, cx| canvas.name_entered(text, window, cx));
                }),
                Rc::new(move |window, cx| {
                    let _ = dismiss.update(cx, |canvas, cx| canvas.name_dismissed(window, cx));
                }),
            )
        });
        let mut canvas = SchematicCanvas {
            focus_handle,
            state: state.clone(),
            bg_style: Style {
                size: Size {
                    width: Length::Definite(DefiniteLength::Fraction(1.)),
                    height: Length::Definite(DefiniteLength::Fraction(1.)),
                },
                ..Style::default()
            },
            viewport: Viewport::default(),
            symbol_viewport: Viewport::default(),
            screen_bounds: Bounds::default(),
            scene: None,
            scene_key: None,
            scene_task: None,
            symbol_scene: None,
            symbol_scene_key: None,
            symbol_scene_task: None,
            selection: Vec::new(),
            selection_spans: Vec::new(),
            hover: None,
            highlighted_net: None,
            symbol_selection: Vec::new(),
            symbol_tool: SymbolTool::Select,
            symbol_drag: None,
            symbol_drag_overrides: None,
            cell_parameters: Vec::new(),
            cell_parameters_for: None,
            cell_parameters_task: None,
            is_dragging: false,
            drag_start: Point::default(),
            offset_start: Point::default(),
            drag: None,
            drag_overrides: None,
            pending: None,
            edit_in_flight: false,
            awaited_revision: None,
            overrides_generation: 0,
            edit_task: None,
            pending_fit: true,
            pending_symbol_fit: true,
            tool: Tool::Select,
            cursor: None,
            cursor_world: None,
            in_symbol_view: false,
            label_prompt: None,
            name_input,
            properties_focus: None,
            call_arguments: None,
            call_arguments_task: None,
            blocked_delete: Vec::new(),
            clipboard: None,
            _subscriptions: subscriptions,
        };
        canvas.refresh(cx);
        canvas
    }

    /// Switches tools, telling the toolbar.
    fn set_tool(&mut self, tool: Tool, cx: &mut Context<Self>) {
        let kind = tool.kind();
        self.tool = tool;
        self.label_prompt = None;
        if self.state.read(cx).schematic_tool != kind {
            self.state.update(cx, |state, cx| {
                state.schematic_tool = kind;
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Where a gesture adds code: the body of the cell on screen, in the
    /// snapshot it was drawn on.
    fn target(&self, cx: &App) -> Option<SchematicTarget> {
        let base_revision = self.state.read(cx).compilation_revision?;
        let (output, cell) = self.compiled(cx)?;
        let compiled = output.cells.get(&cell)?;
        Some(SchematicTarget {
            base_revision,
            cell: compiled
                .scopes
                .contains_key(&compiled.root)
                .then(|| compiled.scope_span(compiled.root).clone())?,
        })
    }

    /// Starts placing `preview`, which follows the pointer until a click.
    pub(crate) fn place_instance(
        &mut self,
        preview: SchematicInstancePreview,
        cx: &mut Context<Self>,
    ) {
        self.set_tool(
            Tool::PlaceInstance {
                preview,
                orientation: Placement::default(),
            },
            cx,
        );
    }

    /// Starts placing a device of `preview`'s kind.
    pub(crate) fn place_device(&mut self, preview: DevicePreview, cx: &mut Context<Self>) {
        self.set_tool(
            Tool::PlaceDevice {
                preview,
                orientation: Placement::default(),
            },
            cx,
        );
    }

    /// The symbol a placement tool draws at `at`.
    fn tool_symbol(&self, at: GridPt) -> Option<PlacedSymbol> {
        match &self.tool {
            Tool::PlaceInstance {
                preview,
                orientation,
            } => Some(place_symbol(
                &preview.symbol,
                &Placement {
                    position: Some(at),
                    ..*orientation
                },
                &preview.cell_name,
                &preview.pin_names,
            )),
            Tool::PlaceDevice {
                preview,
                orientation,
            } => {
                let symbol = device_symbol(preview.kind, preview.terminals);
                let pin_names = (0..preview.terminals)
                    .map(|terminal| preview.kind.terminal_label(terminal, preview.terminals))
                    .collect::<Vec<_>>();
                Some(place_symbol(
                    &symbol,
                    &Placement {
                        position: Some(at),
                        ..*orientation
                    },
                    "?",
                    &pin_names,
                ))
            }
            _ => None,
        }
    }

    /// What the active tool draws under the pointer.
    fn tool_preview(&self) -> Option<Overlay> {
        let cursor = self.cursor?;
        let point = (cursor.0 as f64, cursor.1 as f64);
        match &self.tool {
            Tool::Select => None,
            Tool::PlaceInstance { .. } | Tool::PlaceDevice { .. } => {
                self.tool_symbol(cursor).map(Overlay::Symbol)
            }
            Tool::Wire {
                draft: Some(draft), ..
            } => {
                let points = draft
                    .preview(cursor)
                    .into_iter()
                    .map(|(x, y)| (x as f64, y as f64))
                    .collect();
                Some(Overlay::Wire(points))
            }
            Tool::Wire {
                draft: None,
                flipped,
            } => {
                let scene = self.scene.as_ref()?;
                let Some(Hit::Flyline(index)) = self.hover else {
                    return None;
                };
                let points = entry::flyline_preview(scene, index, *flipped)?
                    .into_iter()
                    .map(|(x, y)| (x as f64, y as f64))
                    .collect();
                Some(Overlay::Wire(points))
            }
            Tool::Port { name: Some(name) } => Some(Overlay::Signal {
                at: point,
                name: name.clone(),
                port: true,
            }),
            Tool::Label { name: Some(name) } => Some(Overlay::Signal {
                at: point,
                name: name.clone(),
                port: false,
            }),
            Tool::Port { name: None } | Tool::Label { name: None } => None,
        }
    }

    /// A one-line hint for the active tool.
    pub(crate) fn hint(&self) -> Option<&'static str> {
        if self.label_prompt.is_some() {
            return Some("Type the net label's name and press Enter");
        }
        if self.in_symbol_view {
            return match &self.symbol_tool {
                SymbolTool::Select => None,
                SymbolTool::Line { .. } => Some(
                    "Click each point of the line; double-click or Enter finishes, Backspace drops the last point",
                ),
                SymbolTool::Polygon { .. } => {
                    Some("Click each point of the polygon; double-click or Enter closes it")
                }
                SymbolTool::Rect { start: None } => Some("Click the first corner of the rectangle"),
                SymbolTool::Rect { .. } => Some("Click the opposite corner"),
                SymbolTool::Circle { center: None } => Some("Click the center of the circle"),
                SymbolTool::Circle { .. } => Some("Click a point on the circle"),
                SymbolTool::Arc { center: None, .. } => Some("Click the center of the arc"),
                SymbolTool::Arc { start: None, .. } => Some("Click where the arc starts"),
                SymbolTool::Arc { .. } => {
                    Some("Click where the arc ends, counterclockwise from its start")
                }
                SymbolTool::Text { text: None } => Some("Type the text and press Enter"),
                SymbolTool::Text { .. } => Some("Click to place the text"),
                SymbolTool::Pin { port: None, .. } => Some("Every port has a pin"),
                SymbolTool::Pin { .. } => Some("Click to place the pin; Tab picks another port"),
            };
        }
        match &self.tool {
            Tool::Select => None,
            Tool::PlaceInstance { .. } => {
                Some("Click to place the instance; R rotates, Shift+R mirrors, Esc stops")
            }
            Tool::PlaceDevice { .. } => {
                Some("Click to place the device; R rotates, Shift+R mirrors, Esc stops")
            }
            Tool::Wire { draft: None, .. } => Some(
                "Click a pin or wire to start a wire, or a flyline to wire it; / flips the bend",
            ),
            Tool::Wire { draft: Some(_), .. } => Some(
                "Click to add a corner, a pin or wire to finish, or double-click for a new label; / flips the bend, Backspace drops the last corner",
            ),
            Tool::Port { name: None } => Some("Type the port's name and press Enter"),
            Tool::Port { name: Some(_) } => {
                Some("Click to place the port, or click a pin or wire to connect it there")
            }
            Tool::Label { name: None } => Some("Type the label's name and press Enter"),
            Tool::Label { name: Some(_) } => Some("Click a pin or wire to place the label on it"),
        }
    }

    /// The title of the name prompt, while a tool waits for a name.
    pub(crate) fn name_prompt(&self) -> Option<&'static str> {
        if self.label_prompt.is_some() {
            return Some("Net label name");
        }
        if self.in_symbol_view {
            return matches!(self.symbol_tool, SymbolTool::Text { text: None }).then_some("Text");
        }
        match self.tool {
            Tool::Port { name: None } => Some("Port name"),
            Tool::Label { name: None } => Some("Net label name"),
            _ => None,
        }
    }

    /// The references that stopped the last deletion.
    pub(crate) fn blocked_delete(&self) -> &[BlockingReference] {
        &self.blocked_delete
    }

    /// The source arguments of the primary selection's call, once fetched.
    pub(crate) fn call_arguments(&self) -> Option<&(Span, CallArguments)> {
        self.call_arguments.as_ref()
    }

    /// Reveals `span` in the editor.
    pub(crate) fn reveal(&self, span: Span, cx: &App) {
        let _ = self.state.read(cx).lang_server_client.select_span(span);
    }

    fn show_message(
        &self,
        typ: MessageType,
        message: impl Into<gpui::SharedString>,
        cx: &mut Context<Self>,
    ) {
        self.state.update(cx, |state, cx| {
            state.show_message(typ, message);
            cx.notify();
        });
    }

    /// Opens the name prompt for the port and label tools.
    fn prompt_name(&mut self, placeholder: &str, window: &mut Window, cx: &mut Context<Self>) {
        let focus = self.name_input.update(cx, |input, cx| {
            input.show("", placeholder, cx);
            input.focus_handle.clone()
        });
        window.focus(&focus);
        cx.notify();
    }

    /// The prompt's name, for the tool that asked for it.
    fn name_entered(&mut self, name: String, window: &mut Window, cx: &mut Context<Self>) {
        let name = name.trim().to_owned();
        if name.is_empty() {
            return;
        }
        self.name_input
            .update(cx, |input, cx| input.show("", "Name…", cx));
        window.focus(&self.focus_handle);
        if let SymbolTool::Text { text: None } = self.symbol_tool
            && self.in_symbol_view
        {
            self.set_symbol_tool(
                SymbolTool::Text {
                    text: Some(TextSpec::Literal(name)),
                },
                cx,
            );
            return;
        }
        if let Some((draft, at)) = self.label_prompt.take() {
            if let (Some(scene), Some(target)) = (self.scene.clone(), self.target(cx)) {
                let request = entry::label_request(&scene, target, &draft, name.clone(), at);
                self.send_signal(request, cx);
            }
            self.set_tool(
                Tool::Wire {
                    draft: None,
                    flipped: false,
                },
                cx,
            );
            return;
        }
        match self.tool {
            Tool::Port { .. } => self.set_tool(Tool::Port { name: Some(name) }, cx),
            Tool::Label { .. } => self.set_tool(Tool::Label { name: Some(name) }, cx),
            _ => {}
        }
    }

    fn name_dismissed(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        window.focus(&self.focus_handle);
        if self.label_prompt.take().is_some() {
            cx.notify();
            return;
        }
        if matches!(self.symbol_tool, SymbolTool::Text { text: None }) {
            self.set_symbol_tool(SymbolTool::Select, cx);
        }
        if self.tool.wants_name() {
            self.set_tool(Tool::Select, cx);
        }
    }

    /// Whether a new edit has to wait for the last one, which is in flight or
    /// not yet in a snapshot.
    fn edit_waiting(&self) -> bool {
        self.edit_in_flight || self.awaited_revision.is_some()
    }

    /// Whether a new edit can go out now, saying why not when it waits for a
    /// snapshot.
    fn can_edit(&self, cx: &mut Context<Self>) -> bool {
        if self.edit_in_flight {
            return false;
        }
        if self.awaited_revision.is_some() {
            self.show_message(
                MessageType::WARNING,
                "Waiting for the last edit to compile.",
                cx,
            );
            return false;
        }
        true
    }

    /// Notes that an edit was applied as source revision `revision`. New
    /// edits wait for a snapshot that new, unless it is already here.
    fn edit_accepted(&mut self, revision: u64, cx: &mut Context<Self>) {
        let current = self.state.read(cx).compilation_revision;
        if current.is_some_and(|current| current >= revision) {
            if self.pending.take().is_some() {
                self.set_overrides_changed();
                self.refresh(cx);
            }
        } else {
            self.awaited_revision = Some(revision);
        }
    }

    /// Sends an edit that adds `ghosts`, keeping them drawn until a snapshot
    /// compiled from the rewritten source arrives.
    fn send_edit(&mut self, ghosts: Vec<Overlay>, edit: EditFuture, cx: &mut Context<Self>) {
        if !self.can_edit(cx) {
            return;
        }
        let previous = self.pending.take().unwrap_or_default();
        self.pending = Some(PendingEdit {
            ghosts,
            after_revision: self.state.read(cx).compilation_revision,
            ..previous
        });
        self.edit_in_flight = true;
        cx.notify();
        self.edit_task = Some(cx.spawn(async move |this, cx| {
            let result = edit.await;
            let _ = this.update(cx, |canvas, cx| {
                canvas.finish_edit(result, &[], cx);
            });
        }));
    }

    fn send_signal(&mut self, request: AddSignalRequest, cx: &mut Context<Self>) {
        let ghost = Overlay::Signal {
            at: (request.at.0 as f64, request.at.1 as f64),
            name: request.name.clone(),
            port: request.port,
        };
        let client = self.state.read(cx).lang_server_client.clone();
        self.send_edit(
            vec![ghost],
            Box::pin(async move { client.add_signal(request).await }),
            cx,
        );
    }

    fn send_wire(&mut self, request: WireRequest, points: Vec<GridPt>, cx: &mut Context<Self>) {
        let ghost = Overlay::Wire(
            points
                .into_iter()
                .map(|(x, y)| (x as f64, y as f64))
                .collect(),
        );
        let client = self.state.read(cx).lang_server_client.clone();
        let edit: EditFuture = match request {
            WireRequest::Connect(request) => Box::pin(async move { client.connect(request).await }),
            WireRequest::Route(request) => Box::pin(async move { client.place(request).await }),
        };
        self.send_edit(vec![ghost], edit, cx);
    }

    /// Reports why `error` stops a wire, revealing the element without a
    /// path.
    fn report_end_error(&mut self, error: EndError, cx: &mut Context<Self>) {
        match error {
            EndError::NoPath { hit, name } => {
                self.select(vec![hit], cx);
                self.show_message(
                    MessageType::ERROR,
                    format!(
                        "`{name}` cannot be wired: nothing at the end of the cell body names it, because it was created in a loop or a function. Its source is shown in the editor."
                    ),
                    cx,
                );
            }
            EndError::NotAnEnd => {}
        }
    }

    /// Deletes the selection, or lists the references that stop it.
    fn delete_selection(&mut self, cx: &mut Context<Self>) {
        if !self.can_edit(cx) {
            return;
        }
        let Some(base_revision) = self.state.read(cx).compilation_revision else {
            return;
        };
        let mut targets = match (self.view(cx), &self.symbol_scene) {
            (View::Symbol, Some(scene)) => {
                symbol_tools::delete_targets(scene, &self.symbol_selection)
            }
            (View::Symbol, None) => Vec::new(),
            _ => self.selection_spans.clone(),
        };
        targets.dedup();
        if targets.is_empty() {
            return;
        }
        let client = self.state.read(cx).lang_server_client.clone();
        self.edit_in_flight = true;
        self.blocked_delete.clear();
        cx.notify();
        self.edit_task = Some(cx.spawn(async move |this, cx| {
            let result = client
                .delete(DeleteRequest {
                    base_revision,
                    targets,
                })
                .await;
            let _ = this.update(cx, |canvas, cx| {
                canvas.edit_in_flight = false;
                match result {
                    Ok(Some(DeleteOutcome::Applied(receipt))) => {
                        canvas.edit_accepted(receipt.revision, cx);
                        canvas.selection.clear();
                        canvas.selection_spans.clear();
                        canvas.symbol_selection.clear();
                        canvas.call_arguments = None;
                    }
                    Ok(Some(DeleteOutcome::Blocked(references))) => {
                        canvas.show_message(
                            MessageType::ERROR,
                            format!(
                                "Not deleted: {} other reference{} must go first; see the properties panel.",
                                references.len(),
                                if references.len() == 1 { "" } else { "s" }
                            ),
                            cx,
                        );
                        canvas.blocked_delete = references;
                    }
                    Ok(None) | Err(_) => {}
                }
                cx.notify();
            });
        }));
    }

    /// Fetches the source arguments of the primary selection's call.
    fn fetch_call_arguments(&mut self, cx: &mut Context<Self>) {
        let span = self.primary_span(cx);
        if self.call_arguments.as_ref().map(|(span, _)| span) == span.as_ref() {
            return;
        }
        self.call_arguments = None;
        let (Some(span), Some(revision)) = (span, self.state.read(cx).compilation_revision) else {
            self.call_arguments_task = None;
            return;
        };
        let client = self.state.read(cx).lang_server_client.clone();
        self.call_arguments_task = Some(cx.spawn(async move |this, cx| {
            let result = client.call_arguments(revision, span.clone()).await;
            let _ = this.update(cx, |canvas, cx| {
                if let Ok(Some(arguments)) = result
                    && canvas.primary_span(cx) == Some(span.clone())
                {
                    canvas.call_arguments = Some((span, arguments));
                    cx.notify();
                }
            });
        }));
    }

    /// Renames the primary selection's binding, with every reference to
    /// it.
    pub(crate) fn rename(&mut self, name: String, cx: &mut Context<Self>) {
        let (Some(site), Some(base_revision)) = (
            self.primary_span(cx),
            self.state.read(cx).compilation_revision,
        ) else {
            return;
        };
        let client = self.state.read(cx).lang_server_client.clone();
        let request = RenameRequest {
            base_revision,
            site,
            name,
        };
        self.call_arguments = None;
        self.send_edit(
            Vec::new(),
            Box::pin(async move { client.rename(request).await }),
            cx,
        );
    }

    /// The spans the selection copies, and the corner of its bounds.
    fn copy_targets(&self, cx: &App) -> Option<(Vec<Span>, Pt)> {
        if self.view(cx) == View::Symbol {
            let scene = self.symbol_scene.as_ref()?;
            let mut targets = Vec::new();
            let mut bounds = None;
            for selected in &self.symbol_selection {
                let SymbolSelection::Item(index) = selected else {
                    continue;
                };
                let item = scene.items.get(*index)?;
                if item.origin.is_some() {
                    continue;
                }
                if let Some(span) = item.span.clone() {
                    targets.push(span);
                    bounds = symbols::union(bounds, item.kind.bounds());
                }
            }
            let (x0, y0, _, _) = bounds?;
            Some((targets, (x0, y0)))
        } else {
            let mut bounds = None;
            for hit in &self.selection {
                bounds = symbols::union(bounds, self.hit_bounds(*hit));
            }
            let (x0, y0, _, _) = bounds?;
            let mut targets = self.selection_spans.clone();
            targets.dedup();
            Some((targets, (x0, y0)))
        }
    }

    /// Copies the selection's statements.
    pub(crate) fn copy(&mut self, _: &Copy, _window: &mut Window, cx: &mut Context<Self>) {
        self.note_view(cx);
        let (Some((targets, origin)), Some(base_revision)) = (
            self.copy_targets(cx),
            self.state.read(cx).compilation_revision,
        ) else {
            return;
        };
        if targets.is_empty() {
            return;
        }
        let client = self.state.read(cx).lang_server_client.clone();
        cx.spawn(async move |this, cx| {
            let result = client.copy_statements(base_revision, targets).await;
            let _ = this.update(cx, |canvas, cx| {
                if let Ok(Some(items)) = result {
                    canvas.clipboard = Some((items, origin));
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// Pastes the copied statements with their corner at the pointer.
    pub(crate) fn paste(&mut self, _: &Paste, _window: &mut Window, cx: &mut Context<Self>) {
        self.note_view(cx);
        let (Some((items, origin)), Some(target)) = (self.clipboard.clone(), self.target(cx))
        else {
            return;
        };
        let at = self.cursor_world.unwrap_or(origin);
        let (offset, symbol_offset) = if self.in_symbol_view {
            let (x, y) = symbol_tools::snap_quarter((at.0 - origin.0, at.1 - origin.1));
            ((0, 0), (x, y))
        } else {
            let (x, y) = symbol_tools::snap_grid((at.0 - origin.0, at.1 - origin.1));
            ((x, y), (0., 0.))
        };
        let client = self.state.read(cx).lang_server_client.clone();
        let request = PasteRequest {
            target,
            items,
            offset,
            symbol_offset,
        };
        self.send_edit(
            Vec::new(),
            Box::pin(async move { client.paste(request).await }),
            cx,
        );
    }

    /// Rewrites or adds an argument of the primary selection's call.
    pub(crate) fn set_argument(
        &mut self,
        argument: CallArgument,
        value: String,
        cx: &mut Context<Self>,
    ) {
        let (Some(call), Some(base_revision)) = (
            self.primary_span(cx),
            self.state.read(cx).compilation_revision,
        ) else {
            return;
        };
        let client = self.state.read(cx).lang_server_client.clone();
        let request = SetArgumentRequest {
            base_revision,
            call,
            argument,
            value,
        };
        self.call_arguments = None;
        self.send_edit(
            Vec::new(),
            Box::pin(async move { client.set_argument(request).await }),
            cx,
        );
    }

    /// Moves the primary selection to `position`, rewriting its wires.
    pub(crate) fn set_position(&mut self, position: GridPt, cx: &mut Context<Self>) {
        let Some(scene) = self.scene.clone() else {
            return;
        };
        let Some(hit) = self.selection.first().copied() else {
            return;
        };
        let current = match hit {
            Hit::Element(index) | Hit::Pin { element: index, .. } => scene
                .elements
                .get(index)
                .map(|e| e.placement.position.unwrap_or((0, 0))),
            Hit::Port(index) => scene.ports.get(index).map(|p| p.position),
            Hit::Label(index) => scene.labels.get(index).map(|l| l.position),
            Hit::Wire(_) | Hit::Flyline(_) => None,
        };
        let Some(current) = current else {
            return;
        };
        let delta = (position.0 - current.0, position.1 - current.1);
        let overrides = tools::move_overrides(&scene, &[hit], delta);
        self.commit(overrides, cx);
    }

    /// Rewrites the route of the primary selection's wire.
    pub(crate) fn set_route(&mut self, corners: Vec<GridPt>, cx: &mut Context<Self>) {
        let Some(scene) = self.scene.clone() else {
            return;
        };
        let Some(Hit::Wire(index)) = self.selection.first().copied() else {
            return;
        };
        let Some(wire) = scene.wires.get(index).filter(|wire| wire.editable) else {
            return;
        };
        let mut overrides = Overrides::default();
        overrides.routes.insert(wire.span.clone(), Some(corners));
        self.commit(overrides, cx);
    }

    /// The compiled data and the cell on screen.
    fn compiled(&self, cx: &App) -> Option<(Arc<CompiledData>, argonc::compile::CellId)> {
        let state = self.state.read(cx);
        let solved = state.solved_cell.read(cx);
        solved
            .as_ref()
            .map(|solved| (solved.output.clone(), solved.output.top))
    }

    fn prefs(&self, cx: &App) -> DisplayPrefs {
        DisplayPrefs {
            show_flylines: self.state.read(cx).show_flylines,
        }
    }

    /// The overrides the scene is drawn with: the drag in progress, or the
    /// pending edit.
    fn overrides(&self) -> Overrides {
        self.drag_overrides
            .clone()
            .or_else(|| {
                self.pending
                    .as_ref()
                    .map(|pending| pending.overrides.clone())
            })
            .unwrap_or_default()
    }

    fn set_overrides_changed(&mut self) {
        self.overrides_generation = self.overrides_generation.wrapping_add(1);
    }

    /// The symbol overrides the symbol scene is drawn with: the drag in
    /// progress, or the pending edit.
    fn symbol_overrides(&self) -> SymbolOverrides {
        self.symbol_drag_overrides
            .clone()
            .or_else(|| {
                self.pending
                    .as_ref()
                    .map(|pending| pending.symbol_overrides.clone())
            })
            .unwrap_or_default()
    }

    /// Rebuilds the scenes when the compiled data, the cell on screen, the
    /// display preferences, or the overrides changed, off the UI thread.
    pub(crate) fn refresh(&mut self, cx: &mut Context<Self>) {
        let revision = self.state.read(cx).compilation_revision;
        if let (Some(revision), Some(awaited)) = (revision, self.awaited_revision)
            && revision >= awaited
        {
            self.awaited_revision = None;
        }
        let Some((output, cell)) = self.compiled(cx) else {
            self.scene = None;
            self.scene_key = None;
            self.symbol_scene = None;
            self.symbol_scene_key = None;
            return;
        };
        // A snapshot compiled after the gesture retires its overlay.
        if let Some(pending) = &self.pending
            && let (Some(revision), Some(after)) = (revision, pending.after_revision)
            && revision > after
        {
            self.pending = None;
            self.set_overrides_changed();
        }
        let prefs = self.prefs(cx);
        let inst_name = self.state.read(cx).symbol_inst_name();
        let key = SceneKey {
            output: Arc::as_ptr(&output),
            cell,
            prefs,
            generation: self.overrides_generation,
        };
        if self.scene_key != Some(key) {
            let cell_changed = self.scene_key.is_none_or(|previous| previous.cell != cell);
            if cell_changed {
                self.selection.clear();
                self.selection_spans.clear();
                self.hover = None;
                self.highlighted_net = None;
                self.pending_fit = true;
            }
            self.scene_key = Some(key);
            let data = output.clone();
            let overrides = self.overrides();
            self.scene_task = Some(cx.spawn(async move |this, cx| {
                let scene = cx
                    .background_spawn(async move {
                        Arc::new(build_scene_with(&data, cell, &prefs, &overrides))
                    })
                    .await;
                let _ = this.update(cx, |canvas, cx| {
                    if canvas.scene_key == Some(key) {
                        canvas.scene = Some(scene);
                        canvas.retain_selection();
                        cx.notify();
                    }
                });
            }));
        }
        let symbol_key = SymbolSceneKey {
            output: Arc::as_ptr(&output),
            cell,
            inst_name: inst_name.clone(),
            generation: self.overrides_generation,
        };
        if self.symbol_scene_key.as_ref() != Some(&symbol_key) {
            if self
                .symbol_scene_key
                .as_ref()
                .is_none_or(|previous| previous.cell != cell)
            {
                self.symbol_selection.clear();
                self.pending_symbol_fit = true;
                self.symbol_tool = SymbolTool::Select;
            }
            self.symbol_scene_key = Some(symbol_key.clone());
            let overrides = self.symbol_overrides();
            self.symbol_scene_task = Some(cx.spawn(async move |this, cx| {
                let scene = cx
                    .background_spawn(async move {
                        Arc::new(build_symbol_scene_with(
                            &output, cell, &inst_name, &overrides,
                        ))
                    })
                    .await;
                let _ = this.update(cx, |canvas, cx| {
                    if canvas.symbol_scene_key.as_ref() == Some(&symbol_key) {
                        canvas.symbol_scene = Some(scene);
                        canvas.retain_symbol_selection();
                        cx.notify();
                    }
                });
            }));
        }
    }

    /// Rebuilds the symbol scene at once with the current overrides, for a
    /// drag preview.
    fn rebuild_symbol_now(&mut self, cx: &mut Context<Self>) {
        let Some((output, cell)) = self.compiled(cx) else {
            return;
        };
        self.set_overrides_changed();
        let inst_name = self.state.read(cx).symbol_inst_name();
        let overrides = self.symbol_overrides();
        self.symbol_scene_key = Some(SymbolSceneKey {
            output: Arc::as_ptr(&output),
            cell,
            inst_name: inst_name.clone(),
            generation: self.overrides_generation,
        });
        self.symbol_scene = Some(Arc::new(build_symbol_scene_with(
            &output, cell, &inst_name, &overrides,
        )));
        self.retain_symbol_selection();
        cx.notify();
    }

    /// Drops symbol selections that the rebuilt scene no longer has.
    fn retain_symbol_selection(&mut self) {
        let Some(scene) = &self.symbol_scene else {
            self.symbol_selection.clear();
            return;
        };
        self.symbol_selection.retain(|selected| match selected {
            SymbolSelection::Item(index) => *index < scene.items.len(),
            SymbolSelection::Pin(port) => scene.pins.iter().any(|pin| pin.port == *port),
        });
    }

    /// Fetches the parameters of the cell on screen, for the text tool,
    /// once per cell the symbol view shows.
    fn fetch_cell_parameters(&mut self, cx: &mut Context<Self>) {
        let cell = self.compiled(cx).map(|(_, cell)| cell);
        if cell.is_none() || self.cell_parameters_for == cell {
            return;
        }
        self.cell_parameters_for = cell;
        self.cell_parameters.clear();
        let Some(target) = self.target(cx) else {
            return;
        };
        let client = self.state.read(cx).lang_server_client.clone();
        self.cell_parameters_task = Some(cx.spawn(async move |this, cx| {
            let result = client
                .cell_parameters(target.base_revision, target.cell)
                .await;
            let _ = this.update(cx, |canvas, cx| {
                if let Ok(Some(parameters)) = result {
                    canvas.cell_parameters = parameters;
                    cx.notify();
                }
            });
        }));
    }

    pub(crate) fn cell_parameters(&self) -> &[String] {
        &self.cell_parameters
    }

    fn set_symbol_tool(&mut self, tool: SymbolTool, cx: &mut Context<Self>) {
        let kind = tool.kind();
        self.symbol_tool = tool;
        if self.state.read(cx).symbol_tool != kind {
            self.state.update(cx, |state, cx| {
                state.symbol_tool = kind;
                cx.notify();
            });
        }
        cx.notify();
    }

    /// Starts placing `text` with the text tool.
    pub(crate) fn place_symbol_text(&mut self, text: TextSpec, cx: &mut Context<Self>) {
        self.set_symbol_tool(SymbolTool::Text { text: Some(text) }, cx);
    }

    /// Picks `port` for the pin tool.
    pub(crate) fn pick_port(&mut self, port: usize, cx: &mut Context<Self>) {
        let placed = match &self.symbol_tool {
            SymbolTool::Pin { placed, .. } => placed.clone(),
            _ => Vec::new(),
        };
        self.set_symbol_tool(
            SymbolTool::Pin {
                port: Some(port),
                placed,
            },
            cx,
        );
    }

    /// The next port without a pin after `after`, skipping `placed`.
    fn next_missing_port(&self, after: Option<usize>, placed: &[usize]) -> Option<usize> {
        let scene = self.symbol_scene.as_ref()?;
        let missing = scene
            .missing_ports
            .iter()
            .copied()
            .filter(|port| !placed.contains(port))
            .collect::<Vec<_>>();
        let position = after.and_then(|after| missing.iter().position(|port| *port == after));
        match position {
            Some(index) => missing.get((index + 1) % missing.len()).copied(),
            None => missing.first().copied(),
        }
    }

    /// Writes the generated box as the cell's own symbol.
    pub(crate) fn create_symbol(&mut self, cx: &mut Context<Self>) {
        let Some(target) = self.target(cx) else {
            return;
        };
        if !self
            .symbol_scene
            .as_ref()
            .is_some_and(|scene| scene.generated)
        {
            return;
        }
        let client = self.state.read(cx).lang_server_client.clone();
        self.send_edit(
            Vec::new(),
            Box::pin(async move { client.create_symbol(CreateSymbolRequest { target }).await }),
            cx,
        );
    }

    /// Draws `items` in the cell on screen, showing `ghosts` until the
    /// compiled result arrives.
    fn add_symbol_items(
        &mut self,
        items: Vec<NewSymbolItem>,
        ghosts: Vec<Overlay>,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.target(cx) else {
            return;
        };
        let client = self.state.read(cx).lang_server_client.clone();
        self.send_edit(
            ghosts,
            Box::pin(async move {
                client
                    .add_symbol_items(AddSymbolItemsRequest { target, items })
                    .await
            }),
            cx,
        );
    }

    /// Sends symbol `overrides` to the analyzer as one edit, keeping them
    /// drawn until a snapshot compiled from the rewritten source arrives.
    fn commit_symbol(&mut self, overrides: SymbolOverrides, cx: &mut Context<Self>) {
        if overrides.is_empty() || !self.can_edit(cx) {
            return;
        }
        let edits = symbol_tools::symbol_edits(&overrides);
        let request = PlaceRequest {
            base_revision: self.state.read(cx).compilation_revision.unwrap_or_default(),
            edits,
        };
        let previous = self.pending.take().unwrap_or_default();
        self.pending = Some(PendingEdit {
            symbol_overrides: overrides,
            after_revision: self.state.read(cx).compilation_revision,
            ..previous
        });
        self.edit_in_flight = true;
        self.rebuild_symbol_now(cx);
        let client = self.state.read(cx).lang_server_client.clone();
        self.edit_task = Some(cx.spawn(async move |this, cx| {
            let result = client.place(request).await;
            let _ = this.update(cx, |canvas, cx| {
                canvas.finish_edit(result, &[], cx);
            });
        }));
    }

    /// The overlay of a new symbol item.
    fn item_overlay(kind: &SymbolItemKind, inst_name: &str) -> Overlay {
        let placed = place_symbol(
            &argonc::compile::Symbol {
                items: vec![argonc::compile::SymbolItem {
                    kind: kind.clone(),
                    site: None,
                }],
                pins: Vec::new(),
            },
            &Placement::default(),
            inst_name,
            &[],
        );
        match (
            placed.shapes.into_iter().next(),
            placed.texts.into_iter().next(),
        ) {
            (Some(shape), _) => Overlay::Shape(shape),
            (None, Some(text)) => Overlay::Text(text),
            (None, None) => Overlay::Wire(Vec::new()),
        }
    }

    /// What the symbol tool draws under the pointer.
    fn symbol_tool_preview(&self) -> Vec<Overlay> {
        let Some(cursor) = self.cursor_world else {
            return Vec::new();
        };
        let quarter = symbol_tools::snap_quarter(cursor);
        let mut overlays = Vec::new();
        match &self.symbol_tool {
            SymbolTool::Select => {}
            SymbolTool::Line { points } | SymbolTool::Polygon { points } => {
                if !points.is_empty() {
                    let mut preview = points.clone();
                    preview.push(quarter);
                    overlays.push(Overlay::Shape(Shape::Line(preview)));
                }
            }
            SymbolTool::Rect { start: Some(start) } => {
                overlays.push(Overlay::Shape(Shape::Polygon {
                    points: vec![*start, (quarter.0, start.1), quarter, (start.0, quarter.1)],
                    fill: false,
                }));
            }
            SymbolTool::Circle {
                center: Some(center),
            } => {
                let r = symbol_tools::snapped_radius(*center, quarter);
                overlays.push(Overlay::Shape(Shape::Circle {
                    center: *center,
                    r,
                    fill: false,
                }));
            }
            SymbolTool::Arc {
                center: Some(center),
                start,
            } => match start {
                Some(start) => {
                    let r = symbol_tools::snapped_radius(*center, *start);
                    overlays.push(Overlay::Shape(Shape::Arc {
                        center: *center,
                        r,
                        start: symbol_tools::angle_of(*center, *start),
                        end: symbol_tools::angle_of(*center, quarter),
                    }));
                }
                None => overlays.push(Overlay::Shape(Shape::Line(vec![*center, quarter]))),
            },
            SymbolTool::Text { text: Some(text) } => {
                let shown = match text {
                    TextSpec::Literal(text) => text.clone(),
                    TextSpec::InstName => "X?".to_owned(),
                    TextSpec::Param(name) => format!("{name}=?"),
                };
                overlays.push(Overlay::Text(place_text(
                    &Placement::default(),
                    shown,
                    quarter,
                    1.,
                    Align::Left,
                )));
            }
            SymbolTool::Pin {
                port: Some(port), ..
            } => {
                let name = self
                    .symbol_scene
                    .as_ref()
                    .and_then(|scene| {
                        scene
                            .missing_ports
                            .iter()
                            .position(|p| p == port)
                            .map(|index| scene.missing[index].clone())
                    })
                    .unwrap_or_default();
                let (x, y) = symbol_tools::snap_grid(cursor);
                overlays.push(Overlay::Pin {
                    at: (x as f64, y as f64),
                    name,
                });
            }
            SymbolTool::Rect { start: None }
            | SymbolTool::Circle { center: None }
            | SymbolTool::Arc { center: None, .. }
            | SymbolTool::Text { text: None }
            | SymbolTool::Pin { port: None, .. } => {}
        }
        overlays
    }

    /// The handles of the selected editable items.
    fn selection_handles(&self) -> Vec<Pt> {
        let Some(scene) = &self.symbol_scene else {
            return Vec::new();
        };
        let mut handles = Vec::new();
        for selected in &self.symbol_selection {
            if let SymbolSelection::Item(index) = selected
                && let Some(item) = scene.items.get(*index)
                && item.editable
            {
                handles.extend(
                    symbol_tools::handles(&item.kind)
                        .into_iter()
                        .map(|handle| handle.at),
                );
            }
        }
        handles
    }

    /// The handle of a selected editable item under `point`.
    fn handle_under(&self, point: Pt, tolerance: f64) -> Option<(usize, HandleKind)> {
        let scene = self.symbol_scene.as_ref()?;
        for selected in &self.symbol_selection {
            let SymbolSelection::Item(index) = selected else {
                continue;
            };
            let Some(item) = scene.items.get(*index).filter(|item| item.editable) else {
                continue;
            };
            for handle in symbol_tools::handles(&item.kind) {
                if (handle.at.0 - point.0).abs() <= tolerance
                    && (handle.at.1 - point.1).abs() <= tolerance
                {
                    return Some((*index, handle.kind));
                }
            }
        }
        None
    }

    /// A click of a symbol tool other than Select, at the world point
    /// `point`.
    fn symbol_tool_click(&mut self, point: Pt, click_count: usize, cx: &mut Context<Self>) {
        let quarter = symbol_tools::snap_quarter(point);
        let inst_name = self.state.read(cx).symbol_inst_name();
        match self.symbol_tool.clone() {
            SymbolTool::Select => {}
            SymbolTool::Line { mut points } | SymbolTool::Polygon { mut points } => {
                let polygon = matches!(self.symbol_tool, SymbolTool::Polygon { .. });
                if points.last() != Some(&quarter) {
                    points.push(quarter);
                }
                // A double-click ends the shape at its point.
                if click_count >= 2 {
                    self.finish_polyline(points, polygon, cx);
                    return;
                }
                self.symbol_tool = if polygon {
                    SymbolTool::Polygon { points }
                } else {
                    SymbolTool::Line { points }
                };
                cx.notify();
            }
            SymbolTool::Rect { start: None } => {
                self.symbol_tool = SymbolTool::Rect {
                    start: Some(quarter),
                };
                cx.notify();
            }
            SymbolTool::Rect { start: Some(start) } => {
                if start.0 == quarter.0 || start.1 == quarter.1 {
                    return;
                }
                let kind = SymbolItemKind::Rect {
                    x0: start.0.min(quarter.0),
                    y0: start.1.min(quarter.1),
                    x1: start.0.max(quarter.0),
                    y1: start.1.max(quarter.1),
                    fill: false,
                };
                let ghost = Self::item_overlay(&kind, &inst_name);
                self.add_symbol_items(
                    vec![NewSymbolItem::Rect {
                        x0: start.0.min(quarter.0),
                        y0: start.1.min(quarter.1),
                        x1: start.0.max(quarter.0),
                        y1: start.1.max(quarter.1),
                        fill: false,
                    }],
                    vec![ghost],
                    cx,
                );
                self.set_symbol_tool(SymbolTool::Rect { start: None }, cx);
            }
            SymbolTool::Circle { center: None } => {
                self.symbol_tool = SymbolTool::Circle {
                    center: Some(quarter),
                };
                cx.notify();
            }
            SymbolTool::Circle {
                center: Some(center),
            } => {
                let r = symbol_tools::snapped_radius(center, quarter);
                if r <= 0. {
                    return;
                }
                let kind = SymbolItemKind::Circle {
                    x: center.0,
                    y: center.1,
                    r,
                    fill: false,
                };
                let ghost = Self::item_overlay(&kind, &inst_name);
                self.add_symbol_items(
                    vec![NewSymbolItem::Circle {
                        x: center.0,
                        y: center.1,
                        r,
                        fill: false,
                    }],
                    vec![ghost],
                    cx,
                );
                self.set_symbol_tool(SymbolTool::Circle { center: None }, cx);
            }
            SymbolTool::Arc { center: None, .. } => {
                self.symbol_tool = SymbolTool::Arc {
                    center: Some(quarter),
                    start: None,
                };
                cx.notify();
            }
            SymbolTool::Arc {
                center: Some(center),
                start: None,
            } => {
                if symbol_tools::snapped_radius(center, quarter) <= 0. {
                    return;
                }
                self.symbol_tool = SymbolTool::Arc {
                    center: Some(center),
                    start: Some(quarter),
                };
                cx.notify();
            }
            SymbolTool::Arc {
                center: Some(center),
                start: Some(start),
            } => {
                let r = symbol_tools::snapped_radius(center, start);
                let kind = SymbolItemKind::Arc {
                    x: center.0,
                    y: center.1,
                    r,
                    start: symbol_tools::angle_of(center, start),
                    end: symbol_tools::angle_of(center, quarter),
                };
                let ghost = Self::item_overlay(&kind, &inst_name);
                self.add_symbol_items(
                    vec![NewSymbolItem::Arc {
                        x: center.0,
                        y: center.1,
                        r,
                        start: symbol_tools::angle_of(center, start),
                        end: symbol_tools::angle_of(center, quarter),
                    }],
                    vec![ghost],
                    cx,
                );
                self.set_symbol_tool(
                    SymbolTool::Arc {
                        center: None,
                        start: None,
                    },
                    cx,
                );
            }
            SymbolTool::Text { text: Some(text) } => {
                let (item, kind) = match text {
                    TextSpec::Literal(text) => (
                        NewSymbolItem::Text {
                            text: text.clone(),
                            x: quarter.0,
                            y: quarter.1,
                            size: 1.,
                            align: Align::Left,
                        },
                        SymbolItemKind::Text {
                            text: SymbolText::Literal(text),
                            x: quarter.0,
                            y: quarter.1,
                            size: 1.,
                            align: Align::Left,
                        },
                    ),
                    TextSpec::InstName => (
                        NewSymbolItem::InstName {
                            x: quarter.0,
                            y: quarter.1,
                            size: 1.,
                            align: Align::Left,
                        },
                        SymbolItemKind::Text {
                            text: SymbolText::InstName,
                            x: quarter.0,
                            y: quarter.1,
                            size: 1.,
                            align: Align::Left,
                        },
                    ),
                    TextSpec::Param(name) => (
                        NewSymbolItem::Param {
                            name: name.clone(),
                            x: quarter.0,
                            y: quarter.1,
                            size: 1.,
                            align: Align::Left,
                        },
                        SymbolItemKind::Text {
                            text: SymbolText::Literal(format!("{name}=?")),
                            x: quarter.0,
                            y: quarter.1,
                            size: 1.,
                            align: Align::Left,
                        },
                    ),
                };
                let ghost = Self::item_overlay(&kind, &inst_name);
                self.add_symbol_items(vec![item], vec![ghost], cx);
                self.set_symbol_tool(SymbolTool::Select, cx);
            }
            SymbolTool::Pin {
                port: Some(port),
                mut placed,
            } => {
                let (x, y) = symbol_tools::snap_grid(point);
                let name = self
                    .symbol_scene
                    .as_ref()
                    .and_then(|scene| {
                        scene
                            .missing_ports
                            .iter()
                            .position(|p| *p == port)
                            .map(|index| scene.missing[index].clone())
                    })
                    .unwrap_or_default();
                self.add_symbol_items(
                    vec![NewSymbolItem::Pin {
                        port,
                        x,
                        y,
                        label: true,
                    }],
                    vec![Overlay::Pin {
                        at: (x as f64, y as f64),
                        name,
                    }],
                    cx,
                );
                placed.push(port);
                match self.next_missing_port(Some(port), &placed) {
                    Some(next) => self.set_symbol_tool(
                        SymbolTool::Pin {
                            port: Some(next),
                            placed,
                        },
                        cx,
                    ),
                    None => self.set_symbol_tool(SymbolTool::Select, cx),
                }
            }
            SymbolTool::Text { text: None } | SymbolTool::Pin { port: None, .. } => {}
        }
    }

    /// Writes the line or polygon being drawn, when it has enough points.
    fn finish_polyline(&mut self, points: Vec<Pt>, polygon: bool, cx: &mut Context<Self>) {
        let needed = if polygon { 3 } else { 2 };
        if points.len() < needed {
            self.show_message(
                MessageType::WARNING,
                if polygon {
                    "A polygon needs at least three points."
                } else {
                    "A line needs at least two points."
                },
                cx,
            );
            return;
        }
        let inst_name = self.state.read(cx).symbol_inst_name();
        let (item, kind) = if polygon {
            (
                NewSymbolItem::Polygon {
                    points: points.clone(),
                    fill: false,
                },
                SymbolItemKind::Polygon {
                    points,
                    fill: false,
                },
            )
        } else {
            (
                NewSymbolItem::Line {
                    points: points.clone(),
                },
                SymbolItemKind::Line { points },
            )
        };
        let ghost = Self::item_overlay(&kind, &inst_name);
        self.add_symbol_items(vec![item], vec![ghost], cx);
        self.set_symbol_tool(
            if polygon {
                SymbolTool::Polygon { points: Vec::new() }
            } else {
                SymbolTool::Line { points: Vec::new() }
            },
            cx,
        );
    }

    /// A click of the symbol view's select tool.
    fn symbol_select_click(&mut self, event: &MouseDownEvent, cx: &mut Context<Self>) {
        let point = self.world_at(event.position, cx);
        let tolerance = (HIT_TOLERANCE / self.current_viewport(cx).scale.max(1e-3)) as f64;
        let Some(scene) = self.symbol_scene.clone() else {
            return;
        };
        if !self.edit_waiting()
            && let Some((item, handle)) = self.handle_under(point, tolerance)
        {
            self.symbol_drag = Some(SymbolDrag {
                kind: SymbolDragKind::Handle { item, handle },
                start: point,
                current: point,
                base: scene,
                selection: self.symbol_selection.clone(),
                click: None,
            });
            return;
        }
        let hits = symbol_tools::hits_at(&scene, point, tolerance);
        let primary = self.symbol_selection.first().copied();
        let next = match (primary, hits.len()) {
            (_, 0) => None,
            (Some(current), _) if event.click_count == 1 && !event.modifiers.shift => hits
                .iter()
                .position(|hit| *hit == current)
                .map(|index| hits[(index + 1) % hits.len()])
                .or(Some(hits[0])),
            _ => Some(hits[0]),
        };
        let selection = match next {
            None if event.modifiers.shift => self.symbol_selection.clone(),
            None => Vec::new(),
            Some(hit) if event.modifiers.shift => {
                let mut selection = self.symbol_selection.clone();
                if let Some(index) = selection.iter().position(|s| *s == hit) {
                    selection.remove(index);
                } else {
                    selection.push(hit);
                }
                selection
            }
            Some(hit) if self.symbol_selection.contains(&hit) => self.symbol_selection.clone(),
            Some(hit) => vec![hit],
        };
        let editable = |hit: SymbolSelection| match hit {
            SymbolSelection::Item(index) => scene.items.get(index).is_some_and(|i| i.editable),
            SymbolSelection::Pin(port) => scene
                .pins
                .iter()
                .find(|pin| pin.port == port)
                .is_some_and(|pin| pin.editable),
        };
        // A press on the primary selection keeps it, so it can be dragged;
        // a release without a drag then cycles to what overlaps it.
        let grabbed = !self.edit_waiting()
            && primary.is_some_and(|current| {
                event.click_count == 1
                    && !event.modifiers.shift
                    && hits.contains(&current)
                    && editable(current)
            });
        let click = if grabbed {
            Some(selection)
        } else {
            self.select_symbol(selection, cx);
            None
        };
        if (grabbed || next.is_some_and(editable)) && !self.edit_waiting() {
            self.symbol_drag = Some(SymbolDrag {
                kind: SymbolDragKind::Move,
                start: point,
                current: point,
                base: scene,
                selection: self.symbol_selection.clone(),
                click,
            });
        }
    }

    /// Rebuilds the scene at once with the current overrides, for a drag
    /// preview that follows the pointer.
    fn rebuild_now(&mut self, cx: &mut Context<Self>) {
        let Some((output, cell)) = self.compiled(cx) else {
            return;
        };
        self.set_overrides_changed();
        let prefs = self.prefs(cx);
        let overrides = self.overrides();
        self.scene_key = Some(SceneKey {
            output: Arc::as_ptr(&output),
            cell,
            prefs,
            generation: self.overrides_generation,
        });
        self.scene = Some(Arc::new(build_scene_with(
            &output, cell, &prefs, &overrides,
        )));
        self.retain_selection();
        cx.notify();
    }

    /// The hit in `scene` whose call is `span`.
    fn hit_for_span(scene: &Scene, span: &Span) -> Option<Hit> {
        scene
            .elements
            .iter()
            .position(|element| element.span.as_ref() == Some(span))
            .map(Hit::Element)
            .or_else(|| {
                scene
                    .ports
                    .iter()
                    .position(|port| port.span == *span)
                    .map(Hit::Port)
            })
            .or_else(|| {
                scene
                    .labels
                    .iter()
                    .position(|label| label.span == *span)
                    .map(Hit::Label)
            })
            .or_else(|| {
                scene
                    .wires
                    .iter()
                    .position(|wire| wire.span == *span)
                    .map(Hit::Wire)
            })
    }

    /// The call of `hit`, if it has one.
    fn span_of_hit(scene: &Scene, hit: Hit) -> Option<Span> {
        match hit {
            Hit::Element(index) | Hit::Pin { element: index, .. } => {
                scene.elements.get(index).and_then(|e| e.span.clone())
            }
            Hit::Port(index) => scene.ports.get(index).map(|p| p.span.clone()),
            Hit::Label(index) => scene.labels.get(index).map(|l| l.span.clone()),
            Hit::Wire(index) => scene.wires.get(index).map(|w| w.span.clone()),
            Hit::Flyline(_) => None,
        }
    }

    /// Re-resolves the selection against the current scene by its spans.
    fn retain_selection(&mut self) {
        let Some(scene) = &self.scene else {
            self.selection.clear();
            return;
        };
        self.selection = self
            .selection_spans
            .iter()
            .filter_map(|span| Self::hit_for_span(scene, span))
            .collect();
        if self
            .highlighted_net
            .is_some_and(|net| net as usize >= scene.nets.len())
        {
            self.highlighted_net = None;
        }
    }

    fn view(&self, cx: &App) -> View {
        self.state.read(cx).view
    }

    /// Records which view the canvas shows, for what has no context.
    fn note_view(&mut self, cx: &App) {
        self.in_symbol_view = self.view(cx) == View::Symbol;
    }

    fn viewport_mut(&mut self, view: View) -> &mut Viewport {
        match view {
            View::Symbol => &mut self.symbol_viewport,
            _ => &mut self.viewport,
        }
    }

    fn current_viewport(&self, cx: &App) -> Viewport {
        match self.view(cx) {
            View::Symbol => self.symbol_viewport,
            _ => self.viewport,
        }
    }

    pub(crate) fn scene(&self) -> Option<&Arc<Scene>> {
        self.scene.as_ref()
    }

    pub(crate) fn symbol_scene(&self) -> Option<&Arc<SymbolScene>> {
        self.symbol_scene.as_ref()
    }

    pub(crate) fn selection(&self) -> &[Hit] {
        &self.selection
    }

    pub(crate) fn symbol_selection(&self) -> &[SymbolSelection] {
        &self.symbol_selection
    }

    /// The span of the primary selection in the current view.
    fn primary_span(&self, cx: &App) -> Option<Span> {
        match self.view(cx) {
            View::Symbol => {
                let scene = self.symbol_scene.as_ref()?;
                match self.symbol_selection.first()? {
                    SymbolSelection::Item(index) => scene.items.get(*index)?.span.clone(),
                    SymbolSelection::Pin(port) => scene
                        .pins
                        .iter()
                        .find(|pin| pin.port == *port)?
                        .span
                        .clone(),
                }
            }
            _ => self.selection_spans.first().cloned(),
        }
    }

    pub(crate) fn highlighted_net(&self) -> Option<NetIdx> {
        self.highlighted_net
    }

    /// Whether an edit the analyzer accepted is still drawn as an overlay.
    #[cfg(test)]
    pub(crate) fn has_pending_edit(&self) -> bool {
        self.pending.is_some()
    }

    #[cfg(test)]
    pub(crate) fn edit_in_flight(&self) -> bool {
        self.edit_in_flight
    }

    /// The world bounds the current view shows when fit.
    fn content_bounds(&self, cx: &App) -> Option<WorldBounds> {
        match self.view(cx) {
            View::Symbol => self.symbol_scene.as_ref().and_then(|scene| scene.bounds),
            _ => self.scene.as_ref().and_then(|scene| scene.bounds),
        }
    }

    /// Called by the element before painting: records the canvas bounds and
    /// fits a freshly built scene.
    pub(crate) fn before_paint(&mut self, bounds: Bounds<Pixels>, cx: &mut Context<Self>) {
        self.screen_bounds = bounds;
        let view = self.view(cx);
        self.in_symbol_view = view == View::Symbol;
        if self.in_symbol_view {
            self.fetch_cell_parameters(cx);
        }
        let pending = match view {
            View::Symbol => self.pending_symbol_fit && self.symbol_scene.is_some(),
            _ => self.pending_fit && self.scene.is_some(),
        };
        if pending {
            match view {
                View::Symbol => self.pending_symbol_fit = false,
                _ => self.pending_fit = false,
            }
            self.fit_to_screen(cx);
        }
    }

    /// What the element paints this frame.
    pub(crate) fn frame(&self, cx: &App) -> Frame {
        let state = self.state.read(cx);
        let content = match state.view {
            View::Symbol => match &self.symbol_scene {
                Some(scene) => FrameContent::Symbol(scene.clone()),
                None => FrameContent::Nothing,
            },
            _ => match &self.scene {
                Some(scene) => FrameContent::Schematic(scene.clone()),
                None => FrameContent::Nothing,
            },
        };
        Frame {
            theme: state.theme(),
            viewport: self.current_viewport(cx),
            content,
            selection: self.selection.clone(),
            hover: self.hover,
            highlighted_net: self.highlighted_net,
            symbol_selection: self.symbol_selection.clone(),
            pending: self.pending.is_some()
                || self.awaited_revision.is_some()
                || self.drag_overrides.is_some()
                || self.symbol_drag_overrides.is_some(),
            overlays: {
                let mut overlays = self
                    .pending
                    .as_ref()
                    .map(|pending| pending.ghosts.clone())
                    .unwrap_or_default();
                if state.view == View::Symbol {
                    overlays.extend(self.symbol_tool_preview());
                } else {
                    overlays.extend(self.tool_preview());
                }
                overlays
            },
            handles: if state.view == View::Symbol {
                self.selection_handles()
            } else {
                Vec::new()
            },
        }
    }

    /// Fits the current view's content. An empty view keeps its camera.
    pub(crate) fn fit_to_screen(&mut self, cx: &mut Context<Self>) {
        let Some(bounds) = self.content_bounds(cx) else {
            return;
        };
        let size = self.screen_bounds.size;
        if size.width <= px(0.) || size.height <= px(0.) {
            return;
        }
        let view = self.view(cx);
        let (x0, y0, x1, y1) = bounds;
        // A margin in grid units keeps pin labels and the origin in view.
        let world = (
            (x0 - 2.) as f32,
            (y0 - 2.) as f32,
            (x1 + 2.) as f32,
            (y1 + 2.) as f32,
        );
        self.viewport_mut(view).fit(size, world, FIT_MARGIN);
        cx.notify();
    }

    /// Centers the view on `bounds`, zooming in only as far as needed to
    /// show it.
    pub(crate) fn zoom_to(&mut self, bounds: WorldBounds, cx: &mut Context<Self>) {
        let size = self.screen_bounds.size;
        if size.width <= px(0.) || size.height <= px(0.) {
            return;
        }
        let view = self.view(cx);
        let scale = self.viewport_mut(view).scale;
        let (x0, y0, x1, y1) = bounds;
        let viewport = self.viewport_mut(view);
        viewport.fit(
            size,
            (
                (x0 - 2.) as f32,
                (y0 - 2.) as f32,
                (x1 + 2.) as f32,
                (y1 + 2.) as f32,
            ),
            FIT_MARGIN,
        );
        if viewport.scale > scale {
            // Keep the previous zoom, centered on the target.
            let center = ((x0 + x1) as f32 / 2., (y0 + y1) as f32 / 2.);
            viewport.scale = scale;
            viewport.offset = Point::new(
                px(f32::from(size.width) / 2. - center.0 * scale),
                px(f32::from(size.height) / 2. + center.1 * scale),
            );
        }
        cx.notify();
    }

    /// The world bounds of `hit`.
    fn hit_bounds(&self, hit: Hit) -> Option<WorldBounds> {
        let scene = self.scene.as_ref()?;
        let point = |(x, y): (i64, i64)| (x as f64, y as f64, x as f64, y as f64);
        Some(match hit {
            Hit::Element(index) => scene.elements.get(index)?.bounds,
            Hit::Pin { element, terminal } => {
                point(scene.elements.get(element)?.pins.get(terminal)?.position?)
            }
            Hit::Port(index) => point(scene.ports.get(index)?.position),
            Hit::Label(index) => point(scene.labels.get(index)?.position),
            Hit::Wire(index) => {
                let mut bounds = None;
                for point in &scene.wires.get(index)?.points {
                    symbols::include(&mut bounds, *point);
                }
                bounds?
            }
            Hit::Flyline(index) => {
                let flyline = scene.flylines.get(index)?;
                let mut bounds = None;
                symbols::include(&mut bounds, flyline.from);
                symbols::include(&mut bounds, flyline.to);
                bounds?
            }
        })
    }

    /// Selects `hits`, revealing the first one's source in the editor.
    pub(crate) fn select(&mut self, hits: Vec<Hit>, cx: &mut Context<Self>) {
        self.selection = hits;
        self.selection_spans = self
            .scene
            .as_ref()
            .map(|scene| {
                self.selection
                    .iter()
                    .filter_map(|hit| Self::span_of_hit(scene, *hit))
                    .collect()
            })
            .unwrap_or_default();
        if let Some(span) = self.selection_spans.first() {
            let _ = self
                .state
                .read(cx)
                .lang_server_client
                .select_span(span.clone());
        }
        self.blocked_delete.clear();
        self.fetch_call_arguments(cx);
        cx.notify();
    }

    /// Selects `hit` and brings it into view.
    pub(crate) fn select_and_zoom(&mut self, hit: Hit, cx: &mut Context<Self>) {
        self.select(vec![hit], cx);
        if let Some(bounds) = self.hit_bounds(hit) {
            self.zoom_to(bounds, cx);
        }
    }

    pub(crate) fn highlight_net(&mut self, net: Option<NetIdx>, cx: &mut Context<Self>) {
        self.highlighted_net = net;
        cx.notify();
    }

    /// Selects `selection` in the symbol view, with every group completed,
    /// revealing the first item's source.
    pub(crate) fn select_symbol(
        &mut self,
        selection: Vec<SymbolSelection>,
        cx: &mut Context<Self>,
    ) {
        self.symbol_selection = match &self.symbol_scene {
            Some(scene) => symbol_tools::expand_groups(scene, selection),
            None => selection,
        };
        if let Some(span) = self.primary_span(cx) {
            let _ = self.state.read(cx).lang_server_client.select_span(span);
        }
        self.blocked_delete.clear();
        self.fetch_call_arguments(cx);
        cx.notify();
    }

    /// Selects the pin of `port` and brings it into view.
    pub(crate) fn select_pin_and_zoom(&mut self, port: usize, cx: &mut Context<Self>) {
        self.select_symbol(vec![SymbolSelection::Pin(port)], cx);
        if let Some(scene) = &self.symbol_scene
            && let Some(pin) = scene.pins.iter().find(|pin| pin.port == port)
        {
            let (x, y) = (pin.position.0 as f64, pin.position.1 as f64);
            self.zoom_to((x, y, x, y), cx);
        }
    }

    /// The instance the primary selection names, if it is one.
    fn selected_instance(&self) -> Option<String> {
        let scene = self.scene.as_ref()?;
        let index = match self.selection.first()? {
            Hit::Element(index) | Hit::Pin { element: index, .. } => *index,
            _ => return None,
        };
        let element = scene.elements.get(index)?;
        matches!(element.element, Element::Instance(_)).then(|| element.name.clone())
    }

    fn world_at(&self, position: Point<Pixels>, cx: &App) -> (f64, f64) {
        let (x, y) = self
            .current_viewport(cx)
            .from_px(self.screen_bounds, position);
        (x as f64, y as f64)
    }

    fn grid_at(&self, position: Point<Pixels>, cx: &App) -> (i64, i64) {
        let (x, y) = self.world_at(position, cx);
        (x.round() as i64, y.round() as i64)
    }

    fn hits_under(&self, position: Point<Pixels>, cx: &App) -> Vec<Hit> {
        let Some(scene) = &self.scene else {
            return Vec::new();
        };
        let tolerance = (HIT_TOLERANCE / self.current_viewport(cx).scale.max(1e-3)) as f64;
        hits_at(scene, self.world_at(position, cx), tolerance)
    }

    /// Whether `hit` can be dragged.
    fn draggable(&self, hit: Hit) -> bool {
        let Some(scene) = &self.scene else {
            return false;
        };
        match hit {
            Hit::Element(index) | Hit::Pin { element: index, .. } => scene
                .elements
                .get(index)
                .is_some_and(|element| element.editable && element.span.is_some()),
            Hit::Port(index) => scene.ports.get(index).is_some_and(|port| port.editable),
            Hit::Label(index) => scene.labels.get(index).is_some_and(|label| label.editable),
            Hit::Wire(index) => scene.wires.get(index).is_some_and(|wire| wire.editable),
            Hit::Flyline(_) => false,
        }
    }

    /// The drag a press on `hit` at `point` starts, if `hit` can be dragged.
    fn drag_kind(&self, hit: Hit, point: (f64, f64)) -> Option<DragKind> {
        if !self.draggable(hit) {
            return None;
        }
        match hit {
            Hit::Wire(wire) => self
                .nearest_segment(wire, point)
                .map(|segment| DragKind::Segment { wire, segment }),
            _ => Some(DragKind::Items),
        }
    }

    /// The segment of wire `wire` nearest `point`.
    fn nearest_segment(&self, wire: usize, point: (f64, f64)) -> Option<usize> {
        let scene = self.scene.as_ref()?;
        let wire = scene.wires.get(wire)?;
        wire.points
            .windows(2)
            .enumerate()
            .map(|(index, pair)| (index, wires::segment_distance(point, pair[0], pair[1])))
            .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
            .map(|(index, _)| index)
    }

    /// Sends `overrides` to the analyzer as one edit, keeping them drawn
    /// until a snapshot compiled from the rewritten source arrives, or
    /// dropping them if the edit is refused.
    fn commit(&mut self, overrides: Overrides, cx: &mut Context<Self>) {
        let Some(scene) = self.scene.clone() else {
            return;
        };
        if overrides.is_empty() || !self.can_edit(cx) {
            return;
        }
        let edits = tools::placement_edits(&scene, &overrides);
        let request = PlaceRequest {
            base_revision: self.state.read(cx).compilation_revision.unwrap_or_default(),
            edits,
        };
        let edited_spans = request
            .edits
            .iter()
            .map(|edit| edit.call.clone())
            .collect::<Vec<_>>();
        let previous = self.pending.take().unwrap_or_default();
        self.pending = Some(PendingEdit {
            overrides,
            after_revision: self.state.read(cx).compilation_revision,
            ..previous
        });
        self.edit_in_flight = true;
        self.rebuild_now(cx);
        let client = self.state.read(cx).lang_server_client.clone();
        self.edit_task = Some(cx.spawn(async move |this, cx| {
            let result = client.place(request).await;
            let _ = this.update(cx, |canvas, cx| {
                canvas.finish_edit(result, &edited_spans, cx);
            });
        }));
    }

    fn finish_edit(
        &mut self,
        result: anyhow::Result<Option<EditReceipt>>,
        edited_spans: &[Span],
        cx: &mut Context<Self>,
    ) {
        self.edit_in_flight = false;
        match result {
            Ok(Some(receipt)) => {
                self.edit_accepted(receipt.revision, cx);
                // The selection follows its calls to where the edit put them.
                for span in &mut self.selection_spans {
                    if let Some(index) = edited_spans.iter().position(|edited| edited == span)
                        && let Some(moved) = receipt.spans.get(index)
                    {
                        *span = moved.clone();
                    }
                }
            }
            Ok(None) | Err(_) => {
                self.pending = None;
                self.rebuild_now(cx);
            }
        }
        cx.notify();
    }

    // Actions.

    pub(crate) fn fit_action(&mut self, _: &Fit, _window: &mut Window, cx: &mut Context<Self>) {
        self.fit_to_screen(cx);
    }

    fn pan(&mut self, delta: Point<Pixels>, cx: &mut Context<Self>) {
        let view = self.view(cx);
        self.viewport_mut(view).offset += delta;
        self.hover = None;
        cx.notify();
    }

    pub(crate) fn pan_left(&mut self, _: &PanLeft, _window: &mut Window, cx: &mut Context<Self>) {
        self.pan(Point::new(KEYBOARD_PAN_STEP, px(0.)), cx);
    }

    pub(crate) fn pan_right(&mut self, _: &PanRight, _window: &mut Window, cx: &mut Context<Self>) {
        self.pan(Point::new(-KEYBOARD_PAN_STEP, px(0.)), cx);
    }

    pub(crate) fn pan_up(&mut self, _: &PanUp, _window: &mut Window, cx: &mut Context<Self>) {
        self.pan(Point::new(px(0.), KEYBOARD_PAN_STEP), cx);
    }

    pub(crate) fn pan_down(&mut self, _: &PanDown, _window: &mut Window, cx: &mut Context<Self>) {
        self.pan(Point::new(px(0.), -KEYBOARD_PAN_STEP), cx);
    }

    fn zoom(&mut self, factor: f32, position: Point<Pixels>, cx: &mut Context<Self>) {
        let view = self.view(cx);
        let bounds = self.screen_bounds;
        let viewport = self.viewport_mut(view);
        let scale = (viewport.scale * factor).clamp(1e-3, 1e4);
        viewport.zoom_about(bounds, position, scale);
        cx.notify();
    }

    pub(crate) fn zoom_in(&mut self, _: &ZoomIn, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom(KEYBOARD_ZOOM_FACTOR, self.screen_bounds.center(), cx);
    }

    pub(crate) fn zoom_out(&mut self, _: &ZoomOut, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom(
            KEYBOARD_ZOOM_FACTOR.recip(),
            self.screen_bounds.center(),
            cx,
        );
    }

    pub(crate) fn cancel(&mut self, _: &Cancel, _window: &mut Window, cx: &mut Context<Self>) {
        self.note_view(cx);
        if self.drag.take().is_some() {
            self.drag_overrides = None;
            self.rebuild_now(cx);
            return;
        }
        if self.symbol_drag.take().is_some() {
            self.symbol_drag_overrides = None;
            self.rebuild_symbol_now(cx);
            return;
        }
        if self.label_prompt.take().is_some() {
            cx.notify();
            return;
        }
        if self.in_symbol_view {
            match &mut self.symbol_tool {
                SymbolTool::Select => {
                    self.symbol_selection.clear();
                    self.blocked_delete.clear();
                    self.call_arguments = None;
                    cx.notify();
                }
                SymbolTool::Line { points } | SymbolTool::Polygon { points }
                    if !points.is_empty() =>
                {
                    points.clear();
                    cx.notify();
                }
                _ => self.set_symbol_tool(SymbolTool::Select, cx),
            }
            return;
        }
        if let Tool::Wire {
            draft: draft @ Some(_),
            ..
        } = &mut self.tool
        {
            *draft = None;
            cx.notify();
            return;
        }
        if !matches!(self.tool, Tool::Select) {
            self.set_tool(Tool::Select, cx);
            return;
        }
        self.selection.clear();
        self.selection_spans.clear();
        self.symbol_selection.clear();
        self.highlighted_net = None;
        self.blocked_delete.clear();
        self.call_arguments = None;
        cx.notify();
    }

    pub(crate) fn select_mode(
        &mut self,
        _: &SelectMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.note_view(cx);
        if self.in_symbol_view {
            self.set_symbol_tool(SymbolTool::Select, cx);
        } else {
            self.set_tool(Tool::Select, cx);
        }
    }

    /// Starts a symbol drawing tool, unless the symbol is generated.
    fn start_symbol_tool(&mut self, tool: SymbolTool, cx: &mut Context<Self>) {
        self.note_view(cx);
        if !self.in_symbol_view {
            return;
        }
        if self
            .symbol_scene
            .as_ref()
            .is_some_and(|scene| scene.generated)
        {
            self.show_message(
                MessageType::WARNING,
                "The symbol is generated; use Create Symbol before drawing on it.",
                cx,
            );
            return;
        }
        self.set_symbol_tool(tool, cx);
    }

    pub(crate) fn symbol_line(
        &mut self,
        _: &SymbolLine,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_symbol_tool(SymbolTool::Line { points: Vec::new() }, cx);
    }

    pub(crate) fn symbol_polygon(
        &mut self,
        _: &DrawPolygon,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_symbol_tool(SymbolTool::Polygon { points: Vec::new() }, cx);
    }

    pub(crate) fn symbol_rect(
        &mut self,
        _: &DrawRect,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_symbol_tool(SymbolTool::Rect { start: None }, cx);
    }

    pub(crate) fn symbol_circle(
        &mut self,
        _: &SymbolCircle,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_symbol_tool(SymbolTool::Circle { center: None }, cx);
    }

    pub(crate) fn symbol_arc(
        &mut self,
        _: &SymbolArc,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_symbol_tool(
            SymbolTool::Arc {
                center: None,
                start: None,
            },
            cx,
        );
    }

    pub(crate) fn symbol_text(
        &mut self,
        _: &SymbolTextAction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.start_symbol_tool(SymbolTool::Text { text: None }, cx);
        if matches!(self.symbol_tool, SymbolTool::Text { text: None }) {
            self.prompt_name("Text…", window, cx);
        }
    }

    pub(crate) fn symbol_pin(
        &mut self,
        _: &SymbolPin,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let port = self.next_missing_port(None, &[]);
        self.start_symbol_tool(
            SymbolTool::Pin {
                port,
                placed: Vec::new(),
            },
            cx,
        );
    }

    /// Picks the next port without a pin for the pin tool.
    pub(crate) fn next_port(&mut self, _: &NextPort, _window: &mut Window, cx: &mut Context<Self>) {
        if let SymbolTool::Pin { port, placed } = &self.symbol_tool {
            let next = self.next_missing_port(*port, placed);
            let placed = placed.clone();
            self.set_symbol_tool(SymbolTool::Pin { port: next, placed }, cx);
        }
    }

    fn turn_symbol(&mut self, mirror: bool, cx: &mut Context<Self>) {
        self.note_view(cx);
        if !self.in_symbol_view {
            return;
        }
        let Some(scene) = self.symbol_scene.clone() else {
            return;
        };
        let overrides = symbol_tools::turn_overrides(&scene, &self.symbol_selection, mirror);
        self.commit_symbol(overrides, cx);
    }

    /// Rotates the symbol selection by 90 degrees about its center.
    pub(crate) fn symbol_rotate(
        &mut self,
        _: &SymbolRotate,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.turn_symbol(false, cx);
    }

    /// Mirrors the symbol selection left to right about its center.
    pub(crate) fn symbol_mirror(
        &mut self,
        _: &SymbolMirror,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.turn_symbol(true, cx);
    }

    pub(crate) fn create_symbol_action(
        &mut self,
        _: &CreateSymbol,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.note_view(cx);
        if self.in_symbol_view {
            self.create_symbol(cx);
        }
    }

    pub(crate) fn wire_tool(&mut self, _: &Wire, _window: &mut Window, cx: &mut Context<Self>) {
        if self.view(cx) == View::Symbol {
            return;
        }
        self.set_tool(
            Tool::Wire {
                draft: None,
                flipped: false,
            },
            cx,
        );
    }

    pub(crate) fn port_tool(&mut self, _: &PlacePort, window: &mut Window, cx: &mut Context<Self>) {
        if self.view(cx) == View::Symbol {
            return;
        }
        self.set_tool(Tool::Port { name: None }, cx);
        self.prompt_name("Port name…", window, cx);
    }

    pub(crate) fn label_tool(
        &mut self,
        _: &PlaceLabel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.view(cx) == View::Symbol {
            return;
        }
        self.set_tool(Tool::Label { name: None }, cx);
        self.prompt_name("Label name…", window, cx);
    }

    /// Flips which way the segment to the pointer bends.
    pub(crate) fn flip_bend(&mut self, _: &FlipBend, _window: &mut Window, cx: &mut Context<Self>) {
        match &mut self.tool {
            Tool::Wire {
                draft: Some(draft), ..
            } => draft.flipped = !draft.flipped,
            Tool::Wire { flipped, .. } => *flipped = !*flipped,
            _ => return,
        }
        cx.notify();
    }

    /// Deletes the selection.
    pub(crate) fn delete(&mut self, _: &Delete, _window: &mut Window, cx: &mut Context<Self>) {
        self.note_view(cx);
        self.delete_selection(cx);
    }

    /// In a drawing tool, drops the last click; in Select, deletes the
    /// selection.
    pub(crate) fn backspace(
        &mut self,
        _: &Backspace,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.note_view(cx);
        if self.in_symbol_view {
            match &mut self.symbol_tool {
                SymbolTool::Line { points } | SymbolTool::Polygon { points } => {
                    points.pop();
                }
                SymbolTool::Rect { start } => *start = None,
                SymbolTool::Circle { center } => *center = None,
                SymbolTool::Arc { center, start } => {
                    if start.take().is_none() {
                        *center = None;
                    }
                }
                SymbolTool::Select => self.delete_selection(cx),
                SymbolTool::Text { .. } | SymbolTool::Pin { .. } => {}
            }
            cx.notify();
            return;
        }
        match &mut self.tool {
            Tool::Wire {
                draft: draft @ Some(_),
                ..
            } => {
                if !draft.as_mut().is_some_and(WireDraft::undo_click) {
                    *draft = None;
                }
                cx.notify();
            }
            Tool::Select => self.delete_selection(cx),
            _ => {}
        }
    }

    /// Ends the wire being drawn at a new net label under the pointer, or
    /// finishes the line or polygon being drawn.
    pub(crate) fn enter(&mut self, _: &Enter, window: &mut Window, cx: &mut Context<Self>) {
        self.note_view(cx);
        if self.in_symbol_view {
            match self.symbol_tool.clone() {
                SymbolTool::Line { points } => self.finish_polyline(points, false, cx),
                SymbolTool::Polygon { points } => self.finish_polyline(points, true, cx),
                _ => {}
            }
            return;
        }
        let Tool::Wire {
            draft: Some(draft), ..
        } = &self.tool
        else {
            return;
        };
        let at = self.cursor.unwrap_or_else(|| draft.last());
        self.label_prompt = Some((draft.clone(), at));
        self.prompt_name("Label name…", window, cx);
    }

    /// Hands the properties panel the focus, to edit the selection.
    pub(crate) fn edit_properties(
        &mut self,
        _: &Edit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.note_view(cx);
        let empty = if self.in_symbol_view {
            self.symbol_selection.is_empty()
        } else {
            self.selection.is_empty()
        };
        if empty {
            return;
        }
        cx.emit(CanvasEvent::EditProperties);
        if let Some(focus) = &self.properties_focus {
            window.focus(focus);
        }
    }

    pub(crate) fn descend(&mut self, _: &Descend, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(name) = self.selected_instance() else {
            return;
        };
        self.state.update(cx, |state, cx| {
            state.descend_schematic(&name, cx);
        });
    }

    pub(crate) fn return_up(&mut self, _: &Return, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.return_up(cx);
        });
    }

    pub(crate) fn toggle_flylines(
        &mut self,
        _: &ToggleFlylines,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state.update(cx, |state, cx| {
            state.show_flylines = !state.show_flylines;
            cx.notify();
        });
    }

    /// Rotates the selection by 90 degrees, rewriting its wires' routes.
    pub(crate) fn rotate(&mut self, _: &Rotate, _window: &mut Window, cx: &mut Context<Self>) {
        self.orient(false, cx);
    }

    /// Mirrors the selection, rewriting its wires' routes.
    pub(crate) fn mirror(&mut self, _: &Mirror, _window: &mut Window, cx: &mut Context<Self>) {
        self.orient(true, cx);
    }

    fn orient(&mut self, mirror: bool, cx: &mut Context<Self>) {
        if self.view(cx) == View::Symbol {
            return;
        }
        // A placement tool turns what it is placing.
        if let Tool::PlaceInstance { orientation, .. } | Tool::PlaceDevice { orientation, .. } =
            &mut self.tool
        {
            if mirror {
                orientation.reflect = !orientation.reflect;
            } else {
                orientation.angle += geometry::transform::Rotation::R90;
            }
            cx.notify();
            return;
        }
        let Some(scene) = self.scene.clone() else {
            return;
        };
        let overrides = tools::orient_overrides(&scene, &self.selection, mirror);
        self.commit(overrides, cx);
    }

    /// A click of a tool other than Select.
    fn tool_click(&mut self, event: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let at = self.grid_at(event.position, cx);
        let point = self.world_at(event.position, cx);
        let hits = self.hits_under(event.position, cx);
        let Some(target) = self.target(cx) else {
            return;
        };
        match self.tool.clone() {
            Tool::Select => {}
            Tool::PlaceInstance {
                preview,
                orientation,
            } => {
                let ghost = self.tool_symbol(at).map(Overlay::Symbol);
                let client = self.state.read(cx).lang_server_client.clone();
                let request = PlaceSchematicInstanceRequest {
                    target,
                    invocation: preview.invocation,
                    placement: ElementPlacement {
                        at,
                        angle: tools::degrees(orientation.angle),
                        reflect: orientation.reflect,
                    },
                };
                self.send_edit(
                    ghost.into_iter().collect(),
                    Box::pin(async move { client.place_schematic_instance(request).await }),
                    cx,
                );
            }
            Tool::PlaceDevice {
                preview,
                orientation,
            } => {
                let ghost = self.tool_symbol(at).map(Overlay::Symbol);
                let client = self.state.read(cx).lang_server_client.clone();
                let request = PlaceDeviceRequest {
                    target,
                    kind: preview.kind.name().to_owned(),
                    terminals: preview.terminals,
                    args: preview.args,
                    placement: ElementPlacement {
                        at,
                        angle: tools::degrees(orientation.angle),
                        reflect: orientation.reflect,
                    },
                };
                self.send_edit(
                    ghost.into_iter().collect(),
                    Box::pin(async move { client.place_device(request).await }),
                    cx,
                );
            }
            Tool::Wire {
                draft: None,
                flipped,
            } => {
                let Some(scene) = self.scene.clone() else {
                    return;
                };
                match entry::end_among(&scene, &hits, point) {
                    Some(Ok(start)) => {
                        let mut draft = WireDraft::new(start);
                        draft.flipped = flipped;
                        self.set_tool(
                            Tool::Wire {
                                draft: Some(draft),
                                flipped,
                            },
                            cx,
                        );
                    }
                    Some(Err(error)) => self.report_end_error(error, cx),
                    None => {
                        if let Some(Hit::Flyline(index)) =
                            hits.iter().find(|hit| matches!(hit, Hit::Flyline(_)))
                            && let Some(request) =
                                entry::flyline_request(&scene, target, *index, flipped)
                        {
                            let points =
                                entry::flyline_preview(&scene, *index, flipped).unwrap_or_default();
                            self.send_wire(request, points, cx);
                        }
                    }
                }
            }
            Tool::Wire {
                draft: Some(mut draft),
                flipped,
            } => {
                let Some(scene) = self.scene.clone() else {
                    return;
                };
                match entry::end_among(&scene, &hits, point) {
                    Some(Ok(end)) => {
                        let points = draft.route_to(&end);
                        let mut polyline = vec![draft.start.pin];
                        polyline.extend(points);
                        polyline.push(end.pin);
                        let request = entry::wire_request(&scene, target, &draft, &end);
                        self.send_wire(request, polyline, cx);
                        self.set_tool(
                            Tool::Wire {
                                draft: None,
                                flipped,
                            },
                            cx,
                        );
                    }
                    Some(Err(error)) => self.report_end_error(error, cx),
                    None if event.click_count >= 2 => {
                        // The first click of the pair fixed a corner here.
                        draft.undo_click();
                        self.tool = Tool::Wire {
                            draft: Some(draft.clone()),
                            flipped,
                        };
                        self.label_prompt = Some((draft, at));
                        self.prompt_name("Label name…", window, cx);
                    }
                    None => {
                        draft.click(at);
                        self.tool = Tool::Wire {
                            draft: Some(draft),
                            flipped,
                        };
                        cx.notify();
                    }
                }
            }
            Tool::Port { name: Some(name) } | Tool::Label { name: Some(name) } => {
                let Some(scene) = self.scene.clone() else {
                    return;
                };
                let port = matches!(self.tool, Tool::Port { .. });
                match entry::signal_request(&scene, target, name, port, &hits, point) {
                    Ok(request) => {
                        self.send_signal(request, cx);
                        self.set_tool(Tool::Select, cx);
                    }
                    Err(EndError::NotAnEnd) => self.show_message(
                        MessageType::WARNING,
                        "Click a pin or wire to place the label on it.",
                        cx,
                    ),
                    Err(error) => self.report_end_error(error, cx),
                }
            }
            Tool::Port { name: None } | Tool::Label { name: None } => {}
        }
    }

    /// Writes the position of every unplaced, editable element and port.
    pub(crate) fn fix_positions(
        &mut self,
        _: &FixPositions,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(scene) = self.scene.clone() else {
            return;
        };
        let overrides = tools::fix_positions(&scene);
        self.commit(overrides, cx);
    }

    /// Removes the selected wire's route, leaving its connection as a
    /// flyline.
    pub(crate) fn remove_route(
        &mut self,
        _: &RemoveRoute,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.remove_selected_route(cx);
    }

    pub(crate) fn remove_selected_route(&mut self, cx: &mut Context<Self>) {
        let Some(scene) = self.scene.clone() else {
            return;
        };
        let mut overrides = Overrides::default();
        for hit in &self.selection {
            if let Hit::Wire(index) = hit
                && let Some(wire) = scene.wires.get(*index).filter(|wire| wire.editable)
            {
                overrides.routes.insert(wire.span.clone(), None);
            }
        }
        self.commit(overrides, cx);
    }

    pub(crate) fn dark_mode(&mut self, _: &DarkMode, _window: &mut Window, cx: &mut Context<Self>) {
        self.state.update(cx, |state, cx| {
            state.dark_mode = true;
            cx.notify();
        });
    }

    pub(crate) fn light_mode(
        &mut self,
        _: &LightMode,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.state.update(cx, |state, cx| {
            state.dark_mode = false;
            cx.notify();
        });
    }

    // Mouse.

    fn on_left_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.note_view(cx);
        match self.view(cx) {
            View::Symbol => {
                if self.edit_in_flight {
                    return;
                }
                if matches!(self.symbol_tool, SymbolTool::Select) {
                    self.symbol_select_click(event, cx);
                } else if !self.edit_waiting()
                    && !matches!(self.symbol_tool, SymbolTool::Text { text: None })
                {
                    let point = self.world_at(event.position, cx);
                    self.symbol_tool_click(point, event.click_count, cx);
                }
            }
            _ => {
                if self.edit_in_flight {
                    return;
                }
                if !matches!(self.tool, Tool::Select) {
                    if self.edit_waiting() || self.tool.wants_name() || self.label_prompt.is_some()
                    {
                        return;
                    }
                    self.tool_click(event, window, cx);
                    return;
                }
                let hits = self.hits_under(event.position, cx);
                let primary = self.selection.first().copied();
                let next = match (primary, hits.len()) {
                    (_, 0) => None,
                    // Repeated clicks cycle through what overlaps.
                    (Some(current), _) if event.click_count == 1 && !event.modifiers.shift => hits
                        .iter()
                        .position(|hit| *hit == current)
                        .map(|index| hits[(index + 1) % hits.len()])
                        .or(Some(hits[0])),
                    _ => Some(hits[0]),
                };
                let selection = match next {
                    None if event.modifiers.shift => self.selection.clone(),
                    None => Vec::new(),
                    Some(hit) if event.modifiers.shift => {
                        let mut selection = self.selection.clone();
                        if let Some(index) = selection.iter().position(|s| *s == hit) {
                            selection.remove(index);
                        } else {
                            selection.push(hit);
                        }
                        selection
                    }
                    Some(hit) if self.selection.contains(&hit) => self.selection.clone(),
                    Some(hit) => vec![hit],
                };
                // A drag starts on something editable, once the last edit is
                // in a snapshot. A press on the primary selection keeps it,
                // so it can be dragged; a release without a drag then cycles
                // to what overlaps it.
                let start = self.world_at(event.position, cx);
                let grab = primary
                    .filter(|current| {
                        event.click_count == 1
                            && !event.modifiers.shift
                            && hits.contains(current)
                            && !self.edit_waiting()
                    })
                    .and_then(|current| self.drag_kind(current, start));
                let click = if grab.is_some() {
                    Some(selection)
                } else {
                    self.select(selection, cx);
                    None
                };
                if event.click_count == 2
                    && let Some(name) = self.selected_instance()
                {
                    self.state.update(cx, |state, cx| {
                        state.descend_schematic(&name, cx);
                    });
                    return;
                }
                let kind = grab.or_else(|| {
                    next.filter(|_| !self.edit_waiting())
                        .and_then(|hit| self.drag_kind(hit, start))
                });
                if let (Some(kind), Some(base)) = (kind, self.scene.clone()) {
                    self.drag = Some(Drag {
                        kind,
                        start,
                        current: start,
                        base,
                        selection: self.selection.clone(),
                        click,
                    });
                }
            }
        }
    }

    fn on_middle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.is_dragging = true;
        self.drag_start = event.position;
        self.offset_start = self.current_viewport(cx).offset;
    }

    pub(crate) fn on_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_dragging {
            let view = self.view(cx);
            let offset = self.offset_start + (event.position - self.drag_start);
            self.viewport_mut(view).offset = offset;
            self.hover = None;
            cx.notify();
            return;
        }
        let world = self.world_at(event.position, cx);
        if let Some(drag) = &mut self.drag {
            drag.current = world;
            let preview = drag.preview();
            if preview.is_some() {
                drag.click = None;
            }
            if preview != self.drag_overrides {
                self.drag_overrides = preview;
                self.rebuild_now(cx);
            }
            return;
        }
        if let Some(drag) = &mut self.symbol_drag {
            drag.current = world;
            let preview = drag.preview();
            if preview.is_some() {
                drag.click = None;
            }
            if preview != self.symbol_drag_overrides {
                self.symbol_drag_overrides = preview;
                self.rebuild_symbol_now(cx);
            }
            return;
        }
        let cursor = Some(self.grid_at(event.position, cx));
        let world = Some(world);
        if cursor != self.cursor || world != self.cursor_world {
            self.cursor = cursor;
            self.cursor_world = world;
            let tool_active = if self.in_symbol_view {
                !matches!(self.symbol_tool, SymbolTool::Select)
            } else {
                !matches!(self.tool, Tool::Select)
            };
            if tool_active {
                cx.notify();
            }
        }
        let hover = match self.view(cx) {
            View::Symbol => None,
            _ => {
                let hits = self.hits_under(event.position, cx);
                match &self.tool {
                    // The wire tool cares about what a click would attach to.
                    Tool::Wire { draft: None, .. } => hits
                        .iter()
                        .find(|hit| !matches!(hit, Hit::Element(_)))
                        .copied(),
                    Tool::Wire { .. } | Tool::Port { .. } | Tool::Label { .. } => hits
                        .iter()
                        .find(|hit| !matches!(hit, Hit::Element(_) | Hit::Flyline(_)))
                        .copied(),
                    Tool::PlaceInstance { .. } | Tool::PlaceDevice { .. } => None,
                    Tool::Select => hits.into_iter().next(),
                }
            }
        };
        if hover != self.hover {
            self.hover = hover;
            cx.notify();
        }
    }

    fn on_left_mouse_up(&mut self, _: &MouseUpEvent, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(drag) = self.symbol_drag.take() {
            let overrides = self.symbol_drag_overrides.take();
            match overrides.or_else(|| drag.preview()) {
                Some(overrides) if !overrides.is_empty() && self.can_edit(cx) => {
                    self.commit_symbol(overrides, cx)
                }
                _ => self.rebuild_symbol_now(cx),
            }
            if let Some(selection) = drag.click {
                self.select_symbol(selection, cx);
            }
            return;
        }
        let Some(drag) = self.drag.take() else {
            return;
        };
        let overrides = self.drag_overrides.take();
        match overrides.or_else(|| drag.preview()) {
            Some(overrides) if !overrides.is_empty() && self.can_edit(cx) => {
                self.commit(overrides, cx)
            }
            _ => self.rebuild_now(cx),
        }
        if let Some(selection) = drag.click {
            self.select(selection, cx);
        }
    }

    fn on_middle_mouse_up(
        &mut self,
        _: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_dragging {
            self.is_dragging = false;
            cx.notify();
        }
    }

    fn on_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let delta = event.delta.pixel_delta(px(20.));
        let factor = (-f32::from(delta.y) / 400.).exp();
        self.zoom(factor, event.position, cx);
    }
}

impl Focusable for SchematicCanvas {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SchematicCanvas {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let context = match self.view(cx) {
            View::Symbol => "Canvas SymbolCanvas",
            _ => "Canvas SchematicCanvas",
        };
        div()
            .flex()
            .flex_1()
            .relative()
            .key_context(context)
            .track_focus(&self.focus_handle(cx))
            .size_full()
            .on_mouse_down(MouseButton::Left, cx.listener(Self::on_left_mouse_down))
            .on_mouse_down(MouseButton::Middle, cx.listener(Self::on_middle_mouse_down))
            .on_mouse_move(cx.listener(Self::on_mouse_move))
            .on_mouse_up(MouseButton::Left, cx.listener(Self::on_left_mouse_up))
            .on_mouse_up_out(MouseButton::Left, cx.listener(Self::on_left_mouse_up))
            .on_mouse_up(MouseButton::Middle, cx.listener(Self::on_middle_mouse_up))
            .on_mouse_up_out(MouseButton::Middle, cx.listener(Self::on_middle_mouse_up))
            .on_scroll_wheel(cx.listener(Self::on_scroll_wheel))
            .on_action(cx.listener(Self::fit_action))
            .on_action(cx.listener(Self::pan_left))
            .on_action(cx.listener(Self::pan_right))
            .on_action(cx.listener(Self::pan_up))
            .on_action(cx.listener(Self::pan_down))
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::select_mode))
            .on_action(cx.listener(Self::descend))
            .on_action(cx.listener(Self::return_up))
            .on_action(cx.listener(Self::toggle_flylines))
            .on_action(cx.listener(Self::rotate))
            .on_action(cx.listener(Self::mirror))
            .on_action(cx.listener(Self::fix_positions))
            .on_action(cx.listener(Self::remove_route))
            .on_action(cx.listener(Self::wire_tool))
            .on_action(cx.listener(Self::port_tool))
            .on_action(cx.listener(Self::label_tool))
            .on_action(cx.listener(Self::flip_bend))
            .on_action(cx.listener(Self::delete))
            .on_action(cx.listener(Self::backspace))
            .on_action(cx.listener(Self::enter))
            .on_action(cx.listener(Self::edit_properties))
            .on_action(cx.listener(Self::symbol_line))
            .on_action(cx.listener(Self::symbol_polygon))
            .on_action(cx.listener(Self::symbol_rect))
            .on_action(cx.listener(Self::symbol_circle))
            .on_action(cx.listener(Self::symbol_arc))
            .on_action(cx.listener(Self::symbol_text))
            .on_action(cx.listener(Self::symbol_pin))
            .on_action(cx.listener(Self::next_port))
            .on_action(cx.listener(Self::symbol_rotate))
            .on_action(cx.listener(Self::symbol_mirror))
            .on_action(cx.listener(Self::create_symbol_action))
            .on_action(cx.listener(Self::copy))
            .on_action(cx.listener(Self::paste))
            .on_action(cx.listener(Self::dark_mode))
            .on_action(cx.listener(Self::light_mode))
            .child(SchematicElement {
                canvas: cx.entity().clone(),
            })
    }
}
