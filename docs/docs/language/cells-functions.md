---
title: Cells and functions
description: Cells describe layout; functions compute values.
---

# Cells and functions

Cells and functions look alike but do different jobs: a cell describes layout, a function computes a value.

## Cells

A cell describes layout and can be placed inside other cells:

```argon
cell pad(width: Float, height: Float) {
    rect("met1", x0=0., y0=0., w=width, h=height);
}
```

Calling `pad(100., 80.)` gives you a cell value. Pass it to [`inst`](/language/builtins/hierarchy#inst) to place it.

## Functions

A function computes a value. Its last expression is the return value:

```argon
fn half(value: Float) -> Float {
    value / 2.
}
```

A function can also emit geometry or constraints into the scope it's called from:

```argon
fn align_left(a: Rect, b: Rect) {
    eq(a.x0, b.x0);
}
```

## Arguments and return types

Argument types are written `name: Type`, and the return type follows `->`. Some argument types can be inferred, but writing them out gives clearer call sites and error messages.

```argon
fn inset_bounds(rect_: Rect, amount: Float) -> Rect {
    crect(
        x0=rect_.x0 + amount,
        y0=rect_.y0 + amount,
        x1=rect_.x1 - amount,
        y1=rect_.y1 - amount,
    )
}
```

## Bindings and order

`let` introduces an immutable binding:

```argon
let bounds = bbox(child);
```

Top-level declarations are resolved across the whole module, so a cell can call a function declared further down the file.
