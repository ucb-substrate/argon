---
title: Built-in functions
description: Index of the functions and types available to Argon source.
---

# Built-in functions

Built-in functions are provided by the compiler and need no module prefix. They fall into four groups:

| Page | Functions |
| --- | --- |
| [Geometry](/language/builtins/geometry) | `rect`, `crect`, `polygon`, `path`, `text` |
| [Constraints](/language/builtins/constraints) | `float`, `eq`, `dimension` |
| [Hierarchy](/language/builtins/hierarchy) | `inst`, `bbox` |
| [Collections](/language/builtins/collections) | `list`, `cons`, `head`, `tail`, `range_full` |

Functions written in Argon itself live under `std::`; see the [standard library](/language/std).

## Types

| Category | Types |
| --- | --- |
| Scalars | [`Float`](/language/types/scalars#float), [`Int`](/language/types/scalars#int), [`Bool`](/language/types/scalars#bool), [`String`](/language/types/scalars#string), [`Any`](/language/types/scalars#any), [`()`](/language/types/scalars#unit) |
| Geometry | [`Rect`](/language/types/rect), [`Polygon`](/language/types/polygon), [`Path`](/language/types/path), [`Point`](/language/types/point) |
| Hierarchy | [`Cell`](/language/types/instance#cell-values), [`Inst`](/language/types/instance#instance-values) |
| Collections | [`[T]`](/language/types/collections#sequences), [`(A, B)`](/language/types/collections#tuples) |

## Signature notation

- Arguments before the keyword list are positional and required.
- `name?` is an optional keyword argument.
- `T` is a type parameter inferred from the arguments.
- Initial values end in `i`, such as `x0i` or `widthi`.
- Unless stated otherwise, coordinates and dimensions are [`Float`](/language/types/scalars#float).
