---
title: Schematic view
description: "Viewing a cell's schematic in the GUI: symbols, wires, flylines, and navigation."
---

# Schematic view

The GUI shows three views of the open cell: its layout, its schematic, and its symbol. Press <kbd>V</kbd> to cycle through the layout, schematic, and symbol views, <kbd>Shift</kbd>+<kbd>V</kbd> to cycle back, or click a view in the control at the right end of the tool strip. Each view keeps its own camera and selection, and all three show the same cell.

The schematic view draws exactly what the source stores, as [placement and routes](/language/schematic#placement-and-routes) on the calls that create each element and connection. It never routes a wire itself, and the source stays the only persistent state.

## What is drawn

| Item | Drawn as |
| --- | --- |
| Instance | Its cell's [symbol](/language/schematic#symbols), under the instance's placement. A cell that draws no symbol gets a generated box with its ports as pins. |
| Device | The symbol of its kind, with its key parameters beside it. A `Subckt` device is a box with numbered pins. |
| Port | A pin glyph with the port's name, where a `pub` signal is placed. |
| Net label | A tag with the net's name, where a private signal is placed. |
| Wire | The route of a `connect`, from the pin of its first signal through its corners to the pin of its second. |
| Junction | A dot where three or more wire ends of one net meet, or a wire ends on another wire of its net. |
| Flyline | A dashed line between parts of a net that no wire joins. |
| Conflict | A red cross where items of different nets touch, which shows a connection the netlist does not have. |

Elements and wires the GUI cannot edit, because they come from a loop or a function or their placement is computed, are drawn in a muted color. The properties panel says why.

### Placement

A placed element is drawn where its `x` and `y` put it. An unplaced instance or device goes to a slot of a grid anchored at the origin, by its creation order, so placing one element never moves the others. Unplaced ports line the left and right of that grid, with supplies on top and grounds at the bottom.

### Wires and flylines

Two wires of one net that overlap are drawn once, with a junction where they part. A route that ends at an unplaced signal, or at a port that has no pin in the child's symbol, is not drawn; the properties panel of its net says why.

A connection that no wire draws, such as a `connect` without a `route` or a device's terminal list, is shown as a flyline between the nearest pins of the parts it joins. Parts that show the net's name, with a port pin or a label, count as joined already. View → Flylines hides them for the session.

### Zoomed out

Far out, each element is drawn as its bounding box and text is left out.

## Navigation

| Action | How |
| --- | --- |
| Pan | Drag with the middle mouse button, or the arrow keys. |
| Zoom | Scroll, or <kbd>Cmd/Ctrl</kbd>+<kbd>+</kbd> and <kbd>-</kbd>. |
| Fit | <kbd>F</kbd>. An empty view leaves the camera where it is. |
| Select | Click an element, pin, wire, port, or label. Where things overlap, clicking again cycles through them. <kbd>Shift</kbd>+click adds to the selection. The selection's source is revealed in Neovim. |
| Descend | <kbd>E</kbd> with an instance selected, or double-click it. Every view then shows that cell. |
| Return | <kbd>Shift</kbd>+<kbd>E</kbd>. |
| Symbol view | <kbd>V</kbd>. See [the symbol view](/gui/symbol). |

Descending pushes a step onto the path from the open cell to the cell on screen. The path is kept by instance name across compiles, so an edit in Neovim never throws you back to the top; if a step no longer exists, the view returns to the deepest one that does and says so.

## Entering a schematic

A schematic can be drawn from an empty cell without typing in Neovim beyond the invocation prompts. Each tool writes a statement at the end of the cell body, naming `std::schematic` by its full path, so the cell needs no `use` declarations. Every gesture is one undo step.

| Tool | Key | Gesture | What the source gets |
| --- | --- | --- | --- |
| Instance | <kbd>I</kbd> | Opens `:Argon sinst ` in Neovim. Once the invocation is entered, its symbol follows the pointer; a click places it, and the tool stays active for the next one. <kbd>R</kbd> and <kbd>Shift</kbd>+<kbd>R</kbd> turn it first. | `pub let mos0 = std::schematic::inst(mos(true, 1., 2), x=8, y=-4);` |
| Device | <kbd>M</kbd> | Opens `:Argon device Nmos ` in Neovim; the device palette in the right sidebar opens it with another kind. The device then follows the pointer like an instance. | `pub let nmos0 = std::schematic::device(std::schematic::DeviceKind::Nmos, std::schematic::signals(4), "nch", w=1., l=0.15, x=0, y=-4);` |
| Wire | <kbd>W</kbd> | Click a pin, port, label, or wire to start, click empty canvas for each corner, and click a pin or wire to finish. The segment to the pointer bends once; <kbd>/</kbd> flips which way. Double-click empty canvas, or press <kbd>Enter</kbd>, to finish at a new net label after naming it. <kbd>Backspace</kbd> drops the last click. | `std::schematic::connect(a, nmos0.g, route=[(-6, -4)]);` |
| Wire a flyline | <kbd>W</kbd> | Hover a flyline to preview a wire with one bend between its pins, <kbd>/</kbd> flips the bend, and a click writes it. | `route=[(-6, -4)]` on the `connect` that made the flyline, or a new `connect` when a terminal list made it |
| Port | <kbd>P</kbd> | Type the name in the right sidebar, then click to place it. Clicking a pin or wire places the port there and connects them. | `pub let a = std::schematic::Signal(x=-6, y=0);` then `std::schematic::connect(a, nmos0.g);` |
| Net label | <kbd>L</kbd> | Type the name, then click a pin or wire to place the label on it. | `let n1 = std::schematic::Signal(x=3, y=2);` then `std::schematic::connect(n1, nmos0.d);` |
| Delete | <kbd>Delete</kbd> | Deletes the selection. | The statement of each element or wire, the `connect` statements that name a deleted element, and `std::schematic::Signal()` in place of a deleted signal in a device's terminal list |

Instances are named after their cell and devices after their kind, with a number that makes the name unique in the cell. A port or label name you type must be a valid identifier that the cell does not bind yet.

A wire that ends on another wire connects to that wire's nearer end and runs along it, so the drawing shows a branch without rewriting the other wire. Wiring to an unplaced element also writes that element's `x=` and `y=`, so it stays where it was drawn.

A pin that nothing at the end of the cell body names, because its element was created in a loop or a function, cannot start or end a wire. The GUI says so and reveals the element's source.

Deleting an element that something else still reads, such as another `let`, a struct literal, a `connect` in a loop, or a parent reading the field through an instance, is refused. The properties panel lists those references, and clicking one reveals it in Neovim.

New elements and wires are drawn as ghosts until the recompiled result arrives. An edit is refused, with a message, when the schematic changed in Neovim since the gesture began or an earlier edit is still being applied.

### Renaming, copying, and exporting

| Gesture | What happens |
| --- | --- |
| Edit the name in the properties panel | The binding is renamed with every reference to it, in every file, through the navigation index: the `connect` statements and terminal lists that name it, and the parents that read it through an instance. A net is renamed by renaming its port or label. The new name must be a free identifier. |
| <kbd>Cmd</kbd>+<kbd>C</kbd>, <kbd>Cmd</kbd>+<kbd>V</kbd> | Copies the selection's statements and pastes them with the corner of their bounds at the pointer. Bindings get fresh names when theirs are taken, placements and routes move with the paste, and copied wires and terminal lists name the copies. A copied wire to something that was not copied keeps that end. |
| File → Export Netlist… | Writes the netlist of the cell on screen and the cells under it to a file you choose: Spectre for a `.scs` file, SPICE otherwise. |

## Editing placement and wires

Every gesture is one source edit, applied through Neovim as one undo step. The GUI writes exactly what you did, and the source stays the only record of the drawing; what you see is always the compiled result, with the gesture drawn over it until the recompiled result arrives.

| Gesture | What the source gets |
| --- | --- |
| Drag an element, port, or label | `x=` and `y=` on the call that created it, and the routes of its wires rewritten so they stay orthogonal |
| <kbd>R</kbd>, <kbd>Shift</kbd>+<kbd>R</kbd> | `angle=` or `reflect=` on the selection, with its wires rewritten |
| <kbd>Shift</kbd>+click | Adds to the selection, so several elements move together; a wire between two of them moves with them |
| Drag a wire segment sideways | The corners at both ends of every route that runs along it, with a corner added where it ends at a pin |
| Remove Route (properties panel, or the View menu) | Removes `route=`, leaving the connection as a flyline |
| Fix Positions (View menu) | `x=` and `y=` for every unplaced element and port, so nothing moves until you move it |

When a pin moves, the corner next to it follows the move so the end segment keeps its direction, and the next segment stretches. A straight wire, or one whose end segment is diagonal, gets a corner.

An element or wire that comes from a loop or a function, or whose placement or route is computed, is locked: it is drawn muted, and the properties panel says why. Locked routes are never rewritten; their ends simply follow the pins.

A gesture is refused, with a message, when the schematic changed in Neovim since you started it or an earlier edit is still being applied. Refused edits leave nothing behind.

## Sidebars

The left sidebar lists the cell's instances and devices, then its nets with the ports first. Clicking an element selects it and brings it into view, and double-clicking an instance descends into it. Clicking a net highlights it on the canvas.

The right sidebar shows the device palette, the name prompt of the port and label tools, and the properties of the selection: its name, cell or invocation, model and parameters, position and orientation, and the net of a selected pin; for a wire, its net and route. A highlighted net lists its conflicts and the routes that could not be drawn.

Values with a source are editable in place: click one, or press <kbd>Q</kbd> for the first, type the new text, and press <kbd>Enter</kbd>. An instance's invocation and a device's model and parameters rewrite that argument of the call; a position moves the item with its wires; a route replaces the wire's corners. <kbd>Esc</kbd> leaves the value as it was.

## Errors and empty cells

A cell with no schematic shows a hint instead of a drawing. A cell with execution errors is drawn without the elements and connections that did not evaluate. While the workspace has compile errors, the view keeps drawing the last good result under the error banner.

A layout-only cell shows the hint in the schematic view, and a schematic-only cell shows one in the layout view.
