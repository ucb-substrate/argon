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
arc run --cell <EXPR> [--output <PATH>] [--gds]
```

| Option | Required/default | Description |
| --- | --- | --- |
| `--cell <EXPR>` | Required | Cell to run, such as `top(10., 20.)`. Arguments are Argon expressions evaluated in the library's scope. |
| `-o, --output <PATH>` | `target/argon.bin` | Where to write the compiled layout. |
| `--gds` | Off | Also write `target/argon.gds`. |
| `--manifest-path <PATH>` | `Argon.toml` | Library to run. |
| `--argonc <PATH>` | `ARGONC` or `argonc` | Compiler executable to use. |

```bash
arc run --cell 'top(10., 20.)'
arc run --cell 'top(10., 20.)' --gds
```

:::tip
Quote the cell expression so the shell doesn't interpret the parentheses.
:::

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
