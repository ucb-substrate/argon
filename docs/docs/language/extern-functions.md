---
title: Extern functions
description: Call Rust functions compiled to WebAssembly from Argon.
---

# Extern functions

An `extern` function is implemented in Rust, compiled to WebAssembly, and called from Argon like any other function. Use one for work that doesn't build geometry, such as electrical sizing, table lookups, or numerical searches. Argon code then draws the result.

## Declaring one

An `extern` declaration names its module and gives the signature, with no body:

```argon title="lib.ar"
struct Ladder {
    pitch: Float,
    widths: [Float],
}

extern "routines" fn ladder(count: Int, total: Float) -> Ladder;

cell top(count: Int, total: Float) {
    let l = ladder(count, total);
    for i in std::range(count) {
        let x0 = (i as Float) * l.pitch;
        std::layout::rect("met1", x0=x0, y0=0., x1=x0 + l.widths[i], y1=100.)!;
    }
}
```

`Argon.toml` maps the module name to the `.wasm` file, relative to the manifest:

```toml
[wasm]
routines = "routines/target/wasm32-unknown-unknown/release/routines.wasm"
```

Parameters and results can be `Int`, `Float`, `Bool`, `String`, `()`, sequences, tuples, and non-generic structs whose fields are those types. Parameters cannot have defaults, and the function cannot have type parameters.

## Writing one in Rust

Build a `cdylib` crate that depends on `argon-guest` and `serde`:

```toml title="routines/Cargo.toml"
[lib]
crate-type = ["cdylib"]

[dependencies]
argon-guest = { git = "https://github.com/ucb-substrate/argon" }
serde = { version = "1", features = ["derive"] }
```

Each function returns `Result<T, E>`, where `T` is serializable and `E` displays as the error message. Invoke `export!` once, with every function:

```rust title="routines/src/lib.rs"
#[derive(serde::Serialize)]
struct Ladder {
    pitch: f64,
    widths: Vec<f64>,
}

fn ladder(count: i64, total: f64) -> Result<Ladder, String> {
    if count <= 0 {
        return Err(format!("count must be positive, found {count}"));
    }
    let pitch = (total / count as f64).floor();
    let widths = (0..count).map(|i| (pitch * (0.3 + 0.4 * i as f64 / count as f64)).floor()).collect();
    Ok(Ladder { pitch, widths })
}

argon_guest::export!(ladder);
```

`Float` is `f64`, `Int` is any integer type, `[T]` is `Vec<T>`, tuples are tuples, and a struct matches an Argon struct by field name. Build the module with:

```sh
cargo build --release --target wasm32-unknown-unknown
```

The editor recompiles when the `.wasm` file changes.

## How calls run

- A call waits until every `Float` in its arguments is solved, as a comparison does. A result is a constant, so geometry that depends on it can't be dragged in the GUI.
- Each call runs in a fresh instance with no access to files, the network, or the clock, so its result depends only on its arguments. Argon reuses the result of an identical earlier call.
- An `Err` result, a panic, or a result that doesn't match the declared type is reported as an error at the call.
- A call runs until it finishes, as any function does. While the editor waits on one for more than a second, the compilation progress names it, and an edit that starts a newer compile stops it.
