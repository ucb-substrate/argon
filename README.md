# Argon

[![ci](https://github.com/ucb-substrate/argon/actions/workflows/ci.yml/badge.svg)](https://github.com/ucb-substrate/argon/actions/workflows/ci.yml)
[![License](https://img.shields.io/badge/License-BSD_3--Clause-blue.svg)](https://opensource.org/licenses/BSD-3-Clause)

21st century design automation tools.

## Installation/Usage

To use Argon, you will need:
- [Rust (tested on 1.90.0)](https://www.rust-lang.org/tools/install)
- One of [Neovim](https://github.com/neovim/neovim/blob/master/INSTALL.md) or [VS Code](https://code.visualstudio.com/download)

Begin by cloning and compiling the Argon source code:

```bash
git clone https://github.com/ucb-substrate/argon.git
cd argon
cargo b
```

### VS Code

To use VS Code as your code editor, you will additionally need:
- [Node JS (tested on 25.0.0)](https://nodejs.org/en/download)

First, open your VS Code user settings using Command Palette > Preferences: Open User Settings (JSON).
Add the following key:

```json
{
    "argonLsp.argonRepoDir": "<absolute_path_to_argon_repo>"
}
```

To open an example Argon workspace, run the following from the root directory of your Argon clone:

```
code --extensionDevelopmentPath=plugins/vscode core/compiler/examples/argon_workspace
```

Open the `lib.ar` file within the workspace. You can then start the GUI by running Command Palette > Argon LSP: Start GUI.

From within the GUI, type `:openCell test()` to open the `test` cell. You should now be able to view and edit layouts in both VS Code and GUI.

## Contributing

If you'd like to contribute to Argon, please let us know. You can:
* Ping us in the `#substrate` channel in the Berkeley Architecture Research Slack workspace.
* Open an issue and/or PR.
* Email `rahulkumar -AT- berkeley -DOT- edu` and `rohankumar -AT- berkeley -DOT- edu`.

Documentation updates, tests, and bugfixes are always welcome.
For larger feature additions, please discuss your ideas with us before implementing them.

Contributions can be submitted by opening a pull request against the `main` branch
of this repository.

Unless you explicitly state otherwise, any contribution intentionally submitted for inclusion
in the work by you shall be licensed under the BSD 3-Clause license, without any additional terms or conditions.
