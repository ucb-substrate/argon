---
title: Control flow
description: if, match, and for.
---

# Control flow

Argon has `if`, `match` on enums, and `for` over sequences. `if` and `match` are expressions, and an `if` with no `else` is a statement.

## `if` expressions

An `if` is an expression, so it can be the body of a function. Both branches must have the same type.

```argon
fn choose_pitch(dense: Bool) -> Float {
    if dense {
        80.
    } else {
        120.
    }
}
```

## `if` statements

When the point of an `if` is to build something rather than to produce a value, the `else` may be left off:

```argon
cell strap(w: Float, h: Float, dummy: Bool) {
    let met1 = rect("met1", x0=0., y0=0., x1=w, y1=h)!;

    if !dummy && w >= 40. {
        let via = rect("via1")!;
        eq(via.x0, met1.x0 + 10.);
        eq(via.x1, met1.x1 - 10.);
    }
}
```

Such an `if` yields nothing, so it is a **statement**: it is only allowed where a statement is expected, and never where a value is. `let x = if dense { 80. };` is a syntax error — an `if` used for its value needs its `else`.

For the same reason the branch may not produce a value either: it has to end in a `let` or a `;`, as the body above does. `if c { rect("met1")! }` is an error, and the fix is a `;`.

## `else if`

`else` takes either a block or another `if`, so conditions chain:

```argon
fn width(dense: Bool, wide: Bool) -> Float {
    if dense {
        80.
    } else if wide {
        200.
    } else {
        120.
    }
}
```

A chain used for its value must end in an `else` block, since one of the branches has to run. A chain used as a statement need not — and then nothing is built when no condition holds:

```argon
cell band(n: Int) {
    if n == 0 {
        rect("met1", x0=0., y0=0., x1=100., y1=20.);
    } else if n == 1 {
        rect("met1", x0=0., y0=0., x1=200., y1=20.);
    }
}
```

## Enums and `match`

An enum is a fixed set of variants:

```argon
enum Metal {
    M1,
    M2,
}

fn width(layer: Metal) -> Float {
    match layer {
        Metal::M1 => 80.,
        Metal::M2 => 120.,
    }
}
```

Match arms use `=>` and end with commas.

## `for` loops

A `for` loop walks a sequence, usually to emit geometry or instances:

```argon
for i in std::range(4) {
    rect("met1", x0=(i as Float) * 100., y0=0., w=60., h=60.);
}
```

[`std::range(stop)`](/language/std#range) yields the integers from zero up to, but not including, `stop`.
