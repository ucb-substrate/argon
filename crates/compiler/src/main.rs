use std::{
    collections::HashSet,
    ffi::OsStr,
    path::{Path, PathBuf},
    process::ExitCode,
};

use argonc::{
    artifact,
    ast::Expr,
    compile::{self, CellArg, CompileInput, CompileOutput},
    diagnostics::{self, Diagnostic},
    gds::GdsMap,
    parse::{self, parse_workspace_with_std_and_deps},
};
use clap::{Parser, ValueEnum};
use gds::GdsUnits;
use itertools::Itertools;

#[derive(Debug, Clone, Copy, ValueEnum)]
enum ErrorFormat {
    Human,
    Json,
}

#[derive(Debug, Parser)]
#[command(version, about = "The Argon compiler")]
struct Args {
    /// Root Argon source file. Additional files are compiled as path modules.
    #[arg(required = true)]
    inputs: Vec<PathBuf>,

    /// Cell invocation to instantiate, for example `top(10., 20.)`.
    #[arg(long)]
    cell: Option<String>,

    /// KLayout layer-properties file. Required when a cell is instantiated.
    #[arg(long, requires = "cell")]
    lyp: Option<PathBuf>,

    /// Path dependency in NAME=PATH form. PATH may be a directory or lib.ar.
    #[arg(long = "extern", value_parser = parse_extern)]
    dependencies: Vec<(String, PathBuf)>,

    /// Binary compiler-output path. Defaults to the root source with a .bin suffix.
    #[arg(short, long, requires = "cell")]
    output: Option<PathBuf>,

    /// Also emit GDS to this path.
    #[arg(long, requires = "cell")]
    gds: Option<PathBuf>,

    /// Run all non-executing compiler stages, then stop.
    #[arg(long, conflicts_with_all = ["cell", "lyp", "output", "gds"])]
    check: bool,

    /// Diagnostic output format.
    #[arg(long, value_enum, default_value = "human")]
    error_format: ErrorFormat,
}

fn main() -> ExitCode {
    let args = Args::parse();
    let format = args.error_format;
    let previous_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(|_| {}));
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(args)));
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

fn run(args: Args) -> Result<(), Failed> {
    let format = args.error_format;
    let root = source_root(&args.inputs[0]);
    let mut dependencies = args.dependencies;
    let mut names: HashSet<String> = dependencies.iter().map(|(name, _)| name.clone()).collect();
    for input in args.inputs.iter().skip(1) {
        let Some(stem) = dependency_name(input) else {
            return Err(fail(
                format,
                format!("cannot derive a module name from `{}`", input.display()),
            ));
        };
        if !names.insert(stem.to_string()) {
            return Err(fail(format, format!("duplicate path module `{stem}`")));
        }
        dependencies.push((stem.to_string(), input.clone()));
    }

    let parse_output = parse_workspace_with_std_and_deps(&root, dependencies);
    let parse_errors = parse_output.static_errors();
    let ast = parse_output.ast();
    let Some((typed_ast, mut static_output)) = compile::static_compile(&ast) else {
        return Err(fail(
            format,
            format!("could not parse library root `{}`", root.display()),
        ));
    };
    static_output.errors.extend(parse_errors);
    if !static_output.errors.is_empty() {
        return Err(compile_failed(
            format,
            CompileOutput::StaticErrors(static_output),
        ));
    }
    if args.check {
        return Ok(());
    }

    let Some(cell) = args.cell.as_deref() else {
        return Err(fail(format, "either --check or --cell is required"));
    };
    let Some(lyp) = args.lyp.as_deref() else {
        return Err(fail(
            format,
            "--lyp is required when compiling a cell; pass the path to a KLayout layer-properties file",
        ));
    };
    argonc::layer::read_lyp(lyp).map_err(|error| fail(format, error.to_string()))?;
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
        .map(cell_arg)
        .collect::<Result<Vec<_>, _>>()
        .map_err(|message| fail(format, message))?;
    let output = compile::dynamic_compile(
        &typed_ast,
        CompileInput {
            cell: &cell_path,
            args: cell_args,
            lyp_file: lyp,
        },
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
        let map = GdsMap::from_lyp(lyp).map_err(|error| {
            fail(
                format,
                format!("could not read `{}`: {error}", lyp.display()),
            )
        })?;
        output
            .to_gds(map, GdsUnits::new(1e-3, 1e-9), &gds_path)
            .map_err(|error| {
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

fn dependency_name(path: &Path) -> Option<&str> {
    if path.file_name() == Some(OsStr::new("lib.ar")) {
        path.parent()?.file_name()?.to_str()
    } else {
        path.file_stem()?.to_str()
    }
}

fn cell_arg(expr: &Expr<&str, parse::ParseMetadata>) -> Result<CellArg, String> {
    match expr {
        Expr::FloatLiteral(value) => Ok(CellArg::Float(value.value)),
        Expr::IntLiteral(value) => Ok(CellArg::Int(value.value)),
        Expr::BoolLiteral(value) => Ok(CellArg::Bool(value.value)),
        Expr::SeqNil(_) => Ok(CellArg::Seq(Vec::new())),
        _ => Err("--cell arguments must be integer, float, boolean, or empty-list literals".into()),
    }
}

fn parse_extern(value: &str) -> Result<(String, PathBuf), String> {
    let (name, path) = value
        .split_once('=')
        .ok_or_else(|| "expected NAME=PATH".to_string())?;
    if name.is_empty() || path.is_empty() {
        return Err("expected non-empty NAME and PATH".to_string());
    }
    Ok((name.to_string(), PathBuf::from(path)))
}

fn fail(format: ErrorFormat, message: impl Into<String>) -> Failed {
    Failed(format, vec![Diagnostic::error(message)])
}

fn compile_failed(format: ErrorFormat, output: CompileOutput) -> Failed {
    Failed(format, diagnostics::from_compile_output(&output))
}
