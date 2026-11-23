//! Hashing what a view draws, so a frame can tell what changed since the last.
//!
//! Transcript rows, blocks and selection identities are fingerprinted for
//! every row of every visible chat on every frame, so this hash must be fast
//! and spread its input well. The values never leave the process. Their
//! input includes what agents write, and the seed is fixed and public, as
//! `DefaultHasher`'s key was, so a collision could be crafted. What one would
//! cost is a row of that same transcript not drawn again, or its selection
//! confused, which does not justify SipHash's cost, more than the rest of
//! building the rows. The fixed seed also gives the same value for the same
//! input in every run.

use std::hash::BuildHasher;

pub(crate) type Hasher = foldhash::quality::FoldHasher<'static>;

/// A fresh hasher, always seeded alike.
pub(crate) fn hasher() -> Hasher {
    foldhash::quality::FixedState::default().build_hasher()
}
