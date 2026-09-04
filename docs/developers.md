# Developer Documentation

## Testing

Run the complete test suite with `cargo test --workspace`. Cross-component test
infrastructure, Rust test cases, and fixtures live in the `argon-tests` package
under `crates/tests`. Its headless full-stack tests launch Neovim, the real
language server, and a GUI RPC harness together. They cover editor-to-GUI
recompilation, GUI-to-editor source edits, and diagnostic recovery. Neovim 0.12
or newer must be available on `PATH` to run them locally.

In normal use, Neovim launches the analyzer as an LSP child and communicates
with it over standard input and output. The unit tests instead run the real
analyzer library in-process and connect Neovim to its LSP stream over TCP using
`vim.lsp.rpc.connect()`. The analyzer's separate GUI-facing RPC port connects
to the headless GUI harness.

## Debugging

[`tracing`](https://tokio.rs/tokio/topics/tracing) is used for logging in the analyzer, compiler, and Argone.
The analyzer and Argone write tracing events to the shared
`~/.local/state/argon/argon.log` file. The log filter is configured in
`~/.config/argon/config.toml`; both paths respect their corresponding XDG
base-directory environment variables.

For example, you may add an `tracing::info!("debug");` statement to a line in the GUI 
and check the GUI log to determine whether the subsequent code is reached.

## Documentation checks

CI enforces that documentation links resolve, in two jobs that mirror the two
places documentation lives. Both are worth running locally before pushing a
change that touches docs or doc comments.

Rust doc comments are checked by rustdoc itself. The denied lints -- broken
intra-doc links among them -- are declared in `[workspace.lints.rustdoc]` in the
root `Cargo.toml`, so they apply to any `cargo doc` invocation:

Markdown files are checked by [lychee](https://lychee.cli.rs), which validates
relative file and image paths and in-page `#heading` anchors:

```bash
brew install lychee   # or: cargo install lychee --locked
lychee --config lychee.toml '**/*.md'
```

`lychee.toml` runs the checker offline, so external http(s) links are
deliberately left unchecked and the results never depend on a third-party site
being reachable.
