//! Compile the real OMP decoder for UI tests without the host transport graph.
#[path = "protocol/adapter.rs"]
mod adapter;
pub(super) use adapter::{Adapter, Step};
#[path = "protocol/image_references.rs"]
mod image_references;
#[path = "protocol/omp.rs"]
pub(super) mod omp;
#[path = "protocol/terminal_output.rs"]
mod terminal_output;
