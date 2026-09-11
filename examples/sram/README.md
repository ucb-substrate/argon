# Native SRAM layout example

This example is a native port of SRAM22's column-peripheral layout construction
and its representative 64-word by 24-bit SRAM assembly without the control
logic macro. It does not import GDS. Rust/Substrate performs electrical sizing,
architecture derivation, and replica-load partitioning in the
`argon-sram-sizing` crate. Argon receives a concrete parameter record and only
constructs devices, implants, wells, contacts, power straps, signal routing,
taps, and boundary geometry. The fixed foundry DFF and sense-amplifier macro
artwork is kept in separate native source modules.

Generate the complete guarded SRAM example with:

```sh
arc run --cell 'example_no_control_logic()' --gds
```

Open `target/argon.gds` and select `example_no_control_logic`. Use
`example_no_control_logic_inner()` when the untranslated inner layout is more
convenient for debugging. The top-level power network uses a deterministic,
blockage-aware alternating mesh; it intentionally does not reproduce SRAM22's
greedy power-strap choices.

From this directory, generate the example column peripherals with:

```sh
arc run --cell 'example_col_peripherals()' --gds
```

Open `target/argon.gds` and select `example_col_peripherals`.

The checked-in [`generated.ar`](generated.ar) contains the Rust-sized 64-by-24
record used by the example cells. Regenerate it from the workspace root with:

```sh
cargo run --release -p argon-sram-sizing -- \
  8 4 64 24 sram examples/sram/generated.ar
```

The four numeric arguments are write-mask granularity, mux ratio, number of
words, and data width. You can generate another practical organization into
the same file and open `example_col_peripherals()` to inspect it. The complete
no-control-logic assembly remains specialized to 64 words by 24 bits; its cell
checks that the generated record has that organization.

The Rust sizer follows SRAM22's load-based device and decoder-chain sizing.
Mux ratios 4 and 8 and practical power-of-two word counts are supported when
the word and write-mask counts divide evenly. Its test sweep covers mux ratios
4 and 8, mask granularities 1, 2, 4, and 8, word counts from 32 through 256,
and data widths from 8 through 64.

Generated `.gds` and `.bin` files live under the ignored `target/` directory
and can be deleted after viewing. SRAM22 reference layouts are used only outside this example during port
development. They are not repository inputs, runtime dependencies, or
correctness infrastructure.
