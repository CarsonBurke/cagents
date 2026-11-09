//! Retained Claude actor ownership and durable delivery.
//!
//! The matched host routes native Claude through the authenticated helper.
//! The journal preserves exact input receipts and semantic outcomes; cached presentation
//! never substitutes for those durable acknowledgements.
const MAX_FRAME: usize = 34 * 1024 * 1024;

mod deadline_io;
pub(super) mod execution_exit;
mod final_snapshot;
#[cfg(test)]
pub(crate) use final_snapshot::fixture as receipt_fixture;
pub(super) mod journal;
pub(super) mod transport;
