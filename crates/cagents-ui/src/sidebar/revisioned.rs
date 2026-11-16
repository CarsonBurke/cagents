//! A value that can say cheaply that it has not changed.
//!
//! The navigation list is rebuilt only when its inputs change, and the check
//! runs on every frame. Comparing every project, worktree and channel to a
//! saved copy costs time in proportion to the workspace, even when nothing
//! moved, which is nearly always. A [`Revisioned`] value carries a revision
//! that is new whenever anything could have changed it, so an unchanged value
//! is recognised by one comparison.
//!
//! Revisions come from one counter shared by every value, so two values with
//! the same revision hold equal contents: a value cannot reach a revision
//! another value had unless it was cloned from it. The value is only
//! reachable for writing through [`DerefMut`], which takes a new revision
//! before handing out the reference, so a change can never keep the revision
//! it had before. A mutable borrow that changes nothing still takes one; a
//! revision says "may have changed", and the caller compares contents to tell.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};

static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

/// A revision no earlier call returned. Other caches that tell states apart
/// by a revision draw theirs here too, so one never repeats another's.
pub(crate) fn next_revision() -> u64 {
    NEXT_REVISION.fetch_add(1, Ordering::Relaxed)
}

/// Clones keep the revision, since they hold the same contents; equality
/// compares the contents alone.
#[derive(Clone)]
pub struct Revisioned<T> {
    value: T,
    revision: u64,
}

impl<T> Revisioned<T> {
    pub fn new(value: T) -> Self {
        Self {
            value,
            revision: next_revision(),
        }
    }

    /// Equal for two values only while both hold the contents one of them
    /// was last given or changed to.
    pub fn revision(this: &Self) -> u64 {
        this.revision
    }
}

impl<T> Deref for Revisioned<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.value
    }
}

impl<T> DerefMut for Revisioned<T> {
    fn deref_mut(&mut self) -> &mut T {
        self.revision = next_revision();
        &mut self.value
    }
}

impl<T: Default> Default for Revisioned<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

impl<T> From<T> for Revisioned<T> {
    fn from(value: T) -> Self {
        Self::new(value)
    }
}

impl<T: std::fmt::Debug> std::fmt::Debug for Revisioned<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.value.fmt(formatter)
    }
}

impl<T: PartialEq> PartialEq for Revisioned<T> {
    fn eq(&self, other: &Self) -> bool {
        self.value == other.value
    }
}

impl<T: Eq> Eq for Revisioned<T> {}

impl<'a, T> IntoIterator for &'a Revisioned<T>
where
    &'a T: IntoIterator,
{
    type Item = <&'a T as IntoIterator>::Item;
    type IntoIter = <&'a T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        (&**self).into_iter()
    }
}

impl<'a, T> IntoIterator for &'a mut Revisioned<T>
where
    &'a mut T: IntoIterator,
{
    type Item = <&'a mut T as IntoIterator>::Item;
    type IntoIter = <&'a mut T as IntoIterator>::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        (&mut **self).into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::Revisioned;

    #[test]
    fn only_a_mutable_borrow_or_a_new_value_takes_a_new_revision() {
        let mut value = Revisioned::new(vec![1, 2]);
        let first = Revisioned::revision(&value);
        assert_eq!(value.len(), 2);
        assert_eq!(value.iter().sum::<i32>(), 3);
        assert_eq!(Revisioned::revision(&value), first, "reading keeps it");

        let copy = value.clone();
        assert_eq!(Revisioned::revision(&copy), first, "a copy holds the same");

        value.push(3);
        let pushed = Revisioned::revision(&value);
        assert_ne!(pushed, first);
        assert_eq!(Revisioned::revision(&copy), first, "the copy is untouched");

        for entry in &mut value {
            *entry += 0;
        }
        assert_ne!(
            Revisioned::revision(&value),
            pushed,
            "a borrow for writing may have changed it"
        );

        let replaced = Revisioned::new(vec![1, 2]);
        assert_eq!(replaced, copy, "equality compares contents");
        assert_ne!(
            Revisioned::revision(&replaced),
            first,
            "equal contents built anew never reuse a revision"
        );
    }
}
