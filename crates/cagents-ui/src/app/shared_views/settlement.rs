//! Which scopes this window has seeded and published, with a journal of the
//! scopes whose membership changed.
//!
//! A complete save keeps only the scopes a publication can act on, and most
//! of what decides that is whether a scope is seeded and has a settled
//! baseline at all. [`super::discovery::ViewDiscovery`] keeps that split
//! between saves, so it must learn which scopes joined or left either key
//! set: seeding a worktree changes one scope's, and must not cost a
//! rediscovery of thousands. Each set records the scopes it gained or lost
//! until they are taken. Changing a value in place records nothing: whether a
//! baseline is empty is read afresh on each pass.
//!
//! Each set also carries an identity, new for every set and every copy, so a
//! set replaced wholesale is never mistaken for the one whose journal was
//! followed.

use std::collections::{HashMap, HashSet};
use std::ops::Deref;

use crate::sidebar::next_revision;
use crate::workspace_model::views::Scope;

/// Scopes recorded before a journal gives up and says only that it lost
/// track, which a rediscovery costs no more than replaying.
const JOURNAL_LIMIT: usize = 1024;

/// The scopes a key set gained or lost since they were last taken.
#[derive(Debug)]
struct Journal {
    identity: u64,
    changed: Vec<Scope>,
    overflowed: bool,
}

impl Default for Journal {
    fn default() -> Self {
        Self {
            identity: next_revision(),
            changed: Vec::new(),
            overflowed: false,
        }
    }
}

impl Journal {
    fn record(&mut self, scope: &Scope) {
        if self.changed.len() < JOURNAL_LIMIT {
            self.changed.push(scope.clone());
        } else {
            self.overflowed = true;
        }
    }

    fn take(&mut self) -> KeyChanges {
        let changed = std::mem::take(&mut self.changed);
        KeyChanges {
            identity: self.identity,
            changed: (!std::mem::take(&mut self.overflowed)).then_some(changed),
        }
    }
}

/// What a key set's journal held when it was taken.
#[derive(Debug)]
pub(in crate::app) struct KeyChanges {
    /// The set these changes belong to.
    pub(in crate::app) identity: u64,
    /// The scopes that joined or left it, or `None` if there were too many
    /// to record.
    pub(in crate::app) changed: Option<Vec<Scope>>,
}

/// Scopes in a set that records which ones joined or left it.
#[derive(Debug, Default)]
pub(in crate::app) struct ScopeSet {
    scopes: HashSet<Scope>,
    journal: Journal,
}

/// A copy is a new set, with a new identity and an empty journal.
impl Clone for ScopeSet {
    fn clone(&self) -> Self {
        Self {
            scopes: self.scopes.clone(),
            journal: Journal::default(),
        }
    }
}

impl Deref for ScopeSet {
    type Target = HashSet<Scope>;

    fn deref(&self) -> &HashSet<Scope> {
        &self.scopes
    }
}

impl PartialEq for ScopeSet {
    fn eq(&self, other: &Self) -> bool {
        self.scopes == other.scopes
    }
}

impl<'a> IntoIterator for &'a ScopeSet {
    type Item = &'a Scope;
    type IntoIter = std::collections::hash_set::Iter<'a, Scope>;

    fn into_iter(self) -> Self::IntoIter {
        self.scopes.iter()
    }
}

impl ScopeSet {
    /// Adds `scope`, returning whether it was new.
    pub(in crate::app) fn insert(&mut self, scope: Scope) -> bool {
        if self.scopes.contains(&scope) {
            return false;
        }
        self.journal.record(&scope);
        self.scopes.insert(scope)
    }

    /// Removes `scope`, returning whether it was there.
    #[cfg(test)]
    pub(in crate::app) fn remove(&mut self, scope: &Scope) -> bool {
        let removed = self.scopes.remove(scope);
        if removed {
            self.journal.record(scope);
        }
        removed
    }

    pub(in crate::app) fn take_changes(&mut self) -> KeyChanges {
        self.journal.take()
    }

    #[cfg(test)]
    pub(in crate::app) fn clear(&mut self) {
        *self = Self::default();
    }
}

impl Extend<Scope> for ScopeSet {
    fn extend<I: IntoIterator<Item = Scope>>(&mut self, scopes: I) {
        for scope in scopes {
            self.insert(scope);
        }
    }
}

/// A value per scope, in a map that records which scopes joined or left it.
#[derive(Debug)]
pub(in crate::app) struct ScopeMap<V> {
    values: HashMap<Scope, V>,
    journal: Journal,
}

impl<V> Default for ScopeMap<V> {
    fn default() -> Self {
        Self {
            values: HashMap::new(),
            journal: Journal::default(),
        }
    }
}

/// A copy is a new map, with a new identity and an empty journal.
impl<V: Clone> Clone for ScopeMap<V> {
    fn clone(&self) -> Self {
        Self {
            values: self.values.clone(),
            journal: Journal::default(),
        }
    }
}

impl<V> Deref for ScopeMap<V> {
    type Target = HashMap<Scope, V>;

    fn deref(&self) -> &HashMap<Scope, V> {
        &self.values
    }
}

impl<V: PartialEq> PartialEq for ScopeMap<V> {
    fn eq(&self, other: &Self) -> bool {
        self.values == other.values
    }
}

impl<'a, V> IntoIterator for &'a ScopeMap<V> {
    type Item = (&'a Scope, &'a V);
    type IntoIter = std::collections::hash_map::Iter<'a, Scope, V>;

    fn into_iter(self) -> Self::IntoIter {
        self.values.iter()
    }
}

impl<V> ScopeMap<V> {
    pub(in crate::app) fn insert(&mut self, scope: Scope, value: V) -> Option<V> {
        if let Some(slot) = self.values.get_mut(&scope) {
            return Some(std::mem::replace(slot, value));
        }
        self.journal.record(&scope);
        self.values.insert(scope, value)
    }

    #[cfg(test)]
    pub(in crate::app) fn remove(&mut self, scope: &Scope) -> Option<V> {
        let removed = self.values.remove(scope);
        if removed.is_some() {
            self.journal.record(scope);
        }
        removed
    }

    /// The value at `scope`, which keeps its key.
    pub(in crate::app) fn get_mut(&mut self, scope: &Scope) -> Option<&mut V> {
        self.values.get_mut(scope)
    }

    pub(in crate::app) fn entry_or_default(&mut self, scope: Scope) -> &mut V
    where
        V: Default,
    {
        if !self.values.contains_key(&scope) {
            self.journal.record(&scope);
        }
        self.values.entry(scope).or_default()
    }

    pub(in crate::app) fn take_changes(&mut self) -> KeyChanges {
        self.journal.take()
    }

    #[cfg(test)]
    pub(in crate::app) fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scope() -> Scope {
        Scope::Channel(crate::model::ChannelId::new())
    }

    #[test]
    fn only_a_changed_key_is_journaled() {
        let scope = scope();
        let mut set = ScopeSet::default();
        assert!(set.insert(scope.clone()));
        assert!(!set.insert(scope.clone()));
        let changes = set.take_changes();
        assert_eq!(changes.changed, Some(vec![scope.clone()]));
        assert_eq!(set.take_changes().changed, Some(Vec::new()), "taken once");

        let mut map = ScopeMap::<Vec<u8>>::default();
        map.entry_or_default(scope.clone()).push(1);
        map.entry_or_default(scope.clone()).push(2);
        map.get_mut(&scope).unwrap().clear();
        assert_eq!(map.insert(scope.clone(), vec![3]), Some(Vec::new()));
        assert_eq!(
            map.take_changes().changed,
            Some(vec![scope.clone()]),
            "values change in place"
        );
        assert!(map.remove(&scope).is_some());
        assert_eq!(map.take_changes().changed, Some(vec![scope]));
    }

    #[test]
    fn a_copy_or_an_overflowing_journal_is_told_apart() {
        let mut set = ScopeSet::default();
        let identity = set.take_changes().identity;
        assert_ne!(set.clone().take_changes().identity, identity);
        assert_ne!(ScopeSet::default().take_changes().identity, identity);
        for _ in 0..=JOURNAL_LIMIT {
            set.insert(scope());
        }
        let changes = set.take_changes();
        assert_eq!(changes.identity, identity);
        assert_eq!(changes.changed, None, "too many to replay");
        assert_eq!(set.take_changes().changed, Some(Vec::new()));
    }
}
