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

    pub(in crate::app) fn failed_owner_mut(&mut self, target: Target) -> Option<&mut Owner> {
        match self.entries.get_mut(&target)? {
            Entry::Failed { owner, .. } => Some(owner),
            Entry::Pending(_) => None,
        }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::CardId;
    use std::sync::Arc;

    #[derive(Clone, Debug, PartialEq, Eq)]
    struct Owner {
        caller: u64,
        source: u64,
        handle: Option<Arc<u64>>,
    }

    fn target(card: u128, generation: u64) -> Target {
        Target {
            card: CardId::from_uuid(uuid::Uuid::from_u128(card)),
            generation,
        }
    }

    fn owner(caller: u64, source: u64) -> Owner {
        Owner {
            caller,
            source,
            handle: Some(Arc::new(source)),
        }
    }

    #[test]
    fn failed_completion_retains_exact_custody_while_a_neighbor_completes() {
        let mut ledger = Ledger::new();
        let (a, b) = (target(1, 1), target(2, 2));
        let (owner_a, owner_b) = (owner(7, 11), owner(8, 12));
        ledger.begin(a, owner_a.clone()).unwrap();
        ledger.begin(b, owner_b.clone()).unwrap();
        let failure = "original failure\nretained verbatim";
        assert!(ledger.complete(a, Err(failure.into())));
        assert_eq!(ledger.owner(a), Some(&owner_a));
        assert_eq!(ledger.failure(a), Some(failure));
        assert!(!ledger.is_pending(a));
        assert_eq!(
            ledger.failed().collect::<Vec<_>>(),
            [(a, &owner_a, failure)]
        );
        assert_eq!(ledger.pending().collect::<Vec<_>>(), [(b, &owner_b)]);

        assert!(!ledger.complete(a, Ok(())));
        assert!(!ledger.complete(a, Err("conflicting failure".into())));
        assert_eq!(ledger.failure(a), Some(failure));
        assert!(ledger.complete(b, Ok(())));
        assert!(ledger.owner(b).is_none());
        assert!(!ledger.complete(b, Err("duplicate neighbor completion".into())));
        assert_eq!(ledger.owner(a), Some(&owner_a));
        assert_eq!(ledger.failure(a), Some(failure));
    }

    #[test]
    fn explicit_retry_moves_original_owner_and_fences_old_replies() {
        let mut ledger = Ledger::new();
        let (old, next, neighbor) = (target(1, 1), target(1, 3), target(2, 2));
        let (original, unrelated) = (owner(19, 23), owner(29, 31));
        ledger.begin(old, original.clone()).unwrap();
        ledger.begin(neighbor, unrelated.clone()).unwrap();
        assert!(ledger.complete(old, Err("try explicitly".into())));

        let dispatch = ledger.retry(old, next).unwrap();
        assert_eq!(dispatch, original);
        assert!(Arc::ptr_eq(
            dispatch.handle.as_ref().unwrap(),
            original.handle.as_ref().unwrap()
        ));
        assert!(Arc::ptr_eq(
            ledger.owner(next).unwrap().handle.as_ref().unwrap(),
            original.handle.as_ref().unwrap()
        ));
        assert!(ledger.owner(old).is_none());
        assert!(ledger.is_pending(next));
        assert!(ledger.failure(next).is_none());
        assert!(!ledger.complete(old, Ok(())));
        assert!(!ledger.complete(old, Err("late failure".into())));
        assert!(ledger.retry(old, target(1, 4)).is_none());
        assert!(ledger.retry(next, target(1, 4)).is_none());
        assert_eq!(ledger.owner(next), Some(&original));
        assert_eq!(ledger.owner(neighbor), Some(&unrelated));
        assert!(ledger.is_pending(neighbor));

        assert!(ledger.complete(next, Err("retry failure".into())));
        assert!(!ledger.complete(next, Ok(())));
        let final_target = target(1, 4);
        assert_eq!(ledger.retry(next, final_target), Some(original.clone()));
        assert!(!ledger.complete(next, Ok(())));
        assert!(ledger.complete(final_target, Ok(())));
        assert!(ledger.owner(final_target).is_none());
        assert_eq!(ledger.owner(neighbor), Some(&unrelated));
    }

    #[test]
    fn begin_and_retry_collisions_preserve_both_owners_and_failure() {
        let mut ledger = Ledger::new();
        let (failed, pending, free) = (target(1, 1), target(2, 2), target(3, 3));
        let (original, neighbor, refused) = (owner(41, 43), owner(47, 53), owner(59, 61));
        ledger.begin(failed, original.clone()).unwrap();
        ledger.begin(pending, neighbor.clone()).unwrap();
        assert_eq!(ledger.begin(pending, refused.clone()), Err(refused.clone()));
        assert!(ledger.complete(failed, Err("keep this failure".into())));
        assert_eq!(ledger.begin(failed, refused.clone()), Err(refused.clone()));
        assert!(ledger.retry(failed, pending).is_none());
        assert!(ledger.retry(failed, failed).is_none());
        assert!(ledger.retry(pending, free).is_none());
        assert_eq!(ledger.owner(failed), Some(&original));
        assert_eq!(ledger.failure(failed), Some("keep this failure"));
        assert_eq!(ledger.owner(pending), Some(&neighbor));
        assert!(ledger.is_pending(pending));
        // A refused retry did not reserve an otherwise unused target.
        assert!(ledger.begin(free, refused).is_ok());
    }

    #[test]
    fn completed_and_older_unused_generations_never_accept_reused_custody() {
        let mut ledger = Ledger::new();
        let (completed, old, next) = (target(1, 1), target(2, 3), target(2, 4));
        let (first, second) = (owner(67, 71), owner(73, 79));
        ledger.begin(completed, first.clone()).unwrap();
        assert!(ledger.complete(completed, Ok(())));
        assert_eq!(ledger.begin(completed, second.clone()), Err(second.clone()));

        ledger.begin(old, second.clone()).unwrap();
        assert_eq!(
            ledger.begin(target(3, 2), first.clone()),
            Err(first.clone()),
            "an unused but older global generation is stale too"
        );
        assert!(ledger.complete(old, Err("still owned".into())));
        assert!(ledger.retry(old, completed).is_none());
        assert_eq!(ledger.failure(old), Some("still owned"));
        assert_eq!(ledger.retry(old, next), Some(second.clone()));
        assert_eq!(ledger.begin(old, first.clone()), Err(first));
        assert!(!ledger.complete(completed, Ok(())));
        assert!(!ledger.complete(old, Ok(())));
        assert!(ledger.retry(old, target(2, 5)).is_none());
        assert_eq!(ledger.owner(next), Some(&second));
        assert!(ledger.is_pending(next));
    }

    #[test]
    fn zero_and_wrapped_generations_refuse_without_losing_failed_custody() {
        let mut ledger = Ledger::new();
        let (original, refused) = (owner(83, 89), owner(97, 101));
        assert_eq!(
            ledger.begin(target(1, 0), refused.clone()),
            Err(refused.clone())
        );
        let last = target(1, u64::MAX);
        ledger.begin(last, original.clone()).unwrap();
        assert!(ledger.complete(last, Err("generation exhausted".into())));
        assert!(ledger.retry(last, target(1, 1)).is_none());
        assert_eq!(
            ledger.begin(target(2, 1), refused.clone()),
            Err(refused.clone())
        );
        assert_eq!(
            ledger.begin(target(2, u64::MAX), refused.clone()),
            Err(refused)
        );
        assert!(!ledger.complete(last, Ok(())));
        assert_eq!(ledger.owner(last), Some(&original));
        assert_eq!(ledger.failure(last), Some("generation exhausted"));
    }
}
