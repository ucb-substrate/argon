//! The Argon GUI as a WebAssembly module, for pages that host it.

#[cfg(target_family = "wasm")]
pub use argone::web::{receive, start};
