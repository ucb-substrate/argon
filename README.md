# Argon

[![License](https://img.shields.io/badge/License-BSD_3--Clause-blue.svg)](https://opensource.org/licenses/BSD-3-Clause)

Argon is a programming language for writing constraint-based integrated circuit layout generators.
Argon's primary feature is bidirectional editing between Neovim and a custom GUI.
Simpler geometric constraints can be entered visually in the GUI, while more complex logic can be
implemented in code.

Argon's syntax and type system is inspired by Rust. Unlike Rust, Argon is not intended to be a fully featured 
general-purpose programming language. The main goal of Argon is to allow interoperability with the GUI,
enable the creation of most practical parametric cells, and allow for performance optimizations such
as caching and incremental compilation.

## Installation

To use Argon, you will need:
- [Rust (tested on 1.90.0)](https://www.rust-lang.org/tools/install)
- [Neovim (version 0.12.0 or above)](https://github.com/neovim/neovim/blob/master/INSTALL.md)
- Git

Install Argon from source:

```bash
cargo install --git https://github.com/ucb-substrate/argon --locked argon
```

To install from a local clone, you can run:

```bash
cargo install --locked --path crates/argon
```

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
ring_osc = "~/Downloads/ring_osc.gds"
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
arc run --cell 'array(cons(250., cons(350., [])), Mode::Fast)'
```

Parameters declared with a default value are keyword parameters. They are
passed by name, may be omitted, and must follow the positional parameters. A
default is an ordinary expression evaluated at each call; it may refer to the
parameters declared before it and to module-level items, and its type must match
the declared type exactly:

```rust
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
the construct's body. Struct values are valid cell arguments, including on the
command line:

```bash
arc run --cell 'via(ViaParams { layer: "met1", size: Size { w: 100., h: 50. }, n: 1 })'
```

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
let outline = polygon("met1", 3,
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
`bbox(inst)`.

### IDE

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
Code navigation works on variables, function and cell names, enums and their
variants, module paths, and the fields of an instance:

| Mapping | Action |
|---|---|
| `gd`, `<C-]>` | Go to definition |
| `grr` | List references |

Navigation crosses files, follows path dependencies into other libraries, and
jumps into the standard library, which is written to `~/.cache/argon` the
first time you navigate into it. It keeps working while the workspace has
errors, answering from the last version that type-checked.

From an Argon project directory, start Neovim and the GUI together:

```bash
argone pdks/sky130
```

From within the GUI, hit the `o` hotkey, type `inv(1200., 2000., 4)` after the prefilled `:Argon openCell` command, and press Enter to 
open the `inv` cell. You should now be able to edit layouts in both Neovim and the GUI.

## Parametric Cell Tutorial

Create a new Argon library with the following command:

```bash
arc new tutorial
```

Your library directory should look like this:

```text
tutorial
├── Argon.toml
├── tech.toml
└── lib.ar
```

The generated `lib.ar` contains a `top()` cell with a “Hello world!”
text label. The manifest points to the generated default technology file,
so the workspace is ready to open immediately:

```bash
argone tutorial
```

With the layout canvas focused, press `o` and enter `top()` after the
prefilled `:Argon openCell ` command to see the starter label.

For the rest of the tutorial, replace the generated `top()` cell in `lib.ar`
with:

```rust
cell inset_rect() {
}
```

With the layout canvas focused, press `o`, type `inset_rect()` after the prefilled
`:Argon openCell ` command, and press `Enter`. Click the `met2` layer in the
right sidebar to select it. Press `r` to activate the Rectangle tool, then click
two points on the canvas to draw your first rectangle.
You should see a rectangle appear in the GUI and code editor.

Select the `met1` layer and draw another rectangle that surrounds the first.
Press `Esc` to leave the Rectangle tool.

Let us now dimension the rectangles such that the `met2`
rectangle is inset by `50.` relative to the `met1` rectangle.
Press `d` to activate the Dimension tool and click the top edge of each
rectangle. Click elsewhere to place the dimension label. The dimension should
be highlighted yellow while it is being edited. Type `50.` and press `Enter`
to set its value. The decimal point is important because `50` is an integer
literal, while the dimension requires a float. To edit an existing dimension
later, press `s`, select its label, and press `q`.

> [!TIP]
> If you make a mistake, you can undo and redo changes from the GUI using `u` and `Ctrl-R`,
> respectively, or manually modify the code in the text editor if needed.

Repeat for the other 3 sides of the rectangle.

Now, let's parametrize the width and height of the outer rectangle. In the code editor, add a width and height parameter to your cell:

```rust
cell inset_rect(w: Float, h: Float) {
    // ...
}
```

Once you save, you may notice that an error popped up saying that the open cell is invalid.
This is because we opened the cell with no arguments, but the cell now requires us to specify `w`
and `h`. To resolve this, focus the canvas, press `o`, enter
`inset_rect(200., 200.)`, and press `Enter`.

You can now press `d` and dimension the width of the `met1` rectangle by
selecting the top edge, then clicking above the rectangle to place the label.
Enter the dimension as `w`. Dimension the right edge to `h`. You
can press `f` to fit the layout to your screen.

You may notice that none of the rectangles have a solid boundary, indicating that they are not fully constrained. In order to
constrain the edges to absolute coordinates, you can dimension the left and bottom edges of the `met1` rectangle relative to the origin.
If the origin is not in view, you can also add the following lines to your code (make sure to
save in order to have your changes reflected in the GUI):

```rust
cell inset_rect(w: Float, h: Float) {
    // ...
    eq(rect1.x0, 0.);
    eq(rect1.y0, 0.);
}
```

You can also define a hierarchical cell in your code editor as follows:

```rust
cell triple_rect() {
    let cell1 = inset_rect(200., 200.);
    let inst1 = inst(cell1);
    let inst2 = inst(cell1, xi=300.);
    let inst3 = inst(inset_rect(300., 400.), xi=600.);
}
```

After saving, focus the canvas, press `o`, enter `triple_rect()`, and press
`Enter`. You should be able to constrain the instances relative to one another
based on their constituent rectangles.

You can also add an instance from the GUI. Select the destination scope in the
hierarchy sidebar, press `i`, enter a cell invocation such as
`inset_rect(150., 150.)`, and press `Enter`. Move the instance outline to the
desired location and click to insert it. The placement tool remains active so
you can click again to insert more copies; press `Esc` when finished.

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
built-in sizes. Run `:Argon reload` after editing the file.

## Logs

The analyzer and Argone write to one shared log at
`~/.local/state/argon/argon.log`. If `XDG_STATE_HOME` is set, the log is
written to `$XDG_STATE_HOME/argon/argon.log` instead. While the analyzer is
running, open it with `:Argon log`.

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
