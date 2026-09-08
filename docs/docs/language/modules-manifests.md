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
