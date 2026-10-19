---
title: Control flow
description: if, match, and for.
---

# Control flow

Argon has `if`, `match` on enums and structs, and `for` over sequences. `if` and `match` are expressions, and an `if` with no `else` is a statement. A `for` loop in brackets is a list comprehension, an expression that builds a list.

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
use std::layout::rect;

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
use std::layout::rect;

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

Match arms use `=>` and end with commas. The arms must cover every variant,
unless a bare name or `_` arm matches the rest.

### Variants with payloads

A variant may carry values, written either as a tuple or as named fields. A
tuple variant is constructed like a call and matched by position, where `_`
skips an element:

```argon
enum Shape {
    Circle(Float),
    Box(Float, Float),
}

fn width(s: Shape) -> Float {
    match s {
        Shape::Circle(r) => 2. * r,
        Shape::Box(w, _) => w,
    }
}
```

A variant with named fields is constructed with braces, like a
[struct](/language/types-values), and matched by naming the fields to bind. The
literal accepts the same `field` shorthand for `field: field`, but no `..base`:
every field must be given. A pattern must name every field too, unless it ends
in `..`; `field: name` renames a binding and `field: _` drops one.

```argon
use std::layout::rect;

enum Shape {
    Circle { r: Float },
    Box { w: Float, h: Float },
}

fn width(s: Shape) -> Float {
    match s {
        Shape::Circle { r } => 2. * r,
        Shape::Box { w, .. } => w,
    }
}

cell top() {
    let w = 300.;
    let h = 20.;
    // Shorthand: `w` and `h` stand for `w: w` and `h: h`.
    let r = rect("met1", x0=0., y0=0., w=width(Shape::Box { w, h }), h=h);
}
```

A field pattern binds a name or `_`, never a nested pattern. As with a struct
literal, a variant literal in an `if` condition, a `match` scrutinee, or a
`for` sequence must be parenthesized, since `name {` there begins the
construct's body.

### Matching structs

A `match` on a [struct](/language/types-values) takes the same braced patterns,
naming the struct instead of a variant. A field pattern may be a unit variant,
which matches only that variant, so an arm like the second one below is needed
to cover the rest:

```argon
struct Via {
    layer: String,
    n: Option<Int>,
}

fn count(v: Via) -> Int {
    match v {
        Via { n: None, .. } => 1,
        Via { n, .. } => std::unwrap_or(n, 1),
    }
}
```

## `for` loops

A `for` loop walks a sequence, usually to emit geometry or instances:

```argon
for i in std::range(4) {
    std::layout::rect("met1", x0=(i as Float) * 100., y0=0., w=60., h=60.);
}
```

[`std::range(stop)`](/language/std#range) yields the integers from zero up to, but not including, `stop`.

An `if` after the sequence skips the elements for which it is false:

```argon
for w in widths if w >= 40. {
    std::layout::rect("met1", x0=0., y0=0., x1=w, y1=20.);
}
```

## List comprehensions

A `for` loop in brackets is an expression: its value is the list of its body's values, one per element that passes the filter.

```argon
fn pitches(n: Int) -> [Float] {
    [for i in std::range(n) { 80. * i as Float }]
}

fn wide(widths: [Float]) -> [Float] {
    [for w in widths if w >= 40. { w }]
}
```

Comprehensions combine with [`std::flatten`](/language/std#flatten), [`std::sum`](/language/std#sum), [`std::any`](/language/std#any), and [`std::all`](/language/std#all) to build, filter, and fold lists without recursion:

```argon
fn count_wide(widths: [Float]) -> Int {
    std::len([for w in widths if w >= 40. { w }])
}
```

## Recursion and tail calls

Every function call and every branch taken opens an execution scope, which the GUI's hierarchy shows. A recursive function that builds its result after the recursive call returns, like `cons(x, f(rest))` or `1 + f(rest)`, therefore nests one call and one branch per step. Prefer a comprehension or a `std` helper for that.

A call whose value is the function's own value, directly or as the value of an `if`, `match`, or block that is, is a **tail call**. Its scope opens beside the caller's instead of inside it, so a loop written as tail recursion stays one level deep however many times it repeats. Its scopes are named like the first call's, with the step appended: `fn count[1]`, `fn count[2]`, and so on.

```argon
// One level deep for any n: the recursive call is the value of the `else`.
fn count_from(items: [Int], n: Int) -> Int {
    if items == [] { n } else { count_from(tail(items), n + 1) }
}
```
