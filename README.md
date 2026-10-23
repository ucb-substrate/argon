# Argon

[![License](https://img.shields.io/badge/License-BSD_3--Clause-blue.svg)](https://opensource.org/licenses/BSD-3-Clause)

Argon is a programming language for writing constraint-based integrated circuit layout generators.
Argon's primary feature is bidirectional editing between a text editor (Neovim or VS Code) and a custom GUI.
Simpler geometric constraints can be entered visually in the GUI, while more complex logic can be
implemented in code.

Argon's syntax and type system is inspired by Rust. Unlike Rust, Argon is not intended to be a fully featured 
general-purpose programming language. The main goal of Argon is to allow interoperability with the GUI,
enable the creation of most practical parametric cells, and allow for performance optimizations such
as caching and incremental compilation.

## Documentation

The documentation site is published at <https://ucb-substrate.github.io/argon/>.
It is built with Docusaurus and lives in [`docs/`](docs). Its pages are
under [`docs/docs/`](docs/docs), split into [guides](docs/docs/guides/index.md), a
[language reference](docs/docs/language/overview.md), a
[GUI manual](docs/docs/gui/workspace.md), and a
[tools reference](docs/docs/tools/overview.md), each with its own sidebar.

```bash
cd docs
npm install
npm start
```

Run `npm run build` for a production build with strict internal-link and anchor checks.
See [`docs/README.md`](docs/README.md) for the layout of the site.

## Installation

To use Argon, you will need:
- [Rust (tested on 1.90.0)](https://www.rust-lang.org/tools/install)
- An editor: [Neovim (version 0.12.0 or above)](https://github.com/neovim/neovim/blob/master/INSTALL.md)
  or [VS Code (version 1.100 or above)](https://code.visualstudio.com/)
- Git

Install Argon from source:

```bash
cargo install --git https://github.com/ucb-substrate/argon --locked argon
```

To install from a local clone, you can run:

```bash
cargo install --locked --path crates/argon
```

Then set up your editor, as described under [IDE](#ide).

## Command-line compilation

Use `arc` from an Argon library containing `lib.ar` and `Argon.toml`. The
manifest names the library and can set its Argon technology file and path
dependencies and GDS cell imports:

```toml
name = "my-library"
tech = "tech.toml"

[dependencies]
pdk = "../pdk"

[gds]
ring_osc = "layout/ring_osc.gds"
"macros::sram" = "layout/sram.gds"
```

From the library directory, check the source or run a cell:

```bash
arc check
arc run --cell 'top(10., 20.)'
arc run --cell 'top()' --gds
```

`arc check` checks the library without executing a cell. `arc run` writes the
result to `target/argon.bin`; pass `--gds` to also write `target/argon.gds`.
Dependency cells use their dependency name, for example
`arc run --cell 'pdk::fet1v8(true, 150., 5)'`.

Cell arguments are ordinary Argon expressions, evaluated by the compiler in the
library's scope, so they may call functions and build sequences:

```bash
arc run --cell 'top(pitch * 4., -width / 2.)'
arc run --cell 'array([250., 350.], Mode::Fast)'
```

Layout is built with the functions of the `std::layout` module: `rect`,
`crect`, `polygon`, `path`, `text`, `inst`, `bbox`, `dimension`, and rectangle
helpers such as `array` and `intersection`. A module imports the ones it calls
with a grouped `use`, as in `use std::layout::{inst, rect};`, or calls them by
full path, as in `std::layout::rect(...)`. Code inserted by the GUI uses full
paths, so it needs no import.

Parameters declared with a default value are keyword parameters. They are
passed by name, may be omitted, and must follow the positional parameters. A
default is an ordinary expression evaluated at each call; it may refer to the
parameters declared before it and to module-level items, and its type must match
the declared type exactly:

```rust
use std::layout::inst;

cell via(layer: String, w: Float, h: Float = w, n: Int = 1) {
    // ...
}

cell top() {
    let square = inst(via("met1", 100.));
    let stack = inst(via("met1", 100., h=300., n=3));
}
```

Positional parameters cannot be passed by name, and keyword parameters cannot be
passed positionally. Keyword arguments also work in cell invocations:
`arc run --cell 'via("met1", 100., n=3)'`.

Structs group related values under named fields. Declare them at module level,
construct them with braces, and read fields with `.`. Every field must be given
exactly once unless `..base` supplies the ones not listed, and a bare `name` is
shorthand for `name: name`:

```rust
use std::layout::{inst, rect};

struct Size {
    w: Float,
    h: Float,
}

struct ViaParams {
    layer: String,
    size: Size,
    n: Int,
}

fn grow(s: Size, by: Float) -> Size {
    Size { w: s.w + by, ..s }
}

cell via(p: ViaParams) {
    let r = rect(p.layer, x0=0., y0=0., w=p.size.w, h=p.size.h);
}

cell top() {
    let size = grow(Size { w: 100., h: 50. }, 10.);
    let n = 2;
    let v = inst(via(ViaParams { layer: "met1", size, n }));
}
```

Struct types are nominal: two structs with the same fields are different types,
and a struct may not contain itself. Fields may have any type, including enums,
other structs, sequences, and tuples. A struct declared in another module is
imported with `use`, and a literal may name its module, as in
`geom::Size { w: 1., h: 2. }`. Inside an `if` condition, a `match` scrutinee, or
a `for` sequence a literal must be parenthesized, since `name {` there begins
the construct's body. Struct and tuple values are valid cell arguments,
including on the command line:

```bash
arc run --cell 'via(ViaParams { layer: "met1", size: Size { w: 100., h: 50. }, n: 1 })'
```

A `let` takes a struct apart with a pattern that names the struct and binds its
fields. A bare `name` binds the field of that name, `field: other` binds it
under another name, and `field: _` drops it. A pattern must name every field
unless it ends in `..`, and field patterns do not nest. At the top of a cell,
each name a pattern binds is a field of the cell, like any other `let`. A
`match` on a struct takes the same patterns:

```rust
use std::layout::rect;

fn area(s: Size) -> Float {
    let Size { w, h: height } = s;
    w * height
}

cell via_pair(p: ViaParams) {
    let ViaParams { layer, size, .. } = p;
    let left = rect(layer, x0=0., y0=0., w=size.w, h=size.h);
    let right = rect(layer, x0=2. * size.w, y0=0., w=size.w, h=size.h);
}
```

Enum variants may carry values, either as a tuple or as named fields. A tuple
variant is constructed like a call and taken apart by a `match` pattern that
binds its payload; `_` skips an element, and a bare name or `_` arm matches
anything. Every variant is also an item that `use` can import:

```rust
enum Shape {
    Circle(Float),
    Box(Float, Float),
    Empty,
}

fn width(s: Shape) -> Float {
    match s {
        Shape::Circle(r) => 2. * r,
        Shape::Box(w, _) => w,
        _ => 0.,
    }
}
```

A variant with named fields is written and constructed like a struct, with the
same shorthand, and matched by a pattern that names the fields it binds. A
pattern must name every field unless it ends in `..`, `field: name` renames a
binding, and `field: _` drops one. Unlike a struct literal, a variant takes no
`..base`, and like a struct literal it must be parenthesized in an `if`
condition, a `match` scrutinee, or a `for` sequence:

```rust
use std::layout::rect;

enum Shape {
    Circle { r: Float },
    Box { w: Float, h: Float },
    Empty,
}

fn width(s: Shape) -> Float {
    match s {
        Shape::Circle { r } => 2. * r,
        Shape::Box { w, .. } => w,
        Shape::Empty => 0.,
    }
}

cell top() {
    let w = 300.;
    let h = 20.;
    let boxed = Shape::Box { w, h };
    let r = rect("met1", x0=0., y0=0., w=width(boxed), h=h);
}
```

Structs, enums, functions, and cells take type parameters, written in angle
brackets after the name as in Rust. A type parameter is opaque: a value of
type `T` can be stored, passed, and returned, but not added, compared, cast,
emitted, indexed, or placed. Type arguments are inferred from the arguments
and field values at each use, so `last(widths)` is a `Float` when `widths` is
a `[Float]`:

```rust
struct Pair<A, B> {
    first: A,
    second: B,
}

fn swap<A, B>(p: Pair<A, B>) -> Pair<B, A> {
    Pair { first: p.second, second: p.first }
}

fn second<T>(items: [T]) -> T {
    head(tail(items))
}

cell row<T>(items: [T], pitch: Float) {
    let first = head(items);
    // ...
}
```

The standard library declares `enum Option<T> { Some(T), None }`, and every
module starts with `Option`, `Some`, and `None` in scope. An `Option`
parameter with a `None` default is an optional cell parameter, `Option<Node>`
lets a struct contain itself, and `std::unwrap_or`, `std::is_some`,
`std::is_none`, `std::first`, and `std::last` work over any element type:

```rust
use std::layout::inst;

struct Node {
    width: Float,
    next: Option<Node>,
}

cell via(layer: String, w: Float, n: Option<Int> = None) {
    let count = std::unwrap_or(n, 1);
    // ...
}

cell top() {
    let single = inst(via("met1", 100.));
    let triple = inst(via("met1", 100., n=Some(3)));
}
```

Inside a function body, inference spans the whole body, so `let o = None;`
is fine when a later expression pins its type. Inside a cell, each top-level
statement is typed on its own, so a value with nothing to infer from takes a
type annotation or a turbofish: `let o: Option<Int> = None;` or
`let o = None::<Int>;`. Type arguments in a type position are mandatory for a
generic type (`Option<Float>`, never a bare `Option`) and forbidden for a
non-generic one.

Shapes are valid cell arguments too. A `Rect`, `Polygon`, `Path`, or `Point`
parameter receives the shape by value: the caller's solver resolves its
coordinates first, and the cell sees them as constants in its own coordinate
frame, exactly as a `Float` argument arrives as a number. Placing an instance
elsewhere does not move them, and constraints inside the cell cannot move the
caller's geometry; share live geometry with a `fn` instead. Inside the cell the
shape is construction geometry: it is not drawn and does not count toward the
cell's extent, but its edges can constrain what the cell draws. `!` draws it on
its layer when it was drawn in the caller, so layout geometry and shapes read
out of an instance qualify while a `crect` or a `bbox` does not. Shapes at
different positions are different arguments, so they compile to different
cells:

```rust
use std::layout::{crect, inst, rect};

cell via_array(region: Rect, size: Float, pitch: Float) {
    let via = crect(layer="via1", x0=0., y0=0., w=size, h=size);
    let vias = std::layout::max_array(via, region.w, region.h, pitch, pitch);
    eq(vias.x0 - region.x0, region.x1 - vias.x1);
    eq(vias.y0 - region.y0, region.y1 - vias.y1);
}

cell top() {
    let met1 = rect("met1", x0=0., y0=0., w=100., h=50.);
    let met2 = rect("met2", x0=10., y0=5., w=80., h=40.);
    let vias = inst(via_array(std::layout::intersection(met1, met2), 10., 20.));
}
```

Shapes built on the command line work the same way:

```bash
arc run --cell 'via_array(crect(x0=0., y0=0., w=90., h=40.), 10., 20.)'
```

Top-level declarations may appear in any order: a cell, function, struct, or
enum can be used above the line that declares it. Cells may also instantiate
themselves or one another, which is how a recursive layout is written. The
recursion has to end on a parameter, since the compiler stops at a fixed
nesting depth:

```rust
use std::layout::{inst, rect};

cell tree(n: Int) {
    pub let leaf = rect("met1", x0=0., y0=0., w=100., h=100.);
    if n > 0 {
        let child = inst(tree(n - 1));
        eq(child.leaf.x0, leaf.x1 + 50.);
        eq(child.y, 0.);
    }
}
```

An instance reads only the fields its cell declares with `pub let`, such as
`child.leaf` above. `pub` applies only to a `let` at the top level of a cell
body, and a `pub let` with a pattern makes every name it binds public.

Cell types are nominal: two cells with the same fields are different types,
and a function that accepts either takes `Any`. The type of an instance field
is worked out when it is first read, so the one thing that cannot be resolved
is a field whose type depends on itself through the fields of other cells,
which is reported as an error at the read that closes the cycle.

GDS imports are zero-argument cells. A module-qualified entry such as
`"macros::sram"` can be referenced as `lib::macros::sram()` or imported with
`use lib::macros::sram;`. Paths in the manifest are relative to `Argon.toml`,
and a leading `~/` is expanded to the user's home directory.
When invoking `argonc` directly, pass the technology file with
`--tech tech.toml` and the same mapping as
`--gds-import 'macros::sram=layout/sram.gds'`.

The technology file is TOML. `dbu` is meters per GDS database unit and can
instead use `"m"`, `"mm"`, `"um"`, `"nm"`, or `"pm"`. Every other length is
an integer multiple of the DBU. `display_unit` is also the coordinate unit used
in Argon source, while `grid` controls solver and GUI snapping:

```toml
dbu = "nm"       # physical size of one GDS database unit
display_unit = 1 # one source/display unit is one DBU
grid = 1         # snap grid is one DBU

[[layers]]
name = "met1.pin"
gds = [68, 16]
fill = "#0000ff"
border = "#0000ff"

[layers.style]
expanded = false       # whether a layer group starts expanded
frame_brightness = 0   # -100 black, 0 unchanged, 100 white (border)
fill_brightness = 0    # -100 black, 0 unchanged, 100 white (fill)
dither_pattern = "I0" # I0 solid, I1 clear, other Ix built-in, Cx custom
line_style = "I0"     # empty/I0 solid, other Ix built-in, Cx custom
valid = true           # false: display shapes but do not allow selection
visible = true         # initial visibility in the layer list
transparent = false    # background-dependent transparent composition
width = 1              # border width in screen pixels
marked = false         # draw small crosses over the layer
xfill = false          # draw a diagonal X through boxes
animation = 0          # 0 none, 1 scrolling, 2 blinking, 3 inverse blinking

[[layers]]
name = "met1.label"
gds = [68, 5]
fill = "#0000ff"
border = "#0000ff"

[pin_layers]
"met1.pin" = "met1.label"
```

For example, with `dbu = "nm"`, `display_unit = 1000`, and `grid = 5`,
Argon coordinates are expressed in microns and snap to a 5 nm grid.

Each layer maps its Argon name to a GDS layer/datatype pair. A `pin_layers`
entry maps a pin-shape layer to the text layer that names contained pins. GDS
coordinates are transformed between database and source/display units during
import and export.

Polygons normally take a layer and a point count. Each generated point has
independent solver coordinates, addressable either as `polygon.x0`,
`polygon.y0`, and so on, or through `polygon.points[0].x` and `.y`:

```argon
let outline = std::layout::polygon("met1", 3,
    x0=0., y0=0.,
    x1=100., y1=0.,
    y2=100.,
);
eq(outline.x2, 50.);
```

The GUI polygon tool (toolbar button or `p`) places vertices in click order;
press Enter after the final vertex to close and insert the polygon. It writes
editable fallback coordinates (`x0i`, `y0i`, `x1i`, `y1i`, and so on), so
vertex drags persist. Add hard `x0`/`y0` kwargs or `eq` constraints later when
coordinates should become fixed. Handwritten geometry does not need fallback
kwargs up front: the first drag inserts any missing `*i` coordinates into
polygon, rectangle, or instance constructors. Escape clears an in-progress
polygon. Polygon edges touching a point with any unconstrained coordinate are
dashed; a point constrained in only one axis remains draggable along its free
axis. Polygon fills use the layer's solid or stippled fill style just like
rectangles.

Imported rectangular geometry can be used by GUI dimensions. Unlabeled shapes
receive stable fields such as `gds_rect_12`; a shape on a configured pin layer
uses text from the corresponding contained label layer as its field name.
Repeated pin names are arrays (`inst.VDD[0]`, `inst.VDD[1]`). When an instance
is collapsed in the GUI, its displayed bounding-box edges are available through
`std::layout::bbox(inst)`.

### IDE

Argon works with Neovim or VS Code. Either one starts `argon-analyzer` from
your `PATH`, which reports errors as you type and provides completion and
navigation, and shows the GUI next to your source.

#### Neovim

Install the Neovim plugin with the built-in `vim.pack` package manager by
adding this to your `init.lua`:

```lua
vim.pack.add({
    'https://github.com/ucb-substrate/argon',
})
```

The plugin detects `.ar` files and starts `argon-analyzer` from your
`PATH`.

Errors are reported as you type; `:Argon diagnostics` opens them in a list.
The analyzer exposes LSP completion for visible local bindings, functions,
cells, modules, enum variants, builtin types/functions, keyword arguments, and
type-aware fields. Suggestions are filtered by syntax: declaration-name slots
stay empty, type positions show types, and expressions do not offer declaration
keywords. Argon does not enable or configure a completion UI; Neovim's
`<C-x><C-o>` works without additional setup, and completion plugins can consume
the same LSP results.

To include LSP suggestions in Neovim 0.12's built-in automatic completion,
configure the editor rather than Argon:

```lua
vim.opt.autocomplete = true
vim.opt.complete:append('o')
```

Navigation and IntelliSense work on variables, function and cell names, enums
and their variants, module paths, and the fields of an instance. References to
the symbol under the cursor are highlighted after `CursorHold`:

| Mapping | Action |
|---|---|
| `gd`, `<C-]>` | Go to definition |
| `grr` | List references |
| `K` | Show type or signature information |
| `<C-s>` (insert mode) | Show signature help |
| `gO` | List symbols in the current file |

Navigation crosses files, follows path dependencies into other libraries, and
jumps into the standard library, which is written to `~/.cache/argon` the
first time you navigate into it. It keeps working while the workspace has
errors, answering from the last version that type-checked.

From an Argon project directory, start Neovim and the GUI together:

```bash
argone pdks/sky130
```

From within the GUI, hit the `o` hotkey, type `inv(1200., 2000., 4)` after the
prefilled `:Argon openCell` command, and press Enter to open the `inv` cell.
You should now be able to edit layouts in both Neovim and the GUI.

#### VS Code

The VS Code extension is built from a checkout of this repository, and building
it also builds the GUI as WebAssembly. The
[VS Code extension page](https://ucb-substrate.github.io/argon/tools/vscode#install)
lists the steps.

Errors appear in the Problems panel as you type. Completion, go to definition,
references, hover, signature help, and document symbols use VS Code's usual
commands, and cover the same symbols as in Neovim.

Open the `pdks/sky130` folder in VS Code and open its `lib.ar`. Press
`Ctrl+\` or run **Argon: Focus GUI** from the Command Palette to open the GUI
in a panel beside the source. In the GUI, press `o`, type `inv(1200., 2000., 4)`
in the **Argon: Open Cell** prompt, and press Enter to open the `inv` cell.
`Ctrl+\` switches focus between the source and the GUI.

## Tutorials

The documentation site has a getting-started tutorial for each editor. Both
create a library, draw and constrain a cell, build a small hierarchy, and
export GDS, and they differ only where the editors do:

- [Getting started with Neovim](https://ucb-substrate.github.io/argon/guides/getting-started/installation)
- [Getting started with VS Code](https://ucb-substrate.github.io/argon/guides/vscode/getting-started/installation)

The [guides](docs/docs/guides/index.md) list the other tutorials.

## Configuration

Argon's configuration file is `~/.config/argon/config.toml`, or
`$XDG_CONFIG_HOME/argon/config.toml` when `XDG_CONFIG_HOME` is set. The GUI's
font and icon sizes can be overridden in logical pixels:

```toml
[gui]
font_size = 14
icon_size = 18
```

Both values are optional and must be between 1 and 256. Omit them to use the
built-in sizes. After editing the file, run `:Argon reload` in Neovim or
**Argon: Reload Configuration** in VS Code.

## Logs

The analyzer and Argone write to one shared log at
`~/.local/state/argon/argon.log`. If `XDG_STATE_HOME` is set, the log is
written to `$XDG_STATE_HOME/argon/argon.log` instead. While the analyzer is
running, open it with `:Argon log` in Neovim or **Argon: Open Log** in VS Code.

Configure the log level in `~/.config/argon/config.toml` (or
`$XDG_CONFIG_HOME/argon/config.toml`):

```toml
[log]
level = "debug"
```

The level follows [`RUST_LOG` filter syntax](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/fmt/index.html#filtering-events-with-environment-variables).
It defaults to `error`; `warn` or `error` is recommended unless you are
troubleshooting.

## Contributing

If you'd like to contribute to Argon, please let us know. You can:
* Ping us in the `#substrate` channel in the Berkeley Architecture Research Slack workspace.
* Open an issue and/or PR.
* Email `rahulkumar -AT- berkeley -DOT- edu` and `rohankumar -AT- berkeley -DOT- edu`.

Documentation updates, tests, and bugfixes are always welcome.
For larger feature additions, please discuss your ideas with us before implementing them.

Contributions can be submitted by opening a pull request against the `main` branch
of this repository. Developer documentation can be found in the [`docs/`](docs/developers.md) folder.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in the work by you shall be licensed under the BSD 3-Clause license, without any additional terms or conditions.
