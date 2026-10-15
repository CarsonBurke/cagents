//! Original text reads share the ingestion cache, independently of UI paging.
use super::*;
use crate::chat::cache::{PAGE_BYTES, SOURCE_BYTES, SourceError, SourcePage, SourceQuery};
use std::sync::atomic::{AtomicU64, Ordering};

pub(super) const SCHEMA: &str = "CREATE TABLE source_epochs(id TEXT PRIMARY KEY REFERENCES items(id) ON DELETE CASCADE, epoch INTEGER NOT NULL);
CREATE TABLE child_sources(agent TEXT NOT NULL,id TEXT NOT NULL,epoch INTEGER NOT NULL,len INTEGER NOT NULL,supported INTEGER NOT NULL,PRIMARY KEY(agent,id));
CREATE TABLE child_source_chunks(agent TEXT NOT NULL,id TEXT NOT NULL,position INTEGER NOT NULL,data BLOB NOT NULL,PRIMARY KEY(agent,id,position),FOREIGN KEY(agent,id) REFERENCES child_sources(agent,id) ON DELETE CASCADE);";
// Unique across every Store in this daemon; deleting/recreating an ID or replacing
// an actor cannot accidentally validate an older read from another cache.
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
    pub(crate) fn source_page(&mut self, query: SourceQuery) -> Result<SourcePage, SourceError> {
        if !(4..=PAGE_BYTES).contains(&query.limit) || query.source.source_id.len()>METADATA_LIMIT || query.source.agent_id.as_ref().is_some_and(|id|id.len()>METADATA_LIMIT)
            || (query.offset>0 && (query.until.is_none() || query.expected_epoch.is_none())) || (query.until.is_some() && query.expected_epoch.is_none()) {
            return Err(SourceError::InvalidRange);
        }
        self.flush_child_source().map_err(storage)?;
        self.flush_text().map_err(storage)?;
        let (epoch,length,supported) = if let Some(agent)=&query.source.agent_id {
            self.db.query_row("SELECT epoch,len,supported FROM child_sources WHERE agent=?1 AND id=?2",params![agent,query.source.source_id],|r|Ok((r.get::<_,u64>(0)?,r.get::<_,usize>(1)?,r.get::<_,bool>(2)?))).optional().map_err(storage)?.ok_or(SourceError::NotFound)?
        } else {
            let (epoch,length,metadata): (u64,usize,Vec<u8>) = self.db.query_row("SELECT source_epochs.epoch,items.len0,items.metadata FROM items JOIN source_epochs USING(id) WHERE items.id=?1",[&query.source.source_id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(storage)?.ok_or(SourceError::NotFound)?;
            let item: Item=postcard::from_bytes(&metadata).map_err(storage)?;
            (epoch,length,matches!(item.content,ItemContent::Text{..}))
        };
        if !supported { return Err(SourceError::Unsupported); }
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
        Ok(SourcePage{source:query.source,epoch,source_len:until,offset:query.offset,next_offset,text,complete:next_offset==until})
    }

    fn flush_child_source(&mut self)->Result<(),String>{
        if let Some((agent,id,delta))=self.pending_child_source.take(){
            if let Err(error)=self.append_child_source(&agent,&id,&delta){
                self.pending_child_source=Some((agent,id,delta));
                return Err(error);
            }
        }
        Ok(())
    }
    fn append_child_source(&mut self,id:&str,item_id:&str,delta:&str)->Result<(),String>{
                let tx=self.db.transaction().map_err(|e|e.to_string())?;
                let existing:Option<(usize,bool)>=tx.query_row("SELECT len,supported FROM child_sources WHERE agent=?1 AND id=?2",params![id,item_id],|r|Ok((r.get(0)?,r.get(1)?))).optional().map_err(|e|e.to_string())?;
                let length=match existing {
                    Some((_,false))=>return Ok(()),
                    Some((length,true))=>length,
                    None=>{tx.execute("INSERT INTO child_sources VALUES(?1,?2,?3,0,1)",params![id,item_id,epoch()?]).map_err(|e|e.to_string())?;0}
                };
                let end=length.checked_add(delta.len()).ok_or("Child source length overflow")?;
                tx.execute("UPDATE child_sources SET len=?3 WHERE agent=?1 AND id=?2",params![id,item_id,end]).map_err(|e|e.to_string())?;
                if length<SOURCE_BYTES {
                    let mut keep=(SOURCE_BYTES-length).min(delta.len());
                    while !delta.is_char_boundary(keep) {keep-=1;}
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
                let was_text:bool=self.db.query_row("SELECT supported FROM child_sources WHERE agent=?1 AND id=?2",params![id,item.id],|row|row.get(0)).optional().map_err(|e|e.to_string())?.unwrap_or(false);
                if text.is_some() || was_text {self.source_changed()?;}
                let tx=self.db.transaction().map_err(|e|e.to_string())?;
                tx.execute("INSERT INTO child_sources VALUES(?1,?2,?3,?4,?5) ON CONFLICT(agent,id) DO UPDATE SET epoch=excluded.epoch,len=excluded.len,supported=excluded.supported",params![id,item.id,epoch()?,text.map_or(0,str::len),text.is_some()]).map_err(|e|e.to_string())?;
                tx.execute("DELETE FROM child_source_chunks WHERE agent=?1 AND id=?2",params![id,item.id]).map_err(|e|e.to_string())?;
                if let Some(text)=text.filter(|text|text.len()<=SOURCE_BYTES){child_chunks(&tx,id,&item.id,0,text)?;}
                tx.commit().map_err(|e|e.to_string())?;
            }
            Event::SubagentTextDelta{id,item_id,delta,..}=>{
                if id.len()>METADATA_LIMIT || item_id.len()>METADATA_LIMIT {return Ok(());}
                if !delta.is_empty() {self.source_changed()?;}
                if self.pending_child_source.as_ref().is_some_and(|pending|pending.0!=*id || pending.1!=*item_id || pending.2.len().saturating_add(delta.len())>64*1024) {
                    self.flush_child_source()?;
                }
                if delta.len()>64*1024 {self.append_child_source(id,item_id,delta)?;}
                else if let Some(pending)=&mut self.pending_child_source {pending.2.push_str(delta);}
                else {self.pending_child_source=Some((id.clone(),item_id.clone(),delta.clone()));}
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
        assert_eq!(store.pending_child_source.as_ref().unwrap().2.len(),2048);
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
