//! Private, disk-backed transcript. Stable provider IDs and byte-addressed chunks
//! keep both long conversations and a single enormous tool result pageable.
use crate::chat::{Event, Item, ItemContent, Phase, Role};
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    fs,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
};

const PART_BYTES: usize = 128 * 1024;
const WINDOW_BYTES: usize = 3 * 1024 * 1024;
const WINDOW_ITEMS: usize = 128;
/// The serialized item carries identity only, so it is not pageable and must stay small.
const METADATA_LIMIT: usize = 64 * 1024;
/// An identity string a provider can make arbitrarily long is trimmed to this
/// many bytes, so one runaway label costs a label rather than a turn.
const NAME_BYTES: usize = 1024;
/// Patch and snippet text live inside vectors that no chunk can page, so an
/// oversized item keeps at most this much of them. Half the identity budget
/// leaves room for the paths, titles and URLs alongside.
const BULK_BYTES: usize = METADATA_LIMIT / 2;
/// Charged per vector element beside its own text to cover what postcard adds:
/// a variant tag and a length prefix per string. Without it an item of many
/// tiny paths could satisfy the text budget and still overrun the serialized one.
const ELEMENT_BYTES: usize = 16;
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
                CREATE TABLE items(id TEXT PRIMARY KEY, ordinal INTEGER UNIQUE NOT NULL, metadata BLOB NOT NULL, len0 INTEGER NOT NULL, len1 INTEGER NOT NULL, len2 INTEGER NOT NULL);
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
    /// Every unbounded field becomes a byte-addressed chunk, so the serialized item
    /// holds identity alone. Returns a non-fatal warning instead of an error when an
    /// item's identity still cannot be cached intact: a single unrepresentable item
    /// must never end the conversation.
    fn put(&mut self, mut item: Item, older: bool) -> Result<Option<String>, String> {
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
            return Ok(None);
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
        let (body0, body1, body2) = match &mut item.content {
            ItemContent::Text { text, .. } => (std::mem::take(text), String::new(), String::new()),
            // A command's output is the only field of it that grows without a
            // bound; its label, argv and directory are identity.
            ItemContent::Command { output, .. } => {
                (std::mem::take(output), String::new(), String::new())
            }
            // A patch and a search snippet sit inside a vector, and a chunk is
            // addressed by byte offset, so a paged body could never be split
            // back into the elements it came from. These payloads stay in the
            // item and `shrink` bounds them instead.
            ItemContent::Changes { .. } | ItemContent::WebSearch { .. } => {
                (String::new(), String::new(), String::new())
            }
            ItemContent::Tool { input, output, .. } => {
                (std::mem::take(input), std::mem::take(output), String::new())
            }
            // An inline attachment is a body, not identity: a screenshot data URL is
            // routinely megabytes and would otherwise blow the identity budget.
            ItemContent::Image { description, path } => (
                std::mem::take(description),
                String::new(),
                std::mem::take(path),
            ),
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
        let mut metadata = postcard::to_stdvec(&item).map_err(|e| e.to_string())?;
        let mut warning = None;
        if metadata.len() > METADATA_LIMIT {
            // Every identity field a provider controls is trimmed rather than
            // dropping a turn of the conversation.
            shrink(&mut item.content);
            metadata = postcard::to_stdvec(&item).map_err(|e| e.to_string())?;
            warning = Some(format!(
                "Provider item {} exceeded the 64 KiB identity budget and was shortened to fit",
                brief(&item.id)
            ));
        }
        if metadata.len() > METADATA_LIMIT {
            return Ok(Some(format!(
                "Provider item {} exceeds the 64 KiB identity budget and was not cached",
                brief(&item.id)
            )));
        }
        let transaction = self.db.transaction().map_err(|e| e.to_string())?;
        transaction.execute("INSERT INTO items VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(id) DO UPDATE SET metadata=excluded.metadata,len0=excluded.len0,len1=excluded.len1,len2=excluded.len2", params![item.id,ordinal,metadata,len0,body1.len() as i64,body2.len() as i64]).map_err(|e| e.to_string())?;
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
        insert_body(&transaction, &item.id, 2, 0, &body2)?;
        transaction.commit().map_err(|e| e.to_string())?;
        self.dirty = true;
        Ok(warning)
    }
    /// Returns the events the store does not own, plus any non-fatal warning raised
    /// while caching. Warnings are events, never errors: the caller must stay alive.
    pub fn apply(&mut self, event: Event) -> Result<Vec<Event>, String> {
        let mut out = Vec::new();
        if !matches!(&event, Event::TextDelta { .. })
            && let Some(warning) = self.flush_text()?
        {
            out.push(nonfatal(warning));
        }
        match event {
            Event::Item(item) => {
                if let Some(warning) = self.put(item, false)? {
                    out.push(nonfatal(warning));
                }
            }
            Event::History {
                mut items,
                has_earlier,
            } => {
                self.provider_earlier = has_earlier;
                let mut seen = std::collections::BTreeSet::new();
                items.retain(|item| seen.insert(item.id.clone()));
                let mut affected = 0usize;
                let mut first_warning = None;
                for item in items.into_iter().rev() {
                    if let Some(warning) = self.put(item, true)? {
                        affected += 1;
                        if first_warning.is_none() {
                            first_warning = Some(warning);
                        }
                    }
                }
                if let Some(warning) = first_warning {
                    out.push(nonfatal(if affected > 1 {
                        format!("{warning} ({affected} replayed items affected)")
                    } else {
                        warning
                    }));
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
                    if let Some(warning) = self.flush_text()? {
                        out.push(nonfatal(warning));
                    }
                }
                if delta.len() > 64 * 1024 {
                    if let Some(warning) = self.append(&id, role, thinking, &delta)? {
                        out.push(nonfatal(warning));
                    }
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
                out.push(Event::Reset { reason });
                return Ok(out);
            }
            other => {
                out.push(other);
                return Ok(out);
            }
        }
        Ok(out)
    }
    fn flush_text(&mut self) -> Result<Option<String>, String> {
        match self.pending_text.take() {
            Some(pending) => match self.append(&pending.0, pending.1, pending.2, &pending.3) {
                Ok(warning) => Ok(warning),
                Err(error) => {
                    self.pending_text = Some(pending);
                    Err(error)
                }
            },
            None => Ok(None),
        }
    }
    fn append(
        &mut self,
        id: &str,
        role: Role,
        thinking: bool,
        delta: &str,
    ) -> Result<Option<String>, String> {
        let mut warning = None;
        if !self.exists(id)? {
            warning = self.put(
                Item {
                    id: id.into(),
                    role,
                    // A delta stream carries no phase, so the shell this
                    // synthesizes to hold the text names the default rather
                    // than inventing narration the provider never declared.
                    content: ItemContent::Text {
                        text: String::new(),
                        thinking,
                        phase: Phase::Final,
                    },
                },
                false,
            )?;
            if !self.exists(id)? {
                // The identity itself could not be cached; the delta has nowhere to go.
                return Ok(warning);
            }
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
        Ok(warning)
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
        let _ = self.flush_text()?;
        let records = {
            let mut statement = self.db.prepare("SELECT id,ordinal,metadata,len0,len1,len2 FROM items WHERE ordinal<=?1 ORDER BY ordinal DESC LIMIT 129").map_err(|e| e.to_string())?;
            let rows = statement
                .query_map([self.end.map_or(i64::MAX, |cursor| cursor.0)], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, i64>(3)? as usize,
                        row.get::<_, i64>(4)? as usize,
                        row.get::<_, i64>(5)? as usize,
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
        'records: for (id, ordinal, metadata, len0, len1, len2) in records {
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
                // An inline attachment is never paged: half a base64 payload renders as
                // nothing, so it is restored whole or replaced by an explanation.
                let attachment =
                    if len2 > 0 && metadata.len() + (end - start) + len2 + 128 <= WINDOW_BYTES {
                        Some(
                            String::from_utf8(self.field(&id, 2, 0, len2)?)
                                .map_err(|e| e.to_string())?,
                        )
                    } else {
                        None
                    };
                let attached = attachment.as_ref().map_or(0, String::len);
                match &mut item.content {
                    ItemContent::Text { text, .. } => *text = body0,
                    ItemContent::Command {
                        label,
                        command,
                        output,
                        ..
                    } => {
                        *output = body0;
                        if parts > 1 {
                            // The view shows the command itself when the
                            // provider supplied no label, so a paged command
                            // has to name itself in the label that now carries
                            // which part of the output this is.
                            let base = if label.is_empty() {
                                command.clone()
                            } else {
                                std::mem::take(label)
                            };
                            *label = format!("{base} (part {}/{parts})", part + 1);
                        }
                    }
                    // Bounded when the item was cached, so there is no chunked
                    // body to restore here.
                    ItemContent::Changes { .. } | ItemContent::WebSearch { .. } => {}
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
                    ItemContent::Image { path, description } => {
                        *description = body0;
                        match attachment {
                            Some(data) => *path = data,
                            None if len2 > 0 => {
                                *description = format!(
                                    "{description} (inline image of {} KiB exceeds the presentation window)",
                                    len2 / 1024
                                );
                            }
                            None => {}
                        }
                    }
                }
                let size = metadata.len() + end - start + 128 + attached;
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
/// Bounds the identity fields a provider can make arbitrarily large so an
/// oversized item is shortened rather than lost. The payloads that live in a
/// vector share one budget and the vector is cut where that budget runs out,
/// because an item naming a thousand paths cannot carry a useful patch for
/// each of them anyway and its paths are worth more than its last patch.
fn shrink(content: &mut ItemContent) {
    match content {
        ItemContent::Command {
            label,
            command,
            working_directory,
            ..
        } => {
            clamp(label, NAME_BYTES);
            clamp(command, NAME_BYTES);
            clamp(working_directory, NAME_BYTES);
        }
        ItemContent::Changes { changes, .. } => {
            let mut budget = BULK_BYTES;
            changes.retain_mut(|change| {
                clamp(&mut change.path, NAME_BYTES);
                let Some(room) = budget.checked_sub(change.path.len() + ELEMENT_BYTES) else {
                    return false;
                };
                clamp_lines(&mut change.diff, room);
                budget = room - change.diff.len();
                true
            });
        }
        ItemContent::WebSearch { query, results } => {
            clamp(query, NAME_BYTES);
            let mut budget = BULK_BYTES;
            results.retain_mut(|result| {
                clamp(&mut result.title, NAME_BYTES);
                clamp(&mut result.url, NAME_BYTES);
                let spent = result.title.len() + result.url.len() + ELEMENT_BYTES;
                let Some(room) = budget.checked_sub(spent) else {
                    return false;
                };
                clamp(&mut result.snippet, room);
                budget = room - result.snippet.len();
                true
            });
        }
        ItemContent::Tool { name, .. } => clamp(name, NAME_BYTES),
        // Prose and attachments keep nothing but their role and ID here; their
        // text was already taken out into chunks.
        ItemContent::Text { .. } | ItemContent::Image { .. } => {}
    }
}
/// Truncates to at most `limit` bytes without splitting a UTF-8 sequence.
fn clamp(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    let mut end = limit;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
}
/// Truncates patch text after the last newline that fits in `limit`.
///
/// A unified diff is line-structured: the first byte of a line decides whether
/// that line is added, removed or context, and a hunk header declares how many
/// of each follow. A patch cut mid-line therefore does not merely lose text, it
/// can survive as an intact-looking patch describing an edit the provider never
/// made, so the cut only ever lands on a line boundary and the remainder stays
/// a true prefix of what was sent.
fn clamp_lines(text: &mut String, limit: usize) {
    if text.len() <= limit {
        return;
    }
    // A newline is ASCII, so a position found in the byte prefix is always a
    // character boundary.
    let end = text.as_bytes()[..limit]
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(0, |index| index + 1);
    text.truncate(end);
}
/// A caching shortfall degrades presentation; it never terminates the provider.
fn nonfatal(message: String) -> Event {
    Event::Error {
        message,
        fatal: false,
    }
}
/// Provider IDs are untrusted and unbounded; diagnostics quote only a prefix.
fn brief(id: &str) -> &str {
    let mut end = id.len().min(64);
    while end > 0 && !id.is_char_boundary(end) {
        end -= 1;
    }
    &id[..end]
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
                    phase: Phase::Final,
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
    /// A 2 MiB screenshot used to be rejected by the 64 KiB identity guard, which
    /// killed the provider and made the session unreopenable.
    #[test]
    fn inline_image_far_past_the_identity_budget_round_trips() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        let payload = format!("data:image/png;base64,{}", "QUJD".repeat(512 * 1024));
        let warnings = store
            .apply(Event::Item(Item {
                id: "shot".into(),
                role: Role::User,
                content: ItemContent::Image {
                    path: payload.clone(),
                    description: "screenshot".into(),
                },
            }))
            .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let window = store.window().unwrap();
        assert_eq!(window.items.len(), 1);
        match &window.items[0].content {
            ItemContent::Image { path, description } => {
                assert_eq!(path, &payload);
                assert_eq!(description, "screenshot");
            }
            other => panic!("unexpected content {other:?}"),
        }
        // The session stays usable: later items still apply and page.
        store
            .apply(Event::Item(Item {
                id: "after".into(),
                role: Role::Assistant,
                content: ItemContent::Text {
                    text: "ok".into(),
                    thinking: false,
                    phase: Phase::Final,
                },
            }))
            .unwrap();
        assert_eq!(store.window().unwrap().items.len(), 2);
    }
    /// Unbounded identity metadata degrades presentation instead of ending the actor.
    #[test]
    fn oversized_identity_metadata_reports_without_failing() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        let events = store
            .apply(Event::Item(Item {
                id: "call".into(),
                role: Role::Assistant,
                content: ItemContent::Tool {
                    name: "n".repeat(128 * 1024),
                    input: "{}".into(),
                    output: String::new(),
                    status: crate::chat::ToolStatus::Running,
                },
            }))
            .unwrap();
        assert!(
            matches!(events.as_slice(), [Event::Error { fatal: false, .. }]),
            "{events:?}"
        );
        let window = store.window().unwrap();
        assert_eq!(window.items.len(), 1);
        match &window.items[0].content {
            ItemContent::Tool { name, input, .. } => {
                assert!(name.len() <= NAME_BYTES);
                assert_eq!(input, "{}");
            }
            other => panic!("unexpected content {other:?}"),
        }
    }
    /// A patch cut mid-line can stay syntactically intact while describing an
    /// edit nobody made, so an oversized change set must lose whole lines only.
    #[test]
    fn oversized_change_set_is_truncated_on_a_line_boundary() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        let mut diff = String::from("@@ -1 +1,8000 @@\n-gone\n");
        for line in 0..8000 {
            diff.push_str(&format!("+line {line} of the provider's patch\n"));
        }
        assert!(diff.len() > PART_BYTES);
        let events = store
            .apply(Event::Item(Item {
                id: "edit".into(),
                role: Role::Assistant,
                content: ItemContent::Changes {
                    changes: vec![crate::chat::FileChange {
                        path: "src/app.rs".into(),
                        kind: crate::chat::ChangeKind::Updated,
                        diff: diff.clone(),
                    }],
                    status: crate::chat::ToolStatus::Succeeded,
                },
            }))
            .unwrap();
        assert!(
            matches!(events.as_slice(), [Event::Error { fatal: false, .. }]),
            "{events:?}"
        );
        let window = store.window().unwrap();
        assert_eq!(window.items.len(), 1);
        match &window.items[0].content {
            ItemContent::Changes { changes, .. } => {
                let [change] = changes.as_slice() else {
                    panic!("unexpected change set {changes:?}");
                };
                assert_eq!(change.path, "src/app.rs");
                assert!(change.diff.len() < diff.len(), "patch was not bounded");
                assert!(
                    diff.starts_with(change.diff.as_str()),
                    "surviving patch is not a prefix of what the provider sent"
                );
                assert!(
                    change.diff.ends_with('\n'),
                    "patch was cut inside a line: {:?}",
                    &change.diff[change.diff.len().saturating_sub(64)..]
                );
                for line in change.diff.lines() {
                    assert!(
                        line.starts_with(|first: char| matches!(first, '@' | '+' | '-' | ' ')),
                        "surviving patch has a line no diff parser accepts: {line:?}"
                    );
                }
            }
            other => panic!("unexpected content {other:?}"),
        }
    }
    /// Command output is the one field of a command that pages, so it must come
    /// back from the chunk it was written to rather than from an empty one.
    #[test]
    fn command_output_pages_without_losing_bytes() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        let original = "cargo output line\n".repeat(20_000);
        assert!(original.len() > 2 * PART_BYTES);
        store
            .apply(Event::Item(Item {
                id: "exec".into(),
                role: Role::Assistant,
                content: ItemContent::Command {
                    label: "Run cargo".into(),
                    command: "cargo test".into(),
                    working_directory: "/repo".into(),
                    action: crate::chat::CommandAction::Run,
                    status: crate::chat::ToolStatus::Succeeded,
                    exit_code: Some(0),
                    duration_ms: Some(12),
                    output: original.clone(),
                },
            }))
            .unwrap();
        let window = store.window().unwrap();
        let parts = original.len().div_ceil(PART_BYTES);
        assert_eq!(window.items.len(), parts);
        let mut restored = String::new();
        for (index, item) in window.items.iter().enumerate() {
            match &item.content {
                ItemContent::Command {
                    label,
                    command,
                    output,
                    exit_code,
                    ..
                } => {
                    assert_eq!(command, "cargo test");
                    assert_eq!(*exit_code, Some(0));
                    assert_eq!(label, &format!("Run cargo (part {}/{parts})", index + 1));
                    restored.push_str(output);
                }
                other => panic!("unexpected content {other:?}"),
            }
        }
        assert_eq!(restored, original);
    }
}
