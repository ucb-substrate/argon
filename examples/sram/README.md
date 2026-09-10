# Native SRAM layout example

This example is a source-native Argon port of SRAM22's column-peripheral layout
construction and its representative 64-word by 24-bit SRAM assembly without
the control-logic macro. It does not import GDS. The cells construct
devices, implants, wells, contacts, power straps, signal routing, taps, and
boundary geometry in Argon source. The fixed foundry DFF and sense-amplifier
macro artwork is kept in separate native source modules.

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

Or select an SRAM organization directly. The four arguments are write-mask
granularity, mux ratio, number of words, and data width:

```sh
arc run --cell 'col_peripherals_for_sram(sram_params(8, 4, 64, 24))' --gds
```

The practical interface follows SRAM22's load-based device and decoder-chain
sizing. Common mux ratios 4 and 8 and write-mask granularities 1, 2, 4, and 8
are supported when the word and mask counts divide evenly.

Generated `.gds` and `.bin` files live under the ignored `target/` directory
and can be deleted after viewing. SRAM22 reference layouts are used only outside this example during port
development. They are not repository inputs, runtime dependencies, or
correctness infrastructure.
