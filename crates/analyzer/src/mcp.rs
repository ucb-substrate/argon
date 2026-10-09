//! A Model Context Protocol server that lets agents work through running
//! analyzers, so their edits land in the user's Neovim buffers.
//!
//! It speaks newline-delimited JSON-RPC on stdin and stdout. Each tool call
//! goes to the session whose workspace contains the file it names, or to the
//! session chosen with `workspace`, the one used last, or the only one running.

use std::{
    collections::HashMap,
    io,
    net::{Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
    sync::Arc,
};

use serde_json::{Value, json};
use tarpc::{context, tokio_serde::formats::Bincode};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    sync::{Mutex, mpsc},
};

use crate::{
    agent::{
        AgentClient, AgentStatus, CALL_DEADLINE, CompileStatus, CreateRequest, EditRequest,
        Replacement, Report,
    },
    session::{self, SessionInfo},
    transport,
};

/// Used when a client asks for no particular protocol version.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

const INSTRUCTIONS: &str = "This server edits Argon layout sources through the user's running \
Argon sessions. Edits go into the user's Neovim buffers, so the user watches them land in Neovim \
and the layout GUI and can undo them. Buffers may hold unsaved changes, so read .ar files with \
read_file rather than from disk, edit them with edit_file or create_file rather than writing \
files, and check cells with compile_cell rather than `arc run`. Never save files; the user does. \
The user may have several Argon libraries open, one session each: file tools use the session \
that contains the file, and the other tools take `workspace` to choose one. Call status to see \
which sessions are running.";

pub(crate) fn no_session_message(directory: &Path) -> String {
    format!(
        "No running Argon session was found for {}. Ask the user to open the project with `argone` (or open its lib.ar in Neovim with the Argon plugin), then try again.",
        directory.display()
    )
}

async fn connect_agent(info: &SessionInfo) -> io::Result<AgentClient> {
    let token = info.token().ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "the session record has no valid token",
        )
    })?;
    let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, info.port));
    let stream = transport::connect(addr, &token, transport::Role::Agent).await?;
    let transport = tarpc::serde_transport::new(stream, Bincode::default());
    Ok(AgentClient::new(tarpc::client::Config::default(), transport).spawn())
}

/// Forgets the record of an analyzer that is no longer listening.
fn forget_if_gone(record: &Path, error: &io::Error) {
    if error.kind() == io::ErrorKind::ConnectionRefused {
        let _ = std::fs::remove_file(record);
    }
}

/// Connects to the innermost running analyzer whose workspace contains
/// `directory`, discarding records of analyzers that are gone.
pub(crate) async fn connect_session(directory: &Path) -> Result<AgentClient, String> {
    let mut last_error = None;
    for (record, info) in session::discover(directory) {
        match connect_agent(&info).await {
            Ok(client) => return Ok(client),
            Err(error) => {
                forget_if_gone(&record, &error);
                last_error = Some(error);
            }
        }
    }
    Err(match last_error {
        Some(error) => format!(
            "Could not connect to the Argon session for {}: {error}",
            directory.display()
        ),
        None => no_session_message(directory),
    })
}

fn call_context() -> context::Context {
    let mut context = context::current();
    context.deadline = std::time::Instant::now() + CALL_DEADLINE;
    context
}

fn is_disconnected(error: &tarpc::client::RpcError) -> bool {
    matches!(
        error,
        tarpc::client::RpcError::Shutdown
            | tarpc::client::RpcError::Send(_)
            | tarpc::client::RpcError::Channel(_)
    )
}

/// A connected analyzer.
#[derive(Clone)]
struct Session {
    /// The session record, which identifies one analyzer process.
    record: PathBuf,
    root: PathBuf,
    client: AgentClient,
}

/// Which session a tool call is for.
#[derive(Clone, Copy)]
enum Target<'a> {
    /// The innermost session whose workspace contains this file or directory.
    Containing(&'a Path),
    /// The session used last, else one containing the bridge's directory,
    /// else the only one running.
    Default,
}

/// Shows one session's paths relative to the bridge's directory.
struct PathView<'a> {
    directory: &'a Path,
    root: &'a Path,
}

impl PathView<'_> {
    fn show(&self, path: &Path) -> String {
        let path = self.root.join(path);
        path.strip_prefix(self.directory)
            .map(Path::to_path_buf)
            .or_else(|_| {
                session::canonical(&path)
                    .strip_prefix(session::canonical(self.directory))
                    .map(Path::to_path_buf)
            })
            .ok()
            .filter(|relative| !relative.as_os_str().is_empty())
            .unwrap_or(path)
            .display()
            .to_string()
    }
}

/// Answers MCP messages, routing each tool call to one of the user's
/// running analyzers.
pub struct Bridge {
    /// Where relative paths are resolved, like the agent's own working directory.
    directory: PathBuf,
    sessions: Mutex<HashMap<PathBuf, Session>>,
    last_used: Mutex<Option<PathBuf>>,
}

impl Bridge {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            sessions: Mutex::new(HashMap::new()),
            last_used: Mutex::new(None),
        }
    }

    fn absolute(&self, path: &str) -> PathBuf {
        let path = Path::new(path);
        crate::agent::normalize(&if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.directory.join(path)
        })
    }

    fn view<'a>(&'a self, session: &'a Session) -> PathView<'a> {
        PathView {
            directory: &self.directory,
            root: &session.root,
        }
    }

    async fn connect(&self, record: &Path, info: &SessionInfo) -> io::Result<Session> {
        if let Some(session) = self.sessions.lock().await.get(record) {
            return Ok(session.clone());
        }
        let session = Session {
            record: record.to_path_buf(),
            root: info.root.clone(),
            client: connect_agent(info).await?,
        };
        self.sessions
            .lock()
            .await
            .insert(record.to_path_buf(), session.clone());
        Ok(session)
    }

    async fn first_reachable(&self, candidates: Vec<(PathBuf, SessionInfo)>) -> Option<Session> {
        for (record, info) in candidates {
            match self.connect(&record, &info).await {
                Ok(session) => return Some(session),
                Err(error) => forget_if_gone(&record, &error),
            }
        }
        None
    }

    /// Every session that accepts a connection, ordered by workspace.
    async fn running(&self) -> Vec<Session> {
        let mut running = Vec::new();
        for (record, info) in session::list() {
            match self.connect(&record, &info).await {
                Ok(session) => running.push(session),
                Err(error) => forget_if_gone(&record, &error),
            }
        }
        running.sort_by(|a, b| a.root.cmp(&b.root));
        running
    }

    fn workspaces(&self, sessions: &[Session]) -> String {
        sessions
            .iter()
            .map(|session| {
                let view = PathView {
                    directory: &self.directory,
                    root: Path::new(""),
                };
                format!("  {}", view.show(&session.root))
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    async fn session(&self, target: Target<'_>) -> Result<Session, String> {
        let path = match target {
            Target::Containing(path) => path,
            Target::Default => {
                let last = self.last_used.lock().await.clone();
                if let Some(record) = last
                    && let Some(session) = self.sessions.lock().await.get(&record).cloned()
                {
                    return Ok(session);
                }
                if let Some(session) = self
                    .first_reachable(session::discover(&self.directory))
                    .await
                {
                    return Ok(session);
                }
                let mut running = self.running().await;
                return match running.len() {
                    0 => Err(no_session_message(&self.directory)),
                    1 => Ok(running.remove(0)),
                    _ => Err(format!(
                        "Several Argon sessions are running. Pass `workspace` with one of these, or name a file in it:\n{}",
                        self.workspaces(&running)
                    )),
                };
            }
        };
        if let Some(session) = self.first_reachable(session::discover(path)).await {
            return Ok(session);
        }
        let running = self.running().await;
        Err(if running.is_empty() {
            no_session_message(path)
        } else {
            format!(
                "No running Argon session contains {}. Ask the user to open its library with `argone`. Running sessions:\n{}",
                path.display(),
                self.workspaces(&running)
            )
        })
    }

    /// Runs `call` against the session for `target`, choosing again once if
    /// that analyzer went away.
    async fn call<T, F, Fut>(&self, target: Target<'_>, call: F) -> Result<(Session, T), String>
    where
        F: Fn(AgentClient) -> Fut,
        Fut: Future<Output = Result<T, tarpc::client::RpcError>>,
    {
        let mut retried = false;
        loop {
            let session = self.session(target).await?;
            match call(session.client.clone()).await {
                Ok(value) => {
                    *self.last_used.lock().await = Some(session.record.clone());
                    return Ok((session, value));
                }
                Err(error) if is_disconnected(&error) && !retried => {
                    self.sessions.lock().await.remove(&session.record);
                    retried = true;
                }
                Err(error) => return Err(error.to_string()),
            }
        }
    }

    fn workspace_target<'a>(&self, workspace: &'a Option<PathBuf>) -> Target<'a> {
        workspace
            .as_deref()
            .map_or(Target::Default, Target::Containing)
    }

    fn workspace_argument(&self, arguments: &Value) -> Option<PathBuf> {
        arguments
            .get("workspace")
            .and_then(Value::as_str)
            .map(|workspace| self.absolute(workspace))
    }

    /// The response to one JSON-RPC message, or `None` for a notification.
    pub async fn handle(&self, message: Value) -> Option<Value> {
        let id = message.get("id").cloned()?;
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        let params = message.get("params").cloned().unwrap_or(Value::Null);
        let result = match method {
            "initialize" => Ok(initialize_result(&params)),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => Ok(self.call_tool(&params).await),
            _ => Err((-32601, format!("method not found: {method}"))),
        };
        Some(match result {
            Ok(result) => json!({ "jsonrpc": "2.0", "id": id, "result": result }),
            Err((code, message)) => json!({
                "jsonrpc": "2.0",
                "id": id,
                "error": { "code": code, "message": message },
            }),
        })
    }

    async fn call_tool(&self, params: &Value) -> Value {
        let name = params.get("name").and_then(Value::as_str).unwrap_or("");
        let arguments = params
            .get("arguments")
            .cloned()
            .unwrap_or_else(|| json!({}));
        let outcome = match self.run_tool(name, &arguments).await {
            Ok(text) => (text, false),
            Err(text) => (text, true),
        };
        json!({
            "content": [{ "type": "text", "text": outcome.0 }],
            "isError": outcome.1,
        })
    }

    async fn run_tool(&self, name: &str, arguments: &Value) -> Result<String, String> {
        let workspace = self.workspace_argument(arguments);
        let target = self.workspace_target(&workspace);
        match name {
            "status" => {
                let running = self.running().await;
                let result = self
                    .call(target, |client| async move {
                        client.status(call_context()).await
                    })
                    .await;
                let (session, status) = match result {
                    Ok(chosen) => chosen,
                    // Listing the sessions is how the agent learns to choose.
                    Err(_) if workspace.is_none() && running.len() > 1 => {
                        return Ok(format!(
                            "Running Argon sessions (pass `workspace` to choose one):\n{}",
                            self.workspaces(&running)
                        ));
                    }
                    Err(error) => return Err(error),
                };
                let mut text = format_status(&status, &self.view(&session));
                let others = running
                    .into_iter()
                    .filter(|other| other.record != session.record)
                    .collect::<Vec<_>>();
                if !others.is_empty() {
                    text.push_str(&format!(
                        "\nOther running sessions (pass `workspace` to use one):\n{}",
                        self.workspaces(&others)
                    ));
                }
                Ok(text)
            }
            "read_file" => {
                let path = self.absolute(string_argument(arguments, "path")?);
                let offset = arguments.get("offset").and_then(Value::as_u64);
                let limit = arguments.get("limit").and_then(Value::as_u64);
                let (session, file) = self
                    .call(Target::Containing(&path), |client| {
                        let path = path.clone();
                        async move { client.read_file(call_context(), path).await }
                    })
                    .await?;
                let file = file?;
                Ok(format!(
                    "{} ({})\n{}",
                    self.view(&session).show(&file.path),
                    if file.from_editor {
                        "from the Neovim buffer, which may be unsaved"
                    } else {
                        "from disk; not open in Neovim"
                    },
                    numbered_lines(&file.contents, offset, limit)
                ))
            }
            "edit_file" => {
                let request = EditRequest {
                    path: self.absolute(string_argument(arguments, "path")?),
                    edits: replacements(arguments)?,
                    label: label(arguments, "Agent edit"),
                };
                let (session, report) = self
                    .call(Target::Containing(&request.path), |client| {
                        let request = request.clone();
                        async move { client.edit_file(call_context(), request).await }
                    })
                    .await?;
                let view = self.view(&session);
                Ok(format!(
                    "Applied the edit to the Neovim buffer for {}; it is unsaved.\n{}",
                    view.show(&request.path),
                    format_report(&report?, &view)
                ))
            }
            "create_file" => {
                let request = CreateRequest {
                    path: self.absolute(string_argument(arguments, "path")?),
                    contents: string_argument(arguments, "contents")?.to_owned(),
                    label: label(arguments, "Agent created a file"),
                };
                let (session, report) = self
                    .call(Target::Containing(&request.path), |client| {
                        let request = request.clone();
                        async move { client.create_file(call_context(), request).await }
                    })
                    .await?;
                let view = self.view(&session);
                Ok(format!(
                    "Created {} as an unsaved Neovim buffer.\n{}",
                    view.show(&request.path),
                    format_report(&report?, &view)
                ))
            }
            "diagnostics" => {
                let (session, report) = self
                    .call(target, |client| async move {
                        client.diagnostics(call_context()).await
                    })
                    .await?;
                Ok(format_report(&report, &self.view(&session)))
            }
            "compile_cell" => {
                let cell = string_argument(arguments, "cell")?.to_owned();
                let (session, report) = self
                    .call(target, |client| {
                        let cell = cell.clone();
                        async move { client.compile_cell(call_context(), cell).await }
                    })
                    .await?;
                Ok(format_report(&report?, &self.view(&session)))
            }
            "open_cell" => {
                let cell = string_argument(arguments, "cell")?.to_owned();
                let (session, report) = self
                    .call(target, |client| {
                        let cell = cell.clone();
                        async move { client.open_cell(call_context(), cell).await }
                    })
                    .await?;
                Ok(format!(
                    "Opened {cell} in the GUI.\n{}",
                    format_report(&report?, &self.view(&session))
                ))
            }
            _ => Err(format!("Unknown tool `{name}`.")),
        }
    }
}

fn initialize_result(params: &Value) -> Value {
    // The tools use only features every protocol revision shares, so the
    // client's preferred revision is accepted as is.
    let version = params
        .get("protocolVersion")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_PROTOCOL_VERSION);
    json!({
        "protocolVersion": version,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": "argon", "version": env!("CARGO_PKG_VERSION") },
        "instructions": INSTRUCTIONS,
    })
}

fn string_argument<'a>(arguments: &'a Value, name: &str) -> Result<&'a str, String> {
    arguments
        .get(name)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("Missing string argument `{name}`."))
}

fn label(arguments: &Value, default: &str) -> String {
    arguments
        .get("label")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|label| !label.is_empty())
        .unwrap_or(default)
        .to_owned()
}

fn replacements(arguments: &Value) -> Result<Vec<Replacement>, String> {
    let edits = arguments
        .get("edits")
        .and_then(Value::as_array)
        .ok_or_else(|| "Missing array argument `edits`.".to_owned())?;
    edits
        .iter()
        .enumerate()
        .map(|(index, edit)| {
            let field = |name| {
                edit.get(name)
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .ok_or_else(|| format!("Edit {}: missing string `{name}`.", index + 1))
            };
            Ok(Replacement {
                old: field("old_string")?,
                new: field("new_string")?,
                replace_all: edit
                    .get("replace_all")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            })
        })
        .collect()
}

/// Lines prefixed with their one-based number and a tab.
fn numbered_lines(contents: &str, offset: Option<u64>, limit: Option<u64>) -> String {
    let first = offset.unwrap_or(1).max(1) as usize;
    let count = limit.map_or(usize::MAX, |limit| limit as usize);
    contents
        .lines()
        .enumerate()
        .skip(first - 1)
        .take(count)
        .map(|(index, line)| format!("{:>6}\t{line}", index + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

fn format_status(status: &AgentStatus, view: &PathView) -> String {
    let mut lines = vec![
        format!(
            "Workspace: {}",
            status
                .root
                .as_ref()
                .map_or_else(|| "(not open yet)".to_owned(), |root| view.show(root))
        ),
        format!(
            "Open cell: {}",
            status.open_cell.as_deref().unwrap_or("(none)")
        ),
        format!(
            "GUI: {}",
            if status.gui_connected {
                "connected"
            } else {
                "not running"
            }
        ),
        format!(
            "Follow mode: {}",
            if status.follow_mode {
                "on (open_cell may switch the GUI's cell)"
            } else {
                "off (open_cell is unavailable; use compile_cell)"
            }
        ),
        format!(
            "Approval: {}",
            match status.approval {
                crate::ApprovalMode::Never => "edits apply immediately",
                crate::ApprovalMode::Always => "the user confirms each edit in Neovim",
            }
        ),
        format!(
            "Unsaved changes in Neovim: {}",
            if status.workspace_modified {
                "yes"
            } else {
                "no"
            }
        ),
        format!(
            "Files changed on disk: {}",
            if status.watching_files {
                "reloaded automatically"
            } else {
                "not watched"
            }
        ),
    ];
    if !status.open_files.is_empty() {
        lines.push("Open in Neovim:".to_owned());
        lines.extend(
            status
                .open_files
                .iter()
                .map(|path| format!("  {}", view.show(path))),
        );
    }
    lines.join("\n")
}

fn format_report(report: &Report, view: &PathView) -> String {
    let mut lines = Vec::new();
    if report.compiled_revision < report.revision {
        lines.push(format!(
            "Compilation of revision {} has not finished; the results below are from revision {}.",
            report.revision, report.compiled_revision
        ));
    }
    match &report.summary {
        Some(summary) => {
            let cell = summary.cell.as_deref().unwrap_or("(no cell open)");
            let status = match summary.status {
                CompileStatus::Valid => "compiled",
                CompileStatus::ExecErrors => "compiled with execution errors",
                CompileStatus::StaticErrors => "has static errors",
                CompileStatus::ParseErrors => "has parse errors",
            };
            lines.push(format!("Cell {cell} {status}."));
            if matches!(
                summary.status,
                CompileStatus::Valid | CompileStatus::ExecErrors
            ) {
                lines.push(format!(
                    "Top cell: {} rects, {} polygons, {} paths, {} instances; {} cells in the hierarchy.",
                    summary.rects, summary.polygons, summary.paths, summary.instances, summary.cells
                ));
                if let Some([x0, y0, x1, y1]) = summary.bbox {
                    lines.push(format!("Bounding box: ({x0}, {y0}) to ({x1}, {y1})."));
                }
                if summary.unsolved_vars > 0 {
                    lines.push(format!(
                        "{} coordinates are set only by initial conditions.",
                        summary.unsolved_vars
                    ));
                }
            }
        }
        None => lines.push(
            "No cell is open in the GUI, so only the workspace was checked. Use compile_cell to compile one."
                .to_owned(),
        ),
    }
    if report.diagnostics.is_empty() {
        lines.push("No diagnostics.".to_owned());
    } else {
        lines.push("Diagnostics:".to_owned());
        lines.extend(report.diagnostics.iter().map(|diagnostic| {
            format!(
                "  {}:{}:{}: {}",
                view.show(&diagnostic.path),
                diagnostic.line,
                diagnostic.column,
                diagnostic.message
            )
        }));
    }
    if !report.messages.is_empty() {
        lines.push("Messages:".to_owned());
        lines.extend(report.messages.iter().map(|message| format!("  {message}")));
    }
    lines.join("\n")
}

fn tool_definitions() -> Value {
    let path = json!({
        "type": "string",
        "description": "File path, absolute or relative to your working directory. The Argon session whose workspace contains the file is used."
    });
    let workspace = json!({
        "type": "string",
        "description": "Directory of the Argon library to use, absolute or relative to your working directory. Needed only when several sessions are running; defaults to the one used last."
    });
    let label = json!({
        "type": "string",
        "description": "A few words describing the change, shown to the user while it is applied."
    });
    let cell = json!({
        "type": "string",
        "description": "A cell invocation, such as `top()` or `inverter(2, nf=4)`."
    });
    json!([
        {
            "name": "status",
            "description": "List the running Argon sessions and describe one: its workspace, the cell open in the GUI, follow mode, the approval mode, and the files open in Neovim.",
            "inputSchema": { "type": "object", "properties": { "workspace": workspace } }
        },
        {
            "name": "read_file",
            "description": "Read a workspace file as the user currently has it, including unsaved Neovim edits. Use this instead of reading .ar files from disk, which can be out of date. Each line is prefixed with its number and a tab; never include that prefix in edit_file's old_string.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": path,
                    "offset": { "type": "integer", "description": "First line to show, one-based." },
                    "limit": { "type": "integer", "description": "Most lines to show." }
                },
                "required": ["path"]
            }
        },
        {
            "name": "edit_file",
            "description": "Edit an Argon source file through the user's Neovim buffer, so the change shows up live in Neovim and the GUI and can be undone. Each old_string must match the current text exactly and be unique unless replace_all is set. All edits in one call are matched against the file before any is applied, must not overlap, and land as one undo step. The buffer is left unsaved. The result reports the recompiled cell's shape and any diagnostics. The user may be asked to approve the edit, and may reject it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": path,
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": { "type": "string", "description": "Exact text to replace." },
                                "new_string": { "type": "string", "description": "Replacement text." },
                                "replace_all": { "type": "boolean", "description": "Replace every occurrence. Defaults to false." }
                            },
                            "required": ["old_string", "new_string"]
                        }
                    },
                    "label": label
                },
                "required": ["path", "edits", "label"]
            }
        },
        {
            "name": "create_file",
            "description": "Create a new Argon source file as an unsaved Neovim buffer. A module must also be declared from its parent, for example `mod utils;` in lib.ar loads utils.ar. The directory must already exist.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "path": path,
                    "contents": { "type": "string" },
                    "label": label
                },
                "required": ["path", "contents", "label"]
            }
        },
        {
            "name": "diagnostics",
            "description": "List the current errors across the workspace and summarize the cell open in the GUI.",
            "inputSchema": { "type": "object", "properties": { "workspace": workspace } }
        },
        {
            "name": "compile_cell",
            "description": "Compile a cell against the current source, unsaved edits included, without changing what the GUI shows. Reports errors and the cell's shape and bounding box. Use this instead of `arc run`.",
            "inputSchema": {
                "type": "object",
                "properties": { "cell": cell, "workspace": workspace },
                "required": ["cell"]
            }
        },
        {
            "name": "open_cell",
            "description": "Show a cell in the user's GUI. Works only while the user has follow mode on (`:Argon follow` in Neovim); otherwise use compile_cell.",
            "inputSchema": {
                "type": "object",
                "properties": { "cell": cell, "workspace": workspace },
                "required": ["cell"]
            }
        }
    ])
}

fn parse_error(error: serde_json::Error) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": Value::Null,
        "error": { "code": -32700, "message": format!("parse error: {error}") },
    })
}

/// Serves MCP on stdin and stdout until stdin closes.
pub async fn run(directory: Option<PathBuf>) -> io::Result<()> {
    let directory = match directory {
        Some(directory) => directory,
        None => std::env::current_dir()?,
    };
    let bridge = Arc::new(Bridge::new(directory));
    let (responses, mut outgoing) = mpsc::unbounded_channel::<Value>();
    let writer = tokio::spawn(async move {
        let mut stdout = tokio::io::stdout();
        while let Some(response) = outgoing.recv().await {
            let mut line = response.to_string();
            line.push('\n');
            stdout.write_all(line.as_bytes()).await?;
            stdout.flush().await?;
        }
        io::Result::Ok(())
    });

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let message = match serde_json::from_str::<Value>(&line) {
            Ok(message) => message,
            Err(error) => {
                let _ = responses.send(parse_error(error));
                continue;
            }
        };
        // Requests run concurrently, so a call waiting on the user's
        // approval does not hold up the others.
        let bridge = bridge.clone();
        let responses = responses.clone();
        tokio::spawn(async move {
            if let Some(response) = bridge.handle(message).await {
                let _ = responses.send(response);
            }
        });
    }
    drop(responses);
    writer.await.map_err(io::Error::other)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::{AgentDiagnostic, CompileSummary};

    fn bridge(directory: &Path) -> Bridge {
        Bridge::new(directory.to_path_buf())
    }

    #[tokio::test]
    async fn answers_the_handshake_and_lists_tools() {
        let directory = tempfile::tempdir().unwrap();
        let bridge = bridge(directory.path());
        let response = bridge
            .handle(json!({
                "jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": { "protocolVersion": "2025-03-26", "capabilities": {} }
            }))
            .await
            .unwrap();
        assert_eq!(response["result"]["protocolVersion"], "2025-03-26");
        assert!(response["result"]["capabilities"]["tools"].is_object());

        assert!(
            bridge
                .handle(json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }))
                .await
                .is_none()
        );

        let tools = bridge
            .handle(json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list" }))
            .await
            .unwrap();
        let names = tools["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|tool| tool["name"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "status",
                "read_file",
                "edit_file",
                "create_file",
                "diagnostics",
                "compile_cell",
                "open_cell"
            ]
        );

        let unknown = bridge
            .handle(json!({ "jsonrpc": "2.0", "id": 3, "method": "resources/list" }))
            .await
            .unwrap();
        assert_eq!(unknown["error"]["code"], -32601);
    }

    #[tokio::test]
    async fn tool_calls_report_errors_as_tool_results() {
        let directory = tempfile::tempdir().unwrap();
        let bridge = bridge(directory.path());
        let missing = bridge
            .handle(json!({
                "jsonrpc": "2.0", "id": 1, "method": "tools/call",
                "params": { "name": "edit_file", "arguments": { "path": "lib.ar" } }
            }))
            .await
            .unwrap();
        assert_eq!(missing["result"]["isError"], true);
        assert!(
            missing["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("edits")
        );
    }

    #[test]
    fn numbers_lines_within_the_requested_window() {
        let text = "a\nb\nc\n";
        assert_eq!(
            numbered_lines(text, None, None),
            "     1\ta\n     2\tb\n     3\tc"
        );
        assert_eq!(numbered_lines(text, Some(2), Some(1)), "     2\tb");
    }

    #[test]
    fn reports_say_when_compilation_lags_behind() {
        let report = Report {
            revision: 5,
            compiled_revision: 4,
            summary: Some(CompileSummary {
                cell: Some("top()".to_owned()),
                status: CompileStatus::Valid,
                cells: 2,
                rects: 3,
                polygons: 0,
                paths: 0,
                instances: 1,
                unsolved_vars: 0,
                bbox: Some([0., 0., 10., 5.]),
            }),
            diagnostics: vec![AgentDiagnostic {
                path: PathBuf::from("lib.ar"),
                line: 3,
                column: 5,
                message: "undeclared variable".to_owned(),
            }],
            messages: Vec::new(),
        };
        let view = PathView {
            directory: Path::new("/work"),
            root: Path::new("/work/lib"),
        };
        let text = format_report(&report, &view);
        assert!(text.contains("revision 5 has not finished"));
        assert!(text.contains("Cell top() compiled."));
        assert!(text.contains("3 rects"));
        assert!(text.contains("Bounding box: (0, 0) to (10, 5)."));
        assert!(text.contains("  lib/lib.ar:3:5: undeclared variable"));
    }
}
