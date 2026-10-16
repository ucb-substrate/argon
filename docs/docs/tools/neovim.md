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
| `:Argon gui` | Focus the GUI's canvas. With `argone --terminal`, start the GUI first if it isn't running. |
| `:Argon openCell <EXPR>` | Compile and show a cell, such as `top(100.)`. |
| `:Argon newCell <NAME>` | Insert an empty cell in the current module and open it. |
| `:Argon renameCell <NAME>` | Rename the open cell and every reference to it. |
| `:Argon inst <EXPR>` | Place an instance of a cell from the GUI, in the layout view. |
| `:Argon sinst <EXPR>` | Place a schematic instance of a cell from the GUI, in the schematic view. |
| `:Argon device <KIND> [<COUNT>] [<ARGS>]` | Place a device from the GUI, such as `:Argon device Nmos "nch", w=1., l=0.15`. |
| `:Argon diagnostics` | Open the diagnostics panel and fill the quickfix list. |
| `:Argon follow [on\|off]` | Make the current window follow [agent edits](./agents), or stop. |
| `:Argon reload` | Reload the configuration file and apply it to the GUI. |
| `:Argon set <KEY> [VALUE]` | Set or unset a configuration key for this session. Values use TOML syntax. |
| `:Argon saveConfig [PATH]` | Write the current configuration to `PATH`, or to the default location. |
| `:Argon log` | Open the Argon log in a new tab. |

## Placing devices

`:Argon device` takes the kind, written as a variant of
[`DeviceKind`](/language/std-schematic#devicekind) in any case, then the
model and parameters as they would be written in a `device` call. A `Subckt`
device needs its terminal count after the kind, and a `Bjt`, `Npn`, or `Pnp`
device takes an optional count of 3 or 4; the other kinds know their count.
A device without a model, such as `:Argon device Res value=1.`, gets an empty
model. The analyzer compiles the device before the GUI shows it, so a mistake
is reported at once. The GUI's device palette opens the command with a kind
filled in.

```
:Argon device Nmos "nch", w=1., l=0.15
:Argon device Subckt 3 "opamp"
:Argon device Cap value=1e-12
```

## Completion

`openCell`, `inst`, and `sinst` complete their argument from the open project.
At the start of the argument they offer the cells in scope, and completing one
opens its argument list; inside the parentheses they offer the callee's
keyword parameters, along with the functions, constants, and modules that are
valid there. `inst` completes against the scope selected in the GUI, so that
scope's parameters and bindings are offered too, and `sinst` against the body
of the cell the schematic view shows. `device` completes the device kind.

```
:Argon openCell inv<Tab>          inverter  inverter_array
:Argon openCell inverter(<Tab>    nf=  layer=  rect  std  …
```

A parameter is offered as a keyword only if it declares a default, since those
are the ones that can be named at a call site.

## Focus handoff

In the `argone` window, <kbd>Cmd</kbd>+<kbd>Backslash</kbd> switches focus between Neovim and the canvas, and <kbd>Ctrl</kbd>+<kbd>Backslash</kbd> belongs to Neovim. With `argone --terminal`, <kbd>Ctrl</kbd>+<kbd>Backslash</kbd> switches between the terminal and the GUI. When the GUI opens a command in Neovim, focus returns to the canvas once the command finishes or is cancelled.

## How edits flow

Neovim owns the source. The analyzer receives each change, recompiles the open cell, and publishes diagnostics. Edits made from the GUI or by an [agent](./agents) arrive as ordinary buffer edits, so they mark the buffer modified and can be undone.

When a file changes on disk, the analyzer reloads its buffer if the buffer has no unsaved changes, and warns you if it does.

Compilation progress is available to Fidget and other LSP progress plugins. Argon does not add a second spinner or completion message to Neovim's message area.

## Diagnostics

The diagnostics panel lists parser, resolver, type, and execution errors across the workspace. It has mappings to refresh, jump to an entry, and close. For runtime detail, use `:Argon log`.
