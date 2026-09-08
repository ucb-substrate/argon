---
title: GUI workspace
description: How the visual editor is laid out and how it relates to the source.
sidebar_label: Workspace
---

# GUI workspace

The GUI shows the cell compiled from your current Neovim buffers. It's built for looking at layout and editing it spatially; the source stays the single source of truth.

## Regions

| Region | Purpose |
| --- | --- |
| Canvas | Solved geometry, selection, dimensions, and placement previews. |
| Tool strip | Select, rectangle, polygon, path, dimension, and instance tools. |
| Hierarchy sidebar | Scopes and nested instances. Selecting one sets where new geometry is inserted. |
| Layer sidebar | The drawing layer and per-layer visibility. |
| Neovim command line | Where the GUI asks for text, such as a cell invocation or a name. |

## Open a cell

Press <kbd>O</kbd>, type an invocation such as `inverter(1200., 2000., 4)`, and press <kbd>Enter</kbd>. The arguments are parsed and type-checked in the library's scope.

What you open is an invocation, not just a cell name. If you change a cell's signature, reopen it with matching arguments.

## Two-way editing

1. Neovim sends the current source to the analyzer.
2. The analyzer parses, checks, and compiles the open cell.
3. The GUI draws the latest valid result.
4. When you draw or drag on the canvas, the GUI asks the analyzer for a source edit.
5. Neovim applies the edit, and the loop starts again.

A compile error keeps the last good layout on screen unless the open cell itself no longer compiles. Press <kbd>Ctrl</kbd>+<kbd>Shift</kbd>+<kbd>D</kbd> to see diagnostics.
