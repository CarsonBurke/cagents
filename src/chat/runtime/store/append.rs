//! Local, bounded proof that a resident body's bytes precede successful appends.
//! This does not authorize wire consumers or retain completed body payloads.
use super::*;
use std::{
    collections::HashMap,
    sync::{Arc, Weak},
};

struct Resident {
    item: Weak<Item>,
    root: String,
    starts: [usize; 2],
}
#[derive(Default)]
pub(super) struct Changes {
    fields: HashMap<String, [Option<usize>; 2]>,
    residents: HashMap<String, Resident>,
    cache: Option<uuid::Uuid>,
}
impl Changes {
    pub(super) fn clear(&mut self) {
        self.fields.clear();
        self.residents.clear();
        self.cache = None;
    }
    pub(super) fn remove(&mut self, id: &str) {
        self.fields.remove(id);
    }
    fn record(&mut self, id: &str, field: usize, offset: usize, prior: [Option<usize>; 2]) {
        let fields = self.fields.entry(id.into()).or_insert(prior);
        fields[field] = Some(fields[field].map_or(offset, |old| old.min(offset)));
    }
    /// Read-only range proof for a payload shared by a fresh-metadata Item.
    /// Caller must separately establish exact resident/cache/root/start ownership.
    pub(super) fn preserves(&self, root: &str, field: usize, end: usize) -> bool {
        self.fields
            .get(root)
            .and_then(|fields| fields.get(field))
            .is_some_and(|offset| offset.is_none_or(|offset| end <= offset))
    }
    pub(super) fn publish(
        &mut self,
        cache: uuid::Uuid,
        items: &[Arc<Item>],
        roots: Vec<Option<(String, [usize; 2])>>,
    ) {
        self.fields.clear();
        self.residents.clear();
        let mut seen = std::collections::HashSet::new();
        let mut duplicates = std::collections::HashSet::new();
        for (item, source) in items.iter().zip(roots) {
            if !seen.insert(item.id.clone()) {
                duplicates.insert(item.id.clone());
            }
            let Some((root, starts)) = source else {
                continue;
            };
            self.residents.insert(
                item.id.clone(),
                Resident {
                    item: Arc::downgrade(item),
                    root,
                    starts,
                },
            );
        }
        // Native-part IDs and provider IDs share a namespace. Ambiguity must
        // force the ordinary SQL read, never adopt another root's body.
        for id in duplicates {
            self.residents.remove(&id);
        }
        self.cache = Some(cache);
    }
    pub(super) fn owns(
        &self,
        cache: uuid::Uuid,
        root: &str,
        field: usize,
        start: usize,
        item: &Arc<Item>,
    ) -> bool {
        self.cache == Some(cache)
            && self.residents.get(&item.id).is_some_and(|resident| {
                resident.root == root
                    && resident.starts[field] == start
                    && resident
                        .item
                        .upgrade()
                        .is_some_and(|published| Arc::ptr_eq(&published, item))
            })
    }
}
fn field_body(item: &Item, field: usize) -> Option<&str> {
    match (&item.content, field) {
        (ItemContent::Text { text, .. }, 0) => Some(text),
        (ItemContent::Command { output, .. }, 0) => Some(output),
        (ItemContent::Tool { input, .. }, 0) => Some(input),
        (ItemContent::Tool { output, .. }, 1) => Some(output),
        _ => None,
    }
}
impl Store {
    pub(super) fn append_changed(&mut self, id: &str, field: usize, offset: usize) {
        let prior = self.appended.fields.get(id).copied();
        let first = self
            .changed
            .as_ref()
            .is_some_and(|changed| !changed.contains(id));
        self.changed(id);
        // A replacement followed by an append cannot resurrect a prefix proof.
        if self.changed.is_some() && (first || prior.is_some()) {
            self.appended
                .record(id, field, offset, prior.unwrap_or([None; 2]));
        }
    }
    pub(super) fn field_reusing(
        &self,
        id: &str,
        field: usize,
        start: usize,
        end: usize,
        previous: Option<&Arc<Item>>,
    ) -> Result<(String, bool), String> {
        if let Some(previous) = previous
            && self
                .appended
                .owns(self.copy_cache, id, field, start, previous)
            && let Some(fields) = self.appended.fields.get(id)
            && let Some(old) = field_body(previous, field)
            && let Some(old_end) = start.checked_add(old.len())
            && old_end <= end
            && fields[field].map_or(old_end == end, |offset| old_end <= offset)
        {
            let suffix = self.field(id, field as i64, old_end, end)?;
            let mut body = String::with_capacity(end - start);
            body.push_str(old);
            body.push_str(&String::from_utf8(suffix).map_err(|error| error.to_string())?);
            return Ok((body, true));
        }
        Ok((
            String::from_utf8(self.field(id, field as i64, start, end)?)
                .map_err(|error| error.to_string())?,
            false,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::presentation::ItemChange;
    fn prose(id: &str, text: String) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            started_at: Some(3),
            at: Some(10),
            content: ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: Phase::Final,
            },
        }
    }
    fn delta(store: &mut Store, text: &str) {
        store
            .apply(Event::TextDelta {
                id: "live".into(),
                role: Role::Assistant,
                thinking: false,
                delta: text.into(),
            })
            .unwrap();
    }
    fn text(item: &Item) -> &str {
        let ItemContent::Text { text, .. } = &item.content else {
            panic!("text item")
        };
        text
    }
    #[test]
    fn append_publication_reads_only_new_sqlite_bytes_with_exact_rewrite_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::at(directory.path().into()).unwrap();
        store
            .put(prose("live", "x".repeat(64 * 1024)), true)
            .unwrap();
        let mut previous = store.window().unwrap().items;
        let source = store.copy_parent_source("live").unwrap().unwrap();
        delta(&mut store, "αβ");
        store.field_bytes.set(0);
        let appended = store.window_reusing(&mut previous).unwrap();
        assert_eq!(store.field_bytes.get(), "αβ".len());
        assert!(
            matches!(&appended.changes[0], ItemChange::Append { offset, suffix, .. }
            if *offset == 64 * 1024 && suffix == "αβ")
        );
        let next_source = store.copy_parent_source("live").unwrap().unwrap();
        assert_eq!(next_source.cache, source.cache);
        assert_eq!(next_source.token, source.token);
        assert_eq!(next_source.revision, source.revision);
        assert_eq!(next_source.bytes, source.bytes + "αβ".len() as u64);
        let metadata: Vec<u8> = store
            .db
            .query_row("SELECT metadata FROM items WHERE id='live'", [], |row| {
                row.get(0)
            })
            .unwrap();
        let current: Item = postcard::from_bytes(&metadata).unwrap();
        assert_eq!(appended.items[0].at, current.at);
        assert_eq!(appended.items[0].role, current.role);
        assert_eq!(appended.items[0].started_at, Some(3));
        previous = appended.items;
        store
            .put(prose("live", "z".repeat(64 * 1024 + 4)), false)
            .unwrap();
        delta(&mut store, "δ tail");
        store.field_bytes.set(0);
        let rewritten = store.window_reusing(&mut previous).unwrap();
        assert!(store.field_bytes.get() >= 64 * 1024 + 4 + "δ tail".len());
        assert!(matches!(&rewritten.changes[0], ItemChange::Replace(_)));
        assert!(text(&rewritten.items[0]).starts_with('z'));
        assert!(text(&rewritten.items[0]).ends_with("δ tail"));
        assert_ne!(
            store.copy_parent_source("live").unwrap().unwrap().revision,
            source.revision
        );
        previous = rewritten.items;
        delta(&mut store, " committed");
        store.flush_text().unwrap();
        assert!(store.appended.fields.contains_key("live"));
        previous = store.window_reusing(&mut previous).unwrap().items;
        assert!(store.changed.as_ref().unwrap().is_empty());
        // OutputDelta reuses both the unchanged Tool input and output prefix.
        let tool_dir = tempfile::tempdir().unwrap();
        let mut tool = Store::at(tool_dir.path().into()).unwrap();
        tool.apply(Event::Item(Item {
            id: "tool".into(),
            role: Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Tool {
                name: "shell".into(),
                input: "i".repeat(16 * 1024),
                output: "o".repeat(16 * 1024),
                status: crate::chat::ToolStatus::Running,
            },
        }))
        .unwrap();
        let mut tool_previous = tool.window().unwrap().items;
        tool.apply(Event::OutputDelta {
            id: "tool".into(),
            offset: 16 * 1024,
            delta: "δ".into(),
        })
        .unwrap();
        tool.field_bytes.set(0);
        let tool_next = tool.window_reusing(&mut tool_previous).unwrap();
        assert_eq!(tool.field_bytes.get(), "δ".len());
        assert!(
            matches!(&tool_next.changes[0], ItemChange::Append { offset, suffix, .. }
            if *offset == 16 * 1024 && suffix == "δ")
        );
        assert!(
            matches!(&tool_next.items[0].content, ItemContent::Tool { input, output, .. }
            if input == &"i".repeat(16 * 1024) && output == &format!("{}δ", "o".repeat(16 * 1024)))
        );
        // Force the existing post-body-commit secondary-index error path.
        store.db.execute_batch("DROP TABLE file_edits").unwrap();
        assert!(
            store
                .put(prose("live", "q".repeat(64 * 1024)), false)
                .is_err()
        );
        assert!(!store.appended.fields.contains_key("live"));
        assert!(store.changed.as_ref().unwrap().contains("live"));
        let len: i64 = store
            .db
            .query_row("SELECT len0 FROM items WHERE id='live'", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(len, 64 * 1024);
        delta(&mut store, " after-error");
        store.field_bytes.set(0);
        let recovered = store.window_reusing(&mut previous).unwrap();
        assert!(store.field_bytes.get() >= 64 * 1024 + " after-error".len());
        assert!(matches!(&recovered.changes[0], ItemChange::Replace(_)));
        assert!(text(&recovered.items[0]).starts_with('q'));
        assert!(text(&recovered.items[0]).ends_with(" after-error"));
    }
    #[test]
    fn append_prefix_rejects_foreign_and_stale_resident_windows() {
        let da = tempfile::tempdir().unwrap();
        let db = tempfile::tempdir().unwrap();
        let mut a = Store::at(da.path().into()).unwrap();
        let mut b = Store::at(db.path().into()).unwrap();
        a.put(prose("live", "x".repeat(64 * 1024)), true).unwrap();
        b.put(prose("live", "z".repeat(64 * 1024)), true).unwrap();
        let mut foreign = a.window().unwrap().items;
        let own = b.window().unwrap().items;
        assert_ne!(a.copy_cache, b.copy_cache);
        delta(&mut b, "β");
        b.field_bytes.set(0);
        let next = b.window_reusing(&mut foreign).unwrap();
        assert!(b.field_bytes.get() >= 64 * 1024 + "β".len());
        assert!(matches!(&next.changes[0], ItemChange::Replace(_)));
        assert!(text(&next.items[0]).starts_with('z'));
        assert!(text(&next.items[0]).ends_with('β'));
        assert!(!Arc::ptr_eq(&own[0], &next.items[0]));
        let mut stale = next.items.clone();
        b.put(prose("live", "q".repeat(64 * 1024)), false).unwrap();
        let mut resident = next.items;
        let replacement = b.window_reusing(&mut resident).unwrap();
        delta(&mut b, "γ");
        b.field_bytes.set(0);
        let current = b.window_reusing(&mut stale).unwrap();
        assert!(b.field_bytes.get() >= 64 * 1024 + "γ".len());
        assert!(matches!(&current.changes[0], ItemChange::Replace(_)));
        assert!(text(&current.items[0]).starts_with('q'));
        assert!(!Arc::ptr_eq(&replacement.items[0], &current.items[0]));
        // The pre-existing unchanged shortcut may return a foreign resident.
        // It must never turn that Arc into authority for the next append.
        let mut foreign_again = a.window().unwrap().items;
        let mut foreign_published = b.window_reusing(&mut foreign_again).unwrap().items;
        let untrusted_shortcut = text(&foreign_published[0]).starts_with('x');
        delta(&mut b, "λ");
        b.field_bytes.set(0);
        let repaired = b.window_reusing(&mut foreign_published).unwrap();
        assert_eq!(
            text(&repaired.items[0]),
            format!("{}γλ", "q".repeat(64 * 1024))
        );
        if untrusted_shortcut {
            assert!(b.field_bytes.get() >= 64 * 1024 + "γλ".len());
            assert!(matches!(&repaired.changes[0], ItemChange::Replace(_)));
        }
    }
    #[test]
    fn reset_discards_pending_append_and_keeps_provenance_bounded() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::at(directory.path().into()).unwrap();
        store.put(prose("live", "x".repeat(1024)), true).unwrap();
        let mut previous = store.window().unwrap().items;
        delta(&mut store, " committed");
        store.flush_text().unwrap();
        delta(&mut store, "MUST-NOT-REPLAY");
        let cache = store.copy_cache;
        store
            .apply(Event::Reset {
                reason: "test incarnation".into(),
            })
            .unwrap();
        assert!(store.pending_text.is_none());
        assert!(store.appended.fields.is_empty());
        assert!(store.appended.residents.is_empty());
        assert_ne!(store.copy_cache, cache);
        store.put(prose("live", "z".repeat(1024)), true).unwrap();
        delta(&mut store, "β");
        store.field_bytes.set(0);
        let after = store.window_reusing(&mut previous).unwrap();
        assert!(store.field_bytes.get() >= 1024 + "β".len());
        assert!(matches!(&after.changes[0], ItemChange::Replace(_)));
        assert_eq!(text(&after.items[0]), format!("{}β", "z".repeat(1024)));
        for index in 0..WINDOW_ITEMS * 2 + 1 {
            let id = format!("hidden-{index}");
            store.put(prose(&id, "x".into()), true).unwrap();
            store
                .apply(Event::TextDelta {
                    id,
                    role: Role::Assistant,
                    thinking: false,
                    delta: "y".into(),
                })
                .unwrap();
            store.flush_text().unwrap();
        }
        assert!(store.changed.is_none());
        assert!(store.appended.fields.is_empty());
        assert!(store.appended.residents.is_empty());
        store.window().unwrap();
        assert!(store.appended.fields.is_empty());
        assert!(store.appended.residents.len() <= WINDOW_ITEMS);
    }
}
