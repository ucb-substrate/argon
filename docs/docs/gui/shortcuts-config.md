---
title: Shortcuts and configuration
description: Keyboard shortcuts, the configuration file, and troubleshooting.
---

# Shortcuts and configuration

## Keyboard shortcuts

These work in every view while the canvas has focus:

| Shortcut | Action |
| --- | --- |
| <kbd>V</kbd> | Next view: layout, schematic, symbol, and around to layout |
| <kbd>Shift</kbd>+<kbd>V</kbd> | Previous view, around the same cycle |
| <kbd>O</kbd> | Open a cell |
| <kbd>Cmd</kbd>+<kbd>N</kbd> | Create a cell in the current module (also while a text field has focus) |
| <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>R</kbd> | Rename the open cell (also while a text field has focus) |
| <kbd>F</kbd> | Fit the view to the canvas |
| <kbd>U</kbd> | Undo (in the source buffer) |
| <kbd>Ctrl</kbd>+<kbd>R</kbd> | Redo |
| Arrow keys | Pan |
| <kbd>Cmd/Ctrl</kbd>+<kbd>+</kbd> / <kbd>-</kbd> | Zoom in or out |
| <kbd>:</kbd> | Focus the Neovim command line, where `:Argon` arguments complete with <kbd>Tab</kbd> |
| <kbd>Ctrl</kbd>+<kbd>Backslash</kbd> | Switch between the GUI and Neovim |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>D</kbd> | Show diagnostics |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>M</kbd> | Show messages |
| <kbd>Backspace</kbd> | Undo the last point or edge picked by the active tool |
| <kbd>Esc</kbd> | Cancel the current operation or clear the selection |
| <kbd>Enter</kbd> | Confirm or finish the current operation |

In the layout view:

| Shortcut | Action |
| --- | --- |
| <kbd>R</kbd> | Rectangle tool |
| <kbd>P</kbd> | Polygon tool |
| <kbd>S</kbd> | Select mode |
| <kbd>D</kbd> | Dimension tool |
| <kbd>Q</kbd> | Edit the selected item |
| <kbd>I</kbd> | Place an instance |
| <kbd>0</kbd>, <kbd>1</kbd>, <kbd>*</kbd> | Draw the hierarchy as boxes, one level, or in full |

In the [schematic view](/gui/schematic):

| Shortcut | Action |
| --- | --- |
| <kbd>S</kbd> | Select |
| <kbd>R</kbd>, <kbd>Shift</kbd>+<kbd>R</kbd> | Rotate or mirror the selection, or the element being placed |
| <kbd>E</kbd> | Descend into the selected instance |
| <kbd>Shift</kbd>+<kbd>E</kbd> | Return to the cell above |
| <kbd>I</kbd> | Place a schematic instance |
| <kbd>M</kbd> | Place a device |
| <kbd>W</kbd> | Wire |
| <kbd>/</kbd> | Flip the bend of the wire segment being drawn |
| <kbd>P</kbd> | Add a port |
| <kbd>L</kbd> | Add a net label |
| <kbd>Q</kbd> | Edit the selection's properties |
| <kbd>Delete</kbd>, <kbd>Backspace</kbd> | Delete the selection; in a drawing tool, <kbd>Backspace</kbd> drops the last click |
| <kbd>Enter</kbd> | End the wire being drawn at a new net label |
| <kbd>Esc</kbd> | Drop the wire being drawn, leave the tool, or clear the selection |
| <kbd>Cmd</kbd>+<kbd>C</kbd>, <kbd>Cmd</kbd>+<kbd>V</kbd> | Copy the selection; paste it at the pointer |

In the [symbol view](/gui/symbol):

| Shortcut | Action |
| --- | --- |
| <kbd>S</kbd> | Select |
| <kbd>L</kbd> | Line |
| <kbd>P</kbd> | Polygon |
| <kbd>R</kbd> | Rectangle |
| <kbd>C</kbd> | Circle |
| <kbd>A</kbd> | Arc |
| <kbd>T</kbd> | Text |
| <kbd>N</kbd> | Pin, for the next port without one |
| <kbd>Tab</kbd> | Pick another port for the pin tool |
| <kbd>X</kbd>, <kbd>Shift</kbd>+<kbd>X</kbd> | Rotate or mirror the selection about its center |
| <kbd>Q</kbd> | Edit the selection's properties |
| <kbd>Delete</kbd>, <kbd>Backspace</kbd> | Delete the selection; in a drawing tool, <kbd>Backspace</kbd> drops the last click |
| <kbd>Enter</kbd> | Finish the line or polygon being drawn |
| <kbd>Esc</kbd> | Drop the shape being drawn, leave the tool, or clear the selection |
| <kbd>Cmd</kbd>+<kbd>C</kbd>, <kbd>Cmd</kbd>+<kbd>V</kbd> | Copy the selected items; paste them at the pointer |

## Configuration

Argon reads `$XDG_CONFIG_HOME/argon/config.toml`, or `~/.config/argon/config.toml` if that variable isn't set.

```toml
[gui]
dark_mode = true
hierarchy_depth = 3
icon_size = 20
font_size = 14

[log]
level = "info"
```

`font_size` and `icon_size` are in logical pixels, from 1 to 256. After editing the file, run `:Argon reload`. To change a value for this session only, use `:Argon set`; to write the current settings back to disk, use `:Argon saveConfig`.

## Troubleshooting

### The open cell is invalid after a signature change

Press <kbd>O</kbd> and reopen it with arguments that match the new signature. Argon won't guess values for new parameters.

### Something won't move

A solid edge or an existing dimension is already fixing that coordinate. An initial value can't override a constraint.

### You need more detail on an error

Run `:Argon diagnostics` and `:Argon log`. For more, set `log.level = "debug"` and reload the configuration.

### The GUI didn't start

Check that `argon-analyzer` and `argone` are on your `PATH`, the buffer's file ends in `.ar`, and the library has `Argon.toml` and `lib.ar`. Then run `:Argon gui`.
