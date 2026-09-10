---
title: Cell management
description: Open, create, and rename cells from the GUI.
---

# Cell management

You can open, create, and rename cells from the GUI. Creating and renaming edit the source through Neovim.

## Open a cell

Press <kbd>O</kbd> and type a full invocation, arguments included. Opening only changes what the canvas shows; it doesn't touch the source.

## Create a cell

Choose **New Cell…** from the File menu, click **New cell** in the toolbar, or press <kbd>Cmd</kbd>+<kbd>N</kbd>. Type a name and confirm. Argon inserts an empty cell into the current module and opens it.

The name must be a valid identifier, not a keyword, and not already declared in the module. The source buffer must also be editable through Neovim.

The insertion is an ordinary buffer edit, so it marks the buffer modified and can be undone.

## Rename the open cell

Choose **Rename Cell…** from the File menu, click **Rename cell** in the toolbar, or press <kbd>Cmd</kbd>+<kbd>Shift</kbd>+<kbd>R</kbd> while a source-defined cell is open. Type the new name and confirm. The declaration and every reference that resolves to it are updated.

Rename is semantic. Comments, strings, fields, functions, and unrelated cells that happen to share the name are left alone. Imported GDS cells and other read-only declarations can't be renamed.

Once the edit is applied, Argon reopens the same invocation under the new name. If the name is invalid or already taken, nothing changes and an error is shown.

:::info
Creating and renaming need a running Neovim and analyzer, because the GUI never writes to source files directly.
:::
