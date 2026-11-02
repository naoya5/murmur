//! Adapters: each file wraps one external system behind a small API (guardrail G2).
//! The assembly layer (`controller.rs`, `inference.rs`, `ipc.rs`) calls them; adapters never call
//! each other's internals.

pub mod audio;
pub mod engine;
pub mod hotkeys;
pub mod image;
pub mod models;
pub mod output;
pub mod pointer;
pub mod screen;
pub mod settings_file;
pub mod sheet;
pub mod sounds;
pub mod store;
