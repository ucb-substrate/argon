//! A Claude Code `PreToolUse` hook that steers file tools on `.ar` files to
//! the analyzer while an Argon session is running.
//!
//! Edits are always redirected so they land in Neovim. Reads are redirected
//! only when Neovim holds unsaved changes the file on disk lacks.

use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    time::Duration,
};

use serde_json::{Value, json};

use crate::{agent::AgentClient, mcp};

/// How long the hook waits for the analyzer before letting the tool run.
const HOOK_TIMEOUT: Duration = Duration::from_secs(3);

/// The decision for one tool call, or `None` to let it proceed.
async fn decide(
    input: &Value,
    connect: impl AsyncFnOnce(&Path) -> Option<AgentClient>,
) -> Option<String> {
    let tool = input.get("tool_name")?.as_str()?;
    let file = input.get("tool_input")?.get("file_path")?.as_str()?;
    let mut path = PathBuf::from(file);
    if path.extension().is_none_or(|extension| extension != "ar") {
        return None;
    }
    if path.is_relative()
        && let Some(cwd) = input.get("cwd").and_then(Value::as_str)
    {
        path = Path::new(cwd).join(path);
    }
    let directory = path.parent()?.to_path_buf();
    let client = connect(&directory).await?;
    if tool == "Read" {
        let file = client
            .read_file(tarpc::context::current(), path.clone())
            .await
            .ok()?
            .ok()?;
        let disk = std::fs::read_to_string(&path).ok();
        if !file.from_editor || disk.as_deref() == Some(file.contents.as_str()) {
            return None;
        }
        return Some(format!(
            "{} has unsaved changes in the user's Neovim buffer, so the file on disk is out of date. Read it with the argon MCP server's read_file tool (mcp__argon__read_file) instead.",
            path.display()
        ));
    }
    Some(format!(
        "An Argon session is running for {}. Change .ar files with the argon MCP server's edit_file or create_file tools (mcp__argon__edit_file, mcp__argon__create_file) so the change goes through the user's Neovim buffer and shows up live in Neovim and the GUI. Don't write .ar files directly.",
        directory.display()
    ))
}

/// Why a `PreToolUse` call should be redirected, or `None` to let it run.
pub async fn decision(input: &Value) -> Option<String> {
    tokio::time::timeout(
        HOOK_TIMEOUT,
        decide(input, async |directory: &Path| {
            mcp::connect_session(directory).await.ok()
        }),
    )
    .await
    .ok()
    .flatten()
}

fn deny(reason: &str) -> Value {
    json!({
        "hookSpecificOutput": {
            "hookEventName": "PreToolUse",
            "permissionDecision": "deny",
            "permissionDecisionReason": reason,
        }
    })
}

/// Reads the hook input from stdin and prints a decision when the call
/// should be redirected. Any failure lets the tool call proceed.
pub async fn run() -> io::Result<()> {
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    let Ok(input) = serde_json::from_str::<Value>(&input) else {
        return Ok(());
    };
    if let Some(reason) = decision(&input).await {
        println!("{}", deny(&reason));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn no_session(_: &Path) -> Option<AgentClient> {
        None
    }

    #[tokio::test]
    async fn ignores_other_files_and_workspaces_without_a_session() {
        for input in [
            json!({ "tool_name": "Edit", "tool_input": { "file_path": "/w/notes.md" } }),
            json!({ "tool_name": "Bash", "tool_input": { "command": "ls" } }),
            json!({ "tool_name": "Edit", "tool_input": { "file_path": "/w/lib.ar" } }),
        ] {
            assert_eq!(decide(&input, no_session).await, None);
        }
    }

    #[test]
    fn denials_use_the_pre_tool_use_decision_format() {
        let output = deny("use the MCP tool");
        assert_eq!(output["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(output["hookSpecificOutput"]["permissionDecision"], "deny");
        assert_eq!(
            output["hookSpecificOutput"]["permissionDecisionReason"],
            "use the MCP tool"
        );
    }
}
