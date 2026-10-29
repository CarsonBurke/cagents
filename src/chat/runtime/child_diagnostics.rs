//! Exact child diagnostic originals live in one private, quota-limited store.
//! Resident items are previews; only published source authority grants a read.
use super::{Event, Item, ItemContent, Phase, Snapshot, Store};
use crate::{
    agent::Provider,
    chat::{TextCopyPage, TextCopyScope, TextCopySource},
};
use std::collections::{BTreeMap, BTreeSet};

const ORIGINAL_BYTES: usize = 1024 * 1024;
const TOTAL_BYTES: usize = 8 * ORIGINAL_BYTES;
const METADATA_BYTES: usize = 1024 * 1024;
const RECORDS: usize = 32 * 64;
const OMITTED: &str = "\n[Diagnostic preview; the original requires its retained copy source.]";
// Match Failure::parse's complete classification prefix even for four-byte
// Unicode scalars, while staying below the child resident item budget.
const PREVIEW_CHARS: usize = 8192;
const PREVIEW_BYTES: usize = PREVIEW_CHARS * 4 + OMITTED.len();

struct Entry {
    key: String,
    source: TextCopySource,
    metadata: usize,
}

/// One 256-KiB SQLite page-cache target, a 12-MiB database ceiling and DELETE
/// rollback journaling. Allow 13 MiB for a transient rollback journal: the
/// intended aggregate disk envelope is 26 MiB, not merely the payload quota.
/// One connection/lease/journal plus SQLite's directory sync use <=4 backing
/// descriptors. No connection or plaintext body is retained per child/read.
#[derive(Default)]
pub(super) struct Backing {
    store: Option<Box<Store>>,
    entries: BTreeMap<(String, String), Entry>,
    bytes: usize,
    metadata: usize,
}

pub(super) fn marked(item: &Item) -> bool {
    crate::chat::failure::provider_error_text(Provider::Codex, item).is_some()
}

pub(super) fn project(item: &mut Item) {
    if !marked(item) {
        return;
    }
    if let ItemContent::Delivery { body, .. } = &mut item.content {
        if let Some((end, _)) = body.char_indices().nth(PREVIEW_CHARS) {
            let mut preview = String::with_capacity(end + OMITTED.len());
            preview.push_str(&body[..end]);
            preview.push_str(OMITTED);
            // Truncation alone retains the potentially refused 32-MiB frame
            // allocation. Replace its owner before entering resident state.
            *body = preview;
        } else if body.capacity() > PREVIEW_BYTES {
            *body = body.as_str().to_owned();
        }
    }
}

impl Backing {
    pub(super) fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
    fn clear(&mut self) {
        *self = Self::default();
    }

    fn remove(&mut self, child: &str, item: &str) -> Result<(), String> {
        let Some(entry) = self.entries.remove(&(child.into(), item.into())) else {
            return Ok(());
        };
        // Revoke before attempting IO, including a replacement that is refused.
        self.bytes -= entry.source.bytes as usize;
        self.metadata -= entry.metadata;
        if let Some(store) = &mut self.store {
            store.apply(Event::ItemsRemoved {
                ids: vec![entry.key],
            })?;
        }
        Ok(())
    }

    fn put(&mut self, child: &str, item: &Item) -> Result<(), String> {
        // An admitted key can never exceed this bound. Refuse oversized new
        // identities before copying them merely to look for a previous entry.
        if child.len() > METADATA_BYTES || item.id.len() > METADATA_BYTES {
            return Ok(());
        }
        self.remove(child, &item.id)?;
        let Some(body) = crate::chat::failure::provider_error_text(Provider::Codex, item) else {
            return Ok(());
        };
        // Charge duplicate identity/scope strings and fixed map/descriptor
        // overhead, not just record count. Body storage is separately charged.
        let metadata = child
            .len()
            .checked_mul(3)
            .and_then(|bytes| bytes.checked_add(item.id.len()))
            .and_then(|bytes| bytes.checked_add(36 + 512));
        let Some(metadata) = metadata.filter(|bytes| *bytes <= METADATA_BYTES) else {
            return Ok(());
        };
        if body.len() > ORIGINAL_BYTES
            || self.bytes.saturating_add(body.len()) > TOTAL_BYTES
            || self.metadata.saturating_add(metadata) > METADATA_BYTES
            || self.entries.len() >= RECORDS
        {
            return Ok(());
        }
        let key = uuid::Uuid::new_v4().to_string();
        let store = match &mut self.store {
            Some(store) => store,
            slot @ None => slot.insert(Box::new(Store::diagnostic_backing()?)),
        };
        let warning = store.put(
            Item {
                id: key.clone(),
                role: item.role,
                started_at: item.started_at,
                at: item.at,
                content: ItemContent::Text {
                    text: body.into(),
                    thinking: false,
                    phase: Phase::Commentary,
                },
            },
            false,
        )?;
        if warning.is_some() {
            return Err("Diagnostic original was not cached completely".into());
        }
        let mut source = store
            .diagnostic_source(&key)?
            .ok_or("Diagnostic copy source is unavailable")?;
        source.scope = TextCopyScope::Subagent { id: child.into() };
        self.bytes += body.len();
        self.metadata += metadata;
        self.entries.insert(
            (child.into(), item.id.clone()),
            Entry {
                key,
                source,
                metadata,
            },
        );
        Ok(())
    }

    pub(super) fn observe(&mut self, event: &mut Event) {
        let result = match event {
            Event::SubagentItem { id, item } => {
                let result = self.put(id, item);
                project(item);
                result
            }
            Event::SubagentItemsRemoved { id, ids } => {
                ids.iter().try_for_each(|item| self.remove(id, item))
            }
            Event::Reset { .. } => {
                self.clear();
                Ok(())
            }
            _ => Ok(()),
        };
        // Storage uncertainty never leaks into parent outcome or leaves old
        // originals readable. The marked preview remains useful, with no source.
        if result.is_err() {
            self.clear();
        }
    }

    pub(super) fn owners(&self, state: &Snapshot) -> BTreeSet<String> {
        self.entries
            .iter()
            .filter(|((child, item), _)| {
                state.subagents.iter().any(|agent| {
                    agent.id == *child
                        && agent
                            .items
                            .iter()
                            .any(|value| value.id == *item && marked(value))
                })
            })
            .map(|(_, entry)| entry.key.clone())
            .collect()
    }

    pub(super) fn retain(&mut self, owners: &BTreeSet<String>) {
        while let Some((child, item)) = self
            .entries
            .iter()
            .find(|(_, entry)| !owners.contains(&entry.key))
            .map(|(key, _)| key.clone())
        {
            if self.remove(&child, &item).is_err() {
                self.clear();
                break;
            }
        }
        if self.entries.is_empty() {
            self.store = None;
        }
    }

    pub(super) fn publish(&self, state: &mut Snapshot) {
        for agent in &state.subagents {
            let sources = state
                .text_copy_sources
                .children
                .entry(agent.id.clone())
                .or_default();
            sources.resize(agent.items.len(), None);
            for (index, item) in agent.items.iter().enumerate() {
                if marked(item) {
                    sources[index] = self
                        .entries
                        .get(&(agent.id.clone(), item.id.clone()))
                        .map(|entry| entry.source.clone());
                }
            }
        }
    }

    pub(super) fn read(
        &mut self,
        child: &str,
        item: &str,
        source: &TextCopySource,
        offset: u64,
    ) -> Result<TextCopyPage, String> {
        let entry = self
            .entries
            .get(&(child.into(), item.into()))
            .filter(|entry| entry.source == *source)
            .ok_or("Original diagnostic unavailable: source changed, was refused or was evicted")?;
        let store = self
            .store
            .as_ref()
            .ok_or("Original diagnostic backing is unavailable")?;
        if offset > source.bytes {
            return Err("Invalid original diagnostic range".into());
        }
        if offset < source.bytes {
            match store.diagnostic_boundary(&entry.key, offset as usize) {
                Ok(true) => {}
                Ok(false) => return Err("Original diagnostic offset splits UTF-8".into()),
                Err(error) => {
                    self.clear();
                    return Err(error);
                }
            }
        }
        let mut internal = source.clone();
        internal.scope = TextCopyScope::Conversation;
        let mut page = match store.text_copy(&internal, offset) {
            Ok(page) => page,
            Err(error) => {
                self.clear();
                return Err(error);
            }
        };
        page.source = source.clone();
        Ok(page)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn notice(child: &str, id: &str, body: &str) -> Event {
        let mut item = Item {
            id: id.into(),
            role: crate::chat::Role::System,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: Phase::Commentary,
            },
            at: None,
            started_at: None,
        };
        assert!(crate::chat::failure::mark_provider_error(
            Provider::Codex,
            &mut item
        ));
        Event::SubagentItem {
            id: child.into(),
            item,
        }
    }

    fn apply(backing: &mut Backing, state: &mut Snapshot, mut event: Event) {
        backing.observe(&mut event);
        super::super::apply_event(state, &mut BTreeMap::new(), event);
        backing.retain(&backing.owners(state));
        backing.publish(state);
    }

    fn source(state: &Snapshot, child: &str, index: usize) -> TextCopySource {
        state.text_copy_sources.children[child][index]
            .clone()
            .unwrap()
    }

    fn full(
        backing: &mut Backing,
        child: &str,
        item: &str,
        source: &TextCopySource,
    ) -> Result<String, String> {
        let mut raw = String::new();
        loop {
            let page = backing.read(child, item, source, raw.len() as u64)?;
            if page.source != *source || page.offset != raw.len() as u64 {
                return Err("Changed page identity".into());
            }
            raw.push_str(&page.text);
            if page.done {
                return Ok(raw);
            }
            if page.text.is_empty() {
                return Err("No page progress".into());
            }
        }
    }

    #[test]
    fn large_utf8_original_is_scoped_and_rewrites_or_refusal_revoke_old_bytes() {
        let mut backing = Backing::default();
        let mut state = Snapshot::default();
        let raw = format!(
            "HEAD request-id: exact\n{}\nTAIL λ",
            "猫café λ\n".repeat(30_000)
        );
        apply(&mut backing, &mut state, notice("child", "same", &raw));
        let first = source(&state, "child", 0);
        assert_eq!(full(&mut backing, "child", "same", &first).unwrap(), raw);
        let projected = &state.subagents[0].items[0];
        assert!(marked(projected));
        assert!(projected.content.byte_len() <= 64 * 1024);
        assert!(
            matches!(&projected.content, ItemContent::Delivery { body, .. } if body.capacity() <= PREVIEW_BYTES)
        );
        assert!(
            crate::chat::failure::provider_error_text(Provider::Codex, projected)
                .unwrap()
                .contains("Diagnostic preview")
        );
        assert!(backing.read("sibling", "same", &first, 0).is_err());
        let mut forged = first.clone();
        forged.scope = TextCopyScope::Subagent {
            id: "sibling".into(),
        };
        assert!(backing.read("child", "same", &forged, 0).is_err());
        assert!(backing.read("child", "same", &first, 1).is_ok());
        assert_eq!(
            full(&mut backing, "child", "same", &first).unwrap(),
            raw,
            "foreign reads cannot revoke valid authority"
        );
        apply(
            &mut backing,
            &mut state,
            notice("child", "same", "replacement"),
        );
        let replacement = source(&state, "child", 0);
        assert!(backing.read("child", "same", &first, 0).is_err());
        assert_eq!(
            full(&mut backing, "child", "same", &replacement).unwrap(),
            "replacement"
        );
        apply(
            &mut backing,
            &mut state,
            notice("child", "same", &"x".repeat(ORIGINAL_BYTES + 1)),
        );
        assert!(state.text_copy_sources.children["child"][0].is_none());
        assert!(marked(&state.subagents[0].items[0]));
        assert!(
            matches!(&state.subagents[0].items[0].content, ItemContent::Delivery { body, .. } if body.capacity() <= PREVIEW_BYTES)
        );
        assert!(backing.read("child", "same", &replacement, 0).is_err());
        assert!(backing.is_empty());
    }

    #[test]
    fn aggregate_payload_and_identity_quotas_refuse_without_losing_failure_metadata() {
        let mut backing = Backing::default();
        let body = "λ".repeat(ORIGINAL_BYTES / 2);
        // No projection reconciliation in this quota oracle: these are eight
        // independently owned originals, not eight giant resident bodies.
        for index in 0..8 {
            backing.observe(&mut notice("child", &index.to_string(), &body));
        }
        assert_eq!(backing.bytes, TOTAL_BYTES);
        let mut refused = notice("child", "over-quota", "extra");
        backing.observe(&mut refused);
        assert_eq!(backing.entries.len(), 8);
        assert!(matches!(refused, Event::SubagentItem { item, .. } if marked(&item)));
        backing.remove("child", "0").unwrap();
        backing.observe(&mut notice("child", "reclaimed", &body));
        assert_eq!(backing.bytes, TOTAL_BYTES);
        let mut identities = Backing::default();
        let previous = identities.entries.len();
        identities.observe(&mut notice(
            &"z".repeat(METADATA_BYTES),
            "huge-identity",
            "body",
        ));
        assert_eq!(identities.entries.len(), previous);
        assert!(identities.store.is_none());
        assert!(identities.metadata <= METADATA_BYTES);
    }

    #[test]
    fn byte_thresholds_and_backing_churn_keep_marked_previews_and_bounded_disk() {
        let mut backing = Backing::default();
        let mut state = Snapshot::default();
        let Event::SubagentItem { mut item, .. } = notice("child", "threshold", "") else {
            unreachable!()
        };
        let marker_bytes = item.content.byte_len();
        for size in [64 * 1024 - marker_bytes, 64 * 1024 - marker_bytes + 1] {
            if let ItemContent::Delivery { body, .. } = &mut item.content {
                *body = "x".repeat(size);
            }
            apply(
                &mut backing,
                &mut state,
                Event::SubagentItem {
                    id: "child".into(),
                    item: item.clone(),
                },
            );
            let source = source(&state, "child", 0);
            assert_eq!(
                full(&mut backing, "child", "threshold", &source).unwrap(),
                "x".repeat(size)
            );
            assert!(marked(&state.subagents[0].items[0]));
            assert!(state.subagents[0].items[0].content.byte_len() <= 64 * 1024);
        }
        for index in 0..16 {
            let body = format!("generation {index}\n{}", "λ".repeat(128 * 1024));
            apply(
                &mut backing,
                &mut state,
                notice("child", "threshold", &body),
            );
            let source = source(&state, "child", 0);
            assert_eq!(
                full(&mut backing, "child", "threshold", &source).unwrap(),
                body
            );
            let directory = backing.store.as_ref().unwrap().directory();
            let bytes: u64 = std::fs::read_dir(directory)
                .unwrap()
                .map(|entry| entry.unwrap().metadata().unwrap().len())
                .sum();
            assert!(bytes <= 26 * 1024 * 1024);
            assert!(!directory.join("transcript.sqlite-wal").exists());
            assert!(!directory.join("transcript.sqlite-shm").exists());
        }
    }

    #[test]
    fn unicode_classification_prefix_and_small_text_with_large_capacity_stay_truthful() {
        let raw = format!(
            "{}You've hit your limit · resets 3:40pm (America/Vancouver)\n{}",
            "猫".repeat(3000),
            "🙂".repeat(10_000)
        );
        let before = crate::chat::failure::Failure::parse(Provider::Codex, &raw).unwrap();
        let mut backing = Backing::default();
        let mut state = Snapshot::default();
        apply(&mut backing, &mut state, notice("child", "unicode", &raw));
        let body = crate::chat::failure::provider_error_text(
            Provider::Codex,
            &state.subagents[0].items[0],
        )
        .unwrap();
        let after = crate::chat::failure::Failure::parse(Provider::Codex, body).unwrap();
        assert_eq!(before.kind, after.kind);
        assert_eq!(before.summary, after.summary);
        assert_eq!(before.action, after.action);
        let Event::SubagentItem { mut item, .. } = notice("child", "capacity", "small diagnostic")
        else {
            unreachable!()
        };
        if let ItemContent::Delivery { body, .. } = &mut item.content {
            body.reserve(2 * ORIGINAL_BYTES);
        }
        project(&mut item);
        assert!(
            matches!(&item.content, ItemContent::Delivery { body, .. } if body == "small diagnostic" && body.capacity() <= PREVIEW_BYTES)
        );
    }

    #[test]
    fn eviction_reset_and_failed_storage_revoke_both_rows_and_copy_authority() {
        let mut backing = Backing::default();
        let mut state = Snapshot::default();
        apply(&mut backing, &mut state, notice("child", "0", "original"));
        let first = source(&state, "child", 0);
        for index in 1..65 {
            apply(
                &mut backing,
                &mut state,
                notice("child", &index.to_string(), "next"),
            );
        }
        assert_eq!(state.subagents[0].items.len(), 64);
        assert_eq!(backing.entries.len(), 64);
        assert!(backing.read("child", "0", &first, 0).is_err());
        let captured = source(&state, "child", 0);
        let captured_item = state.subagents[0].items[0].id.clone();
        backing
            .store
            .as_mut()
            .unwrap()
            .read_only_for_test()
            .unwrap();
        apply(
            &mut backing,
            &mut state,
            notice("child", &captured_item, "failed replacement"),
        );
        assert!(backing.is_empty());
        assert!(
            state.text_copy_sources.children["child"]
                .iter()
                .all(Option::is_none)
        );
        assert!(backing.read("child", &captured_item, &captured, 0).is_err());
        apply(&mut backing, &mut state, notice("fresh", "new", "猫 λ"));
        let fresh = source(&state, "fresh", 0);
        assert_ne!(fresh.cache, captured.cache);
        assert!(
            backing.read("fresh", "new", &fresh, 1).is_err(),
            "UTF-8 split must fail"
        );
        for index in 0..33 {
            apply(
                &mut backing,
                &mut state,
                notice(&format!("rich-{index}"), "same", "owned"),
            );
        }
        assert!(!backing.entries.keys().any(|(child, _)| child == "fresh"));
        apply(
            &mut backing,
            &mut state,
            Event::Reset {
                reason: "test reset".into(),
            },
        );
        assert!(backing.is_empty() && backing.store.is_none());
        assert!(backing.read("fresh", "new", &fresh, 0).is_err());
    }
}
