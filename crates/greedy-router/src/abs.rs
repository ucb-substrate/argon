//! Abstract routing utilities.
//!
//! These APIs deal with abstract routing notions (tracks, layers, etc.)
//! rather than raw layout (rectangles, GDS layers, etc.).

use std::collections::{HashMap, HashSet};

use crate::geom::Dir;
use grid::Grid;
use indexmap::IndexSet;
use itertools::Itertools;
use rustc_hash::FxBuildHasher;

use super::error::*;

/// Specifies which grid cells can be connected to one another.
#[derive(Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Copy, Clone)]
pub struct Net(usize);

/// Specifies which grid cells are currently connected to one another.
#[derive(Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Copy, Clone)]
pub struct ConnectionGroup(usize);

#[derive(Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Copy, Clone)]
pub struct Layer(pub usize);

#[derive(Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Copy, Clone)]
pub enum HasVia {
    Yes,
    No,
}

impl Layer {
    pub fn id(&self) -> usize {
        self.0
    }

    pub fn above(&self) -> Self {
        Self(self.0 + 1)
    }
    pub fn below(&self) -> Option<Self> {
        match self.0 {
            0 => None,
            n => Some(Self(n - 1)),
        }
    }
}

impl From<bool> for HasVia {
    fn from(value: bool) -> Self {
        if value { Self::Yes } else { Self::No }
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Copy, Clone)]
pub enum State {
    Occupied {
        net: Net,
        via: HasVia,
        conn_group: ConnectionGroup,
    },
    Blocked {
        net: Option<Net>,
    },
    Empty,
}

impl State {
    pub fn blocked() -> State {
        State::Blocked { net: None }
    }
    pub fn is_occupied(&self) -> bool {
        matches!(self, State::Occupied { .. })
    }
    pub fn is_occupied_by(&self, net: Net) -> bool {
        if let State::Occupied { net: other, .. } = self {
            net == *other
        } else {
            false
        }
    }
    pub fn is_empty(&self) -> bool {
        matches!(self, State::Empty)
    }
    pub fn is_blocked(&self) -> bool {
        matches!(self, State::Blocked { .. })
    }
    pub fn is_blocked_by(&self, net: Net) -> bool {
        if let State::Blocked { net: Some(other) } = self {
            net == *other
        } else {
            false
        }
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone)]
pub enum PosAction {
    // Move by 1 in the positive y direction.
    Up,
    // Move by 1 in the negative y direction.
    Down,
    // Move by 1 in the positive x direction.
    Right,
    // Move by 1 in the negative x direction.
    Left,
    // Move by 1 in the positive z direction.
    ZUp,
    // Move by 1 in the negative z direction.
    ZDown,
}

impl PosAction {
    fn all() -> impl Iterator<Item = PosAction> {
        use PosAction::*;
        [Up, Down, Right, Left, ZUp, ZDown].iter().copied()
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Copy, Clone)]
pub struct Pos {
    /// The current layer.
    pub(crate) layer: Layer,
    /// X-coordinate. Indexes the vertical-going tracks.
    pub(crate) tx: usize,
    /// Y-coordinate. Indexes the horizontal-going tracks.
    pub(crate) ty: usize,
    /// Whether the current position is continuous with the previous position.
    ///
    /// For example, set to true if this `Pos` is the start of a segment created after jumping through
    /// an existing route for a given net.
    jump: bool,
}

impl Pos {
    fn next(&self, action: PosAction) -> Pos {
        let pos = match action {
            PosAction::Up => Pos {
                ty: self.ty + 1,
                ..*self
            },
            PosAction::Down => Pos {
                ty: self.ty - 1,
                ..*self
            },
            PosAction::Right => Pos {
                tx: self.tx + 1,
                ..*self
            },
            PosAction::Left => Pos {
                tx: self.tx - 1,
                ..*self
            },
            PosAction::ZUp => Pos {
                layer: Layer(self.layer.0 + 1),
                ..*self
            },
            PosAction::ZDown => Pos {
                layer: Layer(self.layer.0 - 1),
                ..*self
            },
        };
        pos.no_jump()
    }
}

#[derive(Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Copy, Clone)]
pub struct Segment {
    pub(crate) track_id: usize,
    pub(crate) span: PosSpan,
    /// Indicates if this segment is on the lower or left boundary of the routing region.
    pub(crate) lower_boundary: bool,
    /// Indicates if this segment is on the upper or right boundary of the routing region.
    pub(crate) upper_boundary: bool,
}

#[derive(Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Copy, Clone)]
pub struct PosSpan {
    /// The current layer.
    pub(crate) layer: Layer,
    pub(crate) tx_min: usize,
    pub(crate) tx_max: usize,
    pub(crate) ty_min: usize,
    pub(crate) ty_max: usize,
}

#[derive(Default, Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Copy, Clone)]
pub struct PosSpanBuilder {
    /// The current layer.
    pub(crate) layer: Option<Layer>,
    pub(crate) tx_min: Option<usize>,
    pub(crate) tx_max: Option<usize>,
    pub(crate) ty_min: Option<usize>,
    pub(crate) ty_max: Option<usize>,
}

impl PosSpanBuilder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_layer(layer: Layer) -> Self {
        Self {
            layer: Some(layer),
            ..Default::default()
        }
    }

    pub fn with(&mut self, dir: Dir, min: usize, max: usize) -> &mut Self {
        match dir {
            Dir::Horiz => {
                self.tx_min = Some(min);
                self.tx_max = Some(max);
            }
            Dir::Vert => {
                self.ty_min = Some(min);
                self.ty_max = Some(max);
            }
        }
        self
    }

    pub fn build(&mut self) -> PosSpan {
        PosSpan {
            layer: self.layer.unwrap(),
            tx_min: self.tx_min.unwrap(),
            tx_max: self.tx_max.unwrap(),
            ty_min: self.ty_min.unwrap(),
            ty_max: self.ty_max.unwrap(),
        }
    }
}

impl From<Pos> for PosSpan {
    fn from(value: Pos) -> Self {
        Self {
            layer: value.layer,
            tx_min: value.tx,
            tx_max: value.tx,
            ty_min: value.ty,
            ty_max: value.ty,
        }
    }
}

impl Pos {
    pub fn new(layer: Layer, tx: usize, ty: usize) -> Self {
        Self {
            layer,
            tx,
            ty,
            jump: false,
        }
    }

    // Returns a new `Pos` that is the same as the provided [`Pos`] but is marked as the
    // beginning of a new segment of the path.
    pub fn mark_jump(&self) -> Self {
        let mut new_pos = *self;
        new_pos.jump = true;
        new_pos
    }

    // Returns a new `Pos` that is the same as the provided [`Pos`] but without a jump mark.
    pub fn no_jump(&self) -> Self {
        let mut new_pos = *self;
        new_pos.jump = false;
        new_pos
    }

    // Returns whether this [`Pos`] is the beginning of a new segment of the path.
    pub fn is_jump(&self) -> bool {
        self.jump
    }

    pub fn coord(&self, dir: Dir) -> usize {
        match dir {
            Dir::Horiz => self.tx,
            Dir::Vert => self.ty,
        }
    }
}

impl PosSpan {
    pub fn contains(&self, other: Pos) -> bool {
        self.layer == other.layer
            && (other.tx >= self.tx_min && other.tx <= self.tx_max)
            && (other.ty >= self.ty_min && other.ty <= self.ty_max)
    }

    pub fn span(&self, dir: Dir) -> (usize, usize) {
        match dir {
            Dir::Horiz => (self.tx_min, self.tx_max),
            Dir::Vert => (self.ty_min, self.ty_max),
        }
    }
}

pub struct AbstractLayerInfo {
    pub(crate) grid_space: usize,
    pub(crate) dir: Dir,
    pub(crate) grid: Grid<State>,
}

impl AbstractLayerInfo {
    pub fn num_tracks(&self) -> usize {
        // This seems backwards, but it is correct.
        // The number of horizontal tracks = the number of y coordinates,
        // which are the second indices into the grid and are therefore "cols".
        match self.dir {
            Dir::Horiz => self.grid.cols() / self.grid_space,
            Dir::Vert => self.grid.rows() / self.grid_space,
        }
    }

    pub fn num_parallel_grid_points(&self) -> usize {
        match self.dir {
            Dir::Horiz => self.grid.rows(),
            Dir::Vert => self.grid.cols(),
        }
    }

    pub fn iter_track(&self, id: usize) -> Box<dyn Iterator<Item = &State> + '_> {
        match self.dir {
            Dir::Horiz => Box::new(self.grid.iter_col(id)),
            Dir::Vert => Box::new(self.grid.iter_row(id)),
        }
    }
}

struct AbstractNetInfo {
    curr_net: Net,
    curr_group: ConnectionGroup,
    assigned_nets: HashSet<Net>,
    /// Insertion-ordered so that `pos_in_group` lists positions in the same order on
    /// every run and platform: the router's search takes them as jump successors, and
    /// with a randomly seeded hash set the path it picked among equal-length routes
    /// varied from process to process.
    conn_groups: HashMap<ConnectionGroup, IndexSet<Pos, FxBuildHasher>>,
}

impl AbstractNetInfo {
    fn new() -> Self {
        Self {
            curr_net: Net(0),
            curr_group: ConnectionGroup(0),
            assigned_nets: HashSet::new(),
            conn_groups: HashMap::new(),
        }
    }

    fn get_unused_net(&mut self) -> Net {
        for net in self.assigned_nets.iter() {
            if &self.curr_net == net {
                self.curr_net.0 += 1;
            }
        }
        self.assigned_nets.insert(self.curr_net);
        self.curr_net
    }

    fn get_unused_conn_group(&mut self) -> ConnectionGroup {
        for group in self.conn_groups.keys() {
            if &self.curr_group == group {
                self.curr_group.0 += 1;
            }
        }
        self.conn_groups
            .insert(self.curr_group, IndexSet::default());
        self.curr_group
    }

    fn delete_from_group(&mut self, pos: Pos, conn_group: ConnectionGroup) {
        if let Some(v) = self.conn_groups.get_mut(&conn_group) {
            // Moves the last position into the gap: O(1), and the resulting order is
            // still a function of the operation sequence alone.
            v.swap_remove(&pos);
        }
    }

    fn add_to_group(&mut self, pos: Pos, conn_group: ConnectionGroup) {
        self.conn_groups.entry(conn_group).or_default().insert(pos);
    }

    fn pos_in_group(&self, conn_group: ConnectionGroup) -> Option<Vec<Pos>> {
        self.conn_groups
            .get(&conn_group)
            .map(|set| set.iter().copied().collect())
    }
}

pub struct GreedyAbstractRouter {
    nets: AbstractNetInfo,
    layers: Vec<AbstractLayerInfo>,
    tx: usize,
    ty: usize,
    /// Grid points, keyed by the lower layer and `(tx, ty)`, where no via may
    /// be placed, except by the given net.
    no_via: HashMap<(usize, usize, usize), Option<Net>>,
}

#[derive(Debug, Eq, PartialEq, Hash, Copy, Clone)]
pub struct AbstractLayerConfig {
    pub grid_space: usize,
    pub dir: Dir,
}

pub type AbstractRoute = Vec<Pos>;

#[derive(Debug, Eq, PartialEq, Hash, Ord, PartialOrd, Copy, Clone)]
enum Node {
    Span(PosSpan),
    Pos(Pos),
}

impl Node {
    pub fn unwrap_pos(self) -> Pos {
        match self {
            Self::Pos(pos) => pos,
            _ => panic!("expected `Node` to be a `Pos` variant"),
        }
    }
}

impl GreedyAbstractRouter {
    pub fn new(
        layers: impl IntoIterator<Item = AbstractLayerConfig>,
        tx: usize,
        ty: usize,
    ) -> Self {
        Self {
            nets: AbstractNetInfo::new(),
            layers: layers
                .into_iter()
                .map(|cfg| AbstractLayerInfo {
                    grid_space: cfg.grid_space,
                    dir: cfg.dir,
                    grid: Grid::init(tx, ty, State::Empty),
                })
                .collect(),
            tx,
            ty,
            no_via: HashMap::new(),
        }
    }

    /// The number of grid points in x and y.
    pub fn size(&self) -> (usize, usize) {
        (self.tx, self.ty)
    }

    /// The state of the grid point at `pos`.
    pub fn state(&self, pos: Pos) -> State {
        *self.grid(pos.layer).get(pos.tx, pos.ty).unwrap()
    }

    /// Forbids a via between `lower` and the layer above it at `(tx, ty)` to
    /// every net, or to every net but `net`.
    pub fn forbid_via(&mut self, lower: Layer, tx: usize, ty: usize, net: Option<Net>) {
        let entry = self.no_via.entry((lower.0, tx, ty)).or_insert(net);
        if *entry != net {
            *entry = None;
        }
    }

    /// Whether `net` may place a via between the layers of `from` and `to`.
    fn via_allowed(&self, from: Pos, to: Pos, net: Net) -> bool {
        if self.no_via.is_empty() || from.layer == to.layer {
            return true;
        }
        let lower = std::cmp::min(from.layer, to.layer);
        match self.no_via.get(&(lower.0, from.tx, from.ty)) {
            None => true,
            Some(only) => *only == Some(net),
        }
    }

    /// Occupies `positions` for `net` as one connected group, merging any
    /// groups they already belong to.
    pub fn occupy_positions(&mut self, positions: &[Pos], net: Net) {
        let mut groups = Vec::new();
        for pos in positions {
            if let State::Occupied { conn_group, .. } = self.state(*pos) {
                groups.push(conn_group);
            }
        }
        let group = match groups.first() {
            None => self.nets.get_unused_conn_group(),
            Some(&first) => {
                for &other in &groups[1..] {
                    if other != first {
                        for pos in self.nets.pos_in_group(other).unwrap() {
                            self.occupy_inner(pos, net, first);
                        }
                    }
                }
                first
            }
        };
        for pos in positions {
            self.occupy_inner(pos.no_jump(), net, group);
        }
    }

    pub fn route_with_net(
        &mut self,
        src: PosSpan,
        dst: PosSpan,
        net: Net,
    ) -> Result<AbstractRoute> {
        let grid = self.grid(src.layer);
        let (nx, ny) = (grid.rows(), grid.cols());
        assert!(src.tx_max >= src.tx_min);
        assert!(src.ty_max >= src.ty_min);
        assert!(src.tx_max < nx);
        assert!(src.ty_max < ny);
        assert!(dst.tx_max < nx);
        assert!(dst.ty_max < ny);
        assert!(dst.tx_min <= dst.tx_max);
        assert!(dst.ty_min <= dst.ty_max);
        let successors = |pos: &Node| self.successors(*pos, dst, net);
        let success = |pos: &Node| match pos {
            Node::Span(_) => false,
            Node::Pos(p) => dst.contains(*p),
        };
        let nodes = pathfinding::directed::bfs::bfs(&Node::Span(src), successors, success)
            .ok_or(Error::NoRouteFound)?;

        let mut groups = Vec::new();
        for node in nodes.iter().skip(1) {
            let node = &node.unwrap_pos();
            let state = self.grid(node.layer).get(node.tx, node.ty).unwrap();
            if let State::Occupied { conn_group, .. } = state {
                groups.push(*conn_group);
            }
        }
        let group = if groups.is_empty() {
            self.nets.get_unused_conn_group()
        } else {
            for i in 1..groups.len() {
                for pos in self.nets.pos_in_group(groups[i]).unwrap() {
                    self.occupy_inner(pos, net, groups[0]);
                }
            }
            groups[0]
        };

        for node in nodes.iter().skip(1) {
            self.occupy_inner(node.unwrap_pos(), net, group);
        }

        Ok(nodes[1..].iter().map(|n| n.unwrap_pos()).collect_vec())
    }

    pub fn route(&mut self, src: PosSpan, dst: PosSpan) -> Result<AbstractRoute> {
        let net = self.get_unused_net();
        self.route_with_net(src, dst, net)
    }

    pub fn get_unused_net(&mut self) -> Net {
        self.nets.get_unused_net()
    }

    fn clip_grid_index(&self, index: usize, dir: Dir) -> usize {
        std::cmp::min(
            index,
            match dir {
                Dir::Horiz => self.tx - 1,
                Dir::Vert => self.ty - 1,
            },
        )
    }

    fn block_single_inner(&mut self, pos: Pos, net: Option<Net>) {
        let s = self.grid_mut(pos.layer).get_mut(pos.tx, pos.ty).unwrap();
        *s = match s {
            State::Occupied { .. } => {
                return;
            }
            State::Blocked { net: other } => {
                if net == *other {
                    State::Blocked { net }
                } else {
                    State::Blocked { net: None }
                }
            }
            State::Empty => State::Blocked { net },
        };
    }

    fn block_inner(&mut self, pos: Pos, net: Option<Net>) {
        self.block_single_inner(pos, net);
    }

    fn block_span_inner(&mut self, span: PosSpan, net: Option<Net>) {
        for tx in span.tx_min..=self.clip_grid_index(span.tx_max, Dir::Horiz) {
            for ty in span.ty_min..=self.clip_grid_index(span.ty_max, Dir::Vert) {
                let pos = Pos::new(span.layer, tx, ty);
                self.block_single_inner(pos, net);
            }
        }
    }

    /// Blocks the given `Pos`, leaving it unchanged if it is already occupied.
    pub fn block(&mut self, pos: Pos) {
        self.block_inner(pos, None)
    }

    /// Blocks the given `PosSpan`, leaving occupied positions unchanged.
    pub fn block_span(&mut self, span: PosSpan) {
        self.block_span_inner(span, None)
    }

    /// Blocks the given `Pos` for the provided `Net`, leaving it unchanged if it is already occupied.
    pub fn block_for_net(&mut self, pos: Pos, net: Net) {
        self.block_inner(pos, Some(net));
    }

    /// Blocks the given `PosSpan` for the provided `Net`, leaving occupied positions unchanged.
    pub fn block_span_for_net(&mut self, span: PosSpan, net: Net) {
        self.block_span_inner(span, Some(net));
    }

    fn occupy_inner(&mut self, pos: Pos, net: Net, conn_group: ConnectionGroup) {
        if let State::Occupied {
            conn_group: old_group,
            ..
        } = self.grid(pos.layer).get(pos.tx, pos.ty).unwrap()
        {
            self.nets.delete_from_group(pos, *old_group);
        }

        *self.grid_mut(pos.layer).get_mut(pos.tx, pos.ty).unwrap() = State::Occupied {
            net,
            via: HasVia::No,
            conn_group,
        };
        self.nets.add_to_group(pos, conn_group);
    }

    pub fn occupy(&mut self, pos: Pos, net: Net) -> Result<()> {
        let group = if let State::Occupied { conn_group, .. } =
            self.grid(pos.layer).get(pos.tx, pos.ty).unwrap()
        {
            *conn_group
        } else {
            self.nets.get_unused_conn_group()
        };
        self.occupy_inner(pos, net, group);
        Ok(())
    }
    pub fn occupy_span(&mut self, span: PosSpan, net: Net) -> Result<()> {
        let mut groups = Vec::new();
        for tx in span.tx_min..=self.clip_grid_index(span.tx_max, Dir::Horiz) {
            for ty in span.ty_min..=self.clip_grid_index(span.ty_max, Dir::Vert) {
                let pos = Pos::new(span.layer, tx, ty);
                if let State::Occupied { conn_group, .. } =
                    self.grid(pos.layer).get(pos.tx, pos.ty).unwrap()
                {
                    groups.push(*conn_group);
                }
            }
        }
        let group = if groups.is_empty() {
            self.nets.get_unused_conn_group()
        } else {
            for i in 1..groups.len() {
                for pos in self.nets.pos_in_group(groups[i]).unwrap() {
                    self.occupy_inner(pos, net, groups[0]);
                }
            }
            groups[0]
        };
        for tx in span.tx_min..=self.clip_grid_index(span.tx_max, Dir::Horiz) {
            for ty in span.ty_min..=self.clip_grid_index(span.ty_max, Dir::Vert) {
                let pos = Pos::new(span.layer, tx, ty);
                self.occupy_inner(pos, net, group);
            }
        }

        Ok(())
    }

    pub fn segments(&self, layer: Layer) -> Vec<Segment> {
        let mut out = Vec::new();
        let info = self.layer_info(layer);
        let p_round = self.parallel_grid_spacing(layer);

        let max_grid = round_down(info.num_parallel_grid_points() - 1, p_round);
        // 0 always rounds up to 0, so this doesn't do anything.
        // It's here just in case we change behavior later.
        let min_grid = round_up(0, p_round);

        let dir = info.dir;

        for i in 0..info.num_tracks() {
            let tid = i * info.grid_space;
            for (empty, run) in &info
                .iter_track(tid)
                .enumerate()
                .chunk_by(|(_, s)| s.is_empty())
            {
                if !empty {
                    continue;
                }
                let (p_min, p_max) = run.map(|(x, _)| x).minmax().into_option().unwrap();
                let p_min_r = round_up(p_min, p_round);
                let p_max_r = round_down(p_max, p_round);
                if p_min_r >= p_max_r {
                    continue;
                }
                let span = PosSpanBuilder::with_layer(layer)
                    .with(dir, p_min_r, p_max_r)
                    .with(!dir, tid, tid)
                    .build();

                let lower_boundary = p_min_r <= min_grid;
                let upper_boundary = p_max_r >= max_grid;

                out.push(Segment {
                    track_id: i,
                    span,
                    lower_boundary,
                    upper_boundary,
                });
            }
        }
        out
    }

    fn pos_next(&self, pos: Pos, dst_span: PosSpan, net: Net) -> Vec<Node> {
        let mut candidates = Vec::new();
        for action in PosAction::all() {
            if self.is_valid_action(pos, action) {
                candidates.push(pos.next(action));
            }
        }

        let state = self.grid(pos.layer).get(pos.tx, pos.ty).unwrap();

        let mut filtered_candidates = candidates
            .into_iter()
            .filter(|n| {
                let val = self.grid(n.layer).get(n.tx, n.ty);
                val.map(|s| s.is_empty() || s.is_occupied_by(net) || s.is_blocked_by(net))
                    .unwrap_or_default()
                    || dst_span.contains(*n)
            })
            .filter(|n| self.via_allowed(pos, *n, net))
            .map(Node::Pos)
            .collect_vec();

        if let State::Occupied {
            conn_group,
            net: other,
            ..
        } = state
            && *other == net
        {
            let pos_list = self.nets.pos_in_group(*conn_group).unwrap();
            filtered_candidates.extend(pos_list.into_iter().map(|pos| Node::Pos(pos.mark_jump())));
        }

        filtered_candidates
    }

    fn is_valid_action(&self, pos: Pos, action: PosAction) -> bool {
        let layer_info = self.layer_info(pos.layer);
        let grid = &layer_info.grid;

        // Check boundary conditions.
        if !match action {
            PosAction::Up => pos.ty < grid.cols() - 1,
            PosAction::Down => pos.ty > 0,
            PosAction::Right => pos.tx < grid.rows() - 1,
            PosAction::Left => pos.tx > 0,
            PosAction::ZUp => pos.layer.0 < self.layers.len() - 1,
            PosAction::ZDown => pos.layer.0 > 0,
        } {
            return false;
        }

        // Check layer direction matches up with action.
        match layer_info.dir {
            Dir::Horiz => {
                if action == PosAction::Up || action == PosAction::Down {
                    return false;
                }
            }
            Dir::Vert => {
                if action == PosAction::Right || action == PosAction::Left {
                    return false;
                }
            }
        }

        // Ensure that next position is on the grid for its corresponding layer.
        let next_pos = pos.next(action);
        let next_layer_info = self.layer_info(next_pos.layer);
        next_pos
            .coord(!next_layer_info.dir)
            .is_multiple_of(next_layer_info.grid_space)
    }

    fn span_next(&self, span: PosSpan) -> Vec<Node> {
        let mut next =
            Vec::with_capacity((span.tx_max - span.tx_min + 1) * (span.ty_max - span.ty_min + 1));
        for tx in span.tx_min..=self.clip_grid_index(span.tx_max, Dir::Horiz) {
            for ty in span.ty_min..=self.clip_grid_index(span.ty_max, Dir::Vert) {
                let pos = Pos::new(span.layer, tx, ty);
                next.push(Node::Pos(pos));
            }
        }
        next
    }

    fn successors(&self, node: Node, dst_span: PosSpan, net: Net) -> Vec<Node> {
        match node {
            Node::Pos(pos) => self.pos_next(pos, dst_span, net),
            Node::Span(span) => self.span_next(span),
        }
    }

    fn grid(&self, layer: Layer) -> &Grid<State> {
        &self.layers[layer.0].grid
    }

    fn grid_mut(&mut self, layer: Layer) -> &mut Grid<State> {
        &mut self.layers[layer.0].grid
    }

    pub(crate) fn dir(&self, layer: Layer) -> Dir {
        self.layers[layer.0].dir
    }

    pub(crate) fn layer_info(&self, layer: Layer) -> &AbstractLayerInfo {
        &self.layers[layer.0]
    }

    pub(crate) fn parallel_grid_spacing(&self, layer: Layer) -> usize {
        let above = if layer.0 < self.layers.len() - 1 {
            self.layer_info(layer.above()).grid_space
        } else {
            1
        };
        let below = if let Some(below) = layer.below() {
            self.layer_info(below).grid_space
        } else {
            1
        };
        std::cmp::max(above, below)
    }
}

fn round_down(x: usize, grid: usize) -> usize {
    (x / grid) * grid
}

fn round_up(x: usize, grid: usize) -> usize {
    x.div_ceil(grid) * grid
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_round_down() {
        assert_eq!(round_down(331, 200), 200);
        assert_eq!(round_down(400, 331), 331);
        assert_eq!(round_down(900, 300), 900);
        assert_eq!(round_down(901, 300), 900);
        assert_eq!(round_down(900, 299), 897);
    }

    #[test]
    fn test_basic_greedy_two_layer_abstract_routing() {
        let mut router = GreedyAbstractRouter::new(
            vec![
                AbstractLayerConfig {
                    grid_space: 1,
                    dir: Dir::Horiz,
                },
                AbstractLayerConfig {
                    grid_space: 1,
                    dir: Dir::Vert,
                },
            ],
            1_000,
            1_000,
        );

        router
            .route(
                Pos::new(Layer(0), 0, 0).into(),
                Pos::new(Layer(1), 4, 4).into(),
            )
            .expect("failed to route");
    }

    /// A route that joins an existing route of its own net may start from any point of
    /// that route, and here five of them are equally close to the destination. The
    /// choice must not depend on hash iteration order, which differs between router
    /// instances and between processes.
    #[test]
    fn test_jump_successor_choice_is_deterministic() {
        let route_once = || {
            let mut router = GreedyAbstractRouter::new(
                vec![
                    AbstractLayerConfig {
                        grid_space: 1,
                        dir: Dir::Horiz,
                    },
                    AbstractLayerConfig {
                        grid_space: 1,
                        dir: Dir::Vert,
                    },
                ],
                20,
                20,
            );
            let net = router.get_unused_net();
            router
                .route_with_net(
                    Pos::new(Layer(0), 0, 10).into(),
                    Pos::new(Layer(0), 19, 10).into(),
                    net,
                )
                .expect("failed to route the trunk");
            // Reachable in the same number of steps from (3..=7, 10) on the trunk.
            let dst = PosSpanBuilder::with_layer(Layer(0))
                .with(Dir::Horiz, 3, 7)
                .with(Dir::Vert, 15, 15)
                .build();
            router
                .route_with_net(Pos::new(Layer(0), 0, 10).into(), dst, net)
                .expect("failed to route the branch")
        };
        let first = route_once();
        for _ in 0..32 {
            assert_eq!(route_once(), first);
        }
    }
}
