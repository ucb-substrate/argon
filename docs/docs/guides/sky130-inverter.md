---
title: SKY130 inverter
description: Assemble SKY130 transistor and tap cells into a parameterized inverter.
---

# SKY130 inverter

This guide assembles an inverter layout from the transistor and tap cells in Argon's SKY130 library. It assumes you've worked through [Getting started](/guides/getting-started/installation).

## Set up the library

Clone the Argon repository, which contains the SKY130 library, and create a library next to it:

```bash
git clone https://github.com/ucb-substrate/argon.git
arc new inverter
cd inverter
rm tech.toml
```

Point the manifest at the SKY130 technology file and add the SKY130 library as a dependency:

```toml title="Argon.toml"
name = "inverter"
tech = "../argon/pdks/sky130/sky130.tech.toml"

[dependencies]
sky130 = "../argon/pdks/sky130"
```

Coordinates are in nanometers, and the dimensions in this guide satisfy the SKY130 design rules. The guide uses two cells from `sky130`:

| Cell | Draws | Public fields |
| --- | --- | --- |
| `fet1v8(nfet: Bool, w: Float, nf: Int)` | A 1.8 V transistor with `nf` gate fingers, each `w` wide | `poly_bbox`, `li1_bbox`, `sdm` |
| `tap(ntap: Int, w: Float, h: Float)` | A substrate tap if `ntap` is `0`, a well tap if it's `1` | `tap`, `li1`, `sdm` |

`poly_bbox` bounds the gate fingers, and `li1_bbox` bounds the `li1` columns over the sources and drains. `sdm` is a cell's implant, its outermost rectangle. A tap's `li1` is its contact strip.

## Place the devices

Replace `lib.ar` with an empty cell:

```argon title="lib.ar"
use sky130::{fet1v8, tap};
use std::layout::{array, center_rects, crect, rect};

cell inv(nw: Float, pw: Float, nf: Int) {
}
```

The `use` lines import everything the guide needs. Open the editor:

```bash
argone .
```

Click the canvas, press <kbd>O</kbd>, and open `inv(1000., 1600., 4)`.

Place the devices in the order below. For each, press <kbd>I</kbd> for the Instance tool, type the invocation, click to place it, and press <kbd>Esc</kbd>. The GUI binds the new instance to `inst0`. Rename it in Neovim before placing the next device, since later invocations refer to earlier names.

| Name | Invocation | Place it |
| --- | --- | --- |
| `nmos` | `fet1v8(true, nw, nf)` | Anywhere |
| `pmos` | `fet1v8(false, pw, nf)` | Above the NMOS |
| `ptap` | `tap(0, nmos.sdm.w - 260., 800.)` | Below the NMOS |
| `ntap` | `tap(1, pmos.sdm.w - 260., 800.)` | Above the PMOS |

A tap's implant extends 130 nm past each side of the tap, so a width of `sdm.w - 260.` matches it to the transistor's implant.

The GUI writes each position as `xi` and `yi`, which are only starting values. Fix the NMOS at the origin and line up the left edges by adding to `inv`:

```argon
eq(nmos.x, 0.);
eq(nmos.y, 0.);
eq(pmos.x, 0.);
eq(ptap.sdm.x0, nmos.sdm.x0);
eq(ntap.sdm.x0, pmos.sdm.x0);
```

The PMOS and taps still have dashed edges because their heights are free.

## Stack the devices

From bottom to top, the stack is p-tap, NMOS, PMOS, n-tap. For each gap below, press <kbd>D</kbd> for the Dimension tool, click the two implant edges, then click where the label should go. Type the gap and press <kbd>Enter</kbd>.

| Lower edge | Upper edge | Gap |
| --- | --- | --- |
| Top of the p-tap implant | Bottom of the NMOS implant | `100.` |
| Top of the NMOS implant | Bottom of the PMOS implant | `1130.` |
| Top of the PMOS implant | Bottom of the n-tap implant | `70.` |

The 1130 nm gap leaves room for three `li1` rails between the transistors. Every edge is now solid. In source, the dimensions amount to:

```argon
eq(nmos.sdm.y0 - ptap.sdm.y1, 100.);
eq(pmos.sdm.y0 - nmos.sdm.y1, 1130.);
eq(ntap.sdm.y0 - pmos.sdm.y1, 70.);
```

The remaining steps add code to the end of `inv`.

## Draw the n-well

```argon
rect("nwell.drawing", x0=pmos.sdm.x0 - 180., x1=pmos.sdm.x1 + 180., y0=pmos.sdm.y0 - 180., y1=ntap.sdm.y1 + 180.);
```

The well encloses the PMOS and the n-tap.

## Connect the gates

```argon
pub let a = rect("li1.drawing", x0=nmos.poly_bbox.x0 - 90., x1=nmos.poly_bbox.x1 + 90., h=170.);
eq(a.y0 - nmos.li1_bbox.y1, pmos.li1_bbox.y0 - a.y1);

let finger = crect(layer="poly.drawing", x0=0., y0=0., w=150., h=pmos.poly_bbox.y1 - nmos.poly_bbox.y0);
let fingers = array(finger, nf, 430., 0.);
eq(fingers.x0, nmos.poly_bbox.x0);
eq(fingers.y0, nmos.poly_bbox.y0);

center_rects(rect("poly.drawing", w=a.w, h=270.), a);
center_rects(rect("npc.drawing", w=a.w + 40., h=370.), a);
let licon = crect(layer="licon1.drawing", x0=0., y0=0., w=170., h=170.);
center_rects(array(licon, nf, 430., 0.), a);
```

- `a` is the input: an `li1` rail centered between the transistors.
- [`array`](/language/std-layout#array) draws `nf` copies of a template at the 430 nm finger pitch and returns their bounds. Here it extends each gate finger from the NMOS to the PMOS. The template is a [construction rectangle](/language/std-layout#crect); only its layer and size are copied, but its position must still be fixed.
- A poly strip joins the fingers under `a`, and one `licon1` contact per finger connects them to it. `npc` surrounds the contacts. [`center_rects`](/language/std-layout#center-rects) centers each on `a`.

## Connect the output

Each transistor's `li1` columns alternate source, drain, source, and so on, so the drains are the odd columns. Add a helper after `inv` that straps every other column:

```argon title="lib.ar"
/// Draws `n` li1 straps from `y0` to `y1` on every other device column, starting at `x0`.
fn straps(n: Int, x0: Float, y0: Float, y1: Float) {
    let strap = crect(layer="li1.drawing", x0=0., y0=0., w=170., h=y1 - y0);
    let bbox = array(strap, n, 860., 0.);
    eq(bbox.x0, x0);
    eq(bbox.y0, y0);
}
```

Then add to `inv`:

```argon
let out_n = rect("li1.drawing", x0=nmos.li1_bbox.x0 + 430., x1=a.x1 + 340., y1=a.y0 - 170., h=170.);
let out_p = rect("li1.drawing", x0=out_n.x0, x1=out_n.x1, y0=a.y1 + 170., h=170.);
pub let out = rect("li1.drawing", x1=out_n.x1, w=170., y0=out_n.y0, y1=out_p.y1);
let drains = (nf + 1) / 2;
straps(drains, out_n.x0, nmos.li1_bbox.y0, out_n.y1);
straps(drains, out_p.x0, out_p.y0, pmos.li1_bbox.y1);
```

`out_n` and `out_p` run below and above the input rail, and `out` joins them on the right. The straps tie each transistor's drains to the nearer rail.

## Connect the supplies

The even columns are sources. Strap them to the taps:

```argon
let sources = nf / 2 + 1;
straps(sources, nmos.li1_bbox.x0, ptap.li1.y0, nmos.li1_bbox.y1);
straps(sources, pmos.li1_bbox.x0, pmos.li1_bbox.y0, ntap.li1.y1);
```

The inverter is done. Open `inv(1000., 1600., 1)` or `inv(1000., 1600., 5)` to see the straps and contacts follow `nf`.

## Check and export

```bash
arc fmt
arc check
arc run --cell 'inv(1000., 1600., 4)' --gds
```

This writes `target/argon.gds`. The finished `lib.ar` is in `examples/sky130_inverter` in the Argon repository.
