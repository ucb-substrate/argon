use std::{
    env,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

use anyhow::{Context, Result, anyhow, bail};
use arc::Library;
use argonc::diagnostics::{self, Diagnostic};
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about = "The Argon library manager")]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    /// Parse, resolve, and type-check an Argon library.
    Check(LibraryArgs),
    /// Execute an Argon cell and write the compiler output.
    Run(RunArgs),
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
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        CommandKind::Check(args) => check(args),
        CommandKind::Run(args) => run(args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            print_error(&error.to_string());
            ExitCode::FAILURE
        }
    }
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

fn run(args: RunArgs) -> Result<()> {
    let library = Library::load(&args.library.manifest_path)?;
    let lyp = library.lyp.as_ref().ok_or_else(|| {
        anyhow!(
            "cannot run a cell because manifest `{}` does not set `lyp`; add `lyp = \"path/to/layers.lyp\"`",
            library.manifest_path.display()
        )
    })?;
    status("Running", &format!("{} in {}", args.cell, library.name));
    let output = args
        .output
        .unwrap_or_else(|| library.target_path("argon.bin"));
    let mut command = compiler_command(&args.library.argonc, &library);
    command
        .arg("--cell")
        .arg(args.cell)
        .arg("--lyp")
        .arg(lyp)
        .arg("--output")
        .arg(&output);
    if args.gds {
        let gds = library.target_path("argon.gds");
        command.arg("--gds").arg(gds);
    }
    run_compiler(command)?;
    status("Finished", &format!("output: {}", output.display()));
    Ok(())
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
