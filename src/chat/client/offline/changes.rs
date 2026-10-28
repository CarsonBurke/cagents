//! Frozen, typed Changes sidecar proofs for private last-view files.
//!
//! Headers retain file-list metadata, never patch bodies. Every decoded full
//! patch is reserved with prose before serde, then adopted by its exact Arc.
use crate::chat::{
    Snapshot,
    cache::changes::{Manifest as Page, ManifestQuery},
    source_owners::{self, ResidentChanges, ResidentPatch},
};
use std::{collections::HashSet, sync::Arc};

#[derive(Clone, Default)]
pub(super) struct Manifest {
    pages: Vec<Arc<Page>>,
    patches: Vec<ResidentPatch>,
    pub(super) bytes: usize,
    pub(super) largest: usize,
}
impl Manifest {
    pub(super) fn capture(snapshot: &Snapshot, view: &ResidentChanges) -> Result<Self, String> {
        validate_pages(snapshot, &view.manifests)?;
        let mut seen = HashSet::new();
        let mut bytes = 0usize;
        let mut largest = 0;
        for (text, record) in &view.patches {
            if !seen.insert((record.source.clone(), record.file.file_index))
                || text.len() != record.file.source_len
                || source_owners::resident_patch(text).as_ref() != Some(record)
            {
                return Err("The captured patch lacks its unique admitted owner".into());
            }
            validate_patch(&view.manifests, record)?;
            bytes = bytes
                .checked_add(text.len())
                .ok_or("Cached patch size overflow")?;
            largest = largest.max(text.len());
        }
        Ok(Self {
            pages: view.manifests.clone(),
            patches: view
                .patches
                .iter()
                .map(|(_, record)| record.clone())
                .collect(),
            bytes,
            largest,
        })
    }

    pub(super) fn validate(
        &self,
        snapshot: &Snapshot,
        view: &ResidentChanges,
    ) -> Result<(), String> {
        if view.manifests != self.pages || view.patches.len() != self.patches.len() {
            return Err("The decoded file list differs from its captured proof".into());
        }
        validate_pages(snapshot, &view.manifests)?;
        for ((text, actual), expected) in view.patches.iter().zip(&self.patches) {
            if actual != expected || text.len() != expected.file.source_len {
                return Err("The decoded patch differs from its captured proof".into());
            }
            validate_patch(&view.manifests, actual)?;
        }
        Ok(())
    }

    pub(super) fn validate_live(
        &self,
        snapshot: &Snapshot,
        view: &ResidentChanges,
    ) -> Result<(), String> {
        self.validate(snapshot, view)?;
        if view
            .patches
            .iter()
            .any(|(text, record)| source_owners::resident_patch(text).as_ref() != Some(record))
        {
            return Err("The fallback patch has lost its admitted owner".into());
        }
        Ok(())
    }
}

fn validate_pages(snapshot: &Snapshot, pages: &[Arc<Page>]) -> Result<(), String> {
    let mut seen = HashSet::new();
    let mut files = HashSet::new();
    let mut metadata_bytes = 0usize;
    for page in pages {
        let query = ManifestQuery {
            source: page.source.clone(),
            offset: page.offset,
            limit: crate::chat::cache::changes::MANIFEST_FILES,
            expected_epoch: Some(page.epoch),
        };
        let matching = snapshot.changes_parts.iter().any(|part| {
            part.source == page.source
                && part.epoch == page.epoch
                && part.metadata.total_files == page.metadata.total_files
                && part.metadata.total_bytes == page.metadata.total_bytes
        });
        if !matching
            || !page.matches_query(&query)
            || !seen.insert((page.source.clone(), page.offset))
        {
            return Err("The cached file list is not part of this exact source projection".into());
        }
        // Logical metadata has its own limit; it is not charged as patch text.
        metadata_bytes = metadata_bytes
            .checked_add(std::mem::size_of::<Page>())
            .and_then(|sum| sum.checked_add(page.source.source_id.len()))
            .and_then(|sum| sum.checked_add(page.source.agent_id.as_ref().map_or(0, String::len)))
            .ok_or("Cached file metadata size overflow")?;
        for file in &page.files {
            if !files.insert((page.source.clone(), file.file_index)) {
                return Err("The cached file list repeats a source file".into());
            }
            metadata_bytes = metadata_bytes
                .checked_add(std::mem::size_of_val(file))
                .and_then(|sum| sum.checked_add(file.path.len()))
                .ok_or("Cached file metadata size overflow")?;
        }
        if metadata_bytes > crate::terminal_ipc::MAX_FRAME_BYTES {
            return Err("The cached file metadata exceeds its separate inline budget".into());
        }
    }
    Ok(())
}

fn validate_patch(pages: &[Arc<Page>], record: &ResidentPatch) -> Result<(), String> {
    let mut candidates = pages
        .iter()
        .filter(|page| page.source == record.source && page.epoch == record.epoch)
        .flat_map(|page| page.files.iter())
        .filter(|file| **file == record.file);
    if candidates.next().is_none() || candidates.next().is_some() {
        return Err("The cached patch lacks its exact original file proof".into());
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::chat::{
        ChangeKind, Role, ToolStatus,
        cache::{
            SourceEpoch, SourceKey,
            changes::{File, Metadata, Part},
        },
    };

    pub(in crate::chat::client::offline) fn fixture() -> (Snapshot, ResidentChanges) {
        let source = SourceKey {
            agent_id: None,
            source_id: "opaque-root:changes:0:7".into(),
        };
        let epoch = SourceEpoch {
            incarnation: uuid::Uuid::from_u128(7),
            revision: 19,
        };
        let text: Arc<str> =
            Arc::from("--- a/repeated.rs\n+++ b/repeated.rs\n@@ -1 +1 @@\n-old\n+complete tail\n");
        let file = File {
            file_index: 0,
            path: "repeated.rs".into(),
            kind: ChangeKind::Updated,
            source_len: text.len(),
            additions: Some(1),
            deletions: Some(1),
        };
        let metadata = Metadata {
            role: Role::Assistant,
            status: ToolStatus::Succeeded,
            at: Some(29),
            started_at: Some(23),
            total_files: 1,
            total_bytes: text.len(),
            additions: Some(1),
            deletions: Some(1),
            unavailable: None,
        };
        let page = Arc::new(Page {
            source: source.clone(),
            epoch,
            metadata: metadata.clone(),
            offset: 0,
            next_offset: None,
            files: vec![file.clone()],
            omitted_files: 0,
        });
        let record = ResidentPatch {
            source: source.clone(),
            epoch,
            file,
        };
        assert_eq!(
            source_owners::admit_patch(&text, &record, None),
            source_owners::Admission::Ready
        );
        let snapshot = Snapshot {
            changes_parts: vec![Part {
                item_id: source.source_id.clone(),
                source,
                epoch,
                metadata,
                files: Vec::new(),
                replacement: true,
            }],
            ..Snapshot::default()
        };
        (
            snapshot,
            ResidentChanges {
                manifests: vec![page],
                patches: vec![(text, record)],
            },
        )
    }

    #[test]
    fn decoded_full_patch_is_admitted_before_exposure_with_exact_file_proof() {
        let (snapshot, original) = fixture();
        let header =
            super::super::sources::Manifest::capture_changes(&snapshot, &original).unwrap();
        let reservation = header.reserve().unwrap();
        let encoded = serde_json::to_vec(&super::super::View {
            snapshot: Arc::new(snapshot),
            changes: original.clone(),
            raw: Default::default(),
        })
        .unwrap();
        let mut decoded: super::super::View = serde_json::from_slice(&encoded).unwrap();
        assert!(!Arc::ptr_eq(
            &original.patches[0].0,
            &decoded.changes.patches[0].0
        ));
        assert!(source_owners::resident_patch(&decoded.changes.patches[0].0).is_none());
        header
            .adopt_changes(
                Arc::make_mut(&mut decoded.snapshot),
                &decoded.changes,
                reservation,
            )
            .unwrap();
        assert_eq!(
            source_owners::resident_patch(&decoded.changes.patches[0].0),
            Some(original.patches[0].1.clone())
        );
        assert!(decoded.changes.patches[0].0.ends_with("+complete tail\n"));
        let weak = Arc::downgrade(&decoded.changes.patches[0].0);
        drop(decoded);
        assert!(
            weak.upgrade().is_none(),
            "the ledger must not own decoded patch plaintext"
        );
    }

    #[test]
    fn duplicate_body_or_unadmitted_equal_text_never_enters_a_view_header() {
        let (snapshot, mut view) = fixture();
        let header = Manifest::capture(&snapshot, &view).unwrap();
        view.patches.push(view.patches[0].clone());
        assert!(
            Manifest::capture(&snapshot, &view).is_err(),
            "serde would allocate this repeated body twice"
        );
        view.patches.pop();
        view.patches[0].0 = Arc::from(view.patches[0].0.as_ref());
        assert!(
            header.validate_live(&snapshot, &view).is_err(),
            "equal bytes are not an admitted Arc"
        );
        assert!(Manifest::capture(&snapshot, &view).is_err());
    }

    #[test]
    fn incarnation_and_original_file_identity_survive_cached_round_trip() {
        let (mut snapshot, view) = fixture();
        let header = Manifest::capture(&snapshot, &view).unwrap();
        snapshot.changes_parts[0].epoch.incarnation = uuid::Uuid::from_u128(99);
        assert!(
            header.validate(&snapshot, &view).is_err(),
            "same revision in another Store is unrelated"
        );
        snapshot.changes_parts[0].epoch = view.manifests[0].epoch;
        // Current authoritative status may advance; it is not frozen patch authority.
        snapshot.changes_parts[0].metadata.status = ToolStatus::Running;
        assert!(header.validate(&snapshot, &view).is_ok());
        let mut changed = view.clone();
        changed.patches[0].1.file.path = "another/repeated.rs".into();
        assert!(header.validate(&snapshot, &changed).is_err());
        let mut missing = view.clone();
        missing.manifests.clear();
        assert!(header.validate(&snapshot, &missing).is_err());
    }
}
