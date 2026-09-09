# SRAM rendering verification — 2026-09-08

## Pan freeze reproduced

Sampling the running native stress-project window caught the main thread in
`CanvasElement::paint`, painting and then freeing a huge flattened rectangle
list. The process's sampled peak physical footprint was 44.3 GB. A missing
covered tile view could fall back to direct geometry, and density was checked
only after that traversal. Recentring also discarded tiles still needed by the
last presented camera.

The earlier ready-tile benchmark did not exercise this failure: it installed
completed tiles and disabled background prefetch. Its paint timings were not
representative of cold loading or fast pans that outran tile generation.

## Changes

- UI collection and BVH queries have a shared work budget. Cold or excessive
  traversal starts background rendering without submitting partial geometry.
  Background density decisions use the same budget to avoid returning to the
  expensive direct path.
- Recentring preserves tiles covering the camera actually on screen until its
  replacement is ready. Pending work retains a complete image when available.
- The bottom-right rendering indicator covers classification, tile rendering,
  and the overview. Camera changes cannot strand a completed decision task.
- Layer-specific BVHs group interleaved SRAM geometry. Splits follow object
  centers, and index construction partitions instead of repeatedly sorting.
- Subpixel features and tightly spaced parallel wires coalesce at the interaction
  raster's pixel size. Visible gaps remain separate; zooming in restores the
  original geometry. Coarse layer footprints do not require detailed child BVHs.
- Retained tiles outside a newly centered prefetch ring are included in painting,
  as well as coverage checks. Zoom holds the last camera until the exact requested
  LOD covers the viewport, avoiding intermediate camera/LOD handoffs.

## Pan-stop and zoom flicker regression

The full editor reproduced a status-bar layout feedback loop: the idle canvas
was 798×696.5 pixels, and showing “Rendering” changed it to 798×692. Finishing
rendering hid the label, resized the canvas again, invalidated the tile field,
and restarted rendering. Isolated canvas benchmarks could not detect it.

The activity row now retains its intrinsic layout size while idle, with its
contents invisible and its animation disabled. Tests cover default, 14 px, and
20 px fonts. A regular full-editor test paints after every background task during
pan bursts and zooms, checks at most one camera handoff after input stops, and
verifies that additional idle paints do not restart rendering.
The stress lifecycle also exposed a stranded activity flag when a completed
density decision reused ready raster tiles without starting another worker.
Decision completion now recomputes activity from the remaining tasks in that case.

The unchanged 16-SRAM fixture also passes the full-editor lifecycle with the
canvas fixed at 798×692 inside a 1200×800 window. Across 32 pan frames, input plus
paint had a 0.569 ms median and 0.879 ms maximum. Two stopped pans beyond the
cached field each held the old camera, advanced once when the new view arrived,
and finished the complete background field in 0.95–1.00 seconds. Zooms also had
one camera handoff. Initial background work took 793 ms; the warm 1× viewport
raster took 95.8 ms. These timings use a smaller canvas than the isolated-canvas
comparison below and should not be compared as another speedup.

A later early-navigation blank frame was traced to the first direct-to-raster
handoff. Direct painting could show a complete view and start a raster worker,
then clear the canvas on the next paint before any tile existed. The viewer now
retains the bounded direct frame at its original camera while the first tiles
load, including when a pan leaves the cached area. A 3,000-shape regression
paints before the first tile and between every task, asserting that no complete
frame is replaced by a blank placeholder. Hidden-layer and scope changes still
invalidate retained geometry.

## Full stress-project lifecycle

Fixture: unchanged `target/render-stress`, a 4×4 bank of
`sram22_1024x32m8w8.gds` macros, compiled to `target/argon.bin` (11,905 unique
cells). Release profile, arm64 macOS 15.7.9, 1200×800 logical-pixel canvas,
repository SKY130 technology.

`local_project_pan_lifecycle` starts with cold indexes and no tiles. It exercises
normal background workers, rapid keyboard-path and middle-mouse pans up to
several viewports, reversals before tiles finish, and zooms at 1×, 4×, and 16× fit.
It checks complete presented coverage, camera catch-up, bounded UI geometry,
and completion of the rendering indicator.

| Measurement | Before LOD changes* | After |
| --- | ---: | ---: |
| Warm full-view raster, 1× fit | 2,148 ms | 189 ms |
| Warm full-view raster, 4× fit | 293 ms | 178 ms |
| Warm full-view raster, 16× fit | 26 ms | 26 ms |
| Initial background field and overview | 4,797 ms | 848 ms |
| Cold UI paint | 0.056 ms | 0.050 ms |
| Maximum pan input + paint, 30 frames | 2.15 ms | 2.03 ms |

\* This baseline already includes the pan-freeze guard. It isolates the LOD
changes rather than attempting another unbounded native paint.

The full zoomed-out raster is about 11.4× faster. Before the hierarchy changes
described below, loading/deserializing and preparing the compiled artifact took
32.9 seconds in this run, separate from rendering time. The test's peak physical
footprint was 1.71 GiB;
this is not a native-window memory comparison. These are individual runs on
one machine, not statistical performance guarantees.

The full prefetched field can still take around 1.4–1.7 seconds to finish at
closer zooms. That work runs in the background; it is separate from pan input
and paint latency.

## Incremental rectangle updates

`local_sram_edit_pipeline` uses a persistent `IncrementalCompiler`, instantiates
the same 4×4 bank, and adds one `met1.drawing` rectangle to the top-level cell.
The GDS import cache has one miss initially and one hit after editing; the edit
does not re-import the SRAM. Measurements below are release builds on the same
machine with a full 1200×800 editor window (798×692 canvas).

The first optimization prepared geometry once per unique scope within a
snapshot, reducing hierarchy preparation from 28.829 s to 0.970 s. Updates now
share metadata across snapshots, use persistent path maps, and transmit only
changed cells. Geometry dependencies are checked before reusing bounds or
renderer indexes; numerical IDs alone never prove that a cell is unchanged.

| Edit stage | Previous snapshot preparation | Incremental update |
| --- | ---: | ---: |
| GUI hierarchy preparation | 970 ms | 25.7 ms |
| Serialized update | 433.7 MB full graph | 160,414 bytes, 2 changed cells |
| Encode delta | — | 0.30 ms |
| Decode and materialize delta | — | 1.74 ms |
| Incremental compilation | 37.7 ms | 38.7 ms |
| Apply prepared update on UI | — | 6.62 ms |
| First updated frame after application | 821 ms with index rebuilding* | 111 ms |
| Render field complete / indicator idle | 872 ms with index rebuilding* | 163 ms |
| Maximum UI paint during edit | — | 0.77 ms |

\* Measured after adding delta transport but before reusing unchanged rendering
indexes. The frame and idle times exclude compilation, serialization, and
hierarchy preparation. Summing the measured stages gives roughly 183 ms to the
first updated frame; this excludes live socket/LSP scheduling and native GPU
submission. The retained previous layout stays visible throughout the update.
These are individual runs, not statistical performance guarantees.

The full fixture's 202,555 paths and 11,930 scope addresses match the expanded
reference before and after the edit, including every bound and layer. The slow
reference check is excluded from preparation timing. Regular tests also cover
changed children, removals, renamed scopes, hidden paths, and shared instances.
A raster regression changes a child without changing its numeric ID, checks that
its parent's dependent index is invalidated, and compares reused-index rendering
with a fresh render pixel for pixel.

New connections start with a full snapshot. Each connection keeps an acknowledged
base for deltas, preserves cell ordering, removes absent cells, and falls back to
a full snapshot if the receiver does not have the base. Source spans and other
cell metadata travel with changed cells. Initial hierarchy preparation still
takes about 1.03 s; structural hierarchy edits use the full path builder.

The disappearing-layout regression was caused by treating a newly allocated
compiler cell ID as a presentation change, clearing the retained image. Geometry
edits now retain it and stage replacement tiles until they cover the viewport.
Zoom staging also waits for the density decision, preventing a new raster from
being immediately replaced by direct geometry. The full-editor regression paints
after every task through 56 small zoom steps crossing the raster/direct threshold
and through rectangle addition/removal. Restoring the old ID comparison makes
that test fail with a disappearing layout.

The original 512×32 SRAM also passes the edit lifecycle (a 4×4 bank) and the
pixel-stability checks when opened directly at 1× through 128× fit. Its edit's
hierarchy preparation was 22.9 ms and every intermediate paint stayed complete.

Placed rectangles now keep a separate preview until the displayed frame contains
their compiled geometry. A source-edit acknowledgement or the arrival of a new
snapshot alone does not retire it. Rectangle submission awaits the editor
asynchronously so navigation and painting continue while that reply is pending.
Tool previews stay outside retained layout frames to avoid caching uncommitted
geometry.

The direct and raster placement regressions paint between background tasks,
including a delayed source-edit reply, tool changes, panning, and zooming. They
verify continuous preview coverage through the frame handoff, both reply/snapshot
arrival orders, distinct names for rapid placements, rejection of one placement
without removing another, and undo before a compiled placement is displayed.
The same placement lifecycle passes on the original 512×32 SRAM fixture.

Cached rectangle outlines also preserve the compiler's per-edge constraint
state. Previously the raster builder hard-coded solid borders, so unconstrained
placements lost their dashes at the frame handoff. A pixel regression covers
free, partially constrained, constrained, reversed, rotated, and reflected
rectangles with hollow, solid, and patterned fills. It verifies unchanged pixels
through a clipped pan, so tile boundaries cannot restart the dash pattern.
Restoring the hard-coded solid style makes that regression fail.

## 512×32 correctness

Fixture: `~/Downloads/sram22_512x32m4w8.gds`, SHA-256
`c60a1a6d9e3e3053807b20473976b0af3315ff6c37030485c70cb4e108a96c78`.
The original Downloads file was not modified. Tests cover both a wrapper
instance and opening the SRAM directly, at 1× through 128× fit:

- Nonblank rasters and pixel-identical repeated rendering.
- Zero changed overlapping interior pixels after a 32-pixel pan.
- Stable renderer and camera across select/rectangle/dimension tools.
- Complete retained coverage while replacement tiles load, followed by camera
  advancement when the new tiles arrive.

First raster timing can include lazy index construction; the benchmark prints
warm raster timing separately. Sparse close-up views retain direct geometry.

140 regular GUI tests pass, including regressions for
cold pan deferral, displayed-tile retention, layer and wire aggregation, visible
gaps, hidden descendants, hierarchy cutoffs, invalidation, and source lookup.
The release application builds successfully. Formatting and whitespace checks
pass.

Eight Neovim/analyzer integration tests also pass after removing Argon's
message-area spinner. Standard LSP compilation progress still begins and ends
correctly for Fidget and other progress UIs. Restart Neovim to unload an already
running copy of the old spinner module. The local stress-project launcher uses
`nvim-local.sh` to prepend this checkout to Neovim's runtime path, so it tests the
changed plugin while keeping the normal user configuration and Fidget. A
headless launch with that configuration confirmed the local plugin and Fidget
load without the removed spinner module.

GUI startup now registers asynchronously and acknowledges the connection before
compiling or preparing its initial snapshot. Previously, an initial source error
could make the GUI wait for registration while the analyzer waited for a GUI
snapshot callback, preventing the window constructor from returning. The startup
regression holds the first error update until registration completes; restoring
the old handshake makes it time out. A GPUI test also verifies that foreground
tasks keep running while the registration reply is pending.

Canvas measurements use GPUI's test platform and production rendering code.
They do not measure Metal submission, monitor refresh, or visually certify the
native window. Computer Use access to the test application was not approved.
The running old application must be restarted to load the rebuilt binary.

See [benchmark commands](README.md#sram-rendering-regression).
