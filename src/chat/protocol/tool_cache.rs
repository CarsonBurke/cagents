//! Optional resident copies of canonical tool items, never ownership or history.
use crate::chat::{Item, ItemContent};
use std::{collections::{HashMap,VecDeque},sync::Arc};

pub(crate) const MAX_BYTES:usize=2*1024*1024;
const MAX_ITEMS:usize=512;
const MAX_ID_BYTES:usize=4096;

#[derive(Clone,Debug,PartialEq,Eq,Hash)]
pub struct ItemKey {
    pub agent_id:Option<String>,
    pub item_id:String,
}
impl ItemKey {
    pub fn new(agent_id:Option<&str>,id:&str)->Self {
        Self {agent_id:agent_id.filter(|id|!id.is_empty()).map(str::to_owned),item_id:id.into()}
    }
}
/// Bound to the exact resident Store. Neither an account/recipe lookup nor a
/// presentation window is sufficient authority for reconstructing old tools.
pub trait ItemReader:Send+Sync {
    fn read(&self,key:&ItemKey)->Result<Option<Item>,String>;
}
pub(crate) type Reader=Arc<dyn ItemReader>;

#[derive(Default)]
pub(super) struct ToolCache {
    items:HashMap<ItemKey,Item>,
    order:VecDeque<ItemKey>,
    bytes:usize,
}
impl ToolCache {
    pub fn get(&self,agent:Option<&str>,id:&str)->Option<&Item> {self.items.get(&ItemKey::new(agent,id))}
    pub fn insert(&mut self,agent:Option<&str>,item:Item) {
        if item.id.len()>MAX_ID_BYTES || agent.is_some_and(|id|id.len()>MAX_ID_BYTES) {return;}
        let key=ItemKey::new(agent,&item.id);
        if let Some(old)=self.items.remove(&key) {self.bytes=self.bytes.saturating_sub(cost(&key,&old));}
        self.order.retain(|old|old!=&key);
        self.bytes=self.bytes.saturating_add(cost(&key,&item));
        self.order.push_back(key.clone());self.items.insert(key,item);
    }
    /// Called only after one adapter entry point has finished constructing its
    /// Step. Items emitted earlier in the same frame remain available until
    /// that point; the actor commits them before processing the next frame.
    pub fn trim(&mut self) {
        while self.bytes>MAX_BYTES || self.items.len()>MAX_ITEMS {
            let Some(key)=self.order.pop_front() else {break;};
            if let Some(item)=self.items.remove(&key) {self.bytes=self.bytes.saturating_sub(cost(&key,&item));}
        }
    }
    pub fn remove(&mut self,agent:Option<&str>,id:&str) {
        let key=ItemKey::new(agent,id);
        if let Some(item)=self.items.remove(&key) {self.bytes=self.bytes.saturating_sub(cost(&key,&item));}
        self.order.retain(|old|old!=&key);
    }
    pub fn clear(&mut self) {self.items.clear();self.order.clear();self.bytes=0;}
}
#[cfg(test)]
impl std::ops::Index<&str> for ToolCache {
    type Output=Item;
    fn index(&self,id:&str)->&Item {self.get(None,id).unwrap()}
}
fn cost(key:&ItemKey,item:&Item)->usize {
    let strings=match &item.content {
        ItemContent::Text {text,..}=>text.capacity(),
        ItemContent::Command {label,command,working_directory,output,..}=>label.capacity().saturating_add(command.capacity()).saturating_add(working_directory.capacity()).saturating_add(output.capacity()),
        ItemContent::Changes {changes,..}=>changes.iter().fold(changes.capacity().saturating_mul(std::mem::size_of::<crate::chat::FileChange>()),|bytes,change|bytes.saturating_add(change.path.capacity()).saturating_add(change.diff.capacity()).saturating_add(match &change.kind {crate::chat::ChangeKind::Renamed {from}=>from.capacity(),_=>0})),
        ItemContent::WebSearch {query,results}=>results.iter().fold(query.capacity().saturating_add(results.capacity().saturating_mul(std::mem::size_of::<crate::chat::SearchResult>())),|bytes,result|bytes.saturating_add(result.title.capacity()).saturating_add(result.url.capacity()).saturating_add(result.snippet.capacity())),
        ItemContent::Tool {name,input,output,..}=>name.capacity().saturating_add(input.capacity()).saturating_add(output.capacity()),
        ItemContent::Image {path,description}=>path.capacity().saturating_add(description.capacity()),
        ItemContent::Delivery {from,sender,summary,body,..}=>from.capacity().saturating_add(sender.capacity()).saturating_add(summary.capacity()).saturating_add(body.capacity()),
    };
    strings.saturating_add(item.id.capacity()).saturating_add(key.item_id.capacity().saturating_mul(2)).saturating_add(key.agent_id.as_ref().map_or(0,String::capacity).saturating_mul(2)).saturating_add(std::mem::size_of::<Item>()).saturating_add(2*std::mem::size_of::<ItemKey>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{Role,ToolStatus};
    fn tool(id:&str,bytes:usize)->Item {Item {id:id.into(),role:Role::Assistant,started_at:None,at:None,content:ItemContent::Tool{name:"Write".into(),input:"x".repeat(bytes),output:String::new(),status:ToolStatus::Running}}}
    #[test]
    fn body_budget_counts_pending_tools_and_allocated_capacity() {
        let mut cache=ToolCache::default();
        for index in 0..20 {cache.insert(None,tool(&format!("tool-{index}"),1024*1024));cache.trim();assert!(cache.bytes<=MAX_BYTES);}
        assert!(cache.get(None,"tool-0").is_none());
        assert!(cache.get(None,"tool-19").is_some());
        let mut oversized=tool("reserved",1);
        if let ItemContent::Tool {input,..}=&mut oversized.content {input.reserve(MAX_BYTES*2);}
        cache.insert(None,oversized);cache.trim();assert!(cache.get(None,"reserved").is_none());
        assert!(cache.bytes<=MAX_BYTES);
    }
    #[test]
    fn namespace_and_same_frame_visibility_never_borrow_another_child() {
        let mut cache=ToolCache::default();
        cache.insert(Some("a"),tool("same",MAX_BYTES+1));
        cache.insert(Some("b"),tool("same",1));
        assert_eq!(cache.get(Some("a"),"same").unwrap().content.byte_len(),MAX_BYTES+6);
        assert!(cache.get(None,"same").is_none());
        cache.trim();assert!(cache.get(Some("a"),"same").is_none());assert!(cache.get(Some("b"),"same").is_some());
        cache.remove(Some("b"),"same");assert_eq!(cache.bytes,0);
    }
}
