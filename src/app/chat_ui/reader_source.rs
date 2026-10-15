//! Body-free reader geometry fences. A same-source reload restores its offsets;
//! a verified append can keep them, while a replacement starts at native defaults.
use std::{collections::VecDeque, sync::Arc};

#[derive(Clone, Debug)]
pub(super) struct Source {
    digest: blake3::Hash,
    // Allocate incremental state only once this source actually grows.
    hasher: Option<Box<blake3::Hasher>>,
    ancestors: Arc<VecDeque<blake3::Hash>>,
}
impl Source {
    pub fn new(text: &str) -> Self {
        Self { digest: blake3::hash(text.as_bytes()), hasher: None, ancestors: Arc::default() }
    }
    pub fn digest(&self) -> blake3::Hash { self.digest }
    pub fn accepts(&self, previous: blake3::Hash) -> bool {
        self.digest == previous || self.ancestors.contains(&previous)
    }
    /// Caller has established that `suffix` extends this exact `previous` body.
    pub fn append(&mut self, previous: &str, suffix: &str) {
        if suffix.is_empty() { return; }
        let ancestors = Arc::make_mut(&mut self.ancestors);
        if ancestors.len() == 256 { ancestors.pop_front(); }
        ancestors.push_back(self.digest);
        let hasher = self.hasher.get_or_insert_with(|| {
            let mut hasher = Box::new(blake3::Hasher::new());
            hasher.update(previous.as_bytes()); hasher
        });
        hasher.update(suffix.as_bytes()); self.digest = hasher.finalize();
    }
    pub fn updated(previous: &Self, old: &str, new: &str) -> Self {
        if let Some(suffix) = new.strip_prefix(old) {
            let mut source = previous.clone(); source.append(old, suffix); source
        } else { Self::new(new) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_source_reload_and_append_accept_geometry_but_replacement_does_not() {
        let mut source = Source::new("wide line"); let original = source.digest();
        source.append("wide line", "\nshort");
        assert!(source.accepts(original));
        assert!(Source::new("wide line\nshort").accepts(source.digest()));
        assert!(!Source::new("short").accepts(original));
        for _ in 0..300 { source.append("", "x"); }
        assert_eq!(source.ancestors.len(), 256);
        assert!(!source.accepts(original));
    }
}
