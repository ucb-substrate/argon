---
title: Schematics
description: Signals, nets, ports, devices, and schematic instances.
---

# Schematics

A cell can describe a schematic as well as a layout. Its schematic is a set of signals joined into nets, the devices and child cells connected to those nets, and the ports an instance of the cell connects to. The schematic items live in [`std::schematic`](/language/std-schematic).

```argon
use std::schematic::{DeviceKind, Signal, connect, device};
use std::schematic::inst as sinst;

cell mos(nfet: Bool, w: Float, nf: Int) {
    pub let d = Signal();
    pub let g = Signal();
    pub let s = Signal();
    pub let b = Signal();

    let model = if nfet {
        "sky130_fd_pr__nfet_01v8"
    } else {
        "sky130_fd_pr__pfet_01v8"
    };

    device(DeviceKind::Subckt, [d, g, s, b], model, l=0.15, w=w, nf=nf);
}

cell inv(pw: Float, nw: Float, nf: Int) {
    pub let a = Signal();
    pub let out = Signal();
    pub let vdd = Signal();
    pub let vss = Signal();

    let nmos = sinst(mos(true, nw, nf));
    let pmos = sinst(mos(false, pw, nf));
    connect(a, nmos.g);
    connect(a, pmos.g);
    connect(out, nmos.d);
    connect(out, pmos.d);
    connect(vdd, pmos.s);
    connect(vdd, pmos.b);
    connect(vss, nmos.s);
    connect(vss, nmos.b);
}
```

`mos` has four ports, `d`, `g`, `s`, and `b`, and one device. `inv` has the ports `a`, `out`, `vdd`, and `vss`, and two instances of `mos`, named `Xnmos` and `Xpmos`.

## Views

A cell body may mix layout and schematic statements. Calling a cell runs its body once and produces both views. [`std::layout::inst`](/language/std-layout#inst) places the cell's layout, and [`std::schematic::inst`](/language/std-schematic#inst) places its schematic. The same cell value can be placed both ways:

```argon
let cell = inv(2., 1., 2);
let drawn = std::layout::inst(cell, x=0., y=0.);
let wired = std::schematic::inst(cell);
```

A cell with no schematic content has an empty schematic, with no ports.

## Signals and nets

`Signal()` creates a signal of the current cell, and `connect(a, b)` joins the nets of two signals into one. Connecting a signal to itself, or connecting the same pair twice, is allowed.

Signals are values, so they can be stored in sequences, tuples, structs, and enum payloads, and passed to and returned from functions. A signal created in a function belongs to the cell that calls the function.

```argon
struct Supply {
    vdd: Signal,
    vss: Signal,
}

fn supply() -> Supply {
    Supply { vdd: Signal(), vss: Signal() }
}
```

Signals can't be compared, emitted with `!`, or used in arithmetic.

## Ports

A port is a net that holds a public signal the cell created: one that a [`pub let`](/language/cells-functions#public-fields) holds, anywhere inside its value, and that came from `Signal()` in this cell or in a function it called.

- **Order.** The public fields are walked in declaration order, depth first, and a net becomes a port the first time the walk reaches it.
- **Aliasing.** A port is a net, not a path. A net that several public paths reach, through `connect` or by holding the same signal twice, is one port.
- **Terminals.** A terminal of a placed instance is never a port by itself, even when a `pub let` holds it. Connect it to a `pub` signal of the cell to export it.

```argon
cell bus() {
    pub let a = Signal();
    pub let data = [a, Signal()];  // ports: a, data_1 (data_0 is a)
    pub let clk = data[1];          // data_1 is now named clk
}
```

## Naming

Names in a schematic come from the `let` bindings that reach each net or instance.

A path through a value is joined with `_`: the field name, then the index of a sequence or tuple element, the name of a struct field, or the index or name of an enum payload element (the variant's name is left out). `pub let pin = Pin { net: s, .. }` reaches `s` as `pin_net`.

**Ports** are named after the shortest public path that reaches them: the path with the fewest segments, then the fewest characters, then the one reached first. An identifier containing `_`, such as `v_dd`, is one segment.

**Other nets** take the first of these that exists, preferring the shortest within each:

1. a path from a top-level `let` that ends at a signal the cell created;
2. a path that ends at an instance terminal, with the child's port name as its last segment, such as `nmos_b`;
3. the name of the `let`, at any depth, that first bound the signal, or for a terminal, the instance's name and the port name;
4. `net` followed by the net's index.

A name bound to a signal beats an instance terminal's path even when the terminal's path is shorter: a net reached as `bus_0` and as `m_g` is named `bus_0`.

**Instances** are named `X` followed by the path of the first top-level `let` that holds them, by the name of the `let` that bound them, or by `inst` and their index. **Devices** are named by their element letter and their index among the cell's devices, in creation order: `M0`, `R1`, `X2`.

Names are unique within a cell, compared without regard to case. A repeated name gets the first free suffix `_1`, `_2`, …, with ports named first. Instance and device names are unique together.

## Devices

`device(kind, terminals, model, ..params)` adds a primitive device to the cell. `kind` is a [`DeviceKind`](/language/std-schematic#devicekind), which decides how many terminals the device takes and whether it needs a model.

| Kind | Element | Terminals | Model | `value` |
| --- | --- | --- | --- | --- |
| `Mos` | `M` | 4 (d, g, s, b) | Required | Not allowed |
| `Res` | `R` | 2 | Optional | Optional |
| `Cap` | `C` | 2 | Optional | Optional |
| `Diode` | `D` | 2 (anode, cathode) | Required | Not allowed |
| `Bjt` | `Q` | 3 or 4 (c, b, e, and an optional substrate) | Required | Not allowed |
| `Subckt` | `X` | 1 or more | Required | Not allowed |

A resistor or capacitor needs a model, a `value`, or both. The model is a name without whitespace; `""` means no model. Every keyword argument is a device parameter, which must be a `Float`, `Int`, or `String`. A `Float` parameter may depend on constraints, and the device waits until it is solved.

```argon
device(DeviceKind::Res, [a, b], "", value=1000.);
device(DeviceKind::Mos, [d, g, s, b], "nch", w=2., l=0.15);
```

## Schematic instances

`std::schematic::inst(cell)` places a cell in the schematic and returns a [schematic instance](/language/types/instance#schematic-instances). Reading a port of the cell through it gives the instance's terminal, a signal of the current cell that `connect` can join to other nets.

```argon
let r1 = sinst(resistor(1000.));
connect(input, r1.p);
```

A terminal that nothing connects to becomes a net of its own.

A parent can read a `pub` schematic instance of a child, and its terminals through it. When `x` holds `pub let m = sinst(mos(..));`, `x.m.g` is the parent's signal on that net if the net is a port of `x`, and detached otherwise. Holding `m` does not make its terminals ports of `x`.

## Reading across views

Each kind of instance reads what its view has. A layout instance reads geometry, and a schematic instance reads signals and schematic instances. Anything else a field holds reads as a *detached* placeholder, which is an error only when it is used:

```argon
cell pad() {
    pub let metal = std::layout::rect("met1", x0=0., y0=0., x1=10., y1=10.);
    pub let net = Signal();
}

cell top() {
    let p = sinst(pad());
    let held = p.metal;  // fine: holding a detached value
    let w = p.metal.w;   // error: `metal` of `pad` was read through a schematic instance
}
```

Binding a detached value, storing it, passing it to or returning it from a function, and storing it in a `pub` field are not uses. Reading a field of it, indexing it, computing with it, comparing it, branching on it, passing it to a built-in, native, or cell, connecting it, and emitting it with `!` are.

A signal on a net that isn't a port of the instance's cell is detached as well: connect it to a `pub let … = Signal()` in that cell to export it.

## Cell parameters

A cell parameter can't hold a signal or a schematic instance, at any depth of its type. `cell c(p: Pin)` is an error when `Pin` has a `Signal` field, and so is calling a generic cell with a signal type argument. Pass signals between cells by placing the cell and connecting its ports.
