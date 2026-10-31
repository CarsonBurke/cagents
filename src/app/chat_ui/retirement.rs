//! In-memory custody for exact cleanup attempts. Dispatch and effects belong
//! to the caller; an owner is never inferred or replaced by this ledger.
//!
//! Each dispatch must receive a fresh `State::next_target` immediately before
//! admission. Its window-global generation must never reset, wrap, or be reused.
//! The accepted high-water mark rejects stale targets without retaining old keys;
//! after accepting `u64::MAX`, no further admission or retry can succeed.

use super::Target;
use std::collections::{HashMap, hash_map::Entry as MapEntry};

pub(in crate::app) struct Ledger<Owner: Clone> {
    entries: HashMap<Target, Entry<Owner>>,
    accepted_generation: Option<u64>,
}

enum Entry<Owner> {
    Pending(Owner),
    Failed { owner: Owner, failure: String },
}

impl<Owner: Clone> Ledger<Owner> {
    pub(in crate::app) fn new() -> Self {
        Self {
            entries: HashMap::new(),
            accepted_generation: None,
        }
    }

    /// Starts a fresh generation, returning custody unchanged on collision,
    /// stale generation, or allocator exhaustion. Zero is not a dispatch target.
    pub(in crate::app) fn begin(&mut self, target: Target, owner: Owner) -> Result<(), Owner> {
        if !self.accepts(target) {
            return Err(owner);
        }
        self.entries.insert(target, Entry::Pending(owner));
        self.accepted_generation = Some(target.generation);
        Ok(())
    }

    /// Only the first pending completion applies. A failure retains its owner
    /// and exact message until an explicit retry, including after a late Ok.
    pub(in crate::app) fn complete(&mut self, target: Target, result: Result<(), String>) -> bool {
        let MapEntry::Occupied(entry) = self.entries.entry(target) else {
            return false;
        };
        if !matches!(entry.get(), Entry::Pending(_)) {
            return false;
        }
        match result {
            Ok(()) => {
                entry.remove();
            }
            Err(failure) => {
                let Entry::Pending(owner) = entry.remove() else {
                    unreachable!("pending completion checked above");
                };
                self.entries
                    .insert(target, Entry::Failed { owner, failure });
            }
        }
        true
    }

    /// Moves failed custody to a fresh generation and returns a dispatch
    /// clone. Refusal leaves both the failed owner and every other entry intact.
    pub(in crate::app) fn retry(&mut self, failed: Target, next: Target) -> Option<Owner> {
        if !self.accepts(next) {
            return None;
        }
        let Entry::Failed { owner, .. } = self.entries.get(&failed)? else {
            return None;
        };
        // Clone before changing custody, so a panicking Clone cannot consume it.
        let dispatch = owner.clone();
        let Some(Entry::Failed { owner, .. }) = self.entries.remove(&failed) else {
            unreachable!("failed retry checked above");
        };
        self.entries.insert(next, Entry::Pending(owner));
        self.accepted_generation = Some(next.generation);
        Some(dispatch)
    }

    fn accepts(&self, target: Target) -> bool {
        target.generation != 0
            && self
                .accepted_generation
                .is_none_or(|accepted| target.generation > accepted)
    }

    pub(in crate::app) fn owner(&self, target: Target) -> Option<&Owner> {
        self.entries.get(&target).map(|entry| match entry {
            Entry::Pending(owner) | Entry::Failed { owner, .. } => owner,
        })
    }

    pub(in crate::app) fn owner_mut(&mut self, target: Target) -> Option<&mut Owner> {
        self.entries.get_mut(&target).map(|entry| match entry {
            Entry::Pending(owner) | Entry::Failed { owner, .. } => owner,
        })
    }

    pub(in crate::app) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub(in crate::app) fn iter(&self) -> impl Iterator<Item = &Target> {
        self.entries.keys()
    }

    pub(in crate::app) fn owners(&self) -> impl Iterator<Item = &Owner> {
        self.entries.values().map(|entry| match entry {
            Entry::Pending(owner) | Entry::Failed { owner, .. } => owner,
        })
    }

    pub(in crate::app) fn failure(&self, target: Target) -> Option<&str> {
        match self.entries.get(&target)? {
            Entry::Failed { failure, .. } => Some(failure),
            Entry::Pending(_) => None,
        }
    }

    pub(in crate::app) fn is_pending(&self, target: Target) -> bool {
        matches!(self.entries.get(&target), Some(Entry::Pending(_)))
    }

    pub(in crate::app) fn pending(&self) -> impl Iterator<Item = (Target, &Owner)> {
        self.entries
            .iter()
            .filter_map(|(&target, entry)| match entry {
                Entry::Pending(owner) => Some((target, owner)),
                Entry::Failed { .. } => None,
            })
    }

    pub(in crate::app) fn failed(&self) -> impl Iterator<Item = (Target, &Owner, &str)> {
        self.entries
            .iter()
            .filter_map(|(&target, entry)| match entry {
                Entry::Failed { owner, failure } => Some((target, owner, failure.as_str())),
                Entry::Pending(_) => None,
            })
    }
}

impl<Owner: Clone> Default for Ledger<Owner> {
    fn default() -> Self {
        Self::new()
    }
}
