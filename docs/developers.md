# Developer Documentation

## Debugging

[`tracing`](https://tokio.rs/tokio/topics/tracing) is used for logging in the analyzer, compiler, and Argone.
The analyzer and Argone write tracing events to `~/.local/state/argon/analyzer.log` and `~/.local/state/argon/argone.log`, respectively.

For example, you may add an `tracing::info!("debug");` statement to a line in the GUI 
and check the GUI log to determine whether the subsequent code is reached.
