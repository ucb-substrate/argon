---
title: Drawing schematics
description: Placement and routes for the GUI's schematic view, and the symbol a parent's schematic draws a cell with.
---

# Drawing schematics

These keywords and functions only change how the GUI draws a [schematic](/language/schematic). Netlists don't depend on them, so a cell that only needs a netlist can leave them all out.

## Placement and routes

A schematic can carry its drawing in source, as keywords on the calls that create its elements and connections. They only say where the GUI draws things. Nothing in a cell can read them back, and deleting every one of them leaves netlists byte-identical.

```argon
pub let a = Signal(x=-8, y=0);
let nmos = std::schematic::inst(mos(true, nw, nf), x=0, y=-4);
let m0 = device(DeviceKind::Mos, signals(4), "nch", w=1., l=0.15, x=6, y=0, angle=90);
connect(a, nmos.g, route=[(-4, 0), (-4, -4)]);
```

| Keyword | Type | Accepted by | Meaning |
| --- | --- | --- | --- |
| `x`, `y` | `Int` | `inst`, `Signal`, `device` | The position of the element's origin, in grid units, with y pointing up. |
| `angle` | `Int` | `inst`, `Signal`, `device` | Counterclockwise rotation in degrees, a multiple of 90. |
| `reflect` | `Bool` | `inst`, `device` | Mirror about the x axis before rotating, as `std::layout::inst` does. |
| `route` | `[(Int, Int)]` | `connect` | The corners of the wire that draws the connection. |

- `x` and `y` come together; one without the other is an error. `angle` and `reflect` may come alone, and then orient the element wherever the GUI places it.
- Coordinates are `Int` grid units, so `x=4 * i` works in a loop and a placement never waits on the solver. A `Float` such as `x=0.` is reported as a grid-unit mistake.
- `angle` is normalized as `std::layout::inst` normalizes it: `450` means `90`, and `-90` means `270`. A value that isn't a multiple of 90 is an error.
- A placed signal is drawn as a port pin when a `pub` field reaches it, and as a net label showing its net's name otherwise.
- A wire runs from the pin of the first signal, through the corners of `route` in order, to the pin of the second, so it follows its elements when they move. `route=[]` is a straight wire between the two pins, and a `connect` without `route` draws no wire: the GUI shows such a connection as a flyline. Only `connect` draws wires; a device's terminal list joins its pins to signals without drawing anything.
- A `Signal(x=…, y=…)` inside a function places the signal of every call at the same point, so a function that creates signals should take positions as parameters or leave them out.

An inverter built from devices, placed and wired:

```argon
use std::schematic::{DeviceKind, Signal, connect, device, signals};

cell device_inv(pw: Float, nw: Float) {
    pub let a = Signal(x=-6, y=0);
    pub let out = Signal(x=4, y=0);
    pub let vdd = Signal(x=0, y=8);
    pub let vss = Signal(x=0, y=-8);

    let mp = device(DeviceKind::Pmos, signals(4), "pch", w=pw, l=0.15, x=0, y=4);
    let mn = device(DeviceKind::Nmos, signals(4), "nch", w=nw, l=0.15, x=0, y=-4);

    connect(a, mp.g, route=[(-4, 0), (-4, 4)]);
    connect(a, mn.g, route=[(-4, 0), (-4, -4)]);
    connect(out, mp.d, route=[(0, 0)]);
    connect(out, mn.d, route=[(0, 0)]);
    connect(vdd, mp.s, route=[]);
    connect(vdd, mp.b, route=[(2, 8)]);
    connect(vss, mn.s, route=[]);
    connect(vss, mn.b, route=[(2, -8)]);
}
```

The two `a` wires share the segment from the port to (-4, 0), which is drawn once with a junction where they part. Its netlist names the devices after their bindings:

```text
.subckt device_inv a out vdd vss
Mmp out a vdd vdd pch w=2 l=0.15
Mmn out a vss vss nch w=1 l=0.15
.ends device_inv
```

## Symbols

A cell draws its symbol with [`std::symbol`](/language/std-symbol) calls in its body, next to its layout and schematic statements. One run of the body produces all three views, so a symbol can depend on the cell's parameters. A parent's schematic draws each instance of the cell with this symbol.

```argon
use std::symbol::{circle, inst_name, line, param, pin, polygon};

cell device_inv(pw: Float, nw: Float) {
    // Ports, devices, and connections as above.

    polygon([(-2., 2.), (-2., -2.), (2., 0.)]);
    circle(x=2.25, y=0., r=0.25);
    line([(-4., 0.), (-2., 0.)]);
    line([(2.5, 0.), (4., 0.)]);
    line([(0., 1.), (0., 4.)]);
    line([(0., -1.), (0., -4.)]);

    pin(a, x=-4, y=0);
    pin(out, x=4, y=0);
    pin(vdd, x=0, y=4, label=true);
    pin(vss, x=0, y=-4, label=true);

    inst_name(x=2., y=2.);
    param("pw", pw, x=2., y=-2.);
    param("nw", nw, x=2., y=-3.);
}
```

- **Units.** Everything is in grid units. Shapes and text take `Float` coordinates, so arrows and bubbles can fall between grid points. Pins take `Int` coordinates, so they sit on the grid and a parent's wires end on grid points; a `Float` pin coordinate is reported as a grid-unit mistake.
- **Values.** A `Float` that depends on constraints waits until it is solved, as a device parameter does. A value that isn't a finite number is an error, and so are a `line` with fewer than two points, a `polygon` with fewer than three, and a radius or text size that isn't positive.
- **Text.** `text` draws literal text, `inst_name` the instance's name in the parent, such as `Xinv` or `Mmp`, and `param` a parameter as `name=value`, formatted as netlists format parameters. Text is centered vertically on `y`; `align` says whether `x` is its left end, its center, or its right end, and `size` is its height, 1 by default. The GUI keeps text upright under every placement.
- **Which cells have symbols.** A cell draws a symbol when it makes at least one `std::symbol` call, directly or through a function. A cell that makes none is drawn as a generated box: its ports as pins along the sides, supplies on top and grounds on the bottom, with the instance name above and the cell name below.
- **Functions.** A `std::symbol` call inside a function draws in the cell that called the function, as `Signal()` creates a signal there. So symbols can be shared as functions, and the device helpers are ordinary Argon.
- **Names.** `rect`, `polygon`, and `text` also exist in `std::layout`. Import one set under aliases, or call it by path.
- **Can't be read back.** A symbol is only presentation. Nothing in evaluation reads it, it isn't reachable through an instance, and a parent's symbol doesn't include its children's. Deleting every `std::symbol` call leaves netlists byte-identical.

### Pins

A pin names a port through any signal on that port's net, the way `connect` names a net: once `connect(a, m.g)` has joined them, `pin(a, …)` and `pin(m.g, …)` name the same port. Pins are checked once the body has run and the ports are known. Each of these is an error:

- a `pin` on a net that isn't a port;
- two pins for one port, since a parent's wire has to end at one pin;
- two pins at one point;
- a port without a pin, in a cell that draws a symbol. The error points at the `pub let` that made the port.

A cell with errors still has the rest of its symbol drawn. A symbol pin is separate from a [placed signal](#placement-and-routes) in the cell's own schematic: one says where the port is on the symbol, and the other where the port is drawn inside the cell.

With `label=true`, the port's name is drawn at size 1 beside the pin, clear of a stub of any length. The pin's side is the edge of the symbol's shapes it is nearest to or farthest beyond, with left and right winning ties; a symbol without shapes uses its pins instead. The name goes here:

| Side | Position | Alignment |
| --- | --- | --- |
| Top | (x + 0.5, y - 1) | `Left` |
| Bottom | (x + 0.5, y + 1) | `Left` |
| Left | (x + 0.5, y + 1) | `Left` |
| Right | (x - 0.5, y + 1) | `Right` |

In the inverter above, `vdd`'s name is at (0.5, 3). A label follows the shapes, so a shape that changes which edge is nearest moves it to another side.

### Device symbols

`std::symbol` draws the device symbols as Argon functions over the natives, listed under [device helpers](/language/std-symbol#device-helpers). A cell that wraps one device can draw itself as that device; the `mos` cell at the top of [Schematics](/language/schematic) does it with:

```argon
if nfet {
    std::symbol::nmos(d, g, s, b);
} else {
    std::symbol::pmos(d, g, s, b);
};
```

A raw `device` is drawn with the helper for its kind and terminal count, so a wrapper cell and the device it wraps look the same.
