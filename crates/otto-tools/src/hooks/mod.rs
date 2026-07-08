//! Built-in [`ToolHook`](crate::hook::ToolHook) implementations.
//!
//! Currently just [`RtkHook`], which routes shell commands through the RTK
//! (Rust Token Killer) proxy to compact noisy dev-command output.

pub mod rtk;

pub use rtk::RtkHook;
