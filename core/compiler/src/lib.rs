pub mod ast;
pub mod compile;
pub mod config;
pub mod layer;
pub mod parse;
pub mod solver;

#[cfg(test)]
mod tests {

    use std::{io::BufReader, path::PathBuf};

    use crate::parse::parse_workspace_with_std;
    use approx::assert_relative_eq;
    use gds21::{GdsBoundary, GdsElement, GdsLibrary, GdsPoint, GdsStruct};
    use indexmap::IndexMap;
    use regex::Regex;

    use crate::compile::{CellArg, CompileInput, compile};
    const EPSILON: f64 = 1e-10;

    const ARGON_SCOPES: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/scopes/lib.ar");
    const BASIC_LYP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/lyp/basic.lyp");
    const SKY130_LYP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/lyp/sky130.lyp");
    const ARGON_IMMEDIATE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/immediate/lib.ar");
    const ARGON_IF: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/if/lib.ar");
    const ARGON_IF_INCONSISTENT: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/if_inconsistent/lib.ar"
    );
    const ARGON_VIA: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/via/lib.ar");
    const ARGON_VIA_ARRAY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/via_array/lib.ar");
    const ARGON_FUNC_OUT_OF_ORDER: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/func_out_of_order/lib.ar"
    );
    const ARGON_HIERARCHY: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/hierarchy/lib.ar");
    const ARGON_NESTED_INST: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples/nested_inst/lib.ar");
    const ARGON_CELL_OUT_OF_ORDER: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/cell_out_of_order/lib.ar"
    );
    const ARGON_FALLBACK_BASIC: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/fallback_basic/lib.ar"
    );
    const ARGON_FALLBACK_INST: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples/fallback_inst/lib.ar");
    const ARGON_BOOL_LITERAL: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples/bool_literal/lib.ar");
    const ARGON_DIMENSIONS: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples/dimensions/lib.ar");
    const ARGON_PARAM_FLOAT: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples/param_float/lib.ar");
    const ARGON_PARAM_INT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/param_int/lib.ar");
    const ARGON_SKY130_INVERTER: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/sky130_inverter/lib.ar"
    );
    const ARGON_ENUMERATIONS: &str =
        concat!(env!("CARGO_MANIFEST_DIR"), "/examples/enumerations/lib.ar");
    const ARGON_BBOX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/bbox/lib.ar");
    const ARGON_WORKSPACE: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/argon_workspace/lib.ar"
    );
    const ARGON_EXTERNAL_MODS: &str = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/external_mods/main_crate/lib.ar"
    );

    #[test]
    fn argon_scopes() {
        let ast = parse_workspace_with_std(ARGON_SCOPES).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_IMMEDIATE).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_IF).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_IF_INCONSISTENT).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_VIA).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_VIA_ARRAY).unwrap_asts();
        let cell = compile(
            &ast,
            CompileInput {
                cell: &["vias"],
                args: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_func_out_of_order() {
        let ast = parse_workspace_with_std(ARGON_FUNC_OUT_OF_ORDER).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_HIERARCHY).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_NESTED_INST).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_CELL_OUT_OF_ORDER).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_FALLBACK_BASIC).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_FALLBACK_INST).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_BOOL_LITERAL).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_DIMENSIONS).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_PARAM_FLOAT).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_PARAM_INT).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_WORKSPACE).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_EXTERNAL_MODS).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_SKY130_INVERTER).unwrap_asts();
        let cells = compile(
            &ast,
            CompileInput {
                cell: &["inverter"],
                args: vec![CellArg::Float(1_200.), CellArg::Float(2_000.)],
                lyp_file: &PathBuf::from(SKY130_LYP),
            },
        );
        println!("cells: {cells:?}");

        let cells = cells.unwrap_valid();
        let cell = &cells.cells[&cells.top];
        let mut gds = GdsLibrary::new("TOP");
        let mut ocell = GdsStruct::new("cell");
        let lyp =
            klayout_lyp::from_reader(BufReader::new(std::fs::File::open(SKY130_LYP).unwrap()))
                .unwrap();
        let layers = lyp
            .layers
            .iter()
            .map(|layer_prop| {
                let re = Regex::new(r"(\d*)/(\d*)@\d*").unwrap();
                let caps = re.captures(&layer_prop.source).unwrap();
                let layer = caps.get(1).unwrap().as_str().parse().unwrap();
                let datatype = caps.get(2).unwrap().as_str().parse().unwrap();
                (layer_prop.name.as_str(), (layer, datatype))
            })
            .collect::<IndexMap<_, _>>();
        for rect in cell.objects.iter().filter_map(|obj| obj.1.get_rect()) {
            if let Some(layer) = &rect.layer {
                let (layer, datatype) = layers[&layer.as_str()];
                let x0 = rect.x0.0 as i32;
                let x1 = rect.x1.0 as i32;
                let y0 = rect.y0.0 as i32;
                let y1 = rect.y1.0 as i32;
                ocell.elems.push(GdsElement::GdsBoundary(GdsBoundary {
                    layer,
                    datatype,
                    xy: vec![
                        GdsPoint::new(x0, y0),
                        GdsPoint::new(x0, y1),
                        GdsPoint::new(x1, y1),
                        GdsPoint::new(x1, y0),
                    ],
                    ..Default::default()
                }));
            }
        }
        gds.structs.push(ocell);
        let work_dir =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/argon_sky130_inverter");
        std::fs::create_dir_all(&work_dir).expect("failed to create dirs");
        gds.save(work_dir.join("layout.gds"))
            .expect("failed to write GDS");
    }

    #[test]
    fn argon_enumerations() {
        let ast = parse_workspace_with_std(ARGON_ENUMERATIONS).unwrap_asts();
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
        let ast = parse_workspace_with_std(ARGON_BBOX).unwrap_asts();
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
}
