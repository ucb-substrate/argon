use std::io::{self, BufRead, Write};

use lrlex::lrlex_mod;
use lrpar::lrpar_mod;

pub mod compile;
pub mod parse;
pub mod solver;

lrlex_mod!("cadlang.l");
lrpar_mod!("cadlang.y");

pub fn main() {
    // Get the `LexerDef` for the `cadlang` language.
    let lexerdef = cadlang_l::lexerdef();
    let stdin = io::stdin();
    loop {
        print!(">>> ");
        io::stdout().flush().ok();
        match stdin.lock().lines().next() {
            Some(Ok(ref l)) => {
                if l.trim().is_empty() {
                    continue;
                }
                // Now we create a lexer with the `lexer` method with which
                // we can lex an input.
                let lexer = lexerdef.lexer(l);
                // Pass the lexer to the parser and lex and parse the input.
                let (res, errs) = cadlang_y::parse(&lexer);
                for e in errs {
                    println!("{}", e.pp(&lexer, &cadlang_y::token_epp));
                }
                match res {
                    Some(Ok(r)) => println!("Result: {:?}", r),
                    _ => eprintln!("Unable to evaluate expression."),
                }
            }
            _ => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::HashMap, path::PathBuf};

    use gds21::{GdsBoundary, GdsElement, GdsLibrary, GdsPoint, GdsStruct};
    use parse::parse;

    use crate::compile::{compile, CompileInput};

    use super::*;

    const CADLANG_SIMPLE: &str = r#"enum Layer {
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

    #[test]
    fn cadlang_simple() {
        let ast = parse(CADLANG_SIMPLE).expect("failed to parse Cadlang");
        let cell = compile(CompileInput {
            cell: "simple",
            ast: &ast,
            params: HashMap::from_iter([("y_enclosure", 20.)]),
        })
        .expect("failed to compile Cadlang cell");
        println!("cell: {cell:?}");
    }

    const CADLANG_VIA: &str = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/examples/via.cl"));

    #[test]
    fn cadlang_via() {
        let ast = parse(CADLANG_VIA).expect("failed to parse Cadlang");
        let cell = compile(CompileInput {
            cell: "vias",
            ast: &ast,
            params: HashMap::new(),
        })
        .expect("failed to compile Cadlang cell");
        println!("cell: {cell:?}");
        assert_eq!(cell.rects.len(), 11);
    }

    const CADLANG_SKY130_INVERTER: &str = include_str!("../examples/sky130_inverter.cl");

    #[test]
    fn cadlang_sky130_inverter() {
        let ast = parse(CADLANG_SKY130_INVERTER).expect("failed to parse Cadlang");
        let cell = compile(CompileInput {
            cell: "inverter",
            ast: &ast,
            params: HashMap::from_iter([("nw", 1_200.), ("pw", 2_000.)]),
        })
        .expect("failed to solve compile Cadlang cell");
        println!("cell: {cell:?}");

        let mut gds = GdsLibrary::new("TOP");
        let mut ocell = GdsStruct::new("cell");
        for rect in &cell.rects {
            if let Some(layer) = &rect.layer {
                let (layer, datatype) = match layer.as_str() {
                    "Nwell" => (64, 20),
                    "Diff" => (65, 20),
                    "Tap" => (65, 44),
                    "Psdm" => (94, 20),
                    "Nsdm" => (93, 44),
                    "Poly" => (66, 20),
                    "Licon1" => (66, 44),
                    "Npc" => (95, 20),
                    "Li1" => (67, 20),
                    _ => unreachable!(),
                };
                let x0 = rect.x0 as i32;
                let x1 = rect.x1 as i32;
                let y0 = rect.y0 as i32;
                let y1 = rect.y1 as i32;
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
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("build/cadlang_sky130_inverter");
        std::fs::create_dir_all(&work_dir).expect("failed to create dirs");
        gds.save(work_dir.join("layout.gds"))
            .expect("failed to write GDS");
    }
}
