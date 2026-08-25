use std::{
    env,
    ffi::{OsStr, OsString},
    fs,
    io::{self, BufRead, BufReader, Write},
    net::{Ipv4Addr, SocketAddr, SocketAddrV4, TcpListener, TcpStream},
    path::{Path, PathBuf},
    process::{Child, ChildStdin, Command, ExitCode, ExitStatus, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand};

const STARTUP_POLL_INTERVAL: Duration = Duration::from_millis(100);
const STARTUP_TIMEOUT: Duration = Duration::from_secs(30);
const LOCAL_FORWARD_ATTEMPTS: usize = 8;
const NVIM_FOCUS_MAPPING: &str = "lua local function focus_argon_gui() local client = require('argon.client'); client.any_buf_request('custom/startGui', nil, client.print_error) end; for _, lhs in ipairs({ '<C-Bslash>', string.char(28) }) do vim.keymap.set({ 'n', 'i', 'v', 'c', 't' }, lhs, focus_argon_gui, { desc = 'Focus Argon GUI', silent = true, nowait = true }) end";

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

    /// Run only the graphical editor connected to an analyzer.
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

    /// Local port forwarded to the remote analyzer.
    #[arg(long)]
    local_analyzer_port: Option<u16>,

    /// Analyzer RPC port on the remote host.
    #[arg(long)]
    remote_analyzer_port: Option<u16>,

    /// Local GUI callback port.
    #[arg(long)]
    local_gui_port: Option<u16>,

    /// GUI callback port exposed on the remote host.
    #[arg(long)]
    remote_gui_port: Option<u16>,
}

#[derive(Debug, Args)]
struct GuiArgs {
    /// Analyzer RPC address to connect to.
    lang_server_addr: Option<SocketAddr>,

    /// Local callback port. Omit to let the operating system allocate one.
    #[arg(long)]
    listen_port: Option<u16>,

    /// Callback address advertised to the analyzer, for example through an SSH tunnel.
    #[arg(long)]
    register_addr: Option<SocketAddr>,

    /// Coordinate an SSH launch through stdin and stdout.
    #[arg(long)]
    ssh_control: bool,
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
        None => {
            let focus_target = argone::focus::capture_target();
            run_nvim(&cli.nvim, &cli.path, focus_target.as_deref())
        }
        Some(CommandKind::Ssh(args)) => {
            let focus_target = argone::focus::capture_target();
            run_ssh(&cli.nvim, args, focus_target.as_deref())
        }
        Some(CommandKind::Gui(args)) => run_gui(args),
    }
}

fn run_gui(args: GuiArgs) -> Result<ExitStatus> {
    if args.ssh_control {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, args.listen_port.unwrap_or(0)))
            .context("failed to bind the GUI callback listener")?;
        let listen_addr = listener.local_addr()?;
        println!("ARGON_GUI 1 {}", listen_addr.port());
        io_flush_stdout()?;
        let mut forwarding = String::new();
        std::io::stdin()
            .lock()
            .read_line(&mut forwarding)
            .context("failed to read SSH forwarding configuration")?;
        let (lang_server_addr, register_addr) = parse_address_pair(&forwarding)?;
        argone::run_with_listener(lang_server_addr, listener, register_addr);
    } else {
        let lang_server_addr = args.lang_server_addr.ok_or_else(|| {
            anyhow!("an analyzer address is required unless --ssh-control is used")
        })?;
        argone::run(lang_server_addr, args.listen_port, args.register_addr);
    }
    Ok(success_status())
}

fn io_flush_stdout() -> Result<()> {
    std::io::stdout().flush().context("failed to flush stdout")
}

fn run_nvim(nvim: &OsStr, path: &Path, focus_target: Option<&str>) -> Result<ExitStatus> {
    let (working_directory, target) = nvim_location(path)?;
    let mut command = Command::new(nvim);
    command
        .current_dir(working_directory)
        .arg("--cmd")
        .arg("let g:argon_auto_gui = v:true")
        .arg("--cmd")
        .arg(NVIM_FOCUS_MAPPING)
        .arg(target)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());
    if let Some(focus_target) = focus_target {
        command.env(argone::focus::TARGET_ENV, focus_target);
    }
    command
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

fn run_ssh(nvim: &OsStr, args: SshArgs, focus_target: Option<&str>) -> Result<ExitStatus> {
    let control = SshControl::new(&args)?;
    validate_port_overrides(&args)?;
    let analyzer = resolve_remote_analyzer(&args, &control)?;
    let mut relay = start_remote_relay(&args, &control, &analyzer)?;

    let mut nvim_command = control.command(&args);
    nvim_command
        .arg("-t")
        .arg(&args.host)
        .arg(interactive_terminal_command(&remote_nvim_command(
            nvim,
            &args.path,
            args.remote_analyzer_port,
            &relay.socket_path,
        )))
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit());

    let mut nvim_ssh = nvim_command
        .spawn()
        .with_context(|| format!("failed to start `{}`", args.ssh.to_string_lossy()))?;

    let result = start_forwarded_session(&args, &control, &mut nvim_ssh, &mut relay, focus_target);
    let (mut tunnel, mut gui) = match result {
        Ok(processes) => processes,
        Err(error) => {
            let _ = nvim_ssh.kill();
            let _ = nvim_ssh.wait();
            return Err(error);
        }
    };

    let status = nvim_ssh.wait();
    let _ = gui.kill();
    let _ = gui.wait();
    let _ = tunnel.kill();
    let _ = tunnel.wait();
    status.context("failed while waiting for SSH")
}

fn start_forwarded_session(
    args: &SshArgs,
    control: &SshControl,
    nvim_ssh: &mut Child,
    relay: &mut Relay,
    focus_target: Option<&str>,
) -> Result<(Child, Child)> {
    let remote_analyzer_port = wait_for_remote_analyzer(nvim_ssh, relay)?;
    let mut gui = launch_forwarded_gui(args.local_gui_port, focus_target)?;
    let local_gui_port = gui.port;

    let tunnel = match start_tunnel(args, control, remote_analyzer_port, local_gui_port) {
        Ok(tunnel) => tunnel,
        Err(error) => {
            gui.stop();
            return Err(error);
        }
    };
    let analyzer_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, tunnel.local_analyzer_port);
    let gui_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, tunnel.remote_gui_port);
    if let Err(error) =
        writeln!(gui.input, "{analyzer_addr} {gui_addr}").and_then(|_| gui.input.flush())
    {
        let mut child = tunnel.child;
        let _ = child.kill();
        let _ = child.wait();
        gui.stop();
        return Err(error).context("failed to send forwarding configuration to the GUI");
    }
    Ok((tunnel.child, gui.child))
}

fn validate_port_overrides(args: &SshArgs) -> Result<()> {
    for (name, port) in [
        ("--local-analyzer-port", args.local_analyzer_port),
        ("--remote-analyzer-port", args.remote_analyzer_port),
        ("--local-gui-port", args.local_gui_port),
        ("--remote-gui-port", args.remote_gui_port),
    ] {
        if port == Some(0) {
            bail!("{name} must be nonzero; omit it to allocate a port automatically");
        }
    }
    if args.local_analyzer_port.is_some() && args.local_analyzer_port == args.local_gui_port {
        bail!("local analyzer and GUI ports must be different");
    }
    if args.remote_analyzer_port.is_some() && args.remote_analyzer_port == args.remote_gui_port {
        bail!("remote analyzer and GUI ports must be different");
    }
    Ok(())
}

fn resolve_remote_analyzer(args: &SshArgs, control: &SshControl) -> Result<String> {
    let probe = interactive_shell_command(
        "analyzer=$(command -v argon-analyzer) && [ -x \"$analyzer\" ] || exit 127; \
         printf '\\nARGON_EXECUTABLE 1 %s\\n' \"$analyzer\"",
    );
    let output = control
        .command(args)
        .arg("-T")
        .arg(&args.host)
        .arg(probe)
        .output()
        .with_context(|| format!("failed to start `{}`", args.ssh.to_string_lossy()))?;
    if output.status.success() {
        return parse_executable_announcement(&String::from_utf8_lossy(&output.stdout))
            .map(str::to_owned)
            .context("the remote shell did not report the `argon-analyzer` executable path");
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = format!("{stdout}\n{stderr}").trim().to_owned();
    if output.status.code() == Some(127) || detail.is_empty() {
        bail!(
            "`argon-analyzer` was not found on `{}` in the interactive shell environment; install it on the remote machine and ensure it is available on PATH",
            args.host
        );
    }
    bail!(
        "could not check for `argon-analyzer` on `{}`: {detail}",
        args.host
    )
}

fn parse_executable_announcement(output: &str) -> Option<&str> {
    output.lines().find_map(|line| {
        line.strip_prefix("ARGON_EXECUTABLE 1 ")
            .map(str::trim)
            .filter(|path| !path.is_empty())
    })
}

struct Relay {
    child: Child,
    lines: mpsc::Receiver<io::Result<String>>,
    socket_path: String,
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn start_remote_relay(args: &SshArgs, control: &SshControl, analyzer: &str) -> Result<Relay> {
    let mut child = control
        .command(args)
        .arg("-T")
        .arg(&args.host)
        .arg(format!("exec {} relay", shell_quote(analyzer)))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start the remote analyzer relay")?;
    let stdout = child.stdout.take().expect("relay stdout was piped");
    let (line_tx, lines) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines() {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let mut relay = Relay {
        child,
        lines,
        socket_path: String::new(),
    };
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match relay.lines.recv_timeout(STARTUP_POLL_INTERVAL) {
            Ok(Ok(line)) => {
                if let Some(socket_path) = parse_relay_announcement(&line) {
                    relay.socket_path = socket_path.to_owned();
                    return Ok(relay);
                }
            }
            Ok(Err(error)) => return Err(error.into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }
        if let Some(status) = relay.child.try_wait()? {
            bail!("remote analyzer relay failed to start ({status})");
        }
        if Instant::now() >= deadline {
            bail!("timed out starting the remote analyzer relay");
        }
    }
}

fn parse_relay_announcement(line: &str) -> Option<&str> {
    line.strip_prefix("ARGON_RELAY 1 ")
        .map(str::trim)
        .filter(|path| !path.is_empty())
}

fn parse_analyzer_announcement(line: &str) -> Option<u16> {
    line.strip_prefix("ARGON_ANALYZER 1 ")
        .and_then(|port| port.trim().parse().ok())
        .filter(|port| *port != 0)
}

fn ssh_command(args: &SshArgs) -> Command {
    let mut command = Command::new(&args.ssh);
    for option in &args.ssh_options {
        command.arg("-o").arg(option);
    }
    command
}

struct SshControl {
    _directory: tempfile::TempDir,
    path: PathBuf,
    ssh: OsString,
    host: String,
    options: Vec<String>,
}

impl SshControl {
    fn new(args: &SshArgs) -> Result<Self> {
        let directory = tempfile::Builder::new()
            .prefix("argone-")
            .tempdir_in(short_temp_root())
            .context("could not create a temporary SSH session directory")?;
        let path = directory.path().join("ssh");
        if path.as_os_str().len() > 90 {
            bail!(
                "temporary SSH control path is too long: `{}`",
                path.display()
            );
        }
        Ok(Self {
            _directory: directory,
            path,
            ssh: args.ssh.clone(),
            host: args.host.clone(),
            options: args.ssh_options.clone(),
        })
    }

    fn command(&self, args: &SshArgs) -> Command {
        let mut command = ssh_command(args);
        #[cfg(unix)]
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
        #[cfg(unix)]
        {
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
        }
        let _ = fs::remove_file(&self.path);
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

fn remote_nvim_command(
    nvim: &OsStr,
    path: &Path,
    analyzer_port: Option<u16>,
    relay_socket: &str,
) -> String {
    let path = shell_quote(&path.to_string_lossy());
    let nvim = shell_quote(&nvim.to_string_lossy());
    let relay_vim = shell_quote(&format!(
        "let g:argon_analyzer_relay = '{}'",
        relay_socket.replace('\'', "''")
    ));
    let focus_mapping = shell_quote(NVIM_FOCUS_MAPPING);
    let rpc_port = analyzer_port
        .map(|port| format!(" --cmd 'let g:argon_analyzer_rpc_port = {port}'"))
        .unwrap_or_default();
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
         exec {nvim}{rpc_port} --cmd {relay_vim} --cmd {focus_mapping} \"$1\""
    )
}

fn wait_for_remote_analyzer(nvim_ssh: &mut Child, relay: &mut Relay) -> Result<u16> {
    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match relay.lines.recv_timeout(STARTUP_POLL_INTERVAL) {
            Ok(Ok(line)) => {
                if let Some(port) = parse_analyzer_announcement(&line) {
                    let _ = relay.child.wait();
                    return Ok(port);
                }
            }
            Ok(Err(error)) => return Err(error.into()),
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {}
        }
        if let Some(status) = relay
            .child
            .try_wait()
            .context("failed while waiting for the analyzer relay")?
        {
            bail!("analyzer relay exited before reporting the RPC port ({status})");
        }
        if let Some(status) = nvim_ssh
            .try_wait()
            .context("failed while waiting for Neovim")?
        {
            bail!("remote Neovim exited before the analyzer started ({status})");
        }
        if Instant::now() >= deadline {
            bail!("timed out waiting for `argon-analyzer` to start");
        }
    }
}

struct GuiLaunch {
    child: Child,
    input: ChildStdin,
    port: u16,
}

impl GuiLaunch {
    fn stop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn launch_forwarded_gui(gui_port: Option<u16>, focus_target: Option<&str>) -> Result<GuiLaunch> {
    let mut command = Command::new(env::current_exe()?);
    command.arg("gui").arg("--ssh-control");
    if let Some(port) = gui_port {
        command.arg("--listen-port").arg(port.to_string());
    }
    if let Some(focus_target) = focus_target {
        command.env(argone::focus::TARGET_ENV, focus_target);
    }
    let mut child = command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .context("failed to start the local Argon GUI")?;
    let input = child.stdin.take().expect("GUI stdin was piped");
    let stdout = child.stdout.take().expect("GUI stdout was piped");
    let (line_tx, line_rx) = mpsc::channel();
    thread::spawn(move || {
        let mut stdout = BufReader::new(stdout);
        let mut line = String::new();
        let result = stdout.read_line(&mut line).map(|_| line);
        if line_tx.send(result).is_ok() {
            let _ = io::copy(&mut stdout, &mut io::sink());
        }
    });

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    loop {
        match line_rx.recv_timeout(STARTUP_POLL_INTERVAL) {
            Ok(Ok(line)) => {
                let port = match parse_gui_announcement(&line) {
                    Ok(port) => port,
                    Err(error) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        return Err(error).context("the GUI reported an invalid callback port");
                    }
                };
                return Ok(GuiLaunch { child, input, port });
            }
            Ok(Err(error)) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error.into());
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {}
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("GUI closed stdout before reporting its callback port");
            }
        }
        if let Some(status) = child.try_wait()? {
            bail!("GUI exited before startup completed ({status})");
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            bail!("timed out waiting for the GUI to start");
        }
    }
}

struct Tunnel {
    child: Child,
    local_analyzer_port: u16,
    remote_gui_port: u16,
}

fn start_tunnel(
    args: &SshArgs,
    control: &SshControl,
    remote_analyzer_port: u16,
    local_gui_port: u16,
) -> Result<Tunnel> {
    let attempts = if args.local_analyzer_port.is_some() {
        1
    } else {
        LOCAL_FORWARD_ATTEMPTS
    };
    let mut last_collision = None;
    for _ in 0..attempts {
        let local_analyzer_port = match args.local_analyzer_port {
            Some(port) => port,
            None => available_local_port()?,
        };
        match start_tunnel_once(
            args,
            control,
            local_analyzer_port,
            remote_analyzer_port,
            local_gui_port,
        ) {
            Ok(tunnel) => return Ok(tunnel),
            Err(error) if args.local_analyzer_port.is_none() && error.local_collision => {
                last_collision = Some(error.message);
            }
            Err(error) => bail!(error.message),
        }
    }
    bail!(
        "could not allocate a local analyzer forwarding port after {LOCAL_FORWARD_ATTEMPTS} attempts{}",
        last_collision
            .map(|error| format!(": {error}"))
            .unwrap_or_default()
    )
}

struct TunnelStartError {
    message: String,
    local_collision: bool,
}

fn start_tunnel_once(
    args: &SshArgs,
    control: &SshControl,
    local_analyzer_port: u16,
    remote_analyzer_port: u16,
    local_gui_port: u16,
) -> std::result::Result<Tunnel, TunnelStartError> {
    let local_analyzer_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, local_analyzer_port);
    let mut command = control.command(args);
    command
        // A multiplex control operation does not report the port allocated for `-R 0`.
        .arg("-S")
        .arg("none")
        .arg("-o")
        .arg("ExitOnForwardFailure=yes")
        .arg("-o")
        .arg("LogLevel=INFO")
        .arg("-L")
        .arg(format!(
            "{local_analyzer_addr}:127.0.0.1:{remote_analyzer_port}"
        ))
        .arg("-R")
        .arg(format!(
            "127.0.0.1:{}:127.0.0.1:{local_gui_port}",
            args.remote_gui_port.unwrap_or(0)
        ))
        .arg("-N")
        .arg(&args.host)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| TunnelStartError {
        message: format!("failed to start `{}`: {error}", args.ssh.to_string_lossy()),
        local_collision: false,
    })?;
    let stderr = child.stderr.take().expect("SSH stderr was piped");
    let (line_tx, line_rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stderr).lines() {
            if line_tx.send(line).is_err() {
                break;
            }
        }
    });

    let deadline = Instant::now() + STARTUP_TIMEOUT;
    let mut lines = Vec::new();
    let mut remote_gui_port = args.remote_gui_port;
    loop {
        while let Ok(line) = line_rx.try_recv() {
            match line {
                Ok(line) => {
                    if remote_gui_port.is_none() {
                        remote_gui_port = parse_allocated_remote_port(&line);
                    }
                    lines.push(line);
                }
                Err(error) => lines.push(error.to_string()),
            }
        }
        if let Some(status) = child.try_wait().map_err(|error| TunnelStartError {
            message: format!("failed while waiting for the SSH tunnel: {error}"),
            local_collision: false,
        })? {
            for line in line_rx {
                match line {
                    Ok(line) => lines.push(line),
                    Err(error) => lines.push(error.to_string()),
                }
            }
            let detail = lines.join("\n");
            return Err(TunnelStartError {
                local_collision: local_forward_collision(&detail, local_analyzer_port),
                message: if detail.is_empty() {
                    format!("SSH tunnel exited before forwarding was ready ({status})")
                } else {
                    format!("SSH could not establish forwarding: {detail}")
                },
            });
        }
        let detail = lines.join("\n");
        if local_forward_collision(&detail, local_analyzer_port) {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TunnelStartError {
                local_collision: true,
                message: format!("SSH could not establish forwarding: {detail}"),
            });
        }
        if let Some(remote_gui_port) = remote_gui_port.filter(|_| {
            TcpStream::connect_timeout(&local_analyzer_addr.into(), STARTUP_POLL_INTERVAL).is_ok()
        }) {
            thread::spawn(move || {
                while let Ok(line) = line_rx.recv() {
                    if let Ok(line) = line {
                        eprintln!("{line}");
                    }
                }
            });
            return Ok(Tunnel {
                child,
                local_analyzer_port,
                remote_gui_port,
            });
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(TunnelStartError {
                message: "timed out waiting for SSH forwarding to become ready".to_owned(),
                local_collision: false,
            });
        }
        thread::sleep(STARTUP_POLL_INTERVAL);
    }
}

fn available_local_port() -> Result<u16> {
    let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
    Ok(listener.local_addr()?.port())
}

fn parse_allocated_remote_port(line: &str) -> Option<u16> {
    let rest = line
        .find("Allocated port ")
        .map(|start| &line[start + "Allocated port ".len()..])?;
    rest.split_whitespace().next()?.parse().ok()
}

fn local_forward_collision(stderr: &str, port: u16) -> bool {
    let mentions_port = stderr.contains(&port.to_string());
    mentions_port
        && (stderr.contains("Address already in use")
            || stderr.contains("cannot listen to port")
            || stderr.contains("Could not request local forwarding"))
}

fn parse_port(value: &str) -> Result<u16> {
    let port = value.trim().parse::<u16>()?;
    if port == 0 {
        bail!("port must be nonzero");
    }
    Ok(port)
}

fn parse_gui_announcement(value: &str) -> Result<u16> {
    let port = value
        .strip_prefix("ARGON_GUI 1 ")
        .ok_or_else(|| anyhow!("unexpected GUI startup response"))?;
    parse_port(port)
}

fn parse_address_pair(value: &str) -> Result<(SocketAddr, SocketAddr)> {
    let mut fields = value.split_whitespace();
    let analyzer = fields
        .next()
        .ok_or_else(|| anyhow!("missing analyzer forwarding address"))?
        .parse()?;
    let gui = fields
        .next()
        .ok_or_else(|| anyhow!("missing GUI forwarding address"))?
        .parse()?;
    if fields.next().is_some() {
        bail!("forwarding configuration contains extra fields");
    }
    Ok((analyzer, gui))
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn interactive_shell_command(command: &str) -> String {
    format!("exec \"${{SHELL:-/bin/sh}}\" -ic {}", shell_quote(command))
}

fn interactive_terminal_command(command: &str) -> String {
    // Hide the terminal during shell startup, then restore it for Neovim.
    let command = format!("exec <&3 >&4 2>&5; {command}");
    format!(
        "exec 3<&0 4>&1 5>&2; exec \"${{SHELL:-/bin/sh}}\" -ic {} </dev/null >/dev/null 2>/dev/null",
        shell_quote(&command)
    )
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
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::*;

    fn ssh_args(ssh: impl Into<OsString>) -> SshArgs {
        SshArgs {
            host: "server".to_owned(),
            path: PathBuf::from("."),
            ssh: ssh.into(),
            ssh_options: Vec::new(),
            local_analyzer_port: None,
            remote_analyzer_port: None,
            local_gui_port: None,
            remote_gui_port: None,
        }
    }

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
    fn parses_forwarding_addresses() {
        assert_eq!(
            parse_address_pair("127.0.0.1:1234 127.0.0.1:5678\n").unwrap(),
            (
                "127.0.0.1:1234".parse().unwrap(),
                "127.0.0.1:5678".parse().unwrap()
            )
        );
        assert!(parse_address_pair("127.0.0.1:1234").is_err());
        assert!(parse_address_pair("127.0.0.1:1 127.0.0.1:2 extra").is_err());
    }

    #[test]
    fn parses_startup_protocol_messages() {
        assert_eq!(parse_gui_announcement("ARGON_GUI 1 1234\n").unwrap(), 1234);
        assert_eq!(
            parse_relay_announcement("ARGON_RELAY 1 /tmp/session/rpc.sock\n"),
            Some("/tmp/session/rpc.sock")
        );
        assert_eq!(
            parse_analyzer_announcement("ARGON_ANALYZER 1 5678\n"),
            Some(5678)
        );
        assert!(parse_gui_announcement("1234").is_err());
        assert_eq!(parse_analyzer_announcement("ARGON_ANALYZER 2 5678"), None);
    }

    #[test]
    fn parses_openssh_allocated_port_message() {
        assert_eq!(
            parse_allocated_remote_port(
                "Allocated port 45678 for remote forward to 127.0.0.1:1234"
            ),
            Some(45678)
        );
        assert_eq!(
            parse_allocated_remote_port(
                "debug1: Allocated port 45678 for remote forward to 127.0.0.1:1234"
            ),
            Some(45678)
        );
        assert_eq!(parse_allocated_remote_port("Permission denied"), None);
    }

    #[test]
    fn recognizes_only_local_forward_collisions() {
        assert!(local_forward_collision(
            "bind [127.0.0.1]:45678: Address already in use\nCould not request local forwarding.",
            45678
        ));
        assert!(!local_forward_collision("Permission denied", 45678));
    }

    #[cfg(unix)]
    #[test]
    fn missing_remote_analyzer_has_an_actionable_error() {
        let args = ssh_args("/usr/bin/false");
        let control = SshControl::new(&args).unwrap();
        let error = resolve_remote_analyzer(&args, &control).unwrap_err();
        assert_eq!(
            error.to_string(),
            "`argon-analyzer` was not found on `server` in the interactive shell environment; install it on the remote machine and ensure it is available on PATH"
        );
    }

    #[test]
    fn quotes_remote_shell_arguments() {
        assert_eq!(shell_quote("plain"), "'plain'");
        assert_eq!(shell_quote("a b's"), "'a b'\"'\"'s'");
        assert_eq!(
            interactive_shell_command("exec nvim 'some path'"),
            "exec \"${SHELL:-/bin/sh}\" -ic 'exec nvim '\"'\"'some path'\"'\"''"
        );
        assert_eq!(
            interactive_terminal_command("exec nvim"),
            "exec 3<&0 4>&1 5>&2; exec \"${SHELL:-/bin/sh}\" -ic 'exec <&3 >&4 2>&5; exec nvim' </dev/null >/dev/null 2>/dev/null"
        );
    }

    #[test]
    fn finds_executable_announcements_among_shell_output() {
        assert_eq!(
            parse_executable_announcement(
                "Welcome to the server\r\nARGON_EXECUTABLE 1 /home/me/.cargo/bin/argon-analyzer\r\n"
            ),
            Some("/home/me/.cargo/bin/argon-analyzer")
        );
    }

    #[test]
    fn builds_remote_command_without_unquoted_user_input() {
        let command = remote_nvim_command(
            OsStr::new("custom nvim"),
            Path::new("/work/a project's files"),
            Some(12001),
            "/tmp/argon-session/analyzer.sock",
        );
        assert!(command.contains("set -- '/work/a project'\"'\"'s files';"));
        assert!(command.contains("let g:argon_analyzer_rpc_port = 12001"));
        assert!(command.contains("let g:argon_analyzer_relay"));
        assert!(command.contains("<C-Bslash>"));
        assert!(command.contains("'custom nvim'"));
    }

    #[cfg(unix)]
    #[test]
    fn interactive_shell_passes_relay_socket_to_nvim() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        fs::create_dir(&project).unwrap();
        fs::write(project.join("lib.ar"), "cell top() {}\n").unwrap();

        let nvim = directory.path().join("fake-nvim");
        fs::write(&nvim, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").unwrap();
        fs::set_permissions(&nvim, fs::Permissions::from_mode(0o700)).unwrap();

        let command = interactive_terminal_command(&remote_nvim_command(
            nvim.as_os_str(),
            &project,
            None,
            "/tmp/relay socket.sock",
        ));
        let output = Command::new("/bin/sh")
            .arg("-c")
            .arg(command)
            .env("SHELL", "/bin/sh")
            .output()
            .unwrap();

        assert!(output.status.success());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            format!(
                "--cmd\nlet g:argon_analyzer_relay = '/tmp/relay socket.sock'\n--cmd\n{NVIM_FOCUS_MAPPING}\nlib.ar\n"
            )
        );
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

        let command = remote_nvim_command(
            OsStr::new("/usr/bin/true"),
            &project,
            Some(12001),
            "/tmp/argon-session/analyzer.sock",
        );
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
