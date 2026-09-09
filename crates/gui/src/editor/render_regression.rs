//! Opt-in rendering checks against a local GDS, without checking a large SRAM
//! into the repository. Run in release mode; see bench/README.md.
use super::*;

fn test_canvas(cx: &mut gpui::TestAppContext) -> Entity<LayoutCanvas> {
    let state = cx.new(|cx| {
        let solved_cell = cx.new(|_| None);
        let layers = cx.new(|_| editor::Layers {
            layers: IndexMap::new(),
            selected_layer: None,
        });
        EditorState {
            hierarchy_depth: usize::MAX,
            dark_mode: true,
            icon_size: None,
            font_size: None,
            workspace_path: None,
            workspace_modified: false,
            compilation_activities: Default::default(),
            snapshot_preparations: Default::default(),
            latest_snapshot_preparation: None,
            rendering: false,
            compilation_revision: None,
            compilation_error: None,
            fatal_error: None,
            message: None,
            connection_error: None,
            subscriptions: vec![
                cx.observe(&solved_cell, |_, _, cx| cx.notify()),
                cx.observe(&layers, |_, _, cx| cx.notify()),
            ],
            solved_cell,
            hide_external_geometry: false,
            layers,
            lang_server_client: crate::rpc::SyncLangServerClient::for_render_test(cx.to_async()),
            tool: cx.new(|_| ToolState::default()),
        }
    });
    cx.new(|cx| {
        let focus = cx.focus_handle();
        let input_focus = cx.focus_handle();
        let input = cx.new(|cx| {
            TextInput::new_dimension_input(cx, input_focus.clone(), focus.clone(), &state)
        });
        LayoutCanvas::new(cx, &state, focus, input_focus, input)
    })
}

#[gpui::test]
fn direct_views_can_request_a_new_density_decision(cx: &mut gpui::TestAppContext) {
    let canvas = test_canvas(cx);
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("lib.ar");
    std::fs::write(
        &source,
        "cell top() { let r = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.); }",
    )
    .unwrap();
    let config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tech/basic.tech.toml"),
    ));
    let (solved, layers) = prepare(&source, &config);
    canvas.update(cx, |canvas, cx| {
        let state = canvas.state.read(cx);
        state
            .solved_cell
            .clone()
            .update(cx, |cell, _| *cell = Some(solved));
        canvas
            .state
            .read(cx)
            .layers
            .clone()
            .update(cx, |state, _| state.layers = (*layers).clone());
        canvas.update_raster_presentation(cx);
        canvas.screen_bounds = Bounds::new(Point::default(), Size::new(px(64.), px(64.)));
        assert!(!canvas.raster_prefetch_enabled);
        canvas.request_visible_raster_decision(cx);
        assert!(
            canvas.raster_decision_refinement.is_some(),
            "a direct view must still reclassify after a zoom"
        );
    });
    cx.run_until_parked();
    canvas.update(cx, |canvas, _| {
        assert!(canvas.raster_decision_refinement.is_none());
        assert!(!canvas.raster_cache_enabled);
    });
    canvas.update(cx, |canvas, cx| {
        let state = canvas.state.read(cx);
        let solved = state.solved_cell.read(cx).clone().unwrap();
        let layers = Arc::new(state.layers.read(cx).layers.clone());
        let viewport = ViewportTransform {
            size: canvas.screen_bounds.size,
            screen_size: canvas.screen_bounds.size,
            scale: canvas.scale,
            offset: canvas.offset,
        };
        let cache = build_navigation_raster(input(
            solved,
            layers,
            viewport,
            canvas.raster_spatial_index.clone(),
        ))
        .unwrap();
        let image = cache.image.clone();
        let center = RasterTileIndex { x: 0, y: 0 };
        canvas.raster_tiles = Some(LayoutRasterTileSet {
            tiles: HashMap::from_iter([(center, cache)]),
            anchor_offset: canvas.offset,
            tile_size: viewport.size,
            screen_viewport: viewport.size,
            scale: canvas.scale,
            content_revision: canvas.raster_content_revision,
            center,
        });
        canvas.fit_to_screen(cx);
        assert_eq!(
            canvas.raster_tiles.as_ref().unwrap().tiles[&center].image,
            image,
            "fit must retain the last image until the new view is ready"
        );
    });
    cx.run_until_parked();
    canvas.update(cx, |canvas, cx| {
        assert!(!canvas.raster_cache_enabled);
        assert!(
            !canvas.state.read(cx).rendering,
            "fitting a sparse view must finish rendering"
        );
    });
}

#[gpui::test]
fn tools_and_pan_gestures_preserve_the_retained_presentation(cx: &mut gpui::TestAppContext) {
    let canvas = test_canvas(cx);
    canvas.update(cx, |canvas, _| {
        canvas.raster_cache_enabled = true;
        canvas.raster_prefetch_enabled = true;
        canvas.raster_content_revision = 7;
        canvas.raster_stale_tiles_displayable = true;
        canvas
            .last_presented_raster
            .set(Some(RasterDisplayTransform {
                scale: 1.,
                offset: Point::default(),
            }));
    });
    for tool in [
        ToolState::DrawRect(DrawRectToolState::default()),
        ToolState::DrawDim(DrawDimToolState::default()),
        ToolState::Select(SelectToolState { selected_obj: None }),
    ] {
        canvas.update(cx, |canvas, cx| {
            canvas.state.read(cx).tool.clone().update(cx, |state, cx| {
                *state = tool;
                cx.notify();
            });
            canvas.begin_navigation();
        });
        cx.run_until_parked();
        canvas.update(cx, |canvas, _| {
            assert!(canvas.raster_cache_enabled);
            assert!(canvas.raster_stale_tiles_displayable);
            assert_eq!(canvas.raster_content_revision, 7);
            assert!(canvas.last_presented_raster.get().is_some());
        });
    }
}

fn prepare(
    source: &std::path::Path,
    config: &argonc::WorkspaceConfig,
) -> (CompileOutputState, Arc<IndexMap<SharedString, LayerState>>) {
    let ast = argonc::parse::parse_workspace_with_config(config).ast();
    let output = argonc::compile::compile(
        &ast,
        argonc::compile::CompileInput {
            cell: &["top"],
            args: vec![],
        },
        config,
    )
    .unwrap_valid();
    let snapshot = editor::prepare_compilation_snapshot(
        analyzer::rpc::CompilationSnapshot {
            revision: 1,
            output: compile::CompileOutput::Valid(output),
        },
        editor::CompilationPreparationContext {
            layers: IndexMap::new(),
            selected_scope: None,
            scope_state: None,
            previous: None,
        },
    );
    let prepared = snapshot.prepared_output.expect("prepared GDS layout");
    let output = snapshot.output.unwrap_valid();
    assert!(source.is_file());
    (
        CompileOutputState {
            output: Arc::new(output),
            selected_scope: prepared.selected_scope,
            state: Arc::new(prepared.state),
            scope_paths: Arc::new(prepared.scope_paths),
            hierarchy: prepared.hierarchy,
        },
        Arc::new(prepared.layers),
    )
}

fn input(
    solved: CompileOutputState,
    layers: Arc<IndexMap<SharedString, LayerState>>,
    viewport: ViewportTransform,
    spatial_index: Arc<RasterSpatialIndex>,
) -> NavigationRasterInput {
    NavigationRasterInput {
        solved_cell: solved,
        layers,
        hierarchy_depth: usize::MAX,
        hide_external_geometry: false,
        viewport,
        text_color: rgb(0xffffff),
        include_text: true,
        content_revision: 1,
        content_revision_signal: Arc::new(AtomicU64::new(1)),
        scale_signal: Arc::new(AtomicU64::new(viewport.scale.to_bits() as u64)),
        cell_raster_tiles: Arc::new(Mutex::new(CellRasterTileCache::default())),
        spatial_index,
        use_spatial_index: true,
        cancel_if_generation_changes: None,
    }
}

#[test]
fn dense_instance_arrays_collapse_without_filling_large_empty_gaps() {
    let node = |pitch: f64| {
        RasterBvhNode::build(
            (0..64)
                .map(|emit_index| {
                    let x = (emit_index % 8) as f64 * pitch;
                    let y = (emit_index / 8) as f64 * pitch;
                    RasterBvhItem {
                        emit_index,
                        bounds: RasterBvhBounds {
                            min_x: x,
                            min_y: y,
                            max_x: x + 1.,
                            max_y: y + 1.,
                        },
                        lod_layer: None,
                        instance_layers: Some(vec!["met1".into(), "met2".into()].into()),
                    }
                })
                .collect(),
        )
        .unwrap()
    };
    let query = RasterBvhBounds {
        min_x: -1.,
        min_y: -1.,
        max_x: 100.,
        max_y: 100.,
    };
    let mut emits = Vec::new();
    let mut occupancies = Vec::new();
    node(1.).query_lod(query, 2., &mut emits, &mut occupancies);
    assert!(emits.is_empty());
    assert_eq!(
        occupancies.len(),
        1,
        "64 unresolved cells should be one aggregate"
    );
    assert_eq!(occupancies[0].layers.len(), 2);
    emits.clear();
    occupancies.clear();
    node(10.).query_lod(query, 2., &mut emits, &mut occupancies);
    assert_eq!(
        emits.len(),
        64,
        "separated cells must retain their empty space"
    );
    assert!(occupancies.is_empty());
}

#[test]
fn raster_invalidation_distinguishes_edits_from_visibility_and_ui_changes() {
    assert_eq!(
        raster_presentation_change(false, false, false, true),
        RasterPresentationChange::None
    );
    assert_eq!(
        raster_presentation_change(false, true, true, true),
        RasterPresentationChange::Geometry
    );
    assert_eq!(
        raster_presentation_change(false, true, true, false),
        RasterPresentationChange::Presentation
    );
    assert_eq!(
        raster_presentation_change(false, false, true, true),
        RasterPresentationChange::Presentation
    );
    assert_eq!(
        raster_presentation_change(true, true, true, true),
        RasterPresentationChange::Presentation
    );
}

#[test]
fn targeted_dimension_names_match_full_scope_resolution() {
    let source =
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/seq_cell/lib.ar");
    let config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tech/basic.tech.toml"),
    ));
    let (solved, _) = prepare(&source, &config);
    let mut checked = 0;
    for (&cell_id, cell) in &solved.output.cells {
        for &scope in cell.scopes.keys() {
            let reference = solved.output.reachable_objs(cell_id, scope);
            for &object in cell.objects.keys() {
                assert_eq!(
                    solved
                        .output
                        .reachable_obj_name(cell_id, scope, object)
                        .as_ref(),
                    reference.get(&object)
                );
                checked += 1;
            }
        }
    }
    assert!(checked > 10);
}

#[test]
fn instance_lod_preserves_hidden_descendants_and_hierarchy_cutoffs() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("lib.ar");
    std::fs::write(
        &source,
        r#"
cell leaf() { let shape = rect("met1", x0=0., y0=0., x1=1., y1=1.); }
cell branch() { let child = inst(leaf(), x=0., y=0.); }
cell top() { let child = inst(branch(), x=0., y=0.); }
"#,
    )
    .unwrap();
    let config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tech/basic.tech.toml"),
    ));
    let (mut solved, layers) = prepare(&source, &config);
    let viewport = ViewportTransform {
        size: Size::new(px(64.), px(64.)),
        screen_size: Size::new(px(64.), px(64.)),
        scale: 2.,
        offset: Point::new(px(20.), px(20.)),
    };
    for hidden in [false, true] {
        if hidden {
            let scopes = Arc::make_mut(&mut solved.state);
            let (_, scope) = scopes
                .iter_mut()
                .find(|(_, scope)| {
                    solved.output.cells[&scope.address.cell]
                        .objects
                        .values()
                        .any(|value| matches!(value, SolvedValue::Rect(_)))
                })
                .unwrap();
            scope.visible = false;
        }
        let hierarchy_depth = if hidden { usize::MAX } else { 2 };
        let spatial_index = Arc::new(RasterSpatialIndex::for_presentation(
            hierarchy_depth,
            Some(&solved.state),
        ));
        assert!(!spatial_index.collapse_instances);
        let mut indexed = input(solved.clone(), layers.clone(), viewport, spatial_index);
        indexed.hierarchy_depth = hierarchy_depth;
        let mut linear = indexed.clone();
        linear.use_spatial_index = false;
        let actual = build_navigation_raster(indexed).unwrap();
        let reference = build_navigation_raster(linear).unwrap();
        assert_eq!(actual.image.as_bytes(0), reference.image.as_bytes(0));
        assert!(
            !actual.scope_labels.is_empty(),
            "collapsed descendants must still display their bounding boxes"
        );
    }
}

#[gpui::test]
#[ignore = "requires ARGON_RENDER_GDS pointing to a local SRAM GDS"]
fn local_sram_render_benchmark(cx: &mut gpui::TestAppContext) {
    let gds = std::path::PathBuf::from(
        std::env::var_os("ARGON_RENDER_GDS").expect("set ARGON_RENDER_GDS"),
    );
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("lib.ar");
    std::fs::write(
        &source,
        "cell top() { let memory = inst(sram(), x=0., y=0.); }\n",
    )
    .unwrap();
    let config = argonc::WorkspaceConfig::new(&source)
        .with_tech(Some(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../pdks/sky130/sky130.tech.toml"),
        ))
        .with_gds_imports([("sram".to_owned(), gds)]);
    let start = Instant::now();
    let (mut solved, layers) = prepare(&source, &config);
    if std::env::var_os("ARGON_RENDER_OPEN_SRAM").is_some() {
        let instance = solved.output.cells[&solved.output.top]
            .objects
            .values()
            .find_map(SolvedValue::get_instance)
            .unwrap();
        solved.selected_scope = solved.scope_paths[&ScopeAddress {
            cell: instance.cell,
            scope: solved.output.cells[&instance.cell].root,
        }]
            .clone();
    }
    eprintln!(
        "SRAM preparation: {:?}; {} unique cells",
        start.elapsed(),
        solved.output.cells.len()
    );
    let bbox = solved.state[&solved.selected_scope].bbox.as_ref().unwrap();
    let size = Size::new(px(1200.), px(800.));
    let fit_scale = (1100. / (bbox.x1 - bbox.x0)).min(700. / (bbox.y1 - bbox.y0)) as f32;
    let center = (
        (bbox.x0 + bbox.x1) as f32 / 2.,
        (bbox.y0 + bbox.y1) as f32 / 2.,
    );
    let spatial_index = Arc::new(RasterSpatialIndex::default());
    let canvas = test_canvas(cx);
    canvas.update(cx, |canvas, cx| {
        canvas
            .state
            .read(cx)
            .solved_cell
            .clone()
            .update(cx, |value, _| *value = Some(solved.clone()));
        canvas
            .state
            .read(cx)
            .layers
            .clone()
            .update(cx, |value, _| value.layers = (*layers).clone());
        canvas.update_raster_presentation(cx);
        canvas.raster_content_revision = 1;
        canvas
            .raster_content_revision_signal
            .store(1, Ordering::Release);
        canvas.raster_spatial_index = spatial_index.clone();
        canvas.pending_init = false;
    });
    let cx = cx.add_empty_window();
    for zoom in [1., 2., 4., 8., 16., 32., 64., 128.] {
        let scale = fit_scale * zoom;
        let viewport = ViewportTransform {
            size,
            screen_size: size,
            scale,
            offset: Point::new(px(600. - center.0 * scale), px(400. + center.1 * scale)),
        };
        let start = Instant::now();
        let decision = visible_geometry_render_decision_indexed(
            &solved,
            &spatial_index,
            &layers,
            usize::MAX,
            false,
            viewport,
            true,
        )
        .unwrap();
        let cold_decision = start.elapsed();
        let start = Instant::now();
        for _ in 0..10 {
            std::hint::black_box(
                visible_geometry_render_decision_indexed(
                    &solved,
                    &spatial_index,
                    &layers,
                    usize::MAX,
                    false,
                    viewport,
                    false,
                )
                .unwrap(),
            );
        }
        let warm_decision = start.elapsed() / 10;
        let render_input = input(
            solved.clone(),
            layers.clone(),
            viewport,
            spatial_index.clone(),
        );
        let start = Instant::now();
        let first = build_navigation_raster(render_input.clone()).unwrap();
        let render = start.elapsed();
        assert!(
            first
                .image
                .as_bytes(0)
                .unwrap()
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] != 0),
            "zoom {zoom}: blank image"
        );
        let repeat_start = Instant::now();
        let repeat = build_navigation_raster(render_input.clone()).unwrap();
        let warm_render = repeat_start.elapsed();
        assert_eq!(
            first.image.as_bytes(0),
            repeat.image.as_bytes(0),
            "zoom {zoom}: idle repaint changed pixels"
        );
        // An integral pan must preserve all overlapping interior pixels. This
        // catches tile seams and camera-dependent LOD/stipple phase changes.
        let mut pan_input = render_input;
        pan_input.viewport.offset.x += px(32.);
        let pan = build_navigation_raster(pan_input).unwrap();
        let image_size = first.image.size(0);
        let width = image_size.width.0 as usize;
        let height = image_size.height.0 as usize;
        let before = first.image.as_bytes(0).unwrap();
        let after = pan.image.as_bytes(0).unwrap();
        let mut changed = 0;
        for y in 8..height - 8 {
            for x in 40..width - 8 {
                if before[(y * width + x - 32) * 4..(y * width + x - 32 + 1) * 4]
                    != after[(y * width + x) * 4..(y * width + x + 1) * 4]
                {
                    changed += 1;
                }
            }
        }
        eprintln!(
            "zoom {zoom:>2}: cold decision {cold_decision:?}, warm decision {warm_decision:?}, first raster {render:?}, warm raster {warm_render:?}, raster enabled {}, pan changed {changed} interior pixels",
            decision.use_raster
        );
        assert_eq!(changed, 0, "zoom {zoom}: panning changed interior geometry");

        let tile_index = RasterTileIndex { x: 0, y: 0 };
        let display = RasterDisplayTransform {
            scale,
            offset: viewport.offset,
        };
        canvas.update(cx, |canvas, _| {
            canvas.scale = scale;
            canvas.offset = viewport.offset;
            canvas.screen_bounds = Bounds::new(Point::default(), size);
            canvas.raster_cache_enabled = decision.use_raster;
            // The benchmark supplies completed tiles itself, so no background
            // prefetch competes with the measured UI paint.
            canvas.raster_prefetch_enabled = false;
            canvas.raster_tiles = Some(LayoutRasterTileSet {
                tiles: HashMap::from_iter([(tile_index, first)]),
                anchor_offset: viewport.offset,
                tile_size: size,
                screen_viewport: size,
                scale,
                content_revision: 1,
                center: tile_index,
            });
            canvas.raster_display = Some(display);
            canvas.last_presented_raster.set(None);
        });
        let mut paint_times = Vec::new();
        for tool in [
            ToolState::default(),
            ToolState::DrawRect(DrawRectToolState::default()),
            ToolState::DrawDim(DrawDimToolState::default()),
            ToolState::default(),
        ] {
            canvas.update(cx, |canvas, cx| {
                canvas.state.read(cx).tool.clone().update(cx, |value, cx| {
                    *value = tool;
                    cx.notify();
                });
            });
            let start = Instant::now();
            cx.draw(
                Point::default(),
                size.map(gpui::AvailableSpace::Definite),
                |_, _| CanvasElement {
                    inner: canvas.clone(),
                },
            );
            paint_times.push(start.elapsed());
            canvas.update(cx, |canvas, _| {
                assert_eq!(
                    canvas.last_presented_raster.get(),
                    decision.use_raster.then_some(display),
                    "zoom {zoom}: tool activation changed the renderer or camera"
                );
            });
        }
        eprintln!("zoom {zoom:>2}: GPUI canvas paints (select/rect/dim/select) {paint_times:?}");
        if decision.use_raster {
            canvas.update(cx, |canvas, _| {
                canvas.begin_navigation();
                canvas.offset.x += px(32.);
            });
            cx.draw(
                Point::default(),
                size.map(gpui::AvailableSpace::Definite),
                |_, _| CanvasElement {
                    inner: canvas.clone(),
                },
            );
            canvas.update(cx, |canvas, _| {
                assert_eq!(
                    canvas.last_presented_raster.get(),
                    Some(display),
                    "incomplete pan coverage must retain the last frame"
                );
                let target = RasterTileTarget {
                    anchor_offset: canvas.offset,
                    tile_size: size,
                    screen_viewport: size,
                    scale,
                    content_revision: 1,
                    center: tile_index,
                    generation: canvas.raster_generation,
                };
                canvas.raster_tile_target = Some(target);
                canvas.install_navigation_tile(target, tile_index, pan);
            });
            cx.draw(
                Point::default(),
                size.map(gpui::AvailableSpace::Definite),
                |_, _| CanvasElement {
                    inner: canvas.clone(),
                },
            );
            canvas.update(cx, |canvas, _| {
                assert_eq!(
                    canvas.last_presented_raster.get(),
                    Some(RasterDisplayTransform {
                        scale,
                        offset: canvas.offset
                    }),
                    "completed pan tiles must advance to the requested camera"
                );
            });
        }
    }
}

fn load_canvas(
    canvas: &Entity<LayoutCanvas>,
    solved: CompileOutputState,
    layers: Arc<IndexMap<SharedString, LayerState>>,
    cx: &mut gpui::TestAppContext,
) {
    let state = canvas.read_with(cx, |canvas, _| canvas.state.clone());
    state.update(cx, |state, cx| {
        state.solved_cell.update(cx, |cell, _| *cell = Some(solved));
        state
            .layers
            .update(cx, |state, _| state.layers = (*layers).clone());
        cx.notify();
    });
}

#[gpui::test]
fn cold_pan_defers_expansion_and_keeps_the_rendering_indicator(cx: &mut gpui::TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("lib.ar");
    std::fs::write(
        &source,
        r#"
cell leaf() { let shape = rect("met1", x0=0., y0=0., x1=10., y1=10.); }
cell top() {
    for row in std::range(100) {
        for col in std::range(100) {
            let child = inst(leaf(), x=(col as Float)*12., y=(row as Float)*12.);
        }
    }
}
"#,
    )
    .unwrap();
    let config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tech/basic.tech.toml"),
    ));
    let (solved, layers) = prepare(&source, &config);
    let canvas = test_canvas(cx);
    load_canvas(&canvas, solved, layers, cx);
    // Reproduce a pan out of a previously sparse view, before any index or
    // navigation tile has been built. It must not materialize the hierarchy.
    canvas.update(cx, |canvas, _| {
        canvas.pending_init = false;
        canvas.scale = 1.;
        canvas.offset = Point::new(px(0.), px(1200.));
        canvas.raster_decision_refinement = None;
        canvas.raster_cache_enabled = false;
        canvas.raster_prefetch_enabled = false;
    });
    let cx = cx.add_empty_window();
    let size = Size::new(px(1200.), px(1200.));
    cx.draw(
        Point::default(),
        size.map(gpui::AvailableSpace::Definite),
        |_, _| CanvasElement {
            inner: canvas.clone(),
        },
    );
    canvas.update(cx, |canvas, cx| {
        assert!(
            canvas.rects.is_empty(),
            "cold pan must not submit a partial flattened layout"
        );
        assert!(canvas.raster_worker_active);
        assert!(
            canvas.state.read(cx).rendering,
            "pending raster must show the spinner"
        );
    });
    cx.run_until_parked();
    cx.draw(
        Point::default(),
        size.map(gpui::AvailableSpace::Definite),
        |_, _| CanvasElement {
            inner: canvas.clone(),
        },
    );
    canvas.update(cx, |canvas, cx| {
        assert!(canvas.last_presented_raster.get().is_some());
        assert!(
            !canvas.state.read(cx).rendering,
            "finished rendering must stop the spinner"
        );
        assert!(canvas.rects.len() <= UI_GEOMETRY_WORK_LIMIT);
    });
}

#[gpui::test]
#[ignore = "requires ARGON_RENDER_ARTIFACT pointing to a compiled local project"]
fn local_project_pan_lifecycle(cx: &mut gpui::TestAppContext) {
    let path = std::env::var_os("ARGON_RENDER_ARTIFACT").expect("set ARGON_RENDER_ARTIFACT");
    let start = Instant::now();
    let output = argonc::artifact::read(path).unwrap();
    let snapshot = editor::prepare_compilation_snapshot(
        analyzer::rpc::CompilationSnapshot {
            revision: 1,
            output,
        },
        editor::CompilationPreparationContext {
            layers: IndexMap::new(),
            selected_scope: None,
            scope_state: None,
            previous: None,
        },
    );
    let prepared = snapshot.prepared_output.unwrap();
    let solved = CompileOutputState {
        output: Arc::new(snapshot.output.unwrap_valid()),
        selected_scope: prepared.selected_scope,
        state: Arc::new(prepared.state),
        scope_paths: Arc::new(prepared.scope_paths),
        hierarchy: prepared.hierarchy,
    };
    eprintln!(
        "Project preparation: {:?}, {} unique cells",
        start.elapsed(),
        solved.output.cells.len()
    );
    let canvas = test_canvas(cx);
    load_canvas(&canvas, solved, Arc::new(prepared.layers), cx);
    let full_editor = std::env::var_os("ARGON_RENDER_EDITOR")
        .is_some()
        .then(|| test_editor(&canvas, cx));
    let cx = cx.add_empty_window();
    let size = Size::new(px(1200.), px(800.));
    let draw = |cx: &mut gpui::VisualTestContext| {
        let start = Instant::now();
        if let Some(editor) = &full_editor {
            editor.update(cx, |_, cx| cx.notify());
        }
        cx.draw(
            Point::default(),
            size.map(gpui::AvailableSpace::Definite),
            |_, _| match &full_editor {
                Some(editor) => div().size_full().child(editor.clone()).into_any_element(),
                None => CanvasElement {
                    inner: canvas.clone(),
                }
                .into_any_element(),
            },
        );
        start.elapsed()
    };
    // Paint after every scheduled task, including density decisions and each
    // tile arrival. Sampling only the settled frame misses renderer oscillation.
    let settle = |cx: &mut gpui::VisualTestContext| {
        let original_bounds = canvas.read_with(cx, |canvas, _| canvas.screen_bounds);
        let mut previous = None;
        let mut changes = 0;
        for frame in 0..1024 {
            draw(cx);
            let (signature, pending) = canvas.update(cx, |canvas, _| {
                assert_eq!(
                    canvas.screen_bounds, original_bounds,
                    "render completion must not resize the viewport"
                );
                let display = canvas.last_presented_raster.get();
                let mut images = Vec::new();
                if let (Some(display), Some(tiles)) = (display, canvas.raster_tiles.as_ref()) {
                    images = visible_raster_tiles(
                        tiles,
                        canvas.screen_bounds,
                        display.scale,
                        display.offset,
                    )
                    .into_iter()
                    .map(|cache| Arc::as_ptr(&cache.image) as usize)
                    .collect();
                    images.sort_unstable();
                }
                (
                    (canvas.raster_cache_enabled, display, images),
                    canvas.raster_worker_active
                        || canvas.raster_decision_refinement.is_some()
                        || canvas.raster_overview_requested_revision.is_some(),
                )
            });
            if previous.as_ref() != Some(&signature) {
                eprintln!(
                    "  frame {frame}: raster {}, camera {:?}, {} visible images",
                    signature.0,
                    signature.1,
                    signature.2.len()
                );
                previous = Some(signature);
                changes += 1;
            }
            if !pending {
                eprintln!("  settled after {frame} tasks, {changes} presentation changes");
                return;
            }
            assert!(
                cx.dispatcher.tick(false),
                "rendering indicator stranded without work"
            );
        }
        panic!("rendering failed to settle after 1024 interleaved paints");
    };
    let first_paint = draw(cx);
    eprintln!(
        "Canvas bounds: {:?}; full editor {}",
        canvas.read_with(cx, |canvas, _| canvas.screen_bounds),
        full_editor.is_some()
    );
    canvas.update(cx, |canvas, cx| {
        assert!(canvas.state.read(cx).rendering);
        assert!(canvas.rects.len() <= UI_GEOMETRY_WORK_LIMIT);
    });
    let start = Instant::now();
    settle(cx);
    eprintln!(
        "Cold UI paint {first_paint:?}; initial background field {:?}; ready UI paint {:?}",
        start.elapsed(),
        draw(cx)
    );
    canvas.update(cx, |canvas, cx| {
        assert!(!canvas.raster_worker_active);
        assert!(!canvas.state.read(cx).rendering);
    });
    canvas.update(cx, |canvas, cx| {
        let state = canvas.state.read(cx);
        eprintln!(
            "LOD: instance collapse {}, hierarchy {}, hidden scopes {}",
            canvas.raster_spatial_index.collapse_instances,
            state.hierarchy_depth,
            state
                .solved_cell
                .read(cx)
                .as_ref()
                .unwrap()
                .state
                .values()
                .filter(|scope| !scope.visible)
                .count()
        );
    });
    let mut pan_times = Vec::new();
    for zoom in [1., 4., 16.] {
        if zoom != 1. {
            canvas.update(cx, |canvas, cx| {
                canvas.zoom_about(canvas.screen_bounds.center(), canvas.scale * 4., cx);
            });
            draw(cx);
            settle(cx);
            draw(cx);
        }
        let render_input = canvas.update(cx, |canvas, cx| {
            let target = canvas.raster_tile_target.unwrap();
            canvas
                .navigation_raster_input(cx, target, target.center)
                .unwrap()
        });
        eprintln!("Zoom {zoom}: begin full viewport CPU raster");
        let start = Instant::now();
        let raster = build_navigation_raster(render_input).unwrap();
        assert!(
            raster
                .image
                .as_bytes(0)
                .unwrap()
                .as_chunks::<4>()
                .0
                .iter()
                .any(|pixel| pixel[3] != 0)
        );
        eprintln!(
            "Zoom {zoom}: full viewport CPU raster {:?}",
            start.elapsed()
        );
        // Do not pump background tasks between input and paint. A fast burst
        // can outrun the tile field, then reverse before its replacement lands.
        for (mouse_pan, burst) in [
            (false, [32., 900., 3600., -4400., -132.]),
            (true, [-32., -900., -3600., 4400., 132.]),
        ] {
            let mut position = Point::new(px(600.), px(400.));
            if mouse_pan {
                cx.update(|window, cx| {
                    canvas.update(cx, |canvas, cx| {
                        canvas.on_middle_mouse_down(
                            &MouseDownEvent {
                                button: MouseButton::Middle,
                                position,
                                ..Default::default()
                            },
                            window,
                            cx,
                        );
                    })
                });
            }
            for delta in burst {
                let start = Instant::now();
                if mouse_pan {
                    position.x += px(delta);
                    cx.update(|window, cx| {
                        canvas.update(cx, |canvas, cx| {
                            canvas.on_mouse_move(
                                &MouseMoveEvent {
                                    position,
                                    pressed_button: Some(MouseButton::Middle),
                                    ..Default::default()
                                },
                                window,
                                cx,
                            );
                        })
                    });
                } else {
                    canvas.update(cx, |canvas, cx| {
                        canvas.pan_view(Point::new(px(delta), px(0.)), cx)
                    });
                }
                let input_time = start.elapsed();
                let paint_time = draw(cx);
                pan_times.push(input_time + paint_time);
                canvas.update(cx, |canvas, _| {
                    assert!(canvas.rects.len() <= UI_GEOMETRY_WORK_LIMIT);
                    if let Some(display) = canvas.last_presented_raster.get() {
                        assert!(raster_tiles_cover_bounds(
                            canvas.raster_tiles.as_ref().unwrap(),
                            canvas.screen_bounds,
                            canvas.screen_bounds,
                            display.scale,
                            display.offset
                        ));
                    }
                });
            }
            if mouse_pan {
                cx.update(|window, cx| {
                    canvas.update(cx, |canvas, cx| {
                        canvas.on_middle_mouse_up(
                            &MouseUpEvent {
                                button: MouseButton::Middle,
                                position,
                                ..Default::default()
                            },
                            window,
                            cx,
                        );
                    })
                });
            }
            let start = Instant::now();
            settle(cx);
            let catch_up = start.elapsed();
            draw(cx);
            canvas.update(cx, |canvas, cx| {
                assert!(!canvas.state.read(cx).rendering);
                if canvas.raster_cache_enabled {
                    assert!(raster_display_matches_camera(
                        canvas.last_presented_raster.get().unwrap(),
                        canvas.scale,
                        canvas.offset
                    ));
                }
            });
            eprintln!("Zoom {zoom}: background catch-up {catch_up:?}");
        }
    }
    // Unlike the out-and-back bursts above, stop outside the retained field
    // and let its replacement arrive before returning. At 16x this still
    // shows dense SRAM geometry and exercises a real stopped-pan handoff.
    for delta in [3600., -3600.] {
        let previous = canvas.read_with(cx, |canvas, _| canvas.last_presented_raster.get());
        let start = Instant::now();
        canvas.update(cx, |canvas, cx| {
            canvas.pan_view(Point::new(px(delta), px(0.)), cx)
        });
        draw(cx);
        pan_times.push(start.elapsed());
        canvas.update(cx, |canvas, _| {
            assert_eq!(
                canvas.last_presented_raster.get(),
                previous,
                "an uncovered pan must retain its complete frame"
            )
        });
        let start = Instant::now();
        settle(cx);
        eprintln!(
            "Stopped pan {delta}: background catch-up {:?}",
            start.elapsed()
        );
        canvas.update(cx, |canvas, cx| {
            assert!(!canvas.state.read(cx).rendering);
            assert!(raster_display_matches_camera(
                canvas.last_presented_raster.get().unwrap(),
                canvas.scale,
                canvas.offset
            ));
        });
    }
    pan_times.sort();
    eprintln!(
        "Pan input + paint: median {:?}, max {:?} ({} frames)",
        pan_times[pan_times.len() / 2],
        pan_times.last().unwrap(),
        pan_times.len()
    );
    assert!(
        pan_times.last().unwrap().as_millis() < 100,
        "pan must never expand the SRAM synchronously"
    );
}

#[gpui::test]
fn recentering_keeps_tiles_used_by_the_last_presented_camera(cx: &mut gpui::TestAppContext) {
    let canvas = test_canvas(cx);
    canvas.update(cx, |canvas, _| {
        let size = Size::new(px(100.), px(100.));
        canvas.screen_bounds = Bounds::new(Point::default(), size);
        canvas.scale = 1.;
        canvas.offset = Point::default();
        canvas.prepare_navigation_tile_target();
        let target = canvas.raster_tile_target.unwrap();
        let cache = LayoutRasterCache {
            image: Arc::new(RenderImage::new(vec![image::Frame::new(
                image::RgbaImage::new(1, 1),
            )])),
            scale_safe_lod: true,
            texts: Arc::from([]),
            scope_labels: Arc::from([]),
            viewport: size,
            screen_viewport: size,
            scale: 1.,
            offset: Point::default(),
            content_revision: canvas.raster_content_revision,
        };
        for index in navigation_tile_order(target.center) {
            let mut tile = cache.clone();
            tile.offset = raster_tile_offset(target.anchor_offset, target.tile_size, index);
            canvas.install_navigation_tile(target, index, tile);
        }
        let display = RasterDisplayTransform {
            scale: 1.,
            offset: Point::default(),
        };
        canvas.last_presented_raster.set(Some(display));
        // New ring [-6, -2] overlaps the old ring but excludes the displayed
        // tile (0, 0). Pruning it immediately used to destroy every fallback.
        canvas.offset.x = px(400.);
        canvas.prepare_navigation_tile_target();
        let tiles = canvas.raster_tiles.as_ref().unwrap();
        assert!(raster_tiles_cover_bounds(
            tiles,
            canvas.screen_bounds,
            canvas.screen_bounds,
            display.scale,
            display.offset
        ));
        assert!(!raster_tiles_cover_bounds(
            tiles,
            canvas.screen_bounds,
            canvas.screen_bounds,
            canvas.scale,
            canvas.offset
        ));
        assert!(
            tiles.tiles.len() <= 29,
            "retain only the target ring and displayed coverage"
        );
        let painted =
            visible_raster_tiles(tiles, canvas.screen_bounds, display.scale, display.offset);
        assert!(
            painted.iter().any(|tile| tile.offset == Point::default()),
            "paint must include the displayed tile outside the new prefetch ring"
        );
    });
}

#[test]
fn interleaved_layers_coalesce_before_visiting_sram_leaf_shapes() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("lib.ar");
    let mut program = String::from("cell top() {\n");
    for y in 0..32 {
        for x in 0..32 {
            for layer in ["met1", "met2"] {
                program.push_str(&format!(
                    "let r{x}_{y}_{layer} = rect(\"{layer}\", x0={x}., y0={y}., x1={}., y1={}.);\n",
                    x + 1,
                    y + 1,
                ));
            }
        }
    }
    program.push_str("}\n");
    std::fs::write(&source, program).unwrap();
    let config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tech/basic.tech.toml"),
    ));
    let (solved, _) = prepare(&source, &config);
    let index = RasterSpatialIndex::default();
    let root = solved.state[&solved.selected_scope].address;
    let extents = index.layer_extents(&solved, root.cell);
    assert_eq!(extents.len(), 2);
    assert!(
        index.cells.lock().unwrap().is_empty(),
        "coarse footprints must not build detailed BVHs"
    );
    index.cell_index(&solved, root.cell);
    let bounds = RasterBvhBounds {
        min_x: -1.,
        min_y: -1.,
        max_x: 33.,
        max_y: 33.,
    };
    let (emits, occupancies) = index
        .query_lod_ready_bounded(root, bounds, 8., &mut GeometryWorkBudget(256))
        .expect("zoomed-out work must follow screen detail")
        .unwrap();
    assert!(emits.is_empty());
    assert!(
        occupancies.len() <= 64,
        "interleaved layers must not force one occupancy per shape"
    );
    assert!(occupancies.iter().all(|lod| lod.layers.len() == 1));
    let (emits, occupancies) = index.query_lod(&solved, root, bounds, 2048, 0.);
    assert_eq!(
        emits.len(),
        2048,
        "zooming in must recover every original shape"
    );
    assert!(occupancies.is_empty());
}

#[test]
fn subpixel_parallel_wires_merge_but_visible_gaps_remain() {
    let wire = |emit_index, x0, x1, y0, y1| RasterBvhItem {
        emit_index,
        bounds: RasterBvhBounds {
            min_x: x0,
            max_x: x1,
            min_y: y0,
            max_y: y1,
        },
        lod_layer: Some("met1".into()),
        instance_layers: None,
    };
    let node = RasterBvhNode::build(
        (0..32)
            .map(|i| wire(i, 0., 100., i as f64 * 0.01, i as f64 * 0.01 + 0.001))
            .collect(),
    )
    .unwrap();
    let query = RasterBvhBounds {
        min_x: -1.,
        min_y: -1.,
        max_x: 101.,
        max_y: 1.,
    };
    let mut emits = Vec::new();
    let mut occupancies = Vec::new();
    node.query_lod(query, 8., &mut emits, &mut occupancies);
    assert!(emits.is_empty());
    assert_eq!(
        occupancies.len(),
        1,
        "subpixel wire pitch must not require visiting every wire"
    );
    occupancies.clear();
    node.query_lod(query, 0.008, &mut emits, &mut occupancies);
    assert_eq!(emits.len(), 32, "zooming in must resolve every wire again");
    assert!(occupancies.is_empty());
    let separated =
        RasterBvhNode::build(vec![wire(0, 0., 1., 0., 0.1), wire(1, 99., 100., 0., 0.1)]).unwrap();
    emits.clear();
    separated.query_lod(query, 8., &mut emits, &mut occupancies);
    assert_eq!(
        occupancies.len(),
        2,
        "a thin row must not fill a visible gap between segments"
    );
}

fn test_editor(
    canvas: &Entity<LayoutCanvas>,
    cx: &mut gpui::TestAppContext,
) -> Entity<editor::Editor> {
    let state = canvas.read_with(cx, |canvas, _| canvas.state.clone());
    cx.new(|cx| editor::Editor {
        title_bar: cx.new(|_| editor::TitleBar::new(&state)),
        tool_bar: cx.new(|_| editor::ToolBar::new(&state)),
        hierarchy_sidebar: cx.new(|cx| editor::HierarchySideBar::new(cx, &state, canvas)),
        layer_sidebar: cx.new(|cx| editor::LayerSideBar::new(cx, &state, canvas)),
        state,
        canvas: canvas.clone(),
    })
}

#[gpui::test]
fn rendering_indicator_does_not_resize_the_canvas(cx: &mut gpui::TestAppContext) {
    let canvas = test_canvas(cx);
    let editor = test_editor(&canvas, cx);
    canvas.update(cx, |canvas, _| canvas.pending_init = false);
    let cx = cx.add_empty_window();
    for font_size in [None, Some(14.), Some(20.)] {
        let mut idle_bounds = None;
        for rendering in [false, true, false, true, false] {
            editor.update(cx, |editor, cx| {
                editor.state.update(cx, |state, cx| {
                    state.rendering = rendering;
                    state.font_size = font_size;
                    cx.notify();
                });
                cx.notify();
            });
            cx.draw(
                Point::default(),
                Size::new(px(1200.), px(800.)).map(gpui::AvailableSpace::Definite),
                |_, _| div().size_full().child(editor.clone()),
            );
            let bounds = canvas.read_with(cx, |canvas, _| canvas.screen_bounds);
            eprintln!("font {font_size:?}, rendering {rendering}: canvas {bounds:?}");
            assert_eq!(
                *idle_bounds.get_or_insert(bounds),
                bounds,
                "showing the spinner must not invalidate every viewport tile"
            );
        }
    }
}

#[gpui::test]
fn full_editor_pan_and_zoom_settle_without_presentation_oscillation(cx: &mut gpui::TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("lib.ar");
    std::fs::write(
        &source,
        r#"
cell leaf() { let shape = rect("met1", x0=0., y0=0., x1=10., y1=10.); }
cell top() {
    for row in std::range(128) {
        for col in std::range(128) {
            let child = inst(leaf(), x=(col as Float)*12., y=(row as Float)*12.);
        }
    }
}
"#,
    )
    .unwrap();
    let config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tech/basic.tech.toml"),
    ));
    let (solved, layers) = prepare(&source, &config);
    let canvas = test_canvas(cx);
    load_canvas(&canvas, solved, layers, cx);
    let editor = test_editor(&canvas, cx);
    let cx = cx.add_empty_window();
    let draw = |cx: &mut gpui::VisualTestContext| {
        editor.update(cx, |_, cx| cx.notify());
        cx.draw(
            Point::default(),
            Size::new(px(1200.), px(800.)).map(gpui::AvailableSpace::Definite),
            |_, _| div().size_full().child(editor.clone()),
        );
    };
    draw(cx);
    let bounds = canvas.read_with(cx, |canvas, _| canvas.screen_bounds);
    let settle = |cx: &mut gpui::VisualTestContext| {
        let mut previous = canvas.read_with(cx, |canvas, _| canvas.last_presented_raster.get());
        let mut camera_changes = 0;
        for _ in 0..1024 {
            draw(cx);
            let pending = canvas.update(cx, |canvas, _| {
                assert_eq!(canvas.screen_bounds, bounds);
                let current = canvas.last_presented_raster.get();
                if current != previous {
                    camera_changes += 1;
                    assert!(
                        camera_changes <= 1,
                        "a stopped gesture must have only one camera handoff"
                    );
                    assert!(raster_display_matches_camera(
                        current.expect("must not blank the retained frame"),
                        canvas.scale,
                        canvas.offset
                    ));
                    previous = current;
                }
                if let Some(display) = current {
                    assert!(raster_tiles_cover_bounds(
                        canvas.raster_tiles.as_ref().unwrap(),
                        bounds,
                        bounds,
                        display.scale,
                        display.offset
                    ));
                }
                canvas.raster_worker_active
                    || canvas.raster_decision_refinement.is_some()
                    || canvas.raster_overview_requested_revision.is_some()
            });
            if !pending {
                let generation = canvas.read_with(cx, |canvas, cx| {
                    assert!(!canvas.state.read(cx).rendering);
                    assert!(raster_display_matches_camera(
                        canvas.last_presented_raster.get().unwrap(),
                        canvas.scale,
                        canvas.offset
                    ));
                    canvas.raster_generation
                });
                // Include paints after the spinner disappears, where the
                // status bar's old intrinsic-height feedback loop started.
                for _ in 0..5 {
                    draw(cx);
                    canvas.update(cx, |canvas, cx| {
                        assert_eq!(canvas.screen_bounds, bounds);
                        assert_eq!(canvas.raster_generation, generation);
                        assert!(!canvas.state.read(cx).rendering);
                    });
                }
                return;
            }
            assert!(
                cx.dispatcher.tick(false),
                "pending renderer has no runnable work"
            );
        }
        panic!("full editor rendering never settled");
    };
    settle(cx);
    // Reclassifying a fully cached dense view starts no tile worker. The
    // decision's completion must still clear its own rendering indicator.
    canvas.update(cx, |canvas, cx| canvas.request_visible_raster_decision(cx));
    settle(cx);
    for _ in 0..2 {
        for delta in [32., 900., 3600., -4400., -100.] {
            canvas.update(cx, |canvas, cx| {
                canvas.pan_view(Point::new(px(delta), px(0.)), cx)
            });
            draw(cx);
        }
        settle(cx);
        let previous = canvas.read_with(cx, |canvas, _| canvas.last_presented_raster.get());
        canvas.update(cx, |canvas, cx| {
            canvas.zoom_about(bounds.center(), canvas.scale * 1.25, cx)
        });
        draw(cx);
        canvas.update(cx, |canvas, _| {
            assert_eq!(
                canvas.last_presented_raster.get(),
                previous,
                "zoom must hold the existing image until the requested LOD is ready"
            )
        });
        settle(cx);
    }
}

#[gpui::test]
fn edits_and_small_zoom_steps_keep_a_complete_frame(cx: &mut gpui::TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("lib.ar");
    let program = r#"
cell leaf() { let shape = rect("met1", x0=0., y0=0., x1=10., y1=10.); }
cell top() {
    for row in std::range(96) {
        for col in std::range(96) {
            let child = inst(leaf(), x=(col as Float)*12., y=(row as Float)*12.);
        }
    }
}
"#;
    std::fs::write(&source, program).unwrap();
    let config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tech/basic.tech.toml"),
    ));
    let mut compiler = argonc::incremental::IncrementalCompiler::new();
    compiler.set_source_text(source.clone(), program);
    let first = compiler.compile_cell(&config, &["top".into()], vec![]);
    let canvas = test_canvas(cx);
    let editor = test_editor(&canvas, cx);
    editor.update(cx, |editor, cx| {
        let context = editor.begin_snapshot_preparation(cx, 1);
        let prepared = editor::prepare_compilation_snapshot(
            analyzer::rpc::CompilationSnapshot {
                revision: 1,
                output: first,
            },
            context,
        );
        editor.finish_snapshot_preparation(cx, 1, prepared);
    });
    let cx = cx.add_empty_window();
    let draw = |cx: &mut gpui::VisualTestContext| {
        editor.update(cx, |_, cx| cx.notify());
        cx.draw(
            Point::default(),
            Size::new(px(1200.), px(800.)).map(gpui::AvailableSpace::Definite),
            |_, _| div().size_full().child(editor.clone()),
        );
    };
    let signature = |cx: &gpui::VisualTestContext| {
        canvas.read_with(cx, |canvas, _| {
            canvas
                .last_presented_raster
                .get()
                .map(|display| (true, display))
                .or_else(|| {
                    canvas
                        .retained_direct_frame
                        .as_ref()
                        .map(|frame| (false, frame.display))
                })
        })
    };
    let settle = |cx: &mut gpui::VisualTestContext, require_frame: bool| {
        let mut previous = signature(cx);
        let mut handoffs = 0;
        for _ in 0..1024 {
            draw(cx);
            if require_frame {
                assert!(
                    canvas.read_with(cx, |canvas, _| canvas.painted_complete_frame),
                    "layout disappeared during update"
                );
            }
            let current = signature(cx);
            if current != previous {
                handoffs += 1;
                if require_frame {
                    assert!(
                        handoffs <= 1,
                        "multiple presentations for one stopped gesture: {previous:?} -> {current:?}"
                    );
                }
                previous = current;
            }
            let pending = canvas.read_with(cx, |canvas, _| {
                canvas.raster_worker_active
                    || canvas.raster_decision_refinement.is_some()
                    || canvas.raster_overview_requested_revision.is_some()
            });
            if !pending {
                assert!(canvas.read_with(cx, |canvas, cx| !canvas.state.read(cx).rendering));
                return;
            }
            assert!(cx.dispatcher.tick(false));
        }
        panic!("renderer did not settle");
    };
    settle(cx, false);
    let bounds = canvas.read_with(cx, |canvas, _| canvas.screen_bounds);
    // Cross the raster/direct threshold in small wheel-like increments, then
    // return through it. Every input gets one completed presentation at most.
    for factor in std::iter::repeat_n(1.08, 28).chain(std::iter::repeat_n(1. / 1.08, 28)) {
        canvas.update(cx, |canvas, cx| {
            canvas.zoom_about(bounds.center(), canvas.scale * factor, cx)
        });
        settle(cx, true);
    }
    canvas.update(cx, |canvas, cx| {
        canvas.pan_view(Point::new(px(137.), px(29.)), cx)
    });
    settle(cx, true);
    let old_top = canvas.read_with(cx, |canvas, cx| {
        canvas
            .state
            .read(cx)
            .solved_cell
            .read(cx)
            .as_ref()
            .unwrap()
            .output
            .top
    });
    for (index, rectangle) in [
        "let added = rect(\"met1\", x0=0., y0=0., x1=2000., y1=2000.);",
        "",
    ]
    .iter()
    .enumerate()
    {
        let edited = program.replace("cell top() {", &format!("cell top() {{ {rectangle}"));
        compiler.set_source_text(source.clone(), edited);
        let output = compiler.compile_cell(&config, &["top".into()], vec![]);
        assert_ne!(
            match &output {
                compile::CompileOutput::Valid(data) => data.top,
                _ => panic!("compile failed"),
            },
            old_top,
            "exercise changed compiler handles"
        );
        editor.update(cx, |editor, cx| {
            let id = index as u64 + 2;
            let context = editor.begin_snapshot_preparation(cx, id);
            let prepared = editor::prepare_compilation_snapshot(
                analyzer::rpc::CompilationSnapshot {
                    revision: id,
                    output,
                },
                context,
            );
            editor.finish_snapshot_preparation(cx, id, prepared);
        });
        settle(cx, true);
    }
}

#[gpui::test]
#[ignore = "requires ARGON_RENDER_GDS; measures a top-level edit through snapshot preparation"]
fn local_sram_edit_pipeline(cx: &mut gpui::TestAppContext) {
    let gds = std::path::PathBuf::from(
        std::env::var_os("ARGON_RENDER_GDS").expect("set ARGON_RENDER_GDS"),
    );
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("lib.ar");
    let program = r#"
cell bank() {
    let memory = sram();
    let bounds = bbox(memory);
    let pitch_x = bounds.x1 - bounds.x0 + 50000.;
    let pitch_y = bounds.y1 - bounds.y0 + 50000.;
    for row in std::range(4) {
        for col in std::range(4) {
            let macro = inst(memory, x=(col as Float)*pitch_x-bounds.x0, y=(row as Float)*pitch_y-bounds.y0);
        }
    }
}

cell top() { let bank = inst(bank(), x=0., y=0.); }
"#;
    std::fs::write(&source, program).unwrap();
    let config = argonc::WorkspaceConfig::new(&source)
        .with_tech(Some(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../pdks/sky130/sky130.tech.toml"),
        ))
        .with_gds_imports([("sram".to_owned(), gds)]);
    let mut compiler = argonc::incremental::IncrementalCompiler::new();
    compiler.set_source_text(source.clone(), program);
    let start = Instant::now();
    let first = compiler.compile_cell(&config, &["top".to_owned()], vec![]);
    assert!(matches!(first, compile::CompileOutput::Valid(_)));
    eprintln!(
        "Initial compile {:?}; stats {:?}",
        start.elapsed(),
        compiler.stats()
    );
    let misses = compiler.stats().gds_cache.misses;
    let sender_base = analyzer::rpc::CompilationSnapshot {
        revision: 1,
        output: first,
    };
    let receiver_base = roundtrip_update(&sender_base, None, None);
    let start = Instant::now();
    let mut first = editor::prepare_compilation_snapshot(
        receiver_base.clone(),
        editor::CompilationPreparationContext {
            layers: IndexMap::new(),
            selected_scope: None,
            scope_state: None,
            previous: None,
        },
    );
    eprintln!("Initial GUI preparation {:?}", start.elapsed());
    let prepared = first.prepared_output.take().unwrap();
    eprintln!(
        "Prepared {} distinct paths, {} scope addresses",
        prepared.state.len(),
        prepared.scope_paths.len()
    );
    if std::env::var_os("ARGON_VERIFY_HIERARCHY").is_some() {
        let start = Instant::now();
        let compile::CompileOutput::Valid(output) = &first.output else {
            unreachable!()
        };
        editor::hierarchy::tests::verify_prepared(output, &prepared);
        eprintln!(
            "Full SRAM hierarchy matches expanded reference ({:?}, excluded from preparation timing)",
            start.elapsed()
        );
    }
    let context = editor::CompilationPreparationContext {
        layers: prepared.layers,
        selected_scope: Some(prepared.selected_scope.clone()),
        scope_state: Some(Arc::new(prepared.state.clone())),
        previous: Some(CompileOutputState {
            output: Arc::new(first.output.unwrap_valid()),
            selected_scope: prepared.selected_scope.clone(),
            state: Arc::new(prepared.state),
            scope_paths: Arc::new(prepared.scope_paths),
            hierarchy: prepared.hierarchy,
        }),
    };

    let canvas = test_canvas(cx);
    load_canvas(
        &canvas,
        context.previous.clone().unwrap(),
        Arc::new(context.layers.clone()),
        cx,
    );
    let editor = test_editor(&canvas, cx);
    let cx = cx.add_empty_window();
    let settle = |cx: &mut gpui::VisualTestContext, require_frame: bool| {
        let mut max_paint = std::time::Duration::ZERO;
        let start_render = Instant::now();
        let mut first_current_frame = None;
        for _ in 0..2048 {
            let start = Instant::now();
            editor.update(cx, |_, cx| cx.notify());
            cx.draw(
                Point::default(),
                Size::new(px(1200.), px(800.)).map(gpui::AvailableSpace::Definite),
                |_, _| div().size_full().child(editor.clone()),
            );
            max_paint = max_paint.max(start.elapsed());
            let pending = canvas.read_with(cx, |canvas, _| {
                let current = canvas.raster_tiles.as_ref().is_some_and(|tiles| {
                    canvas.last_presented_raster.get().is_some()
                        && tiles.content_revision == canvas.raster_content_revision
                }) || canvas
                    .retained_direct_frame
                    .as_ref()
                    .is_some_and(|frame| frame.content_revision == canvas.raster_content_revision);
                if current && canvas.painted_complete_frame && first_current_frame.is_none() {
                    first_current_frame = Some(start_render.elapsed());
                }
                if require_frame {
                    assert!(
                        canvas.painted_complete_frame,
                        "SRAM disappeared during edit"
                    );
                }
                canvas.raster_worker_active
                    || canvas.raster_decision_refinement.is_some()
                    || canvas.raster_overview_requested_revision.is_some()
            });
            if !pending {
                assert!(canvas.read_with(cx, |canvas, cx| !canvas.state.read(cx).rendering));
                if require_frame {
                    eprintln!(
                        "First current-revision frame {:?}",
                        first_current_frame.expect("updated frame never appeared")
                    );
                }
                return max_paint;
            }
            assert!(cx.dispatcher.tick(false));
        }
        panic!("SRAM renderer did not settle");
    };
    settle(cx, false);
    canvas.update(cx, |canvas, cx| {
        canvas.pan_view(Point::new(px(137.), px(29.)), cx)
    });
    settle(cx, true);
    let edited = program.replace("let bank = inst(bank(), x=0., y=0.);", "let bank = inst(bank(), x=0., y=0.); let added = rect(\"met1.drawing\", x0=0., y0=0., x1=1000., y1=1000.);");
    compiler.set_source_text(source, edited);
    let start = Instant::now();
    let output = compiler.compile_cell(&config, &["top".to_owned()], vec![]);
    eprintln!(
        "One-rectangle incremental compile {:?}; stats {:?}",
        start.elapsed(),
        compiler.stats()
    );
    assert!(matches!(output, compile::CompileOutput::Valid(_)));
    assert_eq!(
        compiler.stats().gds_cache.misses,
        misses,
        "editing the top must not re-import the SRAM"
    );
    let snapshot = analyzer::rpc::CompilationSnapshot {
        revision: 2,
        output,
    };
    let received = roundtrip_update(&snapshot, Some(&sender_base), Some(&receiver_base));
    let start = Instant::now();
    let context = editor.update(cx, |editor, cx| editor.begin_snapshot_preparation(cx, 2));
    let prepared = editor::prepare_compilation_snapshot(received, context);
    eprintln!("One-rectangle GUI preparation {:?}", start.elapsed());
    assert!(prepared.prepared_output.is_some());
    if std::env::var_os("ARGON_VERIFY_HIERARCHY").is_some() {
        editor::hierarchy::tests::verify_prepared(
            match &prepared.output {
                compile::CompileOutput::Valid(data) => data,
                _ => unreachable!(),
            },
            prepared.prepared_output.as_ref().unwrap(),
        );
    }

    let start = Instant::now();
    editor.update(cx, |editor, cx| {
        editor.finish_snapshot_preparation(cx, 2, prepared)
    });
    eprintln!("Apply edit on UI {:?}", start.elapsed());
    let start = Instant::now();
    let max_paint = settle(cx, true);
    eprintln!(
        "Render edit to idle {:?}; maximum UI paint {:?}",
        start.elapsed(),
        max_paint
    );
}

#[gpui::test]
fn direct_frame_stays_visible_until_the_first_pan_raster_arrives(cx: &mut gpui::TestAppContext) {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("lib.ar");
    let mut program = String::from("cell top() {\n");
    for i in 0..3000 {
        let x = (i % 60) * 10;
        let y = (i / 60) * 10;
        program.push_str(&format!(
            "let r{i} = rect(\"met1\", x0={x}., y0={y}., x1={}., y1={}.);\n",
            x + 5,
            y + 5
        ));
    }
    program.push_str("}\n");
    std::fs::write(&source, program).unwrap();
    let config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tech/basic.tech.toml"),
    ));
    let (solved, layers) = prepare(&source, &config);
    let size = Size::new(px(1200.), px(800.));
    let viewport = ViewportTransform {
        size,
        screen_size: size,
        scale: 1.,
        offset: Point::new(px(0.), px(800.)),
    };
    let index = Arc::new(RasterSpatialIndex::default());
    // Warm only the spatial index. There are deliberately no retained images.
    build_navigation_raster(input(
        solved.clone(),
        layers.clone(),
        viewport,
        index.clone(),
    ))
    .unwrap();
    let canvas = test_canvas(cx);
    load_canvas(&canvas, solved, layers, cx);
    canvas.update(cx, |canvas, cx| {
        canvas.update_raster_presentation(cx);
        canvas.pending_init = false;
        canvas.scale = viewport.scale;
        canvas.offset = viewport.offset;
        canvas.raster_spatial_index = index;
        canvas.raster_cache_enabled = false;
        canvas.raster_prefetch_enabled = false;
        canvas.raster_decision_refinement = None;
    });
    let cx = cx.add_empty_window();
    let draw = |cx: &mut gpui::VisualTestContext| {
        cx.draw(
            Point::default(),
            size.map(gpui::AvailableSpace::Definite),
            |_, _| CanvasElement {
                inner: canvas.clone(),
            },
        );
    };
    draw(cx);
    let frame = canvas.read_with(cx, |canvas, _| {
        assert!(canvas.painted_complete_frame);
        assert_eq!(canvas.rects.len(), 3000);
        assert!(canvas.raster_worker_active);
        canvas.retained_direct_frame.clone().unwrap()
    });
    // Repainting before the first tile used to clear the previously visible
    // direct layout. A pan outrunning the worker must keep it as well.
    for delta in [0., 4500., -4468.] {
        if delta != 0. {
            canvas.update(cx, |canvas, cx| {
                canvas.pan_view(Point::new(px(delta), px(0.)), cx)
            });
        }
        draw(cx);
        canvas.update(cx, |canvas, _| {
            assert!(
                canvas.painted_complete_frame,
                "first raster handoff must never submit a blank placeholder"
            );
            assert!(
                canvas
                    .rects
                    .iter()
                    .map(|(rect, _)| rect)
                    .eq(frame.rects.iter().map(|(rect, _)| rect))
            );
            assert!(Arc::ptr_eq(
                canvas.retained_direct_frame.as_ref().unwrap(),
                &frame
            ));
        });
    }
    for _ in 0..1024 {
        draw(cx);
        let pending = canvas.read_with(cx, |canvas, _| {
            assert!(
                canvas.painted_complete_frame,
                "tile arrival must not blank the canvas"
            );
            canvas.raster_worker_active
                || canvas.raster_decision_refinement.is_some()
                || canvas.raster_overview_requested_revision.is_some()
        });
        if !pending {
            break;
        }
        assert!(cx.dispatcher.tick(false));
    }
    canvas.update(cx, |canvas, cx| {
        assert!(!canvas.state.read(cx).rendering);
        assert!(raster_display_matches_camera(
            canvas.last_presented_raster.get().unwrap(),
            canvas.scale,
            canvas.offset
        ));
        assert!(canvas.retained_direct_frame.is_none());
    });
}

/// Use independent sender/receiver caches and actual serialization, so the
/// tests cannot accidentally reuse pointers that would be lost on the wire.
fn roundtrip_update(
    snapshot: &analyzer::rpc::CompilationSnapshot,
    sender_base: Option<&analyzer::rpc::CompilationSnapshot>,
    receiver_base: Option<&analyzer::rpc::CompilationSnapshot>,
) -> analyzer::rpc::CompilationSnapshot {
    let start = Instant::now();
    let update = analyzer::rpc::CompilationUpdate::new(snapshot.clone(), sender_base);
    let changed = update.snapshot.data().map_or(0, |data| data.cells.len());
    let bytes = bincode::serialize(&update).unwrap();
    eprintln!(
        "Snapshot delta: {changed} cells, {} bytes; encode {:?}",
        bytes.len(),
        start.elapsed()
    );
    let start = Instant::now();
    let decoded: analyzer::rpc::CompilationUpdate = bincode::deserialize(&bytes).unwrap();
    let result = decoded.materialize(receiver_base).unwrap();
    eprintln!("Decode + materialize {:?}", start.elapsed());
    result
}

#[test]
fn reused_render_indexes_invalidate_changed_children_and_match_fresh_pixels() {
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("lib.ar");
    std::fs::write(
        &source,
        r#"
cell leaf() { let r = rect("met1", x0=0., y0=0., x1=10., y1=10.); }
cell unchanged() { let r = rect("met2", x0=0., y0=0., x1=10., y1=10.); }
cell top() { let a = inst(leaf(), x=0., y=0.); let b = inst(unchanged(), x=70., y=0.); }
"#,
    )
    .unwrap();
    let config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/tech/basic.tech.toml"),
    ));
    let (before, layers) = prepare(&source, &config);
    let old_index = Arc::new(RasterSpatialIndex::default());
    for cell in before.output.cells.keys() {
        old_index.cell_index(&before, *cell);
        old_index.layer_extents(&before, *cell);
    }
    let viewport = ViewportTransform {
        size: Size::new(px(240.), px(120.)),
        screen_size: Size::new(px(240.), px(120.)),
        scale: 2.,
        offset: Point::new(px(10.), px(100.)),
    };
    let before_pixels = build_navigation_raster(input(
        before.clone(),
        layers.clone(),
        viewport,
        old_index.clone(),
    ))
    .unwrap();
    let mut output = before.output.as_ref().clone();
    let leaf = *output
        .cells
        .iter()
        .find(|(_, cell)| cell.name == "leaf")
        .unwrap()
        .0;
    let unchanged = *output
        .cells
        .iter()
        .find(|(_, cell)| cell.name == "unchanged")
        .unwrap()
        .0;
    let changed_cell = Arc::make_mut(output.cells.get_mut(&leaf).unwrap());
    for object in changed_cell.objects.values_mut() {
        if let SolvedValue::Rect(rect) = object
            && !rect.construction
        {
            rect.x1.0 = 40.;
        }
    }
    let mut prepared = editor::prepare_compilation_snapshot(
        analyzer::rpc::CompilationSnapshot {
            revision: 2,
            output: compile::CompileOutput::Valid(output),
        },
        editor::CompilationPreparationContext {
            layers: layers.as_ref().clone(),
            selected_scope: Some(before.selected_scope.clone()),
            scope_state: Some(before.state.clone()),
            previous: Some(before.clone()),
        },
    );
    let metadata = prepared.prepared_output.take().unwrap();
    let after = CompileOutputState {
        output: Arc::new(prepared.output.unwrap_valid()),
        selected_scope: metadata.selected_scope,
        state: Arc::new(metadata.state),
        scope_paths: Arc::new(metadata.scope_paths),
        hierarchy: metadata.hierarchy,
    };
    let mut reused = RasterSpatialIndex::default();
    reused.reuse_ready_cells(&old_index, |cell| {
        after
            .hierarchy
            .same_cell(&before.hierarchy, &after.output, cell)
    });
    let cells = reused.cells.lock().unwrap();
    assert!(!cells.contains_key(&leaf));
    assert!(
        !cells.contains_key(&after.output.top),
        "an unchanged parent handle still depends on changed child bounds"
    );
    assert!(Arc::ptr_eq(
        &cells[&unchanged],
        &old_index.cells.lock().unwrap()[&unchanged]
    ));
    drop(cells);
    let reused_pixels = build_navigation_raster(input(
        after.clone(),
        layers.clone(),
        viewport,
        Arc::new(reused),
    ))
    .unwrap();
    let fresh_pixels =
        build_navigation_raster(input(after, layers, viewport, Arc::default())).unwrap();
    assert_eq!(
        reused_pixels.image.as_bytes(0),
        fresh_pixels.image.as_bytes(0)
    );
    assert_ne!(
        before_pixels.image.as_bytes(0),
        fresh_pixels.image.as_bytes(0)
    );
}

#[gpui::test]
fn placed_rectangle_stays_visible_until_direct_frame_arrives(cx: &mut gpui::TestAppContext) {
    rectangle_placement_handoff(cx, false, None);
}

#[gpui::test]
fn placed_rectangle_stays_visible_until_raster_frame_arrives(cx: &mut gpui::TestAppContext) {
    rectangle_placement_handoff(cx, true, None);
}

#[gpui::test]
#[ignore = "requires ARGON_RENDER_GDS; places a rectangle over the local SRAM"]
fn local_sram_rectangle_placement(cx: &mut gpui::TestAppContext) {
    let gds = std::env::var_os("ARGON_RENDER_GDS").expect("set ARGON_RENDER_GDS");
    rectangle_placement_handoff(cx, true, Some(gds.into()));
}

fn rectangle_placement_handoff(
    cx: &mut gpui::TestAppContext,
    dense: bool,
    gds: Option<std::path::PathBuf>,
) {
    use analyzer::rpc::{LangServerRequest, LangServerResponse, RectangleEditResult};
    use futures::{FutureExt, SinkExt, StreamExt};
    let directory = tempfile::tempdir().unwrap();
    let source = directory.path().join("lib.ar");
    let layer_name = if gds.is_some() {
        "met1.drawing"
    } else {
        "met1"
    };
    let program = if gds.is_some() {
        "cell top() { let memory = inst(sram(), x=0., y=0.); }"
    } else if dense {
        r#"
cell leaf() { let shape = rect("met1", x0=0., y0=0., x1=10., y1=10.); }
cell top() { for row in std::range(96) { for col in std::range(96) {
    let child = inst(leaf(), x=(col as Float)*12., y=(row as Float)*12.);
} } }
"#
    } else {
        "cell top() { let existing = rect(\"met1\", x0=0., y0=0., x1=100., y1=100.); }"
    };
    std::fs::write(&source, program).unwrap();
    let is_sram = gds.is_some();
    let mut config = argonc::WorkspaceConfig::new(&source).with_tech(Some(
        std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(if is_sram {
            "../../pdks/sky130/sky130.tech.toml"
        } else {
            "../../examples/tech/basic.tech.toml"
        }),
    ));
    if let Some(gds) = gds {
        config = config.with_gds_imports([("sram".to_owned(), gds)]);
    }
    let mut compiler = argonc::incremental::IncrementalCompiler::new();
    compiler.set_source_text(source.clone(), program);
    let canvas = test_canvas(cx);
    let (client, mut server) = crate::rpc::SyncLangServerClient::for_rpc_test(cx.to_async());
    canvas.update(cx, |canvas, cx| {
        canvas
            .state
            .update(cx, |state, _| state.lang_server_client = client)
    });
    let editor = test_editor(&canvas, cx);
    let cx = cx.add_empty_window();
    let apply = |output, revision, cx: &mut gpui::VisualTestContext| {
        editor.update(cx, |editor, cx| {
            let context = editor.begin_snapshot_preparation(cx, revision);
            let prepared = editor::prepare_compilation_snapshot(
                analyzer::rpc::CompilationSnapshot { revision, output },
                context,
            );
            editor.finish_snapshot_preparation(cx, revision, prepared);
        })
    };
    apply(
        compiler.compile_cell(&config, &["top".into()], vec![]),
        1,
        cx,
    );
    let draw = |cx: &mut gpui::VisualTestContext| {
        editor.update(cx, |_, cx| cx.notify());
        cx.draw(
            Point::default(),
            Size::new(px(1200.), px(800.)).map(gpui::AvailableSpace::Definite),
            |_, _| div().size_full().child(editor.clone()),
        );
    };
    let settle = |cx: &mut gpui::VisualTestContext, require_preview: bool| {
        for _ in 0..1024 {
            draw(cx);
            let pending = canvas.read_with(cx, |canvas, _| {
                if require_preview {
                    assert_eq!(
                        canvas.painted_rectangle_previews.len(),
                        1,
                        "placed rectangle disappeared"
                    );
                }
                canvas.raster_worker_active
                    || canvas.raster_decision_refinement.is_some()
                    || canvas.raster_overview_requested_revision.is_some()
            });
            if !pending {
                return;
            }
            assert!(cx.dispatcher.tick(false));
        }
        panic!("renderer did not settle");
    };
    settle(cx, false);
    if !is_sram {
        assert_eq!(
            canvas.read_with(cx, |canvas, _| canvas.last_presented_raster.get().is_some()),
            dense
        );
    }
    canvas.update(cx, |canvas, cx| {
        let state = canvas.state.read(cx);
        let layers = state.layers.clone();
        let tool = state.tool.clone();
        layers.update(cx, |layers, _| {
            layers.selected_layer = Some(layer_name.into())
        });
        tool.update(cx, |tool, _| {
            *tool = ToolState::DrawRect(DrawRectToolState::default())
        });
    });
    let bounds = canvas.read_with(cx, |canvas, _| canvas.screen_bounds);
    let p0 = bounds.center();
    let p1 = p0 + Point::new(px(80.), px(60.));
    let click = |position, cx: &mut gpui::VisualTestContext| {
        cx.update(|window, cx| {
            canvas.update(cx, |canvas, cx| {
                canvas.mouse_position = position;
                canvas.on_left_mouse_down(
                    &MouseDownEvent {
                        button: MouseButton::Left,
                        position,
                        ..Default::default()
                    },
                    window,
                    cx,
                );
            })
        })
    };
    let committed_rect_count = canvas.read_with(cx, |canvas, _| {
        canvas
            .retained_direct_frame
            .as_ref()
            .map(|frame| frame.rects.len())
    });
    click(p0, cx);
    canvas.update(cx, |canvas, _| canvas.mouse_position = p1);
    draw(cx);
    assert_eq!(
        canvas.read_with(cx, |canvas, _| canvas.painted_rectangle_previews.len()),
        1
    );
    assert_eq!(
        canvas.read_with(cx, |canvas, _| canvas
            .retained_direct_frame
            .as_ref()
            .map(|frame| frame.rects.len())),
        committed_rect_count,
        "uncommitted tool previews must not enter retained frames"
    );
    click(p1, cx);
    draw(cx);
    assert_eq!(
        canvas.read_with(cx, |canvas, _| canvas.painted_rectangle_previews.len()),
        1
    );
    assert_eq!(
        canvas.read_with(cx, |canvas, _| canvas.pending_rectangles.len()),
        1
    );
    // Actual RPC dispatch is stalled here. The second click must already have
    // returned, and tool changes, panning, zooming, and painting must still work.
    canvas.update(cx, |canvas, cx| {
        canvas.state.read(cx).tool.clone().update(cx, |tool, cx| {
            *tool = ToolState::default();
            cx.notify();
        });
        canvas.pan_view(Point::new(px(12.), px(8.)), cx);
        canvas.zoom_about(bounds.center(), canvas.scale * 1.02, cx);
    });
    settle(cx, true);
    let request = loop {
        if let Some(message) = server.next().now_or_never() {
            let tarpc::ClientMessage::Request(request) = message.unwrap().unwrap() else {
                panic!("expected source edit")
            };
            break request;
        }
        assert!(cx.dispatcher.tick(false));
        draw(cx);
        assert_eq!(
            canvas.read_with(cx, |canvas, _| canvas.painted_rectangle_previews.len()),
            1
        );
    };
    let LangServerRequest::DrawRect {
        scope_span,
        var_name,
        rect,
    } = request.message
    else {
        panic!("expected rectangle RPC")
    };
    let edited = program.replace(
        "cell top() {",
        &format!(
            "cell top() {{ let {var_name} = rect(\"{layer_name}\", x0i={}, y0i={}, x1i={}, y1i={})!;",
            compile::format_initial_condition(rect.x0, 0.1),
            compile::format_initial_condition(rect.y0, 0.1),
            compile::format_initial_condition(rect.x1, 0.1),
            compile::format_initial_condition(rect.y1, 0.1)
        ),
    );
    let mut accepted = Some(tarpc::Response {
        request_id: request.id,
        message: Ok(LangServerResponse::DrawRect(Some(RectangleEditResult {
            span: scope_span.clone(),
            revision: 1,
        }))),
    });
    if !dense {
        server
            .send(accepted.take().unwrap())
            .now_or_never()
            .unwrap()
            .unwrap();
        // Paint after every task through the reply, before compilation begins.
        for _ in 0..128 {
            draw(cx);
            assert_eq!(
                canvas.read_with(cx, |canvas, _| canvas.painted_rectangle_previews.len()),
                1
            );
            if canvas.read_with(cx, |canvas, _| {
                canvas.pending_rectangles[0].receipt.is_some()
            }) {
                break;
            }
            assert!(cx.dispatcher.tick(false));
        }
        assert!(canvas.read_with(cx, |canvas, _| {
            canvas.pending_rectangles[0].receipt.is_some()
        }));
    }
    compiler.set_source_text(source.clone(), edited);
    apply(
        compiler.compile_cell(&config, &["top".into()], vec![]),
        2,
        cx,
    );
    let target = canvas.read_with(cx, |canvas, _| canvas.raster_content_revision);
    let mut showed_committed = false;
    for _ in 0..1024 {
        draw(cx);
        let pending = canvas.read_with(cx, |canvas, _| {
            assert!(canvas.painted_complete_frame);
            let current = canvas
                .last_presented_raster
                .get()
                .and_then(|_| {
                    canvas
                        .raster_tiles
                        .as_ref()
                        .map(|tiles| tiles.content_revision)
                })
                .or_else(|| {
                    canvas
                        .retained_direct_frame
                        .as_ref()
                        .map(|frame| frame.content_revision)
                })
                .unwrap();
            if current < target {
                assert_eq!(
                    canvas.painted_rectangle_previews.len(),
                    1,
                    "preview retired before its frame arrived"
                );
            } else {
                showed_committed = true;
                assert_eq!(
                    canvas.painted_rectangle_previews.len(),
                    0,
                    "preview double-painted the committed rectangle"
                );
                assert!(canvas.pending_rectangles.is_empty());
            }
            canvas.raster_worker_active
                || canvas.raster_decision_refinement.is_some()
                || canvas.raster_overview_requested_revision.is_some()
        });
        if !pending {
            break;
        }
        assert!(cx.dispatcher.tick(false));
    }
    assert!(showed_committed);
    if dense {
        // Also exercise compilation and frame presentation before the edit
        // receipt arrives. A late reply must not resurrect the retired preview.
        server
            .send(accepted.take().unwrap())
            .now_or_never()
            .unwrap()
            .unwrap();
        for _ in 0..8 {
            cx.dispatcher.tick(false);
            draw(cx);
            assert!(canvas.read_with(cx, |canvas, _| canvas.pending_rectangles.is_empty()));
            assert_eq!(
                canvas.read_with(cx, |canvas, _| canvas.painted_rectangle_previews.len()),
                0
            );
        }
    }
    // Rapid placements reserve distinct names before either edit has compiled.
    // Rejecting one drops only its preview; undoing the other before its first
    // compiled frame must not leave an optimistic rectangle behind forever.
    canvas.update(cx, |canvas, cx| {
        canvas.state.read(cx).tool.clone().update(cx, |tool, _| {
            *tool = ToolState::DrawRect(DrawRectToolState::default())
        })
    });
    click(p0, cx);
    click(p1, cx);
    draw(cx);
    assert_eq!(
        canvas.read_with(cx, |canvas, _| canvas.painted_rectangle_previews.len()),
        1
    );
    click(p0 + Point::new(px(100.), px(0.)), cx);
    click(p1 + Point::new(px(100.), px(0.)), cx);
    draw(cx);
    canvas.read_with(cx, |canvas, _| {
        assert_eq!(canvas.painted_rectangle_previews.len(), 2);
        assert_eq!(canvas.pending_rectangles.len(), 2);
        assert_ne!(
            canvas.pending_rectangles[0].name,
            canvas.pending_rectangles[1].name
        );
    });
    let request = loop {
        if let Some(message) = server.next().now_or_never()
            && let tarpc::ClientMessage::Request(request) = message.unwrap().unwrap()
        {
            break request;
        }
        assert!(cx.dispatcher.tick(false));
    };
    server
        .send(tarpc::Response {
            request_id: request.id,
            message: Ok(LangServerResponse::DrawRect(None)),
        })
        .now_or_never()
        .unwrap()
        .unwrap();
    for _ in 0..128 {
        draw(cx);
        if canvas.read_with(cx, |canvas, _| canvas.pending_rectangles.len() == 1) {
            break;
        }
        assert!(cx.dispatcher.tick(false));
    }
    assert_eq!(
        canvas.read_with(cx, |canvas, _| canvas.pending_rectangles.len()),
        1
    );
    draw(cx);
    assert_eq!(
        canvas.read_with(cx, |canvas, _| canvas.painted_rectangle_previews.len()),
        1
    );
    let request = loop {
        if let Some(message) = server.next().now_or_never()
            && let tarpc::ClientMessage::Request(request) = message.unwrap().unwrap()
        {
            break request;
        }
        assert!(cx.dispatcher.tick(false));
    };
    server
        .send(tarpc::Response {
            request_id: request.id,
            message: Ok(LangServerResponse::DrawRect(Some(RectangleEditResult {
                span: scope_span,
                revision: 2,
            }))),
        })
        .now_or_never()
        .unwrap()
        .unwrap();
    for _ in 0..128 {
        draw(cx);
        if canvas.read_with(cx, |canvas, _| {
            canvas.pending_rectangles[0].receipt.is_some()
        }) {
            break;
        }
        assert!(cx.dispatcher.tick(false));
    }
    assert!(canvas.read_with(cx, |canvas, _| {
        canvas.pending_rectangles[0].receipt.is_some()
    }));
    // The newer snapshot omits the accepted insertion, as when undo wins the
    // compilation race. It contains the previously committed layout unchanged.
    apply(
        compiler.compile_cell(&config, &["top".into()], vec![]),
        3,
        cx,
    );
    settle(cx, false);
    assert!(canvas.read_with(cx, |canvas, _| canvas.pending_rectangles.is_empty()));
    assert_eq!(
        canvas.read_with(cx, |canvas, _| canvas.painted_rectangle_previews.len()),
        0
    );
    assert!(canvas.read_with(cx, |canvas, _| canvas.painted_complete_frame));
}
