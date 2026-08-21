use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{ErrorKind, Read},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, Command, ExitCode, ExitStatus, Stdio},
    thread,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};

const ANALYZER_PROBE_INTERVAL: Duration = Duration::from_millis(100);
const ANALYZER_PROBE_TIMEOUT: Duration = Duration::from_millis(100);

#[derive(Debug, Parser)]
#[command(
    version,
    about = "Open an Argon project in Neovim and the graphical editor"
)]
struct Cli {
    /// Neovim executable to launch.
    #[arg(long, global = true, default_value = "nvim")]
    nvim: OsString,

    #[command(subcommand)]
    command: Option<CommandKind>,

    /// Argon project directory or source file.
    #[arg(value_name = "PATH", default_value = ".")]
    path: PathBuf,
}

#[derive(Debug, Subcommand)]
enum CommandKind {
    /// Open a remote Argon project using SSH.
    Ssh(SshArgs),

    /// Run only the graphical editor. Used internally by Argon.
    #[command(name = "__gui", hide = true)]
    Gui(GuiArgs),
}

#[derive(Debug, Args)]
struct SshArgs {
    /// Host name or SSH configuration alias.
    host: String,

    /// Project directory or source file on the remote host.
    #[arg(value_name = "PATH", default_value = ".")]
    path: PathBuf,

    /// SSH executable to launch.
    #[arg(long, default_value = "ssh")]
    ssh: OsString,

    /// Pass an option through to OpenSSH, as with `ssh -o OPTION`.
    #[arg(short = 'o', long = "ssh-option")]
    ssh_options: Vec<String>,
}

#[derive(Debug, Args)]
struct GuiArgs {
    lang_server_addr: SocketAddr,

    #[arg(long)]
    listen_port: Option<u16>,

    #[arg(long)]
    register_addr: Option<SocketAddr>,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(status) if status.success() => ExitCode::SUCCESS,
        Ok(status) => ExitCode::from(status.code().unwrap_or(1) as u8),
        Err(error) => {
            eprintln!("argone: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<ExitStatus> {
    match cli.command {
        None => run_nvim(&cli.nvim, &cli.path),
        Some(CommandKind::Ssh(args)) => run_ssh(&cli.nvim, args),
        Some(CommandKind::Gui(args)) => {
            argone::run(args.lang_server_addr, args.listen_port, args.register_addr);
            Ok(success_status())
        }
    }
}

fn run_nvim(nvim: &OsStr, path: &Path) -> Result<ExitStatus> {
    let (working_directory, target) = nvim_location(path)?;
    let mut command = Command::new(nvim);
    command
        .current_dir(working_directory)
        .arg("--cmd")
        .arg("let g:argon_auto_gui = v:true")
        .arg(target)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to start `{}`", nvim.to_string_lossy()))
}

fn nvim_location(path: &Path) -> Result<(PathBuf, PathBuf)> {
    let metadata =
        fs::metadata(path).with_context(|| format!("could not access `{}`", path.display()))?;
    if metadata.is_dir() {
        let source = path.join("lib.ar");
        if !source.is_file() {
            bail!(
                "project directory `{}` does not contain `lib.ar`",
                path.display()
            );
        }
        return Ok((path.to_path_buf(), PathBuf::from("lib.ar")));
    }
    if metadata.is_file() {
        let directory = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .to_path_buf();
        let target = path
            .file_name()
            .map(PathBuf::from)
            .ok_or_else(|| anyhow!("source path `{}` has no file name", path.display()))?;
        return Ok((directory, target));
    }
    bail!("`{}` is not a file or directory", path.display())
}

fn run_ssh(nvim: &OsStr, args: SshArgs) -> Result<ExitStatus> {
    let control = SshControl::new(&args)?;
    let (remote_analyzer_port, remote_gui_port) = remote_ports(nvim, &args, &control)?;
    let (local_analyzer_port, local_gui_port) = allocate_port_pair()?;
    let local_analyzer_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, local_analyzer_port);
    let local_gui_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, local_gui_port);
    let remote_gui_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, remote_gui_port);

    let mut command = control.command(&args);
    command
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-L")
        .arg(format!(
            "{local_analyzer_addr}:127.0.0.1:{remote_analyzer_port}"
        ))
        .arg("-R")
        .arg(format!("{remote_gui_addr}:{local_gui_addr}"))
        .arg("-t")
        .arg(&args.host)
        .arg(remote_nvim_command(nvim, &args.path, remote_analyzer_port))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut ssh = command
        .spawn()
        .with_context(|| format!("failed to start `{}`", args.ssh.to_string_lossy()))?;
    wait_for_analyzer(&mut ssh, local_analyzer_addr)?;

    let mut gui = launch_forwarded_gui(local_analyzer_addr, local_gui_port, remote_gui_addr)?;
    let status = ssh.wait();
    let _ = gui.kill();
    let _ = gui.wait();
    status.context("failed while waiting for SSH")
}

fn remote_ports(nvim: &OsStr, args: &SshArgs, control: &SshControl) -> Result<(u16, u16)> {
    let output = control
        .command(args)
        .arg("-T")
        .arg(&args.host)
        .arg(remote_port_command(nvim))
        .output()
        .with_context(|| format!("failed to start `{}`", args.ssh.to_string_lossy()))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        let detail = if stderr.is_empty() {
            String::new()
        } else {
            format!(": {stderr}")
        };
        bail!("could not allocate ports on `{}`{}", args.host, detail);
    }
    parse_port_pair(&String::from_utf8_lossy(&output.stdout))
        .with_context(|| format!("invalid response from `{}`", args.host))
}

fn ssh_command(args: &SshArgs) -> Command {
    let mut command = Command::new(&args.ssh);
    for option in &args.ssh_options {
        command.arg("-o").arg(option);
    }
    command
}

struct SshControl {
    directory: PathBuf,
    path: PathBuf,
    ssh: OsString,
    host: String,
    options: Vec<String>,
}

impl SshControl {
    fn new(args: &SshArgs) -> Result<Self> {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let directory = short_temp_root().join(format!("argone-{}-{nonce}", std::process::id()));
        fs::create_dir(&directory).with_context(|| {
            format!(
                "could not create SSH session directory `{}`",
                directory.display()
            )
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        }
        let path = directory.join("ssh");
        if path.as_os_str().len() > 90 {
            let _ = fs::remove_dir(&directory);
            bail!(
                "temporary SSH control path is too long: `{}`",
                path.display()
            );
        }
        Ok(Self {
            directory,
            path,
            ssh: args.ssh.clone(),
            host: args.host.clone(),
            options: args.ssh_options.clone(),
        })
    }

    fn command(&self, args: &SshArgs) -> Command {
        let mut command = ssh_command(args);
        command
            .arg("-o")
            .arg("ControlMaster=auto")
            .arg("-o")
            .arg("ControlPersist=30")
            .arg("-o")
            .arg(format!("ControlPath={}", self.path.display()));
        command
    }
}

impl Drop for SshControl {
    fn drop(&mut self) {
        let mut command = Command::new(&self.ssh);
        for option in &self.options {
            command.arg("-o").arg(option);
        }
        let _ = command
            .arg("-S")
            .arg(&self.path)
            .arg("-O")
            .arg("exit")
            .arg(&self.host)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status();
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}

#[cfg(unix)]
fn short_temp_root() -> PathBuf {
    PathBuf::from("/tmp")
}

#[cfg(windows)]
fn short_temp_root() -> PathBuf {
    env::temp_dir()
}

fn remote_nvim_command(nvim: &OsStr, path: &Path, analyzer_port: u16) -> String {
    let path = shell_quote(&path.to_string_lossy());
    let nvim = shell_quote(&nvim.to_string_lossy());
    format!(
        "set -- {path}; \
         if [ -d \"$1\" ]; then \
           cd -- \"$1\" || exit 1; \
           if [ ! -f lib.ar ]; then \
             printf '%s\\n' 'argone: remote project does not contain lib.ar' >&2; exit 1; \
           fi; \
           set -- lib.ar; \
         elif [ -f \"$1\" ]; then \
           argon_file=$1; \
           cd -- \"$(dirname -- \"$argon_file\")\" || exit 1; \
           set -- \"$(basename -- \"$argon_file\")\"; \
         else \
           printf '%s\\n' 'argone: remote path is not a file or directory' >&2; exit 1; \
         fi; \
         exec {nvim} \
           --cmd 'let g:argon_analyzer_rpc_port = {analyzer_port}' \
           \"$1\""
    )
}

fn remote_port_command(nvim: &OsStr) -> String {
    let script = concat!(
        "lua local a=assert(vim.uv.new_tcp()); ",
        "assert(a:bind('127.0.0.1', 0)); ",
        "local b=assert(vim.uv.new_tcp()); ",
        "assert(b:bind('127.0.0.1', 0)); ",
        "io.stdout:write(a:getsockname().port .. ' ' .. b:getsockname().port .. '\\n')"
    );
    format!(
        "{} --clean --headless -i NONE -n -c {} -c qa",
        shell_quote(&nvim.to_string_lossy()),
        shell_quote(script)
    )
}

fn launch_forwarded_gui(
    analyzer_addr: SocketAddrV4,
    gui_port: u16,
    register_addr: SocketAddrV4,
) -> Result<Child> {
    Command::new(env::current_exe()?)
        .arg("__gui")
        .arg(analyzer_addr.to_string())
        .arg("--listen-port")
        .arg(gui_port.to_string())
        .arg("--register-addr")
        .arg(register_addr.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start the local Argon GUI")
}

fn wait_for_analyzer(ssh: &mut Child, addr: SocketAddrV4) -> Result<()> {
    loop {
        if endpoint_is_live(addr) {
            return Ok(());
        }
        if let Some(status) = ssh.try_wait().context("failed while waiting for SSH")? {
            bail!("SSH exited before the remote analyzer started ({status})");
        }
        thread::sleep(ANALYZER_PROBE_INTERVAL);
    }
}

fn endpoint_is_live(addr: SocketAddrV4) -> bool {
    let Ok(mut stream) = TcpStream::connect_timeout(&addr.into(), ANALYZER_PROBE_TIMEOUT) else {
        return false;
    };
    if stream
        .set_read_timeout(Some(ANALYZER_PROBE_TIMEOUT))
        .is_err()
    {
        return false;
    }
    let mut byte = [0];
    matches!(
        stream.read(&mut byte),
        Err(error) if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut)
    )
}

fn allocate_port_pair() -> Result<(u16, u16)> {
    let first = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    let second = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok((first.local_addr()?.port(), second.local_addr()?.port()))
}

fn parse_port_pair(output: &str) -> Result<(u16, u16)> {
    for line in output.lines().rev() {
        let mut fields = line.split_whitespace();
        let Some(first) = fields.next().and_then(|port| port.parse().ok()) else {
            continue;
        };
        let Some(second) = fields.next().and_then(|port| port.parse().ok()) else {
            continue;
        };
        if fields.next().is_none() {
            return Ok((first, second));
        }
    }
    bail!("response did not contain an analyzer and GUI port")
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(unix)]
fn success_status() -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

#[cfg(windows)]
fn success_status() -> ExitStatus {
    use std::os::windows::process::ExitStatusExt;
    ExitStatus::from_raw(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_local_and_ssh_commands() {
        let local = Cli::try_parse_from(["argone", "project"]).unwrap();
        assert!(local.command.is_none());
        assert_eq!(local.path, Path::new("project"));

        let remote = Cli::try_parse_from(["argone", "ssh", "server", "/work/chip"]).unwrap();
        let Some(CommandKind::Ssh(remote)) = remote.command else {
            panic!("expected SSH command");
        };
        assert_eq!(remote.host, "server");
        assert_eq!(remote.path, Path::new("/work/chip"));
    }

    #[test]
    fn parses_exactly_two_ports() {
        assert_eq!(parse_port_pair("1234 5678\n").unwrap(), (1234, 5678));
        assert_eq!(
            parse_port_pair("Welcome to the build server\n1234 5678\n").unwrap(),
            (1234, 5678)
        );
        assert!(parse_port_pair("1234").is_err());
        assert!(parse_port_pair("1234 5678 9012").is_err());
    }

    #[test]
    fn quotes_remote_shell_arguments() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a b's"), "'a b'\"'\"'s'");
    }

    #[test]
    fn builds_remote_command_without_unquoted_user_input() {
        let command = remote_nvim_command(
            OsStr::new("custom nvim"),
            Path::new("/work/a project's files"),
            12001,
        );
        assert!(command.starts_with("set -- '/work/a project'\"'\"'s files';"));
        assert!(command.contains("let g:argon_analyzer_rpc_port = 12001"));
        assert!(command.contains("exec 'custom nvim'"));
        assert!(command.ends_with("\"$1\""));
    }

    #[test]
    fn remote_port_probe_uses_only_neovim() {
        let command = remote_port_command(OsStr::new("custom nvim"));
        assert!(command.starts_with("'custom nvim' --clean --headless"));
        assert!(command.contains("vim.uv.new_tcp()"));
    }

    #[cfg(unix)]
    #[test]
    fn remote_launch_command_handles_project_paths_with_spaces() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let project = env::temp_dir().join(format!("argone remote project {nonce}"));
        fs::create_dir(&project).unwrap();
        fs::write(project.join("lib.ar"), "cell top() {}\n").unwrap();

        let command = remote_nvim_command(OsStr::new("/usr/bin/true"), &project, 12001);
        let status = Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .status()
            .unwrap();
        assert!(status.success());

        fs::remove_file(project.join("lib.ar")).unwrap();
        fs::remove_dir(project).unwrap();
    }

    #[test]
    fn project_directory_opens_lib_source_from_that_directory() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let project = env::temp_dir().join(format!("argone-project-{nonce}"));
        fs::create_dir(&project).unwrap();
        fs::write(project.join("lib.ar"), "cell top() {}\n").unwrap();

        let location = nvim_location(&project).unwrap();
        assert_eq!(location, (project.clone(), PathBuf::from("lib.ar")));

        fs::remove_file(project.join("lib.ar")).unwrap();
        fs::remove_dir(project).unwrap();
    }
}
