---
title: Shortcuts and configuration
description: Keyboard shortcuts, the configuration file, and troubleshooting.
---

# Shortcuts and configuration

## Keyboard shortcuts

| Shortcut | Action |
| --- | --- |
| <kbd>R</kbd> | Rectangle tool |
| <kbd>P</kbd> | Polygon tool |
| <kbd>S</kbd> | Select mode |
| <kbd>D</kbd> | Dimension tool |
| <kbd>Q</kbd> | Edit the selected item |
| <kbd>I</kbd> | Place an instance |
| <kbd>O</kbd> | Open a cell |
| <kbd>Cmd</kbd>+<kbd>N</kbd> | Create a cell in the current module |
| <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>R</kbd> | Rename the open cell |
| <kbd>F</kbd> | Fit the layout to the canvas |
| <kbd>U</kbd> | Undo (in the source buffer) |
| <kbd>Ctrl</kbd>+<kbd>R</kbd> | Redo |
| Arrow keys | Pan |
| <kbd>Cmd/Ctrl</kbd>+<kbd>+</kbd> / <kbd>-</kbd> | Zoom in or out |
| <kbd>:</kbd> | Focus the Neovim command line |
| <kbd>Ctrl</kbd>+<kbd>Backslash</kbd> | Switch between the GUI and Neovim |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>D</kbd> | Show diagnostics |
| <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>M</kbd> | Show messages |
| <kbd>Esc</kbd> | Cancel the current operation |
| <kbd>Enter</kbd> | Confirm or finish the current operation |

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
