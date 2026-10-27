//! Canonical delegated tools share the original Store lease, never its bounded
//! presentation window or the parent's item namespace. Bodies are chunked once.
use super::*;
use crate::chat::protocol::CanonicalItemKey;

pub(super) const SCHEMA: &str = "CREATE TABLE child_tools(agent TEXT NOT NULL,id TEXT NOT NULL,metadata BLOB NOT NULL,len0 INTEGER NOT NULL,len1 INTEGER NOT NULL,spans BLOB NOT NULL,cost INTEGER NOT NULL,PRIMARY KEY(agent,id));
CREATE TABLE child_tool_chunks(agent TEXT NOT NULL,id TEXT NOT NULL,field INTEGER NOT NULL,position INTEGER NOT NULL,data BLOB NOT NULL,PRIMARY KEY(agent,id,field,position),FOREIGN KEY(agent,id) REFERENCES child_tools(agent,id) ON DELETE CASCADE);
CREATE TABLE child_tool_revoked(agent TEXT NOT NULL,id TEXT NOT NULL,kind INTEGER NOT NULL DEFAULT 0 CHECK(kind IN (0,1)),PRIMARY KEY(agent,id));
CREATE TABLE child_tool_budget(singleton INTEGER PRIMARY KEY CHECK(singleton=1),bytes INTEGER NOT NULL,items INTEGER NOT NULL,chunks INTEGER NOT NULL);
INSERT INTO child_tool_budget VALUES(1,0,0,0);";
const ITEM_BYTES: usize = 64 * 1024 * 1024;
const STORE_BYTES: usize = 256 * 1024 * 1024;
const STORE_ITEMS: usize = 8192;
const STORE_CHUNKS: usize = 65536;
const PENDING_BYTES: usize = 64 * 1024;
const KEY_BYTES: usize = 4096;
const SOURCE_ERROR: &str = "The subagent's complete original tool record is unavailable. Existing history was kept; no replacement result was inferred";
const BUDGET_ERROR: &str = "This conversation reached its complete subagent tool history limit. Existing history was kept; exact late updates are unavailable";

pub(super) struct Pending {
    agent: String,
    id: String,
    input: bool,
    offset: usize,
    delta: String,
}

impl Store {
    fn child_source_refused(&mut self, reason: &'static str) -> Option<String> {
        let first = self.child_tools_unavailable.is_none();
        self.child_tools_unavailable = Some(reason);
        self.pending_child_tool = None;
        first.then(|| reason.into())
    }

    fn child_tool_revoked(&self, agent: &str, id: &str) -> Result<bool, String> {
        self.db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM child_tool_revoked WHERE agent=?1 AND id=?2)",
                params![agent, id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())
    }

    pub(super) fn revoked_child_tool_event(&self, event: &Event) -> Result<bool, String> {
        let (key, prose) = match event {
            Event::SubagentItem { id, item } => (
                Some((id, &item.id)),
                !matches!(
                    item.content,
                    ItemContent::Tool { .. }
                        | ItemContent::Command { .. }
                        | ItemContent::Changes { .. }
                ),
            ),
            Event::SubagentOutputDelta { id, item_id, .. } => (Some((id, item_id)), false),
            Event::SubagentTextDelta { id, item_id, .. } => (Some((id, item_id)), true),
            _ => (None, false),
        };
        let Some((agent, id)) = key else {
            return Ok(false);
        };
        self.db.query_row("SELECT EXISTS(SELECT 1 FROM child_tool_revoked WHERE agent=?1 AND id=?2 AND (?3=0 OR kind=0))",params![agent,id,prose],|row|row.get(0)).map_err(|e|e.to_string())
    }
    fn revoke_child_tool(&mut self, agent: &str, id: &str) -> Result<(), String> {
        self.revoke_child_tool_kind(agent, id, 0)
    }
    fn revoke_child_tool_kind(&mut self, agent: &str, id: &str, kind: i64) -> Result<(), String> {
        let tx = self.db.transaction().map_err(|e| e.to_string())?;
        let cost: Option<usize> = tx
            .query_row(
                "SELECT cost FROM child_tools WHERE agent=?1 AND id=?2",
                params![agent, id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let marked: bool = tx
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM child_tool_revoked WHERE agent=?1 AND id=?2)",
                params![agent, id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if cost.is_some() && marked {
            return Err("Delegated source is both live and revoked".into());
        }
        if let Some(cost) = cost {
            let chunks: usize = tx
                .query_row(
                    "SELECT count(*) FROM child_tool_chunks WHERE agent=?1 AND id=?2",
                    params![agent, id],
                    |row| row.get(0),
                )
                .map_err(|e| e.to_string())?;
            tx.execute(
                "DELETE FROM child_tools WHERE agent=?1 AND id=?2",
                params![agent, id],
            )
            .map_err(|e| e.to_string())?;
            tx.execute(
                "UPDATE child_tool_budget SET bytes=bytes-?1+?2,chunks=chunks-?3 WHERE singleton=1",
                params![cost, agent.len() + id.len() + 64, chunks],
            )
            .map_err(|e| e.to_string())?;
        }
        if cost.is_some() || marked {
            tx.execute("INSERT INTO child_tool_revoked(agent,id,kind) VALUES(?1,?2,?3) ON CONFLICT(agent,id) DO UPDATE SET kind=min(child_tool_revoked.kind,excluded.kind)",params![agent,id,kind]).map_err(|e|e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        self.child_tools_have_revocations |= cost.is_some() || marked;
        Ok(())
    }

    pub(super) fn cache_child_tool(&mut self, event: &Event) -> Result<Option<String>, String> {
        // Revocation discards uncommitted bytes before deleting the namespace;
        // a pending batch must not re-create a removed item after this event.
        match event {
            Event::Reset { .. } => {
                self.pending_child_tool = None;
                let tx = self.db.transaction().map_err(|e| e.to_string())?;
                tx.execute_batch("DELETE FROM child_tools; DELETE FROM child_tool_revoked; UPDATE child_tool_budget SET bytes=0,items=0,chunks=0 WHERE singleton=1;").map_err(|e| e.to_string())?;
                tx.commit().map_err(|e| e.to_string())?;
                self.child_tools_unavailable = None;
                self.child_tools_have_revocations = false;
                return Ok(None);
            }
            Event::SubagentItemsRemoved { id, ids } => {
                if self
                    .pending_child_tool
                    .as_ref()
                    .is_some_and(|pending| pending.agent == *id && ids.contains(&pending.id))
                {
                    self.pending_child_tool = None;
                }
                for item in ids {
                    self.revoke_child_tool(id, item)?;
                }
                return Ok(None);
            }
            _ => {}
        }
        if self.child_tools_unavailable.is_some() {
            return Ok(None);
        }
        let delta = match event {
            Event::SubagentOutputDelta {
                id,
                item_id,
                offset,
                delta,
            } => Some((id, item_id, *offset, delta, false)),
            _ => None,
        };
        if let Some((agent, id, offset, delta, input)) = delta {
            if agent.len() > KEY_BYTES || id.len() > KEY_BYTES {
                return Ok(self.child_source_refused(BUDGET_ERROR));
            }
            if self.pending_child_tool.as_ref().is_some_and(|pending| {
                pending.agent != *agent
                    || pending.id != *id
                    || pending.input != input
                    || pending.offset.checked_add(pending.delta.len()) != Some(offset)
                    || pending.delta.len().saturating_add(delta.len()) > PENDING_BYTES
            }) {
                if let Some(warning) = self.flush_child_tool()? {
                    return Ok(Some(warning));
                }
            }
            if delta.len() > PENDING_BYTES {
                return self.append_child_tool(agent, id, input, offset, delta);
            }
            if let Some(pending) = &mut self.pending_child_tool {
                pending.delta.push_str(delta);
            } else {
                self.pending_child_tool = Some(Pending {
                    agent: agent.clone(),
                    id: id.clone(),
                    input,
                    offset,
                    delta: delta.clone(),
                });
            }
            return Ok(None);
        }
        let replaces_pending = match event {
            Event::SubagentItem { id, item } => self
                .pending_child_tool
                .as_ref()
                .is_some_and(|pending| pending.agent == *id && pending.id == item.id),
            Event::SubagentTextDelta { id, item_id, .. } => self
                .pending_child_tool
                .as_ref()
                .is_some_and(|pending| pending.agent == *id && pending.id == *item_id),
            _ => false,
        };
        let changes_kind = matches!(event, Event::SubagentTextDelta { .. })
            || matches!(event,Event::SubagentItem{item,..} if !matches!(item.content,ItemContent::Tool{..}|ItemContent::Command{..}|ItemContent::Changes{..}));
        if replaces_pending && changes_kind {
            self.pending_child_tool = None;
        } else if replaces_pending && let Some(warning) = self.flush_child_tool()? {
            return Ok(Some(warning));
        }
        if let Event::SubagentItem { id, item } = event {
            return self.put_child_tool(id, item);
        }
        // A prose delta using a previous tool ID is a destructive kind change.
        if let Event::SubagentTextDelta { id, item_id, .. } = event {
            self.revoke_child_tool_kind(id, item_id, 1)?;
        }
        Ok(None)
    }

    fn put_child_tool(&mut self, agent: &str, original: &Item) -> Result<Option<String>, String> {
        let id = &original.id;
        if agent.len() > KEY_BYTES || id.len() > KEY_BYTES {
            return Ok(self.child_source_refused(BUDGET_ERROR));
        }
        if self.child_tool_revoked(agent, id)? {
            return Ok(Some(
                "Delegated tool source was removed; a late update cannot recreate its private body"
                    .into(),
            ));
        }
        let mut identity = agent.len().saturating_add(id.len()).saturating_add(128);
        let (content, bodies, spans) = match &original.content {
            ItemContent::Tool {
                name,
                input,
                output,
                status,
            } => {
                identity = identity.saturating_add(name.len());
                if identity > METADATA_LIMIT {
                    return Ok(self.child_source_refused(BUDGET_ERROR));
                }
                (
                    ItemContent::Tool {
                        name: name.clone(),
                        input: String::new(),
                        output: String::new(),
                        status: *status,
                    },
                    [vec![input.as_str()], vec![output.as_str()]],
                    Vec::new(),
                )
            }
            ItemContent::Command {
                label,
                command,
                working_directory,
                action,
                status,
                exit_code,
                duration_ms,
                output,
            } => {
                identity = identity
                    .saturating_add(label.len())
                    .saturating_add(command.len())
                    .saturating_add(working_directory.len());
                if identity > METADATA_LIMIT {
                    return Ok(self.child_source_refused(BUDGET_ERROR));
                }
                (
                    ItemContent::Command {
                        label: label.clone(),
                        command: command.clone(),
                        working_directory: working_directory.clone(),
                        action: *action,
                        status: *status,
                        exit_code: *exit_code,
                        duration_ms: *duration_ms,
                        output: String::new(),
                    },
                    [vec![output.as_str()], Vec::new()],
                    Vec::new(),
                )
            }
            ItemContent::Changes { changes, status } => {
                for change in changes {
                    identity = identity
                        .saturating_add(change.path.len())
                        .saturating_add(128);
                    if let crate::chat::ChangeKind::Renamed { from } = &change.kind {
                        identity = identity.saturating_add(from.len());
                        if from.is_empty() || from.contains('\0') {
                            return Ok(self.child_source_refused(SOURCE_ERROR));
                        }
                    }
                    if change.path.is_empty() || change.path.contains('\0') {
                        return Ok(self.child_source_refused(SOURCE_ERROR));
                    }
                    if identity > METADATA_LIMIT {
                        return Ok(self.child_source_refused(BUDGET_ERROR));
                    }
                }
                (
                    ItemContent::Changes {
                        changes: changes
                            .iter()
                            .map(|change| crate::chat::FileChange {
                                path: change.path.clone(),
                                kind: change.kind.clone(),
                                diff: String::new(),
                            })
                            .collect(),
                        status: *status,
                    },
                    [
                        changes.iter().map(|change| change.diff.as_str()).collect(),
                        Vec::new(),
                    ],
                    changes.iter().map(|change| change.diff.len()).collect(),
                )
            }
            _ => {
                self.revoke_child_tool_kind(agent, id, 1)?;
                return Ok(None);
            }
        };
        let mut lengths = [0usize; 2];
        for (length, fragments) in lengths.iter_mut().zip(&bodies) {
            for fragment in fragments {
                *length = length.saturating_add(fragment.len());
            }
        }
        if lengths[0].saturating_add(lengths[1]) > ITEM_BYTES {
            return Ok(self.child_source_refused(BUDGET_ERROR));
        }
        let mut item = Item {
            id: id.clone(),
            role: original.role,
            at: original.at,
            started_at: original.started_at,
            content,
        };
        let previous: Option<(usize, usize, Vec<u8>)> = self
            .db
            .query_row(
                "SELECT cost,len0,metadata FROM child_tools WHERE agent=?1 AND id=?2",
                params![agent, id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some((_, _, bytes)) = &previous {
            let old: Item = postcard::from_bytes(bytes).map_err(|e| e.to_string())?;
            if let (ItemContent::Tool { name, .. }, ItemContent::Tool { name: old, .. }) =
                (&mut item.content, old.content)
                && name.is_empty()
            {
                *name = old;
            }
        }
        let metadata = postcard::to_stdvec(&item).map_err(|e| e.to_string())?;
        let spans = postcard::to_stdvec(&spans).map_err(|e| e.to_string())?;
        if metadata.len() > METADATA_LIMIT || spans.len() > METADATA_LIMIT {
            return Ok(self.child_source_refused(BUDGET_ERROR));
        }
        let preserve_input = matches!(&original.content,ItemContent::Tool{input,..} if input.is_empty())
            && previous.as_ref().is_some_and(|(_, _, bytes)| {
                postcard::from_bytes::<Item>(bytes)
                    .is_ok_and(|old| matches!(old.content, ItemContent::Tool { .. }))
            });
        if preserve_input {
            lengths[0] = previous.as_ref().unwrap().1;
        }
        let old_chunks: usize = self
            .db
            .query_row(
                "SELECT count(*) FROM child_tool_chunks WHERE agent=?1 AND id=?2",
                params![agent, id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        let changes = matches!(item.content, ItemContent::Changes { .. });
        let input_chunks = if changes {
            0
        } else if preserve_input {
            self.db
                .query_row(
                    "SELECT count(*) FROM child_tool_chunks WHERE agent=?1 AND id=?2 AND field=0",
                    params![agent, id],
                    |row| row.get::<_, usize>(0),
                )
                .map_err(|e| e.to_string())?
        } else {
            fragment_chunks(&bodies[0])
        };
        let chunks = input_chunks.saturating_add(fragment_chunks(&bodies[1]));
        let cost = identity
            .saturating_add(metadata.len())
            .saturating_add(spans.len())
            .saturating_add(lengths[0])
            .saturating_add(lengths[1])
            .saturating_add(chunks.saturating_mul(chunk_cost(agent, id)));
        let (total, count, total_chunks): (usize, usize, usize) = self
            .db
            .query_row(
                "SELECT bytes,items,chunks FROM child_tool_budget WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| e.to_string())?;
        if lengths[0].saturating_add(lengths[1]) > ITEM_BYTES
            || total
                .saturating_sub(previous.as_ref().map_or(0, |row| row.0))
                .saturating_add(cost)
                > STORE_BYTES
            || previous.is_none() && count >= STORE_ITEMS
            || total_chunks
                .saturating_sub(old_chunks)
                .saturating_add(chunks)
                > STORE_CHUNKS
        {
            return Ok(self.child_source_refused(BUDGET_ERROR));
        }
        let tx = self.db.transaction().map_err(|e| e.to_string())?;
        tx.execute("INSERT INTO child_tools VALUES(?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(agent,id) DO UPDATE SET metadata=excluded.metadata,len0=excluded.len0,len1=excluded.len1,spans=excluded.spans,cost=excluded.cost",params![agent,id,metadata,lengths[0],lengths[1],spans,cost]).map_err(|e|e.to_string())?;
        tx.execute(
            "DELETE FROM child_tool_chunks WHERE agent=?1 AND id=?2 AND (?3=0 OR field=1)",
            params![agent, id, preserve_input],
        )
        .map_err(|e| e.to_string())?;
        // The existing file index already owns full delegated Changes patches.
        // Retain identity/spans here, never a second copy of those bodies.
        if !preserve_input && !changes {
            insert_fragments(&tx, agent, id, 0, 0, &bodies[0])?;
        }
        insert_fragments(&tx, agent, id, 1, 0, &bodies[1])?;
        tx.execute("UPDATE child_tool_budget SET bytes=bytes-?1+?2,items=items+?3,chunks=chunks-?4+?5 WHERE singleton=1",params![previous.as_ref().map_or(0,|row|row.0),cost,usize::from(previous.is_none()),old_chunks,chunks]).map_err(|e|e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(None)
    }

    fn flush_child_tool(&mut self) -> Result<Option<String>, String> {
        let Some(pending) = self.pending_child_tool.take() else {
            return Ok(None);
        };
        match self.append_child_tool(
            &pending.agent,
            &pending.id,
            pending.input,
            pending.offset,
            &pending.delta,
        ) {
            Ok(warning) => Ok(warning),
            Err(error) => {
                self.pending_child_tool = Some(pending);
                Err(error)
            }
        }
    }

    fn append_child_tool(
        &mut self,
        agent: &str,
        id: &str,
        input: bool,
        offset: usize,
        delta: &str,
    ) -> Result<Option<String>, String> {
        if self.child_tool_revoked(agent, id)? {
            return Ok(Some(
                "Delegated tool source was removed; its late delta was not stored".into(),
            ));
        }
        let record: Option<(Vec<u8>, usize, usize)> = self
            .db
            .query_row(
                "SELECT metadata,len0,len1 FROM child_tools WHERE agent=?1 AND id=?2",
                params![agent, id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        let Some((bytes, len0, len1)) = record else {
            return Ok(self.child_source_refused(SOURCE_ERROR));
        };
        let item: Item = postcard::from_bytes(&bytes).map_err(|e| e.to_string())?;
        let field = match item.content {
            ItemContent::Tool { .. } => {
                if input {
                    0
                } else {
                    1
                }
            }
            ItemContent::Command { .. } if !input => 0,
            _ => return Ok(self.child_source_refused(SOURCE_ERROR)),
        };
        let length = if field == 0 { len0 } else { len1 };
        if offset != length {
            return Ok(self.child_source_refused(SOURCE_ERROR));
        }
        let (total, total_chunks): (usize, usize) = self
            .db
            .query_row(
                "SELECT bytes,chunks FROM child_tool_budget WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(|e| e.to_string())?;
        let chunks = delta.len().div_ceil(PART_BYTES);
        let cost = delta
            .len()
            .saturating_add(chunks.saturating_mul(chunk_cost(agent, id)));
        if len0.saturating_add(len1).saturating_add(delta.len()) > ITEM_BYTES
            || total.saturating_add(cost) > STORE_BYTES
            || total_chunks.saturating_add(chunks) > STORE_CHUNKS
        {
            return Ok(self.child_source_refused(BUDGET_ERROR));
        }
        let tx = self.db.transaction().map_err(|e| e.to_string())?;
        insert_fragments(&tx, agent, id, field, offset, &[delta])?;
        let update = if field == 0 {
            "UPDATE child_tools SET len0=len0+?3,cost=cost+?4 WHERE agent=?1 AND id=?2"
        } else {
            "UPDATE child_tools SET len1=len1+?3,cost=cost+?4 WHERE agent=?1 AND id=?2"
        };
        tx.execute(update, params![agent, id, delta.len(), cost])
            .map_err(|e| e.to_string())?;
        tx.execute(
            "UPDATE child_tool_budget SET bytes=bytes+?1,chunks=chunks+?2 WHERE singleton=1",
            params![cost, chunks],
        )
        .map_err(|e| e.to_string())?;
        tx.commit().map_err(|e| e.to_string())?;
        Ok(None)
    }

    pub(super) fn validate_child_adapter_key(&self, key: &CanonicalItemKey) -> Result<(), String> {
        if let Some(reason) = self.child_tools_unavailable {
            return Err(reason.into());
        }
        let agent = key
            .agent_id
            .as_ref()
            .ok_or("A delegated tool requires its exact child identity")?;
        if agent.len() > KEY_BYTES || key.item_id.len() > KEY_BYTES {
            return Err(BUDGET_ERROR.into());
        }
        let kind: Option<i64> = self
            .db
            .query_row(
                "SELECT kind FROM child_tool_revoked WHERE agent=?1 AND id=?2",
                params![agent, key.item_id],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(kind) = kind {
            return Err(if kind == 0 {
                "Delegated tool source was explicitly removed"
            } else {
                "Delegated source changed kind; the old tool cannot be replayed"
            }
            .into());
        }
        let (bytes, items, chunks): (i64, i64, i64) = self
            .db
            .query_row(
                "SELECT bytes,items,chunks FROM child_tool_budget WHERE singleton=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .map_err(|e| e.to_string())?;
        if bytes < 0
            || bytes > STORE_BYTES as i64
            || items < 0
            || items > STORE_ITEMS as i64
            || chunks < 0
            || chunks > STORE_CHUNKS as i64
        {
            return Err(BUDGET_ERROR.into());
        }
        Ok(())
    }
    pub(super) fn flush_child_adapter_read(
        &mut self,
        key: &CanonicalItemKey,
    ) -> Result<(), String> {
        self.validate_child_adapter_key(key)?;
        if self
            .pending_child_tool
            .as_ref()
            .is_some_and(|p| Some(&p.agent) == key.agent_id.as_ref() && p.id == key.item_id)
            && let Some(warning) = self.flush_child_tool()?
        {
            return Err(warning);
        }
        self.validate_child_adapter_key(key)
    }
    pub(super) fn child_adapter_item(
        &self,
        key: &CanonicalItemKey,
    ) -> Result<Option<Item>, String> {
        if let Some(reason) = self.child_tools_unavailable {
            return Err(reason.into());
        }
        self.validate_child_adapter_key(key)?;
        let agent = key
            .agent_id
            .as_ref()
            .ok_or("A delegated tool requires its exact child identity")?;
        let record=self.db.query_row("SELECT CASE WHEN length(metadata)<=?3 THEN metadata ELSE NULL END,len0,len1,CASE WHEN length(spans)<=?3 THEN spans ELSE NULL END FROM child_tools WHERE agent=?1 AND id=?2",params![agent,key.item_id,METADATA_LIMIT],|row|Ok((row.get::<_,Option<Vec<u8>>>(0)?,row.get::<_,i64>(1)?,row.get::<_,i64>(2)?,row.get::<_,Option<Vec<u8>>>(3)?))).optional().map_err(|e|e.to_string())?;
        let Some((metadata, len0, len1, spans)) = record else {
            return Ok(None);
        };
        let spans = spans.ok_or("Delegated patch metadata exceeds its canonical limit")?;
        let lengths = [len0, len1].map(usize::try_from);
        let [Ok(len0), Ok(len1)] = lengths else {
            return Err("Delegated tool has invalid canonical lengths".into());
        };
        if len0.checked_add(len1).is_none_or(|len| len > ITEM_BYTES) {
            return Err(BUDGET_ERROR.into());
        }
        let mut item: Item = postcard::from_bytes(
            &metadata.ok_or("Delegated tool metadata exceeds its canonical limit")?,
        )
        .map_err(|e| e.to_string())?;
        if item.id != key.item_id {
            return Err("Delegated tool canonical identity changed".into());
        }
        let extra:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM child_tool_chunks WHERE agent=?1 AND id=?2 AND field NOT IN (0,1))",params![agent,key.item_id],|row|row.get(0)).map_err(|e|e.to_string())?;
        if extra {
            return Err("Delegated tool canonical layout has an unknown body field".into());
        }
        if let ItemContent::Changes { changes, status } = &mut item.content {
            self.child_patch_items(agent, &key.item_id, changes, *status, &spans, len0, len1)?;
            return Ok(Some(item));
        }
        let body0 = String::from_utf8(self.child_tool_field(agent, &key.item_id, 0, len0)?)
            .map_err(|e| e.to_string())?;
        let body1 = String::from_utf8(self.child_tool_field(agent, &key.item_id, 1, len1)?)
            .map_err(|e| e.to_string())?;
        match &mut item.content {
            ItemContent::Tool { input, output, .. } => {
                *input = body0;
                *output = body1;
            }
            ItemContent::Command { output, .. } if body1.is_empty() => {
                *output = body0;
            }
            _ => return Err("Delegated source is not an exact canonical tool".into()),
        }
        Ok(Some(item))
    }

    fn child_patch_items(
        &self,
        agent: &str,
        id: &str,
        changes: &mut [crate::chat::FileChange],
        status: crate::chat::ToolStatus,
        spans: &[u8],
        len0: usize,
        len1: usize,
    ) -> Result<(), String> {
        let spans: Vec<usize> = postcard::from_bytes(spans).map_err(|e| e.to_string())?;
        if len1 != 0 || spans.len() != changes.len() {
            return Err("Delegated patch identity or layout changed".into());
        }
        let duplicate: bool = self
            .db
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM child_tool_chunks WHERE agent=?1 AND id=?2)",
                params![agent, id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if duplicate {
            return Err("Delegated patch has an unexpected duplicate canonical body".into());
        }
        let source = format!("c:{agent}");
        let count: usize = self
            .db
            .query_row(
                "SELECT count(*) FROM file_edits WHERE source=?1 AND item_id=?2",
                params![source, id],
                |row| row.get(0),
            )
            .map_err(|e| e.to_string())?;
        if count != changes.len() {
            return Err("Delegated patch manifest is missing or was shortened".into());
        }
        let mut offset = 0usize;
        for (index, (change, len)) in changes.iter_mut().zip(spans).enumerate() {
            let manifest=self.db.query_row("SELECT CASE WHEN length(CAST(path AS BLOB))<=?4 THEN path ELSE NULL END,CASE WHEN length(kind)<=?4 THEN kind ELSE NULL END,CASE WHEN length(status)<=?4 THEN status ELSE NULL END,start,len FROM file_edits WHERE source=?1 AND item_id=?2 AND file_index=?3",params![source,id,index,METADATA_LIMIT],|row|Ok((row.get::<_,Option<String>>(0)?,row.get::<_,Option<Vec<u8>>>(1)?,row.get::<_,Option<Vec<u8>>>(2)?,row.get::<_,i64>(3)?,row.get::<_,i64>(4)?))).optional().map_err(|e|e.to_string())?.ok_or("Delegated patch manifest has an invalid file index")?;
            let kind: crate::chat::ChangeKind = postcard::from_bytes(
                &manifest
                    .1
                    .ok_or("Delegated patch kind exceeds its identity limit")?,
            )
            .map_err(|e| e.to_string())?;
            let actual_status: crate::chat::ToolStatus = postcard::from_bytes(
                &manifest
                    .2
                    .ok_or("Delegated patch status exceeds its identity limit")?,
            )
            .map_err(|e| e.to_string())?;
            if manifest.0.as_ref() != Some(&change.path)
                || kind != change.kind
                || actual_status != status
                || usize::try_from(manifest.3).ok() != Some(offset)
                || usize::try_from(manifest.4).ok() != Some(len)
            {
                return Err(
                    "Delegated patch manifest changed its exact identity or outcome".into(),
                );
            }
            offset = offset
                .checked_add(len)
                .filter(|end| *end <= len0)
                .ok_or("Delegated patch length overflow")?;
            let mut statement=self.db.prepare_cached("SELECT position,length(data),CASE WHEN length(data)<=?4 THEN data ELSE NULL END FROM file_patch_chunks WHERE source=?1 AND item_id=?2 AND file_index=?3 ORDER BY position").map_err(|e|e.to_string())?;
            let rows = statement
                .query_map(params![source, id, index, PART_BYTES], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<Vec<u8>>>(2)?,
                    ))
                })
                .map_err(|e| e.to_string())?;
            let mut bytes = Vec::with_capacity(len);
            for row in rows {
                let (position, length, data) = row.map_err(|e| e.to_string())?;
                let data = data.ok_or("Delegated patch has an oversized canonical chunk")?;
                if usize::try_from(position).ok() != Some(bytes.len())
                    || data.is_empty()
                    || length != data.len() as i64
                    || data.len() > len.saturating_sub(bytes.len())
                {
                    return Err(
                        "Delegated patch has a missing, overlapping or invalid chunk".into(),
                    );
                }
                bytes.extend(data);
            }
            if bytes.len() != len {
                return Err("Delegated patch source is incomplete".into());
            }
            change.diff = String::from_utf8(bytes).map_err(|e| e.to_string())?;
        }
        if offset != len0 {
            return Err("Delegated patch source has unclaimed bytes".into());
        }
        Ok(())
    }

    fn child_tool_field(
        &self,
        agent: &str,
        id: &str,
        field: i64,
        expected: usize,
    ) -> Result<Vec<u8>, String> {
        let mut query=self.db.prepare_cached("SELECT position,length(data),CASE WHEN length(data)<=?4 THEN data ELSE NULL END FROM child_tool_chunks WHERE agent=?1 AND id=?2 AND field=?3 ORDER BY position").map_err(|e|e.to_string())?;
        let rows = query
            .query_map(params![agent, id, field, PART_BYTES], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                ))
            })
            .map_err(|e| e.to_string())?;
        let mut result = Vec::with_capacity(expected);
        for row in rows {
            let (offset, length, bytes) = row.map_err(|e| e.to_string())?;
            let bytes = bytes.ok_or("Delegated tool has an oversized canonical chunk")?;
            if usize::try_from(offset).ok() != Some(result.len())
                || bytes.is_empty()
                || length != bytes.len() as i64
                || bytes.len() > expected.saturating_sub(result.len())
            {
                return Err("Delegated tool has a missing, overlapping or invalid chunk".into());
            }
            result.extend(bytes);
        }
        if result.len() != expected {
            return Err("Delegated tool canonical source is incomplete".into());
        }
        Ok(result)
    }
}

fn chunk_cost(agent: &str, id: &str) -> usize {
    agent.len().saturating_add(id.len()).saturating_add(64)
}
fn fragment_chunks(fragments: &[&str]) -> usize {
    fragments
        .iter()
        .map(|fragment| fragment.len().div_ceil(PART_BYTES))
        .sum()
}

fn insert_fragments(
    tx: &rusqlite::Transaction<'_>,
    agent: &str,
    id: &str,
    field: i64,
    mut offset: usize,
    fragments: &[&str],
) -> Result<(), String> {
    // Append only new bytes. Rewriting the final partial chunk on each child
    // switch amplifies tiny deltas into repeated 128 KiB writes. Row/key cost
    // and total chunk count are separately charged to the namespace budget.
    let mut statement = tx
        .prepare_cached("INSERT INTO child_tool_chunks VALUES(?1,?2,?3,?4,?5)")
        .map_err(|e| e.to_string())?;
    for fragment in fragments {
        for bytes in fragment.as_bytes().chunks(PART_BYTES) {
            statement
                .execute(params![agent, id, field, offset, bytes])
                .map_err(|e| e.to_string())?;
            offset = offset
                .checked_add(bytes.len())
                .ok_or("Delegated tool chunk offset overflow")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChangeKind, FileChange, ToolStatus};
    use std::sync::{Arc, Mutex};

    fn tool(id: &str, input: &str, output: &str) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Tool {
                name: "ExactTool".into(),
                input: input.into(),
                output: output.into(),
                status: ToolStatus::Running,
            },
        }
    }
    fn put(store: &mut Store, agent: &str, item: Item) {
        assert!(
            !store
                .apply(Event::SubagentItem {
                    id: agent.into(),
                    item
                })
                .unwrap()
                .iter()
                .any(|event| matches!(event, Event::Error { .. }))
        );
    }
    fn read(store: &mut Store, agent: &str, id: &str) -> Option<Item> {
        {
            let key = CanonicalItemKey::new(Some(agent), id);
            store.flush_child_adapter_read(&key).unwrap();
            store.child_adapter_item(&key).unwrap()
        }
    }

    #[test]
    fn exact_child_tools_outlive_window_limits_without_parent_or_sibling_aliasing() {
        let mut store = Store::new().unwrap();
        let input = "é🙂full invocation\n".repeat(100_000);
        let original = tool("same", &input, "initial");
        put(&mut store, "one", original.clone());
        put(&mut store, "two", tool("same", "sibling", "other"));
        store
            .apply(Event::Item(tool("same", "parent", "parent result")))
            .unwrap();
        for n in 0..200 {
            put(&mut store, "one", tool(&format!("new-{n}"), "later", ""));
        }
        store.window().unwrap();
        let cursor = (
            store.end,
            store.first,
            store.forward,
            store.paging,
            store.dirty,
        );
        assert_eq!(read(&mut store, "one", "same"), Some(original.clone()));
        assert!(
            matches!(read(&mut store,"two","same").unwrap().content,ItemContent::Tool{input,..} if input=="sibling")
        );
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM items WHERE id='same'", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            (
                store.end,
                store.first,
                store.forward,
                store.paging,
                store.dirty
            ),
            cursor
        );
        let mut completed = original;
        if let ItemContent::Tool { status, output, .. } = &mut completed.content {
            *status = ToolStatus::Succeeded;
            *output = "late result".into();
        }
        put(&mut store, "one", completed.clone());
        assert_eq!(read(&mut store, "one", "same"), Some(completed));
    }

    #[test]
    fn exact_write_patch_spans_and_command_metadata_round_trip_after_late_completion() {
        let mut store = Store::new().unwrap();
        let mut original = Item {
            id: "write".into(),
            role: Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Changes {
                status: ToolStatus::Running,
                changes: vec![
                    FileChange {
                        path: "new.rs".into(),
                        kind: ChangeKind::Added,
                        diff: "+é🙂\n".repeat(100_000),
                    },
                    FileChange {
                        path: "renamed.rs".into(),
                        kind: ChangeKind::Renamed {
                            from: "old.rs".into(),
                        },
                        diff: "-old\n+new\n".repeat(20_000),
                    },
                ],
            },
        };
        put(&mut store, "child", original.clone());
        assert_eq!(
            store
                .db
                .query_row(
                    "SELECT count(*) FROM child_tool_chunks WHERE id='write'",
                    [],
                    |r| r.get::<_, usize>(0)
                )
                .unwrap(),
            0,
            "patch bytes must not be duplicated"
        );
        assert!(
            store
                .db
                .query_row(
                    "SELECT count(*) FROM file_patch_chunks WHERE item_id='write'",
                    [],
                    |r| r.get::<_, usize>(0)
                )
                .unwrap()
                > 0
        );
        if let ItemContent::Changes { status, .. } = &mut original.content {
            *status = ToolStatus::Succeeded;
        }
        put(&mut store, "child", original.clone());
        assert_eq!(read(&mut store, "child", "write"), Some(original));
        let command = Item {
            id: "cmd".into(),
            role: Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Command {
                label: "Tests".into(),
                command: "cargo test".into(),
                working_directory: "/issued".into(),
                action: crate::chat::CommandAction::Run,
                status: ToolStatus::Succeeded,
                exit_code: Some(0),
                duration_ms: Some(42),
                output: "full command output\n".repeat(100_000),
            },
        };
        put(&mut store, "child", command.clone());
        assert_eq!(read(&mut store, "child", "cmd"), Some(command));
    }

    #[test]
    fn unrelated_status_keeps_one_small_pending_batch_until_a_matching_read_or_switch() {
        let mut store = Store::new().unwrap();
        put(&mut store, "child", tool("tool", "original input", ""));
        let initial: usize = store
            .db
            .query_row("SELECT count(*) FROM child_tool_chunks", [], |r| r.get(0))
            .unwrap();
        for offset in 0..4096 {
            store
                .apply(Event::SubagentOutputDelta {
                    id: "child".into(),
                    item_id: "tool".into(),
                    offset,
                    delta: "x".into(),
                })
                .unwrap();
            store
                .apply(Event::Status {
                    busy: true,
                    detail: "working".into(),
                })
                .unwrap();
            assert!(
                store
                    .pending_child_tool
                    .as_ref()
                    .is_some_and(|pending| pending.delta.len() <= PENDING_BYTES)
            );
        }
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM child_tool_chunks", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            initial
        );
        let item = read(&mut store, "child", "tool").unwrap();
        assert!(
            matches!(item.content,ItemContent::Tool{input,output,..} if input=="original input" && output=="x".repeat(4096))
        );
        assert!(store.pending_child_tool.is_none());
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM child_tool_chunks", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            initial + 1
        );
    }

    fn cache_writes(store: &Store, reset: bool) -> usize {
        let (mut current, mut high) = (0, 0);
        // Read SQLite's actual dirty-cache page-write counter, rather than
        // inferring write volume from the number of rows left afterward.
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
        usize::try_from(current).unwrap()
    }

    #[test]
    fn switching_children_with_fifty_byte_deltas_never_rewrites_old_tool_body_pages() {
        const STEPS: usize = 128;
        fn workload(previous_output: usize) -> usize {
            let mut store = Store::new().unwrap();
            store.db.execute_batch("PRAGMA cache_size=-64").unwrap();
            for child in ["one", "two"] {
                put(
                    &mut store,
                    child,
                    tool("tool", "", &"x".repeat(previous_output)),
                );
            }
            cache_writes(&store, true);
            for step in 0..STEPS {
                store
                    .apply(Event::SubagentOutputDelta {
                        id: if step % 2 == 0 { "one" } else { "two" }.into(),
                        item_id: "tool".into(),
                        offset: previous_output + (step / 2) * 50,
                        delta: "y".repeat(50),
                    })
                    .unwrap();
                store
                    .apply(Event::Status {
                        busy: true,
                        detail: "interleaved status".into(),
                    })
                    .unwrap();
            }
            assert!(store.flush_child_tool().unwrap().is_none());
            let writes = cache_writes(&store, false);
            assert!(
                writes > 0,
                "the counter must observe actual canonical commits"
            );
            assert!(
                writes <= STEPS * 16 + 64,
                "bounded dirty page writes, observed {writes}"
            );
            for child in ["one", "two"] {
                assert!(
                    matches!(read(&mut store,child,"tool").unwrap().content,ItemContent::Tool{output,..} if output.len()==previous_output+STEPS/2*50 && output.ends_with(&"y".repeat(STEPS/2*50)))
                );
            }
            writes
        }
        let short = workload(0);
        let almost_full = workload(PART_BYTES - 8192);
        assert!(
            almost_full <= short + STEPS * 4,
            "append I/O must not scale with already recorded body: short={short}, long={almost_full}"
        );
    }

    #[test]
    fn removal_reset_and_cross_kind_replacement_never_recreate_pending_tool_source() {
        let mut store = Store::new().unwrap();
        put(&mut store, "one", tool("same", "original", ""));
        put(&mut store, "two", tool("same", "sibling", ""));
        store
            .apply(Event::SubagentOutputDelta {
                id: "one".into(),
                item_id: "same".into(),
                offset: 0,
                delta: "pending private bytes".into(),
            })
            .unwrap();
        store
            .apply(Event::SubagentItemsRemoved {
                id: "one".into(),
                ids: vec!["same".into()],
            })
            .unwrap();
        assert!(store.pending_child_tool.is_none());
        assert!(
            store
                .child_adapter_item(&CanonicalItemKey::new(Some("one"), "same"))
                .is_err()
        );
        let late = store
            .apply(Event::SubagentItem {
                id: "one".into(),
                item: tool("same", "must not revive", ""),
            })
            .unwrap();
        assert!(
            late.iter()
                .any(|event| matches!(event, Event::Error { fatal: false, .. }))
        );
        assert!(
            !late
                .iter()
                .any(|event| matches!(event, Event::SubagentItem { .. }))
        );
        assert_eq!(
            store
                .db
                .query_row(
                    "SELECT count(*) FROM child_tools WHERE agent='one' AND id='same'",
                    [],
                    |r| r.get::<_, usize>(0)
                )
                .unwrap(),
            0
        );
        let patch = Item {
            id: "same".into(),
            role: Role::Assistant,
            at: None,
            started_at: None,
            content: ItemContent::Changes {
                status: ToolStatus::Succeeded,
                changes: vec![FileChange {
                    path: "private.rs".into(),
                    kind: ChangeKind::Added,
                    diff: "private patch must not reappear".into(),
                }],
            },
        };
        store
            .apply(Event::SubagentItem {
                id: "one".into(),
                item: patch,
            })
            .unwrap();
        assert_eq!(
            store
                .db
                .query_row(
                    "SELECT count(*) FROM file_edits WHERE source='c:one' AND item_id='same'",
                    [],
                    |r| r.get::<_, usize>(0)
                )
                .unwrap(),
            0
        );
        assert!(read(&mut store, "two", "same").is_some());
        let prose = Item {
            id: "same".into(),
            content: ItemContent::Text {
                text: "prose".into(),
                thinking: false,
                phase: Phase::Final,
            },
            ..tool("same", "", "")
        };
        put(&mut store, "two", prose);
        assert!(
            store
                .validate_child_adapter_key(&CanonicalItemKey::new(Some("two"), "same"))
                .is_err()
        );
        // The current port fences this old scoped identity even for a full item:
        // a replay has no new generation proof after the source changed kind.
        let replay = store
            .apply(Event::SubagentItem {
                id: "two".into(),
                item: tool("same", "tool again", ""),
            })
            .unwrap();
        assert!(replay.iter().any(|event| matches!(event,
            Event::Error { message, fatal: false } if message.contains("removed or changed kind")
        )));
        assert!(
            !replay
                .iter()
                .any(|event| matches!(event, Event::SubagentItem { .. }))
        );
        let key = CanonicalItemKey::new(Some("two"), "same");
        assert!(
            store
                .validate_child_adapter_key(&key)
                .unwrap_err()
                .contains("changed kind")
        );
        assert!(store.child_adapter_item(&key).is_err());
        assert_eq!(
            store
                .db
                .query_row(
                    "SELECT count(*) FROM child_tools WHERE agent='two' AND id='same'",
                    [],
                    |row| row.get::<_, usize>(0)
                )
                .unwrap(),
            0
        );
        store
            .apply(Event::SubagentTextDelta {
                id: "two".into(),
                item_id: "same".into(),
                role: Role::Assistant,
                thinking: false,
                delta: "new kind".into(),
            })
            .unwrap();
        assert!(
            store
                .validate_child_adapter_key(&CanonicalItemKey::new(Some("two"), "same"))
                .is_err()
        );
        put(&mut store, "one", tool("again", "source", ""));
        store
            .apply(Event::SubagentOutputDelta {
                id: "one".into(),
                item_id: "again".into(),
                offset: 0,
                delta: "pending".into(),
            })
            .unwrap();
        store
            .apply(Event::Reset {
                reason: "privacy reset".into(),
            })
            .unwrap();
        assert!(store.pending_child_tool.is_none());
        assert!(read(&mut store, "one", "again").is_none());
        assert_eq!(
            store
                .db
                .query_row("SELECT bytes+items FROM child_tool_budget", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn corrupt_chunks_and_declared_lengths_refuse_exact_replay() {
        for kind in 0..6 {
            let mut store = Store::new().unwrap();
            put(
                &mut store,
                "child",
                tool("tool", &"x".repeat(2 * PART_BYTES), ""),
            );
            match kind {
                0 => {
                    store
                        .db
                        .execute("DELETE FROM child_tool_chunks WHERE position=0", [])
                        .unwrap();
                }
                1 => {
                    store
                        .db
                        .execute(
                            "UPDATE child_tool_chunks SET position=1 WHERE position=0",
                            [],
                        )
                        .unwrap();
                }
                2 => {
                    store
                        .db
                        .execute(
                            "UPDATE child_tool_chunks SET data=zeroblob(?1) WHERE position=0",
                            [PART_BYTES + 1],
                        )
                        .unwrap();
                }
                3 => {
                    store
                        .db
                        .execute("UPDATE child_tools SET len0=-1", [])
                        .unwrap();
                }
                4 => {
                    store
                        .db
                        .execute("UPDATE child_tools SET len0=?1", [ITEM_BYTES + 1])
                        .unwrap();
                }
                _ => {
                    store
                        .db
                        .execute(
                            "INSERT INTO child_tool_chunks VALUES('child','tool',2,0,X'61')",
                            [],
                        )
                        .unwrap();
                }
            }
            assert!(
                store
                    .child_adapter_item(&CanonicalItemKey::new(Some("child"), "tool"))
                    .is_err(),
                "corruption {kind}"
            );
        }
    }

    #[test]
    fn delegated_patch_reader_reauthenticates_the_complete_shared_file_manifest() {
        for corruption in 0..7 {
            let mut store = Store::new().unwrap();
            let item = Item {
                id: "write".into(),
                role: Role::Assistant,
                at: None,
                started_at: None,
                content: ItemContent::Changes {
                    status: ToolStatus::Succeeded,
                    changes: vec![FileChange {
                        path: "exact.rs".into(),
                        kind: ChangeKind::Added,
                        diff: "+é\n".repeat(PART_BYTES),
                    }],
                },
            };
            put(&mut store, "child", item);
            match corruption {
                0 => {
                    store
                        .db
                        .execute("UPDATE file_edits SET path='other.rs'", [])
                        .unwrap();
                }
                1 => {
                    store
                        .db
                        .execute(
                            "UPDATE file_edits SET status=?1",
                            [postcard::to_stdvec(&ToolStatus::Failed).unwrap()],
                        )
                        .unwrap();
                }
                2 => {
                    store
                        .db
                        .execute_batch(
                            "DELETE FROM file_patch_chunks; UPDATE file_edits SET file_index=1;",
                        )
                        .unwrap();
                }
                3 => {
                    store
                        .db
                        .execute("UPDATE file_edits SET start=1", [])
                        .unwrap();
                }
                4 => {
                    store
                        .db
                        .execute("DELETE FROM file_patch_chunks WHERE position=0", [])
                        .unwrap();
                }
                5 => {
                    store
                        .db
                        .execute(
                            "UPDATE file_patch_chunks SET data=zeroblob(?1) WHERE position=0",
                            [PART_BYTES + 1],
                        )
                        .unwrap();
                }
                _ => {
                    store.db.execute("INSERT INTO file_edits SELECT source,item_id,file_index+1,ordinal,path||'.other',kind,status,imported,additions,deletions,start,len FROM file_edits",[]).unwrap();
                }
            }
            assert!(
                store
                    .child_adapter_item(&CanonicalItemKey::new(Some("child"), "write"))
                    .is_err(),
                "shared manifest corruption {corruption}"
            );
        }
    }

    #[test]
    fn source_budget_or_bad_append_fails_explicitly_without_serving_stale_records() {
        for limit in 0..3 {
            let mut store = Store::new().unwrap();
            put(&mut store, "child", tool("old", "preserved", ""));
            match limit {
                0 => {
                    store
                        .db
                        .execute("UPDATE child_tool_budget SET bytes=?1", [STORE_BYTES])
                        .unwrap();
                }
                1 => {
                    store
                        .db
                        .execute("UPDATE child_tool_budget SET items=?1", [STORE_ITEMS])
                        .unwrap();
                }
                _ => {
                    store
                        .db
                        .execute("UPDATE child_tool_budget SET chunks=?1", [STORE_CHUNKS])
                        .unwrap();
                }
            }
            let warnings = store
                .apply(Event::SubagentItem {
                    id: "child".into(),
                    item: tool("new", "over budget", ""),
                })
                .unwrap();
            assert!(
                warnings
                    .iter()
                    .any(|event| matches!(event, Event::Error { fatal: false, .. }))
            );
            assert!(
                store
                    .child_adapter_item(&CanonicalItemKey::new(Some("child"), "old"))
                    .is_err()
            );
            assert_eq!(
                store
                    .db
                    .query_row("SELECT count(*) FROM child_tools WHERE id='old'", [], |r| r
                        .get::<_, usize>(0))
                    .unwrap(),
                1
            );
            assert!(store.pending_child_tool.is_none());
            store
                .apply(Event::Reset {
                    reason: "new conversation".into(),
                })
                .unwrap();
            put(&mut store, "child", tool("fresh", "exact", ""));
            assert!(read(&mut store, "child", "fresh").is_some());
        }
        let mut store = Store::new().unwrap();
        put(&mut store, "child", tool("tool", "known", ""));
        store
            .apply(Event::SubagentOutputDelta {
                id: "child".into(),
                item_id: "tool".into(),
                offset: 99,
                delta: "lost prefix".into(),
            })
            .unwrap();
        let key = CanonicalItemKey::new(Some("child"), "tool");
        // CanonicalItemReader::read flushes the matching batch before reading
        // committed SQL. Exercise that boundary rather than the raw SQL helper.
        assert!(store.flush_child_adapter_read(&key).is_err());
        assert!(store.child_adapter_item(&key).is_err());
        assert!(store.child_tools_unavailable.is_some());
    }

    #[test]
    fn failed_reset_keeps_revocation_and_the_other_exact_source_atomic() {
        let mut store = Store::new().unwrap();
        put(&mut store, "child", tool("removed", "private", ""));
        store
            .apply(Event::SubagentItemsRemoved {
                id: "child".into(),
                ids: vec!["removed".into()],
            })
            .unwrap();
        put(&mut store, "child", tool("other", "retained", ""));
        store.db.execute_batch("CREATE TRIGGER reject_source_reset BEFORE DELETE ON child_tool_revoked BEGIN SELECT RAISE(ABORT,'injected reset refusal'); END;").unwrap();
        assert!(
            store
                .apply(Event::Reset {
                    reason: "failed reset".into()
                })
                .is_err()
        );
        assert!(store.child_tools_have_revocations);
        assert!(
            store
                .child_adapter_item(&CanonicalItemKey::new(Some("child"), "removed"))
                .is_err()
        );
        assert!(read(&mut store, "child", "other").is_some());
        store
            .db
            .execute_batch("DROP TRIGGER reject_source_reset")
            .unwrap();
        store
            .apply(Event::Reset {
                reason: "successful reset".into(),
            })
            .unwrap();
        assert!(!store.child_tools_have_revocations);
        assert!(read(&mut store, "child", "removed").is_none());
    }

    #[test]
    fn canonical_child_reader_keeps_no_store_lease_or_second_transcript() {
        use std::os::unix::fs::PermissionsExt;
        let store = Arc::new(Mutex::new(Store::new().unwrap()));
        let weak = Arc::downgrade(&store);
        let reader = super::super::adapter_item_reader(&store);
        let directory = store.lock().unwrap().directory.clone();
        put(
            &mut store.lock().unwrap(),
            "child",
            tool("tool", "full", "result"),
        );
        assert!(
            reader
                .read(&CanonicalItemKey::new(Some("child"), "tool"))
                .unwrap()
                .is_some()
        );
        assert_eq!(
            std::fs::metadata(directory.join("transcript.sqlite"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            store
                .lock()
                .unwrap()
                .db
                .query_row("SELECT count(*) FROM items", [], |r| r.get::<_, usize>(0))
                .unwrap(),
            0
        );
        drop(store);
        assert!(weak.upgrade().is_none());
        assert!(!directory.exists());
        assert!(
            reader
                .read(&CanonicalItemKey::new(Some("child"), "tool"))
                .is_err()
        );
    }
    #[test]
    fn kind_change_fences_known_tools_but_unknown_removal_does_not_allocate_an_identity() {
        let mut store = Store::new().unwrap();
        store
            .apply(Event::SubagentItemsRemoved {
                id: "child".into(),
                ids: vec!["fresh".into()],
            })
            .unwrap();
        assert_eq!(
            store
                .db
                .query_row("SELECT count(*) FROM child_tool_revoked", [], |r| r
                    .get::<_, usize>(0))
                .unwrap(),
            0
        );
        put(&mut store, "child", tool("fresh", "new full source", ""));
        assert!(read(&mut store, "child", "fresh").is_some());
        let prose = Item {
            content: ItemContent::Text {
                text: "replacement".into(),
                thinking: false,
                phase: Phase::Final,
            },
            ..tool("fresh", "", "")
        };
        put(&mut store, "child", prose);
        store
            .apply(Event::SubagentItemsRemoved {
                id: "child".into(),
                ids: vec!["fresh".into()],
            })
            .unwrap();
        assert!(
            store
                .validate_child_adapter_key(&CanonicalItemKey::new(Some("child"), "fresh"))
                .is_err()
        );
        let late = store
            .apply(Event::SubagentItem {
                id: "child".into(),
                item: tool("fresh", "old source", "late"),
            })
            .unwrap();
        assert!(
            !late
                .iter()
                .any(|event| matches!(event, Event::SubagentItem { .. }))
        );
        assert_eq!(
            store
                .db
                .query_row(
                    "SELECT count(*) FROM child_tools WHERE id='fresh'",
                    [],
                    |r| r.get::<_, usize>(0)
                )
                .unwrap(),
            0
        );
    }
}
