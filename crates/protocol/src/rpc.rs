//! The RPC services between the analyzer and the GUI, and the types they carry.

use std::{
    io::{BufRead, BufReader},
    path::PathBuf,
};

use argonc::{
    ast::Span,
    compile::{
        Align, BasicRect, CellId, CompileOutput, CompiledData, DeviceKind, PreviewGeometry, Symbol,
    },
    parse,
};
use bumpalo::Bump;
use ls_types::{MessageType, Uri};
use serde::{Deserialize, Serialize};

use crate::config::ArgonConfig;

/// A single source rewrite: replace the text at `span` with `value`. Used to
/// persist solution-space-exploration drags by updating initial-condition
/// values (e.g. the `100.` in `x1i=100.`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueEdit {
    pub span: Span,
    pub value: String,
}

/// Maps a span through value replacements applied to the text it was recorded
/// against. A span containing a replacement grows or shrinks with it.
pub fn remap_span_after_value_edits(span: &Span, edits: &[ValueEdit]) -> Span {
    let start = span.span.start();
    let end = span.span.end();
    let mut shift_before = 0isize;
    let mut shift_within = 0isize;
    for edit in edits.iter().filter(|edit| edit.span.path == span.path) {
        let old_start = edit.span.span.start();
        let old_end = edit.span.span.end();
        let delta = edit.value.len() as isize - (old_end - old_start) as isize;
        if old_end <= start {
            shift_before += delta;
        } else if old_start < end {
            shift_within += delta;
        }
    }
    Span {
        path: span.path.clone(),
        span: cfgrammar::Span::new(
            (start as isize + shift_before) as usize,
            (end as isize + shift_before + shift_within) as usize,
        ),
    }
}

/// Ensures that a geometry constructor has a named initial-condition kwarg.
/// If the kwarg already exists, its value is replaced; otherwise the analyzer
/// inserts it into the call using the current source layout.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitialConditionEdit {
    pub call_span: Span,
    pub name: String,
    pub value: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DimensionParams {
    pub p: String,
    pub n: String,
    pub value: String,
    pub coord: String,
    pub pstop: String,
    pub nstop: String,
    pub horiz: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolygonParams {
    pub layer: String,
    pub points: Vec<(f64, f64)>,
    pub constraints: Vec<DrawSegmentConstraint>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PathParams {
    pub layer: String,
    pub width: f64,
    pub points: Vec<(f64, f64)>,
    pub constraints: Vec<DrawSegmentConstraint>,
}

#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum DrawSegmentConstraint {
    Horizontal(usize),
    Vertical(usize),
    DiagonalPositive(usize),
    DiagonalNegative(usize),
}

/// Flattened geometry and the source location where placing it will insert an
/// instance. The GUI keeps this separate from the currently open layout and
/// uses it only for the cursor-following placement outline.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstancePreview {
    pub geometry: PreviewGeometry,
    pub invocation: String,
    pub scope_span: Span,
}

/// Receipt for an accepted drag edit: the replacements applied, and the first
/// source revision that includes them. A snapshot compiled from that revision
/// or a later one shows the drag.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValueEditResult {
    pub edits: Vec<ValueEdit>,
    pub revision: u64,
}

/// Receipt for an accepted rectangle insertion. Later snapshots can supersede
/// it (for example, an undo) even if its first compiled frame was never shown.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RectangleEditResult {
    pub span: Span,
    pub revision: u64,
    /// The source-authoritative binding name, which may differ when several
    /// drawings outpace the last compiled geometry snapshot.
    pub name: String,
}

/// A keyword to set on a call, or to remove when it has no value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeywordEdit {
    pub name: String,
    pub value: Option<String>,
}

/// The keyword edits of one call the GUI drew a schematic element or
/// connection from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlacementEdit {
    pub call: Span,
    pub keywords: Vec<KeywordEdit>,
}

/// A schematic gesture: placement and route rewrites applied as one edit.
/// `base_revision` is the snapshot the GUI drew the gesture on; the edit is
/// refused unless it is the published one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceRequest {
    pub base_revision: u64,
    pub edits: Vec<PlacementEdit>,
}

/// Receipt for an accepted schematic edit: the source revision it was
/// applied to, and the span of each edited call afterward, in request order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditReceipt {
    pub revision: u64,
    pub spans: Vec<Span>,
}

/// Where a schematic gesture adds code: the body of the cell on screen, in
/// the snapshot at `base_revision`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchematicTarget {
    pub base_revision: u64,
    /// The span of the cell body.
    pub cell: Span,
}

/// How a new element is placed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ElementPlacement {
    pub at: (i64, i64),
    /// Degrees counterclockwise: 0, 90, 180, or 270.
    pub angle: i64,
    pub reflect: bool,
}

/// Adds a port or a net label, connected to `connect_to` when given.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AddSignalRequest {
    pub target: SchematicTarget,
    pub name: String,
    pub port: bool,
    pub at: (i64, i64),
    /// The path of the pin or signal the new signal connects to.
    pub connect_to: Option<String>,
    pub route: Option<Vec<(i64, i64)>>,
    /// Placements of other calls, applied in the same edit.
    pub placements: Vec<PlacementEdit>,
}

/// Places an instance of a cell in the schematic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceSchematicInstanceRequest {
    pub target: SchematicTarget,
    pub invocation: String,
    pub placement: ElementPlacement,
}

/// Places a device.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlaceDeviceRequest {
    pub target: SchematicTarget,
    /// The `DeviceKind` variant.
    pub kind: String,
    pub terminals: usize,
    /// The model and parameters, as argument text.
    pub args: String,
    pub placement: ElementPlacement,
}

/// Connects two pins or signals, named by their paths.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConnectRequest {
    pub target: SchematicTarget,
    pub a: String,
    pub b: String,
    pub route: Option<Vec<(i64, i64)>>,
    /// Placements of other calls, applied in the same edit.
    pub placements: Vec<PlacementEdit>,
}

/// Deletes elements, signals, and connections by the calls that created
/// them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeleteRequest {
    pub base_revision: u64,
    pub targets: Vec<Span>,
}

/// A use of a deleted binding that the GUI cannot clean up.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BlockingReference {
    pub span: Span,
    /// The zero-based line of the reference.
    pub line: u32,
    /// The source line, trimmed.
    pub text: String,
}

/// What a deletion did.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DeleteOutcome {
    Applied(EditReceipt),
    /// Nothing was deleted: these references must go first.
    Blocked(Vec<BlockingReference>),
}

/// An argument of a call, by position or keyword.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CallArgument {
    Positional(usize),
    Keyword(String),
}

/// Rewrites or inserts one argument of a call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetArgumentRequest {
    pub base_revision: u64,
    pub call: Span,
    pub argument: CallArgument,
    pub value: String,
}

/// The source text of a call's arguments.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallArguments {
    pub positional: Vec<String>,
    pub keywords: Vec<(String, String)>,
}

/// A symbol item to draw, in symbol coordinates.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum NewSymbolItem {
    Line {
        points: Vec<(f64, f64)>,
    },
    Polygon {
        points: Vec<(f64, f64)>,
        fill: bool,
    },
    Rect {
        x0: f64,
        y0: f64,
        x1: f64,
        y1: f64,
        fill: bool,
    },
    Circle {
        x: f64,
        y: f64,
        r: f64,
        fill: bool,
    },
    Arc {
        x: f64,
        y: f64,
        r: f64,
        start: f64,
        end: f64,
    },
    Text {
        text: String,
        x: f64,
        y: f64,
        size: f64,
        align: Align,
    },
    InstName {
        x: f64,
        y: f64,
        size: f64,
        align: Align,
    },
    /// `name=value` for the cell parameter `name`.
    Param {
        name: String,
        x: f64,
        y: f64,
        size: f64,
        align: Align,
    },
    /// The pin of the port at position `port`.
    Pin {
        port: usize,
        x: i64,
        y: i64,
        label: bool,
    },
}

/// Draws symbol items in the cell on screen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AddSymbolItemsRequest {
    pub target: SchematicTarget,
    pub items: Vec<NewSymbolItem>,
}

/// Writes the generated box of the cell on screen as `std::symbol` calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CreateSymbolRequest {
    pub target: SchematicTarget,
}

/// Renames the binding whose value is the call at `site`, with every
/// reference to it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenameRequest {
    pub base_revision: u64,
    pub site: Span,
    pub name: String,
}

/// What a copied statement created.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ClipboardKind {
    Signal,
    Instance,
    Device,
    Connect,
    Symbol,
}

/// A statement copied from a cell body, to be pasted with a fresh name and
/// a shifted placement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClipboardItem {
    pub kind: ClipboardKind,
    /// The binding, for a `let` statement.
    pub name: Option<String>,
    pub public: bool,
    /// The call, as source text.
    pub call: String,
}

/// Pastes copied statements into the cell on screen, moved by `offset`
/// grid units in the schematic and `symbol_offset` in the symbol.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PasteRequest {
    pub target: SchematicTarget,
    pub items: Vec<ClipboardItem>,
    pub offset: (i64, i64),
    pub symbol_offset: (f64, f64),
}

/// A schematic instance ready to be placed: what the GUI draws while the
/// invocation follows the cursor.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchematicInstancePreview {
    pub invocation: String,
    pub cell_name: String,
    /// The cell's symbol, or its generated box.
    pub symbol: Symbol,
    /// The port names, in port order.
    pub pin_names: Vec<String>,
}

/// A device ready to be placed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevicePreview {
    pub kind: DeviceKind,
    pub terminals: usize,
    /// The model and parameters, as argument text.
    pub args: String,
}

/// A compiled GUI result tied to the exact analyzer source revision that
/// produced it. Diagnostics continue to travel over LSP.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompilationSnapshot {
    pub revision: u64,
    pub output: CompileOutput,
}

impl CompilationSnapshot {
    pub fn data(&self) -> Option<&CompiledData> {
        match &self.output {
            CompileOutput::Valid(data) => Some(data),
            CompileOutput::ExecErrors(errors) => errors.output.as_ref(),
            _ => None,
        }
    }

    pub fn data_mut(&mut self) -> Option<&mut CompiledData> {
        match &mut self.output {
            CompileOutput::Valid(data) => Some(data),
            CompileOutput::ExecErrors(errors) => errors.output.as_mut(),
            _ => None,
        }
    }
}

/// A connection-local delta. Cells are immutable: pointer identity in the
/// compiler session proves reuse, including source spans and non-geometry data.
/// A new connection always starts with a full snapshot. The ordered cell list
/// also removes cells absent from the new result, without retaining old graphs.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompilationUpdate {
    pub snapshot: CompilationSnapshot,
    pub base_revision: Option<u64>,
    pub cell_order: Vec<CellId>,
}

/// A losslessly compressed compilation update for the analyzer-to-GUI RPC.
///
/// Scope-heavy layouts contain millions of repeated names and source paths.
/// Streaming postcard through zstd in both directions avoids allocating a
/// second, equally large uncompressed byte buffer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompressedCompilationUpdate {
    bytes: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusedCellPreview {
    pub previous_cell: CellId,
    pub update: CompressedCompilationUpdate,
}

/// GUI-side costs for accepting a compilation snapshot. Returning these to the
/// language server makes a long transfer distinguishable from decompression,
/// delta materialization, and hierarchy preparation in editor progress.
#[derive(Debug, Copy, Clone, Default, Serialize, Deserialize)]
pub struct GuiUpdateResult {
    pub accepted: bool,
    pub decode_seconds: f64,
    pub materialize_seconds: f64,
    pub prepare_seconds: f64,
}

impl CompressedCompilationUpdate {
    #[cfg(not(target_family = "wasm"))]
    pub fn encode(update: &CompilationUpdate) -> Result<Self, String> {
        let encoder =
            zstd::stream::write::Encoder::new(Vec::new(), 1).map_err(|error| error.to_string())?;
        // postcard writes a few bytes at a time, which zstd handles slowly.
        let writer = postcard::to_io(update, std::io::BufWriter::new(encoder))
            .map_err(|error| error.to_string())?;
        let encoder = writer.into_inner().map_err(|error| error.to_string())?;
        let bytes = encoder.finish().map_err(|error| error.to_string())?;
        Ok(Self { bytes })
    }

    pub fn decode(self) -> Result<CompilationUpdate, String> {
        let decoder = decompress(self.bytes.as_slice())?;
        let arena = Bump::new();
        let mut deserializer = postcard::Deserializer::from_flavor(StreamFlavor {
            reader: BufReader::new(decoder),
            arena: &arena,
            scratch: Vec::new(),
        });
        CompilationUpdate::deserialize(&mut deserializer).map_err(|error| error.to_string())
    }

    pub fn encoded_len(&self) -> usize {
        self.bytes.len()
    }
}

/// A reader of the zstd-compressed `bytes`.
#[cfg(not(target_family = "wasm"))]
fn decompress(bytes: &[u8]) -> Result<impl std::io::Read + '_, String> {
    zstd::stream::read::Decoder::new(bytes).map_err(|error| error.to_string())
}

/// A reader of the zstd-compressed `bytes`, in pure Rust for the web.
#[cfg(target_family = "wasm")]
fn decompress(bytes: &[u8]) -> Result<impl std::io::Read + '_, String> {
    ruzstd::decoding::StreamingDecoder::new(bytes).map_err(|error| error.to_string())
}

/// Reads postcard data from a stream without holding all of it in memory.
/// Borrowed strings and bytes are copied into `arena`.
struct StreamFlavor<'de, R> {
    reader: R,
    arena: &'de Bump,
    scratch: Vec<u8>,
}

impl<'de, R: BufRead + 'de> postcard::de_flavors::Flavor<'de> for StreamFlavor<'de, R> {
    type Remainder = ();
    type Source = ();

    fn pop(&mut self) -> postcard::Result<u8> {
        let byte = match self.reader.fill_buf() {
            Ok([byte, ..]) => *byte,
            _ => return Err(postcard::Error::DeserializeUnexpectedEnd),
        };
        self.reader.consume(1);
        Ok(byte)
    }

    fn try_take_n(&mut self, ct: usize) -> postcard::Result<&'de [u8]> {
        let bytes = self.arena.alloc_slice_fill_copy(ct, 0);
        self.reader
            .read_exact(bytes)
            .map_err(|_| postcard::Error::DeserializeUnexpectedEnd)?;
        Ok(bytes)
    }

    fn try_take_n_temp<'a>(&'a mut self, ct: usize) -> postcard::Result<&'a [u8]>
    where
        'de: 'a,
    {
        self.scratch.resize(ct, 0);
        self.reader
            .read_exact(&mut self.scratch)
            .map_err(|_| postcard::Error::DeserializeUnexpectedEnd)?;
        Ok(&self.scratch)
    }

    fn finalize(self) -> postcard::Result<()> {
        Ok(())
    }
}

impl CompilationUpdate {
    pub fn new(snapshot: CompilationSnapshot, previous: Option<&CompilationSnapshot>) -> Self {
        let mut update = Self {
            snapshot,
            base_revision: None,
            cell_order: Vec::new(),
        };
        if let Some((previous, old)) = previous.and_then(|p| p.data().map(|data| (p, data)))
            && let Some(data) = update.snapshot.data_mut()
        {
            update.base_revision = Some(previous.revision);
            update.cell_order = data.cells.keys().copied().collect();
            data.cells.retain(|id, cell| {
                !old.cells
                    .get(id)
                    .is_some_and(|old| std::sync::Arc::ptr_eq(old, cell))
            });
        }
        update
    }

    /// Reject an unknown base rather than ever applying a partial hierarchy.
    /// The sender retries with a full snapshot when this returns None.
    pub fn materialize(
        mut self,
        previous: Option<&CompilationSnapshot>,
    ) -> Option<CompilationSnapshot> {
        if let Some(revision) = self.base_revision {
            let old = previous.filter(|p| p.revision == revision)?.data()?;
            let data = self.snapshot.data_mut()?;
            let mut cells = indexmap::IndexMap::with_capacity(self.cell_order.len());
            for id in self.cell_order {
                let cell = data
                    .cells
                    .swap_remove(&id)
                    .or_else(|| old.cells.get(&id).cloned())?;
                if cells.insert(id, cell).is_some() {
                    return None;
                }
            }
            if !data.cells.is_empty() || !cells.contains_key(&data.top) {
                return None;
            }
            data.cells = cells;
        }
        Some(self.snapshot)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LangServerAction {
    Save,
    Undo,
    Redo,
}

/// A command the GUI asks the editor to open at its command prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum EditorCommand {
    /// The editor's message history.
    Messages,
    /// An `Argon` subcommand, such as `inst`, and the argument typed so far.
    Argon {
        name: String,
        argument: String,
        /// Whether to run the command at once instead of waiting for the user.
        submit: bool,
    },
}

impl EditorCommand {
    /// The subcommand `name`, waiting for the user to finish `argument`.
    pub fn prefill(name: &str, argument: impl Into<String>) -> Self {
        Self::Argon {
            name: name.to_owned(),
            argument: argument.into(),
            submit: false,
        }
    }

    /// The subcommand `name`, run without an argument.
    pub fn run(name: &str) -> Self {
        Self::Argon {
            name: name.to_owned(),
            argument: String::new(),
            submit: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FocusEditorParams {
    /// The command to open, or `None` for an empty prompt.
    pub command: Option<EditorCommand>,
    pub return_to_gui: bool,
}

/// The document a GUI undo or redo applies to.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct HistoryParams {
    /// The document the GUI last edited, if any.
    pub uri: Option<Uri>,
}

/// A cell of the library that the GUI can open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellEntry {
    /// The cell's path from the library root, such as `utils::pad`.
    pub path: String,
    /// The cell's parameter names, in order.
    pub parameters: Vec<String>,
    /// Whether some parameter has no default, so opening the cell needs arguments.
    pub requires_arguments: bool,
}

/// A cell invocation recently opened in the library.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecentCell {
    /// The invocation, such as `inverter(1200., 2000., 4)`.
    pub invocation: String,
    /// The invoked cell's path as written, without a leading `lib::`, such as
    /// `utils::pad`. Empty if the invocation does not parse.
    pub cell: String,
}

impl RecentCell {
    pub fn new(invocation: String) -> Self {
        let cell = parse::parse_cell(&invocation)
            .map(|call| {
                let path = call
                    .func
                    .path
                    .iter()
                    .map(|ident| ident.name)
                    .collect::<Vec<_>>();
                path.strip_prefix(&["lib"]).unwrap_or(&path).join("::")
            })
            .unwrap_or_default();
        Self { invocation, cell }
    }
}

// Requests are cloned so a call can be resent after reconnecting.
#[tarpc::service(derive = [Clone, Serialize, Deserialize])]
pub trait LangServer {
    async fn select_span(span: Span);
    async fn draw_rect(
        scope_span: Span,
        var_name: String,
        rect: BasicRect<f64>,
    ) -> Option<RectangleEditResult>;
    async fn draw_polygon(
        scope_span: Span,
        var_name: String,
        polygon: PolygonParams,
    ) -> Option<Span>;
    async fn draw_path(scope_span: Span, var_name: String, path: PathParams) -> Option<Span>;
    async fn place_instance(scope_span: Span, invocation: String, x: f64, y: f64) -> Option<Span>;
    async fn draw_dimension(scope_span: Span, params: DimensionParams) -> Option<Span>;
    async fn edit_dimension(span: Span, value: String) -> Option<Span>;
    /// `base_revision` is the compile revision whose spans the edits refer to.
    async fn update_values(
        edits: Vec<ValueEdit>,
        initial_conditions: Vec<InitialConditionEdit>,
        base_revision: Option<u64>,
    ) -> Option<ValueEditResult>;
    async fn add_eq_constraint(scope_span: Span, lhs: String, rhs: String);
    async fn place(request: PlaceRequest) -> Option<EditReceipt>;
    async fn add_signal(request: AddSignalRequest) -> Option<EditReceipt>;
    async fn place_schematic_instance(
        request: PlaceSchematicInstanceRequest,
    ) -> Option<EditReceipt>;
    async fn place_device(request: PlaceDeviceRequest) -> Option<EditReceipt>;
    async fn connect(request: ConnectRequest) -> Option<EditReceipt>;
    async fn delete(request: DeleteRequest) -> Option<DeleteOutcome>;
    async fn set_argument(request: SetArgumentRequest) -> Option<EditReceipt>;
    async fn call_arguments(base_revision: u64, call: Span) -> Option<CallArguments>;
    async fn add_symbol_items(request: AddSymbolItemsRequest) -> Option<EditReceipt>;
    async fn create_symbol(request: CreateSymbolRequest) -> Option<EditReceipt>;
    /// The parameter names of the cell whose body is `cell`.
    async fn cell_parameters(base_revision: u64, cell: Span) -> Option<Vec<String>>;
    async fn rename(request: RenameRequest) -> Option<EditReceipt>;
    /// The statements that made `targets`, for pasting.
    async fn copy_statements(base_revision: u64, targets: Vec<Span>) -> Option<Vec<ClipboardItem>>;
    async fn paste(request: PasteRequest) -> Option<EditReceipt>;
    async fn open_cell(cell: String);
    async fn show_message(typ: MessageType, message: String);
    async fn dispatch_action(action: LangServerAction);
    async fn focus_editor(params: FocusEditorParams);
}

#[tarpc::service]
pub trait Gui {
    async fn compilation_started(activity_id: u64);
    async fn compilation_finished(activity_id: u64);
    async fn update_cell(update: CompressedCompilationUpdate) -> GuiUpdateResult;
    /// An independent, focused-cell geometry snapshot. It never advances the
    /// full-snapshot delta base or replaces the opened top-level hierarchy.
    async fn preview_cell(preview: FocusedCellPreview) -> GuiUpdateResult;
    async fn show_message(typ: MessageType, message: String);
    async fn fit();
    async fn set_workspace_path(path: Option<PathBuf>);
    async fn workspace_modified(modified: bool);
    async fn selected_scope() -> Option<Span>;
    async fn selected_cell() -> Option<CellId>;
    async fn place_instance(preview: InstancePreview);
    /// The body of the cell the schematic view shows.
    async fn schematic_target() -> Option<Span>;
    async fn place_schematic_instance(preview: SchematicInstancePreview);
    async fn place_device(preview: DevicePreview);
    async fn configure(config: ArgonConfig);
    async fn activate();
    /// Replaces the library's cells, which the GUI offers before a cell is open.
    async fn set_cells(cells: Vec<CellEntry>);
    /// Replaces the library's recently opened cells, most recent first.
    async fn set_recent_cells(cells: Vec<RecentCell>);
    /// Briefly outlines objects whose source overlaps `spans`, once the GUI
    /// shows `revision` or later.
    async fn highlight_spans(revision: u64, spans: Vec<Span>);
    /// Shows what an agent is doing, or clears it with `None`.
    async fn agent_activity(label: Option<String>);
}
