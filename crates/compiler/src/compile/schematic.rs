//! The schematic view of a cell: signals joined into nets by `connect`, the
//! ports an instance of the cell exposes, and the instances and devices that
//! connect to its nets.

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

use geometry::transform::Rotation;
use indexmap::IndexMap;
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use crate::ast::Span;

use super::{CellId, CompiledCell, ObjectId, Value, ValueId, VariantValues};

/// The index of a net in [`Schematic::nets`].
pub type NetIdx = u32;
/// The index of a node in [`Schematic::nodes`].
pub type NodeIdx = u32;
/// The index of a path in [`Schematic::paths`].
pub type PathId = u32;

/// The schematic view of a compiled cell.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Schematic {
    pub nets: Vec<Net>,
    /// The nets an instance of the cell connects to, distinct and in port
    /// order.
    pub ports: Vec<NetIdx>,
    pub instances: Vec<SchematicInstance>,
    pub devices: Vec<Device>,
    /// Every instance and device, in creation order.
    pub elements: Vec<Element>,
    /// Every node in source order: a `Signal()` the cell created, or a
    /// terminal of one of its instances or devices.
    pub nodes: Vec<Node>,
    /// Every `connect` that ran, in evaluation order.
    pub connections: Vec<Connection>,
    /// Paths into the cell's values, sharing prefixes.
    pub paths: Vec<PathNode>,
    /// The path of each port, relative to an instance of the cell.
    pub port_paths: Vec<Option<PathId>>,
}

impl Schematic {
    /// The position of `net` among the ports, if it is one.
    pub fn port_of(&self, net: NetIdx) -> Option<usize> {
        self.ports.iter().position(|port| *port == net)
    }

    /// The name of the port at position `port`.
    pub fn port_name(&self, port: usize) -> &str {
        &self.nets[self.ports[port] as usize].name
    }

    /// The segments of `path`, outermost first.
    pub fn path_segments(&self, path: PathId) -> Vec<&PathSegment> {
        let mut segments = Vec::new();
        let mut next = Some(path);
        while let Some(id) = next {
            let node = &self.paths[id as usize];
            segments.push(&node.segment);
            next = node.parent;
        }
        segments.reverse();
        segments
    }

    /// `path` as source text, such as `data[1]` or `m0.g`.
    pub fn path_text(&self, path: PathId) -> String {
        let mut text = String::new();
        for segment in self.path_segments(path) {
            match segment {
                PathSegment::Binding(name) => text.push_str(name),
                PathSegment::Field(name) | PathSegment::Terminal(name) => {
                    text.push('.');
                    text.push_str(name);
                }
                PathSegment::Element(index) => {
                    write!(text, "[{index}]").expect("writing to a string")
                }
                PathSegment::Tuple(index) => write!(text, ".{index}").expect("writing to a string"),
            }
        }
        text
    }
}

/// A net of a [`Schematic`].
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Net {
    pub name: String,
}

/// An instance or device of a [`Schematic`], by its index.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Element {
    Instance(usize),
    Device(usize),
}

/// A point of a [`Schematic`] that sits on a net: a `Signal()` the cell
/// created, or a terminal of one of its instances or devices.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
    pub net: NetIdx,
    pub source: NodeSource,
    /// How code at the end of the cell body names the node.
    pub path: Option<PathId>,
    /// Whether a `pub` field of the cell reaches the node, which draws a
    /// placed signal as a port pin rather than a net label.
    pub public: bool,
}

/// Where a [`Node`] came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum NodeSource {
    /// A `Signal()` call.
    Signal {
        span: Span,
        placement: Option<Placement>,
        /// Whether the GUI may rewrite the call.
        editable: bool,
    },
    /// Port `port` of the instance at `instance`.
    Terminal { instance: usize, port: usize },
    /// Terminal `terminal` of the device at `device`.
    DeviceTerminal { device: usize, terminal: usize },
}

/// A `connect` call that ran.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Connection {
    pub nodes: [NodeIdx; 2],
    pub span: Span,
    /// The corners of its wire, when the call has a `route`.
    pub route: Option<Vec<(i64, i64)>>,
    /// Whether the GUI may rewrite the call.
    pub editable: bool,
}

/// One segment of a path into the cell's values, with the path it extends.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PathNode {
    pub parent: Option<PathId>,
    pub segment: PathSegment,
}

/// One step of a path, as code at the end of the cell body writes it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PathSegment {
    /// A top-level `let`.
    Binding(String),
    /// `.name`: a struct field, or a field of a cell read through an instance.
    Field(String),
    /// `[i]`: an element of a sequence.
    Element(usize),
    /// `.i`: an element of a tuple.
    Tuple(usize),
    /// `.d`: a terminal of a device.
    Terminal(String),
}

/// How an element of a schematic is drawn, in grid units.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Placement {
    /// The position of the element's origin, when `x` and `y` are given.
    pub position: Option<(i64, i64)>,
    /// Counterclockwise rotation.
    pub angle: Rotation,
    /// Whether the element is mirrored about the x axis before rotating.
    pub reflect: bool,
}

/// A cell placed with `std::schematic::inst`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchematicInstance {
    /// The element name, such as `Xnmos`.
    pub name: String,
    pub cell: CellId,
    /// The net connected to each port of `cell`.
    pub terminals: Vec<NetIdx>,
    pub span: Option<Span>,
    pub placement: Option<Placement>,
    /// Whether the GUI may rewrite the creating call.
    pub editable: bool,
    /// How code at the end of the cell body names the instance.
    pub path: Option<PathId>,
    /// The node on each port of `cell`.
    pub terminal_nodes: Vec<NodeIdx>,
}

/// A device placed with `std::schematic::device`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Device {
    /// The element name, such as `M0`.
    pub name: String,
    pub kind: DeviceKind,
    pub terminals: Vec<NetIdx>,
    pub model: Option<String>,
    /// The `value` of a resistor or capacitor.
    pub value: Option<ParamValue>,
    /// The other parameters, in source order.
    pub params: Vec<(String, ParamValue)>,
    pub span: Option<Span>,
    pub placement: Option<Placement>,
    /// Whether the GUI may rewrite the creating call.
    pub editable: bool,
    /// How code at the end of the cell body names the device.
    pub path: Option<PathId>,
    /// The device's own node on each terminal.
    pub terminal_nodes: Vec<NodeIdx>,
    /// The node passed for each terminal, which the terminal is joined to.
    pub attached_nodes: Vec<NodeIdx>,
    /// The span of each terminal expression, when the terminal list is a
    /// literal.
    pub terminal_spans: Vec<Option<Span>>,
}

/// The kind of a [`Device`], mirroring `std::schematic::DeviceKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceKind {
    Mos,
    /// A `Mos` drawn with an n-channel symbol.
    Nmos,
    /// A `Mos` drawn with a p-channel symbol.
    Pmos,
    Res,
    Cap,
    Diode,
    Bjt,
    /// A `Bjt` drawn with an NPN symbol.
    Npn,
    /// A `Bjt` drawn with a PNP symbol.
    Pnp,
    Subckt,
}

impl DeviceKind {
    pub const ALL: [DeviceKind; 10] = [
        DeviceKind::Mos,
        DeviceKind::Nmos,
        DeviceKind::Pmos,
        DeviceKind::Res,
        DeviceKind::Cap,
        DeviceKind::Diode,
        DeviceKind::Bjt,
        DeviceKind::Npn,
        DeviceKind::Pnp,
        DeviceKind::Subckt,
    ];

    /// The kind the `DeviceKind` variant `variant` names.
    pub fn from_variant(variant: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == variant)
    }

    /// The name of the `DeviceKind` variant.
    pub fn name(self) -> &'static str {
        match self {
            DeviceKind::Mos => "Mos",
            DeviceKind::Nmos => "Nmos",
            DeviceKind::Pmos => "Pmos",
            DeviceKind::Res => "Res",
            DeviceKind::Cap => "Cap",
            DeviceKind::Diode => "Diode",
            DeviceKind::Bjt => "Bjt",
            DeviceKind::Npn => "Npn",
            DeviceKind::Pnp => "Pnp",
            DeviceKind::Subckt => "Subckt",
        }
    }

    /// The kind that decides the device's terminals and netlist element:
    /// `Mos` for the MOS polarities, `Bjt` for the bipolar ones.
    pub fn base(self) -> Self {
        match self {
            DeviceKind::Nmos | DeviceKind::Pmos => DeviceKind::Mos,
            DeviceKind::Npn | DeviceKind::Pnp => DeviceKind::Bjt,
            other => other,
        }
    }

    /// The SPICE element letter.
    pub fn letter(self) -> char {
        match self.base() {
            DeviceKind::Mos => 'M',
            DeviceKind::Res => 'R',
            DeviceKind::Cap => 'C',
            DeviceKind::Diode => 'D',
            DeviceKind::Bjt => 'Q',
            DeviceKind::Subckt => 'X',
            DeviceKind::Nmos | DeviceKind::Pmos | DeviceKind::Npn | DeviceKind::Pnp => {
                unreachable!("a base kind")
            }
        }
    }

    /// The names of the terminals of a device of this kind with `count`
    /// terminals; empty for a kind whose terminals are only numbered.
    pub fn terminal_names(self, count: usize) -> &'static [&'static str] {
        match self.base() {
            DeviceKind::Mos => &["d", "g", "s", "b"],
            DeviceKind::Res | DeviceKind::Cap | DeviceKind::Diode => &["p", "n"],
            DeviceKind::Bjt if count == 4 => &["c", "b", "e", "sub"],
            DeviceKind::Bjt => &["c", "b", "e"],
            _ => &[],
        }
    }

    /// Every terminal name a `Device` value can have a field for.
    pub const TERMINAL_FIELDS: [&'static str; 9] = ["d", "g", "s", "b", "p", "n", "c", "e", "sub"];

    /// The name of terminal `index` of a device with `count` terminals, or
    /// its number for a kind without terminal names.
    pub fn terminal_label(self, index: usize, count: usize) -> String {
        self.terminal_names(count)
            .get(index)
            .map_or_else(|| index.to_string(), |name| (*name).to_owned())
    }

    /// Checks whether a device of this kind may have a model and a `value`
    /// as given, returning why it is invalid.
    pub(super) fn check_model(self, model: bool, value: bool) -> Result<(), String> {
        match self.base() {
            DeviceKind::Res | DeviceKind::Cap if !model && !value => {
                Err("requires a model or a `value`".to_owned())
            }
            DeviceKind::Res | DeviceKind::Cap => Ok(()),
            _ if !model => Err("requires a model".to_owned()),
            _ if value => Err("does not take a `value`".to_owned()),
            _ => Ok(()),
        }
    }

    /// Checks the terminal count of a device of this kind.
    pub fn check_terminals(self, terminals: usize) -> Result<(), String> {
        let (expected, ok) = match self.base() {
            DeviceKind::Mos => ("4 terminals (d, g, s, b)", terminals == 4),
            DeviceKind::Res | DeviceKind::Cap => ("2 terminals", terminals == 2),
            DeviceKind::Diode => ("2 terminals (anode, cathode)", terminals == 2),
            DeviceKind::Bjt => (
                "3 or 4 terminals (c, b, e[, substrate])",
                matches!(terminals, 3 | 4),
            ),
            DeviceKind::Subckt => ("at least 1 terminal", terminals >= 1),
            DeviceKind::Nmos | DeviceKind::Pmos | DeviceKind::Npn | DeviceKind::Pnp => {
                unreachable!("a base kind")
            }
        };
        if ok {
            Ok(())
        } else {
            Err(format!("expects {expected}, found {terminals}"))
        }
    }
}

/// The value of a device parameter.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum ParamValue {
    Float(f64),
    Int(i64),
    String(String),
}

/// A placeholder for a field value that the instance it was read through
/// cannot provide. Using it is an error; holding it is not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Detached {
    pub kind: DetachedKind,
    pub reason: DetachedReason,
    /// The name of the instance's cell.
    pub cell: String,
    /// The field read, with the path to the value within it.
    pub field: String,
    /// Where the field was read.
    pub span: Span,
}

/// What a [`Detached`] value stands for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetachedKind {
    Signal,
    Geometry,
    LayoutInstance,
    SchematicInstance,
    Device,
}

impl DetachedKind {
    fn name(self) -> &'static str {
        match self {
            DetachedKind::Signal => "signal",
            DetachedKind::Geometry => "shape",
            DetachedKind::LayoutInstance => "layout instance",
            DetachedKind::SchematicInstance => "schematic instance",
            DetachedKind::Device => "device",
        }
    }
}

/// Why a [`Detached`] value has nothing behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DetachedReason {
    /// A signal or schematic instance read through a layout instance.
    ThroughLayoutInstance,
    /// Geometry or a layout instance read through a schematic instance.
    ThroughSchematicInstance,
    /// A signal on a net that is not a port of the instance's cell.
    InternalNet,
}

impl std::fmt::Display for Detached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            kind,
            reason,
            cell,
            field,
            span: _,
        } = self;
        match reason {
            DetachedReason::ThroughSchematicInstance => write!(
                f,
                "`{field}` of `{cell}` was read through a schematic instance and has no \
                 placement; read it through `std::layout::inst(...)`"
            ),
            DetachedReason::ThroughLayoutInstance => write!(
                f,
                "`{field}` of `{cell}` is a {} read through a layout instance; read it through \
                 `std::schematic::inst(...)`",
                kind.name()
            ),
            DetachedReason::InternalNet => write!(
                f,
                "`{field}` is internal to `{cell}`'s schematic; connect it to a \
                 `pub let ... = Signal()` in `{cell}` to export it"
            ),
        }
    }
}

/// A node of a cell's schematic. `connect` joins nodes into nets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct NodeId(pub(super) u64);

/// A signal: a node of the cell that created it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SignalRef {
    pub cell: CellId,
    pub node: NodeId,
}

/// A schematic instance as a value.
#[derive(Debug, Clone)]
pub struct SchematicInstValue {
    /// Identifies an instance placed in the current cell; `None` for one read
    /// through another instance.
    pub id: Option<ObjectId>,
    pub cell: CellId,
    /// One `Value::Signal` or `Value::Detached` per port of `cell`.
    pub terminals: Vec<Value>,
}

/// A device as a value: a handle whose fields are its terminals.
#[derive(Debug, Clone)]
pub struct DeviceValue {
    /// Identifies a device placed in the current cell; `None` for one read
    /// through an instance.
    pub id: Option<ObjectId>,
    pub kind: DeviceKind,
    /// One `Value::Signal` or `Value::Detached` per terminal.
    pub terminals: Vec<Value>,
}

impl DeviceValue {
    /// The terminal the field `name` reads, if the device's kind has one.
    pub fn terminal(&self, name: &str) -> Option<&Value> {
        let index = self
            .kind
            .terminal_names(self.terminals.len())
            .iter()
            .position(|terminal| *terminal == name)?;
        self.terminals.get(index)
    }
}

/// Where a node came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum NodeOrigin {
    /// Created by `Signal()` in this cell, directly or in an inlined `fn`.
    Local {
        span: Span,
        placement: Option<Placement>,
        editable: bool,
    },
    /// Port `port` of the instance recorded at `instance`.
    Terminal { instance: usize, port: usize },
    /// Terminal `terminal` of the device recorded at `device`.
    DeviceTerminal { device: usize, terminal: usize },
}

impl NodeOrigin {
    fn is_local(&self) -> bool {
        matches!(self, NodeOrigin::Local { .. })
    }
}

#[derive(Debug, Clone)]
pub(super) struct NodeInfo {
    pub(super) origin: NodeOrigin,
    /// Orders nodes by source position, not by evaluation order: the creating
    /// call's value id, then the port of a terminal.
    pub(super) order: (ValueId, usize),
}

/// An instance placed by `std::schematic::inst` while its cell executes.
#[derive(Debug, Clone)]
pub(super) struct SchInstRecord {
    /// The creating call's value id.
    pub(super) order: ValueId,
    /// The id of the instance's value.
    pub(super) id: ObjectId,
    pub(super) cell: CellId,
    pub(super) terminals: Vec<NodeId>,
    pub(super) span: Span,
    pub(super) placement: Option<Placement>,
    pub(super) editable: bool,
}

/// A device placed by `std::schematic::device` while its cell executes.
#[derive(Debug, Clone)]
pub(super) struct DeviceRecord {
    /// The creating call's value id.
    pub(super) order: ValueId,
    /// The id of the device's value.
    pub(super) id: ObjectId,
    pub(super) kind: DeviceKind,
    /// The device's own node on each terminal.
    pub(super) terminals: Vec<NodeId>,
    /// The node passed for each terminal.
    pub(super) attached: Vec<NodeId>,
    pub(super) terminal_spans: Vec<Option<Span>>,
    pub(super) model: Option<String>,
    pub(super) value: Option<ParamValue>,
    pub(super) params: Vec<(String, ParamValue)>,
    pub(super) span: Span,
    pub(super) placement: Option<Placement>,
    pub(super) editable: bool,
}

/// A `connect` that ran while its cell executes.
#[derive(Debug, Clone)]
pub(super) struct ConnectRecord {
    /// The call's value id.
    pub(super) order: ValueId,
    pub(super) nodes: [NodeId; 2],
    pub(super) span: Span,
    pub(super) route: Option<Vec<(i64, i64)>>,
    pub(super) editable: bool,
}

/// The schematic of a cell while it executes.
#[derive(Debug, Clone, Default)]
pub(super) struct SchematicState {
    nodes: IndexMap<NodeId, NodeInfo>,
    /// Union-find over `nodes`, merged by `connect`. A root is its own parent.
    parent: IndexMap<NodeId, NodeId>,
    instances: Vec<SchInstRecord>,
    devices: Vec<DeviceRecord>,
    connections: Vec<ConnectRecord>,
    /// The `let` name that first bound each value, used for naming.
    name_hints: IndexMap<ValueId, String>,
}

impl SchematicState {
    pub(super) fn add_node(&mut self, node: NodeId, origin: NodeOrigin, order: (ValueId, usize)) {
        self.nodes.insert(node, NodeInfo { origin, order });
        self.parent.insert(node, node);
    }

    /// Whether `node` is a node of this schematic.
    pub(super) fn contains(&self, node: NodeId) -> bool {
        self.nodes.contains_key(&node)
    }

    /// The root of `node`'s class.
    fn root(&self, mut node: NodeId) -> NodeId {
        while self.parent[&node] != node {
            node = self.parent[&node];
        }
        node
    }

    /// The root of `node`'s class, compressing the path to it.
    fn find(&mut self, node: NodeId) -> NodeId {
        let root = self.root(node);
        let mut node = node;
        while node != root {
            let next = self.parent[&node];
            self.parent.insert(node, root);
            node = next;
        }
        root
    }

    /// Joins the nets of `a` and `b`, keeping the earlier node as the root.
    pub(super) fn connect(&mut self, a: NodeId, b: NodeId) {
        let (a, b) = (self.find(a), self.find(b));
        if a == b {
            return;
        }
        let (root, child) = if self.nodes[&a].order <= self.nodes[&b].order {
            (a, b)
        } else {
            (b, a)
        };
        self.parent.insert(child, root);
    }

    pub(super) fn add_connection(&mut self, record: ConnectRecord) {
        self.connections.push(record);
    }

    /// The index the next recorded instance takes.
    pub(super) fn next_instance(&self) -> usize {
        self.instances.len()
    }

    pub(super) fn add_instance(&mut self, record: SchInstRecord) {
        self.instances.push(record);
    }

    /// The index the next recorded device takes.
    pub(super) fn next_device(&self) -> usize {
        self.devices.len()
    }

    pub(super) fn add_device(&mut self, record: DeviceRecord) {
        self.devices.push(record);
    }

    /// The device recorded at `device`.
    pub(super) fn device(&self, device: usize) -> &DeviceRecord {
        &self.devices[device]
    }

    /// The device recorded at `device`, to finish once its parameters are
    /// ready.
    pub(super) fn device_mut(&mut self, device: usize) -> &mut DeviceRecord {
        &mut self.devices[device]
    }

    /// Records `name` as the name of `value`, unless it already has one.
    pub(super) fn hint(&mut self, value: ValueId, name: &str) {
        self.name_hints
            .entry(value)
            .or_insert_with(|| name.to_owned());
    }

    /// The spans of the editable calls that produced exactly one node,
    /// element, or connection each.
    fn unique_editable_spans(&self) -> HashSet<&Span> {
        let mut counts: HashMap<&Span, usize> = HashMap::new();
        let spans = self
            .nodes
            .values()
            .filter_map(|info| match &info.origin {
                NodeOrigin::Local {
                    span,
                    editable: true,
                    ..
                } => Some(span),
                _ => None,
            })
            .chain(
                self.instances
                    .iter()
                    .filter(|record| record.editable)
                    .map(|record| &record.span),
            )
            .chain(
                self.devices
                    .iter()
                    .filter(|record| record.editable)
                    .map(|record| &record.span),
            )
            .chain(
                self.connections
                    .iter()
                    .filter(|record| record.editable)
                    .map(|record| &record.span),
            );
        for span in spans {
            *counts.entry(span).or_default() += 1;
        }
        counts
            .into_iter()
            .filter(|(_, count)| *count == 1)
            .map(|(span, _)| span)
            .collect()
    }

    /// Builds the compiled schematic of `cell`, whose top-level `let`s are
    /// `fields` as `(name, value, public)` in declaration order, and the net of
    /// every node. `cells` holds the compiled cells this one instantiates.
    pub(super) fn emit(
        &self,
        cell: CellId,
        fields: &[(&str, &Value, bool)],
        cells: &IndexMap<CellId, Arc<CompiledCell>>,
    ) -> (Schematic, HashMap<NodeId, NetIdx>) {
        // Every device has a terminal, so a cell without nodes or instances
        // has no schematic.
        if self.nodes.is_empty() && self.instances.is_empty() {
            return (Schematic::default(), HashMap::new());
        }
        // One net per class, ordered by the class's earliest node, which is
        // its root.
        let mut roots = self
            .nodes
            .keys()
            .copied()
            .filter(|node| self.root(*node) == *node)
            .collect::<Vec<_>>();
        roots.sort_by_key(|node| self.nodes[node].order);
        let net_of_root = roots
            .iter()
            .enumerate()
            .map(|(net, root)| (*root, net as NetIdx))
            .collect::<HashMap<_, _>>();
        let net_of = self
            .nodes
            .keys()
            .map(|node| (*node, net_of_root[&self.root(*node)]))
            .collect::<HashMap<_, _>>();
        let mut members = vec![Vec::new(); roots.len()];
        for node in self.nodes.keys() {
            members[net_of[node] as usize].push(*node);
        }
        for nodes in &mut members {
            nodes.sort_by_key(|node| self.nodes[node].order);
        }

        // Nodes in source order.
        let mut node_order = self.nodes.keys().copied().collect::<Vec<_>>();
        node_order.sort_by_key(|node| self.nodes[node].order);
        let node_idx = node_order
            .iter()
            .enumerate()
            .map(|(index, node)| (*node, index as NodeIdx))
            .collect::<HashMap<_, _>>();

        // Instances and devices in creation order.
        let mut instance_order = (0..self.instances.len()).collect::<Vec<_>>();
        instance_order.sort_by_key(|&record| self.instances[record].order);
        let mut device_order = (0..self.devices.len()).collect::<Vec<_>>();
        device_order.sort_by_key(|&record| self.devices[record].order);
        let mut instance_position = vec![0; self.instances.len()];
        for (position, record) in instance_order.iter().enumerate() {
            instance_position[*record] = position;
        }
        let mut device_position = vec![0; self.devices.len()];
        for (position, record) in device_order.iter().enumerate() {
            device_position[*record] = position;
        }
        let port_name = |cell: CellId, port: usize| match cells.get(&cell) {
            Some(cell) => cell.schematic.port_name(port).to_owned(),
            None => port.to_string(),
        };
        let signal_node = |signal: &SignalRef| {
            (signal.cell == cell)
                .then(|| self.nodes.get(&signal.node).map(|info| (signal.node, info)))
                .flatten()
        };

        let mut paths = PathTable::default();

        // Ports: the nets of the signals this cell created that its public
        // fields hold, in the order they are first reached.
        let mut ports = Vec::new();
        let mut port_names: HashMap<NetIdx, Best<String>> = HashMap::new();
        let mut port_paths: HashMap<NetIdx, Best<PathId>> = HashMap::new();
        let mut public_nodes = HashSet::new();
        let mut rank = 0;
        for (name, value, _) in fields.iter().filter(|(_, _, public)| *public) {
            walk(
                value,
                &mut vec![Segment::Binding(name)],
                &mut |leaf, path| {
                    let Leaf::Signal(signal) = leaf else {
                        return;
                    };
                    let Some((node, info)) = signal_node(&signal) else {
                        return;
                    };
                    public_nodes.insert(node);
                    if !info.origin.is_local() {
                        return;
                    }
                    let net = net_of[&node];
                    let ranking = Ranking::of(path, rank);
                    port_names
                        .entry(net)
                        .or_insert_with(|| {
                            ports.push(net);
                            Best::default()
                        })
                        .improve(ranking, || join(path));
                    if nameable(path) {
                        port_paths.entry(net).or_default().improve(ranking, || {
                            paths.intern(path).expect("the path is nameable")
                        });
                    }
                    rank += 1;
                },
            );
        }

        // Every path from a top-level `let` to a node, and to an instance.
        let mut local: HashMap<NetIdx, Best<String>> = HashMap::new();
        let mut terminal: HashMap<NetIdx, Best<String>> = HashMap::new();
        let mut node_paths: HashMap<NodeId, Best<PathId>> = HashMap::new();
        let mut instance_paths: HashMap<ObjectId, (String, Option<PathId>)> = HashMap::new();
        let mut device_paths: HashMap<ObjectId, (String, Option<PathId>)> = HashMap::new();
        for (name, value, _) in fields {
            walk(
                value,
                &mut vec![Segment::Binding(name)],
                &mut |leaf, path| match leaf {
                    Leaf::Signal(signal) => {
                        let Some((node, info)) = signal_node(&signal) else {
                            return;
                        };
                        let tier = if info.origin.is_local() {
                            &mut local
                        } else {
                            &mut terminal
                        };
                        let ranking = Ranking::of(path, rank);
                        tier.entry(net_of[&node])
                            .or_default()
                            .improve(ranking, || join(path));
                        if nameable(path) {
                            node_paths.entry(node).or_default().improve(ranking, || {
                                paths.intern(path).expect("the path is nameable")
                            });
                        }
                        rank += 1;
                    }
                    Leaf::Instance(instance) => {
                        let instance_path = paths.intern(path);
                        if let Some(id) = instance.id {
                            instance_paths
                                .entry(id)
                                .or_insert_with(|| (join(path), instance_path));
                        }
                        let child = cells.get(&instance.cell);
                        for (port, value) in instance.terminals.iter().enumerate() {
                            let Value::Signal(signal) = value else {
                                continue;
                            };
                            let Some((node, _)) = signal_node(signal) else {
                                continue;
                            };
                            let name = format!("{}_{}", join(path), port_name(instance.cell, port));
                            let ranking = Ranking::new(path.len(), name.chars().count(), rank);
                            self::terminal_tier(&mut terminal, net_of[&node], ranking, name);
                            if let (Some(parent), Some(child)) = (instance_path, child)
                                && let Some(Some(port_path)) = child.schematic.port_paths.get(port)
                            {
                                node_paths.entry(node).or_default().improve(ranking, || {
                                    paths.append(parent, &child.schematic, *port_path)
                                });
                            }
                            rank += 1;
                        }
                    }
                    Leaf::Device(device) => {
                        let device_path = paths.intern(path);
                        if let Some(id) = device.id {
                            device_paths
                                .entry(id)
                                .or_insert_with(|| (join(path), device_path));
                        }
                        let count = device.terminals.len();
                        for (index, value) in device.terminals.iter().enumerate() {
                            let Value::Signal(signal) = value else {
                                continue;
                            };
                            let Some((node, _)) = signal_node(signal) else {
                                continue;
                            };
                            let label = device.kind.terminal_label(index, count);
                            let name = format!("{}_{label}", join(path));
                            let ranking = Ranking::new(path.len(), name.chars().count(), rank);
                            self::terminal_tier(&mut terminal, net_of[&node], ranking, name);
                            if let Some(parent) = device_path {
                                node_paths.entry(node).or_default().improve(ranking, || {
                                    paths.terminal(parent, device.kind, index, count)
                                });
                            }
                            rank += 1;
                        }
                    }
                },
            );
        }

        // Element names, unique together in creation order.
        let mut elements = instance_order
            .iter()
            .enumerate()
            .map(|(position, record)| (self.instances[*record].order, Element::Instance(position)))
            .chain(
                device_order.iter().enumerate().map(|(position, record)| {
                    (self.devices[*record].order, Element::Device(position))
                }),
            )
            .collect::<Vec<_>>();
        elements.sort_by_key(|(order, _)| *order);
        let elements = elements
            .into_iter()
            .map(|(_, element)| element)
            .collect::<Vec<_>>();
        let mut element_names = Namer::default();
        let mut instance_names = vec![String::new(); instance_order.len()];
        let mut device_names = vec![String::new(); device_order.len()];
        // Whether each device is named after a binding rather than its index.
        let mut device_bound = vec![false; device_order.len()];
        for element in &elements {
            match *element {
                Element::Instance(position) => {
                    let record = &self.instances[instance_order[position]];
                    let name = instance_paths
                        .get(&record.id)
                        .map(|(name, _)| name)
                        .or_else(|| self.name_hints.get(&record.order))
                        .cloned()
                        .unwrap_or_else(|| format!("inst{position}"));
                    instance_names[position] = element_names.claim(&format!("X{name}"));
                }
                Element::Device(position) => {
                    let record = &self.devices[device_order[position]];
                    let name = device_paths
                        .get(&record.id)
                        .map(|(name, _)| name)
                        .or_else(|| self.name_hints.get(&record.order))
                        .cloned();
                    device_bound[position] = name.is_some();
                    let name = name.unwrap_or_else(|| position.to_string());
                    device_names[position] =
                        element_names.claim(&format!("{}{name}", record.kind.letter()));
                }
            }
        }

        // Net names: ports by their shortest path; other nets by the first
        // tier with a candidate, each tier preferring its shortest.
        let mut names = vec![None; roots.len()];
        for (net, candidate) in &port_names {
            names[*net as usize] = candidate.value().cloned();
        }
        for (net, nodes) in members.iter().enumerate() {
            if names[net].is_some() {
                continue;
            }
            let net_idx = net as NetIdx;
            let hinted = || {
                let mut best = Best::default();
                for (rank, node) in nodes.iter().enumerate() {
                    let info = &self.nodes[node];
                    let (delimiters, name) = match &info.origin {
                        NodeOrigin::Local { .. } => match self.name_hints.get(&info.order.0) {
                            Some(hint) => (0, hint.clone()),
                            None => continue,
                        },
                        NodeOrigin::Terminal { instance, port } => {
                            let position = instance_position[*instance];
                            let record = &self.instances[*instance];
                            let instance = &instance_names[position]["X".len()..];
                            let port = port_name(record.cell, *port);
                            (1, format!("{instance}_{port}"))
                        }
                        NodeOrigin::DeviceTerminal { device, terminal } => {
                            let position = device_position[*device];
                            let record = &self.devices[*device];
                            let name = &device_names[position];
                            // Named after a binding, without the element letter.
                            let device = if device_bound[position] {
                                &name[record.kind.letter().len_utf8()..]
                            } else {
                                name.as_str()
                            };
                            let terminal = record
                                .kind
                                .terminal_label(*terminal, record.terminals.len());
                            (1, format!("{device}_{terminal}"))
                        }
                    };
                    best.improve(Ranking::new(delimiters, name.chars().count(), rank), || {
                        name
                    });
                }
                best.into_value()
            };
            names[net] = local
                .get(&net_idx)
                .or_else(|| terminal.get(&net_idx))
                .and_then(|best| best.value().cloned())
                .or_else(hinted)
                .or_else(|| Some(format!("net{net}")));
        }
        let mut net_names = Namer::default();
        let mut unique = vec![String::new(); roots.len()];
        let others = (0..roots.len() as NetIdx).filter(|net| !port_names.contains_key(net));
        for net in ports.iter().copied().chain(others) {
            let name = names[net as usize].as_deref().expect("every net is named");
            unique[net as usize] = net_names.claim(name);
        }

        let unique_editable = self.unique_editable_spans();
        let editable = |editable: bool, span: &Span| editable && unique_editable.contains(span);
        let nets_of =
            |nodes: &[NodeId]| -> Vec<NetIdx> { nodes.iter().map(|node| net_of[node]).collect() };
        let indices_of = |nodes: &[NodeId]| -> Vec<NodeIdx> {
            nodes.iter().map(|node| node_idx[node]).collect()
        };
        let nodes = node_order
            .iter()
            .map(|node| {
                let info = &self.nodes[node];
                let source = match &info.origin {
                    NodeOrigin::Local {
                        span,
                        placement,
                        editable: site,
                    } => NodeSource::Signal {
                        span: span.clone(),
                        placement: *placement,
                        editable: editable(*site, span),
                    },
                    NodeOrigin::Terminal { instance, port } => NodeSource::Terminal {
                        instance: instance_position[*instance],
                        port: *port,
                    },
                    NodeOrigin::DeviceTerminal { device, terminal } => NodeSource::DeviceTerminal {
                        device: device_position[*device],
                        terminal: *terminal,
                    },
                };
                Node {
                    net: net_of[node],
                    source,
                    path: node_paths.get(node).and_then(|best| best.value().copied()),
                    public: public_nodes.contains(node),
                }
            })
            .collect();
        let connections = self
            .connections
            .iter()
            .sorted_by_key(|record| record.order)
            .map(|record| Connection {
                nodes: record.nodes.map(|node| node_idx[&node]),
                span: record.span.clone(),
                route: record.route.clone(),
                editable: editable(record.editable, &record.span),
            })
            .collect();
        let schematic = Schematic {
            nets: unique.into_iter().map(|name| Net { name }).collect(),
            port_paths: ports
                .iter()
                .map(|net| port_paths.get(net).and_then(|best| best.value().copied()))
                .collect(),
            ports,
            instances: instance_order
                .iter()
                .zip(instance_names)
                .map(|(record, name)| {
                    let record = &self.instances[*record];
                    SchematicInstance {
                        name,
                        cell: record.cell,
                        terminals: nets_of(&record.terminals),
                        span: Some(record.span.clone()),
                        placement: record.placement,
                        editable: editable(record.editable, &record.span),
                        path: instance_paths.get(&record.id).and_then(|(_, path)| *path),
                        terminal_nodes: indices_of(&record.terminals),
                    }
                })
                .collect(),
            devices: device_order
                .iter()
                .zip(device_names)
                .map(|(record, name)| {
                    let record = &self.devices[*record];
                    Device {
                        name,
                        kind: record.kind,
                        terminals: nets_of(&record.terminals),
                        model: record.model.clone(),
                        value: record.value.clone(),
                        params: record.params.clone(),
                        span: Some(record.span.clone()),
                        placement: record.placement,
                        editable: editable(record.editable, &record.span),
                        path: device_paths.get(&record.id).and_then(|(_, path)| *path),
                        terminal_nodes: indices_of(&record.terminals),
                        attached_nodes: indices_of(&record.attached),
                        terminal_spans: record.terminal_spans.clone(),
                    }
                })
                .collect(),
            elements,
            nodes,
            connections,
            paths: paths.nodes,
        };
        (schematic, net_of)
    }
}

/// A signal, schematic instance, or device found in a value.
enum Leaf<'v> {
    Signal(SignalRef),
    Instance(&'v SchematicInstValue),
    Device(&'v DeviceValue),
}

/// Offers `name` for the net `net` in the terminal tier `tier`.
fn terminal_tier(
    tier: &mut HashMap<NetIdx, Best<String>>,
    net: NetIdx,
    ranking: Ranking,
    name: String,
) {
    tier.entry(net).or_default().improve(ranking, || name);
}

/// One step of a path into a value.
#[derive(Debug, Clone, Copy)]
enum Segment<'v> {
    /// A top-level `let`.
    Binding(&'v str),
    /// A struct field.
    Field(&'v str),
    /// An element of a sequence.
    Element(usize),
    /// An element of a tuple.
    Tuple(usize),
    /// An element of a tuple variant's payload, which no path can name.
    Payload(usize),
    /// A field of a struct variant's payload, which no path can name.
    PayloadField(&'v str),
}

impl Segment<'_> {
    /// How many characters the segment adds to a name.
    fn chars(&self) -> usize {
        match self {
            Segment::Binding(name) | Segment::Field(name) | Segment::PayloadField(name) => {
                name.chars().count()
            }
            Segment::Element(index) | Segment::Tuple(index) | Segment::Payload(index) => {
                index.checked_ilog10().unwrap_or(0) as usize + 1
            }
        }
    }
}

impl std::fmt::Display for Segment<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Segment::Binding(name) | Segment::Field(name) | Segment::PayloadField(name) => {
                write!(f, "{name}")
            }
            Segment::Element(index) | Segment::Tuple(index) | Segment::Payload(index) => {
                write!(f, "{index}")
            }
        }
    }
}

/// `path` joined with `_`.
fn join(path: &[Segment<'_>]) -> String {
    path.iter().join("_")
}

/// Whether code can name the value at `path`, which it cannot inside an enum
/// payload.
fn nameable(path: &[Segment<'_>]) -> bool {
    !path
        .iter()
        .any(|segment| matches!(segment, Segment::Payload(_) | Segment::PayloadField(_)))
}

/// Paths into a cell's values, interned so that paths with a common prefix
/// share it.
#[derive(Default)]
struct PathTable {
    nodes: Vec<PathNode>,
    ids: HashMap<PathNode, PathId>,
}

impl PathTable {
    /// The id of `segment` under `parent`, added if new.
    fn node(&mut self, parent: Option<PathId>, segment: PathSegment) -> PathId {
        let node = PathNode { parent, segment };
        if let Some(id) = self.ids.get(&node) {
            return *id;
        }
        let id = self.nodes.len() as PathId;
        self.nodes.push(node.clone());
        self.ids.insert(node, id);
        id
    }

    /// Interns `path`, unless it is not [`nameable`].
    fn intern(&mut self, path: &[Segment<'_>]) -> Option<PathId> {
        let mut parent = None;
        for segment in path {
            let segment = match *segment {
                Segment::Binding(name) => PathSegment::Binding(name.to_owned()),
                Segment::Field(name) => PathSegment::Field(name.to_owned()),
                Segment::Element(index) => PathSegment::Element(index),
                Segment::Tuple(index) => PathSegment::Tuple(index),
                Segment::Payload(_) | Segment::PayloadField(_) => return None,
            };
            parent = Some(self.node(parent, segment));
        }
        parent
    }

    /// The path of terminal `terminal` of a device of `kind` at `parent`:
    /// its field, or an element of `terminals` for a kind without terminal
    /// names.
    fn terminal(
        &mut self,
        parent: PathId,
        kind: DeviceKind,
        terminal: usize,
        count: usize,
    ) -> PathId {
        match kind.terminal_names(count).get(terminal) {
            Some(name) => self.node(Some(parent), PathSegment::Terminal((*name).to_owned())),
            None => {
                let terminals = self.node(Some(parent), PathSegment::Field("terminals".to_owned()));
                self.node(Some(terminals), PathSegment::Element(terminal))
            }
        }
    }

    /// `parent` followed by `path` of `child`, whose binding becomes a field.
    fn append(&mut self, parent: PathId, child: &Schematic, path: PathId) -> PathId {
        let mut id = parent;
        for segment in child.path_segments(path) {
            let segment = match segment {
                PathSegment::Binding(name) | PathSegment::Field(name) => {
                    PathSegment::Field(name.clone())
                }
                other => other.clone(),
            };
            id = self.node(Some(id), segment);
        }
        id
    }
}

/// Calls `visit` on every signal, schematic instance, and device in `value`,
/// with the path that reaches it. Terminals are not visited.
fn walk<'v>(
    value: &'v Value,
    path: &mut Vec<Segment<'v>>,
    visit: &mut dyn FnMut(Leaf<'v>, &[Segment<'v>]),
) {
    fn enter<'v>(
        segment: Segment<'v>,
        value: &'v Value,
        path: &mut Vec<Segment<'v>>,
        visit: &mut dyn FnMut(Leaf<'v>, &[Segment<'v>]),
    ) {
        path.push(segment);
        walk(value, path, visit);
        path.pop();
    }
    match value {
        Value::Signal(signal) => visit(Leaf::Signal(*signal), path),
        Value::SchematicInst(instance) => visit(Leaf::Instance(instance), path),
        Value::Device(device) => visit(Leaf::Device(device), path),
        Value::Seq(items) => {
            for (index, item) in items.iter().enumerate() {
                enter(Segment::Element(index), item, path, visit);
            }
        }
        Value::Tuple(items) => {
            for (index, item) in items.iter().enumerate() {
                enter(Segment::Tuple(index), item, path, visit);
            }
        }
        Value::Struct(value) => {
            for (name, field) in &value.fields {
                enter(Segment::Field(name), field, path, visit);
            }
        }
        Value::Enum(value) => match &value.payload {
            VariantValues::Tuple(items) => {
                for (index, item) in items.iter().enumerate() {
                    enter(Segment::Payload(index), item, path, visit);
                }
            }
            VariantValues::Struct(fields) => {
                for (name, field) in fields {
                    enter(Segment::PayloadField(name), field, path, visit);
                }
            }
        },
        _ => {}
    }
}

/// Orders candidate names shortest first: by hierarchical delimiters, then
/// characters, then the order they were found in.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Ranking {
    delimiters: usize,
    chars: usize,
    found: usize,
}

impl Ranking {
    fn new(delimiters: usize, chars: usize, found: usize) -> Self {
        Self {
            delimiters,
            chars,
            found,
        }
    }

    /// The ranking of the name joining `path` with `_`.
    fn of(path: &[Segment<'_>], found: usize) -> Self {
        let delimiters = path.len().saturating_sub(1);
        let chars = path.iter().map(Segment::chars).sum::<usize>() + delimiters;
        Self::new(delimiters, chars, found)
    }
}

/// The best-ranked candidate found so far.
#[derive(Debug, Clone)]
struct Best<T>(Option<(Ranking, T)>);

impl<T> Default for Best<T> {
    fn default() -> Self {
        Self(None)
    }
}

impl<T> Best<T> {
    /// Replaces the candidate with `value` if `ranking` beats its ranking.
    fn improve(&mut self, ranking: Ranking, value: impl FnOnce() -> T) {
        if self.0.as_ref().is_none_or(|(best, _)| ranking < *best) {
            self.0 = Some((ranking, value()));
        }
    }

    fn value(&self) -> Option<&T> {
        self.0.as_ref().map(|(_, value)| value)
    }

    fn into_value(self) -> Option<T> {
        self.0.map(|(_, value)| value)
    }
}

/// Makes names unique case-insensitively, appending `_1`, `_2`, ... to a name
/// that is already taken.
#[derive(Debug, Default)]
pub(crate) struct Namer {
    taken: HashSet<String>,
}

impl Namer {
    /// A namer that never hands out any of `reserved`.
    pub(crate) fn with_reserved<'a>(reserved: impl IntoIterator<Item = &'a str>) -> Self {
        Self {
            taken: reserved.into_iter().map(str::to_ascii_lowercase).collect(),
        }
    }

    /// `name`, or the first free name with a numeric suffix, which is then
    /// taken.
    pub(crate) fn claim(&mut self, name: &str) -> String {
        if self.taken.insert(name.to_ascii_lowercase()) {
            return name.to_owned();
        }
        (1..)
            .map(|suffix| format!("{name}_{suffix}"))
            .find(|candidate| self.taken.insert(candidate.to_ascii_lowercase()))
            .expect("some suffix is free")
    }
}
