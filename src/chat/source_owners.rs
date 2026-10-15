//! Shared admission for complete logical message bodies reconstructed by the UI.
//! Disk restores reserve before decoding and transfer that charge to actual
//! immutable Item owners. This is payload accounting, not allocator/RSS usage.
use super::{Item, ItemContent, Snapshot, cache::{SourceKey,SourceMetadata,SourceEpoch}};
use std::collections::{HashMap,HashSet};
use std::sync::{Arc,Mutex};

pub(crate) const SOURCE_BYTES:usize=32*1024*1024;
pub(crate) const RESIDENT_BYTES:usize=72*1024*1024;

/// Full prose and patch readers share two slots, including their admission
/// wait. Metadata-only manifest requests do not allocate complete bodies.
pub(crate) fn readers()->&'static Arc<tokio::sync::Semaphore> {
    static READERS:std::sync::OnceLock<Arc<tokio::sync::Semaphore>>=std::sync::OnceLock::new();
    READERS.get_or_init(||Arc::new(tokio::sync::Semaphore::new(2)))
}

static READER_CAPACITY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Allocated String capacity in the two readers, including a completed body
/// awaiting admission. Shared `known` items remain counted only by Owners.
#[derive(Default)]
pub(crate) struct ReaderCapacity(std::sync::atomic::AtomicUsize);
impl ReaderCapacity {
    pub(crate) fn bytes(&self)->usize {self.0.load(std::sync::atomic::Ordering::Relaxed)}
    pub(crate) fn set(&self, bytes: usize) {
        let old = self.0.swap(bytes, std::sync::atomic::Ordering::Relaxed);
        if bytes >= old { READER_CAPACITY.fetch_add(bytes-old, std::sync::atomic::Ordering::Relaxed); }
        else { READER_CAPACITY.fetch_sub(old-bytes, std::sync::atomic::Ordering::Relaxed); }
    }
}
impl Drop for ReaderCapacity {
    fn drop(&mut self) { self.set(0); }
}
pub(crate) fn reader_capacity()->usize { READER_CAPACITY.load(std::sync::atomic::Ordering::Relaxed) }

/// A weak reference follows the actual immutable allocation, including queued
/// render replies and offline writers. It never keeps source text alive.
#[derive(Clone)]
enum WeakOwner { Item(std::sync::Weak<Item>), Patch(std::sync::Weak<str>) }
#[derive(Clone,Copy,Debug,PartialEq,Eq,Hash)]
struct OwnerId(u8,usize);
#[derive(Clone,Copy)]
enum Owner<'a> { Item(&'a Arc<Item>), Patch(&'a Arc<str>) }
impl Owner<'_> {
    fn id(self)->OwnerId {match self {
        Self::Item(item)=>OwnerId(0,Arc::as_ptr(item) as usize),
        Self::Patch(text)=>OwnerId(1,Arc::as_ptr(text) as *const () as usize),
    }}
    fn weak(self)->WeakOwner {match self {Self::Item(item)=>WeakOwner::Item(Arc::downgrade(item)),Self::Patch(text)=>WeakOwner::Patch(Arc::downgrade(text))}}
}
impl WeakOwner {
    fn id(&self)->OwnerId {match self {Self::Item(item)=>OwnerId(0,item.as_ptr() as usize),Self::Patch(text)=>OwnerId(1,text.as_ptr() as *const () as usize)}}
    fn strong_count(&self)->usize {match self {Self::Item(item)=>item.strong_count(),Self::Patch(text)=>text.strong_count()}}
}
#[derive(Default)]
struct Owners {
    entries:Vec<(WeakOwner,usize)>,
    provenance:HashMap<OwnerId,ResidentSource>,
    patch_provenance:HashMap<OwnerId,ResidentPatch>,
    last_emitted:Option<Usage>,
    reserved:usize,
    turnover:Option<(WeakOwner,WeakOwner)>,
}
fn owners()->&'static Arc<Mutex<Owners>> {
    static OWNERS:std::sync::OnceLock<Arc<Mutex<Owners>>>=std::sync::OnceLock::new();
    OWNERS.get_or_init(||Arc::new(Mutex::default()))
}
impl Owners {
    fn prune(&mut self) {
        self.entries.retain(|(owner,_)|owner.strong_count()>0);
        let live:HashSet<_>=self.entries.iter().map(|(owner,_)|owner.id()).collect();
        self.provenance.retain(|pointer,_|live.contains(pointer));
        self.patch_provenance.retain(|pointer,_|live.contains(pointer));
        if self.turnover.as_ref().is_some_and(|(old,new)|old.strong_count()==0 || new.strong_count()==0) {self.turnover=None;}
    }
    fn bytes(&self)->usize {self.entries.iter().map(|(_,bytes)|*bytes).sum()}
    fn admit(&mut self,item:&Arc<Item>,bytes:usize,replaced:Option<&Arc<Item>>)->Admission {
        self.admit_owner(Owner::Item(item),bytes,replaced.map(Owner::Item))
    }
    fn admit_owner(&mut self,item:Owner<'_>,bytes:usize,replaced:Option<Owner<'_>>)->Admission {
        self.prune();
        if bytes>SOURCE_BYTES {return Admission::Capacity;}
        if self.entries.iter().any(|(owner,_)|owner.id()==item.id()) {return Admission::Ready;}
        let total=self.bytes().saturating_add(self.reserved);
        if total.saturating_add(bytes)<=RESIDENT_BYTES {self.entries.push((item.weak(),bytes));return Admission::Ready;}
        if self.turnover.is_some() {return Admission::Turnover;}
        let old=replaced.and_then(|replaced|self.entries.iter().find(|(owner,_)|owner.id()==replaced.id()));
        let Some((old,old_bytes))=old else {return Admission::Capacity;};
        if total.saturating_sub(*old_bytes).saturating_add(bytes)>RESIDENT_BYTES
            || total.saturating_add(bytes)>RESIDENT_BYTES+SOURCE_BYTES {return Admission::Capacity;}
        self.turnover=Some((old.clone(),item.weak()));self.entries.push((item.weak(),bytes));Admission::Ready
    }
}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(crate) enum Admission {Ready,Turnover,Capacity}
#[derive(Clone,Copy,Debug,Default,PartialEq,Eq)]
pub(crate) struct Usage {pub admitted_bytes:usize,pub reserved_bytes:usize}
fn emit(pool:&Arc<Mutex<Owners>>,state:&mut Owners) {
    if !Arc::ptr_eq(pool,owners()) {return;}
    let usage=Usage {admitted_bytes:state.bytes(),reserved_bytes:state.reserved};
    if state.last_emitted==Some(usage) {return;}state.last_emitted=Some(usage);
    tracing::trace!(target:"cagents::memory",owner="chat_source_pool",process_id=std::process::id(),pool_id=1u64,
        accounting="logical_source_payload",admitted_bytes=usage.admitted_bytes,reserved_bytes=usage.reserved_bytes,
        "logical source payload ownership changed");
}
pub(crate) fn usage()->Usage {
    let pool=owners();let mut state=pool.lock().unwrap_or_else(|error|error.into_inner());state.prune();emit(pool,&mut state);
    Usage {admitted_bytes:state.bytes(),reserved_bytes:state.reserved}
}
fn text_bytes(item:&Item)->Option<usize> {match &item.content {ItemContent::Text {text,..}=>Some(text.len()),_=>None}}
pub(crate) fn admit(item:&Arc<Item>,source:&SourceKey,epoch:SourceEpoch,replaced:Option<&Arc<Item>>)->Admission {
    let Some(metadata)=SourceMetadata::from_item(item) else {return Admission::Capacity;};
    let Some(bytes)=text_bytes(item) else {return Admission::Capacity;};if item.id!=source.source_id {return Admission::Capacity;}
    let record=ResidentSource {source:source.clone(),bytes,epoch,metadata};let id=Owner::Item(item).id();
    let pool=owners();let mut state=pool.lock().unwrap_or_else(|error|error.into_inner());
    if state.provenance.get(&id).is_some_and(|previous|previous!=&record) {return Admission::Capacity;}
    let result=state.admit(item,bytes,replaced);
    if result==Admission::Ready {state.provenance.insert(id,record);}
    emit(pool,&mut state);result
}
/// Full prose provenance belongs to the exact loaded Item, never newer raw parts.
#[derive(Clone,Debug,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
pub(crate) struct ResidentSource {pub source:SourceKey,pub bytes:usize,pub epoch:SourceEpoch,pub metadata:SourceMetadata}
/// A patch's original file index/path/kind/length is frozen with its body epoch.
/// Live tool status and timestamps remain in the captured snapshot separately.
#[derive(Clone,Debug,PartialEq,Eq,serde::Serialize,serde::Deserialize)]
pub(crate) struct ResidentPatch {pub source:SourceKey,pub epoch:SourceEpoch,pub file:super::cache::changes::File}
pub(crate) fn admit_patch(text:&Arc<str>,record:&ResidentPatch,replaced:Option<&Arc<str>>)->Admission {
    if record.source.source_id.is_empty() || text.len()!=record.file.source_len {return Admission::Capacity;}
    let pool=owners();let mut state=pool.lock().unwrap_or_else(|error|error.into_inner());
    if state.patch_provenance.get(&Owner::Patch(text).id()).is_some_and(|previous|previous!=record) {return Admission::Capacity;}
    let result=state.admit_owner(Owner::Patch(text),text.len(),replaced.map(Owner::Patch));
    if result==Admission::Ready {state.patch_provenance.insert(Owner::Patch(text).id(),record.clone());}
    emit(pool,&mut state);result
}
pub(crate) fn resident_patch(text:&Arc<str>)->Option<ResidentPatch> {
    let pool=owners();let mut state=pool.lock().unwrap_or_else(|error|error.into_inner());state.prune();emit(pool,&mut state);
    state.patch_provenance.get(&Owner::Patch(text).id()).cloned()
}
pub(crate) fn resident_sources(snapshot:&Snapshot)->Vec<ResidentSource> {
    let pool=owners();let mut state=pool.lock().unwrap_or_else(|error|error.into_inner());state.prune();emit(pool,&mut state);resident_sources_in(&state,snapshot)
}
fn resident_sources_in(owners:&Owners,snapshot:&Snapshot)->Vec<ResidentSource> {
    std::iter::once((None,&snapshot.items)).chain(snapshot.subagents.iter().map(|agent|(Some(agent.id.clone()),&agent.items)))
        .flat_map(|(agent_id,items)|items.iter().filter_map(|item| {
            let record=owners.provenance.get(&Owner::Item(item).id())?;
            (record.source.agent_id==agent_id && record.source.source_id==item.id).then(||record.clone())
        }).collect::<Vec<_>>()).collect()
}
#[derive(Clone,Copy,Debug,PartialEq,Eq)]
pub(crate) enum ReserveError {Capacity,InvalidPayload}
pub(crate) struct Reservation {pool:Arc<Mutex<Owners>>,bytes:usize}
pub(crate) fn reserve(bytes:usize,largest_source:usize)->Result<Reservation,ReserveError> {reserve_in(owners().clone(),bytes,largest_source)}
fn reserve_in(pool:Arc<Mutex<Owners>>,bytes:usize,largest:usize)->Result<Reservation,ReserveError> {
    if largest>SOURCE_BYTES || largest>bytes || (bytes>0 && largest==0) {return Err(ReserveError::InvalidPayload);}
    {let mut owners=pool.lock().unwrap_or_else(|error|error.into_inner());owners.prune();emit(&pool,&mut owners);
        if owners.bytes().saturating_add(owners.reserved).saturating_add(bytes)>RESIDENT_BYTES {return Err(ReserveError::Capacity);}
        owners.reserved+=bytes;emit(&pool,&mut owners);}
    Ok(Reservation {pool,bytes})
}
impl Reservation {
    pub(crate) fn adopt(self,items:&[(Arc<Item>,ResidentSource)])->Result<(),ReserveError> {self.adopt_mixed(items,&[])}
    /// Only the exact source entries recorded in the private cache header are
    /// selected here; raw bounded fragments are accounted separately.
    pub(crate) fn adopt_mixed(mut self,items:&[(Arc<Item>,ResidentSource)],patches:&[(Arc<str>,ResidentPatch)])->Result<(),ReserveError> {
        let mut unique=HashMap::new();let mut prose=HashMap::new();let mut patch_proofs=HashMap::new();
        let mut roots=HashMap::new();let mut root_files=HashMap::new();let mut total=0usize;
        for (item,record) in items {
            let bytes=text_bytes(item).ok_or(ReserveError::InvalidPayload)?;
            if bytes>SOURCE_BYTES || record.bytes!=bytes || record.source.source_id!=item.id
                || SourceMetadata::from_item(item).as_ref()!=Some(&record.metadata) {return Err(ReserveError::InvalidPayload);}
            let owner=Owner::Item(item);let id=owner.id();
            if roots.insert(&record.source,id).is_some_and(|previous|previous!=id) {return Err(ReserveError::InvalidPayload);}
            if let Some(previous)=prose.insert(id,record) {if previous!=record {return Err(ReserveError::InvalidPayload);}}
            if unique.insert(id,(owner,bytes)).is_none() {total=total.checked_add(bytes).ok_or(ReserveError::InvalidPayload)?;}
        }
        for (text,record) in patches {
            let bytes=text.len();if bytes>SOURCE_BYTES || bytes!=record.file.source_len || record.source.source_id.is_empty() {return Err(ReserveError::InvalidPayload);}
            let owner=Owner::Patch(text);let id=owner.id();
            if root_files.insert((&record.source,record.file.file_index),id).is_some_and(|previous|previous!=id) {return Err(ReserveError::InvalidPayload);}
            if let Some(previous)=patch_proofs.insert(id,record) {if previous!=record {return Err(ReserveError::InvalidPayload);}}
            if unique.insert(id,(owner,bytes)).is_none() {total=total.checked_add(bytes).ok_or(ReserveError::InvalidPayload)?;}
        }
        if total!=self.bytes {return Err(ReserveError::InvalidPayload);}
        let mut owners=self.pool.lock().unwrap_or_else(|error|error.into_inner());owners.prune();
        if prose.iter().any(|(id,record)|owners.provenance.get(id).is_some_and(|existing|existing!=*record))
            || patch_proofs.iter().any(|(id,record)|owners.patch_provenance.get(id).is_some_and(|existing|existing!=*record)) {
            drop(owners);return Err(ReserveError::InvalidPayload);
        }
        owners.reserved-=self.bytes;self.bytes=0;
        for (id,(owner,bytes)) in unique {if !owners.entries.iter().any(|(existing,_)|existing.id()==id) {owners.entries.push((owner.weak(),bytes));}}
        for (id,record) in prose {owners.provenance.insert(id,record.clone());}
        for (id,record) in patch_proofs {owners.patch_provenance.insert(id,record.clone());}
        emit(&self.pool,&mut owners);Ok(())
    }
}
impl Drop for Reservation {
    fn drop(&mut self) {if self.bytes>0 {let mut owners=self.pool.lock().unwrap_or_else(|error|error.into_inner());owners.prune();owners.reserved-=self.bytes;emit(&self.pool,&mut owners);}}
}

#[cfg(test)]
mod tests {
    use super::*;
    fn source_epoch(revision:u64)->SourceEpoch {SourceEpoch {incarnation:uuid::Uuid::from_u128(1),revision}}
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
        let record=|item:&Arc<Item>,epoch|ResidentSource {source:SourceKey {agent_id:None,source_id:item.id.clone()},
            bytes:text_bytes(item).unwrap(),epoch,metadata:SourceMetadata::from_item(item).unwrap()};
        let first_record=record(&first,source_epoch(7));
        reservation.adopt(&[(first.clone(),first_record.clone()),(second.clone(),record(&second,source_epoch(8)))]).unwrap();
        assert_eq!(pool.lock().unwrap().provenance[&Owner::Item(&first).id()],first_record);
        let mut replacement_metadata=first_record.metadata.clone();replacement_metadata.at=Some(999);
        let snapshot=Snapshot {items:vec![first.clone()],source_parts:vec![super::super::cache::SourcePart {
            item_id:first.id.clone(),source:first_record.source.clone(),start:0,end:first_record.bytes,source_len:first_record.bytes,
            replacement:false,epoch:source_epoch(99),metadata:replacement_metadata}],..Snapshot::default()};
        assert_eq!(resident_sources_in(&pool.lock().unwrap(),&snapshot),vec![first_record],
            "an old displayed prefix keeps its admitted epoch/metadata while raw source parts move to a replacement");
        drop(snapshot);
        assert_eq!(pool.lock().unwrap().reserved,0);
        assert_eq!(pool.lock().unwrap().bytes(),bytes);
        let queued=first.clone();drop(first);drop(second);
        {let mut owners=pool.lock().unwrap();owners.prune();assert_eq!(owners.bytes(),text_bytes(&queued).unwrap());}
        drop(queued);
        {let mut owners=pool.lock().unwrap();owners.prune();assert_eq!(owners.bytes(),0);}
        let failed=reserve_in(pool.clone(),100,100).unwrap();drop(failed);
        assert_eq!(pool.lock().unwrap().reserved,0);
        let mismatch=reserve_in(pool.clone(),100,100).unwrap();
        let wrong=text("wrong","short");
        assert_eq!(mismatch.adopt(&[(wrong.clone(),record(&wrong,source_epoch(9)))]),Err(ReserveError::InvalidPayload));
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
    fn patch_record(text:&Arc<str>,file_index:usize)->ResidentPatch {
        ResidentPatch {source:SourceKey {agent_id:Some("child".into()),source_id:"opaque:change:root".into()},epoch:source_epoch(7),
            file:super::super::cache::changes::File {file_index,path:format!("src/file-{file_index}.rs"),kind:super::super::ChangeKind::Updated,
                source_len:text.len(),additions:Some(1),deletions:Some(1)}}
    }
    #[test]
    fn mixed_restore_adopts_prose_and_patch_allocations_in_one_pool() {
        let pool=Arc::new(Mutex::new(Owners::default()));let item=text("prose-root","complete prose");
        let patch:Arc<str>=Arc::from("@@ -1,1 +1,1 @@\n-old\n+new\n");
        let prose=ResidentSource {source:SourceKey {agent_id:None,source_id:item.id.clone()},bytes:text_bytes(&item).unwrap(),
            epoch:source_epoch(7),metadata:SourceMetadata::from_item(&item).unwrap()};
        let record=patch_record(&patch,3);let bytes=prose.bytes+patch.len();
        let reservation=reserve_in(pool.clone(),bytes,prose.bytes.max(patch.len())).unwrap();
        reservation.adopt_mixed(&[(item.clone(),prose.clone())],&[(patch.clone(),record.clone())]).unwrap();
        {let owners=pool.lock().unwrap();assert_eq!(owners.reserved,0);assert_eq!(owners.bytes(),bytes);
            assert_eq!(owners.patch_provenance[&Owner::Patch(&patch).id()],record);}
        let pending_writer=patch.clone();let weak=Arc::downgrade(&patch);drop(item);drop(patch);
        {let mut owners=pool.lock().unwrap();owners.prune();assert_eq!(owners.bytes(),pending_writer.len());}
        assert!(weak.upgrade().is_some());drop(pending_writer);
        {let mut owners=pool.lock().unwrap();owners.prune();assert_eq!(owners.bytes(),0);assert!(owners.patch_provenance.is_empty());}
        assert!(weak.upgrade().is_none());
    }
    #[test]
    fn duplicated_decoded_patch_sources_cannot_undercharge_or_conflict() {
        let pool=Arc::new(Mutex::new(Owners::default()));let first:Arc<str>=Arc::from("full patch");
        let decoded_again:Arc<str>=Arc::from(first.as_ref());assert!(!Arc::ptr_eq(&first,&decoded_again));
        let record=patch_record(&first,0);
        // Reserving only one pre-serialization shared owner cannot authorize
        // two decoded owners. Even a correctly doubled reservation must reject
        // ambiguous duplicate source/file attribution, rather than pick one.
        for bytes in [first.len(),first.len()*2] {
            let reservation=reserve_in(pool.clone(),bytes,first.len()).unwrap();
            assert_eq!(reservation.adopt_mixed(&[],&[(first.clone(),record.clone()),(decoded_again.clone(),record.clone())]),Err(ReserveError::InvalidPayload));
            assert_eq!(pool.lock().unwrap().reserved,0);assert_eq!(pool.lock().unwrap().bytes(),0);
        }
        let second_record=patch_record(&decoded_again,1);
        reserve_in(pool.clone(),first.len()*2,first.len()).unwrap().adopt_mixed(&[],&[(first,record),(decoded_again,second_record)]).unwrap();
    }

}
