//! A compiled cell's conductors, joined into nets through the technology's
//! vias, and checks of those nets against the names the source gives them
//! with `net=` tags and pin labels.
//!
//! A cell is checked against its own tags and labels, and against its
//! subcells' where an instance's `nets=` maps them to the cell's signals.
//! Other names in subcells are checked when those cells are.

use std::collections::HashMap;

use std::cell::RefCell;

use crate::ast::Span;
use crate::compile::{
    CellId, CompiledCell, CompiledData, NodeSource, ObjectId, SolvedInstance, SolvedValue,
};
use crate::tech::Technology;

/// A rectangle as `(x0, y0, x1, y1)`.
pub type Bounds = [f64; 4];

/// Coordinates closer than this are equal; layouts sit on a grid far coarser.
const EPS: f64 = 1e-6;

/// A shape covering more grid buckets than this is checked against every
/// query instead of being bucketed.
const MAX_BUCKETS: usize = 4096;

/// Where a flattened shape was drawn.
#[derive(Clone, Debug)]
pub struct Source {
    /// The cell whose body drew the object.
    pub cell: CellId,
    pub object: ObjectId,
    pub span: Option<Span>,
    /// The instances leading from the flattened cell to `cell`, outermost
    /// first, each with the cell that placed it.
    pub instances: Vec<(CellId, ObjectId)>,
}

/// A conductor rectangle in the flattened cell's frame.
#[derive(Clone, Debug)]
pub struct Shape {
    /// Index into [`Conductors::layers`].
    pub layer: usize,
    pub bounds: Bounds,
    /// Index into [`Conductors::sources`].
    pub source: usize,
}

/// A text label in the flattened cell's frame.
#[derive(Clone, Debug)]
pub struct Label {
    pub id: ObjectId,
    pub text: String,
    pub layer: String,
    pub x: f64,
    pub y: f64,
    pub span: Option<Span>,
    /// The cell whose body drew the label.
    pub cell: CellId,
    /// The instances leading to `cell`, as for [`Source::instances`]; empty
    /// for the flattened cell's own labels.
    pub instances: Vec<(CellId, ObjectId)>,
}

/// The conductors of a cell and its subcells, joined into nets.
#[derive(Debug)]
pub struct Conductors {
    pub cell: CellId,
    /// Conductor layer names; a shape's layer indexes this.
    pub layers: Vec<String>,
    /// Whether each layer is a via's cut layer.
    pub cut: Vec<bool>,
    pub shapes: Vec<Shape>,
    pub sources: Vec<Source>,
    /// Shapes on pin layers, by layer name, for finding labels' pins.
    pub pins: Vec<(String, Bounds)>,
    pub labels: Vec<Label>,
    /// Objects of the cell itself drawn on layers that don't conduct.
    pub untracked: Vec<(ObjectId, String)>,
    /// The net of each shape, from 0.
    pub net: Vec<usize>,
    pub nets: usize,
    /// Shapes that touch, as pairs of shape indices.
    pub edges: Vec<(usize, usize)>,
}

/// A name the source gives a shape: a `net=` tag or a pin label.
#[derive(Clone, Debug)]
pub struct Naming {
    pub name: String,
    pub site: Site,
}

/// A place in the layout and the source that put something there.
#[derive(Clone, Debug)]
pub struct Site {
    pub layer: String,
    pub bounds: Bounds,
    /// The shape's index in [`Conductors::shapes`], if it is one.
    pub shape: Option<usize>,
    pub source: Source,
}

/// What a check of a cell's names against its connectivity found.
#[derive(Clone, Debug)]
pub enum Issue {
    /// One net carries several names. `names` has a site for each name, and
    /// `path` the shapes joining the first two, in order.
    Short { names: Vec<Naming>, path: Vec<Site> },
    /// One name is on several nets, with one site per net.
    Open { name: String, pieces: Vec<Site> },
    /// A pin label that names no net, and why.
    Unattached { label: Naming, reason: String },
    /// A `net=` tag on a shape whose layer doesn't conduct.
    Untracked { name: String, site: Site },
    /// A name in an instance's `nets=` that its cell has no signal of, so it
    /// maps nothing.
    UnknownPort {
        name: String,
        cell: String,
        site: Site,
    },
}

/// A cell's connectivity and what checking its names found.
#[derive(Debug)]
pub struct Check {
    pub conductors: Conductors,
    /// The names of each net, by net.
    pub names: Vec<Vec<Naming>>,
    pub issues: Vec<Issue>,
}

impl Check {
    /// The name a report gives net `net`: its names joined with `+`, or `None`.
    pub fn net_name(&self, net: usize) -> Option<String> {
        let mut names = self.names[net]
            .iter()
            .map(|n| n.name.as_str())
            .collect::<Vec<_>>();
        names.sort_unstable();
        names.dedup();
        (!names.is_empty()).then(|| names.join("+"))
    }
}

/// The cells under `top`, `top` included, that have `net=` tags or labels on
/// pin label layers, which are the cells worth checking.
pub fn named_cells(data: &CompiledData, top: CellId) -> Vec<CellId> {
    let labels = label_layers(&data.tech);
    let mut seen = Vec::new();
    let mut stack = vec![top];
    while let Some(cell) = stack.pop() {
        if seen.contains(&cell) {
            continue;
        }
        seen.push(cell);
        for object in data.cells[&cell].objects.values() {
            if let SolvedValue::Instance(instance) = object
                && !instance.construction
            {
                stack.push(instance.cell);
            }
        }
    }
    seen.into_iter()
        .filter(|cell| {
            let compiled = &data.cells[cell];
            !compiled.net_tags.is_empty()
                || !compiled.inst_nets.is_empty()
                || compiled.objects.values().any(|object| {
                    matches!(object, SolvedValue::Text(text) if labels.contains_key(text.layer.as_str()))
                })
        })
        .collect()
}

/// Label layers, each with the pin layers it labels. Text on a pin layer
/// labels that layer's own pins.
fn label_layers(tech: &Technology) -> HashMap<&str, Vec<&str>> {
    let mut map: HashMap<&str, Vec<&str>> = HashMap::new();
    for (pin, label) in &tech.pin_layers {
        for layer in [label.as_str(), pin.as_str()] {
            let pins = map.entry(layer).or_default();
            if !pins.contains(&pin.as_str()) {
                pins.push(pin.as_str());
            }
        }
    }
    map
}

/// The drawing layer a pin layer marks: `met1.pin` marks `met1.drawing`.
fn drawing_layer(pin: &str) -> String {
    match pin.rsplit_once('.') {
        Some((base, _)) => format!("{base}.drawing"),
        None => pin.to_owned(),
    }
}

/// Flattens `cell`, joins its conductors into nets, and checks them against
/// its own `net=` tags and pin labels.
pub fn check(data: &CompiledData, cell: CellId) -> Check {
    let conductors = Conductors::extract(data, cell);
    let compiled = &data.cells[&cell];
    let mut names: Vec<Vec<Naming>> = vec![Vec::new(); conductors.nets];
    let mut issues = Vec::new();

    // Tags: the cell's own, and its subcells' that instances' `nets=` carry up.
    let names_of = Names::new(data);
    let mut tagged = HashMap::<ObjectId, bool>::new();
    for (index, shape) in conductors.shapes.iter().enumerate() {
        let source = &conductors.sources[shape.source];
        let Some(tag) = names_of.tag(source.cell, source.object) else {
            continue;
        };
        let name = if source.instances.is_empty() {
            tagged.insert(source.object, true);
            tag
        } else {
            match names_of.carry(source.cell, &tag, &source.instances) {
                Some(name) => name,
                None => continue,
            }
        };
        let net = conductors.net[index];
        // One naming per tagged object and net is enough.
        if names[net]
            .iter()
            .any(|n| n.name == name && n.site.source.object == source.object)
        {
            continue;
        }
        names[net].push(Naming {
            name,
            site: conductors.site(index),
        });
    }
    for (object, layer) in &conductors.untracked {
        if let Some(name) = names_of.tag(cell, *object)
            && !tagged.contains_key(object)
            && let Some(object_value) = compiled.objects.get(object)
            && let Some(bounds) = object_bounds(object_value)
        {
            issues.push(Issue::Untracked {
                name,
                site: Site {
                    layer: layer.clone(),
                    bounds,
                    shape: None,
                    source: Source {
                        cell,
                        object: *object,
                        span: object_span(object_value).cloned(),
                        instances: Vec::new(),
                    },
                },
            });
        }
    }

    // Names in instances' `nets=` that their cells don't have.
    for (instance, ports) in &compiled.inst_nets {
        let Some(SolvedValue::Instance(inst)) = compiled.objects.get(instance) else {
            continue;
        };
        let child = &data.cells[&inst.cell];
        let known = cell_names(child);
        for (port, _) in ports {
            let declared = known.aliases.contains_key(port)
                || child.schematic.nets.iter().any(|net| &net.name == port);
            if !declared {
                issues.push(Issue::UnknownPort {
                    name: port.clone(),
                    cell: child.name.clone(),
                    site: Site {
                        layer: String::new(),
                        bounds: [inst.x, inst.y, inst.x, inst.y],
                        shape: None,
                        source: Source {
                            cell,
                            object: *instance,
                            span: Some(inst.span.clone()),
                            instances: Vec::new(),
                        },
                    },
                });
            }
        }
    }

    // Labels: text on a label layer, over a pin shape, over a conductor.
    let label_pins = label_layers(&data.tech);
    let layer_index: HashMap<&str, usize> = conductors
        .layers
        .iter()
        .enumerate()
        .map(|(i, l)| (l.as_str(), i))
        .collect();
    let index = Index::new(&conductors.shapes);
    for label in &conductors.labels {
        let Some(pin_layers) = label_pins.get(label.layer.as_str()) else {
            continue;
        };
        let own = label.instances.is_empty();
        let name = if own {
            label.text.clone()
        } else {
            match names_of.carry(label.cell, &label.text, &label.instances) {
                Some(name) => name,
                None => continue,
            }
        };
        let point = [label.x, label.y, label.x, label.y];
        let naming = |shape: Option<usize>, layer: &str, bounds: Bounds| Naming {
            name: name.clone(),
            site: Site {
                layer: layer.to_owned(),
                bounds,
                shape,
                source: Source {
                    cell: label.cell,
                    object: label.id,
                    span: label.span.clone(),
                    instances: label.instances.clone(),
                },
            },
        };
        let pin = conductors.pins.iter().find(|(layer, bounds)| {
            pin_layers.contains(&layer.as_str()) && contains(*bounds, point)
        });
        let Some((pin_layer, pin_bounds)) = pin else {
            // A subcell's label is that cell's own business when unattached.
            if !own {
                continue;
            }
            issues.push(Issue::Unattached {
                label: naming(None, &label.layer, point),
                reason: format!(
                    "no {} shape is under it, so it makes no port",
                    pin_layers.join(" or ")
                ),
            });
            continue;
        };
        let drawing = drawing_layer(pin_layer);
        // A pin on a layer no via joins, such as a well, can't be checked.
        if !layer_index.contains_key(drawing.as_str()) {
            continue;
        }
        let under = layer_index.get(drawing.as_str()).and_then(|layer| {
            index.query(point).into_iter().find(|&i| {
                conductors.shapes[i].layer == *layer && contains(conductors.shapes[i].bounds, point)
            })
        });
        match under {
            Some(shape) => {
                let net = conductors.net[shape];
                if !names[net].iter().any(|n| n.name == name) {
                    names[net].push(naming(
                        Some(shape),
                        &drawing,
                        conductors.shapes[shape].bounds,
                    ));
                }
            }
            None if own => issues.push(Issue::Unattached {
                label: naming(None, pin_layer, *pin_bounds),
                reason: format!("its {pin_layer} pin is not over any {drawing} shape"),
            }),
            None => {}
        }
    }

    // Signals joined by `connect` are one schematic net, so each signal's own
    // name stands for its net's.
    let canonical = |name: &str| names_of.canonical(cell, name);

    // Shorts: one net, several names.
    let adjacency = conductors.adjacency();
    for (net, namings) in names.iter().enumerate() {
        let mut distinct: Vec<&Naming> = Vec::new();
        for naming in namings {
            if !distinct
                .iter()
                .any(|d| canonical(&d.name) == canonical(&naming.name))
            {
                distinct.push(naming);
            }
        }
        if distinct.len() < 2 {
            continue;
        }
        let path = match (distinct[0].site.shape, distinct[1].site.shape) {
            (Some(a), Some(b)) => conductors.path(&adjacency, a, b),
            _ => Vec::new(),
        };
        let _ = net;
        issues.push(Issue::Short {
            names: distinct.into_iter().cloned().collect(),
            path,
        });
    }

    // Opens: one name, several nets.
    let mut pieces: HashMap<String, Vec<(usize, &Naming)>> = HashMap::new();
    for (net, namings) in names.iter().enumerate() {
        for naming in namings {
            let entry = pieces.entry(canonical(&naming.name)).or_default();
            if !entry.iter().any(|(n, _)| *n == net) {
                entry.push((net, naming));
            }
        }
    }
    let mut opens = pieces
        .into_iter()
        .filter(|(_, nets)| nets.len() > 1)
        .collect::<Vec<_>>();
    opens.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, nets) in opens {
        issues.push(Issue::Open {
            name,
            pieces: nets.into_iter().map(|(_, n)| n.site.clone()).collect(),
        });
    }

    Check {
        conductors,
        names,
        issues,
    }
}

fn contains(outer: Bounds, inner: Bounds) -> bool {
    outer[0] <= inner[0] + EPS
        && outer[1] <= inner[1] + EPS
        && outer[2] + EPS >= inner[2]
        && outer[3] + EPS >= inner[3]
}

/// Whether `a` and `b` share area, or an edge of positive length.
fn touches(a: Bounds, b: Bounds) -> bool {
    let dx = a[2].min(b[2]) - a[0].max(b[0]);
    let dy = a[3].min(b[3]) - a[1].max(b[1]);
    (dx > EPS && dy > -EPS) || (dx > -EPS && dy > EPS)
}

/// Whether `a` and `b` share area.
pub fn overlaps(a: Bounds, b: Bounds) -> bool {
    a[2].min(b[2]) - a[0].max(b[0]) > EPS && a[3].min(b[3]) - a[1].max(b[1]) > EPS
}

impl Conductors {
    /// Flattens `cell` and joins its conductors.
    pub fn extract(data: &CompiledData, cell: CellId) -> Self {
        let tech = &data.tech;
        let mut layers: Vec<String> = Vec::new();
        let mut cut = Vec::new();
        let mut add = |layer: &str, is_cut: bool, layers: &mut Vec<String>| match layers
            .iter()
            .position(|l| l == layer)
        {
            Some(i) => cut[i] |= is_cut,
            None => {
                layers.push(layer.to_owned());
                cut.push(is_cut);
            }
        };
        for via in &tech.vias {
            add(&via.bot, false, &mut layers);
            add(&via.top, false, &mut layers);
            add(&via.cut, true, &mut layers);
        }
        let layer_index: HashMap<String, usize> = layers
            .iter()
            .enumerate()
            .map(|(i, l)| (l.clone(), i))
            .collect();
        let pin_layers: Vec<&str> = tech.pin_layers.keys().map(String::as_str).collect();

        let mut this = Self {
            cell,
            layers,
            cut,
            shapes: Vec::new(),
            sources: Vec::new(),
            pins: Vec::new(),
            labels: Vec::new(),
            untracked: Vec::new(),
            net: Vec::new(),
            nets: 0,
            edges: Vec::new(),
        };
        let mut path = Vec::new();
        flatten(
            data,
            cell,
            Transform::IDENTITY,
            &mut path,
            &mut |owner, object, transform, path| {
                let (layer, rects) = match object {
                    SolvedValue::Rect(rect) => match &rect.layer {
                        Some(layer) => (
                            layer.as_str(),
                            vec![transform.bounds([rect.x0.0, rect.y0.0, rect.x1.0, rect.y1.0])],
                        ),
                        None => return,
                    },
                    SolvedValue::Polygon(polygon) => (
                        polygon.layer.as_str(),
                        decompose(
                            &polygon
                                .points
                                .iter()
                                .map(|(x, y)| transform.apply((x.0, y.0)))
                                .collect::<Vec<_>>(),
                        ),
                    ),
                    SolvedValue::Path(p) => match p.outline() {
                        Some(points) => (
                            p.layer.as_str(),
                            decompose(
                                &points
                                    .into_iter()
                                    .map(|pt| transform.apply(pt))
                                    .collect::<Vec<_>>(),
                            ),
                        ),
                        None => return,
                    },
                    SolvedValue::Text(text) => {
                        let (x, y) = transform.apply((text.x, text.y));
                        this.labels.push(Label {
                            id: text.id,
                            text: text.text.clone(),
                            layer: text.layer.clone(),
                            x,
                            y,
                            span: text.span.clone(),
                            cell: owner,
                            instances: path
                                .iter()
                                .map(|(parent, inst)| (*parent, inst.id))
                                .collect(),
                        });
                        return;
                    }
                    _ => return,
                };
                if pin_layers.contains(&layer) {
                    this.pins
                        .extend(rects.iter().map(|bounds| (layer.to_owned(), *bounds)));
                }
                let Some(&index) = layer_index.get(layer) else {
                    if path.is_empty() {
                        this.untracked.push((object_id(object), layer.to_owned()));
                    }
                    return;
                };
                let source = this.sources.len();
                this.sources.push(Source {
                    cell: owner,
                    object: object_id(object),
                    span: object_span(object).cloned(),
                    instances: path
                        .iter()
                        .map(|(parent, inst)| (*parent, inst.id))
                        .collect(),
                });
                for bounds in rects {
                    this.shapes.push(Shape {
                        layer: index,
                        bounds,
                        source,
                    });
                }
            },
        );
        this.split_gates(tech);
        this.join(tech);
        this
    }

    /// Cuts each shape on a layer a gate splits wherever a gate crosses it.
    fn split_gates(&mut self, tech: &Technology) {
        for gate in &tech.gates {
            let (Some(g), Some(s)) = (
                self.layers.iter().position(|l| *l == gate.layer),
                self.layers.iter().position(|l| *l == gate.splits),
            ) else {
                continue;
            };
            let gates = self
                .shapes
                .iter()
                .filter(|shape| shape.layer == g)
                .cloned()
                .collect::<Vec<_>>();
            if gates.is_empty() {
                continue;
            }
            let index = Index::new(&gates);
            let mut shapes = Vec::with_capacity(self.shapes.len());
            for shape in std::mem::take(&mut self.shapes) {
                if shape.layer != s {
                    shapes.push(shape);
                    continue;
                }
                let mut pieces = vec![shape.bounds];
                for i in index.query(shape.bounds) {
                    let cutter = gates[i].bounds;
                    pieces = pieces
                        .into_iter()
                        .flat_map(|piece| subtract(piece, cutter))
                        .collect();
                }
                shapes.extend(pieces.into_iter().map(|bounds| Shape { bounds, ..shape }));
            }
            self.shapes = shapes;
        }
    }

    /// Joins touching shapes of a layer, and each cut to the shapes of its
    /// via's layers that it overlaps. A cut over several of its vias' bottom
    /// layers joins only the one whose via the technology file lists last,
    /// as a cut on a capacitor's top plate doesn't reach the plate below.
    fn join(&mut self, tech: &Technology) {
        let n = self.shapes.len();
        let mut sets = UnionFind::new(n);
        let index = Index::new(&self.shapes);
        let mut edges = Vec::new();
        // Each cut layer's top layers, and its bottom layers by precedence.
        let mut tops: HashMap<usize, Vec<usize>> = HashMap::new();
        let mut bots: HashMap<usize, Vec<usize>> = HashMap::new();
        for via in &tech.vias {
            let find = |name: &str| self.layers.iter().position(|l| l == name);
            if let (Some(c), Some(b), Some(t)) = (find(&via.cut), find(&via.bot), find(&via.top)) {
                let top = tops.entry(c).or_default();
                if !top.contains(&t) {
                    top.push(t);
                }
                let bot = bots.entry(c).or_default();
                bot.retain(|l| *l != b);
                bot.push(b);
            }
        }
        // Each cut's overlapping bottom shapes, as (precedence, shape).
        let mut below: HashMap<usize, Vec<(usize, usize)>> = HashMap::new();
        for a in 0..n {
            let shape = &self.shapes[a];
            for b in index.query(shape.bounds) {
                if b <= a {
                    continue;
                }
                let other = &self.shapes[b];
                if other.layer == shape.layer {
                    if touches(shape.bounds, other.bounds) {
                        edges.push((a, b));
                        sets.union(a, b);
                    }
                    continue;
                }
                if !overlaps(shape.bounds, other.bounds) {
                    continue;
                }
                for (cut, target) in [(a, b), (b, a)] {
                    let (layer, target_layer) = (self.shapes[cut].layer, self.shapes[target].layer);
                    if tops.get(&layer).is_some_and(|t| t.contains(&target_layer)) {
                        edges.push((cut, target));
                        sets.union(cut, target);
                    } else if let Some(rank) = bots
                        .get(&layer)
                        .and_then(|b| b.iter().position(|l| *l == target_layer))
                    {
                        below.entry(cut).or_default().push((rank, target));
                    }
                }
            }
        }
        for (cut, targets) in below {
            let best = targets.iter().map(|(rank, _)| *rank).max().unwrap_or(0);
            for (rank, target) in targets {
                if rank == best {
                    edges.push((cut, target));
                    sets.union(cut, target);
                }
            }
        }
        let mut ids = HashMap::new();
        self.net = (0..n)
            .map(|i| {
                let root = sets.find(i);
                let next = ids.len();
                *ids.entry(root).or_insert(next)
            })
            .collect();
        self.nets = ids.len();
        self.edges = edges;
    }

    /// Each shape's neighbors through [`Self::edges`].
    pub fn adjacency(&self) -> Vec<Vec<usize>> {
        let mut adjacency = vec![Vec::new(); self.shapes.len()];
        for &(a, b) in &self.edges {
            adjacency[a].push(b);
            adjacency[b].push(a);
        }
        adjacency
    }

    /// The shapes on a shortest chain of touching shapes from `a` to `b`.
    pub fn path(&self, adjacency: &[Vec<usize>], a: usize, b: usize) -> Vec<Site> {
        let mut previous = HashMap::from([(a, a)]);
        let mut queue = std::collections::VecDeque::from([a]);
        while let Some(at) = queue.pop_front() {
            if at == b {
                break;
            }
            for &next in &adjacency[at] {
                if let std::collections::hash_map::Entry::Vacant(entry) = previous.entry(next) {
                    entry.insert(at);
                    queue.push_back(next);
                }
            }
        }
        if !previous.contains_key(&b) {
            return Vec::new();
        }
        let mut chain = vec![b];
        while *chain.last().unwrap() != a {
            chain.push(previous[chain.last().unwrap()]);
        }
        chain.reverse();
        chain.into_iter().map(|i| self.site(i)).collect()
    }

    /// Where shape `index` is and what drew it.
    pub fn site(&self, index: usize) -> Site {
        let shape = &self.shapes[index];
        Site {
            layer: self.layers[shape.layer].clone(),
            bounds: shape.bounds,
            shape: Some(index),
            source: self.sources[shape.source].clone(),
        }
    }
}

/// Names of nets across a hierarchy: each cell's signal names, its tags, and
/// the names its instances' `nets=` maps carry up to their parents.
struct Names<'a> {
    data: &'a CompiledData,
    cells: RefCell<HashMap<CellId, CellNames>>,
}

/// A cell's signal names, by the name of the net each stands for, and the
/// net name of each object it tags.
#[derive(Clone, Default)]
struct CellNames {
    aliases: HashMap<String, String>,
    tags: HashMap<ObjectId, String>,
}

impl<'a> Names<'a> {
    fn new(data: &'a CompiledData) -> Self {
        Self {
            data,
            cells: RefCell::default(),
        }
    }

    fn with<R>(&self, cell: CellId, read: impl FnOnce(&CellNames) -> R) -> R {
        let mut cells = self.cells.borrow_mut();
        let names = cells
            .entry(cell)
            .or_insert_with(|| cell_names(&self.data.cells[&cell]));
        read(names)
    }

    /// The name of the net `name` stands for in `cell`: a signal's own name
    /// stands for its net's, as `connect` may have joined it to another.
    fn canonical(&self, cell: CellId, name: &str) -> String {
        self.with(cell, |names| {
            names
                .aliases
                .get(name)
                .cloned()
                .unwrap_or_else(|| name.to_owned())
        })
    }

    /// The net name `object`'s `net=` tag gives it in `cell`.
    fn tag(&self, cell: CellId, object: ObjectId) -> Option<String> {
        self.with(cell, |names| names.tags.get(&object).cloned())
    }

    /// `name`, a name in `cell`, as the outermost instance of `instances`
    /// names it through each instance's `nets=` map, or `None` if an instance
    /// on the way doesn't map it.
    fn carry(&self, cell: CellId, name: &str, instances: &[(CellId, ObjectId)]) -> Option<String> {
        let mut name = self.canonical(cell, name);
        let mut child = cell;
        for (parent, instance) in instances.iter().rev() {
            let compiled = &self.data.cells[parent];
            let map = &compiled.inst_nets.iter().find(|(id, _)| id == instance)?.1;
            let (_, net) = map
                .iter()
                .find(|(port, _)| self.canonical(child, port) == name)?;
            name = compiled.schematic.nets.get(*net as usize)?.name.clone();
            child = *parent;
        }
        Some(name)
    }
}

fn cell_names(compiled: &CompiledCell) -> CellNames {
    let schematic = &compiled.schematic;
    let net_name = |net: u32| schematic.nets.get(net as usize).map(|n| n.name.clone());
    let aliases = schematic
        .nodes
        .iter()
        .filter(|node| matches!(node.source, NodeSource::Signal { .. }))
        .filter_map(|node| Some((schematic.path_text(node.path?), net_name(node.net)?)))
        .collect();
    let tags = compiled
        .net_tags
        .iter()
        .filter_map(|(object, net)| Some((*object, net_name(*net)?)))
        .collect();
    CellNames { aliases, tags }
}

/// The bounding box of a shape's points in its cell's frame.
fn object_bounds(object: &SolvedValue) -> Option<Bounds> {
    let bounds = |points: &mut dyn Iterator<Item = (f64, f64)>| {
        points.fold(None, |acc: Option<Bounds>, (x, y)| {
            Some(match acc {
                None => [x, y, x, y],
                Some(b) => [b[0].min(x), b[1].min(y), b[2].max(x), b[3].max(y)],
            })
        })
    };
    match object {
        SolvedValue::Rect(r) => Some([r.x0.0, r.y0.0, r.x1.0, r.y1.0]),
        SolvedValue::Polygon(p) => bounds(&mut p.points.iter().map(|(x, y)| (x.0, y.0))),
        SolvedValue::Path(p) => bounds(&mut p.outline()?.into_iter()),
        _ => None,
    }
}

fn object_id(object: &SolvedValue) -> ObjectId {
    match object {
        SolvedValue::Rect(r) => r.id,
        SolvedValue::Polygon(p) => p.id,
        SolvedValue::Path(p) => p.id,
        SolvedValue::Text(t) => t.id,
        SolvedValue::Dimension(d) => d.id,
        SolvedValue::Instance(i) => i.id,
    }
}

fn object_span(object: &SolvedValue) -> Option<&Span> {
    match object {
        SolvedValue::Rect(r) => r.span.as_ref(),
        SolvedValue::Polygon(p) => p.span.as_ref(),
        SolvedValue::Path(p) => p.span.as_ref(),
        SolvedValue::Text(t) => t.span.as_ref(),
        SolvedValue::Dimension(d) => d.span.as_ref(),
        SolvedValue::Instance(i) => Some(&i.span),
    }
}

/// `a` minus `b`, as up to four rectangles.
fn subtract(a: Bounds, b: Bounds) -> Vec<Bounds> {
    if !overlaps(a, b) {
        return vec![a];
    }
    let mut out = Vec::new();
    if b[0] > a[0] + EPS {
        out.push([a[0], a[1], b[0], a[3]]);
    }
    if b[2] < a[2] - EPS {
        out.push([b[2], a[1], a[2], a[3]]);
    }
    let (x0, x1) = (a[0].max(b[0]), a[2].min(b[2]));
    if b[1] > a[1] + EPS {
        out.push([x0, a[1], x1, b[1]]);
    }
    if b[3] < a[3] - EPS {
        out.push([x0, b[3], x1, a[3]]);
    }
    out
}

/// A polygon as rectangles, by slabs between its distinct x coordinates: a
/// Manhattan polygon exactly, and any other by rectangles inside it, on
/// slabs split finer, so it never joins what it doesn't touch.
fn decompose(points: &[(f64, f64)]) -> Vec<Bounds> {
    const SPLIT: usize = 8;
    let n = points.len();
    if n < 3 {
        return Vec::new();
    }
    let manhattan = (0..n).all(|i| {
        let (a, b) = (points[i], points[(i + 1) % n]);
        (a.0 - b.0).abs() < EPS || (a.1 - b.1).abs() < EPS
    });
    let mut xs = points.iter().map(|p| p.0).collect::<Vec<_>>();
    xs.sort_by(f64::total_cmp);
    xs.dedup_by(|a, b| (*a - *b).abs() < EPS);
    if !manhattan {
        xs = xs
            .windows(2)
            .flat_map(|w| (0..SPLIT).map(move |k| w[0] + (w[1] - w[0]) * k as f64 / SPLIT as f64))
            .chain(xs.last().copied())
            .collect();
    }
    let mut out = Vec::new();
    for slab in xs.windows(2) {
        let mid = (slab[0] + slab[1]) / 2.;
        // Each edge spanning the slab, as its y at the middle and both sides.
        let mut edges = (0..n)
            .filter_map(|i| {
                let (a, b) = (points[i], points[(i + 1) % n]);
                if !(a.0.min(b.0) < mid && a.0.max(b.0) > mid) {
                    return None;
                }
                let at = |x: f64| a.1 + (b.1 - a.1) * (x - a.0) / (b.0 - a.0);
                Some((at(mid), at(slab[0]), at(slab[1])))
            })
            .collect::<Vec<_>>();
        edges.sort_by(|a, b| a.0.total_cmp(&b.0));
        for pair in edges.as_chunks::<2>().0 {
            let y0 = pair[0].1.max(pair[0].2);
            let y1 = pair[1].1.min(pair[1].2);
            if y1 - y0 > EPS {
                out.push([slab[0], y0, slab[1], y1]);
            }
        }
    }
    out
}

/// A Manhattan placement: reflection and rotation, then a translation.
#[derive(Clone, Copy)]
struct Transform {
    matrix: [f64; 4],
    offset: (f64, f64),
}

impl Transform {
    const IDENTITY: Self = Self {
        matrix: [1., 0., 0., 1.],
        offset: (0., 0.),
    };

    /// Reflected across the x axis if the instance says so, then rotated,
    /// then moved, as `std::layout::inst` places a cell.
    fn of(instance: &SolvedInstance) -> Self {
        let (sin, cos) = instance.angle.degrees().to_radians().sin_cos();
        let (sin, cos) = (sin.round(), cos.round());
        let flip = if instance.reflect { -1. } else { 1. };
        Self {
            matrix: [cos, -sin * flip, sin, cos * flip],
            offset: (instance.x, instance.y),
        }
    }

    fn apply(&self, (x, y): (f64, f64)) -> (f64, f64) {
        let [a, b, c, d] = self.matrix;
        (a * x + b * y + self.offset.0, c * x + d * y + self.offset.1)
    }

    fn bounds(&self, r: Bounds) -> Bounds {
        let (x0, y0) = self.apply((r[0], r[1]));
        let (x1, y1) = self.apply((r[2], r[3]));
        [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)]
    }

    /// This transform after `inner`.
    fn compose(&self, inner: &Self) -> Self {
        let [a, b, c, d] = self.matrix;
        let [e, f, g, h] = inner.matrix;
        Self {
            matrix: [a * e + b * g, a * f + b * h, c * e + d * g, c * f + d * h],
            offset: self.apply(inner.offset),
        }
    }
}

/// Calls `visit` with each drawn object under `cell`, the cell drawing it,
/// its transform into `cell`'s frame, and the instances leading to it.
fn flatten<'a>(
    data: &'a CompiledData,
    cell: CellId,
    transform: Transform,
    path: &mut Vec<(CellId, &'a SolvedInstance)>,
    visit: &mut impl FnMut(CellId, &'a SolvedValue, &Transform, &[(CellId, &'a SolvedInstance)]),
) {
    for object in data.cells[&cell]
        .objects
        .values()
        .filter(|object| object.is_layout())
    {
        if let SolvedValue::Instance(instance) = object {
            path.push((cell, instance));
            let inner = transform.compose(&Transform::of(instance));
            flatten(data, instance.cell, inner, path, visit);
            path.pop();
        } else {
            visit(cell, object, &transform, path);
        }
    }
}

struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, mut i: usize) -> usize {
        while self.parent[i] != i {
            self.parent[i] = self.parent[self.parent[i]];
            i = self.parent[i];
        }
        i
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        if a != b {
            self.parent[a.max(b)] = a.min(b);
        }
    }
}

/// A uniform grid of buckets over rectangles, for finding those near one.
pub struct Index {
    origin: (f64, f64),
    size: f64,
    buckets: HashMap<(i64, i64), Vec<usize>>,
    /// Rectangles too large to bucket, returned by every query.
    large: Vec<usize>,
}

impl Index {
    /// An index of `shapes` by their bounds.
    pub fn new<T: HasBounds>(shapes: &[T]) -> Self {
        let mut extent = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
        for shape in shapes {
            let b = shape.bounds();
            extent = [
                extent[0].min(b[0]),
                extent[1].min(b[1]),
                extent[2].max(b[2]),
                extent[3].max(b[3]),
            ];
        }
        // Buckets about twice a typical shape, so a few large shapes don't
        // crowd the small ones into a few buckets, but no more than 2048
        // across the extent.
        let span = (extent[2] - extent[0]).max(extent[3] - extent[1]).max(1.);
        let mut sides = shapes
            .iter()
            .map(|shape| {
                let b = shape.bounds();
                (b[2] - b[0]).max(b[3] - b[1])
            })
            .collect::<Vec<_>>();
        let typical = if sides.is_empty() {
            span
        } else {
            let middle = sides.len() / 2;
            *sides.select_nth_unstable_by(middle, f64::total_cmp).1
        };
        let size = (2. * typical).max(span / 2048.).max(1.);
        let mut index = Self {
            origin: (extent[0].min(0.), extent[1].min(0.)),
            size,
            buckets: HashMap::new(),
            large: Vec::new(),
        };
        for (i, shape) in shapes.iter().enumerate() {
            let [i0, j0, i1, j1] = index.range(shape.bounds());
            if ((i1 - i0 + 1) * (j1 - j0 + 1)) as usize > MAX_BUCKETS {
                index.large.push(i);
                continue;
            }
            for bi in i0..=i1 {
                for bj in j0..=j1 {
                    index.buckets.entry((bi, bj)).or_default().push(i);
                }
            }
        }
        index
    }

    fn range(&self, b: Bounds) -> [i64; 4] {
        let cell = |v: f64, o: f64| ((v - o) / self.size).floor() as i64;
        [
            cell(b[0] - EPS, self.origin.0),
            cell(b[1] - EPS, self.origin.1),
            cell(b[2] + EPS, self.origin.0),
            cell(b[3] + EPS, self.origin.1),
        ]
    }

    /// The rectangles that may touch `b`, each once, in increasing order.
    pub fn query(&self, b: Bounds) -> Vec<usize> {
        let [i0, j0, i1, j1] = self.range(b);
        let mut out = self.large.clone();
        if ((i1 - i0 + 1) * (j1 - j0 + 1)) as usize > self.buckets.len() {
            for bucket in self.buckets.values() {
                out.extend_from_slice(bucket);
            }
        } else {
            for bi in i0..=i1 {
                for bj in j0..=j1 {
                    if let Some(bucket) = self.buckets.get(&(bi, bj)) {
                        out.extend_from_slice(bucket);
                    }
                }
            }
        }
        out.sort_unstable();
        out.dedup();
        out
    }
}

/// Something with a bounding rectangle.
pub trait HasBounds {
    fn bounds(&self) -> Bounds;
}

impl HasBounds for Shape {
    fn bounds(&self) -> Bounds {
        self.bounds
    }
}

impl HasBounds for Bounds {
    fn bounds(&self) -> Bounds {
        *self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subtracts_a_crossing_rectangle_into_two_pieces() {
        let pieces = subtract([0., 0., 1000., 500.], [400., -100., 550., 600.]);
        assert_eq!(pieces, vec![[0., 0., 400., 500.], [550., 0., 1000., 500.]]);
        assert_eq!(
            subtract([0., 0., 10., 10.], [20., 0., 30., 10.]),
            vec![[0., 0., 10., 10.]]
        );
    }

    #[test]
    fn decomposes_an_l_shaped_polygon() {
        let l = [
            (0., 0.),
            (300., 0.),
            (300., 100.),
            (100., 100.),
            (100., 300.),
            (0., 300.),
        ];
        let mut rects = decompose(&l);
        rects.sort_by(|a, b| a[0].total_cmp(&b[0]));
        assert_eq!(rects, vec![[0., 0., 100., 300.], [100., 0., 300., 100.]]);
    }

    #[test]
    fn touches_along_edges_but_not_at_corners() {
        assert!(touches([0., 0., 10., 10.], [10., 0., 20., 10.]));
        assert!(!touches([0., 0., 10., 10.], [10., 10., 20., 20.]));
        assert!(!touches([0., 0., 10., 10.], [11., 0., 20., 10.]));
    }

    #[test]
    fn indexes_and_finds_nearby_rectangles() {
        let rects: Vec<Bounds> = (0..100)
            .map(|i| [i as f64 * 10., 0., i as f64 * 10. + 10., 10.])
            .chain([[0., 0., 1e6, 5.]])
            .collect();
        let index = Index::new(&rects);
        let near = index.query([15., 0., 25., 10.]);
        assert!(near.contains(&1) && near.contains(&2) && near.contains(&100));
        assert!(!near.contains(&50));
    }
}
