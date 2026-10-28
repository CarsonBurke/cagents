//! Exact file patches are read independently from bounded display fragments.
//! The metadata root never claims a transport prefix is a complete patch.
use super::*;
use chat::cache::changes::{File,Manifest,ManifestQuery,PatchPage,PatchQuery};

/// A completed read remains within the shared two-reader admission until the
/// caller either adopts its immutable text into the source pool or drops it.
pub(super) struct PendingPatch {
    pub manifest:Arc<Manifest>,
    pub file:File,
    pub text:Arc<str>,
    pub deadline:Instant,
    pub prepared:tool_text::Shared,
    _reader:tokio::sync::OwnedSemaphorePermit,
    _capacity:chat::source_owners::ReaderCapacity,
}

pub(super) async fn read_manifest<F,Fut>(query:ManifestQuery,mut fetch:F)->Result<Arc<Manifest>,String>
where F:FnMut(ManifestQuery)->Fut,
      Fut:std::future::Future<Output=Result<Manifest,chat::cache::SourceError>> {
    if query.limit==0 || query.limit>chat::cache::changes::MANIFEST_FILES {
        return Err("The requested file page is invalid.".into());
    }
    let page=tokio::time::timeout(Duration::from_secs(40),fetch(query.clone())).await
        .map_err(|_|"The original file list could not be read in time.".to_owned())?
        .map_err(|_|"The original file list changed or is unavailable.".to_owned())?;
    let end=page.offset.checked_add(page.files.len()).ok_or("The original file list is invalid.")?;
    let terminal=end.checked_add(page.omitted_files)==Some(page.metadata.total_files);
    let next_valid=match page.next_offset {
        Some(next)=>next==end && next>page.offset && next<page.metadata.total_files && page.omitted_files==0,
        None=>terminal,
    };
    if page.source!=query.source || page.offset!=query.offset
        || query.expected_epoch.is_some_and(|epoch|epoch!=page.epoch)
        || page.files.len()>query.limit || !next_valid
        || (page.omitted_files>0 && page.metadata.unavailable.is_none())
        || page.files.iter().enumerate().any(|(index,file)|file.file_index!=page.offset+index || file.source_len>page.metadata.total_bytes)
        || postcard::to_allocvec(&page).map_err(|_|"The original file list is invalid.".to_owned())?.len()>chat::cache::PAGE_BYTES {
        return Err("The original file list is incomplete or changed. Retry loading it.".into());
    }
    Ok(Arc::new(page))
}

pub(super) async fn read_patch<F,Fut>(manifest:Arc<Manifest>,file:File,mut fetch:F)->Result<PendingPatch,String>
where F:FnMut(PatchQuery)->Fut,
      Fut:std::future::Future<Output=Result<PatchPage,chat::cache::SourceError>> {
    if file.source_len>chat::source_owners::SOURCE_BYTES {
        return Err("This patch exceeds the 32 MiB display limit.".into());
    }
    if !manifest.files.iter().any(|listed|listed==&file) {
        return Err("The patch is not part of the verified file list.".into());
    }
    let deadline=Instant::now()+Duration::from_secs(40);
    tokio::time::timeout(Duration::from_secs(40),async move {
        let reader=chat::source_owners::readers().clone().acquire_owned().await
            .map_err(|_|"Patch loading stopped.".to_owned())?;
        let capacity=chat::source_owners::ReaderCapacity::default();
        let mut text=String::with_capacity(file.source_len);capacity.set(text.capacity());
        let mut offset=0;
        loop {
            let page=fetch(PatchQuery {source:manifest.source.clone(),file_index:file.file_index,offset,
                limit:chat::cache::PAGE_BYTES,expected_epoch:manifest.epoch,until:file.source_len}).await
                .map_err(|error|match error {
                    chat::cache::SourceError::Changed=>"The patch changed while loading. Retry its current contents.".to_owned(),
                    _=>"The complete patch is unavailable. Retry when the session is connected.".to_owned(),
                })?;
            if page.source!=manifest.source || page.epoch!=manifest.epoch || page.file!=file || page.offset!=offset
                || page.text.len()>chat::cache::PAGE_BYTES || page.next_offset>file.source_len || page.next_offset.checked_sub(offset)!=Some(page.text.len())
                || page.complete!=(page.next_offset==file.source_len)
                || (!page.complete && page.next_offset==offset) {
                return Err("The patch changed or returned an incomplete page.".into());
            }
            text.push_str(&page.text);offset=page.next_offset;
            if page.complete {break;}
        }
        // Arc<str> conversion can briefly overlap the builder allocation. Count
        // that explicit stage; it is not hidden inside a claimed RSS bound.
        capacity.set(text.capacity().saturating_add(text.len()));
        let text:Arc<str>=Arc::from(text);capacity.set(text.len());
        tokio::task::spawn_blocking(move || {
            let prepared=tool_text::Shared::prepare(text.clone(),tool_text::Language::Diff);
            PendingPatch {manifest,file,text,deadline,prepared,_reader:reader,_capacity:capacity}
        }).await.map_err(|_|"Patch preparation stopped.".to_owned())
    }).await.map_err(|_|"The complete patch could not be read in time.".to_owned())?
}


#[derive(Default)]
pub(super) struct State {
    intents:disclosure_intent::Cache<chat::cache::SourceKey,Disclosure>,
    entries:std::collections::BTreeMap<chat::cache::SourceKey,Entry>,
    aliases:HashMap<String,chat::cache::SourceKey>,
    raw:Option<Arc<Snapshot>>,
    projected:Option<Arc<Snapshot>>,
    selected:Option<String>,
    session:Option<String>,
    incarnation:Option<uuid::Uuid>,
    revision:u64,
    context:u64,
}
struct Entry {
    revision:u64,
    render_revision:u64,
    copy_all:bool,
    copy_intent:Option<clipboard_intent::Intent>,
    copy_error:Option<String>,
    part:chat::cache::changes::Part,
    pages:std::collections::BTreeMap<usize,Arc<Manifest>>,
    requested:HashMap<usize,u64>,
    page_errors:HashMap<usize,String>,
    wanted_pages:HashSet<usize>,
    manual_pages:HashSet<usize>,
    opened:HashSet<usize>,
    default_open_applied:bool,
    suspended:HashSet<usize>,
    suspended_paths:HashMap<usize,(bool,usize)>,
    patches:HashMap<usize,LoadedPatch>,
    restored:HashMap<usize,(Arc<str>,chat::source_owners::ResidentPatch)>,
    patch_requests:HashMap<usize,u64>,
    patch_errors:HashMap<usize,String>,
}
/// Sparse numeric interaction metadata only: no Part, Manifest, path or body.
struct Disclosure {epoch:chat::cache::SourceEpoch,opened:Vec<usize>,pages:Vec<usize>,default_open_applied:bool,suspended:Vec<usize>,paths:Vec<(usize,(bool,usize))>}
impl Disclosure {
    fn capture(entry:&Entry)->Option<Self> {
        if !entry.default_open_applied && entry.opened.is_empty() && entry.manual_pages.is_empty() {return None;}
        let mut paths=entry.suspended_paths.clone();let mut pages=entry.manual_pages.clone();
        for (offset,page) in &entry.pages {pages.insert(*offset);for file in &page.files {paths.insert(file.file_index,(matches!(file.kind,chat::ChangeKind::Renamed {..}),*offset));}}
        let suspended=entry.suspended.iter().copied().chain(entry.patches.keys().copied()).filter(|index|entry.opened.contains(index)).collect::<HashSet<_>>();
        Some(Self {epoch:entry.part.epoch,opened:entry.opened.iter().copied().collect(),pages:pages.into_iter().collect(),default_open_applied:entry.default_open_applied,
            suspended:suspended.into_iter().collect(),paths:paths.into_iter().collect()})
    }
    fn bytes(&self,key:&chat::cache::SourceKey)->usize {
        std::mem::size_of::<Self>()+std::mem::size_of::<chat::cache::SourceKey>()+key.source_id.len()+key.agent_id.as_ref().map_or(0,String::len)
            +(self.opened.len()+self.pages.len()+self.suspended.len())*std::mem::size_of::<usize>()+self.paths.len()*std::mem::size_of::<(usize,(bool,usize))>()
    }
    fn install(self,entry:&mut Entry) {
        entry.opened=self.opened.into_iter().collect();entry.manual_pages=self.pages.into_iter().collect();entry.default_open_applied=self.default_open_applied;
        entry.suspended=self.suspended.into_iter().collect();entry.suspended_paths=self.paths.into_iter().collect();
    }
}
/// Immutable, ordered logical file descriptors for Find/Catalog. The same
/// patch Arc is drawn and indexed; byte offsets never refer to a raw prefix.
#[derive(Clone)]
pub(super) struct FindSource {
    pub source:chat::cache::SourceKey,
    pub epoch:chat::cache::SourceEpoch,
    pub revision:u64,
    pub manifests:Vec<Arc<Manifest>>,
    pub patches:Vec<(usize,Arc<str>)>,
    pub complete:bool,
    pub prefixes:Vec<(usize,Arc<Vec<[u8;32]>>)>,
}
pub(super) struct LoadedPatch {
    pub text:Arc<str>,
    pub record:chat::source_owners::ResidentPatch,
    pub view:tool_text::ToolText,
    pub prefixes:Arc<Vec<[u8;32]>>,
}
impl LoadedPatch {
    fn take_body(&mut self)->(Arc<str>,Arc<Vec<[u8;32]>>) {
        let text=std::mem::replace(&mut self.text,Arc::from(""));
        let prefixes=std::mem::take(&mut self.prefixes);
        let view=std::mem::replace(&mut self.view,tool_text::ToolText::from_shared(None,
            tool_text::Shared::prepare(Arc::from(""),tool_text::Language::Plain)));
        drop(view);(text,prefixes)
    }
}
impl Drop for LoadedPatch {
    fn drop(&mut self) {
        // All old UI body/index fields are empty before enqueue. A fast
        // reclaimer therefore cannot leave the UI holding the final owner.
        let body=self.take_body();retire_copy(body);
    }
}
/// Action clones share a single completion owner; a stale result is taken and
/// released once, including its reader permit and allocated-capacity guard.
#[derive(Clone)]
pub(super) struct PatchReady(Arc<Mutex<Option<PendingPatch>>>);
static NEXT:AtomicU64=AtomicU64::new(1);
fn attempt()->u64 {NEXT.fetch_add(1,Ordering::Relaxed)}
impl Entry {
    fn new(part:chat::cache::changes::Part)->Self {
        Self {revision:attempt(),render_revision:0,copy_all:false,copy_intent:None,copy_error:None,part,pages:Default::default(),requested:Default::default(),page_errors:Default::default(),
            wanted_pages:Default::default(),manual_pages:Default::default(),opened:Default::default(),default_open_applied:false,suspended:Default::default(),suspended_paths:Default::default(),patches:Default::default(),restored:Default::default(),
            patch_requests:Default::default(),patch_errors:Default::default()}
    }
    fn file(&self,index:usize)->Option<(&Arc<Manifest>,&File)> {
        self.pages.values().find_map(|page|page.files.iter().find(|file|file.file_index==index).map(|file|(page,file)))
    }
    fn current(&self,index:usize)->Option<&LoadedPatch> {
        let (_,file)=self.file(index)?;
        self.patches.get(&index).filter(|loaded|loaded.record.epoch==self.part.epoch
            && loaded.record.source==self.part.source && &loaded.record.file==file)
    }
    fn demand(&mut self,expanded:bool,find:bool) {
        let old=(self.requested.len(),self.patch_requests.len(),self.patches.len(),self.suspended.len(),self.wanted_pages.len());
        if self.copy_all && self.copy_intent.as_ref().is_none_or(|intent|!intent.current()) {
            self.copy_all=false;self.copy_intent=None;self.revision=attempt();
        }
        let all=find || self.copy_all;
        if !all {
            self.wanted_pages=self.manual_pages.clone();
            if !expanded {self.wanted_pages.clear();}
            self.requested.retain(|offset,_|self.wanted_pages.contains(offset));
        }
        if expanded || all {self.wanted_pages.insert(0);}
        if all {
            for page in self.pages.values() {
                if let Some(next)=page.next_offset {self.wanted_pages.insert(next);}
            }
        }
        for offset in &self.wanted_pages {
            if !self.pages.contains_key(offset) && !self.page_errors.contains_key(offset) {
                self.requested.entry(*offset).or_insert_with(attempt);
            }
        }
        let wanted:HashSet<usize>=if all {self.pages.values().flat_map(|page|page.files.iter().map(|file|file.file_index)).chain(self.suspended.iter().copied()).collect()}
            else if expanded {self.opened.clone()} else {HashSet::new()};
        self.patch_requests.retain(|index,_|wanted.contains(index));
        self.patches.retain(|index,_|wanted.contains(index));
        self.restored.retain(|index,_|wanted.contains(index));
        self.suspended.retain(|index|wanted.contains(index));
        let ready:Vec<_>=wanted.into_iter().filter(|index|self.file(*index).is_some()
            && self.current(*index).is_none() && !self.patch_errors.contains_key(index)).collect();
        for index in ready {self.patch_requests.entry(index).or_insert_with(attempt);}
        if old!=(self.requested.len(),self.patch_requests.len(),self.patches.len(),self.suspended.len(),self.wanted_pages.len()) {self.revision=attempt();}
    }
}
impl State {
    pub(super) fn clear(&mut self) {
        self.evict(false);self.entries.clear();self.intents.clear();self.aliases.clear();self.selected=None;self.session=None;self.incarnation=None;
        self.revision=self.revision.wrapping_add(1);self.context=self.context.wrapping_add(1);
    }
    /// Keep only exact-source file/page disclosure intent on ordinary hide.
    /// Manifests, bodies, requests and copy demand are released; source reset,
    /// Ready and account revocation still clear the intent entirely.
    pub(super) fn evict(&mut self,find:bool) {
        if self.raw.is_none() && self.projected.is_none() && self.entries.values().all(|entry|
            entry.pages.is_empty() && entry.patches.is_empty() && entry.restored.is_empty()
                && entry.requested.is_empty() && entry.patch_requests.is_empty() && !entry.copy_all) {return;}
        for entry in self.entries.values_mut() {
            for (offset,page) in &entry.pages {
                for file in &page.files {
                    entry.suspended_paths.insert(file.file_index,(matches!(file.kind,chat::ChangeKind::Renamed {..}),*offset));
                    entry.manual_pages.insert(*offset);
                }
            }
            for index in entry.patches.keys() {
                if entry.opened.contains(index) || find {entry.suspended.insert(*index);}
            }
            entry.patches.clear();retire_copy(std::mem::take(&mut entry.restored));
            retire_copy(std::mem::take(&mut entry.pages));
            entry.requested.clear();entry.patch_requests.clear();entry.wanted_pages.clear();
            entry.copy_all=false;entry.copy_intent=None;entry.copy_error=None;
            entry.revision=attempt();
        }
        if let Some(raw)=self.raw.take() {retire_copy(raw);}
        if let Some(projected)=self.projected.take() {retire_copy(projected);}
        self.context=self.context.wrapping_add(1);self.revision=self.revision.wrapping_add(1);
    }
    /// Previously rendered leaf identities only; no synthetic selection bytes.
    pub(super) fn pending_paths(&self,item:&str)->Vec<Vec<u32>> {
        let Some(entry)=self.aliases.get(item).and_then(|key|self.entries.get(key)) else {return Vec::new();};
        let mut paths=Vec::new();
        for (index,(renamed,_offset)) in &entry.suspended_paths {
            if entry.file(*index).is_none() && let Ok(index)=u32::try_from(*index) {
                paths.push(vec![7,index,0]);if *renamed {paths.push(vec![7,index,1]);}
            }
        }
        for index in &entry.suspended {
            if entry.current(*index).is_none() && let Ok(index)=u32::try_from(*index) {paths.push(vec![6,index]);}
        }
        paths.sort_by_key(|path|(path[1],if path[0]==6 {2}else {path[2]}));paths
    }
    /// Potential prior leaf positions in rendered file order. The selection
    /// adapter substitutes a pending marker only when its real leaf is absent.
    pub(super) fn suspended_paths(&self,item:&str)->Vec<Vec<u32>> {
        let Some(entry)=self.aliases.get(item).and_then(|key|self.entries.get(key)) else {return Vec::new();};
        let mut paths=Vec::new();
        for (index,(renamed,offset)) in &entry.suspended_paths {if (entry.pages.contains_key(offset) || entry.wanted_pages.contains(offset)) && let Ok(index)=u32::try_from(*index) {
            paths.push(vec![7,index,0]);if *renamed {paths.push(vec![7,index,1]);}
        }}
        for index in &entry.suspended {
            // demand() prunes deliberate close/Find-only removals. A demanded
            // same-source retry error still preserves cheap selection intent.
            if let Ok(index)=u32::try_from(*index) {paths.push(vec![6,index]);}
        }
        paths.sort_by_key(|path|(path[1],if path[0]==6 {2}else {path[2]}));paths
    }
    pub(super) fn reconcile_demand(&mut self,expanded:&HashSet<String>,find:bool) {
        let mut changed=false;
        for (key,entry) in &mut self.entries {let old=entry.revision;entry.demand(expanded.contains(&key.source_id),find);changed|=entry.revision!=old;}
        if changed {self.revision=self.revision.wrapping_add(1);}
    }
    pub(super) fn revision(&self)->u64 {self.revision}
    pub(super) fn context_token(&self)->u64 {self.context}
    pub(super) fn item_revision(&self,item:&str)->u64 {self.aliases.get(item).and_then(|key|self.entries.get(key)).map_or(0,|entry|entry.revision.max(entry.render_revision))}
    /// Geometry readiness invalidates containing retained rows without
    /// pretending the immutable file source or Find document changed.
    /// Only current loaded leaves of the selected displayed alias can renew
    /// an accepted Find navigation after asynchronous geometry completes.
    pub(super) fn item_matches(&self,item:&str,matches:impl Fn(&tool_text::ToolText)->bool)->bool {
        self.aliases.get(item).and_then(|key|self.entries.get(key)).is_some_and(|entry|
            entry.patches.iter().any(|(index,loaded)|matches(&loaded.view) && entry.current(*index).is_some()))
    }
    pub(super) fn render_ready(&mut self,ready:impl Fn(&tool_text::ToolText)->bool)->bool {
        let mut changed=false;
        for entry in self.entries.values_mut() {
            let owns=entry.patches.keys().filter_map(|index|entry.current(*index)).any(|patch|ready(&patch.view));
            if owns {entry.render_revision=attempt();changed=true;}
        }
        changed
    }
    pub(super) fn root(&self,item:&str)->Option<&str> {self.aliases.get(item).map(|key|key.source_id.as_str())}
    pub(super) fn metadata(&self,item:&str)->Option<&chat::cache::changes::Metadata> {
        let key=self.aliases.get(item)?;Some(&self.entries.get(key)?.part.metadata)
    }
    /// (records, logical metadata bytes); excludes allocator/container RSS.
    pub(super) fn paged_intent_metadata(&self)->(usize,usize) {self.intents.usage()}
    pub(super) fn retained_bytes(&self)->usize {
        self.entries.values().map(|entry|entry.patches.values().map(|patch|patch.text.len()).sum::<usize>()
            +entry.restored.values().map(|(text,_)|text.len()).sum::<usize>()).sum()
    }
    /// Immutable sidecar owners carry their actual admitted provenance. They
    /// cannot be reconstructed from the empty projected Changes Item body.
    pub(super) fn capture(&self)->chat::source_owners::ResidentChanges {
        chat::source_owners::ResidentChanges {
            manifests:self.entries.values().flat_map(|entry|entry.pages.values().cloned()).collect(),
            patches:self.entries.values().flat_map(|entry|entry.patches.keys().filter_map(|index|entry.current(*index))
                .map(|patch|(patch.text.clone(),patch.record.clone())).chain(entry.restored.values().cloned())).collect(),
        }
    }
    /// Install only already-admitted decoded offline owners into the current
    /// typed projection. Source/hash preparation is scheduled off the UI;
    /// source identity, file metadata and epoch are never inferred from paths.
    pub(super) fn restore(&mut self,view:chat::source_owners::ResidentChanges)->Result<(),String> {
        let mut files=HashMap::new();let mut pages=HashSet::new();
        for page in &view.manifests {
            let Some(entry)=self.entries.get(&page.source).filter(|entry|entry.part.epoch==page.epoch) else {return Err("The offline file list belongs to another source.".into());};
            let query=ManifestQuery {source:page.source.clone(),offset:page.offset,limit:chat::cache::changes::MANIFEST_FILES,expected_epoch:Some(page.epoch)};
            if !page.matches_query(&query) || page.metadata.total_files!=entry.part.metadata.total_files
                || page.metadata.total_bytes!=entry.part.metadata.total_bytes || !pages.insert((page.source.clone(),page.offset)) {
                return Err("The offline file list is inconsistent.".into());
            }
            for file in &page.files {
                if files.insert((page.source.clone(),file.file_index),(page.epoch,file.clone())).is_some() {return Err("The offline file list has duplicate files.".into());}
            }
        }
        let mut seen=HashSet::new();
        for (text,record) in &view.patches {
            if !seen.insert((record.source.clone(),record.file.file_index))
                || files.get(&(record.source.clone(),record.file.file_index))!=Some(&(record.epoch,record.file.clone()))
                || chat::source_owners::resident_patch(text).as_ref()!=Some(record) {
                return Err("The offline patch does not hold its exact decoded source lease.".into());
            }
        }
        for page in view.manifests {self.entries.get_mut(&page.source).expect("validated source").pages.insert(page.offset,page);}
        for (text,record) in view.patches {
            let entry=self.entries.get_mut(&record.source).expect("validated source");let index=record.file.file_index;
            entry.opened.insert(index);entry.restored.insert(index,(text,record));entry.patch_requests.insert(index,attempt());entry.revision=attempt();
        }
        self.revision=self.revision.wrapping_add(1);Ok(())
    }
    pub(super) fn project(&mut self,raw:Arc<Snapshot>,selected:Option<&str>,expanded:&HashSet<String>,find:bool)->Arc<Snapshot> {
        if self.session!=raw.session_id || self.incarnation!=raw.source_incarnation || self.selected.as_deref()!=selected {
            self.clear();self.selected=selected.map(str::to_owned);
        }
        self.session=raw.session_id.clone();self.incarnation=raw.source_incarnation;
        let active=match selected {Some(id)=>raw.subagents.iter().find(|agent|agent.id==id).map(|agent|&agent.items),None=>Some(&raw.items)};
        let parts:HashMap<_,_>=raw.changes_parts.iter().filter(|part|part.source.agent_id.as_deref()==selected
            && raw.source_incarnation.is_none_or(|incarnation|part.epoch.incarnation==incarnation))
            .map(|part|(part.item_id.as_str(),part)).collect();
        let mut visible=HashSet::new();let mut aliases=HashMap::new();
        if let Some(items)=active {
            for item in items {
                if !matches!(item.content,ItemContent::Changes {..}) {continue;}
                let Some(part)=parts.get(item.id.as_str()).copied() else {continue;};
                visible.insert(part.source.clone());aliases.insert(item.id.clone(),part.source.clone());
                aliases.insert(part.source.source_id.clone(),part.source.clone());
                match self.entries.get_mut(&part.source) {
                    Some(entry) if entry.part.epoch==part.epoch=>entry.part=part.clone(),
                    Some(entry)=>{
                        // Old body owners provide one bounded replacement lease;
                        // current() hides them until the new exact file is read.
                        let old=std::mem::take(&mut entry.patches);
                        *entry=Entry::new(part.clone());entry.patches=old;
                        self.revision=self.revision.wrapping_add(1);
                    }
                    None=>{
                        let mut entry=Entry::new(part.clone());
                        if let Some(intent)=self.intents.take(&part.source).filter(|intent|intent.epoch==part.epoch) {intent.install(&mut entry);}
                        self.entries.insert(part.source.clone(),entry);self.revision=self.revision.wrapping_add(1);
                    }
                }
            }
        }
        let removed:Vec<_>=self.entries.keys().filter(|key|!visible.contains(*key)).cloned().collect();
        for key in removed {if let Some(mut entry)=self.entries.remove(&key) {
            if let Some(intent)=Disclosure::capture(&entry) {let bytes=intent.bytes(&key);self.intents.remember(key,intent,bytes);}
            // No active descriptor, pending Catalog leaf, reader, copy demand
            // or strong source owner survives merely because intent does.
            retire_copy(std::mem::take(&mut entry.pages));retire_copy(std::mem::take(&mut entry.restored));
            drop(entry);self.revision=self.revision.wrapping_add(1);
        }}
        self.aliases=aliases;
        for (key,entry) in &mut self.entries {entry.demand(expanded.contains(&key.source_id),find);}
        let unchanged=self.raw.as_ref().is_some_and(|old|Arc::ptr_eq(old,&raw));
        if unchanged && let Some(projected)=&self.projected {return projected.clone();}
        self.raw=Some(raw.clone());
        if self.entries.is_empty() {self.projected=Some(raw.clone());return raw;}
        let mut projected=(*raw).clone();let mut seen=HashSet::new();
        let items=match selected {
            Some(id)=>&mut projected.subagents.iter_mut().find(|agent|agent.id==id).expect("active child").items,
            None=>&mut projected.items,
        };
        *items=items.iter().filter_map(|item|{
            let Some(key)=self.aliases.get(&item.id) else {return Some(item.clone());};
            if !seen.insert(key) {return None;}
            let metadata=&self.entries[key].part.metadata;
            Some(Arc::new(chat::Item {id:key.source_id.clone(),role:metadata.role,at:metadata.at,started_at:metadata.started_at,
                content:ItemContent::Changes {status:metadata.status,changes:Vec::new()}}))
        }).collect();
        // Offline rehydration sees the logical empty root, not the original
        // transport row. Give that displayed root its own explicit metadata
        // alias without claiming its empty body contains complete file patches.
        for (key,entry) in &self.entries {
            projected.changes_parts.retain(|part|part.source!=*key || part.item_id!=key.source_id);
            projected.changes_parts.push(chat::cache::changes::Part {item_id:key.source_id.clone(),source:key.clone(),
                epoch:entry.part.epoch,metadata:entry.part.metadata.clone(),files:Vec::new(),replacement:true});
        }
        let projected=Arc::new(projected);self.projected=Some(projected.clone());projected
    }
    pub(super) fn toggle_file(&mut self,item:&str,index:usize,find:bool) {
        let Some(key)=self.aliases.get(item) else {return;};let Some(entry)=self.entries.get_mut(key) else {return;};
        if !entry.opened.remove(&index) {entry.opened.insert(index);} else if !find {
            entry.suspended.remove(&index);entry.patches.remove(&index);entry.restored.remove(&index);entry.patch_requests.remove(&index);entry.patch_errors.remove(&index);
        }
        entry.demand(true,find);entry.revision=attempt();self.revision=self.revision.wrapping_add(1);
    }
    pub(super) fn page(&mut self,item:&str,offset:usize,find:bool) {
        let Some(key)=self.aliases.get(item) else {return;};let Some(entry)=self.entries.get_mut(key) else {return;};
        if offset!=0 && !entry.pages.values().any(|page|page.next_offset==Some(offset)) {return;}
        entry.manual_pages.insert(offset);entry.wanted_pages.insert(offset);entry.page_errors.remove(&offset);entry.demand(true,find);entry.revision=attempt();self.revision=self.revision.wrapping_add(1);
    }
    pub(super) fn retry(&mut self,item:&str,index:Option<usize>,find:bool) {
        let Some(key)=self.aliases.get(item) else {return;};let Some(entry)=self.entries.get_mut(key) else {return;};
        match index {Some(index)=>{entry.patch_errors.remove(&index);entry.opened.insert(index);},None=>entry.page_errors.clear()}
        entry.demand(true,find);entry.revision=attempt();self.revision=self.revision.wrapping_add(1);
    }
    fn complete_manifest(&mut self,key:&chat::cache::SourceKey,offset:usize,request:u64,result:Result<Arc<Manifest>,String>)->bool {
        let Some(entry)=self.entries.get_mut(key).filter(|entry|entry.requested.get(&offset)==Some(&request)) else {return false;};
        entry.requested.remove(&offset);
        match result {
            Ok(page) if page.source==*key && page.epoch==entry.part.epoch
                && page.metadata.total_files==entry.part.metadata.total_files && page.metadata.total_bytes==entry.part.metadata.total_bytes=>{
                if page.omitted_files>0 && entry.copy_all {entry.copy_all=false;entry.copy_error=Some("The original file list is incomplete. No partial copy was made.".into());}
                if offset==0 && !entry.default_open_applied {if page.metadata.total_files==1 {entry.opened.insert(0);}entry.default_open_applied=true;}
                entry.pages.insert(offset,page);
            }
            Ok(_)=>{entry.copy_all=false;entry.copy_error=Some("The original file list changed. No partial copy was made.".into());entry.page_errors.insert(offset,"The file list changed. Reload its current contents.".into());}
            Err(error)=>{if entry.copy_all {entry.copy_error=Some(error.clone());}entry.copy_all=false;entry.page_errors.insert(offset,error);}
        }
        entry.revision=attempt();self.revision=self.revision.wrapping_add(1);true
    }
    fn complete_patch(&mut self,key:&chat::cache::SourceKey,index:usize,request:u64,result:Result<PatchReady,String>)->bool {
        let Some(entry)=self.entries.get_mut(key).filter(|entry|entry.patch_requests.get(&index)==Some(&request)) else {return false;};
        entry.patch_requests.remove(&index);
        let result=result.and_then(|ready|ready.0.lock().unwrap_or_else(|error|error.into_inner()).take().ok_or_else(||"Patch result expired.".into()));
        match result {
            Ok(patch) if patch.deadline>Instant::now() && patch.manifest.source==*key && patch.manifest.epoch==entry.part.epoch
                && entry.file(index).is_some_and(|(_,file)|*file==patch.file)=>{
                let record=chat::source_owners::ResidentPatch {source:key.clone(),epoch:patch.manifest.epoch,file:patch.file.clone()};
                if chat::source_owners::resident_patch(&patch.text).as_ref()!=Some(&record) {
                    entry.copy_all=false;entry.copy_error=Some("The complete patch no longer holds its source lease. No partial copy was made.".into());
                    entry.patch_errors.insert(index,"The patch no longer holds its source lease. Retry loading it.".into());
                } else {
                    entry.restored.remove(&index);
                    let prefixes=patch.prepared.prefixes();
                    let view=tool_text::ToolText::from_shared(None,patch.prepared);
                    entry.patches.insert(index,LoadedPatch {text:patch.text,record,view,prefixes});
                }
            }
            Ok(_)=>{entry.copy_all=false;entry.copy_error=Some("The original patch changed. No partial copy was made.".into());entry.patch_errors.insert(index,"The patch changed before it could be displayed. Retry loading it.".into());}
            Err(error)=>{if entry.copy_all {entry.copy_error=Some(error.clone());}entry.copy_all=false;entry.patch_errors.insert(index,error);}
        }
        entry.revision=attempt();self.revision=self.revision.wrapping_add(1);true
    }
    pub(super) fn request_copy_all(&mut self,item:&str,intent:clipboard_intent::Intent)->bool {
        if !intent.current() {return false;}
        let Some(key)=self.aliases.get(item) else {return false;};let Some(entry)=self.entries.get_mut(key) else {return false;};
        if entry.part.metadata.unavailable.is_some() || entry.pages.values().any(|page|page.omitted_files>0) {return false;}
        entry.copy_error=None;entry.copy_intent=Some(intent);entry.copy_all=true;entry.demand(true,false);entry.revision=attempt();true
    }
    pub(super) fn take_copy_all(&mut self)->Option<(String,Vec<Arc<str>>,clipboard_intent::Intent)> {
        for (key,entry) in &mut self.entries {
            if !entry.copy_all || !entry.pages.values().any(|page|page.next_offset.is_none() && page.omitted_files==0)
                || entry.pages.values().map(|page|page.files.len()).sum::<usize>()!=entry.part.metadata.total_files {continue;}
            let patches:Option<Vec<_>>=entry.pages.values().flat_map(|page|page.files.iter())
                .map(|file|entry.current(file.file_index).map(|patch|patch.text.clone())).collect();
            if let Some(patches)=patches {
                entry.copy_all=false;let intent=entry.copy_intent.take()?;
                if !intent.current() {continue;}
                return Some((key.source_id.clone(),patches,intent));
            }
        }
        None
    }
    pub(super) fn epoch(&self,item:&str)->Option<chat::cache::SourceEpoch> {
        let key=self.aliases.get(item)?;Some(self.entries.get(key)?.part.epoch)
    }
    pub(super) fn copy_patch(&self,item:&str,index:usize)->Option<Arc<str>> {
        let key=self.aliases.get(item)?;Some(self.entries.get(key)?.current(index)?.text.clone())
    }
    pub(super) fn find_sources(&self)->Vec<FindSource> {
        self.entries.iter().map(|(key,entry)|FindSource {
            source:key.clone(),epoch:entry.part.epoch,revision:entry.revision,
            prefixes:entry.pages.values().flat_map(|page|page.files.iter()).filter_map(|file|
                entry.current(file.file_index).map(|patch|(file.file_index,patch.prefixes.clone()))).collect(),
            manifests:entry.pages.values().cloned().collect(),
            patches:entry.pages.values().flat_map(|page|page.files.iter()).filter_map(|file|
                entry.current(file.file_index).map(|patch|(file.file_index,patch.text.clone()))).collect(),
            complete:entry.pages.values().any(|page|page.next_offset.is_none() && page.omitted_files==0)
                && entry.pages.values().map(|page|page.files.len()).sum::<usize>()==entry.part.metadata.total_files
                && entry.pages.values().flat_map(|page|page.files.iter()).all(|file|entry.current(file.file_index).is_some()),
        }).collect()
    }
    pub(super) fn find_patches(&self)->Vec<(String,usize,Arc<str>)> {
        self.entries.iter().flat_map(|(key,entry)|entry.patches.keys().filter_map(|index|
            entry.current(*index).map(|patch|(key.source_id.clone(),*index,patch.text.clone())))).collect()
    }
    pub(super) fn complete_for_find(&self)->bool {
        self.entries.values().all(|entry|entry.pages.values().any(|page|page.next_offset.is_none() && page.omitted_files==0)
            && entry.pages.values().map(|page|page.files.len()).sum::<usize>()==entry.part.metadata.total_files
            && entry.pages.values().flat_map(|page|page.files.iter()).all(|file|entry.current(file.file_index).is_some()))
    }
}


fn current_item<'a>(runtime:&'a Runtime,item:&str)->Option<&'a chat::Item> {
    let items=match runtime.selected_subagent.as_deref() {
        Some(id)=>runtime.snapshot.subagents.iter().find(|agent|agent.id==id).map(|agent|agent.items.as_slice()),
        None=>Some(runtime.snapshot.items.as_slice()),
    }?;
    items.iter().find(|current|current.id==item).map(Arc::as_ref)
}
pub(super) fn detail(runtime:&Runtime,item:&str)->Option<Element<'static,Message>> {
    let key=runtime.logical_changes.aliases.get(item)?;
    let entry=runtime.logical_changes.entries.get(key)?;
    let target=runtime.target;let mut rows:Vec<Element<'static,Message>>=Vec::new();
    let painted=current_item(runtime,item).and_then(|item|runtime.find_paint_for(item));
    if let Some(reason)=&entry.part.metadata.unavailable {
        rows.push(design::caption(format!("Some original file metadata is unavailable: {reason:?}")).into());
    }
    rows.push(widget::button::text(if entry.copy_all {"Loading all patches for copy…"} else {"Copy all patches"})
        .on_press_maybe((!entry.copy_all && entry.part.metadata.unavailable.is_none() && entry.pages.values().all(|page|page.omitted_files==0)).then(||action(Action::CopyAllChangePatches(target,item.into())))).into());
    if let Some(error)=&entry.copy_error {rows.push(design::caption(error.clone()).into());}
    if entry.pages.is_empty() && entry.page_errors.is_empty() {rows.push(design::caption("Loading original file list…").into());}
    for error in entry.page_errors.values() {
        rows.push(widget::row::with_children(vec![design::caption(error.clone()).width(Length::Fill).into(),
            widget::button::text("Retry file list").on_press(action(Action::RetryChangeSource(target,item.into(),None))).into()])
            .spacing(step::GAP_S).into());
    }
    for page in entry.pages.values() {
        for file in &page.files {
            let index=file.file_index;let open=entry.opened.contains(&index) || runtime.find.as_ref().is_some_and(|query|!query.is_empty());
            let label=step::path_label(&file.path,&runtime.session.launch.recipe.working_directory);
            let Ok(logical_index)=u32::try_from(index) else {rows.push(design::caption("The file index exceeds the selectable document range.").into());continue;};
            let logical=|path:&[u32]|selectable_prose::Logical {identity:selection::logical_identity(item,path),start:0,paint:painted.clone()};
            let title=widget::row::with_children(vec![
                widget::button::custom(widget::icon(Icon::handle(if open {Icon::PanDown}else {Icon::PanEnd})).size(design::COMPACT_ICON_SIZE))
                    .name(if open {"Collapse changed file"} else {"Expand changed file"})
                    .on_press(action(Action::ToggleChangeFile(target,item.into(),index))).into(),
                if runtime.changes_find.metadata_ready(item,entry.part.epoch) {
                    crate::interaction::tooltip(selectable_prose::logical_text(vec![iced::widget::text::Span::new(label.display)].into(),
                        design::TEXT_BODY.into(),iced::Font::MONOSPACE,target,None,logical(&[7,logical_index,0])),label.absolute)
                } else {crate::interaction::tooltip(design::body(label.display),label.absolute)},
                design::caption(format!("+{} −{}",file.additions.map_or("?".into(),|value|value.to_string()),file.deletions.map_or("?".into(),|value|value.to_string()))).into(),
            ]).spacing(step::GAP_S).align_y(Alignment::Center);
            rows.push(title.into());
            if let chat::ChangeKind::Renamed {from}=&file.kind {
                let from=step::path_label(from,&runtime.session.launch.recipe.working_directory);
                rows.push(if runtime.changes_find.metadata_ready(item,entry.part.epoch) {
                    crate::interaction::tooltip(selectable_prose::logical_text(vec![iced::widget::text::Span::new(from.display)].into(),
                        design::TEXT_CAPTION.into(),iced::Font::MONOSPACE,target,None,logical(&[7,logical_index,1])),from.absolute)
                } else {crate::interaction::tooltip(design::caption(from.display),from.absolute)});
            }
            if !open {continue;}
            if let Some(patch)=entry.current(index) {
                rows.push(widget::row::with_children(vec![design::caption("Patch").width(Length::Fill).into(),
                    widget::button::text("Copy patch").on_press(action(Action::CopyChangePatch(target,item.into(),index))).into()])
                    .spacing(step::GAP_S).into());
                if runtime.changes_find.represents(item,index,&patch.text,entry.part.epoch) {
                    rows.push(patch.view.view_logical_with_reader(runtime.syntax_theme,Some((320.0,false)),target,None,
                        logical(&[6,logical_index]),runtime.scroll_memory.clone(),runtime.selected_subagent.clone()));
                } else {rows.push(design::caption("Preparing complete patch selection…").into());}
            } else if let Some(error)=entry.patch_errors.get(&index) {
                rows.push(widget::row::with_children(vec![design::caption(error.clone()).width(Length::Fill).into(),
                    widget::button::text("Retry patch").on_press(action(Action::RetryChangeSource(target,item.into(),Some(index)))).into()])
                    .spacing(step::GAP_S).into());
            } else {rows.push(design::caption("Loading complete patch…").into());}
        }
        if let Some(offset)=page.next_offset && !entry.pages.contains_key(&offset) {
            rows.push(widget::button::text("Show more changed files").on_press(action(Action::LoadChangePage(target,item.into(),offset))).into());
        }
        if page.omitted_files>0 {rows.push(design::caption(format!("{} original files are unavailable; this file list is incomplete.",page.omitted_files)).into());}
    }
    if let Some(notice)=changes_find::notice(runtime) {rows.push(notice);}
    Some(widget::column::with_children(rows).spacing(step::GAP_S).width(Length::Fill).into())
}


#[derive(Clone)]
pub(super) struct CopyReady(Arc<CopySlot>);
struct CopySlot {value:Mutex<Option<Result<CopyOutput,String>>>,intent:clipboard_intent::Intent}
impl Drop for CopySlot {fn drop(&mut self) {if let Some(value)=self.value.get_mut().unwrap_or_else(|error|error.into_inner()).take() {retire_copy(value);}}}
struct CopyOutput {text:String,_capacity:chat::source_owners::ReaderCapacity,_permit:tokio::sync::OwnedSemaphorePermit}
struct CopyInput(Mutex<Option<Vec<Arc<str>>>>);
impl CopyInput {fn take(&self)->Option<Vec<Arc<str>>> {self.0.lock().unwrap_or_else(|error|error.into_inner()).take()}}
impl clipboard_intent::Pending for CopyInput {fn withdraw(&self) {if let Some(texts)=self.take() {retire_copy(texts);}}}
impl Drop for CopyInput {fn drop(&mut self) {if let Some(texts)=self.0.get_mut().unwrap_or_else(|error|error.into_inner()).take() {retire_copy(texts);}}}
fn retire_copy(value:impl Send+'static) {
    type Retired=Box<dyn std::any::Any+Send>;
    static SENDER:std::sync::OnceLock<std::sync::mpsc::Sender<Retired>>=std::sync::OnceLock::new();
    let sender=SENDER.get_or_init(||{let(send,receive)=std::sync::mpsc::channel::<Retired>();std::thread::Builder::new().name("cagents-patch-copy-retire".into()).spawn(move||{for value in receive {drop(value);}}).expect("start patch copy retirement");send});
    if let Err(value)=sender.send(Box::new(value)) {drop(value);}
}
/// Clipboard conversion is whole-source work. Queued input withdrawal and
/// output retirement preserve actual payload leases until final destruction.
pub(super) fn copy_task(runtime:&mut Runtime,one:Option<(String,usize,clipboard_intent::Intent)>)->Task<Message> {
    let find=runtime.find.as_ref().is_some_and(|query|!query.is_empty());
    runtime.logical_changes.reconcile_demand(&runtime.expanded,find);changes_find::sync(runtime);
    let (item,texts,intent)=if let Some((item,index,intent))=one {
        if !intent.current() {return Task::none();}
        let Some(text)=runtime.logical_changes.copy_patch(&item,index) else {return Task::none();};(item,vec![text],intent)
    } else {
        let Some(value)=runtime.logical_changes.take_copy_all() else {return Task::none();};value
    };
    let Some(epoch)=runtime.logical_changes.epoch(&item) else {return Task::none();};
    let target=runtime.target;let context=runtime.logical_changes.context;
    let input=Arc::new(CopyInput(Mutex::new(Some(texts))));intent.register(&input);
    cosmic::task::future(async move {
        let current=intent.clone();
        let value=tokio::time::timeout(Duration::from_secs(40),async move {
            let permit=chat::source_owners::readers().clone().acquire_owned().await.map_err(|_|"Copy stopped.".to_owned())?;
            tokio::task::spawn_blocking(move || {
                let texts=input.take().ok_or_else(||"A newer copy replaced this request.".to_owned())?;
                if !current.current() {return Err("A newer copy replaced this request.".to_owned());}
                let bytes=texts.iter().try_fold(0usize,|total,text|total.checked_add(text.len()).and_then(|total|total.checked_add(1)))
                    .ok_or_else(||"The complete patches are too large to copy.".to_owned())?;
                if bytes>chat::source_owners::SOURCE_BYTES+texts.len() {return Err("The complete patches exceed the 32 MiB copy limit.".to_owned());}
                let capacity=chat::source_owners::ReaderCapacity::default();let mut text=String::with_capacity(bytes);capacity.set(text.capacity());
                for (index,part) in texts.iter().enumerate() {if index>0 && !text.ends_with('\n') {text.push('\n');}text.push_str(part);}
                Ok(CopyOutput {text,_capacity:capacity,_permit:permit})
            }).await.map_err(|_|"Copy stopped.".to_owned())?
        }).await.unwrap_or_else(|_|Err("The complete patches could not be copied in time.".to_owned()));
        let result=Ok(CopyReady(Arc::new(CopySlot {value:Mutex::new(Some(value)),intent})));
        Message::ChatUi(Action::CompleteChangeCopy(target,item,epoch,context,result))
    })
}
pub(super) fn complete_copy(runtime:&mut Runtime,item:&str,epoch:chat::cache::SourceEpoch,context:u64,result:Result<CopyReady,String>)->Task<Message> {
    if runtime.logical_changes.context!=context || runtime.logical_changes.epoch(item)!=Some(epoch) || !runtime.visible.load(Ordering::Relaxed) {return Task::none();}
    // A superseded completion still releases copy-only demand immediately;
    // manually expanded and active Find sources remain independently wanted.
    let find=runtime.find.as_ref().is_some_and(|query|!query.is_empty());
    runtime.logical_changes.reconcile_demand(&runtime.expanded,find);changes_find::sync(runtime);
    let value=match result {
        Ok(ready)=>{
            if !ready.0.intent.current() {return Task::none();}
            let Some(value)=ready.0.value.lock().unwrap_or_else(|error|error.into_inner()).take() else {return Task::none();};value
        }
        Err(error)=>Err(error),
    };
    match value {
        Ok(output)=>{
            runtime.copied=Some((item.into(),Instant::now()));
            let find=runtime.find.as_ref().is_some_and(|query|!query.is_empty());
            runtime.logical_changes.reconcile_demand(&runtime.expanded,find);changes_find::sync(runtime);
            return iced::clipboard::write(output.text).map(cosmic::Action::App);
        },
        Err(error)=>if let Some(key)=runtime.logical_changes.aliases.get(item)
            && let Some(entry)=runtime.logical_changes.entries.get_mut(key) {
            entry.page_errors.insert(0,error);entry.revision=attempt();
        },
    }
    Task::none()
}

#[derive(Clone)]
enum Read {Manifest(ManifestQuery),Patch(Arc<Manifest>,File,Option<Arc<str>>),Offline(Arc<Manifest>,File,Arc<str>)}
#[derive(Clone)]
struct Request {target:Target,key:chat::cache::SourceKey,index:usize,attempt:u64,patch:bool,handle:Option<ClientHandle>,read:Read}
impl PartialEq for Request {fn eq(&self,other:&Self)->bool {self.target==other.target && self.key==other.key
    && self.index==other.index && self.attempt==other.attempt && self.patch==other.patch}}
impl Eq for Request {}
impl std::hash::Hash for Request {fn hash<H:std::hash::Hasher>(&self,state:&mut H) {
    use std::hash::Hash;std::any::TypeId::of::<Self>().hash(state);self.target.hash(state);self.key.hash(state);
    self.index.hash(state);self.attempt.hash(state);self.patch.hash(state);
}}
async fn prepare_offline(manifest:Arc<Manifest>,file:File,text:Arc<str>)->Result<PatchReady,String> {
    tokio::time::timeout(Duration::from_secs(40),async move {
        let reader=chat::source_owners::readers().clone().acquire_owned().await.map_err(|_|"Patch loading stopped.".to_owned())?;
        tokio::task::spawn_blocking(move || {
            let prepared=tool_text::Shared::prepare(text.clone(),tool_text::Language::Diff);
            // The decoded immutable allocation is already charged to Owners.
            PatchReady(Arc::new(Mutex::new(Some(PendingPatch {manifest,file,text,prepared,
                deadline:Instant::now()+Duration::from_secs(40),_reader:reader,_capacity:Default::default()}))))
        }).await.map_err(|_|"Patch preparation stopped.".to_owned())
    }).await.map_err(|_|"Offline patch preparation timed out.".to_owned())?
}

fn stream(request:&Request)->Pin<Box<dyn iced::futures::Stream<Item=Message>+Send>> {
    let request=request.clone();Box::pin(iced::futures::stream::once(async move {
        match request.read {
            Read::Offline(manifest,file,text)=>{
                let result=prepare_offline(manifest,file,text).await;
                action(Action::ChangePatchReady(request.target,request.key,request.index,request.attempt,result))
            }
            Read::Manifest(query)=>{
                let Some(handle)=request.handle else {return action(Action::ChangesManifestReady(request.target,request.key,request.index,request.attempt,Err("Reconnect to load the original file list.".into())));};
                let result=read_manifest(query,move |query|{let handle=handle.clone();async move {handle.changes_manifest(query).await}}).await;
                action(Action::ChangesManifestReady(request.target,request.key,request.index,request.attempt,result))
            }
            Read::Patch(manifest,file,replaced)=>{
                let Some(handle)=request.handle else {return action(Action::ChangePatchReady(request.target,request.key,request.index,request.attempt,Err("Reconnect to load the complete patch.".into())));};
                let result=async {
                    let pending=read_patch(manifest,file,move |query|{let handle=handle.clone();async move {handle.change_patch(query).await}}).await?;
                    let record=chat::source_owners::ResidentPatch {source:pending.manifest.source.clone(),epoch:pending.manifest.epoch,file:pending.file.clone()};
                    loop {
                        match chat::source_owners::admit_patch(&pending.text,&record,replaced.as_ref()) {
                            chat::source_owners::Admission::Ready=>break,
                            chat::source_owners::Admission::Turnover if pending.deadline>Instant::now()=>tokio::time::sleep(Duration::from_millis(16)).await,
                            _=>return Err("Complete patches have reached the display memory limit. Close another expanded patch and retry.".to_owned()),
                        }
                    }
                    Ok(PatchReady(Arc::new(Mutex::new(Some(pending)))))
                }.await;
                action(Action::ChangePatchReady(request.target,request.key,request.index,request.attempt,result))
            }
        }
    }))
}
pub(super) fn subscriptions(runtime:&Runtime)->Vec<Subscription<Message>> {
    if !runtime.visible.load(Ordering::Relaxed) {return Vec::new();}
    let handle=runtime.handle().map(|handle|(**handle).clone());let mut requests=Vec::new();
    for (key,entry) in &runtime.logical_changes.entries {
        for (&offset,&attempt) in &entry.requested {
            requests.push(Request {target:runtime.target,key:key.clone(),index:offset,attempt,patch:false,handle:handle.clone(),
                read:Read::Manifest(ManifestQuery {source:key.clone(),offset,limit:chat::cache::changes::MANIFEST_FILES,expected_epoch:Some(entry.part.epoch)})});
        }
        for (&index,&attempt) in &entry.patch_requests {
            if let Some((manifest,file))=entry.file(index) {
                requests.push(Request {target:runtime.target,key:key.clone(),index,attempt,patch:true,handle:handle.clone(),
                    read:entry.restored.get(&index).map_or_else(
                        ||Read::Patch(manifest.clone(),file.clone(),entry.patches.get(&index).map(|patch|patch.text.clone())),
                        |(text,_)|Read::Offline(manifest.clone(),file.clone(),text.clone()))});
            }
        }
    }
    requests.into_iter().map(|request|Subscription::run_with(request,stream)).collect()
}
pub(super) fn manifest_ready(runtime:&mut Runtime,key:&chat::cache::SourceKey,offset:usize,attempt:u64,result:Result<Arc<Manifest>,String>) {
    if runtime.visible.load(Ordering::Relaxed) && runtime.logical_changes.complete_manifest(key,offset,attempt,result) {runtime.refresh();runtime.refresh_find();}
}
pub(super) fn patch_ready(runtime:&mut Runtime,key:&chat::cache::SourceKey,index:usize,attempt:u64,result:Result<PatchReady,String>) {
    if runtime.visible.load(Ordering::Relaxed) && runtime.logical_changes.complete_patch(key,index,attempt,result) {
        runtime.logical_changes.reconcile_demand(&runtime.expanded,runtime.find.as_ref().is_some_and(|query|!query.is_empty()));
        runtime.refresh_find();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat::{ChangeKind,FileChange,ToolStatus};
    fn epoch(revision:u64)->chat::cache::SourceEpoch {chat::cache::SourceEpoch {incarnation:uuid::Uuid::from_u128(1),revision}}
    fn fixture(text:&str)->Arc<Manifest> {
        Arc::new(Manifest {source:chat::cache::SourceKey {agent_id:None,source_id:"opaque:native-part:looks-like-a-suffix".into()},
            epoch:epoch(7),metadata:chat::cache::changes::Metadata {role:chat::Role::Assistant,status:ToolStatus::Succeeded,
                at:Some(20),started_at:Some(10),total_files:1,total_bytes:text.len(),additions:Some(1),deletions:Some(1),unavailable:None},
            offset:0,next_offset:None,files:vec![File {file_index:0,path:"src/fixture.rs".into(),kind:ChangeKind::Updated,
                source_len:text.len(),additions:Some(1),deletions:Some(1)}],omitted_files:0})
    }
    fn page(query:PatchQuery,text:&str,file:File)->PatchPage {
        let mut end=(query.offset+query.limit).min(text.len());while !text.is_char_boundary(end) {end-=1;}
        PatchPage {source:query.source,epoch:query.expected_epoch,file,offset:query.offset,next_offset:end,
            text:text[query.offset..end].into(),complete:end==query.until}
    }
    #[tokio::test]
    async fn complete_patch_read_preserves_utf8_and_keeps_actual_output_owner_until_drop() {
        let text=Arc::<str>::from(format!("@@ -1,1 +1,1 @@\n-before\n+after\n{}LAST_PATCH_ANCHOR\n"," // αβ context\n".repeat(40_000)));
        let manifest=fixture(&text);let file=manifest.files[0].clone();
        let calls=Arc::new(Mutex::new(Vec::new()));let captured=calls.clone();
        let expected=text.clone();let expected_file=file.clone();
        let loaded=read_patch(manifest.clone(),file,move |query| {
            captured.lock().unwrap().push(query.clone());let page=page(query,&text,expected_file.clone());async move {Ok(page)}
        }).await.unwrap();
        assert_eq!(loaded.text.as_ref(),expected.as_ref());assert!(calls.lock().unwrap().len()>1);
        assert!(calls.lock().unwrap().iter().all(|query|query.source==manifest.source && query.expected_epoch==manifest.epoch
            && query.until==expected.len() && query.file_index==0));
        let weak=Arc::downgrade(&loaded.text);assert!(weak.upgrade().is_some());drop(loaded);
        assert!(weak.upgrade().is_none(),"discarded pending output releases the actual completed patch allocation");
    }
    #[tokio::test]
    async fn malformed_patch_pages_never_return_a_partial_complete_patch() {
        for malformed in 0..6 {
            let text="@@ -1,1 +1,1 @@\n-before\n+after\n";
            let manifest=fixture(text);let file=manifest.files[0].clone();let returned=file.clone();
            let result=read_patch(manifest,file,move |query| {
                let mut value=page(query,text,returned.clone());
                match malformed {
                    0=>value.epoch=epoch(8),
                    1=>value.file.file_index=99,
                    2=>value.file.path="different/path.rs".into(),
                    3=>value.next_offset-=1,
                    4=>value.complete=false,
                    _=>value.source.agent_id=Some("different-child".into()),
                }
                async move {Ok(value)}
            }).await;
            assert!(result.is_err(),"malformed page case {malformed}");
        }
    }
    #[tokio::test]
    async fn manifest_pages_require_exact_offsets_and_explicit_omission() {
        let manifest=fixture("patch");
        for malformed in 0..5 {
            let query=ManifestQuery {source:manifest.source.clone(),offset:0,limit:128,expected_epoch:Some(manifest.epoch)};
            let mut returned=(*manifest).clone();
            match malformed {
                0=>returned.files[0].file_index=1,
                1=>returned.metadata.total_files=2,
                2=>returned.omitted_files=1,
                3=>returned.next_offset=Some(0),
                _=>returned.epoch=epoch(8),
            }
            assert!(read_manifest(query,move |_| {let returned=returned.clone();async move {Ok(returned)}}).await.is_err());
        }
        let mut omitted=(*manifest).clone();omitted.files.clear();omitted.omitted_files=1;
        omitted.metadata.unavailable=Some(chat::cache::changes::Unavailable::MetadataTooLarge {limit:1024});
        let query=ManifestQuery {source:manifest.source.clone(),offset:0,limit:128,expected_epoch:Some(manifest.epoch)};
        let result=read_manifest(query,move |_| {let omitted=omitted.clone();async move {Ok(omitted)}}).await.unwrap();
        assert_eq!(result.omitted_files,1);assert!(result.metadata.unavailable.is_some());
    }
    #[tokio::test]
    async fn explicit_copy_all_retries_failed_current_reads_without_accepting_old_attempts() {
        let text = "@@ -1 +1 @@\n-before\n+after\n";
        for manifest_failed in [true, false] {
            let mut files = (*fixture(text)).clone();
            files.metadata.total_files = 2;
            files.metadata.total_bytes = 2 * text.len();
            files.metadata.additions = Some(2);
            files.metadata.deletions = Some(2);
            let mut second = files.files[0].clone();
            second.file_index = 1;
            second.path = "src/second.rs".into();
            files.files.push(second);
            let page = Arc::new(files);
            let raw = raw(&page, "collapsed-copy-source");
            let expanded = HashSet::new();
            let mut state = State::default();
            state.project(raw, None, &expanded, false);
            let intents = clipboard_intent::Intents::default();
            assert!(state.request_copy_all(&page.source.source_id, intents.next()));
            let first_manifest = state.entries[&page.source].requested[&0];
            let failed_attempt = if manifest_failed {
                assert!(state.complete_manifest(
                    &page.source,
                    0,
                    first_manifest,
                    Err("temporary file-list failure".into())
                ));
                first_manifest
            } else {
                assert!(state.complete_manifest(&page.source, 0, first_manifest, Ok(page.clone())));
                state.reconcile_demand(&expanded, false);
                let first_patch = state.entries[&page.source].patch_requests[&0];
                assert!(state.complete_patch(
                    &page.source,
                    0,
                    first_patch,
                    Err("temporary patch failure".into())
                ));
                first_patch
            };
            state.reconcile_demand(&expanded, false);
            assert!(!state.entries[&page.source].copy_all);
            assert!(
                state.entries[&page.source].opened.is_empty(),
                "multi-file rows remained collapsed"
            );
            assert!(state.entries[&page.source].copy_error.is_some());
            assert!(
                state.take_copy_all().is_none(),
                "a failed copy never supplies partial contents"
            );
            // The visible primary action starts one fresh request, even while
            // the changed-file rows themselves have remained collapsed.
            assert!(state.request_copy_all(&page.source.source_id, intents.next()));
            if manifest_failed {
                let retry = state.entries[&page.source].requested[&0];
                assert_ne!(retry, failed_attempt);
                assert!(!state.complete_manifest(&page.source, 0, failed_attempt, Ok(page.clone())));
                assert!(state.complete_manifest(&page.source, 0, retry, Ok(page.clone())));
                state.reconcile_demand(&expanded, false);
            } else {
                assert!(!state.complete_patch(
                    &page.source,
                    0,
                    failed_attempt,
                    Err("late first-attempt failure".into())
                ));
            }
            for index in 0..2 {
                let retry = state.entries[&page.source].patch_requests[&index];
                if !manifest_failed && index == 0 {
                    assert_ne!(retry, failed_attempt);
                }
                let file = page.files[index].clone();
                let returned = file.clone();
                let pending = read_patch(page.clone(), file.clone(), move |query| {
                    std::future::ready(Ok(self::page(query, text, returned.clone())))
                })
                .await
                .unwrap();
                let record = chat::source_owners::ResidentPatch {
                    source: page.source.clone(),
                    epoch: page.epoch,
                    file,
                };
                assert_eq!(
                    chat::source_owners::admit_patch(&pending.text, &record, None),
                    chat::source_owners::Admission::Ready
                );
                let prepared = PatchReady(Arc::new(Mutex::new(Some(pending))));
                assert!(state.complete_patch(&page.source, index, retry, Ok(prepared)));
            }
            let (item, patches, intent) = state
                .take_copy_all()
                .expect("the explicit retry can finish");
            assert_eq!(item, page.source.source_id);
            assert!(intent.current());
            assert_eq!(
                patches
                    .iter()
                    .map(|patch| patch.as_ref())
                    .collect::<Vec<_>>(),
                vec![text, text]
            );
            assert!(!state.entries[&page.source].copy_all);
            drop(patches);
            state.reconcile_demand(&expanded, false);
            flush_retired();
        }
    }

    #[test]
    fn newer_copy_cancels_loading_all_file_copy_but_preserves_search_demand() {
        for find in [false,true] {
            let page=fixture("original patch");let(mut state,_,_)=open(&page,"visible-part");
            let intents=clipboard_intent::Intents::default();
            assert!(state.request_copy_all(&page.source.source_id,intents.next()));
            intents.next();state.reconcile_demand(&HashSet::new(),find);
            assert!(!state.entries[&page.source].copy_all);
            assert_eq!(state.entries[&page.source].patch_requests.contains_key(&0),find);
            assert!(state.take_copy_all().is_none());
        }
    }

    #[tokio::test]
    async fn actual_patch_copy_result_cannot_replace_a_newer_literal_copy() {
        use iced::futures::StreamExt as _;
        let text="@@ -1 +1 @@\n-before\n+after\n";let page=fixture(text);
        let(mut state,raw,expanded)=open(&page,"late-part");let request=state.entries[&page.source].patch_requests[&0];
        assert!(state.complete_patch(&page.source,0,request,Ok(ready(&page,text).await)));
        let session=fresh_chat_session(Provider::Claude,TerminalId::new(),"/project".into(),&Account::system(Provider::Claude),false).unwrap();
        let handle=Arc::new(ClientHandle::fixture(session.clone(),(*raw).clone()));
        let mut runtime=Runtime::new(Target {card:CardId::new(),generation:1},session,Connection::Live(handle),true,crate::settings::EditorSyntaxTheme::default());
        runtime.logical_changes=state;runtime.expanded=expanded;let target=runtime.target;
        let mut app=crate::app::tests::context_inspector_test_app(false);app.cards.insert(target.card,CardRuntime::Chat(Box::new(runtime)));
        let task=app.handle_chat_ui(Action::CopyChangePatch(target,page.source.source_id.clone(),0));
        let mut stream=iced::runtime::task::into_stream(task).unwrap();
        let completed=loop {
            if let Some(iced::runtime::Action::Output(cosmic::Action::App(message@Message::ChatUi(Action::CompleteChangeCopy(..)))))=stream.next().await {break message;}
        };
        drop(app.handle_chat_ui(Action::CopyLiteral("newer literal code".into())));
        drop(app.handle_chat_ui(match completed {Message::ChatUi(action)=>action,_=>unreachable!()}));
        assert!(app.chat_runtime_mut(target).unwrap().copied.is_none());
        assert!(app.chat_runtime_mut(target).unwrap().logical_changes.entries[&page.source].copy_error.is_none());
    }

    #[tokio::test]
    async fn full_revocation_rejects_successful_patch_read_and_copy_with_unchanged_source_proof() {
        use iced::futures::StreamExt as _;
        let text="@@ -1 +1 @@\n-before\n+exact revoked patch\n";let page=fixture(text);
        let(mut state,raw,expanded)=open(&page,"same-visible-part");let request=state.entries[&page.source].patch_requests[&0];
        assert!(state.complete_patch(&page.source,0,request,Ok(ready(&page,text).await)));
        let owner=Arc::downgrade(&state.entries[&page.source].patches[&0].text);
        let session=fresh_chat_session(Provider::Claude,TerminalId::new(),"/project".into(),&Account::system(Provider::Claude),false).unwrap();
        let handle=Arc::new(ClientHandle::fixture(session.clone(),(*raw).clone()));
        let mut runtime=Runtime::new(Target {card:CardId::new(),generation:1},session,Connection::Live(handle),true,crate::settings::EditorSyntaxTheme::default());
        runtime.logical_changes=state;runtime.expanded=expanded;let target=runtime.target;
        let mut app=crate::app::tests::context_inspector_test_app(false);app.cards.insert(target.card,CardRuntime::Chat(Box::new(runtime)));
        let task=app.handle_chat_ui(Action::CopyChangePatch(target,page.source.source_id.clone(),0));
        let mut stream=iced::runtime::task::into_stream(task).unwrap();
        let copy=loop {if let Some(iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(action@Action::CompleteChangeCopy(..)))))=stream.next().await {break action;}};
        if let Action::CompleteChangeCopy(_,_,_,_,Ok(reply))=&copy {
            assert_eq!(reply.0.value.lock().unwrap().as_ref().unwrap().as_ref().unwrap().text,text);
        } else {panic!("actual successful copy completion required")}
        let pending=ready(&page,text).await;
        let pending_owner=Arc::downgrade(&pending.0.lock().unwrap().as_ref().unwrap().text);
        let runtime=app.chat_runtime_mut(target).unwrap();let snapshot=runtime.snapshot.clone();let actor=runtime.handle().cloned().unwrap();
        let context=runtime.logical_changes.context;runtime.forget_transcript();
        assert!(Arc::ptr_eq(&snapshot,&runtime.snapshot));assert!(Arc::ptr_eq(&actor,runtime.handle().unwrap()));
        assert_ne!(runtime.logical_changes.context,context);flush_retired();assert!(owner.upgrade().is_none());
        drop(app.handle_chat_ui(Action::ChangePatchReady(target,page.source.clone(),0,request,Ok(pending))));
        drop(app.handle_chat_ui(copy));flush_retired();
        assert!(pending_owner.upgrade().is_none());
        let runtime=app.chat_runtime_mut(target).unwrap();assert!(runtime.logical_changes.entries.is_empty());
        assert_eq!(runtime.logical_changes.retained_bytes(),0);
        assert!(runtime.copied.is_none(),"unchanged source epoch and current clipboard intent cannot bypass explicit revocation");
    }

    #[test]
    fn reader_future_and_pending_output_are_send_without_ui_cache_owners() {
        fn assert_send<T:Send>() {}assert_send::<PendingPatch>();
        fn future_send<T:Send>(_:T) {}
        let manifest=fixture("patch");let file=manifest.files[0].clone();
        future_send(read_patch(manifest,file,|_|async {Err(chat::cache::SourceError::Changed)}));
    }
    fn raw(page:&Manifest,visible_id:&str)->Arc<Snapshot> {
        let fragment="@@";
        let item=Arc::new(chat::Item {id:visible_id.into(),role:page.metadata.role,at:page.metadata.at,started_at:page.metadata.started_at,
            content:ItemContent::Changes {status:page.metadata.status,changes:vec![FileChange {path:"bounded-path".into(),kind:ChangeKind::Updated,diff:fragment.into()}]}});
        let part=chat::cache::changes::Part {item_id:visible_id.into(),source:page.source.clone(),epoch:page.epoch,
            metadata:page.metadata.clone(),files:vec![chat::cache::changes::FilePart {display_slot:0,file_index:0,start:0,end:fragment.len(),source_len:page.files[0].source_len}],replacement:false};
        Arc::new(Snapshot {generation:1,changes_revision:1,items:vec![item],changes_parts:vec![part],..Snapshot::default()})
    }
    fn open(page:&Arc<Manifest>,visible:&str)->(State,Arc<Snapshot>,HashSet<String>) {
        let raw=raw(page,visible);let expanded=HashSet::from([page.source.source_id.clone()]);let mut state=State::default();
        state.project(raw.clone(),None,&expanded,false);let request=state.entries[&page.source].requested[&0];
        assert!(state.complete_manifest(&page.source,0,request,Ok(page.clone())));
        state.project(raw.clone(),None,&expanded,false);(state,raw,expanded)
    }
    async fn ready(page:&Arc<Manifest>,text:&str)->PatchReady {
        let file=page.files[0].clone();let returned=file.clone();let text=text.to_owned();
        let pending=read_patch(page.clone(),file,move |query|std::future::ready(Ok(self::page(query,&text,returned.clone())))).await.unwrap();
        let record=chat::source_owners::ResidentPatch {source:page.source.clone(),epoch:page.epoch,file:page.files[0].clone()};
        assert_eq!(chat::source_owners::admit_patch(&pending.text,&record,None),chat::source_owners::Admission::Ready);
        PatchReady(Arc::new(Mutex::new(Some(pending))))
    }
    #[test]
    fn projected_changes_are_metadata_only_and_collapsed_sources_make_no_reads() {
        let page=fixture("original whole patch");let raw=raw(&page,"opaque-other-native-part-suffix");let mut state=State::default();
        let projected=state.project(raw.clone(),None,&HashSet::new(),false);
        assert_eq!(projected.items.len(),1);assert_eq!(projected.items[0].id,page.source.source_id);
        assert!(matches!(&projected.items[0].content,ItemContent::Changes {changes,..} if changes.is_empty()));
        assert!(state.entries[&page.source].requested.is_empty());assert!(state.entries[&page.source].patch_requests.is_empty());
        assert_eq!(state.root("opaque-other-native-part-suffix"),Some(page.source.source_id.as_str()));
        assert_eq!(state.metadata(&page.source.source_id).unwrap().total_bytes,"original whole patch".len());
        assert!(!state.complete_for_find());
    }
    #[tokio::test]
    async fn complete_patch_installs_exact_source_then_collapse_releases_actual_weak_owner() {
        let text="@@ -1 +1 @@\n-before\n+after\nLAST_PATCH_ANCHOR";let page=fixture(text);
        let (mut state,raw,expanded)=open(&page,"only-late-part-visible");let request=state.entries[&page.source].patch_requests[&0];
        let prepared=ready(&page,text).await;let weak=Arc::downgrade(&prepared.0.lock().unwrap().as_ref().unwrap().text);
        assert!(state.complete_patch(&page.source,0,request,Ok(prepared)));
        let loaded=state.copy_patch(&page.source.source_id,0).unwrap();assert_eq!(loaded.as_ref(),text);
        assert!(Arc::ptr_eq(&loaded,&state.entries[&page.source].current(0).unwrap().view.text));
        assert!(state.complete_for_find());assert_eq!(state.find_patches().len(),1);
        let sources=state.find_sources();assert_eq!(sources.len(),1);assert!(sources[0].complete);assert_eq!(sources[0].epoch,page.epoch);
        assert_eq!(sources[0].manifests[0].files[0].path,page.files[0].path);assert_eq!(sources[0].patches[0].0,0);
        assert!(Arc::ptr_eq(&sources[0].patches[0].1,&loaded));drop(sources);drop(loaded);
        state.toggle_file(&page.source.source_id,0,false);state.project(raw,None,&expanded,false);
        assert!(state.copy_patch(&page.source.source_id,0).is_none());flush_retired();assert!(weak.upgrade().is_none());
    }
    #[tokio::test]
    async fn same_session_source_reset_rejects_old_patch_and_unchanged_descriptor() {
        let text="@@ -1 +1 @@\n-before\n+old\n";let page=fixture(text);let(mut state,raw,expanded)=open(&page,"visible-part");
        let request=state.entries[&page.source].patch_requests[&0];let old=ready(&page,text).await;
        let context=state.context;let mut reset=(*raw).clone();reset.source_incarnation=Some(uuid::Uuid::from_u128(999));
        state.project(Arc::new(reset),None,&expanded,false);
        assert_ne!(state.context,context);assert!(state.entries.is_empty());
        assert!(!state.complete_patch(&page.source,0,request,Ok(old)));
        assert_eq!(state.retained_bytes(),0);
    }

    #[tokio::test]
    async fn stale_store_nonce_and_cleared_context_cannot_install_pending_patch_or_retain_it() {
        let text="exact original patch";let page=fixture(text);let (mut state,_,expanded)=open(&page,"part");
        let request=state.entries[&page.source].patch_requests[&0];let pending=ready(&page,text).await;
        let weak=Arc::downgrade(&pending.0.lock().unwrap().as_ref().unwrap().text);
        let mut replacement=(*page).clone();replacement.epoch.incarnation=uuid::Uuid::from_u128(999);
        state.project(raw(&replacement,"part"),None,&expanded,false);
        assert!(!state.complete_patch(&page.source,0,request,Ok(pending)));flush_retired();assert!(weak.upgrade().is_none());
        let new_request=state.entries[&page.source].requested[&0];state.clear();
        assert!(!state.complete_manifest(&page.source,0,new_request,Ok(page)));
        assert!(state.entries.is_empty());assert!(state.capture().patches.is_empty());
    }
    fn flush_retired() {
        struct Done(std::sync::mpsc::Sender<()>);impl Drop for Done {fn drop(&mut self) {let _=self.0.send(());}}
        let(send,recv)=std::sync::mpsc::channel();retire_copy(Done(send));recv.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    #[tokio::test]
    async fn live_hide_keeps_exact_open_file_and_page_intent_without_patch_owners() {
        let text="@@ -1 +1 @@\n-before\n+after exact original\n";let page=fixture(text);
        let(mut state,source,expanded)=open(&page,"visible part");let request=state.entries[&page.source].patch_requests[&0];
        let pending=ready(&page,text).await;let weak=Arc::downgrade(&pending.0.lock().unwrap().as_ref().unwrap().text);
        assert!(state.complete_patch(&page.source,0,request,Ok(pending)));
        state.evict(false);flush_retired();assert!(weak.upgrade().is_none());assert_eq!(state.retained_bytes(),0);
        assert!(state.entries[&page.source].opened.contains(&0));assert!(state.entries[&page.source].manual_pages.contains(&0));
        assert_eq!(state.pending_paths(&page.source.source_id),vec![vec![7,0,0],vec![6,0]]);
        state.project(source.clone(),None,&expanded,false);let request=state.entries[&page.source].requested[&0];
        assert!(state.complete_manifest(&page.source,0,request,Ok(page.clone())));
        assert_eq!(state.pending_paths(&page.source.source_id),vec![vec![6,0]]);
        state.reconcile_demand(&expanded,false);
        let request=state.entries[&page.source].patch_requests[&0];assert!(state.complete_patch(&page.source,0,request,Ok(ready(&page,text).await)));
        assert_eq!(state.copy_patch(&page.source.source_id,0).unwrap().as_ref(),text);assert!(state.pending_paths(&page.source.source_id).is_empty());
        state.evict(false);let mut replacement=(*source).clone();replacement.changes_parts[0].epoch.revision+=1;
        state.project(Arc::new(replacement),None,&expanded,false);
        assert!(state.entries[&page.source].opened.is_empty());assert!(state.pending_paths(&page.source.source_id).is_empty(),"rewrites cannot suspend an old selection");
        state.clear();assert!(state.entries.is_empty(),"Reset/Ready/revoke forgets intent completely");
    }
    #[tokio::test]
    async fn ending_find_after_eviction_does_not_leave_undemanded_patch_pending() {
        let page=fixture("exact original patch");let(mut state,source,expanded)=open(&page,"part");
        let request=state.entries[&page.source].patch_requests[&0];
        assert!(state.complete_patch(&page.source,0,request,Ok(ready(&page,"exact original patch").await)));
        // It was shown by Find, and manually closed while Find kept it exposed.
        state.entries.get_mut(&page.source).unwrap().opened.clear();state.evict(true);
        state.project(source,None,&expanded,true);assert!(state.entries[&page.source].suspended.contains(&0));
        let revision=state.revision();state.reconcile_demand(&expanded,false);
        assert!(state.revision()>revision,"removing pending intent must invalidate the cached Catalog even with no loaded body");
        assert!(!state.entries[&page.source].suspended.contains(&0));
        assert!(!state.suspended_paths(&page.source.source_id).contains(&vec![6,0]));
    }
    #[tokio::test]
    async fn live_single_file_reload_preserves_an_explicitly_collapsed_default() {
        let page=fixture("exact collapsed patch");let(mut state,source,expanded)=open(&page,"part");
        assert!(state.entries[&page.source].opened.contains(&0));
        state.toggle_file(&page.source.source_id,0,false);state.evict(false);
        state.project(source,None,&expanded,false);let request=state.entries[&page.source].requested[&0];
        assert!(state.complete_manifest(&page.source,0,request,Ok(page.clone())));
        state.reconcile_demand(&expanded,false);assert!(!state.entries[&page.source].opened.contains(&0));
        assert!(!state.entries[&page.source].patch_requests.contains_key(&0));
    }
    #[test]
    fn patch_body_transfer_removes_ui_owners_before_a_fast_reclaimer_can_run() {
        let page=fixture("last exact patch body");let text:Arc<str>=Arc::from("last exact patch body");let weak=Arc::downgrade(&text);
        let prepared=tool_text::Shared::prepare(text.clone(),tool_text::Language::Diff);let prefixes=prepared.prefixes();
        let record=chat::source_owners::ResidentPatch {source:page.source.clone(),epoch:page.epoch,file:page.files[0].clone()};
        let mut loaded=LoadedPatch {text,record,view:tool_text::ToolText::from_shared(None,prepared),prefixes};
        let (text,prefixes)=loaded.take_body();assert!(loaded.text.is_empty());assert!(loaded.view.text.is_empty());
        assert_eq!(Arc::strong_count(&text),1,"UI fields no longer race the transferred body guard");
        drop(loaded);retire_copy((text,prefixes));flush_retired();assert!(weak.upgrade().is_none());
    }
    #[test]
    fn unselected_children_never_demand_patch_bodies_and_selection_retires_parent_owners() {
        let page=fixture("original parent patch");let parent=raw(&page,"parent-part");let mut all=(*parent).clone();
        for index in 0..30 {
            let id=format!("child-{index}");let mut child_page=(*page).clone();child_page.source.agent_id=Some(id.clone());
            let child=raw(&child_page,&format!("child-part-{index}"));all.changes_parts.extend(child.changes_parts.clone());
            all.subagents.push(chat::Subagent {id,kind:String::new(),description:String::new(),status:ToolStatus::Succeeded,
                model:None,waiting:false,detail:String::new(),background:true,items:child.items.clone()});
        }
        let all=Arc::new(all);let expanded=HashSet::from([page.source.source_id.clone()]);let mut state=State::default();
        state.project(all.clone(),None,&expanded,true);assert_eq!(state.entries.len(),1);assert!(state.entries.keys().all(|key|key.agent_id.is_none()));
        state.project(all,Some("child-17"),&expanded,true);assert_eq!(state.entries.len(),1);
        assert!(state.entries.keys().all(|key|key.agent_id.as_deref()==Some("child-17")));
    }
    #[test]
    fn manifest_omission_stays_explicit_and_never_claims_complete_find() {
        let page=fixture("original patch");let raw=raw(&page,"part");let mut state=State::default();let expanded=HashSet::from([page.source.source_id.clone()]);
        state.project(raw,None,&expanded,true);let request=state.entries[&page.source].requested[&0];
        let mut omitted=(*page).clone();omitted.files.clear();omitted.omitted_files=1;omitted.metadata.unavailable=Some(chat::cache::changes::Unavailable::IncompleteIndex);
        assert!(state.complete_manifest(&page.source,0,request,Ok(Arc::new(omitted))));
        assert!(!state.complete_for_find());assert!(state.find_patches().is_empty());assert_eq!(state.entries[&page.source].pages[&0].omitted_files,1);
    }
    #[tokio::test]
    async fn offline_capture_keeps_exact_patch_provenance_and_restores_only_admitted_decoded_owners() {
        let text="@@ -1 +1 @@\n-before\n+after\nEND";let page=fixture(text);let (mut state,raw,expanded)=open(&page,"part");
        let request=state.entries[&page.source].patch_requests[&0];assert!(state.complete_patch(&page.source,0,request,Ok(ready(&page,text).await)));
        let captured=state.capture();assert_eq!(captured.manifests.len(),1);assert_eq!(captured.patches.len(),1);
        let projected=state.project(raw.clone(),None,&expanded,false);
        assert!(projected.changes_parts.iter().any(|part|part.item_id==page.source.source_id && part.replacement && part.files.is_empty()));
        let weak=Arc::downgrade(&captured.patches[0].0);let mut restored=State::default();restored.project(projected,None,&expanded,false);
        assert!(restored.restore(captured.clone()).is_ok());assert_eq!(restored.retained_bytes(),text.len());
        let mut forged=captured.clone();forged.patches[0].1.epoch.incarnation=uuid::Uuid::from_u128(888);
        assert!(restored.restore(forged).is_err());
        let mut uncharged=captured.clone();uncharged.patches[0].0=Arc::from(text);
        assert!(restored.restore(uncharged).is_err(),"equal bytes do not prove a decoded lease");
        drop(captured);state.clear();assert!(weak.upgrade().is_some(),"actual restored owner remains charged");
        restored.clear();flush_retired();assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn mounted_detail_tree_retires_copy_and_patch_controls_on_collapse_and_epoch_change() {
        use iced::advanced::{Layout,layout,widget::Tree};
        let text="@@ -1 +1 @@\n-before\n+after\nLAST_PATCH_ANCHOR";let page=fixture(text);
        let (mut state,raw,expanded)=open(&page,"late-native-part");let request=state.entries[&page.source].patch_requests[&0];
        assert!(state.complete_patch(&page.source,0,request,Ok(ready(&page,text).await)));
        let session=fresh_chat_session(Provider::Claude,TerminalId::new(),"/project".into(),&Account::system(Provider::Claude),false).unwrap();
        let handle=Arc::new(ClientHandle::fixture(session.clone(),(*raw).clone()));
        let mut runtime=Runtime::new(Target {card:CardId::new(),generation:1},session,Connection::Live(handle),true,crate::settings::EditorSyntaxTheme::default());
        runtime.logical_changes=state;runtime.expanded=expanded;
        let renderer=crate::app::tests::headless_renderer();
        let labels=|element:&mut Element<'static,Message>,tree:&mut Tree| {
            tree.diff(element.as_widget_mut());
            let node=element.as_widget_mut().layout(tree,&renderer,&layout::Limits::new(iced::Size::ZERO,iced::Size::new(600.0,600.0)));
            let nodes=element.as_widget().a11y_nodes(Layout::new(&node),tree,iced::mouse::Cursor::Unavailable);
            nodes.root().iter().chain(nodes.children()).filter_map(|node|node.node().label().map(str::to_owned)).collect::<Vec<_>>()
        };
        let mut element=detail(&runtime,&page.source.source_id).unwrap();let mut tree=Tree::new(&element);
        assert!(labels(&mut element,&mut tree).iter().any(|label|label=="Copy patch"));
        runtime.logical_changes.toggle_file(&page.source.source_id,0,false);
        let mut element=detail(&runtime,&page.source.source_id).unwrap();
        assert!(!labels(&mut element,&mut tree).iter().any(|label|label=="Copy patch"));
        let mut changed=(*page).clone();changed.epoch.incarnation=uuid::Uuid::from_u128(44);
        runtime.logical_changes.project(self::raw(&changed,"late-native-part"),None,&runtime.expanded,false);
        let mut element=detail(&runtime,&page.source.source_id).unwrap();
        assert!(!labels(&mut element,&mut tree).iter().any(|label|label=="Copy patch"));
        assert!(runtime.logical_changes.copy_patch(&page.source.source_id,0).is_none());
    }

    #[test]
    fn selected_child_reused_root_identity_uses_its_exact_item_for_patch_highlights() {
        let id="same-opaque-root";
        let item=|status|Arc::new(chat::Item {id:id.into(),role:chat::Role::Assistant,at:None,started_at:None,
            content:ItemContent::Changes {status,changes:vec![]}});
        let parent=item(ToolStatus::Failed);let child=item(ToolStatus::Succeeded);
        let session=fresh_chat_session(Provider::Claude,TerminalId::new(),"/project".into(),&Account::system(Provider::Claude),false).unwrap();
        let snapshot=Snapshot {ready:true,items:vec![parent.clone()],subagents:vec![chat::Subagent {
            id:"selected-child".into(),kind:String::new(),description:String::new(),status:ToolStatus::Succeeded,
            model:None,waiting:false,detail:String::new(),background:false,items:vec![child.clone()]}],..Default::default()};
        let handle=Arc::new(ClientHandle::fixture(session.clone(),snapshot));
        let mut runtime=Runtime::new(Target {card:CardId::new(),generation:1},session,Connection::Live(handle),true,crate::settings::EditorSyntaxTheme::default());
        runtime.selected_subagent=Some("selected-child".into());
        let identity=selection::logical_identity(id,&[6,0]);let body:Arc<str>=Arc::from("+CHILD_ONLY_MATCH");
        let external=Arc::new(find::External {item:Arc::from(id),revision:777,blocks:vec![find::Block {identity,text:body}].into()});
        runtime.find_external.insert(id.into(),external.clone());
        let mut paint=find::Paint::new(1,&[find::Match {turn:Arc::from(id),item:Arc::from(id),identity,range:1..17}],None);
        paint.sources=Arc::new(HashMap::from([(id.into(),find::SourceProof {item:Arc::downgrade(&child),growing:false,external:Some(Arc::downgrade(&external)),supplemental:None})]));
        paint.signatures=Arc::new(HashMap::from([(Arc::<str>::from(id),1)]));runtime.find_paint=Some(Arc::new(paint));
        assert!(std::ptr::eq(current_item(&runtime,id).unwrap(),child.as_ref()));
        assert!(runtime.find_paint_for(current_item(&runtime,id).unwrap()).is_some(),"actual detail lookup preserves accepted child paint");
        assert!(runtime.find_paint_for(parent.as_ref()).is_none(),"same spelling is not the child source proof");
        runtime.selected_subagent=Some("vanished-child".into());assert!(current_item(&runtime,id).is_none(),"no fallback to another transcript");
        runtime.selected_subagent=None;assert!(std::ptr::eq(current_item(&runtime,id).unwrap(),parent.as_ref()));
    }

    #[tokio::test]
    async fn closing_find_retires_all_file_body_demand_and_pending_metadata_requests() {
        let text="@@ -1 +1 @@\n-before\n+after\n";let page=fixture(text);let (mut state,raw,expanded)=open(&page,"part");
        state.project(raw,None,&expanded,true);let request=state.entries[&page.source].patch_requests[&0];
        assert!(state.complete_patch(&page.source,0,request,Ok(ready(&page,text).await)));
        let weak=Arc::downgrade(&state.entries[&page.source].current(0).unwrap().text);
        {let entry=state.entries.get_mut(&page.source).unwrap();entry.wanted_pages.insert(128);entry.requested.insert(128,999);}
        state.reconcile_demand(&HashSet::new(),false);
        let entry=&state.entries[&page.source];assert!(entry.patch_requests.is_empty());assert!(entry.requested.is_empty());
        assert!(entry.wanted_pages.is_empty());assert_eq!(state.retained_bytes(),0);flush_retired();assert!(weak.upgrade().is_none());
    }

    #[tokio::test]
    async fn manual_file_toggle_page_and_retry_preserve_other_active_find_sources() {
        let text="@@ -1 +1 @@\n-before\n+after\n";let mut page=(*fixture(text)).clone();
        page.metadata.total_files=2;page.metadata.total_bytes*=2;let mut second=page.files[0].clone();second.file_index=1;second.path="src/second.rs".into();page.files.push(second);
        let page=Arc::new(page);let(mut state,raw,expanded)=open(&page,"part");state.project(raw,None,&expanded,true);
        let first=state.entries[&page.source].patch_requests[&0];let pending=state.entries[&page.source].patch_requests[&1];
        assert!(state.complete_patch(&page.source,0,first,Ok(ready(&page,text).await)));
        let weak=Arc::downgrade(&state.entries[&page.source].current(0).unwrap().text);
        state.toggle_file(&page.source.source_id,0,true);state.toggle_file(&page.source.source_id,0,true);
        state.page(&page.source.source_id,0,true);state.retry(&page.source.source_id,Some(1),true);
        let entry=&state.entries[&page.source];assert!(entry.current(0).is_some());assert!(weak.upgrade().is_some());
        assert_eq!(entry.patch_requests[&1],pending,"unrelated pending file remains scheduled during active Find");
        assert_eq!(state.find_patches().len(),1);assert!(!state.complete_for_find());
    }

    #[tokio::test]
    async fn nested_reader_readiness_invalidates_retained_rows_without_reindexing_semantic_sources() {
        let text="@@ -1 +1 @@\n-before\n+after\n";let page=fixture(text);let(mut state,_raw,_expanded)=open(&page,"part");
        let request=state.entries[&page.source].patch_requests[&0];assert!(state.complete_patch(&page.source,0,request,Ok(ready(&page,text).await)));
        let semantic=state.revision();let before=state.item_revision(&page.source.source_id);let file_revision=state.find_sources()[0].revision;
        assert!(state.render_ready(|view|view.text.as_ref()==text));assert!(state.item_revision(&page.source.source_id)>before);
        assert_eq!(state.revision(),semantic);assert_eq!(state.find_sources()[0].revision,file_revision);
        assert!(!state.render_ready(|_|false));
    }

    #[tokio::test]
    async fn paged_away_changes_release_bodies_but_restore_exact_file_page_and_default_choices() {
        let text="@@ -1 +1 @@\n-before\n+after\n";let page=fixture(text);let(mut state,raw,expanded)=open(&page,"visible-part");
        let request=state.entries[&page.source].patch_requests[&0];assert!(state.complete_patch(&page.source,0,request,Ok(ready(&page,text).await)));
        let owner=Arc::downgrade(&state.entries[&page.source].current(0).unwrap().text);
        state.page(&page.source.source_id,0,false);
        let mut away=(*raw).clone();away.items.clear();away.changes_parts.clear();away.has_earlier=true;
        state.project(Arc::new(away.clone()),None,&expanded,false);flush_retired();
        assert!(expanded.contains(&page.source.source_id));assert!(owner.upgrade().is_none());
        assert!(state.entries.is_empty() && state.aliases.is_empty());assert!(state.suspended_paths(&page.source.source_id).is_empty());
        assert_eq!(state.intents.usage().0,1);
        state.project(raw.clone(),None,&expanded,false);
        let entry=&state.entries[&page.source];assert!(entry.opened.contains(&0) && entry.manual_pages.contains(&0) && entry.default_open_applied);
        assert!(entry.requested.contains_key(&0));
        assert!(state.suspended_paths(&page.source.source_id).contains(&vec![6,0]));
        let request=state.entries[&page.source].requested[&0];assert!(state.complete_manifest(&page.source,0,request,Ok(page.clone())));
        state.project(raw.clone(),None,&expanded,false);let request=state.entries[&page.source].patch_requests[&0];
        assert!(state.complete_patch(&page.source,0,request,Ok(ready(&page,text).await)));assert_eq!(state.copy_patch(&page.source.source_id,0).unwrap().as_ref(),text);
        state.toggle_file(&page.source.source_id,0,false);assert!(!state.entries[&page.source].opened.contains(&0));
        state.project(Arc::new(away.clone()),None,&expanded,false);state.project(raw.clone(),None,&expanded,false);
        let request=state.entries[&page.source].requested[&0];assert!(state.complete_manifest(&page.source,0,request,Ok(page.clone())));
        assert!(!state.entries[&page.source].opened.contains(&0),"explicitly closed single-file default stays closed after paging");
        state.project(Arc::new(away),None,&expanded,false);let mut rewritten=(*raw).clone();rewritten.changes_parts[0].epoch.revision+=1;
        state.project(Arc::new(rewritten),None,&expanded,false);
        assert!(!state.entries[&page.source].opened.contains(&0));assert!(!state.entries[&page.source].default_open_applied);
        state.clear();assert_eq!(state.intents.usage(),(0,0));
    }

    #[test]
    fn later_original_manifest_page_choice_survives_only_exact_epoch_page_return() {
        let mut first=(*fixture("patch")).clone();let file=first.files[0].clone();
        first.metadata.total_files=129;first.metadata.total_bytes=129*file.source_len;
        first.files=(0..128).map(|index|File {file_index:index,path:format!("file-{index}.rs"),..file.clone()}).collect();first.next_offset=Some(128);
        let first=Arc::new(first);let(mut state,raw,expanded)=open(&first,"page-part");
        state.page(&first.source.source_id,128,false);assert!(state.entries[&first.source].manual_pages.contains(&128));
        let mut away=(*raw).clone();away.items.clear();away.changes_parts.clear();state.project(Arc::new(away),None,&expanded,false);
        assert!(state.entries.is_empty() && state.suspended_paths(&first.source.source_id).is_empty());
        state.project(raw,None,&expanded,false);let entry=&state.entries[&first.source];
        assert!(entry.manual_pages.contains(&128) && entry.requested.contains_key(&128));
    }

    #[test]
    fn actual_changes_actions_and_same_tree_clear_identical_ready_selection_on_new_epoch() {
        const CHILD:&str="CAGENTS_TEST_CHANGES_SELECTION_EPOCH";
        if std::env::var_os(CHILD).is_none() {
            let output=std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact","app::chat_ui::changes_sources::tests::actual_changes_actions_and_same_tree_clear_identical_ready_selection_on_new_epoch","--nocapture","--test-threads=1"]).env(CHILD,"1").output().unwrap();
            assert!(output.status.success(),"{}\n{}",String::from_utf8_lossy(&output.stdout),String::from_utf8_lossy(&output.stderr));assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed"));return;
        }
        let renderer=crate::app::tests::headless_renderer();
        tokio::runtime::Runtime::new().unwrap().block_on(async {
            use iced::{advanced::{layout,widget::Tree},futures::StreamExt as _};
            let text="@@ -1 +1 @@\n-before\n+identical after\n";let page=fixture(text);
            let(mut state,raw,expanded)=open(&page,"visible-part");let request=state.entries[&page.source].patch_requests[&0];
            let session=fresh_chat_session(Provider::Claude,TerminalId::new(),"/project".into(),&Account::system(Provider::Claude),false).unwrap();
            let handle=Arc::new(ClientHandle::fixture(session.clone(),(*raw).clone()));
            let mut runtime=Runtime::new(Target {card:CardId::new(),generation:1},session,Connection::Live(handle.clone()),true,crate::settings::EditorSyntaxTheme::default());
            runtime.logical_changes=state;runtime.expanded=expanded;runtime.tool_groups_expanded=true;runtime.refresh();let target=runtime.target;
            let mut app=crate::app::tests::context_inspector_test_app(false);app.cards.insert(target.card,CardRuntime::Chat(Box::new(runtime)));
            drop(app.handle_chat_ui(Action::ChangePatchReady(target,page.source.clone(),0,request,Ok(ready(&page,text).await))));
            let mut worker=changes_find::fixture_stream(app.chat_runtime_mut(target).unwrap()).expect("actual production metadata worker");
            let Message::ChatUi(action)=worker.next().await.unwrap() else {panic!("Changes index completion");};drop(app.handle_chat_ui(action));drop(worker);
            let runtime=app.chat_runtime_mut(target).unwrap();let (catalog,bookmark)=runtime.selection_document();let (anchor,end)=catalog.endpoints().unwrap();
            *bookmark.borrow_mut()=selection::Bookmark {anchor:Some(anchor),end:Some(end),anchor_prefix:catalog.fingerprint(anchor),end_prefix:catalog.fingerprint(end),
                anchor_source:catalog.source_stamp_at(anchor),end_source:catalog.source_stamp_at(end)};
            let old_stamp=catalog.source_stamp_at(end);let old_copy=catalog.copy(anchor,end).unwrap();assert!(old_stamp.is_some());drop(catalog);
            let frame=|runtime:&Runtime,tree:&mut Option<Tree>| {
                let (catalog,bookmark)=runtime.selection_document();let mut element=selection::transcript_virtualized(detail(runtime,&page.source.source_id).unwrap(),target,None,catalog,bookmark);
                let tree=tree.get_or_insert_with(||Tree::new(&element));tree.diff(element.as_widget_mut());
                let _=element.as_widget_mut().layout(tree,&renderer,&layout::Limits::new(iced::Size::ZERO,iced::Size::new(640.0,1000.0)));
            };
            let mut tree=None;frame(runtime,&mut tree);assert!(bookmark.borrow().anchor.is_some());
            // Publish, install manifest, patch and real metadata-worker result
            // before laying out this same Tree again: there is no loading gap.
            let mut changed=(*page).clone();changed.epoch.revision+=1;let changed=Arc::new(changed);
            let mut changed_raw=(*raw).clone();changed_raw.generation+=1;changed_raw.changes_revision+=1;changed_raw.changes_parts[0].epoch=changed.epoch;
            handle.publish_snapshot_for_test(changed_raw);runtime.refresh();
            let request=runtime.logical_changes.entries[&page.source].requested[&0];
            drop(app.handle_chat_ui(Action::ChangesManifestReady(target,page.source.clone(),0,request,Ok(changed.clone()))));
            let request=app.chat_runtime_mut(target).unwrap().logical_changes.entries[&page.source].patch_requests[&0];
            drop(app.handle_chat_ui(Action::ChangePatchReady(target,page.source.clone(),0,request,Ok(ready(&changed,text).await))));
            let mut worker=changes_find::fixture_stream(app.chat_runtime_mut(target).unwrap()).expect("replacement actual production worker");
            let Message::ChatUi(action)=worker.next().await.unwrap() else {panic!("replacement Changes index completion");};drop(app.handle_chat_ui(action));drop(worker);
            let runtime=app.chat_runtime_mut(target).unwrap();let (current,_)=runtime.selection_document();let (current_anchor,current_end)=current.endpoints().unwrap();
            assert_eq!(current.copy(current_anchor,current_end).unwrap(),old_copy);assert_ne!(current.source_stamp_at(current_end),old_stamp);drop(current);
            frame(runtime,&mut tree);assert!(bookmark.borrow().anchor.is_none() && bookmark.borrow().end.is_none(),"same Tree clears old-source selection immediately, before input");
        });
    }

}
