---
title: Technology files
description: Units, grid, GDS layer mapping, layer styles, design rules, and vias.
---

# Technology files

The technology file is TOML. It sets the units and grid, maps Argon layers to GDS layers, says how each layer is drawn, and can give design-rule values and vias. The `tech` field in [`Argon.toml`](./modules-manifests#library-manifest) says which technology file a library uses.

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

## Rules and vias

The optional `[rules]` table gives design-rule values by name, which [`std::layout::rule`](/language/std-layout#rule) reads. Lengths are in database units, like `grid`, and a name ending in `.area` is an area in database units squared. Names are free-form; the SKY130 file uses `<layer>.width`, `<layer>.space`, and `<layer>.area`, `<outer>.<inner>_enclosure` for an enclosure on every side, and `<outer>.<inner>_end_enclosure` for the larger one on two opposite sides, with a comment naming each rule.

```toml
[rules]
"met1.width" = 140
"met1.space" = 140
"met1.area" = 83000
"met1.mcon_enclosure" = 30
"met1.mcon_end_enclosure" = 60
```

Each `[[vias]]` entry is a cut layer that joins a `bot` layer to a `top` layer, which [`via` and `via_stack`](/language/std-layout#vias-and-rules) draw. `size` is a square cut's width and `space` the spacing between cuts. Each enclosure is a pair: the first applies on every side of the cut array, and the second on two opposite sides, in whichever direction fits more cuts. One cut layer can appear in several entries, such as a contact that lands on either poly or diffusion with different enclosures.

```toml
[[vias]]
cut = "mcon.drawing"
bot = "li1.drawing"
top = "met1.drawing"
size = 170
space = 190
bot_enclosure = [0, 0]
top_enclosure = [30, 60]
```

`via_stack` chains entries from its bottom shape's layer up to its top shape's, so a stack needs an entry for each step.


## Connectivity

The `[[vias]]` entries say which layers conduct and what joins them: each via's cut joins the shapes on its bottom and top layers that it overlaps, and shapes on one layer join where they overlap or share an edge. A cut over the bottom layers of several of its vias joins only the one whose via is listed last, as a via on a capacitor's top plate doesn't reach the plate below it. Each `[[gates]]` entry names a layer that splits another where it crosses it, as a transistor's gate splits its diffusion into source and drain:

```toml
[[gates]]
layer = "poly.drawing"
splits = "diff.drawing"
```

The headless MCP server's `compile` and `nets` tools use these to check [`net=` tags](/language/std-layout#rect) and pin labels, and to measure nets.

## Capacitance

The optional `[[capacitance]]` and `[[overlap_capacitance]]` entries give the rough coefficients the `nets` tool estimates wiring capacitance with, in aF per µm² of area and aF per µm of perimeter. List the `[[capacitance]]` layers from the lowest up: a layer's area to the substrate excludes what the layers listed before it cover, and an overlap between two layers excludes what a layer listed between them covers. A layer listed with zero coefficients only shields.

```toml
[[capacitance]]
layer = "met1.drawing"
area = 26
perimeter = 41

[[overlap_capacitance]]
top = "met2.drawing"
bot = "met1.drawing"
area = 134
```
