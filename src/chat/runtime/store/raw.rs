//! A versioned original-call namespace inside the leased transcript database.
//! Original arguments and derived patches are distinct sources. Open stream
//! inputs borrow canonical chunks; sealing copies those chunks without building
//! a full prefix in RAM before the derived item replaces them.
use super::*;
use crate::chat::cache::raw::*;
use crate::chat::cache::{PAGE_BYTES, SourceEpoch, SourceError, SourceKey};
use rusqlite::{Transaction, TransactionBehavior};
use std::collections::HashMap;

const VERSION: i64 = 1;
const DISK_BYTES: usize = 256 * 1024 * 1024;
const KEYS: usize = 8192;
const CHUNKS: usize = 65536;
const KEY_BYTES: usize = 4096;
const OPEN_KEYS: usize = 512;
const OPEN: i64 = 0;
const SEALED: i64 = 1;

const SCHEMA:&str="CREATE TABLE raw_source_schema(singleton INTEGER PRIMARY KEY CHECK(singleton=1),version INTEGER NOT NULL,nonce TEXT NOT NULL,next_revision INTEGER NOT NULL,bytes INTEGER NOT NULL,keys INTEGER NOT NULL,chunks INTEGER NOT NULL,disabled TEXT);
CREATE TABLE raw_tool_sources(agent TEXT NOT NULL,id TEXT NOT NULL,name TEXT NOT NULL,representation INTEGER NOT NULL,input_len INTEGER NOT NULL,command_len INTEGER NOT NULL,state INTEGER NOT NULL,hash BLOB,epoch INTEGER NOT NULL,cost INTEGER NOT NULL,PRIMARY KEY(agent,id));
CREATE TABLE raw_tool_chunks(agent TEXT NOT NULL,id TEXT NOT NULL,field INTEGER NOT NULL,position INTEGER NOT NULL,data BLOB NOT NULL,PRIMARY KEY(agent,id,field,position),FOREIGN KEY(agent,id) REFERENCES raw_tool_sources(agent,id) ON DELETE CASCADE);
CREATE TABLE raw_tool_revoked(agent TEXT NOT NULL,id TEXT NOT NULL,PRIMARY KEY(agent,id));";

pub(super) struct State {
    pub(super) nonce: uuid::Uuid,
    open: HashMap<SourceKey, usize>,
    open_bytes: usize,
    blocked: Option<String>,
}
/// Version zero means no namespace. An immediate reservation covers discovery,
/// schema creation and publication. Unknown/partial/future schemas never become
/// an implicit upgrade through CREATE IF NOT EXISTS.
pub(super) fn initialize(db: &mut Connection) -> Result<State, String> {
    let tx = db
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .map_err(err)?;
    let exists:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='raw_source_schema')",[],|row|row.get(0)).map_err(err)?;
    let nonce = uuid::Uuid::new_v4();
    if !exists {
        tx.execute_batch(SCHEMA).map_err(err)?;
        tx.execute(
            "INSERT INTO raw_source_schema VALUES(1,?1,?2,1,0,0,0,NULL)",
            params![VERSION, nonce.to_string()],
        )
        .map_err(err)?;
    } else {
        let version: i64 = tx
            .query_row(
                "SELECT version FROM raw_source_schema WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .map_err(err)?;
        if version != VERSION {
            return Err(
                "Original tool source schema is unsupported; no source data was changed".into(),
            );
        }
        // Reopening the database fences an older handle before it can write
        // through a stale schema/revocation view.
        tx.execute(
            "UPDATE raw_source_schema SET nonce=?1 WHERE singleton=1",
            [nonce.to_string()],
        )
        .map_err(err)?;
    }
    let blocked = tx
        .query_row(
            "SELECT disabled FROM raw_source_schema WHERE singleton=1",
            [],
            |row| row.get::<_, Option<String>>(0),
        )
        .map_err(err)?;
    let mut open = HashMap::new();
    let mut open_bytes = 0usize;
    {
        let mut query = tx
            .prepare("SELECT agent,id,input_len FROM raw_tool_sources s WHERE state=0 AND NOT EXISTS(SELECT 1 FROM raw_tool_revoked r WHERE r.agent=s.agent AND r.id=s.id) LIMIT 513")
            .map_err(err)?;
        let rows = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, usize>(2)?,
                ))
            })
            .map_err(err)?;
        for row in rows {
            let (agent, id, len) = row.map_err(err)?;
            if open.len() == OPEN_KEYS {
                return Err("Original tool source has too many unfinished captures".into());
            }
            open_bytes = open_bytes.saturating_add(len);
            open.insert(key(&agent, &id), len);
        }
    }
    tx.commit().map_err(err)?;
    Ok(State {
        nonce,
        open,
        open_bytes,
        blocked,
    })
}
fn err(error: impl ToString) -> String {
    error.to_string()
}
fn key(agent: &str, id: &str) -> SourceKey {
    SourceKey {
        agent_id: (!agent.is_empty()).then(|| agent.into()),
        source_id: id.into(),
    }
}
fn agent(key: &SourceKey) -> &str {
    key.agent_id.as_deref().unwrap_or("")
}
fn valid_key(key: &SourceKey) -> bool {
    !key.source_id.is_empty()
        && key.source_id.len() <= KEY_BYTES
        && key
            .agent_id
            .as_ref()
            .is_none_or(|id| !id.is_empty() && id.len() <= KEY_BYTES)
}
pub(super) fn guard(db: &Connection, nonce: uuid::Uuid) -> Result<(), String> {
    let (version, current): (i64, String) = db
        .query_row(
            "SELECT version,nonce FROM raw_source_schema WHERE singleton=1",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(err)?;
    if version != VERSION || current != nonce.to_string() {
        return Err("Original tool source schema or owner changed; this handle was revoked".into());
    }
    Ok(())
}
fn revision(tx: &Transaction<'_>) -> Result<i64, String> {
    let value: i64 = tx
        .query_row(
            "SELECT next_revision FROM raw_source_schema WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .map_err(err)?;
    let next = value
        .checked_add(1)
        .ok_or("Original tool source revision capacity exhausted")?;
    tx.execute(
        "UPDATE raw_source_schema SET next_revision=?1 WHERE singleton=1",
        [next],
    )
    .map_err(err)?;
    Ok(value)
}
fn digest(bytes: &[u8]) -> [u8; 32] {
    Sha256::digest(bytes).into()
}
/// Full captures contain the provider's serialized native JSON value. Compare
/// that representation without allocating another complete serialized body.
fn same_native_json(stream: &str, full: &str) -> bool {
    struct Compare<'a>(&'a [u8]);
    impl std::io::Write for Compare<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self.0.strip_prefix(bytes).ok_or_else(|| {
                std::io::Error::new(std::io::ErrorKind::InvalidData, "Different native JSON")
            })?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stream) else {
        return false;
    };
    let mut compare = Compare(full.as_bytes());
    serde_json::to_writer(&mut compare, &value).is_ok() && compare.0.is_empty()
}
fn insert(
    tx: &Transaction<'_>,
    source: &SourceKey,
    field: i64,
    bytes: &[u8],
) -> Result<usize, String> {
    let mut statement = tx
        .prepare_cached("INSERT INTO raw_tool_chunks VALUES(?1,?2,?3,?4,?5)")
        .map_err(err)?;
    for (index, chunk) in bytes.chunks(PART_BYTES).enumerate() {
        statement
            .execute(params![
                agent(source),
                source.source_id,
                field,
                index * PART_BYTES,
                chunk
            ])
            .map_err(err)?;
    }
    Ok(bytes.len().div_ceil(PART_BYTES))
}
// Charge both table and primary-key copies, plus the future tombstone reserved
// while an original body is retained after an identity ambiguity. This is a
// logical namespace budget; SQLite page/WAL overhead belongs to the same Store.
fn key_bytes(source: &SourceKey) -> usize {
    agent(source).len().saturating_add(source.source_id.len())
}
fn source_cost(source: &SourceKey, name: &str) -> usize {
    name.len()
        .saturating_add(key_bytes(source).saturating_mul(4))
        .saturating_add(384)
}
fn chunk_cost(source: &SourceKey) -> usize {
    key_bytes(source).saturating_mul(2).saturating_add(128)
}
fn tombstone_cost(source: &SourceKey) -> usize {
    key_bytes(source).saturating_mul(2).saturating_add(192)
}
fn command_bytes(command: Option<&RawCommandCall>) -> Result<Vec<u8>, String> {
    command
        .map(serde_json::to_vec)
        .transpose()
        .map_err(err)
        .map(|value| value.unwrap_or_default())
}
fn source_error(error: impl ToString) -> SourceError {
    SourceError::Transport(error.to_string())
}

pub(super) fn revoked(db: &Connection, source: &SourceKey) -> Result<bool, String> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM raw_tool_revoked WHERE agent=?1 AND id=?2)",
        params![agent(source), source.source_id],
        |row| row.get(0),
    )
    .map_err(err)
}
impl Store {
    pub(super) fn raw_guard(&self) -> Result<(), String> {
        guard(&self.db, self.raw_sources.nonce)
    }
    fn raw_refused(&mut self, message: String) -> Result<Option<String>, String> {
        let first = self.raw_sources.blocked.is_none();
        self.raw_sources.blocked = Some(message.clone());
        self.raw_sources.open.clear();
        self.raw_sources.open_bytes = 0;
        self.dirty = true;
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(err)?;
        guard(&tx, self.raw_sources.nonce)?;
        tx.execute(
            "UPDATE raw_source_schema SET disabled=?1 WHERE singleton=1",
            [&message],
        )
        .map_err(err)?;
        tx.commit().map_err(err)?;
        Ok(first.then_some(message))
    }
    pub(super) fn raw_revoked(&self, source: &SourceKey) -> Result<bool, String> {
        self.db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM raw_tool_revoked WHERE agent=?1 AND id=?2)",
                params![agent(source), source.source_id],
                |row| row.get(0),
            )
            .map_err(err)
    }
    fn raw_fence_identity(&mut self, source: &SourceKey) -> Result<(), String> {
        if self
            .pending_input
            .as_ref()
            .is_some_and(|pending| pending.0 == source.source_id)
        {
            self.pending_input = None;
        }
        if self
            .pending_text
            .as_ref()
            .is_some_and(|pending| pending.0 == source.source_id)
        {
            self.pending_text = None;
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(err)?;
        guard(&tx, self.raw_sources.nonce)?;
        tx.execute(
            "INSERT OR IGNORE INTO raw_tool_revoked VALUES(?1,?2)",
            params![agent(source), source.source_id],
        )
        .map_err(err)?;
        tx.commit().map_err(err)?;
        if let Some(length) = self.raw_sources.open.remove(source) {
            self.raw_sources.open_bytes = self.raw_sources.open_bytes.saturating_sub(length);
        }
        self.dirty = true;
        Ok(())
    }
    fn raw_full(
        &mut self,
        source: &SourceKey,
        name: &str,
        input: &str,
        command: Option<&RawCommandCall>,
    ) -> Result<Option<String>, String> {
        self.raw_guard()?;
        if self.raw_sources.blocked.is_some() {
            return Ok(None);
        }
        if !valid_key(source)
            || name.is_empty()
            || name.len() > KEY_BYTES
            || input.len() > RAW_SOURCE_BYTES
        {
            return self.raw_refused("Original tool call exceeds its bounded capture limit; no shortened call was substituted".into());
        }
        if self.raw_revoked(source)? {
            return Ok(Some(
                "This original tool source was removed; a replay cannot recreate it".into(),
            ));
        }
        let hash = digest(input.as_bytes());
        let previous:Option<(i64,Option<Vec<u8>>,String,usize,usize)>=self.db.query_row("SELECT state,hash,name,cost,(SELECT count(*) FROM raw_tool_chunks c WHERE c.agent=s.agent AND c.id=s.id) FROM raw_tool_sources s WHERE agent=?1 AND id=?2",params![agent(source),source.source_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?))).optional().map_err(err)?;
        if let Some((state, old_hash, old_name, ..)) = &previous {
            if *state == SEALED && old_hash.as_deref() == Some(hash.as_slice()) && old_name == name
            {
                return Ok(None);
            }
            if *state == SEALED || old_name != name {
                self.raw_fence_identity(source)?;
                return Ok(Some("A provider reused an original tool identity with a different name or arguments; the ambiguous update was discarded".into()));
            }
            if let Some(length) = self
                .raw_sources
                .open
                .get(source)
                .copied()
                .filter(|n| *n > 0)
            {
                // Claude's complete assistant carrier precedes block_stop.
                // Prove it describes this complete stream before its Item can
                // replace canonical chunks. Incomplete JSON cannot establish
                // that identity; fence only this call, never unrelated sources.
                let streamed = self.adapter_item(&crate::chat::protocol::CanonicalItemKey::new(
                    None,
                    &source.source_id,
                ))?;
                let matches = streamed.is_some_and(|item| match item.content {
                    ItemContent::Tool {
                        input: streamed, ..
                    } => streamed.len() == length && same_native_json(&streamed, input),
                    _ => false,
                });
                if !matches {
                    self.raw_fence_identity(source)?;
                    return Ok(Some("A complete original tool carrier did not match its streamed arguments; the ambiguous update was discarded".into()));
                }
                // Keep the exact streamed bytes and their StreamJson origin;
                // the equal native object proves identity, not replacement.
                return self.raw_seal(source, length, &hash, command);
            }
        }
        let command = command_bytes(command)?;
        if input.len().saturating_add(command.len()) > RAW_SOURCE_BYTES {
            return self.raw_refused("Original tool call exceeds its bounded capture limit".into());
        }
        let chunk_count = input.len().div_ceil(PART_BYTES) + command.len().div_ceil(PART_BYTES);
        let cost = input
            .len()
            .saturating_add(command.len())
            .saturating_add(source_cost(source, name))
            .saturating_add(chunk_count.saturating_mul(chunk_cost(source)));
        let current_open = self.raw_sources.open.get(source).copied().unwrap_or(0);
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(err)?;
        guard(&tx, self.raw_sources.nonce)?;
        let (bytes, keys, chunks): (usize, usize, usize) = tx
            .query_row(
                "SELECT bytes,keys,chunks FROM raw_source_schema WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(err)?;
        let old_cost = previous.as_ref().map_or(0, |row| row.3);
        let old_chunks = previous.as_ref().map_or(0, |row| row.4);
        if bytes
            .saturating_sub(old_cost)
            .saturating_add(cost)
            .saturating_add(self.raw_sources.open_bytes.saturating_sub(current_open))
            > DISK_BYTES
            || previous.is_none() && keys >= KEYS
            || chunks
                .saturating_sub(old_chunks)
                .saturating_add(chunk_count)
                > CHUNKS
        {
            drop(tx);
            return self.raw_refused("This conversation reached its original tool history limit; complete original calls are unavailable until reset".into());
        }
        if revoked(&tx, source)? {
            return Err("Original tool source was removed during capture".into());
        }
        let epoch = revision(&tx)?;
        tx.execute("INSERT INTO raw_tool_sources VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10) ON CONFLICT(agent,id) DO UPDATE SET name=excluded.name,representation=excluded.representation,input_len=excluded.input_len,command_len=excluded.command_len,state=excluded.state,hash=excluded.hash,epoch=excluded.epoch,cost=excluded.cost",params![agent(source),source.source_id,name,1,input.len(),command.len(),SEALED,hash.as_slice(),epoch,cost]).map_err(err)?;
        tx.execute(
            "DELETE FROM raw_tool_chunks WHERE agent=?1 AND id=?2",
            params![agent(source), source.source_id],
        )
        .map_err(err)?;
        insert(&tx, source, 0, input.as_bytes())?;
        insert(&tx, source, 1, &command)?;
        tx.execute("UPDATE raw_source_schema SET bytes=bytes-?1+?2,keys=keys+?3,chunks=chunks-?4+?5 WHERE singleton=1",params![old_cost,cost,usize::from(previous.is_none()),old_chunks,chunk_count]).map_err(err)?;
        tx.commit().map_err(err)?;
        self.raw_sources.open.remove(source);
        self.raw_sources.open_bytes = self.raw_sources.open_bytes.saturating_sub(current_open);
        self.dirty = true;
        Ok(None)
    }

    fn raw_start(&mut self, source: &SourceKey, name: &str) -> Result<Option<String>, String> {
        self.raw_guard()?;
        if self.raw_sources.blocked.is_some() {
            return Ok(None);
        }
        if source.agent_id.is_some() {
            return Ok(None);
        } // Child raw arguments are not captured by this lane.
        if !valid_key(source) || name.is_empty() || name.len() > KEY_BYTES {
            return self
                .raw_refused("Original tool call identity exceeds the capture limit".into());
        }
        if self.raw_revoked(source)? {
            return Ok(Some(
                "Removed original tool call cannot be recreated".into(),
            ));
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(err)?;
        guard(&tx, self.raw_sources.nonce)?;
        if revoked(&tx, source)? {
            return Err("Original tool call was removed".into());
        }
        let old: Option<(String, i64)> = tx
            .query_row(
                "SELECT name,state FROM raw_tool_sources WHERE agent=?1 AND id=?2",
                params![agent(source), source.source_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .map_err(err)?;
        if let Some((old, state)) = old {
            if state == SEALED || old != name {
                // A stream must retain its unfinished call's native name and
                // cannot claim an already sealed ID. Preserve old bodies while
                // fencing later placeholders and deltas for this identity.
                drop(tx);
                self.raw_fence_identity(source)?;
                return Ok(Some(
                    "Provider reused an original tool identity; its ambiguous stream was discarded"
                        .into(),
                ));
            }
            return Ok(None);
        }
        if self.raw_sources.open.len() >= OPEN_KEYS {
            drop(tx);
            return self.raw_refused("Too many unfinished original tool calls".into());
        }
        let (bytes, keys): (usize, usize) = tx
            .query_row(
                "SELECT bytes,keys FROM raw_source_schema WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(err)?;
        let cost = source_cost(source, name);
        if keys >= KEYS
            || bytes
                .saturating_add(cost)
                .saturating_add(self.raw_sources.open_bytes)
                > DISK_BYTES
        {
            drop(tx);
            return self.raw_refused("Original tool history identity budget reached".into());
        }
        let epoch = revision(&tx)?;
        tx.execute(
            "INSERT INTO raw_tool_sources VALUES(?1,?2,?3,0,0,0,0,NULL,?4,?5)",
            params![agent(source), source.source_id, name, epoch, cost],
        )
        .map_err(err)?;
        tx.execute(
            "UPDATE raw_source_schema SET bytes=bytes+?1,keys=keys+1 WHERE singleton=1",
            [cost],
        )
        .map_err(err)?;
        tx.commit().map_err(err)?;
        self.raw_sources.open.insert(source.clone(), 0);
        Ok(None)
    }
    fn raw_delta(
        &mut self,
        id: &str,
        offset: usize,
        delta: &str,
    ) -> Result<Option<String>, String> {
        let source = key("", id);
        let Some(length) = self.raw_sources.open.get(&source).copied() else {
            return Ok(None);
        };
        // The canonical writer owns bytes. Only bounded resident counters change
        // per fragment; no raw-prefix SQL rewrite occurs here.
        let next = offset
            .checked_add(delta.len())
            .ok_or("Original input length overflow")?;
        if offset != length {
            return self.raw_refused("Original input stream has a gap or overlap".into());
        }
        let total = self.raw_sources.open_bytes.saturating_add(delta.len());
        if next > RAW_SOURCE_BYTES || total > DISK_BYTES {
            return self.raw_refused("Original input streaming budget reached".into());
        }
        self.raw_sources.open.insert(source, next);
        self.raw_sources.open_bytes = total;
        Ok(None)
    }
    fn raw_seal(
        &mut self,
        source: &SourceKey,
        input_len: usize,
        hash: &[u8; 32],
        command: Option<&RawCommandCall>,
    ) -> Result<Option<String>, String> {
        self.raw_guard()?;
        if self.raw_sources.blocked.is_some() || source.agent_id.is_some() {
            return Ok(None);
        }
        self.flush_input()?;
        let command = command_bytes(command)?;
        if input_len.saturating_add(command.len()) > RAW_SOURCE_BYTES {
            return self.raw_refused("Original tool call exceeds the bounded source limit".into());
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(err)?;
        guard(&tx, self.raw_sources.nonce)?;
        if revoked(&tx, source)? {
            return Err("Original tool call was removed before completion".into());
        }
        let (state, old_hash, old_cost): (i64, Option<Vec<u8>>, usize) = tx
            .query_row(
                "SELECT state,hash,cost FROM raw_tool_sources WHERE agent=?1 AND id=?2",
                params![agent(source), source.source_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(err)?
            .ok_or("Original stream start is missing")?;
        if state == SEALED {
            if old_hash.as_deref() == Some(hash.as_slice()) {
                return Ok(None);
            }
            return Err("Original stream differs from an already completed call".into());
        }
        if self.raw_sources.open.get(source).copied() != Some(input_len) {
            return Err("Original input capture is incomplete".into());
        }
        // Validate exact source coverage without materializing its whole body.
        let mut query=tx.prepare_cached("SELECT position,length(data) FROM chunks WHERE id=?1 AND field=0 ORDER BY position").map_err(err)?;
        let rows = query
            .query_map([&source.source_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })
            .map_err(err)?;
        let mut cursor = 0usize;
        let mut count = 0usize;
        for row in rows {
            let (position, length) = row.map_err(err)?;
            let length = usize::try_from(length).map_err(|_| "Invalid original input chunk")?;
            if position != cursor as i64 || length == 0 || length > PART_BYTES {
                return Err("Original input chunks have a gap, overlap or oversized row".into());
            }
            cursor = cursor
                .checked_add(length)
                .ok_or("Original input size overflow")?;
            count += 1;
        }
        drop(query);
        if cursor != input_len {
            return Err("Original input body was shortened; no raw call was substituted".into());
        }
        let command_count = command.len().div_ceil(PART_BYTES);
        let new_chunks = count + command_count;
        let cost = old_cost
            .saturating_add(input_len)
            .saturating_add(command.len())
            .saturating_add(new_chunks.saturating_mul(chunk_cost(source)));
        let (bytes, chunks): (usize, usize) = tx
            .query_row(
                "SELECT bytes,chunks FROM raw_source_schema WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(err)?;
        if bytes
            .saturating_sub(old_cost)
            .saturating_add(cost)
            .saturating_add(self.raw_sources.open_bytes.saturating_sub(input_len))
            > DISK_BYTES
            || chunks.saturating_add(new_chunks) > CHUNKS
        {
            drop(tx);
            return self.raw_refused("Original tool source storage budget reached".into());
        }
        // SQL-owned transfer: raw arguments survive the next Changes rewrite.
        tx.execute("INSERT INTO raw_tool_chunks(agent,id,field,position,data) SELECT '',id,0,position,data FROM chunks WHERE id=?1 AND field=0",[&source.source_id]).map_err(err)?;
        insert(&tx, source, 1, &command)?;
        tx.execute("UPDATE raw_tool_sources SET state=1,input_len=?3,command_len=?4,hash=?5,cost=?6 WHERE agent=?1 AND id=?2",params![agent(source),source.source_id,input_len,command.len(),hash.as_slice(),cost]).map_err(err)?;
        tx.execute(
            "UPDATE raw_source_schema SET bytes=bytes-?1+?2,chunks=chunks+?3 WHERE singleton=1",
            params![old_cost, cost, new_chunks],
        )
        .map_err(err)?;
        tx.commit().map_err(err)?;
        self.raw_sources.open.remove(source);
        self.raw_sources.open_bytes = self.raw_sources.open_bytes.saturating_sub(input_len);
        self.dirty = true;
        Ok(None)
    }
    pub(super) fn cache_raw_call(&mut self, event: &Event) -> Result<Option<String>, String> {
        let capture = match event {
            Event::RawToolCapture(capture) => match capture {
                RawToolCapture::Start { source, name } => self.raw_start(source, name),
                RawToolCapture::Full {
                    source,
                    name,
                    input,
                    command,
                } if source.agent_id.is_none() => {
                    self.raw_full(source, name, input, command.as_ref())
                }
                RawToolCapture::Seal {
                    source,
                    input_len,
                    canonical_hash,
                    command,
                } => self.raw_seal(source, *input_len, canonical_hash, command.as_ref()),
                _ => Ok(None),
            },
            Event::InputDelta { id, offset, delta } => self.raw_delta(id, *offset, delta),
            Event::ItemsRemoved { ids } => {
                self.raw_remove(ids, true)?;
                Ok(None)
            }
            Event::Reset { .. } => {
                self.raw_reset()?;
                Ok(None)
            }
            Event::Item(item)
                if !matches!(
                    &item.content,
                    ItemContent::Tool { .. }
                        | ItemContent::Command { .. }
                        | ItemContent::Changes { .. }
                ) =>
            {
                self.raw_remove(std::slice::from_ref(&item.id), false)?;
                Ok(None)
            }
            Event::History { items, .. } | Event::LatestHistory { items, .. } => {
                let ids: Vec<_> = items
                    .iter()
                    .filter(|item| {
                        !matches!(
                            &item.content,
                            ItemContent::Tool { .. }
                                | ItemContent::Command { .. }
                                | ItemContent::Changes { .. }
                        )
                    })
                    .map(|item| item.id.clone())
                    .collect();
                self.raw_remove(&ids, false)?;
                Ok(None)
            }
            _ => Ok(None),
        };
        match capture {
            Ok(warning) => Ok(warning),
            Err(message)
                if matches!(event, Event::RawToolCapture(_) | Event::InputDelta { .. }) =>
            {
                self.raw_refused(format!("Original tool source is unavailable: {message}"))
            }
            Err(message) => Err(message),
        }
    }
    fn raw_remove(&mut self, ids: &[String], explicit_removal: bool) -> Result<(), String> {
        // These bytes have not entered SQLite. Flushing after publishing the
        // tombstone would either revive removed data or repeatedly fail every
        // later event against the same pending batch.
        if explicit_removal
            && self
                .pending_input
                .as_ref()
                .is_some_and(|pending| ids.contains(&pending.0))
        {
            self.pending_input = None;
        }
        if explicit_removal
            && self
                .pending_text
                .as_ref()
                .is_some_and(|pending| ids.contains(&pending.0))
        {
            self.pending_text = None;
        }
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(err)?;
        guard(&tx, self.raw_sources.nonce)?;
        for id in ids {
            let old:Option<(usize,usize)>=tx.query_row("SELECT cost,(SELECT count(*) FROM raw_tool_chunks c WHERE c.agent=s.agent AND c.id=s.id) FROM raw_tool_sources s WHERE agent='' AND id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(err)?;
            let Some((cost, chunks)) = old else {
                continue;
            };
            // Ordinary prose/history is not a raw-call kind change. Only an
            // actually captured identity authorizes discarding pending bytes.
            if !explicit_removal {
                if self
                    .pending_input
                    .as_ref()
                    .is_some_and(|pending| &pending.0 == id)
                {
                    self.pending_input = None;
                }
                if self
                    .pending_text
                    .as_ref()
                    .is_some_and(|pending| &pending.0 == id)
                {
                    self.pending_text = None;
                }
            }
            tx.execute("DELETE FROM raw_tool_chunks WHERE agent='' AND id=?1", [id])
                .map_err(err)?;
            tx.execute(
                "DELETE FROM raw_tool_sources WHERE agent='' AND id=?1",
                [id],
            )
            .map_err(err)?;
            tx.execute("INSERT OR IGNORE INTO raw_tool_revoked VALUES('',?1)", [id])
                .map_err(err)?;
            // The identity/key charge remains reserved by its tombstone.
            tx.execute(
                "UPDATE raw_source_schema SET bytes=bytes-?1+?2,chunks=chunks-?3 WHERE singleton=1",
                params![cost, tombstone_cost(&key("", id)), chunks],
            )
            .map_err(err)?;
        }
        tx.commit().map_err(err)?;
        for id in ids {
            if let Some(length) = self.raw_sources.open.remove(&key("", id)) {
                self.raw_sources.open_bytes = self.raw_sources.open_bytes.saturating_sub(length);
            }
        }
        Ok(())
    }
    fn raw_reset(&mut self) -> Result<(), String> {
        self.pending_input = None;
        self.pending_text = None;
        let nonce = uuid::Uuid::new_v4();
        let tx = self
            .db
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .map_err(err)?;
        guard(&tx, self.raw_sources.nonce)?;
        tx.execute_batch("DELETE FROM raw_tool_chunks;DELETE FROM raw_tool_sources;DELETE FROM raw_tool_revoked;").map_err(err)?;
        tx.execute("UPDATE raw_source_schema SET nonce=?1,bytes=0,keys=0,chunks=0,disabled=NULL WHERE singleton=1",[nonce.to_string()]).map_err(err)?;
        tx.commit().map_err(err)?;
        self.raw_sources = State {
            nonce,
            open: HashMap::new(),
            open_bytes: 0,
            blocked: None,
        };
        Ok(())
    }
    pub(super) fn raw_event_revoked(&self, event: &Event) -> Result<bool, String> {
        self.raw_guard()?;
        let ids: Vec<&str> = match event {
            Event::RawToolCapture(capture) if capture.source().agent_id.is_none() => {
                vec![&capture.source().source_id]
            }
            Event::Item(item) => vec![&item.id],
            Event::InputDelta { id, .. } | Event::OutputDelta { id, .. } => vec![id],
            _ => return Ok(false),
        };
        for id in ids {
            if self.raw_revoked(&key("", id))? {
                return Ok(true);
            }
        }
        Ok(false)
    }
    pub(super) fn raw_filter_history(&self, event: &mut Event) -> Result<bool, String> {
        let (Event::History { items, .. } | Event::LatestHistory { items, .. }) = event else {
            return Ok(false);
        };
        self.raw_guard()?;
        let mut removed = false;
        let mut retained = Vec::with_capacity(items.len());
        for item in std::mem::take(items) {
            if self.raw_revoked(&key("", &item.id))? {
                removed = true;
            } else {
                retained.push(item);
            }
        }
        *items = retained;
        Ok(removed)
    }
    pub(super) fn raw_parts(&self, id: &str) -> Result<Vec<RawSourcePart>, String> {
        let _read = self.db.unchecked_transaction().map_err(err)?;
        self.raw_guard()?;
        if self.raw_sources.blocked.is_some() {
            return Ok(Vec::new());
        }
        let row:Option<(String,i64,usize,usize,u64)>=self.db.query_row("SELECT name,representation,input_len,command_len,epoch FROM raw_tool_sources WHERE agent='' AND id=?1 AND state=1 AND NOT EXISTS(SELECT 1 FROM raw_tool_revoked r WHERE r.agent='' AND r.id=raw_tool_sources.id)",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?))).optional().map_err(err)?;
        let Some((name, rep, input, command, revision)) = row else {
            return Ok(Vec::new());
        };
        let epoch = SourceEpoch {
            incarnation: self.source_incarnation,
            revision,
        };
        let mut parts = vec![RawSourcePart {
            item_id: id.into(),
            source: key("", id),
            field: RawSourceField::ToolInput,
            epoch,
            source_len: input,
            metadata: RawSourceMetadata {
                tool_name: name.clone(),
                representation: if rep == 0 {
                    RawRepresentation::StreamJson
                } else {
                    RawRepresentation::CanonicalJson
                },
                complete_call: true,
            },
        }];
        if command > 0 {
            parts.push(RawSourcePart {
                item_id: id.into(),
                source: key("", id),
                field: RawSourceField::CommandCall,
                epoch,
                source_len: command,
                metadata: RawSourceMetadata {
                    tool_name: name,
                    representation: RawRepresentation::CommandMetadataJson,
                    complete_call: true,
                },
            });
        }
        Ok(parts)
    }
    /// Called only while the parent canonical reader holds its read snapshot.
    pub(super) fn raw_command(&self, source: &SourceKey) -> Result<Option<RawCommandCall>, String> {
        self.raw_guard()?;
        if let Some(reason) = &self.raw_sources.blocked {
            return Err(reason.clone());
        }
        if self.raw_revoked(source)? {
            return Err("Original command was removed".into());
        }
        let len: Option<usize> = self
            .db
            .query_row(
                "SELECT command_len FROM raw_tool_sources WHERE agent=?1 AND id=?2 AND state=1",
                params![agent(source), source.source_id],
                |r| r.get(0),
            )
            .optional()
            .map_err(err)?;
        let Some(len) = len.filter(|len| *len > 0) else {
            return Ok(None);
        };
        if len > RAW_SOURCE_BYTES {
            return Err("Original command exceeds read limit".into());
        }
        serde_json::from_slice(&self.raw_owned_range(source, 1, 0, len)?)
            .map(Some)
            .map_err(err)
    }
    pub(super) fn raw_original(
        &mut self,
        source: &SourceKey,
    ) -> Result<Option<(String, String)>, String> {
        let _read = self.db.unchecked_transaction().map_err(err)?;
        self.raw_guard()?;
        if let Some(reason) = &self.raw_sources.blocked {
            return Err(reason.clone());
        }
        if self.raw_revoked(source)? {
            return Err("Original call was removed".into());
        }
        let row: Option<(String, usize, i64)> = self
            .db
            .query_row(
                "SELECT name,input_len,state FROM raw_tool_sources WHERE agent=?1 AND id=?2",
                params![agent(source), source.source_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()
            .map_err(err)?;
        let Some((name, len, state)) = row else {
            return Ok(None);
        };
        if state != SEALED {
            return Err("Original call is incomplete".into());
        }
        if len > RAW_SOURCE_BYTES {
            return Err("Original call exceeds bounded read limit".into());
        }
        let bytes = self.raw_owned_range(source, 0, 0, len)?;
        Ok(Some((name, String::from_utf8(bytes).map_err(err)?)))
    }

    fn raw_owned_range(
        &self,
        source: &SourceKey,
        field: i64,
        offset: usize,
        end: usize,
    ) -> Result<Vec<u8>, String> {
        let mut statement=self.db.prepare_cached("SELECT position,length(data),CASE WHEN length(data)<=?5 THEN data ELSE NULL END FROM raw_tool_chunks WHERE agent=?1 AND id=?2 AND field=?3 AND position+length(data)>?4 AND position<?6 ORDER BY position").map_err(err)?;
        let rows = statement
            .query_map(
                params![
                    agent(source),
                    source.source_id,
                    field,
                    offset,
                    PART_BYTES,
                    end
                ],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                    ))
                },
            )
            .map_err(err)?;
        let mut result = Vec::with_capacity(end - offset);
        let mut cursor = offset;
        for row in rows {
            let (position, length, data) = row.map_err(err)?;
            let position = usize::try_from(position)
                .map_err(|_| "Original tool source has an invalid chunk position")?;
            let data = data.ok_or("Original tool source has an oversized chunk")?;
            if data.is_empty()
                || length != data.len() as i64
                || position > cursor
                || (!result.is_empty() && position != cursor)
                || position
                    .checked_add(data.len())
                    .is_none_or(|finish| finish <= cursor)
            {
                return Err("Original tool source has a missing or invalid chunk".into());
            }
            let from = cursor - position;
            let keep = (end - cursor).min(data.len() - from);
            result.extend_from_slice(&data[from..from + keep]);
            cursor += keep;
        }
        if cursor != end {
            return Err(
                "Original tool source is incomplete; no shortened prefix was substituted".into(),
            );
        }
        Ok(result)
    }
    pub(crate) fn raw_source_page(
        &mut self,
        query: RawSourceQuery,
    ) -> Result<RawSourcePage, SourceError> {
        if !valid_key(&query.source)
            || !(4..=PAGE_BYTES).contains(&query.limit)
            || (query.offset > 0 && (query.until.is_none() || query.expected_epoch.is_none()))
            || query.until.is_some() && query.expected_epoch.is_none()
        {
            return Err(SourceError::InvalidRange);
        }
        let _read = self.db.unchecked_transaction().map_err(source_error)?;
        self.raw_guard().map_err(source_error)?;
        if let Some(reason) = &self.raw_sources.blocked {
            return Err(source_error(reason));
        }
        if self.raw_revoked(&query.source).map_err(source_error)? {
            return Err(SourceError::NotFound);
        }
        let record:Option<(String,i64,usize,usize,i64,u64)>=self.db.query_row("SELECT name,representation,input_len,command_len,state,epoch FROM raw_tool_sources WHERE agent=?1 AND id=?2",params![agent(&query.source),query.source.source_id],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?))).optional().map_err(source_error)?;
        let (name, representation, input_len, command_len, state, revision) =
            record.ok_or(SourceError::NotFound)?;
        if state != SEALED {
            return Err(source_error(
                "Original tool input is still streaming or unavailable",
            ));
        }
        let field = match query.field {
            RawSourceField::ToolInput => 0,
            RawSourceField::CommandCall => 1,
        };
        let length = if field == 0 { input_len } else { command_len };
        if field == 1 && length == 0 {
            return Err(SourceError::Unsupported);
        }
        let epoch = SourceEpoch {
            incarnation: self.source_incarnation,
            revision,
        };
        if query
            .expected_epoch
            .is_some_and(|expected| expected != epoch)
        {
            return Err(SourceError::Changed);
        }
        let until = query.until.unwrap_or(length);
        if until > length || query.offset > until {
            return Err(SourceError::InvalidRange);
        }
        if until > RAW_SOURCE_BYTES {
            return Err(SourceError::TooLarge {
                source_len: until,
                limit: RAW_SOURCE_BYTES,
            });
        }
        if until < length
            && self
                .raw_owned_range(&query.source, field, until, until + 1)
                .map_err(source_error)?
                .first()
                .is_some_and(|byte| byte & 0xc0 == 0x80)
        {
            return Err(SourceError::InvalidRange);
        }
        let target = query.offset.saturating_add(query.limit).min(until);
        let bytes = self
            .raw_owned_range(
                &query.source,
                field,
                query.offset,
                target.saturating_add(3).min(until),
            )
            .map_err(source_error)?;
        if bytes.first().is_some_and(|byte| byte & 0xc0 == 0x80) {
            return Err(SourceError::InvalidRange);
        }
        let mut keep = target - query.offset;
        while keep > 0 && keep < bytes.len() && bytes[keep] & 0xc0 == 0x80 {
            keep -= 1;
        }
        let text = String::from_utf8(bytes[..keep].to_vec())
            .map_err(|_| source_error("Original tool source is not valid UTF-8"))?;
        if keep == 0 && query.offset < until {
            return Err(SourceError::InvalidRange);
        }
        let next_offset = query.offset + keep;
        Ok(RawSourcePage {
            source: query.source,
            field: query.field,
            epoch,
            source_len: until,
            offset: query.offset,
            next_offset,
            text,
            complete: next_offset == until,
            metadata: RawSourceMetadata {
                tool_name: name,
                representation: if field == 1 {
                    RawRepresentation::CommandMetadataJson
                } else if representation == 0 {
                    RawRepresentation::StreamJson
                } else {
                    RawRepresentation::CanonicalJson
                },
                complete_call: until == length,
            },
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::protocol::Adapter;
    use crate::chat::{CommandAction, ToolStatus};
    use serde_json::json;
    fn query(id: &str) -> RawSourceQuery {
        RawSourceQuery {
            source: key("", id),
            field: RawSourceField::ToolInput,
            offset: 0,
            limit: 256 * 1024,
            expected_epoch: None,
            until: None,
        }
    }
    fn full(store: &mut Store, id: &str, input: &str) {
        store
            .apply(Event::RawToolCapture(RawToolCapture::Full {
                source: key("", id),
                name: "Write".into(),
                input: input.into(),
                command: None,
            }))
            .unwrap();
    }
    fn commit(
        store: &std::sync::Arc<std::sync::Mutex<Store>>,
        adapter: &mut dyn Adapter,
        step: crate::chat::protocol::Step,
    ) {
        for event in step.events {
            store.lock().unwrap().apply(event).unwrap();
        }
        adapter.committed();
    }
    #[test]
    fn raw_migration_is_explicit_atomic_and_fences_an_already_open_handle() {
        let mut db = Connection::open_in_memory().unwrap();
        let first = initialize(&mut db).unwrap();
        guard(&db, first.nonce).unwrap();
        let second = initialize(&mut db).unwrap();
        assert!(guard(&db, first.nonce).is_err());
        guard(&db, second.nonce).unwrap();
        db.execute("UPDATE raw_source_schema SET version=9", [])
            .unwrap();
        assert!(initialize(&mut db).is_err());
        let unchanged: i64 = db
            .query_row("SELECT version FROM raw_source_schema", [], |r| r.get(0))
            .unwrap();
        assert_eq!(unchanged, 9);
        let mut partial = Connection::open_in_memory().unwrap();
        partial
            .execute("CREATE TABLE raw_tool_chunks(private BLOB)", [])
            .unwrap();
        assert!(initialize(&mut partial).is_err());
        assert!(
            !partial
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='raw_source_schema')",
                    [],
                    |r| r.get::<_, bool>(0)
                )
                .unwrap(),
            "partial migration DDL must roll back"
        );
    }
    #[test]
    fn raw_migration_reservation_serializes_with_revocation_and_rejects_stale_writes() {
        let mut store = Store::new().unwrap();
        full(&mut store, "call", "{\"content\":\"private\"}");
        let path: Option<String> = store
            .db
            .query_row(
                "SELECT file FROM pragma_database_list WHERE name='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let mut reopened = Connection::open(path.unwrap()).unwrap();
        let _owner = initialize(&mut reopened).unwrap();
        assert!(store.raw_source_page(query("call")).is_err());
        assert!(
            store
                .apply(Event::RawToolCapture(RawToolCapture::Full {
                    source: key("", "next"),
                    name: "Write".into(),
                    input: "null".into(),
                    command: None
                }))
                .is_err()
        );
        assert_eq!(
            reopened
                .query_row(
                    "SELECT count(*) FROM raw_tool_sources WHERE id='next'",
                    [],
                    |r| r.get::<_, usize>(0)
                )
                .unwrap(),
            0
        );
    }
    #[test]
    fn sealed_raw_source_pages_preserve_epoch_representation_and_prefix_truth() {
        let mut store = Store::new().unwrap();
        let input = json!({"file_path":"file","content":"λ".repeat(300_000)}).to_string();
        full(&mut store, "call", &input);
        let first = store.raw_source_page(query("call")).unwrap();
        assert_eq!(
            first.metadata.representation,
            RawRepresentation::CanonicalJson
        );
        assert!(first.metadata.complete_call);
        let mut prefix = query("call");
        prefix.expected_epoch = Some(first.epoch);
        prefix.until = Some(64);
        prefix.limit = 64;
        let frozen = store.raw_source_page(prefix).unwrap();
        assert!(frozen.complete);
        assert!(!frozen.metadata.complete_call);
        full(&mut store, "call", &input);
        assert_eq!(
            store.raw_source_page(query("call")).unwrap().epoch,
            first.epoch
        );
        let mut all = first.text;
        let mut offset = first.next_offset;
        while offset < input.len() {
            let mut page = query("call");
            page.offset = offset;
            page.until = Some(input.len());
            page.expected_epoch = Some(first.epoch);
            let page = store.raw_source_page(page).unwrap();
            offset = page.next_offset;
            all.push_str(&page.text);
        }
        assert_eq!(all, input);
        let mut wrong = query("call");
        wrong.expected_epoch = Some(SourceEpoch {
            incarnation: uuid::Uuid::new_v4(),
            revision: first.epoch.revision,
        });
        assert_eq!(store.raw_source_page(wrong), Err(SourceError::Changed));
    }
    #[test]
    fn removal_reset_and_failed_reset_never_recreate_private_raw_calls() {
        let mut store = Store::new().unwrap();
        full(&mut store, "private", "null");
        let old = store.raw_source_page(query("private")).unwrap().epoch;
        store
            .apply(Event::ItemsRemoved {
                ids: vec!["private".into()],
            })
            .unwrap();
        full(&mut store, "private", "null");
        assert_eq!(
            store.raw_source_page(query("private")),
            Err(SourceError::NotFound)
        );
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM raw_tool_sources", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            0
        );
        store.db.execute_batch("CREATE TRIGGER refuse_raw_reset BEFORE DELETE ON raw_tool_revoked BEGIN SELECT RAISE(FAIL,'retained privacy proof');END;").unwrap();
        assert!(
            store
                .apply(Event::Reset {
                    reason: "test".into()
                })
                .is_err()
        );
        assert_eq!(
            store.raw_source_page(query("private")),
            Err(SourceError::NotFound)
        );
        store
            .db
            .execute_batch("DROP TRIGGER refuse_raw_reset")
            .unwrap();
        store
            .apply(Event::Reset {
                reason: "test".into(),
            })
            .unwrap();
        full(&mut store, "private", "null");
        assert_ne!(store.raw_source_page(query("private")).unwrap().epoch, old);
    }
    #[test]
    fn corrupt_raw_chunks_never_promote_a_partial_original() {
        for missing in [0, PART_BYTES] {
            let mut store = Store::new().unwrap();
            full(&mut store, "call", &"x".repeat(PART_BYTES * 3));
            store
                .db
                .execute(
                    "DELETE FROM raw_tool_chunks WHERE id='call' AND field=0 AND position=?1",
                    [missing],
                )
                .unwrap();
            assert!(store.raw_original(&key("", "call")).is_err());
        }
        let mut store = Store::new().unwrap();
        full(&mut store, "call", &"x".repeat(PART_BYTES * 2));
        store
            .db
            .execute(
                "UPDATE raw_tool_chunks SET position=position-1 WHERE position=?1",
                [PART_BYTES],
            )
            .unwrap();
        assert!(store.raw_original(&key("", "call")).is_err());
    }
    #[test]
    fn streaming_seal_transfers_exact_chunks_before_derived_rewrite_and_canonical_replay() {
        let mut store = Store::new().unwrap();
        let input = format!(
            "{{ \"file_path\" : \"file\", \"content\" : \"{}\" }}",
            "x".repeat(1024 * 1024)
        );
        store
            .apply(Event::RawToolCapture(RawToolCapture::Start {
                source: key("", "call"),
                name: "Write".into(),
            }))
            .unwrap();
        store
            .apply(Event::Item(Item {
                id: "call".into(),
                role: Role::Assistant,
                started_at: None,
                at: None,
                content: ItemContent::Tool {
                    name: "Write".into(),
                    input: String::new(),
                    output: String::new(),
                    status: ToolStatus::Running,
                },
            }))
            .unwrap();
        assert!(
            store.raw_source_page(query("call")).is_err(),
            "open capture never claims OriginalCap completeness"
        );
        for (index, bytes) in input.as_bytes().chunks(50).enumerate() {
            store
                .apply(Event::InputDelta {
                    id: "call".into(),
                    offset: index * 50,
                    delta: String::from_utf8(bytes.to_vec()).unwrap(),
                })
                .unwrap();
            store
                .apply(Event::Status {
                    busy: true,
                    detail: String::new(),
                })
                .unwrap();
        }
        let canonical = serde_json::from_str::<serde_json::Value>(&input)
            .unwrap()
            .to_string();
        store
            .apply(Event::RawToolCapture(RawToolCapture::Seal {
                source: key("", "call"),
                input_len: input.len(),
                canonical_hash: digest(canonical.as_bytes()),
                command: None,
            }))
            .unwrap();
        let page = store.raw_source_page(query("call")).unwrap();
        assert_eq!(page.metadata.representation, RawRepresentation::StreamJson);
        let epoch = page.epoch;
        store
            .apply(Event::Item(Item {
                id: "call".into(),
                role: Role::Assistant,
                started_at: None,
                at: None,
                content: ItemContent::Changes {
                    changes: Vec::new(),
                    status: ToolStatus::Running,
                },
            }))
            .unwrap();
        assert_eq!(
            store.raw_original(&key("", "call")).unwrap().unwrap().1,
            input
        );
        full(&mut store, "call", &canonical);
        assert_eq!(store.raw_source_page(query("call")).unwrap().epoch, epoch);
        assert_eq!(
            store
                .raw_source_page(query("call"))
                .unwrap()
                .metadata
                .representation,
            RawRepresentation::StreamJson
        );
        let copied: usize = store
            .db
            .query_row("SELECT sum(length(data)) FROM raw_tool_chunks", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            copied,
            input.len(),
            "seal stores one exact original input, not an additional derived patch"
        );
    }
    #[test]
    fn oversized_command_metadata_hydrates_only_from_captured_original_projection() {
        let mut store = Store::new().unwrap();
        let command = "c".repeat(100_000);
        let original = RawCommandCall {
            label: "full".into(),
            command: command.clone(),
            working_directory: "/tmp".into(),
            action: CommandAction::Run,
        };
        store
            .apply(Event::RawToolCapture(RawToolCapture::Full {
                source: key("", "command"),
                name: "Bash".into(),
                input: json!({"command":command}).to_string(),
                command: Some(original),
            }))
            .unwrap();
        store
            .apply(Event::Item(Item {
                id: "command".into(),
                role: Role::Assistant,
                started_at: None,
                at: None,
                content: ItemContent::Command {
                    label: "full".into(),
                    command: command.clone(),
                    working_directory: "/tmp".into(),
                    action: CommandAction::Run,
                    output: "actual output".into(),
                    status: ToolStatus::Running,
                    exit_code: None,
                    duration_ms: None,
                },
            }))
            .unwrap();
        let item = store
            .adapter_item(&crate::chat::protocol::CanonicalItemKey::new(
                None, "command",
            ))
            .unwrap()
            .unwrap();
        let ItemContent::Command {
            command: actual,
            output,
            ..
        } = item.content
        else {
            panic!()
        };
        assert_eq!(actual, command);
        assert_eq!(output, "actual output");
        let descriptors = store.window().unwrap().raw_source_parts;
        assert!(
            descriptors
                .iter()
                .any(|part| part.field == RawSourceField::CommandCall)
        );
    }
    #[test]
    fn actual_failed_write_after_body_and_metadata_eviction_recovers_native_name_and_arguments() {
        let store = std::sync::Arc::new(std::sync::Mutex::new(Store::new().unwrap()));
        let mut adapter = crate::chat::protocol::claude::fixture_adapter();
        adapter.bind_item_reader(super::super::adapter_item_reader(&store));
        let input = json!({"file_path":"large.txt","content":"x".repeat(3*1024*1024)});
        let call = |id: &str, input: serde_json::Value| json!({"type":"assistant","uuid":format!("carrier-{id}"),"message":{"id":format!("message-{id}"),"content":[{"type":"tool_use","id":id,"name":"Write","input":input}]}});
        let step = adapter.receive(call("old", input.clone())).unwrap();
        commit(&store, adapter.as_mut(), step);
        for index in 0..513 {
            let step = adapter
                .receive(call(
                    &format!("next-{index}"),
                    json!({"file_path":"small","content":"x"}),
                ))
                .unwrap();
            commit(&store, adapter.as_mut(), step);
        }
        let step=adapter.receive(json!({"type":"user","uuid":"failure","message":{"content":[{"type":"tool_result","tool_use_id":"old","content":"permission denied","is_error":true}]}})).unwrap();
        commit(&store, adapter.as_mut(), step);
        let item = store
            .lock()
            .unwrap()
            .adapter_item(&crate::chat::protocol::CanonicalItemKey::new(None, "old"))
            .unwrap()
            .unwrap();
        let ItemContent::Tool {
            name,
            input: actual,
            status,
            ..
        } = item.content
        else {
            panic!(
                "actual failed Write must regain its original tool call, not a synthetic Edit or patch"
            )
        };
        assert_eq!(name, "Write");
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&actual).unwrap(),
            input
        );
        assert_eq!(status, ToolStatus::Failed);
    }
}

#[cfg(test)]
mod identity_reuse_tests {
    use super::*;
    #[test]
    fn an_unfinished_stream_cannot_change_its_original_tool_name() {
        let mut store = Store::new().unwrap();
        let source = key("", "open");
        store
            .apply(Event::RawToolCapture(RawToolCapture::Start {
                source: source.clone(),
                name: "Write".into(),
            }))
            .unwrap();
        let original = Item {
            id: "open".into(),
            role: Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Tool {
                name: "Write".into(),
                input: String::new(),
                output: String::new(),
                status: crate::chat::ToolStatus::Running,
            },
        };
        store.apply(Event::Item(original.clone())).unwrap();
        let metadata: Vec<u8> = store
            .db
            .query_row("SELECT metadata FROM items WHERE id='open'", [], |r| {
                r.get(0)
            })
            .unwrap();

        // A repeated start for the same native name is harmless. A changed
        // name cannot inherit its input stream, even before the first delta.
        let repeated = store
            .apply(Event::RawToolCapture(RawToolCapture::Start {
                source: source.clone(),
                name: "Write".into(),
            }))
            .unwrap();
        assert!(repeated.is_empty());
        let changed = store
            .apply(Event::RawToolCapture(RawToolCapture::Start {
                source: source.clone(),
                name: "Bash".into(),
            }))
            .unwrap();
        assert!(
            changed
                .iter()
                .any(|event| matches!(event, Event::Error { fatal: false, .. }))
        );
        assert!(store.raw_revoked(&source).unwrap());

        let mut ambiguous = original;
        if let ItemContent::Tool { name, .. } = &mut ambiguous.content {
            *name = "Bash".into();
        }
        store.apply(Event::Item(ambiguous)).unwrap();
        store
            .apply(Event::InputDelta {
                id: "open".into(),
                offset: 0,
                delta: "{}".into(),
            })
            .unwrap();
        assert_eq!(
            store
                .db
                .query_row("SELECT metadata FROM items WHERE id='open'", [], |r| {
                    r.get::<_, Vec<u8>>(0)
                })
                .unwrap(),
            metadata
        );
        assert!(store.raw_original(&source).is_err());
        assert!(store.raw_parts("open").unwrap().is_empty());
        assert!(!store.raw_sources.open.contains_key(&source));

        // Fence this identity without disabling unrelated original capture.
        store
            .apply(Event::RawToolCapture(RawToolCapture::Full {
                source: key("", "unrelated"),
                name: "Write".into(),
                input: "{}".into(),
                command: None,
            }))
            .unwrap();
        assert_eq!(
            store.raw_original(&key("", "unrelated")).unwrap(),
            Some(("Write".into(), "{}".into()))
        );
    }

    #[test]
    fn a_full_completion_cannot_rename_an_unfinished_original_call() {
        let mut store = Store::new().unwrap();
        let source = key("", "full-open");
        store
            .apply(Event::RawToolCapture(RawToolCapture::Start {
                source: source.clone(),
                name: "Write".into(),
            }))
            .unwrap();
        let warning = store
            .apply(Event::RawToolCapture(RawToolCapture::Full {
                source: source.clone(),
                name: "Bash".into(),
                input: "{}".into(),
                command: None,
            }))
            .unwrap();
        assert!(
            warning
                .iter()
                .any(|event| matches!(event, Event::Error { fatal: false, .. }))
        );
        assert!(store.raw_revoked(&source).unwrap());
        assert!(store.raw_original(&source).is_err());
        assert!(store.raw_parts("full-open").unwrap().is_empty());
        assert_eq!(
            store
                .db
                .query_row(
                    "SELECT name FROM raw_tool_sources WHERE id='full-open'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "Write"
        );

        let legitimate = key("", "same-name");
        store
            .apply(Event::RawToolCapture(RawToolCapture::Start {
                source: legitimate.clone(),
                name: "Write".into(),
            }))
            .unwrap();
        assert!(
            store
                .apply(Event::RawToolCapture(RawToolCapture::Full {
                    source: legitimate.clone(),
                    name: "Write".into(),
                    input: "{}".into(),
                    command: None,
                }))
                .unwrap()
                .is_empty()
        );
        assert!(!store.raw_revoked(&legitimate).unwrap());
        assert_eq!(
            store.raw_original(&legitimate).unwrap(),
            Some(("Write".into(), "{}".into()))
        );
    }

    #[test]
    fn reopening_raw_capture_recovers_live_streams_without_revoked_open_identities() {
        let mut store = Store::new().unwrap();
        for index in 0..=OPEN_KEYS {
            let source = key("", &format!("revoked-{index}"));
            for name in ["Write", "Bash"] {
                store
                    .apply(Event::RawToolCapture(RawToolCapture::Start {
                        source: source.clone(),
                        name: name.into(),
                    }))
                    .unwrap();
            }
            assert!(store.raw_revoked(&source).unwrap());
        }
        assert!(store.raw_sources.open.is_empty());
        let live = key("", "still-live");
        store
            .apply(Event::RawToolCapture(RawToolCapture::Start {
                source: live.clone(),
                name: "Write".into(),
            }))
            .unwrap();
        let path: String = store
            .db
            .query_row(
                "SELECT file FROM pragma_database_list WHERE name='main'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let mut reopened = Connection::open(path).unwrap();
        let state = initialize(&mut reopened)
            .expect("revoked unfinished calls must not consume reopened live capture slots");
        assert!(guard(&store.db, store.raw_sources.nonce).is_err());
        store.db = reopened;
        store.raw_sources = state;
        assert_eq!(store.raw_sources.open.len(), 1);
        assert_eq!(store.raw_sources.open.get(&live), Some(&0));
        assert_eq!(store.raw_sources.open_bytes, 0);

        let removed = key("", "revoked-0");
        let warning = store
            .apply(Event::RawToolCapture(RawToolCapture::Full {
                source: removed.clone(),
                name: "Write".into(),
                input: "{}".into(),
                command: None,
            }))
            .unwrap();
        assert!(
            warning
                .iter()
                .any(|event| matches!(event, Event::Error { fatal: false, .. }))
        );
        assert!(store.raw_original(&removed).is_err());
        store
            .apply(Event::RawToolCapture(RawToolCapture::Full {
                source: live.clone(),
                name: "Write".into(),
                input: "{}".into(),
                command: None,
            }))
            .unwrap();
        assert_eq!(
            store.raw_original(&live).unwrap(),
            Some(("Write".into(), "{}".into()))
        );
        assert!(store.raw_sources.open.is_empty());
    }

    #[test]
    fn a_new_stream_cannot_replace_an_already_completed_original_item() {
        let mut store = Store::new().unwrap();
        store
            .apply(Event::RawToolCapture(RawToolCapture::Full {
                source: key("", "old"),
                name: "Write".into(),
                input: "null".into(),
                command: None,
            }))
            .unwrap();
        store
            .apply(Event::Item(Item {
                id: "old".into(),
                role: Role::Assistant,
                started_at: None,
                at: None,
                content: ItemContent::Changes {
                    changes: Vec::new(),
                    status: crate::chat::ToolStatus::Succeeded,
                },
            }))
            .unwrap();
        let metadata: Vec<u8> = store
            .db
            .query_row("SELECT metadata FROM items WHERE id='old'", [], |r| {
                r.get(0)
            })
            .unwrap();
        store
            .apply(Event::RawToolCapture(RawToolCapture::Start {
                source: key("", "old"),
                name: "Write".into(),
            }))
            .unwrap();
        let warnings = store
            .apply(Event::Item(Item {
                id: "old".into(),
                role: Role::Assistant,
                started_at: None,
                at: None,
                content: ItemContent::Tool {
                    name: "Write".into(),
                    input: String::new(),
                    output: String::new(),
                    status: crate::chat::ToolStatus::Running,
                },
            }))
            .unwrap();
        assert!(
            warnings
                .iter()
                .any(|event| matches!(event, Event::Error { fatal: false, .. }))
        );
        assert_eq!(
            store
                .db
                .query_row("SELECT metadata FROM items WHERE id='old'", [], |r| r
                    .get::<_, Vec<u8>>(0))
                .unwrap(),
            metadata
        );
        assert!(
            store.raw_original(&key("", "old")).is_err(),
            "identity ambiguity must never grant an old call as a new original"
        );
    }
}

#[cfg(test)]
mod write_budget_tests {
    use super::*;
    fn writes(store: &Store, reset: bool) -> usize {
        let (mut current, mut high) = (0, 0);
        let result = unsafe {
            rusqlite::ffi::sqlite3_db_status(
                store.db.handle(),
                rusqlite::ffi::SQLITE_DBSTATUS_CACHE_WRITE,
                &mut current,
                &mut high,
                i32::from(reset),
            )
        };
        assert_eq!(result, rusqlite::ffi::SQLITE_OK);
        current as usize
    }
    #[test]
    fn raw_capture_never_rewrites_old_prefix_pages_for_fifty_byte_fragments() {
        fn workload(prefix: usize) -> usize {
            let mut store = Store::new().unwrap();
            store.db.execute_batch("PRAGMA cache_size=-64").unwrap();
            store
                .apply(Event::RawToolCapture(RawToolCapture::Start {
                    source: key("", "stream"),
                    name: "Write".into(),
                }))
                .unwrap();
            store
                .apply(Event::Item(Item {
                    id: "stream".into(),
                    role: Role::Assistant,
                    started_at: None,
                    at: None,
                    content: ItemContent::Tool {
                        name: "Write".into(),
                        input: String::new(),
                        output: String::new(),
                        status: crate::chat::ToolStatus::Running,
                    },
                }))
                .unwrap();
            if prefix > 0 {
                store
                    .apply(Event::InputDelta {
                        id: "stream".into(),
                        offset: 0,
                        delta: "x".repeat(prefix),
                    })
                    .unwrap();
                store.flush_input().unwrap();
            }
            writes(&store, true);
            for index in 0..128 {
                store
                    .apply(Event::InputDelta {
                        id: "stream".into(),
                        offset: prefix + index * 50,
                        delta: "y".repeat(50),
                    })
                    .unwrap();
                store
                    .apply(Event::Status {
                        busy: true,
                        detail: String::new(),
                    })
                    .unwrap();
            }
            store.flush_input().unwrap();
            let result = writes(&store, false);
            assert!(result > 0);
            assert!(
                result < 128 * 16 + 64,
                "bounded real dirty-page writes, observed {result}"
            );
            result
        }
        let empty = workload(0);
        let large = workload(1024 * 1024);
        assert!(
            large <= empty + 64,
            "already committed megabyte prefix must not change append write cost: empty={empty}, large={large}"
        );
    }
    #[test]
    fn quota_failure_never_advertises_a_shortened_original_and_reset_recovers() {
        let mut store = Store::new().unwrap();
        store
            .db
            .execute("UPDATE raw_source_schema SET bytes=?1", [DISK_BYTES])
            .unwrap();
        let warnings = store
            .apply(Event::RawToolCapture(RawToolCapture::Full {
                source: key("", "refused"),
                name: "Write".into(),
                input: "null".into(),
                command: None,
            }))
            .unwrap();
        assert!(
            warnings
                .iter()
                .any(|event| matches!(event, Event::Error { fatal: false, .. }))
        );
        assert!(
            store
                .raw_source_page(RawSourceQuery {
                    source: key("", "refused"),
                    field: RawSourceField::ToolInput,
                    offset: 0,
                    limit: 256,
                    expected_epoch: None,
                    until: None
                })
                .is_err()
        );
        store
            .apply(Event::Reset {
                reason: "new conversation".into(),
            })
            .unwrap();
        store
            .apply(Event::RawToolCapture(RawToolCapture::Full {
                source: key("", "refused"),
                name: "Write".into(),
                input: "null".into(),
                command: None,
            }))
            .unwrap();
        assert_eq!(
            store.raw_original(&key("", "refused")).unwrap(),
            Some(("Write".into(), "null".into()))
        );
    }
}

#[cfg(test)]
mod mixed_history_tests {
    use super::*;
    fn ordinary(id: &str) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Text {
                text: "unrelated history".into(),
                thinking: false,
                phase: Phase::Final,
            },
        }
    }
    fn captured(store: &mut Store) {
        store
            .apply(Event::RawToolCapture(RawToolCapture::Full {
                source: key("", "private"),
                name: "Write".into(),
                input: "null".into(),
                command: None,
            }))
            .unwrap();
        store
            .apply(Event::Item(Item {
                id: "private".into(),
                role: Role::Assistant,
                started_at: None,
                at: None,
                content: ItemContent::Changes {
                    changes: Vec::new(),
                    status: crate::chat::ToolStatus::Succeeded,
                },
            }))
            .unwrap();
    }
    #[test]
    fn a_revoked_history_member_never_discards_unrelated_history_or_recovery_bookkeeping() {
        for earlier in [true, false] {
            let mut store = Store::new().unwrap();
            captured(&mut store);
            store
                .apply(Event::ItemsRemoved {
                    ids: vec!["private".into()],
                })
                .unwrap();
            store.pending_earlier = Some((0, 0));
            let items = vec![ordinary("private"), ordinary("valid")];
            let event = if earlier {
                Event::History {
                    items,
                    has_earlier: false,
                }
            } else {
                store.apply(Event::LatestHistoryStarted).unwrap();
                Event::LatestHistory {
                    items,
                    has_earlier: false,
                }
            };
            let out = store.apply(event).unwrap();
            assert!(
                out.iter()
                    .any(|event| matches!(event, Event::HistoryRecovered))
            );
            assert!(store.exists("valid").unwrap());
            assert!(!store.exists("private").unwrap());
            assert!(!store.provider_earlier);
            assert!(store.latest_boundary.is_none());
            assert!(store.pending_earlier.is_none());
        }
    }
    #[test]
    fn ambiguous_kind_change_filters_only_its_own_history_member() {
        let mut store = Store::new().unwrap();
        captured(&mut store);
        let out = store
            .apply(Event::LatestHistory {
                items: vec![ordinary("private"), ordinary("valid")],
                has_earlier: false,
            })
            .unwrap();
        assert!(
            out.iter()
                .any(|event| matches!(event, Event::HistoryRecovered))
        );
        assert!(store.exists("valid").unwrap());
        let metadata: Vec<u8> = store
            .db
            .query_row("SELECT metadata FROM items WHERE id='private'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let item: Item = postcard::from_bytes(&metadata).unwrap();
        assert!(matches!(item.content, ItemContent::Changes { .. }));
        assert!(store.raw_revoked(&key("", "private")).unwrap());
    }
}

#[cfg(test)]
mod pending_revocation_tests {
    use super::*;
    fn start(store: &mut Store, id: &str) {
        store
            .apply(Event::RawToolCapture(RawToolCapture::Start {
                source: key("", id),
                name: "Write".into(),
            }))
            .unwrap();
        store
            .apply(Event::Item(Item {
                id: id.into(),
                role: Role::Assistant,
                started_at: None,
                at: None,
                content: ItemContent::Tool {
                    name: "Write".into(),
                    input: String::new(),
                    output: String::new(),
                    status: crate::chat::ToolStatus::Running,
                },
            }))
            .unwrap();
    }
    #[test]
    fn removed_uncommitted_input_is_discarded_before_tombstone_without_poisoning_following_events()
    {
        let mut store = Store::new().unwrap();
        start(&mut store, "private");
        store
            .apply(Event::InputDelta {
                id: "private".into(),
                offset: 0,
                delta: "p".repeat(50),
            })
            .unwrap();
        assert!(store.pending_input.is_some());
        store
            .apply(Event::ItemsRemoved {
                ids: vec!["private".into()],
            })
            .unwrap();
        assert!(store.pending_input.is_none());
        assert!(!store.exists("private").unwrap());
        assert!(store.raw_original(&key("", "private")).is_err());
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM chunks WHERE id='private'", [], |r| {
                    r.get::<_, usize>(0)
                })
                .unwrap(),
            0
        );
        store
            .apply(Event::Status {
                busy: false,
                detail: "normal following event".into(),
            })
            .unwrap();
    }
    #[test]
    fn removing_one_key_preserves_unrelated_pending_input() {
        let mut store = Store::new().unwrap();
        start(&mut store, "removed");
        start(&mut store, "retained");
        store
            .apply(Event::InputDelta {
                id: "retained".into(),
                offset: 0,
                delta: "r".repeat(50),
            })
            .unwrap();
        store
            .apply(Event::ItemsRemoved {
                ids: vec!["removed".into()],
            })
            .unwrap();
        assert_eq!(
            store
                .db
                .query_row("SELECT len0 FROM items WHERE id='retained'", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            50
        );
        assert_eq!(store.field("retained", 0, 0, 50).unwrap(), vec![b'r'; 50]);
    }
}

#[cfg(test)]
mod ordinary_prose_tests {
    use super::*;
    fn prose(text: &str) -> Item {
        Item {
            id: "ordinary".into(),
            role: Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: Phase::Final,
            },
        }
    }
    #[test]
    fn raw_kind_detection_does_not_discard_uncaptured_prose_pending_before_history_overlap() {
        for latest in [true, false] {
            let mut store = Store::new().unwrap();
            store.apply(Event::Item(prose("already stored"))).unwrap();
            store
                .apply(Event::TextDelta {
                    id: "ordinary".into(),
                    role: Role::Assistant,
                    thinking: false,
                    delta: " plus new streamed bytes".into(),
                })
                .unwrap();
            assert!(store.pending_text.is_some());
            let items = vec![prose("already stored")];
            let event = if latest {
                Event::LatestHistory {
                    items,
                    has_earlier: false,
                }
            } else {
                Event::History {
                    items,
                    has_earlier: false,
                }
            };
            store.apply(event).unwrap();
            assert!(store.pending_text.is_none());
            let window = store.window().unwrap();
            assert!(
                matches!(&window.items[0].content,ItemContent::Text{text,..} if text=="already stored plus new streamed bytes")
            );
        }
    }
}

#[cfg(test)]
mod cache_fence_tests {
    use super::*;
    use serde_json::json;
    fn call(input: &str) -> serde_json::Value {
        json!({"type":"assistant","uuid":format!("carrier-{input}"),"message":{"id":format!("message-{input}"),"content":[{"type":"tool_use","id":"same","name":"Opaque","input":{"actual":input}}]}})
    }
    fn commit(
        store: &std::sync::Arc<std::sync::Mutex<Store>>,
        adapter: &mut dyn crate::chat::protocol::Adapter,
        step: crate::chat::protocol::Step,
    ) {
        for event in step.events {
            store.lock().unwrap().apply(event).unwrap();
        }
        adapter.committed();
    }
    #[test]
    fn full_carriers_preserve_streamed_identity_before_block_stop() {
        fn carrier(id: &str, input: serde_json::Value) -> serde_json::Value {
            json!({"type":"assistant","uuid":format!("carrier-{id}"),"message":{"id":"stream-message","content":[{"type":"tool_use","id":id,"name":"Opaque","input":input}]}})
        }
        fn query(id: &str) -> RawSourceQuery {
            RawSourceQuery {
                source: key("", id),
                field: RawSourceField::ToolInput,
                offset: 0,
                limit: PAGE_BYTES,
                expected_epoch: None,
                until: None,
            }
        }
        for (streamed, complete, matches) in [
            (r#"{ "z": 1, "actual": "first" }"#, true, false),
            (r#"{ "actual": "fir"#, false, false),
            (r#"{ "z": 1, "actual": "first" }"#, true, true),
        ] {
            let store = std::sync::Arc::new(std::sync::Mutex::new(Store::new().unwrap()));
            let mut adapter = crate::chat::protocol::claude::fixture_adapter();
            adapter.bind_item_reader(super::super::adapter_item_reader(&store));
            let initial = carrier("unrelated", json!({"actual":"unrelated"}));
            let step = adapter.receive(initial).unwrap();
            commit(&store, adapter.as_mut(), step);
            let stream = |event| json!({"type":"stream_event","event":event});
            for frame in [
                stream(json!({"type":"message_start","message":{"id":"stream-message"}})),
                stream(
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"same","name":"Opaque"}}),
                ),
                stream(
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":streamed}}),
                ),
            ] {
                let step = adapter.receive(frame).unwrap();
                commit(&store, adapter.as_mut(), step);
            }
            assert!(store.lock().unwrap().pending_input.is_some());
            // The native assistant carrier arrives before content_block_stop.
            // An incomplete prefix is not proof of a matching complete call.
            let input =
                json!({"actual":if matches || !complete {"first"} else {"different"},"z":1});
            let step = adapter.receive(carrier("same", input)).unwrap();
            // The adapter's canonical reader flushes pending stream input,
            // while the emitted Full/Item have not yet changed stored identity.
            let metadata: Vec<u8> = store
                .lock()
                .unwrap()
                .db
                .query_row("SELECT metadata FROM items WHERE id='same'", [], |r| {
                    r.get(0)
                })
                .unwrap();
            commit(&store, adapter.as_mut(), step);
            {
                let mut store = store.lock().unwrap();
                if matches {
                    let page = store.raw_source_page(query("same")).unwrap();
                    assert_eq!(page.text, streamed);
                    assert_eq!(page.metadata.representation, RawRepresentation::StreamJson);
                    assert!(page.complete && page.metadata.complete_call);
                } else {
                    assert!(store.raw_source_page(query("same")).is_err());
                    assert!(store.raw_revoked(&key("", "same")).unwrap());
                    let retained: Vec<u8> = store
                        .db
                        .query_row("SELECT metadata FROM items WHERE id='same'", [], |r| {
                            r.get(0)
                        })
                        .unwrap();
                    assert_eq!(retained, metadata);
                    assert_eq!(
                        store.field("same", 0, 0, streamed.len()).unwrap(),
                        streamed.as_bytes()
                    );
                }
                assert!(store.raw_sources.blocked.is_none());
            }
            let step = adapter
                .receive(stream(json!({"type":"content_block_stop","index":0})))
                .unwrap();
            commit(&store, adapter.as_mut(), step);
            {
                let mut store = store.lock().unwrap();
                assert!(store.raw_sources.blocked.is_none());
                let unrelated = store.raw_source_page(query("unrelated")).unwrap();
                assert_eq!(unrelated.text, json!({"actual":"unrelated"}).to_string());
                if matches {
                    let page = store.raw_source_page(query("same")).unwrap();
                    assert_eq!(page.text, streamed);
                    assert_eq!(page.metadata.representation, RawRepresentation::StreamJson);
                } else {
                    assert!(store.raw_original(&key("", "same")).is_err());
                    if complete {
                        assert_eq!(
                            store.field("same", 0, 0, streamed.len()).unwrap(),
                            streamed.as_bytes()
                        );
                    }
                }
            }
            let step = adapter
                .receive(carrier("fresh", json!({"actual":"fresh"})))
                .unwrap();
            commit(&store, adapter.as_mut(), step);
            let page = store
                .lock()
                .unwrap()
                .raw_source_page(query("fresh"))
                .unwrap();
            assert_eq!(page.text, json!({"actual":"fresh"}).to_string());
            assert_eq!(
                page.metadata.representation,
                RawRepresentation::CanonicalJson
            );
        }
    }
    #[test]
    fn a_resident_optional_tool_copy_cannot_bypass_canonical_privacy_revocation() {
        let store = std::sync::Arc::new(std::sync::Mutex::new(Store::new().unwrap()));
        let mut adapter = crate::chat::protocol::claude::fixture_adapter();
        adapter.bind_item_reader(super::super::adapter_item_reader(&store));
        let step = adapter.receive(call("first")).unwrap();
        commit(&store, adapter.as_mut(), step);
        // Keep the small copy resident on purpose; the authoritative removal
        // arrives through Store, independently of adapter cache housekeeping.
        store
            .lock()
            .unwrap()
            .apply(Event::ItemsRemoved {
                ids: vec!["same".into()],
            })
            .unwrap();
        let result=adapter.receive(json!({"type":"user","uuid":"late","message":{"content":[{"type":"tool_result","tool_use_id":"same","content":"done"}]}})).unwrap();
        assert!(
            result
                .events
                .iter()
                .any(|event| matches!(event, Event::Error { fatal: false, .. }))
        );
        assert!(
            !result
                .events
                .iter()
                .any(|event| matches!(event, Event::Item(_)))
        );
        let replay = adapter.receive(call("replayed")).unwrap();
        commit(&store, adapter.as_mut(), replay);
        assert!(!store.lock().unwrap().exists("same").unwrap());
        assert!(
            store
                .lock()
                .unwrap()
                .raw_original(&key("", "same"))
                .is_err()
        );
    }
    #[test]
    fn changed_full_call_identity_is_fenced_before_it_can_replace_canonical_history() {
        let store = std::sync::Arc::new(std::sync::Mutex::new(Store::new().unwrap()));
        let mut adapter = crate::chat::protocol::claude::fixture_adapter();
        adapter.bind_item_reader(super::super::adapter_item_reader(&store));
        let step = adapter.receive(call("first")).unwrap();
        commit(&store, adapter.as_mut(), step);
        let metadata: Vec<u8> = store
            .lock()
            .unwrap()
            .db
            .query_row("SELECT metadata FROM items WHERE id='same'", [], |r| {
                r.get(0)
            })
            .unwrap();
        let reused = adapter.receive(call("different")).unwrap();
        commit(&store, adapter.as_mut(), reused);
        assert_eq!(
            store
                .lock()
                .unwrap()
                .db
                .query_row("SELECT metadata FROM items WHERE id='same'", [], |r| r
                    .get::<_, Vec<u8>>(0))
                .unwrap(),
            metadata
        );
        let result=adapter.receive(json!({"type":"user","uuid":"ambiguous-result","message":{"content":[{"type":"tool_result","tool_use_id":"same","content":"done"}]}})).unwrap();
        assert!(
            !result
                .events
                .iter()
                .any(|event| matches!(event, Event::Item(_)))
        );
        assert!(
            result
                .events
                .iter()
                .any(|event| matches!(event, Event::Error { fatal: false, .. }))
        );
        assert!(
            store
                .lock()
                .unwrap()
                .window()
                .unwrap()
                .raw_source_parts
                .is_empty(),
            "ambiguous original must not be advertised"
        );
    }
}

#[cfg(test)]
mod descriptor_budget_tests {
    use super::*;
    #[test]
    fn every_normal_command_in_a_full_window_has_both_original_descriptors() {
        let mut store = Store::new().unwrap();
        for index in 0..WINDOW_ITEMS {
            let id = format!("command-{index}");
            let command = RawCommandCall {
                label: String::new(),
                command: "echo actual".into(),
                working_directory: "/tmp".into(),
                action: crate::chat::CommandAction::Run,
            };
            store
                .apply(Event::RawToolCapture(RawToolCapture::Full {
                    source: key("", &id),
                    name: "Bash".into(),
                    input: "{\"command\":\"echo actual\"}".into(),
                    command: Some(command.clone()),
                }))
                .unwrap();
            store
                .apply(Event::Item(Item {
                    id,
                    role: Role::Assistant,
                    started_at: None,
                    at: None,
                    content: ItemContent::Command {
                        label: command.label,
                        command: command.command,
                        working_directory: command.working_directory,
                        action: command.action,
                        status: crate::chat::ToolStatus::Succeeded,
                        exit_code: Some(0),
                        duration_ms: Some(1),
                        output: String::new(),
                    },
                }))
                .unwrap();
        }
        let window = store.window().unwrap();
        assert_eq!(window.items.len(), WINDOW_ITEMS);
        assert_eq!(window.raw_source_parts.len(), WINDOW_ITEMS * 2);
        for item in &window.items {
            for field in [RawSourceField::ToolInput, RawSourceField::CommandCall] {
                assert!(
                    window
                        .raw_source_parts
                        .iter()
                        .any(|part| part.item_id == item.id && part.field == field)
                );
            }
        }
    }
}
