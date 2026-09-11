//! Prepare hierarchy metadata once per immutable cell, sharing unchanged paths
//! and bounds between GUI snapshots.
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
};

use argonc::compile::{SolvedValue, bbox_dim_union, bbox_text_union, bbox_union, ifmatvec};
use geometry::transform::TransformationMatrix;

use super::{
    CompileOutputState, CompiledData, ProcessScopeState, Rect, ScopeAddress, ScopePath, ScopeState,
    mark_layer_used,
};

#[derive(Debug)]
pub struct PreparedScope {
    pub name: Arc<str>,
    /// Shared because long chains of function/control-flow scopes commonly
    /// have exactly the same bounds as their only child.
    pub bbox: Option<Arc<Rect<f64>>>,
    // Most generated execution scopes have zero or one child. Keep those
    // inline instead of performing a heap allocation per scope.
    pub children: smallvec::SmallVec<[ScopeAddress; 2]>,
}

#[derive(Debug, Default)]
pub(super) struct PreparedHierarchy {
    scopes: HashMap<ScopeAddress, Arc<PreparedScope>>,
}

impl PreparedHierarchy {
    /// Resolve a selection from the preceding snapshot by its displayed name
    /// path. Addresses include a content-derived cell ID and can therefore
    /// change after an otherwise non-structural source edit.
    pub(super) fn remap_path(
        &self,
        root: ScopeAddress,
        old_state: &imbl::HashMap<ScopePath, ScopeState>,
        old_selected: ScopeAddress,
    ) -> Option<ScopeAddress> {
        let mut names = Vec::new();
        let mut current = Some(old_selected);
        while let Some(address) = current {
            let scope = old_state.get(&address)?;
            names.push(scope.name.as_ref());
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

fn prepare_geometry(
    output: &CompiledData,
    address: ScopeAddress,
    scopes: &mut HashMap<ScopeAddress, Arc<PreparedScope>>,
    layers: &mut indexmap::IndexSet<String>,
    previous: Option<&CompileOutputState>,
) {
    if scopes.contains_key(&address) {
        return;
    }
    let cell = &output.cells[&address.cell];
    let source_scope = &cell.scopes[&address.scope];
    if let Some(old) = previous
        && let Some(cached) = old
            .state
            .get(&address)
            .map(|scope| &scope.prepared)
            .filter(|_| {
                old.output
                    .cells
                    .get(&address.cell)
                    .is_some_and(|old| Arc::ptr_eq(old, cell))
            })
    {
        // An immutable parent's bounds can still change if a referenced
        // child changed. Walk dependencies in emit order both to validate them
        // and to preserve the renderer's first-use layer ordering.
        for (object, _) in &source_scope.emit {
            match &cell.objects[object] {
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
                SolvedValue::Instance(instance) => prepare_geometry(
                    output,
                    ScopeAddress {
                        cell: instance.cell,
                        scope: output.cells[&instance.cell].root,
                    },
                    scopes,
                    layers,
                    previous,
                ),
                SolvedValue::Dimension(_) => {}
            }
        }
        for child in &source_scope.children {
            prepare_geometry(
                output,
                ScopeAddress {
                    cell: address.cell,
                    scope: *child,
                },
                scopes,
                layers,
                previous,
            );
        }
        if cached.children.iter().all(|child| {
            old.state
                .get(child)
                .is_some_and(|old| Arc::ptr_eq(&old.prepared, &scopes[child]))
        }) {
            scopes.insert(address, cached.clone());
            return;
        }
    }
    let scope = source_scope;
    let mut bbox = None;
    let mut children = smallvec::SmallVec::new();
    for (object, _) in &scope.emit {
        match &cell.objects[object] {
            SolvedValue::Rect(rect) => {
                bbox = shared_bbox_union(bbox, Some(Arc::new(rect.to_float())));
                if let Some(layer) = &rect.layer {
                    layers.insert(layer.clone());
                }
            }
            SolvedValue::Polygon(polygon) => {
                bbox = shared_bbox_union(bbox, polygon.bbox().map(Arc::new));
                layers.insert(polygon.layer.clone());
            }
            SolvedValue::Path(path) => {
                bbox = shared_bbox_union(bbox, path.bbox().map(Arc::new));
                layers.insert(path.layer.clone());
            }
            SolvedValue::Text(text) => {
                bbox = shared_bbox_union(bbox, bbox_text_union(None, text).map(Arc::new));
                layers.insert(text.layer.clone());
            }
            SolvedValue::Dimension(dimension) => {
                bbox = shared_bbox_union(bbox, bbox_dim_union(None, dimension).map(Arc::new));
            }
            SolvedValue::Instance(instance) => {
                let child = ScopeAddress {
                    cell: instance.cell,
                    scope: output.cells[&instance.cell].root,
                };
                prepare_geometry(output, child, scopes, layers, previous);
                children.push(child);
                bbox = shared_bbox_union(
                    bbox,
                    scopes[&child].bbox.as_ref().map(|rect| {
                        let mut matrix = TransformationMatrix::identity();
                        if instance.reflect {
                            matrix = matrix.reflect_vert();
                        }
                        matrix = matrix.rotate(instance.angle);
                        let p0 = ifmatvec(matrix, (rect.x0, rect.y0));
                        let p1 = ifmatvec(matrix, (rect.x1, rect.y1));
                        Arc::new(Rect {
                            layer: None,
                            x0: p0.0.min(p1.0) + instance.x,
                            y0: p0.1.min(p1.1) + instance.y,
                            x1: p0.0.max(p1.0) + instance.x,
                            y1: p0.1.max(p1.1) + instance.y,
                            id: instance.id,
                            construction: true,
                            span: rect.span.clone(),
                        })
                    }),
                );
            }
        }
    }
    for child in &scope.children {
        let child = ScopeAddress {
            cell: address.cell,
            scope: *child,
        };
        prepare_geometry(output, child, scopes, layers, previous);
        children.push(child);
        bbox = shared_bbox_union(bbox, scopes[&child].bbox.clone());
    }
    scopes.insert(
        address,
        Arc::new(PreparedScope {
            name: cell.scope_name_shared(address.scope),
            bbox,
            children,
        }),
    );
}

fn shared_bbox_union(
    left: Option<Arc<Rect<f64>>>,
    right: Option<Arc<Rect<f64>>>,
) -> Option<Arc<Rect<f64>>> {
    match (left, right) {
        (None, right) => right,
        (left, None) => left,
        (Some(left), Some(right)) => Some(Arc::new(
            bbox_union(Some((*left).clone()), Some((*right).clone())).unwrap(),
        )),
    }
}

fn prepare_paths(
    address: ScopeAddress,
    parent: Option<ScopeAddress>,
    path: &mut Vec<String>,
    hidden_paths: &HashSet<Vec<String>>,
    scopes: &HashMap<ScopeAddress, Arc<PreparedScope>>,
    state: &mut ProcessScopeState,
) {
    let scope = &scopes[&address];
    if state.state.contains_key(&address) {
        return;
    }
    path.push(scope.name.to_string());
    state.state.insert(
        address,
        ScopeState {
            parent,
            visible: !hidden_paths.contains(path),
            prepared: Arc::clone(scope),
        },
    );
    for child in scope.children.iter().rev() {
        prepare_paths(*child, Some(address), path, hidden_paths, scopes, state);
    }
    path.pop();
}

/// Populate the overwhelmingly common all-visible hierarchy without building
/// and allocating a displayed-name path for every execution scope. Name paths
/// are needed only to remap the small set of user-hidden scopes across edits.
fn prepare_visible_paths(
    address: ScopeAddress,
    parent: Option<ScopeAddress>,
    scopes: &HashMap<ScopeAddress, Arc<PreparedScope>>,
    state: &mut ProcessScopeState,
) {
    if state.state.contains_key(&address) {
        return;
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
    for child in scope.children.iter().rev() {
        prepare_visible_paths(*child, Some(address), scopes, state);
    }
}

fn hidden_paths(old: Option<&imbl::HashMap<ScopePath, ScopeState>>) -> HashSet<Vec<String>> {
    old.into_iter()
        .flat_map(|state| {
            state
                .iter()
                .filter(|(_, scope)| !scope.visible)
                .map(|(address, _)| {
                    let mut path = Vec::new();
                    let mut current = Some(*address);
                    while let Some(address) = current {
                        let scope = &state[&address];
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
    scopes: &HashMap<ScopeAddress, Arc<PreparedScope>>,
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
        let Some(a) = old.state.get(&before).map(|scope| &scope.prepared) else {
            return false;
        };
        let b = &scopes[&after];
        if a.name != b.name || a.children.len() != b.children.len() {
            return false;
        }
        remap.insert(before, after);
        pending.extend(a.children.iter().copied().zip(b.children.iter().copied()));
    }
    let changed: HashSet<_> = remap
        .iter()
        .filter_map(|(before, after)| {
            (!Arc::ptr_eq(&old.state[before].prepared, &scopes[after])).then_some(*before)
        })
        .collect();
    state.state = old.state.as_ref().clone();
    for (before, after) in &remap {
        let scope = &old.state[before];
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
                    prepared: Arc::clone(&scopes[&address]),
                    ..scope.clone()
                },
            );
        }
    }
    true
}

pub(super) fn prepare(
    output: &CompiledData,
    root: ScopeAddress,
    state: &mut ProcessScopeState,
    old: Option<&imbl::HashMap<ScopePath, ScopeState>>,
    previous: Option<&CompileOutputState>,
) -> PreparedHierarchy {
    let mut scopes = HashMap::new();
    let mut used_layers = indexmap::IndexSet::new();
    prepare_geometry(output, root, &mut scopes, &mut used_layers, previous);
    for layer in &used_layers {
        mark_layer_used(state, layer);
    }
    if !previous.is_some_and(|previous| reuse_paths(root, &scopes, state, previous)) {
        let hidden_paths = hidden_paths(old);
        if hidden_paths.is_empty() {
            prepare_visible_paths(root, None, &scopes, state);
        } else {
            prepare_paths(root, None, &mut Vec::new(), &hidden_paths, &scopes, state);
        }
    }
    PreparedHierarchy { scopes }
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
        for (path, expected) in &expected.state {
            let actual = &actual.state[path];
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
        prepare(&output, root, &mut actual, None, None);
        assert_equivalent(&actual, &expected);
        let mut old = expected.state;
        for (index, (_, scope)) in old.iter_mut().enumerate() {
            scope.visible = index % 2 == 0;
        }
        let mut expected = ProcessScopeState::default();
        Reference::process_scope_reference(&output, root, &mut expected, None, Some(&old));
        let mut actual = ProcessScopeState::default();
        prepare(&output, root, &mut actual, Some(&old), None);
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
            prepare(&output, root, &mut actual, old_state, previous.as_ref());
            let mut expected = ProcessScopeState::default();
            Reference::process_scope_reference(&output, root, &mut expected, None, old_state);
            if previous.is_some() {
                // The compact address changes when the top cell is rebuilt,
                // while the original name path remains `top`.
                expected.state.get_mut(&root).unwrap().visible = false;
            }
            assert_equivalent(&actual, &expected);
            // A visibility override must survive every compatible path change.
            actual.state.get_mut(&root).unwrap().visible = false;
            previous = Some(CompileOutputState {
                selected_scope: root,
                output: Arc::new(output),
                state: Arc::new(actual.state),
            });
        }
    }

    struct Reference;
    impl Reference {
        fn process_scope_reference(
            solved_cell: &CompiledData,
            scope: ScopeAddress,
            state: &mut ProcessScopeState,
            parent: Option<ScopeAddress>,
            old_scope_state: Option<&imbl::HashMap<ScopePath, ScopeState>>,
        ) {
            let cell = &solved_cell.cells[&scope.cell];
            let scope_info = &cell.scopes[&scope.scope];
            let scope_name = cell.scope_name_shared(scope.scope);
            let mut bbox = None;
            for (obj, _) in &scope_info.emit {
                let value = &solved_cell.cells[&scope.cell].objects[obj];
                match value {
                    SolvedValue::Rect(rect) => {
                        bbox = bbox_union(bbox, Some(rect.to_float()));
                        if let Some(layer) = &rect.layer {
                            mark_layer_used(state, layer);
                        }
                    }
                    SolvedValue::Polygon(polygon) => {
                        bbox = bbox_union(bbox, polygon.bbox());
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
                        bbox = bbox_union(bbox, path.bbox());
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
                        bbox = bbox_union(
                            bbox,
                            state.state[&inst_address].bbox.as_ref().map(|rect| {
                                let mut inst_mat = TransformationMatrix::identity();
                                if inst.reflect {
                                    inst_mat = inst_mat.reflect_vert()
                                }
                                inst_mat = inst_mat.rotate(inst.angle);
                                let p0p = ifmatvec(inst_mat, (rect.x0, rect.y0));
                                let p1p = ifmatvec(inst_mat, (rect.x1, rect.y1));
                                Rect {
                                    layer: None,
                                    x0: p0p.0.min(p1p.0) + inst.x,
                                    y0: p0p.1.min(p1p.1) + inst.y,
                                    x1: p0p.0.max(p1p.0) + inst.x,
                                    y1: p0p.1.max(p1p.1) + inst.y,
                                    id: inst.id,
                                    construction: true,
                                    span: rect.span.clone(),
                                }
                            }),
                        );
                    }
                    SolvedValue::Dimension(dim) => {
                        bbox = bbox_dim_union(bbox, dim);
                    }
                    SolvedValue::Text(t) => {
                        bbox = bbox_text_union(bbox, t);
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
                bbox = bbox_union(bbox, state.state[&scope_address].bbox.as_deref().cloned());
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
