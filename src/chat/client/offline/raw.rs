//! Frozen original-call proofs. These cannot be reconstructed from prose or
//! Changes, and do not inherit authority from the prose source revision.
use crate::chat::{
    Snapshot,
    source_owners::{self, ResidentRaw, ResidentRawCalls},
};
use std::collections::HashSet;

#[derive(Clone, Default)]
pub(super) struct Manifest {
    calls: Vec<ResidentRaw>,
    pub(super) bytes: usize,
    pub(super) largest: usize,
}

fn matches(snapshot: &Snapshot, record: &ResidentRaw) -> bool {
    if snapshot
        .source_incarnation
        .is_some_and(|incarnation| incarnation != record.epoch.incarnation)
    {
        return false;
    }
    let mut aliases = snapshot
        .raw_source_parts
        .iter()
        .filter(|part| part.source == record.source && part.field == record.field);
    let Some(first) = aliases.next() else {
        return false;
    };
    std::iter::once(first).chain(aliases).all(|part| {
        part.epoch == record.epoch
            && part.source_len == record.source_len
            && part.metadata == record.metadata
    }) && record
        .source
        .agent_id
        .as_ref()
        .is_none_or(|id| snapshot.subagents.iter().any(|child| &child.id == id))
}

impl Manifest {
    pub(super) fn capture(snapshot: &Snapshot, view: &ResidentRawCalls) -> Result<Self, String> {
        let mut result = Self::default();
        let mut keys = HashSet::new();
        for (text, record) in &view.calls {
            if !keys.insert((record.source.clone(), record.field))
                || !matches(snapshot, record)
                || text.len() != record.source_len
                || source_owners::resident_raw(text).as_ref() != Some(record)
            {
                return Err("The original call lacks its exact unique admitted source".into());
            }
            result.bytes = result
                .bytes
                .checked_add(text.len())
                .ok_or("Cached original size overflow")?;
            result.largest = result.largest.max(text.len());
            result.calls.push(record.clone());
        }
        Ok(result)
    }

    pub(super) fn validate(
        &self,
        snapshot: &Snapshot,
        view: &ResidentRawCalls,
    ) -> Result<(), String> {
        if view.calls.len() != self.calls.len() {
            return Err("The decoded original list differs from its captured proof".into());
        }
        for ((text, actual), expected) in view.calls.iter().zip(&self.calls) {
            if actual != expected
                || text.len() != expected.source_len
                || !matches(snapshot, expected)
            {
                return Err("The decoded original differs from its captured source".into());
            }
        }
        Ok(())
    }

    pub(super) fn validate_live(
        &self,
        snapshot: &Snapshot,
        view: &ResidentRawCalls,
    ) -> Result<(), String> {
        self.validate(snapshot, view)?;
        if view
            .calls
            .iter()
            .any(|(text, record)| source_owners::resident_raw(text).as_ref() != Some(record))
        {
            return Err("The fallback original has lost its admitted owner".into());
        }
        Ok(())
    }
}

pub(super) fn retain_current(snapshot: &Snapshot, view: &mut ResidentRawCalls) {
    view.calls.retain(|(_, record)| matches(snapshot, record));
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use crate::chat::cache::{
        SourceEpoch, SourceKey,
        raw::{RawRepresentation, RawSourceField, RawSourceMetadata, RawSourcePart},
    };
    use std::sync::Arc;

    pub(in crate::chat::client::offline) fn fixture() -> (Snapshot, ResidentRawCalls) {
        let text: Arc<str> = Arc::from(
            "{  \"file_path\": \"original.rs\", \"content\": \"complete original call\" }",
        );
        let record = ResidentRaw {
            source: SourceKey {
                agent_id: None,
                source_id: "opaque-tool-call:0".into(),
            },
            field: RawSourceField::ToolInput,
            epoch: SourceEpoch {
                incarnation: uuid::Uuid::from_u128(7),
                revision: 19,
            },
            source_len: text.len(),
            metadata: RawSourceMetadata {
                tool_name: "Write".into(),
                representation: RawRepresentation::StreamJson,
                complete_call: true,
            },
        };
        assert_eq!(
            source_owners::admit_raw(&text, &record, None),
            source_owners::Admission::Ready
        );
        let snapshot = Snapshot {
            raw_source_parts: vec![RawSourcePart {
                item_id: "native-alias".into(),
                source: record.source.clone(),
                field: record.field,
                epoch: record.epoch,
                source_len: record.source_len,
                metadata: record.metadata.clone(),
            }],
            ..Default::default()
        };
        (
            snapshot,
            ResidentRawCalls {
                calls: vec![(text, record)],
            },
        )
    }

    #[test]
    fn raw_proof_is_separate_from_prose_revision_and_cannot_resurrect_removed_calls() {
        let (mut snapshot, mut view) = fixture();
        let header = Manifest::capture(&snapshot, &view).unwrap();
        let prose_revision = snapshot.source_revision;
        snapshot.raw_source_parts.clear();
        assert_eq!(snapshot.source_revision, prose_revision);
        assert!(header.validate(&snapshot, &view).is_err());
        retain_current(&snapshot, &mut view);
        assert!(view.calls.is_empty());
    }

    #[test]
    fn equivalent_aliases_share_one_raw_proof_but_conflicting_descriptors_refuse_it() {
        let (mut snapshot, view) = fixture();
        snapshot.source_incarnation = Some(view.calls[0].1.epoch.incarnation);
        let mut alias = snapshot.raw_source_parts[0].clone();
        alias.item_id = "another-native-alias".into();
        snapshot.raw_source_parts.push(alias.clone());
        let header = Manifest::capture(&snapshot, &view).unwrap();
        header.validate(&snapshot, &view).unwrap();
        for conflict in 0..3 {
            let mut changed = alias.clone();
            match conflict {
                0 => changed.epoch.revision += 1,
                1 => changed.metadata.tool_name = "different-tool".into(),
                _ => changed.source_len += 1,
            }
            snapshot.raw_source_parts[1] = changed;
            assert!(Manifest::capture(&snapshot, &view).is_err());
            assert!(header.validate(&snapshot, &view).is_err());
            let mut retained = view.clone();
            retain_current(&snapshot, &mut retained);
            assert!(retained.calls.is_empty());
        }
        snapshot.raw_source_parts[1] = alias;
        header.validate(&snapshot, &view).unwrap();
        snapshot.source_incarnation = Some(uuid::Uuid::from_u128(99));
        assert!(header.validate(&snapshot, &view).is_err());
        let mut retained = view;
        retain_current(&snapshot, &mut retained);
        assert!(retained.calls.is_empty());
    }

    #[test]
    fn exact_epoch_field_representation_and_weak_owner_are_required() {
        let (mut snapshot, view) = fixture();
        let header = Manifest::capture(&snapshot, &view).unwrap();
        let mut duplicate = view.clone();
        duplicate.calls.push(view.calls[0].clone());
        assert!(Manifest::capture(&snapshot, &duplicate).is_err());
        for change in 0..3 {
            let mut part = snapshot.raw_source_parts[0].clone();
            match change {
                0 => part.epoch.incarnation = uuid::Uuid::from_u128(99),
                1 => part.field = RawSourceField::CommandCall,
                _ => part.metadata.representation = RawRepresentation::CanonicalJson,
            }
            let original = std::mem::replace(&mut snapshot.raw_source_parts[0], part);
            assert!(header.validate(&snapshot, &view).is_err());
            snapshot.raw_source_parts[0] = original;
        }
        let uncharged = ResidentRawCalls {
            calls: vec![(Arc::from(view.calls[0].0.as_ref()), view.calls[0].1.clone())],
        };
        assert!(header.validate_live(&snapshot, &uncharged).is_err());
        assert!(Manifest::capture(&snapshot, &uncharged).is_err());
    }

    #[test]
    fn decoded_original_is_reserved_before_serde_and_adopted_by_its_actual_arc() {
        let (snapshot, view) = fixture();
        let header =
            super::super::sources::Manifest::capture_all(&snapshot, &Default::default(), &view)
                .unwrap();
        let reservation = header.reserve().unwrap();
        let bytes = serde_json::to_vec(&super::super::View {
            snapshot: Arc::new(snapshot),
            changes: Default::default(),
            raw: view.clone(),
        })
        .unwrap();
        let mut decoded: super::super::View = serde_json::from_slice(&bytes).unwrap();
        assert!(source_owners::resident_raw(&decoded.raw.calls[0].0).is_none());
        header
            .adopt_all(
                Arc::make_mut(&mut decoded.snapshot),
                &decoded.changes,
                &decoded.raw,
                reservation,
            )
            .unwrap();
        assert_eq!(
            source_owners::resident_raw(&decoded.raw.calls[0].0),
            Some(view.calls[0].1.clone())
        );
        let weak = Arc::downgrade(&decoded.raw.calls[0].0);
        drop(decoded);
        assert!(weak.upgrade().is_none());
    }
}
