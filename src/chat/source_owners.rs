//! Shared admission for complete logical message bodies reconstructed by the UI.
//! Disk restores reserve before decoding and transfer that charge to actual
//! immutable Item owners. This is payload accounting, not allocator/RSS usage.
use super::{Item, ItemContent, Snapshot, cache::SourceKey};
use std::collections::{HashMap,HashSet};
use std::sync::{Arc,Mutex};

pub(crate) const SOURCE_BYTES:usize=32*1024*1024;
pub(crate) const RESIDENT_BYTES:usize=72*1024*1024;

/// A lease follows the actual immutable item through snapshots, parser caches
/// and retained trees: weak owners disappear only when the last item owner is
/// gone. Clearing a State cannot release a still-retained item's accounting.
#[derive(Default)]
struct Owners {
    entries:Vec<(std::sync::Weak<Item>,usize)>,
    /// Private offline readers reserve before allocating decoded source bodies.
    reserved:usize,
    /// One global replacement can temporarily overlap the old Item owners.
    turnover:Option<(std::sync::Weak<Item>,std::sync::Weak<Item>)>,
}
fn owners()->&'static Arc<Mutex<Owners>> {
    static OWNERS:std::sync::OnceLock<Arc<Mutex<Owners>>>=std::sync::OnceLock::new();
    OWNERS.get_or_init(||Arc::new(Mutex::default()))
}
impl Owners {
    fn prune(&mut self) {
        self.entries.retain(|(owner,_)|owner.strong_count()>0);
        if self.turnover.as_ref().is_some_and(|(old,new)|old.strong_count()==0 || new.strong_count()==0) {self.turnover=None;}
    }
    fn bytes(&self)->usize {self.entries.iter().map(|(_,bytes)|*bytes).sum()}
    fn admit(&mut self,item:&Arc<Item>,bytes:usize,replaced:Option<&Arc<Item>>)->Admission {
        self.prune();
        if bytes>SOURCE_BYTES {return Admission::Capacity;}
        if self.entries.iter().any(|(owner,_)|owner.as_ptr()==Arc::as_ptr(item)) {return Admission::Ready;}
        let total=self.bytes().saturating_add(self.reserved);
        if total.saturating_add(bytes)<=RESIDENT_BYTES {
            self.entries.push((Arc::downgrade(item),bytes));return Admission::Ready;
        }
        if self.turnover.is_some() {return Admission::Turnover;}
        let old=replaced.and_then(|replaced|self.entries.iter().find(|(owner,_)|owner.as_ptr()==Arc::as_ptr(replaced)));
        let Some((old,old_bytes))=old else {return Admission::Capacity;};
        if total.saturating_sub(*old_bytes).saturating_add(bytes)>RESIDENT_BYTES
            || total.saturating_add(bytes)>RESIDENT_BYTES+SOURCE_BYTES {return Admission::Capacity;}
        self.turnover=Some((old.clone(),Arc::downgrade(item)));
        self.entries.push((Arc::downgrade(item),bytes));Admission::Ready
    }
}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(crate) enum Admission {Ready,Turnover,Capacity}


#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
pub(crate) struct Usage {
    pub admitted_bytes:usize,
    pub reserved_bytes:usize,
}
pub(crate) fn usage()->Usage {
    let mut owners=owners().lock().unwrap_or_else(|error|error.into_inner());owners.prune();
    Usage {admitted_bytes:owners.bytes(),reserved_bytes:owners.reserved}
}
fn text_bytes(item:&Item)->Option<usize> {
    match &item.content {ItemContent::Text {text,..}=>Some(text.len()),_=>None}
}
pub(crate) fn admit(item:&Arc<Item>,replaced:Option<&Arc<Item>>)->Admission {
    let Some(bytes)=text_bytes(item) else {return Admission::Capacity;};
    owners().lock().unwrap_or_else(|error|error.into_inner()).admit(item,bytes,replaced)
}

/// Exact provenance recorded when a projected view is written to private cache.
/// A source ID is opaque; no native-part grammar or byte-range inference occurs.
#[derive(Clone,Debug,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
pub(crate) struct ResidentSource {
    pub source:SourceKey,
    pub bytes:usize,
}
pub(crate) fn resident_sources(snapshot:&Snapshot)->Vec<ResidentSource> {
    let mut owners=owners().lock().unwrap_or_else(|error|error.into_inner());owners.prune();
    let charged:HashSet<_>=owners.entries.iter().map(|(owner,_)|owner.as_ptr()).collect();
    std::iter::once((None,&snapshot.items))
        .chain(snapshot.subagents.iter().map(|agent|(Some(agent.id.clone()),&agent.items)))
        .flat_map(|(agent_id,items)|items.iter().filter_map(|item| {
            if !charged.contains(&Arc::as_ptr(item)) {return None;}
            Some(ResidentSource {source:SourceKey {agent_id:agent_id.clone(),source_id:item.id.clone()},bytes:text_bytes(item)?})
        }).collect::<Vec<_>>()).collect()
}

#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(crate) enum ReserveError {Capacity,InvalidPayload}

/// The reservation follows queued decoded data until adoption or cancellation.
/// It owns no text and releases capacity on every error/drop path.
pub(crate) struct Reservation {
    pool:Arc<Mutex<Owners>>,
    bytes:usize,
}
pub(crate) fn reserve(bytes:usize,largest_source:usize)->Result<Reservation,ReserveError> {
    reserve_in(owners().clone(),bytes,largest_source)
}
fn reserve_in(pool:Arc<Mutex<Owners>>,bytes:usize,largest:usize)->Result<Reservation,ReserveError> {
    if largest>SOURCE_BYTES || largest>bytes || (bytes>0 && largest==0) {return Err(ReserveError::InvalidPayload);}
    {
        let mut owners=pool.lock().unwrap_or_else(|error|error.into_inner());owners.prune();
        if owners.bytes().saturating_add(owners.reserved).saturating_add(bytes)>RESIDENT_BYTES {return Err(ReserveError::Capacity);}
        owners.reserved+=bytes;
    }
    Ok(Reservation {pool,bytes})
}
impl Reservation {
    /// Call off the event thread after validating cache provenance and selecting
    /// precisely the recorded full-source Items, excluding bounded raw fragments.
    /// The caller keeps the decoded snapshot private until this succeeds.
    pub(crate) fn adopt(mut self,items:&[Arc<Item>])->Result<(),ReserveError> {
        let mut unique=HashMap::new();
        let mut total=0usize;
        for item in items {
            if unique.contains_key(&Arc::as_ptr(item)) {continue;}
            let bytes=text_bytes(item).ok_or(ReserveError::InvalidPayload)?;
            if bytes>SOURCE_BYTES {return Err(ReserveError::InvalidPayload);}
            total=total.checked_add(bytes).ok_or(ReserveError::InvalidPayload)?;
            unique.insert(Arc::as_ptr(item),(item,bytes));
        }
        if total!=self.bytes {return Err(ReserveError::InvalidPayload);}
        let mut owners=self.pool.lock().unwrap_or_else(|error|error.into_inner());owners.prune();
        // Transfer under one lock: neither a successful handoff nor an error
        // can create an uncharged interval or double-release the reservation.
        owners.reserved-=self.bytes;self.bytes=0;
        for (pointer,(item,bytes)) in unique {
            if !owners.entries.iter().any(|(owner,_)|owner.as_ptr()==pointer) {
                owners.entries.push((Arc::downgrade(item),bytes));
            }
        }
        Ok(())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {
        if self.bytes>0 {
            let mut owners=self.pool.lock().unwrap_or_else(|error|error.into_inner());
            owners.reserved-=self.bytes;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text(id:&str,body:&str)->Arc<Item> {
        Arc::new(Item {id:id.into(),role:super::super::Role::Assistant,started_at:None,at:None,
            content:ItemContent::Text {text:body.into(),thinking:false,phase:super::super::Phase::Final}})
    }
    #[test]
    fn one_global_turnover_replaces_sources_without_exceeding_transient_cap() {
        const MIB:usize=1024*1024;
        let mut owners=Owners::default();
        let mut old=vec![text("a","a"),text("b","b"),text("c","c")];
        for item in &old {assert_eq!(owners.admit(item,20*MIB,None),Admission::Ready);}
        let cache_owner=old[0].clone();let pending_known=old[0].clone();
        let new_a=text("a","new a");
        assert_eq!(owners.admit(&new_a,20*MIB,Some(&old[0])),Admission::Ready);
        assert_eq!(owners.bytes(),80*MIB);assert!(owners.bytes()<=RESIDENT_BYTES+SOURCE_BYTES);
        let new_b=text("b","new b");
        assert_eq!(owners.admit(&new_b,20*MIB,Some(&old[1])),Admission::Turnover);
        old.remove(0);drop(cache_owner);
        assert_eq!(owners.admit(&new_b,20*MIB,Some(&old[0])),Admission::Turnover);
        drop(pending_known);
        assert_eq!(owners.admit(&new_b,20*MIB,Some(&old[0])),Admission::Ready);
        assert_eq!(owners.bytes(),80*MIB);assert!(owners.bytes()<=RESIDENT_BYTES+SOURCE_BYTES);
        old.remove(0);owners.prune();assert_eq!(owners.bytes(),60*MIB);
        assert!(owners.turnover.is_none());
    }

    #[test]
    fn offline_reservation_transfers_to_actual_weak_owners_and_releases_on_failure() {
        let pool=Arc::new(Mutex::new(Owners::default()));
        let first=text("opaque:root:native-part:1:2","first full source");
        let second=text("another opaque root","second full source");
        let bytes=text_bytes(&first).unwrap()+text_bytes(&second).unwrap();
        let reservation=reserve_in(pool.clone(),bytes,32).unwrap();
        assert_eq!(pool.lock().unwrap().reserved,bytes);
        reservation.adopt(&[first.clone(),second.clone()]).unwrap();
        assert_eq!(pool.lock().unwrap().reserved,0);
        assert_eq!(pool.lock().unwrap().bytes(),bytes);
        let queued=first.clone();drop(first);drop(second);
        {let mut owners=pool.lock().unwrap();owners.prune();assert_eq!(owners.bytes(),text_bytes(&queued).unwrap());}
        drop(queued);
        {let mut owners=pool.lock().unwrap();owners.prune();assert_eq!(owners.bytes(),0);}
        let failed=reserve_in(pool.clone(),100,100).unwrap();drop(failed);
        assert_eq!(pool.lock().unwrap().reserved,0);
        let mismatch=reserve_in(pool.clone(),100,100).unwrap();
        assert_eq!(mismatch.adopt(&[text("wrong","short")]),Err(ReserveError::InvalidPayload));
        assert_eq!(pool.lock().unwrap().reserved,0);
    }
    #[test]
    fn reservation_blocks_readers_before_decode_and_shares_turnover_budget() {
        const MIB:usize=1024*1024;
        let pool=Arc::new(Mutex::new(Owners::default()));
        let first=reserve_in(pool.clone(),60*MIB,20*MIB).unwrap();
        assert!(matches!(reserve_in(pool.clone(),20*MIB,20*MIB),Err(ReserveError::Capacity)));
        assert_eq!(pool.lock().unwrap().reserved,60*MIB);
        let item=text("rpc source","body");
        assert_eq!(pool.lock().unwrap().admit(&item,20*MIB,None),Admission::Capacity);
        drop(first);assert!(reserve_in(pool.clone(),20*MIB,20*MIB).is_ok());
        assert!(matches!(reserve_in(pool,33*MIB,33*MIB),Err(ReserveError::InvalidPayload)));
    }
}
