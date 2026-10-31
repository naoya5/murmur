//! Adapters: each file wraps one external system behind a small API (guardrail G2).
//! The assembly layer (`controller.rs`, `inference.rs`, `ipc.rs`) calls them; adapters never call
//! each other's internals.

pub mod audio;
pub mod hotkeys;
pub mod output;
