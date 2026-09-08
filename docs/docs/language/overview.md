---
title: Language overview
description: The main ideas and syntax of the Argon language.
sidebar_label: Overview
---

# Language overview

Argon is a statically typed language for describing integrated-circuit layout. The syntax and type system are modeled on Rust. What's different is that geometric values, such as the edges of a rectangle, are variables in a linear constraint system rather than fixed numbers.

```argon
cell via_array(cols: Int, pitch: Float) {
    let cut = rect("via", w=20., h=20.);

    for i in std::range(cols) {
        inst(cut, x=(i as Float) * pitch, y=0.);
    }
}
```

## Core ideas

- A **cell** is a layout definition you can call.
- A **function** computes a value, or emits geometry and constraints into the scope that calls it.
- Geometry constructors create rectangles, polygons, paths, and text.
- [`Float`](/language/types/scalars#float) values, including geometry, can be related with equality constraints.
- Calling a cell produces a cell value; [`inst`](/language/builtins/hierarchy#inst) places it in the hierarchy.
- Modules and manifests organize source, dependencies, technology data, and GDS imports.

## Syntax

Declarations and blocks use braces, and statements end with semicolons. A block's last expression, written without a semicolon, is its value.

```argon
fn half(value: Float) -> Float {
    value / 2.
}
```

`let` bindings are immutable. Functions can be used before they're defined; cells are resolved in source order, so declare a cell before the cells that call it.

## Chapters

- [Types and values](./types-values)
- [Cells and functions](./cells-functions)
- [Geometry](./geometry)
- [Constraints](./constraints)
- [Modules and manifests](./modules-manifests)
