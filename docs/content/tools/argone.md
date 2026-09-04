---
title: argone command reference
description: Start Neovim and the GUI together, locally or over SSH.
sidebar_label: argone
---

# `argone` command reference

`argone` starts Neovim on an Argon project and launches the GUI alongside it.

```text
argone [--nvim <PATH>] [PATH]
```

| Argument | Default | Description |
| --- | --- | --- |
| `[PATH]` | `.` | Project directory or `.ar` file. A directory must contain `lib.ar`. |
| `--nvim <PATH>` | `nvim` | Neovim executable to use. |

```bash
argone .
argone examples/hierarchy
```

## Remote editing with SSH

`argone ssh` runs Neovim and the analyzer on a remote machine and the GUI on your own.

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
| `--local-gui-port <PORT>` | Allocated | Local port the GUI listens on for callbacks. |
| `--remote-gui-port <PORT>` | Allocated | Port the callback is exposed on remotely. |

```bash
argone ssh layout-host ~/work/chip
argone ssh layout-host . -o ProxyJump=bastion
```

## `argone gui`

Starts only the GUI and connects it to a running analyzer. `argone` and `argone ssh` use this internally; you rarely need to run it yourself.
