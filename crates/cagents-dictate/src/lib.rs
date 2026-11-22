//! Local dictation for cagents.
//!
//! Speech recognition runs in the `cagents-dictate` companion executable, not
//! in the UI: the model is a 650 MB onnxruntime graph that would otherwise
//! sit in the UI's memory and compete with its thread for the CPU. The UI
//! starts the helper when someone dictates, keeps it while they do, and lets
//! it exit when they stop, so dictation costs nothing while unused.
//!
//! This library is what both sides share: the line protocol between them
//! ([`protocol`]), the models the helper can run ([`catalog`]) and where they
//! are kept ([`store`]). It depends on serde alone. The `engine` feature adds
//! the helper's implementation, which only the helper's own build selects.

pub mod catalog;
pub mod protocol;
pub mod store;

#[cfg(feature = "engine")]
pub mod engine;
