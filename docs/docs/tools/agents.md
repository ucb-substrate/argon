---
title: Agents
description: Let a coding agent edit Argon source through your editor while you watch.
sidebar_label: Agents
---

# Agents

A coding agent can edit an Argon project while you have it open. Its edits go into your Neovim buffers or VS Code documents, the same way GUI edits do, so you watch each one land in the editor and in the GUI. Edits stay unsaved until you save them, and each one can be undone.

Agents connect through `argon-analyzer mcp`, a [Model Context Protocol](https://modelcontextprotocol.io) server. It finds the Argon sessions you have running when the agent calls a tool, so you can start, stop, and switch Argon projects without restarting the agent. An agent working without an editor, such as one in a sandbox, uses the server's [headless mode](#headless-mode) instead.

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

Relative paths are resolved against the agent's working directory. To use a different directory, pass `--root <DIR>`.

## Choosing a session

Each Argon library you have open, for example with `argone`, is its own session, and one server works with all of them:

- File tools (`read_file`, `edit_file`, `create_file`) use the session whose workspace contains the file.
- The other tools take an optional `workspace` argument naming a library directory.
- Without one, they use the session used last, then a session whose workspace contains the agent's working directory, then the only session running.

`status` lists the running sessions, so an agent at the top of a repository can find the library you're working on.

## Tools

| Tool | Description |
| --- | --- |
| `status` | The running sessions, and for one of them: its workspace, the cell open in the GUI, follow mode, the approval mode, and the files open in the editor. |
| `read_file` | A file as you currently have it, unsaved edits included. |
| `edit_file` | Find-and-replace edits applied to an editor buffer as one undo step. Returns the recompiled cell's shape, bounding box, and diagnostics. |
| `create_file` | A new source file, opened as an unsaved buffer. |
| `diagnostics` | Current errors across the workspace. |
| `compile_cell` | Compiles a cell against the current source without changing what the GUI shows. |
| `open_cell` | Shows a cell in the GUI. Available only in follow mode. |

The agent can edit `.ar` files in the workspace and in the libraries it depends on. A file that isn't open is loaded into a hidden buffer, which `:ls` lists in Neovim. The agent never saves.

## Watching edits

Each agent edit briefly highlights the text it wrote with the `ArgonAgentEdit` group (linked to `IncSearch` by default). The GUI outlines the objects the edit changed, and its status bar shows what the agent is doing. LSP progress plugins such as Fidget show it in Neovim. In [VS Code](./vscode#agent-edits), the edited text is highlighted the same way.

`:Argon follow` makes the current window follow agent edits: it switches to each edited file and scrolls to the change, unless you are typing in it. `:Argon follow off`, or closing the window, turns it off. In VS Code, use **Argon: Follow Agent Edits**. The agent can switch the GUI's cell with `open_cell` only while follow mode is on.

## Approving edits

By default edits apply immediately. To review each one first, set the approval mode:

```toml
[agent]
approval = "always"
```

The editor then shows each edit as a diff and asks before applying it. A rejected edit is reported to the agent and isn't retried. With `approval = "never"`, the default, the editor is never asked. Change it for the current session with `:Argon set agent.approval always`.

## Redirecting file tools

An agent can still read and write `.ar` files with its own tools, bypassing your buffers. If a file changes on disk, the editor reloads it when its buffer has no unsaved changes, and the analyzer warns otherwise.

For Claude Code, `argon-analyzer hook` is a `PreToolUse` hook that steers those tools to the MCP server while a session is running. It blocks edits to `.ar` files, and blocks reads when the editor has unsaved changes the file on disk lacks. Add it to `.claude/settings.json`:

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

## Headless mode

`argon-analyzer mcp --headless` needs no editor or running session. Its tools compile the library on disk with the same compiler as `arc run`, so the agent edits files with its own tools and calls them after each edit:

```json
{
  "mcpServers": {
    "argon": {
      "type": "stdio",
      "command": "argon-analyzer",
      "args": ["mcp", "--headless"]
    }
  }
}
```

Each call uses the library whose `Argon.toml` is in the agent's working directory or above it, or the one an optional `workspace` argument names. Other paths, in arguments and in results, are relative to the library's directory. Coordinates are in the technology's units, nm for SKY130.

| Tool | Arguments | Description |
| --- | --- | --- |
| `compile` | `cells`, `sweep`, `export`, `layers`, `verbose` | Compiles cell invocations in parallel and reports their sizes, bounding boxes, and errors: one cell in full, and several as a [summary](#sweeps). `sweep` adds a grid of invocations of one cell. With `export`, writes each cell that compiles to `<export>/<name>/argon.gds` and `argon.spice`, exactly as `arc run --gds --spice` writes them, where `<name>` is the invocation's tokens joined by `-`; a cell that fails removes its old files there. With `layers`, adds each layer's drawn area. `verbose` or `layers` lists every cell in full. When the cell or its subcells have [`net=` tags](/language/std-layout#rect) or pin labels, also checks them against the layout's connectivity and lists each short, open, and label with no pin or conductor under it, as warnings. With no cells, only parses and type-checks the library. |
| `inspect` | `cell`, `exprs` | Evaluates expressions at the end of the cell's body after it is solved, where its parameters and top-level `let`s are in scope, such as `out.x1 - out.x0`. |
| `nets` | `cell`, `nets`, `pairs` | Reports each net of the cell's layout: its wiring's length and area on each layer, its cuts, and a rough capacitance to the substrate and to each net it overlaps on another layer, from the technology file's [capacitance coefficients](/language/technology#capacitance). Nets are the conductors the technology's vias join, named by `net=` tags and pin labels; unnamed nets are listed largest first. A shape at least half the cell's longer side and 20 times as long as it is wide is listed as long. `nets` limits the report to those names, and `pairs`, such as `[["outp", "outn"]]`, compares nets that should match, flagging differences over 5%. Lateral coupling between neighbors on one layer isn't counted. |
| `locate` | `cell`, `boxes`, `unit` | Finds the shapes that overlap or touch each box, such as a DRC violation, with the source line that drew each one, the call in its cell's body when that line is in a function, and the instances leading to it. A box takes optional `layers`, a `gds_cell` whose coordinates it is in (the cell a DRC report names for the violation), and a `label`. `unit` is `um`, the default, or `nm`. |

The [quick reference](/language/reference) lists the language's rules and every function on one page, and [Writing a layout generator](/guides/layout-generator) builds and checks a generator with these tools.

An error names the line that failed and, when that line is in a function or another cell, each call in the top cell's body that led to it:

```
inv(1001., 1600., 4): error
  /path/to/pdks/sky130/lib.ar:12:16: solved value 1261 is off the 5 grid (nearest grid point is 1260)
    called from lib.ar:5:38
```

### Sweeps

`sweep` compiles every combination of one cell's parameter values, the last parameter varying fastest. `params` is a list of value lists, one per parameter from the first, or an object of value lists by name, which must give every parameter without a default. A number is written as its parameter's declared type needs, so `1000` passes `1000.` to a `Float`. A string is Argon source, such as `"Mode::Fast"`, except for a `String` parameter, which receives it as text.

```json
{"sweep": {"cell": "inv", "params": [[1000, 1001, 2000], [1600], [2, 4]]}, "export": "target/sweep"}
```

Several cells are summarized: one line for those that compile, with the ranges of their widths and heights and the smallest and largest by area; in full, any that compile with coordinates set only by initial values or whose export failed; and each distinct error once, with the cells it occurs in:

```
4 of 6 cells compile: 1.77–2.63 × 6.72–7.72 µm; 11.89 µm² at inv(1000., 1600., 2) to 20.30 µm² at inv(2000., 1600., 4); each exported to target/sweep/<name>/.
2 cells fail: inv(1001., 1600., 2) and inv(1001., 1600., 4)
/path/to/pdks/sky130/lib.ar:12:16: solved value 1261 is off the 5 grid (nearest grid point is 1260)
  called from lib.ar:5:38
  in all 2 failing cells
```

### Connectivity

A connectivity problem names the shapes on each side, and for a short, the chain of touching shapes that joins the two names. Here both of the [latch](/guides/layout-generator)'s output wires run at one height:

```
latch(1000., 2, 2000., 2): 3.41 × 4.99 µm = 17.02 µm², (-1705, -545) to (1705, 4445); 13 shapes, 2 instances in the top cell
  nets: 1 connectivity problem (warnings; LVS would fail):
    short in latch: `outp`, `outn` are one net
      outp: met1.drawing (-575, 1000) to (-285, 1900): lib.ar:142:5, called from lib.ar:45:5
      outn: met1.drawing (285, 1000) to (575, 1900): lib.ar:142:5, called from lib.ar:46:5
      joined by, from `outp` to `outn`:
        met1.drawing (-575, 1000) to (-285, 1900): lib.ar:67:19 in halves[1]
        via.drawing (-505, 1085) to (-355, 1235): <argon-std>/layout.ar:20:30, called from lib.ar:42:5
        met2.drawing (-930, 1090) to (500, 1230): lib.ar:117:5, called from lib.ar:41:5
        via.drawing (355, 1085) to (505, 1235): <argon-std>/layout.ar:20:30, called from lib.ar:40:5
        met1.drawing (285, 1000) to (575, 1900): lib.ar:67:19 in halves[0]
```

In a summary of several cells, a cell with connectivity problems is listed in full.

## Security

The analyzer listens on a local port, which the GUI and the MCP server connect to. Every connection must present a token that the analyzer generates at startup, so other users on the same machine can't connect to it. The token reaches the GUI over a private socket that `argone` creates, through its environment for `argone gui`, or over SSH for `argone ssh`. The MCP server reads it from a session record in `$XDG_STATE_HOME/argon/sessions` (`~/.local/state/argon/sessions` by default), which only you can read.
