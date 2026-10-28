//! Cache-only staging for peer inspection. These values grant no caller, channel,
//! path, actor or publication authority. The host must separately authorize and
//! revalidate its exact source, then retain publication custody through delivery.
use super::*;
use serde::{Deserialize, Serialize};
use std::{
    cell::Cell,
    io::{self, Write},
};

pub(crate) const PAGE_BYTES: usize = 128 * 1024;
pub(crate) const PAGE_PARTS: usize = 16;
const SCAN_ITEMS: usize = 128;
const BODY_BYTES: usize = 8 * 1024;
const TOOL_NAME_BYTES: usize = 256;
// Space for the host's source-bound cursor (<=4096), session/schema and matched
// response envelope. The host still validates its actual converted page/frame.
const CODEC_BYTES: usize = PAGE_BYTES - 8192;
const UNAVAILABLE: &str = "Transcript reset or source replacement requires a new reader";
const POISONED: &str =
    "Transcript read snapshot could not be released; cache reader is unavailable";

pub(super) struct State {
    incarnation: uuid::Uuid,
    reset_pending: bool,
    poisoned: Cell<bool>,
}
impl State {
    pub(super) fn guard_local(&self) -> Result<(), String> {
        if self.poisoned.get() {
            return Err(POISONED.into());
        }
        if self.reset_pending {
            return Err(UNAVAILABLE.into());
        }
        Ok(())
    }
    pub(super) fn guard(&self, db: &Connection) -> Result<(), String> {
        self.guard_local()?;
        let (incarnation, pending): (String, bool) = db
            .query_row(
                "SELECT incarnation,pending FROM peer_query_state WHERE singleton=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        if incarnation != self.incarnation.to_string() || pending {
            return Err(UNAVAILABLE.into());
        }
        Ok(())
    }
}
pub(super) fn initialize(db: &Connection) -> Result<State, String> {
    // Store::at creates a new private database; this is not an existing-cache
    // opener, migration, recovered actor grant or provider-history fallback.
    db.execute_batch("CREATE TABLE peer_query_state(singleton INTEGER PRIMARY KEY CHECK(singleton=1),incarnation TEXT NOT NULL,pending INTEGER NOT NULL CHECK(pending IN(0,1)));
        CREATE TABLE peer_query_items(id TEXT PRIMARY KEY REFERENCES items(id) ON DELETE CASCADE,token BLOB UNIQUE NOT NULL CHECK(length(token)=16));")
        .map_err(|e| e.to_string())?;
    let incarnation = uuid::Uuid::new_v4();
    db.execute(
        "INSERT INTO peer_query_state VALUES(1,?1,0)",
        [incarnation.to_string()],
    )
    .map_err(|e| e.to_string())?;
    Ok(State {
        incarnation,
        reset_pending: false,
        poisoned: Cell::new(false),
    })
}
pub(super) fn replace_item(db: &Connection, id: &str) -> Result<(), String> {
    // Only authoritative replacements rotate the token. Append transactions
    // preserve it; ordinal rebalancing never changes source identity.
    db.execute("INSERT INTO peer_query_items VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET token=excluded.token",
        params![id, uuid::Uuid::new_v4().as_bytes().as_slice()]).map_err(|e|e.to_string())?;
    Ok(())
}

/// Per-reader cache pin, including a successful initially empty page. It owns
/// neither a Store nor an actor and must be supplied again for every page.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Reader {
    incarnation: Option<uuid::Uuid>,
}
/// A value, never a read grant. Fields stay private to the codec. The host wraps
/// this in its own caller/target/Home/placement/source-bound continuation.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Cursor {
    cache: uuid::Uuid,
    item: uuid::Uuid,
    offset: usize,
    emitted: bool,
    skipped: bool,
}
impl std::fmt::Debug for Cursor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerCacheCursor")
            .field("offset", &self.offset)
            .finish_non_exhaustive()
    }
}
/// Codec-internal allowlist, explicitly converted to the host-owned wire DTO.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) enum Kind {
    UserText,
    AssistantText {
        phase: Phase,
    },
    ToolOutput {
        name: String,
        status: crate::chat::ToolStatus,
    },
    CommandOutput {
        status: crate::chat::ToolStatus,
        exit_code: Option<i32>,
    },
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Part {
    pub item: uuid::Uuid,
    pub kind: Kind,
    pub started_at: Option<u64>,
    pub at: Option<u64>,
    pub offset: usize,
    pub text: String,
    pub complete: bool,
}
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Page {
    pub incarnation: uuid::Uuid,
    pub parts: Vec<Part>,
    pub next: Option<Cursor>,
    /// End of the currently cached projection, not proof of provider history.
    pub at_end: bool,
    pub cached_only: bool,
    pub provider_has_earlier: bool,
    pub omitted_items: usize,
}

struct Lease<'a> {
    store: &'a Store,
    transaction: Option<rusqlite::Transaction<'a>>,
    finished: bool,
}
impl<'a> Lease<'a> {
    fn acquire(store: &'a Store) -> Result<Self, String> {
        if !store.db.is_autocommit() {
            store.peer_query.poisoned.set(true);
            return Err("Transcript read refused an unowned active transaction".into());
        }
        let transaction = store
            .db
            .unchecked_transaction()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            store,
            transaction: Some(transaction),
            finished: false,
        })
    }
    fn finish(&mut self) -> Result<(), String> {
        self.finished = true;
        let result = self.transaction.take().ok_or(POISONED)?.rollback();
        if result.is_err()
            || !self.store.db.is_autocommit()
            || !matches!(
                self.store.db.transaction_state::<&str>(None),
                Ok(rusqlite::TransactionState::None)
            )
        {
            self.store.peer_query.poisoned.set(true);
            return Err(POISONED.into());
        }
        Ok(())
    }
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if !self.finished {
            let _ = self.finish();
        }
    }
}
impl Store {
    /// Stages allowlisted cache bytes. No UI cursor, snapshot, provider command,
    /// subscription, ACK, history load, cache path or Raw-source grant is used.
    pub(crate) fn peer_page(
        &mut self,
        reader: &mut Reader,
        after: Option<Cursor>,
    ) -> Result<Page, String> {
        self.peer_query.guard(&self.db)?;
        if reader
            .incarnation
            .is_some_and(|pin| pin != self.peer_query.incarnation)
            || after
                .as_ref()
                .is_some_and(|c| c.cache != self.peer_query.incarnation)
        {
            return Err(UNAVAILABLE.into());
        }
        // Capture the empty cache too. No subsequent Reset can revive this reader.
        reader.incarnation = Some(self.peer_query.incarnation);
        // flush_text checks the reset gate before taking its pending private
        // body; append checks it again inside the same body write transaction.
        self.flush_text()?;
        self.with_peer_snapshot(|store| store.peer_source_page(after))
    }
    fn with_peer_snapshot<T>(
        &self,
        read: impl FnOnce(&Self) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut lease = Lease::acquire(self)?;
        let result = self.peer_query.guard(&self.db).and_then(|()| read(self));
        // Even successful private bytes must not escape a failed explicit release.
        lease.finish()?;
        // Releasing the owned SQLite snapshot is necessary but not freshness
        // proof: an independent WAL writer may have reset during preparation.
        self.peer_query.guard(&self.db)?;
        result
    }
    fn peer_source_page(&self, after: Option<Cursor>) -> Result<Page, String> {
        let first = match &after {
            Some(c) => self.db.query_row("SELECT i.ordinal FROM items i JOIN peer_query_items p USING(id) WHERE p.token=?1",
                [c.item.as_bytes().as_slice()], |r| r.get::<_,i64>(0)).optional().map_err(|e|e.to_string())?
                .ok_or("Transcript anchor was replaced or removed")?,
            None => i64::MIN,
        };
        let mut page = Page {
            incarnation: self.peer_query.incarnation,
            parts: Vec::new(),
            next: after.clone(),
            at_end: false,
            cached_only: true,
            provider_has_earlier: self.provider_earlier,
            omitted_items: 0,
        };
        let mut query = self.db.prepare("SELECT CASE WHEN length(CAST(i.id AS BLOB))<=65536 THEN i.id ELSE NULL END,i.ordinal,CASE WHEN length(i.metadata)<=65536 THEN i.metadata ELSE NULL END,i.len0,i.len1,p.token FROM items i LEFT JOIN peer_query_items p USING(id) WHERE i.ordinal>=?1 ORDER BY i.ordinal LIMIT 128").map_err(|e|e.to_string())?;
        let mut rows = query.query([first]).map_err(|e| e.to_string())?;
        let mut last_ordinal = None;
        let mut unfinished = false;
        let mut scanned = 0;
        'items: while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            scanned += 1;
            debug_assert!(scanned <= SCAN_ITEMS);
            let id: String = row.get(0).map_err(|e| e.to_string())?;
            let ordinal: i64 = row.get(1).map_err(|e| e.to_string())?;
            let metadata: Option<Vec<u8>> = row.get(2).map_err(|e| e.to_string())?;
            let metadata = metadata.ok_or("Transcript metadata exceeds the codec bound")?;
            let item: Item = postcard::from_bytes(&metadata)
                .map_err(|_| "Transcript metadata is unavailable")?;
            let token: Vec<u8> = row.get(5).map_err(|e| e.to_string())?;
            let token = uuid::Uuid::from_slice(&token)
                .map_err(|_| "Transcript item token is unavailable")?;
            let cursor = after.as_ref().filter(|c| c.item == token);
            let mut next = Cursor {
                cache: self.peer_query.incarnation,
                item: token,
                offset: 0,
                emitted: false,
                skipped: false,
            };
            let Some(projection) = Projection::from_item(item)? else {
                next.skipped = true;
                next.emitted = true;
                page.omitted_items += 1;
                page.next = Some(next);
                last_ordinal = Some(ordinal);
                continue;
            };
            if cursor.is_some_and(|c| c.skipped) {
                return Err("Transcript projection changed under its cursor".into());
            }
            let total = match &projection.body {
                Body::Field(field) => {
                    usize::try_from(row.get::<_, i64>(3 + *field).map_err(|e| e.to_string())?)
                        .map_err(|_| "Invalid transcript body length")?
                }
            };
            let mut offset = cursor.map_or(0, |c| c.offset);
            if offset > total {
                return Err("Transcript cursor exceeds its body".into());
            }
            let mut emitted = cursor.is_some_and(|c| c.emitted);
            while offset < total || !emitted {
                let (text, end) = self.peer_body(&id, &projection.body, offset, total)?;
                next.offset = end;
                next.emitted = true;
                let previous = page.next.clone();
                page.parts.push(Part {
                    item: token,
                    kind: projection.kind.clone(),
                    started_at: projection.started_at,
                    at: projection.at,
                    offset,
                    text,
                    complete: end == total,
                });
                page.next = Some(next.clone());
                if page.encoded_bytes().is_err() {
                    page.parts.pop();
                    page.next = previous;
                    unfinished = true;
                    break 'items;
                }
                offset = end;
                emitted = true;
                last_ordinal = Some(ordinal);
                if page.parts.len() == PAGE_PARTS {
                    unfinished = offset < total;
                    break 'items;
                }
            }
            if cursor.is_none() || offset > cursor.map_or(0, |c| c.offset) {
                page.next = Some(Cursor {
                    offset,
                    emitted,
                    ..next
                });
            }
            last_ordinal = Some(ordinal);
        }
        drop(rows);
        drop(query);
        let later = match last_ordinal {
            Some(ordinal) => self
                .db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM items WHERE ordinal>?1)",
                    [ordinal],
                    |r| r.get::<_, bool>(0),
                )
                .map_err(|e| e.to_string())?,
            None => false,
        };
        page.at_end = !unfinished && !later;
        page.validate_codec()?;
        Ok(page)
    }
    fn peer_body(
        &self,
        id: &str,
        body: &Body,
        start: usize,
        total: usize,
    ) -> Result<(String, usize), String> {
        let target = start.saturating_add(BODY_BYTES).min(total);
        let mut bytes = match body {
            Body::Field(field) => self.field(
                id,
                *field as i64,
                start,
                target.saturating_add(3).min(total),
            )?,
        };
        if bytes.len() != target.saturating_add(3).min(total) - start
            || bytes.first().is_some_and(|b| b & 0xc0 == 0x80)
        {
            return Err("Transcript body range is unavailable".into());
        }
        let mut keep = target - start;
        while keep > 0 && keep < bytes.len() && bytes[keep] & 0xc0 == 0x80 {
            keep -= 1;
        }
        bytes.truncate(keep);
        let text = String::from_utf8(bytes).map_err(|_| "Transcript body is not UTF-8")?;
        if text.is_empty() && start < total {
            return Err("Transcript cursor made no progress".into());
        }
        Ok((text, start + keep))
    }
    pub(super) fn reset_peer_cache(&mut self, reason: String) -> Result<Vec<Event>, String> {
        // Revoke before any fallible write or pending-text flush. This is a
        // private cache namespace, not the canonical model or actor journal.
        self.peer_query.reset_pending = true;
        if self.peer_query.poisoned.get() || !self.db.is_autocommit() {
            return Err(POISONED.into());
        }
        let old = self.peer_query.incarnation.to_string();
        let next = uuid::Uuid::new_v4();
        let synchronous: i64 = self
            .db
            .pragma_query_value(None, "synchronous", |r| r.get(0))
            .map_err(|e| e.to_string())?;
        self.db
            .pragma_update(None, "synchronous", 3)
            .map_err(|e| e.to_string())?;
        let established: i64 = self
            .db
            .pragma_query_value(None, "synchronous", |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if established != 3 {
            return Err("Transcript reset durability was refused".into());
        }
        let reset = (|| {
            let changed = self
                .db
                .execute(
                    "UPDATE peer_query_state SET pending=1 WHERE singleton=1 AND incarnation=?1",
                    [&old],
                )
                .map_err(|e| e.to_string())?;
            if changed != 1 {
                return Err(UNAVAILABLE.into());
            }
            let tx = self
                .db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| e.to_string())?;
            let owner: String = tx
                .query_row(
                    "SELECT incarnation FROM peer_query_state WHERE singleton=1 AND pending=1",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            if owner != old {
                return Err(UNAVAILABLE.into());
            }
            tx.execute_batch("DELETE FROM file_edits; DELETE FROM file_totals; DELETE FROM file_paths; DELETE FROM copy_sources; DELETE FROM items;").map_err(|e|e.to_string())?;
            tx.execute(
                "UPDATE peer_query_state SET incarnation=?1,pending=0 WHERE singleton=1",
                [next.to_string()],
            )
            .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
            self.peer_query.incarnation = next;
            Ok::<_, String>(())
        })();
        let restore = self
            .db
            .pragma_update(None, "synchronous", synchronous)
            .map_err(|e| e.to_string());
        reset?;
        restore?;
        let actual: i64 = self
            .db
            .pragma_query_value(None, "synchronous", |r| r.get(0))
            .map_err(|e| e.to_string())?;
        if actual != synchronous {
            return Err("Transcript cache synchronization policy could not be restored".into());
        }
        self.peer_query.reset_pending = false;
        self.pending_text = None;
        self.file_revision = self.file_revision.saturating_add(1);
        self.child_files_seen = false;
        self.file_index_incomplete = false;
        self.file_keys = 0;
        self.applied_files = 0;
        self.copy_cache = uuid::Uuid::new_v4();
        self.copy_revision = self.copy_revision.wrapping_add(1);
        self.copy_projection.clear();
        self.end = None;
        self.first = None;
        self.newest = None;
        self.forward = None;
        self.paging = false;
        self.provider_earlier = false;
        self.cached_earlier = false;
        self.pending_earlier = None;
        self.latest_boundary = None;
        self.dirty = true;
        Ok(vec![Event::Reset { reason }])
    }
}

enum Body {
    Field(usize),
}
struct Projection {
    kind: Kind,
    started_at: Option<u64>,
    at: Option<u64>,
    body: Body,
}
impl Projection {
    fn from_item(item: Item) -> Result<Option<Self>, String> {
        let (kind, body) = match (item.role, item.content) {
            (
                Role::User,
                ItemContent::Text {
                    thinking: false, ..
                },
            ) => (Kind::UserText, Body::Field(0)),
            (
                Role::Assistant,
                ItemContent::Text {
                    thinking: false,
                    phase,
                    ..
                },
            ) => (Kind::AssistantText { phase }, Body::Field(0)),
            (Role::User | Role::Assistant, ItemContent::Tool { name, status, .. }) => {
                if name.len() > TOOL_NAME_BYTES || name.chars().any(char::is_control) {
                    return Err("Peer tool name exceeds its safe metadata bound".into());
                }
                (Kind::ToolOutput { name, status }, Body::Field(1))
            }
            (
                Role::User | Role::Assistant,
                ItemContent::Command {
                    status, exit_code, ..
                },
            ) => (Kind::CommandOutput { status, exit_code }, Body::Field(0)),
            _ => return Ok(None),
        };
        Ok(Some(Self {
            kind,
            started_at: item.started_at,
            at: item.at,
            body,
        }))
    }
}
impl Reader {
    /// Installs a value returned by the authenticated matched actor only after
    /// complete response validation. The pin is not caller or cache authority.
    pub(crate) fn accept_returned(&mut self, returned: Self, page: &Page) -> Result<(), String> {
        page.validate_codec()?;
        if returned.incarnation != Some(page.incarnation)
            || self
                .incarnation
                .is_some_and(|old| Some(old) != returned.incarnation)
        {
            return Err("Retained transcript response changed its reader pin".into());
        }
        *self = returned;
        Ok(())
    }
}
impl Page {
    pub(crate) fn validate_codec(&self) -> Result<(), String> {
        if self.incarnation.is_nil()
            || !self.cached_only
            || self.parts.len() > PAGE_PARTS
            || self.omitted_items > SCAN_ITEMS
            || (!self.at_end && self.next.is_none())
            || self
                .next
                .as_ref()
                .is_some_and(|cursor| cursor.cache != self.incarnation || cursor.item.is_nil())
        {
            return Err("Invalid transcript cache page".into());
        }
        for part in &self.parts {
            if part.item.is_nil()
                || part.text.len() > BODY_BYTES
                || part.offset.checked_add(part.text.len()).is_none()
                || (part.text.is_empty() && !part.complete)
            {
                return Err("Invalid transcript cache part".into());
            }
            if let Kind::ToolOutput { name, .. } = &part.kind
                && (name.len() > TOOL_NAME_BYTES || name.chars().any(char::is_control))
            {
                return Err("Invalid transcript cache tool name".into());
            }
        }
        self.encoded_bytes()?;
        Ok(())
    }
    fn encoded_bytes(&self) -> Result<usize, String> {
        let mut json = Counter(0);
        serde_json::to_writer(&mut json, self)
            .map_err(|_| "Peer page exceeds its encoded byte bound")?;
        let mut wire = Counter(0);
        postcard::to_io(self, &mut wire).map_err(|_| "Peer page exceeds its encoded byte bound")?;
        Ok(json.0.max(wire.0))
    }
}
struct Counter(usize);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .filter(|n| *n <= CODEC_BYTES)
            .ok_or_else(|| io::Error::other("peer page byte bound"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prose(id: &str, text: &str) -> Item {
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
    fn contents(page: &Page) -> String {
        page.parts.iter().map(|p| p.text.as_str()).collect()
    }
    fn cursor(page: &Page) -> Cursor {
        page.next.clone().unwrap()
    }
    fn window_state(
        store: &Store,
    ) -> (
        Option<super::super::Cursor>,
        Option<super::super::Cursor>,
        Option<super::super::Cursor>,
        Option<super::super::Cursor>,
        bool,
        bool,
        bool,
    ) {
        (
            store.end,
            store.first,
            store.newest,
            store.forward,
            store.paging,
            store.cached_earlier,
            store.provider_earlier,
        )
    }

    #[test]
    fn peer_pages_bound_encoded_tools_and_unicode_without_exposing_private_fields_or_moving_ui() {
        let mut store = Store::new().unwrap();
        let text = "a\\\"\n🦀".repeat(30_000);
        store.apply(Event::Item(prose("text", &text))).unwrap();
        store
            .apply(Event::Item(Item {
                id: "tool".into(),
                role: Role::Assistant,
                at: None,
                started_at: None,
                content: ItemContent::Tool {
                    name: "Read fixture".into(),
                    input: "RAW-INPUT-DO-NOT-EXPORT".into(),
                    output: "tool result".into(),
                    status: crate::chat::ToolStatus::Succeeded,
                },
            }))
            .unwrap();
        store
            .apply(Event::Item(Item {
                id: "command".into(),
                role: Role::Assistant,
                at: None,
                started_at: None,
                content: ItemContent::Command {
                    label: "PRIVATE-LABEL".into(),
                    command: "PRIVATE-ARGV".into(),
                    working_directory: "/PRIVATE-CWD".into(),
                    action: crate::chat::CommandAction::Run,
                    status: crate::chat::ToolStatus::Succeeded,
                    exit_code: Some(0),
                    duration_ms: Some(1),
                    output: "command result".into(),
                },
            }))
            .unwrap();
        store
            .apply(Event::Item(Item {
                id: "image".into(),
                role: Role::Assistant,
                at: None,
                started_at: None,
                content: ItemContent::Image {
                    path: "/PRIVATE-ASSET".into(),
                    description: "image description".into(),
                    reference: None,
                },
            }))
            .unwrap();
        let mut secret = prose("thinking", "PRIVATE-THINKING");
        secret.content = ItemContent::Text {
            text: "PRIVATE-THINKING".into(),
            thinking: true,
            phase: Phase::Final,
        };
        store.apply(Event::Item(secret)).unwrap();
        let mut system = prose("system", "PRIVATE-SYSTEM");
        system.role = Role::System;
        store.apply(Event::Item(system)).unwrap();
        store
            .apply(Event::Item(Item {
                id: "delivery".into(),
                role: Role::User,
                at: None,
                started_at: None,
                content: ItemContent::Delivery {
                    kind: crate::chat::DeliveryKind::Message,
                    from: "PRIVATE-SENDER".into(),
                    sender: "PRIVATE-AUTHORITY".into(),
                    summary: "PRIVATE-SUMMARY".into(),
                    body: "PRIVATE-DELIVERY".into(),
                },
            }))
            .unwrap();
        store
            .apply(Event::Item(Item {
                id: "search".into(),
                role: Role::Assistant,
                at: None,
                started_at: None,
                content: ItemContent::WebSearch {
                    query: "PRIVATE-QUERY".into(),
                    results: vec![],
                },
            }))
            .unwrap();
        let before = window_state(&store);
        let copy_cache = store.copy_cache;
        let mut reader = Reader::default();
        let mut after = None;
        let mut result = String::new();
        let mut encoded = String::new();
        let mut calls = 0;
        loop {
            let page = store.peer_page(&mut reader, after.clone()).unwrap();
            let json = serde_json::to_string(&page).unwrap();
            assert!(json.len() <= CODEC_BYTES);
            assert!(postcard::to_stdvec(&page).unwrap().len() <= CODEC_BYTES);
            assert!(page.parts.len() <= PAGE_PARTS);
            assert!(page.cached_only);
            assert!(page.parts.iter().all(|p| p.text.len() <= BODY_BYTES));
            result.push_str(&contents(&page));
            encoded.push_str(&json);
            calls += 1;
            if page.at_end {
                break;
            }
            assert_ne!(page.next, after, "every partial page must advance");
            after = page.next;
            assert!(calls < 100);
        }
        assert_eq!(result, format!("{text}tool resultcommand result"));
        assert!(calls > 1);
        for private in [
            "RAW-INPUT-DO-NOT-EXPORT",
            "PRIVATE-ARGV",
            "PRIVATE-LABEL",
            "PRIVATE-CWD",
            "PRIVATE-ASSET",
            "PRIVATE-THINKING",
            "PRIVATE-SYSTEM",
            "image description",
            "PRIVATE-SENDER",
            "PRIVATE-AUTHORITY",
            "PRIVATE-SUMMARY",
            "PRIVATE-DELIVERY",
            "PRIVATE-QUERY",
        ] {
            assert!(!encoded.contains(private), "{private}");
        }
        assert_eq!(window_state(&store), before);
        assert_eq!(store.copy_cache, copy_cache);
        assert!(store.db.is_autocommit());
    }

    #[test]
    fn peer_cursor_continues_appends_and_rebalance_but_refuses_rewrites_removed_anchors_and_cache_reuse()
     {
        let mut placeholder = Store::new().unwrap();
        placeholder.apply(Event::Item(prose("empty", ""))).unwrap();
        let mut empty_reader = Reader::default();
        let empty = placeholder.peer_page(&mut empty_reader, None).unwrap();
        assert_eq!(empty.parts.len(), 1);
        assert!(empty.parts[0].text.is_empty());
        placeholder
            .apply(Event::TextDelta {
                id: "empty".into(),
                role: Role::Assistant,
                thinking: false,
                delta: "🦀first bytes".into(),
            })
            .unwrap();
        assert_eq!(
            contents(
                &placeholder
                    .peer_page(&mut empty_reader, empty.next)
                    .unwrap()
            ),
            "🦀first bytes"
        );
        let mut store = Store::new().unwrap();
        store.apply(Event::Item(prose("a", "first"))).unwrap();
        let mut reader = Reader::default();
        let first = store.peer_page(&mut reader, None).unwrap();
        assert_eq!(contents(&first), "first");
        let after = cursor(&first);
        store
            .apply(Event::TextDelta {
                id: "a".into(),
                role: Role::Assistant,
                thinking: false,
                delta: "🦀tail".into(),
            })
            .unwrap();
        store.rebalance().unwrap();
        let appended = store.peer_page(&mut reader, Some(after.clone())).unwrap();
        assert_eq!(contents(&appended), "🦀tail");
        let unchanged = store.peer_page(&mut reader, appended.next.clone()).unwrap();
        assert!(unchanged.parts.is_empty());
        assert!(unchanged.at_end);
        store.apply(Event::Item(prose("a", "replacement"))).unwrap();
        assert!(
            store
                .peer_page(&mut reader, Some(after))
                .unwrap_err()
                .contains("anchor")
        );
        let current = store.peer_page(&mut reader, None).unwrap();
        store
            .apply(Event::ItemsRemoved {
                ids: vec!["a".into()],
            })
            .unwrap();
        assert!(store.peer_page(&mut reader, current.next).is_err());
        let mut other = Store::new().unwrap();
        other.apply(Event::Item(prose("a", "replacement"))).unwrap();
        assert!(other.peer_page(&mut reader, None).is_err());
    }

    #[test]
    fn peer_empty_reader_and_cursor_are_revoked_by_reset_before_pending_text_flush() {
        let mut store = Store::new().unwrap();
        let mut empty = Reader::default();
        assert!(store.peer_page(&mut empty, None).unwrap().at_end);
        store.apply(Event::Item(prose("same", "old"))).unwrap();
        let mut reader = Reader::default();
        let old = store.peer_page(&mut reader, None).unwrap();
        store
            .apply(Event::TextDelta {
                id: "same".into(),
                role: Role::Assistant,
                thinking: false,
                delta: "private pending".into(),
            })
            .unwrap();
        assert!(store.pending_text.is_some());
        store
            .apply(Event::Reset {
                reason: "new conversation".into(),
            })
            .unwrap();
        assert!(store.pending_text.is_none());
        store.apply(Event::Item(prose("same", "new"))).unwrap();
        assert!(store.peer_page(&mut empty, None).is_err());
        assert!(store.peer_page(&mut reader, old.next).is_err());
        let mut current = Reader::default();
        assert_eq!(
            contents(&store.peer_page(&mut current, None).unwrap()),
            "new"
        );
    }

    #[test]
    fn peer_excluded_item_scan_is_finite_and_progresses_without_fabricating_visible_content() {
        let mut store = Store::new().unwrap();
        for i in 0..(SCAN_ITEMS + 3) {
            let mut item = prose(&format!("system-{i}"), "not visible");
            item.role = Role::System;
            store.apply(Event::Item(item)).unwrap();
        }
        store.apply(Event::Item(prose("visible", "tail"))).unwrap();
        let mut reader = Reader::default();
        let first = store.peer_page(&mut reader, None).unwrap();
        assert!(first.parts.is_empty());
        assert!(!first.at_end);
        assert_eq!(first.omitted_items, SCAN_ITEMS);
        let second = store.peer_page(&mut reader, first.next).unwrap();
        assert_eq!(contents(&second), "tail");
        assert!(second.at_end);
    }

    unsafe extern "C" fn deny_delete(
        _: *mut std::ffi::c_void,
        action: i32,
        first: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
    ) -> i32 {
        if action == rusqlite::ffi::SQLITE_DELETE
            && !first.is_null()
            && unsafe { std::ffi::CStr::from_ptr(first) }.to_bytes() == b"items"
        {
            rusqlite::ffi::SQLITE_DENY
        } else {
            rusqlite::ffi::SQLITE_OK
        }
    }
    unsafe extern "C" fn deny_rollback(
        _: *mut std::ffi::c_void,
        action: i32,
        first: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
        _: *const std::ffi::c_char,
    ) -> i32 {
        if action == rusqlite::ffi::SQLITE_TRANSACTION
            && !first.is_null()
            && unsafe { std::ffi::CStr::from_ptr(first) }.to_bytes() == b"ROLLBACK"
        {
            rusqlite::ffi::SQLITE_DENY
        } else {
            rusqlite::ffi::SQLITE_OK
        }
    }
    fn authorizer(
        store: &Store,
        callback: Option<
            unsafe extern "C" fn(
                *mut std::ffi::c_void,
                i32,
                *const std::ffi::c_char,
                *const std::ffi::c_char,
                *const std::ffi::c_char,
                *const std::ffi::c_char,
            ) -> i32,
        >,
    ) {
        // SAFETY: callbacks are static, never dereference context, and this
        // thread owns the Connection for their complete installed lifetime.
        assert_eq!(
            unsafe {
                rusqlite::ffi::sqlite3_set_authorizer(
                    store.db.handle(),
                    callback,
                    std::ptr::null_mut(),
                )
            },
            rusqlite::ffi::SQLITE_OK
        );
    }
    #[test]
    fn failed_peer_reset_refuses_pending_flush_and_keeps_old_copy_and_file_rows_until_atomic_retry()
    {
        let mut store = Store::new().unwrap();
        store.apply(Event::Item(prose("same", "original"))).unwrap();
        let mut reader = Reader::default();
        let old = store.peer_page(&mut reader, None).unwrap();

        let changes = Item {
            id: "changes".into(),
            role: Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Changes {
                changes: vec![crate::chat::FileChange {
                    path: "fixture.rs".into(),
                    kind: crate::chat::ChangeKind::Updated,
                    diff: "@@ -1 +1 @@\n-old\n+new\n".repeat(20_000),
                }],
                status: crate::chat::ToolStatus::Succeeded,
            },
        };
        store.apply(Event::Item(changes.clone())).unwrap();
        let window = store.window().unwrap();
        let index = window
            .items
            .iter()
            .position(|item| item.id == "same")
            .unwrap();
        let source = store.copy_projection[index].clone().unwrap();
        assert_eq!(store.text_copy(&source, 0).unwrap().text, "original");
        let read_files = crate::chat::Command::LoadFileChanges {
            request_id: 1,
            after: None,
        };
        let Some(crate::chat::FileChangeReply::Files(files)) =
            store.file_query(&read_files).unwrap()
        else {
            panic!("real file summary")
        };
        let file = files.files.first().unwrap();
        let key = file.key.clone();
        let Some(crate::chat::FileChangeReply::Patches(first_patch)) = store
            .file_query(&crate::chat::Command::LoadFileChangePatches {
                request_id: 2,
                key: key.clone(),
                cursor: None,
                revision: file.revision,
            })
            .unwrap()
        else {
            panic!("real patches")
        };
        assert!(first_patch.error.is_none());
        assert!(!first_patch.patches.is_empty());
        let old_cursor = first_patch
            .next
            .clone()
            .expect("large real patch has continuation");
        let read_patches = crate::chat::Command::LoadFileChangePatches {
            request_id: 3,
            key: key.clone(),
            cursor: Some(old_cursor.clone()),
            revision: first_patch.revision,
        };
        let Some(crate::chat::FileChangeReply::Patches(continued)) =
            store.file_query(&read_patches).unwrap()
        else {
            panic!("real continuation")
        };
        assert!(continued.error.is_none() && !continued.patches.is_empty());
        let mut published = crate::chat::Snapshot::default();
        store.publish_files(&mut published).unwrap();
        store.publish_copy_sources(&mut published);
        assert_eq!(published.file_changes.revision, store.file_revision);
        assert!(!published.file_changes.files.is_empty());
        assert!(
            published
                .text_copy_sources
                .conversation
                .iter()
                .any(Option::is_some)
        );
        // Install pending text AFTER the non-text event's ordinary flush.
        store
            .apply(Event::TextDelta {
                id: "same".into(),
                role: Role::Assistant,
                thinking: false,
                delta: "private pending".into(),
            })
            .unwrap();
        let counts = |store: &Store| {
            ["copy_sources", "file_edits", "file_totals", "file_paths"].map(|table| {
                store
                    .db
                    .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .unwrap()
            })
        };
        let rows = counts(&store);
        assert!(rows.iter().all(|n| *n > 0));
        authorizer(&store, Some(deny_delete));
        assert!(
            store
                .apply(Event::Reset {
                    reason: "reset".into()
                })
                .is_err()
        );
        authorizer(&store, None);
        assert_eq!(counts(&store), rows);
        assert!(store.text_copy(&source, 0).is_err());
        assert!(store.file_query(&read_files).is_err());
        assert!(store.file_query(&read_patches).is_err());
        assert!(
            store
                .file_query(&crate::chat::Command::Refresh)
                .unwrap()
                .is_none(),
            "unrelated commands retain dispatch"
        );
        // Same revision/coverage used to bypass publication entirely.
        assert!(store.publish_files(&mut published).is_err());
        assert!(published.file_changes.files.is_empty());
        store.publish_copy_sources(&mut published);
        assert!(published.text_copy_sources.conversation.is_empty());
        assert!(store.peer_page(&mut reader, old.next).is_err());
        assert!(store.flush_text().is_err());
        assert_eq!(store.pending_text.as_ref().unwrap().3, "private pending");
        assert_eq!(
            store
                .db
                .query_row("SELECT len0 FROM items WHERE id='same'", [], |r| r
                    .get::<_, i64>(0))
                .unwrap(),
            8
        );
        let pending: bool = store
            .db
            .query_row("SELECT pending FROM peer_query_state", [], |r| r.get(0))
            .unwrap();
        assert!(pending);
        store
            .apply(Event::Reset {
                reason: "retry".into(),
            })
            .unwrap();
        assert!(store.pending_text.is_none());
        assert_eq!(counts(&store), [0; 4]);
        let mut current = Reader::default();
        assert!(
            store
                .peer_page(&mut current, None)
                .unwrap()
                .parts
                .is_empty()
        );
        assert!(store.peer_page(&mut reader, None).is_err());
        store
            .apply(Event::Item(prose("same", "new after reset")))
            .unwrap();
        let mut changed = changes;
        if let ItemContent::Changes { changes, .. } = &mut changed.content {
            changes[0].diff = "@@ -1 +1 @@\n-old\n+new after reset\n".into();
        }
        store.apply(Event::Item(changed)).unwrap();
        let window = store.window().unwrap();
        let index = window
            .items
            .iter()
            .position(|item| item.id == "same")
            .unwrap();
        let new_source = store.copy_projection[index].clone().unwrap();
        assert_eq!(
            store.text_copy(&new_source, 0).unwrap().text,
            "new after reset"
        );
        assert!(store.text_copy(&source, 0).is_err());
        let Some(crate::chat::FileChangeReply::Files(new_files)) =
            store.file_query(&read_files).unwrap()
        else {
            panic!("new real summary")
        };
        let current_file = new_files.files.first().unwrap();
        let Some(crate::chat::FileChangeReply::Patches(new_patch)) = store
            .file_query(&crate::chat::Command::LoadFileChangePatches {
                request_id: 4,
                key: current_file.key.clone(),
                cursor: None,
                revision: current_file.revision,
            })
            .unwrap()
        else {
            panic!("new real patches")
        };
        assert!(
            new_patch
                .patches
                .iter()
                .any(|patch| patch.diff.contains("new after reset"))
        );
        let Some(crate::chat::FileChangeReply::Patches(stale)) =
            store.file_query(&read_patches).unwrap()
        else {
            panic!("old cursor refusal")
        };
        assert!(stale.error.is_some() && stale.patches.is_empty());
        store.publish_files(&mut published).unwrap();
        store.publish_copy_sources(&mut published);
        assert!(!published.file_changes.files.is_empty());
        assert!(
            published
                .text_copy_sources
                .conversation
                .iter()
                .any(Option::is_some)
        );
        let page = store.peer_page(&mut current, None).unwrap();
        let encoded = postcard::to_stdvec(&(current.clone(), page)).unwrap();
        let (returned, page): (Reader, Page) = postcard::from_bytes(&encoded).unwrap();
        let mut retained = Reader::default();
        retained.accept_returned(returned, &page).unwrap();
        assert!(retained.accept_returned(Reader::default(), &page).is_err());
        let json = serde_json::to_string(&(retained.clone(), &page)).unwrap();
        let (returned, page): (Reader, Page) = serde_json::from_str(&json).unwrap();
        retained.accept_returned(returned, &page).unwrap();
    }

    #[test]
    fn peer_explicit_release_failure_discards_staged_bytes_and_permanently_poisons_connection() {
        let mut store = Store::new().unwrap();
        store
            .apply(Event::Item(prose("a", "must not escape")))
            .unwrap();
        let window = store.window().unwrap();
        let source = store.copy_projection[window.items.iter().position(|i| i.id == "a").unwrap()]
            .clone()
            .unwrap();
        assert_eq!(store.text_copy(&source, 0).unwrap().text, "must not escape");
        let mut reader = Reader::default();
        authorizer(&store, Some(deny_rollback));
        assert_eq!(store.peer_page(&mut reader, None).unwrap_err(), POISONED);
        authorizer(&store, None);
        if !store.db.is_autocommit() {
            store.db.execute_batch("ROLLBACK").unwrap();
        }
        assert_eq!(store.peer_page(&mut reader, None).unwrap_err(), POISONED);
        assert!(store.flush_text().is_err());
        assert!(store.text_copy(&source, 0).is_err());
        let mut state = crate::chat::Snapshot::default();
        store.publish_copy_sources(&mut state);
        assert!(state.text_copy_sources.conversation.is_empty());
        assert!(store.publish_files(&mut state).is_err());
    }
    #[test]
    fn independent_wal_reset_during_preparation_rejects_staged_page_after_explicit_release() {
        let mut store = Store::new().unwrap();
        store
            .apply(Event::Item(prose("same", "private old source")))
            .unwrap();
        let writer = Connection::open(store.directory.join("transcript.sqlite")).unwrap();
        let result = store.with_peer_snapshot(|store| {
            // The guard has already acquired this reader's actual SQLite snapshot.
            writer
                .execute(
                    "UPDATE peer_query_state SET incarnation=?1",
                    [uuid::Uuid::new_v4().to_string()],
                )
                .unwrap();
            let private = store.peer_source_page(None)?;
            assert_eq!(contents(&private), "private old source");
            Ok(private)
        });
        assert!(result.unwrap_err().contains("replacement"));
        assert!(store.db.is_autocommit());
        let mut reader = Reader::default();
        assert!(store.peer_page(&mut reader, None).is_err());
    }
}
