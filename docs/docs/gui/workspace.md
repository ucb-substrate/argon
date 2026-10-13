---
title: GUI workspace
description: How the visual editor is laid out and how it relates to the source.
sidebar_label: Workspace
---

# GUI workspace

The GUI shows the cell compiled from your current Neovim buffers. It's built for looking at a cell and editing it spatially; the source stays the single source of truth.

## Views

A cell has three views, shown one at a time in the same window: its layout, its [schematic](/gui/schematic), and its [symbol](/gui/symbol). Switch with the control at the right end of the tool strip, the View menu, <kbd>V</kbd> (the next view, in that order and around to the layout), or <kbd>Shift</kbd>+<kbd>V</kbd> (the previous one). Each view has its own camera, selection, and tools; the sidebars follow the view.

## Regions

| Region | Purpose |
| --- | --- |
| Canvas | The active view: solved geometry with selection, dimensions, and placement previews; or the schematic's symbols and wires; or the cell's symbol. |
| Tool strip | The active view's tools, and the view control. In the layout view: select, rectangle, polygon, path, dimension, and instance. |
| Left sidebar | In the layout view, scopes and nested instances; selecting one sets where new geometry is inserted. In the schematic view, the instances, devices, and nets. In the symbol view, the ports. |
| Right sidebar | In the layout view, the drawing layer and per-layer visibility. In the other views, the properties of the selection. |
| Neovim command line | Where the GUI asks for text, such as a cell invocation or a name. Cell invocations complete with <kbd>Tab</kbd>. |

## Start screen

Until a cell is open, the window shows a start screen in place of the tool strip and sidebars. It has buttons to open a cell or [create one](/gui/cell-management#create-a-cell), and lists the cells of your library, including those in its modules. Click a cell to open it. If the cell has a parameter without a default, the Neovim command line opens with `Argon openCell` and the cell's name filled in, so you can type the arguments.

## Open a cell

Press <kbd>O</kbd>, type an invocation such as `inverter(1200., 2000., 4)`, and press <kbd>Enter</kbd>. The arguments are parsed and type-checked in the library's scope. <kbd>Tab</kbd> completes cell names and, inside the parentheses, the cell's keyword parameters.

What you open is an invocation, not just a cell name. If you change a cell's signature, reopen it with matching arguments.

## Two-way editing

1. Neovim sends the current source to the analyzer.
2. The analyzer parses, checks, and compiles the open cell.
3. The GUI draws the latest valid result.
4. When you draw or drag on the canvas, the GUI asks the analyzer for a source edit.
5. Neovim applies the edit, and the loop starts again.

A compile error keeps the last good layout on screen unless the open cell itself no longer compiles. Press <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>D</kbd> to see diagnostics.
