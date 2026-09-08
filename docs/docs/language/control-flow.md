---
title: Control flow
description: if, match, and for.
---

# Control flow

Argon has `if`, `match` on enums, and `for` over sequences. `if` and `match` are expressions.

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
