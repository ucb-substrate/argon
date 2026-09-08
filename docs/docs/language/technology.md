---
title: Technology files
description: Units, grid, GDS layer mapping, and layer styles.
---

# Technology files

The technology file is TOML. It sets the units and grid, maps Argon layers to GDS layers, and says how each layer is drawn.

```toml
dbu = 1e-10
display_unit = 10
grid = 1
style_name = "Default Layer Properties"

[[layers]]
name = "met1"
gds = [1, 0]
fill = "#0000ff"
border = "#0000ff"

[[layers]]
name = "text.label"
gds = [10, 0]
fill = "#0080ff"
border = "#0080ff"
```

## Global values

| Key | Meaning |
| --- | --- |
| `dbu` | Physical size of one GDS database unit |
| `display_unit` | Size of one source-coordinate unit, in database units |
| `grid` | Snap grid for the solver and the GUI |
| `style_name` | Name given to the layer-style collection |

## Layers

Each `[[layers]]` entry names a layer and gives its GDS layer and datatype. The optional style settings control fill, border, line style, visibility, validity, grouping, patterns, transparency, markings, and animation.

The GUI's layer sidebar comes from this file. A layer that is visible but not valid can be looked at but not drawn on.

The `tech` field in [`Argon.toml`](./modules-manifests#library-manifest) says which technology file a library uses.
