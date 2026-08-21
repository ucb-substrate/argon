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

Currently, Argon supports the following features:
- Drawing rectangles and dimension constraints in GUI
- Live reload of GUI upon changes in code editor
- Parametric cells
- Hierarchy
- Linear constraint solving: fast sparse elimination, with a general (dense) solver as fallback
- Basic diagnostic reporting in the code editor
- Basic detection of under/overconstrained systems

Future versions of Argon will hopefully support:
- Detection/reporting of under/overconstrained geometry and conflicting constraints
- Faster linear constraint solving (not necessarily supporting general constraints) 
- Additional editing capabilities in GUI (e.g. instantiating cells)
- Incremental compilation/caching
- More advanced data types (e.g. Rust-style enums)
- Integration with Rust

## Installation

To use Argon, you will need:
- [Rust (tested on 1.90.0)](https://www.rust-lang.org/tools/install)
- [Neovim (version 0.12.0 or above)](https://github.com/neovim/neovim/blob/master/INSTALL.md)
- Git

Install Argon from source:

```bash
cargo install --git https://github.com/ucb-substrate/argon --locked \
    argonc arc argon-analyzer argone
```

## Command-line compilation

Use `arc` from an Argon library containing `lib.ar` and `Argon.toml`. The
manifest names the library and can set its layer-properties file and path
dependencies:

```toml
name = "my-library"
lyp = "layers.lyp"

[dependencies]
pdk = "../pdk"
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

### Neovim

Install the Neovim plugin with the built-in `vim.pack` package manager by
adding this to your `init.lua`:

```lua
vim.pack.add({
    'https://github.com/ucb-substrate/argon',
})
```

The plugin detects `.ar` files and starts `argon-analyzer` from your
`PATH`; no repository path is needed in your Neovim configuration.

From an Argon project directory, start Neovim and the GUI together:

```bash
argone
```

You can also give `argone` a project directory or an Argon source file:

```bash
argone path/to/project
argone path/to/project/lib.ar
```

`argone` runs Neovim in the current terminal and starts the GUI as soon as
the Argon analyzer is ready. To edit a project on another machine, use an SSH
host or alias from your OpenSSH configuration:

```bash
argone ssh build-server /path/to/project
```

Argone selects and forwards the RPC ports automatically. Neovim, the Argon
Neovim plugin, and `argon-analyzer` must be installed on the remote machine,
but `argone` itself is only needed locally; the graphical application also
runs only on the local machine.

Launching Neovim yourself remains supported. In that mode, start or activate
the GUI by running `:Argon gui`.

Run `:Argon diagnostics` to open a compiler-style view of every Argon
diagnostic in the project, including diagnostics from files other than the
current buffer. Press `<Enter>` on an entry to jump to its file and location,
use `]d` and `[d` to move between entries, `r` to refresh, and `q` to close the
panel. The same entries are also loaded into Neovim's quickfix list, so
`:copen`, `:cnext`, and `:cprev` provide the standard cross-file navigation.

From within the GUI, type `:openCell inv(1200., 2000., 4)` to open the `inv` cell. You should now be able to edit layouts 
in both Neovim and the GUI.

## Parametric Cell Tutorial

Create a new Argon library with the following command:

```bash
mkdir tutorial && touch tutorial/lib.ar
```

Your library directory should look like this:

```
tutorial
└── lib.ar
```

Inside `lib.ar`, define a new cell:

```rust
cell inset_rect() {
}
```

Start the GUI and run `:openCell inset_rect()`. Click on the `met2` layer from the layer sidebar on the right to select it.
Hit `r` to use the Rect tool and click on two points on the screen to draw your first rectangle.
You should see a rectangle appear in the GUI and code editor.

Select the `met1` layer and draw another rectangle that surrounds the first. You can use the `ESC` key to exit the Rect tool.

Let us now dimension the rectangles such that the `met2`
rectangle is inset by `50.` relative to the `met1` rectangle.
Hit `d` to use the Dimension tool and click on the top edge of each rectangle. Click somewhere else to place the dimension label.
The dimension should now be highlighted yellow, indicating that you are editing that dimension. Type `5.` and hit enter to set the value
of the dimension (the decimal point is important, since just `5` is considered an integer literal rather than a float).

> [!TIP]
> If you make a mistake, you can undo and redo changes from the GUI using `u` and `Ctrl + r`,
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
and `h`. To resolve this, go back to the GUI and run `:openCell inset_rect(200., 200.)`. 

You can now dimension the width of the `met1` rectangle by selecting the top edge then 
clicking above the rectangle to place the dimension label.
Enter the dimension as `w`. Dimension the right edge to `h`. You
can use the `f` keybind to fit the layout to your screen.

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

After saving, try opening this cell from the GUI by running `:openCell triple_rect()`. You
should be able to constrain the instances relative to one another based on their
constituent rectangles.

## Logs

<!-- TODO: Implement commands to open GUI log -->
Argon writes log messages to `~/.local/state/argon/analyzer.log` (analyzer) and `~/.local/state/argon/argone.log` (Argone).
Log level can be set using the `ARGON_LOG` environment variable
or in the Neovim configuration. If no configuration is specified, only errors will be logged.
Log level configuration follows [`RUST_LOG`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/fmt/index.html#filtering-events-with-environment-variables) syntax.

For performance, it is recommended to use `ARGON_LOG=warn` or `ARGON_LOG=error` unless you are troubleshooting an issue.

### Analyzer logs

While the analyzer is running, you can open its logs using the `:Argon log` command.

To configure the log level, you can use the `vim.g.argon.log.level` key:

```lua
vim.g.argon = {
    -- ...
    log = {
        level = "debug"
    }
}
```

The Neovim plugin will then supply `ARGON_LOG=debug` when starting the analyzer and Argone.

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
