//! Ingestion-owned edit metadata. Patch bytes remain pageable SQLite chunks.
use super::Store;
use crate::chat::{
    Command, Event, FileChangeKey, FileChangePage, FileChangePatch, FileChangeSource,
    FileChangeSummary, FileChangesCoverage, FileChangesSummary, FilePatchCursor, Item, ItemContent,
    Snapshot, ToolStatus,
};
use rusqlite::{OptionalExtension, params};

const FILES: usize = 128;
const PATCHES: usize = 32;
const PATCH_BYTES: usize = 256 * 1024;

pub(super) const SCHEMA: &str = "
CREATE TABLE file_edits(source TEXT NOT NULL,item_id TEXT NOT NULL,file_index INTEGER NOT NULL,
 ordinal INTEGER NOT NULL,path TEXT NOT NULL,kind BLOB NOT NULL,status BLOB NOT NULL,
 imported INTEGER NOT NULL,additions INTEGER,deletions INTEGER,start INTEGER NOT NULL,len INTEGER NOT NULL,
 PRIMARY KEY(source,item_id,file_index));
CREATE INDEX file_edits_path ON file_edits(source,path,ordinal,file_index);
CREATE INDEX file_edits_status ON file_edits(source,path,status,ordinal,file_index);
CREATE TABLE file_totals(source TEXT NOT NULL,path TEXT NOT NULL,succeeded INTEGER NOT NULL,
 pending INTEGER NOT NULL,failed INTEGER NOT NULL,imported INTEGER NOT NULL,
 additions INTEGER NOT NULL,deletions INTEGER NOT NULL,unknown INTEGER NOT NULL,revision INTEGER NOT NULL,resolved TEXT,
 PRIMARY KEY(source,path));
CREATE INDEX file_totals_imported ON file_totals(imported);
CREATE TABLE file_paths(identity TEXT PRIMARY KEY,applied INTEGER NOT NULL);
CREATE TABLE file_patch_chunks(source TEXT NOT NULL,item_id TEXT NOT NULL,file_index INTEGER NOT NULL,
 position INTEGER NOT NULL,data BLOB NOT NULL,PRIMARY KEY(source,item_id,file_index,position),
 FOREIGN KEY(source,item_id,file_index) REFERENCES file_edits(source,item_id,file_index) ON DELETE CASCADE);";

fn source_key(source: &FileChangeSource) -> String {
    match source {
        FileChangeSource::Conversation => String::new(),
        FileChangeSource::Subagent { id } => format!("c:{id}"),
    }
}
fn source(value: String) -> FileChangeSource {
    match value.strip_prefix("c:") {
        Some(id) => FileChangeSource::Subagent { id: id.into() },
        None => FileChangeSource::Conversation,
    }
}
/// Only unified hunk lines represent additions/deletions. An empty/opaque patch
/// has unknown counts; a recognizable zero-edit hunk has known zero counts.
fn counts(patch: &str) -> Option<(u64, u64)> {
    fn range(text: &str) -> Option<u64> {
        let (start, count) = text.split_once(',').unwrap_or((text, "1"));
        start.parse::<u64>().ok()?;
        count.parse().ok()
    }
    let mut remaining = (0_u64, 0_u64);
    let mut recognized = false;
    let (mut added, mut deleted) = (0, 0);
    for line in patch.lines() {
        if let Some(header) = line.strip_prefix("@@ -") {
            if remaining != (0, 0) {
                return None;
            }
            let (old, new) = header.split_once(" +")?;
            let new = new.split_once(" @@")?.0;
            remaining = (range(old)?, range(new)?);
            recognized = true;
        } else if remaining != (0, 0) {
            match line.as_bytes().first()? {
                b'+' => {
                    remaining.1 = remaining.1.checked_sub(1)?;
                    added += 1;
                }
                b'-' => {
                    remaining.0 = remaining.0.checked_sub(1)?;
                    deleted += 1;
                }
                b' ' => {
                    remaining.0 = remaining.0.checked_sub(1)?;
                    remaining.1 = remaining.1.checked_sub(1)?;
                }
                b'\\' => {} // \ No newline at end of file
                _ => return None,
            }
        } else if (line.starts_with('+') && !line.starts_with("+++ "))
            || (line.starts_with('-') && !line.starts_with("--- "))
        {
            // Declared hunk lengths must account for every edit line. File
            // headers outside hunks are metadata; extra edits are malformed.
            return None;
        }
    }
    (recognized && remaining == (0, 0)).then_some((added, deleted))
}

fn kind_bytes(kind: &crate::chat::ChangeKind) -> usize {
    match kind {
        crate::chat::ChangeKind::Renamed { from } => from.len(),
        _ => 0,
    }
}

impl Store {
    /// Apply signed deltas instead of regrouping every old patch on each streamed
    /// replacement. Removing an item's old rows exactly reverses its contribution.
    fn resolved_file(&self, source: &str, path: &str) -> Option<String> {
        use std::path::{Component, Path, PathBuf};
        let path = Path::new(path);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else if source.is_empty() {
            self.file_cwd.as_ref()?.join(path)
        } else {
            return None;
        };
        if !absolute.is_absolute() {
            return None;
        }
        let mut normalized = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::CurDir => {}
                // ParentDir cannot be collapsed lexically: a preceding
                // symlink changes which parent the kernel actually traverses.
                Component::ParentDir => normalized.push(".."),
                other => normalized.push(other.as_os_str()),
            }
        }
        Some(normalized.to_string_lossy().into_owned())
    }
    fn adjust_file_total(
        &mut self,
        source: &str,
        path: &str,
        status: ToolStatus,
        imported: bool,
        counts: Option<(u64, u64)>,
        sign: i64,
    ) -> Result<(), String> {
        let succeeded = i64::from(status == ToolStatus::Succeeded);
        let pending = i64::from(status == ToolStatus::Running);
        let failed = i64::from(succeeded == 0 && pending == 0);
        let before:Option<(i64,i64,Option<String>)>=self.db.query_row("SELECT succeeded,succeeded+pending+failed,resolved FROM file_totals WHERE source=?1 AND path=?2",params![source,path],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|e|e.to_string())?;
        let (old_succeeded, old_count, resolved) =
            before.unwrap_or_else(|| (0, 0, self.resolved_file(source, path)));
        let new_succeeded = old_succeeded + sign * succeeded;
        let new_count = old_count + sign;
        if new_succeeded < 0 || new_count < 0 {
            return Err("File contribution count underflow".into());
        }
        if old_count == 0 && new_count > 0 {
            self.file_keys += 1;
        }
        if old_count > 0 && new_count == 0 {
            self.file_keys -= 1;
        }
        if (old_succeeded > 0) != (new_succeeded > 0) {
            // Known paths count once across explicit owner rows. Relative child
            // paths have no known cwd and retain an opaque scope-specific identity.
            let identity = resolved
                .as_ref()
                .map(|p| format!("p:{p}"))
                .unwrap_or_else(|| {
                    format!("u:{}", serde_json::to_string(&(source, path)).unwrap())
                });
            let old: i64 = self
                .db
                .query_row(
                    "SELECT applied FROM file_paths WHERE identity=?1",
                    [&identity],
                    |r| r.get(0),
                )
                .optional()
                .map_err(|e| e.to_string())?
                .unwrap_or(0);
            let new = old + if new_succeeded > 0 { 1 } else { -1 };
            if new < 0 {
                return Err("Applied file count underflow".into());
            }
            if old == 0 && new > 0 {
                self.applied_files += 1;
            }
            if old > 0 && new == 0 {
                self.applied_files -= 1;
            }
            if new == 0 {
                self.db
                    .execute("DELETE FROM file_paths WHERE identity=?1", [identity])
                    .map_err(|e| e.to_string())?;
            } else {
                self.db.execute("INSERT INTO file_paths VALUES(?1,?2) ON CONFLICT(identity) DO UPDATE SET applied=excluded.applied",params![identity,new]).map_err(|e|e.to_string())?;
            }
        }
        if new_count == 0 {
            self.db
                .execute(
                    "DELETE FROM file_totals WHERE source=?1 AND path=?2",
                    params![source, path],
                )
                .map_err(|e| e.to_string())?;
        } else {
            let (added, deleted) = counts.unwrap_or_default();
            self.db.execute("INSERT INTO file_totals VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11)
                ON CONFLICT(source,path) DO UPDATE SET succeeded=succeeded+excluded.succeeded,
                pending=pending+excluded.pending,failed=failed+excluded.failed,imported=imported+excluded.imported,
                additions=additions+excluded.additions,deletions=deletions+excluded.deletions,unknown=unknown+excluded.unknown,revision=excluded.revision",
                params![source,path,sign*succeeded,sign*pending,sign*failed,sign*i64::from(imported),sign*succeeded*added as i64,sign*succeeded*deleted as i64,
                    sign*succeeded*i64::from(counts.is_none()),self.file_revision.saturating_add(1),resolved]).map_err(|e|e.to_string())?;
        }
        Ok(())
    }
    fn remove_file_item(&mut self, source: &str, id: &str) -> Result<bool, String> {
        let rows = {
            let mut statement = self.db.prepare_cached("SELECT path,status,imported,additions,deletions FROM file_edits WHERE source=?1 AND item_id=?2").map_err(|e| e.to_string())?;
            statement
                .query_map(params![source, id], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, bool>(2)?,
                        row.get::<_, Option<u64>>(3)?,
                        row.get::<_, Option<u64>>(4)?,
                    ))
                })
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?
        };
        for (path, status, imported, added, deleted) in &rows {
            let status = postcard::from_bytes(status).map_err(|e| e.to_string())?;
            self.adjust_file_total(source, path, status, *imported, added.zip(*deleted), -1)?;
        }
        self.db
            .execute(
                "DELETE FROM file_edits WHERE source=?1 AND item_id=?2",
                params![source, id],
            )
            .map_err(|e| e.to_string())?;
        for (path, _, _, _, _) in &rows {
            self.db.execute("DELETE FROM file_totals WHERE source=?1 AND path=?2 AND succeeded+pending+failed=0",params![source,path]).map_err(|e|e.to_string())?;
        }
        Ok(!rows.is_empty())
    }
    /// `body`/`spans` are the already-extracted main item patch body. Child
    /// patches have their own chunks because child resident windows can evict.
    pub(super) fn index_file_item(
        &mut self,
        item: &Item,
        source: &str,
        ordinal: i64,
        imported: bool,
        body: &str,
        spans: &[usize],
    ) -> Result<(), String> {
        self.db
            .execute_batch("SAVEPOINT file_index")
            .map_err(|e| e.to_string())?;
        let revision = self.file_revision;
        let counts = (self.file_keys, self.applied_files);
        match self.index_file_item_inner(item, source, ordinal, imported, body, spans) {
            Ok(()) => self
                .db
                .execute_batch("RELEASE file_index")
                .map_err(|e| e.to_string()),
            Err(error) => {
                let _ = self
                    .db
                    .execute_batch("ROLLBACK TO file_index; RELEASE file_index");
                self.file_revision = revision;
                (self.file_keys, self.applied_files) = counts;
                Err(error)
            }
        }
    }
    fn index_file_item_inner(
        &mut self,
        item: &Item,
        source: &str,
        ordinal: i64,
        imported: bool,
        body: &str,
        spans: &[usize],
    ) -> Result<(), String> {
        let mut changed = self.remove_file_item(source, &item.id)?;
        if let ItemContent::Changes { changes, status } = &item.content {
            let identity =
                changes
                    .iter()
                    .fold(source.len().saturating_add(item.id.len()), |bytes, c| {
                        bytes
                            .saturating_add(c.path.len())
                            .saturating_add(kind_bytes(&c.kind))
                            .saturating_add(128)
                    });
            if identity > super::METADATA_LIMIT || item.id.is_empty()
                || changes.iter().any(|c|c.path.is_empty()||c.path.contains('\0')||matches!(&c.kind,crate::chat::ChangeKind::Renamed{from} if from.is_empty()||from.contains('\0'))) {
                self.file_index_incomplete = true;
                if changed {
                    self.file_revision = self.file_revision.saturating_add(1);
                }
                return Ok(());
            }
            let mut start = 0;
            for (index, change) in changes.iter().enumerate() {
                let length = spans.get(index).copied().unwrap_or(change.diff.len());
                let patch = if source.is_empty() {
                    &body[start..start + length]
                } else {
                    &change.diff
                };
                let count = counts(patch);
                self.db
                    .execute(
                        "INSERT INTO file_edits VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
                        params![
                            source,
                            item.id,
                            index as i64,
                            ordinal,
                            change.path,
                            postcard::to_stdvec(&change.kind).map_err(|e| e.to_string())?,
                            postcard::to_stdvec(status).map_err(|e| e.to_string())?,
                            imported,
                            count.map(|v| v.0),
                            count.map(|v| v.1),
                            start as i64,
                            length as i64
                        ],
                    )
                    .map_err(|e| e.to_string())?;
                self.adjust_file_total(source, &change.path, *status, imported, count, 1)?;
                if !source.is_empty() {
                    for (part, bytes) in patch.as_bytes().chunks(super::PART_BYTES).enumerate() {
                        self.db
                            .execute(
                                "INSERT INTO file_patch_chunks VALUES(?1,?2,?3,?4,?5)",
                                params![
                                    source,
                                    item.id,
                                    index as i64,
                                    (part * super::PART_BYTES) as i64,
                                    bytes
                                ],
                            )
                            .map_err(|e| e.to_string())?;
                    }
                }
                start += length;
                changed = true;
            }
        }
        if changed {
            self.file_revision = self.file_revision.saturating_add(1);
        }
        Ok(())
    }
    pub(super) fn finish_file_status(
        &mut self,
        source: &str,
        id: &str,
        status: ToolStatus,
    ) -> Result<(), String> {
        let old = postcard::to_stdvec(&ToolStatus::Running).map_err(|error| error.to_string())?;
        let rows = {
            let mut query=self.db.prepare("SELECT path,imported,additions,deletions FROM file_edits WHERE source=?1 AND item_id=?2 AND status=?3").map_err(|error|error.to_string())?;
            query
                .query_map(params![source, id, old], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, bool>(1)?,
                        row.get::<_, Option<u64>>(2)?,
                        row.get::<_, Option<u64>>(3)?,
                    ))
                })
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?
        };
        for (path, imported, added, deleted) in &rows {
            self.adjust_file_total(
                source,
                path,
                ToolStatus::Running,
                *imported,
                added.zip(*deleted),
                -1,
            )?;
            self.adjust_file_total(source, path, status, *imported, added.zip(*deleted), 1)?;
        }
        if !rows.is_empty() {
            self.db
                .execute(
                    "UPDATE file_edits SET status=?4 WHERE source=?1 AND item_id=?2 AND status=?3",
                    params![
                        source,
                        id,
                        old,
                        postcard::to_stdvec(&status).map_err(|error| error.to_string())?
                    ],
                )
                .map_err(|error| error.to_string())?;
            self.file_revision = self.file_revision.saturating_add(1);
        }
        Ok(())
    }
    pub(super) fn index_child_event(&mut self, event: &Event) -> Result<(), String> {
        match event {
            Event::SubagentItem { id, item } => {
                self.child_files_seen = true;
                let key = format!("c:{id}");
                let old: Option<i64> = self
                    .db
                    .query_row(
                        "SELECT ordinal FROM file_edits WHERE source=?1 AND item_id=?2 LIMIT 1",
                        params![key, item.id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                let ordinal = match old {
                    Some(value) => value,
                    None => {
                        self.child_file_ordinal = self.child_file_ordinal.saturating_add(1);
                        self.child_file_ordinal
                    }
                };
                self.index_file_item(item, &key, ordinal, false, "", &[])?;
            }
            Event::SubagentItemsRemoved { id, ids } => {
                for item in ids {
                    if self.remove_file_item(&format!("c:{id}"), item)? {
                        self.file_revision = self.file_revision.saturating_add(1);
                    }
                }
            }
            _ => {}
        }
        Ok(())
    }
    pub(super) fn files_removed(&mut self, ids: &[String]) -> Result<(), String> {
        for id in ids {
            if self.remove_file_item("", id)? {
                self.file_revision = self.file_revision.saturating_add(1);
            }
        }
        Ok(())
    }
    pub(super) fn files_reset(&mut self) -> Result<(), String> {
        self.db
            .execute("DELETE FROM file_edits", [])
            .map_err(|e| e.to_string())?;
        self.db
            .execute("DELETE FROM file_totals", [])
            .map_err(|e| e.to_string())?;
        self.file_revision = self.file_revision.saturating_add(1);
        self.child_files_seen = false;
        self.file_index_incomplete = false;
        self.db
            .execute("DELETE FROM file_paths", [])
            .map_err(|e| e.to_string())?;
        self.file_keys = 0;
        self.applied_files = 0;
        Ok(())
    }
    pub(crate) fn publish_files(&mut self, state: &mut Snapshot) -> Result<(), String> {
        if state.file_changes.revision != self.file_revision
            || state.file_changes.coverage.has_unloaded_history != self.provider_earlier
            || state.file_changes.coverage.has_incomplete_delegated_history != self.child_files_seen
            || state.file_changes.coverage.has_uncached_changes != self.file_index_incomplete
        {
            state.file_changes = self.files_summary(0, None)?.into();
        }
        Ok(())
    }
    fn files_summary(
        &self,
        request_id: u64,
        after: Option<&FileChangeKey>,
    ) -> Result<FileChangesSummary, String> {
        let (after_source, after_path) = after
            .map(|k| (source_key(&k.source), k.path.clone()))
            .unwrap_or_default();
        let mut statement=self.db.prepare_cached("SELECT source,path,succeeded,pending,failed,imported,additions,deletions,unknown,
            COALESCE((SELECT kind FROM file_edits e WHERE e.source=t.source AND e.path=t.path AND status=?5 ORDER BY ordinal DESC,file_index DESC LIMIT 1),
            (SELECT kind FROM file_edits e WHERE e.source=t.source AND e.path=t.path ORDER BY ordinal DESC,file_index DESC LIMIT 1)),revision,resolved
            FROM file_totals t WHERE (?1=0 OR (source,path)>(?2,?3)) ORDER BY source,path LIMIT ?4").map_err(|e|e.to_string())?;
        let rows = statement
            .query_map(
                params![
                    after.is_some(),
                    after_source,
                    after_path,
                    FILES as i64 + 1,
                    postcard::to_stdvec(&ToolStatus::Succeeded).map_err(|e| e.to_string())?
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, u64>(2)?,
                        r.get::<_, u64>(3)?,
                        r.get::<_, u64>(4)?,
                        r.get::<_, u64>(5)?,
                        r.get::<_, u64>(6)?,
                        r.get::<_, u64>(7)?,
                        r.get::<_, u64>(8)?,
                        r.get::<_, Vec<u8>>(9)?,
                        r.get::<_, u64>(10)?,
                        r.get::<_, Option<String>>(11)?,
                    ))
                },
            )
            .map_err(|e| e.to_string())?;
        let mut files = Vec::new();
        let mut bytes = 128;
        let mut has_more = false;
        for row in rows {
            let (
                source_id,
                path,
                succeeded,
                pending,
                failed,
                imported,
                added,
                deleted,
                unknown,
                kind,
                revision,
                resolved_path,
            ) = row.map_err(|e| e.to_string())?;
            let file = FileChangeSummary {
                ownership_unverified: imported > 0 || !source_id.is_empty(),
                resolved_path,
                revision,
                key: FileChangeKey {
                    source: source(source_id),
                    path,
                },
                kind: postcard::from_bytes(&kind).map_err(|e| e.to_string())?,
                succeeded,
                pending,
                failed,
                imported,
                additions: (succeeded > 0 && unknown == 0).then_some(added),
                deletions: (succeeded > 0 && unknown == 0).then_some(deleted),
            };
            let size = postcard::to_stdvec(&file).map_err(|e| e.to_string())?.len() + 128;
            if files.len() == FILES || bytes + size > PATCH_BYTES {
                has_more = true;
                break;
            }
            bytes += size;
            files.push(file);
        }
        Ok(FileChangesSummary {
            request_id,
            revision: self.file_revision,
            files,
            total_files: self.file_keys,
            applied_files: self.applied_files,
            has_more,
            coverage: FileChangesCoverage {
                has_unloaded_history: self.provider_earlier,
                has_incomplete_delegated_history: self.child_files_seen,
                has_uncached_changes: self.file_index_incomplete,
                has_imported_history: self
                    .db
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM file_totals WHERE imported>0)",
                        [],
                        |r| r.get(0),
                    )
                    .map_err(|e| e.to_string())?,
            },
        })
    }
    pub(crate) fn file_query(
        &self,
        command: &Command,
    ) -> Result<Option<crate::chat::FileChangeReply>, String> {
        let requested_key = match command {
            Command::LoadFileChanges { after, .. } => after.as_ref(),
            Command::LoadFileChangePatches { key, .. } => Some(key),
            _ => return Ok(None),
        };
        if requested_key.is_some_and(|key| {
            source_key(&key.source).len().saturating_add(key.path.len()) > super::METADATA_LIMIT
        }) {
            return Err("File identity exceeds the cached edit metadata limit".into());
        }
        let reply = match command {
            Command::LoadFileChanges { request_id, after } => crate::chat::FileChangeReply::Files(
                self.files_summary(*request_id, after.as_ref())?,
            ),
            Command::LoadFileChangePatches {
                request_id,
                key,
                cursor,
                revision,
            } => {
                let mut page = FileChangePage {
                    request_id: *request_id,
                    key: key.clone(),
                    revision: self
                        .db
                        .query_row(
                            "SELECT revision FROM file_totals WHERE source=?1 AND path=?2",
                            params![source_key(&key.source), key.path],
                            |r| r.get(0),
                        )
                        .optional()
                        .map_err(|e| e.to_string())?
                        .unwrap_or(0),
                    patches: Vec::new(),
                    next: None,
                    error: None,
                };
                if cursor.is_some() && *revision != page.revision {
                    page.error = Some(
                        "Reported edits changed; select the file again to read its current patches"
                            .into(),
                    );
                } else if let Err(error) = self.read_file_patches(&mut page, cursor.as_ref()) {
                    page.error = Some(error);
                }
                crate::chat::FileChangeReply::Patches(page)
            }
            _ => unreachable!(),
        };
        Ok(Some(reply))
    }
    fn read_file_patches(
        &self,
        page: &mut FileChangePage,
        cursor: Option<&FilePatchCursor>,
    ) -> Result<(), String> {
        let key = source_key(&page.key.source);
        let ordinal = cursor.map_or(i64::MIN, |c| c.ordinal);
        let index = cursor.map_or(0, |c| c.file_index);
        if let Some(cursor) = cursor {
            if cursor.file_index > i64::MAX as usize {
                return Err("Invalid patch continuation index".into());
            }
            let exists:bool=self.db.query_row("SELECT EXISTS(SELECT 1 FROM file_edits WHERE source=?1 AND path=?2 AND ordinal=?3 AND file_index=?4 AND len>=?5)",params![key,page.key.path,cursor.ordinal,cursor.file_index as i64,i64::try_from(cursor.offset).map_err(|_|"Invalid patch continuation offset")?],|r|r.get(0)).map_err(|e|e.to_string())?;
            if !exists {
                return Err("Patch continuation does not name a cached edit".into());
            }
        }

        let mut statement=self.db.prepare_cached("SELECT item_id,file_index,ordinal,kind,status,imported,start,len FROM file_edits WHERE source=?1 AND path=?2 AND (ordinal,file_index)>=(?3,?4) ORDER BY ordinal,file_index LIMIT ?5").map_err(|e|e.to_string())?;
        let records = statement
            .query_map(
                params![
                    key,
                    page.key.path,
                    ordinal,
                    index as i64,
                    PATCHES as i64 + 1
                ],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, usize>(1)?,
                        r.get::<_, i64>(2)?,
                        r.get::<_, Vec<u8>>(3)?,
                        r.get::<_, Vec<u8>>(4)?,
                        r.get::<_, bool>(5)?,
                        r.get::<_, usize>(6)?,
                        r.get::<_, usize>(7)?,
                    ))
                },
            )
            .map_err(|e| e.to_string())?
            .collect::<Result<Vec<_>, _>>()
            .map_err(|e| e.to_string())?;
        let mut remaining = PATCH_BYTES.saturating_sub(key.len() + page.key.path.len() + 256);
        for (id, index, ordinal, kind, status, imported, start, length) in records {
            let offset = cursor
                .filter(|c| c.ordinal == ordinal && c.file_index == index)
                .map_or(0, |c| c.offset);
            if offset > length {
                return Err("Invalid patch continuation offset".into());
            }
            let metadata_bytes = id.len() + kind.len() + status.len() + 128;
            if remaining <= metadata_bytes || page.patches.len() == PATCHES {
                page.next = Some(FilePatchCursor {
                    ordinal,
                    file_index: index,
                    offset,
                });
                break;
            }
            remaining -= metadata_bytes;
            let end = offset.saturating_add(remaining).min(length);
            let mut bytes = if key.is_empty() {
                self.field(&id, 0, start + offset, start + end)?
            } else {
                let mut statement=self.db.prepare_cached("SELECT substr(data,MAX(?4-position,0)+1,MIN(position+length(data),?5)-MAX(position,?4)) FROM file_patch_chunks WHERE source=?1 AND item_id=?2 AND file_index=?3 AND position>=COALESCE((SELECT MAX(position) FROM file_patch_chunks WHERE source=?1 AND item_id=?2 AND file_index=?3 AND position<=?4),?4) AND position<?5 AND position+length(data)>?4 ORDER BY position").map_err(|e|e.to_string())?;
                let rows = statement
                    .query_map(
                        params![key, id, index as i64, offset as i64, end as i64],
                        |r| r.get::<_, Vec<u8>>(0),
                    )
                    .map_err(|e| e.to_string())?;
                let mut bytes = Vec::with_capacity(end - offset);
                for row in rows {
                    bytes.extend(row.map_err(|e| e.to_string())?);
                }
                bytes
            };
            // A byte limit can land inside UTF-8; keep the cursor at the last
            // complete character. Never replace bytes and lose offset fidelity.
            let valid = match std::str::from_utf8(&bytes) {
                Ok(_) => bytes.len(),
                Err(e) if e.error_len().is_none() => e.valid_up_to(),
                Err(_) => return Err("Invalid UTF-8 patch cursor".into()),
            };
            bytes.truncate(valid);
            if valid == 0 && end > offset {
                if page.patches.is_empty() {
                    return Err("Invalid patch continuation boundary".into());
                }
                page.next = Some(FilePatchCursor {
                    ordinal,
                    file_index: index,
                    offset,
                });
                break;
            }
            remaining -= valid;
            page.patches.push(FileChangePatch {
                ownership_unverified: imported || !key.is_empty(),
                item_id: id,
                kind: postcard::from_bytes(&kind).map_err(|e| e.to_string())?,
                status: postcard::from_bytes(&status).map_err(|e| e.to_string())?,
                imported,
                offset,
                total_bytes: length,
                diff: String::from_utf8(bytes).map_err(|e| e.to_string())?,
            });
            if offset + valid < length {
                page.next = Some(FilePatchCursor {
                    ordinal,
                    file_index: index,
                    offset: offset + valid,
                });
                break;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChangeKind, FileChange, Role};
    fn item(id: &str, path: &str, diff: &str, status: ToolStatus, kind: ChangeKind) -> Item {
        Item {
            id: id.into(),
            role: Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Changes {
                changes: vec![FileChange {
                    path: path.into(),
                    diff: diff.into(),
                    kind,
                }],
                status,
            },
        }
    }
    fn key(path: &str) -> FileChangeKey {
        FileChangeKey {
            source: FileChangeSource::Conversation,
            path: path.into(),
        }
    }
    fn summary(store: &mut Store) -> FileChangesSummary {
        let mut state = Snapshot::default();
        store.publish_files(&mut state).unwrap();
        state.file_changes.as_ref().clone()
    }
    fn page(
        store: &mut Store,
        key: FileChangeKey,
        cursor: Option<FilePatchCursor>,
        revision: u64,
    ) -> FileChangePage {
        let reply = store
            .file_query(&Command::LoadFileChangePatches {
                request_id: 9,
                key,
                cursor,
                revision,
            })
            .unwrap();
        match reply.unwrap() {
            crate::chat::FileChangeReply::Patches(page) => page,
            _ => panic!("expected patches"),
        }
    }
    const DIFF: &str = "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1,2 @@\n-old\n+new\n+another\n";
    #[test]
    fn file_changes_replace_remove_and_replay_stable_contributions() {
        let mut store = Store::new().unwrap();
        let edit = item(
            "edit",
            "a.rs",
            DIFF,
            ToolStatus::Running,
            ChangeKind::Updated,
        );
        store.apply(Event::Item(edit.clone())).unwrap();
        let first = summary(&mut store);
        assert_eq!(first.applied_files, 0);
        assert_eq!(first.files[0].pending, 1);
        let succeeded = item(
            "edit",
            "a.rs",
            DIFF,
            ToolStatus::Succeeded,
            ChangeKind::Added,
        );
        store.apply(Event::Item(succeeded.clone())).unwrap();
        store
            .apply(Event::History {
                items: vec![edit, succeeded],
                has_earlier: false,
            })
            .unwrap();
        let result = summary(&mut store);
        assert_eq!(result.applied_files, 1);
        assert_eq!(result.files[0].succeeded, 1);
        assert_eq!(
            (result.files[0].additions, result.files[0].deletions),
            (Some(2), Some(1))
        );
        assert_eq!(result.files[0].imported, 0);
        for (id, status) in [
            ("pending", ToolStatus::Running),
            ("failed", ToolStatus::Failed),
        ] {
            store
                .apply(Event::Item(item(
                    id,
                    "a.rs",
                    DIFF,
                    status,
                    ChangeKind::Deleted,
                )))
                .unwrap();
        }
        let row = summary(&mut store).files.remove(0);
        assert_eq!(row.kind, ChangeKind::Added);
        assert_eq!((row.succeeded, row.pending, row.failed), (1, 1, 1));
        store
            .apply(Event::ItemsRemoved {
                ids: vec!["edit".into()],
            })
            .unwrap();
        assert_eq!(summary(&mut store).applied_files, 0);
        store
            .apply(Event::Reset {
                reason: "reset".into(),
            })
            .unwrap();
        assert_eq!(summary(&mut store).total_files, 0);
    }
    #[test]
    fn file_changes_survive_transcript_eviction_and_preserve_history_and_delegate_scope() {
        let mut store = Store::new().unwrap();
        store
            .apply(Event::History {
                items: vec![item(
                    "same",
                    "a.rs",
                    DIFF,
                    ToolStatus::Succeeded,
                    ChangeKind::Updated,
                )],
                has_earlier: true,
            })
            .unwrap();
        store
            .apply(Event::SubagentItem {
                id: "delegate".into(),
                item: item(
                    "same",
                    "a.rs",
                    DIFF,
                    ToolStatus::Succeeded,
                    ChangeKind::Added,
                ),
            })
            .unwrap();
        for n in 0..150 {
            store
                .apply(Event::Item(Item {
                    id: format!("text{n}"),
                    role: Role::Assistant,
                    content: ItemContent::Text {
                        text: "text".into(),
                        thinking: false,
                        phase: crate::chat::Phase::Final,
                    },
                    started_at: None,
                    at: None,
                }))
                .unwrap();
        }
        let window = store.window().unwrap();
        assert!(window.items.iter().all(|i| i.id != "same"));
        let cached_summary = summary(&mut store);
        assert_eq!(cached_summary.total_files, 2);
        assert_eq!(cached_summary.applied_files, 2);
        assert!(
            cached_summary.coverage.has_unloaded_history
                && cached_summary.coverage.has_imported_history
                && cached_summary.coverage.has_incomplete_delegated_history
        );
        assert!(cached_summary.files.iter().all(|f| f.ownership_unverified));
        let child = FileChangeKey {
            source: FileChangeSource::Subagent {
                id: "delegate".into(),
            },
            path: "a.rs".into(),
        };
        let patch = page(&mut store, child, None, 0);
        assert_eq!(patch.patches[0].diff, DIFF);
        assert!(patch.patches[0].ownership_unverified);
        store
            .apply(Event::SubagentItemsRemoved {
                id: "delegate".into(),
                ids: vec!["same".into()],
            })
            .unwrap();
        assert_eq!(summary(&mut store).total_files, 1);
    }
    #[test]
    fn file_changes_continuations_ignore_unrelated_edits_but_reject_replacement() {
        let mut store = Store::new().unwrap();
        let diff = "λ".repeat(PATCH_BYTES);
        store
            .apply(Event::Item(item(
                "edit",
                "a.rs",
                &diff,
                ToolStatus::Succeeded,
                ChangeKind::Updated,
            )))
            .unwrap();
        let first = page(&mut store, key("a.rs"), None, 0);
        assert!(first.error.is_none());
        let next = first.next.clone().unwrap();
        assert!(postcard::to_stdvec(&first).unwrap().len() <= PATCH_BYTES);
        store
            .apply(Event::Item(item(
                "other",
                "b.rs",
                DIFF,
                ToolStatus::Succeeded,
                ChangeKind::Updated,
            )))
            .unwrap();
        let second = page(&mut store, key("a.rs"), Some(next.clone()), first.revision);
        assert!(second.error.is_none());
        assert_eq!(second.patches[0].offset, next.offset);
        store
            .apply(Event::Item(item(
                "edit",
                "a.rs",
                DIFF,
                ToolStatus::Succeeded,
                ChangeKind::Updated,
            )))
            .unwrap();
        let stale = page(&mut store, key("a.rs"), Some(next), first.revision);
        assert!(stale.error.is_some());
        assert!(stale.patches.is_empty());
        assert!(
            page(&mut store, key("a.rs"), None, first.revision)
                .error
                .is_none()
        );
    }
    #[test]
    fn file_changes_utf8_page_boundary_preserves_prior_records_and_progresses() {
        for child in [false, true] {
            let mut store = Store::new().unwrap();
            let selected = FileChangeKey {
                source: if child {
                    FileChangeSource::Subagent {
                        id: "delegate".into(),
                    }
                } else {
                    FileChangeSource::Conversation
                },
                path: "a.rs".into(),
            };
            let budget =
                PATCH_BYTES - source_key(&selected.source).len() - selected.path.len() - 256;
            let metadata = "first".len()
                + postcard::to_stdvec(&ChangeKind::Updated).unwrap().len()
                + postcard::to_stdvec(&ToolStatus::Succeeded).unwrap().len()
                + 128;
            let metadata2 = "second".len()
                + postcard::to_stdvec(&ChangeKind::Updated).unwrap().len()
                + postcard::to_stdvec(&ToolStatus::Succeeded).unwrap().len()
                + 128;
            let first = "a".repeat(budget - metadata - metadata2 - 1);
            for (id, diff) in [("first", first.as_str()), ("second", "🙂last")] {
                let edit = item(id, "a.rs", diff, ToolStatus::Succeeded, ChangeKind::Updated);
                store
                    .apply(if child {
                        Event::SubagentItem {
                            id: "delegate".into(),
                            item: edit,
                        }
                    } else {
                        Event::Item(edit)
                    })
                    .unwrap();
            }
            let one = page(&mut store, selected.clone(), None, 0);
            assert!(one.error.is_none());
            assert_eq!(one.patches.len(), 1);
            let next = one.next.clone().unwrap();
            assert_eq!(next.offset, 0);
            let two = page(&mut store, selected, Some(next), one.revision);
            assert!(two.error.is_none());
            assert_eq!(two.patches[0].diff, "🙂last");
            assert!(two.next.is_none());
        }
    }
    #[test]
    fn file_changes_rebalance_remaps_edit_order_and_invalidates_byte_cursors() {
        let mut store = Store::new().unwrap();
        for id in ["one", "two"] {
            store
                .apply(Event::Item(item(
                    id,
                    "a.rs",
                    DIFF,
                    ToolStatus::Succeeded,
                    ChangeKind::Updated,
                )))
                .unwrap();
        }
        let prior = page(&mut store, key("a.rs"), None, 0);
        store
            .db
            .execute(
                "UPDATE items SET ordinal=CASE id WHEN 'one' THEN 1 ELSE 2 END",
                [],
            )
            .unwrap();
        store
            .db
            .execute(
                "UPDATE file_edits SET ordinal=CASE item_id WHEN 'one' THEN 1 ELSE 2 END",
                [],
            )
            .unwrap();
        store.rebalance().unwrap();
        store
            .apply(Event::History {
                items: vec![
                    item(
                        "one",
                        "a.rs",
                        DIFF,
                        ToolStatus::Succeeded,
                        ChangeKind::Updated,
                    ),
                    item(
                        "middle",
                        "a.rs",
                        DIFF,
                        ToolStatus::Succeeded,
                        ChangeKind::Updated,
                    ),
                    item(
                        "two",
                        "a.rs",
                        DIFF,
                        ToolStatus::Succeeded,
                        ChangeKind::Updated,
                    ),
                ],
                has_earlier: false,
            })
            .unwrap();
        let current = page(&mut store, key("a.rs"), None, 0);
        assert_ne!(prior.revision, current.revision);
        assert_eq!(
            current
                .patches
                .iter()
                .map(|p| p.item_id.as_str())
                .collect::<Vec<_>>(),
            vec!["one", "middle", "two"]
        );
        let stale = page(
            &mut store,
            key("a.rs"),
            Some(FilePatchCursor {
                ordinal: 1,
                file_index: 0,
                offset: 0,
            }),
            prior.revision,
        );
        assert!(stale.error.is_some());
    }
    #[test]
    fn file_changes_lists_budget_metadata_and_refuse_colliding_truncated_keys() {
        let mut store = Store::new().unwrap();
        for n in 0..10 {
            store
                .apply(Event::Item(item(
                    &format!("item{n}"),
                    &format!("{n}{}", "p".repeat(30000)),
                    DIFF,
                    ToolStatus::Succeeded,
                    ChangeKind::Updated,
                )))
                .unwrap();
        }
        let first = summary(&mut store);
        assert_eq!(first.total_files, 10);
        assert!(first.has_more);
        assert!(first.files.len() < 10);
        assert!(postcard::to_stdvec(&first).unwrap().len() <= PATCH_BYTES);
        let second = store
            .file_query(&Command::LoadFileChanges {
                request_id: 2,
                after: Some(first.files.last().unwrap().key.clone()),
            })
            .unwrap();
        let second = match second.unwrap() {
            crate::chat::FileChangeReply::Files(page) => page,
            _ => panic!("expected files"),
        };
        assert_eq!(second.total_files, 10);
        assert_ne!(first.files[0].key, second.files[0].key);
        store
            .apply(Event::Item(item(
                "oversized",
                &"p".repeat(super::super::METADATA_LIMIT + 1),
                DIFF,
                ToolStatus::Succeeded,
                ChangeKind::Updated,
            )))
            .unwrap();
        let result = summary(&mut store);
        assert!(result.coverage.has_uncached_changes);
        assert_eq!(result.total_files, 10);
    }
    #[test]
    fn file_changes_counts_literal_header_like_source_and_rejects_partial_hunks() {
        assert_eq!(
            counts("@@ -1,2 +1,2 @@\n--- marker\n-old\n+++ marker\n+new\n"),
            Some((2, 2))
        );
        assert_eq!(counts("@@ -0,0 +1,0 @@\n"), Some((0, 0)));
        assert_eq!(counts("@@ -1 +1 @@\n-old\n"), None);
        assert_eq!(counts("@@ gibberish\n+new\n"), None);
        assert_eq!(counts("opaque provider patch"), None);
        assert_eq!(counts("@@ -0,0 +1,0 @@\n+unaccounted\n"), None);
        assert_eq!(counts("@@ -1 +1 @@\n-old\n+new\n+extra\n"), None);
        assert_eq!(
            counts(
                "@@ -1 +1 @@\n-old\n+new\n\\ No newline at end of file\n--- a/b.rs\n+++ b/b.rs\n@@ -0,0 +1 @@\n+another\n"
            ),
            Some((2, 1))
        );
    }

    #[test]
    fn file_changes_count_resolved_paths_once_without_losing_owner_rows() {
        let mut store = Store::new().unwrap();
        store.file_cwd = Some("/project".into());
        store
            .apply(Event::Item(item(
                "main",
                "src/a.rs",
                DIFF,
                ToolStatus::Succeeded,
                ChangeKind::Updated,
            )))
            .unwrap();
        store
            .apply(Event::SubagentItem {
                id: "delegate".into(),
                item: item(
                    "child",
                    "/project/src/a.rs",
                    DIFF,
                    ToolStatus::Succeeded,
                    ChangeKind::Updated,
                ),
            })
            .unwrap();
        let result = summary(&mut store);
        assert_eq!(result.total_files, 2);
        assert_eq!(result.applied_files, 1);
        assert!(
            result
                .files
                .iter()
                .all(|f| f.resolved_path.as_deref() == Some("/project/src/a.rs"))
        );
        store
            .apply(Event::ItemsRemoved {
                ids: vec!["main".into()],
            })
            .unwrap();
        assert_eq!(summary(&mut store).applied_files, 1);
        store
            .apply(Event::SubagentItem {
                id: "unknown".into(),
                item: item(
                    "relative",
                    "src/a.rs",
                    DIFF,
                    ToolStatus::Succeeded,
                    ChangeKind::Updated,
                ),
            })
            .unwrap();
        let result = summary(&mut store);
        assert_eq!(result.applied_files, 2);
        assert!(result.files.iter().any(|f| f.resolved_path.is_none()));
        store
            .apply(Event::SubagentItemsRemoved {
                id: "delegate".into(),
                ids: vec!["child".into()],
            })
            .unwrap();
        assert_eq!(summary(&mut store).applied_files, 1);
        store
            .apply(Event::SubagentItemsRemoved {
                id: "unknown".into(),
                ids: vec!["relative".into()],
            })
            .unwrap();
        assert_eq!(summary(&mut store).applied_files, 0);
    }

    #[test]
    fn file_changes_complete_coverage_is_independent_of_cached_transcript_window() {
        let mut store = Store::new().unwrap();
        store
            .apply(Event::History {
                items: vec![item(
                    "edit",
                    "a.rs",
                    DIFF,
                    ToolStatus::Succeeded,
                    ChangeKind::Updated,
                )],
                has_earlier: false,
            })
            .unwrap();
        for n in 0..500 {
            store
                .apply(Event::Item(Item {
                    id: format!("text-{n}"),
                    role: Role::Assistant,
                    started_at: None,
                    at: None,
                    content: ItemContent::Text {
                        text: "text".into(),
                        thinking: false,
                        phase: crate::chat::Phase::Final,
                    },
                }))
                .unwrap();
        }
        assert!(store.window().unwrap().has_earlier);
        assert!(!summary(&mut store).coverage.has_unloaded_history);
        assert!(store.earlier().unwrap());
        assert!(store.window().unwrap().has_earlier);
        let indexed = summary(&mut store);
        assert!(!indexed.coverage.has_unloaded_history);
        assert_eq!(indexed.applied_files, 1);
    }

    #[test]
    fn file_changes_open_path_preserves_parent_traversal_after_symlinks() {
        let directory = tempfile::tempdir().unwrap();
        let project = directory.path().join("project");
        let other = directory.path().join("other");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::create_dir_all(other.join("subdir")).unwrap();
        std::os::unix::fs::symlink(other.join("subdir"), project.join("link")).unwrap();
        std::fs::write(other.join("x.rs"), "actual provider target").unwrap();
        std::fs::write(project.join("x.rs"), "wrong lexical target").unwrap();
        let mut store = Store::new().unwrap();
        store.file_cwd = Some(project.clone());
        store
            .apply(Event::Item(item(
                "edit",
                "link/../x.rs",
                DIFF,
                ToolStatus::Succeeded,
                ChangeKind::Updated,
            )))
            .unwrap();
        let row = summary(&mut store).files.remove(0);
        let path = row.resolved_path.unwrap();
        assert_eq!(std::path::Path::new(&path), project.join("link/../x.rs"));
        assert_eq!(
            std::fs::read_to_string(path).unwrap(),
            "actual provider target"
        );
    }
}
