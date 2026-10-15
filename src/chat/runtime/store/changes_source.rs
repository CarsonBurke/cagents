//! Logical Changes identities reuse the ingestion file index and patch chunks.
//! Only small root metadata and a body fingerprint are stored here.
use super::*;
use crate::chat::cache::{PAGE_BYTES, SOURCE_BYTES, SourceError, SourceKey, changes::*};

pub(super) const SCHEMA: &str = "CREATE TABLE changes_sources(source TEXT NOT NULL,item_id TEXT NOT NULL,epoch INTEGER NOT NULL,fingerprint BLOB,metadata BLOB NOT NULL,indexed INTEGER NOT NULL,PRIMARY KEY(source,item_id));";

pub(super) struct Prepared {
    metadata: Metadata,
    fingerprint: Option<Vec<u8>>,
    same_body: bool,
}

fn prepare(item: &Item) -> Option<Prepared> {
    let ItemContent::Changes { changes, status } = &item.content else { return None; };
    let mut identity = item.id.len().saturating_add(128);
    let mut total_bytes = 0usize;
    let mut invalid = item.id.is_empty();
    let mut hash = Sha256::new();
    hash.update((changes.len() as u64).to_le_bytes());
    for file in changes {
        identity = identity.saturating_add(file.path.len()).saturating_add(128);
        invalid |= file.path.is_empty() || file.path.contains('\0');
        hash.update((file.path.len() as u64).to_le_bytes());
        hash.update(file.path.as_bytes());
        match &file.kind {
            crate::chat::ChangeKind::Renamed { from } => {
                identity = identity.saturating_add(from.len());
                invalid |= from.is_empty() || from.contains('\0');
                hash.update([3]);
                hash.update((from.len() as u64).to_le_bytes());
                hash.update(from.as_bytes());
            }
            crate::chat::ChangeKind::Added => hash.update([0]),
            crate::chat::ChangeKind::Deleted => hash.update([1]),
            crate::chat::ChangeKind::Updated => hash.update([2]),
        }
        total_bytes = total_bytes.saturating_add(file.diff.len());
        hash.update((file.diff.len() as u64).to_le_bytes());
        hash.update(file.diff.as_bytes());
    }
    let unavailable = if invalid { Some(Unavailable::InvalidMetadata) }
        else if identity > METADATA_LIMIT { Some(Unavailable::MetadataTooLarge { limit: METADATA_LIMIT }) }
        else if total_bytes > SOURCE_BYTES { Some(Unavailable::SourceTooLarge { limit: SOURCE_BYTES }) }
        else { None };
    Some(Prepared {
        metadata: Metadata { role: item.role, status: *status, at: item.at, started_at: item.started_at,
            total_files: changes.len(), total_bytes, additions: None, deletions: None, unavailable },
        fingerprint: Some(hash.finalize().to_vec()),
        same_body: false,
    })
}

fn owner(key: &SourceKey) -> String {
    key.agent_id.as_ref().map_or_else(String::new, |id| format!("c:{id}"))
}
fn storage(error: impl ToString) -> SourceError { SourceError::Transport(error.to_string()) }
fn valid_key(key: &SourceKey) -> bool {
    !key.source_id.is_empty() && key.source_id.len() <= METADATA_LIMIT
        && key.agent_id.as_ref().is_none_or(|id| id.len() <= METADATA_LIMIT)
}

/// Called inside the parent body transaction, or before the child's indexed
/// replacement. An unfinished index is explicit if later ingestion fails.
pub(super) fn record(db: &Connection, source: &str, id: &str, prepared: Option<&Prepared>) -> Result<bool, String> {
    let Some(prepared) = prepared else {
        return db.execute("DELETE FROM changes_sources WHERE source=?1 AND item_id=?2", params![source,id]).map(|count| count>0).map_err(|e|e.to_string());
    };
    let previous: Option<(u64,Option<Vec<u8>>)> = db.query_row("SELECT epoch,fingerprint FROM changes_sources WHERE source=?1 AND item_id=?2",params![source,id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|e.to_string())?;
    let epoch = match previous {
        Some((epoch,fingerprint)) if prepared.same_body && fingerprint.is_some() && fingerprint == prepared.fingerprint => epoch,
        _ => super::source::epoch()?,
    };
    let metadata = postcard::to_stdvec(&prepared.metadata).map_err(|e|e.to_string())?;
    db.execute("INSERT INTO changes_sources VALUES(?1,?2,?3,?4,?5,0) ON CONFLICT(source,item_id) DO UPDATE SET epoch=excluded.epoch,fingerprint=excluded.fingerprint,metadata=excluded.metadata,indexed=0",params![source,id,epoch,prepared.fingerprint,metadata]).map_err(|e|e.to_string())?;
    Ok(true)
}

impl Store {
    pub(super) fn prepare_changes_source(&self,item:&Item,source:&str)->Result<Option<Prepared>,String> {
        let Some(mut prepared)=prepare(item) else {return Ok(None);};
        let previous:Option<(Option<Vec<u8>>,bool)>=self.db.query_row("SELECT fingerprint,indexed FROM changes_sources WHERE source=?1 AND item_id=?2",params![source,item.id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|e.to_string())?;
        if prepared.metadata.unavailable.is_some() || !matches!(previous,Some((Some(ref fingerprint),true)) if Some(fingerprint)==prepared.fingerprint.as_ref()) {return Ok(Some(prepared));}
        let ItemContent::Changes{changes,..}=&item.content else {unreachable!();};
        // The digest rejects ordinary rewrites cheaply. Equality is verified
        // against exact cached bytes before it can retain a read authority.
        for (index,file) in changes.iter().enumerate() {
            let record:Option<(String,Vec<u8>,usize,usize)>=self.db.query_row("SELECT path,kind,start,len FROM file_edits WHERE source=?1 AND item_id=?2 AND file_index=?3",params![source,item.id,index],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).optional().map_err(|e|e.to_string())?;
            let Some((path,kind,start,len))=record else {return Ok(Some(prepared));};
            if path!=file.path || len!=file.diff.len() || postcard::from_bytes::<crate::chat::ChangeKind>(&kind).map_err(|e|e.to_string())?!=file.kind {return Ok(Some(prepared));}
            let key=SourceKey{agent_id:source.strip_prefix("c:").map(str::to_owned),source_id:item.id.clone()};
            for offset in (0..len).step_by(PAGE_BYTES) {
                let end=offset.saturating_add(PAGE_BYTES).min(len);
                if self.change_patch_bytes(&key,index,start,offset,end)?!=file.diff.as_bytes()[offset..end] {return Ok(Some(prepared));}
            }
        }
        prepared.same_body=true;
        Ok(Some(prepared))
    }

    fn change_patch_bytes(&self,key:&SourceKey,index:usize,start:usize,offset:usize,end:usize)->Result<Vec<u8>,String> {
        if key.agent_id.is_none() {return self.field(&key.source_id,0,start+offset,start+end);}
        let mut statement=self.db.prepare_cached("SELECT substr(data,MAX(?4-position,0)+1,MIN(position+length(data),?5)-MAX(position,?4)) FROM file_patch_chunks WHERE source=?1 AND item_id=?2 AND file_index=?3 AND position>=COALESCE((SELECT MAX(position) FROM file_patch_chunks WHERE source=?1 AND item_id=?2 AND file_index=?3 AND position<=?4),?4) AND position<?5 AND position+length(data)>?4 ORDER BY position").map_err(|e|e.to_string())?;
        let records=statement.query_map(params![owner(key),key.source_id,index,offset,end],|r|r.get::<_,Vec<u8>>(0)).map_err(|e|e.to_string())?;
        let mut bytes=Vec::with_capacity(end-offset);
        for record in records {bytes.extend(record.map_err(|e|e.to_string())?);}
        Ok(bytes)
    }

    pub(super) fn finish_changes_index(&self, source: &str, id: &str) -> Result<(), String> {
        let bytes: Option<Vec<u8>> = self.db.query_row("SELECT metadata FROM changes_sources WHERE source=?1 AND item_id=?2",params![source,id],|r|r.get(0)).optional().map_err(|e|e.to_string())?;
        let Some(bytes) = bytes else { return Ok(()); };
        let mut metadata: Metadata = postcard::from_bytes(&bytes).map_err(|e|e.to_string())?;
        if matches!(metadata.unavailable,Some(Unavailable::InvalidMetadata | Unavailable::MetadataTooLarge {..})) { return Ok(()); }
        let (count,known,added,deleted):(usize,usize,u64,u64) = self.db.query_row("SELECT COUNT(*),COUNT(additions),COALESCE(SUM(additions),0),COALESCE(SUM(deletions),0) FROM file_edits WHERE source=?1 AND item_id=?2",params![source,id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?))).map_err(|e|e.to_string())?;
        if count != metadata.total_files { return Err("Exact Changes file index is incomplete".into()); }
        metadata.additions = (known == count).then_some(added);
        metadata.deletions = (known == count).then_some(deleted);
        self.db.execute("UPDATE changes_sources SET metadata=?3,indexed=1 WHERE source=?1 AND item_id=?2",params![source,id,postcard::to_stdvec(&metadata).map_err(|e|e.to_string())?]).map_err(|e|e.to_string())?;
        Ok(())
    }

    fn changes_header(&self, key: &SourceKey, expected: Option<Epoch>) -> Result<(Epoch,Metadata),SourceError> {
        let row: Option<(u64,Vec<u8>,bool)> = self.db.query_row("SELECT epoch,metadata,indexed FROM changes_sources WHERE source=?1 AND item_id=?2",params![owner(key),key.source_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(storage)?;
        let (revision,bytes,indexed) = row.ok_or(SourceError::NotFound)?;
        let epoch=Epoch { incarnation:self.source_incarnation,revision };
        if expected.is_some_and(|expected| expected != epoch) { return Err(SourceError::Changed); }
        let mut metadata: Metadata = postcard::from_bytes(&bytes).map_err(storage)?;
        if !indexed && !matches!(metadata.unavailable,Some(Unavailable::MetadataTooLarge {..} | Unavailable::InvalidMetadata)) { metadata.unavailable = Some(Unavailable::IncompleteIndex); }
        Ok((epoch,metadata))
    }

    pub(crate) fn changes_manifest(&self, query: ManifestQuery) -> Result<Manifest,SourceError> {
        if !valid_key(&query.source) || query.offset>i64::MAX as usize || !(1..=MANIFEST_FILES).contains(&query.limit)
            || (query.offset > 0 && query.expected_epoch.is_none()) { return Err(SourceError::InvalidRange); }
        let (epoch,metadata) = self.changes_header(&query.source,query.expected_epoch)?;
        if query.offset > metadata.total_files { return Err(SourceError::InvalidRange); }
        if matches!(metadata.unavailable,Some(Unavailable::MetadataTooLarge {..} | Unavailable::InvalidMetadata | Unavailable::IncompleteIndex)) {
            if query.offset != 0 { return Err(SourceError::InvalidRange); }
            let omitted_files = metadata.total_files;
            return Ok(Manifest { source:query.source,epoch,metadata,offset:0,next_offset:None,files:Vec::new(),omitted_files });
        }
        let mut statement = self.db.prepare_cached("SELECT file_index,path,kind,len,additions,deletions FROM file_edits WHERE source=?1 AND item_id=?2 AND file_index>=?3 ORDER BY file_index LIMIT ?4").map_err(storage)?;
        let records = statement.query_map(params![owner(&query.source),query.source.source_id,query.offset,query.limit+1],|r|Ok((r.get::<_,usize>(0)?,r.get::<_,String>(1)?,r.get::<_,Vec<u8>>(2)?,r.get::<_,usize>(3)?,r.get::<_,Option<u64>>(4)?,r.get::<_,Option<u64>>(5)?))).map_err(storage)?;
        let mut result = Manifest { source:query.source,epoch,metadata,offset:query.offset,next_offset:None,files:Vec::new(),omitted_files:0 };
        let mut bytes = postcard::to_stdvec(&result).map_err(storage)?.len()+32;
        for record in records {
            let (file_index,path,kind,source_len,additions,deletions) = record.map_err(storage)?;
            if file_index!=query.offset+result.files.len() {return Err(storage("Exact Changes manifest has a missing file index"));}
            let file = File { file_index,path,kind:postcard::from_bytes(&kind).map_err(storage)?,source_len,additions,deletions };
            let size = postcard::to_stdvec(&file).map_err(storage)?.len()+16;
            if result.files.len() == query.limit || bytes.saturating_add(size) > PAGE_BYTES {
                if result.files.is_empty() { return Err(storage("Changes file metadata exceeds its page budget")); }
                result.next_offset = Some(file_index);
                break;
            }
            bytes += size;
            result.files.push(file);
        }
        if result.next_offset.is_none() && query.offset + result.files.len() != result.metadata.total_files {
            return Err(storage("Exact Changes manifest is incomplete"));
        }
        Ok(result)
    }

    pub(crate) fn change_patch(&self, query: PatchQuery) -> Result<PatchPage,SourceError> {
        if !valid_key(&query.source) || query.file_index>i64::MAX as usize || !(4..=PAGE_BYTES).contains(&query.limit) { return Err(SourceError::InvalidRange); }
        let (epoch,metadata) = self.changes_header(&query.source,Some(query.expected_epoch))?;
        if metadata.total_bytes > SOURCE_BYTES { return Err(SourceError::TooLarge { source_len:metadata.total_bytes,limit:SOURCE_BYTES }); }
        if metadata.unavailable.is_some() { return Err(storage("Exact Changes source is unavailable; inspect its manifest")); }
        let row: Option<(String,Vec<u8>,usize,usize,Option<u64>,Option<u64>)> = self.db.query_row("SELECT path,kind,start,len,additions,deletions FROM file_edits WHERE source=?1 AND item_id=?2 AND file_index=?3",params![owner(&query.source),query.source.source_id,query.file_index],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?,r.get(5)?))).optional().map_err(storage)?;
        let (path,kind,start,source_len,additions,deletions) = row.ok_or(SourceError::NotFound)?;
        if query.until != source_len || query.offset > source_len { return Err(SourceError::InvalidRange); }
        let file=File { file_index:query.file_index,path,kind:postcard::from_bytes(&kind).map_err(storage)?,source_len,additions,deletions };
        let envelope=PatchPage {source:query.source.clone(),epoch,file:file.clone(),offset:query.offset,next_offset:source_len,text:String::new(),complete:false};
        let overhead=postcard::to_stdvec(&envelope).map_err(storage)?.len().saturating_add(256);
        let limit=query.limit.min(PAGE_BYTES.saturating_sub(overhead));
        if limit<4 && query.offset<source_len {return Err(storage("Changes patch metadata exceeds its page budget"));}
        let end = query.offset.saturating_add(limit).min(source_len);
        let mut bytes=self.change_patch_bytes(&query.source,query.file_index,start,query.offset,end).map_err(storage)?;
        if bytes.len() != end-query.offset { return Err(storage("Exact Changes patch chunks are incomplete")); }
        let valid = match std::str::from_utf8(&bytes) {
            Ok(_) => bytes.len(),
            Err(error) if error.error_len().is_none() && end < source_len => error.valid_up_to(),
            Err(_) => return Err(SourceError::InvalidRange),
        };
        if valid == 0 && query.offset < source_len { return Err(SourceError::InvalidRange); }
        let next_offset = query.offset+valid;
        bytes.truncate(valid);
        Ok(PatchPage { source:query.source,epoch,file,offset:query.offset,next_offset,text:String::from_utf8(bytes).map_err(storage)?,complete:next_offset==source_len })
    }

    pub(super) fn changes_part(&self, item_id: &str, source: SourceKey, range: Option<std::ops::Range<usize>>) -> Result<Option<Part>,String> {
        let (epoch,metadata) = match self.changes_header(&source,None) {
            Ok(header) => header,
            Err(SourceError::NotFound) => return Ok(None),
            Err(error) => return Err(error.to_string()),
        };
        let mut result = Part { item_id:item_id.into(),source,epoch,metadata,files:Vec::new(),replacement:range.is_none() };
        if matches!(result.metadata.unavailable,Some(Unavailable::MetadataTooLarge {..} | Unavailable::InvalidMetadata | Unavailable::IncompleteIndex)) {
            result.replacement = true;
            return Ok(Some(result));
        }
        let Some(range) = range else { return Ok(Some(result)); };
        let mut statement = self.db.prepare_cached("SELECT file_index,start,len FROM file_edits WHERE source=?1 AND item_id=?2 ORDER BY file_index").map_err(|e|e.to_string())?;
        let records = statement.query_map(params![owner(&result.source),result.source.source_id],|r|Ok((r.get::<_,usize>(0)?,r.get::<_,usize>(1)?,r.get::<_,usize>(2)?))).map_err(|e|e.to_string())?;
        for record in records {
            let (file_index,start,source_len) = record.map_err(|e|e.to_string())?;
            let end = start+source_len;
            let within = if source_len == 0 { range.contains(&start) || (start == result.metadata.total_bytes && range.end == start) }
                else { start < range.end && end > range.start };
            if within {
                result.files.push(FilePart { display_slot:result.files.len(),file_index,start:range.start.max(start)-start,end:range.end.min(end)-start,source_len });
            }
        }
        Ok(Some(result))
    }

    /// Only resident identities are looked up. Status-only publications reuse
    /// cached associations; no historical child transcript is scanned.
    pub(in crate::chat::runtime) fn publish_changes_parts(&self, state: &mut crate::chat::Snapshot) -> Result<(),String> {
        let cached: std::collections::HashMap<_,_> = state.changes_parts.iter().filter(|part|part.source.agent_id.is_some()).map(|part|(part.source.clone(),part)).collect();
        let changed = state.changes_revision != self.changes_revision
            || state.changes_parts.iter().any(|part|part.epoch.incarnation!=self.source_incarnation);
        let mut parts = Vec::new();
        for agent in &state.subagents {
            for item in &agent.items {
                let source = SourceKey { agent_id:Some(agent.id.clone()),source_id:item.id.clone() };
                if !changed && let Some(part) = cached.get(&source) { parts.push((**part).clone());continue; }
                let header = match self.changes_header(&source,None) {
                    Ok((_,metadata)) => metadata,
                    Err(SourceError::NotFound) => continue,
                    Err(error) => return Err(error.to_string()),
                };
                let mut part=self.changes_part(&item.id,source,Some(0..header.total_bytes))?.expect("header was read under the same Store lock");
                let whole=match &item.content {
                    ItemContent::Changes{changes,..} => changes.len()==header.total_files
                        && changes.len()==part.files.len()
                        && changes.iter().zip(&part.files).all(|(file,part)|file.diff.len()==part.source_len),
                    _=>false,
                };
                if !whole {part.replacement=true;part.files.clear();}
                parts.push(part);
            }
        }
        drop(cached);
        state.changes_parts.retain(|part|part.source.agent_id.is_none());
        state.changes_parts.extend(parts);
        state.changes_revision = self.changes_revision;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PATCH:&str="@@ -1 +1 @@\n-old\n+new\n";
    fn item(id:&str,files:&[(&str,&str)])->Item {
        Item {id:id.into(),role:Role::Assistant,at:Some(15),started_at:Some(10),content:ItemContent::Changes {
            changes:files.iter().map(|(path,diff)|crate::chat::FileChange {path:(*path).into(),kind:crate::chat::ChangeKind::Updated,diff:(*diff).into()}).collect(),
            status:crate::chat::ToolStatus::Succeeded,
        }}
    }
    fn key(agent:Option<&str>,id:&str)->SourceKey {SourceKey {agent_id:agent.map(str::to_owned),source_id:id.into()}}
    fn manifest(store:&Store,source:SourceKey)->Manifest {
        store.changes_manifest(ManifestQuery {source,offset:0,limit:MANIFEST_FILES,expected_epoch:None}).unwrap()
    }
    fn read(store:&Store,manifest:&Manifest,index:usize,limit:usize)->String {
        let file=manifest.files.iter().find(|file|file.file_index==index).unwrap();
        let mut query=PatchQuery {source:manifest.source.clone(),file_index:index,offset:0,limit,expected_epoch:manifest.epoch,until:file.source_len};
        let mut text=String::new();
        loop {
            let page=store.change_patch(query.clone()).unwrap();
            assert!(page.matches_query(&query));
            assert!(postcard::to_stdvec(&page).unwrap().len()+128<=PAGE_BYTES);
            text.push_str(&page.text);
            if page.complete {break;}
            query.offset=page.next_offset;
        }
        text
    }
    fn apply(store:&mut Store,agent:Option<&str>,item:Item) {
        let event=agent.map_or_else(||Event::Item(item.clone()),|id|Event::SubagentItem{id:id.into(),item:item.clone()});
        store.apply(event).unwrap();
    }

    #[test]
    fn exact_manifest_preserves_duplicate_paths_empty_files_and_unicode() {
        let mut store=Store::new().unwrap();
        let original=item("opaque:native-part:7:changes:0-3:0-0",&[("same.rs",PATCH),("same.rs","α🙂last\r\n"),("empty.rs","")]);
        store.apply(Event::History {items:vec![original],has_earlier:false}).unwrap();
        let source=key(None,"opaque:native-part:7:changes:0-3:0-0");
        let first=store.changes_manifest(ManifestQuery {source:source.clone(),offset:0,limit:1,expected_epoch:None}).unwrap();
        assert_eq!((first.metadata.at,first.metadata.started_at),(Some(15),Some(10)));
        assert_eq!(first.metadata.total_files,3);
        assert_eq!(first.next_offset,Some(1));
        assert_eq!((first.files[0].additions,first.files[0].deletions),(Some(1),Some(1)));
        let second=store.changes_manifest(ManifestQuery {source:source.clone(),offset:1,limit:1,expected_epoch:Some(first.epoch)}).unwrap();
        assert_eq!(second.files[0].path,first.files[0].path);
        assert_eq!(second.files[0].file_index,1);
        assert_eq!(read(&store,&second,1,4),"α🙂last\r\n");
        let last=store.changes_manifest(ManifestQuery {source,offset:2,limit:1,expected_epoch:Some(first.epoch)}).unwrap();
        assert!(last.next_offset.is_none());
        assert_eq!(read(&store,&last,2,4),"");
        assert_eq!(last.files[0].source_len,0);
        assert_eq!(last.omitted_files,0);
    }

    #[test]
    fn logical_patch_crosses_native_window_and_retains_original_stats() {
        let mut store=Store::new().unwrap();
        let mut patch=String::from("@@ -1,60001 +1,60001 @@\n-old\n+AFTER_FIRST_LINE\n");
        for line in 0..60_000 {patch.push_str(&format!(" context_{line:08}_{}🙂\n","word".repeat(12)));}
        patch.push_str(" LAST_CONTEXT_TOKEN_END_OF_FULL_PATCH\n");
        // The header must include the final anchor context line as well.
        patch=patch.replacen("60001","60002",2);
        assert!(patch.len()>WINDOW_BYTES);
        apply(&mut store,None,item("logical",&[("large.rs",&patch)]));
        let window=store.window().unwrap();
        assert!(window.changes_parts.iter().any(|part|part.files.iter().any(|file|file.start>0)));
        assert!(!window.changes_parts.iter().any(|part|part.files.iter().any(|file|file.start==0)),"the root beginning must be outside the resident window");
        let before=(store.end,store.first,store.forward,store.paging,store.dirty);
        let first=manifest(&store,key(None,"logical"));
        assert_eq!((first.metadata.additions,first.metadata.deletions),(Some(1),Some(1)));
        assert_eq!(read(&store,&first,0,PAGE_BYTES),patch);
        assert_eq!(before,(store.end,store.first,store.forward,store.paging,store.dirty));
        assert!(store.changes_manifest(ManifestQuery {source:key(None,"logical:native-part:1:changes:0-4:0-0"),offset:0,limit:1,expected_epoch:None}).is_err());
    }

    #[test]
    fn epoch_tracks_exact_bodies_and_incarnations_but_not_live_status() {
        for agent in [None,Some("child")] {
            let mut store=Store::new().unwrap();
            let original=item("same",&[("a.rs",PATCH)]);
            apply(&mut store,agent,original.clone());
            let first=manifest(&store,key(agent,"same"));
            let mut live=original.clone();
            if let ItemContent::Changes {status,..}=&mut live.content {*status=crate::chat::ToolStatus::Running;}
            apply(&mut store,agent,live);
            let status=manifest(&store,key(agent,"same"));
            assert_eq!(status.epoch,first.epoch);
            assert_eq!(status.metadata.status,crate::chat::ToolStatus::Running);
            let changed=item("same",&[("a.rs","@@ -1 +1 @@\n-old\n+NEW\n")]);
            // Even a matching digest hint cannot authorize different bytes.
            let prepared=prepare(&changed).unwrap();
            store.db.execute("UPDATE changes_sources SET fingerprint=?3 WHERE source=?1 AND item_id=?2",params![owner(&first.source),"same",prepared.fingerprint]).unwrap();
            apply(&mut store,agent,changed.clone());
            let rewrite=manifest(&store,key(agent,"same"));
            assert_ne!(rewrite.epoch,first.epoch);
            let stale=PatchQuery {source:first.source.clone(),file_index:0,offset:0,limit:64,expected_epoch:first.epoch,until:first.files[0].source_len};
            assert_eq!(store.change_patch(stale.clone()),Err(SourceError::Changed));
            let mut replacement=Store::new().unwrap();
            apply(&mut replacement,agent,changed);
            replacement.db.execute("UPDATE changes_sources SET epoch=?3 WHERE source=?1 AND item_id=?2",params![owner(&rewrite.source),"same",rewrite.epoch.revision]).unwrap();
            let current=manifest(&replacement,key(agent,"same"));
            assert_eq!(current.epoch.revision,rewrite.epoch.revision);
            assert_ne!(current.epoch.incarnation,rewrite.epoch.incarnation);
            assert_eq!(replacement.change_patch(PatchQuery {expected_epoch:rewrite.epoch,until:rewrite.files[0].source_len,..stale}),Err(SourceError::Changed));
        }
    }

    #[test]
    fn parent_and_child_sources_removals_and_reset_do_not_alias() {
        let mut store=Store::new().unwrap();
        apply(&mut store,None,item("same",&[("a",PATCH)]));
        apply(&mut store,Some("child"),item("same",&[("a","child bytes")]));
        let parent=manifest(&store,key(None,"same"));
        let child=manifest(&store,key(Some("child"),"same"));
        assert_eq!(read(&store,&parent,0,64),PATCH);
        assert_eq!(read(&store,&child,0,64),"child bytes");
        store.apply(Event::SubagentItemsRemoved {id:"child".into(),ids:vec!["same".into()]}).unwrap();
        assert_eq!(store.changes_manifest(ManifestQuery {source:child.source,offset:0,limit:1,expected_epoch:Some(child.epoch)}),Err(SourceError::NotFound));
        assert_eq!(read(&store,&parent,0,64),PATCH);
        store.apply(Event::ItemsRemoved {ids:vec!["same".into()]}).unwrap();
        apply(&mut store,None,item("same",&[("a",PATCH)]));
        assert_ne!(manifest(&store,key(None,"same")).epoch,parent.epoch);
        store.apply(Event::Reset {reason:"test".into()}).unwrap();
        assert!(store.changes_manifest(ManifestQuery {source:key(None,"same"),offset:0,limit:1,expected_epoch:None}).is_err());
    }

    #[test]
    fn metadata_shortfalls_and_unfinished_indices_are_explicit() {
        let mut store=Store::new().unwrap();
        let path="p".repeat(METADATA_LIMIT+1);
        apply(&mut store,None,item("oversized",&[(&path,PATCH),("later.rs",PATCH)]));
        let unavailable=manifest(&store,key(None,"oversized"));
        assert_eq!(unavailable.metadata.total_files,2);
        assert_eq!(unavailable.omitted_files,2);
        assert_eq!(unavailable.metadata.unavailable,Some(Unavailable::MetadataTooLarge {limit:METADATA_LIMIT}));
        assert!(unavailable.files.is_empty());
        apply(&mut store,None,item("normal",&[("a",PATCH)]));
        store.db.execute("UPDATE changes_sources SET indexed=0 WHERE item_id='normal'",[]).unwrap();
        let unfinished=manifest(&store,key(None,"normal"));
        assert_eq!(unfinished.omitted_files,1);
        assert_eq!(unfinished.metadata.unavailable,Some(Unavailable::IncompleteIndex));
    }

    #[test]
    fn displayed_file_slots_retain_original_indices_and_ranges_on_reuse() {
        let mut store=Store::new().unwrap();
        let large="🙂 context\n".repeat(PART_BYTES/3);
        let original=item("logical",&[("same.rs","first\n"),("same.rs",&large),("empty.rs","")]);
        let ItemContent::Changes{changes,..}=&original.content else {unreachable!();};
        let expected=changes.clone();
        apply(&mut store,None,original);
        let window=store.window().unwrap();
        assert!(window.items.len()>1);
        for part in &window.changes_parts {
            assert_eq!(part.source,key(None,"logical"));
            assert!(!part.replacement);
            let visible=window.items.iter().find(|item|item.id==part.item_id).unwrap();
            let ItemContent::Changes{changes,..}=&visible.content else {unreachable!();};
            assert_eq!(changes.len(),part.files.len());
            for file in &part.files {
                assert_eq!(changes[file.display_slot].diff,expected[file.file_index].diff[file.start..file.end]);
            }
        }
        let mut previous=window.items.clone();
        let reused=store.window_reusing(&mut previous).unwrap();
        assert_eq!(reused.changes_parts,window.changes_parts);
        assert!(reused.items.iter().zip(&window.items).all(|(left,right)|std::sync::Arc::ptr_eq(left,right)));
    }

    #[test]
    fn patch_reads_reject_prefixes_interior_utf8_and_missing_authority() {
        let mut store=Store::new().unwrap();
        apply(&mut store,None,item("unicode",&[("a","🙂end")]));
        let first=manifest(&store,key(None,"unicode"));
        let query=PatchQuery{source:first.source.clone(),file_index:0,offset:0,limit:4,expected_epoch:first.epoch,until:first.files[0].source_len};
        for invalid in [PatchQuery{offset:1,..query.clone()},PatchQuery{until:4,..query.clone()},PatchQuery{limit:1,..query.clone()},PatchQuery{file_index:usize::MAX,..query.clone()}] {
            assert_eq!(store.change_patch(invalid),Err(SourceError::InvalidRange));
        }
        assert_eq!(store.changes_manifest(ManifestQuery{source:first.source,offset:1,limit:1,expected_epoch:None}),Err(SourceError::InvalidRange));
    }

    #[test]
    fn manifests_page_original_files_instead_of_aggregating_their_paths() {
        let mut store=Store::new().unwrap();
        let paths=vec![("same.rs",PATCH);200];
        apply(&mut store,None,item("many",&paths));
        let first=manifest(&store,key(None,"many"));
        assert_eq!(first.metadata.total_files,200);
        assert_eq!((first.metadata.additions,first.metadata.deletions),(Some(200),Some(200)));
        assert_eq!(first.files.len(),MANIFEST_FILES);
        let next=first.next_offset.unwrap();
        assert_eq!(next,MANIFEST_FILES);
        let query=ManifestQuery {source:first.source.clone(),offset:next,limit:MANIFEST_FILES,expected_epoch:Some(first.epoch)};
        let last=store.changes_manifest(query.clone()).unwrap();
        assert!(last.matches_query(&query));
        assert_eq!(last.files.last().unwrap().file_index,199);
        assert!(last.next_offset.is_none());
        assert_eq!(last.omitted_files,0);
        assert_eq!(read(&store,&last,199,64),PATCH);
    }

    #[test]
    fn oversized_logical_body_keeps_explicit_metadata_and_refuses_patch_reads() {
        let mut store=Store::new().unwrap();
        let body="x".repeat(SOURCE_BYTES+1);
        apply(&mut store,None,item("large",&[("large.rs",&body)]));
        let first=manifest(&store,key(None,"large"));
        assert_eq!(first.metadata.total_bytes,body.len());
        assert_eq!(first.files[0].source_len,body.len());
        assert_eq!(first.metadata.unavailable,Some(Unavailable::SourceTooLarge {limit:SOURCE_BYTES}));
        assert_eq!(store.change_patch(PatchQuery {source:first.source,file_index:0,offset:0,limit:PAGE_BYTES,expected_epoch:first.epoch,until:body.len()}),Err(SourceError::TooLarge {source_len:body.len(),limit:SOURCE_BYTES}));
    }
}
