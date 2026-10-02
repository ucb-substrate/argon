---
title: Agents
description: Let a coding agent edit Argon source through your Neovim buffers while you watch.
sidebar_label: Agents
---

# Agents

A coding agent can edit an Argon project while you have it open. Its edits go into your Neovim buffers, the same way GUI edits do, so you watch each one land in Neovim and in the GUI. Edits stay unsaved until you save them, and each one can be undone with `u`.

Agents connect through `argon-analyzer mcp`, a [Model Context Protocol](https://modelcontextprotocol.io) server. It finds the analyzer that serves the agent's working directory, so start Argon first (for example with `argone`) and then the agent.

## Setup

Register the server with your agent. For Claude Code, run this in the project:

```bash
claude mcp add --scope project argon -- argon-analyzer mcp
```

That writes `.mcp.json`:

```json
{
  "mcpServers": {
    "argon": {
      "type": "stdio",
      "command": "argon-analyzer",
      "args": ["mcp"]
    }
  }
}
```

To search for a session from a directory other than the agent's working directory, pass `--root <DIR>`.

## Tools

| Tool | Description |
| --- | --- |
| `status` | Workspace root, the cell open in the GUI, follow mode, the approval mode, and the files open in Neovim. |
| `read_file` | A file as you currently have it, unsaved edits included. |
| `edit_file` | Find-and-replace edits applied to a Neovim buffer as one undo step. Returns the recompiled cell's shape, bounding box, and diagnostics. |
| `create_file` | A new source file, opened as an unsaved buffer. |
| `diagnostics` | Current errors across the workspace. |
| `compile_cell` | Compiles a cell against the current source without changing what the GUI shows. |
| `open_cell` | Shows a cell in the GUI. Available only in follow mode. |

The agent can edit `.ar` files in the workspace and in the libraries it depends on. A file that isn't open is loaded into a hidden buffer, which `:ls` lists. The agent never saves.

## Watching edits

Each agent edit briefly highlights the text it wrote with the `ArgonAgentEdit` group (linked to `IncSearch` by default). The GUI outlines the objects the edit changed, and its status bar shows what the agent is doing. LSP progress plugins such as Fidget show it in Neovim.

`:Argon follow` makes the current window follow agent edits: it switches to each edited file and scrolls to the change, unless you are typing in it. `:Argon follow off`, or closing the window, turns it off. The agent can switch the GUI's cell with `open_cell` only while follow mode is on.

## Approving edits

By default edits apply immediately. To review each one first, set the approval mode:

```toml
[agent]
approval = "always"
```

Neovim then shows each edit as a diff and asks before applying it. A rejected edit is reported to the agent and isn't retried. With `approval = "never"`, the default, Neovim is never asked. Change it for the current session with `:Argon set agent.approval always`.

## Redirecting file tools

An agent can still read and write `.ar` files with its own tools, bypassing your buffers. If a file changes on disk, the analyzer reloads it into Neovim when its buffer has no unsaved changes, and warns otherwise.

For Claude Code, `argon-analyzer hook` is a `PreToolUse` hook that steers those tools to the MCP server while a session is running. It blocks edits to `.ar` files, and blocks reads when Neovim has unsaved changes the file on disk lacks. Add it to `.claude/settings.json`:

```json
{
  "hooks": {
    "PreToolUse": [
      {
        "matcher": "Read|Edit|Write|MultiEdit",
        "hooks": [{ "type": "command", "command": "argon-analyzer hook" }]
      }
    ]
  }
}
```

## Security

The analyzer listens on a local port, which the GUI and the MCP server connect to. Every connection must present a token that the analyzer generates at startup, so other users on the same machine can't connect to it. The token reaches the GUI through its environment, or over SSH for `argone ssh`. The MCP server reads it from a session record in `$XDG_STATE_HOME/argon/sessions` (`~/.local/state/argon/sessions` by default), which only you can read.
