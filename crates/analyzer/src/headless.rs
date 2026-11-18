//! The tools `argon-analyzer mcp --headless` serves. They compile the library
//! on disk with [`argonc::driver`], as `arc run` does, so agents edit files
//! with their own tools and check exactly what `arc run` exports.

use std::{
    cell::RefCell,
    collections::{HashMap, HashSet},
    fmt::Write as _,
    fs, io,
    panic::{self, AssertUnwindSafe},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

use arcstr::ArcStr;
use argonc::{
    COMPILE_STACK_SIZE, WorkspaceConfig,
    ast::{Decl, Span, TySpecKind},
    compile::{
        Arrayed, CellId, CompileOutput, CompiledCell, CompiledData, CompiledEmit, FieldValue,
        ObjectId, SolvedInstance, SolvedValue,
    },
    diagnostics::Diagnostic,
    driver::{self, Sources},
    netlist::{NetlistFormat, NetlistOptions},
    tech::Technology,
};
use serde_json::{Value, json};

use crate::mcp::{Tools, string_argument};

const INSTRUCTIONS: &str = "These tools compile the Argon library on disk with the same compiler as \
`arc run`, so edit .ar files with your own tools and call the tools after each edit. Use compile to \
check cells, measure them, and export their GDS and SPICE for pv.py, passing a whole sweep in one \
call; inspect to read values the solver determined; locate to find the shapes at DRC violations \
and the source lines that drew them; and nets to measure each net's wiring and rough capacitance. Coordinates are in the technology's source units, nm for \
SKY130. Each tool uses the library whose Argon.toml is in your working directory or above it, or \
the one `workspace` names; other paths, in arguments and in results, are relative to that \
library's directory.";

/// Shapes listed for one `locate` box before the rest are counted.
const MAX_SHAPES_PER_BOX: usize = 20;

/// Items of a list value shown by `inspect` before the rest are counted.
const MAX_LIST_ITEMS: usize = 50;

/// The most points one `sweep` argument expands to.
const MAX_SWEEP_POINTS: usize = 4096;

/// Cells named in a list of a sweep's results before the rest are counted.
const MAX_LISTED_CELLS: usize = 8;

/// Calls named for one diagnostic before the rest are counted.
const MAX_LISTED_CALLS: usize = 5;

/// One cell's result in `compile`.
enum CellReport {
    Compiled {
        /// Its size and bounding box, export, and layer areas, as one entry.
        line: String,
        /// Width and height in µm, if it has a layout.
        size: Option<(f64, f64)>,
        /// Whether a sweep's summary shows it in full: some coordinates are
        /// set only by initial conditions, or its export failed.
        notable: bool,
    },
    Failed(Vec<Rendered>),
    /// The compiler panicked.
    Internal,
}

/// A diagnostic as `path:line:column: message`, with the calls in the top
/// cell's body that led to it.
#[derive(Clone, PartialEq, Eq)]
struct Rendered {
    line: String,
    calls: Vec<String>,
}

/// Writes each of `rendered` with its calls on the line after it.
fn write_rendered(text: &mut String, rendered: &[Rendered], indent: &str) {
    for diagnostic in rendered {
        let _ = writeln!(text, "{indent}{}", diagnostic.line);
        if !diagnostic.calls.is_empty() {
            let _ = writeln!(
                text,
                "{indent}  called from {}",
                english_list(&diagnostic.calls, MAX_LISTED_CALLS)
            );
        }
    }
}

/// Cell invocations as an English list.
fn listed(cells: &[&str]) -> String {
    english_list(cells, MAX_LISTED_CELLS)
}

/// `items` as an English list, naming at most `named` before counting the rest.
fn english_list(items: &[impl AsRef<str>], named: usize) -> String {
    let mut shown = items
        .iter()
        .take(named)
        .map(|item| item.as_ref().to_owned())
        .collect::<Vec<_>>();
    if items.len() > named {
        shown.push(format!("{} more", items.len() - named));
    }
    match shown.as_slice() {
        [] => String::new(),
        [one] => one.clone(),
        [first, second] => format!("{first} and {second}"),
        [rest @ .., last] => format!("{}, and {last}", rest.join(", ")),
    }
}

/// The range of `values`, such as `2.35–4.23`, or one number when they agree.
fn range(values: impl Iterator<Item = f64> + Clone) -> String {
    let low = number(values.clone().fold(f64::INFINITY, f64::min));
    let high = number(values.fold(f64::NEG_INFINITY, f64::max));
    if low == high {
        low
    } else {
        format!("{low}–{high}")
    }
}

/// A parameter of a cell, as a sweep passes it.
struct Param {
    name: String,
    /// The name of its declared type, such as `Float`; empty for a sequence
    /// or tuple type.
    ty: String,
    /// Whether it has a default, which lets it be passed by keyword.
    default: bool,
}

/// The parameters of the cell at `path`, such as `latch` or `sky130::mos`.
fn cell_params(workspace: &WorkspaceConfig, path: &str) -> Result<Vec<Param>, String> {
    let parsed = argonc::parse::parse_workspace_with_config_and_sources(workspace, &Sources::new());
    if !parsed.static_errors().is_empty() {
        return Err(format!(
            "The library has parse errors, so `sweep` can't read `{path}`'s parameters. Compile it with `cells` to see them."
        ));
    }
    let ast = parsed.ast();
    let segments = path.split("::").map(str::trim).collect::<Vec<_>>();
    let (name, module) = segments.split_last().expect("split yields a segment");
    let module = module
        .iter()
        .skip_while(|segment| **segment == "lib")
        .map(|segment| segment.to_string())
        .collect::<Vec<_>>();
    let cell = ast
        .get(&module)
        .and_then(|module| {
            module.ast.decls.iter().find_map(|decl| match decl {
                Decl::Cell(cell) if cell.name.name.as_str() == *name => Some(cell),
                _ => None,
            })
        })
        .ok_or_else(|| format!("No cell `{path}` in the library."))?;
    Ok(cell
        .args
        .iter()
        .map(|arg| Param {
            name: arg.name.name.to_string(),
            ty: match &arg.ty.kind {
                TySpecKind::Path { name, args } if args.is_empty() => name.name.to_string(),
                _ => String::new(),
            },
            default: arg.default.is_some(),
        })
        .collect())
}

/// `value` as Argon source for a parameter of type `ty`: a number in the
/// type's form, a boolean, a string quoted for a `String` parameter, and any
/// other string as source, such as `Mode::Fast`.
fn argon_literal(value: &Value, ty: &str) -> Result<String, String> {
    match value {
        Value::Number(number) => {
            let float = number
                .as_f64()
                .filter(|float| float.is_finite())
                .ok_or_else(|| format!("{number} is not a finite number"))?;
            match ty {
                "Float" if float.fract() == 0. && float.abs() < 1e15 => {
                    Ok(format!("{}.", float as i64))
                }
                "Float" => Ok(format!("{float}")),
                "Int" => number
                    .as_i64()
                    .or_else(|| (float.fract() == 0. && float.abs() < 9e15).then_some(float as i64))
                    .map(|int| int.to_string())
                    .ok_or_else(|| format!("{number} is not an Int")),
                _ => Ok(number.to_string()),
            }
        }
        Value::Bool(value) => Ok(value.to_string()),
        Value::String(text) if ty == "String" => Ok(format!(
            "\"{}\"",
            text.replace('\\', "\\\\").replace('"', "\\\"")
        )),
        Value::String(source) => Ok(source.clone()),
        _ => Err(format!("{value} is not a number, a boolean, or a string")),
    }
}

/// The headless tools, answering for libraries under `directory`.
pub struct Headless {
    directory: PathBuf,
}

impl Headless {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }
}

impl Tools for Headless {
    fn instructions(&self) -> &'static str {
        INSTRUCTIONS
    }

    fn definitions(&self) -> Value {
        tool_definitions()
    }

    async fn run_tool(&self, name: &str, arguments: &Value) -> Result<String, String> {
        let directory = self.directory.clone();
        let arguments = arguments.clone();
        let name = name.to_owned();
        // Compiling blocks, so it runs off the async runtime's threads.
        tokio::task::spawn_blocking(move || {
            let call = Call::new(&directory, arguments);
            match name.as_str() {
                "compile" => call?.compile(),
                "inspect" => call?.inspect(),
                "locate" => call?.locate(),
                "nets" => call?.nets(),
                _ => Err(format!("Unknown tool `{name}`.")),
            }
        })
        .await
        .unwrap_or_else(|_| Err(INTERNAL_ERROR.to_owned()))
    }
}

const INTERNAL_ERROR: &str = "internal compiler error";

/// One tool call: its arguments and the library it is for.
struct Call {
    /// The library's directory, which relative paths start from.
    directory: PathBuf,
    workspace: WorkspaceConfig,
    arguments: Value,
}

impl Call {
    /// The call for the library whose manifest is in the directory
    /// `workspace` names, relative to `directory`, or in `directory` or the
    /// nearest directory above it.
    fn new(directory: &Path, arguments: Value) -> Result<Self, String> {
        let start = match arguments.get("workspace").and_then(Value::as_str) {
            Some(workspace) => crate::agent::normalize(&directory.join(workspace)),
            None => directory.to_path_buf(),
        };
        let manifest = arc::find_manifest_path(&start).map_err(|_| {
            format!(
                "No Argon.toml in {} or any directory above it. Pass `workspace` with the library's directory.",
                start.display()
            )
        })?;
        let library = arc::Library::load(&manifest).map_err(|error| format!("{error:#}"))?;
        if library.tech.is_none() {
            return Err(format!(
                "{} does not set `tech`; add `tech = \"path/to/tech.toml\"`.",
                manifest.display()
            ));
        }
        Ok(Self {
            directory: library.directory().to_path_buf(),
            workspace: crate::workspace_config(library.root.clone(), Some(&library)),
            arguments,
        })
    }

    fn path(&self, path: &str) -> PathBuf {
        crate::agent::normalize(&self.directory.join(path))
    }

    fn strings(&self, name: &str) -> Result<Vec<String>, String> {
        let Some(value) = self.arguments.get(name) else {
            return Ok(Vec::new());
        };
        value
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .map(|item| item.as_str().map(str::to_owned))
                    .collect()
            })
            .ok_or_else(|| format!("`{name}` must be a list of strings."))
    }

    /// `path` relative to the library's directory when it is inside it.
    fn show(&self, path: &Path) -> String {
        path.strip_prefix(&self.directory)
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .unwrap_or(path)
            .display()
            .to_string()
    }

    fn compile(&self) -> Result<String, String> {
        let workspace = &self.workspace;
        let mut cells = self.strings("cells")?;
        if let Some(sweep) = self.arguments.get("sweep") {
            cells.extend(self.sweep(sweep)?);
        }
        let mut unique = HashSet::new();
        cells.retain(|cell| unique.insert(cell.clone()));
        if cells.is_empty() {
            let diagnostics = parallel(&[()], |_| driver::check(workspace, &Sources::new()))
                .remove(0)
                .ok_or(INTERNAL_ERROR)?;
            return if diagnostics.is_empty() {
                Ok("The library parses and type-checks.".to_owned())
            } else {
                Err(self.diagnostics(&diagnostics, ""))
            };
        }
        let export = self
            .arguments
            .get("export")
            .and_then(Value::as_str)
            .map(|directory| self.path(directory));
        let flag = |name: &str| {
            self.arguments
                .get(name)
                .and_then(Value::as_bool)
                .unwrap_or(false)
        };
        let layers = flag("layers");
        let verbose = flag("verbose") || layers || cells.len() == 1;
        let names = cells
            .iter()
            .map(|cell| export_name(cell))
            .collect::<Vec<_>>();
        if export.is_some() {
            let mut seen = HashMap::new();
            for (cell, name) in cells.iter().zip(&names) {
                if let Some(other) = seen.insert(name, cell) {
                    return Err(format!(
                        "`{other}` and `{cell}` would both export to {name}/; compile them in separate calls."
                    ));
                }
            }
        }
        let jobs = cells.iter().zip(&names).collect::<Vec<_>>();
        let reports = parallel(&jobs, |(cell, name)| {
            let destination = export.as_ref().map(|directory| directory.join(name));
            if let Some(destination) = &destination {
                // Old files would pass for this export if it fails.
                for file in ["argon.gds", "argon.spice"] {
                    let _ = fs::remove_file(destination.join(file));
                }
            }
            let output = driver::compile_cell(workspace, &Sources::new(), cell, true);
            self.report(output, destination.as_deref(), layers)
        });
        let reports = reports
            .into_iter()
            .map(|report| report.unwrap_or(CellReport::Internal))
            .collect::<Vec<_>>();
        let text = if verbose {
            let mut text = String::new();
            for (cell, report) in cells.iter().zip(&reports) {
                match report {
                    CellReport::Compiled { line, .. } => {
                        let _ = writeln!(text, "{cell}: {line}");
                    }
                    CellReport::Failed(rendered) => {
                        let _ = writeln!(text, "{cell}: error");
                        write_rendered(&mut text, rendered, "  ");
                    }
                    CellReport::Internal => {
                        let _ = writeln!(text, "{cell}: {INTERNAL_ERROR}");
                    }
                }
            }
            text
        } else {
            self.sweep_summary(&cells, &reports, export.as_deref())
        };
        Ok(text.trim_end().to_owned())
    }

    /// The invocations of a `sweep` argument: its cell with every combination
    /// of its parameters' values, each written as the parameter's declared
    /// type needs.
    fn sweep(&self, sweep: &Value) -> Result<Vec<String>, String> {
        let cell = sweep
            .get("cell")
            .and_then(Value::as_str)
            .ok_or("`sweep` needs `cell`, the cell's name, such as `latch`.")?;
        let params = sweep.get("params").ok_or(
            "`sweep` needs `params`: a list of value lists, one per positional parameter, or an object of value lists by parameter name.",
        )?;
        let declared = cell_params(&self.workspace, cell)?;
        // Each parameter given, in declaration order, with whether it is passed
        // by keyword and its values.
        let given = match params {
            Value::Array(lists) => {
                if lists.len() > declared.len() {
                    return Err(format!(
                        "`{cell}` takes {} parameters, but `sweep` gives {}.",
                        declared.len(),
                        lists.len()
                    ));
                }
                declared
                    .iter()
                    .zip(lists)
                    .map(|(param, values)| (param, false, values))
                    .collect::<Vec<_>>()
            }
            Value::Object(lists) => {
                if let Some(name) = lists
                    .keys()
                    .find(|name| declared.iter().all(|param| &param.name != *name))
                {
                    return Err(format!("`{cell}` has no parameter `{name}`."));
                }
                let mut given = Vec::new();
                for param in &declared {
                    match lists.get(&param.name) {
                        // A parameter without a default can only be positional.
                        Some(values) => given.push((param, param.default, values)),
                        None if !param.default => {
                            return Err(format!(
                                "`sweep` must give `{}`, which has no default.",
                                param.name
                            ));
                        }
                        None => {}
                    }
                }
                given
            }
            _ => {
                return Err(
                    "`sweep`'s `params` must be a list of lists or an object of lists.".to_owned(),
                );
            }
        };
        // Each parameter's `name=` prefix, empty when positional, and its literals.
        let mut axes = Vec::<(String, Vec<String>)>::new();
        for (param, keyword, values) in given {
            let values = values
                .as_array()
                .filter(|values| !values.is_empty())
                .ok_or_else(|| format!("`sweep` gives `{}` no list of values.", param.name))?;
            let literals = values
                .iter()
                .map(|value| {
                    argon_literal(value, &param.ty)
                        .map_err(|error| format!("`{}`: {error}", param.name))
                })
                .collect::<Result<Vec<_>, _>>()?;
            let prefix = if keyword {
                format!("{}=", param.name)
            } else {
                String::new()
            };
            axes.push((prefix, literals));
        }
        let points = axes
            .iter()
            .try_fold(1usize, |count, (_, values)| count.checked_mul(values.len()))
            .filter(|count| *count <= MAX_SWEEP_POINTS)
            .ok_or_else(|| {
                format!(
                    "`sweep` makes more than {MAX_SWEEP_POINTS} points; split it into several calls."
                )
            })?;
        Ok((0..points)
            .map(|mut index| {
                // The last parameter varies fastest.
                let mut args = vec![String::new(); axes.len()];
                for (arg, (prefix, values)) in args.iter_mut().zip(&axes).rev() {
                    *arg = format!("{prefix}{}", values[index % values.len()]);
                    index /= values.len();
                }
                format!("{cell}({})", args.join(", "))
            })
            .collect())
    }

    /// A sweep's results: one line for the cells that compile, those that
    /// need a look in full, and each distinct error once with its cells.
    fn sweep_summary(
        &self,
        cells: &[String],
        reports: &[CellReport],
        export: Option<&Path>,
    ) -> String {
        let mut text = String::new();
        let sized = cells
            .iter()
            .zip(reports)
            .filter_map(|(cell, report)| match report {
                CellReport::Compiled { size, .. } => Some((cell.as_str(), *size)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let failed = cells
            .iter()
            .zip(reports)
            .filter(|(_, report)| !matches!(report, CellReport::Compiled { .. }))
            .collect::<Vec<_>>();
        if sized.is_empty() {
            let _ = writeln!(text, "None of the {} cells compile.", cells.len());
        } else {
            let mut line = if failed.is_empty() {
                format!("All {} cells compile", cells.len())
            } else {
                format!("{} of {} cells compile", sized.len(), cells.len())
            };
            let sizes = sized
                .iter()
                .filter_map(|(cell, size)| Some((*cell, (*size)?)))
                .collect::<Vec<_>>();
            let by_area = |a: &&(&str, (f64, f64)), b: &&(&str, (f64, f64))| {
                (a.1.0 * a.1.1).total_cmp(&(b.1.0 * b.1.1))
            };
            if let (Some(smallest), Some(largest)) =
                (sizes.iter().min_by(by_area), sizes.iter().max_by(by_area))
            {
                let _ = write!(
                    line,
                    ": {} × {} µm; {:.2} µm² at {} to {:.2} µm² at {}",
                    range(sizes.iter().map(|(_, size)| size.0)),
                    range(sizes.iter().map(|(_, size)| size.1)),
                    smallest.1.0 * smallest.1.1,
                    smallest.0,
                    largest.1.0 * largest.1.1,
                    largest.0,
                );
            }
            if let Some(export) = export {
                let _ = write!(line, "; each exported to {}/<name>/", self.show(export));
            }
            let _ = writeln!(text, "{line}.");
        }
        for (cell, report) in cells.iter().zip(reports) {
            if let CellReport::Compiled {
                line,
                notable: true,
                ..
            } = report
            {
                let _ = writeln!(text, "{cell}: {line}");
            }
        }
        if failed.is_empty() {
            return text;
        }
        let names = failed
            .iter()
            .map(|(cell, _)| cell.as_str())
            .collect::<Vec<_>>();
        let _ = writeln!(
            text,
            "{} {}: {}",
            failed.len(),
            if failed.len() == 1 {
                "cell fails"
            } else {
                "cells fail"
            },
            listed(&names)
        );
        // Each distinct diagnostic once, with the failing cells that report it.
        let mut groups = Vec::<(Rendered, Vec<&str>)>::new();
        for (cell, report) in &failed {
            let rendered = match report {
                CellReport::Failed(rendered) => rendered.clone(),
                _ => vec![Rendered {
                    line: INTERNAL_ERROR.to_owned(),
                    calls: Vec::new(),
                }],
            };
            for diagnostic in rendered {
                match groups.iter_mut().find(|(seen, _)| *seen == diagnostic) {
                    Some((_, group)) => group.push(cell),
                    None => groups.push((diagnostic, vec![cell])),
                }
            }
        }
        for (diagnostic, group) in groups {
            write_rendered(&mut text, std::slice::from_ref(&diagnostic), "");
            if failed.len() > 1 {
                if group.len() == failed.len() {
                    let _ = writeln!(text, "  in all {} failing cells", group.len());
                } else {
                    let _ = writeln!(text, "  in {}", listed(&group));
                }
            }
        }
        text
    }

    /// One cell's result in `compile`, exporting it to `destination`.
    fn report(
        &self,
        output: Result<CompileOutput, Vec<Diagnostic>>,
        destination: Option<&Path>,
        layers: bool,
    ) -> CellReport {
        let output = match output {
            Ok(output) => output,
            Err(diagnostics) => return CellReport::Failed(self.render(&diagnostics)),
        };
        let CompileOutput::Valid(data) = &output else {
            return CellReport::Internal;
        };
        let mut line = summary(data);
        let mut notable = !data.cells[&data.top].unsolved_vars.is_empty();
        if let Some(destination) = destination {
            match export(&output, destination) {
                Ok(()) => {
                    let _ = write!(line, " → {}/", self.show(destination));
                }
                Err(error) => {
                    notable = true;
                    let _ = write!(line, "; export failed: {error}");
                }
            }
        }
        if layers {
            let _ = write!(line, "\n  {}", layer_report(data));
        }
        let show = |path: &Path| self.show(path);
        let (nets, problems) =
            crate::nets::connectivity_lines(data, &crate::nets::Sources::new(&show), "  ");
        if !nets.is_empty() {
            let _ = write!(line, "\n{}", nets.trim_end());
        }
        // A cell with connectivity problems is listed in full in a summary.
        notable |= problems > 0;
        let um = um_per_unit(&data.tech);
        let size = data
            .layout_bbox(data.top)
            .map(|bbox| ((bbox.x1 - bbox.x0) * um, (bbox.y1 - bbox.y0) * um));
        CellReport::Compiled {
            line,
            size,
            notable,
        }
    }

    fn nets(&self) -> Result<String, String> {
        let cell = string_argument(&self.arguments, "cell")?;
        let only = self.strings("nets")?;
        let pairs = match self.arguments.get("pairs") {
            None => Vec::new(),
            Some(value) => value
                .as_array()
                .and_then(|pairs| {
                    pairs
                        .iter()
                        .map(|pair| match pair.as_array()?.as_slice() {
                            [a, b] => Some((a.as_str()?.to_owned(), b.as_str()?.to_owned())),
                            _ => None,
                        })
                        .collect::<Option<Vec<_>>>()
                })
                .ok_or("`pairs` must be a list of two-name lists.")?,
        };
        let output = self.compile_cell(&self.workspace, &Sources::new(), cell)?;
        let CompileOutput::Valid(data) = &output else {
            return Err(INTERNAL_ERROR.to_owned());
        };
        let show = |path: &Path| self.show(path);
        Ok(crate::nets::nets_report(
            data,
            &crate::nets::Sources::new(&show),
            &only,
            &pairs,
        ))
    }

    /// Each diagnostic as `path:line:column: message`, with the calls it ran under.
    fn diagnostics(&self, diagnostics: &[Diagnostic], indent: &str) -> String {
        let mut text = String::new();
        write_rendered(&mut text, &self.render(diagnostics), indent);
        text
    }

    /// `diagnostics` as lines, each distinct line once with every call that
    /// led to it.
    fn render(&self, diagnostics: &[Diagnostic]) -> Vec<Rendered> {
        let files = Files::default();
        let mut rendered = Vec::<Rendered>::new();
        for diagnostic in diagnostics {
            let location = match (&diagnostic.path, diagnostic.start) {
                (Some(path), Some(start)) => {
                    let position = match &diagnostic.source {
                        Some(source) => line_column(source, start),
                        None => files.line_column(path, start),
                    };
                    format!("{}:{}:{}: ", self.show(path), position.0, position.1)
                }
                _ => String::new(),
            };
            let line = format!("{location}{}", diagnostic.message);
            let call = diagnostic.called_at.as_ref().map(|call| {
                let (line, column) = files.line_column(&call.path, call.start);
                format!("{}:{line}:{column}", self.show(&call.path))
            });
            let index = match rendered.iter().position(|seen| seen.line == line) {
                Some(index) => index,
                None => {
                    rendered.push(Rendered {
                        line,
                        calls: Vec::new(),
                    });
                    rendered.len() - 1
                }
            };
            let calls = &mut rendered[index].calls;
            if let Some(call) = call
                && !calls.contains(&call)
            {
                calls.push(call);
            }
        }
        rendered
    }

    /// Compiles `cell` with `sources` on the compiler's stack.
    fn compile_cell(
        &self,
        workspace: &WorkspaceConfig,
        sources: &Sources,
        cell: &str,
    ) -> Result<CompileOutput, String> {
        parallel(&[()], |_| {
            driver::compile_cell(workspace, sources, cell, true)
        })
        .remove(0)
        .ok_or_else(|| INTERNAL_ERROR.to_owned())?
        .map_err(|diagnostics| format!("{cell}: error\n{}", self.diagnostics(&diagnostics, "  ")))
    }

    fn inspect(&self) -> Result<String, String> {
        let workspace = &self.workspace;
        let cell = string_argument(&self.arguments, "cell")?;
        let exprs = self.strings("exprs")?;
        if exprs.is_empty() {
            return Err("Pass at least one expression in `exprs`.".to_owned());
        }
        let output = self.compile_cell(workspace, &Sources::new(), cell)?;
        let CompileOutput::Valid(data) = &output else {
            return Err(INTERNAL_ERROR.to_owned());
        };
        let top = &data.cells[&data.top];
        let body = top.scope_span(top.root).clone();
        let lets = top
            .fields
            .keys()
            .chain(&top.private_fields)
            .map(|name| format!("`{name}`"))
            .collect::<Vec<_>>();
        let text = fs::read_to_string(&body.path).map_err(|error| {
            format!(
                "Could not read {}, which declares the cell: {error}",
                self.show(&body.path)
            )
        })?;
        let prefix = reserved_prefix(&text);
        // Before the closing brace of the cell's body.
        let insert_at = body.span.end() - 1;

        let mut results: Vec<Option<String>> = vec![None; exprs.len()];
        let mut pending = (0..exprs.len()).collect::<Vec<_>>();
        while !pending.is_empty() {
            let mut overlay = text[..insert_at].to_owned();
            // Each expression's statement, and where the expression starts.
            let mut ranges = Vec::new();
            for &index in &pending {
                let statement = overlay.len();
                overlay.push_str(&format!("\n    pub let {prefix}{index} = ("));
                let start = overlay.len();
                overlay.push_str(&exprs[index]);
                overlay.push_str(");");
                ranges.push((index, statement..overlay.len(), start));
            }
            overlay.push('\n');
            overlay.push_str(&text[insert_at..]);
            let sources = Sources::from_iter([(body.path.to_path_buf(), ArcStr::from(overlay))]);
            let diagnostics = match parallel(&[()], |_| {
                driver::compile_cell(workspace, &sources, cell, true).map(|output| {
                    let CompileOutput::Valid(data) = &output else {
                        return Vec::new();
                    };
                    let top = &data.cells[&data.top];
                    ranges
                        .iter()
                        .map(|(index, _, _)| {
                            let field = top.fields.get(&format!("{prefix}{index}"));
                            field.map_or_else(
                                || "no value".to_owned(),
                                |field| format_field(data, top, field),
                            )
                        })
                        .collect()
                })
            })
            .remove(0)
            .ok_or(INTERNAL_ERROR)?
            {
                Ok(values) => {
                    for ((index, _, _), value) in ranges.iter().zip(values) {
                        results[*index] = Some(format!("{} = {value}", exprs[*index]));
                    }
                    break;
                }
                Err(diagnostics) => diagnostics,
            };
            // Blame each error on the expression it lands in, or on the one
            // whose call led to it, then try again without those expressions.
            let files = Files::default();
            let probe = |location: Option<(&Path, usize)>| {
                let (_, offset) = location.filter(|(path, _)| *path == &*body.path)?;
                ranges
                    .iter()
                    .find(|(_, statement, _)| statement.contains(&offset))
                    .map(|(index, _, start)| (*index, offset.saturating_sub(*start) + 1))
            };
            let mut failed = HashSet::new();
            for diagnostic in &diagnostics {
                let primary = diagnostic.path.as_deref().zip(diagnostic.start);
                let call = diagnostic
                    .called_at
                    .as_ref()
                    .map(|call| (call.path.as_path(), call.start));
                let (index, message) = if let Some((index, column)) = probe(primary) {
                    (
                        index,
                        format!("error at column {column}: {}", diagnostic.message),
                    )
                } else if let Some((index, _)) = probe(call) {
                    let location = primary.map_or_else(String::new, |(path, offset)| {
                        let (line, column) = files.line_column(path, offset);
                        format!(" in {}:{line}:{column}", self.show(path))
                    });
                    (index, format!("error{location}: {}", diagnostic.message))
                } else {
                    continue;
                };
                if failed.insert(index) {
                    results[index] = Some(format!("{}: {message}", exprs[index]));
                }
            }
            if failed.is_empty() {
                return Err(format!(
                    "{cell}: error with the expressions added\n{}",
                    self.diagnostics(&diagnostics, "  ")
                ));
            }
            pending.retain(|index| !failed.contains(index));
        }
        let mut text = results
            .into_iter()
            .map(|result| result.unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n");
        if text.contains("is not declared in this scope") {
            let _ = write!(
                text,
                "\nIn scope are {}'s parameters and its top-level `let`s: {}.",
                top.name,
                lets.join(", ")
            );
        }
        Ok(text)
    }

    fn locate(&self) -> Result<String, String> {
        let workspace = &self.workspace;
        let cell = string_argument(&self.arguments, "cell")?;
        let boxes = self
            .arguments
            .get("boxes")
            .and_then(Value::as_array)
            .filter(|boxes| !boxes.is_empty())
            .ok_or("Pass at least one box in `boxes`.")?
            .iter()
            .enumerate()
            .map(|(index, value)| {
                Query::parse(value).map_err(|error| format!("Box {}: {error}", index + 1))
            })
            .collect::<Result<Vec<_>, _>>()?;
        let microns = match self.arguments.get("unit").and_then(Value::as_str) {
            None | Some("um") => true,
            Some("nm") => false,
            Some(unit) => return Err(format!("Unknown unit `{unit}`; use \"um\" or \"nm\".")),
        };
        let output = self.compile_cell(workspace, &Sources::new(), cell)?;
        let CompileOutput::Valid(data) = &output else {
            return Err(INTERNAL_ERROR.to_owned());
        };
        let structs = data.gds_struct_names();
        let scale = if microns {
            1. / um_per_unit(&data.tech)
        } else {
            1.
        };
        let unit = if microns { "µm" } else { "nm" };
        let sites = Sites::new(data);
        let files = Files::default();
        let mut text = String::new();
        for query in &boxes {
            let frame = match &query.gds_cell {
                None => data.top,
                Some(name) => match structs
                    .iter()
                    .find(|(_, struct_name)| struct_name.as_str() == name)
                {
                    Some((id, _)) => *id,
                    None => {
                        let names = structs.values().map(ArcStr::as_str).collect::<Vec<_>>();
                        let _ = writeln!(
                            text,
                            "{}: no GDS cell named `{name}`; the export has {}",
                            query.title(unit, &structs[&data.top]),
                            names.join(", ")
                        );
                        continue;
                    }
                },
            };
            let (layers, unknown) = match query.layers.as_deref() {
                None => (None, Vec::new()),
                Some(names) => {
                    let (matched, unknown) = match_layers(&data.tech, names);
                    (Some(matched), unknown)
                }
            };
            let area = query.area.map(|value| value * scale);
            let mut found = Vec::new();
            walk(
                data,
                frame,
                Transform::IDENTITY,
                &mut Vec::new(),
                &mut |cell, object, transform, path| {
                    let Some((layer, bounds)) = shape_bounds(object, transform) else {
                        return;
                    };
                    if layers
                        .as_ref()
                        .is_some_and(|layers| !layers.contains(layer))
                    {
                        return;
                    }
                    if touches(bounds, area) {
                        found.push((
                            layer.to_owned(),
                            bounds,
                            sites.describe(cell, object, path, &files, self),
                        ));
                    }
                },
            );
            let _ = write!(text, "{}:", query.title(unit, &structs[&frame]));
            if !unknown.is_empty() {
                let _ = write!(
                    text,
                    " (no layer in the technology matches {})",
                    unknown.join(", ")
                );
            }
            text.push('\n');
            if found.is_empty() {
                text.push_str("  no shapes\n");
            }
            let total = found.len();
            for (layer, [x0, y0, x1, y1], site) in found.into_iter().take(MAX_SHAPES_PER_BOX) {
                let _ = writeln!(
                    text,
                    "  {layer} ({}, {}) to ({}, {}): {site}",
                    number(x0),
                    number(y0),
                    number(x1),
                    number(y1)
                );
            }
            if total > MAX_SHAPES_PER_BOX {
                let _ = writeln!(text, "  ... and {} more", total - MAX_SHAPES_PER_BOX);
            }
        }
        Ok(text.trim_end().to_owned())
    }
}

/// One box `locate` searches.
struct Query {
    area: [f64; 4],
    layers: Option<Vec<String>>,
    gds_cell: Option<String>,
    label: Option<String>,
}

impl Query {
    fn parse(value: &Value) -> Result<Self, String> {
        let coordinate = |name| {
            value
                .get(name)
                .and_then(Value::as_f64)
                .ok_or_else(|| format!("missing number `{name}`"))
        };
        let [x0, y0, x1, y1] = [
            coordinate("x0")?,
            coordinate("y0")?,
            coordinate("x1")?,
            coordinate("y1")?,
        ];
        let layers = match value.get("layers") {
            None => None,
            Some(layers) => Some(
                layers
                    .as_array()
                    .and_then(|items| {
                        items
                            .iter()
                            .map(|item| item.as_str().map(str::to_owned))
                            .collect()
                    })
                    .ok_or("`layers` must be a list of strings")?,
            ),
        };
        let text = |name| value.get(name).and_then(Value::as_str).map(str::to_owned);
        Ok(Self {
            area: [x0.min(x1), y0.min(y1), x0.max(x1), y0.max(y1)],
            layers,
            gds_cell: text("gds_cell"),
            label: text("label"),
        })
    }

    fn title(&self, unit: &str, cell: &str) -> String {
        let [x0, y0, x1, y1] = self.area.map(number);
        let label = self
            .label
            .as_ref()
            .map_or_else(String::new, |label| format!("{label} "));
        format!("{label}({x0}, {y0}) to ({x1}, {y1}) {unit} in {cell}")
    }
}

/// The layers of `tech` that `names` match, by full name, by the name before
/// the purpose, or as `layer/datatype`, ignoring case and punctuation, and
/// the names that match none.
fn match_layers(tech: &Technology, names: &[String]) -> (HashSet<String>, Vec<String>) {
    let key = |name: &str| {
        name.chars()
            .filter(char::is_ascii_alphanumeric)
            .map(|c| c.to_ascii_lowercase())
            .collect::<String>()
    };
    let mut matched = HashSet::new();
    let mut unknown = Vec::new();
    for name in names {
        let before = matched.len();
        let wanted = key(name);
        for layer in &tech.layers {
            let base = layer.name.split('.').next().unwrap_or(&layer.name);
            let gds = format!("{}/{}", layer.gds_layer, layer.gds_datatype);
            if [layer.name.as_str(), base, gds.as_str()]
                .iter()
                .any(|candidate| key(candidate) == wanted)
            {
                matched.insert(layer.name.clone());
            }
        }
        if matched.len() == before {
            unknown.push(format!("`{name}`"));
        }
    }
    (matched, unknown)
}

/// Whether `bounds` overlaps or touches `area`, within half a source unit.
fn touches(bounds: [f64; 4], area: [f64; 4]) -> bool {
    const SLACK: f64 = 0.5;
    bounds[0] <= area[2] + SLACK
        && area[0] <= bounds[2] + SLACK
        && bounds[1] <= area[3] + SLACK
        && area[1] <= bounds[3] + SLACK
}

/// Where each object was drawn, for describing shapes `locate` finds.
struct Sites<'a> {
    cells: HashMap<CellId, CellSites<'a>>,
}

/// One cell's emission records and binding names, by object.
struct CellSites<'a> {
    emits: HashMap<ObjectId, &'a CompiledEmit>,
    names: HashMap<ObjectId, String>,
}

impl<'a> Sites<'a> {
    fn new(data: &'a CompiledData) -> Self {
        let cells = data
            .cells
            .iter()
            .map(|(id, cell)| {
                let mut emits = HashMap::new();
                let mut names = HashMap::new();
                for scope in cell.scopes.values() {
                    for (object, emit) in scope.emit.iter() {
                        emits.entry(*object).or_insert(emit);
                    }
                }
                // Root-scope names first, so they win over a function's locals.
                let root = cell.scopes.get(&cell.root).into_iter();
                let others = cell
                    .scopes
                    .iter()
                    .filter(|(id, _)| **id != cell.root)
                    .map(|(_, scope)| scope);
                for scope in root.chain(others) {
                    for (_, (name, value)) in scope.bindings.iter() {
                        name_objects(name, value, &mut names);
                    }
                }
                (*id, CellSites { emits, names })
            })
            .collect();
        Self { cells }
    }

    /// The source line that drew `object` in `cell`, the call in the cell's
    /// body it ran under, and the instances leading to `cell`.
    fn describe(
        &self,
        cell: CellId,
        object: &SolvedValue,
        path: &[(CellId, &SolvedInstance)],
        files: &Files,
        call: &Call,
    ) -> String {
        let at = |span: &Span| {
            let (line, column) = files.line_column(&span.path, span.span.start());
            format!("{}:{line}:{column}", call.show(&span.path))
        };
        let origin = |cell: CellId, id: ObjectId| {
            self.cells[&cell]
                .emits
                .get(&id)
                .and_then(|emit| emit.origin.as_ref())
                .map(|origin| format!(", called from {}", at(origin)))
                .unwrap_or_default()
        };
        let (id, span) = object_site(object);
        let mut text = match span {
            Some(span) => format!("{}{}", at(span), origin(cell, id)),
            None => "no source location".to_owned(),
        };
        if !path.is_empty() {
            let instances = path
                .iter()
                .map(|(parent, instance)| {
                    let name = self.cells[parent]
                        .names
                        .get(&instance.id)
                        .map_or_else(|| "instance".to_owned(), Clone::clone);
                    format!(
                        "{name} ({}{})",
                        at(&instance.span),
                        origin(*parent, instance.id)
                    )
                })
                .collect::<Vec<_>>();
            let _ = write!(text, "; in {}", instances.join(" > "));
        }
        text
    }
}

fn name_objects(name: &str, value: &Arrayed<ObjectId>, names: &mut HashMap<ObjectId, String>) {
    match value {
        Arrayed::Elem(id) => {
            names.entry(*id).or_insert_with(|| name.to_owned());
        }
        Arrayed::Array(items) => {
            for (index, item) in items.iter().enumerate() {
                name_objects(&format!("{name}[{index}]"), item, names);
            }
        }
    }
}

fn object_site(object: &SolvedValue) -> (ObjectId, Option<&Span>) {
    match object {
        SolvedValue::Rect(rect) => (rect.id, rect.span.as_ref()),
        SolvedValue::Polygon(polygon) => (polygon.id, polygon.span.as_ref()),
        SolvedValue::Path(path) => (path.id, path.span.as_ref()),
        SolvedValue::Text(text) => (text.id, text.span.as_ref()),
        SolvedValue::Dimension(dimension) => (dimension.id, dimension.span.as_ref()),
        SolvedValue::Instance(instance) => (instance.id, Some(&instance.span)),
    }
}

/// A placement: a Manhattan rotation and reflection, then a translation.
#[derive(Clone, Copy)]
struct Transform {
    matrix: [f64; 4],
    offset: (f64, f64),
}

impl Transform {
    const IDENTITY: Self = Self {
        matrix: [1., 0., 0., 1.],
        offset: (0., 0.),
    };

    fn of(instance: &SolvedInstance) -> Self {
        Self::placement(
            instance.angle.degrees(),
            instance.reflect,
            (instance.x, instance.y),
        )
    }

    /// Reflected across the x axis if `reflect`, then rotated by `degrees`,
    /// then moved by `offset`, as `std::layout::inst` places a cell.
    fn placement(degrees: f64, reflect: bool, offset: (f64, f64)) -> Self {
        let (sin, cos) = degrees.to_radians().sin_cos();
        let (sin, cos) = (sin.round(), cos.round());
        let flip = if reflect { -1. } else { 1. };
        Self {
            matrix: [cos, -sin * flip, sin, cos * flip],
            offset,
        }
    }

    fn apply(&self, (x, y): (f64, f64)) -> (f64, f64) {
        let [a, b, c, d] = self.matrix;
        (a * x + b * y + self.offset.0, c * x + d * y + self.offset.1)
    }

    /// This transform after `inner`.
    fn compose(&self, inner: &Self) -> Self {
        let [a, b, c, d] = self.matrix;
        let [e, f, g, h] = inner.matrix;
        Self {
            matrix: [a * e + b * g, a * f + b * h, c * e + d * g, c * f + d * h],
            offset: self.apply(inner.offset),
        }
    }
}

/// Calls `visit` with each drawn object under `cell`, the cell holding it,
/// its transform into `cell`'s frame, and the instances leading to it, each
/// with the cell holding it.
fn walk<'a>(
    data: &'a CompiledData,
    cell: CellId,
    transform: Transform,
    path: &mut Vec<(CellId, &'a SolvedInstance)>,
    visit: &mut impl FnMut(CellId, &'a SolvedValue, &Transform, &[(CellId, &'a SolvedInstance)]),
) {
    for object in data.cells[&cell]
        .objects
        .values()
        .filter(|object| object.is_layout())
    {
        if let SolvedValue::Instance(instance) = object {
            path.push((cell, instance));
            let inner = transform.compose(&Transform::of(instance));
            walk(data, instance.cell, inner, path, visit);
            path.pop();
        } else {
            visit(cell, object, &transform, path);
        }
    }
}

/// A drawn shape's layer and bounds after `transform`; `None` for text and
/// shapes with no layer.
fn shape_bounds<'a>(object: &'a SolvedValue, transform: &Transform) -> Option<(&'a str, [f64; 4])> {
    let (layer, points) = match object {
        SolvedValue::Rect(rect) => (
            rect.layer.as_deref()?,
            vec![(rect.x0.0, rect.y0.0), (rect.x1.0, rect.y1.0)],
        ),
        SolvedValue::Polygon(polygon) => (
            polygon.layer.as_str(),
            polygon.points.iter().map(|(x, y)| (x.0, y.0)).collect(),
        ),
        SolvedValue::Path(path) => (path.layer.as_str(), path.outline()?),
        _ => return None,
    };
    bounds(points.into_iter().map(|point| transform.apply(point))).map(|bounds| (layer, bounds))
}

fn bounds(points: impl IntoIterator<Item = (f64, f64)>) -> Option<[f64; 4]> {
    points.into_iter().fold(None, |bounds, (x, y)| {
        Some(match bounds {
            None => [x, y, x, y],
            Some([x0, y0, x1, y1]) => [x0.min(x), y0.min(y), x1.max(x), y1.max(y)],
        })
    })
}

/// Size, bounding box, and counts of a compiled cell's layout.
fn summary(data: &CompiledData) -> String {
    let top = &data.cells[&data.top];
    let layout = top.objects.values().filter(|object| object.is_layout());
    let (mut shapes, mut instances) = (0, 0);
    for object in layout {
        match object {
            SolvedValue::Instance(_) => instances += 1,
            SolvedValue::Rect(rect) if rect.layer.is_some() => shapes += 1,
            SolvedValue::Polygon(_) | SolvedValue::Path(_) => shapes += 1,
            _ => {}
        }
    }
    let plural = |n: usize, noun: &str| format!("{n} {noun}{}", if n == 1 { "" } else { "s" });
    let counts = format!(
        "{}, {} in the top cell",
        plural(shapes, "shape"),
        plural(instances, "instance")
    );
    let Some(bbox) = data.layout_bbox(data.top) else {
        return format!("empty; {counts}");
    };
    let um = um_per_unit(&data.tech);
    let (w, h) = (bbox.x1 - bbox.x0, bbox.y1 - bbox.y0);
    let mut text = format!(
        "{} × {} µm = {:.2} µm², ({}, {}) to ({}, {}); {counts}",
        number(w * um),
        number(h * um),
        w * h * um * um,
        number(bbox.x0),
        number(bbox.y0),
        number(bbox.x1),
        number(bbox.y1),
    );
    if !top.unsolved_vars.is_empty() {
        let _ = write!(
            text,
            "; {} coordinates set only by initial conditions",
            top.unsolved_vars.len()
        );
    }
    text
}

/// Each layer's drawn area, flattened, and the fraction of the bounding box it covers.
fn layer_report(data: &CompiledData) -> String {
    let mut rects = HashMap::<&str, Vec<[f64; 4]>>::new();
    let mut approximate = false;
    walk(
        data,
        data.top,
        Transform::IDENTITY,
        &mut Vec::new(),
        &mut |_, object, transform, _| {
            let (layer, outline) = match object {
                SolvedValue::Rect(rect) => match rect.layer.as_deref() {
                    Some(layer) => (layer, vec![(rect.x0.0, rect.y0.0), (rect.x1.0, rect.y1.0)]),
                    None => return,
                },
                SolvedValue::Polygon(polygon) => (
                    polygon.layer.as_str(),
                    polygon.points.iter().map(|(x, y)| (x.0, y.0)).collect(),
                ),
                SolvedValue::Path(path) => match path.outline() {
                    Some(outline) => (path.layer.as_str(), outline),
                    None => return,
                },
                _ => return,
            };
            let outline = outline
                .into_iter()
                .map(|point| transform.apply(point))
                .collect::<Vec<_>>();
            let layer_rects = rects.entry(layer).or_default();
            if outline.len() == 2 {
                layer_rects.extend(bounds(outline));
            } else if !manhattan_rects(&outline, layer_rects) {
                approximate = true;
                layer_rects.extend(bounds(outline));
            }
        },
    );
    let um = um_per_unit(&data.tech);
    let bbox_area = data
        .layout_bbox(data.top)
        .map_or(0., |bbox| (bbox.x1 - bbox.x0) * (bbox.y1 - bbox.y0));
    let mut areas = rects
        .into_iter()
        .map(|(layer, rects)| (layer, union_area(&rects)))
        .collect::<Vec<_>>();
    areas.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(b.0)));
    let mut text = areas
        .iter()
        .map(|(layer, area)| {
            let fraction = if bbox_area > 0. {
                area / bbox_area * 100.
            } else {
                0.
            };
            format!("{layer} {:.2} µm² ({fraction:.0}%)", area * um * um)
        })
        .collect::<Vec<_>>()
        .join(", ");
    if approximate {
        text.push_str(" (non-Manhattan shapes counted by their bounding boxes)");
    }
    text
}

/// Splits a Manhattan polygon into rects, appending them to `rects`, or
/// returns `false` if it has a diagonal edge.
fn manhattan_rects(points: &[(f64, f64)], rects: &mut Vec<[f64; 4]>) -> bool {
    let edges = points.iter().zip(points.iter().cycle().skip(1));
    let mut verticals = Vec::new();
    for (a, b) in edges {
        if a.0 == b.0 {
            verticals.push((a.0, a.1.min(b.1), a.1.max(b.1)));
        } else if a.1 != b.1 {
            return false;
        }
    }
    let mut ys = points.iter().map(|point| point.1).collect::<Vec<_>>();
    ys.sort_by(f64::total_cmp);
    ys.dedup();
    for band in ys.windows(2) {
        let (y0, y1) = (band[0], band[1]);
        let mut xs = verticals
            .iter()
            .filter(|(_, low, high)| *low <= y0 && y1 <= *high)
            .map(|(x, _, _)| *x)
            .collect::<Vec<_>>();
        xs.sort_by(f64::total_cmp);
        // Even-odd: the polygon is inside between alternate crossings.
        rects.extend(
            xs.as_chunks::<2>()
                .0
                .iter()
                .map(|[x0, x1]| [*x0, y0, *x1, y1]),
        );
    }
    true
}

/// The area of the union of `rects`, by a sweep across x that keeps the
/// covered length of y in a segment tree.
fn union_area(rects: &[[f64; 4]]) -> f64 {
    let mut ys = rects
        .iter()
        .flat_map(|rect| [rect[1], rect[3]])
        .collect::<Vec<_>>();
    ys.sort_by(f64::total_cmp);
    ys.dedup();
    if ys.len() < 2 {
        return 0.;
    }
    let mut events = rects
        .iter()
        .filter(|rect| rect[0] < rect[2] && rect[1] < rect[3])
        .flat_map(|rect| {
            [
                (rect[0], 1, rect[1], rect[3]),
                (rect[2], -1, rect[1], rect[3]),
            ]
        })
        .collect::<Vec<_>>();
    events.sort_by(|a, b| a.0.total_cmp(&b.0));
    let index = |y: f64| ys.partition_point(|value| *value < y);
    let mut coverage = Coverage {
        count: vec![0; 4 * ys.len()],
        length: vec![0.; 4 * ys.len()],
        ys: &ys,
    };
    let mut area = 0.;
    let mut last = events.first().map_or(0., |event| event.0);
    for (x, delta, y0, y1) in events {
        area += coverage.length[1] * (x - last);
        last = x;
        coverage.add(1, 0, ys.len() - 1, index(y0), index(y1), delta);
    }
    area
}

/// A segment tree over the intervals between consecutive `ys`.
struct Coverage<'a> {
    ys: &'a [f64],
    /// How many rects cover each node's whole range.
    count: Vec<i32>,
    /// The covered length within each node's range.
    length: Vec<f64>,
}

impl Coverage<'_> {
    /// Adds `delta` to the intervals `from..to`, under `node`, which spans `low..high`.
    fn add(&mut self, node: usize, low: usize, high: usize, from: usize, to: usize, delta: i32) {
        if to <= low || high <= from {
            return;
        }
        if from <= low && high <= to {
            self.count[node] += delta;
        } else {
            let middle = (low + high) / 2;
            self.add(2 * node, low, middle, from, to, delta);
            self.add(2 * node + 1, middle, high, from, to, delta);
        }
        self.length[node] = if self.count[node] > 0 {
            self.ys[high] - self.ys[low]
        } else if high - low == 1 {
            0.
        } else {
            self.length[2 * node] + self.length[2 * node + 1]
        };
    }
}

/// Writes `output`'s GDS and SPICE netlist into `directory` as `arc run
/// --gds --spice` writes them.
fn export(output: &CompileOutput, directory: &Path) -> io::Result<()> {
    fs::create_dir_all(directory)?;
    output
        .to_gds(directory.join("argon.gds"))
        .and_then(|()| {
            output.to_netlist(
                directory.join("argon.spice"),
                NetlistFormat::Spice,
                &NetlistOptions::default(),
            )
        })
        .map_err(|error| io::Error::other(format!("{error:#}")))
}

/// A directory name for `cell`'s export: its tokens joined by `-`, such as
/// `inv-1000-1600-4` for `inv(1000., 1600., 4)`.
fn export_name(cell: &str) -> String {
    cell.split(|c: char| !(c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-')))
        .map(|token| token.trim_end_matches('.'))
        .filter(|token| !token.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// A name prefix that occurs nowhere in `text`.
fn reserved_prefix(text: &str) -> String {
    (0..)
        .map(|n| format!("__argon_inspect{n}_"))
        .find(|prefix| !text.contains(prefix.as_str()))
        .expect("some prefix is unused")
}

/// Runs `job` on each item, on up to one thread per CPU, each with the
/// compiler's stack. A job that panics yields `None`.
fn parallel<T: Sync, R: Send>(items: &[T], job: impl Fn(&T) -> R + Sync) -> Vec<Option<R>> {
    let threads = thread::available_parallelism()
        .map_or(1, |cpus| cpus.get())
        .min(items.len());
    let next = AtomicUsize::new(0);
    let results = items.iter().map(|_| Mutex::new(None)).collect::<Vec<_>>();
    thread::scope(|scope| {
        for _ in 0..threads {
            let spawned = thread::Builder::new()
                .name("argon-mcp-compile".to_owned())
                .stack_size(COMPILE_STACK_SIZE)
                .spawn_scoped(scope, || {
                    loop {
                        let index = next.fetch_add(1, Ordering::Relaxed);
                        let Some(item) = items.get(index) else {
                            break;
                        };
                        let result = panic::catch_unwind(AssertUnwindSafe(|| job(item))).ok();
                        *results[index]
                            .lock()
                            .unwrap_or_else(|poisoned| poisoned.into_inner()) = result;
                    }
                });
            if spawned.is_err() {
                break;
            }
        }
    });
    results
        .into_iter()
        .map(|result| {
            result
                .into_inner()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
        })
        .collect()
}

/// Source files' text, read at most once, for turning offsets into lines and columns.
#[derive(Default)]
struct Files(RefCell<HashMap<PathBuf, Option<Arc<str>>>>);

impl Files {
    fn line_column(&self, path: &Path, offset: usize) -> (usize, usize) {
        let text = self
            .0
            .borrow_mut()
            .entry(path.to_path_buf())
            .or_insert_with(|| match argonc::parse::virtual_source(path) {
                Some(source) => Some(Arc::from(source)),
                None => fs::read_to_string(path).ok().map(Arc::from),
            })
            .clone();
        text.map_or((1, 1), |text| line_column(&text, offset))
    }
}

/// The one-based line and column of byte `offset` in `text`.
fn line_column(text: &str, offset: usize) -> (usize, usize) {
    let mut offset = offset.min(text.len());
    while !text.is_char_boundary(offset) {
        offset -= 1;
    }
    let before = &text[..offset];
    let line_start = before.rfind('\n').map_or(0, |index| index + 1);
    (
        before.matches('\n').count() + 1,
        before[line_start..].chars().count() + 1,
    )
}

/// Micrometers per source unit.
fn um_per_unit(tech: &Technology) -> f64 {
    tech.dbu * tech.display_unit as f64 * 1e6
}

/// `value` with at most three decimals and no trailing zeros.
fn number(value: f64) -> String {
    let text = format!("{:.3}", value + 0.);
    let text = text.trim_end_matches('0').trim_end_matches('.');
    if text == "-0" {
        "0".to_owned()
    } else {
        text.to_owned()
    }
}

/// A field's value as `inspect` shows it.
fn format_field(data: &CompiledData, cell: &CompiledCell, value: &FieldValue) -> String {
    let list = |items: &[FieldValue]| {
        let mut shown = items
            .iter()
            .take(MAX_LIST_ITEMS)
            .map(|item| format_field(data, cell, item))
            .collect::<Vec<_>>();
        if items.len() > MAX_LIST_ITEMS {
            shown.push(format!("... and {} more", items.len() - MAX_LIST_ITEMS));
        }
        shown.join(", ")
    };
    let fields = |fields: &[(String, FieldValue)]| {
        fields
            .iter()
            .map(|(name, value)| format!("{name}: {}", format_field(data, cell, value)))
            .collect::<Vec<_>>()
            .join(", ")
    };
    match value {
        FieldValue::Object(id) => cell.objects.get(id).map_or_else(
            || "a shape".to_owned(),
            |object| format_object(data, object),
        ),
        FieldValue::Float(value) => {
            let text = number(*value);
            if text.contains('.') {
                text
            } else {
                format!("{text}.")
            }
        }
        FieldValue::Int(value) => value.to_string(),
        FieldValue::Bool(value) => value.to_string(),
        FieldValue::String(value) => format!("{value:?}"),
        FieldValue::Point(x, y) => format!("Point({}, {})", number(*x), number(*y)),
        FieldValue::Cell(id) => format!(
            "cell {}",
            data.cells
                .get(id)
                .map_or("(unknown)", |cell| cell.name.as_str())
        ),
        FieldValue::Nil => "nil".to_owned(),
        FieldValue::Seq(items) => format!("[{}]", list(items)),
        FieldValue::Tuple(items) => format!("({})", list(items)),
        FieldValue::Struct {
            name,
            fields: values,
        } => format!("{name} {{ {} }}", fields(values)),
        FieldValue::Enum { variant, payload } if payload.is_empty() => variant.clone(),
        FieldValue::Enum { variant, payload } => format!("{variant}({})", list(payload)),
        FieldValue::StructVariant {
            variant,
            fields: values,
        } => {
            format!("{variant} {{ {} }}", fields(values))
        }
        FieldValue::Signal(_) => "a signal".to_owned(),
        FieldValue::SchematicInst { .. } => "a schematic instance".to_owned(),
        FieldValue::Device { kind, .. } => format!("a {kind:?} device"),
        FieldValue::Detached(_) => "a detached value".to_owned(),
        FieldValue::Plain(plain) => format_field(data, cell, &plain.to_field()),
        FieldValue::Poison => "no value; its error is reported above".to_owned(),
        FieldValue::Unreadable(kind) => format!("a {kind}"),
    }
}

fn format_object(data: &CompiledData, object: &SolvedValue) -> String {
    let corners = |[x0, y0, x1, y1]: [f64; 4]| {
        format!(
            "({}, {}) to ({}, {}), {} × {}",
            number(x0),
            number(y0),
            number(x1),
            number(y1),
            number(x1 - x0),
            number(y1 - y0)
        )
    };
    match object {
        // Rects read through an instance are construction copies of drawn
        // shapes, so only unlayered ones are labelled as construction.
        SolvedValue::Rect(rect) => format!(
            "{} {} {}",
            if rect.construction && rect.layer.is_none() {
                "construction Rect"
            } else {
                "Rect"
            },
            rect.layer.as_deref().unwrap_or("(no layer)"),
            corners([rect.x0.0, rect.y0.0, rect.x1.0, rect.y1.0])
        ),
        SolvedValue::Polygon(polygon) => format!(
            "Polygon {} of {} points, bounds {}",
            polygon.layer,
            polygon.points.len(),
            bounds(polygon.points.iter().map(|(x, y)| (x.0, y.0)))
                .map_or_else(String::new, corners)
        ),
        SolvedValue::Path(path) => format!(
            "Path {} of width {} through {} points",
            path.layer,
            number(path.width.0),
            path.points.len()
        ),
        SolvedValue::Text(text) => format!(
            "Text {:?} on {} at ({}, {})",
            text.text,
            text.layer,
            number(text.x),
            number(text.y)
        ),
        SolvedValue::Dimension(_) => "a dimension".to_owned(),
        SolvedValue::Instance(instance) => {
            let transform = Transform::of(instance);
            let bbox = data.layout_bbox(instance.cell).and_then(|bbox| {
                bounds(
                    [(bbox.x0, bbox.y0), (bbox.x1, bbox.y1)]
                        .into_iter()
                        .map(|point| transform.apply(point)),
                )
            });
            let mut text = format!(
                "Inst of {} at ({}, {})",
                data.cells
                    .get(&instance.cell)
                    .map_or("(unknown)", |cell| cell.name.as_str()),
                number(instance.x),
                number(instance.y)
            );
            if instance.angle.degrees() != 0. {
                let _ = write!(text, ", rotated {}°", instance.angle.degrees());
            }
            if instance.reflect {
                text.push_str(", reflected");
            }
            if let Some(bbox) = bbox {
                let _ = write!(text, ", bbox {}", corners(bbox));
            }
            text
        }
    }
}

fn tool_definitions() -> Value {
    let workspace = json!({
        "type": "string",
        "description": "The library's directory, or a directory inside it, absolute or relative to your working directory. Needed when your working directory is not inside the library."
    });
    let cell = json!({
        "type": "string",
        "description": "A cell invocation, such as `inv(1000., 1600., 4)`."
    });
    json!([
        {
            "name": "compile",
            "description": "Compile cells and report their sizes, bounding boxes, and errors. Pass every point of a sweep in one call, as `cells`, as `sweep`, or both; the cells compile in parallel. One cell is reported in full. Several are summarized: one line for those that compile, with the range of their sizes and the smallest and largest by area; in full, any that compile with coordinates set only by initial conditions or whose export failed; and each distinct error once, with the cells it occurs in. `verbose` lists every cell in full instead. With `export`, also write each cell that compiles to `<export>/<name>/argon.gds` and `argon.spice`, exactly as `arc run --gds --spice` writes them, where <name> is the invocation's tokens joined by `-`, such as `inv-1000-1600-4`; a cell that fails removes its old files there. With no cells, only parse and type-check the library.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cells": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Cell invocations, such as `inv(1000., 1600., 4)`."
                    },
                    "sweep": {
                        "type": "object",
                        "description": "A grid of invocations of one cell: every combination of its parameters' values, the last parameter varying fastest. Each number is written as its parameter's declared type needs, so `1000` passes `1000.` to a Float. A string is Argon source, such as `\"Mode::Fast\"`, except for a String parameter, which receives it as text. Example: `{\"cell\": \"inv\", \"params\": [[1000, 2000], [1600], [1, 2, 4]]}` compiles `inv(1000., 1600., 1)` through `inv(2000., 1600., 4)`.",
                        "properties": {
                            "cell": {
                                "type": "string",
                                "description": "The cell's name, such as `inv`, or its path, such as `sky130::mos`."
                            },
                            "params": {
                                "description": "A list of value lists, one per parameter from the first, or an object of value lists by parameter name, which must give every parameter without a default and passes those with one as keyword arguments.",
                                "oneOf": [
                                    { "type": "array", "items": { "type": "array" } },
                                    { "type": "object", "additionalProperties": { "type": "array" } }
                                ]
                            }
                        },
                        "required": ["cell", "params"]
                    },
                    "verbose": {
                        "type": "boolean",
                        "description": "List every cell's size, bounding box, and errors in full, even when there are several."
                    },
                    "export": {
                        "type": "string",
                        "description": "Directory to export into, relative to the library's directory, such as `target/sweep`."
                    },
                    "layers": {
                        "type": "boolean",
                        "description": "Also report each layer's drawn area, with overlaps counted once, and the fraction of the bounding box it covers. Lists every cell in full, as `verbose` does."
                    },
                    "workspace": workspace
                }
            }
        },
        {
            "name": "inspect",
            "description": "Evaluate Argon expressions at the end of a cell's body, after the cell is solved, and report their values: numbers, points, rects with their layer and coordinates, instances with their position and bounding box, and lists and structs of these. In scope are the cell's parameters, its top-level `let`s, public or not, and anything its module can name; nothing names the cell itself, so use compile for its size. Examples: `out.x1 - out.x0`, `nmos.sdm`, `std::layout::bbox(nmos)`. An expression with an error is reported without affecting the others.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cell": cell,
                    "exprs": {
                        "type": "array",
                        "minItems": 1,
                        "items": { "type": "string" },
                        "description": "Argon expressions."
                    },
                    "workspace": workspace
                },
                "required": ["cell", "exprs"]
            }
        },
        {
            "name": "locate",
            "description": "Find the drawn shapes at layout locations, such as a DRC report's violations, and the source line that drew each one, with the call in its cell's body when that line is inside a function. A shape in an instance's cell also lists the instances leading to it. A shape is found when it overlaps or touches a box on one of the box's layers. Pass all of a report's violations in one call.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "cell": cell,
                    "boxes": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "x0": { "type": "number" },
                                "y0": { "type": "number" },
                                "x1": { "type": "number" },
                                "y1": { "type": "number" },
                                "layers": {
                                    "type": "array",
                                    "items": { "type": "string" },
                                    "description": "Layers to search: a full name such as `met1.drawing`, the name before the dot such as `met1`, or a GDS `layer/datatype` such as `68/20`. Defaults to every layer."
                                },
                                "gds_cell": {
                                    "type": "string",
                                    "description": "The GDS cell whose coordinates the box is in: the cell a DRC report names for it. Defaults to the top cell."
                                },
                                "label": {
                                    "type": "string",
                                    "description": "Shown with the box's shapes, such as the rule's name."
                                }
                            },
                            "required": ["x0", "y0", "x1", "y1"]
                        }
                    },
                    "unit": {
                        "type": "string",
                        "enum": ["um", "nm"],
                        "description": "The boxes' unit: `um` (the default), as pv.py reports, or `nm`, the source's units."
                    },
                    "workspace": workspace
                },
                "required": ["cell", "boxes"]
            }
        },
        crate::nets::tool_definition(&cell, &workspace)
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    const INVERTER: &str = "inv(1000., 1600., 4)";

    /// A copy of `examples/sky130_inverter` whose manifest names this
    /// checkout's SKY130 files by absolute path.
    fn inverter() -> tempfile::TempDir {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
        let directory = tempfile::tempdir().unwrap();
        fs::copy(
            root.join("examples/sky130_inverter/lib.ar"),
            directory.path().join("lib.ar"),
        )
        .unwrap();
        let pdk = fs::canonicalize(root.join("pdks/sky130")).unwrap();
        fs::write(
            directory.path().join("Argon.toml"),
            format!(
                "name = \"inverter\"\ntech = \"{}\"\n\n[dependencies]\nsky130 = \"{}\"\n",
                pdk.join("sky130.tech.toml").display(),
                pdk.display()
            ),
        )
        .unwrap();
        directory
    }

    /// Calls `name` through JSON-RPC, returning whether it failed and its text.
    async fn call(tools: &Headless, name: &str, arguments: Value) -> (bool, String) {
        let response = crate::mcp::handle(
            tools,
            json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": name, "arguments": arguments }
            }),
        )
        .await
        .unwrap();
        let result = &response["result"];
        (
            result["isError"].as_bool().unwrap(),
            result["content"][0]["text"].as_str().unwrap().to_owned(),
        )
    }

    /// A GDS file's records, without the timestamps in BGNLIB and BGNSTR.
    fn gds_records(path: &Path) -> Vec<(u8, Vec<u8>)> {
        const BGNLIB: u8 = 0x01;
        const BGNSTR: u8 = 0x05;
        let bytes = fs::read(path).unwrap();
        let mut records = Vec::new();
        let mut rest = bytes.as_slice();
        while rest.len() >= 4 {
            let length = usize::from(u16::from_be_bytes([rest[0], rest[1]]));
            if length < 4 {
                break;
            }
            let body = match rest[2] {
                BGNLIB | BGNSTR => Vec::new(),
                _ => rest[4..length].to_vec(),
            };
            records.push((rest[2], body));
            rest = &rest[length..];
        }
        records
    }

    #[tokio::test]
    async fn lists_only_the_headless_tools() {
        let library = inverter();
        let tools = Headless::new(library.path().to_path_buf());
        let response = crate::mcp::handle(
            &tools,
            json!({ "jsonrpc": "2.0", "id": 1, "method": "tools/list" }),
        )
        .await
        .unwrap();
        let names = response["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(names, ["compile", "inspect", "locate", "nets"]);
    }

    #[tokio::test]
    async fn exports_what_argonc_writes_and_compiles_edits_on_disk() {
        let library = inverter();
        let tools = Headless::new(library.path().to_path_buf());
        let stale = library.path().join("target/sweep/inv-1001-1600-4");
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("argon.gds"), "stale").unwrap();
        let (error, text) = call(
            &tools,
            "compile",
            json!({ "cells": [INVERTER, "inv(1001., 1600., 4)"], "export": "target/sweep", "verbose": true }),
        )
        .await;
        assert!(!error, "{text}");
        assert!(
            text.contains(
                "inv(1000., 1600., 4): 2.63 × 6.72 µm = 17.67 µm², (-595, -1160) to (2035, 5560); \
                 25 shapes, 4 instances in the top cell → target/sweep/inv-1000-1600-4/\n"
            ),
            "{text}"
        );
        // An error in a library cell names the call in the user's cell.
        assert!(text.contains("inv(1001., 1600., 4): error\n"), "{text}");
        assert!(text.contains("  called from lib.ar:5:38"), "{text}");
        assert!(!stale.join("argon.gds").exists());

        let workspace = Call::new(library.path(), json!({})).unwrap().workspace;
        let reference = library.path().join("reference");
        parallel(&[()], |_| {
            let output = driver::compile_cell(&workspace, &Sources::new(), INVERTER, true).unwrap();
            fs::create_dir_all(&reference).unwrap();
            output.to_gds(reference.join("argon.gds")).unwrap();
            output
                .to_netlist(
                    reference.join("argon.spice"),
                    NetlistFormat::Spice,
                    &NetlistOptions::default(),
                )
                .unwrap();
        });
        let exported = library.path().join("target/sweep/inv-1000-1600-4");
        assert_eq!(
            gds_records(&exported.join("argon.gds")),
            gds_records(&reference.join("argon.gds"))
        );
        assert_eq!(
            fs::read(exported.join("argon.spice")).unwrap(),
            fs::read(reference.join("argon.spice")).unwrap()
        );

        let source = library.path().join("lib.ar");
        let text = fs::read_to_string(&source).unwrap();
        fs::write(
            &source,
            text.replace("pmos.sdm.x0 - 180.", "pmos.sdm.x0 - 380."),
        )
        .unwrap();
        let (_, text) = call(&tools, "compile", json!({ "cells": [INVERTER] })).await;
        assert!(text.contains("(-795, -1160) to (2035, 5560)"), "{text}");
    }

    #[tokio::test]
    async fn resolves_paths_in_the_library_that_workspace_names() {
        let library = inverter();
        let elsewhere = tempfile::tempdir().unwrap();
        let tools = Headless::new(elsewhere.path().to_path_buf());
        let (error, text) = call(&tools, "compile", json!({ "cells": [INVERTER] })).await;
        assert!(error, "{text}");
        assert!(text.contains("Pass `workspace`"), "{text}");
        let (error, text) = call(
            &tools,
            "compile",
            json!({ "cells": [INVERTER], "export": "target/sweep", "workspace": library.path() }),
        )
        .await;
        assert!(!error, "{text}");
        assert!(text.ends_with("→ target/sweep/inv-1000-1600-4/"), "{text}");
        let exported = library.path().join("target/sweep/inv-1000-1600-4");
        assert!(exported.join("argon.gds").is_file());
        assert!(exported.join("argon.spice").is_file());
    }

    #[tokio::test]
    async fn inspects_values_without_writing_the_source() {
        let library = inverter();
        let tools = Headless::new(library.path().to_path_buf());
        let source = fs::read_to_string(library.path().join("lib.ar")).unwrap();
        let (error, text) = call(
            &tools,
            "inspect",
            json!({ "cell": INVERTER, "exprs": ["out_n.x1 - out_n.x0", "out", "out.bogus", "nf", "nmos.sdm", "std::layout::bbox(nmos)"] }),
        )
        .await;
        assert!(!error, "{text}");
        assert_eq!(
            text,
            "out_n.x1 - out_n.x0 = 1665.\n\
             out = Rect li1.drawing (1700, 1400) to (1870, 2250), 170 × 850\n\
             out.bogus: error at column 5: no field bogus on type Rect\n\
             nf = 4\n\
             nmos.sdm = Rect nsdm.drawing (-415, 0) to (1855, 1260), 2270 × 1260\n\
             std::layout::bbox(nmos) = construction Rect (no layer) (-415, 0) to (1855, 1260), 2270 × 1260"
        );
        assert_eq!(
            fs::read_to_string(library.path().join("lib.ar")).unwrap(),
            source
        );
        let (_, text) = call(
            &tools,
            "inspect",
            json!({ "cell": INVERTER, "exprs": ["nmoss.x"] }),
        )
        .await;
        assert!(
            text.contains("\nIn scope are inv's parameters and its top-level `let`s: `nmos`, "),
            "{text}"
        );
    }

    #[tokio::test]
    async fn locates_shapes_in_any_cell_s_frame() {
        let library = inverter();
        let tools = Headless::new(library.path().to_path_buf());
        let (error, text) = call(
            &tools,
            "locate",
            json!({
                "cell": INVERTER,
                "boxes": [
                    { "x0": 1.75, "y0": 1.5, "x1": 1.8, "y1": 1.6, "layers": ["li1"], "label": "li.1" },
                    { "x0": 0.6, "y0": 1.0, "x1": 0.7, "y1": 1.1, "layers": ["DIFF", "COREID"] },
                    { "x0": 0.6, "y0": 1.0, "x1": 0.7, "y1": 1.1, "layers": ["65/20"], "gds_cell": "sky130__fet1v8" },
                    { "x0": 0, "y0": 0, "x1": 0.1, "y1": 0.1, "gds_cell": "missing" }
                ]
            }),
        )
        .await;
        assert!(!error, "{text}");
        let pdk = fs::canonicalize(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../pdks/sky130/lib.ar"),
        )
        .unwrap();
        for expected in [
            "li.1 (1.75, 1.5) to (1.8, 1.6) µm in inv:\n".to_owned(),
            "  li1.drawing (1700, 1400) to (1870, 2250): lib.ar:51:19\n".to_owned(),
            format!(
                "(0.6, 1) to (0.7, 1.1) µm in inv: (no layer in the technology matches `COREID`)\n  diff.drawing (-285, 130) to (1725, 1130): {}:17:16; in nmos (lib.ar:5:20)\n",
                pdk.display()
            ),
            format!(
                "(0.6, 1) to (0.7, 1.1) µm in sky130__fet1v8:\n  diff.drawing (-285, 130) to (1725, 1130): {}:17:16\n",
                pdk.display()
            ),
            "no GDS cell named `missing`".to_owned(),
        ] {
            assert!(text.contains(&expected), "{expected}\n{text}");
        }
    }

    /// A library of `source` on the basic technology, whose unit is 0.1 nm.
    fn basic_library(source: &str) -> tempfile::TempDir {
        let directory = tempfile::tempdir().unwrap();
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        fs::write(
            directory.path().join("Argon.toml"),
            format!(
                "name = \"basic\"\ntech = \"{}\"\n",
                fs::canonicalize(tech).unwrap().display()
            ),
        )
        .unwrap();
        fs::write(directory.path().join("lib.ar"), source).unwrap();
        directory
    }

    const BARS: &str = "use std::layout::rect;\n\
        cell bar(w: Float, n: Int, layer: String = \"met1\") {\n\
        \x20   let r = rect(layer, x0=0., y0=0., x1=w, y1=100. * (n as Float));\n\
        \x20   if n > 2 {\n\
        \x20       eq(r.x0, 5.);\n\
        \x20   }\n\
        }\n\
        fn pin(r: Rect) {\n\
        \x20   eq(r.x0, 7.);\n\
        }\n\
        cell twice() {\n\
        \x20   let a = rect(\"met1\", x0=0., y0=0., x1=10., y1=10.);\n\
        \x20   pin(a);\n\
        \x20   let b = rect(\"met1\", x0=0., y0=20., x1=10., y1=30.);\n\
        \x20   pin(b);\n\
        }\n";

    #[tokio::test]
    async fn summarizes_sweeps_with_each_error_once() {
        let library = basic_library(BARS);
        let tools = Headless::new(library.path().to_path_buf());
        let (error, text) = call(
            &tools,
            "compile",
            json!({
                "sweep": { "cell": "bar", "params": [[1000, 2000.5], [1, 2, 3]] },
                "cells": ["bar(1000., 1)", "twice()"],
                "export": "target/sweep"
            }),
        )
        .await;
        assert!(!error, "{text}");
        assert_eq!(
            text,
            "4 of 7 cells compile: 1–2.001 × 0.1–0.2 µm; 0.10 µm² at bar(1000., 1) to 0.40 µm² \
             at bar(2000.5, 2); each exported to target/sweep/<name>/.\n\
             3 cells fail: twice(), bar(1000., 3), and bar(2000.5, 3)\n\
             lib.ar:9:5: inconsistent constraint: other constraints already determine every value \
             in it, and leave its two sides 7 apart\n\
             \x20 called from lib.ar:13:5 and lib.ar:15:5\n\
             \x20 in twice()\n\
             lib.ar:5:9: inconsistent constraint: other constraints already determine every value \
             in it, and leave its two sides 5 apart\n\
             \x20 in bar(1000., 3) and bar(2000.5, 3)"
        );
        assert!(
            library
                .path()
                .join("target/sweep/bar-2000.5-2/argon.gds")
                .is_file()
        );

        // Keywords pass only parameters with defaults, and `verbose` lists each cell.
        let (error, text) = call(
            &tools,
            "compile",
            json!({
                "sweep": { "cell": "bar", "params": { "layer": ["met2"], "n": [1], "w": [1000] } },
                "verbose": true
            }),
        )
        .await;
        assert!(!error, "{text}");
        assert!(
            text.starts_with("bar(1000., 1, layer=\"met2\"): 1 × 0.1 µm"),
            "{text}"
        );
        let (error, text) = call(
            &tools,
            "compile",
            json!({ "sweep": { "cell": "bar", "params": { "w": [1000] } } }),
        )
        .await;
        assert!(error);
        assert_eq!(text, "`sweep` must give `n`, which has no default.");
        let (error, text) = call(
            &tools,
            "compile",
            json!({ "sweep": { "cell": "bar", "params": [[1000], [1.5]] } }),
        )
        .await;
        assert!(error);
        assert_eq!(text, "`n`: 1.5 is not an Int");
    }

    #[tokio::test]
    async fn reports_an_error_every_cell_shares_once() {
        let library = basic_library(
            "cell top(n: Int) {\n    let s = Signal();\n    device(\"r\", [s, s], \"res\");\n}\n",
        );
        let tools = Headless::new(library.path().to_path_buf());
        let (error, text) = call(
            &tools,
            "compile",
            json!({ "sweep": { "cell": "top", "params": [[1, 2, 3]] } }),
        )
        .await;
        assert!(!error, "{text}");
        assert_eq!(
            text,
            "None of the 3 cells compile.\n\
             3 cells fail: top(1), top(2), and top(3)\n\
             lib.ar:2:13: `Signal` is not in scope; import it with `use std::schematic::Signal;`\n\
             \x20 in all 3 failing cells\n\
             lib.ar:3:5: `device` is not in scope; import it with `use std::schematic::device;`\n\
             \x20 in all 3 failing cells"
        );
    }

    #[test]
    fn writes_sweep_values_as_their_parameters_need() {
        assert_eq!(argon_literal(&json!(1000), "Float").unwrap(), "1000.");
        assert_eq!(argon_literal(&json!(-0.15), "Float").unwrap(), "-0.15");
        assert_eq!(argon_literal(&json!(4.0), "Int").unwrap(), "4");
        assert_eq!(argon_literal(&json!(true), "Bool").unwrap(), "true");
        assert_eq!(
            argon_literal(&json!("say \"hi\""), "String").unwrap(),
            "\"say \\\"hi\\\"\""
        );
        assert_eq!(
            argon_literal(&json!("Mode::Fast"), "Mode").unwrap(),
            "Mode::Fast"
        );
        assert!(argon_literal(&json!([1]), "Float").is_err());
    }

    #[test]
    fn names_exports_after_the_invocation() {
        assert_eq!(export_name("inv(1000., 1600., 4)"), "inv-1000-1600-4");
        assert_eq!(
            export_name("sky130::fet1v8(true, 1.5, nf=2)"),
            "sky130-fet1v8-true-1.5-nf-2"
        );
    }

    #[test]
    fn measures_the_union_of_overlapping_shapes() {
        assert_eq!(
            union_area(&[[0., 0., 2., 2.], [1., 1., 3., 3.], [5., 5., 5., 9.]]),
            7.
        );
        let mut rects = Vec::new();
        // An L of three unit squares.
        assert!(manhattan_rects(
            &[(0., 0.), (2., 0.), (2., 1.), (1., 1.), (1., 2.), (0., 2.)],
            &mut rects
        ));
        assert_eq!(union_area(&rects), 3.);
        assert!(!manhattan_rects(
            &[(0., 0.), (1., 0.), (0., 1.)],
            &mut rects
        ));
    }

    #[test]
    fn places_instances_as_inst_does() {
        // Reflected across x, then rotated a quarter turn, then moved.
        let reflected = Transform::placement(90., true, (10., 0.));
        assert_eq!(reflected.apply((1., 2.)), (12., 1.));
        let rotated = Transform::placement(90., false, (10., 0.));
        assert_eq!(rotated.apply((1., 2.)), (8., 1.));
        let nested = reflected.compose(&rotated);
        assert_eq!(
            nested.apply((1., 2.)),
            reflected.apply(rotated.apply((1., 2.)))
        );
    }

    #[tokio::test]
    async fn flattens_rotated_instances_like_the_compiler() {
        let directory = tempfile::tempdir().unwrap();
        let tech =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../examples/tech/basic.tech.toml");
        fs::write(
            directory.path().join("Argon.toml"),
            format!(
                "name = \"rotated\"\ntech = \"{}\"\n",
                fs::canonicalize(tech).unwrap().display()
            ),
        )
        .unwrap();
        fs::write(
            directory.path().join("lib.ar"),
            "cell inner() {\n    std::layout::rect(\"met1\", x0=1., y0=2., w=3., h=5.);\n}\n\
             cell middle() {\n    std::layout::inst(inner(), x=7., y=0., angle=90, reflect=true);\n}\n\
             cell top() {\n    std::layout::inst(middle(), x=0., y=11., angle=270);\n}\n",
        )
        .unwrap();
        let workspace = Call::new(directory.path(), json!({})).unwrap().workspace;
        let (bbox, flattened) = parallel(&[()], |_| {
            let CompileOutput::Valid(data) =
                driver::compile_cell(&workspace, &Sources::new(), "top()", true).unwrap()
            else {
                unreachable!();
            };
            let bbox = data.layout_bbox(data.top).unwrap();
            let mut flattened = Vec::new();
            walk(
                &data,
                data.top,
                Transform::IDENTITY,
                &mut Vec::new(),
                &mut |_, object, transform, _| {
                    flattened.extend(shape_bounds(object, transform).map(|(_, bounds)| bounds));
                },
            );
            ([bbox.x0, bbox.y0, bbox.x1, bbox.y1], flattened)
        })
        .remove(0)
        .unwrap();
        assert_eq!(flattened, [bbox]);
    }
}
