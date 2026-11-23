//! One counter for every cache that tells states apart by a revision.
//!
//! A value that takes a new revision on each change can be recognised as
//! unchanged by one comparison. Drawing every revision from the same counter
//! means two values hold the same revision only if one was copied from the
//! other, whichever kind of value they are.

use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

/// A revision no earlier call returned.
pub(crate) fn next_revision() -> u64 {
    NEXT_REVISION.fetch_add(1, Ordering::Relaxed)
}
