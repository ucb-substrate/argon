---
title: Your first cell
description: Create a library and open a cell in the GUI.
---

# Your first cell

In this step you create a library, open it in Neovim and the GUI, and draw two rectangles.

## Create a library

```bash
arc new tutorial
cd tutorial
```

This creates three files:

```text
tutorial/
├── Argon.toml  # library manifest
├── tech.toml   # units, layers, and display styles
└── lib.ar      # Argon source
```

`lib.ar` starts with an empty `top()` cell. A cell is a layout definition you can call, like a parameterized block.

## Open the editor

From the library directory:

```bash
argone .
```

This starts Neovim, the analyzer, and the GUI window. Neovim owns the source; the analyzer compiles it and sends the result to the GUI.

Click the canvas and press <kbd>O</kbd>. The command line opens with `:Argon openCell` filled in. Type `top()` and press <kbd>Enter</kbd>.

## Replace the starter cell

Replace the contents of `lib.ar` with:

```argon title="lib.ar"
cell inset_rect() {
}
```

Press <kbd>O</kbd> again and open `inset_rect()`. You don't need to save first: the analyzer compiles the buffer as you type, and the canvas updates a moment after you stop.

## Draw two rectangles

1. Pick the `met2` layer in the layer sidebar.
2. Press <kbd>R</kbd> and click two opposite corners.
3. Pick `met1` and draw a larger rectangle around the first.
4. Press <kbd>Esc</kbd> to return to selection mode.

Look at `lib.ar` in Neovim: each rectangle is now a `rect` call. The GUI writes into the buffer, not the file, so these edits can be undone like any other. The calls use initial values such as `x0i` and `y1i`, which is what lets you drag unconstrained edges later.

## Check the project

```bash
arc fmt
arc check
```

Next, [add constraints](./constraints) to replace the sketched positions with relationships.
