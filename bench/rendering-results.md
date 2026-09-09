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

## GUI hierarchy preparation

`local_sram_edit_pipeline` uses a persistent `IncrementalCompiler`, instantiates
the same 4×4 bank, and adds one `met1.drawing` rectangle to the top-level cell.
The GDS import cache has one miss initially and one hit after editing; the edit
does not re-import the SRAM.

| Preparation stage | Before | After |
| --- | ---: | ---: |
| Initial hierarchy | 25.780 s | 0.937 s |
| After adding one rectangle | 28.829 s | 0.970 s |

The edited hierarchy prepares about 29.7× faster. Geometry bounds and layer
usage are computed once per unique scope. A separate path traversal skips
duplicate subtrees while preserving the old traversal's final path bindings,
parent scopes, visibility, and selected scope. Layer lookup also avoids
allocating a name for each already-known layer.

The full stress fixture's 202,555 distinct paths and 11,930 scope addresses
match the previous expanded reference, including every bound and layer. The
reference verification took 25.50 s and is excluded from preparation timing.
A regular test also checks repeated, rotated, and reflected instances under
different parents, parameterized cells, and preservation of hidden paths.

In the verified run, initial compilation took 6.336 s, the incremental edit
37.7 ms, snapshot encoding plus file write 236.6 ms, and file read plus decoding
542.1 ms.

The full-editor stress lifecycle passes after this change. Reading and preparing
its compiled artifact now takes 1.51 s, and pan input plus paint remains under
0.76 ms across 32 frames on GPUI's test platform.

The serialized artifact is 433,654,007 bytes. File timings include I/O and are
not live RPC timings. These measurements exclude applying the prepared state
and rebuilding the render cache, so they are not an end-to-end GUI latency.

The compiler is incremental, but the GUI protocol is not a geometry-delta
protocol: `CompilationSnapshot` contains the entire `CompileOutput`.
The new preparation reuses geometry metadata within each snapshot; it does not
implement a cell-delta transport. Geometry updates still invalidate rendering
indexes and tiles. Sharing metadata across snapshots and invalidating only
changed geometry remain further opportunities to reduce small-edit latency.

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

133 regular GUI tests and 299 compiler tests pass, including regressions for
cold pan deferral, displayed-tile retention, layer and wire aggregation, visible
gaps, hidden descendants, hierarchy cutoffs, invalidation, and source lookup.
The release application builds successfully. Formatting and whitespace checks
pass.

Seven Neovim/analyzer integration tests also pass after removing Argon's
message-area spinner. Standard LSP compilation progress still begins and ends
correctly for Fidget and other progress UIs. Restart Neovim to unload an already
running copy of the old spinner module. The local stress-project launcher uses
`nvim-local.sh` to prepend this checkout to Neovim's runtime path, so it tests the
changed plugin while keeping the normal user configuration and Fidget. A
headless launch with that configuration confirmed the local plugin and Fidget
load without the removed spinner module.

Canvas measurements use GPUI's test platform and production rendering code.
They do not measure Metal submission, monitor refresh, or visually certify the
native window. Computer Use access to the test application was not approved.
The running old application must be restarted to load the rebuilt binary.

See [benchmark commands](README.md#sram-rendering-regression).
