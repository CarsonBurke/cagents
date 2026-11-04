//! Parent logical-text authority. Bodies remain in the existing transcript.
use super::*;
use crate::chat::{TextCopyPage, TextCopyScope, TextCopySource};

pub(super) const SCHEMA: &str = "
CREATE TABLE copy_sources(
 token INTEGER PRIMARY KEY AUTOINCREMENT, item TEXT UNIQUE NOT NULL,
 revision INTEGER NOT NULL, bytes INTEGER NOT NULL);";

impl Store {
    pub(super) fn copy_parent_source(&self, id: &str) -> Result<Option<TextCopySource>, String> {
        self.db
            .query_row(
                "SELECT token,revision,bytes FROM copy_sources WHERE item=?1",
                [id],
                |row| {
                    Ok(TextCopySource {
                        cache: self.copy_cache,
                        scope: TextCopyScope::Conversation,
                        token: row.get(0)?,
                        revision: row.get(1)?,
                        bytes: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(|error| error.to_string())
    }
    pub(super) fn copy_parent_prefix(&self, id: &str, text: &str) -> Result<bool, String> {
        let Some(source) = self.copy_parent_source(id)? else {
            return Ok(false);
        };
        let previous = source.bytes as usize;
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
    // Called within the existing body transaction, so no indexing/error path
    // can leave old copy authority referring to replacement bytes.
    pub(super) fn copy_parent_put(
        db: &Connection,
        id: &str,
        bytes: Option<usize>,
        prefix: bool,
    ) -> Result<bool, String> {
        let invalidated = !prefix
            && db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM copy_sources WHERE item=?1)",
                    [id],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(|error| error.to_string())?;
        match bytes {
            Some(bytes) => {
                db.execute("INSERT INTO copy_sources(item,revision,bytes) VALUES(?1,1,?2) ON CONFLICT(item) DO UPDATE SET revision=revision+?3,bytes=excluded.bytes",params![id,bytes as i64,i64::from(!prefix)]).map_err(|error|error.to_string())?;
            }
            None => {
                db.execute("DELETE FROM copy_sources WHERE item=?1", [id])
                    .map_err(|error| error.to_string())?;
            }
        }
        Ok(invalidated)
    }
    pub(super) fn copy_parent_append(
        db: &Connection,
        id: &str,
        bytes: usize,
    ) -> Result<(), String> {
        // A suffix without an authoritative initial Item is not a full message.
        db.execute(
            "UPDATE copy_sources SET bytes=bytes+?2 WHERE item=?1",
            params![id, bytes as i64],
        )
        .map_err(|error| error.to_string())?;
        Ok(())
    }
    pub(super) fn copy_remove(&mut self, ids: &[String]) -> Result<(), String> {
        let transaction = self.db.transaction().map_err(|error| error.to_string())?;
        let mut invalidated = false;
        for id in ids {
            invalidated |= transaction
                .execute("DELETE FROM copy_sources WHERE item=?1", [id])
                .map_err(|error| error.to_string())?
                > 0;
        }
        transaction.commit().map_err(|error| error.to_string())?;
        if invalidated {
            self.copy_revision = self.copy_revision.wrapping_add(1);
        }
        Ok(())
    }
    pub(crate) fn publish_copy_sources(&self, state: &mut crate::chat::Snapshot) {
        // Child descriptors belong to the existing bounded runtime items;
        // publishing parent pages must not replace that independent authority.
        state.text_copy_sources.conversation_revision = self.copy_revision;
        state.text_copy_sources.conversation = self.copy_projection.clone();
    }
    pub(crate) fn text_copy(
        &self,
        source: &TextCopySource,
        offset: u64,
    ) -> Result<TextCopyPage, String> {
        if source.scope != TextCopyScope::Conversation || source.cache != self.copy_cache {
            return Err("Full message is unavailable in this owned cache".into());
        }
        if source.bytes > crate::chat::TEXT_COPY_LIMIT_BYTES as u64 {
            return Err(
                "Message exceeds the 32 MiB clipboard limit; use its provider transcript".into(),
            );
        }
        let record: Option<(String, u64, u64)> = self
            .db
            .query_row(
                "SELECT item,revision,bytes FROM copy_sources WHERE token=?1",
                [source.token],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|error| error.to_string())?;
        let Some((id, revision, total)) = record else {
            return Err("Full message is no longer available in the owned cache".into());
        };
        if revision != source.revision || source.bytes > total {
            return Err("Message changed; copy its current text again".into());
        }
        if offset > source.bytes {
            return Err("Invalid message copy range".into());
        }
        let start = usize::try_from(offset).map_err(|_| "Invalid message copy offset")?;
        let mut end = start
            .saturating_add(crate::chat::TEXT_COPY_PAGE_BYTES)
            .min(source.bytes as usize);
        let mut bytes = self.field(&id, 0, start, end)?;
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

    fn context(id: &str, body: &str) -> Item {
        Item {
            role: Role::System,
            content: ItemContent::Delivery {
                kind: crate::chat::DeliveryKind::Notice,
                from: String::new(),
                sender: String::new(),
                summary: "Claude context".into(),
                body: body.into(),
            },
            ..item(id, "")
        }
    }

    #[test]
    fn provider_context_copy_reads_unloaded_body_and_revokes_stale_authority() {
        let mut store = Store::new().unwrap();
        let body = format!(
            "FIRST_CONTEXT\r\n{}MIDDLE_CONTEXT\n{}LAST_CONTEXT λ\r\n",
            "🦀é".repeat(WINDOW_BYTES / 6),
            "literal context\n".repeat(1024),
        );
        assert!(body.len() > WINDOW_BYTES);
        store.apply(Event::Item(context("same", &body))).unwrap();
        let window = store.window().unwrap();
        assert!(window.items.iter().all(|item| item.id != "same"));
        let descriptors: Vec<_> = store.copy_projection.iter().cloned().collect();
        assert!(descriptors.len() > 1);
        let descriptor = descriptors[0]
            .clone()
            .expect("paged context has full-copy authority");
        assert!(
            descriptors
                .iter()
                .all(|source| source.as_ref() == Some(&descriptor))
        );
        assert!(window.items.iter().all(|item| {
            item.role == Role::System
                && item
                    .logical_copy_body()
                    .is_some_and(|body| !body.contains("FIRST_CONTEXT"))
        }));
        assert_eq!(
            read(&store, &descriptor).unwrap().as_bytes(),
            body.as_bytes()
        );
        let mut published = crate::chat::Snapshot::default();
        store.publish_copy_sources(&mut published);
        assert_eq!(published.text_copy_sources.conversation, descriptors);

        let appended = format!("{body}append");
        store
            .apply(Event::Item(context("same", &appended)))
            .unwrap();
        assert_eq!(read(&store, &descriptor).unwrap(), body);
        store
            .apply(Event::Item(context("same", "short recovered body λ\r\n")))
            .unwrap();
        assert!(read(&store, &descriptor).is_err());
        let short = source(&mut store, "same");
        assert_eq!(read(&store, &short).unwrap(), "short recovered body λ\r\n");
        let mut user_notice = context("same", "short recovered body λ\r\n");
        user_notice.role = Role::User;
        store.apply(Event::Item(user_notice)).unwrap();
        assert!(read(&store, &short).is_err());
        store.window().unwrap();
        assert!(store.copy_projection.iter().all(Option::is_none));
        store
            .apply(Event::Item(context("same", "complete again")))
            .unwrap();
        let recovered = source(&mut store, "same");
        assert_eq!(read(&store, &recovered).unwrap(), "complete again");
        store
            .apply(Event::ItemsRemoved {
                ids: vec!["same".into()],
            })
            .unwrap();
        assert!(read(&store, &recovered).is_err());
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
        let mut user = item("user-message", &text);
        user.role = Role::User;
        store.apply(Event::Item(user)).unwrap();
        let user_source = source(&mut store, "user-message");
        assert_eq!(
            read(&store, &user_source).unwrap().as_bytes(),
            text.as_bytes()
        );
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
    fn logical_copy_completion_fence_tracks_destructive_edits_but_not_paging_or_append() {
        let mut store = Store::new().unwrap();
        store.apply(Event::Item(item("answer", "first"))).unwrap();
        let original = source(&mut store, "answer");
        let mut snapshot = crate::chat::Snapshot::default();
        store.publish_copy_sources(&mut snapshot);
        let revision = snapshot.text_copy_sources.conversation_revision;
        store
            .apply(Event::Item(item("answer", "first suffix")))
            .unwrap();
        store
            .apply(Event::Item(item("other", "other body")))
            .unwrap();
        for index in 0..WINDOW_ITEMS + 1 {
            store
                .apply(Event::Item(item(&format!("later-{index}"), "later body")))
                .unwrap();
        }
        assert!(
            store
                .window()
                .unwrap()
                .items
                .iter()
                .all(|item| item.id != "answer")
        );
        assert!(store.earlier().unwrap());
        store.window().unwrap();
        store.publish_copy_sources(&mut snapshot);
        assert_eq!(snapshot.text_copy_sources.conversation_revision, revision);
        assert_eq!(read(&store, &original).unwrap(), "first");
        store
            .apply(Event::Item(item("answer", "rewritten")))
            .unwrap();
        store.publish_copy_sources(&mut snapshot);
        assert_eq!(
            snapshot.text_copy_sources.conversation_revision,
            revision + 1
        );
        // An unrelated removal also cancels pending completion conservatively.
        store
            .apply(Event::ItemsRemoved {
                ids: vec!["other".into()],
            })
            .unwrap();
        store.publish_copy_sources(&mut snapshot);
        assert_eq!(
            snapshot.text_copy_sources.conversation_revision,
            revision + 2
        );
        store
            .apply(Event::Reset {
                reason: "reset".into(),
            })
            .unwrap();
        store.publish_copy_sources(&mut snapshot);
        assert_eq!(
            snapshot.text_copy_sources.conversation_revision,
            revision + 3
        );
    }

    #[test]
    fn logical_copy_authority_commits_with_body_before_failing_file_index() {
        let mut store = Store::new().unwrap();
        store
            .apply(Event::Item(item("answer", "old text")))
            .unwrap();
        let old = source(&mut store, "answer");
        let old_revision = store.copy_revision;
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
        assert_eq!(store.copy_revision, old_revision + 1);
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
