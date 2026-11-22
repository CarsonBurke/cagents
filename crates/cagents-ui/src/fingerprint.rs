//! Hashing what a view draws, so a frame can tell what changed since the last.
//!
//! Transcript rows, blocks and selection identities are fingerprinted for
//! every row of every visible chat on every frame. Those values are compared
//! only within this process and are never keyed by input someone else
//! chooses, so they need a fast hash that spreads its input well, not
//! SipHash's resistance to chosen collisions, which cost more than the rest
//! of building the rows. The seed is fixed, so a value is the same for the
//! same input in every run.

use std::hash::BuildHasher;

pub(crate) type Hasher = foldhash::quality::FoldHasher<'static>;

/// A fresh hasher, always seeded alike.
pub(crate) fn hasher() -> Hasher {
    foldhash::quality::FixedState::default().build_hasher()
}
