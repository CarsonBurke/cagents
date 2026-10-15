//! Exact, bounded canonical reads for optional adapter body caches. These never
//! read the presentation window or retain the actor/Store through a strong Arc.
use super::*;
use crate::chat::protocol::{CanonicalItemKey,CanonicalItemReader};
use std::sync::{Arc,Mutex,Weak};
const MAX_REHYDRATION:usize=64*1024*1024;
struct Reader {store:Weak<Mutex<Store>>, generation:uuid::Uuid}
impl CanonicalItemReader for Reader {
    fn read(&self,key:&CanonicalItemKey)->Result<Option<Item>,String> {
        let store=self.store.upgrade().ok_or("Original tool Store is no longer available")?;
        let mut store=store.lock().unwrap_or_else(|p|p.into_inner());
        if store.canonical_generation!=self.generation {return Err("Original tool source was revoked by a conversation reset".into());}
        store.adapter_item(key)
    }
}
pub(in crate::chat::runtime) fn reader(store:&Arc<Mutex<Store>>)->Arc<dyn CanonicalItemReader> {
    let generation=store.lock().unwrap_or_else(|p|p.into_inner()).canonical_generation;
    Arc::new(Reader {store:Arc::downgrade(store), generation})
}
impl Store {
    fn strict_adapter_field(&self,id:&str,field:i64,expected:usize)->Result<Vec<u8>,String> {
        if expected>MAX_REHYDRATION {return Err("Original tool source exceeds the bounded adapter read limit".into());}
        let mut query=self.db.prepare_cached("SELECT position,length(data),CASE WHEN length(data)<=?3 THEN data ELSE NULL END FROM chunks WHERE id=?1 AND field=?2 ORDER BY position").map_err(|error|error.to_string())?;
        let rows=query.query_map(params![id,field,PART_BYTES],|row|Ok((row.get::<_,i64>(0)?,row.get::<_,i64>(1)?,row.get::<_,Option<Vec<u8>>>(2)?))).map_err(|error|error.to_string())?;
        let mut result=Vec::with_capacity(expected);
        for row in rows {
            let (position,len,data)=row.map_err(|error|error.to_string())?;
            let position=usize::try_from(position).map_err(|_|"Original tool source has a negative chunk offset")?;
            let data=data.ok_or("Original tool source has an oversized chunk")?;
            if position!=result.len() || data.is_empty() || len!=data.len() as i64 || data.len()>expected.saturating_sub(result.len()) {
                return Err("Original tool source has a missing, overlapping or invalid chunk".into());
            }
            result.extend(data);
        }
        if result.len()!=expected {return Err("Original tool source is incomplete; its stored call was preserved".into());}
        Ok(result)
    }
    fn adapter_item(&mut self,key:&CanonicalItemKey)->Result<Option<Item>,String> {
        self.flush_input()?;self.flush_text()?;
        if key.agent_id.is_some() {
            return self.child_adapter_item(key);
        }
        let record=self.db.query_row("SELECT CASE WHEN length(metadata)<=?2 THEN metadata ELSE NULL END,len0,len1,len2,CASE WHEN length(spans)<=?2 THEN spans ELSE NULL END,source_shortened FROM items WHERE id=?1",params![key.item_id,METADATA_LIMIT],|row|Ok((row.get::<_,Option<Vec<u8>>>(0)?,row.get::<_,i64>(1)?,row.get::<_,i64>(2)?,row.get::<_,i64>(3)?,row.get::<_,Option<Vec<u8>>>(4)?,row.get::<_,bool>(5)?))).optional().map_err(|error|error.to_string())?;
        let Some((metadata,len0,len1,len2,spans,shortened))=record else {return Ok(None);};
        if shortened {return Err("Original tool identity was shortened for presentation; refusing to promote it as complete source".into());}
        let metadata=metadata.ok_or("Original tool metadata exceeds its canonical limit")?;
        let spans=spans.ok_or("Original tool patch metadata exceeds its canonical limit")?;
        let lengths=[len0,len1,len2].map(usize::try_from);
        let [Ok(len0),Ok(len1),Ok(len2)]=lengths else {return Err("Original tool source has invalid body lengths".into());};
        if len0.checked_add(len1).and_then(|bytes|bytes.checked_add(len2)).is_none_or(|bytes|bytes>MAX_REHYDRATION) {return Err("Original tool source exceeds the bounded adapter read limit".into());}
        let mut item:Item=postcard::from_bytes(&metadata).map_err(|error|error.to_string())?;
        if item.id!=key.item_id || len2!=0 {return Err("Original tool source identity or layout changed".into());}
        let body0=String::from_utf8(self.strict_adapter_field(&key.item_id,0,len0)?).map_err(|error|error.to_string())?;
        let body1=String::from_utf8(self.strict_adapter_field(&key.item_id,1,len1)?).map_err(|error|error.to_string())?;
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
    struct CountReader {
        inner:Arc<dyn CanonicalItemReader>,
        reads:Arc<std::sync::atomic::AtomicUsize>,
    }
    impl CanonicalItemReader for CountReader {
        fn read(&self,key:&CanonicalItemKey)->Result<Option<Item>,String> {
            self.reads.fetch_add(1,std::sync::atomic::Ordering::Relaxed);
            self.inner.read(key)
        }
    }
    fn commit(store:&Arc<Mutex<Store>>,adapter:&mut dyn crate::chat::protocol::Adapter,step:crate::chat::protocol::Step) {
        let mut reset=false;
        for event in step.events {reset|=matches!(&event,Event::Reset {..});store.lock().unwrap().apply(event).unwrap();}
        if reset {adapter.bind_item_reader(reader(store));}
        adapter.committed();
    }
    #[test]
    fn byte_evicted_write_results_and_replayed_calls_keep_the_exact_original_patch() {
        use crate::chat::ToolStatus;
        use serde_json::json;
        let store=Arc::new(Mutex::new(Store::new().unwrap()));
        let reads=Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let source=reader(&store);
        let mut adapter=crate::chat::protocol::claude::fixture_adapter();
        adapter.bind_item_reader(Arc::new(CountReader {inner:source.clone(),reads:reads.clone()}));
        let call=|index:usize|json!({"type":"assistant","uuid":format!("carrier-{index}"),"message":{"id":format!("message-{index}"),"content":[{"type":"tool_use","id":format!("write-{index}"),"name":"Write","input":{"file_path":format!("file-{index}.txt"),"content":"x".repeat(1024*1024)}}]}});
        for index in 0..3 {let step=adapter.receive(call(index)).unwrap();commit(&store,adapter.as_mut(),step);}
        let before=source.read(&CanonicalItemKey::new(None,"write-0")).unwrap().unwrap();
        let lookup_before=reads.load(std::sync::atomic::Ordering::Relaxed);
        let step=adapter.receive(json!({"type":"user","uuid":"result-carrier","message":{"content":[{"type":"tool_result","tool_use_id":"write-0","content":"created","is_error":false}]}})).unwrap();
        assert!(reads.load(std::sync::atomic::Ordering::Relaxed)>lookup_before,"a byte-evicted result must recover from canonical source");
        commit(&store,adapter.as_mut(),step);
        let after=source.read(&CanonicalItemKey::new(None,"write-0")).unwrap().unwrap();
        let (ItemContent::Changes {changes:before,..},ItemContent::Changes {changes:after_changes,status})=(&before.content,&after.content) else {panic!("Write must remain an exact file edit rather than become a generic Tool");};
        assert_eq!(status,&ToolStatus::Succeeded);assert_eq!(after_changes,before);
        let replay=adapter.receive(call(0)).unwrap();commit(&store,adapter.as_mut(),replay);
        assert_eq!(source.read(&CanonicalItemKey::new(None,"write-0")).unwrap().unwrap().content,after.content,"an older call replay cannot revive a completed byte-evicted tool");
        drop(store);
        let missing=adapter.receive(json!({"type":"user","uuid":"late","message":{"content":[{"type":"tool_result","tool_use_id":"write-1","content":"late"}]}})).unwrap();
        assert!(missing.events.iter().any(|event|matches!(event,Event::Error {fatal:false,..})),"unavailable canonical source is explicit");
        assert!(!missing.events.iter().any(|event|matches!(event,Event::Item(_))),"source failure cannot invent a replacement tool");
    }
    #[test]
    fn missing_canonical_chunks_never_replace_an_evicted_tool_with_partial_source() {
        use serde_json::json;
        for position in [0,PART_BYTES] {
            let store=Arc::new(Mutex::new(Store::new().unwrap()));
            let mut adapter=crate::chat::protocol::claude::fixture_adapter();adapter.bind_item_reader(reader(&store));
            let step=adapter.receive(json!({"type":"assistant","uuid":"corrupt-call","message":{"content":[{"type":"tool_use","id":"corrupt","name":"Opaque","input":{"content":"x".repeat(3*1024*1024)}}]}})).unwrap();
            commit(&store,adapter.as_mut(),step);
            let before={let store=store.lock().unwrap();store.db.execute("DELETE FROM chunks WHERE id='corrupt' AND field=0 AND position=?1",[position]).unwrap();store.db.query_row("SELECT SUM(length(data)) FROM chunks WHERE id='corrupt'",[],|row|row.get::<_,i64>(0)).unwrap()};
            assert!(reader(&store).read(&CanonicalItemKey::new(None,"corrupt")).is_err());
            let step=adapter.receive(json!({"type":"user","uuid":"corrupt-result","message":{"content":[{"type":"tool_result","tool_use_id":"corrupt","content":"finished","is_error":false}]}})).unwrap();
            assert!(step.events.iter().any(|event|matches!(event,Event::Error {fatal:false,..})));
            assert!(!step.events.iter().any(|event|matches!(event,Event::Item(_))),"partial UTF8-valid input must not be promoted to an authoritative replacement");
            commit(&store,adapter.as_mut(),step);
            assert_eq!(store.lock().unwrap().db.query_row("SELECT SUM(length(data)) FROM chunks WHERE id='corrupt'",[],|row|row.get::<_,i64>(0)).unwrap(),before);
        }
    }

    #[test]
    fn byte_evicted_large_agent_call_does_not_reappear_as_a_duplicate_task_row() {
        use serde_json::json;
        let store=Arc::new(Mutex::new(Store::new().unwrap()));
        let mut adapter=crate::chat::protocol::claude::fixture_adapter();adapter.bind_item_reader(reader(&store));
        let step=adapter.receive(json!({"type":"assistant","uuid":"agent-carrier","message":{"id":"agent-message","content":[{"type":"tool_use","id":"large-agent","name":"Agent","input":{"description":"x".repeat(3*1024*1024),"run_in_background":true}}]}})).unwrap();
        commit(&store,adapter.as_mut(),step);
        let progress=adapter.receive(json!({"type":"system","subtype":"task_progress","task_id":"task-large","tool_use_id":"large-agent","status":"running","description":"Still running"})).unwrap();
        assert!(!progress.events.iter().any(|event|matches!(event,Event::Error {..})),"exact canonical source remains available after byte eviction");
        assert!(!progress.events.iter().any(|event|matches!(event,Event::Item(item) if item.id=="claude-task:task-large")),"a byte-evicted Agent call still represents its task");
    }

    #[test]
    fn source_loss_cannot_suppress_independent_task_completion_or_invent_a_tool_row() {
        use serde_json::json;
        let store=Arc::new(Mutex::new(Store::new().unwrap()));
        let mut adapter=crate::chat::protocol::claude::fixture_adapter();adapter.bind_item_reader(reader(&store));
        let step=adapter.receive(json!({"type":"assistant","uuid":"lost-agent","message":{"content":[{"type":"tool_use","id":"lost-agent","name":"Agent","input":{"description":"x".repeat(3*1024*1024),"run_in_background":true}}]}})).unwrap();
        commit(&store,adapter.as_mut(),step);
        store.lock().unwrap().db.execute("DELETE FROM chunks WHERE id='lost-agent' AND field=0 AND position=0",[]).unwrap();
        let done=adapter.receive(json!({"type":"system","subtype":"task_notification","task_id":"task-lost","tool_use_id":"lost-agent","status":"completed","summary":"Finished work","ambient":true})).unwrap();
        assert!(done.events.iter().any(|event|matches!(event,Event::Error {fatal:false,..})));
        assert!(done.events.iter().any(|event|matches!(event,Event::Subagent {id,status:Some(crate::chat::ToolStatus::Succeeded),..} if id=="lost-agent")),"cache/source availability cannot veto actual provider lifecycle proof");
        assert!(!done.events.iter().any(|event|matches!(event,Event::Item(item) if item.id=="claude-task:task-lost")),"unavailable original source cannot become a fabricated synthetic task");
    }

    #[test]
    fn oversized_same_frame_call_and_result_are_available_until_store_commit() {
        use crate::chat::ToolStatus;
        use serde_json::json;
        let store=Arc::new(Mutex::new(Store::new().unwrap()));
        let mut adapter=crate::chat::protocol::claude::fixture_adapter();adapter.bind_item_reader(reader(&store));
        let step=adapter.receive(json!({"type":"user","uuid":"same-frame","message":{"content":[{"type":"tool_use","id":"large-same-frame","name":"Write","input":{"file_path":"same.txt","content":"x".repeat(3*1024*1024)}},{"type":"tool_result","tool_use_id":"large-same-frame","content":"created","is_error":false}]}})).unwrap();
        assert!(!step.events.iter().any(|event|matches!(event,Event::Error {..})));
        commit(&store,adapter.as_mut(),step);
        let item=reader(&store).read(&CanonicalItemKey::new(None,"large-same-frame")).unwrap().unwrap();
        assert!(matches!(item.content,ItemContent::Changes {status:ToolStatus::Succeeded,..}));
    }

    #[test]
    fn evicted_delegated_write_results_keep_exact_child_parent_and_sibling_sources() {
        use serde_json::json;
        let store=Arc::new(Mutex::new(Store::new().unwrap()));
        let mut adapter=crate::chat::protocol::claude::fixture_adapter();
        adapter.bind_item_reader(reader(&store));
        for (parent,path) in [(None,"parent.txt"),(Some("one"),"one.txt"),(Some("two"),"two.txt")] {
            let step=adapter.receive(json!({"type":"assistant","uuid":format!("call-{path}"),"parent_tool_use_id":parent,"message":{"content":[{"type":"tool_use","id":"shared","name":"Write","input":{"file_path":path,"content":"x".repeat(3*1024*1024)}}]}})).unwrap();
            assert!(!step.events.iter().any(|event|matches!(event,Event::Error {..})));
            commit(&store,adapter.as_mut(),step);
        }
        for (parent,path) in [(Some("two"),"two.txt"),(None,"parent.txt"),(Some("one"),"one.txt")] {
            let step=adapter.receive(json!({"type":"user","uuid":format!("result-{path}"),"parent_tool_use_id":parent,"message":{"content":[{"type":"tool_result","tool_use_id":"shared","content":"created","is_error":false}]}})).unwrap();
            assert!(!step.events.iter().any(|event|matches!(event,Event::Error {..})),"evicted exact child source remains readable");
            commit(&store,adapter.as_mut(),step);
            let item=reader(&store).read(&CanonicalItemKey::new(parent,"shared")).unwrap().unwrap();
            assert!(matches!(item.content,ItemContent::Changes {status:crate::chat::ToolStatus::Succeeded,changes} if changes.len()==1 && changes[0].path.ends_with(path) && changes[0].diff.len()>3*1024*1024));
        }
    }

    #[test]
    fn failed_old_write_preserves_attempted_patch_without_guessing_an_expired_tool_name() {
        use serde_json::json;
        let store=Arc::new(Mutex::new(Store::new().unwrap()));
        let mut adapter=crate::chat::protocol::claude::fixture_adapter();adapter.bind_item_reader(reader(&store));
        let step=adapter.receive(json!({"type":"assistant","uuid":"old-call","message":{"content":[{"type":"tool_use","id":"old-write","name":"Write","input":{"file_path":"attempted.txt","content":"x".repeat(3*1024*1024)}}]}})).unwrap();
        commit(&store,adapter.as_mut(),step);
        for index in 0..513 {
            let step=adapter.receive(json!({"type":"assistant","uuid":format!("later-{index}"),"message":{"content":[{"type":"tool_use","id":format!("later-{index}"),"name":"Opaque","input":{}}]}})).unwrap();
            commit(&store,adapter.as_mut(),step);
        }
        let failed=adapter.receive(json!({"type":"user","uuid":"old-failure","message":{"content":[{"type":"tool_result","tool_use_id":"old-write","is_error":true,"content":"permission denied"}]}})).unwrap();
        assert!(failed.events.iter().any(|event|matches!(event,Event::Error{message,fatal:false} if message.contains("permission denied") && message.contains("identity is unavailable"))));
        assert!(failed.events.iter().any(|event|matches!(event,Event::Item(Item{id,content:ItemContent::Changes{changes,status:crate::chat::ToolStatus::Failed},..}) if id=="old-write" && changes[0].diff.len()>3*1024*1024)));
        assert!(!failed.events.iter().any(|event|matches!(event,Event::Item(Item{id,content:ItemContent::Tool{..},..}) if id=="old-write")),"missing original metadata cannot become a synthetic Edit call");
        commit(&store,adapter.as_mut(),failed);
    }

    #[test]
    fn presentation_shortening_is_not_promoted_to_complete_canonical_tool_identity() {
        let store=Arc::new(Mutex::new(Store::new().unwrap()));
        let item=Item {id:"shortened".into(),role:Role::Assistant,started_at:None,at:None,content:ItemContent::Tool {name:"n".repeat(METADATA_LIMIT+1),input:"original body".into(),output:String::new(),status:crate::chat::ToolStatus::Running}};
        let warnings=store.lock().unwrap().apply(Event::Item(item)).unwrap();
        assert!(warnings.iter().any(|event|matches!(event,Event::Error{fatal:false,..})));
        assert!(reader(&store).read(&CanonicalItemKey::new(None,"shortened")).is_err());
        // A later sparse result cannot silently clear the provenance fence.
        store.lock().unwrap().apply(Event::Item(Item {id:"shortened".into(),role:Role::Assistant,started_at:None,at:None,content:ItemContent::Tool {name:String::new(),input:String::new(),output:"done".into(),status:crate::chat::ToolStatus::Succeeded}})).unwrap();
        assert!(reader(&store).read(&CanonicalItemKey::new(None,"shortened")).is_err());
    }

    #[test]
    fn oversized_existing_metadata_is_an_explicit_source_error_not_a_missing_tool() {
        for field in ["metadata","spans"] {
            let store=Arc::new(Mutex::new(Store::new().unwrap()));
            store.lock().unwrap().apply(Event::Item(Item {id:"known".into(),role:Role::Assistant,started_at:None,at:None,content:ItemContent::Tool {name:"Known".into(),input:"original".into(),output:String::new(),status:crate::chat::ToolStatus::Running}})).unwrap();
            store.lock().unwrap().db.execute(&format!("UPDATE items SET {field}=zeroblob(?1) WHERE id='known'"),[METADATA_LIMIT+1]).unwrap();
            assert!(reader(&store).read(&CanonicalItemKey::new(None,"known")).is_err(),"existing over-limit {field} cannot be interpreted as absent");
        }
    }

    #[test]
    fn conversation_reset_revokes_old_reader_before_reusing_exact_item_identity() {
        let store=Arc::new(Mutex::new(Store::new().unwrap()));
        let original=reader(&store);
        let key=CanonicalItemKey::new(None,"reused");
        let tool=|body:&str|Item {id:"reused".into(),role:Role::Assistant,started_at:None,at:None,content:ItemContent::Tool {name:"Opaque".into(),input:body.into(),output:String::new(),status:crate::chat::ToolStatus::Running}};
        store.lock().unwrap().apply(Event::Item(tool("private old source"))).unwrap();
        assert!(original.read(&key).unwrap().is_some());
        store.lock().unwrap().apply(Event::Reset {reason:"provider reset".into()}).unwrap();
        store.lock().unwrap().apply(Event::Item(tool("new source"))).unwrap();
        assert!(original.read(&key).is_err(),"same Store and reused native ID cannot revive a revoked source reader");
        assert!(matches!(reader(&store).read(&key).unwrap().unwrap().content,ItemContent::Tool {input,..} if input=="new source"));
    }

    #[test]
    fn canonical_tool_reader_restores_large_body_without_paging_or_retaining_store() {
        let mut store=Store::new().unwrap();
        let item=Item {id:"large".into(),role:Role::Assistant,started_at:None,at:None,content:ItemContent::Tool {name:"Write".into(),input:"x".repeat(3*1024*1024),output:"result".into(),status:crate::chat::ToolStatus::Running}};
        store.apply(Event::Item(item.clone())).unwrap();
        let store=Arc::new(Mutex::new(store));let weak=Arc::downgrade(&store);let source=reader(&store);
        let restored=source.read(&CanonicalItemKey::new(None,"large")).unwrap().unwrap();
        assert_eq!(restored.content,item.content);
        assert_eq!(restored.id,item.id);
        assert!(source.read(&CanonicalItemKey::new(Some("child"),"large")).unwrap().is_none(),"child identity cannot fall through into the root item");
        drop(store);assert!(weak.upgrade().is_none());assert!(source.read(&CanonicalItemKey::new(None,"large")).is_err());
    }
}
