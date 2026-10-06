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

A `let` can also take a struct apart. The pattern names the struct and binds its fields: a bare `name` binds the field of that name, `field: other` binds it under another name, and `field: _` drops it. The pattern must name every field unless it ends in `..`, and field patterns do not nest.

```argon
let Size { w, h: height } = size;
let ViaParams { layer, .. } = params;
```

At the top of a cell, each name a pattern binds is a field of the cell, like any other `let`.

Top-level declarations are resolved across the whole module, so a cell can call a function declared further down the file.

## Public fields

The top-level `let` bindings of a cell are its fields. An [instance](/language/types/instance#instance-values) of the cell can read a field only if the cell declares it with `pub let`. The other fields are private to the cell.

```argon
cell pad(width: Float, height: Float) {
    pub let metal = rect("met1", x0=0., y0=0., w=width, h=height);
    let margin = 10.;
}

cell top() {
    let p = inst(pad(100., 80.), x=0., y=0.);
    rect("met2", x0=p.metal.x0, y0=p.metal.y0, w=p.metal.w, h=20.);
}
```

Here `p.metal` is readable, but `p.margin` is an error because `margin` is private.

- `pub` applies only to a `let` at the top level of a cell body. A `pub let` in a function, loop, branch, or block is an error.
- A `pub let` with a pattern, such as `pub let Size { w, h } = size;`, makes every name the pattern binds public.
- If a name is bound more than once, the last binding is the field, and its `pub` decides whether the field is public.
