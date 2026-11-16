//! Which scopes this window has seeded and published, with a generation for
//! their key sets.
//!
//! A complete save keeps only the scopes a publication can act on, and most
//! of what decides that is whether a scope is seeded and has a settled
//! baseline at all. [`super::ViewDiscovery`] keeps that split between saves,
//! so it must learn when either key set changes. Every change of a key set
//! draws a new generation from one process-wide counter, as does every new
//! set, so equal generations mean the same set with the same keys. Changing a
//! value in place leaves the generation alone: whether a baseline is empty is
//! read afresh on each pass.

use std::collections::{HashMap, HashSet};
use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::workspace_model::views::Scope;

static GENERATION: AtomicU64 = AtomicU64::new(1);

/// A value no earlier call returned, for caches to tell states apart.
pub(in crate::app) fn next_generation() -> u64 {
    GENERATION.fetch_add(1, Ordering::Relaxed)
}

/// Scopes in a set whose membership carries a generation. A copy keeps the
/// generation, as it keeps the keys.
#[derive(Clone, Debug)]
pub(in crate::app) struct ScopeSet {
    scopes: HashSet<Scope>,
    generation: u64,
}

impl Default for ScopeSet {
    fn default() -> Self {
        Self {
            scopes: HashSet::new(),
            generation: next_generation(),
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
    pub(in crate::app) fn generation(&self) -> u64 {
        self.generation
    }

    /// Adds `scope`, returning whether it was new.
    pub(in crate::app) fn insert(&mut self, scope: Scope) -> bool {
        let added = self.scopes.insert(scope);
        if added {
            self.generation = next_generation();
        }
        added
    }

    /// Removes `scope`, returning whether it was there.
    #[cfg(test)]
    pub(in crate::app) fn remove(&mut self, scope: &Scope) -> bool {
        let removed = self.scopes.remove(scope);
        if removed {
            self.generation = next_generation();
        }
        removed
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

/// A value per scope, whose key set carries a generation. A copy keeps the
/// generation, as it keeps the keys.
#[derive(Clone, Debug)]
pub(in crate::app) struct ScopeMap<V> {
    values: HashMap<Scope, V>,
    generation: u64,
}

impl<V> Default for ScopeMap<V> {
    fn default() -> Self {
        Self {
            values: HashMap::new(),
            generation: next_generation(),
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
    pub(in crate::app) fn generation(&self) -> u64 {
        self.generation
    }

    pub(in crate::app) fn insert(&mut self, scope: Scope, value: V) -> Option<V> {
        let previous = self.values.insert(scope, value);
        if previous.is_none() {
            self.generation = next_generation();
        }
        previous
    }

    #[cfg(test)]
    pub(in crate::app) fn remove(&mut self, scope: &Scope) -> Option<V> {
        let removed = self.values.remove(scope);
        if removed.is_some() {
            self.generation = next_generation();
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
            self.generation = next_generation();
        }
        self.values.entry(scope).or_default()
    }

    #[cfg(test)]
    pub(in crate::app) fn clear(&mut self) {
        *self = Self::default();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_changed_key_set_draws_a_new_generation() {
        let scope = Scope::Channel(crate::model::ChannelId::new());
        let mut set = ScopeSet::default();
        let other = ScopeSet::default();
        assert_ne!(set.generation(), other.generation(), "a new set is new");
        let empty = set.generation();
        assert!(set.insert(scope.clone()));
        let one = set.generation();
        assert_ne!(one, empty);
        assert!(!set.insert(scope.clone()));
        assert_eq!(set.generation(), one);

        let mut map = ScopeMap::<Vec<u8>>::default();
        let empty = map.generation();
        map.entry_or_default(scope.clone()).push(1);
        let one = map.generation();
        assert_ne!(one, empty);
        map.entry_or_default(scope.clone()).push(2);
        map.get_mut(&scope).unwrap().clear();
        assert_eq!(map.insert(scope.clone(), vec![3]), Some(Vec::new()));
        assert_eq!(map.generation(), one, "values change in place");
    }
}
