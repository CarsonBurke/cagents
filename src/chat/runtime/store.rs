//! Private, disk-backed transcript. Stable provider IDs and byte-addressed chunks
//! keep both long conversations and a single enormous tool result pageable.
use super::observed_at;
use crate::chat::{Event, Item, ItemContent, Phase, Role};
use base64::Engine as _;
use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
};

const PART_BYTES: usize = 128 * 1024;
const WINDOW_BYTES: usize = 3 * 1024 * 1024;
const WINDOW_ITEMS: usize = 128;
// Leave room between provider items for overlapping history pages to supply
// missing records without renumbering the whole conversation on every page.
const ORDINAL_STRIDE: i64 = 1 << 32;
const ORDINAL_LIMIT: i64 = i64::MAX / 2;
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
#[derive(Clone, Copy)]
enum LatestBoundary {
    BeforeAll,
    After(i64),
}
pub(super) struct Window {
    pub items: Vec<std::sync::Arc<Item>>,
    pub changes: Vec<crate::chat::presentation::ItemChange>,
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
    latest_boundary: Option<LatestBoundary>,
    pub dirty: bool,
    /// Dirty identities are bounded even while the conversation is hidden.
    changed: Option<std::collections::HashSet<String>>,
    pub presented_generation: Option<u64>,
    pub presentation: Option<crate::chat::presentation::Presentation>,
    pub presented_children: std::collections::HashMap<String, Vec<std::sync::Arc<Item>>>,
    pending_text: Option<(String, Role, bool, String)>,
    /// Recognizes delivered messages as they enter the transcript.
    messages: Option<std::sync::Arc<crate::agent_execution::mailbox::Seal>>,
}
impl Store {
    /// Decode once at ingestion. Snapshots carry a small path, never megabytes
    /// of base64, and identical payloads share the same private file.
    fn image_file(&self, url: &str) -> Result<String, String> {
        if url.len() > crate::chat::MAX_INLINE_IMAGE_URL_BYTES {
            return Err("Image exceeds the 16 MiB image limit".into());
        }
        let (header, encoded) = url.split_once(";base64,").ok_or("Invalid inline image")?;
        let extension = match header {
            "data:image/png" => "png",
            "data:image/jpeg" => "jpg",
            "data:image/webp" => "webp",
            "data:image/gif" => "gif",
            _ => return Err("Unsupported inline image format".into()),
        };
        let digest = Sha256::digest(url.as_bytes());
        let path = self.directory.join(format!("image-{digest:x}.{extension}"));
        if !path.exists() {
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(encoded)
                .map_err(|_| "Invalid inline image encoding")?;
            if bytes.len() > crate::chat::MAX_INLINE_IMAGE_BYTES {
                return Err("Image exceeds the 16 MiB image limit".into());
            }
            let mut file = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .open(&path)
                .map_err(|e| e.to_string())?;
            if let Err(error) = file.write_all(&bytes) {
                let _ = fs::remove_file(&path);
                return Err(error.to_string());
            }
        }
        Ok(path.to_string_lossy().into_owned())
    }
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
                latest_boundary: None,
                messages: None,
                dirty: true,
                changed: Some(Default::default()),
                presented_generation: None,
                presentation: None,
                presented_children: Default::default(),
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
    fn put(&mut self, item: Item, older: bool) -> Result<Option<String>, String> {
        self.put_at(item, older, None)
    }
    fn put_at(
        &mut self,
        mut item: Item,
        older: bool,
        assigned: Option<i64>,
    ) -> Result<Option<String>, String> {
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
        // An adapter cannot know wall-clock time, so a live observation is
        // stamped where the daemon actually sees it. Replayed history is not
        // something the daemon just watched happen, so it keeps whatever the
        // provider supplied.
        if !older {
            item.at = observed_at();
            item.started_at = item.at;
        }
        let mut preserve_input = false;
        if let Some((_, metadata, _)) = &previous {
            let old: Item = postcard::from_bytes(metadata).map_err(|e| e.to_string())?;
            item.started_at = old.started_at.or(old.at).or(item.started_at);
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
        let mut warning = None;
        if let ItemContent::Image { path, description } = &mut item.content
            && path.starts_with("data:")
        {
            match self.image_file(path) {
                Ok(file) => *path = file,
                Err(error) => {
                    path.clear();
                    *description = format!("Image unavailable: {error}");
                    warning = Some(error);
                }
            }
        }
        let (body0, body1, body2) = match &mut item.content {
            ItemContent::Text { text, .. } => (std::mem::take(text), String::new(), String::new()),
            // A delivery's body is as unbounded as prose; who sent it is identity.
            ItemContent::Delivery { body, .. } => {
                (std::mem::take(body), String::new(), String::new())
            }
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
            // Descriptions and provider paths can grow; inline bytes have
            // already become a short private path above.
            ItemContent::Image { description, path } => (
                std::mem::take(description),
                String::new(),
                std::mem::take(path),
            ),
        };
        let ordinal = match &previous {
            Some((ordinal, _, _)) => *ordinal,
            None => match assigned {
                Some(ordinal) => ordinal,
                None => self.edge_ordinals(older, 1)?[0],
            },
        };
        let len0 = if preserve_input {
            previous.as_ref().map_or(0, |(_, _, length)| *length)
        } else {
            body0.len() as i64
        };
        let mut metadata = postcard::to_stdvec(&item).map_err(|e| e.to_string())?;
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
        self.changed(&item.id);
        self.dirty = true;
        Ok(warning)
    }

    fn ordinal(&self, id: &str) -> Result<Option<i64>, String> {
        self.db
            .prepare_cached("SELECT ordinal FROM items WHERE id=?1")
            .map_err(|e| e.to_string())?
            .query_row([id], |row| row.get(0))
            .optional()
            .map_err(|e| e.to_string())
    }

    fn spaced(left: Option<i64>, right: Option<i64>, count: usize) -> Option<Vec<i64>> {
        let count = i128::try_from(count).ok()?;
        let stride = i128::from(ORDINAL_STRIDE);
        let (start, step) = match (left, right) {
            (Some(left), Some(right)) => {
                let step = (i128::from(right) - i128::from(left)) / (count + 1);
                (i128::from(left), step)
            }
            (Some(left), None) => (i128::from(left), stride),
            (None, Some(right)) => (i128::from(right) - stride * (count + 1), stride),
            (None, None) => (0, stride),
        };
        if step <= 0 {
            return None;
        }
        (1..=count)
            .map(|index| {
                let ordinal = start + index * step;
                (ordinal.abs() <= i128::from(ORDINAL_LIMIT)).then(|| ordinal as i64)
            })
            .collect()
    }

    fn edge_ordinals(&mut self, earlier: bool, count: usize) -> Result<Vec<i64>, String> {
        for _ in 0..2 {
            let edge: Option<i64> = self
                .db
                .query_row(
                    if earlier {
                        "SELECT MIN(ordinal) FROM items"
                    } else {
                        "SELECT MAX(ordinal) FROM items"
                    },
                    [],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            if let Some(ordinals) = Self::spaced(
                (!earlier).then_some(edge).flatten(),
                earlier.then_some(edge).flatten(),
                count,
            ) {
                return Ok(ordinals);
            }
            self.rebalance()?;
        }
        Err("Transcript item order exceeds cache capacity".into())
    }

    fn history_ordinals(
        &mut self,
        before: Option<&str>,
        after: Option<&str>,
        count: usize,
    ) -> Result<Vec<i64>, String> {
        if before.is_none() && after.is_none() {
            return self.edge_ordinals(true, count);
        }
        for _ in 0..2 {
            let bounds = if let Some(id) = before {
                let right = self.ordinal(id)?.ok_or("History anchor disappeared")?;
                let left = self
                    .db
                    .query_row(
                        "SELECT MAX(ordinal) FROM items WHERE ordinal<?1",
                        [right],
                        |row| row.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                (left, Some(right))
            } else {
                let left = self
                    .ordinal(after.unwrap())?
                    .ok_or("History anchor disappeared")?;
                let right = self
                    .db
                    .query_row(
                        "SELECT MIN(ordinal) FROM items WHERE ordinal>?1",
                        [left],
                        |row| row.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                (Some(left), right)
            };
            if let Some(ordinals) = Self::spaced(bounds.0, bounds.1, count) {
                return Ok(ordinals);
            }
            self.rebalance()?;
        }
        Err("Transcript history cannot be ordered between its anchors".into())
    }

    fn latest_ordinals(&mut self, count: usize) -> Result<Vec<i64>, String> {
        if self.latest_boundary.is_none() {
            return self.edge_ordinals(false, count);
        }
        for _ in 0..2 {
            let left: Option<i64> = match self.latest_boundary {
                Some(LatestBoundary::After(ordinal)) => self
                    .db
                    .query_row(
                        "SELECT MAX(ordinal) FROM items WHERE ordinal<=?1",
                        [ordinal],
                        |row| row.get(0),
                    )
                    .map_err(|e| e.to_string())?,
                _ => None,
            };
            let right: Option<i64> = self
                .db
                .query_row(
                    "SELECT MIN(ordinal) FROM items WHERE ordinal>?1",
                    [left.unwrap_or(i64::MIN)],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            if let Some(ordinals) = Self::spaced(left, right, count) {
                return Ok(ordinals);
            }
            self.rebalance()?;
        }
        Err("Transcript latest page cannot be ordered before live output".into())
    }

    /// Rebalance only when repeated insertion has exhausted an integer gap.
    /// The temporary range is disjoint from every normally allocated ordinal,
    /// so UNIQUE stays enforced through both phases of the atomic update.
    fn rebalance(&mut self) -> Result<(), String> {
        let transaction = self.db.transaction().map_err(|e| e.to_string())?;
        transaction.execute_batch("CREATE TEMP TABLE ordinal_remap(old INTEGER PRIMARY KEY, temporary INTEGER UNIQUE NOT NULL, new INTEGER UNIQUE NOT NULL);").map_err(|e| e.to_string())?;
        let count: i64 = transaction
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .map_err(|e| e.to_string())?;
        if count > ORDINAL_LIMIT / ORDINAL_STRIDE {
            return Err("Transcript item order exceeds cache capacity".into());
        }
        transaction.execute("INSERT INTO ordinal_remap SELECT ordinal,?1+ROW_NUMBER() OVER(ORDER BY ordinal),?2*ROW_NUMBER() OVER(ORDER BY ordinal) FROM items", params![i64::MIN, ORDINAL_STRIDE]).map_err(|e| e.to_string())?;
        let remap = |cursor: Option<Cursor>| -> Result<Option<Cursor>, String> {
            cursor
                .map(|(ordinal, part)| {
                    let mapped = transaction
                        .query_row(
                            "SELECT new FROM ordinal_remap WHERE old=?1",
                            [ordinal],
                            |row| row.get(0),
                        )
                        .optional()
                        .map_err(|e| e.to_string())?;
                    Ok(mapped.map(|ordinal| (ordinal, part)))
                })
                .transpose()
                .map(Option::flatten)
        };
        let (end, first, pending_earlier) = (
            remap(self.end)?,
            remap(self.first)?,
            remap(self.pending_earlier)?,
        );
        let latest_boundary = match self.latest_boundary {
            Some(LatestBoundary::After(ordinal)) => {
                let mapped: Option<i64> = transaction
                    .query_row(
                        "SELECT MAX(new) FROM ordinal_remap WHERE old<=?1",
                        [ordinal],
                        |row| row.get(0),
                    )
                    .map_err(|e| e.to_string())?;
                Some(mapped.map_or(LatestBoundary::BeforeAll, LatestBoundary::After))
            }
            boundary => boundary,
        };
        transaction.execute_batch("UPDATE items SET ordinal=(SELECT temporary FROM ordinal_remap WHERE old=items.ordinal);
            UPDATE items SET ordinal=(SELECT new FROM ordinal_remap WHERE temporary=items.ordinal);
            DROP TABLE ordinal_remap;").map_err(|e| e.to_string())?;
        transaction.commit().map_err(|e| e.to_string())?;
        self.end = end;
        self.first = first;
        self.pending_earlier = pending_earlier;
        self.latest_boundary = latest_boundary;
        self.dirty = true;
        Ok(())
    }

    fn merge_history(&mut self, items: Vec<Item>, earlier: bool) -> Result<Vec<String>, String> {
        let mut run = Vec::new();
        let mut after = None;
        let mut warnings = Vec::new();
        for item in items {
            if self.ordinal(&item.id)?.is_some() {
                if !run.is_empty() {
                    let ordinals =
                        self.history_ordinals(Some(&item.id), after.as_deref(), run.len())?;
                    for (item, ordinal) in run.drain(..).zip(ordinals) {
                        if let Some(warning) = self.put_at(item, true, Some(ordinal))? {
                            warnings.push(warning);
                        }
                    }
                }
                after = Some(item.id);
            } else {
                run.push(item);
            }
        }
        if !run.is_empty() {
            let ordinals = if after.is_none() {
                if earlier {
                    self.edge_ordinals(true, run.len())?
                } else {
                    self.latest_ordinals(run.len())?
                }
            } else {
                self.history_ordinals(None, after.as_deref(), run.len())?
            };
            for (item, ordinal) in run.into_iter().zip(ordinals) {
                if let Some(warning) = self.put_at(item, true, Some(ordinal))? {
                    warnings.push(warning);
                }
            }
        }
        Ok(warnings)
    }
    /// Shows the prompts `seal` sealed as the messages they deliver, from the
    /// next event on.
    pub(crate) fn recognize_messages(
        &mut self,
        seal: Option<std::sync::Arc<crate::agent_execution::mailbox::Seal>>,
    ) {
        self.messages = seal;
    }
    /// Returns the events the store does not own, plus any non-fatal warning raised
    /// while caching. Warnings are events, never errors: the caller must stay alive.
    pub fn apply(&mut self, mut event: Event) -> Result<Vec<Event>, String> {
        if let Some(seal) = &self.messages {
            match &mut event {
                Event::Item(item) => seal.recognize(item),
                Event::History { items, .. } | Event::LatestHistory { items, .. } => {
                    items.iter_mut().for_each(|item| seal.recognize(item));
                }
                _ => {}
            }
        }
        let mut out = Vec::new();
        let earlier = matches!(&event, Event::History { .. });
        if !matches!(&event, Event::TextDelta { .. })
            && let Some(warning) = self.flush_text()?
        {
            out.push(nonfatal(warning));
        }
        match event {
            Event::OutputDelta { id, offset, delta } => {
                let cached: Option<(Vec<u8>, usize, usize)> = self
                    .db
                    .query_row(
                        "SELECT metadata,len0,len1 FROM items WHERE id=?1",
                        [&id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()
                    .map_err(|error| error.to_string())?;
                let Some((metadata, len0, len1)) = cached else {
                    // Oversized identities are deliberately refused by put_at
                    // without terminating the provider. Their later appends
                    // must preserve that same nonfatal cache boundary.
                    out.push(nonfatal(format!(
                        "Output for uncached provider item {} was not cached",
                        brief(&id)
                    )));
                    return Ok(out);
                };
                let mut item: Item =
                    postcard::from_bytes(&metadata).map_err(|error| error.to_string())?;
                let (field, length) = match item.content {
                    ItemContent::Command { .. } => (0, len0),
                    ItemContent::Tool { .. } => (1, len1),
                    _ => return Err("Output delta names a non-tool item".into()),
                };
                if length != offset {
                    return Err("Output delta does not follow the cached output".into());
                }
                let at = observed_at();
                item.started_at = item.started_at.or(item.at).or(at);
                item.at = at;
                let metadata = postcard::to_stdvec(&item).map_err(|error| error.to_string())?;
                let transaction = self.db.transaction().map_err(|error| error.to_string())?;
                insert_body(&transaction, &id, field, offset as i64, &delta)?;
                let update = if field == 0 {
                    "UPDATE items SET len0=len0+?2,metadata=?3 WHERE id=?1"
                } else {
                    "UPDATE items SET len1=len1+?2,metadata=?3 WHERE id=?1"
                };
                transaction
                    .execute(update, params![id, delta.len(), metadata])
                    .map_err(|error| error.to_string())?;
                transaction.commit().map_err(|error| error.to_string())?;
                self.changed(&id);
                self.dirty = true;
            }
            Event::Item(item) => {
                if let Some(warning) = self.put(item, false)? {
                    out.push(nonfatal(warning));
                }
            }
            Event::History {
                mut items,
                has_earlier,
            }
            | Event::LatestHistory {
                mut items,
                has_earlier,
            } => {
                if !earlier {
                    // A latest reread cannot satisfy an outstanding older
                    // cursor, including a deferred reread after RPC failure.
                    self.pending_earlier = None;
                }
                self.provider_earlier = has_earlier;
                let mut seen = std::collections::BTreeSet::new();
                items.retain(|item| seen.insert(item.id.clone()));
                let warnings = self.merge_history(items, earlier)?;
                if !earlier {
                    self.latest_boundary = None;
                }
                if let Some(warning) = warnings.first() {
                    out.push(nonfatal(if warnings.len() > 1 {
                        format!("{warning} ({} replayed items affected)", warnings.len())
                    } else {
                        warning.clone()
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
                out.push(Event::HistoryRecovered);
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
                self.latest_boundary = None;
                self.dirty = true;
                out.push(Event::Reset { reason });
                return Ok(out);
            }
            Event::HistoryLoading(false) => {
                // A failed provider request must not leave its cursor waiting
                // for an unrelated later history response. Successful pages
                // consume the cursor before publishing loading=false.
                self.pending_earlier = None;
                self.latest_boundary = None;
                out.push(Event::HistoryLoading(false));
                return Ok(out);
            }
            Event::LatestHistoryStarted => {
                let latest: Option<i64> = self
                    .db
                    .query_row("SELECT MAX(ordinal) FROM items", [], |row| row.get(0))
                    .map_err(|e| e.to_string())?;
                self.latest_boundary =
                    Some(latest.map_or(LatestBoundary::BeforeAll, LatestBoundary::After));
                out.push(Event::LatestHistoryStarted);
                return Ok(out);
            }
            event @ (Event::Exited { .. } | Event::Error { fatal: true, .. }) => {
                self.latest_boundary = None;
                self.pending_earlier = None;
                out.push(event);
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
                    started_at: None,
                    at: None,
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
        let mut item: Item = postcard::from_bytes(&metadata).map_err(|e| e.to_string())?;
        if !matches!(item.content, ItemContent::Text { .. }) {
            return Err(format!(
                "Provider reused non-text item ID {id} for a text delta"
            ));
        }
        if !delta.is_empty() {
            // A streaming item is restamped by every delta, so the stamp a
            // finished item carries is the moment its last delta arrived.
            let at = observed_at();
            item.started_at = item.started_at.or(item.at).or(at);
            item.at = at;
            let stamped = postcard::to_stdvec(&item).map_err(|e| e.to_string())?;
            let transaction = self.db.transaction().map_err(|e| e.to_string())?;
            insert_body(&transaction, id, 0, length, delta)?;
            transaction
                .execute(
                    "UPDATE items SET len0=len0+?2,metadata=?3 WHERE id=?1",
                    params![id, delta.len() as i64, stamped],
                )
                .map_err(|e| e.to_string())?;
            transaction.commit().map_err(|e| e.to_string())?;
            self.changed(id);
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
    fn changed(&mut self, id: &str) {
        if let Some(changed) = &mut self.changed {
            changed.insert(id.into());
            if changed.len() > WINDOW_ITEMS * 2 {
                self.changed = None;
            }
        }
    }
    #[cfg(test)]
    pub(super) fn window(&mut self) -> Result<Window, String> {
        self.window_reusing(&mut Vec::new())
    }
    pub(super) fn window_reusing(
        &mut self,
        previous: &mut Vec<std::sync::Arc<Item>>,
    ) -> Result<Window, String> {
        let _ = self.flush_text()?;
        let old: std::collections::HashMap<_, _> = previous
            .iter()
            .map(|item| (item.id.as_str(), item))
            .collect();
        let mut reused = std::collections::HashSet::new();
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
            let kind = layout(&base.content);
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
                let (start0, end0) = (start.min(len0), end.min(len0));
                let (start1, end1) = (start.saturating_sub(len0), end.saturating_sub(len0));
                let mut item = base.clone();
                if part > 0 {
                    // A part index names a position in a body rather than the bytes at
                    // that position, and the slice moves while an item is live: a
                    // command's output grows, and a finished tool call is rewritten into
                    // a command, which drops the input from the body and re-slices what
                    // is left. Since the reader's disclosure state is keyed by item id,
                    // an index alone would hand a part the state its reader gave to
                    // different bytes, so the id also carries the range each field is
                    // read at and which variant decided that field's meaning. Neither
                    // moves while a body only grows, so a part the reader expanded
                    // stays expanded across frames.
                    item.id =
                        format!("{id}:native-part:{part}:{kind}:{start0}-{end0}:{start1}-{end1}");
                }
                let attached =
                    if len2 > 0 && metadata.len() + end - start + len2 + 128 <= WINDOW_BYTES {
                        len2
                    } else {
                        0
                    };
                let size = metadata.len() + end - start + 128 + attached;
                if !items.is_empty() && (bytes + size > WINDOW_BYTES || items.len() == WINDOW_ITEMS)
                {
                    break 'records;
                }
                if self
                    .changed
                    .as_ref()
                    .is_some_and(|changed| !changed.contains(&id))
                    && old.contains_key(item.id.as_str())
                {
                    reused.insert(item.id.clone());
                    newest.get_or_insert((ordinal, part));
                    first = Some((ordinal, part));
                    bytes += size;
                    items.push(std::sync::Arc::new(item));
                    continue;
                }
                let body0 = String::from_utf8(self.field(&id, 0, start0, end0)?)
                    .map_err(|e| e.to_string())?;
                let body1 = String::from_utf8(self.field(&id, 1, start1, end1)?)
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
                    ItemContent::Delivery { body, .. } => *body = body0,
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
                items.push(std::sync::Arc::new(item));
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
        let changes = items
            .iter()
            .filter(|item| !reused.contains(&item.id))
            .map(|item| {
                crate::chat::presentation::ItemChange::between(
                    old.get(item.id.as_str()).map(|item| item.as_ref()),
                    item,
                )
            })
            .collect();
        drop(old);
        let mut old: std::collections::HashMap<_, _> = std::mem::take(previous)
            .into_iter()
            .map(|item| (item.id.clone(), item))
            .collect();
        for item in &mut items {
            if reused.contains(&item.id) {
                *item = old.remove(&item.id).expect("reused resident item");
            }
        }
        self.dirty = false;
        self.changed = Some(Default::default());
        Ok(Window {
            items,
            changes,
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
        ItemContent::Delivery {
            from,
            sender,
            summary,
            ..
        } => {
            clamp(from, NAME_BYTES);
            clamp(sender, NAME_BYTES);
            clamp(summary, NAME_BYTES);
        }
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
/// Names how a variant's payload is spread across the chunk fields, for the
/// part IDs of a paged item. Which field a byte offset belongs to is decided by
/// the variant, so a rewritten item can hold entirely different bytes at the
/// same offset of the same field: a tool call keeps its input in field 0 and a
/// command keeps its output there.
fn layout(content: &ItemContent) -> &'static str {
    match content {
        ItemContent::Text { .. } => "text",
        ItemContent::Command { .. } => "command",
        ItemContent::Changes { .. } => "changes",
        ItemContent::WebSearch { .. } => "search",
        ItemContent::Tool { .. } => "tool",
        ItemContent::Image { .. } => "image",
        ItemContent::Delivery { .. } => "delivery",
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
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn output_for_an_uncacheable_identity_remains_nonfatal() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        let id = "x".repeat(METADATA_LIMIT + 1);
        let item = Item {
            id: id.clone(),
            role: Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Tool {
                name: "tool".into(),
                input: String::new(),
                output: "begin".into(),
                status: crate::chat::ToolStatus::Running,
            },
        };
        assert!(!store.apply(Event::Item(item)).unwrap().is_empty());
        assert!(
            !store
                .apply(Event::OutputDelta {
                    id,
                    offset: 5,
                    delta: " tail".into()
                })
                .unwrap()
                .is_empty()
        );
        store
            .apply(Event::Item(history_item("later", "provider still alive")))
            .unwrap();
        assert_eq!(store.window().unwrap().items[0].id, "later");
    }

    #[test]
    fn output_appends_preserve_existing_chunks_and_final_rewrites() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        let item = Item {
            id: "tool".into(),
            role: Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Tool {
                name: "shell".into(),
                input: "args".into(),
                output: "α".into(),
                status: crate::chat::ToolStatus::Running,
            },
        };
        store.apply(Event::Item(item.clone())).unwrap();
        for offset in [2, 4, 6] {
            store
                .apply(Event::OutputDelta {
                    id: "tool".into(),
                    offset,
                    delta: "β".into(),
                })
                .unwrap();
        }
        assert!(
            store
                .apply(Event::OutputDelta {
                    id: "tool".into(),
                    offset: 1,
                    delta: "bad".into()
                })
                .is_err()
        );
        let chunks: usize = store
            .db
            .query_row(
                "SELECT COUNT(*) FROM chunks WHERE id='tool' AND field=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(chunks, 4, "the original chunk survives each append");
        let window = store.window().unwrap();
        assert!(
            matches!(&window.items[0].content, ItemContent::Tool { input, output, .. } if input == "args" && output == "αβββ")
        );
        let mut rewritten = item;
        if let ItemContent::Tool { output, status, .. } = &mut rewritten.content {
            *output = "final replacement".into();
            *status = crate::chat::ToolStatus::Succeeded;
        }
        store.apply(Event::Item(rewritten)).unwrap();
        assert!(
            matches!(&store.window().unwrap().items[0].content, ItemContent::Tool { output, .. } if output == "final replacement")
        );
    }

    #[test]
    fn resident_window_reuses_unchanged_bodies_and_pages_resynchronize() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        store
            .apply(Event::Item(history_item(
                "fixed",
                &"x".repeat(PART_BYTES / 2),
            )))
            .unwrap();
        store
            .apply(Event::Item(history_item("live", "begin")))
            .unwrap();
        let mut previous = store.window().unwrap().items;
        let fixed = previous[0].clone();
        store
            .apply(Event::TextDelta {
                id: "live".into(),
                role: Role::Assistant,
                thinking: false,
                delta: " tail".into(),
            })
            .unwrap();
        let window = store.window_reusing(&mut previous).unwrap();
        assert!(std::sync::Arc::ptr_eq(&fixed, &window.items[0]));
        assert_eq!(window.changes.len(), 1);
        assert!(
            matches!(&window.changes[0], crate::chat::presentation::ItemChange::Append { offset: 5, suffix, .. } if suffix == " tail")
        );
        previous = window.items;
        store
            .apply(Event::Item(history_item(
                "huge",
                &"λ".repeat(PART_BYTES * 16),
            )))
            .unwrap();
        let mut latest = store.window_reusing(&mut previous).unwrap();
        assert!(
            latest
                .items
                .iter()
                .any(|item| item.id.contains(":native-part:"))
        );
        let baseline = crate::chat::Snapshot {
            generation: 1,
            items: latest.items.clone(),
            ..Default::default()
        };
        store.earlier().unwrap();
        let earlier = store.window_reusing(&mut latest.items).unwrap();
        let mut replayed = baseline;
        crate::chat::presentation::Presentation {
            children: Vec::new(),
            base_generation: 1,
            metadata: crate::chat::Snapshot {
                generation: 2,
                ..Default::default()
            },
            order: earlier.items.iter().map(|item| item.id.clone()).collect(),
            changes: earlier.changes,
        }
        .apply(&mut replayed)
        .unwrap();
        assert_eq!(replayed.items, earlier.items);
    }
    fn history_item(id: &str, body: &str) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: Phase::Final,
            },
            started_at: None,
            at: None,
        }
    }
    fn ids(store: &mut Store) -> Vec<String> {
        store
            .window()
            .unwrap()
            .items
            .into_iter()
            .map(|item| item.id.clone())
            .collect()
    }

    #[test]
    fn overlapping_history_inserts_prefix_middle_and_suffix_without_replacing_live_output() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        for id in ["A", "C"] {
            store.apply(Event::Item(history_item(id, "live"))).unwrap();
        }
        store
            .apply(Event::History {
                items: ["prefix", "A", "B", "C", "D"]
                    .map(|id| history_item(id, "replay"))
                    .to_vec(),
                has_earlier: false,
            })
            .unwrap();
        assert_eq!(ids(&mut store), ["prefix", "A", "B", "C", "D"]);
        store
            .apply(Event::TextDelta {
                id: "C".into(),
                role: Role::Assistant,
                thinking: false,
                delta: " update".into(),
            })
            .unwrap();
        store
            .apply(Event::LatestHistory {
                items: ["B", "C", "D", "E"]
                    .map(|id| history_item(id, "stale"))
                    .to_vec(),
                has_earlier: false,
            })
            .unwrap();
        assert_eq!(ids(&mut store), ["prefix", "A", "B", "C", "D", "E"]);
        assert!(
            matches!(&store.window().unwrap().items[3].content, ItemContent::Text { text, .. } if text == "live update")
        );
    }

    #[test]
    fn disjoint_latest_appends_and_disjoint_earlier_prepends() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        for id in ["A", "B"] {
            store.apply(Event::Item(history_item(id, id))).unwrap();
        }
        store
            .apply(Event::LatestHistory {
                items: ["D", "E"].map(|id| history_item(id, id)).to_vec(),
                has_earlier: true,
            })
            .unwrap();
        store
            .apply(Event::History {
                items: ["Y", "Z"].map(|id| history_item(id, id)).to_vec(),
                has_earlier: false,
            })
            .unwrap();
        assert_eq!(ids(&mut store), ["Y", "Z", "A", "B", "D", "E"]);
    }

    #[test]
    fn latest_history_does_not_satisfy_an_older_cursor_left_after_failure() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        store
            .apply(Event::History {
                items: vec![history_item("A", "A")],
                has_earlier: true,
            })
            .unwrap();
        store.window().unwrap();
        assert!(!store.earlier().unwrap());
        assert!(store.pending_earlier.is_some());
        store
            .apply(Event::LatestHistory {
                items: vec![history_item("B", "B")],
                has_earlier: false,
            })
            .unwrap();
        assert!(store.pending_earlier.is_none());
        assert!(!store.window().unwrap().has_later);
        assert_eq!(ids(&mut store), ["A", "B"]);
    }

    #[test]
    fn latest_history_response_preserves_newer_live_items_after_its_request_boundary() {
        for with_cached_items in [false, true] {
            let mut store = Store::at(std::env::temp_dir()).unwrap();
            if with_cached_items {
                store.apply(Event::Item(history_item("A", "A"))).unwrap();
                // Capture must include buffered text, not just flushed items.
                store
                    .apply(Event::TextDelta {
                        id: "B".into(),
                        role: Role::Assistant,
                        thinking: false,
                        delta: "B".into(),
                    })
                    .unwrap();
            }
            store.apply(Event::LatestHistoryStarted).unwrap();
            store.apply(Event::Item(history_item("F", "live"))).unwrap();
            store
                .apply(Event::LatestHistory {
                    items: ["D", "E"].map(|id| history_item(id, id)).to_vec(),
                    has_earlier: false,
                })
                .unwrap();
            assert_eq!(
                ids(&mut store),
                if with_cached_items {
                    vec!["A", "B", "D", "E", "F"]
                } else {
                    vec!["D", "E", "F"]
                }
            );
            assert!(store.latest_boundary.is_none());
        }
    }

    #[test]
    fn latest_history_boundary_survives_rebalance_and_keeps_the_held_page() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        for id in ["A", "B"] {
            store.apply(Event::Item(history_item(id, id))).unwrap();
        }
        store
            .db
            .execute(
                "UPDATE items SET ordinal=CASE id WHEN 'A' THEN 1 ELSE 2 END",
                [],
            )
            .unwrap();
        store.end = Some((2, 0));
        store.apply(Event::LatestHistoryStarted).unwrap();
        store.apply(Event::Item(history_item("F", "live"))).unwrap();
        store
            .db
            .execute("UPDATE items SET ordinal=3 WHERE id='F'", [])
            .unwrap();
        store
            .apply(Event::LatestHistory {
                items: ["D", "E"].map(|id| history_item(id, id)).to_vec(),
                has_earlier: false,
            })
            .unwrap();
        assert_eq!(store.end, Some((2 * ORDINAL_STRIDE, 0)));
        assert_eq!(ids(&mut store), ["A"]);
        store.latest();
        assert_eq!(ids(&mut store), ["A", "B", "D", "E", "F"]);
    }

    #[test]
    fn latest_history_boundary_uses_the_predecessor_if_its_item_is_removed() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        for id in ["A", "B"] {
            store.apply(Event::Item(history_item(id, id))).unwrap();
        }
        store.apply(Event::LatestHistoryStarted).unwrap();
        store.apply(Event::Item(history_item("F", "live"))).unwrap();
        store
            .apply(Event::ItemsRemoved {
                ids: vec!["B".into()],
            })
            .unwrap();
        store
            .apply(Event::LatestHistory {
                items: vec![history_item("D", "D")],
                has_earlier: false,
            })
            .unwrap();
        assert_eq!(ids(&mut store), ["A", "D", "F"]);
    }

    #[test]
    fn latest_history_boundary_clears_on_failure_reset_and_exit() {
        for event in [
            Event::HistoryLoading(false),
            Event::Reset {
                reason: "reset".into(),
            },
            Event::Exited { code: None },
            Event::Error {
                message: "failed".into(),
                fatal: true,
            },
        ] {
            let mut store = Store::at(std::env::temp_dir()).unwrap();
            store.apply(Event::LatestHistoryStarted).unwrap();
            assert!(store.latest_boundary.is_some());
            store.apply(event).unwrap();
            assert!(store.latest_boundary.is_none());
        }
    }

    #[test]
    fn exhausted_ordinal_gap_rebalances_without_moving_chunk_cursors() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        store
            .apply(Event::Item(history_item("A", &"x".repeat(3 * PART_BYTES))))
            .unwrap();
        store.apply(Event::Item(history_item("C", "live"))).unwrap();
        store
            .db
            .execute(
                "UPDATE items SET ordinal=CASE id WHEN 'A' THEN 1 ELSE 2 END",
                [],
            )
            .unwrap();
        store.end = Some((1, 2));
        store.first = Some((1, 0));
        store.pending_earlier = Some((2, 0));
        store
            .merge_history(
                vec![
                    history_item("A", "stale"),
                    history_item("B", "middle"),
                    history_item("C", "stale"),
                ],
                true,
            )
            .unwrap();
        assert_eq!(store.end, Some((ORDINAL_STRIDE, 2)));
        assert_eq!(store.first, Some((ORDINAL_STRIDE, 0)));
        assert_eq!(store.pending_earlier, Some((2 * ORDINAL_STRIDE, 0)));
        let page = store.window().unwrap();
        assert_eq!(page.items.len(), 2);
        assert!(page.has_later);
        assert_eq!(
            page.items
                .iter()
                .map(|item| item.content.byte_len())
                .sum::<usize>(),
            2 * PART_BYTES
        );
        store.latest();
        let page = store.window().unwrap();
        assert_eq!(page.items.last().unwrap().id, "C");
        assert_eq!(page.items[3].id, "B");
    }

    #[test]
    fn latest_or_failed_request_prevents_a_late_history_response_from_repinning() {
        for cancel_with_latest in [true, false] {
            let mut store = Store::at(std::env::temp_dir()).unwrap();
            let text = |id: &str| Item {
                id: id.into(),
                role: Role::Assistant,
                content: ItemContent::Text {
                    text: id.into(),
                    thinking: false,
                    phase: Phase::Final,
                },
                started_at: None,
                at: None,
            };
            store
                .apply(Event::History {
                    items: vec![text("current")],
                    has_earlier: true,
                })
                .unwrap();
            assert!(store.window().unwrap().has_earlier);
            assert!(!store.earlier().unwrap());
            assert!(store.pending_earlier.is_some());
            if cancel_with_latest {
                store.latest();
            } else {
                store.apply(Event::HistoryLoading(false)).unwrap();
            }
            assert!(store.pending_earlier.is_none());
            store
                .apply(Event::History {
                    items: vec![text("older")],
                    has_earlier: false,
                })
                .unwrap();
            let window = store.window().unwrap();
            assert!(!window.has_later);
            assert_eq!(window.items.last().unwrap().id, "current");
        }
    }

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
                started_at: None,
                at: None,
            }))
            .unwrap();
        let mut pages = Vec::new();
        loop {
            let page = store.window().unwrap();
            let value = page
                .items
                .into_iter()
                .map(|item| match item.content.clone() {
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
    #[test]
    fn delivery_round_trips_with_its_body_paged_like_prose() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        let delivery = ItemContent::Delivery {
            kind: crate::chat::DeliveryKind::Report,
            from: "general-purpose".into(),
            sender: "a17925ac2cee519e2".into(),
            summary: String::new(),
            body: "## Findings\n".repeat(16),
        };
        store
            .apply(Event::Item(Item {
                id: "report".into(),
                role: Role::User,
                content: delivery.clone(),
                started_at: None,
                at: None,
            }))
            .unwrap();
        let window = store.window().unwrap();
        assert_eq!(window.items.len(), 1);
        assert_eq!(window.items[0].content, delivery);
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
                started_at: None,
                at: None,
            }))
            .unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        let window = store.window().unwrap();
        assert_eq!(window.items.len(), 1);
        match &window.items[0].content {
            ItemContent::Image { path, description } => {
                assert!(path.starts_with(store.directory.to_str().unwrap()));
                assert_eq!(fs::read(path).unwrap(), b"ABC".repeat(512 * 1024));
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
                started_at: None,
                at: None,
            }))
            .unwrap();
        assert_eq!(store.window().unwrap().items.len(), 2);
    }
    #[test]
    fn image_payloads_share_one_file_and_invalid_payloads_do_not_end_history() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        for id in ["a", "b", "invalid"] {
            let warnings = store
                .apply(Event::Item(Item {
                    id: id.into(),
                    role: Role::Assistant,
                    started_at: None,
                    at: None,
                    content: ItemContent::Image {
                        path: if id == "invalid" {
                            "data:image/png;base64,%%%".into()
                        } else {
                            "data:image/png;base64,QUJD".into()
                        },
                        description: id.into(),
                    },
                }))
                .unwrap();
            assert_eq!(warnings.is_empty(), id != "invalid");
        }
        let window = store.window().unwrap();
        assert_eq!(window.items.len(), 3);
        let path = |index: usize| match &window.items[index].content {
            ItemContent::Image { path, .. } => path,
            _ => unreachable!(),
        };
        assert_eq!(path(0), path(1));
        assert!(path(2).is_empty());
        let image_path = PathBuf::from(path(0));
        assert_eq!(
            fs::metadata(&image_path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(postcard::to_stdvec(&window.items).unwrap().len() < 1024);
        drop(store);
        assert!(!image_path.exists());
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
                started_at: None,
                at: None,
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
                started_at: None,
                at: None,
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
                started_at: None,
                at: None,
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
    /// A part id is the key the view's disclosure state is held under, and a
    /// provider rewrites a live item: a finished tool call arrives again as a
    /// command, which drops the input from the body and re-slices what is left.
    /// A part that kept its old id would open on its first paint because the
    /// reader had expanded the different bytes that id used to name.
    #[test]
    fn re_sliced_parts_do_not_reuse_the_ids_of_different_bytes() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        let output = "shell output line\n".repeat(19_445);
        let input = "{\"command\":\"ls\"}".repeat(18_750);
        // Both bodies span more than two parts, so a part lands wholly inside
        // the call's input and the byte range it names outlives the rewrite
        // that moves the output to where that input used to be.
        assert!(output.len() > 2 * PART_BYTES && input.len() > 2 * PART_BYTES);
        let parts = |window: Window| {
            window
                .items
                .into_iter()
                .map(|item| item.id.clone())
                .filter(|id| id.contains(":native-part:"))
                .collect::<Vec<_>>()
        };
        store
            .apply(Event::Item(Item {
                id: "call".into(),
                role: Role::Assistant,
                content: ItemContent::Tool {
                    name: "shell".into(),
                    input,
                    output: output.clone(),
                    status: crate::chat::ToolStatus::Running,
                },
                started_at: None,
                at: None,
            }))
            .unwrap();
        let before = parts(store.window().unwrap());
        assert!(before.len() > 1, "{before:?}");
        // Nothing changed between the two windows, so every part keeps its id
        // and a reader who expands one and scrolls does not lose it.
        assert_eq!(parts(store.window().unwrap()), before);
        store
            .apply(Event::Item(Item {
                id: "call".into(),
                role: Role::Assistant,
                content: ItemContent::Command {
                    label: "shell".into(),
                    command: "ls".into(),
                    working_directory: "/repo".into(),
                    action: crate::chat::CommandAction::Run,
                    status: crate::chat::ToolStatus::Succeeded,
                    exit_code: Some(0),
                    duration_ms: Some(3),
                    output,
                },
                started_at: None,
                at: None,
            }))
            .unwrap();
        let after = parts(store.window().unwrap());
        assert!(after.len() > 1, "{after:?}");
        for id in &after {
            assert!(
                !before.contains(id),
                "part id {id} was reused for different bytes"
            );
        }
    }
    #[test]
    fn replayed_items_acquire_a_stable_start_on_the_first_live_delta() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        let text = Item {
            id: "text".into(),
            role: Role::Assistant,
            content: ItemContent::Text {
                text: String::new(),
                thinking: true,
                phase: Phase::Final,
            },
            started_at: None,
            at: None,
        };
        let tool = Item {
            id: "tool".into(),
            role: Role::Assistant,
            content: ItemContent::Tool {
                name: "exec".into(),
                input: "run".into(),
                output: String::new(),
                status: crate::chat::ToolStatus::Running,
            },
            started_at: None,
            at: None,
        };
        store
            .apply(Event::History {
                items: vec![text, tool],
                has_earlier: false,
            })
            .unwrap();
        assert!(
            store
                .window()
                .unwrap()
                .items
                .iter()
                .all(|item| item.started_at.is_none())
        );
        let mut initial_starts = Vec::new();
        for offset in 0..2 {
            store
                .apply(Event::TextDelta {
                    id: "text".into(),
                    role: Role::Assistant,
                    thinking: true,
                    delta: "x".into(),
                })
                .unwrap();
            store
                .apply(Event::OutputDelta {
                    id: "tool".into(),
                    offset,
                    delta: "x".into(),
                })
                .unwrap();
            let first = store.window().unwrap().items;
            let starts: Vec<_> = first.iter().map(|item| item.started_at.unwrap()).collect();
            if offset == 0 {
                initial_starts = starts.clone();
            } else {
                assert_eq!(starts, initial_starts);
            }
            // A same-ID replay must not replace the timing acquired by live work.
            store
                .apply(Event::History {
                    items: first
                        .iter()
                        .map(|item| {
                            let mut replay = (**item).clone();
                            replay.started_at = None;
                            replay.at = None;
                            replay
                        })
                        .collect(),
                    has_earlier: false,
                })
                .unwrap();
            let replayed = store.window().unwrap().items;
            assert_eq!(
                replayed
                    .iter()
                    .map(|item| item.started_at.unwrap())
                    .collect::<Vec<_>>(),
                starts
            );
            if offset == 0 {
                // Set last observations apart from the starts deterministically;
                // the next delta must preserve the first start, not this later stamp.
                for item in first {
                    let mut updated = (*item).clone();
                    updated.at = Some(updated.started_at.unwrap() + 1_000);
                    let metadata = postcard::to_stdvec(&updated).unwrap();
                    store
                        .db
                        .execute(
                            "UPDATE items SET metadata=?2 WHERE id=?1",
                            params![updated.id, metadata],
                        )
                        .unwrap();
                }
            }
        }
        let items = store.window().unwrap().items;
        for item in items {
            assert!(item.started_at.is_some());
        }
    }

    #[test]
    fn tool_start_survives_output_deltas_and_completion_replacement() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        let tool = |status| Item {
            id: "tool".into(),
            role: Role::Assistant,
            content: ItemContent::Tool {
                name: "exec".into(),
                input: "run".into(),
                output: String::new(),
                status,
            },
            started_at: None,
            at: None,
        };
        store
            .apply(Event::Item(tool(crate::chat::ToolStatus::Running)))
            .unwrap();
        let started = store.window().unwrap().items[0].started_at.unwrap();
        store
            .apply(Event::OutputDelta {
                id: "tool".into(),
                offset: 0,
                delta: "working".into(),
            })
            .unwrap();
        let streaming = store.window().unwrap().items[0].clone();
        assert_eq!(streaming.started_at, Some(started));
        assert!(streaming.at.unwrap() >= started);
        store
            .apply(Event::Item(tool(crate::chat::ToolStatus::Succeeded)))
            .unwrap();
        let completed = store.window().unwrap().items[0].clone();
        assert_eq!(completed.started_at, Some(started));
        assert!(completed.at.unwrap() >= streaming.at.unwrap());
    }

    /// The stamp is read as the moment an item finished, and a running daemon
    /// reaches its transcript through this cache, so the stamp has to survive
    /// caching, streaming and paging back out.
    #[test]
    fn cached_items_are_stamped_and_replayed_history_is_not() {
        let mut store = Store::at(std::env::temp_dir()).unwrap();
        let text = |id: &str, text: &str| Item {
            id: id.into(),
            role: Role::Assistant,
            content: ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: Phase::Final,
            },
            started_at: None,
            at: None,
        };
        store.apply(Event::Item(text("live", "first"))).unwrap();
        let created = store.window().unwrap().items[0]
            .at
            .expect("a cached observation carries a stamp");
        assert_eq!(store.window().unwrap().items[0].started_at, Some(created));
        store
            .apply(Event::TextDelta {
                id: "live".into(),
                role: Role::Assistant,
                thinking: false,
                delta: " more".into(),
            })
            .unwrap();
        let streamed = store.window().unwrap().items[0]
            .at
            .expect("accumulating a delta keeps the item stamped");
        assert!(streamed >= created);
        assert_eq!(store.window().unwrap().items[0].started_at, Some(created));
        store
            .apply(Event::History {
                items: vec![text("older", "replay")],
                has_earlier: false,
            })
            .unwrap();
        let items = store.window().unwrap().items;
        assert_eq!(items[0].at, None);
        assert_eq!(items[0].started_at, None);
        assert_eq!(items[1].at, Some(streamed));
        assert_eq!(items[1].started_at, Some(created));
    }
}
