---
title: VS Code extension
description: The GUI panel, the Argon commands, focus handoff, and agent edits in VS Code.
sidebar_label: VS Code extension
---

# VS Code extension

The VS Code extension starts `argon-analyzer` for each Argon project you open, shows the GUI in a panel beside your source, and adds the **Argon** commands to the Command Palette. A project is the nearest directory above a `.ar` file that contains `lib.ar`.

## Install

Building the extension also builds the GUI as WebAssembly, which needs the `wasm32-unknown-unknown` Rust target and a `wasm-bindgen` CLI matching the `wasm-bindgen` version in `Cargo.lock`:

```bash
rustup target add wasm32-unknown-unknown
cargo install wasm-bindgen-cli --locked --version "$(cargo pkgid wasm-bindgen | sed 's/.*@//')"
cd plugins/vscode
npm ci
npx @vscode/vsce package
code --install-extension argon-0.1.0.vsix
```

The extension runs `argon-analyzer` from your `PATH`. To use another executable, set `argon.analyzer.path`.

## The GUI panel

**Argon: Focus GUI** opens the GUI in a panel beside the source, and opening a cell opens it too. The panel keeps running in the background when you switch tabs, and closing it disconnects the GUI.

To show the GUI in a window of its own instead, as `argone --terminal` does, set `argon.gui.location` to `window` and run **Argon: Restart Analyzer**. That window needs `argone` on your `PATH`.

## Commands

| Command | Description |
| --- | --- |
| **Argon: Focus GUI** | Focus the GUI, opening its panel if it isn't open. <kbd>Ctrl</kbd>+<kbd>Backslash</kbd> in an Argon file. |
| **Argon: Open Cell** | Compile and show a cell, such as `top(100.)`. |
| **Argon: New Cell** | Insert an empty cell in the current module and open it. |
| **Argon: Rename Cell** | Rename the open cell and every reference to it. |
| **Argon: Place Instance** | Place an instance of a cell from the GUI, in the layout view. |
| **Argon: Place Schematic Instance** | Place a schematic instance of a cell from the GUI, in the schematic view. |
| **Argon: Place Device** | Place a device from the GUI, such as `Nmos "nch", w=1., l=0.15`. See [placing devices](./neovim#placing-devices). |
| **Argon: Show Diagnostics** | Open the Problems panel. |
| **Argon: Follow Agent Edits** | Make the current editor group follow [agent edits](./agents). **Argon: Stop Following Agent Edits** turns it off. |
| **Argon: Reload Configuration** | Reload the configuration file and apply it to the GUI. |
| **Argon: Set Configuration** | Set or unset a configuration key for this session, such as `agent.approval "always"`. Values use TOML syntax. |
| **Argon: Save Configuration** | Write the current configuration to a path, or to the default location. |
| **Argon: Open Log** | Open the Argon log. |
| **Argon: Restart Analyzer** | Restart the current project's analyzer. |

Commands that take an argument open a prompt. For **Open Cell**, **Place Instance**, and **Place Schematic Instance**, the prompt lists completions from the open project, the same ones the [Neovim plugin](./neovim#completion) offers. Choose one with the arrow keys and <kbd>Enter</kbd> to insert it, or choose **Run** to run the command.

## Focus handoff

When the GUI opens a command, such as placing an instance, the prompt opens in VS Code. Focus returns to the GUI once you run or cancel it.

## How edits flow

VS Code owns the source. Edits made from the GUI or by an [agent](./agents) are applied to the open document, so they mark it modified and can be undone. Undo and redo in the GUI revert and reapply the GUI's edits to the document it last edited; once that document has changed in other ways, they use VS Code's own undo and redo.

A file an agent creates opens as an unsaved document and is written when you save it. When a file changes on disk, VS Code reloads it if it has no unsaved changes; otherwise the analyzer warns you.

## Agent edits

Each agent edit briefly highlights the text it wrote. With `agent.approval = "always"`, VS Code shows each edit as a diff and asks before applying it.
