---
title: Neovim plugin
description: The :Argon commands, focus handoff, and diagnostics.
sidebar_label: Neovim plugin
---

# Neovim plugin

The Neovim plugin starts `argon-analyzer` for `.ar` buffers and adds the `:Argon` commands.

## Commands

| Command | Description |
| --- | --- |
| `:Argon gui` | Start the GUI, or focus it if it's already running. |
| `:Argon openCell <EXPR>` | Compile and show a cell, such as `top(100.)`. |
| `:Argon newCell <NAME>` | Insert an empty cell in the current module and open it. |
| `:Argon renameCell <NAME>` | Rename the open cell and every reference to it. |
| `:Argon inst <EXPR>` | Place an instance of a cell from the GUI. |
| `:Argon diagnostics` | Open the diagnostics panel and fill the quickfix list. |
| `:Argon reload` | Reload the configuration file and apply it to the GUI. |
| `:Argon set <KEY> [VALUE]` | Set or unset a configuration key for this session. Values use TOML syntax. |
| `:Argon saveConfig [PATH]` | Write the current configuration to `PATH`, or to the default location. |
| `:Argon log` | Open the Argon log in a new tab. |

## Completion

`openCell` and `inst` complete their argument from the open project. At the
start of the argument they offer the cells in scope, and completing one opens
its argument list; inside the parentheses they offer the callee's keyword
parameters, along with the functions, constants, and modules that are valid
there. `inst` completes against the scope selected in the GUI, so that scope's
parameters and bindings are offered too.

```
:Argon openCell inv<Tab>          inverter  inverter_array
:Argon openCell inverter(<Tab>    nf=  layer=  rect  std  …
```

A parameter is offered as a keyword only if it declares a default, since those
are the ones that can be named at a call site.

## Focus handoff

<kbd>Ctrl</kbd>+<kbd>Backslash</kbd> switches focus between Neovim and the GUI. When the GUI opens a command in Neovim, focus returns to the canvas once the command finishes or is cancelled.

## How edits flow

Neovim owns the source. The analyzer receives each change, recompiles the open cell, and publishes diagnostics. Edits made from the GUI arrive as ordinary buffer edits, so they mark the buffer modified and can be undone.

## Diagnostics

The diagnostics panel lists parser, resolver, type, and execution errors across the workspace. It has mappings to refresh, jump to an entry, and close. For runtime detail, use `:Argon log`.
