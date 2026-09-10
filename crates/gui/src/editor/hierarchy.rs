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
struct PreparedScope {
    name: String,
    bbox: Option<Rect<f64>>,
    children: Vec<ScopeAddress>,
    layers: indexmap::IndexSet<String>,
}

#[derive(Debug, Default)]
pub(super) struct PreparedHierarchy {
    scopes: HashMap<ScopeAddress, Arc<PreparedScope>>,
}

impl PreparedHierarchy {
    /// Root identity includes immutable cell data and every dependency used to
    /// compute its bounds. It is safe to reuse a renderer index only if all of
    /// those inputs survived the edit, not merely if its numeric ID survived.
    pub(super) fn same_cell(
        &self,
        previous: &Self,
        output: &CompiledData,
        cell: argonc::compile::CellId,
    ) -> bool {
        let Some(cell_info) = output.cells.get(&cell) else {
            return false;
        };
        let address = ScopeAddress {
            cell,
            scope: cell_info.root,
        };
        self.scopes
            .get(&address)
            .zip(previous.scopes.get(&address))
            .is_some_and(|(new, old)| Arc::ptr_eq(new, old))
    }
}

fn prepare_geometry(
    output: &CompiledData,
    address: ScopeAddress,
    scopes: &mut HashMap<ScopeAddress, Arc<PreparedScope>>,
    previous: Option<&CompileOutputState>,
) {
    if scopes.contains_key(&address) {
        return;
    }
    let cell = &output.cells[&address.cell];
    if let Some(old) = previous
        && let Some(cached) = old.hierarchy.scopes.get(&address).filter(|_| {
            old.output
                .cells
                .get(&address.cell)
                .is_some_and(|old| Arc::ptr_eq(old, cell))
        })
    {
        // An immutable parent's bounds can still change if a referenced
        // child changed. Validate dependencies before reusing its bounds.
        for child in &cached.children {
            prepare_geometry(output, *child, scopes, previous);
        }
        if cached.children.iter().all(|child| {
            old.hierarchy
                .scopes
                .get(child)
                .is_some_and(|old| Arc::ptr_eq(old, &scopes[child]))
        }) {
            scopes.insert(address, cached.clone());
            return;
        }
    }
    let scope = &cell.scopes[&address.scope];
    let mut bbox = None;
    let mut children = Vec::new();
    let mut layers = indexmap::IndexSet::new();
    for (object, _) in &scope.emit {
        match &cell.objects[object] {
            SolvedValue::Rect(rect) => {
                bbox = bbox_union(bbox, Some(rect.to_float()));
                if let Some(layer) = &rect.layer {
                    layers.insert(layer.clone());
                }
            }
            SolvedValue::Polygon(polygon) => {
                bbox = bbox_union(bbox, polygon.bbox());
                layers.insert(polygon.layer.clone());
            }
            SolvedValue::Path(path) => {
                bbox = bbox_union(bbox, path.bbox());
                layers.insert(path.layer.clone());
            }
            SolvedValue::Text(text) => {
                bbox = bbox_text_union(bbox, text);
                layers.insert(text.layer.clone());
            }
            SolvedValue::Dimension(dimension) => {
                bbox = bbox_dim_union(bbox, dimension);
            }
            SolvedValue::Instance(instance) => {
                let child = ScopeAddress {
                    cell: instance.cell,
                    scope: output.cells[&instance.cell].root,
                };
                prepare_geometry(output, child, scopes, previous);
                children.push(child);
                layers.extend(scopes[&child].layers.iter().cloned());
                bbox = bbox_union(
                    bbox,
                    scopes[&child].bbox.as_ref().map(|rect| {
                        let mut matrix = TransformationMatrix::identity();
                        if instance.reflect {
                            matrix = matrix.reflect_vert();
                        }
                        matrix = matrix.rotate(instance.angle);
                        let p0 = ifmatvec(matrix, (rect.x0, rect.y0));
                        let p1 = ifmatvec(matrix, (rect.x1, rect.y1));
                        Rect {
                            layer: None,
                            x0: p0.0.min(p1.0) + instance.x,
                            y0: p0.1.min(p1.1) + instance.y,
                            x1: p0.0.max(p1.0) + instance.x,
                            y1: p0.1.max(p1.1) + instance.y,
                            id: instance.id,
                            construction: true,
                            span: rect.span.clone(),
                        }
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
        prepare_geometry(output, child, scopes, previous);
        children.push(child);
        layers.extend(scopes[&child].layers.iter().cloned());
        bbox = bbox_union(bbox, scopes[&child].bbox.clone());
    }
    scopes.insert(
        address,
        Arc::new(PreparedScope {
            name: scope.name.clone(),
            bbox,
            children,
            layers,
        }),
    );
}

fn prepare_paths(
    address: ScopeAddress,
    parent: Option<ScopeAddress>,
    parent_path: &ScopePath,
    scopes: &HashMap<ScopeAddress, Arc<PreparedScope>>,
    visited: &mut HashSet<(ScopeAddress, ScopePath)>,
    state: &mut ProcessScopeState,
    old: Option<&imbl::HashMap<ScopePath, ScopeState>>,
) {
    let scope = &scopes[&address];
    let mut path = parent_path.clone();
    path.push(scope.name.clone());
    if !visited.insert((address, path.clone())) {
        return;
    }
    // Reverse siblings preserve the expanded walk's last-writer semantics,
    // including colliding names and multiple paths to a shared scope.
    state
        .scope_paths
        .entry(address)
        .or_insert_with(|| path.clone());
    state
        .state
        .entry(path.clone())
        .or_insert_with(|| ScopeState {
            name: scope.name.clone(),
            address,
            parent,
            visible: old
                .and_then(|old| old.get(&path))
                .is_none_or(|scope| scope.visible),
            bbox: scope.bbox.clone(),
        });
    for child in scope.children.iter().rev() {
        prepare_paths(*child, Some(address), &path, scopes, visited, state, old);
    }
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
        let Some(a) = old.hierarchy.scopes.get(&before) else {
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
            (!Arc::ptr_eq(&old.hierarchy.scopes[before], &scopes[after])).then_some(*before)
        })
        .collect();
    state.state = old.state.as_ref().clone();
    state.scope_paths = old.scope_paths.as_ref().clone();
    for (path, scope) in old.state.iter() {
        let address = remap[&scope.address];
        let parent = scope.parent.map(|parent| remap[&parent]);
        if changed.contains(&scope.address) || address != scope.address || parent != scope.parent {
            state.state.insert(
                path.clone(),
                ScopeState {
                    address,
                    parent,
                    bbox: scopes[&address].bbox.clone(),
                    ..scope.clone()
                },
            );
        }
    }
    for (before, after) in &remap {
        if before != after {
            state.scope_paths.remove(before);
        }
    }
    for (before, after) in &remap {
        if before != after {
            state
                .scope_paths
                .insert(*after, old.scope_paths[before].clone());
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
    prepare_geometry(output, root, &mut scopes, previous);
    for layer in &scopes[&root].layers {
        mark_layer_used(state, layer);
    }
    if !previous.is_some_and(|previous| reuse_paths(root, &scopes, state, previous)) {
        prepare_paths(
            root,
            None,
            &Vec::new(),
            &scopes,
            &mut HashSet::new(),
            state,
            old,
        );
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
        assert_eq!(actual.scope_paths, expected.scope_paths);
        assert_eq!(actual.state.len(), expected.state.len());
        for (path, expected) in &expected.state {
            let actual = &actual.state[path];
            assert_eq!(actual.name, expected.name);
            assert_eq!(actual.address, expected.address);
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
            let hierarchy = prepare(&output, root, &mut actual, old_state, previous.as_ref());
            let mut expected = ProcessScopeState::default();
            Reference::process_scope_reference(&output, root, &mut expected, None, old_state);
            assert_equivalent(&actual, &expected);
            // A visibility override must survive every compatible path change.
            let hidden = actual
                .state
                .keys()
                .find(|path| path.len() == 2)
                .cloned()
                .unwrap();
            actual.state.get_mut(&hidden).unwrap().visible = false;
            previous = Some(CompileOutputState {
                selected_scope: actual.scope_paths[&root].clone(),
                output: Arc::new(output),
                state: Arc::new(actual.state),
                scope_paths: Arc::new(actual.scope_paths),
                hierarchy: Arc::new(hierarchy),
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
            let scope_info = &solved_cell.cells[&scope.cell].scopes[&scope.scope];
            let mut scope_path = if let Some(parent) = &parent {
                state.scope_paths[parent].clone()
            } else {
                vec![]
            };
            scope_path.push(scope_info.name.clone());
            state.scope_paths.insert(scope, scope_path.clone());
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
                            state.state[&state.scope_paths[&inst_address]]
                                .bbox
                                .as_ref()
                                .map(|rect| {
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
                bbox = bbox_union(
                    bbox,
                    state.state[&state.scope_paths[&scope_address]].bbox.clone(),
                );
            }

            let visible = old_scope_state
                .and_then(|state| state.get(&scope_path).map(|scope| scope.visible))
                .unwrap_or(true);
            state.state.insert(
                scope_path,
                ScopeState {
                    name: scope_info.name.clone(),
                    address: scope,
                    visible,
                    bbox,
                    parent,
                },
            );
        }
    }
}
