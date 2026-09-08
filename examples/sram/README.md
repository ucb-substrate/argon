# Native SRAM layout example

This example is a source-native Argon port of SRAM22's column-peripheral layout
construction. It does not import GDS. The parameterized cells construct
devices, implants, wells, contacts, power straps, signal routing, taps, and
boundary geometry in Argon source. The fixed foundry DFF and sense-amplifier
macro artwork is kept in separate native source modules.

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

SRAM22 reference layouts are used only outside this example during port
development. They are not repository inputs, runtime dependencies, or
correctness infrastructure.
