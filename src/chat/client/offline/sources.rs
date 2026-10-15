//! Admission for projected source bodies in a private last-view publication.
//!
//! Depends on the shared #766 source_owners ledger. Inline IPC windows remain
//! separately bounded; a disk reader reserves every full source before serde
//! can allocate it. These are logical payload charges, never RSS estimates.
use crate::chat::{
    Item, Snapshot,
    cache::{SourceKey, SourceMetadata, SourcePart},
    source_owners::{self, Reservation, ResidentSource},
};
use std::{collections::HashSet, sync::Arc};

#[derive(Clone)]
pub(super) struct Manifest {
    sources: Vec<ResidentSource>,
    body_bytes: usize,
}
impl Manifest {
    pub(super) fn capture(snapshot: &Snapshot) -> Self {
        Self {
            sources: source_owners::resident_sources(snapshot),
            body_bytes: snapshot.transcript_bytes(),
        }
    }

    pub(super) fn reserve(&self) -> Result<Reservation, String> {
        let total = self.sources.iter().try_fold(0usize, |sum, record|
            sum.checked_add(record.bytes)).ok_or("Cached source size overflow")?;
        if total > self.body_bytes || self.body_bytes - total > crate::terminal_ipc::MAX_FRAME_BYTES {
            return Err("Cached inline bodies exceed the original IPC window".into());
        }
        let largest = self.sources.iter().map(|record| record.bytes).max().unwrap_or(0);
        source_owners::reserve(total, largest)
            .map_err(|error| format!("Cached source admission unavailable: {error:?}"))
    }

    fn items(&self, snapshot: &Snapshot) -> Result<Vec<(Arc<Item>, ResidentSource)>, String> {
        if snapshot.transcript_bytes() != self.body_bytes {
            return Err("Cached view body length differs from its private manifest".into());
        }
        let mut keys = HashSet::new();
        let mut items = Vec::with_capacity(self.sources.len());
        for record in &self.sources {
            if !keys.insert(record.source.clone()) {
                return Err("Duplicate cached source identity".into());
            }
            let candidates = match &record.source.agent_id {
                None => &snapshot.items,
                Some(id) => &snapshot.subagents.iter().find(|child| &child.id == id)
                    .ok_or("Cached source child is missing")?.items,
            };
            let mut matching = candidates.iter().filter(|item| item.id == record.source.source_id);
            let item = matching.next().ok_or("Cached source root is missing")?;
            if matching.next().is_some() {
                return Err("Ambiguous cached source root".into());
            }
            let crate::chat::ItemContent::Text { text, .. } = &item.content else {
                return Err("Cached source root is not text".into());
            };
            if text.len() != record.bytes || SourceMetadata::from_item(item).as_ref() != Some(&record.metadata) {
                return Err("Cached source differs from its frozen provenance".into());
            }
            items.push((Arc::clone(item), record.clone()));
        }
        Ok(items)
    }

    pub(super) fn adopt(&self, snapshot: &mut Snapshot, reservation: Reservation) -> Result<(), String> {
        let items = self.items(snapshot)?;
        reservation.adopt(&items)
            .map_err(|error| format!("Cached source ownership transfer failed: {error:?}"))?;
        self.normalize_parts(snapshot);
        Ok(())
    }

    pub(super) fn normalize(&self, snapshot: &mut Snapshot) -> Result<(), String> {
        // The fallback consists of the original charged Arcs. Do not reserve
        // again or manufacture fresh leases just to retain those same owners.
        self.items(snapshot)?;
        self.normalize_parts(snapshot);
        Ok(())
    }

    fn normalize_parts(&self, snapshot: &mut Snapshot) {
        let keys: HashSet<&SourceKey> = self.sources.iter().map(|record| &record.source).collect();
        snapshot.source_parts.retain(|part| !keys.contains(&part.source));
        snapshot.source_parts.extend(self.sources.iter().map(|record| SourcePart {
            item_id: record.source.source_id.clone(), source: record.source.clone(),
            start: 0, end: record.bytes, source_len: record.bytes, replacement: false,
            epoch: record.epoch, metadata: record.metadata.clone(),
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ItemContent, Phase, Role};
    fn source_epoch(incarnation: u128, revision: u64) -> crate::chat::cache::SourceEpoch {
        crate::chat::cache::SourceEpoch { incarnation: uuid::Uuid::from_u128(incarnation), revision }
    }


    fn fixture() -> (Snapshot, ResidentSource) {
        let item = Arc::new(Item {
            id: "opaque-root:native-part:0".into(), role: Role::Assistant,
            content: ItemContent::Text { text: "frozen viewed source".into(), thinking: true, phase: Phase::Final },
            at: Some(17), started_at: Some(11),
        });
        let record = ResidentSource {
            source: SourceKey { agent_id: None, source_id: item.id.clone() },
            bytes: "frozen viewed source".len(), epoch: source_epoch(1, 7),
            metadata: SourceMetadata::from_item(&item).unwrap(),
        };
        let snapshot = Snapshot { items: vec![item], ..Snapshot::default() };
        (snapshot, record)
    }

    #[test]
    fn cached_prefix_keeps_its_admitted_epoch_and_metadata() {
        let (mut snapshot, record) = fixture();
        let untouched = SourcePart {
            item_id: "unloaded-window".into(),
            source: SourceKey { agent_id: None, source_id: "another-source".into() },
            start: 100, end: 120, source_len: 4096, replacement: true, epoch: source_epoch(1, 88),
            metadata: record.metadata.clone(),
        };
        snapshot.source_parts = vec![SourcePart {
            item_id: "newer-replacement-window".into(), source: record.source.clone(),
            start: 10, end: 12, source_len: 9999, replacement: true, epoch: source_epoch(2, 7),
            metadata: SourceMetadata { at: Some(999), ..record.metadata.clone() },
        }, untouched.clone()];
        let manifest = Manifest { sources: vec![record.clone()], body_bytes: snapshot.transcript_bytes() };
        manifest.normalize(&mut snapshot).unwrap();
        assert_eq!(snapshot.source_parts.len(), 2);
        assert_eq!(snapshot.source_parts[0], untouched, "unloaded sources stay honest partial windows");
        let restored = &snapshot.source_parts[1];
        assert_eq!((restored.start, restored.end, restored.source_len, restored.epoch), (0, record.bytes, record.bytes, source_epoch(1, 7)));
        assert_eq!(restored.metadata, record.metadata);
        assert_eq!(restored.item_id, record.source.source_id);
        assert!(!restored.replacement);
    }

    #[test]
    fn malformed_manifest_never_relabels_an_unrelated_or_rewritten_item() {
        let (snapshot, record) = fixture();
        let mut manifest = Manifest { sources: vec![record], body_bytes: snapshot.transcript_bytes() };
        manifest.sources[0].metadata.started_at = Some(900);
        assert!(manifest.items(&snapshot).is_err());
        let mut duplicate = snapshot.clone();
        duplicate.items.push(duplicate.items[0].clone());
        manifest.body_bytes = duplicate.transcript_bytes();
        assert!(manifest.items(&duplicate).is_err());
        manifest.sources[0].source.agent_id = Some("different-child".into());
        assert!(manifest.items(&snapshot).is_err());
    }

    #[test]
    fn multi_view_capacity_is_denied_before_allocating_source_text() {
        let (_, record) = fixture();
        let sources: Vec<_> = (0..3).map(|index| ResidentSource {
            source: SourceKey { source_id: format!("large-{index}"), agent_id: None },
            bytes: source_owners::SOURCE_BYTES, ..record.clone()
        }).collect();
        let manifest = Manifest { sources, body_bytes: 3 * source_owners::SOURCE_BYTES };
        assert!(manifest.reserve().is_err(), "96 MiB must not bypass the shared 72 MiB predecode pool");
        let manifest = Manifest { sources: Vec::new(), body_bytes: crate::terminal_ipc::MAX_FRAME_BYTES + 1 };
        assert!(manifest.reserve().is_err(), "uncharged inline bodies retain their IPC bound");
    }

    #[test]
    fn decoded_source_lease_follows_the_actual_restored_item() {
        let (mut snapshot, record) = fixture();
        let weak = Arc::downgrade(&snapshot.items[0]);
        let manifest = Manifest { sources: vec![record.clone()], body_bytes: snapshot.transcript_bytes() };
        manifest.adopt(&mut snapshot, manifest.reserve().unwrap()).unwrap();
        assert_eq!(source_owners::resident_sources(&snapshot), vec![record]);
        let retained = snapshot.items[0].clone();
        drop(snapshot);
        assert!(weak.upgrade().is_some());
        drop(retained);
        assert!(weak.upgrade().is_none());
    }
}
