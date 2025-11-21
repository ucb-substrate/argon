# Developer Documentation

## Debugging

[`tracing`](https://tokio.rs/tokio/topics/tracing) is used for logging in the language server, compiler, and GUI.
The language server and GUI write tracing events to `~/.local/state/argon/lang-server.log` and `~/local/state/argon/gui.log`, respectively.

For example, you may add an `tracing::info!("debug");` statement to a line in the GUI 
and check the GUI log to determine whether the subsequent code is reached.
