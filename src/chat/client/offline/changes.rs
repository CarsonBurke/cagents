//! The current Snapshot has no authoritative typed Changes descriptors.
//! Keep the full-source API shape, but never qualify sidecars from unrelated
//! file summaries or a manufactured epoch. Empty Changes remain valid.
use crate::chat::{Snapshot, source_owners::ResidentChanges};

#[derive(Clone, Default)]
pub(super) struct Manifest {
    pub(super) bytes: usize,
    pub(super) largest: usize,
}
impl Manifest {
    pub(super) fn capture(snapshot: &Snapshot, view: &ResidentChanges) -> Result<Self, String> {
        let manifest = Self::default();
        manifest.validate(snapshot, view)?;
        Ok(manifest)
    }

    pub(super) fn validate(
        &self,
        _snapshot: &Snapshot,
        view: &ResidentChanges,
    ) -> Result<(), String> {
        if !view.manifests.is_empty() || !view.patches.is_empty() {
            return Err(
                "Typed Changes cannot be cached without authoritative snapshot descriptors".into(),
            );
        }
        Ok(())
    }

    pub(super) fn validate_live(
        &self,
        snapshot: &Snapshot,
        view: &ResidentChanges,
    ) -> Result<(), String> {
        self.validate(snapshot, view)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{
        ChangeKind, Role, ToolStatus,
        cache::{
            SourceEpoch, SourceKey,
            changes::{File, Manifest as Page, Metadata},
        },
        source_owners::ResidentPatch,
    };
    use std::sync::Arc;

    #[test]
    fn empty_changes_are_valid_but_unqualified_manifests_and_patches_are_refused() {
        let snapshot = Snapshot::default();
        let empty = ResidentChanges::default();
        let manifest = Manifest::capture(&snapshot, &empty).unwrap();
        manifest.validate(&snapshot, &empty).unwrap();
        manifest.validate_live(&snapshot, &empty).unwrap();
        assert_eq!((manifest.bytes, manifest.largest), (0, 0));
        let source = SourceKey {
            agent_id: None,
            source_id: "changes-without-descriptor".into(),
        };
        let epoch = SourceEpoch {
            incarnation: uuid::Uuid::from_u128(7),
            revision: 19,
        };
        let file = File {
            file_index: 0,
            path: "file.rs".into(),
            kind: ChangeKind::Updated,
            source_len: 1,
            additions: None,
            deletions: None,
        };
        let page = Arc::new(Page {
            source: source.clone(),
            epoch,
            metadata: Metadata {
                role: Role::Assistant,
                status: ToolStatus::Succeeded,
                at: None,
                started_at: None,
                total_files: 1,
                total_bytes: 1,
                additions: None,
                deletions: None,
                unavailable: None,
            },
            offset: 0,
            next_offset: None,
            files: vec![file.clone()],
            omitted_files: 0,
        });
        for view in [
            ResidentChanges {
                manifests: vec![page],
                patches: vec![],
            },
            ResidentChanges {
                manifests: vec![],
                patches: vec![(
                    Arc::from("x"),
                    ResidentPatch {
                        source,
                        epoch,
                        file,
                    },
                )],
            },
        ] {
            assert!(Manifest::capture(&snapshot, &view).is_err());
            assert!(manifest.validate(&snapshot, &view).is_err());
            assert!(manifest.validate_live(&snapshot, &view).is_err());
        }
    }
}
