---
title: Writing a layout generator
description: Build a parameterized SKY130 latch with its schematic, mirror it, route it, and check a sweep of it with DRC and LVS.
sidebar_label: Writing a layout generator
---

# Writing a layout generator

This guide builds a SKY130 cross-coupled latch: two CMOS inverters, each driving the other's input, mirrored about a vertical line so that both outputs see the same layout. Along the way it shows what most transistor-level generators do:

- describe the schematic in the same cell as the layout;
- place transistors from the [SKY130 library](/language/sky130) with constraints rather than coordinates;
- mirror a half, share diffusion across the axis, and fill implants;
- add taps and a well;
- route with met1 bars, met2 wires, and vias;
- label pins;
- check a sweep of parameters with DRC and LVS, and trace each violation back to source.

The finished library is [`examples/sky130_latch`](https://github.com/ucb-substrate/argon/tree/main/examples/sky130_latch). The guide assumes you know the basics from [Getting started](/guides/getting-started/installation), and the [quick reference](/language/reference) lists every function it calls.

## The library

A library's manifest names the SKY130 technology file and depends on the SKY130 library, with paths relative to the manifest:

```toml title="Argon.toml"
name = "sky130_latch"
tech = "../../pdks/sky130/sky130.tech.toml"

[dependencies]
sky130 = "../../pdks/sky130"
```

## The top cell

`latch` takes each transistor's finger width in nm and finger count. Its body first describes the schematic: four ports, then the four transistors. It then places the two halves of the layout, adds taps and a well, routes, and labels the ports. Each step calls a helper that the sections below go through.

```argon title="lib.ar"
use sky130::{mos, tap};
use std::layout::{center_rects, crect, inst, max_array, mirror_pair, rect, text};
use std::schematic::{DeviceKind, Signal, device};

/// Two cross-coupled CMOS inverters, mirrored about x = 0. `wn` and `wp` are
/// finger widths of at least 420 nm, and `nfn` and `nfp` finger counts.
cell latch(wn: Float, nfn: Int, wp: Float, nfp: Int) {
    pub let outp = Signal();
    pub let outn = Signal();
    pub let vdd = Signal();
    pub let vss = Signal();
    // Inverter A drives outp from outn, and inverter B drives outn from outp.
    fet(true, outp, outn, vss, wn, nfn);
    fet(false, outp, outn, vdd, wp, nfp);
    fet(true, outn, outp, vss, wn, nfn);
    fet(false, outn, outp, vdd, wp, nfp);

    // B is drawn right of the axis and A is its mirror image, so their
    // sources meet at x = 0.
    let halves = mirror_pair(inverter(wn, nfn, wp, nfp), 0.);
    let b = halves[0];
    let a = halves[1];
    eq(b.x, 0.);
    eq(b.y, 0.);
    implant("nsdm.drawing", a.n.sdm, b.n.sdm);
    implant("psdm.drawing", a.p.sdm, b.p.sdm);

    // Taps below the NMOS gates and above the PMOS gates, 205 from the gate
    // bars so the gates' poly contacts clear the taps (licon.9, licon.14).
    let half = std::max(b.n.sdm.x1, b.p.sdm.x1);
    let ptap = inst(rail(0, half), x=0.);
    eq(ptap.m1.y1 + 205., b.n.g.y0);
    let ntap = inst(rail(1, half), x=0.);
    eq(ntap.m1.y0, b.p.g.y1 + 205.);
    // The n-well encloses p-diffusion and the n-tap by 180, 50 past their implants.
    rect("nwell.drawing", x0=-half - 50., x1=half + 50., y0=b.p.sdm.y0 - 50., y1=ntap.sdm.y1 + 50.);

    tie(a.n.s, b.n.s, ptap.m1);
    tie(a.p.s, b.p.s, ntap.m1);
    // Each output's via at x = ±430 runs on met2 to the other inverter's gate
    // trunk at x = ∓860, the two at different heights so that they cross.
    via1(430., mid_y(b.n.d));
    wire(430., mid_y(b.n.d), -860., mid_y(b.n.d));
    via1(-430., mid_y(b.p.d));
    wire(-430., mid_y(b.p.d), 860., mid_y(b.p.d));

    pin("outp", a.out);
    pin("outn", b.out);
    pin("vss", ptap.m1);
    pin("vdd", ntap.m1);
}
```

- **Ports.** Each `pub let … = Signal()` is a port, in declaration order. `fet` adds a transistor to the schematic.
- **Halves.** `mirror_pair` places two copies of `inverter` that mirror each other about x = 0, and returns the unreflected copy first. `b`, right of the axis, is pinned to the origin. Its fields read through the instance, so `b.n.sdm` is the implant of its NMOS, in the latch's coordinates.
- **Taps.** `rail` draws a tap under the NMOS row and one over the PMOS row. Each is placed with one `eq` on its y, and centered with `x=0.`.
- **Routing.** `tie` joins each row's sources to its tap. Each output then runs on met2 to the other inverter's gates.

## One inverter

The `mos` cell from the SKY130 library draws a transistor with its gate fingers joined under a met1 bar `g`, and with met1 bars `s` and `d` over its source and drain columns. Each bar is 320 tall, enough for a via1. Its gate bar is below the diffusion. Placed with `reflect=true`, the cell flips about the x axis, so the PMOS's gate is on top and its drain bar faces the NMOS's.

```argon title="lib.ar"
/// One inverter of the latch, right of the axis x = 0 with its sources toward
/// it: the NMOS at the bottom with its gate below, and the PMOS above it with
/// its gate on top.
cell inverter(wn: Float, nfn: Int, wp: Float, nfp: Int) {
    pub let n = inst(mos(true, wn, nfn), y=0.);
    pub let p = inst(mos(false, wp, nfp), reflect=true);
    eq(n.cols.x0, middle(nfn));
    eq(p.cols.x0, middle(nfp));
    // The drain bars reach 190 past their diffusions, which must be 520 apart:
    // 340 from n-diffusion to the n-well (difftap.9), which encloses p-diffusion by 180.
    eq(p.d.y0, n.d.y1 + 140.);
    // One met1 block joins the drains and reaches the output's via at x = 430.
    pub let out = rect("met1.drawing", x0=std::min(std::min(n.d.x0, p.d.x0), 430. - 130.), x1=std::max(n.d.x1, p.d.x1), y0=n.d.y0, y1=p.d.y1);
    // A met2 trunk at x = 860 joins the gates.
    hook(n.g, 860.);
    hook(p.g, 860.);
    wire(860., mid_y(n.g), 860., mid_y(p.g));
}
```

Neither transistor gets a coordinate except the NMOS's `y`. Each transistor's x comes from where its first contact column `cols` starts, and the PMOS's y from the gap between the drain bars. The gap comes from two SKY130 rules. N-diffusion must stay 0.34 µm from the n-well (difftap.9), and the well must enclose p-diffusion by 0.18 µm, so the diffusions are at least 0.52 µm apart. Each drain bar reaches 0.19 µm past its diffusion, which leaves 0.14 µm between them.

`out` is one met1 rectangle over both drain bars, so it joins them and is the inverter's output. `std::min` and `std::max` make it cover the wider of the two bars. `hook` extends each gate bar to x = 860, where a met2 wire joins the two gates.

## The schematic

`fet` adds one transistor. `device` takes the transistor's terminals in the order drain, gate, source, body, then the model's parameters: `w` is the total width of all fingers in µm, `l` the gate length, and `nf` the number of fingers. Layout and schematic come from the same parameters, so they can't disagree.

```argon title="lib.ar"
/// A transistor in the schematic, with its body on its source.
fn fet(nfet: Bool, d: Signal, g: Signal, s: Signal, w: Float, nf: Int) {
    let total = w * (nf as Float) / 1000.;
    if nfet {
        device(DeviceKind::Nmos, [d, g, s, s], "sky130_fd_pr__nfet_01v8", w=total, l=0.15, nf=nf);
    } else {
        device(DeviceKind::Pmos, [d, g, s, s], "sky130_fd_pr__pfet_01v8", w=total, l=0.15, nf=nf);
    }
}
```

LVS merges a transistor's parallel fingers into one device and compares its total width and finger count with the schematic's. The body goes on the source here because each source is a supply.

## Mirrored halves

Every transistor of the right half starts its first contact column at `middle(nf)`, so its mirror image's first column lands on top of it. That first column is a source, so the two halves share one source column on the axis. With a single finger, the two gates' contacts would collide there, so single-finger transistors keep their diffusions apart instead. Their diffusions are 0.27 µm apart (difftap.3), and their poly gate bars at least 0.21 µm (poly.2).

```argon title="lib.ar"
/// Where a device with `nf` fingers starts right of the axis: its first contact
/// column centered on it, so a mirrored copy shares that column, unless single
/// fingers' gate contacts would collide.
fn middle(nf: Int) -> Float {
    if nf == 1 { 265. } else { -85. }
}

/// Fills the implant `layer` across a mirrored pair's implants `l` and `r`,
/// which would otherwise leave a notch between single fingers (nsd.2, psd.2).
fn implant(layer: String, l: Rect, r: Rect) {
    rect(layer, x0=l.x0, x1=r.x1, y0=r.y0, y1=r.y1);
}
```

Two separate implant rectangles of the same type closer than 0.38 µm leave a notch (nsd.2, psd.2), so `implant` draws one rectangle across both halves.

## Taps and the well

A tap ties the substrate or the n-well to a supply. `rail` places a `tap` from the SKY130 library and covers its local interconnect with met1. It contacts the two with mcons:

- `max_array` draws as many mcons as fit, 170 square on a 360 pitch, in the width it's given.
- `center_rects` centers the array on the bar, rounding to the 5 nm grid when the margin is odd.

```argon title="lib.ar"
/// A tap strapped with met1, centered on x = 0, with its implant spanning ±`half`.
cell rail(ntap: Int, half: Float) {
    let w = 2. * half - 260.;
    let t = inst(tap(ntap, w, 500.), x=-w / 2., y=0.);
    pub let sdm = t.sdm;
    pub let m1 = rect("met1.drawing", x0=t.li1.x0, y0=t.li1.y0, x1=t.li1.x1, y1=t.li1.y1);
    let mcon = crect(layer="mcon.drawing", x0=0., y0=0., w=170., h=170.);
    center_rects(m1, max_array(mcon, m1.w - 120., 170., 360., 360.));
}
```

The top cell keeps each tap 205 from its row's gate bar. That keeps the gates' poly contacts 0.19 µm from the taps (licon.14), and the NMOS gate's 0.11 µm from the p-tap's P+ implant (licon.9). The n-well reaches 50 past the PMOS implant and the n-tap's implant, which encloses their diffusions by 0.18 µm.

## Routing

The routing uses met1 for the bars the transistors already have, met2 for wires between them, and a via1 wherever a wire meets a bar. Wires run on tracks 430 apart:
- x = 0 for the sources;
- ±430 for the outputs;
- ±860 for the gates.

```argon title="lib.ar"
/// Joins a mirrored pair's source bars `l` and `r` across the axis, and drops
/// them to the met1 `rail` on a met2 trunk at x = 0.
fn tie(l: Rect, r: Rect, rail: Rect) {
    let bar = rect("met1.drawing", x0=l.x0, x1=r.x1, y0=r.y0, y1=r.y1);
    via1(0., mid_y(bar));
    via1(0., mid_y(rail));
    wire(0., mid_y(bar), 0., mid_y(rail));
}

fn mid_y(r: Rect) -> Float {
    grid_floor((r.y0 + r.y1) / 2.)
}

/// A via1 centered at (`x`, `y`), with its met2 pad.
fn via1(x: Float, y: Float) {
    rect("via.drawing", x0=x - 75., y0=y - 75., w=150., h=150.);
    rect("met2.drawing", x0=x - 130., y0=y - 160., w=260., h=320.);
}

/// A met2 wire 140 wide from (`x0`, `y0`) to (`x1`, `y1`), either horizontal
/// or vertical.
fn wire(x0: Float, y0: Float, x1: Float, y1: Float) {
    rect("met2.drawing", x0=std::min(x0, x1) - 70., x1=std::max(x0, x1) + 70., y0=std::min(y0, y1) - 70., y1=std::max(y0, y1) + 70.);
}

/// Extends the met1 `bar` to reach `x`, with a via1 there.
fn hook(bar: Rect, x: Float) {
    rect("met1.drawing", x0=std::min(bar.x0, x - 130.), x1=std::max(bar.x1, x + 130.), y0=bar.y0, y1=bar.y1);
    via1(x, mid_y(bar));
}
```

`mid_y` rounds a bar's center down to the grid with `grid_floor`: a center halfway between grid points would put a via off the grid, which is an error. `tie` joins the two halves' source bars across the axis and runs one met2 wire down to the tap. In the top cell, each output's wire runs to the other half's gate track, one at the NMOS drains' height and the other at the PMOS drains', so the two never touch.

## Pins

A port's label is a pin rectangle on a pin layer over a shape of the net on the matching drawing layer, with the port's name as text on the label layer inside it. LVS creates ports only from pins, and matches nets by these names.

```argon title="lib.ar"
/// Labels a port with a met1 pin at the center of `r`.
fn pin(name: String, r: Rect) {
    let p = rect("met1.pin", w=140., h=140.);
    center_rects(r, p);
    text(name, "met1.label", p.x0 + 70., p.y0 + 70.);
}
```

## Check a sweep

A generator is correct only if every point of its parameter range passes DRC and LVS. Check a sweep at once with the `compile` tool of the [headless MCP server](/tools/agents#headless-mode): it compiles each invocation in parallel, reports its size, and with `export` writes each one's GDS and SPICE netlist to its own directory.

```json
{"cells": ["latch(420., 1, 420., 1)", "latch(1000., 2, 2000., 2)", "latch(1000., 4, 1000., 4)"], "export": "target/sweep"}
```

```text
latch(420., 1, 420., 1): 2.35 × 4.3 µm = 10.11 µm², (-1175, -1315) to (1175, 2985); 25 shapes, 4 instances in the top cell → target/sweep/latch-420-1-420-1/
latch(1000., 2, 2000., 2): 2.51 × 6.46 µm = 16.21 µm², (-1255, -1315) to (1255, 5145); 25 shapes, 4 instances in the top cell → target/sweep/latch-1000-2-2000-2/
latch(1000., 4, 1000., 4): 4.23 × 5.46 µm = 23.10 µm², (-2115, -1315) to (2115, 4145); 25 shapes, 4 instances in the top cell → target/sweep/latch-1000-4-1000-4/
```

Without the server, `arc run --cell 'latch(1000., 2, 2000., 2)' --gds --spice` writes one point to `target/argon.gds` and `target/argon.spice`. Then run DRC on each `argon.gds`, and LVS on each `argon.gds` against its `argon.spice`. Include the corners of the range: single fingers, the narrowest widths, rows of different widths, and the largest sizes.

`inspect` reads values from a solved cell, which confirms a spacing without measuring the layout:

```json
{"cell": "latch(1000., 2, 2000., 2)", "exprs": ["b.p.d.y0 - b.n.d.y1", "b.n.g"]}
```

```text
b.p.d.y0 - b.n.d.y1 = 140.
b.n.g = Rect met1.drawing (285, -520) to (575, -200), 290 × 320
```

## Fix what the checks find

Suppose the inverter's drain gap were 60 instead of 140. At `latch(1000., 1, 1000., 2)`, DRC reports two violations:
- difftap.9 in cell `latch`, from (-1.214, 1.13) to (1.214, 1.39) µm;
- poly.2 in cell `inverter`, from (0.49, 1.26) to (0.72, 1.44) µm.

Pass both to `locate`, each with the cell the report names as `gds_cell`, since a violation inside a subcell is in that cell's coordinates:

```json
{"cell": "latch(1000., 1, 1000., 2)", "boxes": [
  {"x0": -1.214, "y0": 1.13, "x1": 1.214, "y1": 1.39, "gds_cell": "latch", "layers": ["diff", "nwell"], "label": "difftap.9"},
  {"x0": 0.49, "y0": 1.26, "x1": 0.72, "y1": 1.44, "gds_cell": "inverter", "layers": ["poly"], "label": "poly.2"}]}
```

```text
difftap.9 (-1.214, 1.13) to (1.214, 1.39) µm in latch:
  diff.drawing (135, 130) to (995, 1130): sky130/lib.ar:15:16; in halves[0] (<argon-std>/layout.ar:114:13, called from lib.ar:20:18) > n (lib.ar:57:17) > fet (sky130/lib.ar:62:15)
  diff.drawing (-995, 130) to (-135, 1130): sky130/lib.ar:15:16; in halves[1] (<argon-std>/layout.ar:115:13, called from lib.ar:20:18) > n (lib.ar:57:17) > fet (sky130/lib.ar:62:15)
  nwell.drawing (-1255, 1390) to (1255, 4065): lib.ar:36:5
poly.2 (0.49, 1.26) to (0.72, 1.44) µm in inverter:
  poly.drawing (490, 0) to (640, 1260): <argon-std>/layout.ar:20:30, called from sky130/lib.ar:12:25; in n (lib.ar:57:17) > fet (sky130/lib.ar:62:15)
  poly.drawing (570, 1440) to (720, 2700): <argon-std>/layout.ar:20:30, called from sky130/lib.ar:12:25; in p (lib.ar:58:17) > fet (sky130/lib.ar:62:15)
```

Paths into the SKY130 library are shortened here. Each shape comes with the line that drew it and the instances leading to it. Both violations involve the NMOS `n`, the PMOS `p`, and the well between them, so the fix is the constraint between the two rows, the drain gap. `locate` names the lines that drew the shapes, not the `eq` that placed them, so look for the constraint that sets the distance it reports.

LVS catches what DRC can't. If both output wires ran at the NMOS drains' height, they would overlap on met2. The layout would still pass DRC, but LVS would report a short between `outn` and `outp`, and one port fewer in the layout than in the schematic.

## Next steps

- [`examples/sky130_strongarm`](https://github.com/ucb-substrate/argon/tree/main/examples/sky130_strongarm) builds a larger circuit the same way, with rows of different devices, more routing tracks, and met3 crossings.
- The [SKY130 library](/language/sky130) page lists its layers and cells.
- [Constraints](/language/constraints) explains underconstrained and inconsistent errors, and [staying on the grid](/language/constraints#staying-on-the-grid).
