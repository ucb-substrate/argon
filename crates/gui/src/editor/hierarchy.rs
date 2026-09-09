//! Prepare hierarchy metadata from the cell DAG without expanding its geometry
//! once per instance. Instance placement affects bounds, but not child metadata.
use std::collections::{HashMap, HashSet};

use argonc::compile::{SolvedValue, bbox_dim_union, bbox_text_union, bbox_union, ifmatvec};
use geometry::transform::TransformationMatrix;

use super::{
    CompiledData, IndexMap, ProcessScopeState, Rect, ScopeAddress, ScopePath, ScopeState,
    mark_layer_used,
};

struct PreparedScope {
    bbox: Option<Rect<f64>>,
    children: Vec<ScopeAddress>,
}

fn prepare_geometry(
    output: &CompiledData,
    address: ScopeAddress,
    state: &mut ProcessScopeState,
    scopes: &mut HashMap<ScopeAddress, PreparedScope>,
) {
    if scopes.contains_key(&address) {
        return;
    }
    let cell = &output.cells[&address.cell];
    let scope = &cell.scopes[&address.scope];
    let mut bbox = None;
    let mut children = Vec::new();
    for (object, _) in &scope.emit {
        match &cell.objects[object] {
            SolvedValue::Rect(rect) => {
                bbox = bbox_union(bbox, Some(rect.to_float()));
                if let Some(layer) = &rect.layer {
                    mark_layer_used(state, layer);
                }
            }
            SolvedValue::Polygon(polygon) => {
                bbox = bbox_union(bbox, polygon.bbox());
                mark_layer_used(state, &polygon.layer);
            }
            SolvedValue::Path(path) => {
                bbox = bbox_union(bbox, path.bbox());
                mark_layer_used(state, &path.layer);
            }
            SolvedValue::Text(text) => {
                bbox = bbox_text_union(bbox, text);
                mark_layer_used(state, &text.layer);
            }
            SolvedValue::Dimension(dimension) => {
                bbox = bbox_dim_union(bbox, dimension);
            }
            SolvedValue::Instance(instance) => {
                let child = ScopeAddress {
                    cell: instance.cell,
                    scope: output.cells[&instance.cell].root,
                };
                prepare_geometry(output, child, state, scopes);
                children.push(child);
                // Reuse local bounds, but include every placement in the
                // parent's bounds in the original emission order.
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
        prepare_geometry(output, child, state, scopes);
        children.push(child);
        bbox = bbox_union(bbox, scopes[&child].bbox.clone());
    }
    scopes.insert(address, PreparedScope { bbox, children });
}

fn prepare_paths(
    output: &CompiledData,
    address: ScopeAddress,
    parent: Option<ScopeAddress>,
    parent_path: &ScopePath,
    scopes: &HashMap<ScopeAddress, PreparedScope>,
    visited: &mut HashSet<(ScopeAddress, ScopePath)>,
    state: &mut ProcessScopeState,
    old: Option<&IndexMap<ScopePath, ScopeState>>,
) {
    let name = &output.cells[&address.cell].scopes[&address.scope].name;
    let mut path = parent_path.clone();
    path.push(name.clone());
    if !visited.insert((address, path.clone())) {
        return;
    }
    // Walk siblings in reverse: the old expanded traversal kept the *last*
    // path for each address and the last scope with a colliding path. First
    // insertion here preserves that choice without replaying duplicate trees.
    state
        .scope_paths
        .entry(address)
        .or_insert_with(|| path.clone());
    state
        .state
        .entry(path.clone())
        .or_insert_with(|| ScopeState {
            name: name.clone(),
            address,
            parent,
            visible: old
                .and_then(|old| old.get(&path))
                .is_none_or(|scope| scope.visible),
            bbox: scopes[&address].bbox.clone(),
        });
    for child in scopes[&address].children.iter().rev() {
        prepare_paths(
            output,
            *child,
            Some(address),
            &path,
            scopes,
            visited,
            state,
            old,
        );
    }
}

pub(super) fn prepare(
    output: &CompiledData,
    root: ScopeAddress,
    state: &mut ProcessScopeState,
    old: Option<&IndexMap<ScopePath, ScopeState>>,
) {
    let mut scopes = HashMap::new();
    prepare_geometry(output, root, state, &mut scopes);
    prepare_paths(
        output,
        root,
        None,
        &Vec::new(),
        &scopes,
        &mut HashSet::new(),
        state,
        old,
    );
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::editor::LayerState;
    use crate::editor::canvas::ShapeFill;
    use gpui::{SharedString, rgb};
    use std::hash::{DefaultHasher, Hash, Hasher};

    pub(in crate::editor) fn verify_prepared(
        output: &CompiledData,
        actual: &super::super::PreparedCompileOutput,
    ) {
        let mut expected = ProcessScopeState {
            layers: actual.layers.clone(),
            ..Default::default()
        };
        for layer in expected.layers.values_mut() {
            layer.used = false;
        }
        let root = ScopeAddress {
            cell: output.top,
            scope: output.cells[&output.top].root,
        };
        Reference::process_scope_reference(output, root, &mut expected, None, None);
        let actual = ProcessScopeState {
            layers: actual.layers.clone(),
            state: actual.state.clone(),
            scope_paths: actual.scope_paths.clone(),
        };
        assert_equivalent(&actual, &expected);
    }

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
        prepare(&output, root, &mut actual, None);
        assert_equivalent(&actual, &expected);
        let mut old = expected.state;
        for (index, scope) in old.values_mut().enumerate() {
            scope.visible = index % 2 == 0;
        }
        let mut expected = ProcessScopeState::default();
        Reference::process_scope_reference(&output, root, &mut expected, None, Some(&old));
        let mut actual = ProcessScopeState::default();
        prepare(&output, root, &mut actual, Some(&old));
        assert_equivalent(&actual, &expected);
    }

    struct Reference;
    impl Reference {
        fn process_scope_reference(
            solved_cell: &CompiledData,
            scope: ScopeAddress,
            state: &mut ProcessScopeState,
            parent: Option<ScopeAddress>,
            old_scope_state: Option<&IndexMap<ScopePath, ScopeState>>,
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
