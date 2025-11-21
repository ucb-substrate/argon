# Argon

[![License](https://img.shields.io/badge/License-BSD_3--Clause-blue.svg)](https://opensource.org/licenses/BSD-3-Clause)

Argon is a programming language for writing constraint-based integrated circuit layout generators.
Argon's primary feature is bidirectional editing between a code editor (Neovim or VS Code) and a custom GUI.
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
- General linear constraint solving (slow)
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
- One of [Neovim (version 0.11.0 or above)](https://github.com/neovim/neovim/blob/master/INSTALL.md) or [VS Code (version 1.100.0 or above)](https://code.visualstudio.com/download)

Begin by cloning and compiling the Argon source code:

```bash
git clone https://github.com/ucb-substrate/argon.git
cd argon
cargo b --release
```

### Neovim

Add the following to your Neovim Lua configuration:

```lua
vim.g.argon = {
    argon_repo_path = '<absolute_path_to_argon_repo>'
}
vim.opt.runtimepath:append(vim.g.argon.argon_repo_path .. '/plugins/nvim')
vim.cmd([[autocmd BufRead,BufNewFile *.ar setfiletype argon]])
```

To open an example Argon workspace, run the following from the root directory of your Argon clone:

```
vim core/compiler/examples/argon_workspace/lib.ar
```

Start the GUI by running `:Argon startGui`.

From within the GUI, type `:openCell test()` to open the `test` cell. You should now be able to edit layouts 
in both Neovim and the GUI.

### VS Code

To use VS Code as your code editor, you will additionally need:
- [Node JS (tested on 25.0.0)](https://nodejs.org/en/download)

First, open your VS Code user settings using `Command Palette > Preferences: Open User Settings (JSON)`.
Add the following key:

```json
{
    "argon.argonRepoDir": "<absolute_path_to_argon_repo>"
}
```

Compile the VS Code extension by running the following from the root directory of your Argon clone:

```bash
cd plugins/vscode
npm install
npm run compile
cd ../..
```

To open an example Argon workspace, run the following from the root directory of your Argon clone:

```bash
code --extensionDevelopmentPath=$(pwd)/plugins/vscode core/compiler/examples/argon_workspace
```

We recommend defining an alias in your shell configuration to simplify future commands:

```bash
alias codear="code --extensionDevelopmentPath=<absolute_path_to_argon_repo>/plugins/vscode"
```

With this alias defined, you can now run:

```bash
codear core/compiler/examples/argon_workspace
```

Open the `lib.ar` file within the workspace. You can then start the GUI by running `Command Palette > Argon: Start GUI`.

> [!WARNING]
> If you cannot find the command for starting the GUI but did not notice any obvious errors, you may be on an old version of VS Code.

From within the GUI, type `:openCell test()` to open the `test` cell. You should now be able to edit layouts 
in both VS Code and the GUI.

## Parametric Cell Tutorial

Create a new Argon workspace with the following command:

```bash
mkdir tutorial && touch tutorial/lib.ar
```

Your workspace directory should look like this:

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

> [!WARNING]
> Double check that there are no errors in your code editor, or the GUI will not be able to
> display the updated cell. If you make a mistake, 
> you can undo and redo changes from the GUI using `u` and `Ctrl + r`,
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
Argon writes log messages to `~/.local/state/argon/lang-server.log` (language server) and `~/local/state/argon/gui.log` (GUI).
Log level can be set using the `ARGON_LOG` environment variable
or in editor-specific configuration. If no configuration is specified, only errors will be logged.
Log level configuration follows [`RUST_LOG`](https://docs.rs/tracing-subscriber/latest/tracing_subscriber/fmt/index.html#filtering-events-with-environment-variables) syntax.

For performance, it is recommended to use `ARGON_LOG=warn` or `ARGON_LOG=error` unless you are troubleshooting an issue.

### Neovim

While the language server is running, you can open the language server logs using the `:Argon log` command 

To configure the log level, you can use the `vim.g.argon.log.level` key:

```lua
vim.g.argon = {
    -- ...
    log = {
        level = "debug"
    }
}
```

The Neovim plugin will then supply `ARGON_LOG=debug` when starting the language server and GUI.

### VS Code

While the language is running, you can open the language logs using the `Command Palette > Argon: Open Log` command.

To configure the log level, you can use the `argon.log.level` key:

```json
{
    "argon.log.level": "debug"
}
```

The VS Code plugin will then supply `ARGON_LOG=debug` when starting the language server and GUI.

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
