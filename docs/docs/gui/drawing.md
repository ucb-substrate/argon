---
title: Drawing and editing
description: The canvas tools for geometry, dimensions, and instances.
---

# Drawing and editing

Canvas shortcuts work while the canvas has focus and no text field is active.

## Select

Press <kbd>S</kbd>. You can select shapes, instances, edges, and dimension labels; what's selected determines which edits are available. Press <kbd>Q</kbd> to edit a selected dimension.

## Rectangle

Pick a layer, press <kbd>R</kbd>, and click two opposite corners. The [`rect`](/language/builtins/geometry#rect) call the GUI writes uses initial values (`x0i` and so on), so you can drag the rectangle around until constraints pin it down.

## Polygon

Pick a layer, press <kbd>P</kbd>, and click the vertices in order. <kbd>Enter</kbd> closes the polygon and writes it to the source; <kbd>Esc</kbd> discards it.

Each vertex is constrained independently. A vertex with one constrained axis can still be dragged along the other.

## Path

Choose Path from the Tools menu or the tool strip, pick a layer, and click the centerline points. A path's points, width, and end extensions are all editable.

## Dimension

Press <kbd>D</kbd>, click two compatible edges, then click where the label should go. Type a float such as `50.` or a cell parameter such as `width`.

Dimensions also work on rectangles imported from GDS. If the technology file configures pin layers, imported pin labels become fields you can refer to.

## Instance

Select the target scope in the hierarchy sidebar, press <kbd>I</kbd>, and type a cell invocation. Move the preview and click to place it. Placement stays active so you can drop more copies; press <kbd>Esc</kbd> to stop.

## Reading the canvas

- Solid edges are fixed by constraints.
- Dashed edges have at least one coordinate that is still free.
- Dragging a free coordinate updates its `*i` argument in the source.
- If a hand-written constructor has no `*i` arguments, the first drag adds them.
