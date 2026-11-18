---
title: arc command reference
description: Create, format, check, run, and document Argon libraries.
sidebar_label: arc
---

# `arc` command reference

`arc` manages Argon libraries. It reads `Argon.toml` to find the source, technology file, dependencies, and GDS imports, so you don't pass them on the command line.

## `arc new`

```text
arc new <PATH> [--name <NAME>]
```

| Argument | Required | Description |
| --- | --- | --- |
| `<PATH>` | Yes | Directory to create. Must not already exist. Its last component is the default library name. |
| `--name <NAME>` | No | Library name to write to `Argon.toml` instead. |

Creates `Argon.toml`, `lib.ar`, and `tech.toml`.

```bash
arc new my-layout
arc new layouts/demo --name demo
```

## `arc fmt`

```text
arc fmt [--manifest-path <PATH>] [--check]
```

| Option | Default | Description |
| --- | --- | --- |
| `--manifest-path <PATH>` | Nearest manifest | Library to format. |
| `--check` | Off | Report unformatted files and exit non-zero, without changing them. |

```bash
arc fmt
arc fmt --check
```

`arc fmt` indents each line four spaces deeper than the line that opened the innermost bracket around it, and it collapses repeated blank lines. It breaks lines wider than 100 columns, rustfmt style. On such a line, a block body, such as an `if` branch or a `match`, breaks first: the last one on the line, along with the bodies `else` chains to it, goes one statement or arm per line. Otherwise the widest call, list, tuple, struct literal, parameter list, or `use` group is split. Its opening bracket ends the line, each element goes on its own line with a trailing comma, and the closing bracket starts a new line. This repeats until every line fits. A call whose only argument is a list, call, or struct literal keeps both openers on one line, as in `merge_rects([`. `use` groups, and lists of short literals and names, are packed several to a line. Line breaks you write are kept, comments stay where they are, and files with syntax errors are only re-indented. A file whose leading comments include the line `// arc-fmt: no-wrap` is only re-indented, which suits generated or transcribed geometry.

```argon
let pins = router::merge_rects([
    tap0.vpwr,
    clk_delay.vdd_port,
    inv_rbl.vpwr,
]);
```

## `arc check`

```text
arc check [--manifest-path <PATH>] [--argonc <PATH>]
```

| Option | Default | Description |
| --- | --- | --- |
| `--manifest-path <PATH>` | `Argon.toml` | Library to check. |
| `--argonc <PATH>` | `ARGONC` or `argonc` | Compiler executable to use. |

Parses, resolves, and type-checks the library without running a cell. No technology file is needed.

## `arc run`

```text
arc run --cell <EXPR> [--output <PATH>] [--gds | --gds-only] [--spice] [--spectre] [--netlist-width <COLS>]
```

| Option | Required/default | Description |
| --- | --- | --- |
| `--cell <EXPR>` | Required | Cell to run, such as `top(10., 20.)`. Arguments are Argon expressions evaluated in the library's scope. |
| `-o, --output <PATH>` | `target/argon.bin` | Where to write the compiled layout. |
| `--gds` | Off | Also write `target/argon.gds`. |
| `--gds-only` | Off | Write only `target/argon.gds`, without the compiled layout the editor opens. Uses much less memory, so it suits generating large layouts. Can't be combined with `--output` or `--gds`. |
| `--spice` | Off | Also write a SPICE netlist to `target/argon.spice`. |
| `--spectre` | Off | Also write a Spectre netlist to `target/argon.scs`. |
| `--netlist-width <COLS>` | `80` | Break netlist lines longer than this many characters; `0` turns wrapping off. Requires `--spice` or `--spectre`. |
| `--manifest-path <PATH>` | `Argon.toml` | Library to run. |
| `--argonc <PATH>` | `ARGONC` or `argonc` | Compiler executable to use. |

```bash
arc run --cell 'top(10., 20.)'
arc run --cell 'top(10., 20.)' --gds
arc run --cell 'top(10., 20.)' --gds-only
arc run --cell 'inv(2., 1., 2)' --spice --spectre
```

:::tip
Quote the cell expression so the shell doesn't interpret the parentheses.
:::

Netlists are described in [Schematics](/language/schematic#netlists).

## `arc doc`

```text
arc doc [--manifest-path <PATH>] [--output <DIR>]
```

| Option | Default | Description |
| --- | --- | --- |
| `--manifest-path <PATH>` | Nearest manifest | Library to document. |
| `-o, --output <DIR>` | `target/doc` | Where to write the generated site. |

Generates a standalone HTML reference for the library, one page per module, with signatures, argument tables, enum variants, source locations, and links between the library's own types. Documentation comes from `//!` module comments and `///` declaration comments. Doctests are not run.

```argon title="lib.ar"
//! Standard cells for this library.

use std::layout::rect;

/// Draws a square on the requested layer.
///
/// # Arguments
/// - `layer`: technology layer name.
/// - `size`: square edge length.
cell square(layer: String, size: Float) {
    rect(layer, x0=0., y0=0., w=size, h=size);
}
```

```bash
arc doc
open target/doc/index.html
```
