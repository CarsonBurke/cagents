//! Original provider calls are explicit typed sources, never reconstructed from
//! a transformed display command or a derived file patch.
use super::*;
use chat::cache::{SourceEpoch,SourceKey,SourceError,raw::{RawSourceField,RawSourcePart,RawSourcePage,RawSourceQuery,RawRepresentation}};
use chat::source_owners::{ResidentRaw,ResidentRawCalls};

#[derive(Clone,Debug,PartialEq,Eq,Hash)]
struct Key {source:SourceKey,field:RawSourceField}
impl Key {fn of(part:&RawSourcePart)->Self {Self {source:part.source.clone(),field:part.field}}}
fn record(part:&RawSourcePart)->ResidentRaw {ResidentRaw {source:part.source.clone(),field:part.field,epoch:part.epoch,source_len:part.source_len,metadata:part.metadata.clone()}}
fn eligible(part:&RawSourcePart)->Result<(),String> {
    if !part.metadata.complete_call {return Err("The provider is still capturing this original call.".into());}
    if part.source_len>chat::source_owners::SOURCE_BYTES {return Err("This original call exceeds the 32 MiB display limit.".into());}
    Ok(())
}
struct Pending {
    part:RawSourcePart,text:Arc<str>,key:u64,prefixes:Arc<Vec<[u8;32]>>,deadline:Instant,
    _reader:tokio::sync::OwnedSemaphorePermit,_capacity:chat::source_owners::ReaderCapacity,
}
/// The same two readers bound prose, patches and original calls, including
/// their admission waits and queued completed outputs.
async fn read<F,Fut>(part:RawSourcePart,mut fetch:F)->Result<Pending,String>
where F:FnMut(RawSourceQuery)->Fut,Fut:std::future::Future<Output=Result<RawSourcePage,SourceError>> {
    eligible(&part)?;
    let deadline=Instant::now()+Duration::from_secs(40);
    tokio::time::timeout(Duration::from_secs(40),async move {
        let reader=chat::source_owners::readers().clone().acquire_owned().await.map_err(|_|"Original loading stopped.".to_owned())?;
        let capacity=chat::source_owners::ReaderCapacity::default();let mut text=String::with_capacity(part.source_len);capacity.set(text.capacity());
        let mut offset=0;
        loop {
            let page=fetch(RawSourceQuery {source:part.source.clone(),field:part.field,offset,limit:chat::cache::PAGE_BYTES,
                expected_epoch:Some(part.epoch),until:Some(part.source_len)}).await
                .map_err(|_|"The original call changed or is unavailable. Reconnect and retry.".to_owned())?;
            if page.source!=part.source || page.field!=part.field || page.epoch!=part.epoch || page.metadata!=part.metadata
                || page.source_len!=part.source_len || page.offset!=offset || page.text.len()>chat::cache::PAGE_BYTES
                || page.next_offset.checked_sub(offset)!=Some(page.text.len()) || page.next_offset>part.source_len
                || page.complete!=(page.next_offset==part.source_len) || (!page.complete && page.next_offset==offset) {
                return Err("The original call returned inconsistent pages. No partial original is shown.".into());
            }
            text.push_str(&page.text);offset=page.next_offset;if page.complete {break;}
        }
        capacity.set(text.capacity().saturating_add(text.len()));let text:Arc<str>=Arc::from(text);capacity.set(text.len());
        tokio::task::spawn_blocking(move || {
            // All source hashing is performed on the reader worker. The final
            // canonical reader port adopts this immutable allocation directly.
            let key=tail_digest(0,&text);let prefixes=prefixes(&text);
            Pending {part,text,key,prefixes,deadline,_reader:reader,_capacity:capacity}
        }).await.map_err(|_|"Original call preparation stopped.".to_owned())
    }).await.map_err(|_|"The original call could not be read in time.".to_owned())?
}
static REVISIONS:AtomicU64=AtomicU64::new(1);
fn next_revision()->u64 {REVISIONS.fetch_add(1,Ordering::Relaxed)}
/// Exact displayed, complete call leaf. The body and validation checkpoints
/// are prepared off-thread and shared with reader, Catalog and Find.
#[derive(Clone)]
pub(super) struct Leaf {
    pub(super) record:ResidentRaw,
    pub(super) text:Arc<str>,
    pub(super) prefixes:Arc<Vec<[u8;32]>>,
}
impl Leaf {
    pub(super) fn path(&self)->[u32;2] {[10,field_index(self.record.field)]}
}
fn field_index(field:RawSourceField)->u32 {match field {RawSourceField::ToolInput=>0,RawSourceField::CommandCall=>1}}
fn prefixes(text:&str)->Arc<Vec<[u8;32]>> {
    let mut hasher=blake3::Hasher::new();let mut values=vec![*hasher.finalize().as_bytes()];
    for chunk in text.as_bytes().chunks_exact(4096) {hasher.update(chunk);values.push(*hasher.finalize().as_bytes());}
    Arc::new(values)
}
struct Loaded {record:ResidentRaw,view:tool_text::ToolText,prefixes:Arc<Vec<[u8;32]>>}
struct Entry {part:RawSourcePart,revision:u64,opened:bool,rehydrating:bool,attempt:Option<u64>,loaded:Option<Loaded>,restored:Option<Arc<str>>,error:Option<String>,pending_copy:Mutex<Option<std::sync::Weak<CopyInput>>>}
struct Disclosure {epoch:SourceEpoch,rehydrating:bool}
impl Entry {
    fn new(part:RawSourcePart)->Self {Self {part,revision:next_revision(),opened:false,rehydrating:false,attempt:None,loaded:None,restored:None,error:None,pending_copy:Mutex::new(None)}}
    fn withdraw_copy(&self) {
        let previous=self.pending_copy.lock().unwrap_or_else(|error|error.into_inner()).take();
        if let Some(input)=previous.and_then(|input|input.upgrade()) {clipboard_intent::Pending::withdraw(input.as_ref());}
    }
}
impl Drop for Entry {fn drop(&mut self) {self.withdraw_copy();}}
#[derive(Default)]
pub(super) struct State {intents:disclosure_intent::Cache<Key,Disclosure>,entries:HashMap<Key,Entry>,aliases:HashMap<String,Vec<Key>>,context:u64,next:u64,actor:Option<std::sync::Weak<ClientHandle>>,session:Option<String>,incarnation:Option<uuid::Uuid>,selected:Option<String>,revision:u64,restore_error:Option<String>,notice_revision:u64}
impl State {
    pub(super) fn clear(&mut self) {
        let entries=std::mem::take(&mut self.entries);for (_,entry) in entries {retire_entry(entry);}self.intents.clear();self.aliases.clear();self.actor=None;self.session=None;self.incarnation=None;self.selected=None;self.restore_error=None;self.notice_revision=0;
        self.context=self.context.wrapping_add(1);self.revision=self.revision.wrapping_add(1);
    }
    /// Hidden views retain only exact-source disclosure intent. A live reveal
    /// re-reads the same complete call instead of retaining its body or relying
    /// on the EOF sidecar. Identity changes still use clear().
    pub(super) fn evict(&mut self) {
        let mut changed=false;
        for entry in self.entries.values_mut() {
            entry.withdraw_copy();
            let mut entry_changed=entry.attempt.take().is_some();
            if let Some(loaded)=entry.loaded.take() {entry.rehydrating=entry.opened;entry.error=None;retire_loaded(loaded);entry_changed=true;}
            if let Some(text)=entry.restored.take() {entry.rehydrating=entry.opened;retire(text);entry_changed=true;}
            if entry_changed {entry.revision=next_revision();}changed |=entry_changed;
        }
        if changed {self.context=self.context.wrapping_add(1);self.revision=self.revision.wrapping_add(1);}
    }
    fn sync(&mut self,snapshot:&Snapshot,selected:Option<&str>,actor:Option<&Arc<ClientHandle>>) {
        let same_actor=match (&self.actor,actor) {(None,None)=>true,(Some(old),Some(new))=>old.as_ptr()==Arc::as_ptr(new),_=>false};
        if !same_actor || self.session!=snapshot.session_id || self.incarnation!=snapshot.source_incarnation || self.selected.as_deref()!=selected {self.clear();}
        self.actor=actor.map(Arc::downgrade);self.session=snapshot.session_id.clone();self.incarnation=snapshot.source_incarnation;self.selected=selected.map(str::to_owned);
        let mut present=HashSet::new();let mut aliases:HashMap<String,Vec<Key>>=HashMap::new();
        for part in snapshot.raw_source_parts.iter().filter(|part|part.source.agent_id.as_deref()==selected
            && snapshot.source_incarnation.is_none_or(|incarnation|part.epoch.incarnation==incarnation)) {
            let key=Key::of(part);present.insert(key.clone());
            // Both aliases are Store-authored. No native-part grammar or path
            // spelling is used to infer a source identity.
            for alias in [&part.item_id,&part.source.source_id] {
                let keys=aliases.entry(alias.clone()).or_default();if !keys.contains(&key) {keys.push(key.clone());}
            }
            match self.entries.get_mut(&key) {
                Some(entry) if record(&entry.part)==record(part)=>entry.part=part.clone(),
                Some(entry)=>{
                    let old=std::mem::replace(entry,Entry::new(part.clone()));retire_entry(old);self.revision=self.revision.wrapping_add(1);
                }
                None=>{
                    let mut entry=Entry::new(part.clone());
                    if let Some(intent)=self.intents.take(&key).filter(|intent|intent.epoch==part.epoch) {
                        entry.opened=true;entry.rehydrating=intent.rehydrating;
                    }
                    self.entries.insert(key,entry);self.revision=self.revision.wrapping_add(1);
                }
            }
        }
        let removed:Vec<_>=self.entries.keys().filter(|key|!present.contains(*key)).cloned().collect();
        for key in removed {if let Some(entry)=self.entries.remove(&key) {
            // A bounded page can omit a still-existing source. Retain no
            // descriptor/body/read permission, only a same-epoch open choice.
            if entry.opened {
                let bytes=key.source.source_id.len()+key.source.agent_id.as_ref().map_or(0,String::len)+std::mem::size_of::<Disclosure>()+std::mem::size_of::<Key>();
                self.intents.remember(key,Disclosure {epoch:entry.part.epoch,rehydrating:entry.rehydrating || entry.loaded.is_some() || entry.restored.is_some()},bytes);
            }
            retire_entry(entry);self.revision=self.revision.wrapping_add(1);
        }}
        self.aliases=aliases;
        for entry in self.entries.values_mut() {
            if entry.opened && entry.loaded.is_none() && entry.restored.is_none() && entry.attempt.is_none() && entry.error.is_none() {
                match eligible(&entry.part) {
                    Ok(())=>{self.next=self.next.wrapping_add(1);entry.attempt=Some(self.next);entry.revision=next_revision();},
                    Err(error)=>entry.error=Some(error),
                }
                self.revision=self.revision.wrapping_add(1);
            }
        }
    }
    fn key(&self,item:&str,field:RawSourceField)->Option<Key> {
        let mut keys=self.aliases.get(item)?.iter().filter(|key|key.field==field);
        let key=keys.next()?;keys.next().is_none().then(||key.clone())
    }
    fn toggle(&mut self,item:&str,field:RawSourceField) {
        let Some(key)=self.key(item,field) else {return;};let Some(entry)=self.entries.get_mut(&key) else {return;};
        entry.opened=!entry.opened;entry.error=None;
        if entry.opened {
            match eligible(&entry.part) {Ok(())=>{self.next=self.next.wrapping_add(1);entry.attempt=Some(self.next);},Err(error)=>entry.error=Some(error)}
        } else {entry.withdraw_copy();entry.rehydrating=false;entry.attempt=None;if let Some(loaded)=entry.loaded.take() {retire_loaded(loaded);}if let Some(text)=entry.restored.take() {retire(text);}}
        entry.revision=next_revision();self.revision=self.revision.wrapping_add(1);
    }
    pub(super) fn revision(&self)->u64 {self.revision}
    pub(super) fn context_token(&self)->u64 {self.context}
    /// Display order matches detail(), with no source/body reconstruction.
    /// Restored bytes remain unavailable until their worker preparation is
    /// installed; stale descriptors are removed synchronously by sync().
    pub(super) fn loaded_leaves(&self,item:&str)->Vec<Leaf> {
        [RawSourceField::ToolInput,RawSourceField::CommandCall].into_iter().filter_map(|field| {
            let key=self.key(item,field)?;let entry=self.entries.get(&key)?;
            if !entry.opened {return None;}
            let loaded=entry.loaded.as_ref()?;
            (loaded.record==record(&entry.part)).then(||Leaf {record:loaded.record.clone(),text:loaded.view.text.clone(),prefixes:loaded.prefixes.clone()})
        }).collect()
    }
    /// Placeholder identities carry no text, only current-source suspended
    /// selection intent while an already-open disclosure rehydrates.
    pub(super) fn pending_paths(&self,item:&str)->Vec<[u32;2]> {
        self.pending_sources(item).into_iter().map(|(path,_)|path).collect()
    }
    /// Current authoritative epoch for a prior same-source pending leaf. The
    /// body-free page tombstone alone cannot expose this capability.
    pub(super) fn pending_sources(&self,item:&str)->Vec<([u32;2],SourceEpoch)> {
        [RawSourceField::ToolInput,RawSourceField::CommandCall].into_iter().filter_map(|field| {
            let key=self.key(item,field)?;let entry=self.entries.get(&key)?;
            (entry.rehydrating && entry.opened && entry.loaded.is_none()).then_some(([10,field_index(field)],entry.part.epoch))
        }).collect()
    }
    pub(super) fn complete_for_find(&self,item:&str)->bool {
        self.aliases.get(item).into_iter().flatten().filter_map(|key|self.entries.get(key)).filter(|entry|entry.opened)
            .all(|entry|entry.loaded.as_ref().is_some_and(|loaded|loaded.record==record(&entry.part)))
    }
    pub(super) fn item_revision(&self,item:&str)->u64 {
        self.aliases.get(item).into_iter().flatten().filter_map(|key|self.entries.get(key)).map(|entry|entry.revision.max(self.notice_revision)).max().unwrap_or(0)
    }
    fn restore_notice(&mut self,error:Option<String>) {
        if self.restore_error!=error {self.restore_error=error;self.notice_revision=next_revision();}
    }
    /// Geometry completions must match this selected displayed alias and the
    /// current immutable call, not merely any tool cache with the same ID.
    pub(super) fn item_matches(&self,item:&str,matches:impl Fn(&tool_text::ToolText)->bool)->bool {
        self.aliases.get(item).into_iter().flatten().filter_map(|key|self.entries.get(key))
            .filter(|entry|entry.opened).any(|entry|entry.loaded.as_ref()
                .is_some_and(|loaded|loaded.record==record(&entry.part) && matches(&loaded.view)))
    }
    pub(super) fn render_ready(&mut self,ready:impl Fn(&tool_text::ToolText)->bool)->bool {
        let mut changed=false;
        for entry in self.entries.values_mut() {
            if entry.loaded.as_ref().is_some_and(|loaded|ready(&loaded.view)) {entry.revision=next_revision();changed=true;}
        }
        if changed {self.revision=self.revision.wrapping_add(1);}changed
    }
    /// Checkpoint index allocations are metadata, outside the shared raw body
    /// pool and ReaderCapacity. Descriptor clones share these same allocations.
    pub(super) fn retained_checkpoint_bytes(&self)->usize {
        self.entries.values().filter_map(|entry|entry.loaded.as_ref())
            .map(|loaded|loaded.prefixes.capacity().saturating_mul(std::mem::size_of::<[u8;32]>())).sum()
    }
    /// (records, logical metadata bytes); excludes allocator/container RSS.
    pub(super) fn paged_intent_metadata(&self)->(usize,usize) {self.intents.usage()}
    pub(super) fn retained_bytes(&self)->usize {self.entries.values().map(|entry|entry.loaded.as_ref().map_or(0,|loaded|loaded.view.text.len())+entry.restored.as_ref().map_or(0,|text|text.len())).sum()}
    pub(super) fn capture(&self)->ResidentRawCalls {
        ResidentRawCalls {calls:self.entries.values().filter_map(|entry|entry.loaded.as_ref().map(|loaded|(loaded.view.text.clone(),loaded.record.clone()))
            .or_else(||entry.restored.as_ref().map(|text|(text.clone(),record(&entry.part))))).collect()}
    }
    pub(super) fn restore(&mut self,view:ResidentRawCalls)->Result<(),String> {
        let validation=(|| {
            let mut seen=HashSet::new();
            for (text,proof) in &view.calls {
                let key=Key {source:proof.source.clone(),field:proof.field};
                if !seen.insert(key.clone()) || self.entries.get(&key).is_none_or(|entry|record(&entry.part)!=*proof)
                    || chat::source_owners::resident_raw(text).as_ref()!=Some(proof) {return Err("The offline original call belongs to another source.".to_owned());}
            }
            Ok(())
        })();
        if let Err(error)=validation {retire(view);return Err(error);}
        for (text,proof) in view.calls {
            let key=Key {source:proof.source,field:proof.field};let entry=self.entries.get_mut(&key).expect("validated raw source");
            entry.opened=true;if let Some(old)=entry.restored.replace(text) {retire(old);}entry.revision=next_revision();self.next=self.next.wrapping_add(1);entry.attempt=Some(self.next);
        }
        self.restore_notice(None);self.revision=self.revision.wrapping_add(1);Ok(())
    }
}
impl Drop for State {fn drop(&mut self) {self.clear();}}
#[derive(Clone)]
struct Request {target:Target,key:Key,context:u64,attempt:u64,part:RawSourcePart,handle:Option<ClientHandle>,offline:Option<Arc<str>>}
impl PartialEq for Request {fn eq(&self,other:&Self)->bool {self.target==other.target && self.key==other.key && self.context==other.context && self.attempt==other.attempt}}
impl Eq for Request {}
impl std::hash::Hash for Request {fn hash<H:std::hash::Hasher>(&self,state:&mut H) {use std::hash::Hash;std::any::TypeId::of::<Self>().hash(state);self.target.hash(state);self.key.hash(state);self.context.hash(state);self.attempt.hash(state);}}
#[derive(Clone)]
pub(super) struct Reply(Arc<ReplySlot>);
struct ReplySlot(Mutex<Option<Result<Pending,String>>>);
impl Drop for ReplySlot {fn drop(&mut self) {if let Some(value)=self.0.get_mut().unwrap_or_else(|error|error.into_inner()).take() {retire(value);}}}
fn stream(request:&Request)->Pin<Box<dyn iced::futures::Stream<Item=Message>+Send>> {
    let request=request.clone();Box::pin(iced::futures::stream::once(async move {
        let result=tokio::time::timeout(Duration::from_secs(40),async {
            let pending=if let Some(text)=request.offline {
                let reader=chat::source_owners::readers().clone().acquire_owned().await.map_err(|_|"Original loading stopped.".to_owned())?;
                let part=request.part.clone();tokio::task::spawn_blocking(move || Pending {key:tail_digest(0,&text),prefixes:prefixes(&text),part,text,deadline:Instant::now()+Duration::from_secs(40),_reader:reader,_capacity:Default::default()})
                    .await.map_err(|_|"Original preparation stopped.".to_owned())?
            } else {
                let handle=request.handle.ok_or("Reconnect to load the complete original call.")?;
                read(request.part.clone(),move |query|{let handle=handle.clone();async move {handle.raw_source_page(query).await}}).await?
            };
            let proof=record(&pending.part);
            loop {match chat::source_owners::admit_raw(&pending.text,&proof,None) {
                chat::source_owners::Admission::Ready=>break,
                chat::source_owners::Admission::Turnover if pending.deadline>Instant::now()=>tokio::time::sleep(Duration::from_millis(16)).await,
                _=>return Err("Complete original calls have reached the display memory limit. Collapse another call and retry.".into()),
            }}
            Ok(pending)
        }).await.unwrap_or_else(|_|Err("The complete original call could not be loaded in time.".into()));
        action(Action::RawOriginalReady(request.target,request.key.source,request.key.field,request.context,request.attempt,Reply(Arc::new(ReplySlot(Mutex::new(Some(result)))))))
    }))
}
pub(super) fn sync(runtime:&mut Runtime) {
    if !runtime.visible.load(Ordering::Relaxed) {runtime.raw_original.evict();return;}
    let handle=runtime.handle().cloned();runtime.raw_original.sync(&runtime.snapshot,runtime.selected_subagent.as_deref(),handle.as_ref());
}
pub(super) fn subscriptions(runtime:&Runtime)->Vec<Subscription<Message>> {
    if !runtime.visible.load(Ordering::Relaxed) {return Vec::new();}
    runtime.raw_original.entries.iter().filter_map(|(key,entry)|Some(Request {target:runtime.target,key:key.clone(),context:runtime.raw_original.context,attempt:entry.attempt?,
        part:entry.part.clone(),handle:runtime.handle().map(|handle|(**handle).clone()),offline:entry.restored.clone()})).map(|request|Subscription::run_with(request,stream)).collect()
}
pub(super) fn complete(runtime:&mut Runtime,source:SourceKey,field:RawSourceField,context:u64,attempt:u64,reply:Reply) {
    let Some(result)=reply.0.0.lock().unwrap_or_else(|error|error.into_inner()).take() else {return;};
    sync(runtime);let state=&mut runtime.raw_original;let key=Key {source,field};
    let accepted=runtime.visible.load(Ordering::Relaxed) && state.context==context && state.entries.get(&key).is_some_and(|entry|entry.opened && entry.attempt==Some(attempt));
    if !accepted {retire(result);return;}
    let entry=state.entries.get_mut(&key).expect("current raw request");entry.attempt=None;
    match result {
        Ok(pending) if record(&pending.part)==record(&entry.part) && pending.deadline>Instant::now() && chat::source_owners::resident_raw(&pending.text).as_ref()==Some(&record(&entry.part))=>{
            let view=tool_text::ToolText::from_captured(pending.text.clone(),pending.key);
            if let Some(old)=entry.loaded.replace(Loaded {record:record(&entry.part),view,prefixes:pending.prefixes.clone()}) {retire_loaded(old);}
            entry.restored=None;entry.rehydrating=false;retire(pending);
        }
        Ok(pending)=>{retire(pending);entry.error=Some("The original call changed before it was displayed. Retry its current source.".into());}
        Err(error)=>entry.error=Some(error),
    }
    entry.revision=next_revision();state.revision=state.revision.wrapping_add(1);
}
fn label(field:RawSourceField)->&'static str {match field {RawSourceField::ToolInput=>"Original tool input",RawSourceField::CommandCall=>"Original command details"}}
fn representation(value:RawRepresentation)->&'static str {match value {RawRepresentation::StreamJson=>"Original streamed JSON",RawRepresentation::CanonicalJson=>"Input received as parsed JSON",RawRepresentation::CommandMetadataJson=>"Recorded command details (JSON)"}}
pub(super) fn detail(runtime:&Runtime,item:&str)->Element<'static,Message> {
    let mut rows=Vec::new();let target=runtime.target;
    if let Some(error)=&runtime.raw_original.restore_error {rows.push(design::caption(error.clone()).into());}
    for field in [RawSourceField::ToolInput,RawSourceField::CommandCall] {
        let Some(key)=runtime.raw_original.key(item,field) else {continue;};let entry=&runtime.raw_original.entries[&key];
        rows.push(widget::button::custom(widget::row::with_children(vec![
                widget::icon(Icon::handle(if entry.opened {Icon::PanDown} else {Icon::PanEnd})).size(design::COMPACT_ICON_SIZE).into(),
                design::caption(label(field)).into(),
            ]).spacing(step::GAP_XS).align_y(Alignment::Center))
            .name(label(field)).class(crate::ui_controls::control_style(entry.opened,None))
            .on_press(action(Action::RawOriginalToggle(target,item.into(),field))).into());
        if !entry.opened {continue;}
        rows.push(design::caption(format!("{} · {}",entry.part.metadata.tool_name,representation(entry.part.metadata.representation))).into());
        if let Some(loaded)=&entry.loaded {
            rows.push(widget::button::text("Copy complete original").on_press(action(Action::RawOriginalCopy(target,item.into(),field))).into());
            rows.push(loaded.view.view_from_start(runtime.syntax_theme,320.0,target,None));
            if let Some(error)=&entry.error {rows.push(design::caption(error.clone()).into());}
        } else if let Some(error)=&entry.error {rows.push(design::caption(error.clone()).into());
            rows.push(widget::button::text("Retry original call").on_press(action(Action::RawOriginalReload(target,item.into(),field))).into());
        } else {rows.push(design::caption("Loading complete original call…").into());}
    }
    if rows.is_empty() {rows.push(design::caption("The provider has no captured original call for this step.").into());}
    widget::column::with_children(rows).spacing(step::GAP_S).width(Length::Fill).into()
}
fn retire_loaded(loaded:Loaded) {
    // ToolText's canonical flow cache contains UI-only Rc owners. Keep one
    // pure body Arc alive while dropping that UI presentation, then transfer
    // the final source allocation to the background reclaimer. No Send cast.
    let text=loaded.view.text.clone();drop(loaded.view);retire((text,loaded.record,loaded.prefixes));
}
fn retire_entry(mut entry:Entry) {
    entry.withdraw_copy();
    if let Some(loaded)=entry.loaded.take() {retire_loaded(loaded);}
    if let Some(text)=entry.restored.take() {retire(text);}
}
pub(super) fn restore(runtime:&mut Runtime,view:ResidentRawCalls) {
    sync(runtime);
    if let Err(error)=runtime.raw_original.restore(view) {runtime.raw_original.restore_notice(Some(error));}
}
fn retire(value:impl Send+'static) {
    type Retired=Box<dyn std::any::Any+Send>;
    static SENDER:std::sync::OnceLock<std::sync::mpsc::Sender<Retired>>=std::sync::OnceLock::new();
    let sender=SENDER.get_or_init(||{let(send,receive)=std::sync::mpsc::channel();std::thread::Builder::new().name("cagents-original-call-retire".into()).spawn(move||{for value in receive {drop(value);}}).expect("start original call retirement");send});
    if let Err(value)=sender.send(Box::new(value)) {drop(value);}
}

struct CopyOutput {text:String,_reader:tokio::sync::OwnedSemaphorePermit,_capacity:chat::source_owners::ReaderCapacity}
#[derive(Clone)]
pub(super) struct CopyReply(Arc<CopySlot>);
struct CopySlot {value:Mutex<Option<Result<CopyOutput,String>>>,intent:clipboard_intent::Intent}
impl Drop for CopySlot {fn drop(&mut self) {if let Some(value)=self.value.get_mut().unwrap_or_else(|error|error.into_inner()).take() {retire(value);}}}
struct CopyInput(Mutex<Option<Arc<str>>>);
impl CopyInput {fn take(&self)->Option<Arc<str>> {self.0.lock().unwrap_or_else(|error|error.into_inner()).take()}}
impl clipboard_intent::Pending for CopyInput {fn withdraw(&self) {if let Some(text)=self.take() {retire(text);}}}
impl Drop for CopyInput {fn drop(&mut self) {if let Some(text)=self.0.get_mut().unwrap_or_else(|error|error.into_inner()).take() {retire(text);}}}
pub(super) fn copy(runtime:&Runtime,item:&str,field:RawSourceField,intent:clipboard_intent::Intent)->Task<Message> {
    if !intent.current() {return Task::none();}
    let Some(key)=runtime.raw_original.key(item,field) else {return Task::none();};
    let Some(entry)=runtime.raw_original.entries.get(&key) else {return Task::none();};let Some(loaded)=entry.loaded.as_ref() else {return Task::none();};
    let proof=loaded.record.clone();let input=Arc::new(CopyInput(Mutex::new(Some(loaded.view.text.clone()))));intent.register(&input);
    // Keep only the newest input; release bookkeeping before withdrawal can retire its body.
    let previous=entry.pending_copy.lock().unwrap_or_else(|error|error.into_inner()).replace(Arc::downgrade(&input));
    if let Some(previous)=previous.and_then(|input|input.upgrade()) {clipboard_intent::Pending::withdraw(previous.as_ref());}
    let target=runtime.target;let context=runtime.raw_original.context;
    Task::perform(async move {
        let current=intent.clone();
        let value=tokio::time::timeout(Duration::from_secs(40),async move {
            let reader=chat::source_owners::readers().clone().acquire_owned().await.map_err(|_|"Original copy stopped.".to_owned())?;
            tokio::task::spawn_blocking(move || {
                let text=input.take().ok_or_else(||"A newer copy replaced this request.".to_owned())?;
                if !current.current() {return Err("A newer copy replaced this request.".to_owned());}
                let capacity=chat::source_owners::ReaderCapacity::default();let text=text.to_string();capacity.set(text.capacity());
                Ok(CopyOutput {text,_reader:reader,_capacity:capacity})
            }).await.map_err(|_|"Original copy stopped.".to_owned())?
        }).await.unwrap_or_else(|_|Err("The original call could not be copied in time.".into()));
        (proof,CopyReply(Arc::new(CopySlot {value:Mutex::new(Some(value)),intent})))
    },move |(proof,reply)|cosmic::Action::App(action(Action::RawOriginalCopied(target,context,proof,reply))))
}
pub(super) fn copied(runtime:&mut Runtime,context:u64,proof:ResidentRaw,reply:CopyReply)->Task<Message> {
    if !reply.0.intent.current() {return Task::none();}
    sync(runtime);let key=Key {source:proof.source.clone(),field:proof.field};
    if runtime.raw_original.context!=context || !runtime.visible.load(Ordering::Relaxed)
        || runtime.raw_original.entries.get(&key).is_none_or(|entry|!entry.opened || entry.loaded.as_ref().is_none_or(|loaded|loaded.record!=proof)) {return Task::none();}
    let Some(value)=reply.0.value.lock().unwrap_or_else(|error|error.into_inner()).take() else {return Task::none();};
    match value {Ok(output)=>{runtime.copied=Some((proof.source.source_id,Instant::now()));iced::clipboard::write(output.text).map(cosmic::Action::App)},
        Err(error)=>{let entry=runtime.raw_original.entries.get_mut(&key).expect("current original");entry.error=Some(error);entry.revision=next_revision();Task::none()}}
}
pub(super) fn toggle(runtime:&mut Runtime,item:&str,field:RawSourceField,reload:bool) {
    sync(runtime);
    if reload && let Some(key)=runtime.raw_original.key(item,field) && let Some(entry)=runtime.raw_original.entries.get_mut(&key) {entry.opened=false;}
    runtime.raw_original.toggle(item,field);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn part(text:&str)->RawSourcePart {
        RawSourcePart {item_id:"visible-late-fragment".into(),source:SourceKey {agent_id:None,source_id:"opaque:native-part:0:1048576".into()},
            field:RawSourceField::ToolInput,epoch:SourceEpoch {incarnation:uuid::Uuid::from_u128(55),revision:7},source_len:text.len(),
            metadata:chat::cache::raw::RawSourceMetadata {tool_name:"Write".into(),representation:RawRepresentation::StreamJson,complete_call:true}}
    }
    fn page(query:RawSourceQuery,part:&RawSourcePart,text:&str)->RawSourcePage {
        let mut end=(query.offset+query.limit).min(text.len());while !text.is_char_boundary(end) {end-=1;}
        RawSourcePage {source:query.source,field:query.field,epoch:part.epoch,source_len:text.len(),offset:query.offset,next_offset:end,
            text:text[query.offset..end].into(),complete:end==text.len(),metadata:part.metadata.clone()}
    }
    #[tokio::test]
    async fn original_reader_preserves_exact_stream_whitespace_and_utf8_across_pages() {
        let text=Arc::<str>::from(format!("{{\n  \"content\": \"{}LAST_ORIGINAL\"\n}}", "λoriginal ".repeat(40_000)));
        let part=part(&text);let proof=part.clone();let expected=text.clone();let calls=Arc::new(Mutex::new(Vec::new()));let seen=calls.clone();
        let pending=read(part,move |query| {seen.lock().unwrap().push(query.clone());std::future::ready(Ok(page(query,&proof,&text)))}).await.unwrap();
        assert_eq!(pending.text.as_ref(),expected.as_ref());assert!(calls.lock().unwrap().len()>1);
        assert!(calls.lock().unwrap().iter().all(|query|query.expected_epoch==Some(pending.part.epoch) && query.until==Some(expected.len())));
        let weak=Arc::downgrade(&pending.text);drop(pending);assert!(weak.upgrade().is_none());
    }
    #[tokio::test]
    async fn original_reader_rejects_wrong_field_epoch_representation_length_and_partial_completion() {
        for malformed in 0..7 {
            let text="{ \"actual\": true }";let part=part(text);let proof=part.clone();
            let result=read(part,move |query| {
                let mut value=page(query,&proof,text);
                match malformed {0=>value.field=RawSourceField::CommandCall,1=>value.epoch.incarnation=uuid::Uuid::from_u128(99),
                    2=>value.metadata.representation=RawRepresentation::CanonicalJson,3=>value.source_len+=1,
                    4=>value.metadata.complete_call=false,5=>value.next_offset-=1,_=>value.complete=false}
                std::future::ready(Ok(value))
            }).await;
            assert!(result.is_err(),"malformed original page {malformed} cannot expose a partial Original");
        }
    }
    #[tokio::test]
    async fn unavailable_or_over_limit_originals_never_read_or_claim_complete_input() {
        let calls=std::sync::atomic::AtomicUsize::new(0);
        for mode in 0..2 {let mut part=part("{}");if mode==0 {part.metadata.complete_call=false;} else {part.source_len=chat::source_owners::SOURCE_BYTES+1;}
            assert!(read(part,|_|{calls.fetch_add(1,Ordering::Relaxed);std::future::ready(Err(SourceError::NotFound))}).await.is_err());}
        assert_eq!(calls.load(Ordering::Relaxed),0);
    }
    #[test]
    fn original_aliases_and_child_scope_never_guess_source_id_from_fragment_spelling() {
        let parent=part("parent");let mut child=part("child");child.source.agent_id=Some("child".into());
        let snapshot=Snapshot {raw_source_parts:vec![parent.clone(),child.clone()],..Default::default()};let mut state=State::default();
        state.sync(&snapshot,None,None);assert_eq!(state.entries.len(),1);assert_eq!(state.key(&parent.item_id,parent.field).unwrap().source,parent.source);
        assert!(state.key("opaque",parent.field).is_none());let context=state.context;
        state.toggle(&parent.item_id,parent.field);assert!(state.entries.values().next().unwrap().attempt.is_some());
        state.sync(&snapshot,Some("child"),None);assert!(state.context>context);assert_eq!(state.entries.len(),1);
        assert_eq!(state.key(&child.item_id,child.field).unwrap().source,child.source);
        assert!(!state.entries.values().next().unwrap().opened,"another transcript cannot inherit original disclosure/request state");
    }
    #[test]
    fn ambiguous_explicit_alias_cannot_choose_another_original_call() {
        let first=part("first");let mut second=part("other");second.source.source_id="other-real-root".into();second.item_id=first.source.source_id.clone();
        let snapshot=Snapshot {raw_source_parts:vec![first.clone(),second.clone()],..Default::default()};let mut state=State::default();state.sync(&snapshot,None,None);
        assert!(state.key(&first.source.source_id,first.field).is_none(),"an alias conflict is unavailable rather than first-match authority");
        assert_eq!(state.key(&first.item_id,first.field).unwrap().source,first.source);
        assert_eq!(state.key(&second.source.source_id,second.field).unwrap().source,second.source);
    }
    #[test]
    fn offline_original_restore_requires_exact_admitted_owner_and_complete_capture_proof() {
        let text:Arc<str>=Arc::from("{  \"name\": \"actual original\" }");let part=part(&text);let proof=record(&part);
        assert_eq!(chat::source_owners::admit_raw(&text,&proof,None),chat::source_owners::Admission::Ready);
        let snapshot=Snapshot {raw_source_parts:vec![part.clone()],..Default::default()};let mut state=State::default();state.sync(&snapshot,None,None);
        let view=ResidentRawCalls {calls:vec![(text.clone(),proof.clone())]};assert!(state.restore(view.clone()).is_ok());
        let mut duplicate=view.clone();duplicate.calls.push((text.clone(),proof.clone()));assert!(state.restore(duplicate).is_err());
        let mut wrong=view.clone();wrong.calls[0].1.epoch.incarnation=uuid::Uuid::from_u128(100);assert!(state.restore(wrong).is_err());
        let mut unadmitted=view.clone();unadmitted.calls[0].0=Arc::from(text.as_ref());assert!(state.restore(unadmitted).is_err());
        assert_eq!(state.capture().calls[0].1,proof);let weak=Arc::downgrade(&text);drop(text);drop(view);
        struct Hold(std::sync::mpsc::Sender<()>,Arc<std::sync::Barrier>);
        impl Drop for Hold {fn drop(&mut self) {let _=self.0.send(());self.1.wait();}}
        let(start,started)=std::sync::mpsc::channel();let release=Arc::new(std::sync::Barrier::new(2));retire(Hold(start,release.clone()));
        started.recv_timeout(Duration::from_secs(5)).unwrap();
        state.toggle(&part.item_id,part.field);
        assert!(weak.upgrade().is_some(),"collapse hands the last restored body to the blocked background reclaimer");
        release.wait();flush();assert!(weak.upgrade().is_none());
    }
    fn runtime_fixture(text:&str)->(Runtime,RawSourcePart) {
        let part=part(text);let session=fresh_chat_session(Provider::Claude,TerminalId::new(),"/project".into(),&Account::system(Provider::Claude),false).unwrap();
        let item=Arc::new(chat::Item {id:part.item_id.clone(),role:chat::Role::Assistant,at:None,started_at:None,
            content:ItemContent::Tool {name:"Write".into(),input:"bounded display input".into(),output:String::new(),status:chat::ToolStatus::Succeeded}});
        let snapshot=Snapshot {ready:true,session_id:Some("native-session".into()),items:vec![item],raw_source_parts:vec![part.clone()],..Default::default()};
        let handle=Arc::new(ClientHandle::fixture(session.clone(),snapshot));
        (Runtime::new(Target {card:CardId::new(),generation:1},session,Connection::Live(handle),true,crate::settings::EditorSyntaxTheme::default()),part)
    }
    async fn reply(part:RawSourcePart,text:&str)->Reply {
        let proof=part.clone();let text=text.to_owned();let pending=read(part,move |query|std::future::ready(Ok(page(query,&proof,&text)))).await.unwrap();
        assert_eq!(chat::source_owners::admit_raw(&pending.text,&record(&pending.part),None),chat::source_owners::Admission::Ready);
        Reply(Arc::new(ReplySlot(Mutex::new(Some(Ok(pending))))))
    }
    fn flush() {
        struct Done(std::sync::mpsc::Sender<()>);impl Drop for Done {fn drop(&mut self) {let _=self.0.send(());}}
        let (done,finished)=std::sync::mpsc::channel();retire(Done(done));finished.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    #[tokio::test]
    async fn app_actions_and_same_tree_keep_original_loading_copy_and_collapse_coherent() {
        use iced::advanced::{Layout,layout,widget::Tree};
        let text="{ \"exact\": \"original λ\" }";let(mut runtime,part)=runtime_fixture(text);let target=runtime.target;
        let mut app=crate::app::tests::context_inspector_test_app(false);let renderer=crate::app::tests::headless_renderer();
        let mut element=detail(&runtime,&part.item_id);let mut tree=Tree::new(&element);
        let labels=|element:&mut Element<'static,Message>,tree:&mut Tree| {
            tree.diff(element.as_widget_mut());let node=element.as_widget_mut().layout(tree,&renderer,&layout::Limits::new(iced::Size::ZERO,iced::Size::new(600.0,600.0)));
            let nodes=element.as_widget().a11y_nodes(Layout::new(&node),tree,iced::mouse::Cursor::Unavailable);
            nodes.root().iter().chain(nodes.children()).filter_map(|node|node.node().label().map(str::to_owned)).collect::<Vec<_>>()
        };
        assert!(!labels(&mut element,&mut tree).iter().any(|label|label=="Copy complete original"));
        app.cards.insert(target.card,CardRuntime::Chat(Box::new(runtime)));
        drop(app.handle_chat_ui(Action::RawOriginalToggle(target,part.item_id.clone(),part.field)));
        let (context,attempt)={let runtime=app.chat_runtime_mut(target).unwrap();let state=&runtime.raw_original;(state.context,state.entries[&Key::of(&part)].attempt.unwrap())};
        let ready=reply(part.clone(),text).await;let weak={let slot=ready.0.0.lock().unwrap();let pending=slot.as_ref().unwrap().as_ref().unwrap();Arc::downgrade(&pending.text)};
        drop(app.handle_chat_ui(Action::RawOriginalReady(target,part.source.clone(),part.field,context,attempt,ready)));
        let runtime=app.chat_runtime_mut(target).unwrap();assert_eq!(runtime.raw_original.entries[&Key::of(&part)].loaded.as_ref().unwrap().view.text.as_ref(),text);
        let mut element=detail(runtime,&part.item_id);assert!(labels(&mut element,&mut tree).iter().any(|label|label=="Copy complete original"));
        drop(app.handle_chat_ui(Action::RawOriginalToggle(target,part.item_id.clone(),part.field)));
        let runtime=app.chat_runtime_mut(target).unwrap();let mut element=detail(runtime,&part.item_id);
        assert!(!labels(&mut element,&mut tree).iter().any(|label|label=="Copy complete original"));flush();assert!(weak.upgrade().is_none());
    }
    #[tokio::test]
    async fn app_rejects_old_call_and_copy_after_scope_replacement_and_descriptor_removal() {
        let text="{ \"secret\": \"old accepted call\" }";let(mut runtime,part)=runtime_fixture(text);let target=runtime.target;
        toggle(&mut runtime,&part.item_id,part.field,false);let context=runtime.raw_original.context;let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
        let ready=reply(part.clone(),text).await;
        let session=runtime.session.clone();let snapshot=(*runtime.snapshot).clone();runtime.connection=Connection::Live(Arc::new(ClientHandle::fixture(session,snapshot)));
        let mut app=crate::app::tests::context_inspector_test_app(false);app.cards.insert(target.card,CardRuntime::Chat(Box::new(runtime)));
        drop(app.handle_chat_ui(Action::RawOriginalReady(target,part.source.clone(),part.field,context,attempt,ready)));
        let runtime=app.chat_runtime_mut(target).unwrap();assert!(runtime.raw_original.context!=context);assert_eq!(runtime.raw_original.retained_bytes(),0);
        let semaphore=Arc::new(tokio::sync::Semaphore::new(1));let permit=semaphore.clone().try_acquire_owned().unwrap();
        let copy=CopyReply(Arc::new(CopySlot {value:Mutex::new(Some(Ok(CopyOutput {text:text.into(),_reader:permit,_capacity:Default::default()}))),intent:clipboard_intent::Intents::default().next()}));
        drop(app.handle_chat_ui(Action::RawOriginalCopied(target,context,record(&part),copy)));
        assert!(app.chat_runtime_mut(target).unwrap().copied.is_none());flush();assert_eq!(semaphore.available_permits(),1);
        let runtime=app.chat_runtime_mut(target).unwrap();let prose=runtime.snapshot.source_revision;
        Arc::make_mut(&mut runtime.snapshot).raw_source_parts.clear();sync(runtime);
        assert_eq!(runtime.snapshot.source_revision,prose);assert!(runtime.raw_original.entries.is_empty(),"Raw removal is authoritative independently of prose revision");
    }
    #[tokio::test]
    async fn actual_original_copy_yields_to_newer_literal_clipboard_intent() {
        use iced::futures::StreamExt as _;
        let text=r#"{ "exact": "original input" }"#;
        let (mut runtime,part)=runtime_fixture(text);let target=runtime.target;
        toggle(&mut runtime,&part.item_id,part.field,false);let context=runtime.raw_original.context;
        let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
        complete(&mut runtime,part.source.clone(),part.field,context,attempt,reply(part.clone(),text).await);
        let mut app=crate::app::tests::context_inspector_test_app(false);app.cards.insert(target.card,CardRuntime::Chat(Box::new(runtime)));
        let task=app.handle_chat_ui(Action::RawOriginalCopy(target,part.item_id.clone(),part.field));
        let mut stream=iced::runtime::task::into_stream(task).unwrap();
        let completed=loop {
            if let Some(iced::runtime::Action::Output(cosmic::Action::App(message@Message::ChatUi(Action::RawOriginalCopied(..)))))=stream.next().await {break message;}
        };
        drop(app.handle_chat_ui(Action::CopyLiteral("newer code copy".into())));
        drop(app.handle_chat_ui(match completed {Message::ChatUi(action)=>action,_=>unreachable!()}));
        assert!(app.chat_runtime_mut(target).unwrap().copied.is_none());
        assert!(app.chat_runtime_mut(target).unwrap().raw_original.entries[&Key::of(&part)].error.is_none());
    }

    #[tokio::test]
    async fn source_reset_invalidates_loaded_call_and_old_same_session_reply() {
        let text=r#"{ "old": "input" }"#;let(mut runtime,part)=runtime_fixture(text);
        toggle(&mut runtime,&part.item_id,part.field,false);let context=runtime.raw_original.context;
        let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();let old=reply(part.clone(),text).await;
        Arc::make_mut(&mut runtime.snapshot).source_incarnation=Some(uuid::Uuid::from_u128(99));
        complete(&mut runtime,part.source.clone(),part.field,context,attempt,old);
        assert_ne!(runtime.raw_original.context,context);assert!(runtime.raw_original.entries.is_empty());
        assert_eq!(runtime.raw_original.retained_bytes(),0);
        assert!(runtime.snapshot.raw_source_parts.iter().any(|old|old.epoch==part.epoch),"old unchanged aliases cannot authorize the new source lifetime");
    }

    #[test]
    fn original_disclosure_invalidates_only_its_own_scoped_item() {
        let first=part("first");let mut second=part("second");second.source.source_id="second-original-root".into();second.item_id="second-visible-part".into();
        let snapshot=Snapshot {raw_source_parts:vec![first.clone(),second.clone()],..Default::default()};let mut state=State::default();state.sync(&snapshot,None,None);
        let first_before=state.item_revision(&first.item_id);let second_before=state.item_revision(&second.item_id);
        state.toggle(&first.item_id,first.field);assert!(state.item_revision(&first.item_id)>first_before);
        assert_eq!(state.item_revision(&second.item_id),second_before,"unrelated turns retain their presentation");
        assert_eq!(state.item_revision(&first.source.source_id),state.item_revision(&first.item_id));
    }
    #[test]
    fn clearing_a_scoped_restore_notice_invalidates_all_rows_that_displayed_it_even_for_empty_restore() {
        let first=part("first");let mut second=part("second");second.source.source_id="second-root".into();second.item_id="second-visible".into();
        let snapshot=Snapshot {raw_source_parts:vec![first.clone(),second.clone()],..Default::default()};let mut state=State::default();state.sync(&snapshot,None,None);
        state.restore_notice(Some("The offline original source changed.".into()));
        let before=[state.item_revision(&first.item_id),state.item_revision(&second.item_id)];
        state.restore(ResidentRawCalls::default()).unwrap();assert!(state.restore_error.is_none());
        assert!(state.item_revision(&first.item_id)>before[0]);assert!(state.item_revision(&second.item_id)>before[1]);
        let stable=state.item_revision(&second.item_id);state.toggle(&first.item_id,first.field);
        assert_eq!(state.item_revision(&second.item_id),stable,"ordinary disclosure still has item-local invalidation");
    }
    #[tokio::test]
    async fn loaded_leaf_sources_are_exact_ordered_shared_and_withdrawn_before_new_epoch_ready() {
        let text="{ \"exact\": \"λ\" }";let(mut runtime,part)=runtime_fixture(text);
        toggle(&mut runtime,&part.item_id,part.field,false);
        assert!(!runtime.raw_original.complete_for_find(&part.item_id));
        let context=runtime.raw_original.context;let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
        complete(&mut runtime,part.source.clone(),part.field,context,attempt,reply(part.clone(),text).await);
        let leaves=runtime.raw_original.loaded_leaves(&part.item_id);assert_eq!(leaves.len(),1);
        assert_eq!(leaves[0].path(),[10,0]);assert_eq!(leaves[0].record,record(&part));
        let second=runtime.raw_original.loaded_leaves(&part.item_id);
        assert!(Arc::ptr_eq(&leaves[0].text,&second[0].text));assert!(Arc::ptr_eq(&leaves[0].prefixes,&second[0].prefixes));
        assert!(runtime.raw_original.complete_for_find(&part.item_id));
        Arc::make_mut(&mut runtime.snapshot).raw_source_parts[0].epoch.revision+=1;sync(&mut runtime);
        assert!(runtime.raw_original.loaded_leaves(&part.item_id).is_empty(),"old epoch cannot be exposed while replacement is pending");
        assert!(runtime.raw_original.loaded_leaves("unknown alias").is_empty());
    }
    #[tokio::test]
    async fn both_loaded_fields_keep_render_order_and_current_geometry_ownership() {
        let input="{ \"input\": \"exact original\" }";let command="{ \"command\": \"full original call\" }";
        let(mut runtime,input_part)=runtime_fixture(input);let mut command_part=input_part.clone();
        command_part.field=RawSourceField::CommandCall;command_part.source_len=command.len();
        command_part.metadata.representation=RawRepresentation::CommandMetadataJson;
        Arc::make_mut(&mut runtime.snapshot).raw_source_parts.push(command_part.clone());sync(&mut runtime);
        // Install in the opposite order; HashMap insertion is not display order.
        for (part,text) in [(command_part,command),(input_part.clone(),input)] {
            toggle(&mut runtime,&part.item_id,part.field,false);let context=runtime.raw_original.context;
            let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
            complete(&mut runtime,part.source.clone(),part.field,context,attempt,reply(part.clone(),text).await);
        }
        let leaves=runtime.raw_original.loaded_leaves(&input_part.item_id);
        assert_eq!(leaves.iter().map(Leaf::path).collect::<Vec<_>>(),vec![[10,0],[10,1]]);
        assert_eq!(leaves.iter().map(|leaf|leaf.text.as_ref()).collect::<Vec<_>>(),vec![input,command]);
        assert!(runtime.raw_original.item_matches(&input_part.item_id,|view|view.text.as_ref()==command));
        assert!(!runtime.raw_original.item_matches("unrelated opaque alias",|_|true));
        toggle(&mut runtime,&input_part.item_id,RawSourceField::CommandCall,false);
        assert!(!runtime.raw_original.item_matches(&input_part.item_id,|view|view.text.as_ref()==command),"collapsed reader cannot resume another pending navigation");
    }
    #[test]
    fn raw_prefix_checkpoints_follow_exact_display_bytes_including_utf8_and_newlines() {
        let text="λ\r\n".repeat(3000);let values=prefixes(&text);
        for (index,value) in values.iter().enumerate() {assert_eq!(*value,*blake3::hash(&text.as_bytes()[..index*4096]).as_bytes());}
    }
    #[tokio::test]
    async fn same_actor_live_hide_releases_call_and_rehydrates_only_current_disclosure_intent() {
        let text="{ \"original\": \"live hidden input\" }";let(mut runtime,part)=runtime_fixture(text);
        toggle(&mut runtime,&part.item_id,part.field,false);let context=runtime.raw_original.context;
        let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
        complete(&mut runtime,part.source.clone(),part.field,context,attempt,reply(part.clone(),text).await);
        let weak=Arc::downgrade(&runtime.raw_original.loaded_leaves(&part.item_id)[0].text);
        let _=runtime.set_visible(false);flush();assert!(weak.upgrade().is_none());
        assert!(runtime.raw_original.entries[&Key::of(&part)].opened);
        assert_eq!(runtime.raw_original.retained_bytes(),0);
        let _=runtime.set_visible(true);sync(&mut runtime);
        assert_eq!(runtime.raw_original.pending_paths(&part.item_id),vec![[10,0]]);
        let context=runtime.raw_original.context;let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
        complete(&mut runtime,part.source.clone(),part.field,context,attempt,reply(part.clone(),text).await);
        assert_eq!(runtime.raw_original.loaded_leaves(&part.item_id)[0].text.as_ref(),text);
        let _=runtime.set_visible(false);
        Arc::make_mut(&mut runtime.snapshot).raw_source_parts[0].epoch.revision+=1;
        let _=runtime.set_visible(true);sync(&mut runtime);
        assert!(!runtime.raw_original.entries[&Key::of(&part)].opened,"new source cannot inherit old disclosure intent");
        runtime.raw_original.clear();assert!(runtime.raw_original.entries.is_empty(),"reset/revoke fully forget intent");
    }
    #[tokio::test]
    async fn prior_raw_selection_intent_survives_same_source_retryable_read_error() {
        let text="{ \"same\": \"source\" }";let(mut runtime,part)=runtime_fixture(text);
        toggle(&mut runtime,&part.item_id,part.field,false);let context=runtime.raw_original.context;
        let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
        complete(&mut runtime,part.source.clone(),part.field,context,attempt,reply(part.clone(),text).await);
        runtime.raw_original.evict();sync(&mut runtime);
        let context=runtime.raw_original.context;let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
        complete(&mut runtime,part.source.clone(),part.field,context,attempt,Reply(Arc::new(ReplySlot(Mutex::new(Some(Err("fixture timeout".into())))))));
        assert!(runtime.raw_original.entries[&Key::of(&part)].error.is_some());
        assert_eq!(runtime.raw_original.pending_paths(&part.item_id),vec![[10,0]]);
        sync(&mut runtime);assert!(runtime.raw_original.entries[&Key::of(&part)].attempt.is_none(),"no automatic error retry loop");
        toggle(&mut runtime,&part.item_id,part.field,true);let context=runtime.raw_original.context;
        let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
        assert_eq!(runtime.raw_original.pending_paths(&part.item_id),vec![[10,0]]);
        complete(&mut runtime,part.source.clone(),part.field,context,attempt,reply(part.clone(),text).await);
        assert!(runtime.raw_original.pending_paths(&part.item_id).is_empty());assert_eq!(runtime.raw_original.loaded_leaves(&part.item_id)[0].text.as_ref(),text);
    }
    #[tokio::test]
    async fn full_revocation_rejects_successful_raw_read_and_copy_with_unchanged_source_proof() {
        use iced::futures::StreamExt as _;
        let text=r#"{ "secret": "exact original input" }"#;
        let (mut runtime,part)=runtime_fixture(text);let target=runtime.target;
        toggle(&mut runtime,&part.item_id,part.field,false);
        let context=runtime.raw_original.context;
        let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
        complete(&mut runtime,part.source.clone(),part.field,context,attempt,reply(part.clone(),text).await);
        let loaded=Arc::downgrade(&runtime.raw_original.loaded_leaves(&part.item_id)[0].text);
        let mut app=crate::app::tests::context_inspector_test_app(false);
        app.cards.insert(target.card,CardRuntime::Chat(Box::new(runtime)));
        let task=app.handle_chat_ui(Action::RawOriginalCopy(target,part.item_id.clone(),part.field));
        let mut stream=iced::runtime::task::into_stream(task).unwrap();
        let copy=loop {if let Some(iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(action@Action::RawOriginalCopied(..)))))=stream.next().await {break action;}};
        if let Action::RawOriginalCopied(_,_,_,reply)=&copy {
            assert_eq!(reply.0.value.lock().unwrap().as_ref().unwrap().as_ref().unwrap().text,text);
        } else {unreachable!()}
        let pending=reply(part.clone(),text).await;
        let pending_owner={let slot=pending.0.0.lock().unwrap();Arc::downgrade(&slot.as_ref().unwrap().as_ref().unwrap().text)};
        let runtime=app.chat_runtime_mut(target).unwrap();let snapshot=runtime.snapshot.clone();
        let actor=runtime.handle().cloned().unwrap();runtime.forget_transcript();
        assert!(Arc::ptr_eq(&snapshot,&runtime.snapshot));assert!(Arc::ptr_eq(&actor,runtime.handle().unwrap()));
        assert_ne!(runtime.raw_original.context,context);
        flush();assert!(loaded.upgrade().is_none(),"full clear retires the visible external body");
        drop(app.handle_chat_ui(Action::RawOriginalReady(target,part.source.clone(),part.field,context,attempt,pending)));
        drop(app.handle_chat_ui(copy));flush();
        assert!(pending_owner.upgrade().is_none(),"obsolete successful reader output is retired");
        let runtime=app.chat_runtime_mut(target).unwrap();assert_eq!(runtime.raw_original.retained_bytes(),0);
        assert!(runtime.raw_original.entries.values().all(|entry|!entry.opened));
        assert!(runtime.copied.is_none(),"a current clipboard intent cannot authorize a revoked source");
    }

    #[tokio::test]
    async fn queued_copy_releases_body_on_hide_reset_collapse_omission_replacement_and_drop() {
        use iced::futures::{FutureExt as _,StreamExt as _};
        for revoke in 0..7 {
            let text=r#"{ "secret": "queued original input" }"#;let(mut runtime,part)=runtime_fixture(text);let target=runtime.target;
            toggle(&mut runtime,&part.item_id,part.field,false);let context=runtime.raw_original.context;
            let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
            complete(&mut runtime,part.source.clone(),part.field,context,attempt,reply(part.clone(),text).await);flush();
            let body=Arc::downgrade(&runtime.raw_original.loaded_leaves(&part.item_id)[0].text);
            let readers=chat::source_owners::readers().clone().acquire_many_owned(2).await.unwrap();
            let mut app=crate::app::tests::context_inspector_test_app(false);app.cards.insert(target.card,CardRuntime::Chat(Box::new(runtime)));
            let task=app.handle_chat_ui(Action::RawOriginalCopy(target,part.item_id.clone(),part.field));
            let input=app.chat_runtime_mut(target).unwrap().raw_original.entries[&Key::of(&part)]
                .pending_copy.lock().unwrap().as_ref().unwrap().upgrade().unwrap();
            let mut stream=iced::runtime::task::into_stream(task).unwrap();
            // Task::stream yields once before polling its copy future. Poll past that yield into the blocked reader acquisition.
            for _ in 0..2 {assert!(stream.next().now_or_never().is_none());}
            assert!(body.upgrade().is_some(),"queued copy starts with the current body");
            if revoke==6 {drop(app.cards.remove(&target.card));} else {
                let runtime=app.chat_runtime_mut(target).unwrap();
                match revoke {
                    0=>{let _=runtime.set_visible(false);},
                    1=>runtime.forget_transcript(),
                    2=>toggle(runtime,&part.item_id,part.field,false),
                    3=>{Arc::make_mut(&mut runtime.snapshot).raw_source_parts.clear();sync(runtime);},
                    4=>{Arc::make_mut(&mut runtime.snapshot).raw_source_parts[0].epoch.revision+=1;sync(runtime);},
                    5=>runtime.raw_original=State::default(),
                    _=>unreachable!(),
                }
            }
            assert!(input.0.lock().unwrap().is_none(),"revocation {revoke} withdraws the queued copy input before any reader is released");
            flush();
            if revoke==0 {
                // Hiding now also checkpoints the admitted original for EOF
                // recovery. That separate writer may legitimately retain the
                // same body after the copy has withdrawn it; wait for its spill
                // while the copy stream and both readers remain blocked.
                tokio::time::timeout(Duration::from_secs(5),async {
                    while body.upgrade().is_some() {tokio::time::sleep(Duration::from_millis(1)).await;}
                }).await.expect("the private EOF writer releases its captured original after spilling");
            }
            assert!(body.upgrade().is_none(),"revocation {revoke} releases the body while the copy future and both reader permits remain alive");
            assert!(stream.next().now_or_never().is_none(),"copy still waits for a reader after revocation");drop(readers);
            let completed=tokio::time::timeout(Duration::from_secs(5),async {
                while let Some(event)=stream.next().await {if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(action@Action::RawOriginalCopied(..))))=event {return action;}}
                panic!("copy task ended without completion")
            }).await.expect("withdrawn copy completes after reader release");
            if let Action::RawOriginalCopied(_,_,_,reply)=&completed {assert!(reply.0.value.lock().unwrap().as_ref().unwrap().is_err());}
            assert_eq!(app.handle_chat_ui(completed).units(),0,"revoked copy cannot write the clipboard");
            if let Some(runtime)=app.chat_runtime_mut(target) {assert!(runtime.copied.is_none());}
        }
    }
    #[tokio::test]
    async fn withdrawn_copy_does_not_cancel_reopened_same_source_copy() {
        use iced::futures::{FutureExt as _,StreamExt as _};
        let text=r#"{ "exact": "reopened original input" }"#;let(mut runtime,part)=runtime_fixture(text);let target=runtime.target;
        toggle(&mut runtime,&part.item_id,part.field,false);let context=runtime.raw_original.context;
        let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
        complete(&mut runtime,part.source.clone(),part.field,context,attempt,reply(part.clone(),text).await);flush();
        let body=Arc::downgrade(&runtime.raw_original.loaded_leaves(&part.item_id)[0].text);
        let readers=chat::source_owners::readers().clone().acquire_many_owned(2).await.unwrap();
        let mut app=crate::app::tests::context_inspector_test_app(false);app.cards.insert(target.card,CardRuntime::Chat(Box::new(runtime)));
        let task=app.handle_chat_ui(Action::RawOriginalCopy(target,part.item_id.clone(),part.field));
        let mut old=iced::runtime::task::into_stream(task).unwrap();for _ in 0..2 {assert!(old.next().now_or_never().is_none());}
        let runtime=app.chat_runtime_mut(target).unwrap();toggle(runtime,&part.item_id,part.field,false);flush();assert!(body.upgrade().is_none());drop(readers);
        toggle(runtime,&part.item_id,part.field,false);let context=runtime.raw_original.context;
        let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
        complete(runtime,part.source.clone(),part.field,context,attempt,reply(part.clone(),text).await);flush();
        let task=app.handle_chat_ui(Action::RawOriginalCopy(target,part.item_id.clone(),part.field));
        let mut new=iced::runtime::task::into_stream(task).unwrap();drop(old);flush();
        let completed=tokio::time::timeout(Duration::from_secs(5),async {
            while let Some(event)=new.next().await {if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(action@Action::RawOriginalCopied(..))))=event {return action;}}
            panic!("reopened copy task ended without completion")
        }).await.expect("reopened source copy completes");
        if let Action::RawOriginalCopied(_,_,_,reply)=&completed {assert_eq!(reply.0.value.lock().unwrap().as_ref().unwrap().as_ref().unwrap().text,text);} else {unreachable!()}
        assert!(app.handle_chat_ui(completed).units()>0,"current reopened source can issue its clipboard write");
        assert!(app.chat_runtime_mut(target).unwrap().copied.is_some());
    }

    #[test]
    fn source_reader_and_retirement_payloads_require_safe_send() {
        fn send<T:Send>() {}send::<Pending>();send::<Reply>();send::<ResidentRawCalls>();send::<Leaf>();
        fn future<T:Send>(_:T) {}future(read(part("{}"),|_|std::future::ready(Err(SourceError::NotFound))));
    }
    #[tokio::test]
    async fn page_omission_keeps_only_bounded_exact_source_disclosure_intent() {
        let text=r#"{ "original": "same paged source" }"#;let(mut runtime,part)=runtime_fixture(text);
        runtime.expanded.insert(part.item_id.clone());toggle(&mut runtime,&part.item_id,part.field,false);
        let context=runtime.raw_original.context;let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
        complete(&mut runtime,part.source.clone(),part.field,context,attempt,reply(part.clone(),text).await);
        let owner=Arc::downgrade(&runtime.raw_original.loaded_leaves(&part.item_id)[0].text);
        let page=runtime.snapshot.clone();let mut away=(*page).clone();away.items.clear();away.raw_source_parts.clear();away.has_earlier=true;
        runtime.snapshot=Arc::new(away);sync(&mut runtime);flush();
        assert!(runtime.expanded.contains(&part.item_id));assert!(owner.upgrade().is_none());
        assert!(runtime.raw_original.entries.is_empty() && runtime.raw_original.aliases.is_empty());
        assert!(runtime.raw_original.pending_paths(&part.item_id).is_empty());assert!(runtime.raw_original.loaded_leaves(&part.item_id).is_empty());
        assert_eq!(runtime.raw_original.intents.usage().0,1);
        runtime.snapshot=page.clone();sync(&mut runtime);
        let entry=&runtime.raw_original.entries[&Key::of(&part)];assert!(entry.opened && entry.attempt.is_some());
        assert_eq!(runtime.raw_original.pending_paths(&part.item_id),vec![[10,0]]);
        assert_eq!(runtime.raw_original.pending_sources(&part.item_id),vec![([10,0],part.epoch)]);
        let context=runtime.raw_original.context;let attempt=runtime.raw_original.entries[&Key::of(&part)].attempt.unwrap();
        complete(&mut runtime,part.source.clone(),part.field,context,attempt,reply(part.clone(),text).await);
        assert_eq!(runtime.raw_original.loaded_leaves(&part.item_id)[0].text.as_ref(),text);
        let mut away=(*page).clone();away.items.clear();away.raw_source_parts.clear();runtime.snapshot=Arc::new(away);sync(&mut runtime);
        let mut rewritten=(*page).clone();rewritten.raw_source_parts[0].epoch.revision+=1;runtime.snapshot=Arc::new(rewritten);sync(&mut runtime);
        assert!(!runtime.raw_original.entries[&Key::of(&part)].opened);assert!(runtime.raw_original.pending_paths(&part.item_id).is_empty());
        runtime.raw_original.clear();assert_eq!(runtime.raw_original.intents.usage(),(0,0));
    }

}

impl Cagents {
    /// Synchronous revocation fence for pending call reads/copies and viewed
    /// original bodies. Registry aliases do not supply read authority.
    pub(super) fn invalidate_original_tool_calls(&mut self) {
        for card in self.cards.values_mut() {if let CardRuntime::Chat(runtime)=card {runtime.raw_original.clear();}}
        for runtime in self.chat_ui.parked.values_mut() {runtime.raw_original.clear();}
    }
}
