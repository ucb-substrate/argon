---
title: Modules and manifests
description: Split source across files and declare dependencies.
---

# Modules and manifests

Modules split a library's source across files. `Argon.toml` names the library and lists what it depends on.

## File modules

Declare a child module with `mod`:

```argon title="lib.ar"
mod utils;

cell top() {
    let spacing = utils::default_spacing();
}
```

`mod utils;` loads `utils.ar`. A module can also be a directory containing a `mod.ar`.

Paths start with `std::` for the standard library, `lib::` for the root of the current library, or a dependency's name for that dependency.

## Imports

`use` brings an item from another module into scope, so it can be named without its path. The item is bound under its last segment, or under the name after `as`:

```argon title="lib.ar"
mod utils;

use std::layout::{inst, rect};
use utils::default_spacing;
use utils::pad as metal_pad;

cell top() {
    let left = inst(metal_pad(100.), x=0., y=0.);
    let right = inst(metal_pad(100.), x=100. + default_spacing(), y=0.);
    rect("met2", x0=left.x, y0=0., x1=right.x, y1=20.);
}
```

A group imports several items from one module: `use utils::{default_spacing, pad as metal_pad};` is the same as the two separate `use` lines above. Each entry in the braces is a single name with an optional `as`, a trailing comma is allowed, and an empty group `{}` is an error.

- A `use` applies only to the module that declares it.
- Imports and declarations share one namespace per module, so a module can't import a name it also declares. Use `as` to import the item under another name.
- A `use` can't bind the name of a [built-in function](/language/builtins), such as `eq` or `float`.

## Standard library

`std` is available in every library without a manifest entry. It is split into modules:

| Module | Contents |
| --- | --- |
| [`std`](/language/std) | `max`, `min`, `Option`, and sequence helpers |
| [`std::layout`](/language/std-layout) | Geometry constructors, `inst`, `bbox`, `dimension`, and rectangle helpers |
| [`std::schematic`](/language/std-schematic) | `Signal`, `connect`, `device`, `inst`, and `DeviceKind` |

Call a standard library item by its full path, such as `std::max(a, b)` or `std::layout::rect("met1")`, or import it with `use`. `Option`, `Some`, and `None` are in scope in every module without an import.

## Library manifest

`Argon.toml` names the library and points at its technology file, dependencies, and GDS imports:

```toml
name = "my-library"
tech = "tech.toml"

[dependencies]
devices = "../devices"

[gds]
"macros::sram" = "gds/sram.gds"
```

Paths are relative to the manifest. Each GDS import becomes a zero-argument cell at the given module path.

## Project layout

```text
my-library/
├── Argon.toml
├── tech.toml
├── lib.ar
├── utils.ar
└── nested/
    └── mod.ar
```

[`arc check`](/tools/arc#arc-check) parses, resolves, and type-checks the whole library.
