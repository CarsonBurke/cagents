//! Integration concern: immutable Changes sidecars -> Find and selection.
//! Requires design's External API and canonical Shared/selection support.
use super::*;
use chat::cache::{SourceEpoch,SourceKey,changes::Manifest};

#[derive(Clone,Debug,PartialEq,Eq)]
pub(super) struct Scope {
    target:Target,actor:Option<usize>,session:Option<String>,selected:Option<String>,
    context:u64,revision:u64,cwd:PathBuf,find_active:bool,
}
impl Scope {
    fn current(runtime:&Runtime)->Option<Self> {
        runtime.visible.load(Ordering::Relaxed).then(||Self {target:runtime.target,
            actor:runtime.handle().map(|handle|Arc::as_ptr(handle) as usize),session:runtime.snapshot.session_id.clone(),
            selected:runtime.selected_subagent.clone(),context:runtime.logical_changes.context_token(),
            revision:runtime.logical_changes.revision(),cwd:runtime.session.launch.recipe.working_directory.clone(),
            find_active:runtime.find.as_ref().is_some_and(|query|!query.is_empty())})
    }
    fn same_actor(&self,other:&Self)->bool {
        self.target==other.target && self.actor==other.actor && self.session==other.session && self.selected==other.selected
            && self.context==other.context && self.cwd==other.cwd
    }
}
#[derive(Clone,Debug)]
struct PageProof {offset:usize,page:std::sync::Weak<Manifest>}
impl PartialEq for PageProof {fn eq(&self,other:&Self)->bool {self.offset==other.offset && self.page.ptr_eq(&other.page)}}
impl Eq for PageProof {}
#[derive(Clone,Debug,PartialEq,Eq)]
struct Proof {
    source:SourceKey,epoch:SourceEpoch,cwd:PathBuf,pages:Vec<PageProof>,patches:Vec<(usize,usize)>,
}
impl Proof {
    fn of(source:&changes_sources::FindSource,cwd:&std::path::Path)->Self {
        Self {source:source.source.clone(),epoch:source.epoch,cwd:cwd.to_owned(),
            // Weak pins the exact allocation header after its body is retired,
            // preventing allocator-address reuse from reviving stale metadata.
            pages:source.manifests.iter().map(|page|PageProof {offset:page.offset,page:Arc::downgrade(page)}).collect(),
            patches:source.patches.iter().map(|(index,text)|(*index,Arc::as_ptr(text) as *const () as usize)).collect()}
    }
}
#[derive(Clone)]
struct Leaf {path:Vec<u32>,text:Arc<str>,prefixes:Arc<Vec<[u8;32]>>}
struct Document {proof:Proof,external:Arc<find::External>,leaves:Vec<Leaf>,#[cfg(test)] probe:Option<Probe>}
#[cfg(test)]
impl Drop for Document {fn drop(&mut self) {if let Some(probe)=&self.probe {let _=probe.started.send(std::thread::current().id());probe.release.wait();}}}
type Documents=HashMap<String,Arc<Document>>;
#[derive(Default)]
pub(super) struct State {scope:Option<Scope>,ready:Documents,query:Option<Request>,complete:bool,error:Option<String>}
impl State {
    pub(super) fn clear(&mut self) {
        if let Some(query)=self.query.take() {query.cancelled.store(true,Ordering::Release);}
        let retired=std::mem::take(&mut self.ready);if !retired.is_empty() {retire(retired);}self.scope=None;self.complete=false;self.error=None;
    }
    fn needs(&self,scope:&Scope)->bool {self.scope.as_ref()!=Some(scope)}
    fn stage(&mut self,scope:Scope,sources:Vec<changes_sources::FindSource>,complete:bool,actor_guard:Option<std::sync::Weak<ClientHandle>>)->bool {
        if !self.needs(&scope) {return false;}
        if let Some(query)=self.query.take() {query.cancelled.store(true,Ordering::Release);}
        let same_actor=self.scope.as_ref().is_none_or(|old|old.same_actor(&scope));
        let proofs:HashMap<_,_>=sources.iter().map(|source|(source.source.source_id.as_str(),Proof::of(source,&scope.cwd))).collect();
        let mut retired=HashMap::new();
        for (item,document) in std::mem::take(&mut self.ready) {
            if same_actor && proofs.get(item.as_str())==Some(&document.proof) {self.ready.insert(item,document);}
            else {retired.insert(item,document);}
        }
        // A pending search/copy/selection may refer only to currently proved
        // leaves. An incomplete flag never licenses obsolete source bytes.
        let invalidated=!retired.is_empty();if invalidated {retire(retired);}
        self.query=Some(Request {id:next(),scope:scope.clone(),payload:Arc::new(RequestSlot(Mutex::new(Some(Payload {sources,previous:self.ready.clone(),#[cfg(test)] probe:None})))),complete,
            cancelled:Arc::new(AtomicBool::new(false)),actor_guard});self.scope=Some(scope);self.complete=false;self.error=None;invalidated
    }
    /// A file already represented by the current immutable document can stay
    /// mounted while another file loads. New or replaced bodies wait for the
    /// worker-built logical leaves; no unbound selectable placeholder exists.
    pub(super) fn represents(&self,item:&str,index:usize,text:&Arc<str>,epoch:SourceEpoch)->bool {
        let Ok(index)=u32::try_from(index) else {return false;};
        self.ready.get(item).filter(|document|document.proof.epoch==epoch).is_some_and(|document|
            document.leaves.iter().any(|leaf|leaf.path==[6,index] && Arc::ptr_eq(&leaf.text,text)))
    }
    pub(super) fn metadata_ready(&self,item:&str,epoch:SourceEpoch)->bool {self.ready.get(item).is_some_and(|document|document.proof.epoch==epoch)}
    pub(super) fn selection(&self,item:&str,epoch:SourceEpoch,builder:&mut selection::DocumentBuilder) {
        self.selection_with_pending(item,epoch,&[],builder);
    }
    pub(super) fn selection_with_pending(&self,item:&str,epoch:SourceEpoch,suspended:&[Vec<u32>],builder:&mut selection::DocumentBuilder) {
        let document=self.ready.get(item).filter(|document|document.proof.epoch==epoch);
        let leaves=document.map(|document|document.leaves.as_slice()).unwrap_or_default();
        // Metadata then rename then patch for each original file index. Pending
        // boundaries must interleave with real leaves, never append wholesale.
        let order=|path:&[u32]|(path[1],if path[0]==6 {2}else {path[2]});
        let mut positions=std::collections::BTreeMap::new();
        for path in suspended {positions.insert(order(path),(path.as_slice(),None));}
        for leaf in leaves {positions.insert(order(&leaf.path),(leaf.path.as_slice(),Some(leaf)));}
        builder.stamped(disclosure_intent::source_stamp(epoch),|builder| {
            for (path,leaf) in positions.values() {
                if let Some(leaf)=leaf {builder.indexed(item,path,leaf.text.clone(),leaf.prefixes.clone());}
                else {builder.pending(item,path);}
            }
        });
    }
    fn map(&self)->HashMap<String,Arc<find::External>> {
        self.ready.iter().map(|(item,document)|(item.clone(),document.external.clone())).collect()
    }
}
impl Drop for State {fn drop(&mut self) {self.clear();}}
static NEXT:AtomicU64=AtomicU64::new(1);
fn next()->u64 {NEXT.fetch_add(1,Ordering::Relaxed)}
struct Payload {sources:Vec<changes_sources::FindSource>,previous:Documents,#[cfg(test)] probe:Option<Probe>}
struct RequestSlot(Mutex<Option<Payload>>);
impl Drop for RequestSlot {fn drop(&mut self) {
    if let Some(payload)=self.0.get_mut().unwrap_or_else(|error|error.into_inner()).take() {retire(payload);}
}}
#[derive(Clone)]
struct Request {id:u64,scope:Scope,payload:Arc<RequestSlot>,complete:bool,cancelled:Arc<AtomicBool>,actor_guard:Option<std::sync::Weak<ClientHandle>>}
impl PartialEq for Request {fn eq(&self,other:&Self)->bool {self.id==other.id && self.scope==other.scope}}
impl Eq for Request {}
impl std::hash::Hash for Request {fn hash<H:std::hash::Hasher>(&self,state:&mut H) {
    use std::hash::Hash;std::any::TypeId::of::<Self>().hash(state);self.id.hash(state);self.scope.target.hash(state);
}}
struct Prepared {scope:Scope,documents:Documents,complete:bool,_permit:tokio::sync::OwnedSemaphorePermit,_actor_guard:Option<std::sync::Weak<ClientHandle>>}
#[derive(Clone)]
pub(super) struct Ready(Arc<ReplySlot>);
struct ReplySlot(Mutex<Option<Result<Prepared,String>>>);
impl Drop for ReplySlot {fn drop(&mut self) {
    if let Some(result)=self.0.get_mut().unwrap_or_else(|error|error.into_inner()).take() {retire(result);}
}}
impl Ready {fn new(result:Result<Prepared,String>)->Self {Self(Arc::new(ReplySlot(Mutex::new(Some(result)))))}}
fn workers()->&'static Arc<tokio::sync::Semaphore> {
    static WORKERS:std::sync::OnceLock<Arc<tokio::sync::Semaphore>>=std::sync::OnceLock::new();
    WORKERS.get_or_init(||Arc::new(tokio::sync::Semaphore::new(2)))
}
fn leaf(path:Vec<u32>,text:Arc<str>,prefixes:Option<Arc<Vec<[u8;32]>>>)->Leaf {
    let prefixes=prefixes.unwrap_or_else(||reader_source::Source::new(&text).prefixes());Leaf {path,text,prefixes}
}
fn build(request:&Request)->Result<Documents,String> {
    let payload=request.payload.0.lock().unwrap_or_else(|error|error.into_inner());
    let payload=payload.as_ref().ok_or("Patch indexing was cancelled.")?;
    let mut documents=HashMap::new();
    for source in &payload.sources {
        if request.cancelled.load(Ordering::Acquire) {return Err("Patch indexing was cancelled.".into());}
        if source.source.agent_id!=request.scope.selected {return Err("The patch belongs to another transcript.".into());}
        let proof=Proof::of(source,&request.scope.cwd);let item=&source.source.source_id;
        if let Some(previous)=payload.previous.get(item).filter(|document|document.proof==proof) {
            documents.insert(item.clone(),previous.clone());continue;
        }
        let patches:HashMap<_,_>=source.patches.iter().map(|(index,text)|(*index,text)).collect();
        let checkpoints:HashMap<_,_>=source.prefixes.iter().map(|(index,prefixes)|(*index,prefixes)).collect();
        let mut leaves=Vec::new();let mut expected=0;
        for page in &source.manifests {
            if page.source!=source.source || page.epoch!=source.epoch || page.offset!=expected {return Err("Patch metadata changed while indexing.".into());}
            for file in &page.files {
                if request.cancelled.load(Ordering::Acquire) {return Err("Patch indexing was cancelled.".into());}
                if file.file_index!=expected {return Err("The file order is incomplete.".into());}
                let index=u32::try_from(file.file_index).map_err(|_|"The file index exceeds the logical document range.".to_owned())?;
                let path=step::path_label(&file.path,&request.scope.cwd).display;
                leaves.push(leaf(vec![7,index,0],Arc::from(path),None));
                if let chat::ChangeKind::Renamed {from}=&file.kind {
                    let from=step::path_label(from,&request.scope.cwd).display;leaves.push(leaf(vec![7,index,1],Arc::from(from),None));
                }
                if let Some(text)=patches.get(&file.file_index) {
                    let Some(prefixes)=checkpoints.get(&file.file_index) else {return Err("The patch has no prepared source checkpoint proof.".into());};
                    if text.len()!=file.source_len || prefixes.len()!=text.len()/4096+1 {return Err("The patch source length changed.".into());}
                    leaves.push(leaf(vec![6,index],(*text).clone(),Some((*prefixes).clone())));
                }
                expected+=1;
            }
        }
        let blocks=leaves.iter().map(|leaf|find::Block {identity:selection::logical_identity(item,&leaf.path),text:leaf.text.clone()}).collect::<Vec<_>>();
        let external=Arc::new(find::External {item:Arc::from(item.as_str()),revision:next(),blocks:blocks.into()});
        documents.insert(item.clone(),Arc::new(Document {proof,external,leaves,#[cfg(test)] probe:None}));
    }
    Ok(documents)
}
fn stream(request:&Request)->Pin<Box<dyn iced::futures::Stream<Item=Message>+Send>> {
    let request=request.clone();Box::pin(iced::futures::stream::once(async move {
        let target=request.scope.target;let id=request.id;
        let result=async move {
            let permit=workers().clone().acquire_owned().await.map_err(|_|"Patch indexing stopped.".to_owned())?;
            tokio::task::spawn_blocking(move || {
                let documents=build(&request)?;
                Ok(Prepared {scope:request.scope,documents,complete:request.complete,_permit:permit,_actor_guard:request.actor_guard})
            }).await.map_err(|_|"Patch indexing stopped.".to_owned())?
        }.await;
        action(Action::ChangesFindReady(target,id,Ready::new(result)))
    }))
}
/// No source scan/cloning happens unless the typed sidecar revision changed.
pub(super) fn sync(runtime:&mut Runtime) {
    let Some(scope)=Scope::current(runtime) else {runtime.changes_find.clear();runtime.set_find_external(HashMap::new(),false);return;};
    if !runtime.changes_find.needs(&scope) {return;}
    let invalidated=runtime.changes_find.stage(scope,runtime.logical_changes.find_sources(),runtime.logical_changes.complete_for_find(),runtime.handle().map(Arc::downgrade));
    if invalidated {runtime.presentation_revision=runtime.presentation_revision.wrapping_add(1);}
    publish(runtime,false);
}
fn publish(runtime:&mut Runtime,complete:bool) {
    let active=runtime.find.as_ref().is_some_and(|query|!query.is_empty());
    let documents=if active {runtime.changes_find.map()} else {HashMap::new()};
    runtime.set_find_external(documents,!active || complete);
}
pub(super) fn close(runtime:&mut Runtime) {
    runtime.logical_changes.reconcile_demand(&runtime.expanded,false);
    sync(runtime);publish(runtime,false);
}
pub(super) fn subscriptions(runtime:&Runtime)->Vec<Subscription<Message>> {
    runtime.changes_find.query.as_ref().filter(|_|runtime.visible.load(Ordering::Relaxed))
        .map(|request|vec![Subscription::run_with(request.clone(),stream)]).unwrap_or_default()
}
/// Explicit fixture drain of the production worker stream and completion
/// action. This never changes the production scheduler or auto-runs in new().
#[cfg(test)]
pub(super) fn fixture_stream(runtime:&Runtime)->Option<Pin<Box<dyn iced::futures::Stream<Item=Message>+Send>>> {
    runtime.changes_find.query.as_ref().map(stream)
}
pub(super) fn complete(runtime:&mut Runtime,id:u64,ready:Ready) {
    let Some(result)=ready.0.0.lock().unwrap_or_else(|error|error.into_inner()).take() else {return;};
    let current=Scope::current(runtime);
    let accepted=runtime.changes_find.query.as_ref().is_some_and(|request|request.id==id && Some(&request.scope)==current.as_ref());
    match result {
        Ok(prepared) if accepted && Some(&prepared.scope)==current.as_ref()=>{
            let old=std::mem::replace(&mut runtime.changes_find.ready,prepared.documents);
            runtime.changes_find.query=None;runtime.changes_find.complete=prepared.complete;
            publish(runtime,prepared.complete);
            // Same exact prepared leaves drive the cached selection document.
            runtime.presentation_revision=runtime.presentation_revision.wrapping_add(1);
            retire((old,prepared._permit));
        }
        Ok(prepared)=>retire(prepared),
        Err(error) if accepted=>{
            runtime.changes_find.query=None;runtime.changes_find.complete=false;
            runtime.changes_find.error=Some(error);publish(runtime,false);
        }
        Err(_)=>{}
    }
}
pub(super) fn notice(runtime:&Runtime)->Option<Element<'static,Message>> {
    runtime.changes_find.error.as_ref().map(|error|design::caption(format!("Complete patch search is unavailable: {error}")).into())
}
fn retire(value:impl Send+'static) {
    type Retired=Box<dyn std::any::Any+Send>;
    static SENDER:std::sync::OnceLock<std::sync::mpsc::Sender<Retired>>=std::sync::OnceLock::new();
    let sender=SENDER.get_or_init(|| {
        let (send,receive)=std::sync::mpsc::channel::<Retired>();
        std::thread::Builder::new().name("cagents-patch-index-retire".into()).spawn(move || {for value in receive {drop(value);}}).expect("start patch metadata retirement");send
    });
    if let Err(value)=sender.send(Box::new(value)) {drop(value);}
}
#[cfg(test)]
struct Probe {started:std::sync::mpsc::Sender<std::thread::ThreadId>,release:Arc<std::sync::Barrier>,finished:std::sync::mpsc::Sender<()>}
#[cfg(test)]
impl Drop for Payload {fn drop(&mut self) {if let Some(probe)=&self.probe {let _=probe.started.send(std::thread::current().id());probe.release.wait();}}}
#[cfg(test)]
impl Drop for Probe {fn drop(&mut self) {let _=self.finished.send(());}}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture()->Request {
        use chat::cache::changes::{File,Metadata};
        let source=SourceKey {agent_id:None,source_id:"opaque-native-part-looking-root".into()};
        let epoch=SourceEpoch {incarnation:uuid::Uuid::from_u128(7),revision:1};
        let text:Arc<str>=Arc::from("@@ -1 +1 @@\n-before\n+after λ\n");
        let manifest=Arc::new(Manifest {source:source.clone(),epoch,metadata:Metadata {role:chat::Role::Assistant,
            status:chat::ToolStatus::Succeeded,at:None,started_at:None,total_files:1,total_bytes:text.len(),additions:Some(1),deletions:Some(1),unavailable:None},
            offset:0,next_offset:None,omitted_files:0,files:vec![File {file_index:0,path:"/project/new.rs".into(),
                kind:chat::ChangeKind::Renamed {from:"/project/old.rs".into()},source_len:text.len(),additions:Some(1),deletions:Some(1)}]});
        Request {id:next(),scope:Scope {target:Target {card:CardId::new(),generation:1},actor:Some(42),session:Some("session".into()),selected:None,
            context:1,revision:1,cwd:"/project".into(),find_active:true},payload:Arc::new(RequestSlot(Mutex::new(Some(Payload {sources:vec![changes_sources::FindSource {source,epoch,revision:1,manifests:vec![manifest],
                prefixes:vec![(0,reader_source::Source::new(&text).prefixes())],patches:vec![(0,text)],complete:true}],
            previous:HashMap::new(),probe:None})))),complete:true,cancelled:Arc::new(AtomicBool::new(false)),actor_guard:None}
    }
    fn sources(request:&Request)->Vec<changes_sources::FindSource> {request.payload.0.lock().unwrap().as_ref().unwrap().sources.clone()}
    fn edit(request:&Request,change:impl FnOnce(&mut Payload)) {change(request.payload.0.lock().unwrap().as_mut().unwrap());}
    fn flush() {
        struct Done(std::sync::mpsc::Sender<()>);impl Drop for Done {fn drop(&mut self) {let _=self.0.send(());}}
        let (done,finished)=std::sync::mpsc::channel();retire(Done(done));finished.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    fn probe()->(Probe,std::sync::mpsc::Receiver<std::thread::ThreadId>,Arc<std::sync::Barrier>,std::sync::mpsc::Receiver<()>) {
        let (start,started)=std::sync::mpsc::channel();let (finish,finished)=std::sync::mpsc::channel();let release=Arc::new(std::sync::Barrier::new(2));
        (Probe {started:start,release:release.clone(),finished:finish},started,release,finished)
    }
    #[test]
    fn metadata_and_patch_blocks_share_exact_display_order_ids_bytes_and_checkpoints() {
        let request=fixture();let output=build(&request).unwrap();let document=&output[&sources(&request)[0].source.source_id];
        assert_eq!(document.leaves.iter().map(|leaf|leaf.path.clone()).collect::<Vec<_>>(),vec![vec![7,0,0],vec![7,0,1],vec![6,0]]);
        assert_eq!(document.leaves[0].text.as_ref(),"new.rs");assert_eq!(document.leaves[1].text.as_ref(),"old.rs");
        assert!(Arc::ptr_eq(&document.leaves[2].text,&sources(&request)[0].patches[0].1));
        assert!(Arc::ptr_eq(&document.leaves[2].prefixes,&sources(&request)[0].prefixes[0].1));
        assert!(Arc::ptr_eq(&document.external.blocks[2].text,&document.leaves[2].text));
        assert_eq!(document.external.blocks[2].identity,selection::logical_identity(&sources(&request)[0].source.source_id,&[6,0]));
    }
    #[test]
    fn memoized_external_owner_survives_unrelated_revision_but_not_body_owner_epoch_or_cwd_change() {
        let mut request=fixture();let original=build(&request).unwrap();let item=sources(&request)[0].source.source_id.clone();edit(&request,|payload|payload.previous=original.clone());
        request.scope.revision+=1;edit(&request,|payload|payload.sources[0].revision+=1);let retained=build(&request).unwrap();
        assert!(Arc::ptr_eq(&original[&item].external,&retained[&item].external));
        edit(&request,|payload|payload.sources[0].patches[0].1=Arc::from(payload.sources[0].patches[0].1.as_ref()));let replaced=build(&request).unwrap();
        assert!(!Arc::ptr_eq(&original[&item].external,&replaced[&item].external));assert!(replaced[&item].external.revision>original[&item].external.revision);
        request.scope.cwd="/elsewhere".into();let moved=build(&request).unwrap();assert_eq!(moved[&item].leaves[0].text.as_ref(),"/project/new.rs");
        assert!(!Arc::ptr_eq(&replaced[&item].external,&moved[&item].external));
    }
    #[test]
    fn manifest_identity_keeps_only_weak_headers_and_cannot_alias_a_replacement_page() {
        let request=fixture();let source=sources(&request).remove(0);let page=source.manifests[0].clone();
        let proof=Proof::of(&source,&request.scope.cwd);let weak=Arc::downgrade(&page);
        assert_eq!(Arc::strong_count(&page),3,"proof retains no manifest body owner");
        let replacement=Arc::new((*page).clone());let mut changed=source.clone();changed.manifests[0]=replacement;
        assert_ne!(proof,Proof::of(&changed,&request.scope.cwd),"equal metadata in a new allocation requires fresh leaves");
        drop(changed);drop(source);drop(page);drop(request);flush();assert!(weak.upgrade().is_none());
        assert!(proof.pages[0].page.upgrade().is_none(),"only the retired allocation header remains pinned");
        assert!(proof.pages[0].page.ptr_eq(&weak));
    }
    #[test]
    fn cancelled_or_foreign_scope_cannot_build_and_released_documents_drop_actual_patch_owners() {
        let request=fixture();let weak=Arc::downgrade(&sources(&request)[0].patches[0].1);let output=build(&request).unwrap();
        request.cancelled.store(true,Ordering::Release);assert!(build(&request).is_err());drop(request);assert!(weak.upgrade().is_some());drop(output);flush();assert!(weak.upgrade().is_none());
        let mut foreign=fixture();foreign.scope.selected=Some("other-child".into());assert!(build(&foreign).is_err());
    }
    #[test]
    fn pending_new_epoch_or_body_immediately_withdraws_obsolete_find_and_catalog_owners() {
        for change in 0..2 {
            let mut request=fixture();let initial=build(&request).unwrap();let item=sources(&request)[0].source.source_id.clone();
            let old=Arc::downgrade(&initial[&item].external);let mut state=State {scope:Some(request.scope.clone()),ready:initial,query:None,complete:true,error:None};
            request.scope.revision+=1;
            if change==0 {edit(&request,|payload|payload.sources[0].epoch.incarnation=uuid::Uuid::from_u128(99));}
            else {edit(&request,|payload|payload.sources[0].patches[0].1=Arc::from("new replacement of exactly same logical file"));}
            assert!(state.stage(request.scope.clone(),sources(&request),false,None));
            assert!(state.map().is_empty(),"pending adapter cannot publish obsolete old External");
            assert!(!state.represents(&item,0,&sources(&request)[0].patches[0].1,sources(&request)[0].epoch));
            let mut builder=selection::Catalog::builder();state.selection(&item,sources(&request)[0].epoch,&mut builder);
            assert!(builder.finish().endpoints().is_none(),"same-epoch replacement cannot keep old Catalog bytes");
            assert!(state.query.as_ref().unwrap().payload.0.lock().unwrap().as_ref().unwrap().previous.is_empty(),"invalid data is not a current memoization owner");
            // Old owners may be waiting for off-thread destruction, but no
            // publication or logical document is allowed to discover them.
            drop(state);drop(request);drop(old);
        }
    }
    #[test]
    fn staging_unrelated_revision_keeps_the_same_proved_external_without_catalog_invalidation() {
        let mut request=fixture();let initial=build(&request).unwrap();let item=sources(&request)[0].source.source_id.clone();let owner=initial[&item].external.clone();
        let mut state=State {scope:Some(request.scope.clone()),ready:initial,query:None,complete:true,error:None};request.scope.revision+=1;
        assert!(!state.stage(request.scope.clone(),sources(&request),false,None));assert!(Arc::ptr_eq(&state.map()[&item],&owner));
        assert!(state.represents(&item,0,&sources(&request)[0].patches[0].1,sources(&request)[0].epoch));
    }

    #[test]
    fn clearing_an_unstarted_query_retires_actual_source_payload_off_the_caller_thread() {
        let request=fixture();let weak=Arc::downgrade(&sources(&request)[0].patches[0].1);let (probe,started,release,finished)=probe();
        edit(&request,|payload|payload.probe=Some(probe));
        let mut state=State {query:Some(request),..Default::default()};let caller=std::thread::current().id();state.clear();
        assert_ne!(started.recv_timeout(Duration::from_secs(5)).unwrap(),caller);assert!(weak.upgrade().is_some());
        release.wait();finished.recv_timeout(Duration::from_secs(5)).unwrap();assert!(weak.upgrade().is_none());
    }
    #[test]
    fn dropping_the_last_stale_reply_clone_keeps_its_permit_through_real_background_destruction() {
        let request=fixture();let weak=Arc::downgrade(&sources(&request)[0].patches[0].1);let scope=request.scope.clone();
        let mut documents=build(&request).unwrap();let (probe,started,release,finished)=probe();
        Arc::get_mut(documents.values_mut().next().unwrap()).unwrap().probe=Some(probe);drop(request);flush();
        let permits=Arc::new(tokio::sync::Semaphore::new(1));let permit=permits.clone().try_acquire_owned().unwrap();
        let ready=Ready::new(Ok(Prepared {scope,documents,complete:true,_permit:permit,_actor_guard:None}));let last=ready.clone();drop(ready);
        assert_eq!(permits.available_permits(),0);let caller=std::thread::current().id();drop(last);
        assert_ne!(started.recv_timeout(Duration::from_secs(5)).unwrap(),caller);assert!(weak.upgrade().is_some());assert_eq!(permits.available_permits(),0);
        release.wait();finished.recv_timeout(Duration::from_secs(5)).unwrap();flush();assert!(weak.upgrade().is_none());assert_eq!(permits.available_permits(),1);
    }
    #[test]
    fn patch_index_error_does_not_replace_provider_or_runtime_failure_diagnostics() {
        let session=fresh_chat_session(Provider::Claude,TerminalId::new(),"/project".into(),&Account::system(Provider::Claude),false).unwrap();
        let handle=Arc::new(ClientHandle::fixture(session.clone(),Snapshot {ready:true,error:Some("Provider overloaded: root cause".into()),..Default::default()}));
        let mut runtime=Runtime::new(Target {card:CardId::new(),generation:1},session,Connection::Live(handle),true,crate::settings::EditorSyntaxTheme::default());
        runtime.error=Some("Existing connection diagnostic".into());let mut request=fixture();request.scope=Scope::current(&runtime).unwrap();
        let id=request.id;runtime.changes_find.query=Some(request);complete(&mut runtime,id,Ready::new(Err("Malformed patch file metadata".into())));
        assert_eq!(runtime.error.as_deref(),Some("Existing connection diagnostic"));assert_eq!(runtime.snapshot.error.as_deref(),Some("Provider overloaded: root cause"));
        assert_eq!(runtime.changes_find.error.as_deref(),Some("Malformed patch file metadata"));assert!(notice(&runtime).is_some());
    }

    #[test]
    fn closed_find_rejects_pending_old_adapter_output_and_cannot_republish_external_bodies() {
        let mut app=crate::app::tests::context_inspector_test_app(false);
        let session=fresh_chat_session(Provider::Claude,TerminalId::new(),"/project".into(),&Account::system(Provider::Claude),false).unwrap();
        let handle=Arc::new(ClientHandle::fixture(session.clone(),Snapshot {ready:true,..Default::default()}));
        let mut runtime=Runtime::new(Target {card:CardId::new(),generation:1},session,Connection::Live(handle),true,crate::settings::EditorSyntaxTheme::default());
        runtime.find=Some("after".into());let target=runtime.target;let mut request=fixture();request.scope=Scope::current(&runtime).unwrap();
        let id=request.id;let scope=request.scope.clone();let documents=build(&request).unwrap();let body=Arc::downgrade(&documents.values().next().unwrap().leaves[2].text);
        runtime.changes_find.scope=Some(scope.clone());runtime.changes_find.query=Some(request);
        let permit=Arc::new(tokio::sync::Semaphore::new(1)).try_acquire_owned().unwrap();
        let ready=Ready::new(Ok(Prepared {scope,documents,complete:true,_permit:permit,_actor_guard:None}));
        app.cards.insert(target.card,CardRuntime::Chat(Box::new(runtime)));drop(app.handle_chat_ui(Action::CloseFind(target)));
        let runtime=app.chat_runtime_mut(target).unwrap();assert!(runtime.find.is_none());assert!(runtime.find_external.is_empty());
        complete(runtime,id,ready);assert!(runtime.find_external.is_empty(),"late old Find output cannot refill a closed search map");
        runtime.changes_find.clear();flush();assert!(body.upgrade().is_none());
    }

    #[test]
    fn actual_changes_ready_and_pending_catalogs_carry_exact_source_epoch_even_for_identical_bytes() {
        let request=fixture();let source=sources(&request)[0].clone();let item=source.source.source_id.clone();
        let mut state=State {scope:Some(request.scope.clone()),ready:build(&request).unwrap(),query:None,complete:true,error:None};
        let mut ready=selection::Catalog::builder();state.selection(&item,source.epoch,&mut ready);let ready=ready.finish();
        let id=selection::logical_identity(&item,&[6,0]);let expected=disclosure_intent::source_stamp(source.epoch);
        assert_eq!(ready.source_stamp(id),Some(expected));let old=ready.text(id).unwrap();
        state.ready.clear();let mut pending=selection::Catalog::builder();state.selection_with_pending(&item,source.epoch,&[vec![6,0]],&mut pending);let pending=pending.finish();
        assert!(pending.is_pending(id));assert_eq!(pending.source_stamp(id),Some(expected));assert_eq!(pending.retained_plain_bytes(),0);
        edit(&request,|payload| {
            payload.sources[0].epoch.revision+=1;let epoch=payload.sources[0].epoch;
            for page in &mut payload.sources[0].manifests {Arc::make_mut(page).epoch=epoch;}
        });
        let changed=sources(&request)[0].clone();state.ready=build(&request).unwrap();
        let mut current=selection::Catalog::builder();state.selection(&item,changed.epoch,&mut current);let current=current.finish();
        assert!(Arc::ptr_eq(&old,&current.text(id).unwrap()),"same immutable body cannot stand in for source provenance");
        assert_ne!(current.source_stamp(id),ready.source_stamp(id));
    }

}
