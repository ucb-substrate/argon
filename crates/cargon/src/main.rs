use std::{
    env,
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
    process::{Command, ExitCode, Stdio},
};

use anyhow::{Context, Result, anyhow, bail};
use argonc::diagnostics::{self, Diagnostic};
use cargon::Project;
use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(version, about = "The Argon package manager")]
struct Cli {
    #[command(subcommand)]
    command: CommandKind,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    /// Compile an Argon project and emit GDS.
    Build(BuildArgs),
    /// Type-check an Argon project without emitting GDS.
    Check(ProjectArgs),
}

#[derive(Debug, Args)]
struct ProjectArgs {
    /// Path to Argon.toml.
    #[arg(long, default_value = "Argon.toml")]
    manifest_path: PathBuf,
    /// Compiler executable. ARGONC is used when this option is omitted.
    #[arg(long, env = "ARGONC", default_value = "argonc")]
    argonc: PathBuf,
}

#[derive(Debug, Args)]
struct BuildArgs {
    #[command(flatten)]
    project: ProjectArgs,
    /// Cell invocation to instantiate, for example `top(10., 20.)`.
    #[arg(long)]
    cell: String,
    /// GDS output path. Defaults to target/argon.gds beside the manifest.
    #[arg(short, long)]
    output: Option<PathBuf>,
}

fn main() -> ExitCode {
    let result = match Cli::parse().command {
        CommandKind::Build(args) => build(args),
        CommandKind::Check(args) => check(args),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            print_error(&error.to_string());
            ExitCode::FAILURE
        }
    }
}

fn check(args: ProjectArgs) -> Result<()> {
    let project = Project::load(&args.manifest_path)?;
    status("Checking", project_name(&project));
    let mut command = compiler_command(&args.argonc, &project);
    command.arg("--check");
    run_compiler(command)?;
    status("Finished", "dev profile");
    Ok(())
}

fn build(args: BuildArgs) -> Result<()> {
    let project = Project::load(&args.project.manifest_path)?;
    let lyp = project.lyp.as_ref().ok_or_else(|| {
        anyhow!(
            "manifest `{}` does not set `lyp`",
            project.manifest_path.display()
        )
    })?;
    status("Compiling", project_name(&project));
    let output = args.output.unwrap_or_else(|| {
        project
            .manifest_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("target/argon.gds")
    });
    let mut command = compiler_command(&args.project.argonc, &project);
    command
        .arg("--cell")
        .arg(args.cell)
        .arg("--lyp")
        .arg(lyp)
        .arg("--output")
        .arg(&output);
    run_compiler(command)?;
    status(
        "Finished",
        &format!("dev profile; output: {}", output.display()),
    );
    Ok(())
}

fn compiler_command(argonc: &Path, project: &Project) -> Command {
    let compiler = sibling_argonc(argonc);
    let mut command = Command::new(compiler);
    command
        .arg(&project.root)
        .arg("--error-format")
        .arg("json")
        .stdout(Stdio::inherit())
        .stderr(Stdio::piped());
    for (name, path) in &project.dependencies {
        command
            .arg("--extern")
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
        bail!("could not compile project due to previous errors");
    }
    Ok(())
}

fn project_name(project: &Project) -> &str {
    project
        .manifest_path
        .parent()
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .unwrap_or("argon-project")
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
