---
title: Install Argon
description: Install the Argon command-line tools and the Neovim plugin.
---

# Install Argon

Argon is installed from source with Cargo. The `argon` package provides four executables: `arc`, `argonc`, `argone`, and `argon-analyzer`.

## Prerequisites

- A Rust toolchain with Cargo.
- Neovim 0.12 or newer.
- Git.

## Install

```bash
cargo install --git https://github.com/ucb-substrate/argon --locked argon
```

Or, from a local checkout:

```bash
cargo install --locked --path crates/argon
```

Check that the tools are on your `PATH`:

```bash
arc --version
argone --version
```

:::note
The Rust and Neovim versions the project is tested against change over time. If an install fails, check the CI configuration in the repository for the versions currently in use.
:::

## Add the Neovim plugin

With Neovim's built-in package manager:

```lua
vim.pack.add({
  'https://github.com/ucb-substrate/argon',
})
```

The plugin detects `.ar` files and starts `argon-analyzer` from your `PATH`.

## Next

[Your first cell](./first-cell) creates a library and opens it in the editor.
