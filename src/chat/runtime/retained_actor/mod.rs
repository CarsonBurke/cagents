//! Retained Claude actor ownership and durable delivery.
//!
//! The matched host routes native Claude through the authenticated helper.
//! The journal preserves exact input receipts and semantic outcomes; cached presentation
//! never substitutes for those durable acknowledgements.
const MAX_FRAME: usize = 34 * 1024 * 1024;

mod deadline_io;
mod execution_exit;
mod final_snapshot;
pub(super) mod journal;
pub(super) mod transport;
