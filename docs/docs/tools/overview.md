---
title: Overview
description: The Argon executables and the Neovim plugin.
---

# Command-line tools

The `argon` package installs four executables.

| Tool | Purpose |
| --- | --- |
| [`arc`](./arc) | Create, format, check, run, export, and document libraries. Reads `Argon.toml`. |
| [`argone`](./argone) | Start Neovim and the GUI together, locally or over SSH. |
| [`argonc`](./argonc) | The compiler itself, driven directly without a manifest. |
| [`argon-analyzer`](./neovim) | The language server. The Neovim plugin starts it; it provides diagnostics and applies the GUI's source edits. |

Day to day you'll use `arc` and `argone`. `argonc` is for scripts and for tools built on the compiler. The [Neovim plugin](./neovim) exposes the analyzer's features as `:Argon` commands. The [GUI](/gui/workspace) has its own book.
