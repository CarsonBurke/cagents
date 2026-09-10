//! Private, disk-backed transcript. Stable provider IDs and byte-addressed chunks
//! keep both long conversations and a single enormous tool result pageable.
use crate::chat::{Event, Item, ItemContent, Role};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
};

const PART_BYTES: usize = 128 * 1024;
const WINDOW_BYTES: usize = 3 * 1024 * 1024;
const WINDOW_ITEMS: usize = 128;
type Cursor = (i64, usize);
pub(super) struct Window {
    pub items: Vec<Item>,
    pub has_earlier: bool,
    pub has_later: bool,
}
pub(crate) struct Store {
    db: Connection,
    directory: PathBuf,
    end: Option<Cursor>,
    first: Option<Cursor>,
    cached_earlier: bool,
    provider_earlier: bool,
    pending_earlier: Option<Cursor>,
    pub dirty: bool,
    pending_text: Option<(String, Role, bool, String)>,
}
impl Store {
    pub fn new() -> Result<Self, String> {
        #[cfg(test)]
        let root = std::env::temp_dir();
        #[cfg(not(test))]
        let root = crate::daemon::RuntimePaths::discover()
            .map_err(|e| e.to_string())?
            .directory;
        Self::at(root)
    }
    fn at(root: PathBuf) -> Result<Self, String> {
        let directory = root.join(format!("chat-{}", uuid::Uuid::new_v4()));
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(|e| format!("Cannot create private transcript cache: {e}"))?;
        let path = directory.join("transcript.sqlite");
        let result = (|| {
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| e.to_string())?;
            let db = Connection::open(&path).map_err(|e| e.to_string())?;
            db.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA cache_size=-8192; PRAGMA foreign_keys=ON;
                CREATE TABLE items(id TEXT PRIMARY KEY, ordinal INTEGER UNIQUE NOT NULL, metadata BLOB NOT NULL, len0 INTEGER NOT NULL, len1 INTEGER NOT NULL);
                CREATE TABLE chunks(id TEXT NOT NULL REFERENCES items(id) ON DELETE CASCADE, field INTEGER NOT NULL, position INTEGER NOT NULL, data BLOB NOT NULL, PRIMARY KEY(id,field,position));").map_err(|e| e.to_string())?;
            Ok(Self {
                db,
                directory: directory.clone(),
                end: None,
                first: None,
                cached_earlier: false,
                provider_earlier: false,
                pending_earlier: None,
                dirty: true,
                pending_text: None,
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&directory);
        }
        result
    }
    fn exists(&self, id: &str) -> Result<bool, String> {
        self.db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM items WHERE id=?1)",
                [id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())
    }
    fn put(&mut self, mut item: Item, older: bool) -> Result<(), String> {
        let previous: Option<(i64, Vec<u8>, i64)> = self
            .db
            .query_row(
                "SELECT ordinal,metadata,len0 FROM items WHERE id=?1",
                [&item.id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if older && previous.is_some() {
            return Ok(());
        }
        let mut preserve_input = false;
        if let Some((_, metadata, _)) = &previous {
            let old: Item = postcard::from_bytes(metadata).map_err(|e| e.to_string())?;
            if let (
                ItemContent::Tool { name, input, .. },
                ItemContent::Tool { name: old_name, .. },
            ) = (&mut item.content, old.content)
            {
                if name.is_empty() {
                    *name = old_name;
                }
                preserve_input = input.is_empty();
            }
        }
        let (body0, body1) = match &mut item.content {
            ItemContent::Text { text, .. } => (std::mem::take(text), String::new()),
            ItemContent::Tool { input, output, .. } => {
                (std::mem::take(input), std::mem::take(output))
            }
            ItemContent::Image { description, .. } => (std::mem::take(description), String::new()),
        };
        let ordinal = match &previous {
            Some((ordinal, _, _)) => *ordinal,
            None => self
                .db
                .query_row(
                    if older {
                        "SELECT COALESCE(MIN(ordinal),0)-1 FROM items"
                    } else {
                        "SELECT COALESCE(MAX(ordinal),0)+1 FROM items"
                    },
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?,
        };
        let len0 = if preserve_input {
            previous.as_ref().map_or(0, |(_, _, length)| *length)
        } else {
            body0.len() as i64
        };
        let metadata = postcard::to_stdvec(&item).map_err(|e| e.to_string())?;
        if metadata.len() > 64 * 1024 {
            return Err("Provider item identity/name/path metadata exceeds 64 KiB".into());
        }
        let transaction = self.db.transaction().map_err(|e| e.to_string())?;
        transaction.execute("INSERT INTO items VALUES(?1,?2,?3,?4,?5) ON CONFLICT(id) DO UPDATE SET metadata=excluded.metadata,len0=excluded.len0,len1=excluded.len1", params![item.id,ordinal,metadata,len0,body1.len() as i64]).map_err(|e| e.to_string())?;
        if preserve_input {
            transaction
                .execute("DELETE FROM chunks WHERE id=?1 AND field=1", [&item.id])
                .map_err(|e| e.to_string())?;
        } else {
            transaction
                .execute("DELETE FROM chunks WHERE id=?1", [&item.id])
                .map_err(|e| e.to_string())?;
            insert_body(&transaction, &item.id, 0, 0, &body0)?;
        }
        insert_body(&transaction, &item.id, 1, 0, &body1)?;
        transaction.commit().map_err(|e| e.to_string())?;
        self.dirty = true;
        Ok(())
    }
    pub fn apply(&mut self, event: Event) -> Result<Option<Event>, String> {
        if !matches!(&event, Event::TextDelta { .. }) {
            self.flush_text()?;
        }
        match event {
            Event::Item(item) => self.put(item, false)?,
            Event::History {
                mut items,
                has_earlier,
            } => {
                self.provider_earlier = has_earlier;
                let mut seen = std::collections::BTreeSet::new();
                items.retain(|item| seen.insert(item.id.clone()));
                for item in items.into_iter().rev() {
                    self.put(item, true)?;
                }
                if let Some(first) = self.pending_earlier.take()
                    && self
                        .db
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM items WHERE ordinal<?1)",
                            [first.0],
                            |row| row.get::<_, bool>(0),
                        )
                        .map_err(|e| e.to_string())?
                {
                    self.end = Some(first);
                }
                self.dirty = true;
            }
            Event::TextDelta {
                id,
                role,
                thinking,
                delta,
            } => {
                if self.pending_text.as_ref().is_some_and(|pending| {
                    pending.0 != id
                        || pending.1 != role
                        || pending.2 != thinking
                        || pending.3.len() + delta.len() > 64 * 1024
                }) {
                    self.flush_text()?;
                }
                if delta.len() > 64 * 1024 {
                    self.append(&id, role, thinking, &delta)?;
                } else if let Some(pending) = &mut self.pending_text {
                    pending.3.push_str(&delta);
                } else {
                    self.pending_text = Some((id, role, thinking, delta));
                }
                self.dirty = true;
            }
            Event::ItemsRemoved { ids } => {
                let transaction = self.db.transaction().map_err(|e| e.to_string())?;
                for id in ids {
                    transaction
                        .execute("DELETE FROM items WHERE id=?1", [id])
                        .map_err(|e| e.to_string())?;
                }
                transaction.commit().map_err(|e| e.to_string())?;
                self.dirty = true;
            }
            Event::Reset { reason } => {
                self.db
                    .execute("DELETE FROM items", [])
                    .map_err(|e| e.to_string())?;
                self.end = None;
                self.first = None;
                self.provider_earlier = false;
                self.cached_earlier = false;
                self.pending_earlier = None;
                self.dirty = true;
                return Ok(Some(Event::Reset { reason }));
            }
            other => return Ok(Some(other)),
        }
        Ok(None)
    }
    fn flush_text(&mut self) -> Result<(), String> {
        if let Some(pending) = self.pending_text.take()
            && let Err(error) = self.append(&pending.0, pending.1, pending.2, &pending.3)
        {
            self.pending_text = Some(pending);
            return Err(error);
        }
        Ok(())
    }
    fn append(&mut self, id: &str, role: Role, thinking: bool, delta: &str) -> Result<(), String> {
        if !self.exists(id)? {
            self.put(
                Item {
                    id: id.into(),
                    role,
                    content: ItemContent::Text {
                        text: String::new(),
                        thinking,
                    },
                },
                false,
            )?;
        }
        let (metadata, length): (Vec<u8>, i64) = self
            .db
            .query_row("SELECT metadata,len0 FROM items WHERE id=?1", [id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })
            .map_err(|e| e.to_string())?;
        let item: Item = postcard::from_bytes(&metadata).map_err(|e| e.to_string())?;
        if !matches!(item.content, ItemContent::Text { .. }) {
            return Err(format!(
                "Provider reused non-text item ID {id} for a text delta"
            ));
        }
        if !delta.is_empty() {
            let transaction = self.db.transaction().map_err(|e| e.to_string())?;
            insert_body(&transaction, id, 0, length, delta)?;
            transaction
                .execute(
                    "UPDATE items SET len0=len0+?2 WHERE id=?1",
                    params![id, delta.len() as i64],
                )
                .map_err(|e| e.to_string())?;
            transaction.commit().map_err(|e| e.to_string())?;
        }
        Ok(())
    }
    /// Returns true when earlier history is already cached (no provider command).
    pub fn earlier(&mut self) -> Result<bool, String> {
        if self.cached_earlier {
            if let Some(first) = self.first {
                self.end = Some(first);
            }
            self.dirty = true;
            Ok(true)
        } else if self.provider_earlier {
            self.pending_earlier = self.first;
            Ok(false)
        } else {
            Err("No earlier transcript is available".into())
        }
    }
    pub fn latest(&mut self) {
        self.end = None;
        self.pending_earlier = None;
        self.dirty = true;
    }
    fn field(&self, id: &str, field: i64, start: usize, end: usize) -> Result<Vec<u8>, String> {
        if end <= start {
            return Ok(Vec::new());
        }
        let mut statement = self.db.prepare_cached("SELECT substr(data,MAX(?3-position,0)+1,MIN(position+length(data),?4)-MAX(position,?3)) FROM chunks WHERE id=?1 AND field=?2 AND position>=COALESCE((SELECT MAX(position) FROM chunks WHERE id=?1 AND field=?2 AND position<=?3),?3) AND position<?4 AND position+length(data)>?3 ORDER BY position").map_err(|e| e.to_string())?;
        let rows = statement
            .query_map(params![id, field, start as i64, end as i64], |row| {
                row.get::<_, Vec<u8>>(0)
            })
            .map_err(|e| e.to_string())?;
        let mut result = Vec::with_capacity(end - start);
        for row in rows {
            result.extend(row.map_err(|e| e.to_string())?);
        }
        Ok(result)
    }
    fn body(&self, id: &str, len0: usize, start: usize, end: usize) -> Result<Vec<u8>, String> {
        let mut result = self.field(id, 0, start.min(len0), end.min(len0))?;
        if end > len0 {
            result.extend(self.field(id, 1, start.saturating_sub(len0), end - len0)?);
        }
        Ok(result)
    }
    fn boundary(&self, id: &str, len0: usize, target: usize) -> Result<usize, String> {
        if target == 0 {
            return Ok(0);
        }
        let start = target.saturating_sub(3);
        let bytes = self.body(id, len0, start, target + 1)?;
        Ok(bytes
            .iter()
            .rposition(|byte| byte & 0xc0 != 0x80)
            .map_or(target, |index| start + index)
            .min(target))
    }
    pub(super) fn window(&mut self) -> Result<Window, String> {
        self.flush_text()?;
        let records = {
            let mut statement = self.db.prepare("SELECT id,ordinal,metadata,len0,len1 FROM items WHERE ordinal<=?1 ORDER BY ordinal DESC LIMIT 129").map_err(|e| e.to_string())?;
            let rows = statement
                .query_map([self.end.map_or(i64::MAX, |cursor| cursor.0)], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)? as usize,
                        row.get::<_, i64>(4)? as usize,
                    ))
                })
                .map_err(|e| e.to_string())?;
            rows.collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?
        };
        let mut items = Vec::new();
        let mut bytes = 0;
        let mut first = None;
        let mut newest = None;
        'records: for (id, ordinal, metadata, len0, len1) in records {
            let base: Item = postcard::from_bytes(&metadata).map_err(|e| e.to_string())?;
            let total = len0 + len1;
            let parts = total.max(1).div_ceil(PART_BYTES);
            let until = if self.end.is_some_and(|cursor| cursor.0 == ordinal) {
                self.end.unwrap().1.min(parts)
            } else {
                parts
            };
            for part in (0..until).rev() {
                let start = self.boundary(&id, len0, part * PART_BYTES)?;
                let end = if (part + 1) * PART_BYTES >= total {
                    total
                } else {
                    self.boundary(&id, len0, (part + 1) * PART_BYTES)?
                };
                let mut item = base.clone();
                if part > 0 {
                    item.id = format!("{id}:native-part:{part}");
                }
                let body0 =
                    String::from_utf8(self.field(&id, 0, start.min(len0), end.min(len0))?)
                        .map_err(|e| e.to_string())?;
                let body1 = String::from_utf8(self.field(
                    &id,
                    1,
                    start.saturating_sub(len0),
                    end.saturating_sub(len0),
                )?)
                .map_err(|e| e.to_string())?;
                match &mut item.content {
                    ItemContent::Text { text, .. } => *text = body0,
                    ItemContent::Tool {
                        name,
                        input,
                        output,
                        ..
                    } => {
                        *input = body0;
                        *output = body1;
                        if parts > 1 {
                            *name = format!("{name} (part {}/{parts})", part + 1);
                        }
                    }
                    ItemContent::Image { description, .. } => *description = body0,
                }
                let size = metadata.len() + end - start + 128;
                if !items.is_empty() && (bytes + size > WINDOW_BYTES || items.len() == WINDOW_ITEMS)
                {
                    break 'records;
                }
                if size > WINDOW_BYTES {
                    return Err(
                        "Provider item metadata exceeds native presentation capacity".into(),
                    );
                }
                if newest.is_none() {
                    newest = Some((ordinal, part));
                }
                bytes += size;
                first = Some((ordinal, part));
                items.push(item);
            }
        }
        items.reverse();
        self.cached_earlier = match first {
            Some((ordinal, part)) => {
                part > 0
                    || self
                        .db
                        .query_row(
                            "SELECT EXISTS(SELECT 1 FROM items WHERE ordinal<?1)",
                            [ordinal],
                            |row| row.get::<_, bool>(0),
                        )
                        .map_err(|e| e.to_string())?
            }
            None => false,
        };
        self.first = first;
        self.dirty = false;
        let has_later = if self.end.is_none() {
            false
        } else if let Some((ordinal, part)) = newest {
            self.db.query_row("SELECT EXISTS(SELECT 1 FROM items WHERE ordinal>?1 OR (ordinal=?1 AND MAX(len0+len1,1)>?2))",params![ordinal,((part+1)*PART_BYTES) as i64],|row|row.get::<_,bool>(0)).map_err(|e|e.to_string())?
        } else {
            self.db
                .query_row("SELECT EXISTS(SELECT 1 FROM items)", [], |row| {
                    row.get::<_, bool>(0)
                })
                .map_err(|e| e.to_string())?
        };
        Ok(Window {
            items,
            has_earlier: self.cached_earlier || self.provider_earlier,
            has_later,
        })
    }
}
impl Drop for Store {
    fn drop(&mut self) {
        // This UUID directory was created exclusively by this store, never discovered by a scan.
        let _ = self.db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)");
        let _ = fs::remove_dir_all(&self.directory);
    }
}
fn insert_body(
    transaction: &rusqlite::Transaction<'_>,
    id: &str,
    field: i64,
    offset: i64,
    body: &str,
) -> Result<(), String> {
    let mut statement = transaction
        .prepare_cached("INSERT INTO chunks VALUES(?1,?2,?3,?4)")
        .map_err(|e| e.to_string())?;
    let mut start = 0;
    while start < body.len() {
        let mut end = (start + PART_BYTES).min(body.len());
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        statement
            .execute(params![
                id,
                field,
                offset + start as i64,
                &body.as_bytes()[start..end]
            ])
            .map_err(|e| e.to_string())?;
        start = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::Role;
    #[test]
    fn oversized_unicode_item_pages_without_losing_bytes() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        let original = "abcé🙂".repeat(600_000);
        store
            .apply(Event::Item(Item {
                id: "large".into(),
                role: Role::Assistant,
                content: ItemContent::Text {
                    text: original.clone(),
                    thinking: false,
                },
            }))
            .unwrap();
        let mut pages = Vec::new();
        loop {
            let page = store.window().unwrap();
            let value = page
                .items
                .into_iter()
                .map(|item| match item.content {
                    ItemContent::Text { text, .. } => text,
                    _ => panic!("unexpected content"),
                })
                .collect::<String>();
            assert!(value.len() <= WINDOW_BYTES);
            pages.push(value);
            if !page.has_earlier {
                break;
            }
            assert!(store.earlier().unwrap());
        }
        pages.reverse();
        assert_eq!(pages.concat(), original);
        store.latest();
        assert!(!store.window().unwrap().has_later);
    }
}
