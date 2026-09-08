---
title: Constraints and fallback values
description: Equality constraints, free values, and the initial values the GUI edits.
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

## Free values

[`float()`](/language/builtins/constraints#float) creates a new solver variable with no value yet:

```argon
let center = float();
eq(center, (bounds.x0 + bounds.x1) / 2.);
```

## Initial values

Keyword arguments ending in `i`, such as `x0i`, `y1i`, `widthi`, or `x2i`, are initial values. The GUI uses them for any coordinate that no constraint determines:

```argon
let shape = rect("met1", x0i=20., y0i=30., x1i=120., y1i=90.);
```

- Dashed edges are under-constrained.
- Dragging an under-constrained shape updates its initial values.
- Adding a constraint turns the edge solid.
- An initial value never overrides a constraint.

You don't need to write initial values by hand. The first time you drag a shape, the GUI adds any that are missing.

## Dimensions

The Dimension tool writes [`dimension`](/language/builtins/constraints#dimension) calls, which record both the constraint and where its label sits. Create them from the canvas; their arguments are tedious to write by hand.
