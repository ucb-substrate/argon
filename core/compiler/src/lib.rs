use std::io::{self, BufRead, Write};

pub mod ast;
pub mod compile;
pub mod parse;
pub mod solver;

pub fn main() {
    let stdin = io::stdin();
    loop {
        print!(">>> ");
        io::stdout().flush().ok();
        match stdin.lock().lines().next() {
            Some(Ok(ref l)) => {
                if l.trim().is_empty() {
                    continue;
                }
                let res = parse::parse(l);
                match res {
                    Ok(r) => println!("Result: {r:?}"),
                    _ => eprintln!("Unable to evaluate expression."),
                }
            }
            _ => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use parse::parse;

    use crate::compile::{compile, CompileInput, VarIdTyPass};

    use super::*;

    const ARGON_SIMPLE: &str = r#"enum Layer {
	Met2,
	Via1,
	Met1,
}

cell simple(y_enclosure: int) {
    let r = Rect!(Layer::Met1, y0=0, y1=100);
    Eq!(r.x0, 0);
    Eq!(r.x1, 100);
    Rect!(Layer::Met2, x0=r.x0-10, x1=r.x1+10, y0=0-y_enclosure, y1=100+y_enclosure);
}"#;
    const ARGON_SCOPES: &str =
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/scopes.ar"));
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
    const ARGON_HIERARCHY: &str = include_str!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/examples/hierarchy.ar"
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
                params: HashMap::new(),
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
                params: HashMap::new(),
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
                params: HashMap::new(),
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
                params: HashMap::new(),
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
                params: HashMap::new(),
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
                params: HashMap::new(),
            },
        );
        println!("{cell:?}");
    }

    #[test]
    fn argon_hierarchy() {
        let ast = parse(ARGON_HIERARCHY).expect("failed to parse Argon");
        let cell = compile(
            &ast,
            CompileInput {
                cell: "top",
                params: HashMap::new(),
            },
        );
        println!("{cell:?}");
    }

    // #[test]
    // fn argon_simple() {
    //     let ast = parse(ARGON_SIMPLE).expect("failed to parse Argon");
    //     let cell = compile(CompileInput {
    //         cell: "simple",
    //         ast: &ast,
    //         params: HashMap::from_iter([("y_enclosure", 20.)]),
    //     })
    //     .expect("failed to compile Argon cell");
    //     println!("cell: {cell:?}");
    // }

    // #[test]
    // fn argon_via_array() {
    //     let ast = parse(ARGON_VIA_ARRAY).expect("failed to parse Argon");
    //     println!("{:?}", &ast);
    //     let cell = compile(CompileInput {
    //         cell: "vias",
    //         ast: &ast,
    //         params: HashMap::new(),
    //     })
    //     .expect("failed to compile Argon cell");
    //     println!("cell: {cell:?}");
    //     assert_eq!(cell.rects.len(), 11);
    // }

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
