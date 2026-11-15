# Native SRAM layout example

This example is a native port of SRAM22's column-peripheral layout construction,
its replica-timed control logic, and its SRAM assembly. It does not import GDS.
Rust/Substrate performs electrical sizing, architecture derivation, and
replica-load partitioning in the `argon-sram-sizing` crate. The
`argon-sram-routing-wasm` crate exposes `argon-greedy-router`, a port of
SRAM22's greedy grid router and power-strap placer. Argon receives a concrete
parameter record and constructs devices, implants, wells, contacts, power
straps, signal routing, taps, and boundary geometry, calling the router for
the control logic's signal routes and the macro's power straps. The fixed
foundry DFF, sense-amplifier, and standard-cell artwork is kept in separate
native source modules.

Both crates compile to WebAssembly modules that `Argon.toml` names under
`[wasm]`. Build them once from the workspace root:

```sh
cargo build --release --target wasm32-unknown-unknown \
  -p argon-sram-sizing-wasm -p argon-sram-routing-wasm
```

Then generate the complete guarded SRAM example with:

```sh
arc run --cell 'example_sram()' --gds
```

Open `target/argon.gds` and select `example_sram`. Use `example_sram_inner()`
when the untranslated inner layout is more convenient for debugging.
`example_no_control_logic()` and `example_no_control_logic_inner()` build the
same assembly without the control logic.

To build another organization without regenerating the record, call the
sizer from Argon:

```sh
arc run --cell 'example_sized_sram(8, 8, 1024, 32)' --gds
```

The arguments are write-mask granularity, mux ratio, number of words, and data
width. [`sizing.ar`](sizing.ar) declares the sizer as an `extern` function, and
[`logic/router.ar`](logic/router.ar) declares the router's functions.

The blocks share a few building blocks in
[`columns/util.ar`](columns/util.ar): `mos`, a row of MOS devices drawn as
sky130's MOS generator draws them; `gate`, a NAND or folded inverter built
from it; and `contact`, the largest via array of a given rule between two
shapes. Each block places its devices as rotated instances, centered or
aligned by their bounding boxes, and routes from the ports the instances
export. Tap cells read their neighbors' exported contacts and stripes from
construction instances rather than recomputing them.

The assembly is parametric. [`logic/floorplan.ar`](logic/floorplan.ar) places
each block relative to the blocks before it, as SRAM22 does, from their
measured bounding boxes, and splits the height beside the column peripherals
between the column decoder and the enable buffers.
[`logic/routing.ar`](logic/routing.ar) draws every signal route from the
blocks' exported ports on a 680 nm routing grid.
[`logic/power.ar`](logic/power.ar) gathers the router's blockages (block
shapes, port boxes, and the signal routes) and every block's supply pins,
then calls the strap placer, which fills the free met1 and met2 tracks of a
680 nm grid with straps. Even tracks carry VSS and odd tracks VDD; a strap
gets a via wherever it crosses a strap or supply pin of its own net. The
macro extends the straps that reach the inner layout's edge out to its guard
rings.

The assembly is built for a mux ratio of 4 or 8 and any word count the sizer
produces. The row decoder is a tree of AND2 and AND3 stages, split as SRAM22
splits it, and each stage exports its supply pins to the strap placer; the
column decoder is a single AND2 or AND3 stage. At all twenty-two of SRAM22's
published mux-4 and mux-8 organizations, from `sram22_64x22m4w22` to
`sram22_2048x64m8w8`, a flat XOR on every layer finds no difference between
the macro and the layout SRAM22's current generator produces. KLayout's sky130
DRC deck finds no errors at 64 by 24 and 512 by 64.

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
box as in SRAM22. The greedy router connects the cells on a 460 nm grid, met1
horizontal and met2 vertical with 320 nm wires, making SRAM22's routing
requests in SRAM22's order so the routes come out the same. The rows' supply
rails are merged into the block's `vdd` and `vss` pins, which the macro's
straps reach like any other block's.

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
