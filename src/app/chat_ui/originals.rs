//! Exact accepted input is fetched independently from the visible transcript.
//! Eligibility and Retry keep metadata only; Edit reads text on explicit intent.
use super::*;
use chat::annotations::{OriginalError, OriginalPage, OriginalPrompt};

static NEXT: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug, PartialEq, Eq)]
struct Context {
    target: Target,
    actor: usize,
    session: String,
    turn: u64,
    original_revision: u64,
    latest_visible_user: Option<String>,
}
impl Context {
    fn current(runtime: &Runtime) -> Option<Self> {
        Some(Self {
            target:runtime.target,
            actor: Arc::as_ptr(runtime.handle()?) as usize,
            session: runtime.snapshot.session_id.clone()?,
            turn: runtime.snapshot.turn_generation,
            original_revision:runtime.snapshot.original_revision,
            latest_visible_user: turn_actions::latest_user(&runtime.snapshot).map(|item|item.id.clone()),
        })
    }
}

#[derive(Clone)]
enum Purpose {
    Metadata,
    Edit { branch: Option<String> },
}
#[derive(Clone)]
struct Query {
    target: Target,
    id: u64,
    context: Context,
    user: Option<String>,
    purpose: Purpose,
    handle: Arc<ClientHandle>,
}
#[derive(Default)]
pub(super) struct State {
    context: Option<Context>,
    metadata: Option<OriginalPrompt>,
    account_hint: Option<AccountId>,
    query: Option<Query>,
    attempted: bool,
}
impl State {
    pub(super) fn clear(&mut self) { *self=Self::default(); }
    pub(super) fn current_user(&self) -> Option<&str> {
        self.metadata.as_ref().map(|original|original.user_id.as_str())
    }
    pub(super) fn metadata_bytes(&self)->usize {
        self.metadata.as_ref().map_or(0,|original| {
            let setting=|value:&chat::annotations::Setting<String>|match value {chat::annotations::Setting::Value(value)=>value.len(),_=>0};
            original.user_id.len()+original.turn_id.len()+setting(&original.settings.model)
                +setting(&original.settings.reasoning)+setting(&original.settings.permission)
                +original.attachments.iter().map(|attachment|attachment.path.as_os_str().len()+attachment.original_path.as_os_str().len()+32).sum::<usize>()
        })
    }
    pub(super) fn loading(&self) -> bool { self.query.is_some() }
    pub(super) fn editing(&self)->bool {self.query.as_ref().is_some_and(|query|matches!(query.purpose,Purpose::Edit {..}))}
    #[cfg(test)]
    pub(super) fn query_id_for_test(&self)->u64 {self.query.as_ref().expect("production query staged").id}
    fn begin(&mut self,target:Target,context:Context,handle:Arc<ClientHandle>,user:Option<String>,purpose:Purpose) {
        self.query=Some(Query {target,id:NEXT.fetch_add(1,Ordering::Relaxed),context,user,purpose,handle});
    }
    fn accept(&mut self,target:Target,id:u64,context:&Context)->Option<Query> {
        if self.context.as_ref()!=Some(context)
            || self.query.as_ref().is_none_or(|query|query.target!=target || query.id!=id || &query.context!=context) {return None;}
        self.query.take()
    }
    fn metadata(&mut self,context:&Context,page:OriginalPage)->bool {
        let valid=page.session_id==context.session && page.offset==0 && page.next_offset==page.text.len()
            && page.text.len()<=4 && page.next_offset<=page.original.text_bytes
            && page.complete==(page.next_offset==page.original.text_bytes)
            && page.original.text_bytes<=chat::annotations::ORIGINAL_TEXT_BYTES
            && !page.original.user_id.is_empty() && page.original.settings.is_exact();
        if valid {self.metadata=Some(page.original);}
        valid
    }
}

pub(super) enum Value {
    Metadata(OriginalPage),
    Text(chat::original::OriginalText),
}
struct Output {
    value: Result<Value,OriginalError>,
    // Bound completed full-input replies as well as concurrent assembly.
    _permit: tokio::sync::OwnedSemaphorePermit,
}
#[derive(Clone)]
pub struct Reply(Arc<Mutex<Option<Output>>>);
impl std::fmt::Debug for Reply {
    fn fmt(&self,f:&mut std::fmt::Formatter<'_>)->std::fmt::Result {f.write_str("OriginalPromptReply")}
}
#[cfg(test)]
impl Reply {
    pub(super) fn fixture(value:Result<Value,OriginalError>)->Self {
        let permit=Arc::new(tokio::sync::Semaphore::new(1)).try_acquire_owned().unwrap();
        Self(Arc::new(Mutex::new(Some(Output {value,_permit:permit}))))
    }
}

#[derive(Clone)]
struct Job { query:Query,account_hint:Option<AccountId>,account_epoch:u64 }
impl PartialEq for Job {
    fn eq(&self,other:&Self)->bool {
        self.query.target==other.query.target && self.query.id==other.query.id
            && self.account_hint==other.account_hint && self.account_epoch==other.account_epoch
    }
}
impl Eq for Job {}
impl std::hash::Hash for Job {
    fn hash<H:std::hash::Hasher>(&self,state:&mut H) {
        use std::hash::Hash;
        std::any::TypeId::of::<Self>().hash(state);self.query.target.hash(state);self.query.id.hash(state);
        self.account_hint.hash(state);self.account_epoch.hash(state);
    }
}
fn readers()->&'static Arc<tokio::sync::Semaphore> {
    static READERS:std::sync::OnceLock<Arc<tokio::sync::Semaphore>>=std::sync::OnceLock::new();
    READERS.get_or_init(||Arc::new(tokio::sync::Semaphore::new(2)))
}
fn stream(job:&Job)->Pin<Box<dyn iced::futures::Stream<Item=Message>+Send>> {
    let job=job.clone();
    Box::pin(iced::futures::stream::once(async move {
        let permit=readers().clone().acquire_owned().await.expect("original readers remain open");
        let query=job.query;
        let value=match query.purpose {
            Purpose::Metadata=>query.handle.original_prompt(None,Some(&query.context.session),0,4).await.map(Value::Metadata),
            _=>query.handle.original_text(query.user.as_deref(),Some(&query.context.session)).await.map(Value::Text),
        };
        action(Action::OriginalReady(query.target,query.id,job.account_hint,job.account_epoch,
            Reply(Arc::new(Mutex::new(Some(Output {value,_permit:permit}))))))
    }))
}

impl Runtime {
    fn revoke_originals(&mut self) {
        self.recovery.originals.clear();
        self.recovery.submitted=None;
        if self.recovery.branch_draft.as_ref().is_some_and(|prompt|prompt.origin_backend) {self.recovery.branch_draft=None;}
        if self.recovery.prepared_prompt.as_ref().is_some_and(|prompt|prompt.origin_backend) {self.recovery.prepared_prompt=None;}
        self.recovery.commands.clear();self.recovery.inflight=None;self.recovery.replay_settings=None;
        // The actual unsent composer and its chips remain user intent.
        self.recovery.attachment_labels.retain(|path,_|self.attachments.iter().any(|attachment|&attachment.path==path));
    }
    pub(super) fn prompt_attachment_names(&self,command:Command)->Command {
        match command {
            Command::Prompt {text,attachments,mode}
                if attachments.iter().any(|attachment|self.recovery.attachment_labels.contains_key(&attachment.path))=>{
                Command::PromptNamed {text,mode,attachments:attachments.into_iter().map(|attachment| {
                    chat::NamedAttachment {display_path:self.recovery.attachment_labels.get(&attachment.path).cloned(),path:attachment.path}
                }).collect()}
            }
            command=>command,
        }
    }
    pub(super) fn sync_original(&mut self) {
        if !self.visible.load(Ordering::Relaxed) || self.disconnected || self.snapshot.exited {
            self.recovery.originals.clear();return;
        }
        let Some(context)=Context::current(self) else {self.recovery.originals.clear();return;};
        if self.recovery.originals.context.as_ref()!=Some(&context) {
            self.recovery.originals.clear();self.recovery.originals.context=Some(context.clone());
        }
        if self.snapshot.ready && !self.snapshot.busy && self.snapshot.read_only_reason.is_none()
            && self.pending.is_none() && self.queued.is_none() && self.selected_subagent.is_none()
            && !self.recovery.originals.attempted && self.recovery.originals.query.is_none()
            && let Some(handle)=self.handle().cloned() {
            self.recovery.originals.attempted=true;
            self.recovery.originals.begin(self.target,context,handle,None,Purpose::Metadata);
        }
    }
    /// Logical metadata payload only. Full input text is counted separately
    /// by recovery_retained_bytes; these values are not allocator/RSS estimates.
    pub(super) fn original_metadata_bytes(&self)->usize {self.recovery.originals.metadata_bytes()}
    pub(super) fn original_retry_user(&self)->Option<&str> {
        (self.snapshot.capabilities.original_retry
            && self.recovery.originals.context.as_ref()==Context::current(self).as_ref()
            && !self.recovery.originals.loading()).then(||self.recovery.originals.current_user()).flatten()
    }
}

impl Cagents {
    pub(super) fn original_subscription(&self,runtime:&Runtime)->Option<Subscription<Message>> {
        if !runtime.visible.load(Ordering::Relaxed) {return None;}
        let query=runtime.recovery.originals.query.clone()?;
        // Registry identity is only an invalidation hint. It grants no read
        // authority: the daemon independently validates the physical scope.
        let account_hint=self.chat_account(runtime.session.launch.provider,&runtime.session.launch.recipe);
        let account_epoch=self.chat_ui.original_epoch;
        Some(Subscription::run_with(Job {query,account_hint,account_epoch},stream))
    }
    /// Called for DeleteFiles intent even if physical deletion partly fails.
    /// Archive/Move do not call this. Registry IDs only invalidate cached UI
    /// lineage; they are never annotation read authority.
    pub(super) fn invalidate_originals_for_account(&mut self,_id:&AccountId) {
        // An archived/unregistered alias can share physical authority with a
        // different filing. Conservatively invalidate all small metadata and
        // still-origin-derived stashes; actual edited composers are user intent.
        self.chat_ui.original_epoch=self.chat_ui.original_epoch.wrapping_add(1);
        for card in self.cards.values_mut() {
            if let CardRuntime::Chat(runtime)=card {runtime.revoke_originals();}
        }
        for runtime in self.chat_ui.parked.values_mut() {runtime.revoke_originals();}
        self.chat_ui.recovery_drafts.retain(|_,prompt|!prompt.origin_backend);
    }
    pub(super) fn begin_original_retry(&mut self,target:Target,user:&str)->Task<Message> {
        let Some(runtime)=self.chat_runtime_mut(target) else {return Task::none();};
        runtime.refresh();
        if !runtime.recovery_available() || runtime.original_retry_user()!=Some(user)
            || runtime.recovery.retried.as_ref()==Some(&(runtime.snapshot.turn_generation,user.into())) {return Task::none();}
        let Some(original)=runtime.recovery.originals.metadata.clone() else {return Task::none();};
        let Some(context)=Context::current(runtime) else {return Task::none();};
        let generation=runtime.snapshot.turn_generation;
        runtime.recovery.retried=Some((generation,user.into()));
        // The actor atomically revalidates current/latest identity, exact
        // metadata and immutable attachment hashes before restoring settings
        // and dispatching. No UI-trusted text or ordinary setter chain.
        self.chat_send(target,Command::PromptOriginal {session_id:context.session,original,turn_generation:generation},false)
    }
    pub(super) fn begin_original_edit(&mut self,target:Target,id:&str)->Task<Message> {
        let Some(runtime)=self.chat_runtime_mut(target) else {return Task::none();};
        runtime.refresh();
        if !runtime.recovery_available() || !runtime.recovery_composer_empty() || runtime.recovery.originals.editing() {return Task::none();}
        let Some(item)=runtime.snapshot.items.iter().find(|item|item.id==id && item.role==chat::Role::User) else {return Task::none();};
        let branch=if runtime.session.launch.provider==Provider::Claude {None}
            else {let Some(branch)=turn_actions::branch_point(runtime.session.launch.provider,&runtime.snapshot,item) else {return Task::none();};Some(branch)};
        let Some(context)=Context::current(runtime) else {return Task::none();};
        let Some(handle)=runtime.handle().cloned() else {return Task::none();};
        runtime.recovery.originals.begin(target,context,handle,Some(id.into()),Purpose::Edit {branch});
        Task::none()
    }
    pub(super) fn original_ready(&mut self,target:Target,id:u64,account_hint:Option<AccountId>,account_epoch:u64,reply:Reply)->Task<Message> {
        let current_epoch=self.chat_ui.original_epoch;
        let Some(runtime)=self.chat_runtime_mut(target) else {return Task::none();};
        runtime.refresh();
        let Some(context)=Context::current(runtime) else {return Task::none();};
        let Some(query)=runtime.recovery.originals.accept(target,id,&context) else {return Task::none();};
        let Some(output)=reply.0.lock().unwrap_or_else(|error|error.into_inner()).take() else {return Task::none();};
        if current_epoch!=account_epoch {
            runtime.recovery.originals.clear();runtime.sync_original();return Task::none();
        }
        runtime.recovery.originals.account_hint=account_hint;
        let value=match output.value {
            Ok(value)=>value,
            Err(OriginalError::Unavailable(_))=>{
                // Editing is new user intent. A complete visible prompt can
                // still be edited even if it predates original annotations;
                // its current settings are never eligibility for Retry.
                if let Purpose::Edit {branch}=&query.purpose
                    && let Some(prompt)=query.user.as_deref().and_then(|id|runtime.recovery.for_user(&runtime.snapshot,id)) {
                    return self.finish_original_edit(target,prompt,branch.clone(),context.session);
                }
                if !matches!(query.purpose,Purpose::Metadata) {
                    runtime.recovery.notice=Some("The complete original prompt is unavailable for this turn.".into());
                }
                return Task::none();
            }
            Err(_)=>{
                if !matches!(query.purpose,Purpose::Metadata) {runtime.recovery.notice=Some("The original prompt changed or could not be read. Try again.".into());}
                return Task::none();
            }
        };
        match (query.purpose,value) {
            (Purpose::Metadata,Value::Metadata(page))=>{
                if runtime.recovery.originals.metadata(&context,page) {runtime.recovery.submitted=None;}
                Task::none()
            }
            (Purpose::Edit {branch},Value::Text(original))=>{
                if original.session_id!=context.session || !runtime.recovery_available() {return Task::none();}
                let Some(mut prompt)=turn_actions::Prompt::from_original(original) else {return Task::none();};
                prompt.account_hint=runtime.recovery.originals.account_hint.clone();
                self.finish_original_edit(target,prompt,branch,context.session)
            }
            _=>Task::none(),
        }
    }
    fn finish_original_edit(&mut self,target:Target,prompt:turn_actions::Prompt,branch:Option<String>,session:String)->Task<Message> {
        let Some(runtime)=self.chat_runtime_mut(target) else {return Task::none();};
        if !runtime.recovery_available() || runtime.snapshot.session_id.as_deref()!=Some(session.as_str()) {return Task::none();}
        if let Some(branch)=branch {
            if !runtime.snapshot.branch_points.iter().any(|point|point.id==branch) {return Task::none();}
            runtime.recovery.submitted=None;runtime.recovery.prepared_prompt=None;
            runtime.recovery.branch_draft=Some(prompt);runtime.recovery.branch_session=Some(session);
            runtime.fork_requested=runtime.session.launch.provider==Provider::Codex;
            self.chat_send(target,Command::Branch {id:Some(branch)},false)
        } else {
            runtime.recovery.submitted=None;runtime.recovery.prepared_prompt=None;
            runtime.load_recovery_prompt(prompt,"Editing this prompt sends a new turn in this Claude conversation.");
            iced::widget::operation::focus(card_control_id(target.card,"chat-composer")).map(cosmic::Action::App)
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    fn original(user:&str,text:&str)->OriginalPrompt {
        use chat::annotations::{OriginalSettings,Setting};
        OriginalPrompt {user_id:user.into(),turn_id:"turn".into(),text_bytes:text.len(),attachments:vec![],
            settings:OriginalSettings {model:Setting::Value("original-model".into()),reasoning:Setting::Value("low".into()),
                fast:Setting::Value(true),permission:Setting::Value("plan".into())}}
    }
    fn page(context:&Context,user:&str,text:&str)->OriginalPage {
        let mut end=text.len().min(4);while !text.is_char_boundary(end) {end-=1;}
        OriginalPage {session_id:context.session.clone(),original:original(user,text),offset:0,next_offset:end,
            text:text[..end].into(),complete:end==text.len()}
    }
    fn runtime()->Runtime {
        let session=fresh_chat_session(Provider::Claude,TerminalId::new(),"/project".into(),&Account::system(Provider::Claude),false).unwrap();
        let handle=Arc::new(ClientHandle::fixture(session.clone(),Snapshot {ready:true,session_id:Some("confirmed-session".into()),
            capabilities:chat::Capabilities {original_retry:true,..Default::default()},..Snapshot::default()}));
        Runtime::new(Target {card:CardId::new(),generation:1},session,Connection::Live(handle),true,crate::settings::EditorSyntaxTheme::default())
    }
    #[test]
    fn cold_paged_out_latest_uses_authoritative_metadata_without_retaining_input() {
        let mut runtime=runtime();runtime.sync_original();
        assert!(runtime.snapshot.items.is_empty());
        let context=Context::current(&runtime).unwrap();
        let id=runtime.recovery.originals.query_id_for_test();
        assert!(runtime.recovery.originals.accept(runtime.target,id,&context).is_some());
        assert!(runtime.recovery.originals.metadata(&context,page(&context,"native-latest","A long accepted prompt")));
        assert_eq!(runtime.original_retry_user(),Some("native-latest"));
        assert!(runtime.recovery.submitted.is_none());
        assert!(runtime.recovery.originals.query.is_none());
        Arc::make_mut(&mut runtime.snapshot).capabilities.original_retry=false;
        assert!(runtime.original_retry_user().is_none(),"providers without atomic exact retry hide the action");
    }
    #[test]
    fn late_queries_cannot_cross_actor_session_turn_user_or_eligibility_revision() {
        for change in 0..6 {
            let mut runtime=runtime();runtime.sync_original();
            let old=Context::current(&runtime).unwrap();let old_id=runtime.recovery.originals.query_id_for_test();
            match change {
                0=>{
                    let handle=Arc::new(ClientHandle::fixture(runtime.session.clone(),(*runtime.snapshot).clone()));
                    runtime.recovery.connection_replaced();runtime.connection=Connection::Live(handle);
                }
                1=>Arc::make_mut(&mut runtime.snapshot).session_id=Some("replacement-session".into()),
                2=>Arc::make_mut(&mut runtime.snapshot).turn_generation+=1,
                3=>Arc::make_mut(&mut runtime.snapshot).items.push(Arc::new(chat::Item {id:"new-unannotated-user".into(),role:chat::Role::User,
                    started_at:None,at:None,content:ItemContent::Text {text:"new input".into(),thinking:false,phase:chat::Phase::Final}})),
                4=>Arc::make_mut(&mut runtime.snapshot).original_revision+=1,
                _=>runtime.target.generation+=1,
            }
            runtime.sync_original();let current=Context::current(&runtime).unwrap();
            assert_ne!(old,current);
            assert!(runtime.recovery.originals.accept(runtime.target,old_id,&current).is_none());
            assert!(runtime.original_retry_user().is_none(),"new unannotated input never falls back to an old original");
            assert_ne!(runtime.recovery.originals.query_id_for_test(),old_id);
        }
    }
    #[test]
    fn metadata_rejects_wrong_session_inexact_settings_and_partial_shape() {
        let runtime=runtime();let context=Context::current(&runtime).unwrap();
        for change in 0..4 {
            let mut state=State::default();let mut page=page(&context,"native-user","accepted prompt");
            match change {
                0=>page.session_id="other-session".into(),
                1=>page.original.settings.model=chat::annotations::Setting::Default,
                2=>page.next_offset+=1,
                _=>page.complete=true,
            }
            assert!(!state.metadata(&context,page));assert!(state.metadata.is_none());
        }
    }
    #[test]
    fn hidden_eviction_and_connection_replacement_reject_old_attempts() {
        let mut runtime=runtime();runtime.sync_original();
        let id=runtime.recovery.originals.query_id_for_test();let context=Context::current(&runtime).unwrap();
        runtime.visible.store(false,Ordering::Relaxed);runtime.sync_original();
        assert!(runtime.recovery.originals.query.is_none());
        assert!(runtime.recovery.originals.accept(runtime.target,id,&context).is_none());
        runtime.visible.store(true,Ordering::Relaxed);runtime.sync_original();
        assert_ne!(runtime.recovery.originals.query_id_for_test(),id);
    }
    #[test]
    fn full_edit_uses_immutable_attachment_bytes_but_displays_original_filename() {
        let mut original=original("native-user","accepted original");
        original.attachments.push(chat::annotations::OriginalAttachment {original_path:"/pictures/architecture.png".into(),
            path:"/private/capture/00000000-0000-4000-8000-000000000000.png".into(),bytes:50,sha256:[3;32]});
        let prompt=turn_actions::Prompt::from_original(chat::original::OriginalText {session_id:"confirmed-session".into(),original,
            text:"accepted original".into()}).unwrap();
        let mut runtime=runtime();runtime.load_recovery_prompt(prompt,"Editing starts a new turn.");
        let attachment=&runtime.attachments[0];
        assert_eq!(attachment.path,PathBuf::from("/private/capture/00000000-0000-4000-8000-000000000000.png"));
        assert_eq!(runtime.attachment_display_path(attachment),Path::new("/pictures/architecture.png"));
        assert_eq!(runtime.composer.text(),"accepted original");
    }
    #[test]
    fn queued_full_reply_holds_reader_admission_until_consumed_or_discarded() {
        let permit=Arc::new(tokio::sync::Semaphore::new(1));
        let original=original("native-user","accepted original");
        let reply=Reply(Arc::new(Mutex::new(Some(Output {
            value:Ok(Value::Text(chat::original::OriginalText {session_id:"confirmed-session".into(),original,text:"accepted original".into()})),
            _permit:permit.clone().try_acquire_owned().unwrap(),
        }))));
        assert_eq!(permit.available_permits(),0);let clone=reply.clone();drop(reply);
        assert_eq!(permit.available_permits(),0);drop(clone);assert_eq!(permit.available_permits(),1);
    }
    #[test]
    fn deleting_an_alias_retires_origin_stashes_and_late_reads_but_preserves_typed_drafts() {
        let mut app=crate::app::tests::context_inspector_test_app(false);
        let mut runtime=runtime();runtime.sync_original();let target=runtime.target;
        let first=Account::managed(Provider::Claude,uuid::Uuid::new_v4(),"/same/physical-home".into()).id();
        let alias=Account::managed(Provider::Claude,uuid::Uuid::new_v4(),"/same/physical-home".into()).id();
        assert_ne!(first,alias);
        let context=Context::current(&runtime).unwrap();
        let original=original("native-user","private accepted original");
        let mut prompt=turn_actions::Prompt::from_original(chat::original::OriginalText {session_id:context.session.clone(),
            original:original.clone(),text:"private accepted original".into()}).unwrap();
        prompt.account_hint=Some(first.clone());
        let prompt=Arc::new(prompt);let weak=Arc::downgrade(&prompt);
        runtime.recovery.prepared_prompt=Some(prompt);
        runtime.recovery.originals.account_hint=Some(first.clone());
        runtime.replace_draft(text_editor::Content::with_text("independently typed draft"));
        runtime.attachments=vec![Attachment {path:"/user/draft.png".into()}];
        let handle=runtime.handle().cloned().unwrap();
        runtime.recovery.originals.begin(target,context.clone(),handle,Some("native-user".into()),Purpose::Edit {branch:None});
        let id=runtime.recovery.originals.query_id_for_test();
        let reply=Reply::fixture(Ok(Value::Text(chat::original::OriginalText {session_id:context.session,
            original,text:"private accepted original".into()})));
        let result_owner=Arc::downgrade(&reply.0);
        app.cards.insert(target.card,CardRuntime::Chat(Box::new(runtime)));
        app.invalidate_originals_for_account(&alias);
        assert!(weak.upgrade().is_none(),"a different registry alias cannot keep origin-derived plaintext alive");
        assert_eq!(app.chat_ui.original_epoch,1);
        drop(app.original_ready(target,id,Some(first),0,reply));
        assert!(result_owner.upgrade().is_none(),"late full result releases its actual payload owner");
        let runtime=app.chat_runtime_mut(target).unwrap();
        assert!(runtime.recovery.originals.metadata.is_none());
        assert!(runtime.recovery.prepared_prompt.is_none());
        assert_eq!(runtime.composer.text(),"independently typed draft");
        assert_eq!(runtime.attachments,vec![Attachment {path:"/user/draft.png".into()}]);
    }

    #[test]
    fn edited_send_keeps_private_dispatch_bytes_and_carries_only_display_hint() {
        let mut runtime=runtime();
        runtime.recovery.attachment_labels.insert("/private/capture.png".into(),"/other-account/original.png".into());
        let command=Command::Prompt {text:"edited user intent".into(),attachments:vec![Attachment {path:"/private/capture.png".into()}],mode:PromptMode::Send};
        assert_eq!(runtime.prompt_attachment_names(command),Command::PromptNamed {text:"edited user intent".into(),
            attachments:vec![chat::NamedAttachment {path:"/private/capture.png".into(),display_path:Some("/other-account/original.png".into())}],mode:PromptMode::Send});
        let ordinary=Command::Prompt {text:"new ordinary intent".into(),attachments:vec![Attachment {path:"/user/new.png".into()}],mode:PromptMode::Send};
        assert_eq!(runtime.prompt_attachment_names(ordinary.clone()),ordinary);
    }

}
