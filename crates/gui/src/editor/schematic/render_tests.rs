//! Headless tests of the schematic canvas: scenes built from compiled cells,
//! drawn once, and the root path across snapshots.

use argonc::compile::{CellArg, CompileOutput};
use gpui::{AppContext as _, AvailableSpace, Entity, Point, Size, TestAppContext, px};
use indexmap::IndexMap;

use super::paint::SchematicElement;
use super::scene::tests::{DEVICE_INVERTER, compile, compile_valid};
use super::{SchematicCanvas, SymbolSelection};
use crate::editor::canvas::ToolState;
use crate::editor::{self, EditorState, View};

fn test_state(cx: &mut TestAppContext, view: View) -> Entity<EditorState> {
    cx.new(|cx| {
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
            view,
            schematic_tool: Default::default(),
            symbol_tool: Default::default(),
            show_flylines: true,
            root_path: Vec::new(),
            base_top: None,
            cells: None,
        }
    })
}

/// Loads `output` as a fresh snapshot from the analyzer.
fn load(state: &Entity<EditorState>, output: CompileOutput, cx: &mut TestAppContext) {
    let top = match &output {
        CompileOutput::Valid(data) => Some(data.top),
        CompileOutput::ExecErrors(errors) => errors.output.as_ref().map(|data| data.top),
        _ => None,
    };
    state.update(cx, |state, cx| {
        state.update(cx, output);
        state.base_top = top;
        state.apply_root_path(cx);
        cx.notify();
    });
}

fn test_canvas(state: &Entity<EditorState>, cx: &mut TestAppContext) -> Entity<SchematicCanvas> {
    cx.new(|cx| {
        let focus = cx.focus_handle();
        SchematicCanvas::new(cx, state, focus)
    })
}

fn draw(canvas: &Entity<SchematicCanvas>, cx: &mut TestAppContext) {
    cx.run_until_parked();
    let cx = cx.add_empty_window();
    let size = Size::new(px(800.), px(600.));
    cx.draw(
        Point::default(),
        size.map(AvailableSpace::Definite),
        |_, _| SchematicElement {
            canvas: canvas.clone(),
        },
    );
    cx.run_until_parked();
}

fn top_name(state: &Entity<EditorState>, cx: &mut TestAppContext) -> String {
    state.read_with(cx, |state, cx| {
        let solved = state.solved_cell.read(cx);
        let solved = solved.as_ref().expect("a compiled cell");
        solved.output.cells[&solved.output.top].name.clone()
    })
}

#[gpui::test]
fn the_inverter_opens_in_the_schematic_view(cx: &mut TestAppContext) {
    let state = test_state(cx, View::Schematic);
    let data = compile_valid(
        DEVICE_INVERTER,
        "device_inv",
        vec![CellArg::Float(2.), CellArg::Float(1.)],
    );
    load(&state, CompileOutput::Valid(data), cx);
    let canvas = test_canvas(&state, cx);
    draw(&canvas, cx);
    canvas.read_with(cx, |canvas, _| {
        let scene = canvas.scene().expect("the scene is built");
        assert_eq!(scene.elements.len(), 2);
        assert_eq!(scene.wires.len(), 8);
        assert!(scene.flylines.is_empty());
        // The first paint fit the drawing into the canvas.
        assert!(canvas.viewport.scale > 1., "{:?}", canvas.viewport);
        assert_eq!(canvas.screen_bounds.size, Size::new(px(800.), px(600.)));
    });
    // Hiding flylines rebuilds the scene without a new snapshot.
    state.update(cx, |state, cx| {
        state.show_flylines = false;
        cx.notify();
    });
    cx.run_until_parked();
    canvas.read_with(cx, |canvas, _| {
        assert!(
            canvas
                .scene()
                .is_some_and(|scene| scene.flylines.is_empty())
        );
    });
}

#[gpui::test]
fn the_symbol_view_draws_the_cells_own_symbol(cx: &mut TestAppContext) {
    let state = test_state(cx, View::Symbol);
    let data = compile_valid(
        DEVICE_INVERTER,
        "device_inv",
        vec![CellArg::Float(2.), CellArg::Float(1.)],
    );
    load(&state, CompileOutput::Valid(data), cx);
    let canvas = test_canvas(&state, cx);
    draw(&canvas, cx);
    canvas.update(cx, |canvas, _| {
        let scene = canvas.symbol_scene().expect("the symbol scene is built");
        assert!(!scene.generated);
        assert_eq!(scene.pins.len(), 4);
        assert!(scene.missing.is_empty());
        assert!(scene.placed.texts.iter().any(|text| text.text == "X?"));
        assert!(scene.placed.texts.iter().any(|text| text.text == "pw=2"));
        assert!(canvas.symbol_viewport.scale > 1.);
        canvas.symbol_selection = vec![SymbolSelection::Item(0)];
        assert_eq!(canvas.symbol_selection(), [SymbolSelection::Item(0)]);
        assert!(canvas.selection().is_empty());
    });
}

#[gpui::test]
fn a_cell_without_a_symbol_shows_the_generated_box(cx: &mut TestAppContext) {
    let state = test_state(cx, View::Symbol);
    let source =
        "use std::schematic::Signal;\ncell top() { pub let a = Signal(); pub let vdd = Signal(); }";
    load(
        &state,
        CompileOutput::Valid(compile_valid(source, "top", Vec::new())),
        cx,
    );
    let canvas = test_canvas(&state, cx);
    draw(&canvas, cx);
    canvas.read_with(cx, |canvas, _| {
        let scene = canvas.symbol_scene().expect("the symbol scene is built");
        assert!(scene.generated);
        assert_eq!(scene.pins.len(), 2);
        assert!(scene.placed.texts.iter().any(|text| text.text == "top"));
    });
}

#[gpui::test]
fn descending_follows_instances_across_snapshots(cx: &mut TestAppContext) {
    let state = test_state(cx, View::Schematic);
    load(
        &state,
        CompileOutput::Valid(compile_valid(DEVICE_INVERTER, "parent", Vec::new())),
        cx,
    );
    let canvas = test_canvas(&state, cx);
    draw(&canvas, cx);
    canvas.read_with(cx, |canvas, _| {
        let scene = canvas.scene().expect("the scene is built");
        // The child is drawn with its own symbol.
        assert!(
            scene.elements[0]
                .symbol
                .texts
                .iter()
                .any(|t| t.text == "Xinv")
        );
        assert_eq!(scene.flylines.len(), 2);
    });

    assert!(state.update(cx, |state, cx| state.descend_schematic("Xinv", cx)));
    assert_eq!(top_name(&state, cx), "device_inv");
    state.read_with(cx, |state, _| assert_eq!(state.symbol_inst_name(), "Xinv"));
    cx.run_until_parked();
    canvas.read_with(cx, |canvas, _| {
        let scene = canvas.scene().expect("the scene is rebuilt");
        assert_eq!(scene.elements.len(), 2, "the devices of the inverter");
        assert!(canvas.pending_fit, "a new cell fits on its next paint");
    });

    // A new snapshot from the analyzer keeps the descent, by name.
    load(
        &state,
        CompileOutput::Valid(compile_valid(DEVICE_INVERTER, "parent", Vec::new())),
        cx,
    );
    assert_eq!(top_name(&state, cx), "device_inv");

    // A step that no longer resolves drops back to the cell above it.
    state.update(cx, |state, cx| {
        state
            .root_path
            .push(editor::RootStep::Schematic("gone".into()));
        state.apply_root_path(cx);
        assert_eq!(state.root_path.len(), 1);
        assert!(
            state
                .message
                .as_ref()
                .is_some_and(|m| m.text.contains("gone"))
        );
    });
    assert_eq!(top_name(&state, cx), "device_inv");

    assert!(state.update(cx, |state, cx| state.return_up(cx)));
    assert_eq!(top_name(&state, cx), "parent");
    assert!(!state.update(cx, |state, cx| state.return_up(cx)));
    assert!(!state.update(cx, |state, cx| state.descend_schematic("nobody", cx)));
}

#[gpui::test]
fn empty_and_partial_cells_draw(cx: &mut TestAppContext) {
    let state = test_state(cx, View::Schematic);
    load(
        &state,
        CompileOutput::Valid(compile_valid("cell top() {}", "top", Vec::new())),
        cx,
    );
    let canvas = test_canvas(&state, cx);
    draw(&canvas, cx);
    canvas.read_with(cx, |canvas, _| {
        assert!(canvas.scene().is_some_and(|scene| scene.empty));
        assert_eq!(canvas.viewport.scale, 1., "an empty view keeps its camera");
    });

    let partial = compile(
        "use std::schematic::{DeviceKind, Signal, device, signals};\n\
         cell top() {\n\
             pub let a = Signal(x=0, y=0);\n\
             let r = device(DeviceKind::Res, signals(2), \"\", value=1., x=4, y=0);\n\
             let bad = device(DeviceKind::Res, signals(2), \"\", x=8, y=0);\n\
         }",
        "top",
        Vec::new(),
    );
    assert!(matches!(partial, CompileOutput::ExecErrors(_)));
    load(&state, partial, cx);
    draw(&canvas, cx);
    canvas.read_with(cx, |canvas, _| {
        let scene = canvas.scene().expect("a partial scene");
        assert_eq!(scene.elements.len(), 2);
        assert_eq!(scene.ports.len(), 1);
    });
}

/// Drives the analyzer side of one `place` request over the in-memory
/// transport.
mod editing {
    use analyzer::rpc::{
        BlockingReference, DeleteOutcome, EditReceipt, LangServerRequest, LangServerResponse,
    };
    use argonc::compile::{CellArg, CompileOutput};
    use futures::{FutureExt, SinkExt, StreamExt};
    use gpui::{Modifiers, MouseButton, MouseDownEvent, TestAppContext};

    use super::super::hit::Hit;
    use super::super::paint::Overlay;
    use super::super::scene::tests::{DEVICE_INVERTER, compile, compile_valid};
    use super::super::symbol_tools::HandleKind;
    use super::super::{SymbolDrag, SymbolDragKind, SymbolSelection, SymbolTool, Tool, tools};
    use super::{draw, test_canvas};
    use crate::editor::View;
    use analyzer::rpc::NewSymbolItem;

    /// Clicks the canvas at the world point `at`, with the left button.
    fn click(
        canvas: &gpui::Entity<super::SchematicCanvas>,
        at: (f64, f64),
        click_count: usize,
        cx: &mut TestAppContext,
    ) {
        let window = cx.add_empty_window();
        window.update(|window, cx| {
            canvas.update(cx, |canvas, cx| {
                let position = canvas
                    .viewport
                    .to_px(canvas.screen_bounds, (at.0 as f32, at.1 as f32));
                let event = MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count,
                    first_mouse: false,
                };
                canvas.on_left_mouse_down(&event, window, cx);
            });
        });
    }

    /// Moves the pointer to the world point `at` of the view on screen.
    fn hover(
        canvas: &gpui::Entity<super::SchematicCanvas>,
        at: (f64, f64),
        cx: &mut TestAppContext,
    ) {
        let window = cx.add_empty_window();
        window.update(|window, cx| {
            canvas.update(cx, |canvas, cx| {
                let position = canvas
                    .current_viewport(cx)
                    .to_px(canvas.screen_bounds, (at.0 as f32, at.1 as f32));
                let event = gpui::MouseMoveEvent {
                    position,
                    pressed_button: None,
                    modifiers: Modifiers::default(),
                };
                canvas.on_mouse_move(&event, window, cx);
            });
        });
    }

    /// Releases the left button.
    fn release(canvas: &gpui::Entity<super::SchematicCanvas>, cx: &mut TestAppContext) {
        let window = cx.add_empty_window();
        window.update(|window, cx| {
            canvas.update(cx, |canvas, cx| {
                let event = gpui::MouseUpEvent {
                    button: MouseButton::Left,
                    position: gpui::Point::default(),
                    modifiers: Modifiers::default(),
                    click_count: 1,
                };
                canvas.on_left_mouse_up(&event, window, cx);
            });
        });
    }

    fn reply(
        server: &mut crate::rpc::TestLangServerTransport,
        request_id: u64,
        message: LangServerResponse,
        cx: &mut TestAppContext,
    ) {
        server
            .send(tarpc::Response {
                request_id,
                message: Ok(message),
            })
            .now_or_never()
            .unwrap()
            .unwrap();
        cx.run_until_parked();
    }

    /// A state whose language server client talks to the test.
    fn rpc_state(
        cx: &mut TestAppContext,
    ) -> (
        gpui::Entity<crate::editor::EditorState>,
        crate::rpc::TestLangServerTransport,
    ) {
        let state = super::test_state(cx, View::Schematic);
        let (client, server) = crate::rpc::SyncLangServerClient::for_rpc_test(cx.to_async());
        state.update(cx, |state, _| state.lang_server_client = client);
        (state, server)
    }

    fn load_inverter(
        state: &gpui::Entity<crate::editor::EditorState>,
        revision: u64,
        cx: &mut TestAppContext,
    ) {
        let data = compile_valid(
            DEVICE_INVERTER,
            "device_inv",
            vec![CellArg::Float(2.), CellArg::Float(1.)],
        );
        state.update(cx, |state, _| {
            state.compilation_revision = Some(revision);
        });
        super::load(state, CompileOutput::Valid(data), cx);
    }

    /// The next request of a gesture. The symbol view's fetch of the cell's
    /// parameters is answered on the way, with none.
    fn next_request(
        server: &mut crate::rpc::TestLangServerTransport,
        cx: &mut TestAppContext,
    ) -> tarpc::Request<LangServerRequest> {
        loop {
            if let Some(message) = server.next().now_or_never()
                && let tarpc::ClientMessage::Request(request) = message.unwrap().unwrap()
            {
                if matches!(request.message, LangServerRequest::CellParameters { .. }) {
                    server
                        .send(tarpc::Response {
                            request_id: request.id,
                            message: Ok(LangServerResponse::CellParameters(Some(Vec::new()))),
                        })
                        .now_or_never()
                        .unwrap()
                        .unwrap();
                    continue;
                }
                return request;
            }
            assert!(cx.dispatcher.tick(false));
        }
    }

    #[gpui::test]
    fn a_move_is_one_place_request_and_stays_until_the_next_snapshot(cx: &mut TestAppContext) {
        let (state, mut server) = rpc_state(cx);
        load_inverter(&state, 7, cx);
        let canvas = test_canvas(&state, cx);
        draw(&canvas, cx);

        // Drag `mp` two units right.
        canvas.update(cx, |canvas, cx| {
            canvas.selection = vec![Hit::Element(0)];
            let scene = canvas.scene().unwrap().clone();
            canvas.selection_spans = vec![scene.elements[0].span.clone().unwrap()];
            let overrides = tools::move_overrides(&scene, &[Hit::Element(0)], (2, 0));
            canvas.commit(overrides, cx);
            assert!(canvas.edit_in_flight());
            assert!(canvas.has_pending_edit());
            // The preview already shows the device at its new place.
            assert_eq!(
                canvas.scene().unwrap().elements[0].placement.position,
                Some((2, 4))
            );
        });
        let request = next_request(&mut server, cx);
        let LangServerRequest::Place { request: place } = request.message else {
            panic!("expected a place request");
        };
        assert_eq!(place.base_revision, 7);
        // The device and its four wires.
        assert_eq!(place.edits.len(), 5);
        let device = &place.edits[0];
        let keywords = device
            .keywords
            .iter()
            .map(|k| (k.name.as_str(), k.value.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(
            keywords,
            [
                ("x", Some("2")),
                ("y", Some("4")),
                ("angle", None),
                ("reflect", None)
            ]
        );
        assert!(
            place.edits[1..]
                .iter()
                .all(|edit| edit.keywords[0].name == "route")
        );

        // Nothing else may start while the edit is in flight.
        canvas.update(cx, |canvas, cx| {
            let scene = canvas.scene().unwrap().clone();
            let overrides = tools::move_overrides(&scene, &[Hit::Element(1)], (0, 1));
            canvas.commit(overrides, cx);
        });
        assert!(server.next().now_or_never().flatten().is_none());

        server
            .send(tarpc::Response {
                request_id: request.id,
                message: Ok(LangServerResponse::Place(Some(EditReceipt {
                    revision: 8,
                    spans: place.edits.iter().map(|edit| edit.call.clone()).collect(),
                }))),
            })
            .now_or_never()
            .unwrap()
            .unwrap();
        while canvas.read_with(cx, |canvas, _| canvas.edit_in_flight()) {
            assert!(cx.dispatcher.tick(false));
        }
        canvas.read_with(cx, |canvas, _| {
            assert!(
                canvas.has_pending_edit(),
                "drawn until the recompiled result arrives"
            );
        });

        // The analyzer would refuse an edit drawn on the old snapshot, so the
        // next one waits for the recompiled result.
        let move_other = |canvas: &mut super::SchematicCanvas, cx: &mut gpui::Context<_>| {
            let scene = canvas.scene().unwrap().clone();
            let overrides = tools::move_overrides(&scene, &[Hit::Element(1)], (0, 1));
            canvas.commit(overrides, cx);
        };
        canvas.update(cx, move_other);
        cx.run_until_parked();
        assert!(server.next().now_or_never().flatten().is_none());
        canvas.read_with(cx, |canvas, _| {
            assert!(canvas.has_pending_edit(), "the first edit stays drawn");
        });

        // The snapshot compiled from the edit retires the overlay.
        load_inverter(&state, 8, cx);
        cx.run_until_parked();
        canvas.read_with(cx, |canvas, _| {
            assert!(!canvas.has_pending_edit());
            assert_eq!(
                canvas.scene().unwrap().elements[0].placement.position,
                Some((0, 4)),
                "the test snapshot has the unedited source"
            );
        });
        canvas.update(cx, move_other);
        let request = next_request(&mut server, cx);
        let LangServerRequest::Place { request: place } = request.message else {
            panic!("expected a place request");
        };
        assert_eq!(place.base_revision, 8);
    }

    #[gpui::test]
    fn a_snapshot_ahead_of_the_receipt_does_not_hold_up_the_next_edit(cx: &mut TestAppContext) {
        let (state, mut server) = rpc_state(cx);
        load_inverter(&state, 7, cx);
        let canvas = test_canvas(&state, cx);
        draw(&canvas, cx);
        let move_element = |element: usize| {
            move |canvas: &mut super::SchematicCanvas, cx: &mut gpui::Context<_>| {
                let scene = canvas.scene().unwrap().clone();
                let overrides = tools::move_overrides(&scene, &[Hit::Element(element)], (2, 0));
                canvas.commit(overrides, cx);
            }
        };
        canvas.update(cx, move_element(0));
        let request = next_request(&mut server, cx);
        // The recompiled result arrives before the edit's receipt.
        load_inverter(&state, 8, cx);
        reply(
            &mut server,
            request.id,
            LangServerResponse::Place(Some(EditReceipt {
                revision: 8,
                spans: Vec::new(),
            })),
            cx,
        );
        canvas.read_with(cx, |canvas, _| {
            assert!(!canvas.edit_in_flight());
            assert!(!canvas.has_pending_edit());
        });
        canvas.update(cx, move_element(1));
        let request = next_request(&mut server, cx);
        assert!(matches!(request.message, LangServerRequest::Place { .. }));
    }

    #[gpui::test]
    fn a_dragged_device_follows_the_pointer(cx: &mut TestAppContext) {
        let (state, mut server) = rpc_state(cx);
        load_inverter(&state, 7, cx);
        let canvas = test_canvas(&state, cx);
        draw(&canvas, cx);
        // Grab `mp` off the grid.
        let (grab, (x, y)) = canvas.update(cx, |canvas, _| {
            let scene = canvas.scene().unwrap().clone();
            let (x, y) = scene.elements[0].placement.position.unwrap();
            let grab = (x as f64 + 0.3, y as f64 + 0.3);
            canvas.selection = vec![Hit::Element(0)];
            canvas.selection_spans = vec![scene.elements[0].span.clone().unwrap()];
            canvas.drag = Some(tools::Drag {
                kind: tools::DragKind::Items,
                start: grab,
                current: grab,
                base: scene,
                selection: vec![Hit::Element(0)],
            });
            (grab, (x, y))
        });
        // The device moves by the pointer's offset, rounded to whole units.
        for (dx, moved) in [(0.4, 0), (1., 1), (2.4, 2), (3., 3), (1.6, 2)] {
            hover(&canvas, (grab.0 + dx, grab.1), cx);
            canvas.read_with(cx, |canvas, _| {
                assert_eq!(
                    canvas.scene().unwrap().elements[0].placement.position,
                    Some((x + moved, y))
                );
            });
        }
        release(&canvas, cx);
        let request = next_request(&mut server, cx);
        let LangServerRequest::Place { request: place } = request.message else {
            panic!("expected a place request");
        };
        let x = (x + 2).to_string();
        let y = y.to_string();
        let keywords = place.edits[0]
            .keywords
            .iter()
            .map(|k| (k.name.as_str(), k.value.as_deref()))
            .collect::<Vec<_>>();
        assert_eq!(
            keywords[..2],
            [("x", Some(x.as_str())), ("y", Some(y.as_str()))]
        );
    }

    #[gpui::test]
    fn the_wire_tool_writes_a_connect_between_two_pins(cx: &mut TestAppContext) {
        let (state, mut server) = rpc_state(cx);
        load_inverter(&state, 5, cx);
        let canvas = test_canvas(&state, cx);
        draw(&canvas, cx);
        let (out, drain) = canvas.read_with(cx, |canvas, _| {
            let scene = canvas.scene().unwrap();
            let out = scene.ports.iter().find(|port| port.name == "out").unwrap();
            let mp = scene.elements.iter().find(|e| e.name == "Mmp").unwrap();
            let drain = mp.pins.iter().find(|pin| pin.name == "d").unwrap();
            (out.position, drain.position.unwrap())
        });
        canvas.update(cx, |canvas, cx| {
            canvas.set_tool(
                Tool::Wire {
                    draft: None,
                    flipped: false,
                },
                cx,
            );
        });
        // Start at `out`, add a corner below it, and end at the drain.
        click(&canvas, (out.0 as f64, out.1 as f64), 1, cx);
        canvas.read_with(cx, |canvas, _| {
            assert!(matches!(&canvas.tool, Tool::Wire { draft: Some(draft), .. } if draft.start.path == "out"));
        });
        click(&canvas, (out.0 as f64 + 2., out.1 as f64 - 3.), 1, cx);
        hover(&canvas, (drain.0 as f64, drain.1 as f64), cx);
        canvas.read_with(cx, |canvas, _| {
            let preview = canvas.tool_preview().expect("the draft is previewed");
            assert!(matches!(preview, Overlay::Wire(points) if points.len() >= 3));
        });
        click(&canvas, (drain.0 as f64, drain.1 as f64), 1, cx);
        let request = next_request(&mut server, cx);
        let LangServerRequest::Connect { request: connect } = request.message else {
            panic!("expected a connect request, got {:?}", request.message);
        };
        assert_eq!(connect.target.base_revision, 5);
        assert_eq!(connect.a, "out");
        assert_eq!(connect.b, "mp.d");
        let route = connect.route.clone().unwrap();
        let mut points = vec![out];
        points.extend(route.iter().copied());
        points.push(drain);
        for pair in points.windows(2) {
            assert!(
                pair[0].0 == pair[1].0 || pair[0].1 == pair[1].1,
                "{points:?}"
            );
        }
        canvas.read_with(cx, |canvas, _| {
            assert!(canvas.edit_in_flight());
            assert!(
                matches!(&canvas.tool, Tool::Wire { draft: None, .. }),
                "the tool stays"
            );
        });
        reply(
            &mut server,
            request.id,
            LangServerResponse::Connect(Some(EditReceipt {
                revision: 6,
                spans: Vec::new(),
            })),
            cx,
        );
        canvas.read_with(cx, |canvas, _| {
            assert!(!canvas.edit_in_flight());
            assert!(
                canvas.has_pending_edit(),
                "the ghost wire stays until the snapshot"
            );
            assert!(matches!(
                canvas.pending.as_ref().unwrap().ghosts.first(),
                Some(Overlay::Wire(_))
            ));
        });
        load_inverter(&state, 6, cx);
        cx.run_until_parked();
        canvas.read_with(cx, |canvas, _| assert!(!canvas.has_pending_edit()));
    }

    #[gpui::test]
    fn wiring_a_flyline_routes_the_connect_that_made_it(cx: &mut TestAppContext) {
        let (state, mut server) = rpc_state(cx);
        let data = compile_valid(
            "use std::schematic::{Signal, DeviceKind, connect, device, signals};
             cell top() {
                 pub let a = Signal(x=-6, y=0);
                 let r = device(DeviceKind::Res, signals(2), \"\", value=1., x=0, y=4);
                 connect(a, r.p);
             }",
            "top",
            vec![],
        );
        state.update(cx, |state, _| state.compilation_revision = Some(2));
        super::load(&state, CompileOutput::Valid(data), cx);
        let canvas = test_canvas(&state, cx);
        draw(&canvas, cx);
        let (from, to) = canvas.read_with(cx, |canvas, _| {
            let scene = canvas.scene().unwrap();
            assert_eq!(scene.flylines.len(), 1);
            (scene.flylines[0].from, scene.flylines[0].to)
        });
        canvas.update(cx, |canvas, cx| {
            canvas.set_tool(
                Tool::Wire {
                    draft: None,
                    flipped: false,
                },
                cx,
            );
        });
        let middle = ((from.0 + to.0) / 2., (from.1 + to.1) / 2.);
        hover(&canvas, middle, cx);
        canvas.read_with(cx, |canvas, _| {
            assert_eq!(canvas.hover, Some(Hit::Flyline(0)));
            assert!(
                matches!(canvas.tool_preview(), Some(Overlay::Wire(points)) if points.len() == 3)
            );
        });
        click(&canvas, middle, 1, cx);
        let request = next_request(&mut server, cx);
        let LangServerRequest::Place { request: place } = request.message else {
            panic!(
                "an existing connect gets a route, got {:?}",
                request.message
            );
        };
        assert_eq!(place.base_revision, 2);
        assert_eq!(place.edits.len(), 1);
        assert_eq!(place.edits[0].keywords[0].name, "route");
        assert!(place.edits[0].keywords[0].value.is_some());
    }

    #[gpui::test]
    fn the_port_tool_connects_the_port_to_the_pin_it_is_placed_on(cx: &mut TestAppContext) {
        let (state, mut server) = rpc_state(cx);
        load_inverter(&state, 3, cx);
        let canvas = test_canvas(&state, cx);
        draw(&canvas, cx);
        let gate = canvas.read_with(cx, |canvas, _| {
            let scene = canvas.scene().unwrap();
            let mn = scene.elements.iter().find(|e| e.name == "Mmn").unwrap();
            mn.pins
                .iter()
                .find(|pin| pin.name == "g")
                .unwrap()
                .position
                .unwrap()
        });
        canvas.update(cx, |canvas, cx| {
            canvas.set_tool(
                Tool::Port {
                    name: Some("en".into()),
                },
                cx,
            );
        });
        click(&canvas, (gate.0 as f64, gate.1 as f64), 1, cx);
        let request = next_request(&mut server, cx);
        let LangServerRequest::AddSignal { request: signal } = request.message else {
            panic!("expected an add_signal request, got {:?}", request.message);
        };
        assert_eq!(signal.name, "en");
        assert!(signal.port);
        assert_eq!(signal.at, gate);
        assert_eq!(signal.connect_to.as_deref(), Some("mn.g"));
        assert_eq!(signal.route, None);
        canvas.read_with(cx, |canvas, _| {
            assert!(
                matches!(canvas.tool, Tool::Select),
                "placing a port ends the tool"
            );
        });
    }

    #[gpui::test]
    fn a_blocked_deletion_lists_its_references(cx: &mut TestAppContext) {
        let (state, mut server) = rpc_state(cx);
        load_inverter(&state, 4, cx);
        let canvas = test_canvas(&state, cx);
        draw(&canvas, cx);
        canvas.update(cx, |canvas, cx| {
            let scene = canvas.scene().unwrap().clone();
            canvas.selection = vec![Hit::Element(0)];
            canvas.selection_spans = vec![scene.elements[0].span.clone().unwrap()];
            canvas.delete_selection(cx);
        });
        let request = next_request(&mut server, cx);
        let LangServerRequest::Delete { request: delete } = request.message else {
            panic!("expected a delete request, got {:?}", request.message);
        };
        assert_eq!(delete.base_revision, 4);
        assert_eq!(delete.targets.len(), 1);
        let reference = BlockingReference {
            span: delete.targets[0].clone(),
            line: 3,
            text: "let alias = mp;".into(),
        };
        reply(
            &mut server,
            request.id,
            LangServerResponse::Delete(Some(DeleteOutcome::Blocked(vec![reference.clone()]))),
            cx,
        );
        canvas.read_with(cx, |canvas, _| {
            assert!(!canvas.edit_in_flight());
            assert_eq!(canvas.blocked_delete(), [reference]);
            assert_eq!(
                canvas.selection().len(),
                1,
                "a blocked deletion keeps the selection"
            );
        });
    }

    /// A state in the symbol view whose language server client talks to the
    /// test, with the device inverter loaded at `revision`.
    fn symbol_state(
        revision: u64,
        cx: &mut TestAppContext,
    ) -> (
        gpui::Entity<crate::editor::EditorState>,
        crate::rpc::TestLangServerTransport,
    ) {
        let state = super::test_state(cx, View::Symbol);
        let (client, server) = crate::rpc::SyncLangServerClient::for_rpc_test(cx.to_async());
        state.update(cx, |state, _| state.lang_server_client = client);
        load_inverter(&state, revision, cx);
        (state, server)
    }

    /// Clicks the symbol view at `at`, with the viewport of the symbol scene.
    fn symbol_click(
        canvas: &gpui::Entity<super::SchematicCanvas>,
        at: (f64, f64),
        click_count: usize,
        cx: &mut TestAppContext,
    ) {
        let window = cx.add_empty_window();
        window.update(|window, cx| {
            canvas.update(cx, |canvas, cx| {
                let position = canvas
                    .symbol_viewport
                    .to_px(canvas.screen_bounds, (at.0 as f32, at.1 as f32));
                let event = MouseDownEvent {
                    button: MouseButton::Left,
                    position,
                    modifiers: Modifiers::default(),
                    click_count,
                    first_mouse: false,
                };
                canvas.on_left_mouse_down(&event, window, cx);
            });
        });
    }

    #[gpui::test]
    fn the_line_tool_writes_a_line_through_its_clicks(cx: &mut TestAppContext) {
        let (state, mut server) = symbol_state(9, cx);
        let canvas = test_canvas(&state, cx);
        draw(&canvas, cx);
        canvas.update(cx, |canvas, cx| {
            canvas.in_symbol_view = true;
            canvas.set_symbol_tool(SymbolTool::Line { points: Vec::new() }, cx);
        });
        symbol_click(&canvas, (-4.1, 2.9), 1, cx);
        symbol_click(&canvas, (-2.0, 3.0), 1, cx);
        canvas.read_with(cx, |canvas, _| {
            assert!(
                matches!(&canvas.symbol_tool, SymbolTool::Line { points } if points.len() == 2)
            );
            assert!(
                matches!(
                    canvas.symbol_tool_preview().first(),
                    Some(Overlay::Shape(_))
                ) || canvas.cursor_world.is_none()
            );
        });
        // A double-click ends the line at the last click.
        symbol_click(&canvas, (-2.0, 3.0), 2, cx);
        let request = next_request(&mut server, cx);
        let LangServerRequest::AddSymbolItems { request: add } = request.message else {
            panic!(
                "expected an add_symbol_items request, got {:?}",
                request.message
            );
        };
        assert_eq!(add.target.base_revision, 9);
        assert_eq!(
            add.items,
            [NewSymbolItem::Line {
                points: vec![(-4., 3.), (-2., 3.)]
            }],
            "clicks snap to quarter units"
        );
        canvas.read_with(cx, |canvas, _| {
            assert!(canvas.edit_in_flight());
            assert!(
                matches!(&canvas.symbol_tool, SymbolTool::Line { points } if points.is_empty())
            );
        });
        reply(
            &mut server,
            request.id,
            LangServerResponse::AddSymbolItems(Some(EditReceipt {
                revision: 10,
                spans: Vec::new(),
            })),
            cx,
        );
        canvas.read_with(cx, |canvas, _| {
            assert!(
                canvas.has_pending_edit(),
                "the ghost line stays until the snapshot"
            );
        });
    }

    #[gpui::test]
    fn the_pin_tool_places_labeled_pins_on_ports_without_one(cx: &mut TestAppContext) {
        let (state, mut server) = symbol_state(2, cx);
        // A cell whose symbol lacks a pin for `out`, which is an error the
        // partial result still draws.
        let output = compile(
            "use std::schematic::Signal;
             use std::symbol::{pin, rect};
             cell half() {
                 pub let a = Signal();
                 pub let out = Signal();
                 rect(x0=-2., y0=-2., x1=2., y1=2.);
                 pin(a, x=-4, y=0);
             }",
            "half",
            vec![],
        );
        assert!(matches!(output, CompileOutput::ExecErrors(_)));
        state.update(cx, |state, _| state.compilation_revision = Some(2));
        super::load(&state, output, cx);
        let canvas = test_canvas(&state, cx);
        draw(&canvas, cx);
        let window = cx.add_empty_window();
        window.update(|window, cx| {
            canvas.update(cx, |canvas, cx| {
                canvas.in_symbol_view = true;
                assert_eq!(canvas.symbol_scene().unwrap().missing, ["out"]);
                canvas.symbol_pin(&crate::actions::SymbolPin, window, cx);
            });
        });
        canvas.read_with(cx, |canvas, _| {
            assert!(matches!(
                canvas.symbol_tool,
                SymbolTool::Pin { port: Some(1), .. }
            ));
        });
        symbol_click(&canvas, (3.8, 0.2), 1, cx);
        let request = next_request(&mut server, cx);
        let LangServerRequest::AddSymbolItems { request: add } = request.message else {
            panic!(
                "expected an add_symbol_items request, got {:?}",
                request.message
            );
        };
        assert_eq!(
            add.items,
            [NewSymbolItem::Pin {
                port: 1,
                x: 4,
                y: 0,
                label: true
            }],
            "pins snap to the grid"
        );
        canvas.read_with(cx, |canvas, _| {
            assert!(
                matches!(canvas.symbol_tool, SymbolTool::Select),
                "every port has a pin now, so the tool ends"
            );
        });
    }

    #[gpui::test]
    fn create_symbol_asks_for_the_generated_box(cx: &mut TestAppContext) {
        let (state, mut server) = symbol_state(4, cx);
        let data = compile_valid(
            "use std::schematic::Signal;\ncell plain() { pub let a = Signal(); pub let vdd = Signal(); }",
            "plain",
            vec![],
        );
        state.update(cx, |state, _| state.compilation_revision = Some(4));
        super::load(&state, CompileOutput::Valid(data), cx);
        let canvas = test_canvas(&state, cx);
        draw(&canvas, cx);
        let window = cx.add_empty_window();
        window.update(|window, cx| {
            canvas.update(cx, |canvas, cx| {
                canvas.in_symbol_view = true;
                assert!(canvas.symbol_scene().unwrap().generated);
                // Drawing tools wait for Create Symbol.
                canvas.symbol_line(&crate::actions::SymbolLine, window, cx);
                assert!(matches!(canvas.symbol_tool, SymbolTool::Select));
                canvas.create_symbol(cx);
            });
        });
        let request = next_request(&mut server, cx);
        let LangServerRequest::CreateSymbol { request: create } = request.message else {
            panic!(
                "expected a create_symbol request, got {:?}",
                request.message
            );
        };
        assert_eq!(create.target.base_revision, 4);
    }

    #[gpui::test]
    fn dragging_a_handle_reshapes_the_item_and_writes_its_coordinates(cx: &mut TestAppContext) {
        let (state, mut server) = symbol_state(6, cx);
        let canvas = test_canvas(&state, cx);
        draw(&canvas, cx);
        // The inverter's input stub: `line([(-4., 0.), (-2., 0.)])`.
        let line = canvas.read_with(cx, |canvas, _| {
            let scene = canvas.symbol_scene().unwrap();
            scene
                .items
                .iter()
                .position(|item| {
                    matches!(&item.kind, argonc::compile::SymbolItemKind::Line { points } if points[0] == (-4., 0.))
                })
                .unwrap()
        });
        canvas.update(cx, |canvas, _| {
            canvas.in_symbol_view = true;
            canvas.symbol_selection = vec![SymbolSelection::Item(line)];
            assert_eq!(canvas.selection_handles().len(), 2);
            canvas.symbol_drag = Some(SymbolDrag {
                kind: SymbolDragKind::Handle {
                    item: line,
                    handle: HandleKind::Point(0),
                },
                start: (-4., 0.),
                current: (-4., 0.),
                base: canvas.symbol_scene().unwrap().clone(),
                selection: canvas.symbol_selection.clone(),
            });
        });
        let window = cx.add_empty_window();
        window.update(|window, cx| {
            canvas.update(cx, |canvas, cx| {
                let position = canvas
                    .symbol_viewport
                    .to_px(canvas.screen_bounds, (-5., 0.5));
                canvas.on_mouse_move(
                    &gpui::MouseMoveEvent {
                        position,
                        pressed_button: Some(MouseButton::Left),
                        modifiers: Modifiers::default(),
                    },
                    window,
                    cx,
                );
                // The preview already shows the moved point.
                let scene = canvas.symbol_scene().unwrap();
                assert!(matches!(&scene.items[line].kind, argonc::compile::SymbolItemKind::Line { points } if points[0] == (-5., 0.5)));
                canvas.on_left_mouse_up(
                    &gpui::MouseUpEvent {
                        button: MouseButton::Left,
                        position,
                        modifiers: Modifiers::default(),
                        click_count: 1,
                    },
                    window,
                    cx,
                );
            });
        });
        let request = next_request(&mut server, cx);
        let LangServerRequest::Place { request: place } = request.message else {
            panic!("expected a place request, got {:?}", request.message);
        };
        assert_eq!(place.base_revision, 6);
        assert_eq!(place.edits.len(), 1);
        assert_eq!(place.edits[0].keywords[0].name, "points");
        assert_eq!(
            place.edits[0].keywords[0].value.as_deref(),
            Some("[(-5., 0.5), (-2., 0.)]")
        );
    }

    #[gpui::test]
    fn a_dragged_symbol_item_follows_the_pointer(cx: &mut TestAppContext) {
        let (state, mut server) = symbol_state(6, cx);
        let canvas = test_canvas(&state, cx);
        draw(&canvas, cx);
        // The inverter's input stub: `line([(-4., 0.), (-2., 0.)])`.
        let line = canvas.read_with(cx, |canvas, _| {
            let scene = canvas.symbol_scene().unwrap();
            scene
                .items
                .iter()
                .position(|item| {
                    matches!(&item.kind, argonc::compile::SymbolItemKind::Line { points } if points[0] == (-4., 0.))
                })
                .unwrap()
        });
        let grab = |canvas: &mut super::SchematicCanvas| {
            canvas.in_symbol_view = true;
            canvas.symbol_selection = vec![SymbolSelection::Item(line)];
            canvas.symbol_drag = Some(SymbolDrag {
                kind: SymbolDragKind::Move,
                start: (-3., 0.),
                current: (-3., 0.),
                base: canvas.symbol_scene().unwrap().clone(),
                selection: canvas.symbol_selection.clone(),
            });
        };
        // A click alone edits nothing.
        canvas.update(cx, |canvas, _| grab(canvas));
        release(&canvas, cx);
        canvas.read_with(cx, |canvas, _| assert!(!canvas.edit_in_flight()));
        canvas.update(cx, |canvas, _| grab(canvas));
        // The line moves by the pointer's offset, snapped to quarter units.
        for (dx, moved) in [(0.5, 0.5), (1., 1.), (1.6, 1.5)] {
            hover(&canvas, (-3. + dx, 0.), cx);
            canvas.read_with(cx, |canvas, _| {
                let scene = canvas.symbol_scene().unwrap();
                assert!(matches!(
                    &scene.items[line].kind,
                    argonc::compile::SymbolItemKind::Line { points }
                        if points[..] == [(-4. + moved, 0.), (-2. + moved, 0.)]
                ));
            });
        }
        release(&canvas, cx);
        let request = next_request(&mut server, cx);
        let LangServerRequest::Place { request: place } = request.message else {
            panic!("expected a place request, got {:?}", request.message);
        };
        assert_eq!(place.edits.len(), 1);
        assert_eq!(
            place.edits[0].keywords[0].value.as_deref(),
            Some("[(-2.5, 0.), (-0.5, 0.)]")
        );
    }

    #[gpui::test]
    fn a_refused_edit_drops_its_overlay_at_once(cx: &mut TestAppContext) {
        let (state, mut server) = rpc_state(cx);
        load_inverter(&state, 3, cx);
        let canvas = test_canvas(&state, cx);
        draw(&canvas, cx);
        canvas.update(cx, |canvas, cx| {
            let scene = canvas.scene().unwrap().clone();
            let overrides = tools::orient_overrides(&scene, &[Hit::Element(1)], false);
            canvas.commit(overrides, cx);
            assert_eq!(
                canvas.scene().unwrap().elements[1].placement.angle,
                geometry::transform::Rotation::R90
            );
        });
        let request = next_request(&mut server, cx);
        assert!(matches!(request.message, LangServerRequest::Place { .. }));
        server
            .send(tarpc::Response {
                request_id: request.id,
                message: Ok(LangServerResponse::Place(None)),
            })
            .now_or_never()
            .unwrap()
            .unwrap();
        while canvas.read_with(cx, |canvas, _| canvas.edit_in_flight()) {
            assert!(cx.dispatcher.tick(false));
        }
        canvas.read_with(cx, |canvas, _| {
            assert!(!canvas.has_pending_edit());
            assert_eq!(
                canvas.scene().unwrap().elements[1].placement.angle,
                geometry::transform::Rotation::R0
            );
        });
    }
}
