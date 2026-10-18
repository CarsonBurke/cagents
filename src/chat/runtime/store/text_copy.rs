//! Logical text authority and bounded reads, independent of presentation parts.
use super::*;
use crate::chat::{TextCopyPage, TextCopyScope, TextCopySource, TextCopySources};

pub(super) const SCHEMA: &str = "
CREATE TABLE copy_sources(
 token INTEGER PRIMARY KEY AUTOINCREMENT,
 kind INTEGER NOT NULL, owner TEXT NOT NULL, item TEXT NOT NULL,
 revision INTEGER NOT NULL, bytes INTEGER NOT NULL, retained INTEGER NOT NULL,
 UNIQUE(kind,owner,item));
CREATE TABLE copy_child_chunks(
 token INTEGER NOT NULL REFERENCES copy_sources(token) ON DELETE CASCADE,
 position INTEGER NOT NULL, data BLOB NOT NULL, PRIMARY KEY(token,position));";

// A private, per-actor disk cache. Existing child presentation bounds remain
// unchanged. Admission refuses new bodies when full; it never evicts a source
// behind an outstanding descriptor or reports a retained prefix as complete.
const CHILD_BYTES: usize = 128 * 1024 * 1024;
const CHILD_SOURCES: usize = 32 * 64;
const CHILD_CHUNK_BYTES: usize = 4 * 1024;
type Record = (u64, u64, usize, usize);

fn owner(scope: &TextCopyScope) -> (i64, &str) {
    match scope {
        TextCopyScope::Conversation => (0, ""),
        TextCopyScope::Subagent { id } => (1, id),
    }
}

impl Store {
    fn copy_record(&self, scope: &TextCopyScope, id: &str) -> Result<Option<Record>, String> {
        let (kind, parent) = owner(scope);
        self.db.query_row(
            "SELECT token,revision,bytes,retained FROM copy_sources WHERE kind=?1 AND owner=?2 AND item=?3",
            params![kind,parent,id],
            |row| Ok((row.get(0)?,row.get(1)?,row.get::<_,i64>(2)? as usize,row.get::<_,i64>(3)? as usize)),
        ).optional().map_err(|error| error.to_string())
    }

    fn copy_descriptor(&self, scope: TextCopyScope, record: Record) -> TextCopySource {
        TextCopySource {
            cache: self.copy_cache,
            scope,
            token: record.0,
            revision: record.1,
            bytes: record.2 as u64,
        }
    }

    pub(super) fn copy_parent_source(&self, id: &str) -> Result<Option<TextCopySource>, String> {
        Ok(self
            .copy_record(&TextCopyScope::Conversation, id)?
            .map(|record| self.copy_descriptor(TextCopyScope::Conversation, record)))
    }

    pub(super) fn copy_parent_prefix(&self, id: &str, text: &str) -> Result<bool, String> {
        let Some((_, _, previous, _)) = self.copy_record(&TextCopyScope::Conversation, id)? else {
            return Ok(false);
        };
        if previous > text.len() {
            return Ok(false);
        }
        for start in (0..previous).step_by(PART_BYTES) {
            let end = (start + PART_BYTES).min(previous);
            if self.field(id, 0, start, end)? != text.as_bytes()[start..end] {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(super) fn copy_parent_put(
        db: &Connection,
        id: &str,
        bytes: Option<usize>,
        prefix: bool,
    ) -> Result<(), String> {
        match bytes {
            Some(bytes) => {
                db.execute("INSERT INTO copy_sources(kind,owner,item,revision,bytes,retained) VALUES(0,'',?1,1,?2,?2) ON CONFLICT(kind,owner,item) DO UPDATE SET revision=revision+?3,bytes=excluded.bytes,retained=excluded.retained",params![id,bytes as i64,i64::from(!prefix)]).map_err(|error|error.to_string())?;
            }
            None => {
                db.execute("DELETE FROM copy_sources WHERE kind=0 AND item=?1", [id])
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(())
    }

    pub(super) fn copy_parent_append(
        db: &Connection,
        id: &str,
        bytes: usize,
    ) -> Result<(), String> {
        // Missing initial content is not proof that the first observed suffix
        // begins the provider message. Only an authoritative Item can admit it.
        db.execute(
            "UPDATE copy_sources SET bytes=bytes+?2,retained=retained+?2 WHERE kind=0 AND item=?1",
            params![id, bytes as i64],
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    }

    pub(super) fn copy_remove(
        &mut self,
        scope: &TextCopyScope,
        ids: &[String],
    ) -> Result<(), String> {
        let (kind, parent) = owner(scope);
        let transaction = self.db.transaction().map_err(|error| error.to_string())?;
        for id in ids {
            transaction
                .execute(
                    "DELETE FROM copy_sources WHERE kind=?1 AND owner=?2 AND item=?3",
                    params![kind, parent, id],
                )
                .map_err(|error| error.to_string())?;
        }
        transaction.commit().map_err(|error| error.to_string())
    }

    fn copy_child_field(&self, token: u64, start: usize, end: usize) -> Result<Vec<u8>, String> {
        let mut statement=self.db.prepare_cached("SELECT position,data FROM copy_child_chunks WHERE token=?1 AND position<?3 AND position+length(data)>?2 ORDER BY position").map_err(|error|error.to_string())?;
        let rows = statement
            .query_map(params![token, start as i64, end as i64], |row| {
                Ok((row.get::<_, i64>(0)? as usize, row.get::<_, Vec<u8>>(1)?))
            })
            .map_err(|error| error.to_string())?;
        let mut result = Vec::with_capacity(end - start);
        let mut next = start;
        for row in rows {
            let (position, data) = row.map_err(|error| error.to_string())?;
            let from = start.saturating_sub(position);
            let until = (end - position).min(data.len());
            if position + from != next || from > until {
                return Err("Full message has an unavailable cached range".into());
            }
            result.extend_from_slice(&data[from..until]);
            next = position + until;
        }
        if next != end {
            return Err("Full message has an unavailable cached range".into());
        }
        Ok(result)
    }

    fn copy_child_write(
        &mut self,
        scope: TextCopyScope,
        id: &str,
        text: &str,
        append: bool,
    ) -> Result<(), String> {
        if id.len() > METADATA_LIMIT
            || owner(&scope).1.len() > METADATA_LIMIT
            || (append && text.is_empty())
        {
            return Ok(());
        }
        let previous = self.copy_record(&scope, id)?;
        if append && previous.is_none() {
            return Ok(());
        }
        let (kind, parent) = owner(&scope);
        let total = if append {
            previous
                .unwrap()
                .2
                .checked_add(text.len())
                .ok_or("Message length overflow")?
        } else {
            text.len()
        };
        let mut prefix = append;
        let mut unchanged = false;
        if !append
            && let Some((token, _, old, retained)) = previous
            && old == retained
            && old <= text.len()
        {
            // Compare only at ingestion, not publication. Each scratch buffer is
            // one existing Store chunk; identical replay performs no writes.
            prefix = true;
            for start in (0..old).step_by(PART_BYTES) {
                let end = (start + PART_BYTES).min(old);
                if self.copy_child_field(token, start, end)? != text.as_bytes()[start..end] {
                    prefix = false;
                    break;
                }
            }
            unchanged = prefix && old == total;
        }
        if unchanged {
            return Ok(());
        }
        if previous.is_none() {
            let count: i64 = self
                .db
                .query_row(
                    "SELECT COUNT(*) FROM copy_sources WHERE kind=1",
                    [],
                    |row| row.get(0),
                )
                .map_err(|error| error.to_string())?;
            if count as usize >= CHILD_SOURCES {
                return Ok(());
            }
        }
        let retained_total: i64 = self
            .db
            .query_row(
                "SELECT COALESCE(SUM(retained),0) FROM copy_sources WHERE kind=1",
                [],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        let old_retained = previous.map_or(0, |record| record.3);
        let keep_prefix = prefix && previous.is_some();
        let complete_prefix = !append || previous.is_none_or(|record| record.2 == record.3);
        let retained = if complete_prefix
            && total <= crate::chat::TEXT_COPY_LIMIT_BYTES
            && (retained_total as usize)
                .saturating_sub(old_retained)
                .saturating_add(total)
                <= CHILD_BYTES
        {
            total
        } else if keep_prefix {
            old_retained
        } else {
            0
        };
        let transaction = self.db.transaction().map_err(|error| error.to_string())?;
        transaction.execute("INSERT INTO copy_sources(kind,owner,item,revision,bytes,retained) VALUES(?1,?2,?3,1,?4,?5) ON CONFLICT(kind,owner,item) DO UPDATE SET revision=revision+?6,bytes=excluded.bytes,retained=excluded.retained",params![kind,parent,id,total as i64,retained as i64,i64::from(!keep_prefix)]).map_err(|error|error.to_string())?;
        let token: u64 = transaction
            .query_row(
                "SELECT token FROM copy_sources WHERE kind=?1 AND owner=?2 AND item=?3",
                params![kind, parent, id],
                |row| row.get(0),
            )
            .map_err(|error| error.to_string())?;
        if !keep_prefix {
            transaction
                .execute("DELETE FROM copy_child_chunks WHERE token=?1", [token])
                .map_err(|error| error.to_string())?;
        }
        let start = if keep_prefix { old_retained } else { 0 };
        if retained > start {
            let added = if append {
                text.as_bytes()
            } else {
                &text.as_bytes()[start..retained]
            };
            // Append admission is all-or-nothing. Existing prefixes are kept
            // when a later suffix cannot be admitted, with bytes>retained.
            let mut remaining = added;
            let mut position = start;
            if keep_prefix {
                let tail: Option<(usize, Vec<u8>)> = transaction.query_row(
                    "SELECT position,data FROM copy_child_chunks WHERE token=?1 ORDER BY position DESC LIMIT 1",
                    [token], |row| Ok((row.get::<_, i64>(0)? as usize, row.get(1)?)),
                ).optional().map_err(|error| error.to_string())?;
                if let Some((tail_position, mut tail)) = tail
                    && tail_position + tail.len() == start
                    && tail.len() < CHILD_CHUNK_BYTES
                {
                    let take = (CHILD_CHUNK_BYTES - tail.len()).min(remaining.len());
                    tail.extend_from_slice(&remaining[..take]);
                    transaction
                        .execute(
                            "UPDATE copy_child_chunks SET data=?3 WHERE token=?1 AND position=?2",
                            params![token, tail_position as i64, tail],
                        )
                        .map_err(|error| error.to_string())?;
                    remaining = &remaining[take..];
                    position += take;
                }
            }
            // Only the <=4 KiB tail is rewritten. This also bounds row overhead
            // for providers delivering one-byte deltas, independently of size.
            for chunk in remaining.chunks(CHILD_CHUNK_BYTES) {
                transaction
                    .execute(
                        "INSERT INTO copy_child_chunks(token,position,data) VALUES(?1,?2,?3)",
                        params![token, position as i64, chunk],
                    )
                    .map_err(|error| error.to_string())?;
                position += chunk.len();
            }
        }
        transaction.commit().map_err(|error| error.to_string())
    }

    pub(super) fn copy_child_event(&mut self, event: &Event) -> Result<(), String> {
        match event {
            Event::SubagentItem { id, item } => {
                let scope = TextCopyScope::Subagent { id: id.clone() };
                if let ItemContent::Text { text, .. } = &item.content {
                    self.copy_child_write(scope, &item.id, text, false)
                } else {
                    self.copy_remove(&scope, std::slice::from_ref(&item.id))
                }
            }
            Event::SubagentTextDelta {
                id, item_id, delta, ..
            } => self.copy_child_write(
                TextCopyScope::Subagent { id: id.clone() },
                item_id,
                delta,
                true,
            ),
            Event::SubagentItemsRemoved { id, ids } => {
                self.copy_remove(&TextCopyScope::Subagent { id: id.clone() }, ids)
            }
            _ => Ok(()),
        }
    }

    pub(crate) fn publish_copy_sources(&self, state: &mut crate::chat::Snapshot) {
        let mut sources = TextCopySources::default();
        if !self.copy_failed {
            sources.conversation = self.copy_projection.clone();
            for child in &state.subagents {
                if child.items.is_empty() {
                    continue;
                }
                let scope = TextCopyScope::Subagent {
                    id: child.id.clone(),
                };
                let entries = child
                    .items
                    .iter()
                    .map(|item| {
                        self.copy_record(&scope, &item.id)
                            .ok()
                            .flatten()
                            .map(|record| self.copy_descriptor(scope.clone(), record))
                    })
                    .collect();
                sources.children.insert(child.id.clone(), entries);
            }
        }
        state.text_copy_sources = sources;
    }

    pub(crate) fn text_copy(
        &self,
        source: &TextCopySource,
        offset: u64,
    ) -> Result<TextCopyPage, String> {
        if self.copy_failed || source.cache != self.copy_cache {
            return Err("Full message is unavailable in this owned cache".into());
        }
        if source.bytes > crate::chat::TEXT_COPY_LIMIT_BYTES as u64 {
            return Err(
                "Message exceeds the 32 MiB clipboard limit; use its provider transcript".into(),
            );
        }
        let record: Option<(i64, String, String, u64, usize, usize)> = self
            .db
            .query_row(
                "SELECT kind,owner,item,revision,bytes,retained FROM copy_sources WHERE token=?1",
                [source.token],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get::<_, i64>(4)? as usize,
                        row.get::<_, i64>(5)? as usize,
                    ))
                },
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some((kind, parent, id, revision, total, retained)) = record else {
            return Err("Full message is no longer available in the owned cache".into());
        };
        let (expected_kind, expected_parent) = owner(&source.scope);
        if kind != expected_kind
            || parent != expected_parent
            || revision != source.revision
            || source.bytes as usize > total
        {
            return Err("Message changed; copy its current text again".into());
        }
        if source.bytes as usize > retained {
            return Err(
                "Full message exceeds the available child text cache; use its provider transcript"
                    .into(),
            );
        }
        if offset > source.bytes {
            return Err("Invalid message copy range".into());
        }
        let start = usize::try_from(offset).map_err(|_| "Invalid message copy offset")?;
        let mut end = start
            .saturating_add(crate::chat::TEXT_COPY_PAGE_BYTES)
            .min(source.bytes as usize);
        // Include enough bytes to find a UTF-8 boundary without splitting a code
        // point. The result remains bounded by PAGE_BYTES.
        let mut bytes = if kind == 0 {
            self.field(&id, 0, start, end)?
        } else {
            self.copy_child_field(source.token, start, end)?
        };
        if bytes.len() != end - start {
            return Err("Full message has an unavailable cached range".into());
        }
        if end < source.bytes as usize {
            while std::str::from_utf8(&bytes).is_err_and(|error| error.error_len().is_none()) {
                bytes.pop();
                end -= 1;
            }
        }
        let text = String::from_utf8(bytes).map_err(|_| "Invalid UTF-8 message copy range")?;
        if text.is_empty() && offset < source.bytes {
            return Err("Message copy made no progress".into());
        }
        Ok(TextCopyPage {
            source: source.clone(),
            offset,
            text,
            done: end == source.bytes as usize,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, text: &str) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: Phase::Final,
            },
        }
    }
    fn source(store: &mut Store, id: &str) -> TextCopySource {
        let window = store.window().unwrap();
        let index = window.items.iter().position(|item| item.id == id).unwrap();
        store.copy_projection[index].clone().unwrap()
    }
    fn read(store: &Store, source: &TextCopySource) -> Result<String, String> {
        let mut body = String::new();
        loop {
            let page = store.text_copy(source, body.len() as u64)?;
            assert_eq!(page.source, *source);
            assert_eq!(page.offset, body.len() as u64);
            assert!(page.text.len() <= crate::chat::TEXT_COPY_PAGE_BYTES);
            body.push_str(&page.text);
            if page.done {
                return Ok(body);
            }
        }
    }

    #[test]
    fn logical_copy_reads_entire_store_message_across_utf8_parts_and_unloaded_prefix() {
        let mut store = Store::new().unwrap();
        // The first four-byte character crosses the nominal 128 KiB cut.
        let text = format!("{}{}tail", "a".repeat(PART_BYTES - 1), "🦀é".repeat(30_000));
        assert!(text.len() > 300 * 1024);
        store
            .apply(Event::Item(item(
                "opaque:native-part:neighbor",
                "other answer",
            )))
            .unwrap();
        store.apply(Event::Item(item("answer", &text))).unwrap();
        let window = store.window().unwrap();
        let parts: Vec<_> = window
            .items
            .iter()
            .filter(|entry| entry.id != "opaque:native-part:neighbor")
            .collect();
        assert!(parts.len() >= 3);
        let sources: Vec<_> = window
            .items
            .iter()
            .zip(&store.copy_projection)
            .filter(|(entry, _)| entry.id != "opaque:native-part:neighbor")
            .map(|(_, source)| source.clone().unwrap())
            .collect();
        assert!(sources.windows(2).all(|pair| pair[0] == pair[1]));
        assert_eq!(
            read(&store, &sources[parts.len() - 1]).unwrap().as_bytes(),
            text.as_bytes()
        );
        let neighbor = source(&mut store, "opaque:native-part:neighbor");
        assert_eq!(read(&store, &neighbor).unwrap(), "other answer");
        // A logical response whose beginning is not even resident remains exact.
        let large = "界🦀".repeat(WINDOW_BYTES / 7 + 100_000);
        store.apply(Event::Item(item("larger", &large))).unwrap();
        let window = store.window().unwrap();
        assert!(window.items.iter().all(|item| item.id != "larger"));
        let descriptor = store.copy_projection.last().unwrap().clone().unwrap();
        assert_eq!(read(&store, &descriptor).unwrap(), large);
    }

    #[test]
    fn logical_copy_prefix_survives_verified_append_but_not_rewrite_removal_reset_or_missing_base()
    {
        let mut store = Store::new().unwrap();
        store
            .apply(Event::Item(item("answer", "first 🦀")))
            .unwrap();
        let old = source(&mut store, "answer");
        store
            .apply(Event::TextDelta {
                id: "answer".into(),
                role: Role::Assistant,
                thinking: false,
                delta: " suffix".into(),
            })
            .unwrap();
        store.window().unwrap();
        assert_eq!(read(&store, &old).unwrap(), "first 🦀");
        store
            .apply(Event::Item(item("answer", "first 🦀 suffix again")))
            .unwrap();
        assert_eq!(read(&store, &old).unwrap(), "first 🦀");
        store
            .apply(Event::Item(item("answer", "replacement")))
            .unwrap();
        assert!(read(&store, &old).is_err());
        let replacement = source(&mut store, "answer");
        store
            .apply(Event::ItemsRemoved {
                ids: vec!["answer".into()],
            })
            .unwrap();
        assert!(read(&store, &replacement).is_err());
        store
            .apply(Event::Item(item("answer", "replacement")))
            .unwrap();
        assert!(read(&store, &replacement).is_err());
        let current = source(&mut store, "answer");
        store
            .apply(Event::Reset {
                reason: "test reset".into(),
            })
            .unwrap();
        assert!(read(&store, &current).is_err());
        store
            .apply(Event::TextDelta {
                id: "missing-base".into(),
                role: Role::Assistant,
                thinking: false,
                delta: "observed suffix".into(),
            })
            .unwrap();
        store.window().unwrap();
        assert!(store.copy_projection.iter().all(Option::is_none));
    }

    #[test]
    fn logical_copy_child_scope_survives_trim_and_never_aliases_parent_or_sibling() {
        let mut store = Store::new().unwrap();
        store.apply(Event::Item(item("same", "parent"))).unwrap();
        let parent = source(&mut store, "same");
        let child = TextCopyScope::Subagent { id: "child".into() };
        let sibling = TextCopyScope::Subagent {
            id: "sibling".into(),
        };
        let body = "🦀界".repeat(50_000);
        store
            .apply(Event::SubagentItem {
                id: "child".into(),
                item: item("same", &body),
            })
            .unwrap();
        store
            .apply(Event::SubagentItem {
                id: "sibling".into(),
                item: item("same", "sibling"),
            })
            .unwrap();
        let child_source = store.copy_descriptor(
            child.clone(),
            store.copy_record(&child, "same").unwrap().unwrap(),
        );
        let sibling_source = store.copy_descriptor(
            sibling.clone(),
            store.copy_record(&sibling, "same").unwrap().unwrap(),
        );
        assert_eq!(read(&store, &parent).unwrap(), "parent");
        assert_eq!(read(&store, &child_source).unwrap(), body);
        assert_eq!(read(&store, &sibling_source).unwrap(), "sibling");
        let mut substituted = child_source.clone();
        substituted.scope = sibling;
        assert!(read(&store, &substituted).is_err());
        let writes = store.db.total_changes();
        store
            .apply(Event::SubagentItem {
                id: "child".into(),
                item: item("same", &body),
            })
            .unwrap();
        assert_eq!(
            store.db.total_changes(),
            writes,
            "identical child replay must not rewrite its body"
        );
        store
            .apply(Event::SubagentItemsRemoved {
                id: "child".into(),
                ids: vec!["same".into()],
            })
            .unwrap();
        assert!(read(&store, &child_source).is_err());
        assert_eq!(read(&store, &parent).unwrap(), "parent");
    }

    #[test]
    fn logical_copy_child_gap_and_budget_refuse_without_fabricating_content() {
        let mut store = Store::new().unwrap();
        let scope = TextCopyScope::Subagent { id: "child".into() };
        store
            .copy_child_write(scope.clone(), "text", "prefix", false)
            .unwrap();
        let old = store.copy_descriptor(
            scope.clone(),
            store.copy_record(&scope, "text").unwrap().unwrap(),
        );
        // Simulate a previously declined suffix after disk admission was full.
        store
            .db
            .execute(
                "UPDATE copy_sources SET bytes=bytes+10 WHERE token=?1",
                [old.token],
            )
            .unwrap();
        store
            .copy_child_write(scope.clone(), "text", "later", true)
            .unwrap();
        let latest = store.copy_descriptor(
            scope.clone(),
            store.copy_record(&scope, "text").unwrap().unwrap(),
        );
        assert_eq!(read(&store, &old).unwrap(), "prefix");
        assert!(read(&store, &latest).unwrap_err().contains("cache"));
        store
            .copy_child_write(
                scope.clone(),
                "text",
                "authoritative recovered full body",
                false,
            )
            .unwrap();
        assert!(read(&store, &old).is_err());
        let recovered = store.copy_descriptor(
            scope.clone(),
            store.copy_record(&scope, "text").unwrap().unwrap(),
        );
        assert_eq!(
            read(&store, &recovered).unwrap(),
            "authoritative recovered full body"
        );
        for _ in 0..100 {
            store
                .copy_child_write(scope.clone(), "text", "a", true)
                .unwrap();
        }
        let rows: i64 = store
            .db
            .query_row("SELECT COUNT(*) FROM copy_child_chunks", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(rows, 1, "tiny deltas must not create one disk row per byte");
        store
            .db
            .execute("DELETE FROM copy_child_chunks", [])
            .unwrap();
        let current = store.copy_descriptor(
            scope.clone(),
            store.copy_record(&scope, "text").unwrap().unwrap(),
        );
        assert!(read(&store, &current).is_err());
        let mut oversized = current;
        oversized.bytes = crate::chat::TEXT_COPY_LIMIT_BYTES as u64 + 1;
        assert!(read(&store, &oversized).unwrap_err().contains("limit"));
    }
    #[test]
    fn logical_copy_authority_commits_with_body_before_failing_file_index() {
        let mut store = Store::new().unwrap();
        store
            .apply(Event::Item(item("answer", "old text")))
            .unwrap();
        let old = source(&mut store, "answer");
        store.db.execute("DROP TABLE file_edits", []).unwrap();
        assert!(
            store
                .apply(Event::Item(item("answer", "new text")))
                .is_err()
        );
        assert!(
            read(&store, &old).is_err(),
            "index failure cannot leave old authority over new bytes"
        );
        let new = store.copy_parent_source("answer").unwrap().unwrap();
        assert_eq!(read(&store, &new).unwrap(), "new text");
    }

    #[test]
    fn logical_copy_projection_preserves_opaque_id_collision_authority() {
        let mut store = Store::new().unwrap();
        let text = "a".repeat(PART_BYTES * 2);
        let collision = "answer:native-part:1:text:131072-262144:0-0";
        store.apply(Event::Item(item("answer", &text))).unwrap();
        store
            .apply(Event::Item(item(collision, "distinct answer")))
            .unwrap();
        let window = store.window().unwrap();
        let copies: Vec<_> = window
            .items
            .iter()
            .zip(&store.copy_projection)
            .filter(|(item, _)| item.id == collision)
            .map(|(_, source)| read(&store, source.as_ref().unwrap()).unwrap())
            .collect();
        assert_eq!(copies, vec![text, "distinct answer".into()]);
    }
}
