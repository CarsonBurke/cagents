//! One counter for every cache that tells states apart by a revision.
//!
//! A value that takes a new revision on each change can be recognised as
//! unchanged by one comparison. Drawing every revision from the same counter
//! means two values hold the same revision only if one was copied from the
//! other, whichever kind of value they are.

use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Deserializer, Serialize, Serializer};

static NEXT_REVISION: AtomicU64 = AtomicU64::new(1);

/// A revision no earlier call returned.
pub fn next_revision() -> u64 {
    NEXT_REVISION.fetch_add(1, Ordering::Relaxed)
}

/// A value that can say cheaply that it has not changed.
///
/// A cache derived from a large value, such as the navigation list from the
/// sidebar's projects, is checked far more often than the value changes.
/// Comparing every entry to a saved copy costs time in proportion to the
/// value, even when nothing moved, which is nearly always. A [`Revisioned`]
/// value carries a revision that is new whenever anything could have changed
/// it, so an unchanged value is recognised by one comparison.
///
/// Revisions come from one counter shared by every value, so two values with
/// the same revision hold equal contents: a value cannot reach a revision
/// another value had unless it was cloned from it. The value is only
/// reachable for writing through [`DerefMut`], which takes a new revision
/// before handing out the reference, so a change can never keep the revision
/// it had before. A mutable borrow that changes nothing still takes one; a
/// revision says "may have changed", and the caller compares contents to tell.
///
/// Clones keep the revision, since they hold the same contents; equality
/// compares the contents alone. Only the contents are saved, and a value read
/// back takes a new revision.
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

impl<T: Serialize> Serialize for Revisioned<T> {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        self.value.serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Revisioned<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        T::deserialize(deserializer).map(Self::new)
    }
}

impl<T: IntoIterator> IntoIterator for Revisioned<T> {
    type Item = T::Item;
    type IntoIter = T::IntoIter;

    fn into_iter(self) -> Self::IntoIter {
        self.value.into_iter()
    }
}

impl<A, T: FromIterator<A>> FromIterator<A> for Revisioned<T> {
    fn from_iter<I: IntoIterator<Item = A>>(items: I) -> Self {
        Self::new(T::from_iter(items))
    }
}

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

    #[test]
    fn a_saved_value_reads_back_with_its_contents_and_a_new_revision() {
        let value = Revisioned::new(vec![1, 2]);
        let saved = serde_json::to_string(&value).unwrap();
        assert_eq!(saved, "[1,2]", "only the contents are saved");
        let read = serde_json::from_str::<Revisioned<Vec<i32>>>(&saved).unwrap();
        assert_eq!(read, value);
        assert_ne!(Revisioned::revision(&read), Revisioned::revision(&value));
    }
}
