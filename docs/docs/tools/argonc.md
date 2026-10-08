---
title: argonc command reference
description: Run the Argon compiler directly.
sidebar_label: argonc
---

# `argonc` command reference

`argonc` is the compiler. It takes every input on the command line, so for everyday work use [`arc`](./arc), which reads them from `Argon.toml`.

```text
argonc <ROOT> (--check | --cell <EXPR>) [OPTIONS]
```

| Argument or option | Required/default | Description |
| --- | --- | --- |
| `<ROOT>` | Required | Library directory or its `lib.ar`. |
| `--check` | One mode required | Parse, resolve, and type-check, then stop. Can't be combined with `--cell`. |
| `--cell <EXPR>` | One mode required | Cell to run. |
| `--tech <PATH>` | Required with `--cell` | Technology file. |
| `--dependency <NAME=PATH>` | Repeatable | Add a dependency. The path can be a directory or a `lib.ar`. |
| `--gds-import <NAME=PATH>` | Repeatable | Import a GDS cell. `NAME` may be a module path. |
| `-o, --output <PATH>` | Beside `lib.ar` | Where to write the compiled layout. |
| `--gds <PATH>` | None | Also write GDS to this path. |
| `--spice <PATH>` | None | Also write a SPICE netlist to this path. Requires `--cell`. |
| `--spectre <PATH>` | None | Also write a Spectre netlist to this path. Requires `--cell`. |
| `--netlist-width <COLS>` | `80` | Break netlist lines longer than this many characters; `0` turns wrapping off. Requires `--spice` or `--spectre`. |
| `--error-format` | `human` | `human` or `json`. |

```bash
argonc . --check
argonc . --cell 'top()' --tech tech.toml -o target/top.bin --gds target/top.gds
argonc . --cell 'inv(2., 1., 2)' --tech tech.toml --spice target/inv.spice --spectre target/inv.scs
```

Netlists are described in [Schematics](/language/schematic#netlists).
