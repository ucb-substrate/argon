//! The schematic view of a cell: signals joined into nets by `connect`, the
//! ports an instance of the cell exposes, and the instances and devices that
//! connect to its nets.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use indexmap::IndexMap;
use itertools::Itertools;
use serde::{Deserialize, Serialize};

use crate::ast::Span;

use super::{CellId, CompiledCell, ObjectId, Value, ValueId, VariantValues};

/// The index of a net in [`Schematic::nets`].
pub type NetIdx = u32;

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

/// A cell placed with `std::schematic::inst`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchematicInstance {
    /// The element name, such as `Xnmos`.
    pub name: String,
    pub cell: CellId,
    /// The net connected to each port of `cell`.
    pub terminals: Vec<NetIdx>,
    pub span: Option<Span>,
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
}

/// The kind of a [`Device`], mirroring `std::schematic::DeviceKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum DeviceKind {
    Mos,
    Res,
    Cap,
    Diode,
    Bjt,
    Subckt,
}

impl DeviceKind {
    pub const ALL: [DeviceKind; 6] = [
        DeviceKind::Mos,
        DeviceKind::Res,
        DeviceKind::Cap,
        DeviceKind::Diode,
        DeviceKind::Bjt,
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
            DeviceKind::Res => "Res",
            DeviceKind::Cap => "Cap",
            DeviceKind::Diode => "Diode",
            DeviceKind::Bjt => "Bjt",
            DeviceKind::Subckt => "Subckt",
        }
    }

    /// The SPICE element letter.
    pub fn letter(self) -> char {
        match self {
            DeviceKind::Mos => 'M',
            DeviceKind::Res => 'R',
            DeviceKind::Cap => 'C',
            DeviceKind::Diode => 'D',
            DeviceKind::Bjt => 'Q',
            DeviceKind::Subckt => 'X',
        }
    }

    /// Checks the terminal count of a device of this kind and whether it has
    /// a model and a `value`, returning why the device is invalid.
    pub(super) fn check(self, terminals: usize, model: bool, value: bool) -> Result<(), String> {
        let (expected, ok) = match self {
            DeviceKind::Mos => ("4 terminals (d, g, s, b)", terminals == 4),
            DeviceKind::Res | DeviceKind::Cap => ("2 terminals", terminals == 2),
            DeviceKind::Diode => ("2 terminals (anode, cathode)", terminals == 2),
            DeviceKind::Bjt => (
                "3 or 4 terminals (c, b, e[, substrate])",
                matches!(terminals, 3 | 4),
            ),
            DeviceKind::Subckt => ("at least 1 terminal", terminals >= 1),
        };
        if !ok {
            return Err(format!("expects {expected}, found {terminals}"));
        }
        match self {
            DeviceKind::Res | DeviceKind::Cap if !model && !value => {
                Err("requires a model or a `value`".to_owned())
            }
            DeviceKind::Res | DeviceKind::Cap => Ok(()),
            _ if !model => Err("requires a model".to_owned()),
            _ if value => Err("does not take a `value`".to_owned()),
            _ => Ok(()),
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
}

impl DetachedKind {
    fn name(self) -> &'static str {
        match self {
            DetachedKind::Signal => "signal",
            DetachedKind::Geometry => "shape",
            DetachedKind::LayoutInstance => "layout instance",
            DetachedKind::SchematicInstance => "schematic instance",
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

/// Where a node came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum NodeOrigin {
    /// Created by `Signal()` in this cell, directly or in an inlined `fn`.
    Local,
    /// Port `port` of the instance recorded at `instance`.
    Terminal { instance: usize, port: usize },
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
}

/// A device placed by `std::schematic::device` while its cell executes.
#[derive(Debug, Clone)]
pub(super) struct DeviceRecord {
    /// The creating call's value id.
    pub(super) order: ValueId,
    pub(super) kind: DeviceKind,
    pub(super) terminals: Vec<NodeId>,
    pub(super) model: Option<String>,
    pub(super) value: Option<ParamValue>,
    pub(super) params: Vec<(String, ParamValue)>,
    pub(super) span: Span,
}

/// The schematic of a cell while it executes.
#[derive(Debug, Clone, Default)]
pub(super) struct SchematicState {
    nodes: IndexMap<NodeId, NodeInfo>,
    /// Union-find over `nodes`, merged by `connect`. A root is its own parent.
    parent: IndexMap<NodeId, NodeId>,
    instances: Vec<SchInstRecord>,
    devices: Vec<DeviceRecord>,
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

    /// The index the next recorded instance takes.
    pub(super) fn next_instance(&self) -> usize {
        self.instances.len()
    }

    pub(super) fn add_instance(&mut self, record: SchInstRecord) {
        self.instances.push(record);
    }

    pub(super) fn add_device(&mut self, record: DeviceRecord) {
        self.devices.push(record);
    }

    /// Records `name` as the name of `value`, unless it already has one.
    pub(super) fn hint(&mut self, value: ValueId, name: &str) {
        self.name_hints
            .entry(value)
            .or_insert_with(|| name.to_owned());
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

        // Instances and devices in creation order.
        let mut instance_order = (0..self.instances.len()).collect::<Vec<_>>();
        instance_order.sort_by_key(|&record| self.instances[record].order);
        let mut device_order = (0..self.devices.len()).collect::<Vec<_>>();
        device_order.sort_by_key(|&record| self.devices[record].order);
        let mut instance_position = vec![0; self.instances.len()];
        for (position, record) in instance_order.iter().enumerate() {
            instance_position[*record] = position;
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

        // Ports: the nets of the signals this cell created that its public
        // fields hold, in the order they are first reached.
        let mut ports = Vec::new();
        let mut port_names = HashMap::new();
        let mut rank = 0;
        for (name, value, _) in fields.iter().filter(|(_, _, public)| *public) {
            walk(value, &mut vec![Segment::Name(name)], &mut |leaf, path| {
                let Leaf::Signal(signal) = leaf else {
                    return;
                };
                let Some((node, info)) = signal_node(&signal) else {
                    return;
                };
                if info.origin != NodeOrigin::Local {
                    return;
                }
                let net = net_of[&node];
                let best = port_names.entry(net).or_insert_with(|| {
                    ports.push(net);
                    Candidate::default()
                });
                Candidate::from_path(path, rank).improve(best);
                rank += 1;
            });
        }

        // Every path from a top-level `let` to a node, and to an instance.
        let mut local = HashMap::new();
        let mut terminal = HashMap::new();
        let mut instance_paths = HashMap::new();
        for (name, value, _) in fields {
            walk(
                value,
                &mut vec![Segment::Name(name)],
                &mut |leaf, path| match leaf {
                    Leaf::Signal(signal) => {
                        let Some((node, info)) = signal_node(&signal) else {
                            return;
                        };
                        let tier = match info.origin {
                            NodeOrigin::Local => &mut local,
                            NodeOrigin::Terminal { .. } => &mut terminal,
                        };
                        Candidate::from_path(path, rank)
                            .improve(tier.entry(net_of[&node]).or_default());
                        rank += 1;
                    }
                    Leaf::Instance(instance) => {
                        if let Some(id) = instance.id {
                            instance_paths.entry(id).or_insert_with(|| join(path));
                        }
                        for (port, value) in instance.terminals.iter().enumerate() {
                            let Value::Signal(signal) = value else {
                                continue;
                            };
                            let Some((node, _)) = signal_node(signal) else {
                                continue;
                            };
                            let name = format!("{}_{}", join(path), port_name(instance.cell, port));
                            Candidate::new(path.len(), name, rank)
                                .improve(terminal.entry(net_of[&node]).or_default());
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
        for element in &elements {
            match *element {
                Element::Instance(position) => {
                    let record = &self.instances[instance_order[position]];
                    let name = instance_paths
                        .get(&record.id)
                        .or_else(|| self.name_hints.get(&record.order))
                        .cloned()
                        .unwrap_or_else(|| format!("inst{position}"));
                    instance_names[position] = element_names.claim(&format!("X{name}"));
                }
                Element::Device(position) => {
                    let record = &self.devices[device_order[position]];
                    device_names[position] =
                        element_names.claim(&format!("{}{position}", record.kind.letter()));
                }
            }
        }

        // Net names: ports by their shortest path; other nets by the first
        // tier with a candidate, each tier preferring its shortest.
        let mut names = vec![None; roots.len()];
        for (net, candidate) in &port_names {
            names[*net as usize] = candidate.clone().into_name();
        }
        for (net, nodes) in members.iter().enumerate() {
            if names[net].is_some() {
                continue;
            }
            let net_idx = net as NetIdx;
            let hinted = || {
                let mut best = Candidate::default();
                for (rank, node) in nodes.iter().enumerate() {
                    let info = &self.nodes[node];
                    let candidate = match info.origin {
                        NodeOrigin::Local => match self.name_hints.get(&info.order.0) {
                            Some(hint) => Candidate::new(0, hint.clone(), rank),
                            None => continue,
                        },
                        NodeOrigin::Terminal { instance, port } => {
                            let position = instance_position[instance];
                            let record = &self.instances[instance];
                            let instance = &instance_names[position]["X".len()..];
                            let port = port_name(record.cell, port);
                            Candidate::new(1, format!("{instance}_{port}"), rank)
                        }
                    };
                    candidate.improve(&mut best);
                }
                best.into_name()
            };
            names[net] = local
                .get(&net_idx)
                .or_else(|| terminal.get(&net_idx))
                .cloned()
                .and_then(Candidate::into_name)
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

        let nets_of =
            |nodes: &[NodeId]| -> Vec<NetIdx> { nodes.iter().map(|node| net_of[node]).collect() };
        let schematic = Schematic {
            nets: unique.into_iter().map(|name| Net { name }).collect(),
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
                    }
                })
                .collect(),
            elements,
        };
        (schematic, net_of)
    }
}

/// A signal or schematic instance found in a value.
enum Leaf<'v> {
    Signal(SignalRef),
    Instance(&'v SchematicInstValue),
}

/// One step of a path into a value: an element's index or a field's name.
#[derive(Debug, Clone, Copy)]
enum Segment<'v> {
    Index(usize),
    Name(&'v str),
}

impl std::fmt::Display for Segment<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Segment::Index(index) => write!(f, "{index}"),
            Segment::Name(name) => write!(f, "{name}"),
        }
    }
}

/// `path` joined with `_`.
fn join(path: &[Segment<'_>]) -> String {
    path.iter().join("_")
}

/// Calls `visit` on every signal and schematic instance in `value`, with the
/// path that reaches it. A schematic instance's terminals are not visited.
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
    let (items, fields) = match value {
        Value::Signal(signal) => return visit(Leaf::Signal(*signal), path),
        Value::SchematicInst(instance) => return visit(Leaf::Instance(instance), path),
        Value::Seq(items) => {
            for (index, item) in items.iter().enumerate() {
                enter(Segment::Index(index), item, path, visit);
            }
            return;
        }
        Value::Tuple(items) => (Some(items), None),
        Value::Struct(value) => (None, Some(&value.fields)),
        Value::Enum(value) => match &value.payload {
            VariantValues::Tuple(items) => (Some(items), None),
            VariantValues::Struct(fields) => (None, Some(fields)),
        },
        _ => return,
    };
    for (index, item) in items.into_iter().flatten().enumerate() {
        enter(Segment::Index(index), item, path, visit);
    }
    for (name, field) in fields.into_iter().flatten() {
        enter(Segment::Name(name), field, path, visit);
    }
}

/// A candidate name for a net, ranked shortest first: by hierarchical
/// delimiters, then characters, then the order it was found in.
#[derive(Debug, Clone, Default)]
struct Candidate(Option<(usize, String, usize)>);

impl Candidate {
    fn new(delimiters: usize, name: String, rank: usize) -> Self {
        Self(Some((delimiters, name, rank)))
    }

    /// The name joining `path` with `_`.
    fn from_path(path: &[Segment<'_>], rank: usize) -> Self {
        Self::new(path.len().saturating_sub(1), join(path), rank)
    }

    fn key(&self) -> Option<(usize, usize, usize)> {
        self.0
            .as_ref()
            .map(|(delimiters, name, rank)| (*delimiters, name.chars().count(), *rank))
    }

    /// Replaces `best` with this candidate if this one is shorter.
    fn improve(self, best: &mut Candidate) {
        let Some(key) = self.key() else {
            return;
        };
        if best.key().is_none_or(|best| key < best) {
            *best = self;
        }
    }

    fn into_name(self) -> Option<String> {
        self.0.map(|(_, name, _)| name)
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
