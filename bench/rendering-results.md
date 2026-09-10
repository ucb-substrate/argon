# Rendering regression coverage

The rendering tests generate their layouts in temporary directories and use the
repository's technology files. No external GDS files or precompiled layout
artifacts are required.

Run the GUI suite in release mode:

```sh
cargo test -p argone --lib --release
```

## Navigation and frame retention

The full editor tests paint between background tasks during pans, zooms, tool
changes, and source edits. They check that a complete layout remains visible
while replacement tiles render, that the camera advances when coverage is ready,
and that rendering activity does not resize the canvas or restart idle work.

```sh
cargo test -p argone --lib --release editor::canvas::render_regression
```

Generated layouts include dense instance arrays, interleaved layers, small
parallel wires, and sparse geometry. Tests cover bounded UI traversal, subpixel
aggregation, visible gaps, hierarchy cutoffs, hidden descendants, and invalidation
of indexes when a child cell changes. Reused indexes are compared with fresh
renders pixel for pixel.

## Rectangle placement and constraints

Direct and raster placement tests hold source-edit replies while painting,
panning, zooming, and switching tools. They check that previews remain visible
until compiled geometry is displayed, and cover rapid placements, rejected edits,
undo, and either reply/snapshot arrival order.

```sh
cargo test -p argone --lib --release placed_rectangle_stays_visible
cargo test -p argone --lib --release compiled_rectangle_rasters
```

Pixel tests preserve dashed edges for unconstrained coordinates, solid edges for
constrained coordinates, and dash alignment through clipped pans. They include
partially constrained, reversed, rotated, and reflected rectangles with several
fill styles.

## Hierarchy and updates

Hierarchy tests compare prepared scope paths, bounds, layers, and visibility
against an expanded reference using generated source hierarchies. Analyzer tests
serialize and reconstruct cell updates with independent sender/receiver caches.

```sh
cargo test -p argone --lib --release editor::hierarchy
cargo test -p argon-analyzer --lib --release compilation_delta
```

GPUI tests exercise production CPU rendering and editor code. They do not measure
native GPU submission or monitor refresh.
