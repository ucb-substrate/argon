# Native SRAM layout example

This example is a native port of SRAM22's column-peripheral layout construction,
its replica-timed control logic, and its SRAM assembly. It does not import GDS.
Rust/Substrate performs electrical sizing, architecture derivation, and
replica-load partitioning in the `argon-sram-sizing` crate. Argon receives a
concrete parameter record and only constructs devices, implants, wells,
contacts, power straps, signal routing, taps, and boundary geometry. The fixed
foundry DFF, sense-amplifier, and standard-cell artwork is kept in separate
native source modules.

Generate the complete guarded SRAM example with:

```sh
arc run --cell 'example_sram()' --gds
```

Open `target/argon.gds` and select `example_sram`. Use `example_sram_inner()`
when the untranslated inner layout is more convenient for debugging.
`example_no_control_logic()` and `example_no_control_logic_inner()` build the
same assembly without the control logic.

The assembly is parametric. [`logic/floorplan.ar`](logic/floorplan.ar) places
each block relative to the blocks before it, as SRAM22 does, from their
measured bounding boxes, and splits the height beside the column peripherals
between the column decoder and the enable buffers.
[`logic/routing.ar`](logic/routing.ar) draws every signal route from the
blocks' exported ports on a 680 nm routing grid.
[`logic/power.ar`](logic/power.ar) rings the grid with banks of alternating
VDD and VSS rails, runs each block's supply rails to a bank, and fills the
remaining free tracks with straps. Even tracks carry VDD and odd tracks VSS,
and like supplies get a via wherever they cross. A strap is drawn only where
it reaches another strap or a feeder. Block bounding boxes, the signal routes,
and the feeders keep the straps away.

The assembly is built for a mux ratio of 4 or 8 and any word count the sizer
produces. The row decoder is a tree of AND2 and AND3 stages, split as SRAM22
splits it, and each stage ties its supply rails to the stage above it; the
column decoder is a single AND2 or AND3 stage. At all twenty-two of SRAM22's
published mux-4 and mux-8 macros, from `sram22_64x22m4w22` to
`sram22_2048x64m8w8`, its block positions, signal routes, and macro size match
the published layout. KLayout's sky130 decks find no DRC errors, LVS matches
the macro's netlist, pin names included, and every well and substrate tap
reaches the supply pins through metal.

From this directory, generate the example column peripherals with:

```sh
arc run --cell 'example_col_peripherals()' --gds
```

Open `target/argon.gds` and select `example_col_peripherals`.

Generate the control logic on its own with:

```sh
arc run --cell 'example_control_logic()' --gds
```

The control logic in [`logic/control`](logic/control) places SRAM22's
`sky130_fd_sc_hs` standard cells and SVT inverters exactly where SRAM22 does
and keeps SRAM22's pin locations. Its delay-chain lengths come from the
generated record. It accepts any chain of at least two inverters, with at
least six in the decoder replica and nine in the wordline pulse chain, which
covers every length the sizer produces. The chains shift the cells after them
in three of the five rows, and the right-side pins follow the cell's bounding
box as in SRAM22. All top-level interconnect is hand-assigned on a 460 nm
grid: met1 runs horizontally at y = 205 + 460k and met2 vertically at
x = 350 + 460j, with 320 nm wires, so wires on distinct grid lines always meet
the 140 nm spacing rule. Tracks are fixed per net. A route to a moving pin
takes its column from the pin and steps past the few fixed columns that other
nets cross in that row, so no two nets share a grid line where their wires
overlap. As in SRAM22, each row rail is its own `vdd` or `vss` pin; the SRAM
joins the rails with met2 feeders to its left power bank.

The checked-in [`generated.ar`](generated.ar) contains the Rust-sized 64-by-24
record used by the example cells. Regenerate it from the workspace root with:

```sh
cargo run --release -p argon-sram-sizing -- \
  8 4 64 24 sram examples/sram/generated.ar
```

The four numeric arguments are write-mask granularity, mux ratio, number of
words, and data width. You can generate another practical organization into
the same file and open `example_col_peripherals()` to inspect it. The complete
assembly needs a mux ratio of 4 or 8; for other mux ratios the generator
stops, since their column decoders are not built yet.

The Rust sizer follows SRAM22's load-based device and decoder-chain sizing.
It also derives the control logic's delay chains as SRAM22 does, from the RC
time constants of logical-effort-sized enable buffers, write-mask drivers, and
row decoder, using the same gradient-descent gate sizing, so its chain
lengths and write-mask driver widths match SRAM22 exactly; its test checks the
chain lengths of SRAM22's published macros.
Mux ratios 4 and 8 and practical power-of-two word counts are supported when
the word and write-mask counts divide evenly. Its test sweep covers mux ratios
4 and 8, mask granularities 1, 2, 4, and 8, word counts from 32 through 256,
and data widths from 8 through 64.

Generated `.gds` and `.bin` files live under the ignored `target/` directory
and can be deleted after viewing. SRAM22 reference layouts are used only outside this example during port
development. They are not repository inputs, runtime dependencies, or
correctness infrastructure.
