//! Checks the SRAM example's WebAssembly sizer against its checked-in record.

use std::{path::PathBuf, process::Command};

use argonc::{
    compile::{CellArg, CompileOutput, CompiledData, SolvedValue},
    incremental::IncrementalCompiler,
    workspace::WorkspaceConfig,
};

use crate::repository_root;

/// Builds the sizer module into its own target directory, so the build does
/// not wait on the lock held by the build running these tests.
fn build_sizer() -> PathBuf {
    let root = repository_root();
    let target = root.join("target/wasm-tests");
    let status = Command::new(std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
        .current_dir(&root)
        .args([
            "build",
            "--quiet",
            "--release",
            "-p",
            "argon-sram-sizing-wasm",
            "--target",
            "wasm32-unknown-unknown",
            "--target-dir",
        ])
        .arg(&target)
        .status()
        .expect("run cargo");
    assert!(
        status.success(),
        "building the sizer module failed; is the wasm32-unknown-unknown target installed?"
    );
    target.join("wasm32-unknown-unknown/release/argon_sram_sizing_wasm.wasm")
}

/// Every cell but the top, described by its shapes and instances.
fn hierarchy(output: &CompileOutput) -> Vec<String> {
    let data = match output {
        CompileOutput::Valid(data) => data,
        CompileOutput::ExecErrors(errors) => panic!("SRAM errors: {:?}", errors.errors),
        CompileOutput::StaticErrors(errors) => panic!("SRAM errors: {:?}", errors.errors),
        _ => panic!("the SRAM did not compile"),
    };
    let mut cells = data
        .cells
        .iter()
        .filter(|(id, _)| **id != data.top)
        .map(|(_, cell)| {
            let mut objects = cell
                .objects
                .values()
                .filter_map(|object| describe(data, object))
                .collect::<Vec<_>>();
            objects.sort();
            format!("{}: {}", cell.name, objects.join("; "))
        })
        .collect::<Vec<_>>();
    cells.sort();
    cells
}

fn describe(data: &CompiledData, object: &SolvedValue) -> Option<String> {
    Some(match object {
        SolvedValue::Rect(rect) => format!(
            "rect {:?} {} {} {} {}",
            rect.layer, rect.x0.0, rect.y0.0, rect.x1.0, rect.y1.0
        ),
        SolvedValue::Polygon(polygon) => format!(
            "polygon {} {:?}",
            polygon.layer,
            polygon
                .points
                .iter()
                .map(|(x, y)| (x.0, y.0))
                .collect::<Vec<_>>()
        ),
        SolvedValue::Path(path) => format!(
            "path {} {} {:?}",
            path.layer,
            path.width.0,
            path.points
                .iter()
                .map(|(x, y)| (x.0, y.0))
                .collect::<Vec<_>>()
        ),
        SolvedValue::Instance(inst) => format!(
            "inst {} {} {} {:?} {}",
            data.cells[&inst.cell].name, inst.x, inst.y, inst.angle, inst.reflect
        ),
        _ => return None,
    })
}

#[test]
fn the_wasm_sizer_reproduces_the_generated_record() {
    let root = repository_root();
    let example = root.join("examples/sram");
    let config = WorkspaceConfig::new(example.join("lib.ar"))
        .with_tech(Some(root.join("pdks/sky130/sky130.tech.toml")))
        .with_wasm_modules([("sizing".to_owned(), build_sizer())]);
    let generated =
        IncrementalCompiler::new().compile_cell(&config, &["example_sram".to_owned()], Vec::new());
    let sized = IncrementalCompiler::new().compile_cell(
        &config,
        &["example_sized_sram".to_owned()],
        [8, 4, 64, 24].map(CellArg::Int).to_vec(),
    );
    let generated = hierarchy(&generated);
    assert!(generated.len() > 100, "the SRAM has its full hierarchy");
    assert_eq!(hierarchy(&sized), generated);
}
