//! A Model Context Protocol server that lets agents work through a running
//! analyzer, so their edits land in the user's Neovim buffers.
//!
//! It speaks newline-delimited JSON-RPC on stdin and stdout and forwards tool
//! calls to the analyzer whose workspace contains its working directory.

use std::{
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
    session, transport,
};

/// Used when a client asks for no particular protocol version.
const DEFAULT_PROTOCOL_VERSION: &str = "2025-06-18";

const INSTRUCTIONS: &str = "This server edits Argon layout sources through the user's running \
Argon session. Edits go into the user's Neovim buffers, so the user watches them land in Neovim \
and the layout GUI and can undo them. Buffers may hold unsaved changes, so read .ar files with \
read_file rather than from disk, edit them with edit_file or create_file rather than writing \
files, and check cells with compile_cell rather than `arc run`. Never save files; the user does.";

pub(crate) fn no_session_message(directory: &Path) -> String {
    format!(
        "No running Argon session was found for {}. Ask the user to open the project with `argone` (or open its lib.ar in Neovim with the Argon plugin), then try again.",
        directory.display()
    )
}

/// Connects to the innermost running analyzer whose workspace contains
/// `directory`, discarding records of analyzers that are gone.
pub(crate) async fn connect_session(directory: &Path) -> Result<AgentClient, String> {
    let mut last_error = None;
    for (record, info) in session::discover(directory) {
        let Some(token) = info.token() else {
            continue;
        };
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, info.port));
        match transport::connect(addr, &token, transport::Role::Agent).await {
            Ok(stream) => {
                let transport = tarpc::serde_transport::new(stream, Bincode::default());
                return Ok(AgentClient::new(tarpc::client::Config::default(), transport).spawn());
            }
            Err(error) => {
                if error.kind() == io::ErrorKind::ConnectionRefused {
                    let _ = std::fs::remove_file(record);
                }
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

/// Answers MCP messages for the session whose workspace contains `directory`.
pub struct Bridge {
    directory: PathBuf,
    client: Mutex<Option<AgentClient>>,
}

impl Bridge {
    pub fn new(directory: PathBuf) -> Self {
        Self {
            directory,
            client: Mutex::new(None),
        }
    }

    async fn client(&self) -> Result<AgentClient, String> {
        let mut client = self.client.lock().await;
        if let Some(client) = client.as_ref() {
            return Ok(client.clone());
        }
        let connected = connect_session(&self.directory).await?;
        *client = Some(connected.clone());
        Ok(connected)
    }

    /// Runs `call`, reconnecting once if the analyzer went away.
    async fn call<T, F, Fut>(&self, call: F) -> Result<T, String>
    where
        F: Fn(AgentClient) -> Fut,
        Fut: Future<Output = Result<T, tarpc::client::RpcError>>,
    {
        let client = self.client().await?;
        match call(client).await {
            Ok(value) => Ok(value),
            Err(error) if is_disconnected(&error) => {
                *self.client.lock().await = None;
                let client = self.client().await?;
                call(client).await.map_err(|error| error.to_string())
            }
            Err(error) => Err(error.to_string()),
        }
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
        match name {
            "status" => {
                let status = self
                    .call(|client| async move { client.status(call_context()).await })
                    .await?;
                Ok(format_status(&status))
            }
            "read_file" => {
                let path = PathBuf::from(string_argument(arguments, "path")?);
                let offset = arguments.get("offset").and_then(Value::as_u64);
                let limit = arguments.get("limit").and_then(Value::as_u64);
                let file = self
                    .call(|client| {
                        let path = path.clone();
                        async move { client.read_file(call_context(), path).await }
                    })
                    .await??;
                Ok(format!(
                    "{} ({})\n{}",
                    file.path.display(),
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
                    path: PathBuf::from(string_argument(arguments, "path")?),
                    edits: replacements(arguments)?,
                    label: label(arguments, "Agent edit"),
                };
                let report = self
                    .call(|client| {
                        let request = request.clone();
                        async move { client.edit_file(call_context(), request).await }
                    })
                    .await??;
                Ok(format!(
                    "Applied the edit to the Neovim buffer for {}; it is unsaved.\n{}",
                    request.path.display(),
                    format_report(&report)
                ))
            }
            "create_file" => {
                let request = CreateRequest {
                    path: PathBuf::from(string_argument(arguments, "path")?),
                    contents: string_argument(arguments, "contents")?.to_owned(),
                    label: label(arguments, "Agent created a file"),
                };
                let report = self
                    .call(|client| {
                        let request = request.clone();
                        async move { client.create_file(call_context(), request).await }
                    })
                    .await??;
                Ok(format!(
                    "Created {} as an unsaved Neovim buffer.\n{}",
                    request.path.display(),
                    format_report(&report)
                ))
            }
            "diagnostics" => {
                let report = self
                    .call(|client| async move { client.diagnostics(call_context()).await })
                    .await?;
                Ok(format_report(&report))
            }
            "compile_cell" => {
                let cell = string_argument(arguments, "cell")?.to_owned();
                let report = self
                    .call(|client| {
                        let cell = cell.clone();
                        async move { client.compile_cell(call_context(), cell).await }
                    })
                    .await??;
                Ok(format_report(&report))
            }
            "open_cell" => {
                let cell = string_argument(arguments, "cell")?.to_owned();
                let report = self
                    .call(|client| {
                        let cell = cell.clone();
                        async move { client.open_cell(call_context(), cell).await }
                    })
                    .await??;
                Ok(format!(
                    "Opened {cell} in the GUI.\n{}",
                    format_report(&report)
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

fn format_status(status: &AgentStatus) -> String {
    let mut lines = vec![
        format!(
            "Workspace: {}",
            status.root.as_ref().map_or_else(
                || "(not open yet)".to_owned(),
                |root| root.display().to_string()
            )
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
                .map(|path| format!("  {}", path.display())),
        );
    }
    lines.join("\n")
}

fn format_report(report: &Report) -> String {
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
                diagnostic.path.display(),
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
        "description": "File path, relative to the workspace root or absolute."
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
            "description": "Describe the running Argon session: workspace root, the cell open in the GUI, follow mode, the approval mode, and the files open in Neovim.",
            "inputSchema": { "type": "object", "properties": {} }
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
            "inputSchema": { "type": "object", "properties": {} }
        },
        {
            "name": "compile_cell",
            "description": "Compile a cell against the current source, unsaved edits included, without changing what the GUI shows. Reports errors and the cell's shape and bounding box. Use this instead of `arc run`.",
            "inputSchema": {
                "type": "object",
                "properties": { "cell": cell },
                "required": ["cell"]
            }
        },
        {
            "name": "open_cell",
            "description": "Show a cell in the user's GUI. Works only while the user has follow mode on (`:Argon follow` in Neovim); otherwise use compile_cell.",
            "inputSchema": {
                "type": "object",
                "properties": { "cell": cell },
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
        let text = format_report(&report);
        assert!(text.contains("revision 5 has not finished"));
        assert!(text.contains("Cell top() compiled."));
        assert!(text.contains("3 rects"));
        assert!(text.contains("Bounding box: (0, 0) to (10, 5)."));
        assert!(text.contains("lib.ar:3:5: undeclared variable"));
    }
}
