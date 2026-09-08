---
title: Geometry
description: Rectangles, polygons, paths, and text.
---

# Geometry

Geometry constructors take positional arguments first, then keyword arguments. The coordinates they expose are solver variables, not plain numbers, so you can constrain them after the fact.

## Rectangles

```argon
let metal = rect("met1", x0=0., y0=0., w=200., h=100.);
let bounds = crect(x0=0., y0=0., x1=400., y1=300.);
```

[`rect`](/language/builtins/geometry#rect) draws a rectangle on a layer. [`crect`](/language/builtins/geometry#crect) makes a construction rectangle, which isn't exported and needs no layer.

Both have `x0`, `y0`, `x1`, `y1`, `w`, and `h`; see [`Rect`](/language/types/rect) for how they relate.

## Polygons

[`polygon`](/language/builtins/geometry#polygon) takes a layer and a vertex count. Set or constrain each coordinate individually:

```argon
let outline = polygon(
    "met1", 3,
    x0=0., y0=0.,
    x1=100., y1=0.,
    x2=50., y2=80.,
);
```

`outline.x2` and `outline.points[2].x` are the same coordinate.

## Paths

A path is a centerline with a width, and optional extensions past each end:

```argon
let route = path(
    "met2", 3,
    width=20.,
    x0=0., y0=0.,
    x1=100., y1=0.,
    x2=100., y2=100.,
);
```

See [`Path`](/language/types/path) and the [`path` constructor](/language/builtins/geometry#path).

## Text

```argon
text("VDD", "text.label", 40., 80.);
```

[`text`](/language/builtins/geometry#text) places a label on a text layer.
