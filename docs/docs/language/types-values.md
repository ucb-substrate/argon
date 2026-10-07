---
title: Types and values
description: Scalars, collections, tuples, and layout types.
---

# Types and values

Argon's types fall into scalars, collections, tuples, and layout types. Write types out in cell and function signatures: some can be inferred, but explicit signatures make call sites and error messages clearer.

Structs, enums, functions, and cells may take type parameters, written in angle brackets after the name: `struct Pair<A, B>`, `fn last<T>(items: [T]) -> T`. A type parameter stands for one type per use, which is inferred from the arguments, and a value of type `T` can only be stored, passed, and returned.

A struct groups values under named fields, written `Size { w: 100., h: 50. }` and read with `.` or taken apart with a pattern, as in `let Size { w, h } = s;` (see [bindings](/language/cells-functions#bindings-and-order)). An enum is a fixed set of variants, each of which may carry a tuple payload or named fields of its own; see [enums and `match`](/language/control-flow#enums-and-match).

| Type | Example | Used for |
| --- | --- | --- |
| [`Float`](/language/types/scalars#float) | `12.`, `-0.5` | Coordinates, distances, and linear expressions |
| [`Int`](/language/types/scalars#int) | `12`, `-3` | Counts, indices, and discrete parameters |
| [`Bool`](/language/types/scalars#bool) | `true`, `false` | Conditions and flags |
| [`String`](/language/types/scalars#string) | `"met1"` | Layer names and text |
| [`Rect`](/language/types/rect) | `rect("met1")` | Rectangles, drawn or construction-only |
| [`Polygon`](/language/types/polygon) | `polygon("met1", 3)` | Polygons |
| [`Path`](/language/types/path) | `path("met1", 2)` | Paths with a width |
| [`Point`](/language/types/point) | `shape.points[0]` | A polygon or path vertex |
| [`Inst`](/language/types/instance) | `inst(child())` | A placed cell |
| [`Signal`](/language/schematic#signals-and-nets) | `Signal()` | A signal of a cell's schematic |
| [`SchematicInst`](/language/types/instance#schematic-instances) | `std::schematic::inst(child())` | A cell placed in a schematic |
| [`[T]`](/language/types/collections#sequences) | `[Float]` | A sequence of one type |
| [`(A, B)`](/language/types/collections#tuples) | `(3, 5)` | A fixed-size tuple of mixed types |
| [`Option<T>`](/language/std#option) | `Some(3)`, `None` | A value that may be absent |
| [`Any`](/language/types/scalars#any) | — | A value of any type |
| [`()`](/language/types/scalars#unit) | `()` | The unit value and type |

The layout examples call `rect`, `polygon`, `path`, and `inst` from [`std::layout`](/language/std-layout), imported with `use std::layout::{inst, path, polygon, rect};`. `Signal` is imported from [`std::schematic`](/language/std-schematic) like any other item, with `use std::schematic::Signal;`, and is only a type name once imported.

## Numeric literals

A decimal point or an exponent makes a literal a float:

```argon
let count = 50;       // Int
let distance = 50.;   // Float
let cap = 1e-12;      // Float
let width = 2.5e3;    // Float
```

An exponent is `e` or `E`, an optional sign, and digits, written with no spaces: `1e-12`, `2.5e3`, `1.0E+6`. Write at least one digit after the decimal point before an exponent, since `1.e3` reads the field `e3` of `1`.

Geometry and constraints use `Float`. Counts and indices use `Int`.

## Operators and casts

Arithmetic: `+`, `-`, `*`, `/`, and `%`. Comparison: `==`, `!=`, `<`, `<=`, `>`, and `>=`.

Cast with `as`:

```argon
let offset = (index as Float) * pitch;
```

## Sequences and tuples

Write a sequence as a bracketed list, index it with brackets, and walk it with [`head`](/language/builtins/collections#head) and [`tail`](/language/builtins/collections#tail).

```argon
let widths = [80., 120., 160.];
let first = widths[0];
let pair = (first, 3);
```

[`std::range`](/language/std#range) makes an integer sequence for loops.
