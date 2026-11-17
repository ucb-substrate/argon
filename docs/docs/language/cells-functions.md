---
title: Cells and functions
description: Cells describe layout; functions compute values.
---

# Cells and functions

Cells and functions look alike but do different jobs: a cell describes layout, a function computes a value.

## Cells

A cell describes layout and can be placed inside other cells:

```argon
use std::layout::rect;

cell pad(width: Float, height: Float) {
    rect("met1", x0=0., y0=0., w=width, h=height);
}
```

Calling `pad(100., 80.)` gives you a cell value. Pass it to [`inst`](/language/std-layout#inst) to place it. Both `rect` and `inst` are in the [`std::layout`](/language/std-layout) module; the `use` line brings `rect` into scope.

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
use std::layout::crect;

fn inset_bounds(rect_: Rect, amount: Float) -> Rect {
    crect(
        x0=rect_.x0 + amount,
        y0=rect_.y0 + amount,
        x1=rect_.x1 - amount,
        y1=rect_.y1 - amount,
    )
}
```

## Keyword parameters

A parameter with a default is a keyword parameter. A call passes it by name, in any order, or leaves it out to get the default, which may refer to the parameters before it. Keyword parameters come after the positional ones, and a positional parameter can't be passed by name.

```argon
use std::layout::{inst, rect};

cell bar(w: Float, h: Float = w, layer: String = "met1") {
    rect(layer, x0=0., y0=0., w=w, h=h);
}

cell top() {
    let square = inst(bar(100.), x=0., y=0.);
    let tall = inst(bar(100., layer="met2", h=300.), x=200., y=0.);
}
```

## Bindings and order

`let` introduces an immutable binding:

```argon
let bounds = std::layout::bbox(child);
```

A `let` can also take a struct apart. The pattern names the struct and binds its fields: a bare `name` binds the field of that name, `field: other` binds it under another name, and `field: _` drops it. The pattern must name every field unless it ends in `..`, and field patterns do not nest.

```argon
let Size { w, h: height } = size;
let ViaParams { layer, .. } = params;
```

At the top of a cell, each name a pattern binds is a field of the cell, like any other `let`.

Top-level declarations are resolved across the whole module, so a cell can instantiate a cell or call a function declared further down the file.

Module-level constants aren't implemented, so a `const` declaration is an error. Use a function without parameters instead, such as `fn pitch() -> Float { 430. }`.

## Public fields

The top-level `let` bindings of a cell are its fields. An [instance](/language/types/instance#instance-values) of the cell can read a field only if the cell declares it with `pub let`. The other fields are private to the cell.

```argon
use std::layout::{inst, rect};

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
- A public field that holds an instance can be read through as well: when `half` has `pub let n = inst(...)`, `h.n.g` reads `g` from it in the parent's coordinates.

## Emission

A postfix `!`, as in `let r = crect(x0=0., y0=0., w=10., h=10.)!;`, emits a value into its scope for the GUI, which lists it in the hierarchy. It doesn't add geometry to the exported layout or add a constraint.

## Reserved names

These names can't be used where noted:

| Names | Not allowed as | Why |
| --- | --- | --- |
| `as`, `cell`, `const`, `else`, `enum`, `false`, `fn`, `for`, `if`, `in`, `let`, `match`, `mod`, `pub`, `struct`, `true`, `use` | Any name | Keywords. |
| The [built-in functions](/language/builtins), such as `eq`, `float`, `grid_floor`, `head`, and `cons` | The name of a cell, function, or other module item, or a name bound with `use` | Every module calls them by bare name. A local `let` may shadow one. |
| `x`, `y` | A `let` at the top level of a cell | They are an [instance's](/language/types/instance#instance-values) position, so they can't also be fields. Cell parameters, and `let`s in functions, loops, branches, and blocks, may use them. |
| `x`, `y`, `angle`, `reflect`, in any case | A [device](/language/std-schematic#device) parameter | They place the device in the schematic; see [placement and routes](/language/schematic-drawing#placement-and-routes). |

Simulators also reserve some net and subcircuit names, which netlists rename; see [reserved names in schematics](/language/schematic#reserved-names).
