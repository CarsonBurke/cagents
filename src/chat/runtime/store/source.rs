//! Original text reads share the ingestion cache, independently of UI paging.
use super::*;
use crate::chat::cache::{PAGE_BYTES, SOURCE_BYTES, SourceError, SourcePage, SourceQuery, SourceMetadata, SourceEpoch};
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) const SCHEMA: &str = "CREATE TABLE source_epochs(id TEXT PRIMARY KEY REFERENCES items(id) ON DELETE CASCADE, epoch INTEGER NOT NULL);
CREATE TABLE child_sources(agent TEXT NOT NULL,id TEXT NOT NULL,epoch INTEGER NOT NULL,len INTEGER NOT NULL,supported INTEGER NOT NULL,metadata BLOB NOT NULL,PRIMARY KEY(agent,id));
CREATE TABLE child_source_chunks(agent TEXT NOT NULL,id TEXT NOT NULL,position INTEGER NOT NULL,data BLOB NOT NULL,PRIMARY KEY(agent,id,position),FOREIGN KEY(agent,id) REFERENCES child_sources(agent,id) ON DELETE CASCADE);";
pub(super) struct PendingChildSource {
    agent:String,
    id:String,
    text:String,
    metadata:SourceMetadata,
}
// Revisions distinguish rewrites within this process. SourceEpoch also binds
// them to the Store incarnation, including across execed actor replacements.
static NEXT_EPOCH: AtomicU64 = AtomicU64::new(1);
fn epoch() -> Result<u64, String> {
    NEXT_EPOCH.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1).filter(|n| *n <= i64::MAX as u64)).map_err(|_| "Source epoch capacity exhausted".into())
}
pub(super) fn rewrite_epoch(transaction: &rusqlite::Transaction<'_>, id: &str) -> Result<(), String> {
    transaction.execute("INSERT INTO source_epochs VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET epoch=excluded.epoch", params![id,epoch()?]).map_err(|e| e.to_string())?;
    Ok(())
}
fn storage(error: impl ToString) -> SourceError { SourceError::Transport(error.to_string()) }

impl Store {
    pub(super) fn source_changed(&mut self)->Result<(),String>{self.source_revision=epoch()?;Ok(())}
    pub(super) fn removes_text_sources(&self,ids:&[String])->Result<bool,String>{
        for id in ids {
            let metadata:Option<Vec<u8>>=self.db.query_row("SELECT metadata FROM items WHERE id=?1",[id],|row|row.get(0)).optional().map_err(|e|e.to_string())?;
            if metadata.as_ref().is_some_and(|bytes|postcard::from_bytes::<Item>(bytes).is_ok_and(|item|matches!(item.content,ItemContent::Text{..}))) {return Ok(true);}
        }
        Ok(false)
    }
    /// Publish only identities/ranges for the bounded resident window. Child
    /// metadata is reused on status-only publications; changed keys are fetched
    /// in bounded indexed batches, never by scanning older child transcripts.
    pub(super) fn publish_source_parts(&mut self,state:&mut crate::chat::Snapshot)->Result<(),String>{
        use crate::chat::cache::{SourceKey,SourcePart};
        let cached:std::collections::HashMap<_,_>=state.source_parts.iter().filter(|part|part.source.agent_id.is_some()).map(|part|(part.source.clone(),part)).collect();
        let visible:Vec<_>=state.subagents.iter().flat_map(|agent|agent.items.iter().filter_map(move |item|{
            if let ItemContent::Text{text,..}=&item.content {Some((SourceKey{agent_id:Some(agent.id.clone()),source_id:item.id.clone()},text.len()))} else {None}
        })).collect();
        let changed=self.source_revision!=state.source_revision
            || state.source_incarnation!=Some(self.source_incarnation);
        let wanted:Vec<_>=visible.iter().filter(|(key,_)|changed || !cached.contains_key(key)).map(|(key,_)|key).collect();
        if !wanted.is_empty(){self.flush_child_source()?;}
        let mut lengths=std::collections::HashMap::new();
        for batch in wanted.chunks(128){
            let placeholders=std::iter::repeat_n("(?,?)",batch.len()).collect::<Vec<_>>().join(",");
            let sql=format!("SELECT agent,id,len,metadata,epoch FROM child_sources WHERE supported=1 AND (agent,id) IN (VALUES {placeholders})");
            let mut statement=self.db.prepare(&sql).map_err(|e|e.to_string())?;
            let parameters:Vec<&dyn rusqlite::ToSql>=batch.iter().flat_map(|key|[key.agent_id.as_ref().unwrap() as &dyn rusqlite::ToSql,&key.source_id as &dyn rusqlite::ToSql]).collect();
            let rows=statement.query_map(parameters.as_slice(),|row|Ok((SourceKey{agent_id:Some(row.get(0)?),source_id:row.get(1)?},row.get::<_,usize>(2)?,row.get::<_,Vec<u8>>(3)?,row.get::<_,u64>(4)?))).map_err(|e|e.to_string())?;
            for row in rows {let (key,len,bytes,epoch)=row.map_err(|e|e.to_string())?;let metadata=postcard::from_bytes::<Option<SourceMetadata>>(&bytes).map_err(|e|e.to_string())?.ok_or("Missing original prose metadata")?;lengths.insert(key,(len,metadata,epoch));}
        }
        let mut parts=Vec::with_capacity(visible.len());
        for (key,visible_len) in visible {
            if !changed && let Some(part)=cached.get(&key){parts.push((**part).clone());continue;}
            let Some((source_len,metadata,epoch))=lengths.get(&key).cloned() else {continue;};
            let replacement=source_len!=visible_len;
            parts.push(SourcePart{item_id:key.source_id.clone(),source:key,start:0,end:if replacement{0}else{source_len},source_len,replacement,epoch:SourceEpoch{incarnation:self.source_incarnation,revision:epoch},metadata});
        }
        drop(cached);
        state.source_parts.retain(|part|part.source.agent_id.is_none());
        state.source_parts.extend(parts);
        state.source_revision=self.source_revision;
        state.source_incarnation=Some(self.source_incarnation);
        Ok(())
    }
    pub(crate) fn source_page(&mut self, query: SourceQuery) -> Result<SourcePage, SourceError> {
        if !(4..=PAGE_BYTES).contains(&query.limit) || query.source.source_id.len()>METADATA_LIMIT || query.source.agent_id.as_ref().is_some_and(|id|id.len()>METADATA_LIMIT)
            || (query.offset>0 && (query.until.is_none() || query.expected_epoch.is_none())) || (query.until.is_some() && query.expected_epoch.is_none()) {
            return Err(SourceError::InvalidRange);
        }
        self.flush_child_source().map_err(storage)?;
        self.flush_text().map_err(storage)?;
        let (epoch,length,metadata) = if let Some(agent)=&query.source.agent_id {
            let (epoch,length,bytes):(u64,usize,Vec<u8>)=self.db.query_row("SELECT epoch,len,metadata FROM child_sources WHERE agent=?1 AND id=?2",params![agent,query.source.source_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(storage)?.ok_or(SourceError::NotFound)?;
            (epoch,length,postcard::from_bytes::<Option<SourceMetadata>>(&bytes).map_err(storage)?)
        } else {
            let (epoch,length,metadata): (u64,usize,Vec<u8>) = self.db.query_row("SELECT source_epochs.epoch,items.len0,items.metadata FROM items JOIN source_epochs USING(id) WHERE items.id=?1",[&query.source.source_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(storage)?.ok_or(SourceError::NotFound)?;
            let item: Item=postcard::from_bytes(&metadata).map_err(storage)?;
            (epoch,length,SourceMetadata::from_item(&item))
        };
        let metadata=metadata.ok_or(SourceError::Unsupported)?;
        let epoch=SourceEpoch{incarnation:self.source_incarnation,revision:epoch};
        if query.expected_epoch.is_some_and(|expected|expected!=epoch) { return Err(SourceError::Changed); }
        let until=query.until.unwrap_or(length);
        if until>length || query.offset>until { return Err(SourceError::InvalidRange); }
        if until>SOURCE_BYTES { return Err(SourceError::TooLarge{source_len:until,limit:SOURCE_BYTES}); }
        // Fetch at most one page plus a UTF-8 lookahead. The requested start is
        // exact; an invalid interior-byte offset is rejected, never rounded.
        let end=query.offset.saturating_add(query.limit).min(until);
        let bytes=if let Some(agent)=&query.source.agent_id {
            let mut statement=self.db.prepare_cached("SELECT substr(data,MAX(?3-position,0)+1,MIN(position+length(data),?4)-MAX(position,?3)) FROM child_source_chunks WHERE agent=?1 AND id=?2 AND position>=COALESCE((SELECT MAX(position) FROM child_source_chunks WHERE agent=?1 AND id=?2 AND position<=?3),?3) AND position<?4 AND position+length(data)>?3 ORDER BY position").map_err(storage)?;
            let rows=statement.query_map(params![agent,query.source.source_id,query.offset,end],|r|r.get::<_,Vec<u8>>(0)).map_err(storage)?;
            let mut bytes=Vec::with_capacity(end-query.offset);
            for row in rows {bytes.extend(row.map_err(storage)?);}
            bytes
        } else {self.field(&query.source.source_id,0,query.offset,end).map_err(storage)?};
        if bytes.len()!=end-query.offset {return Err(storage("Incomplete cached source"));}
        let valid=match std::str::from_utf8(&bytes) {
            Ok(_) => bytes.len(),
            Err(error) if error.error_len().is_none() && end<until => error.valid_up_to(),
            Err(_) => return Err(SourceError::InvalidRange),
        };
        if valid==0 && query.offset<until {return Err(SourceError::InvalidRange);}
        let next_offset=query.offset+valid;
        let text=String::from_utf8(bytes[..valid].to_vec()).map_err(storage)?;
        Ok(SourcePage{source:query.source,epoch,source_len:until,offset:query.offset,next_offset,text,complete:next_offset==until,metadata})
    }

    fn flush_child_source(&mut self)->Result<(),String>{
        if let Some(pending)=self.pending_child_source.take(){
            if let Err(error)=self.append_child_source(&pending){self.pending_child_source=Some(pending);return Err(error);}
        }
        Ok(())
    }
    fn append_child_source(&mut self,pending:&PendingChildSource)->Result<(),String>{
        let (id,item_id,delta)=(&pending.agent,&pending.id,pending.text.as_str());
        let tx=self.db.transaction().map_err(|e|e.to_string())?;
        let existing:Option<(usize,bool,Vec<u8>)>=tx.query_row("SELECT len,supported,metadata FROM child_sources WHERE agent=?1 AND id=?2",params![id,item_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|e|e.to_string())?;
        let (length,mut metadata)=match existing {
            Some((_,false,_))=>return Ok(()),
            Some((length,true,bytes))=>(length,postcard::from_bytes::<Option<SourceMetadata>>(&bytes).map_err(|e|e.to_string())?.ok_or("Missing original prose metadata")?),
            None=>{
                let metadata=postcard::to_stdvec(&Some(pending.metadata.clone())).map_err(|e|e.to_string())?;
                tx.execute("INSERT INTO child_sources(agent,id,epoch,len,supported,metadata) VALUES(?1,?2,?3,0,1,?4)",params![id,item_id,epoch()?,metadata]).map_err(|e|e.to_string())?;
                (0,pending.metadata.clone())
            }
        };
        metadata.at=pending.metadata.at;
        metadata.started_at=metadata.started_at.or(pending.metadata.started_at);
        let metadata=postcard::to_stdvec(&Some(metadata)).map_err(|e|e.to_string())?;
        let end=length.checked_add(delta.len()).ok_or("Child source length overflow")?;
        tx.execute("UPDATE child_sources SET len=?3,metadata=?4 WHERE agent=?1 AND id=?2",params![id,item_id,end,metadata]).map_err(|e|e.to_string())?;
        if length<SOURCE_BYTES {
            let mut keep=(SOURCE_BYTES-length).min(delta.len());while !delta.is_char_boundary(keep) {keep-=1;}
            child_chunks(&tx,id,item_id,length,&delta[..keep])?;
        }
        tx.commit().map_err(|e|e.to_string())?;
        Ok(())
    }

    pub(super) fn cache_child_source(&mut self,event:&Event)->Result<(),String>{
        if !matches!(event,Event::SubagentTextDelta{..}) {self.flush_child_source()?;}
        match event {
            Event::SubagentItem{id,item}=>{
                if id.len()>METADATA_LIMIT || item.id.len()>METADATA_LIMIT {return Ok(());}
                let text=match &item.content {ItemContent::Text{text,..}=>Some(text.as_str()),_=>None};
                let previous:Option<(bool,Vec<u8>)>=self.db.query_row("SELECT supported,metadata FROM child_sources WHERE agent=?1 AND id=?2",params![id,item.id],|row|Ok((row.get(0)?,row.get(1)?))).optional().map_err(|e|e.to_string())?;
                let old_metadata=previous.as_ref().map(|(_,bytes)|postcard::from_bytes::<Option<SourceMetadata>>(bytes).map_err(|e|e.to_string())).transpose()?.flatten();
                if text.is_some() || previous.as_ref().is_some_and(|(supported,_)|*supported) {self.source_changed()?;}
                let metadata=SourceMetadata::from_item(item).map(|mut metadata|{let at=observed_at();metadata.at=at;metadata.started_at=old_metadata.and_then(|old|old.started_at.or(old.at)).or(at);metadata});
                let metadata=postcard::to_stdvec(&metadata).map_err(|e|e.to_string())?;
                let tx=self.db.transaction().map_err(|e|e.to_string())?;
                tx.execute("INSERT INTO child_sources(agent,id,epoch,len,supported,metadata) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(agent,id) DO UPDATE SET epoch=excluded.epoch,len=excluded.len,supported=excluded.supported,metadata=excluded.metadata",params![id,item.id,epoch()?,text.map_or(0,str::len),text.is_some(),metadata]).map_err(|e|e.to_string())?;
                tx.execute("DELETE FROM child_source_chunks WHERE agent=?1 AND id=?2",params![id,item.id]).map_err(|e|e.to_string())?;
                if let Some(text)=text.filter(|text|text.len()<=SOURCE_BYTES){child_chunks(&tx,id,&item.id,0,text)?;}
                tx.commit().map_err(|e|e.to_string())?;
            }
            Event::SubagentTextDelta{id,item_id,role,thinking,delta}=>{
                if id.len()>METADATA_LIMIT || item_id.len()>METADATA_LIMIT {return Ok(());}
                if !delta.is_empty() {self.source_changed()?;}
                if self.pending_child_source.as_ref().is_some_and(|pending|pending.agent!=*id || pending.id!=*item_id || pending.metadata.role!=*role || pending.metadata.thinking!=*thinking || pending.text.len().saturating_add(delta.len())>64*1024) {
                    self.flush_child_source()?;
                }
                let at=observed_at();
                if delta.len()>64*1024 {self.append_child_source(&PendingChildSource{agent:id.clone(),id:item_id.clone(),text:delta.clone(),metadata:SourceMetadata{role:*role,thinking:*thinking,phase:Phase::Final,at,started_at:at}})?;}
                else if let Some(pending)=&mut self.pending_child_source {pending.text.push_str(delta);pending.metadata.at=at;}
                else {self.pending_child_source=Some(PendingChildSource{agent:id.clone(),id:item_id.clone(),text:delta.clone(),metadata:SourceMetadata{role:*role,thinking:*thinking,phase:Phase::Final,at,started_at:at}});}
            }
            Event::SubagentItemsRemoved{id,ids}=>{for source in ids {
                let supported:bool=self.db.query_row("SELECT supported FROM child_sources WHERE agent=?1 AND id=?2",params![id,source],|row|row.get(0)).optional().map_err(|e|e.to_string())?.unwrap_or(false);
                if supported {self.source_changed()?;}
                self.db.execute("DELETE FROM child_sources WHERE agent=?1 AND id=?2",params![id,source]).map_err(|e|e.to_string())?;}}
            Event::Reset{..}=>{self.db.execute("DELETE FROM child_sources",[]).map_err(|e|e.to_string())?;}
            _=>{}
        }
        Ok(())
    }
}
fn child_chunks(tx:&rusqlite::Transaction<'_>,agent:&str,id:&str,offset:usize,text:&str)->Result<(),String>{
    for (index,bytes) in text.as_bytes().chunks(PART_BYTES).enumerate(){
        tx.execute("INSERT INTO child_source_chunks VALUES(?1,?2,?3,?4)",params![agent,id,offset+index*PART_BYTES,bytes]).map_err(|e|e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::cache::SourceKey;
    fn item(id:&str,text:&str)->Item {
        Item{id:id.into(),role:Role::Assistant,content:ItemContent::Text{text:text.into(),thinking:false,phase:Phase::Final},at:None,started_at:None}
    }
    fn query(agent:Option<&str>,id:&str)->SourceQuery {
        SourceQuery{source:SourceKey{agent_id:agent.map(str::to_owned),source_id:id.into()},offset:0,limit:PAGE_BYTES,expected_epoch:None,until:None}
    }
    fn next(page:&SourcePage)->SourceQuery {
        SourceQuery{source:page.source.clone(),offset:page.next_offset,limit:PAGE_BYTES,expected_epoch:Some(page.epoch),until:Some(page.source_len)}
    }
    #[test]
    fn original_unicode_source_crosses_display_parts_without_moving_window() {
        let mut store=Store::new().unwrap();
        let text=format!("---\ntitle: original\n---\n```yaml\n{}\n```\n", "é🙂|<b>raw</b>\n".repeat(70_000));
        store.apply(Event::Item(item("root:native-part:99:text:0-1:0-0",&text))).unwrap();
        store.window().unwrap();
        let before=(store.end,store.first,store.forward,store.paging,store.dirty);
        let mut page=store.source_page(query(None,"root:native-part:99:text:0-1:0-0")).unwrap();
        let mut assembled=page.text.clone();
        while !page.complete {page=store.source_page(next(&page)).unwrap();assert!(page.text.len()<=PAGE_BYTES);assembled.push_str(&page.text);}
        assert_eq!(assembled,text);
        assert_eq!(before,(store.end,store.first,store.forward,store.paging,store.dirty));
        assert_eq!(store.source_page(query(None,"root")),Err(SourceError::NotFound));
    }
    #[test]
    fn explicit_source_parts_preserve_opaque_ids_and_avoid_real_root_collisions() {
        let mut store=Store::new().unwrap();
        let text="a".repeat(2*PART_BYTES);
        let colliding=format!("a:native-part:1:text:{PART_BYTES}-{}:0-0",2*PART_BYTES);
        store.apply(Event::Item(item("a",&text))).unwrap();
        store.apply(Event::Item(item(&colliding,"opaque root"))).unwrap();
        let window=store.window().unwrap();
        let ids:std::collections::HashSet<_>=window.items.iter().map(|item|&item.id).collect();
        assert_eq!(ids.len(),window.items.len());
        let opaque=window.source_parts.iter().find(|part|part.item_id==colliding).unwrap();
        assert_eq!(opaque.source.source_id,colliding);
        assert_eq!((opaque.start,opaque.end,opaque.source_len),(0,11,11));
        let aliases:Vec<_>=window.source_parts.iter().filter(|part|part.source.source_id=="a").collect();
        assert_eq!(aliases.len(),2);
        assert!(aliases.iter().all(|part|part.source_len==text.len() && !part.replacement));
        let escaped=aliases.iter().find(|part|part.start>0).unwrap();
        assert_ne!(escaped.item_id,colliding);
        assert!(escaped.item_id.contains(":native-alias:"));
        assert_eq!(store.source_page(query(None,&opaque.source.source_id)).unwrap().text,"opaque root");
        assert_eq!(store.window().unwrap().source_parts,window.source_parts);
    }

    #[test]
    fn child_placeholder_metadata_names_original_text_without_fake_byte_ranges() {
        let mut store=Store::new().unwrap();
        let text="original".repeat(20_000);
        let mut original=item("opaque:native-part:1:text:0-3:0-0",&text);
        if let ItemContent::Text{thinking,phase,..}=&mut original.content {*thinking=true;*phase=Phase::Commentary;}
        store.apply(Event::SubagentItem{id:"child".into(),item:original}).unwrap();
        let mut state=crate::chat::Snapshot::default();
        state.subagents.push(crate::chat::Subagent{id:"child".into(),items:vec![std::sync::Arc::new(item("opaque:native-part:1:text:0-3:0-0","[placeholder]"))],kind:String::new(),description:String::new(),status:crate::chat::ToolStatus::Running,background:false,model:None,waiting:false,detail:String::new()});
        store.publish_source_parts(&mut state).unwrap();
        assert_eq!(state.source_parts.len(),1);
        let part=&state.source_parts[0];assert!(part.replacement);
        assert_eq!((part.start,part.end,part.source_len),(0,0,text.len()));
        assert_eq!(part.source.agent_id.as_deref(),Some("child"));
        assert_eq!(part.source.source_id,"opaque:native-part:1:text:0-3:0-0");
        assert!(part.metadata.thinking);assert_eq!(part.metadata.phase,Phase::Commentary);
        assert!(part.metadata.at.is_some());assert_eq!(part.metadata.at,part.metadata.started_at);
        let page=store.source_page(query(Some("child"),&part.source.source_id)).unwrap();
        assert_eq!(page.metadata,part.metadata);assert_eq!(page.text,text);
        let before=state.source_parts.clone();
        store.publish_source_parts(&mut state).unwrap();assert_eq!(state.source_parts,before);
    }

    #[test]
    fn published_source_identity_survives_append_but_rotates_on_reset_and_store_replacement() {
        let mut store = Store::new().unwrap();
        let mut state = crate::chat::Snapshot {
            session_id: Some("same-session".into()),
            subagents: vec![crate::chat::Subagent {
                id: "child".into(),
                items: vec![std::sync::Arc::new(item("root", "[placeholder]"))],
                kind: String::new(),
                description: String::new(),
                status: crate::chat::ToolStatus::Running,
                background: false,
                model: None,
                waiting: false,
                detail: String::new(),
            }],
            ..Default::default()
        };
        store
            .apply(Event::SubagentItem {
                id: "child".into(),
                item: item("root", "original source body"),
            })
            .unwrap();
        store.publish_source_parts(&mut state).unwrap();
        let first = state
            .source_incarnation
            .expect("real Store publication includes its source identity");
        assert_eq!(state.source_parts[0].epoch.incarnation, first);
        let epoch = state.source_parts[0].epoch;
        store
            .apply(Event::SubagentTextDelta {
                id: "child".into(),
                item_id: "root".into(),
                role: Role::Assistant,
                thinking: false,
                delta: " appended".into(),
            })
            .unwrap();
        store.publish_source_parts(&mut state).unwrap();
        assert_eq!(state.source_incarnation, Some(first));
        assert_eq!(
            state.source_parts[0].epoch, epoch,
            "appending keeps prefix proof"
        );

        store
            .apply(Event::Reset {
                reason: "same-session replay".into(),
            })
            .unwrap();
        store
            .apply(Event::SubagentItem {
                id: "child".into(),
                item: item("root", "rewritten source body"),
            })
            .unwrap();
        store.publish_source_parts(&mut state).unwrap();
        let reset = state.source_incarnation.unwrap();
        assert_ne!(
            reset, first,
            "Reset must invalidate already-loaded UI sources"
        );
        assert_eq!(state.source_parts[0].epoch.incarnation, reset);
        assert_eq!(state.session_id.as_deref(), Some("same-session"));

        let mut replacement = Store::new().unwrap();
        replacement
            .apply(Event::SubagentItem {
                id: "child".into(),
                item: item("root", "new store"),
            })
            .unwrap();
        // A replacement process can restart the scalar revision counter at the
        // same value. Old cached child metadata must not cross that lifetime.
        state.source_revision = replacement.source_revision;
        replacement.publish_source_parts(&mut state).unwrap();
        assert_ne!(state.source_incarnation, Some(reset));
        assert_eq!(
            state.source_parts[0].epoch.incarnation,
            state.source_incarnation.unwrap()
        );
        assert_eq!(state.source_parts[0].source_len, "new store".len());
    }

    #[test]
    fn replaced_store_rejects_same_revision_and_identical_source_prefix() {
        for agent in [None,Some("child")] {
            let mut old=Store::new().unwrap();
            let mut replacement=Store::new().unwrap();
            let text="same original bytes".repeat(PAGE_BYTES/4);
            for store in [&mut old,&mut replacement] {
                let event=if let Some(agent)=agent {Event::SubagentItem{id:agent.into(),item:item("root",&text)}} else {Event::Item(item("root",&text))};
                store.apply(event).unwrap();
            }
            let first=old.source_page(query(agent,"root")).unwrap();
            // An execed replacement can restart its process-local revision
            // counter. Simulate that exact collision without relying on globals.
            if let Some(agent)=agent {
                replacement.db.execute("UPDATE child_sources SET epoch=?1 WHERE agent=?2 AND id='root'",params![first.epoch.revision,agent]).unwrap();
            } else {
                replacement.db.execute("UPDATE source_epochs SET epoch=?1 WHERE id='root'",[first.epoch.revision]).unwrap();
            }
            let current=replacement.source_page(query(agent,"root")).unwrap();
            assert_eq!(current.epoch.revision,first.epoch.revision);
            assert_ne!(current.epoch.incarnation,first.epoch.incarnation);
            assert_eq!(current.source_len,first.source_len);
            assert_eq!(current.text,first.text);
            assert_eq!(replacement.source_page(next(&first)),Err(SourceError::Changed));
            assert_eq!(old.source_page(next(&first)).unwrap().epoch,first.epoch);
        }
    }

    #[test]
    fn append_keeps_frozen_prefix_and_rewrite_invalidates_it() {
        let mut store=Store::new().unwrap();
        let text="🙂".repeat(PAGE_BYTES);
        store.apply(Event::Item(item("root",&text))).unwrap();
        let first=store.source_page(query(None,"root")).unwrap();
        store.apply(Event::TextDelta{id:"root".into(),role:Role::Assistant,thinking:false,delta:"more".into()}).unwrap();
        let mut page=first.clone();let mut assembled=page.text.clone();
        while !page.complete {page=store.source_page(next(&page)).unwrap();assert_eq!(page.source_len,text.len());assembled.push_str(&page.text);}
        assert_eq!(assembled,text);
        store.apply(Event::Item(item("root","replacement"))).unwrap();
        assert_eq!(store.source_page(next(&first)),Err(SourceError::Changed));
        let replacement=store.source_page(query(None,"root")).unwrap();
        store.apply(Event::ItemsRemoved{ids:vec!["root".into()]}).unwrap();
        store.apply(Event::Item(item("root","replacement"))).unwrap();
        assert_eq!(store.source_page(next(&replacement)),Err(SourceError::Changed));
    }
    #[test]
    fn child_sources_are_scoped_and_survive_resident_window_limits() {
        let mut store=Store::new().unwrap();
        let text="fence\n".repeat(100_000);
        store.apply(Event::SubagentItem{id:"one".into(),item:item("same",&text)}).unwrap();
        store.apply(Event::SubagentItem{id:"two".into(),item:item("same","other child")} ).unwrap();
        store.apply(Event::Item(item("same","parent"))).unwrap();
        let first=store.source_page(query(Some("one"),"same")).unwrap();
        store.apply(Event::SubagentTextDelta{id:"one".into(),item_id:"same".into(),role:Role::Assistant,thinking:false,delta:"tail".into()}).unwrap();
        let second=store.source_page(next(&first)).unwrap();assert_eq!(second.epoch,first.epoch);assert_eq!(second.source_len,text.len());
        assert_eq!(store.source_page(query(Some("two"),"same")).unwrap().text,"other child");
        assert_eq!(store.source_page(query(None,"same")).unwrap().text,"parent");
        store.apply(Event::SubagentItemsRemoved{id:"one".into(),ids:vec!["same".into()]}).unwrap();
        assert_eq!(store.source_page(next(&first)),Err(SourceError::NotFound));
    }
    #[test]
    fn source_revision_ignores_status_but_tracks_off_window_prose() {
        let mut store=Store::new().unwrap();
        store.apply(Event::Item(item("old","original"))).unwrap();
        let initial=store.source_revision;
        store.apply(Event::Status{busy:true,detail:"Working".into()}).unwrap();
        assert_eq!(store.source_revision,initial);
        let mut image=item("image","");image.content=ItemContent::Image{path:"image.png".into(),description:"image".into()};
        store.apply(Event::Item(image)).unwrap();
        assert_eq!(store.source_revision,initial);
        store.apply(Event::TextDelta{id:"old".into(),role:Role::Assistant,thinking:false,delta:" append".into()}).unwrap();
        let append=store.source_revision;assert_ne!(append,initial);
        store.apply(Event::SubagentTextDelta{id:"child".into(),item_id:"same".into(),role:Role::Assistant,thinking:false,delta:"child source".into()}).unwrap();
        assert_ne!(store.source_revision,append);
        let child=store.source_revision;
        store.apply(Event::SubagentItemsRemoved{id:"child".into(),ids:vec!["same".into()]}).unwrap();
        assert_ne!(store.source_revision,child);
    }

    #[test]
    fn child_token_deltas_batch_before_disk_reads() {
        let mut store=Store::new().unwrap();
        for _ in 0..1024 {
            store.apply(Event::SubagentTextDelta{id:"child".into(),item_id:"text".into(),role:Role::Assistant,thinking:false,delta:"é".into()}).unwrap();
        }
        assert_eq!(store.db.query_row("SELECT COUNT(*) FROM child_sources",[],|row|row.get::<_,usize>(0)).unwrap(),0);
        assert_eq!(store.pending_child_source.as_ref().unwrap().text.len(),2048);
        let page=store.source_page(query(Some("child"),"text")).unwrap();
        assert_eq!(page.text,"é".repeat(1024));
        assert!(store.pending_child_source.is_none());
    }

    #[test]
    fn rejects_invalid_boundaries_unsupported_types_and_oversized_sources() {
        let mut store=Store::new().unwrap();
        store.apply(Event::Item(item("unicode","🙂abc"))).unwrap();
        let first=store.source_page(query(None,"unicode")).unwrap();
        let mut invalid=next(&first);invalid.offset=1;assert_eq!(store.source_page(invalid),Err(SourceError::InvalidRange));
        let mut invalid=query(None,"unicode");invalid.limit=PAGE_BYTES+1;assert_eq!(store.source_page(invalid),Err(SourceError::InvalidRange));
        let mut tool=item("tool","");tool.content=ItemContent::Image{path:"image.png".into(),description:"image".into()};
        store.apply(Event::Item(tool)).unwrap();assert_eq!(store.source_page(query(None,"tool")),Err(SourceError::Unsupported));
        // Metadata can prove oversize without allocating the body for this test.
        store.db.execute("UPDATE items SET len0=?1 WHERE id='unicode'",[SOURCE_BYTES+1]).unwrap();
        assert_eq!(store.source_page(query(None,"unicode")),Err(SourceError::TooLarge{source_len:SOURCE_BYTES+1,limit:SOURCE_BYTES}));
    }
}
