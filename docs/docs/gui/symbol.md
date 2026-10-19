---
title: Symbol view
description: Viewing and drawing a cell's symbol in the GUI.
---

# Symbol view

The symbol view shows the [symbol](/language/schematic#symbols) of the cell on screen, at its origin over the grid. Press <kbd>V</kbd> from the schematic view or <kbd>Shift</kbd>+<kbd>V</kbd> from the layout view, or click Symbol in the view control.

Every pin is drawn with its port's name, whether or not its `pin` call asked for a label, so a pin can always be identified. `inst_name` shows the instance name of the last descend step, or `X?` at the open cell, and `param` shows the value the cell was compiled with.

## Generated symbols

A cell that draws no symbol shows its generated box, locked. Its pins are the cell's ports: supplies on top, grounds on the bottom, and the rest split between the left and right sides. The drawing tools are disabled until the box is made the cell's own.

**Create Symbol**, in either sidebar, writes the box as `std::symbol` calls with the same pins, so the wires of every parent stay where they are. The pins get `label=true`, so their names move beside the pins. For a cell `inv` with ports `a` and `out`, the source gets:

```argon
std::symbol::rect(x0=0., y0=-4., x1=4., y1=0.);
std::symbol::inst_name(x=0., y=1.);
std::symbol::text("inv", x=0., y=-5.);
std::symbol::pin(a, x=0, y=-2, label=true);
std::symbol::pin(out, x=4, y=-2, label=true);
```

Create Symbol is refused when the cell already draws a symbol, or when a port cannot be named from the cell body, and says which ports those are.

## Drawing

Each tool writes one `std::symbol` call at the end of the cell body, naming the module by its full path. Shapes and text snap to quarter grid units, and pins to whole grid units, so a parent's wires meet them on the grid. `Float` coordinates are written with the fewest digits that read back as the same value, always with a point, and keywords at their defaults (`fill=false`, `size=1.`, `align=Left`) are left out.

| Tool | Key | Gesture | What the source gets |
| --- | --- | --- | --- |
| Line | <kbd>L</kbd> | Click each point. Double-click or <kbd>Enter</kbd> finishes; <kbd>Backspace</kbd> drops the last point. | `std::symbol::line([(-4., 0.), (-2., 0.)]);` |
| Polygon | <kbd>P</kbd> | Click each point. Double-click or <kbd>Enter</kbd> closes it. | `std::symbol::polygon([(-2., 2.), (-2., -2.), (2., 0.)]);` |
| Rectangle | <kbd>R</kbd> | Click one corner, then the opposite one. | `std::symbol::rect(x0=0., y0=-4., x1=4., y1=0.);` |
| Circle | <kbd>C</kbd> | Click the center, then a point on the circle. | `std::symbol::circle(x=2.25, y=0., r=0.25);` |
| Arc | <kbd>A</kbd> | Click the center, the start, and the end. The arc runs counterclockwise, and its angles snap to 5°. | `std::symbol::arc(x=0., y=0., r=1., start=90., end=270.);` |
| Text | <kbd>T</kbd> | Type the text in the right sidebar, then click to place it. The sidebar also offers the instance name and each cell parameter. | `std::symbol::text("VDD", x=1., y=3.);`, `std::symbol::inst_name(x=2., y=2.);`, or `std::symbol::param("w", w, x=2., y=-2.);` |
| Pin | <kbd>N</kbd> | The first port without a pin is chosen, and <kbd>Tab</kbd> cycles through the others; clicking a missing port in the left sidebar picks it. A click places the pin with its name, and the tool moves on to the next port without a pin. | `std::symbol::pin(a, x=-4, y=0, label=true);` |

New items are drawn as ghosts until the recompiled result arrives.

## Editing

In the select tool (<kbd>S</kbd>), drag an item or a pin to move it, or drag one of a selected item's handles to reshape it: a point of a line or polygon, a corner of a rectangle, the radius of a circle, or an end of an arc. <kbd>Shift</kbd>+click adds to the selection. Clicking an item that a function drew, such as one of `std::symbol::nmos`'s, selects the whole group, and the properties panel names the call that drew it.

| Gesture | What the source gets |
| --- | --- |
| Move or reshape | Every coordinate of the item rewritten: `x=` and `y=`, the corners, the radius, the angles, or the points of a line or polygon |
| <kbd>X</kbd>, <kbd>Shift</kbd>+<kbd>X</kbd> | The selection rotated by 90° or mirrored left to right about its center, snapped to the grid, with every coordinate rewritten. Mirroring reverses an arc and swaps `Left` and `Right` alignment, so text stays beside what it labels. |
| Edit a value in the properties panel | That argument rewritten or inserted: `fill`, `size`, `align`, the text, or a pin's `label` |
| <kbd>Delete</kbd>, <kbd>Backspace</kbd> | The item's statement, or for a group, the statement of the call that drew it |

Moving a pin moves the wire ends in every parent that places the cell, since routes end at pins. The properties of a pin say so.

<kbd>Cmd</kbd>+<kbd>C</kbd> copies the selected items, and <kbd>Cmd</kbd>+<kbd>V</kbd> pastes them with the corner of their bounds at the pointer, snapped to quarter grid units. Pins and the items a function drew are not copied, since a port has one pin and a group comes from its call.

An item drawn by a function or placed by an expression is locked: it is drawn muted, it has no handles, and the properties panel says why. A group can only be deleted when the call that drew it is a whole statement in the cell body.

## Sidebars

The left sidebar lists the cell's ports in port order, each marked as pinned or missing. Clicking a pinned port selects its pin and brings it into view; clicking a missing port, which is marked in red since it has no position on the canvas, picks it for the pin tool. A generated symbol shows the Create Symbol button here.

The right sidebar offers the text tool's placeholders, the name prompt of the text tool, and the properties of the selection: the item's kind and bounds, every argument of its call, the group a function drew, and whether it is locked. Each argument is editable in place: click it, or press <kbd>Q</kbd> for the first, type the new value, and press <kbd>Enter</kbd>.

## Navigation

Panning, zooming, and fitting work as in the [schematic view](/gui/schematic#navigation). <kbd>Shift</kbd>+<kbd>V</kbd> returns to the schematic view, <kbd>V</kbd> goes on to the layout view, and <kbd>Esc</kbd> drops the shape being drawn, leaves the tool, or clears the selection.
