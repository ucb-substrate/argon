# GPUI patterned mask sprites

## Motivation

Argon's retained layout tiles currently store a fully composited RGBA image. Transforming that
image is fast, but it also transforms presentation details that should remain fixed in screen
space, especially the one-pixel slash width and nine-pixel gap used for layer stippling.

The CPU fallback in Argon works around this by retaining two composited style planes and rebuilding
one viewport image after a camera scale change. A GPUI primitive that applies a background through a
texture mask would remove that CPU recomposition and texture upload while retaining the same visual
semantics.

## Proposed API

Add a paint method alongside `Window::paint_image`:

```rust,ignore
pub fn paint_patterned_mask(
    &mut self,
    bounds: Bounds<Pixels>,
    mask: Arc<RenderImage>,
    frame_index: usize,
    fill: Background,
    fill_color: Hsla,
    outline_color: Hsla,
) -> Result<()>;
```

The mask uses two channels:

- red: fill coverage;
- green: outline coverage.

The blue channel is reserved for a future secondary fill or selection mask, and alpha can remain
opaque or mirror the union of the coverage channels. A separate primitive is painted for each
visible layout layer in layer order. This preserves the existing behavior where a higher layer's
stipple covers a lower layer's outline only where the stipple is present.

## Scene primitive

Introduce a `PatternedMaskSprite` next to `PolychromeSprite` and `MonochromeSprite`. It should carry:

- destination bounds and content mask;
- the atlas tile containing the two-channel coverage mask;
- a `Background`, including `PatternSlash`;
- fill and outline colors;
- opacity and stacking order.

The existing polychrome atlas can hold the mask initially, avoiding a new atlas texture kind. A
two-channel atlas format could be evaluated later if memory pressure warrants it.

## Fragment behavior

For each destination fragment:

1. Sample fill and outline coverage from the mask texture.
2. Evaluate the `Background` using the fragment's destination/device position. GPUI already does
   this for `PatternSlash` in quad and path shaders, so its width and interval remain in screen
   pixels regardless of sprite scaling.
3. If outline coverage is nonzero, output the outline color with that coverage.
4. Otherwise output the evaluated fill with fill coverage.
5. Discard a fragment whose resulting alpha is zero.

Outline coverage takes precedence within one layer. Separate layer primitives then use ordinary
source-over composition in z order. Same-layer overlaps remain idempotent because Argon unions them
into a single coverage mask before upload.

Mask sampling may remain linear so transformed geometry edges are antialiased. The stipple must be
evaluated after sampling; changing the existing image sampler to nearest-neighbor would still scale
the stipple period and would not solve the problem.

## Renderer changes

The primitive needs equivalent implementations in every GPUI backend used by the fork:

- Metal in `crates/gpui/src/platform/mac/shaders.metal` and the macOS renderer;
- WGSL in the Blade renderer;
- HLSL in the Windows renderer.

The pattern evaluation should share the same encoded `Background` representation and math as quad
fills. Factoring the existing background evaluation into a common shader helper will prevent the
quad, path, and mask implementations from drifting.

## Argon integration

Argon's navigation raster traversal would produce one compact two-channel mask per visible layer
and tile. During pan or zoom, it would submit the retained masks with transformed bounds instead of
building or scaling a composited RGBA fallback. Native-scale background refinement can continue to
update masks at a more appropriate geometry LOD.

Text, hierarchy labels, selection overlays, and tool previews remain separate GPUI primitives. Tile
retention, center-first scheduling, layer-visibility invalidation, and the atomic inner-3x3 handoff
do not need to change.

## Validation

The GPUI change should include image-based tests or backend snapshots demonstrating that:

- the slash width and interval are unchanged at 0.2x, 1x, and 5x sprite scales;
- translating a mask does not alter the pattern after the gesture completes;
- fill gaps reveal lower-layer outlines;
- higher-layer stipple covers lower-layer outlines;
- same-layer overlaps and corners do not become brighter;
- linear mask sampling antialiases geometry edges without changing interior layer colors.

Performance should be measured with Argon's SRAM layout. The target is one mask draw per visible
layer and tile, no per-wheel texture allocation or upload, and frame times below the display's
16.7 ms budget during continuous navigation.
