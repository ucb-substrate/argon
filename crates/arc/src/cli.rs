use std::{
    env,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

use crate::{Library, create_workspace, doc, find_manifest_path, format_workspace};
use anyhow::{Context, Result, anyhow, bail};
use argonc::diagnostics::{self, Diagnostic};
use clap::{ArgGroup, Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about = "The Argon library manager")]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    /// Create a new Argon workspace.
    New(NewArgs),
    /// Format Argon source files.
    Fmt(FmtArgs),
    /// Parse, resolve, and type-check an Argon library.
    Check(LibraryArgs),
    /// Execute an Argon cell and write the compiler output.
    Run(RunArgs),
    /// Generate static HTML API documentation for an Argon library.
    Doc(DocArgs),
}

#[derive(Debug, Args)]
struct FmtArgs {
    /// Path to Argon.toml. Defaults to the nearest manifest in this directory or a parent.
    #[arg(long, value_name = "PATH")]
    manifest_path: Option<PathBuf>,
    /// Check formatting without writing files.
    #[arg(long)]
    check: bool,
}

#[derive(Debug, Args)]
struct NewArgs {
    /// Directory to create for the new workspace.
    path: PathBuf,
    /// Workspace name. Defaults to the directory name.
    #[arg(long)]
    name: Option<String>,
}

#[derive(Debug, Args)]
struct LibraryArgs {
    /// Path to Argon.toml.
    #[arg(long, default_value = "Argon.toml")]
    manifest_path: PathBuf,
    /// Compiler executable. ARGONC is used when this option is omitted.
    #[arg(long, env = "ARGONC", default_value = "argonc")]
    argonc: PathBuf,
}

#[derive(Debug, Args)]
#[command(group(ArgGroup::new("netlist").args(["spice", "spectre"]).multiple(true)))]
struct RunArgs {
    #[command(flatten)]
    library: LibraryArgs,
    /// Cell invocation to instantiate, for example `top(10., 20.)`.
    #[arg(long)]
    cell: String,
    /// Binary compiler-output path. Defaults to target/argon.bin.
    #[arg(short, long)]
    output: Option<PathBuf>,
    /// Also write target/argon.gds.
    #[arg(long)]
    gds: bool,
    /// Also write a SPICE netlist to target/argon.spice.
    #[arg(long)]
    spice: bool,
    /// Also write a Spectre netlist to target/argon.scs.
    #[arg(long)]
    spectre: bool,
    /// Wrap netlist lines longer than this many columns; 0 disables wrapping.
    #[arg(long, value_name = "COLS", requires = "netlist")]
    netlist_width: Option<usize>,
}

#[derive(Debug, Args)]
struct DocArgs {
    /// Path to Argon.toml. Defaults to the nearest manifest in this directory or a parent.
    #[arg(long, value_name = "PATH")]
    manifest_path: Option<PathBuf>,
    /// Documentation output directory. Defaults to target/doc.
    #[arg(short, long, value_name = "DIR")]
    output: Option<PathBuf>,
}

pub fn run() -> ExitCode {
    let result = match Cli::parse().command {
        CommandKind::New(args) => new(args),
        CommandKind::Fmt(args) => fmt(args),
        CommandKind::Check(args) => check(args),
        CommandKind::Run(args) => run_cell(args),
        CommandKind::Doc(args) => generate_docs(args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            print_error(&error.to_string());
            ExitCode::FAILURE
        }
    }
}

fn generate_docs(args: DocArgs) -> Result<()> {
    let manifest_path = match args.manifest_path {
        Some(path) => path,
        None => find_manifest_path(".")?,
    };
    let library = Library::load(&manifest_path)?;
    let output = args.output.unwrap_or_else(|| library.target_path("doc"));
    status("Documenting", &library.name);
    let report = doc::generate(&library, &output)?;
    status(
        "Generated",
        &format!(
            "{} module{} at {}",
            report.modules,
            if report.modules == 1 { "" } else { "s" },
            report.output.join("index.html").display()
        ),
    );
    Ok(())
}

fn fmt(args: FmtArgs) -> Result<()> {
    let manifest_path = match args.manifest_path {
        Some(path) => path,
        None => find_manifest_path(".")?,
    };
    let report = format_workspace(&manifest_path, args.check)?;
    if args.check && !report.changed.is_empty() {
        for path in &report.changed {
            eprintln!("Diff in {}", path.display());
        }
        bail!(
            "{} Argon source file{} not formatted; run 'arc fmt --manifest-path {}'",
            report.changed.len(),
            if report.changed.len() == 1 {
                " is"
            } else {
                "s are"
            },
            manifest_path.display()
        );
    }
    if !args.check {
        for path in &report.changed {
            status("Formatted", &path.display().to_string());
        }
    }
    Ok(())
}

fn new(args: NewArgs) -> Result<()> {
    let library = create_workspace(&args.path, args.name.as_deref())?;
    status(
        "Created",
        &format!("{} at {}", library.name, args.path.display()),
    );
    Ok(())
}

fn check(args: LibraryArgs) -> Result<()> {
    let library = Library::load(&args.manifest_path)?;
    status("Checking", &library.name);
    let mut command = compiler_command(&args.argonc, &library);
    command.arg("--check");
    run_compiler(command)?;
    status("Finished", &format!("checking {}", library.name));
    Ok(())
}

fn run_cell(args: RunArgs) -> Result<()> {
    let library = Library::load(&args.library.manifest_path)?;
    let output = args
        .output
        .clone()
        .unwrap_or_else(|| library.target_path("argon.bin"));
    let command = run_command(&library, &args, &output)?;
    status("Running", &format!("{} in {}", args.cell, library.name));
    run_compiler(command)?;
    status("Finished", &format!("output: {}", output.display()));
    Ok(())
}

/// The compiler command that runs `args.cell`, writing `output` and any
/// requested GDS and netlists.
fn run_command(library: &Library, args: &RunArgs, output: &Path) -> Result<Command> {
    let tech = library.tech.as_ref().ok_or_else(|| {
        anyhow!(
            "cannot run a cell because manifest `{}` does not set `tech`; add `tech = \"path/to/tech.toml\"`",
            library.manifest_path.display()
        )
    })?;
    let mut command = compiler_command(&args.library.argonc, library);
    command
        .arg("--cell")
        .arg(&args.cell)
        .arg("--tech")
        .arg(tech)
        .arg("--output")
        .arg(output);
    if args.gds {
        let gds = library.target_path("argon.gds");
        command.arg("--gds").arg(gds);
    }
    if args.spice {
        command
            .arg("--spice")
            .arg(library.target_path("argon.spice"));
    }
    if args.spectre {
        command
            .arg("--spectre")
            .arg(library.target_path("argon.scs"));
    }
    if let Some(width) = args.netlist_width {
        command.arg("--netlist-width").arg(width.to_string());
    }
    Ok(command)
}

fn compiler_command(argonc: &Path, library: &Library) -> Command {
    let compiler = sibling_argonc(argonc);
    let mut command = Command::new(compiler);
    command
        .arg(&library.root)
        .arg("--error-format")
        .arg("json")
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped());
    for (name, path) in &library.dependencies {
        command
            .arg("--dependency")
            .arg(format!("{name}={}", path.display()));
    }
    for (name, path) in &library.gds {
        command
            .arg("--gds-import")
            .arg(format!("{name}={}", path.display()));
    }
    command
}

fn sibling_argonc(requested: &Path) -> PathBuf {
    if requested != Path::new("argonc") {
        return requested.to_path_buf();
    }
    env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|parent| parent.join("argonc")))
        .filter(|candidate| candidate.is_file())
        .unwrap_or_else(|| requested.to_path_buf())
}

fn run_compiler(mut command: Command) -> Result<()> {
    let output = command.output().with_context(|| {
        format!(
            "failed to start `{}`",
            command.get_program().to_string_lossy()
        )
    })?;
    let stderr = String::from_utf8_lossy(&output.stderr);
    for line in stderr.lines() {
        match serde_json::from_str::<Diagnostic>(line) {
            Ok(diagnostic) => {
                let mut writer = io::stderr().lock();
                diagnostics::render(&mut writer, &diagnostic, use_color())?;
            }
            Err(_) if !line.trim().is_empty() => eprintln!("{line}"),
            Err(_) => {}
        }
    }
    if !output.status.success() {
        bail!("could not compile library due to previous errors");
    }
    Ok(())
}

fn use_color() -> bool {
    io::stderr().is_terminal() && env::var_os("NO_COLOR").is_none()
}

fn status(label: &str, message: &str) {
    let mut stderr = io::stderr().lock();
    if use_color() {
        let _ = writeln!(stderr, "\x1b[1;32m{label:>12}\x1b[0m {message}");
    } else {
        let _ = writeln!(stderr, "{label:>12} {message}");
    }
}

fn print_error(message: &str) {
    if use_color() {
        eprintln!("\x1b[1;31merror\x1b[0m: {message}");
    } else {
        eprintln!("error: {message}");
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use clap::Parser;

    use super::{Cli, CommandKind, run_command};
    use crate::Library;

    /// The arguments `arc run <args>` passes to the compiler for the inverter
    /// example.
    fn compiler_args(args: &[&str]) -> Vec<String> {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/schematic_inverter/Argon.toml");
        let manifest = manifest.to_str().expect("path should be UTF-8");
        let cli = Cli::try_parse_from(
            [
                "arc",
                "run",
                "--manifest-path",
                manifest,
                "--cell",
                "inv(2., 1., 2)",
            ]
            .into_iter()
            .chain(args.iter().copied()),
        )
        .unwrap();
        let CommandKind::Run(args) = cli.command else {
            panic!("run subcommand should be selected");
        };
        let library = Library::load(&args.library.manifest_path).unwrap();
        run_command(&library, &args, Path::new("out.bin"))
            .unwrap()
            .get_args()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect()
    }

    /// The value that follows `flag` in `args`.
    fn value_of<'a>(args: &'a [String], flag: &str) -> Option<&'a str> {
        let index = args.iter().position(|arg| arg == flag)?;
        args.get(index + 1).map(String::as_str)
    }

    #[test]
    fn run_passes_netlist_options_to_the_compiler() {
        let args = compiler_args(&["--spice", "--spectre", "--netlist-width", "100"]);
        let target = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../examples/schematic_inverter/target");
        let spice = value_of(&args, "--spice").expect("--spice should be passed");
        assert_eq!(Path::new(spice), target.join("argon.spice"));
        let spectre = value_of(&args, "--spectre").expect("--spectre should be passed");
        assert_eq!(Path::new(spectre), target.join("argon.scs"));
        assert_eq!(value_of(&args, "--netlist-width"), Some("100"));

        let args = compiler_args(&["--spectre"]);
        assert!(value_of(&args, "--spectre").is_some());
        assert!(!args.iter().any(|arg| arg == "--spice"));
        assert!(!args.iter().any(|arg| arg == "--netlist-width"));
    }

    #[test]
    fn netlist_width_requires_a_netlist() {
        let error =
            Cli::try_parse_from(["arc", "run", "--cell", "top()", "--netlist-width", "100"])
                .expect_err("--netlist-width without a netlist should be rejected");
        assert_eq!(
            error.kind(),
            clap::error::ErrorKind::MissingRequiredArgument
        );
    }

    #[test]
    fn parses_documentation_output_directory() {
        let cli = Cli::try_parse_from(["arc", "doc", "--output", "site"]).unwrap();
        let CommandKind::Doc(args) = cli.command else {
            panic!("doc subcommand should be selected");
        };
        assert_eq!(args.output, Some(PathBuf::from("site")));
    }
}
