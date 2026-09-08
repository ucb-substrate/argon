---
title: Types and values
description: Scalars, collections, tuples, and layout types.
---

# Types and values

Argon's types fall into scalars, collections, tuples, and layout types. Write types out in cell and function signatures: some can be inferred, but explicit signatures make call sites and error messages clearer.

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
| [`[T]`](/language/types/collections#sequences) | `[Float]` | A sequence of one type |
| [`(A, B)`](/language/types/collections#tuples) | `(3, 5,)` | A fixed-size tuple of mixed types |
| [`Any`](/language/types/scalars#any) | — | A value of any type |
| [`()`](/language/types/scalars#unit) | `()` | The unit value and type |

## Numeric literals

The decimal point is what makes a literal a float:

```argon
let count = 50;     // Int
let distance = 50.; // Float
```

Geometry and constraints use `Float`. Counts and indices use `Int`.

## Operators and casts

Arithmetic: `+`, `-`, `*`, `/`, and `%`. Comparison: `==`, `!=`, `<`, `<=`, `>`, and `>=`.

Cast with `as`:

```argon
let offset = (index as Float) * pitch;
```

## Sequences and tuples

Build a sequence with [`list`](/language/builtins/collections#list) or [`cons`](/language/builtins/collections#cons), index it with brackets, and walk it with [`head`](/language/builtins/collections#head) and [`tail`](/language/builtins/collections#tail).

```argon
let widths = list(80., 120., 160.);
let first = widths[0];
let pair = (first, 3,);
```

[`std::range`](/language/std#range) makes an integer sequence for loops.
