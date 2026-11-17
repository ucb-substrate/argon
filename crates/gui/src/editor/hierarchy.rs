//! Prepare hierarchy metadata once per immutable cell, sharing unchanged paths
//! and bounds between GUI snapshots.
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use argonc::compile::{CellId, ScopeId, SolvedValue, bbox_dim_union, bbox_text_union, ifmatvec};
use geometry::transform::TransformationMatrix;
use rustc_hash::FxHashMap;

use super::{
    CompileOutputState, CompiledData, LayerState, ProcessScopeState, Rect, ScopeAddress,
    ScopeState, mark_layer_used,
};
use gpui::SharedString;

#[derive(Debug)]
pub struct PreparedScope {
    pub name: Arc<str>,
    /// Shared because long chains of function/control-flow scopes commonly
    /// have exactly the same bounds as their only child.
    pub bbox: Option<Arc<ScopeBounds>>,
    // Most generated execution scopes have zero or one child. Keep those
    // inline instead of performing a heap allocation per scope.
    pub children: smallvec::SmallVec<[ScopeAddress; 2]>,
}

/// The bounds of a scope's geometry, as [`argonc::compile::bbox_union`]
/// combines them.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScopeBounds {
    pub x0: f64,
    pub y0: f64,
    pub x1: f64,
    pub y1: f64,
}

impl ScopeBounds {
    fn of(rect: &Rect<f64>) -> Self {
        Self {
            x0: rect.x0,
            y0: rect.y0,
            x1: rect.x1,
            y1: rect.y1,
        }
    }

    fn union(self, other: Self) -> Self {
        Self {
            x0: self.x0.min(other.x0),
            y0: self.y0.min(other.y0),
            x1: self.x1.max(other.x1),
            y1: self.y1.max(other.y1),
        }
    }
}

/// A scope of a [`ScopeStates`].
#[derive(Clone, Copy, Debug)]
pub struct ScopeView<'a> {
    pub visible: bool,
    pub parent: Option<ScopeAddress>,
    pub prepared: &'a Arc<PreparedScope>,
}

impl std::ops::Deref for ScopeView<'_> {
    type Target = PreparedScope;

    fn deref(&self) -> &Self::Target {
        self.prepared
    }
}

/// Every prepared scope, by address.
type PreparedScopes = FxHashMap<ScopeAddress, Arc<PreparedScope>>;

/// The scopes of one cell: each scope's parent within the cell, and its
/// prepared data. The entry of the cell's root names the root itself.
type CellScopeMap = FxHashMap<ScopeId, (ScopeId, Arc<PreparedScope>)>;

#[derive(Clone, Debug, Default)]
struct CellScopes {
    /// The scope whose parent is in another cell, or which has none.
    root: Option<ScopeId>,
    root_parent: Option<ScopeAddress>,
    scopes: Arc<CellScopeMap>,
}

/// Every scope of a prepared hierarchy, with its parent and visibility.
///
/// Scopes are grouped by cell, so a hierarchy prepared after an edit shares
/// the scopes of each unchanged cell with the one before it.
#[derive(Clone, Debug, Default)]
pub struct ScopeStates {
    cells: FxHashMap<CellId, CellScopes>,
    /// The scopes the user hid.
    hidden: HashSet<ScopeAddress>,
    len: usize,
    /// The distinct cells each cell of the compiled output instantiates.
    instances: HashMap<CellId, Arc<[CellId]>>,
}

impl ScopeStates {
    pub fn get(&self, address: &ScopeAddress) -> Option<ScopeView<'_>> {
        let cell = self.cells.get(&address.cell)?;
        let (parent, prepared) = cell.scopes.get(&address.scope)?;
        Some(self.view(cell, address, *parent, prepared))
    }

    fn view<'a>(
        &self,
        cell: &CellScopes,
        address: &ScopeAddress,
        parent: ScopeId,
        prepared: &'a Arc<PreparedScope>,
    ) -> ScopeView<'a> {
        ScopeView {
            visible: self.hidden.is_empty() || !self.hidden.contains(address),
            parent: if cell.root == Some(address.scope) {
                cell.root_parent
            } else {
                Some(ScopeAddress {
                    cell: address.cell,
                    scope: parent,
                })
            },
            prepared,
        }
    }

    /// The scope at `address`, which must be in the hierarchy.
    #[track_caller]
    pub fn scope(&self, address: &ScopeAddress) -> ScopeView<'_> {
        self.get(address)
            .unwrap_or_else(|| panic!("scope {address:?} is not in the hierarchy"))
    }

    pub fn contains_key(&self, address: &ScopeAddress) -> bool {
        self.cells
            .get(&address.cell)
            .is_some_and(|cell| cell.scopes.contains_key(&address.scope))
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn iter(&self) -> impl Iterator<Item = (ScopeAddress, ScopeView<'_>)> {
        self.cells.iter().flat_map(move |(cell_id, cell)| {
            cell.scopes.iter().map(move |(scope, (parent, prepared))| {
                let address = ScopeAddress {
                    cell: *cell_id,
                    scope: *scope,
                };
                (address, self.view(cell, &address, *parent, prepared))
            })
        })
    }

    pub fn values(&self) -> impl Iterator<Item = ScopeView<'_>> {
        self.iter().map(|(_, scope)| scope)
    }

    /// Whether every scope is visible.
    pub fn all_visible(&self) -> bool {
        self.hidden.is_empty()
    }

    /// Adds or replaces the scope at `address`. A parent in another cell, or
    /// none, makes it its cell's root.
    pub fn insert(&mut self, address: ScopeAddress, state: ScopeState) {
        let ScopeState {
            visible,
            parent,
            prepared,
        } = state;
        let cell = self.cells.entry(address.cell).or_default();
        let parent_in_cell = parent
            .filter(|parent| parent.cell == address.cell)
            .map(|parent| parent.scope);
        if parent_in_cell.is_none() {
            cell.root = Some(address.scope);
            cell.root_parent = parent;
        } else if cell.root == Some(address.scope) {
            cell.root = None;
            cell.root_parent = None;
        }
        let entry = (parent_in_cell.unwrap_or(address.scope), prepared);
        let unchanged = cell
            .scopes
            .get(&address.scope)
            .is_some_and(|(parent, prepared)| {
                *parent == entry.0 && Arc::ptr_eq(prepared, &entry.1)
            });
        if !unchanged
            && Arc::make_mut(&mut cell.scopes)
                .insert(address.scope, entry)
                .is_none()
        {
            self.len += 1;
        }
        self.set_visible(address, visible);
    }

    pub fn remove(&mut self, address: &ScopeAddress) {
        let Some(cell) = self.cells.get_mut(&address.cell) else {
            return;
        };
        if !cell.scopes.contains_key(&address.scope) {
            return;
        }
        self.len -= 1;
        if cell.scopes.len() == 1 {
            self.cells.remove(&address.cell);
        } else {
            Arc::make_mut(&mut cell.scopes).remove(&address.scope);
            if cell.root == Some(address.scope) {
                cell.root = None;
                cell.root_parent = None;
            }
        }
        if !self.hidden.is_empty() {
            self.hidden.remove(address);
        }
    }

    /// Removes every scope of `cell`.
    fn remove_cell(&mut self, cell: CellId) {
        if let Some(removed) = self.cells.remove(&cell) {
            self.len -= removed.scopes.len();
            if !self.hidden.is_empty() {
                self.hidden.retain(|address| address.cell != cell);
            }
        }
    }

    pub fn set_visible(&mut self, address: ScopeAddress, visible: bool) {
        if visible {
            if !self.hidden.is_empty() {
                self.hidden.remove(&address);
            }
        } else {
            self.hidden.insert(address);
        }
    }

    pub fn set_all_visible(&mut self, visible: bool) {
        self.hidden = if visible {
            HashSet::new()
        } else {
            self.iter().map(|(address, _)| address).collect()
        };
    }

    pub fn clear(&mut self) {
        *self = Self::default();
    }
}

/// The distinct cells `cell` instantiates.
fn instantiated_cells(cell: &argonc::compile::CompiledCell) -> Arc<[CellId]> {
    let mut cells = cell
        .objects
        .values()
        .filter_map(|value| match value {
            SolvedValue::Instance(instance) => Some(instance.cell),
            _ => None,
        })
        .collect::<Vec<_>>();
    cells.sort_unstable();
    cells.dedup();
    cells.into()
}

/// The cells each cell of `output` instantiates, reusing the lists of the
/// cells it shares with `previous`.
fn cell_instances(
    output: &CompiledData,
    previous: Option<&CompileOutputState>,
) -> HashMap<CellId, Arc<[CellId]>> {
    output
        .cells
        .iter()
        .map(|(id, cell)| {
            let known = previous
                .filter(|old| {
                    old.output
                        .cells
                        .get(id)
                        .is_some_and(|old| Arc::ptr_eq(old, cell))
                })
                .and_then(|old| old.state.instances.get(id).cloned());
            (*id, known.unwrap_or_else(|| instantiated_cells(cell)))
        })
        .collect()
}

#[derive(Debug, Default)]
pub(super) struct PreparedHierarchy {
    scopes: PreparedScopes,
    /// See [`CompileOutputState::all_scopes_visible`].
    pub(super) all_scopes_visible: bool,
}

impl PreparedHierarchy {
    /// Resolve a selection from the preceding snapshot by its displayed name
    /// path. Addresses include a content-derived cell ID and can therefore
    /// change after an otherwise non-structural source edit.
    pub(super) fn remap_path(
        &self,
        root: ScopeAddress,
        old_state: &ScopeStates,
        old_selected: ScopeAddress,
    ) -> Option<ScopeAddress> {
        let mut names = Vec::new();
        let mut current = Some(old_selected);
        while let Some(address) = current {
            let scope = old_state.get(&address)?;
            names.push(scope.prepared.name.as_ref());
            current = scope.parent;
        }
        let mut names = names.into_iter().rev();
        if self.scopes.get(&root)?.name.as_ref() != names.next()? {
            return None;
        }
        let mut address = root;
        for name in names {
            address = self.scopes[&address]
                .children
                .iter()
                .rev()
                .copied()
                .find(|child| self.scopes[child].name.as_ref() == name)?;
        }
        Some(address)
    }
}

/// Where a scope's preparation has reached. A scope waits on each child that
/// is not yet prepared, so children finish first, in the order they are met.
enum GeometryPhase {
    Start,
    /// Revisiting an unchanged cell's scope: its children must be prepared
    /// before its cached bounds can be trusted.
    CheckEmits(usize),
    CheckChildren(usize),
    Emits(usize),
    Children(usize),
}

struct GeometryFrame {
    address: ScopeAddress,
    phase: GeometryPhase,
    bbox: Option<Arc<ScopeBounds>>,
    children: smallvec::SmallVec<[ScopeAddress; 2]>,
}

/// Prepares the bounds of `root` and every scope beneath it, recording the
/// layers they use in first-use order.
fn prepare_geometry(
    output: &CompiledData,
    root: ScopeAddress,
    scopes: &mut PreparedScopes,
    layers: &mut indexmap::IndexSet<String>,
    previous: Option<&CompileOutputState>,
    reusable: &HashSet<CellId>,
) {
    let frame = |address| GeometryFrame {
        address,
        phase: GeometryPhase::Start,
        bbox: None,
        children: smallvec::SmallVec::new(),
    };
    let instance_root = |cell: CellId| ScopeAddress {
        cell,
        scope: output.cells[&cell].root,
    };
    let mut stack = vec![frame(root)];
    while let Some(mut top) = stack.pop() {
        let address = top.address;
        let cell = &output.cells[&address.cell];
        let source_scope = &cell.scopes[&address.scope];
        let cached = previous
            .and_then(|old| old.state.get(&address))
            .map(|scope| scope.prepared);
        match top.phase {
            GeometryPhase::Start => {
                if scopes.contains_key(&address) {
                    continue;
                }
                if reusable.contains(&address.cell)
                    && let Some(cached) = cached
                {
                    // A content-derived cell and its whole instantiated closure are
                    // immutable. The old geometry is already prepared; walking its
                    // millions of execution scopes just to rediscover that costs more
                    // than recompiling a small parent edit.
                    scopes.insert(address, cached.clone());
                    continue;
                }
                let unchanged = cached.is_some()
                    && previous.is_some_and(|old| {
                        old.output
                            .cells
                            .get(&address.cell)
                            .is_some_and(|old| Arc::ptr_eq(old, cell))
                    });
                // An immutable parent's bounds can still change if a referenced
                // child changed. Walk dependencies in emit order both to validate them
                // and to preserve the renderer's first-use layer ordering.
                top.phase = if unchanged {
                    GeometryPhase::CheckEmits(0)
                } else {
                    GeometryPhase::Emits(0)
                };
                stack.push(top);
            }
            GeometryPhase::CheckEmits(index) => {
                let Some((object, _)) = source_scope.emit.iter().as_slice().get(index) else {
                    top.phase = GeometryPhase::CheckChildren(0);
                    stack.push(top);
                    continue;
                };
                top.phase = GeometryPhase::CheckEmits(index + 1);
                let child = match &cell.objects[object] {
                    SolvedValue::Instance(instance) => Some(instance_root(instance.cell)),
                    value => {
                        if let Some(layer) = object_layer(value) {
                            layers.insert(layer.to_owned());
                        }
                        None
                    }
                };
                stack.push(top);
                if let Some(child) = child {
                    stack.push(frame(child));
                }
            }
            GeometryPhase::CheckChildren(index) => {
                if let Some(child) = source_scope.children.iter().as_slice().get(index) {
                    top.phase = GeometryPhase::CheckChildren(index + 1);
                    stack.push(top);
                    stack.push(frame(ScopeAddress {
                        cell: address.cell,
                        scope: *child,
                    }));
                    continue;
                }
                let old = previous.unwrap();
                let cached = cached.unwrap();
                if cached.children.iter().all(|child| {
                    old.state
                        .get(child)
                        .is_some_and(|old| Arc::ptr_eq(old.prepared, &scopes[child]))
                }) {
                    scopes.insert(address, cached.clone());
                } else {
                    top.phase = GeometryPhase::Emits(0);
                    stack.push(top);
                }
            }
            GeometryPhase::Emits(index) => {
                let Some((object, _)) = source_scope.emit.iter().as_slice().get(index) else {
                    top.phase = GeometryPhase::Children(0);
                    stack.push(top);
                    continue;
                };
                match &cell.objects[object] {
                    SolvedValue::Instance(instance) => {
                        let child = instance_root(instance.cell);
                        let Some(prepared) = scopes.get(&child) else {
                            // Come back to this instance once its cell is ready.
                            stack.push(top);
                            stack.push(frame(child));
                            continue;
                        };
                        top.children.push(child);
                        top.bbox = shared_bbox_union(
                            top.bbox,
                            prepared
                                .bbox
                                .as_ref()
                                .map(|rect| Arc::new(instance_bbox(instance, rect))),
                        );
                    }
                    value => {
                        let bounds =
                            object_bbox(value).map(|rect| Arc::new(ScopeBounds::of(&rect)));
                        top.bbox = shared_bbox_union(top.bbox, bounds);
                        if let Some(layer) = object_layer(value) {
                            layers.insert(layer.to_owned());
                        }
                    }
                }
                top.phase = GeometryPhase::Emits(index + 1);
                stack.push(top);
            }
            GeometryPhase::Children(index) => {
                let Some(child) = source_scope.children.iter().as_slice().get(index) else {
                    scopes.insert(
                        address,
                        Arc::new(PreparedScope {
                            name: cell.scope_name_shared(address.scope),
                            bbox: top.bbox,
                            children: top.children,
                        }),
                    );
                    continue;
                };
                let child = ScopeAddress {
                    cell: address.cell,
                    scope: *child,
                };
                let Some(prepared) = scopes.get(&child) else {
                    stack.push(top);
                    stack.push(frame(child));
                    continue;
                };
                top.children.push(child);
                top.bbox = shared_bbox_union(top.bbox, prepared.bbox.clone());
                top.phase = GeometryPhase::Children(index + 1);
                stack.push(top);
            }
        }
    }
}

fn object_layer(value: &SolvedValue) -> Option<&str> {
    match value {
        SolvedValue::Rect(rect) => rect.layer.as_deref(),
        SolvedValue::Polygon(polygon) => Some(&polygon.layer),
        SolvedValue::Path(path) => Some(&path.layer),
        SolvedValue::Text(text) => Some(&text.layer),
        SolvedValue::Dimension(_) | SolvedValue::Instance(_) => None,
    }
}

fn object_bbox(value: &SolvedValue) -> Option<Rect<f64>> {
    match value {
        SolvedValue::Rect(rect) => Some(rect.to_float()),
        SolvedValue::Polygon(polygon) => polygon.bbox(),
        SolvedValue::Path(path) => path.bbox(),
        SolvedValue::Text(text) => bbox_text_union(None, text),
        SolvedValue::Dimension(dimension) => bbox_dim_union(None, dimension),
        SolvedValue::Instance(_) => None,
    }
}

/// The bounds of an instance whose cell's bounds are `rect`.
fn instance_bbox(instance: &argonc::compile::SolvedInstance, rect: &ScopeBounds) -> ScopeBounds {
    let mut matrix = TransformationMatrix::identity();
    if instance.reflect {
        matrix = matrix.reflect_vert();
    }
    matrix = matrix.rotate(instance.angle);
    let p0 = ifmatvec(matrix, (rect.x0, rect.y0));
    let p1 = ifmatvec(matrix, (rect.x1, rect.y1));
    ScopeBounds {
        x0: p0.0.min(p1.0) + instance.x,
        y0: p0.1.min(p1.1) + instance.y,
        x1: p0.0.max(p1.0) + instance.x,
        y1: p0.1.max(p1.1) + instance.y,
    }
}

fn shared_bbox_union(
    left: Option<Arc<ScopeBounds>>,
    right: Option<Arc<ScopeBounds>>,
) -> Option<Arc<ScopeBounds>> {
    match (left, right) {
        (None, right) => right,
        (left, None) => left,
        (Some(left), Some(right)) => Some(Arc::new(left.union(*right))),
    }
}

/// Records every scope under `root` with its parent and visibility. A scope
/// reached along several paths keeps the first, walking the last child first.
fn prepare_paths(
    root: ScopeAddress,
    hidden_paths: &HashSet<Vec<String>>,
    scopes: &PreparedScopes,
    state: &mut ProcessScopeState,
) {
    let mut path: Vec<String> = Vec::new();
    let mut stack = vec![(root, None, 0)];
    while let Some((address, parent, depth)) = stack.pop() {
        if state.state.contains_key(&address) {
            continue;
        }
        let scope = &scopes[&address];
        path.truncate(depth);
        path.push(scope.name.to_string());
        state.state.insert(
            address,
            ScopeState {
                parent,
                visible: !hidden_paths.contains(&path),
                prepared: Arc::clone(scope),
            },
        );
        stack.extend(
            scope
                .children
                .iter()
                .map(|child| (*child, Some(address), depth + 1)),
        );
    }
}

/// Populate the overwhelmingly common all-visible hierarchy without building
/// and allocating a displayed-name path for every execution scope. Name paths
/// are needed only to remap the small set of user-hidden scopes across edits.
fn prepare_visible_paths(
    root: ScopeAddress,
    scopes: &PreparedScopes,
    state: &mut ProcessScopeState,
) {
    let mut stack = vec![(root, None)];
    while let Some((address, parent)) = stack.pop() {
        if state.state.contains_key(&address) {
            continue;
        }
        let scope = &scopes[&address];
        state.state.insert(
            address,
            ScopeState {
                parent,
                visible: true,
                prepared: Arc::clone(scope),
            },
        );
        stack.extend(scope.children.iter().map(|child| (*child, Some(address))));
    }
}

fn hidden_paths(old: Option<&ScopeStates>) -> HashSet<Vec<String>> {
    old.into_iter()
        .flat_map(|state| {
            state.hidden.iter().map(|address| {
                let mut path = Vec::new();
                let mut current = Some(*address);
                while let Some(address) = current {
                    let scope = state
                        .get(&address)
                        .expect("a hidden scope is in the hierarchy");
                    path.push(scope.name.to_string());
                    current = scope.parent;
                }
                path.reverse();
                path
            })
        })
        .collect()
}

/// Geometry edits usually preserve the named hierarchy, even when source cells
/// acquire new handles. Check the ordered DAG and its aliasing, then update only
/// affected metadata in persistent maps. Structural edits use the full path
/// builder, which also handles removals and path collisions.
fn reuse_paths(
    root: ScopeAddress,
    scopes: &PreparedScopes,
    state: &mut ProcessScopeState,
    old: &CompileOutputState,
) -> bool {
    let old_root = ScopeAddress {
        cell: old.output.top,
        scope: old.output.cells[&old.output.top].root,
    };
    let mut remap = HashMap::new();
    let mut inverse = HashMap::new();
    let mut pending = vec![(old_root, root)];
    while let Some((before, after)) = pending.pop() {
        if let Some(mapped) = remap.get(&before) {
            if *mapped != after {
                return false;
            }
            continue;
        }
        if inverse.insert(after, before).is_some() {
            return false;
        }
        let Some(a) = old.state.get(&before).map(|scope| scope.prepared) else {
            return false;
        };
        let b = &scopes[&after];
        if a.name != b.name || a.children.len() != b.children.len() {
            return false;
        }
        remap.insert(before, after);
        pending.extend(a.children.iter().copied().zip(b.children.iter().copied()));
    }
    let old_scope = |address: &ScopeAddress| {
        old.state
            .get(address)
            .expect("a remapped scope is in the old hierarchy")
    };
    let changed: HashSet<_> = remap
        .iter()
        .filter_map(|(before, after)| {
            (!Arc::ptr_eq(old_scope(before).prepared, &scopes[after])).then_some(*before)
        })
        .collect();
    state.state = old.state.as_ref().clone();
    for (before, after) in &remap {
        let scope = old_scope(before);
        let address = remap[before];
        let parent = scope.parent.map(|parent| remap[&parent]);
        if before != after {
            state.state.remove(before);
        }
        if changed.contains(before) || address != *before || parent != scope.parent {
            state.state.insert(
                *after,
                ScopeState {
                    parent,
                    visible: scope.visible,
                    prepared: Arc::clone(&scopes[&address]),
                },
            );
        }
    }
    true
}

/// A reused cell is safe to keep as an opaque prepared subtree only when its
/// own data and every referenced cell are the same immutable allocations.
fn reusable_cells(
    output: &CompiledData,
    old: &CompileOutputState,
    instances: &HashMap<CellId, Arc<[CellId]>>,
) -> HashSet<CellId> {
    let same = |id: &CellId| {
        output.cells.get(id).is_some_and(|cell| {
            old.output
                .cells
                .get(id)
                .is_some_and(|before| Arc::ptr_eq(before, cell))
        })
    };
    // Cached parents can retain their allocation while a referenced
    // child is replaced (notably in generated and test hierarchies). The
    // parent bbox and render index are reusable only if its closure is.
    let mut memo: HashMap<CellId, bool> = HashMap::new();
    for id in output.cells.keys() {
        // A cell is decided after every cell it instantiates.
        let mut stack = vec![(*id, false)];
        while let Some((id, children_done)) = stack.pop() {
            if children_done {
                let reusable = instances[&id]
                    .iter()
                    .all(|child| memo.get(child) == Some(&true));
                memo.insert(id, reusable);
                continue;
            }
            if memo.contains_key(&id) {
                continue;
            }
            if !same(&id) {
                memo.insert(id, false);
                continue;
            }
            stack.push((id, true));
            for child in instances[&id].iter() {
                if !memo.contains_key(child) {
                    stack.push((*child, false));
                }
            }
        }
    }
    memo.into_iter()
        .filter_map(|(id, reusable)| reusable.then_some(id))
        .collect()
}

/// Update only changed cells in the persistent scope map. A common parent
/// rectangle edit changes a handful of scopes, while the SRAM beneath it has
/// millions of unchanged execution scopes; touching those again is unnecessary.
fn reuse_unchanged_paths(
    root: ScopeAddress,
    scopes: &PreparedScopes,
    state: &mut ProcessScopeState,
    old: &CompileOutputState,
    reusable: &HashSet<CellId>,
) -> bool {
    // Hidden scopes within a changed cell need name-path remapping. Let the
    // structural fallback handle that uncommon case without losing visibility.
    if old
        .state
        .hidden
        .iter()
        .any(|address| !reusable.contains(&address.cell))
    {
        return false;
    }

    state.state = old.state.as_ref().clone();
    for id in old.output.cells.keys() {
        if !reusable.contains(id) {
            state.state.remove_cell(*id);
        }
    }
    let state = &mut state.state;
    let mut seen = HashSet::new();
    let mut stack = vec![(root, None)];
    while let Some((address, parent)) = stack.pop() {
        if !seen.insert(address) {
            continue;
        }
        let Some(prepared) = scopes.get(&address) else {
            return false;
        };
        if reusable.contains(&address.cell) {
            let Some(existing) = state.get(&address) else {
                return false;
            };
            if existing.parent != parent {
                let existing = ScopeState {
                    parent,
                    visible: existing.visible,
                    prepared: Arc::clone(existing.prepared),
                };
                state.insert(address, existing);
            }
            continue;
        }
        state.insert(
            address,
            ScopeState {
                parent,
                visible: true,
                prepared: Arc::clone(prepared),
            },
        );
        stack.extend(
            prepared
                .children
                .iter()
                .map(|child| (*child, Some(address))),
        );
    }
    true
}

fn changed_cell_layers(output: &CompiledData, reusable: &HashSet<CellId>) -> HashSet<String> {
    let mut layers = HashSet::new();
    for (id, cell) in &output.cells {
        if reusable.contains(id) {
            continue;
        }
        for object in cell.objects.values() {
            match object {
                SolvedValue::Rect(rect) => {
                    if let Some(layer) = &rect.layer {
                        layers.insert(layer.clone());
                    }
                }
                SolvedValue::Polygon(polygon) => {
                    layers.insert(polygon.layer.clone());
                }
                SolvedValue::Path(path) => {
                    layers.insert(path.layer.clone());
                }
                SolvedValue::Text(text) => {
                    layers.insert(text.layer.clone());
                }
                _ => {}
            }
        }
    }
    layers
}

pub(super) fn prepare(
    output: &CompiledData,
    root: ScopeAddress,
    state: &mut ProcessScopeState,
    old: Option<&ScopeStates>,
    previous: Option<&CompileOutputState>,
    previous_layers: Option<&indexmap::IndexMap<SharedString, LayerState>>,
) -> PreparedHierarchy {
    let instances = cell_instances(output, previous);
    let hierarchy = prepare_scopes(
        output,
        root,
        state,
        old,
        previous,
        previous_layers,
        &instances,
    );
    state.state.instances = instances;
    hierarchy
}

fn prepare_scopes(
    output: &CompiledData,
    root: ScopeAddress,
    state: &mut ProcessScopeState,
    old: Option<&ScopeStates>,
    previous: Option<&CompileOutputState>,
    previous_layers: Option<&indexmap::IndexMap<SharedString, LayerState>>,
    instances: &HashMap<CellId, Arc<[CellId]>>,
) -> PreparedHierarchy {
    let mut scopes = PreparedScopes::default();
    let mut used_layers = indexmap::IndexSet::new();
    if let (Some(previous), Some(previous_layers)) = (previous, previous_layers) {
        let profile_start = web_time::Instant::now();
        let reusable = reusable_cells(output, previous, instances);
        let reusable_elapsed = profile_start.elapsed();
        let old_changed_layers = changed_cell_layers(&previous.output, &reusable);
        let new_changed_layers = changed_cell_layers(output, &reusable);
        let layers_elapsed = profile_start.elapsed();
        let same_layer_names = previous
            .output
            .tech
            .layers
            .iter()
            .map(|layer| layer.name.as_str())
            .eq(output.tech.layers.iter().map(|layer| layer.name.as_str()));
        let no_new_unknown_layers = new_changed_layers.iter().all(|layer| {
            state.layers.contains_key(layer.as_str())
                || previous_layers.contains_key(layer.as_str())
        });
        if same_layer_names
            && no_new_unknown_layers
            && old_changed_layers.is_subset(&new_changed_layers)
        {
            prepare_geometry(
                output,
                root,
                &mut scopes,
                &mut used_layers,
                Some(previous),
                &reusable,
            );
            let geometry_elapsed = profile_start.elapsed();
            if reuse_unchanged_paths(root, &scopes, state, previous, &reusable) {
                let paths_elapsed = profile_start.elapsed();
                for (name, layer) in previous_layers {
                    if layer.used {
                        mark_layer_used(state, name);
                    }
                }
                for layer in new_changed_layers {
                    mark_layer_used(state, &layer);
                }
                if std::env::var_os("ARGON_PROFILE_HIERARCHY").is_some() {
                    eprintln!(
                        "hierarchy_fast,reusable_ms={},layers_ms={},geometry_ms={},paths_ms={},total_ms={},reused_cells={},new_scopes={}",
                        reusable_elapsed.as_millis(),
                        layers_elapsed.as_millis(),
                        geometry_elapsed.as_millis(),
                        paths_elapsed.as_millis(),
                        profile_start.elapsed().as_millis(),
                        reusable.len(),
                        scopes.len(),
                    );
                }
                // Reused scopes keep their visibility and new ones are
                // visible, and no changed cell had a hidden scope.
                return PreparedHierarchy {
                    scopes,
                    all_scopes_visible: previous.all_scopes_visible,
                };
            }
            scopes.clear();
            state.state.clear();
            used_layers.clear();
        }
    }
    prepare_geometry(
        output,
        root,
        &mut scopes,
        &mut used_layers,
        previous,
        &HashSet::new(),
    );
    for layer in &used_layers {
        mark_layer_used(state, layer);
    }
    // Remapped scopes keep their previous visibility.
    let all_scopes_visible = if let Some(previous) =
        previous.filter(|previous| reuse_paths(root, &scopes, state, previous))
    {
        previous.all_scopes_visible
    } else {
        let hidden_paths = hidden_paths(old);
        if hidden_paths.is_empty() {
            prepare_visible_paths(root, &scopes, state);
            true
        } else {
            prepare_paths(root, &hidden_paths, &scopes, state);
            state.state.all_visible()
        }
    };
    PreparedHierarchy {
        scopes,
        all_scopes_visible,
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::editor::LayerState;
    use crate::editor::canvas::ShapeFill;
    use gpui::{SharedString, rgb};
    use std::hash::{DefaultHasher, Hash, Hasher};

    fn assert_equivalent(actual: &ProcessScopeState, expected: &ProcessScopeState) {
        assert_eq!(actual.state.len(), expected.state.len());
        for (path, expected) in expected.state.iter() {
            let actual = actual.state.get(&path).unwrap();
            assert_eq!(actual.name, expected.name);
            assert_eq!(actual.parent, expected.parent);
            assert_eq!(actual.visible, expected.visible);
            assert_eq!(
                format!("{:?}", actual.bbox),
                format!("{:?}", expected.bbox),
                "bounds for {path:?}"
            );
        }
        assert_eq!(actual.layers.len(), expected.layers.len());
        for ((actual_name, actual), (expected_name, expected)) in
            actual.layers.iter().zip(&expected.layers)
        {
            assert_eq!(actual_name, expected_name);
            assert_eq!(actual.used, expected.used);
            assert_eq!(actual.visible, expected.visible);
            assert_eq!(actual.color, expected.color);
            assert_eq!(actual.z, expected.z);
        }
    }

    #[test]
    fn shared_hierarchy_matches_expanded_reference_and_preserves_hidden_paths() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("lib.ar");
        std::fs::write(
            &source,
            r#"
use std::layout::{inst, rect};
cell leaf(width: Float) { let r = rect("met1", x0=0., y0=0., x1=width, y1=5.); }
cell shared() {
    for i in std::range(8) {
        let a = inst(leaf(10.), x=(i as Float)*20., y=0.);
        let b = inst(leaf(15.), x=(i as Float)*20., y=10., angle=90, reflect=true);
    }
}
cell branch() { let a = inst(shared(), x=20., y=30.); }
cell top() {
    let first = inst(shared(), x=0., y=0.);
    let other = inst(branch(), x=500., y=40., angle=270);
    let last = inst(shared(), x=0., y=300., reflect=true);
}
"#,
        )
        .unwrap();
        let config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../examples/tech/basic.tech.toml"),
        ));
        let ast = argonc::parse::parse_workspace_with_config(&config).ast();
        let output = argonc::compile::compile(
            &ast,
            argonc::compile::CompileInput {
                cell: &["top"],
                args: vec![],
            },
            &config,
        )
        .unwrap_valid();
        let root = ScopeAddress {
            cell: output.top,
            scope: output.cells[&output.top].root,
        };
        let mut expected = ProcessScopeState::default();
        Reference::process_scope_reference(&output, root, &mut expected, None, None);
        let mut actual = ProcessScopeState::default();
        prepare(&output, root, &mut actual, None, None, None);
        assert_equivalent(&actual, &expected);
        let mut old = expected.state;
        let addresses = old.iter().map(|(address, _)| address).collect::<Vec<_>>();
        for (index, address) in addresses.into_iter().enumerate() {
            old.set_visible(address, index % 2 == 0);
        }
        let mut expected = ProcessScopeState::default();
        Reference::process_scope_reference(&output, root, &mut expected, None, Some(&old));
        let mut actual = ProcessScopeState::default();
        prepare(&output, root, &mut actual, Some(&old), None, None);
        assert_equivalent(&actual, &expected);
    }

    #[test]
    fn incremental_geometry_and_structural_edits_match_fresh_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("lib.ar");
        let config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../examples/tech/basic.tech.toml"),
        ));
        let base = r#"
use std::layout::{inst, rect};
cell leaf() { let r = rect("met1", x0=0., y0=0., x1=10., y1=5.); }
cell branch() { for i in std::range(4) { let child = inst(leaf(), x=(i as Float)*20., y=0.); } }
cell top() { let a = inst(branch(), x=0., y=0.); let b = inst(branch(), x=100., y=0.); }
"#;
        std::fs::write(&source, base).unwrap();
        let programs = [
            base.to_owned(),
            base.replace(
                "cell top() {",
                "cell top() { let added = rect(\"met2\", x0=-100., y0=-50., x1=200., y1=500.);",
            ),
            base.replace("x1=10.", "x1=50."),
            base.replace("std::range(4)", "std::range(2)"),
            base.replace("cell branch()", "cell renamed()")
                .replace("inst(branch()", "inst(renamed()"),
            base.to_owned(),
        ];
        let mut compiler = argonc::incremental::IncrementalCompiler::new();
        let mut previous: Option<CompileOutputState> = None;
        let mut previous_layers = None;
        for program in programs {
            compiler.set_source_text(source.clone(), program);
            let output = compiler
                .compile_cell(&config, &["top".into()], vec![])
                .unwrap_valid();
            let root = ScopeAddress {
                cell: output.top,
                scope: output.cells[&output.top].root,
            };
            let mut actual = ProcessScopeState::default();
            let old_state = previous.as_ref().map(|old| old.state.as_ref());
            let prepared = prepare(
                &output,
                root,
                &mut actual,
                old_state,
                previous.as_ref(),
                previous_layers.as_ref(),
            );
            assert_eq!(
                prepared.all_scopes_visible,
                actual.state.values().all(|scope| scope.visible)
            );
            let mut expected = ProcessScopeState::default();
            Reference::process_scope_reference(&output, root, &mut expected, None, old_state);
            if previous.is_some() {
                // The compact address changes when the top cell is rebuilt,
                // while the original name path remains `top`.
                expected.state.set_visible(root, false);
            }
            assert_equivalent(&actual, &expected);
            // A visibility override must survive every compatible path change.
            actual.state.set_visible(root, false);
            previous_layers = Some(actual.layers.clone());
            previous = Some(CompileOutputState {
                selected_scope: root,
                output: Arc::new(output),
                state: Arc::new(actual.state),
                all_scopes_visible: false,
            });
        }
    }

    #[test]
    #[ignore = "SRAM hierarchy timing benchmark; build argon-sram-routing-wasm for wasm32-unknown-unknown, then run in release with --nocapture"]
    fn bench_sram_parent_rectangle_preparation() {
        let workspace =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/sram");
        let source = workspace.join("lib.ar");
        let original = std::fs::read_to_string(&source).unwrap();
        let config = argonc::WorkspaceConfig::new(&source)
            .with_tech(Some(workspace.join("../../pdks/sky130/sky130.tech.toml")))
            .with_wasm_modules([(
                "routing".to_owned(),
                workspace.join(
                    "../../target/wasm32-unknown-unknown/release/argon_sram_routing_wasm.wasm",
                ),
            )]);
        let cell = vec!["benchmark_parent".to_owned()];
        let mut compiler = argonc::incremental::IncrementalCompiler::new();
        compiler.set_source_text(
            source.clone(),
            format!(
                "{original}\ncell benchmark_parent() {{ let s = inst(example_no_control_logic(), x=0., y=0.); }}\n"
            ),
        );
        let before = compiler.compile_cell(&config, &cell, vec![]).unwrap_valid();
        let old_root = ScopeAddress {
            cell: before.top,
            scope: before.cells[&before.top].root,
        };
        let mut old_state = ProcessScopeState::default();
        let start = web_time::Instant::now();
        prepare(&before, old_root, &mut old_state, None, None, None);
        let cold = start.elapsed();
        let old_layers = old_state.layers.clone();
        let previous = CompileOutputState {
            output: Arc::new(before),
            selected_scope: old_root,
            state: Arc::new(old_state.state),
            all_scopes_visible: true,
        };
        compiler.set_source_text(
            source,
            format!(
                "{original}\ncell benchmark_parent() {{ let s = inst(example_no_control_logic(), x=0., y=0.); let r = std::layout::rect(\"met1.drawing\", x0=0., y0=0., x1=100., y1=100.)!; }}\n"
            ),
        );
        let after = compiler.compile_cell(&config, &cell, vec![]).unwrap_valid();
        let root = ScopeAddress {
            cell: after.top,
            scope: after.cells[&after.top].root,
        };
        let mut next_state = ProcessScopeState::default();
        let start = web_time::Instant::now();
        prepare(
            &after,
            root,
            &mut next_state,
            Some(previous.state.as_ref()),
            Some(&previous),
            Some(&old_layers),
        );
        let warm = start.elapsed();
        assert_eq!(next_state.state.len(), previous.state.len());
        eprintln!(
            "sram_hierarchy_prepare,cold_ms={},edited_ms={},scopes={}",
            cold.as_millis(),
            warm.as_millis(),
            next_state.state.len()
        );

        // Model a batch editor deletion after many rectangles were drawn in
        // the parent. The SRAM below the parent must remain cached.
        let rectangles = (0..100)
            .map(|i| {
                let x0 = i * 100;
                format!(
                    "let r{i} = std::layout::rect(\"met1.drawing\", x0={x0}., y0=0., x1={}., y1=100.)!;",
                    x0 + 100
                )
            })
            .collect::<Vec<_>>()
            .join(" ");
        let with_rectangles = format!(
            "{original}\ncell benchmark_parent() {{ let s = inst(example_no_control_logic(), x=0., y=0.); {rectangles} }}\n"
        );
        let without_rectangles = format!(
            "{original}\ncell benchmark_parent() {{ let s = inst(example_no_control_logic(), x=0., y=0.); }}\n"
        );
        let mut previous = CompileOutputState {
            output: Arc::new(after),
            selected_scope: root,
            state: Arc::new(next_state.state),
            all_scopes_visible: true,
        };
        let mut previous_layers = next_state.layers;
        for (label, text) in [
            ("many_rectangles", with_rectangles),
            ("bulk_delete", without_rectangles),
        ] {
            compiler.set_source_text(workspace.join("lib.ar"), text);
            let started = web_time::Instant::now();
            let output = compiler.compile_cell(&config, &cell, vec![]).unwrap_valid();
            let compile_ms = started.elapsed().as_millis();
            let next_root = ScopeAddress {
                cell: output.top,
                scope: output.cells[&output.top].root,
            };
            let mut next = ProcessScopeState::default();
            let started = web_time::Instant::now();
            prepare(
                &output,
                next_root,
                &mut next,
                Some(previous.state.as_ref()),
                Some(&previous),
                Some(&previous_layers),
            );
            eprintln!(
                "sram_parent_{label},compile_ms={compile_ms},hierarchy_ms={},scopes={}",
                started.elapsed().as_millis(),
                next.state.len()
            );
            let started = web_time::Instant::now();
            let all_visible = next.state.values().all(|scope| scope.visible);
            eprintln!(
                "sram_parent_{label},visibility_scan_ms={},all_visible={all_visible}",
                started.elapsed().as_millis(),
            );
            previous_layers = next.layers;
            previous = CompileOutputState {
                output: Arc::new(output),
                selected_scope: next_root,
                state: Arc::new(next.state),
                all_scopes_visible: true,
            };
        }
    }

    #[test]
    #[ignore = "SRAM subcell hierarchy timing benchmark; build argon-sram-routing-wasm for wasm32-unknown-unknown, then run in release with --nocapture"]
    fn bench_sram_subcell_preparation() {
        let workspace =
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/sram");
        let source = workspace.join("lib.ar");
        let macro_source = workspace.join("bitcells/macros.ar");
        let original = std::fs::read_to_string(&macro_source).unwrap();
        let edited = original.replacen(
            "cell sp_cell() {",
            "cell sp_cell() { let debug_rect = std::layout::rect(\"met1.drawing\", x0=-100., y0=-100., x1=-90., y1=-90.);",
            1,
        );
        let config = argonc::WorkspaceConfig::new(&source)
            .with_tech(Some(workspace.join("../../pdks/sky130/sky130.tech.toml")))
            .with_wasm_modules([(
                "routing".to_owned(),
                workspace.join(
                    "../../target/wasm32-unknown-unknown/release/argon_sram_routing_wasm.wasm",
                ),
            )]);
        let cell = vec!["example_no_control_logic".to_owned()];
        let mut compiler = argonc::incremental::IncrementalCompiler::new();
        let before = compiler.compile_cell(&config, &cell, vec![]).unwrap_valid();
        let old_root = ScopeAddress {
            cell: before.top,
            scope: before.cells[&before.top].root,
        };
        let mut old_state = ProcessScopeState::default();
        prepare(&before, old_root, &mut old_state, None, None, None);
        let old_layers = old_state.layers.clone();
        let previous = CompileOutputState {
            output: Arc::new(before),
            selected_scope: old_root,
            state: Arc::new(old_state.state),
            all_scopes_visible: true,
        };
        compiler.set_source_text(macro_source, edited);
        let after = compiler.compile_cell(&config, &cell, vec![]).unwrap_valid();
        let root = ScopeAddress {
            cell: after.top,
            scope: after.cells[&after.top].root,
        };
        let mut next = ProcessScopeState::default();
        let started = web_time::Instant::now();
        prepare(
            &after,
            root,
            &mut next,
            Some(previous.state.as_ref()),
            Some(&previous),
            Some(&old_layers),
        );
        let warm = started.elapsed();
        assert_eq!(next.state.len(), previous.state.len());
        eprintln!(
            "sram_subcell_hierarchy_prepare,edited_ms={},scopes={}",
            warm.as_millis(),
            next.state.len()
        );
    }

    fn union(left: Option<ScopeBounds>, right: Option<ScopeBounds>) -> Option<ScopeBounds> {
        match (left, right) {
            (Some(left), Some(right)) => Some(left.union(right)),
            (left, right) => left.or(right),
        }
    }

    fn add(bounds: Option<ScopeBounds>, rect: Option<Rect<f64>>) -> Option<ScopeBounds> {
        union(bounds, rect.map(|rect| ScopeBounds::of(&rect)))
    }

    struct Reference;
    impl Reference {
        fn process_scope_reference(
            solved_cell: &CompiledData,
            scope: ScopeAddress,
            state: &mut ProcessScopeState,
            parent: Option<ScopeAddress>,
            old_scope_state: Option<&ScopeStates>,
        ) {
            let cell = &solved_cell.cells[&scope.cell];
            let scope_info = &cell.scopes[&scope.scope];
            let scope_name = cell.scope_name_shared(scope.scope);
            let mut bbox = None;
            for (obj, _) in &scope_info.emit {
                let value = &solved_cell.cells[&scope.cell].objects[obj];
                match value {
                    SolvedValue::Rect(rect) => {
                        bbox = add(bbox, Some(rect.to_float()));
                        if let Some(layer) = &rect.layer {
                            mark_layer_used(state, layer);
                        }
                    }
                    SolvedValue::Polygon(polygon) => {
                        bbox = add(bbox, polygon.bbox());
                        let layer = SharedString::from(&polygon.layer);
                        if let Some(layer_info) = state.layers.get_mut(&layer) {
                            layer_info.used = true;
                        } else {
                            let mut s = DefaultHasher::new();
                            layer.hash(&mut s);
                            let hash = s.finish() as usize;
                            let color =
                                rgb([0xff0000, 0x0ff000, 0x00ff00, 0x000ff0, 0x0000ff][hash % 5]);
                            state.layers.insert(
                                layer.clone(),
                                LayerState {
                                    name: layer,
                                    color,
                                    fill: ShapeFill::Stippling,
                                    border_color: color,
                                    visible: true,
                                    used: true,
                                    z: state.layers.len(),
                                },
                            );
                        }
                    }
                    SolvedValue::Path(path) => {
                        bbox = add(bbox, path.bbox());
                        let layer = SharedString::from(&path.layer);
                        if let Some(layer_info) = state.layers.get_mut(&layer) {
                            layer_info.used = true;
                        } else {
                            let mut s = DefaultHasher::new();
                            layer.hash(&mut s);
                            let hash = s.finish() as usize;
                            let color =
                                rgb([0xff0000, 0x0ff000, 0x00ff00, 0x000ff0, 0x0000ff][hash % 5]);
                            state.layers.insert(
                                layer.clone(),
                                LayerState {
                                    name: layer,
                                    color,
                                    fill: ShapeFill::Stippling,
                                    border_color: color,
                                    visible: true,
                                    used: true,
                                    z: state.layers.len(),
                                },
                            );
                        }
                    }
                    SolvedValue::Instance(inst) => {
                        let inst_address = ScopeAddress {
                            scope: solved_cell.cells[&inst.cell].root,
                            cell: inst.cell,
                        };
                        Self::process_scope_reference(
                            solved_cell,
                            inst_address,
                            state,
                            Some(scope),
                            old_scope_state,
                        );
                        let child = state
                            .state
                            .get(&inst_address)
                            .unwrap()
                            .bbox
                            .as_deref()
                            .copied();
                        bbox = union(
                            bbox,
                            child.map(|rect| {
                                let mut inst_mat = TransformationMatrix::identity();
                                if inst.reflect {
                                    inst_mat = inst_mat.reflect_vert()
                                }
                                inst_mat = inst_mat.rotate(inst.angle);
                                let p0p = ifmatvec(inst_mat, (rect.x0, rect.y0));
                                let p1p = ifmatvec(inst_mat, (rect.x1, rect.y1));
                                ScopeBounds {
                                    x0: p0p.0.min(p1p.0) + inst.x,
                                    y0: p0p.1.min(p1p.1) + inst.y,
                                    x1: p0p.0.max(p1p.0) + inst.x,
                                    y1: p0p.1.max(p1p.1) + inst.y,
                                }
                            }),
                        );
                    }
                    SolvedValue::Dimension(dim) => {
                        bbox = add(bbox, bbox_dim_union(None, dim));
                    }
                    SolvedValue::Text(t) => {
                        bbox = add(bbox, bbox_text_union(None, t));
                        mark_layer_used(state, &t.layer);
                    }
                }
            }

            for child in &scope_info.children {
                let scope_address = ScopeAddress {
                    scope: *child,
                    cell: scope.cell,
                };
                Self::process_scope_reference(
                    solved_cell,
                    scope_address,
                    state,
                    Some(scope),
                    old_scope_state,
                );
                let child = state
                    .state
                    .get(&scope_address)
                    .unwrap()
                    .bbox
                    .as_deref()
                    .copied();
                bbox = union(bbox, child);
            }

            let visible = old_scope_state
                .and_then(|state| state.get(&scope).map(|scope| scope.visible))
                .unwrap_or(true);
            state.state.insert(
                scope,
                ScopeState {
                    visible,
                    parent,
                    prepared: Arc::new(PreparedScope {
                        name: scope_name,
                        bbox: bbox.map(Arc::new),
                        children: smallvec::SmallVec::new(),
                    }),
                },
            );
        }
    }
}
