---
title: Constraints and fallback values
description: Equality constraints, how the solver determines coordinates, free values, and the initial values the GUI edits.
---

# Constraints and fallback values

Argon has two ways to give a coordinate a value. A constraint fixes it. An initial value only says where it starts, and the GUI can move it.

## Equality constraints

[`eq(left, right)`](/language/builtins/constraints#eq) makes two linear float expressions equal:

```argon
eq(inner.x0, outer.x0 + inset);
eq(inner.y0, outer.y0 + inset);
eq(inner.x1, outer.x1 - inset);
eq(inner.y1, outer.y1 - inset);
```

Either side can mix cell parameters, literals, and geometry fields, as long as the result is linear.

## How the solver works

Every coordinate is a variable of a linear system:

- A rectangle has four, `x0`, `y0`, `x1`, and `y1`. Its `w` and `h` are `x1 - x0` and `y1 - y0`, not variables of their own.
- An instance has two, `x` and `y`. Its fields are expressions of those and of the cell's own geometry.
- [`float()`](/language/builtins/constraints#float) makes one with no constraints on it.

A coordinate keyword such as `x0=0.` or `w=100.` is an equation, just like an `eq`. Each cell's solver collects the equations as the cell runs and solves a variable as soon as they determine it. When the cell finishes, every variable that a rectangle, instance, or label uses must be determined, and no equation may disagree with another. Otherwise the cell fails with one of two errors.

### Underconstrained

A coordinate that no equation determines is reported where its shape was made:

```text
error: rectangle is underconstrained: y1 is not determined
error: instance is underconstrained: x and y are not determined
```

Construction rectangles from `crect` count too. Each rectangle needs two independent equations per axis: two of `x0`, `x1`, and `w`, and two of `y0`, `y1`, and `h`, given directly or through other geometry. Each instance needs its position, given with `x=` and `y=` or by constraining one of its fields:

```argon
cell pad() {
    pub let metal = rect("met1", x0=0., y0=0., w=100., h=100.);
}

cell pair() {
    let a = inst(pad(), x=0., y=0.);
    let b = inst(pad());
    eq(b.metal.x0, a.metal.x1 + 200.);
    eq(b.y, a.y);
}
```

[Initial values](#initial-values) don't count: `arc` reports a coordinate that only an initial value sets as underconstrained.

### Inconsistent

An equation that disagrees with values already determined is reported as an `inconsistent constraint`, at that equation, with how far apart it finds the two sides:

```argon
let r = rect("met1", x0=0., y0=0., w=100., h=100.);
eq(r.x1, 200.); // error: inconsistent constraint: ... leave its two sides 100 apart
```

Repeating a fact the system already implies is fine as long as it agrees: `rect("met1", x0=0., x1=100., w=100., y0=0., h=100.)` compiles. The usual cause is giving a shape both ends and its size where the size depends on parameters, so that the three agree for some parameters and not others. Give each shape exactly two equations per axis, and derive everything else from them.

### Values that wait

Equations are linear, but other uses of a coordinate need its number:

- comparisons and `if` conditions;
- casts such as `as Int`;
- [`std::max`](/language/std#max) and [`std::min`](/language/std#min);
- multiplying or dividing two coordinates.

Each of these waits until the solver has determined the variables it reads. A shape's `w` is available only once both of its edges are, so the width of a shape that isn't placed yet can't be read, even when its size is fixed. If the value is determined only by what comes after the wait, the cell never finishes it, and the shapes involved are reported as underconstrained:

```argon
let vias = array(cut, 3, 320., 0.);
// Waits on `vias.w`, which needs `vias` placed, which needs `n`.
let n = ((met1.w - vias.w) / 10.) as Int;
eq(vias.x0, met1.x0 + (n as Float) * 5.);
```

The error names each expression still waiting on the values it reports, with its line:

```text
error: rectangle is underconstrained: x1 is not determined, and `r.w > 100.` at line 4 waits on it
```

Compute counts and offsets from parameters and from shapes that are already placed. To center one shape in another, use [`std::layout::center_rects`](/language/std-layout#center-rects), which needs no numbers.

## Staying on the grid

Solved coordinates must lie on the technology's [grid](/language/technology), 5 nm in SKY130. One that doesn't is an error:

```text
error: solved value 27.5 is off the 5 grid (nearest grid point is 30)
```

The usual cause is centering: half of an odd number of grid steps falls between grid points. [`center_rects`](/language/std-layout#center-rects) and [`grid_floor`](/language/builtins/constraints#grid-floor) round down to the grid instead, so use them rather than writing midpoints with `/ 2.`:

```argon
center_rects(met1, vias);
text("vdd", "li1.label", grid_floor((pin.x0 + pin.x1) / 2.), grid_floor((pin.y0 + pin.y1) / 2.));
```

Other causes are dividing a length by a count, as in a pitch of `w / 3.`, and literals off the grid.

## Free values

[`float()`](/language/builtins/constraints#float) creates a new solver variable with no value yet. Here it spaces three shapes evenly, whatever the gap turns out to be:

```argon
let gap = float();
eq(b.x0, a.x1 + gap);
eq(c.x0, b.x1 + gap);
```

## Initial values

Keyword arguments ending in `i`, such as `x0i`, `y1i`, `widthi`, or `x2i`, are initial values. The GUI uses them for any coordinate that no constraint determines:

```argon
let shape = std::layout::rect("met1", x0i=20., y0i=30., x1i=120., y1i=90.);
```

- Dashed edges are under-constrained.
- Dragging an under-constrained shape updates its initial values.
- Adding a constraint turns the edge solid.
- An initial value never overrides a constraint.

You don't need to write initial values by hand. The first time you drag a shape, the GUI adds any that are missing.

## Dimensions

The Dimension tool writes [`std::layout::dimension`](/language/std-layout#dimension) calls, which record both the constraint and where its label sits. Create them from the canvas; their arguments are tedious to write by hand.
