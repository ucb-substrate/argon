---
title: argone command reference
description: Start Neovim and the GUI together, locally or over SSH.
sidebar_label: argone
---

# `argone` command reference

`argone` opens an Argon project in one window, with Neovim on the left and the GUI on the right.

```text
argone [--nvim <PATH>] [--terminal] [PATH]
```

| Argument | Default | Description |
| --- | --- | --- |
| `[PATH]` | `.` | Project directory or `.ar` file. A directory must contain `lib.ar`. |
| `--nvim <PATH>` | `nvim` | Neovim executable to use. |
| `--terminal` | Off | Run Neovim in this terminal and the GUI in a window of its own. |

```bash
argone .
argone examples/hierarchy
argone --terminal .
```

Neovim runs inside the window with your own configuration and plugins. Quitting Neovim closes the window; closing the window or pressing <kbd>Cmd</kbd>+<kbd>Q</kbd> asks Neovim to quit, so it can ask about unsaved buffers first. If Neovim exits with an error, the window stays open and shows its exit status.

With `--terminal`, Neovim stays in the terminal you ran `argone` from, and <kbd>Ctrl</kbd>+<kbd>Backslash</kbd> switches between it and the GUI. On platforms other than macOS and Linux, `argone` always works this way.

## Remote editing with SSH

`argone ssh` runs Neovim and the analyzer on a remote machine and the GUI on your own. Neovim appears in the GUI's window as it does locally; with `--terminal`, it runs in your terminal instead.

```text
argone ssh <HOST> [PATH] [OPTIONS]
```

| Argument or option | Default | Description |
| --- | --- | --- |
| `<HOST>` | Required | Host name or OpenSSH config alias. |
| `[PATH]` | `.` | Project directory or file on the remote machine. |
| `--ssh <PATH>` | `ssh` | SSH executable to use. |
| `-o, --ssh-option <OPTION>` | None | An OpenSSH option. Repeat for more than one. |
| `--local-analyzer-port <PORT>` | Allocated | Local port forwarded to the remote analyzer. |
| `--remote-analyzer-port <PORT>` | Allocated | Analyzer port on the remote machine. |
| `--terminal` | Off | Run remote Neovim in this terminal and the GUI in a window of its own. |

```bash
argone ssh layout-host ~/work/chip
argone ssh layout-host . -o ProxyJump=bastion
```

## `argone gui`

Starts only the GUI, without Neovim, and connects it to a running analyzer. `argone --terminal` and `argone ssh --terminal` use this internally; you rarely need to run it yourself.

```text
argone gui <ADDRESS>
```

The GUI authenticates with the analyzer's session token. It reads the token from `ARGON_SESSION_TOKEN`, or, for an analyzer on the same machine, from the analyzer's session record.
