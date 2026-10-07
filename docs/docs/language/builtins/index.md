---
title: Built-in functions
description: Index of the functions and types available to Argon source.
---

# Built-in functions

Built-in functions are provided by the compiler, are in scope in every module, and need no module prefix. They fall into two groups:

| Page | Functions |
| --- | --- |
| [Constraints](/language/builtins/constraints) | `float`, `eq` |
| [Collections](/language/builtins/collections) | `cons`, `head`, `tail`, `range_full` |

These names are reserved: a module can't declare an item with one of them or bind one with `use`.

The layout functions, such as `rect`, `crect`, `polygon`, `path`, `text`, `inst`, `bbox`, and `dimension`, are in the [`std::layout`](/language/std-layout) module and are imported with `use`. Other functions written in Argon itself live under `std::`; see the [standard library](/language/std).

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
