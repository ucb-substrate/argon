---
title: Hierarchy and export
description: Place cells inside other cells, check the library, and write GDS.
---

# Hierarchy and export

A cell becomes reusable once you place it inside another cell.

## Compose a parent cell

Add this after `inset_rect`:

```argon
cell triple_rect() {
    let first = inst(inset_rect(200., 200.), x=0., y=0.);
    let second = inst(inset_rect(240., 180.), x=300., y=0.);
    let third = inst(inset_rect(160., 260.), x=650., y=0.);
}
```

Save, then open `triple_rect()` from the canvas. The hierarchy sidebar lists the three instances under the root scope.

You can also place instances from the GUI: press <kbd>I</kbd>, type a cell invocation, and click to place it in the selected scope. Placement stays active so you can drop several copies; press <kbd>Esc</kbd> when you're done.

## Format and check

```bash
arc fmt
arc check
```

`arc fmt --check` reports unformatted files without changing them, which is useful in CI.

## Export

```bash
arc run --cell 'triple_rect()'
```

This writes the compiled cell to `target/argon.bin`. Add `--gds` to also write `target/argon.gds`:

```bash
arc run --cell 'triple_rect()' --gds
```

:::note
Quote the cell expression so the shell doesn't interpret the parentheses.
:::

That's the end of the getting-started guide. Other guides are listed on the [Guides](/guides) page. From here, the [language reference](/language/overview) covers the rest of the language, and the [GUI](/gui/workspace) and [tools](/tools/overview) books cover the editor and the command line in detail.
