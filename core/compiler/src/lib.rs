use clap::Parser;
use compile::CompileInput;
use std::path::PathBuf;

pub mod ast;
pub mod compile;
pub mod layer;
pub mod parse;
pub mod solver;

/// Argon compiler.
// TODO: Use subcommands for better API clarity.
#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
struct Args {
    /// The Argon source code to compile.
    file: PathBuf,
    /// The name of the cell to compile.
    #[arg(short, long)]
    cell: Option<String>,
    /// Layer properties file.
    #[arg(short, long)]
    lyp: Option<PathBuf>,
    /// Outputs AST as JSON.
    #[arg(short, long)]
    ast: bool,
}

pub fn main() {
    let args = Args::parse();
    let f = std::fs::read_to_string(&args.file).unwrap();
    let ast = parse::parse(&f).unwrap();
    if args.ast {
        println!("{}", serde_json::to_string(&ast).unwrap());
    } else {
        let cell = args.cell.unwrap();
        let cell_ast = parse::parse_cell(&cell).unwrap();
        let o = compile::compile(
            &ast,
            CompileInput {
                cell: cell_ast.func.name,
                params: cell_ast
                    .args
                    .posargs
                    .iter()
                    .map(|arg| match arg {
                        ast::Expr::FloatLiteral(float_literal) => float_literal.value,
                        ast::Expr::IntLiteral(int_literal) => int_literal.value as f64,
                        _ => panic!("must be int or float literal for now"),
                    })
                    .collect(),
                lyp_file: &args.lyp.unwrap(),
            },
        );
        println!("{}", serde_json::to_string(&o).unwrap());
    }
}

#[cfg(test)]
mod tests {

    use parse::parse;

    use crate::compile::{CompileInput, compile};

    use super::*;

    const ARGON_SCOPES: &str =
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/scopes.ar"));
    const BASIC_LYP: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/examples/lyp/basic.lyp");
    const ARGON_IMMEDIATE: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/immediate.ar"
    ));
    const ARGON_IF: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/if.ar"));
    const ARGON_IF_INCONSISTENT: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/if_inconsistent.ar"
    ));
    const ARGON_VIA: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/via.ar"));
    const ARGON_VIA_ARRAY: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/via_array.ar"
    ));
    const ARGON_FUNC_OUT_OF_ORDER: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/func_out_of_order.ar"
    ));
    const ARGON_HIERARCHY: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/hierarchy.ar"
    ));
    const ARGON_NESTED_INST: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/nested_inst.ar"
    ));
    const ARGON_CELL_OUT_OF_ORDER: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/cell_out_of_order.ar"
    ));
    const ARGON_FALLBACK_BASIC: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/fallback_basic.ar"
    ));
    const ARGON_FALLBACK_INST: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/fallback_inst.ar"
    ));
    const ARGON_BOOL_LITERAL: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/bool_literal.ar"
    ));
    const ARGON_SKY130_INVERTER: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/sky130_inverter.ar"
    ));

    #[test]
    fn argon_scopes() {
        let ast = parse(ARGON_SCOPES).expect("failed to parse Argon");
        let cell = compile(
            &ast,
            CompileInput {
                cell: "scopes",
                params: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_immediate() {
        let ast = parse(ARGON_IMMEDIATE).expect("failed to parse Argon");
        let cell = compile(
            &ast,
            CompileInput {
                cell: "immediate",
                params: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_if() {
        let ast = parse(ARGON_IF).expect("failed to parse Argon");
        let cell = compile(
            &ast,
            CompileInput {
                cell: "if_test",
                params: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_if_inconsistent() {
        let ast = parse(ARGON_IF_INCONSISTENT).expect("failed to parse Argon");
        let cell = compile(
            &ast,
            CompileInput {
                cell: "if_test",
                params: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_via() {
        let ast = parse(ARGON_VIA).expect("failed to parse Argon");
        let cell = compile(
            &ast,
            CompileInput {
                cell: "via",
                params: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_via_array() {
        let ast = parse(ARGON_VIA_ARRAY).expect("failed to parse Argon");
        let cell = compile(
            &ast,
            CompileInput {
                cell: "vias",
                params: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_func_out_of_order() {
        let ast = parse(ARGON_FUNC_OUT_OF_ORDER).expect("failed to parse Argon");
        let cell = compile(
            &ast,
            CompileInput {
                cell: "test",
                params: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_hierarchy() {
        let ast = parse(ARGON_HIERARCHY).expect("failed to parse Argon");
        let cells = compile(
            &ast,
            CompileInput {
                cell: "top",
                params: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
    }

    #[test]
    fn argon_nested_inst() {
        let ast = parse(ARGON_NESTED_INST).expect("failed to parse Argon");
        let cells = compile(
            &ast,
            CompileInput {
                cell: "top",
                params: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
    }

    #[test]
    fn argon_cell_out_of_order() {
        let ast = parse(ARGON_CELL_OUT_OF_ORDER).expect("failed to parse Argon");
        let cells = compile(
            &ast,
            CompileInput {
                cell: "top",
                params: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        );
        println!("{cells:#?}");
    }

    #[test]
    fn argon_fallback_basic() {
        let ast = parse(ARGON_FALLBACK_BASIC).expect("failed to parse Argon");
        let cells = compile(
            &ast,
            CompileInput {
                cell: "top",
                params: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        )
        .unwrap_valid();
        assert!(!cells.cells[&cells.top].fallback_constraints_used.is_empty());
        println!("{cells:#?}");
    }

    #[test]
    fn argon_fallback_inst() {
        let ast = parse(ARGON_FALLBACK_INST).expect("failed to parse Argon");
        let cells = compile(
            &ast,
            CompileInput {
                cell: "top",
                params: Vec::new(),
                lyp_file: &PathBuf::from(BASIC_LYP),
            },
        )
        .unwrap_valid();
        assert!(!cells.cells[&cells.top].fallback_constraints_used.is_empty());
        println!("{cells:#?}");
    }

    #[test]
    fn argon_bool_literal() {
        let ast = parse(ARGON_BOOL_LITERAL).expect("failed to parse Argon");
        let cells = compile(
            &ast,
            CompileInput {
                cell: "top",
                params: Vec::new(),
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

    // #[test]
    // fn argon_sky130_inverter() {
    //     let ast = parse(ARGON_SKY130_INVERTER).expect("failed to parse Argon");
    //     let cell = compile(CompileInput {
    //         cell: "inverter",
    //         ast: &ast,
    //         params: HashMap::from_iter([("nw", 1_200.), ("pw", 2_000.)]),
    //     })
    //     .expect("failed to solve compile Argon cell");
    //     println!("cell: {cell:?}");

    //     let mut gds = GdsLibrary::new("TOP");
    //     let mut ocell = GdsStruct::new("cell");
    //     for rect in &cell.rects {
    //         if let Some(layer) = &rect.layer {
    //             let (layer, datatype) = match layer.as_str() {
    //                 "Nwell" => (64, 20),
    //                 "Diff" => (65, 20),
    //                 "Tap" => (65, 44),
    //                 "Psdm" => (94, 20),
    //                 "Nsdm" => (93, 44),
    //                 "Poly" => (66, 20),
    //                 "Licon1" => (66, 44),
    //                 "Npc" => (95, 20),
    //                 "Li1" => (67, 20),
    //                 _ => unreachable!(),
    //             };
    //             let x0 = rect.x0 as i32;
    //             let x1 = rect.x1 as i32;
    //             let y0 = rect.y0 as i32;
    //             let y1 = rect.y1 as i32;
    //             ocell.elems.push(GdsElement::GdsBoundary(GdsBoundary {
    //                 layer,
    //                 datatype,
    //                 xy: vec![
    //                     GdsPoint::new(x0, y0),
    //                     GdsPoint::new(x0, y1),
    //                     GdsPoint::new(x1, y1),
    //                     GdsPoint::new(x1, y0),
    //                 ],
    //                 ..Default::default()
    //             }));
    //         }
    //     }
    //     gds.structs.push(ocell);
    //     let work_dir =
    //         PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/argon_sky130_inverter");
    //     std::fs::create_dir_all(&work_dir).expect("failed to create dirs");
    //     gds.save(work_dir.join("layout.gds"))
    //         .expect("failed to write GDS");
    // }
}
