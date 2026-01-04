pub mod ast;
pub mod compile;
pub mod config;
pub mod gds;
pub mod layer;
pub mod parse;
pub mod solver;

#[cfg(test)]
mod tests {

    use std::path::PathBuf;

    use crate::{
        compile::{ExecErrorKind, SolvedValue},
        gds::GdsMap,
        parse::parse_workspace_with_std,
    };
    use approx::assert_relative_eq;
    use const_format::concatcp;

    use crate::compile::{CellArg, CompileInput, compile};
    const EPSILON: f64 = 1e-10;

    const EXAMPLES_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../examples");
    const ARGON_SCOPES: &str = concatcp!(EXAMPLES_DIR, "/scopes/lib.ar");
    const BASIC_LYP: &str = concatcp!(EXAMPLES_DIR, "/lyp/basic.lyp");
    const ARGON_SKY130_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../pdks/sky130");
    const ARGON_SKY130_LIB: &str = concatcp!(ARGON_SKY130_DIR, "/lib.ar");
    const SKY130_LYP: &str = concatcp!(ARGON_SKY130_DIR, "/sky130.lyp");
    const ARGON_IMMEDIATE: &str = concatcp!(EXAMPLES_DIR, "/immediate/lib.ar");
    const ARGON_IF: &str = concatcp!(EXAMPLES_DIR, "/if/lib.ar");
    const ARGON_IF_INCONSISTENT: &str = concatcp!(EXAMPLES_DIR, "/if_inconsistent/lib.ar");
    const ARGON_VIA: &str = concatcp!(EXAMPLES_DIR, "/via/lib.ar");
    const ARGON_VIA_ARRAY: &str = concatcp!(EXAMPLES_DIR, "/via_array/lib.ar");
    const ARGON_FUNC_OUT_OF_ORDER: &str = concatcp!(EXAMPLES_DIR, "/func_out_of_order/lib.ar");
    const ARGON_HIERARCHY: &str = concatcp!(EXAMPLES_DIR, "/hierarchy/lib.ar");
    const ARGON_NESTED_INST: &str = concatcp!(EXAMPLES_DIR, "/nested_inst/lib.ar");
    const ARGON_CELL_OUT_OF_ORDER: &str = concatcp!(EXAMPLES_DIR, "/cell_out_of_order/lib.ar");
    const ARGON_FALLBACK_BASIC: &str = concatcp!(EXAMPLES_DIR, "/fallback_basic/lib.ar");
    const ARGON_FALLBACK_INST: &str = concatcp!(EXAMPLES_DIR, "/fallback_inst/lib.ar");
    const ARGON_BOOL_LITERAL: &str = concatcp!(EXAMPLES_DIR, "/bool_literal/lib.ar");
    const ARGON_DIMENSIONS: &str = concatcp!(EXAMPLES_DIR, "/dimensions/lib.ar");
    const ARGON_PARAM_FLOAT: &str = concatcp!(EXAMPLES_DIR, "/param_float/lib.ar");
    const ARGON_PARAM_INT: &str = concatcp!(EXAMPLES_DIR, "/param_int/lib.ar");
    const ARGON_ENUMERATIONS: &str = concatcp!(EXAMPLES_DIR, "/enumerations/lib.ar");
    const ARGON_BBOX: &str = concatcp!(EXAMPLES_DIR, "/bbox/lib.ar");
    const ARGON_ROUNDING: &str = concatcp!(EXAMPLES_DIR, "/rounding/lib.ar");
    const ARGON_FLIPPED_RECT: &str = concatcp!(EXAMPLES_DIR, "/flipped_rect/lib.ar");
    const ARGON_SEQ_BASIC: &str = concatcp!(EXAMPLES_DIR, "/seq_basic/lib.ar");
    const ARGON_SEQ_FN: &str = concatcp!(EXAMPLES_DIR, "/seq_fn/lib.ar");
    const ARGON_SEQ_RECUR: &str = concatcp!(EXAMPLES_DIR, "/seq_recur/lib.ar");
    const ARGON_LUB_MATCH: &str = concatcp!(EXAMPLES_DIR, "/lub_match/lib.ar");
    const ARGON_SEQ_CELL: &str = concatcp!(EXAMPLES_DIR, "/seq_cell/lib.ar");
    const ARGON_WORKSPACE: &str = concatcp!(EXAMPLES_DIR, "/argon_workspace/lib.ar");
    const ARGON_EXTERNAL_MODS: &str = concatcp!(EXAMPLES_DIR, "/external_mods/main_crate/lib.ar");
    const ARGON_TEXT: &str = concatcp!(EXAMPLES_DIR, "/text/lib.ar");
    const ARGON_ANY_TYPE_INST: &str = concatcp!(EXAMPLES_DIR, "/any_type_inst/lib.ar");

    #[test]
    fn argon_scopes() {
        let o = parse_workspace_with_std(ARGON_SCOPES);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["scopes"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_immediate() {
        let o = parse_workspace_with_std(ARGON_IMMEDIATE);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["immediate"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_if() {
        let o = parse_workspace_with_std(ARGON_IF);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["if_test"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_if_inconsistent() {
        let o = parse_workspace_with_std(ARGON_IF_INCONSISTENT);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["if_test"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
        cell.unwrap_exec_errors();
    }

    #[test]
    fn argon_via() {
        let o = parse_workspace_with_std(ARGON_VIA);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["via"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_via_array() {
        let o = parse_workspace_with_std(ARGON_VIA_ARRAY);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["vias"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
        let cell = cell.unwrap_valid();
        let cell = &cell.cells[&cell.top];
        let n_rects = cell
            .objects
            .iter()
            .filter(|(_, o)| {
                if let SolvedValue::Rect(r) = &o {
                    !r.construction
                } else {
                    false
                }
            })
            .count();
        assert_eq!(n_rects, 27);
    }

    #[test]
    fn argon_func_out_of_order() {
        let o = parse_workspace_with_std(ARGON_FUNC_OUT_OF_ORDER);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["test"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_hierarchy() {
        let o = parse_workspace_with_std(ARGON_HIERARCHY);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
    }

    #[test]
    fn argon_nested_inst() {
        let o = parse_workspace_with_std(ARGON_NESTED_INST);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
    }

    #[test]
    #[ignore = "not supported"]
    fn argon_cell_out_of_order() {
        let o = parse_workspace_with_std(ARGON_CELL_OUT_OF_ORDER);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
    }

    #[test]
    fn argon_fallback_basic() {
        let o = parse_workspace_with_std(ARGON_FALLBACK_BASIC);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        )
        .unwrap_exec_errors()
        .output
        .unwrap();
        println!("{cells:#?}");
        assert!(!cells.cells[&cells.top].fallback_constraints_used.is_empty());
    }

    #[test]
    fn argon_fallback_inst() {
        let o = parse_workspace_with_std(ARGON_FALLBACK_INST);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        )
        .unwrap_exec_errors()
        .output
        .unwrap();
        assert!(!cells.cells[&cells.top].fallback_constraints_used.is_empty());
        println!("{cells:#?}");
    }

    #[test]
    fn argon_bool_literal() {
        let o = parse_workspace_with_std(ARGON_BOOL_LITERAL);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        let emit = cell.scopes[&cell.root]
            .children
            .iter()
            .flat_map(|s| cell.scopes[s].emit.iter())
            .collect::<Vec<_>>();
        assert_eq!(emit.len(), 1);
        let (obj, _) = emit.first().unwrap();
        assert_eq!(
            cell.objects[obj]
                .as_ref()
                .unwrap_rect()
                .layer
                .as_ref()
                .unwrap(),
            "met1"
        );
    }

    #[test]
    fn argon_dimensions() {
        let o = parse_workspace_with_std(ARGON_DIMENSIONS);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 3);
        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 200., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 100., epsilon = EPSILON);
    }

    #[test]
    fn argon_param_float() {
        let o = parse_workspace_with_std(ARGON_PARAM_FLOAT);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: vec![CellArg::Float(50.), CellArg::Float(20.)],
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        cells.unwrap_valid();
    }

    #[test]
    fn argon_param_int() {
        let o = parse_workspace_with_std(ARGON_PARAM_INT);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: vec![CellArg::Int(50), CellArg::Int(20)],
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        cells.unwrap_valid();
    }

    #[test]
    fn argon_workspace() {
        let o = parse_workspace_with_std(ARGON_WORKSPACE);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["test"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().next().unwrap().1.as_ref().unwrap_rect();
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 10., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 15., epsilon = EPSILON);
    }

    #[test]
    fn argon_external_mods() {
        let o = parse_workspace_with_std(ARGON_EXTERNAL_MODS);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["test"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().next().unwrap().1.as_ref().unwrap_rect();
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 10., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 20., epsilon = EPSILON);
    }

    #[test]
    fn argon_sky130_inverter() {
        let o = parse_workspace_with_std(ARGON_SKY130_LIB);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["inv"],
                args: vec![
                    CellArg::Float(1_200.),
                    CellArg::Float(2_000.),
                    CellArg::Int(4),
                ],
                lyp_file: &PathBuf::from(SKY130_LYP),
            },
        );
        println!("cells: {cells:?}");

        assert!(cells.is_valid());

        let work_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/argon_sky130_inverter");
        cells
            .to_gds(
                GdsMap::from_lyp(SKY130_LYP).expect("failed to create GDS map"),
                work_dir.join("layout.gds"),
            )
            .expect("Failed to write to GDS");
    }

    #[test]
    fn argon_enumerations() {
        let o = parse_workspace_with_std(ARGON_ENUMERATIONS);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().next().unwrap().1.as_ref().unwrap_rect();
        assert_eq!(r.layer.as_deref(), Some("met2"));
        assert_relative_eq!(r.x0.0, 100., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 300., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 400., epsilon = EPSILON);
    }

    #[test]
    fn argon_bbox() {
        let o = parse_workspace_with_std(ARGON_BBOX);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 5);
    }

    #[test]
    fn argon_rounding() {
        let o = parse_workspace_with_std(ARGON_ROUNDING);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_exec_errors();
        assert_eq!(cells.errors.len(), 1);
        assert!(matches!(
            cells.errors.first().unwrap().kind,
            ExecErrorKind::InconsistentConstraint(_)
        ));
    }

    #[test]
    fn argon_flipped_rect() {
        let o = parse_workspace_with_std(ARGON_FLIPPED_RECT);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_exec_errors();
        assert_eq!(cells.errors.len(), 2);
        assert!(matches!(
            cells.errors[0].kind,
            ExecErrorKind::FlippedRect(_)
        ));
        assert!(matches!(
            cells.errors[1].kind,
            ExecErrorKind::FlippedRect(_)
        ));
    }

    #[test]
    fn argon_seq_basic() {
        let o = parse_workspace_with_std(ARGON_SEQ_BASIC);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 400., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 200., epsilon = EPSILON);
    }

    #[test]
    fn argon_seq_fn() {
        let o = parse_workspace_with_std(ARGON_SEQ_FN);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 400., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 1250., epsilon = EPSILON);
    }

    #[test]
    fn argon_seq_recur() {
        let o = parse_workspace_with_std(ARGON_SEQ_RECUR);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 400., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 1200., epsilon = EPSILON);
    }

    #[test]
    fn argon_lub_match() {
        let o = parse_workspace_with_std(ARGON_LUB_MATCH);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 1);
        let r = cell.objects.iter().find_map(|(_, v)| v.get_rect()).unwrap();
        assert_eq!(r.layer.as_ref().unwrap(), "met1");
        assert_relative_eq!(r.x0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.y0.0, 0., epsilon = EPSILON);
        assert_relative_eq!(r.x1.0, 400., epsilon = EPSILON);
        assert_relative_eq!(r.y1.0, 200., epsilon = EPSILON);
    }

    #[test]
    fn argon_seq_cell() {
        let o = parse_workspace_with_std(ARGON_SEQ_CELL);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert!(cell.objects.len() >= 3);
        let inst = cell
            .objects
            .iter()
            .find_map(|(_, v)| v.get_instance())
            .unwrap();
        assert_relative_eq!(inst.x, 2000., epsilon = EPSILON);
        assert_relative_eq!(inst.y, 3000., epsilon = EPSILON);
    }

    #[test]
    fn argon_text() {
        let o = parse_workspace_with_std(ARGON_TEXT);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(SKY130_LYP),
            },
        );
        println!("{cells:#?}");

        let work_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/argon_text");
        cells
            .to_gds(
                GdsMap::from_lyp(SKY130_LYP).expect("failed to create GDS map"),
                work_dir.join("layout.gds"),
            )
            .expect("Failed to write to GDS");

        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 2);
        let t = cell.objects.iter().find_map(|(_, v)| v.get_text()).unwrap();
        assert_eq!(t.layer, "met1.label");
        assert_eq!(t.text, "mytext");
        assert_relative_eq!(t.x, 0., epsilon = EPSILON);
        assert_relative_eq!(t.y, 10., epsilon = EPSILON);
    }

    #[test]
    fn argon_any_type_inst() {
        let o = parse_workspace_with_std(ARGON_ANY_TYPE_INST);
        assert!(o.static_errors().is_empty());
        let ast = o.ast();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["top"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(SKY130_LYP),
            },
        );
        println!("{cells:#?}");

        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        assert_eq!(cell.objects.len(), 2);
    }
}
