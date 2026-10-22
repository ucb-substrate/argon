//! The protocol between the Argon analyzer and its GUI.
//!
//! These types compile for the web, so a GUI running in a browser speaks the
//! same protocol as the native one.

pub mod config;
pub mod rpc;
pub mod wire;
