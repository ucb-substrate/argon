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
The analyzer and Argone write tracing events to `~/.local/state/argon/analyzer.log` and `~/.local/state/argon/argone.log`, respectively.

For example, you may add an `tracing::info!("debug");` statement to a line in the GUI 
and check the GUI log to determine whether the subsequent code is reached.
