//! Reconstruct logical prose before parsing CommonMark. Snapshot parts are a
//! transport window, not independent Markdown documents.
use super::*;
use std::collections::BTreeMap;
use std::ops::Range;

#[cfg(test)]
const PART_BYTES: usize = 128 * 1024;
const SOURCE_BYTES: usize = 32 * 1024 * 1024;
const RESIDENT_BYTES: usize = 72 * 1024 * 1024;

pub(super) use chat::cache::SourceKey as Key;

#[derive(Clone)]
struct Part {
    item: Arc<chat::Item>,
    range: Range<usize>,
    source_len:usize,
    replacement:bool,
    metadata:chat::cache::SourceMetadata,
    epoch:u64,
}


#[derive(Clone)]
pub(super) struct Loaded {
    item:Arc<chat::Item>,
    epoch:u64,
}

struct Entry {
    parts: Vec<Part>,
    request_version:u64,
    version:u64,
    attempt: u64,
    loaded: Option<Loaded>,
    error: Option<String>,
    complete: bool,
    append: Option<(std::sync::Weak<chat::Item>,std::sync::Weak<chat::Item>)>,
}

/// Full text owners are active-surface data. `clear` is part of residency
/// eviction and must run even for disconnected or archived surfaces.
#[derive(Default)]
pub(super) struct State {
    entries: BTreeMap<Key,Entry>,
    raw: Option<Arc<Snapshot>>,
    projected: Option<Arc<Snapshot>>,
    selected:Option<String>,
    aliases:BTreeMap<(Option<String>,String),Key>,
}
static ATTEMPT: AtomicU64=AtomicU64::new(1);
fn next_attempt()->u64 {ATTEMPT.fetch_add(1,Ordering::Relaxed)}

fn same_parts(a:&[Part],b:&[Part])->bool {
    a.len()==b.len() && a.iter().zip(b).all(|(a,b)|a.item.id==b.item.id
        && a.range==b.range && a.source_len==b.source_len && a.replacement==b.replacement
        && a.metadata==b.metadata && a.epoch==b.epoch)
}

impl State {
    pub(super) fn clear(&mut self) {
        self.entries.clear();self.raw=None;self.projected=None;self.aliases.clear();
    }
    pub(super) fn retained_bytes(&self)->usize {
        self.entries.values().filter_map(|entry|entry.loaded.as_ref())
            .filter_map(|loaded|if let ItemContent::Text {text,..}=&loaded.item.content {Some(text.len())} else {None}).sum()
    }
    pub(super) fn alias_count(&self)->usize {self.entries.values().map(|entry|entry.parts.len()).sum()}
    /// One weak lineage edge per source proves the completed-root transition
    /// without rescanning its prefix. Actor/selection eviction drops the edge.
    pub(super) fn append_proven(&self,previous:&Arc<chat::Item>,current:&Arc<chat::Item>)->bool {
        self.entries.values().any(|entry|entry.append.as_ref().is_some_and(|(old,new)|
            old.as_ptr()==Arc::as_ptr(previous) && new.as_ptr()==Arc::as_ptr(current)))
    }
    pub(super) fn aliases(&self,agent_id:Option<&str>,root:&str)->Vec<(String,Range<usize>)> {
        self.entries.get(&Key {agent_id:agent_id.map(str::to_owned),source_id:root.into()})
            .map(|entry|entry.parts.iter().map(|part|(part.item.id.clone(),part.range.clone())).collect())
            .unwrap_or_default()
    }
    pub(super) fn scroll_aliases<'a>(&'a self,agent_id:Option<&'a str>)
        ->impl Iterator<Item=(iced::advanced::widget::Id,&'a str)>+'a {
        self.entries.iter().filter(move |(key,_)|key.agent_id.as_deref()==agent_id)
            .flat_map(|(key,entry)|entry.parts.iter().filter(move |part|part.item.id!=key.source_id)
                .map(move |part|(scroll::item_id(&key.source_id),part.item.id.as_str())))
    }
    fn sync(&mut self,raw:&Arc<Snapshot>,selected:Option<&str>) {
        if self.raw.as_ref().is_some_and(|previous|previous.session_id!=raw.session_id) {self.clear();}
        let mut groups:BTreeMap<Key,Vec<Part>>=BTreeMap::new();
        let active=match selected {
            Some(id)=>raw.subagents.iter().find(|agent|agent.id==id).map(|agent|(Some(agent.id.clone()),&agent.items)),
            None=>Some((None,&raw.items)),
        };
        let source_parts:HashMap<_,_>=raw.source_parts.iter()
            .filter(|part|part.source.agent_id.as_deref()==selected)
            .map(|part|(part.item_id.as_str(),part)).collect();
        for (_agent_id,items) in active {
            for item in items.iter() {
                let ItemContent::Text {text,..}=&item.content else {continue;};
                let Some(part)=source_parts.get(item.id.as_str()) else {continue;};
                if !part.replacement && part.end.checked_sub(part.start)!=Some(text.len()) {continue;}
                groups.entry(part.source.clone()).or_default().push(Part {item:item.clone(),range:part.start..part.end,
                    source_len:part.source_len,replacement:part.replacement,metadata:part.metadata.clone(),epoch:part.epoch});
            }
        }
        groups.retain(|_,parts|parts.len()>1 || parts.iter().any(|part|part.replacement || part.range.start>0
            || part.source_len>part.range.end));
        self.entries.retain(|key,_|groups.contains_key(key));
        for (key,parts) in groups {
            match self.entries.get_mut(&key) {
                Some(entry)=>{
                    let changed=!same_parts(&entry.parts,&parts);
                    entry.parts=parts;
                    if changed {entry.version=entry.version.wrapping_add(1);}
                    if entry.complete && changed {
                        entry.attempt=next_attempt();entry.request_version=entry.version;
                        entry.complete=false;entry.error=None;
                    }
                    // An in-flight read keeps its attempt while new frames
                    // arrive; completion schedules a cheap epoch probe if the
                    // source generation advanced in the meantime.
                },
                None=>{self.entries.insert(key,Entry {parts,request_version:1,version:1,
                    attempt:next_attempt(),loaded:None,error:None,complete:false,append:None});},
            }
        }
        self.aliases=self.entries.iter().flat_map(|(key,entry)|entry.parts.iter()
            .map(move |part|((key.agent_id.clone(),part.item.id.clone()),key.clone()))).collect();
    }
    fn requests(&self)->Vec<(Key,u64,Arc<chat::Item>,Option<Loaded>)> {
        self.entries.iter().filter_map(|(key,entry)| {
            if entry.complete {return None;}
            Some((key.clone(),entry.attempt,entry.parts.iter().max_by_key(|part|part.item.at)?.item.clone(),entry.loaded.clone()))
        }).collect()
    }
    pub(super) fn project(&mut self,raw:Arc<Snapshot>,selected:Option<&str>)->Arc<Snapshot> {
        if self.selected.as_deref()==selected && self.raw.as_ref().is_some_and(|previous|Arc::ptr_eq(previous,&raw))
            && let Some(projected)=&self.projected {return projected.clone();}
        self.sync(&raw,selected);
        self.selected=selected.map(str::to_owned);
        let mut snapshot=(*raw).clone();
        if selected.is_none() {snapshot.items=self.project_items(None,&raw.items);}
        else if let Some(agent)=snapshot.subagents.iter_mut().find(|agent|Some(agent.id.as_str())==selected) {
            agent.items=self.project_items(Some(&agent.id),&agent.items);
        }
        let projected=if self.entries.is_empty() {raw.clone()} else {Arc::new(snapshot)};
        self.raw=Some(raw);self.projected=Some(projected.clone());projected
    }
    fn project_items(&self,agent_id:Option<&str>,items:&[Arc<chat::Item>])->Vec<Arc<chat::Item>> {
        let mut emitted=HashSet::new();
        items.iter().filter_map(|item| {
            let Some(key)=self.aliases.get(&(agent_id.map(str::to_owned),item.id.clone())) else {return Some(item.clone());};
            let Some(entry)=self.entries.get(key) else {return Some(item.clone());};
            let root=key.source_id.as_str();
            if !emitted.insert(root.to_owned()) {return None;}
            if let Some(loaded)=&entry.loaded {return Some(loaded.item.clone());}
            let mut loading=(**item).clone();loading.id=root.into();
            if let ItemContent::Text {text,thinking,..}=&mut loading.content {
                *text=entry.error.clone().unwrap_or_else(||"Loading message…".into());*thinking=false;
            }
            Some(Arc::new(loading))
        }).collect()
    }
    fn complete(&mut self,key:&Key,attempt:u64,loaded:Result<Loaded,String>)->bool {
        let Some(entry)=self.entries.get_mut(key).filter(|entry|entry.attempt==attempt) else {return false;};
        if entry.complete {return false;}
        entry.complete=true;
        entry.append=None;
        match loaded {
            Ok(loaded)=>{
                if let Some(previous)=&entry.loaded
                    && previous.epoch==loaded.epoch
                    && previous.item.id==key.source_id && loaded.item.id==key.source_id
                    && previous.item.role==loaded.item.role
                    && !Arc::ptr_eq(&previous.item,&loaded.item)
                    && let (ItemContent::Text {text:old,thinking:a,phase:x},ItemContent::Text {text:new,thinking:b,phase:y})=
                        (&previous.item.content,&loaded.item.content)
                    && a==b && x==y && old.len()<=new.len() {
                    entry.append=Some((Arc::downgrade(&previous.item),Arc::downgrade(&loaded.item)));
                }
                entry.loaded=Some(loaded);entry.error=None;
            },
            Err(error)=>entry.error=Some(error),
        }
        if entry.version!=entry.request_version {
            entry.complete=false;entry.request_version=entry.version;entry.attempt=next_attempt();
        }
        self.projected=None;true
    }
}

/// A lease follows the actual immutable item through snapshots, parser caches
/// and retained trees: weak owners disappear only when the last item owner is
/// gone. Clearing a State cannot release a still-retained item's accounting.
#[derive(Default)]
struct Owners {
    entries:Vec<(std::sync::Weak<chat::Item>,usize)>,
    /// One global replacement can temporarily overlap the old Item owners.
    turnover:Option<(std::sync::Weak<chat::Item>,std::sync::Weak<chat::Item>)>,
}
fn owners()->&'static Mutex<Owners> {
    static OWNERS:std::sync::OnceLock<Mutex<Owners>>=std::sync::OnceLock::new();
    OWNERS.get_or_init(Mutex::default)
}
impl Owners {
    fn prune(&mut self) {
        self.entries.retain(|(owner,_)|owner.strong_count()>0);
        if self.turnover.as_ref().is_some_and(|(old,new)|old.strong_count()==0 || new.strong_count()==0) {self.turnover=None;}
    }
    fn bytes(&self)->usize {self.entries.iter().map(|(_,bytes)|*bytes).sum()}
    fn admit(&mut self,item:&Arc<chat::Item>,bytes:usize,replaced:Option<&Arc<chat::Item>>)->Admission {
        self.prune();
        if bytes>SOURCE_BYTES {return Admission::Capacity;}
        let total=self.bytes();
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
enum Admission {Ready,Turnover,Capacity}
pub(super) fn live_source_bytes()->usize {
    let mut owners=owners().lock().unwrap_or_else(|error|error.into_inner());owners.prune();owners.bytes()
}
static READER_CAPACITY: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
/// Allocated String capacity in the two readers, including a completed body
/// awaiting admission. Shared `known` items remain counted only by Owners.
#[derive(Default)]
struct ReaderCapacity(std::sync::atomic::AtomicUsize);
impl ReaderCapacity {
    fn set(&self, bytes: usize) {
        let old = self.0.swap(bytes, Ordering::Relaxed);
        if bytes >= old { READER_CAPACITY.fetch_add(bytes-old, Ordering::Relaxed); }
        else { READER_CAPACITY.fetch_sub(old-bytes, Ordering::Relaxed); }
    }
}
impl Drop for ReaderCapacity {
    fn drop(&mut self) { self.set(0); }
}
pub(super) fn reader_source_capacity()->usize { READER_CAPACITY.load(Ordering::Relaxed) }
fn admit(item:&Arc<chat::Item>,bytes:usize,replaced:Option<&Arc<chat::Item>>)->Admission {
    owners().lock().unwrap_or_else(|error|error.into_inner()).admit(item,bytes,replaced)
}

#[derive(Clone)]
struct Source {
    target:Target,
    key:Key,
    attempt:u64,
    handle:ClientHandle,
    known:Option<Loaded>,
}
impl PartialEq for Source {
    fn eq(&self,other:&Self)->bool {self.target==other.target && self.key==other.key && self.attempt==other.attempt}
}
impl Eq for Source {}
impl std::hash::Hash for Source {
    fn hash<H:std::hash::Hasher>(&self,state:&mut H) {
        use std::hash::Hash;
        std::any::TypeId::of::<Self>().hash(state);self.target.hash(state);self.key.hash(state);self.attempt.hash(state);
    }
}
fn stream(source:&Source)->Pin<Box<dyn iced::futures::Stream<Item=Message>+Send>> {
    let source=source.clone();
    Box::pin(iced::futures::stream::once(async move {
        let loaded=load(&source).await;
        action(Action::LogicalSourceLoaded(source.target,source.key,source.attempt,loaded))
    }))
}
async fn load(source:&Source)->Result<Loaded,String> {
    static WORKERS:std::sync::OnceLock<tokio::sync::Semaphore>=std::sync::OnceLock::new();
    let _permit=WORKERS.get_or_init(||tokio::sync::Semaphore::new(2)).acquire().await
        .map_err(|_|"Message loading stopped".to_owned())?;
    let reader_capacity=ReaderCapacity::default();
    let loaded=read_counted(&source.key,source.known.as_ref(),|query| {
        let handle=source.handle.clone();async move {handle.source_page(query).await}
    }, &reader_capacity).await?;
    if source.known.as_ref().is_some_and(|known|Arc::ptr_eq(&known.item,&loaded.item)) {return Ok(loaded);}
    let item=&loaded.item;
    let bytes=match &item.content {ItemContent::Text {text,..}=>text.len(),_=>return Err("The source is not prose.".into())};
    // A previous load may have published its replacement while the UI still
    // owns its old item for one frame. Wait for that single global turnover;
    // never ask the reader to close a chat merely to replace its own source.
    let deadline=Instant::now()+Duration::from_secs(5);
    loop {
        match admit(item,bytes,source.known.as_ref().map(|known|&known.item)) {
            Admission::Ready=>break,
            Admission::Turnover if Instant::now()<deadline=>tokio::time::sleep(Duration::from_millis(16)).await,
            Admission::Turnover=>return Err("Message loading is busy. Try again.".into()),
            Admission::Capacity=>return Err("Large message capacity is full. Close another large chat and retry.".into()),
        }
    }
    Ok(loaded)
}

#[cfg(test)]
async fn read<F,Fut>(key:&Key,known:Option<&Loaded>,mut fetch:F)->Result<Loaded,String>
where F:FnMut(chat::cache::SourceQuery)->Fut,
      Fut:std::future::Future<Output=Result<chat::cache::SourcePage,chat::cache::SourceError>> {
    read_counted(key,known,&mut fetch,&ReaderCapacity::default()).await
}

async fn read_counted<F,Fut>(key:&Key,known:Option<&Loaded>,mut fetch:F,capacity:&ReaderCapacity)->Result<Loaded,String>
where F:FnMut(chat::cache::SourceQuery)->Fut,
      Fut:std::future::Future<Output=Result<chat::cache::SourcePage,chat::cache::SourceError>> {
    let mut text=String::new();let mut epoch=None;let mut until=None;let mut offset=0;let mut metadata=None;
    loop {
        let page=fetch(chat::cache::SourceQuery {
            source:key.clone(),offset,limit:if offset==0 {4} else {256*1024},expected_epoch:epoch,until,
        }).await.map_err(|error|match error {
            chat::cache::SourceError::TooLarge {..}=>"This message exceeds the 32 MiB active message limit.".to_owned(),
            chat::cache::SourceError::Changed=>"The message changed while loading. Retry to load its current contents.".to_owned(),
            _=>"The complete message could not be loaded. Retry when the session is connected.".to_owned(),
        })?;
        let boundary=*until.get_or_insert(page.source_len);
        if page.source!=*key || page.offset!=offset || page.source_len!=boundary
            || epoch.is_some_and(|epoch|epoch!=page.epoch) || page.next_offset>boundary
            || page.next_offset.checked_sub(offset)!=Some(page.text.len())
            || page.complete!=(page.next_offset==boundary) || (!page.complete && page.next_offset==offset) {
            return Err("The message source changed while loading. Retry to load its current contents.".into());
        }
        if boundary>SOURCE_BYTES {return Err("This message exceeds the 32 MiB active message limit.".into());}
        if offset==0 {
            metadata=Some(page.metadata.clone());
            if let Some(known)=known && known.epoch==page.epoch
                && let ItemContent::Text {text:previous,..}=&known.item.content
                && previous.len()<=boundary {
                if previous.len()==boundary && chat::cache::SourceMetadata::from_item(&known.item).as_ref()==Some(&page.metadata) {
                    return Ok(known.clone());
                }
                if !previous.is_empty() {
                    // The epoch proves a rewrite did not occur. Transfer only
                    // the newly appended suffix, preserving complete parser
                    // context without repeatedly reading the old prefix.
                    text=previous.clone();text.reserve_exact(boundary-previous.len());
                    capacity.set(text.capacity());
                    epoch=Some(page.epoch);offset=previous.len();
                    if offset==boundary {break;}
                    continue;
                }
            }
            text.reserve_exact(boundary);
            capacity.set(text.capacity());
        }
        epoch=Some(page.epoch);offset=page.next_offset;text.push_str(&page.text);
        if page.complete {break;}
    }
    let metadata=metadata.ok_or_else(||"Message source metadata is missing.".to_owned())?;
    let item=Arc::new(chat::Item {id:key.source_id.clone(),role:metadata.role,
        started_at:metadata.started_at,at:metadata.at,
        content:ItemContent::Text {text,thinking:metadata.thinking,phase:metadata.phase}});
    Ok(Loaded {item,epoch:epoch.unwrap_or_default()})
}

pub(super) fn subscriptions(runtime:&Runtime)->Vec<Subscription<Message>> {
    if !runtime.visible.load(Ordering::Relaxed) {return Vec::new();}
    let Some(handle)=runtime.handle() else {return Vec::new();};
    runtime.logical_sources.requests().into_iter().map(|(key,attempt,_template,known)| {
        Subscription::run_with(Source {target:runtime.target,key,attempt,handle:handle.clone(),known},stream)
    }).collect()
}

pub(super) fn complete(runtime:&mut Runtime,key:&Key,attempt:u64,loaded:Result<Loaded,String>) {
    if runtime.visible.load(Ordering::Relaxed) && runtime.logical_sources.complete(key,attempt,loaded) {
        runtime.refresh();
    }
}

/// Release the projected body as well as the lookup state. The client/raw
/// snapshot remains bounded by the daemon window; no hidden cache keeps the
/// reconstructed full Item alive merely because this card still exists.
pub(super) fn release(runtime:&mut Runtime) {
    let roots:HashSet<_>=runtime.logical_sources.entries.keys().map(|key|key.source_id.clone()).collect();
    if let Some(raw)=runtime.logical_sources.raw.take() && !roots.is_empty() {
        runtime.snapshot=raw;
        runtime.cache.retain(|id,_|!roots.contains(id));
        runtime.turns.clear();
    }
    runtime.logical_sources.clear();
}

pub(super) fn retry(runtime:&mut Runtime) {
    for entry in runtime.logical_sources.entries.values_mut().filter(|entry|entry.error.is_some()) {
        entry.complete=false;entry.error=None;entry.attempt=next_attempt();
        entry.request_version=entry.version;
    }
    runtime.logical_sources.projected=None;runtime.refresh();
}

pub(super) fn notice(runtime:&Runtime)->Option<Element<'static,Message>> {
    let errors=runtime.logical_sources.entries.values().filter_map(|entry|entry.error.as_ref()).collect::<Vec<_>>();
    if errors.is_empty() {return None;}
    Some(widget::row::with_children(vec![design::caption(errors[0].clone()).width(Length::Fill).into(),
        widget::button::text("Retry loading").on_press(action(Action::RetryLogicalSources(runtime.target))).into()])
        .spacing(GAP_S).width(Length::Fill).into())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn text(id:&str,body:&str)->Arc<chat::Item> {
        Arc::new(chat::Item {id:id.into(),role:chat::Role::Assistant,started_at:Some(10),at:Some(20),
            content:ItemContent::Text {text:body.into(),thinking:false,phase:chat::Phase::Final}})
    }
    fn body(item:&chat::Item)->&str {
        let ItemContent::Text {text,..}=&item.content else {panic!("not text")};text
    }
    fn source_snapshot(mut snapshot:Snapshot)->Arc<Snapshot> {
        if snapshot.source_parts.is_empty() {
            let epoch=snapshot.source_revision;
            snapshot.source_parts=std::iter::once((None,&snapshot.items))
                .chain(snapshot.subagents.iter().map(|agent|(Some(agent.id.clone()),&agent.items)))
                .flat_map(|(agent_id,items)|items.iter().filter_map(move |item| {
                    let ItemContent::Text {text,..}=&item.content else {return None;};
                    Some(chat::cache::SourcePart {item_id:item.id.clone(),source:Key {agent_id:agent_id.clone(),source_id:item.id.clone()},
                        start:0,end:text.len(),source_len:text.len()+1,replacement:false,metadata:chat::cache::SourceMetadata::from_item(item).unwrap(),epoch})
                })).collect();
        }
        Arc::new(snapshot)
    }
    #[test]
    fn transport_seams_project_one_complete_commonmark_document() {
        // The window begins inside a code fence; neither a synthetic fence nor
        // joining this suffix to another loaded suffix can recover the prefix.
        let full=format!("# Heading\n\n[ref]: https://example.com\n\n```rust\n{}\n```\n\n[linked][ref]\n\n| a | b |\n| - | - |\n| one | two |\n", "let crab = \"🦀\";\n".repeat(9_000));
        let mut cut=PART_BYTES;while !full.is_char_boundary(cut) {cut-=1;}
        let suffix=text(&format!("root:native-part:1:text:{cut}-{}:0-0",full.len()),&full[cut..]);
        let raw=source_snapshot(Snapshot {generation:3,items:vec![suffix.clone()],source_parts:vec![chat::cache::SourcePart {
            item_id:suffix.id.clone(),source:Key {agent_id:None,source_id:"root".into()},start:cut,end:full.len(),
            source_len:full.len(),replacement:false,metadata:chat::cache::SourceMetadata::from_item(&suffix).unwrap(),epoch:7}],..Snapshot::default()});
        let mut state=State::default();let loading=state.project(raw.clone(),None);
        assert_eq!(loading.items.len(),1);assert_eq!(loading.items[0].id,"root");
        assert_eq!(body(&loading.items[0]),"Loading message…");
        let key=Key {agent_id:None,source_id:"root".into()};let attempt=state.entries[&key].attempt;
        assert!(state.complete(&key,attempt,Ok(Loaded {item:text("root",&full),epoch:7})));
        let ready=state.project(raw.clone(),None);
        assert_eq!(ready.items.len(),1);assert_eq!(body(&ready.items[0]),full);
        assert!(!Arc::ptr_eq(&loading,&ready),"same-generation hydration must invalidate presentation");
        assert!(Arc::ptr_eq(&ready,&state.project(raw,None)));
        assert_eq!(state.aliases(None,"root"),vec![(suffix.id.clone(),cut..full.len())]);
        let mut expected=CachedItem::updated(&text("root",&full).content,crate::settings::EditorSyntaxTheme::default(),None,false,false,None);
        let mut actual=CachedItem::updated(&ready.items[0].content,crate::settings::EditorSyntaxTheme::default(),None,false,false,None);
        expected.settle(&text("root",&full).content,crate::settings::EditorSyntaxTheme::default());
        actual.settle(&ready.items[0].content,crate::settings::EditorSyntaxTheme::default());
        assert_eq!(format!("{:?}",actual.markdown.items()),format!("{:?}",expected.markdown.items()));
    }
    #[test]
    fn stale_async_completions_do_not_restore_evicted_sources() {
        let mut state=State::default();
        state.project(source_snapshot(Snapshot {generation:1,items:vec![text("root",&"x".repeat(PART_BYTES))],..Snapshot::default()}),None);
        let key=Key {agent_id:None,source_id:"root".into()};let attempt=state.entries[&key].attempt;
        state.clear();
        assert!(!state.complete(&key,attempt,Ok(Loaded {item:text("root","old"),epoch:1})));
        assert_eq!(state.retained_bytes(),0);assert_eq!(state.alias_count(),0);
    }

    #[test]
    fn completed_root_append_proof_is_weak_scoped_and_epoch_fenced() {
        let raw=|generation,epoch,body:&str|source_snapshot(Snapshot {
            generation,source_revision:epoch,items:vec![text("root",body)],..Snapshot::default()});
        let mut state=State::default();state.project(raw(1,7,"pre"),None);
        let key=Key {agent_id:None,source_id:"root".into()};
        let first_attempt=state.entries[&key].attempt;
        let previous=text("root","prefix");let weak=Arc::downgrade(&previous);
        assert!(state.complete(&key,first_attempt,Ok(Loaded {item:previous.clone(),epoch:7})));
        state.project(raw(2,7,"prefix appended"),None);
        let next_attempt=state.entries[&key].attempt;let current=text("root","prefix appended suffix");
        assert!(state.complete(&key,next_attempt,Ok(Loaded {item:current.clone(),epoch:7})));
        assert!(state.append_proven(&previous,&current));
        assert!(!state.append_proven(&text("root","prefix"),&current),"equal text is not immutable identity proof");
        assert!(!state.complete(&key,first_attempt,Ok(Loaded {item:text("root","stale"),epoch:7})));
        assert!(state.append_proven(&previous,&current),"a stale completion cannot revoke current proof");
        drop(previous);assert!(weak.upgrade().is_none(),"proof cannot keep old bodies resident");
        state.project(raw(3,8,"rewritten root"),None);
        let rewritten=text("root","rewritten root is longer");let attempt=state.entries[&key].attempt;
        assert!(state.complete(&key,attempt,Ok(Loaded {item:rewritten.clone(),epoch:8})));
        assert!(!state.append_proven(&current,&rewritten));
        state.project(raw(4,8,"rewritten root is longer"),None);
        let mut changed=(*rewritten).clone();
        let ItemContent::Text {thinking,..}=&mut changed.content else {unreachable!()};*thinking=true;
        let changed=Arc::new(changed);let attempt=state.entries[&key].attempt;
        assert!(state.complete(&key,attempt,Ok(Loaded {item:changed.clone(),epoch:8})));
        assert!(!state.append_proven(&rewritten,&changed),"rendering kind changes require a parse reset");
        state.clear();assert!(!state.append_proven(&current,&rewritten));
    }
    #[test]
    fn per_source_rewrites_revalidate_without_cancelling_inflight_reads() {
        let item=text("root",&"x".repeat(PART_BYTES));let mut state=State::default();
        state.project(source_snapshot(Snapshot {generation:1,source_revision:1,items:vec![item.clone()],..Snapshot::default()}),None);
        let key=Key {agent_id:None,source_id:"root".into()};let first=state.entries[&key].attempt;
        state.project(source_snapshot(Snapshot {generation:2,source_revision:2,items:vec![item.clone()],..Snapshot::default()}),None);
        assert_eq!(state.entries[&key].attempt,first);
        assert!(state.complete(&key,first,Ok(Loaded {item:item.clone(),epoch:2})));
        assert!(!state.entries[&key].complete);
        let probe=state.entries[&key].attempt;assert_ne!(probe,first);
        assert!(!state.complete(&key,first,Ok(Loaded {item:item.clone(),epoch:1})));
        assert!(state.complete(&key,probe,Ok(Loaded {item,epoch:2})));
        assert!(state.entries[&key].complete);
    }
    #[test]
    fn all_pending_sources_progress_and_unrelated_metadata_does_not_probe_them() {
        let items=vec![text("a",&"a".repeat(PART_BYTES)),text("b",&"b".repeat(PART_BYTES))];
        let mut state=State::default();state.project(source_snapshot(Snapshot {generation:1,source_revision:1,
            items:items.clone(),..Snapshot::default()}),None);
        let pending=state.requests();assert_eq!(pending.len(),2);
        // A's in-flight read spans source revisions; B still has an independent
        // request and is admitted by the same fair two-worker semaphore.
        state.project(source_snapshot(Snapshot {generation:2,source_revision:2,items:items.clone(),..Snapshot::default()}),None);
        assert!(state.complete(&pending[0].0,pending[0].1,Ok(Loaded {item:items[0].clone(),epoch:1})));
        assert_eq!(state.requests().len(),2);
        assert!(state.complete(&pending[1].0,pending[1].1,Ok(Loaded {item:items[1].clone(),epoch:2})));
        for (key,attempt,item,_) in state.requests() {
            assert!(state.complete(&key,attempt,Ok(Loaded {item,epoch:2})));
        }
        assert!(state.requests().is_empty());
        state.project(source_snapshot(Snapshot {generation:3,source_revision:2,detail:"Account usage changed".into(),
            items,..Snapshot::default()}),None);
        assert!(state.requests().is_empty(),"metadata alone must not probe source epochs");
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
    fn actor_replacement_does_not_reuse_unknown_session_epochs_or_old_results() {
        let mut state=State::default();let item=text("same-root",&"x".repeat(PART_BYTES));
        let raw=source_snapshot(Snapshot {items:vec![item.clone()],..Snapshot::default()});
        state.project(raw.clone(),None);let old=state.requests().remove(0);
        assert!(state.complete(&old.0,old.1,Ok(Loaded {item:text("same-root","old actor"),epoch:1})));
        // Ready/forget_transcript clear even when both actors lack session IDs
        // and begin at identical publication/source revisions.
        state.clear();let projected=state.project(raw,None);
        assert_eq!(body(&projected.items[0]),"Loading message…");
        let new=state.requests().remove(0);assert_ne!(old.1,new.1);
        assert!(!state.complete(&old.0,old.1,Ok(Loaded {item:text("same-root","stale"),epoch:1})));
        assert!(state.complete(&new.0,new.1,Ok(Loaded {item,epoch:1})));
    }

    #[test]
    fn unselected_children_do_not_read_or_retain_full_sources() {
        let children=(0..30).map(|index|chat::Subagent {id:format!("child-{index}"),kind:"fixture".into(),
            description:String::new(),status:ToolStatus::Succeeded,model:None,waiting:false,detail:String::new(),background:true,
            items:vec![text("root",&"x".repeat(PART_BYTES))]}).collect();
        let raw=source_snapshot(Snapshot {subagents:children,..Snapshot::default()});let mut state=State::default();
        state.project(raw.clone(),None);assert!(state.requests().is_empty());assert_eq!(state.retained_bytes(),0);
        state.project(raw.clone(),Some("child-0"));let request=state.requests().remove(0);
        assert_eq!(request.0.agent_id.as_deref(),Some("child-0"));
        assert!(state.complete(&request.0,request.1,Ok(Loaded {item:text("root","complete first child"),epoch:1})));
        assert!(state.retained_bytes()>0);
        state.project(raw.clone(),Some("child-1"));assert_eq!(state.retained_bytes(),0);
        let requests=state.requests();assert_eq!(requests.len(),1);assert_eq!(requests[0].0.agent_id.as_deref(),Some("child-1"));
        state.project(raw,None);assert!(state.requests().is_empty());assert_eq!(state.retained_bytes(),0);
    }

    #[test]
    fn opaque_provider_id_that_looks_like_a_part_is_never_guessed_or_collapsed() {
        let opaque="root:native-part:1:text:3-7:0-0";
        let fragment_id=format!("{opaque}:cache-alias:1");
        let original=text(opaque,"real");let fragment=text(&fragment_id,"tail");
        let raw=Arc::new(Snapshot {items:vec![fragment.clone(),original.clone()],source_parts:vec![
            chat::cache::SourcePart {item_id:fragment_id,source:Key {agent_id:None,source_id:"root".into()},start:3,end:7,source_len:7,replacement:false,metadata:chat::cache::SourceMetadata::from_item(&fragment).unwrap(),epoch:1},
            chat::cache::SourcePart {item_id:opaque.into(),source:Key {agent_id:None,source_id:opaque.into()},start:0,end:4,source_len:4,replacement:false,metadata:chat::cache::SourceMetadata::from_item(&original).unwrap(),epoch:1},
        ],..Snapshot::default()});
        let mut state=State::default();let projected=state.project(raw,None);
        assert_eq!(projected.items.len(),2);assert_eq!(projected.items[0].id,"root");
        assert!(Arc::ptr_eq(&projected.items[1],&original));
        let pending=state.requests();assert_eq!(pending.len(),1);assert_eq!(pending[0].0.source_id,"root");
    }

    #[test]
    fn hiding_releases_full_projected_items_and_reveal_rehydrates_from_bounded_raw_window() {
        let session=fresh_chat_session(Provider::Claude,TerminalId::new(),"/project".into(),&Account::system(Provider::Claude),false).unwrap();
        let raw=Snapshot {ready:true,items:vec![text("root","pref")],source_parts:vec![chat::cache::SourcePart {
            item_id:"root".into(),source:Key {agent_id:None,source_id:"root".into()},start:0,end:4,source_len:8,replacement:false,metadata:chat::cache::SourceMetadata::from_item(&text("root","pref")).unwrap(),epoch:1}],
            ..Snapshot::default()};
        let handle=Arc::new(ClientHandle::fixture(session.clone(),raw));
        let mut runtime=Runtime::new(Target {card:CardId::new(),generation:1},session,Connection::Live(handle),true,
            crate::settings::EditorSyntaxTheme::default());
        let request=runtime.logical_sources.requests().remove(0);
        let full=text("root","prefbody");let weak=Arc::downgrade(&full);
        assert!(runtime.logical_sources.complete(&request.0,request.1,Ok(Loaded {item:full,epoch:1})));
        runtime.refresh();assert_eq!(body(&runtime.snapshot.items[0]),"prefbody");
        assert!(runtime.cache.contains_key("root"));
        let _=runtime.set_visible(false);
        assert!(weak.upgrade().is_none(),"hidden snapshot/cache/state must release full Item owners");
        assert_eq!(body(&runtime.snapshot.items[0]),"pref");assert!(runtime.logical_sources.requests().is_empty());
        let _=runtime.set_visible(true);runtime.refresh();
        assert_eq!(body(&runtime.snapshot.items[0]),"Loading message…");assert_eq!(runtime.logical_sources.requests().len(),1);
    }

    fn page(query:chat::cache::SourceQuery,text:&str,epoch:u64,metadata:chat::cache::SourceMetadata)->Result<chat::cache::SourcePage,chat::cache::SourceError> {
        if query.expected_epoch.is_some_and(|expected|expected!=epoch) {return Err(chat::cache::SourceError::Changed);}
        let boundary=query.until.unwrap_or(text.len());
        let mut end=(query.offset+query.limit).min(boundary);
        while !text.is_char_boundary(end) {end-=1;}
        Ok(chat::cache::SourcePage {source:query.source,epoch,source_len:boundary,offset:query.offset,next_offset:end,
            text:text[query.offset..end].into(),complete:end==boundary,metadata})
    }
    #[tokio::test]
    async fn append_proof_reads_only_new_suffix_and_uses_original_source_metadata() {
        let key=Key {agent_id:Some("child".into()),source_id:"original".into()};
        let previous=text("original","prefix 🦀");let known=Loaded {item:previous.clone(),epoch:7};
        let original=chat::Item {id:"original".into(),role:chat::Role::Assistant,started_at:Some(100),at:Some(900),
            content:ItemContent::Text {text:"prefix 🦀 appended tail".into(),thinking:true,phase:chat::Phase::Commentary}};
        let metadata=chat::cache::SourceMetadata::from_item(&original).unwrap();let mut queries=Vec::new();
        let capacity=ReaderCapacity::default();
        let loaded=read_counted(&key,Some(&known),|query| {
            queries.push(query.clone());std::future::ready(page(query,body(&original),7,metadata.clone()))
        },&capacity).await.unwrap();
        assert!(capacity.0.load(Ordering::Relaxed)>=body(&original).len());
        assert_eq!(*loaded.item,original);assert_eq!(queries.len(),2);
        assert_eq!(queries[0].offset,0);assert_eq!(queries[0].limit,4);
        assert_eq!(queries[1].offset,body(&previous).len());assert_eq!(queries[1].until,Some(body(&original).len()));
        assert_eq!(queries[1].expected_epoch,Some(7));
        let mut probes=0;
        let reused_capacity=ReaderCapacity::default();
        let retained=read_counted(&key,Some(&loaded),|query| {probes+=1;std::future::ready(page(query,body(&original),7,metadata.clone()))},&reused_capacity).await.unwrap();
        assert_eq!(probes,1);assert!(Arc::ptr_eq(&retained.item,&loaded.item));
        assert_eq!(reused_capacity.0.load(Ordering::Relaxed),0,"known shared Item adds no new reader allocation");
    }
    #[tokio::test]
    async fn a_rewrite_epoch_reloads_the_prefix_and_rejects_mid_read_rewrites() {
        let key=Key {agent_id:None,source_id:"original".into()};let old=text("original","stale prefix");
        let known=Loaded {item:old,epoch:1};let original=text("original","new root body 🦀");
        let metadata=chat::cache::SourceMetadata::from_item(&original).unwrap();let mut queries=Vec::new();
        let loaded=read(&key,Some(&known),|query| {queries.push(query.clone());std::future::ready(page(query,body(&original),2,metadata.clone()))}).await.unwrap();
        assert_eq!(*loaded.item,*original);assert_eq!(queries[1].offset,4);
        let mut calls=0;
        let changed=read(&key,None,|query| {calls+=1;std::future::ready(page(query,body(&original),if calls==1 {2} else {3},metadata.clone()))}).await;
        assert!(changed.is_err());assert_eq!(calls,2);
    }

    #[test]
    fn unselected_child_body_changes_do_not_probe_the_active_parent_source() {
        let child=chat::Subagent {id:"child".into(),kind:String::new(),description:String::new(),status:ToolStatus::Running,
            model:None,waiting:false,detail:String::new(),background:true,items:vec![text("child-body","child prefix")]};
        let raw=source_snapshot(Snapshot {generation:1,source_revision:1,items:vec![text("parent","parent prefix")],
            subagents:vec![child],..Snapshot::default()});
        let mut state=State::default();state.project(raw.clone(),None);let request=state.requests().remove(0);
        assert!(state.complete(&request.0,request.1,Ok(Loaded {item:text("parent","parent complete"),epoch:1})));
        let mut changed=(*raw).clone();changed.generation=2;changed.source_revision=2;
        for part in &mut changed.source_parts {if part.source.agent_id.is_some() {part.epoch=2;part.source_len+=100;}}
        state.project(Arc::new(changed),None);
        assert!(state.requests().is_empty(),"another source revision must not invalidate unchanged parent proof");
    }

}
