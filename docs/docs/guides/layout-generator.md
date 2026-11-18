---
title: Writing a layout generator
description: Build a parameterized SKY130 latch with its schematic, mirror it, route it, and check a sweep of it with DRC and LVS.
sidebar_label: Writing a layout generator
---

# Writing a layout generator

This guide builds a SKY130 cross-coupled latch: two CMOS inverters, each driving the other's input, mirrored about a vertical line so that both outputs see the same layout. Along the way it shows what most transistor-level generators do:

- describe the schematic in the same cell as the layout;
- place transistors from the [SKY130 library](/language/sky130) with constraints rather than coordinates;
- mirror a half and share diffusion across the axis;
- add tap columns and a well;
- route with met1 bars, met2 wires, and vias the technology file sizes;
- label pins;
- check a sweep of parameters with DRC and LVS, and trace each violation back to source.

The finished library is [`examples/sky130_latch`](https://github.com/ucb-substrate/argon/tree/main/examples/sky130_latch). The guide assumes you know the basics from [Getting started](/guides/getting-started/installation), and the [quick reference](/language/reference) lists every function it calls. A [checklist](#a-density-checklist) at the end lists what keeps a generator's layout small.

## The library

A library's manifest names the SKY130 technology file and depends on the SKY130 library, with paths relative to the manifest:

```toml title="Argon.toml"
name = "sky130_latch"
tech = "../../pdks/sky130/sky130.tech.toml"

[dependencies]
sky130 = "../../pdks/sky130"
```

## The top cell

`latch` takes each transistor's finger width in nm and finger count. Its body first describes the schematic: four ports, then the four transistors. It then places the two halves of the layout, joins each row's sources to its taps, adds a well, routes, and labels the ports. Each step calls a helper that the sections below go through.

```argon title="lib.ar"
use sky130::{mos, tap_m1};
use std::layout::{center_rects, inst, mirror_pair, rect, text, via};
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

    // B is drawn right of the axis and A is its mirror image, so they share
    // a source column at x = 0.
    let halves = mirror_pair(inverter(wn, nfn, wp, nfp), 0.);
    let b = halves[0];
    let a = halves[1];
    eq(b.x, 0.);
    eq(b.y, 0.);

    // A met1 rail over each row's source bars joins them to the row's tap columns.
    let ground = rect("met1.drawing", x0=a.ptap.m1.x0, x1=b.ptap.m1.x1, y0=b.n.s.y0, y1=b.n.s.y1);
    let supply = rect("met1.drawing", x0=a.ntap.m1.x0, x1=b.ntap.m1.x1, y0=b.p.s.y0, y1=b.p.s.y1);
    // The n-well encloses p-diffusion and the n-taps by 180, and is at least 840 tall.
    rect(
        "nwell.drawing",
        x0=a.ntap.tap.x0 - 180.,
        x1=b.ntap.tap.x1 + 180.,
        y0=b.p.sdm.y0 - 50.,
        y1=std::max(b.p.sdm.y1 + 50., b.p.sdm.y0 + 790.),
    );

    // Each output's via at x = ±430 runs on met2 to the other inverter's gate
    // trunk at x = ∓860, the two at different heights so that they cross.
    via1(b.out, 430., mid_y(b.n.d));
    wire(430., mid_y(b.n.d), -860., mid_y(b.n.d));
    via1(a.out, -430., mid_y(b.p.d));
    wire(-430., mid_y(b.p.d), 860., mid_y(b.p.d));

    pin("outp", a.out);
    pin("outn", b.out);
    pin("vss", ground);
    pin("vdd", supply);
}
```

- **Ports.** Each `pub let … = Signal()` is a port, in declaration order. `fet` adds a transistor to the schematic.
- **Halves.** `mirror_pair` places two copies of `inverter` that mirror each other about x = 0, and returns the unreflected copy first. `b`, right of the axis, is pinned to the origin. Its fields read through the instance, so `b.n.sdm` is the implant of its NMOS, in the latch's coordinates.
- **Supplies.** Each half ends its rows in tap columns. One met1 rail over each row's source bars, from one tap column to the other, joins the sources to the taps.
- **Routing.** Each output runs on met2 to the other inverter's gates.

## One inverter

The `mos` cell from the SKY130 library draws a transistor with its gate fingers joined under a met1 bar `g`, and with met1 bars `s` and `d` over its source and drain columns. Each bar is 320 tall, enough for a via1. Its gate bar is below the diffusion, and its source bar is the one nearer the gate. Placed with `reflect=true`, the cell flips about the x axis, so the PMOS's gate is on top and its drain bar faces the NMOS's.

```argon title="lib.ar"
/// One inverter of the latch, right of the axis x = 0 with its sources toward
/// it: the NMOS at the bottom with its gate below, and the PMOS above it with
/// its gate on top. A tap column ends each row.
cell inverter(wn: Float, nfn: Int, wp: Float, nfp: Int) {
    // Each transistor's first contact column, a source, is centered on the
    // axis, where its mirror image shares it. A lone finger's gate contact
    // widens away from the axis, clear of its mirror image's.
    pub let n = inst(mos(true, wn, nfn, gate_side=1), y=0.);
    pub let p = inst(mos(false, wp, nfp, gate_side=1), reflect=true);
    eq(n.cols.x0, -85.);
    eq(p.cols.x0, -85.);
    // The drain bars reach 190 past their diffusions, which must be 640 apart,
    // so each tap column's implant is 380 from the same implant in the other
    // row (nsd.2, psd.2).
    eq(p.d.y0, n.d.y1 + 260.);
    // One met1 block joins the drains and reaches the output's via at x = 430.
    pub let out = rect(
        "met1.drawing",
        x0=std::min(std::min(n.d.x0, p.d.x0), 430. - 130.),
        x1=std::max(n.d.x1, p.d.x1),
        y0=n.d.y0,
        y1=p.d.y1,
    );
    // A met2 trunk at x = 860 joins the gates.
    hook(n.g, 860.);
    hook(p.g, 860.);
    wire(860., mid_y(n.g), 860., mid_y(p.g));

    // Both rows' tap columns sit past the wider row, clear of `out`.
    let edge = std::max(n.sdm.x1, p.sdm.x1);
    pub let ptap = tap_column(false, n.sdm, edge);
    pub let ntap = tap_column(true, p.sdm, edge);
}
```

Neither transistor gets a coordinate except the NMOS's `y`. Each transistor's x comes from where its first contact column `cols` starts, and the PMOS's y from the gap between the drain bars. The gap comes from SKY130 rules. N-diffusion must stay 0.34 µm from the n-well (difftap.9), and the well must enclose p-diffusion by 0.18 µm, so the diffusions are at least 0.52 µm apart. The tap columns need more: each column's implant must be 0.38 µm from the same implant in the other row (nsd.2, psd.2), which puts the diffusions 0.64 µm apart. Each drain bar reaches 0.19 µm past its diffusion, which leaves 0.26 µm between them.

`out` is one met1 rectangle over both drain bars, so it joins them and is the inverter's output. `std::min` and `std::max` make it cover the wider of the two bars. `hook` extends each gate bar to x = 860, where a met2 wire joins the two gates. Both tap columns sit past the wider row, so `out` never reaches over a tap.

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

Every transistor of the right half starts its first contact column at x = -85, centered on the axis, so its mirror image's first column lands on top of it. That first column is a source, so the two halves share one source column, and one diffusion, on the axis. A lone finger's gate contact is 270 wide, wider than its finger, so two of them would collide beside the shared column; `gate_side=1` widens each one away from the axis instead, and their poly stays more than 0.21 µm apart (poly.2). With more fingers, `gate_side` changes nothing, since the gate contact spans the fingers.

Because the halves share diffusion, their implants overlap into one rectangle. Two separate implant rectangles of the same type closer than 0.38 µm would leave a notch (nsd.2, psd.2).

## Taps and the well

A tap ties the substrate or the n-well to a supply. `tap_column` places a `tap_m1` from the SKY130 library, a tap covered with met1 and contacted to it, as tall as a row's diffusion and 0.27 µm past its end (difftap.3). The top cell's rails then reach each column on met1.

```argon title="lib.ar"
/// A tap column as tall as the diffusion under the implant `sdm`, 270 right
/// of the diffusion that ends 130 inside `edge` (difftap.3).
fn tap_column(ntap: Bool, sdm: Rect, edge: Float) -> Any {
    let t = inst(tap_m1(ntap, 250., sdm.h - 260.));
    eq(t.tap.x0, edge - 130. + 270.);
    eq(t.tap.y0, sdm.y0 + 130.);
    t
}
```

A column at each row's end costs about 0.52 µm of width there. A full-width tap rail above and below the cell would instead cost about 0.7 µm of height each, across the whole width. Columns are the cheaper choice unless a cell is much taller than it is wide, as this latch is only with wide PMOS fingers.

The n-well reaches 50 past the PMOS implant, which encloses the PMOS diffusion by 0.18 µm, and 0.18 µm past the n-taps. It is at least 0.84 µm tall (nwell.1).

## Routing

The routing uses met1 for the bars the transistors already have, met2 for wires between them, and a via1 wherever a wire meets a bar. Wires run on tracks 430 apart:
- ±430 for the outputs;
- ±860 for the gates.

```argon title="lib.ar"
fn mid_y(r: Rect) -> Float {
    grid_floor((r.y0 + r.y1) / 2.)
}

/// A met2 pad centered at (`x`, `y`) over the met1 `bar`, with a via1 between them.
fn via1(bar: Rect, x: Float, y: Float) {
    let pad = rect("met2.drawing", x0=x - 130., y0=y - 160., w=260., h=320.);
    via(bar, pad);
}

/// A met2 wire 140 wide from (`x0`, `y0`) to (`x1`, `y1`), either horizontal
/// or vertical.
fn wire(x0: Float, y0: Float, x1: Float, y1: Float) {
    rect(
        "met2.drawing",
        x0=std::min(x0, x1) - 70.,
        x1=std::max(x0, x1) + 70.,
        y0=std::min(y0, y1) - 70.,
        y1=std::max(y0, y1) + 70.,
    );
}

/// Extends the met1 `bar` to reach `x`, with a via1 there.
fn hook(bar: Rect, x: Float) {
    let ext = rect(
        "met1.drawing",
        x0=std::min(bar.x0, x - 130.),
        x1=std::max(bar.x1, x + 130.),
        y0=bar.y0,
        y1=bar.y1,
    );
    via1(ext, x, mid_y(bar));
}
```

`mid_y` rounds a bar's center down to the grid with `grid_floor`: a center halfway between grid points would put a via off the grid, which is an error. `via1` draws a met2 pad and calls [`via`](/language/std-layout#via), which draws the largest array of cuts that fits where the bar and the pad overlap, with the enclosures the technology file gives, centered on the grid. In the top cell, each output's wire runs to the other half's gate track, one at the NMOS drains' height and the other at the PMOS drains', so the two never touch.

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

A generator is correct only if every point of its parameter range passes DRC and LVS. Check a sweep at once with the `compile` tool of the [headless MCP server](/tools/agents#headless-mode): it compiles each invocation in parallel, reports their sizes, and with `export` writes each one's GDS and SPICE netlist to its own directory.

```json
{"cells": ["latch(420., 1, 420., 1)", "latch(1000., 2, 2000., 2)", "latch(1000., 4, 1000., 4)"], "export": "target/sweep"}
```

```text
All 3 cells compile: 2.55–5.13 × 2.83–4.99 µm; 7.22 µm² at latch(420., 1, 420., 1) to 20.47 µm² at latch(1000., 4, 1000., 4); each exported to target/sweep/<name>/.
```

Several cells are summarized in one line, and any that fail are listed with each distinct error once; `"verbose": true` lists every cell. A [`sweep`](/tools/agents#sweeps) argument gives each parameter's values instead, and compiles every combination.

Without the server, `arc run --cell 'latch(1000., 2, 2000., 2)' --gds --spice` writes one point to `target/argon.gds` and `target/argon.spice`. Then run DRC on each `argon.gds`, and LVS on each `argon.gds` against its `argon.spice`. Include the corners of the range: single fingers, the narrowest widths, rows of different widths, and the largest sizes.

`inspect` reads values from a solved cell, which confirms a spacing without measuring the layout:

```json
{"cell": "latch(1000., 2, 2000., 2)", "exprs": ["b.p.d.y0 - b.n.d.y1", "b.n.g"]}
```

```text
b.p.d.y0 - b.n.d.y1 = 260.
b.n.g = Rect met1.drawing (285, -520) to (575, -200), 290 × 320
```

## Fix what the checks find

Suppose the inverter's drain gap were 60 instead of 260. At `latch(1000., 1, 1000., 2)`, DRC reports three violations:
- difftap.9 in cell `latch`, from (-0.794, 1.13) to (0.794, 1.39) µm;
- psd.2 in cell `inverter`, from (0.81, 1.06) to (1.47, 1.64) µm;
- poly.2 in cell `inverter`, from (0.14, 1.26) to (0.29, 1.44) µm.

Pass them to `locate`, each with the cell the report names as `gds_cell`, since a violation inside a subcell is in that cell's coordinates:

```json
{"cell": "latch(1000., 1, 1000., 2)", "boxes": [
  {"x0": -0.794, "y0": 1.13, "x1": 0.794, "y1": 1.39, "gds_cell": "latch", "layers": ["diff", "nwell"], "label": "difftap.9"},
  {"x0": 0.81, "y0": 1.06, "x1": 1.47, "y1": 1.64, "gds_cell": "inverter", "layers": ["psdm"], "label": "psd.2"},
  {"x0": 0.14, "y0": 1.26, "x1": 0.29, "y1": 1.44, "gds_cell": "inverter", "layers": ["poly"], "label": "poly.2"}]}
```

```text
difftap.9 (-0.794, 1.13) to (0.794, 1.39) µm in latch:
  diff.drawing (-145, 130) to (575, 1130): sky130/lib.ar:17:16; in halves[0] (<argon-std>/layout.ar:114:13, called from lib.ar:20:18) > n (lib.ar:58:17) > fet (sky130/lib.ar:171:15)
  diff.drawing (-575, 130) to (145, 1130): sky130/lib.ar:17:16; in halves[1] (<argon-std>/layout.ar:115:13, called from lib.ar:20:18) > n (lib.ar:58:17) > fet (sky130/lib.ar:171:15)
  nwell.drawing (-1705, 1390) to (1705, 2750): lib.ar:30:5
psd.2 (0.81, 1.06) to (1.47, 1.64) µm in inverter:
  psdm.drawing (-275, 1440) to (1135, 2700): sky130/lib.ar:34:19; in p (lib.ar:59:17) > fet (sky130/lib.ar:171:15)
  psdm.drawing (1145, 0) to (1655, 1260): sky130/lib.ar:67:19; in ptap (lib.ar:98:13, called from lib.ar:81:20) > t (sky130/lib.ar:95:13)
poly.2 (0.14, 1.26) to (0.29, 1.44) µm in inverter:
  poly.drawing (140, 0) to (290, 1260): <argon-std>/layout.ar:20:30, called from sky130/lib.ar:14:25; in n (lib.ar:58:17) > fet (sky130/lib.ar:171:15)
  poly.drawing (140, 1440) to (290, 2700): <argon-std>/layout.ar:26:30, called from sky130/lib.ar:14:25; in p (lib.ar:59:17) > fet (sky130/lib.ar:171:15)
```

Paths into the SKY130 library are shortened here. Each shape comes with the line that drew it and the instances leading to it. All three violations involve the NMOS `n` or its tap column, the PMOS `p`, and the gap between them, so the fix is the constraint between the two rows, the drain gap. `locate` names the lines that drew the shapes, not the `eq` that placed them, so look for the constraint that sets the distance it reports.

LVS catches what DRC can't. If both output wires ran at the NMOS drains' height, they would overlap on met2. The layout would still pass DRC, but LVS would report a short between `outn` and `outp`, and one port fewer in the layout than in the schematic. The `compile` tool checks the pin labels against the layout's connectivity first, so it reports that short, with the chain of shapes joining the two outputs, before LVS runs. The [`nets`](/tools/agents#headless-mode) tool compares the two outputs' wiring and rough capacitance, which should match in a mirrored layout.

## A density checklist

Before settling a floorplan, check it against these. Each is cheaper to choose at the start than to retrofit.

- **Share diffusion.** Devices on one rail, or that share a drain, belong in one diffusion. Choose each one's source and drain parity by mirroring it (`reflect=true, angle=180`) so neighbors share a column, and place them with `abut`. Lone fingers with different gates abut when their contacts widen apart, the left one left and the right one right (`gate_side`), or alternate rows (`gate_row`).
- **Share gates.** An NMOS and a PMOS with the same gate can share their fingers, as `cmos` draws them, rather than each needing its own gate contact.
- **Fold long devices.** A device far wider than its neighbors is tall can be folded into rows (`fold`) to the height a taller block next to it already sets.
- **Keep loops short.** Fold a chain or a loop so that the wire returning to its start is short, and keep long wires on one layer away from wide metal of other nets.
- **Put taps where area is cheapest.** A tap column at a row's end usually costs less than a tap rail across the row.
- **Keep supplies narrow near switching nets.** A wide supply sheet under a capacitor's bottom plate or beside a switching net adds capacitance to it. Draw supplies as wide as their current needs, and route them around such nets.
- **Check parasitics.** When power or speed matters, extract the layout's parasitics and compare the nets that matter. Geometry that passes DRC and LVS, such as drain straps between source straps, can still add coupling.

## Next steps

- [`examples/sky130_strongarm`](https://github.com/ucb-substrate/argon/tree/main/examples/sky130_strongarm) builds a larger circuit with rows of different devices, more routing tracks, and met3 crossings. Its rows are wide and short, so it uses full-width tap rails rather than tap columns.
- The [SKY130 library](/language/sky130) page lists its layers, rules, vias, and cells.
- [Constraints](/language/constraints) explains underconstrained and inconsistent errors, and [staying on the grid](/language/constraints#staying-on-the-grid).
