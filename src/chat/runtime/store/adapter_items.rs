//! Exact, bounded canonical reads for optional adapter body caches. These never
//! read the presentation window or retain the actor/Store through a strong Arc.
use super::*;
use crate::chat::protocol::{CanonicalItemKey,CanonicalItemReader};
use std::sync::{Arc,Mutex,Weak};
const MAX_REHYDRATION:usize=64*1024*1024;
struct Reader {store:Weak<Mutex<Store>>}
impl CanonicalItemReader for Reader {
    fn read(&self,key:&CanonicalItemKey)->Result<Option<Item>,String> {
        let store=self.store.upgrade().ok_or("Original tool Store is no longer available")?;
        let result=store.lock().unwrap_or_else(|p|p.into_inner()).adapter_item(key);
        result
    }
}
pub(in crate::chat::runtime) fn reader(store:&Arc<Mutex<Store>>)->Arc<dyn CanonicalItemReader> {Arc::new(Reader {store:Arc::downgrade(store)})}
impl Store {
    fn adapter_item(&mut self,key:&CanonicalItemKey)->Result<Option<Item>,String> {
        self.flush_input()?;self.flush_text()?;
        if key.agent_id.is_some() {
            // Child tools require their own canonical ingestion namespace.
            // Returning a 64 KiB window placeholder would corrupt old source.
            return Err("Original delegated tool source is unavailable; its recorded source was preserved".into());
        }
        let record=self.db.query_row("SELECT metadata,len0,len1,len2,spans FROM items WHERE id=?1 AND length(metadata)<=?2 AND length(spans)<=?2",params![key.item_id,METADATA_LIMIT],|row|Ok((row.get::<_,Vec<u8>>(0)?,row.get::<_,i64>(1)?,row.get::<_,i64>(2)?,row.get::<_,i64>(3)?,row.get::<_,Vec<u8>>(4)?))).optional().map_err(|error|error.to_string())?;
        let Some((metadata,len0,len1,len2,spans))=record else {return Ok(None);};
        let lengths=[len0,len1,len2].map(usize::try_from);
        let [Ok(len0),Ok(len1),Ok(len2)]=lengths else {return Err("Original tool source has invalid body lengths".into());};
        if len0.checked_add(len1).and_then(|bytes|bytes.checked_add(len2)).is_none_or(|bytes|bytes>MAX_REHYDRATION) {return Err("Original tool source exceeds the bounded adapter read limit".into());}
        let mut item:Item=postcard::from_bytes(&metadata).map_err(|error|error.to_string())?;
        if item.id!=key.item_id || len2!=0 {return Err("Original tool source identity or layout changed".into());}
        let body0=String::from_utf8(self.field(&key.item_id,0,0,len0)?).map_err(|error|error.to_string())?;
        let body1=String::from_utf8(self.field(&key.item_id,1,0,len1)?).map_err(|error|error.to_string())?;
        match &mut item.content {
            ItemContent::Tool {input,output,..}=> {*input=body0;*output=body1;}
            ItemContent::Command {output,..} if body1.is_empty()=> {*output=body0;}
            ItemContent::Changes {changes,..} if body1.is_empty()=> {
                let spans:Vec<usize>=postcard::from_bytes(&spans).map_err(|error|error.to_string())?;
                if spans.len()!=changes.len() {return Err("Original tool metadata was shortened; refusing to replace its full stored source".into());}
                let mut offset=0usize;
                for (change,len) in changes.iter_mut().zip(spans) {
                    let end=offset.checked_add(len).ok_or("Original tool patch length overflow")?;
                    change.diff=body0.get(offset..end).ok_or("Original tool patch layout changed")?.into();offset=end;
                }
                if offset!=body0.len() {return Err("Original tool patch source is incomplete".into());}
            }
            _=>return Err("Original source is not a canonical tool item".into()),
        }
        Ok(Some(item))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn canonical_tool_reader_restores_large_body_without_paging_or_retaining_store() {
        let mut store=Store::new().unwrap();
        let item=Item {id:"large".into(),role:Role::Assistant,started_at:None,at:None,content:ItemContent::Tool {name:"Write".into(),input:"x".repeat(3*1024*1024),output:"result".into(),status:crate::chat::ToolStatus::Running}};
        store.apply(Event::Item(item.clone())).unwrap();
        let store=Arc::new(Mutex::new(store));let weak=Arc::downgrade(&store);let source=reader(&store);
        let restored=source.read(&CanonicalItemKey::new(None,"large")).unwrap().unwrap();
        assert_eq!(restored.content,item.content);
        assert_eq!(restored.id,item.id);
        assert!(source.read(&CanonicalItemKey::new(Some("child"),"large")).is_err(),"child identity cannot fall through into the root item");
        drop(store);assert!(weak.upgrade().is_none());assert!(source.read(&CanonicalItemKey::new(None,"large")).is_err());
    }
}
