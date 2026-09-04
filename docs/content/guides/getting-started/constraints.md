---
title: Add constraints
description: Constrain the two rectangles and turn width and height into parameters.
---

# Add constraints

A constraint fixes a relationship between geometric values. It's different from the initial values the GUI wrote when you drew the rectangles: a constraint determines a value, while an initial value only positions something that is otherwise free.

## Inset the inner rectangle

Press <kbd>D</kbd> for the Dimension tool. Click the matching edge on each rectangle, then click where the label should go. Type `50.` and press <kbd>Enter</kbd>.

Do the same for the other three sides.

:::warning Floats need a decimal point
`50` is an [`Int`](/language/types/scalars#int); `50.` is a [`Float`](/language/types/scalars#float). Coordinates and dimensions are floats.
:::

In source, the four dimensions amount to:

```argon
eq(inner.x0, outer.x0 + 50.);
eq(inner.y0, outer.y0 + 50.);
eq(inner.x1, outer.x1 - 50.);
eq(inner.y1, outer.y1 - 50.);
```

## Make width and height parameters

Give the cell two arguments and constrain the outer rectangle to them:

```argon
cell inset_rect(w: Float, h: Float) {
    let outer = rect("met1", x0=0., y0=0.);
    eq(outer.w, w);
    eq(outer.h, h);

    let inner = rect("met2");
    eq(inner.x0, outer.x0 + 50.);
    eq(inner.y0, outer.y0 + 50.);
    eq(inner.x1, outer.x1 - 50.);
    eq(inner.y1, outer.y1 - 50.);
}
```

The cell now takes two arguments, so `inset_rect()` no longer compiles. Press <kbd>O</kbd> and open `inset_rect(200., 200.)`.

## Reading the canvas

- A solid edge is fixed by constraints.
- A dashed edge still depends on an initial value.
- Dragging a dashed edge updates its `*i` argument in the source.
- An initial value never overrides a constraint.

[Constraints and fallback values](/language/constraints) covers the model in more depth. Next: [Hierarchy and export](./hierarchy-export).
