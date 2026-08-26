use std::{
    collections::HashSet,
    path::{Path, PathBuf},
    process::ExitCode,
};

use crate::{
    artifact,
    compile::{self, CellArg, CompileInput, CompileOutput},
    diagnostics::{self, Diagnostic},
    parse::{self, parse_workspace_with_std_deps_and_gds},
};
use clap::{Parser, ValueEnum};
use itertools::Itertools;

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

    let analysis = compile::analyze_workspace(parse_workspace_with_std_deps_and_gds(
        &root,
        args.dependencies,
        args.gds_imports.clone(),
    ));
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
        ));
    }
    if args.check {
        return Ok(());
    }

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
    let cell_ast = parse::parse_cell(cell)
        .map_err(|error| fail(format, format!("invalid cell invocation: {error}")))?;
    if !cell_ast.args.kwargs.is_empty() {
        return Err(fail(
            format,
            "keyword arguments are not supported in --cell yet",
        ));
    }
    let cell_path = cell_ast
        .func
        .path
        .iter()
        .map(|ident| ident.name)
        .collect_vec();
    let cell_args = cell_ast
        .args
        .posargs
        .iter()
        .map(CellArg::from_literal)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| {
            fail(
                format,
                "--cell arguments must be integer, float, boolean, or empty-list literals",
            )
        })?;
    let output = compile::dynamic_compile_with_gds(
        &typed_ast,
        CompileInput {
            cell: &cell_path,
            args: cell_args,
            tech_file: tech,
        },
        &args.gds_imports,
    );
    if !matches!(output, CompileOutput::Valid(_)) {
        return Err(compile_failed(format, output));
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

fn compile_failed(format: ErrorFormat, output: CompileOutput) -> Failed {
    Failed(format, diagnostics::from_compile_output(&output))
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        path::PathBuf,
        time::{SystemTime, UNIX_EPOCH},
    };

    use crate::{artifact, compile::CompileOutput};
    use clap::{CommandFactory, Parser};
    use gds::{GdsBoundary, GdsElement, GdsLibrary, GdsPoint, GdsStruct};

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
            diagnostic.contains("invalid cell argument 1: expected Bool, found Int"),
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
