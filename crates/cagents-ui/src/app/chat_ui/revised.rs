//! Values that carry a stamp of their latest edit.
//!
//! A view that derives something expensive from a value can remember the
//! stamp it derived from and compare stamps instead of the values themselves.

use std::ops::Deref;
use std::sync::atomic::{AtomicU64, Ordering};

/// Identifies one state of a [`Revised`] value, or of anything else that
/// takes a fresh stamp whenever it changes.
///
/// Stamps come from one process-wide sequence, so no two states ever share a
/// stamp: a value that replaces another, or that is reset to its default, can
/// never be mistaken for the one it replaced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(super) struct Stamp(u64);

impl Stamp {
    pub(super) fn fresh() -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        Self(NEXT.fetch_add(1, Ordering::Relaxed))
    }
}

/// A new state, which is why the default is fresh rather than zero.
impl Default for Stamp {
    fn default() -> Self {
        Self::fresh()
    }
}

/// A value whose every mutable access takes a fresh [`Stamp`].
///
/// Reads go through `Deref`. Writes go through [`Revised::edit`], so a write
/// that might change nothing should check first: a stamp that moves without a
/// change only costs whoever derives from it a needless rebuild.
#[derive(Clone, Default)]
pub(super) struct Revised<T> {
    value: T,
    stamp: Stamp,
}

impl<T> Revised<T> {
    pub(super) fn new(value: T) -> Self {
        Self {
            value,
            stamp: Stamp::fresh(),
        }
    }

    pub(super) fn stamp(&self) -> Stamp {
        self.stamp
    }

    pub(super) fn edit(&mut self) -> &mut T {
        self.stamp = Stamp::fresh();
        &mut self.value
    }
}

impl<T> From<T> for Revised<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T> Deref for Revised<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<'a, T> IntoIterator for &'a Revised<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.value.into_iter()
    }
}

/// Compares the values; the stamps say only when each was last edited.
impl<T: PartialEq> PartialEq<T> for Revised<T> {
    fn eq(&self, other: &T) -> bool {
        self.value == *other
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Revised<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(formatter)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_edit_and_every_new_value_takes_a_stamp_never_used_before() {
        let mut set = Revised::<HashSet<&str>>::default();
        let mut seen = HashSet::from([set.stamp()]);
        let reads = set.stamp();
        assert!(!set.contains("a") && set.is_empty());
        assert_eq!(set.stamp(), reads, "reading keeps the stamp");
        set.edit().insert("a");
        assert!(seen.insert(set.stamp()), "an edit takes a fresh stamp");
        let copy = set.clone();
        assert_eq!(copy.stamp(), set.stamp(), "a copy holds the same state");
        let replaced = set.stamp();
        set = Revised::default();
        assert!(seen.insert(set.stamp()), "a reset never reuses a stamp");
        set = HashSet::from(["a"]).into();
        assert_ne!(set.stamp(), replaced, "equal content is still a new state");
        assert!(seen.insert(set.stamp()));
        assert_eq!(set, HashSet::from(["a"]));
    }
}
