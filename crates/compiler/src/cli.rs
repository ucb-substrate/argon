use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::ExitCode,
};

use crate::{
    WorkspaceConfig, artifact,
    compile::{self, CompileOutput},
    diagnostics::{self, Diagnostic},
    parse::{self, CellInvocation, parse_workspace_with_config},
};
use clap::{Parser, ValueEnum};

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ErrorFormat {
    Human,
    Json,
}

#[derive(Debug, Parser)]
#[command(version, about = "The Argon compiler")]
struct Args {
    /// Argon library root. May be a directory or its lib.ar file.
    root: PathBuf,

    /// Cell invocation to instantiate, for example `top(10., 20.)`.
    #[arg(long)]
    cell: Option<String>,

    /// Argon TOML technology file. Required when a cell is instantiated.
    #[arg(long, requires = "cell")]
    tech: Option<PathBuf>,

    /// Path dependency in NAME=PATH form. PATH may be a directory or lib.ar.
    #[arg(long = "dependency", value_parser = parse_dependency)]
    dependencies: Vec<(String, PathBuf)>,

    /// GDS cell import in NAME=PATH form. NAME may be a module path.
    #[arg(long = "gds-import", value_parser = parse_gds_import)]
    gds_imports: Vec<(String, PathBuf)>,

    /// Binary compiler-output path. Defaults to lib.bin beside the root lib.ar.
    #[arg(short, long, requires = "cell")]
    output: Option<PathBuf>,

    /// Also emit GDS to this path.
    #[arg(long, requires = "cell")]
    gds: Option<PathBuf>,

    /// Run all non-executing compiler stages, then stop.
    #[arg(long, conflicts_with_all = ["cell", "tech", "output", "gds"])]
    check: bool,

    /// Diagnostic output format.
    #[arg(long, value_enum, default_value = "human")]
    error_format: ErrorFormat,
}

pub fn run() -> ExitCode {
    let args = Args::parse();
    let format = args.error_format;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| execute(args)));
    std::panic::set_hook(previous_hook);
    let result = match result {
        Ok(result) => result,
        Err(_) => Err(fail(
            format,
            "internal compiler error: compilation aborted unexpectedly",
        )),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(diagnostics) => {
            let json = matches!(diagnostics.0, ErrorFormat::Json);
            for diagnostic in diagnostics.1 {
                if let Err(error) = diagnostics::emit(&diagnostic, json) {
                    eprintln!("error: failed to write diagnostic: {error}");
                    break;
                }
            }
            ExitCode::FAILURE
        }
    }
}

struct Failed(ErrorFormat, Vec<Diagnostic>);

fn execute(args: Args) -> Result<(), Failed> {
    let format = args.error_format;
    let root = source_root(&args.root);
    let mut names = HashSet::new();
    for (name, _) in &args.dependencies {
        if !names.insert(name) {
            return Err(fail(format, format!("duplicate path dependency `{name}`")));
        }
    }
    names.clear();
    for (name, _) in &args.gds_imports {
        if !names.insert(name) {
            return Err(fail(format, format!("duplicate GDS import `{name}`")));
        }
    }

    let workspace = WorkspaceConfig::new(&root)
        .with_dependencies(args.dependencies.clone())
        .with_tech(args.tech.clone())
        .with_gds_imports(args.gds_imports.clone());
    // The invocation is spliced into the workspace before analysis so that its
    // arguments are resolved, type-checked, and evaluated like any source
    // expression, which means the entry point must be resolved before parsing.
    let entry = if args.check {
        None
    } else {
        let Some(cell) = args.cell.as_deref() else {
            return Err(fail(format, "either --check or --cell is required"));
        };
        let Some(tech) = args.tech.as_deref() else {
            return Err(fail(
                format,
                "--tech is required when compiling a cell; pass the path to an Argon TOML technology file",
            ));
        };
        crate::tech::read_tech(tech).map_err(|error| fail(format, error.to_string()))?;
        Some(cell)
    };

    let mut parse_output = parse_workspace_with_config(&workspace);
    let entry = match entry {
        Some(cell) => Some(
            parse::add_cell_invocation(&mut parse_output, cell)
                .map_err(|error| fail(format, format!("invalid cell invocation: {error}")))?,
        ),
        None => None,
    };
    let analysis = compile::analyze_workspace(parse_output);
    let Some(typed_ast) = analysis.typed_ast else {
        return Err(fail(
            format,
            format!("could not parse library root `{}`", root.display()),
        ));
    };
    if !analysis.errors.is_empty() {
        return Err(compile_failed(
            format,
            CompileOutput::StaticErrors(compile::StaticErrorCompileOutput {
                errors: analysis.errors,
            }),
            entry.as_ref(),
        ));
    }
    // Nothing left to do for a `--check` run.
    let Some(invocation) = entry else {
        return Ok(());
    };

    let output = compile::execute_cell_invocation(&typed_ast, &invocation, &workspace);
    if !matches!(output, CompileOutput::Valid(_)) {
        return Err(compile_failed(format, output, Some(&invocation)));
    }
    let output_path = args.output.unwrap_or_else(|| root.with_extension("bin"));
    artifact::write(&output, &output_path).map_err(|error| {
        fail(
            format,
            format!("could not write `{}`: {error}", output_path.display()),
        )
    })?;

    if let Some(gds_path) = args.gds {
        output.to_gds(&gds_path).map_err(|error| {
            fail(
                format,
                format!("could not write `{}`: {error}", gds_path.display()),
            )
        })?;
    }
    Ok(())
}

fn source_root(path: &Path) -> PathBuf {
    if path.is_dir() {
        path.join("lib.ar")
    } else {
        path.to_path_buf()
    }
}

fn parse_dependency(value: &str) -> Result<(String, PathBuf), String> {
    let (name, path) = value
        .split_once('=')
        .ok_or_else(|| "expected NAME=PATH".to_string())?;
    if name.is_empty() || path.is_empty() {
        return Err("expected non-empty NAME and PATH".to_string());
    }
    Ok((name.to_string(), PathBuf::from(path)))
}

fn parse_gds_import(value: &str) -> Result<(String, PathBuf), String> {
    let (name, path) = parse_dependency(value)?;
    if name.split("::").all(is_identifier) {
        Ok((name, path))
    } else {
        Err("NAME must be an Argon identifier or module path".to_string())
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn fail(format: ErrorFormat, message: impl Into<String>) -> Failed {
    Failed(format, vec![Diagnostic::error(message)])
}

fn compile_failed(
    format: ErrorFormat,
    output: CompileOutput,
    invocation: Option<&CellInvocation>,
) -> Failed {
    let mut diagnostics = diagnostics::from_compile_output(&output);
    if let Some(invocation) = invocation {
        diagnostics::remap_invocation(&mut diagnostics, invocation);
    }
    Failed(format, diagnostics)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{
        artifact,
        compile::{CompileOutput, Rect},
        solver::LinearExpr,
    };
    use clap::{CommandFactory, Parser};
    use gds::{GdsBoundary, GdsElement, GdsLibrary, GdsPath, GdsPoint, GdsStruct};

    use super::{Args, ErrorFormat, Failed, execute};

    fn temp_source(name: &str, source: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock should be after the Unix epoch")
            .as_nanos();
        let directory = std::env::temp_dir().join(format!("argonc-{name}-{nonce}"));
        fs::create_dir_all(&directory).expect("temporary directory should be created");
        let path = directory.join("lib.ar");
        fs::write(&path, source).expect("temporary source should be written");
        path
    }

    fn basic_tech() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml")
    }

    fn temp_gds(name: &str) -> PathBuf {
        let source = temp_source(name, "");
        let path = source.with_extension("gds");
        let mut library = GdsLibrary::new("fixture");
        let mut structure = GdsStruct::new("layout_top");
        structure.elems.push(GdsElement::GdsBoundary(GdsBoundary {
            layer: 235,
            datatype: 4,
            xy: vec![
                GdsPoint::new(0, 0),
                GdsPoint::new(0, 20),
                GdsPoint::new(10, 20),
                GdsPoint::new(10, 0),
            ],
            ..Default::default()
        }));
        library.structs.push(structure);
        library
            .save(&path)
            .expect("temporary GDS should be written");
        path
    }

    fn temp_path_gds(name: &str) -> PathBuf {
        let source = temp_source(name, "");
        let path = source.with_extension("gds");
        let mut library = GdsLibrary::new("fixture");
        let mut structure = GdsStruct::new("layout_top");
        structure.elems.push(GdsElement::GdsPath(GdsPath {
            layer: 235,
            datatype: 4,
            width: Some(20),
            path_type: Some(2),
            xy: vec![
                GdsPoint::new(0, 0),
                GdsPoint::new(100, 0),
                GdsPoint::new(100, 50),
            ],
            ..Default::default()
        }));
        library.structs.push(structure);
        library
            .save(&path)
            .expect("temporary path GDS should be written");
        path
    }

    fn temp_nested_gds(name: &str) -> PathBuf {
        let source = temp_source(name, "");
        let path = source.with_extension("gds");
        let mut library = GdsLibrary::new("fixture");
        let mut child = GdsStruct::new("layout_child");
        child.elems.push(GdsElement::GdsBoundary(GdsBoundary {
            layer: 235,
            datatype: 4,
            xy: vec![
                GdsPoint::new(0, 0),
                GdsPoint::new(0, 20),
                GdsPoint::new(10, 20),
                GdsPoint::new(10, 0),
            ],
            ..Default::default()
        }));
        let mut top = GdsStruct::new("layout_top");
        top.elems.push(GdsElement::GdsStructRef(gds::GdsStructRef {
            name: "layout_child".into(),
            xy: GdsPoint::new(0, 0),
            ..Default::default()
        }));
        library.structs.extend([child, top]);
        library
            .save(&path)
            .expect("temporary nested GDS should be written");
        path
    }

    fn check_args(root: PathBuf) -> Args {
        Args {
            root,
            cell: None,
            tech: None,
            dependencies: Vec::new(),
            gds_imports: Vec::new(),
            output: None,
            gds: None,
            check: true,
            error_format: ErrorFormat::Human,
        }
    }

    fn execution_args(root: PathBuf, cell: &str, tech: PathBuf) -> Args {
        Args {
            root,
            cell: Some(cell.to_owned()),
            tech: Some(tech),
            dependencies: Vec::new(),
            gds_imports: Vec::new(),
            output: None,
            gds: None,
            check: false,
            error_format: ErrorFormat::Human,
        }
    }

    fn failed(args: Args) -> Failed {
        match execute(args) {
            Ok(()) => panic!("compilation should fail"),
            Err(error) => error,
        }
    }

    fn render_failed(error: Failed) -> String {
        let mut output = Vec::new();
        for diagnostic in error.1 {
            crate::diagnostics::render(&mut output, &diagnostic, false)
                .expect("diagnostic should render");
        }
        String::from_utf8(output).expect("diagnostics should be UTF-8")
    }

    #[test]
    fn checks_a_source_file() {
        let source = temp_source("valid", "cell top() {}\n");
        assert!(execute(check_args(source)).is_ok());
    }

    #[test]
    fn checks_a_library_directory() {
        let source = temp_source("directory-root", "cell top() {}\n");
        let directory = source
            .parent()
            .expect("source should have a parent")
            .to_owned();
        assert!(execute(check_args(directory)).is_ok());
    }

    #[test]
    fn rejects_a_second_positional_root() {
        let first = temp_source("first-root", "cell first() {}\n");
        let second = temp_source("second-root", "cell second() {}\n");
        let error = Args::try_parse_from([
            "argonc",
            first.to_str().expect("path should be UTF-8"),
            second.to_str().expect("path should be UTF-8"),
            "--check",
        ])
        .expect_err("a second root should be rejected");
        assert!(error.to_string().contains("unexpected argument"));
    }

    #[test]
    fn unsupported_source_is_a_diagnostic_not_a_panic() {
        let source = temp_source("unsupported", "struct Point {}\n");
        let diagnostic = render_failed(failed(check_args(source)));
        assert!(
            diagnostic.contains("error: error during parsing"),
            "{diagnostic}"
        );
    }

    #[test]
    fn missing_input_is_reported_cleanly() {
        let diagnostic = render_failed(failed(check_args(PathBuf::from(
            "/path/that/does/not/exist/lib.ar",
        ))));
        assert!(diagnostic.contains("could not load source"), "{diagnostic}");
    }

    #[test]
    fn execution_writes_binary_output_without_gds_by_default() {
        let source = temp_source(
            "run",
            "cell top() { let r = rect(\"met1\", x0=0., y0=0., x1=10., y1=20.); }\n",
        );
        let directory = source.parent().expect("source should have a parent");
        let artifact_path = directory.join("top.bin");
        let implicit_gds_path = source.with_extension("gds");
        let mut args = execution_args(source, "top()", basic_tech());
        args.output = Some(artifact_path.clone());

        assert!(execute(args).is_ok());
        assert!(matches!(
            artifact::read(artifact_path).expect("artifact should decode"),
            CompileOutput::Valid(_)
        ));
        assert!(!implicit_gds_path.exists());
    }

    #[test]
    fn execution_accepts_a_boolean_cell_argument() {
        let source = temp_source("bool-root", "");
        let dependency = temp_source(
            "bool-dependency",
            r#"cell device(enabled: Bool, w: Float, count: Int) {
    if enabled {
        rect("met1", x0=0., y0=0., w=w, h=10.);
    } else {
        rect("met1", x0=0., y0=0., w=w, h=20.);
    };
}
"#,
        );
        let mut args = execution_args(source, "devices::device(true, 150., 5)", basic_tech());
        args.dependencies.push(("devices".to_owned(), dependency));
        args.output = Some(std::env::temp_dir().join("argonc-bool.bin"));
        assert!(execute(args).is_ok());
    }

    /// Reads the sole rect from an invocation-compiled artifact.
    fn compiled_rect(name: &str, source: PathBuf, cell: &str) -> Rect<(f64, LinearExpr)> {
        let artifact_path = source.with_file_name(format!("{name}.bin"));
        let mut args = execution_args(source, cell, basic_tech());
        args.output = Some(artifact_path.clone());
        if let Err(error) = execute(args) {
            panic!("`{cell}` should compile: {}", render_failed(error));
        }
        let CompileOutput::Valid(output) =
            artifact::read(artifact_path).expect("artifact should decode")
        else {
            panic!("`{cell}` should compile successfully");
        };
        let top = &output.cells[&output.top];
        // The generated entry cell is an implementation detail and must not
        // reach the output, and the target keeps its own root scope name.
        assert_eq!(output.cells.len(), 1, "entry cell should not be emitted");
        assert_eq!(top.scopes[&top.root].name, "cell top");
        top.objects
            .values()
            .find_map(|object| object.get_rect())
            .expect("top should emit a rect")
            .clone()
    }

    #[test]
    fn execution_evaluates_arithmetic_cell_arguments() {
        let source = temp_source(
            "arithmetic-args",
            "cell top(x: Float, n: Int, flag: Bool) {\n\
             let h = if flag { 10. } else { 20. };\n\
             let r = rect(\"met1\", x0=x, y0=n as Float, x1=x + 10., y1=n as Float + h);\n\
             }\n",
        );
        let rect = compiled_rect("arithmetic", source, "top(-2.5 * 2., 2 * -1, false)");
        assert_eq!(rect.x0.0, -5.);
        assert_eq!(rect.y0.0, -2.);
        assert_eq!(rect.y1.0, 18.);
    }

    #[test]
    fn execution_evaluates_a_function_call_and_string_cell_argument() {
        let source = temp_source(
            "call-args",
            "fn double(x: Float) -> Float { 2. * x }\n\
             cell top(layer: String, w: Float) {\n\
             let r = rect(layer, x0=0., y0=0., x1=w, y1=10.);\n\
             }\n",
        );
        let rect = compiled_rect("call", source, "top(\"met1\", double(25.))");
        assert_eq!(rect.x1.0, 50.);
        assert_eq!(rect.layer.as_deref(), Some("met1"));
    }

    #[test]
    fn execution_evaluates_a_sequence_cell_argument() {
        let source = temp_source(
            "seq-args",
            "cell top(items: [Float]) {\n\
             let r = rect(\"met1\", x0=0., y0=0., x1=head(items), y1=10.);\n\
             }\n",
        );
        let rect = compiled_rect("seq", source, "top(cons(30., cons(40., [])))");
        assert_eq!(rect.x1.0, 30.);
    }

    #[test]
    fn execution_accepts_an_empty_sequence_cell_argument() {
        let source = temp_source(
            "empty-seq-args",
            "cell top(items: [Float]) {\n\
             let r = rect(\"met1\", x0=0., y0=0., x1=10., y1=20.);\n\
             }\n",
        );
        let rect = compiled_rect("empty-seq", source, "top([])");
        assert_eq!(rect.y1.0, 20.);
    }

    #[test]
    fn execution_evaluates_an_enum_cell_argument() {
        let source = temp_source(
            "enum-args",
            "enum Mode { Fast, Slow, }\n\
             cell top(m: Mode) {\n\
             let w = match m { Mode::Fast => 10., Mode::Slow => 20., };\n\
             let r = rect(\"met1\", x0=0., y0=0., x1=w, y1=10.);\n\
             }\n",
        );
        let rect = compiled_rect("enum", source, "top(Mode::Slow)");
        assert_eq!(rect.x1.0, 20.);
    }

    #[test]
    fn out_of_range_cell_argument_is_reported_cleanly() {
        let source = temp_source("out-of-range-arg", "cell top(n: Int) {}\n");
        let diagnostic = render_failed(failed(execution_args(
            source,
            "top(99999999999999999999)",
            basic_tech(),
        )));
        assert!(
            diagnostic.contains("invalid integer literal `99999999999999999999`"),
            "{diagnostic}"
        );
    }

    #[test]
    fn a_non_cell_invocation_is_reported_cleanly() {
        let source = temp_source(
            "not-a-cell",
            "fn double(x: Float) -> Float { 2. * x }\ncell top() {}\n",
        );
        let diagnostic = render_failed(failed(execution_args(source, "double(2.)", basic_tech())));
        assert!(
            diagnostic.contains("expected type category Cell, found Float"),
            "{diagnostic}"
        );
    }

    #[test]
    fn an_error_in_a_cell_argument_points_at_the_invocation() {
        let source = temp_source("arg-diagnostic", "cell top(x: Float, y: Float) {}\n");
        let diagnostic = render_failed(failed(execution_args(
            source,
            "top(1 + 1, 20.)",
            basic_tech(),
        )));
        assert!(
            diagnostic.contains("expected type Float, found Int"),
            "{diagnostic}"
        );
        // The caret must land on the argument the caller wrote, not on a
        // position in the library source that it was spliced into.
        assert!(diagnostic.contains("--> <argon-cell>:1:5"), "{diagnostic}");
        assert!(diagnostic.contains("1 | top(1 + 1, 20.)"), "{diagnostic}");
        assert!(diagnostic.contains("^^^^^"), "{diagnostic}");
    }

    #[test]
    fn imports_a_gds_cell_at_a_module_path() {
        let source = temp_source(
            "gds-root",
            "use lib::macros::sram;\n\
             cell top() {\n\
             let imported = inst(sram(), x=0., y=0.);\n\
             eq(imported.gds_rect_0.x0, 0.);\n\
             }\n",
        );
        let directory = source.parent().expect("source should have a parent");
        let artifact_path = directory.join("imported.bin");
        let mut args = execution_args(source, "top()", basic_tech());
        args.gds_imports
            .push(("macros::sram".to_owned(), temp_gds("gds-import")));
        args.output = Some(artifact_path.clone());

        if let Err(error) = execute(args) {
            panic!("GDS import should compile: {}", render_failed(error));
        }
        let CompileOutput::Valid(output) =
            artifact::read(artifact_path).expect("artifact should decode")
        else {
            panic!("GDS import should compile successfully");
        };
        let top = &output.cells[&output.top];
        assert_eq!(top.scopes[&top.root].name, "cell top");
        assert_eq!(top.objects.len(), 2);
        let imported = top
            .objects
            .values()
            .find_map(|object| object.get_instance())
            .expect("top should instantiate the imported cell");
        let imported_cell = &output.cells[&imported.cell];
        assert_eq!(imported_cell.objects.len(), 1);
        assert!(imported_cell.fields.contains_key("gds_rect_0"));
    }

    #[test]
    fn imports_and_reexports_a_non_rounded_gds_path() {
        let source = temp_source(
            "gds-path-root",
            "cell top() {\n\
             let imported = inst(routes(), x=0., y=0.);\n\
             eq(imported.gds_path_0.width, 20.);\n\
             eq(imported.gds_path_0.begin_extension, 10.);\n\
             eq(imported.gds_path_0.end_extension, 10.);\n\
             eq(imported.gds_path_0.x1, 100.);\n\
             }\n",
        );
        let directory = source.parent().expect("source should have a parent");
        let artifact_path = directory.join("imported.bin");
        let exported_path = directory.join("roundtrip.gds");
        let mut args = execution_args(source, "top()", basic_tech());
        args.gds_imports
            .push(("routes".to_owned(), temp_path_gds("gds-path-import")));
        args.output = Some(artifact_path.clone());
        args.gds = Some(exported_path.clone());

        if let Err(error) = execute(args) {
            panic!("GDS path import should compile: {}", render_failed(error));
        }
        let CompileOutput::Valid(output) =
            artifact::read(artifact_path).expect("artifact should decode")
        else {
            panic!("GDS path import should compile successfully");
        };
        let imported = output.cells[&output.top]
            .objects
            .values()
            .find_map(|object| object.get_instance())
            .expect("top should instantiate the imported cell");
        let imported_path = output.cells[&imported.cell]
            .objects
            .values()
            .find_map(|object| object.get_path())
            .expect("imported cell should contain a path");
        assert_eq!(imported_path.width.0, 20.);
        assert_eq!(imported_path.begin_extension.0, 10.);
        assert_eq!(imported_path.end_extension.0, 10.);

        let exported = GdsLibrary::load(exported_path).expect("round-trip GDS should load");
        let exported_path = exported
            .structs
            .iter()
            .flat_map(|structure| &structure.elems)
            .find_map(|element| match element {
                GdsElement::GdsPath(path) => Some(path),
                _ => None,
            })
            .expect("round-trip GDS should contain a path");
        assert_eq!(exported_path.width, Some(20));
        assert_eq!(exported_path.path_type, Some(2));
        assert_eq!(exported_path.xy.len(), 3);
    }

    #[test]
    fn constrains_an_imported_gds_shape_field() {
        let source = temp_source(
            "gds-shape-constraint",
            "cell top() {\n\
             let imported = inst(sram());\n\
             eq(imported.gds_rect_0.x0, 100.);\n\
             eq(imported.y, 0.);\n\
             }\n",
        );
        let directory = source.parent().expect("source should have a parent");
        let artifact_path = directory.join("imported.bin");
        let mut args = execution_args(source, "top()", basic_tech());
        args.gds_imports
            .push(("sram".to_owned(), temp_gds("gds-shape-field")));
        args.output = Some(artifact_path.clone());

        if let Err(error) = execute(args) {
            panic!(
                "GDS shape constraint should compile: {}",
                render_failed(error)
            );
        }
        let CompileOutput::Valid(output) =
            artifact::read(artifact_path).expect("artifact should decode")
        else {
            panic!("GDS shape constraint should compile successfully");
        };
        let top = &output.cells[&output.top];
        let imported = top
            .objects
            .values()
            .find_map(|object| object.get_instance())
            .expect("top should instantiate the imported cell");
        assert_eq!(imported.x, 100.);
    }

    #[test]
    fn constrains_a_shape_through_imported_gds_hierarchy() {
        let source = temp_source(
            "nested-gds-shape-constraint",
            "cell top() {\n\
             let imported = inst(sram());\n\
             eq(imported.gds_inst_0.gds_rect_0.x0, 100.);\n\
             eq(imported.y, 0.);\n\
             }\n",
        );
        let directory = source.parent().expect("source should have a parent");
        let artifact_path = directory.join("imported.bin");
        let mut args = execution_args(source, "top()", basic_tech());
        args.gds_imports
            .push(("sram".to_owned(), temp_nested_gds("nested-gds-shape-field")));
        args.output = Some(artifact_path.clone());

        if let Err(error) = execute(args) {
            panic!(
                "nested GDS shape constraint should compile: {}",
                render_failed(error)
            );
        }
        let CompileOutput::Valid(output) =
            artifact::read(artifact_path).expect("artifact should decode")
        else {
            panic!("nested GDS shape constraint should compile successfully");
        };
        let top = &output.cells[&output.top];
        let imported = top
            .objects
            .values()
            .find_map(|object| object.get_instance())
            .expect("top should instantiate the imported cell");
        assert_eq!(imported.x, 100.);
    }

    #[test]
    fn gds_cells_are_declared_before_source_cells() {
        let source = temp_source(
            "gds-declaration-order",
            "cell top() { let imported = inst(sram(), x=0., y=0.); }\n",
        );
        let directory = source.parent().expect("source should have a parent");
        let artifact_path = directory.join("imported.bin");
        let mut args = execution_args(source, "top()", basic_tech());
        args.gds_imports
            .push(("sram".to_owned(), temp_gds("gds-declaration-order-import")));
        args.output = Some(artifact_path.clone());

        if let Err(error) = execute(args) {
            panic!("GDS import should compile: {}", render_failed(error));
        }
        assert!(matches!(
            artifact::read(artifact_path).expect("artifact should decode"),
            CompileOutput::Valid(_)
        ));
    }

    #[test]
    fn invalid_cell_argument_type_is_reported_cleanly() {
        let source = temp_source("invalid-argument-root", "");
        let dependency = temp_source(
            "invalid-argument-dependency",
            "cell device(enabled: Bool, w: Float, count: Int) {}\n",
        );
        let mut args = execution_args(source, "devices::device(1, 150., 5)", basic_tech());
        args.dependencies.push(("devices".to_owned(), dependency));
        args.output = Some(std::env::temp_dir().join("argonc-invalid-argument.bin"));
        let diagnostic = render_failed(failed(args));
        assert!(
            diagnostic.contains("expected type Bool, found Int"),
            "{diagnostic}"
        );
    }

    #[test]
    fn missing_tech_is_reported_with_its_path() {
        let source = temp_source("missing-tech", "cell top() {}\n");
        let missing = source
            .parent()
            .expect("source should have a parent")
            .join("missing.tech.toml");
        let diagnostic = render_failed(failed(execution_args(source, "top()", missing.clone())));
        assert!(
            diagnostic.contains("could not read technology file"),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains(&missing.display().to_string()),
            "{diagnostic}"
        );
    }

    #[test]
    fn malformed_tech_is_reported_with_its_path() {
        let source = temp_source("malformed-tech", "cell top() {}\n");
        let malformed = source
            .parent()
            .expect("source should have a parent")
            .join("malformed.tech.toml");
        fs::write(&malformed, "not valid TOML = [")
            .expect("malformed technology should be written");
        let diagnostic = render_failed(failed(execution_args(source, "top()", malformed.clone())));
        assert!(
            diagnostic.contains("could not parse technology file"),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains(&malformed.display().to_string()),
            "{diagnostic}"
        );
    }

    #[test]
    fn missing_text_layer_is_reported_cleanly() {
        let source = temp_source(
            "missing-text-layer",
            "cell top() {\n    text(\"label\", \"missing.label\", 0., 0.);\n}\n",
        );
        let tech = basic_tech();
        let diagnostic = render_failed(failed(execution_args(source, "top()", tech.clone())));
        assert!(
            diagnostic.contains(&format!(
                "text uses layer `missing.label`, which is not defined in technology file `{}`",
                tech.display()
            )),
            "{diagnostic}"
        );
    }

    #[test]
    fn standard_library_errors_show_embedded_source_lines() {
        let source = temp_source(
            "std-diagnostic",
            r#"cell top() {
    let r = crect(layer="missing.drawing", x0=0., y0=0., w=10., h=10.);
    std::array(r, 2, 20., 0.);
}
"#,
        );
        let tech = basic_tech();
        let diagnostic = render_failed(failed(execution_args(source, "top()", tech.clone())));
        assert!(
            diagnostic.contains(&format!(
                "rectangle uses layer `missing.drawing`, which is not defined in technology file `{}`",
                tech.display()
            )),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains("--> <argon-std>/lib.ar:"),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains("let first_rect = rect(r.layer);"),
            "{diagnostic}"
        );
        assert!(!diagnostic.contains("<argon-std>/lib.ar:1:1"));
    }

    #[test]
    fn help_uses_dependency_terminology() {
        let help = Args::command().render_long_help().to_string();
        assert!(help.contains("<ROOT>"), "{help}");
        assert!(!help.contains("<INPUTS>..."), "{help}");
        assert!(!help.contains("path modules"), "{help}");
        assert!(help.contains("--dependency"), "{help}");
        assert!(help.contains("--gds-import"), "{help}");
        assert!(!help.contains("--extern"), "{help}");
    }
}
