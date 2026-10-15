//! Prompt recovery keeps the exact submitted attachments and settings. Older
//! prompts may be edited only where their provider supplies a stable branch ID.
use super::*;

#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct Prompt {
    pub user_id: Option<String>,
    prior_user_id: Option<String>,
    pub text: String,
    pub attachments: Vec<Attachment>,
    pub attachment_labels: HashMap<PathBuf,PathBuf>,
    pub account_hint: Option<AccountId>,
    /// Retained history input, not an independently edited composer draft.
    pub origin_backend: bool,
    pub settings: Settings,
}

impl Prompt {
    pub(super) fn from_original(value:chat::original::OriginalText)->Option<Self> {
        use chat::annotations::Setting;
        if value.text.len()!=value.original.text_bytes || !value.original.settings.is_exact() {return None;}
        let original=value.original;
        let model=match original.settings.model {
            Setting::Value(model)=>Some((model,match original.settings.reasoning {Setting::Value(reasoning)=>Some(reasoning),Setting::Unsupported=>None,_=>return None})),
            Setting::Unsupported=>None,_=>return None,
        };
        let fast=match original.settings.fast {Setting::Value(value)=>Some(value),Setting::Unsupported=>None,_=>return None};
        let permission=match original.settings.permission {Setting::Value(value)=>Some(value),Setting::Unsupported=>None,_=>return None};
        let attachment_labels=original.attachments.iter().map(|attachment|(attachment.path.clone(),attachment.original_path.clone())).collect();
        let attachments=original.attachments.into_iter().map(|attachment|Attachment {path:attachment.path}).collect();
        Some(Self {user_id:Some(original.user_id),prior_user_id:None,text:value.text,attachments,attachment_labels,account_hint:None,origin_backend:true,
            settings:Settings {model,fast,permission,exact:true}})
    }
}

#[derive(Clone,Debug,PartialEq,Eq)]
pub(super) struct Settings {
    model:Option<(String,Option<String>)>,
    fast:Option<bool>,
    permission:Option<String>,
    exact:bool,
}
impl Settings {
    fn capture(snapshot:&Snapshot)->Self {
        Self {model:snapshot.selected_model.clone().map(|model|(model,snapshot.reasoning.clone())),
            fast:snapshot.fast_mode.as_ref().map(chat::FastMode::enabled),permission:snapshot.permission_mode.clone(),
            exact:(!snapshot.capabilities.model_selection || snapshot.selected_model.is_some())
                && (!snapshot.capabilities.fast_mode || snapshot.fast_mode.is_some())
                && (snapshot.capabilities.permission_modes.is_empty() || snapshot.permission_mode.is_some())}
    }
}

#[derive(Default)]
pub(super) struct State {
    pub submitted: Option<Prompt>,
    pub originals: originals::State,
    pub attachment_labels: HashMap<PathBuf,PathBuf>,
    /// Each failed generation can be retried once until provider output proves
    /// a new turn began. An ACK alone is not that proof.
    pub retried: Option<(u64, String)>,
    pub commands: std::collections::VecDeque<Command>,
    pub inflight: Option<u64>,
    pub notice: Option<String>,
    pub branches_requested: bool,
    pub replay_settings: Option<Settings>,
    pub last_user: Option<String>,
    pub last_assistant: Option<String>,
    pub assistant_user: Option<String>,
    pub recoverable_users: HashSet<String>,
    pub branch_draft: Option<Prompt>,
    pub branch_session: Option<String>,
    pub prepared_prompt: Option<Arc<Prompt>>,
}

impl State {
    /// Provider item IDs and pending command IDs belong to one connection.
    /// An explicitly prepared composer draft is reader input, not replay proof.
    pub fn connection_replaced(&mut self) {
        let prepared_prompt=self.prepared_prompt.take();
        let attachment_labels=std::mem::take(&mut self.attachment_labels);
        *self=Self {prepared_prompt,attachment_labels,..Self::default()};
        if self.prepared_prompt.is_some() {
            self.notice=Some("A prepared prompt is available after you send or clear your current draft.".into());
        }
    }
    pub fn submitting(&mut self, snapshot: &Snapshot, command: &Command) {
        let Command::Prompt { text, attachments, mode: PromptMode::Send } = command else { return; };
        let settings=self.replay_settings.clone().unwrap_or_else(||Settings::capture(snapshot));
        self.submitted = Some(Prompt {
            user_id: None,
            prior_user_id: latest_user(snapshot).map(|item| item.id.clone()),
            text: text.clone(), attachments: attachments.clone(), attachment_labels:self.attachment_labels.clone(),account_hint:None,origin_backend:false,
            settings,
        });
    }

    pub fn observe(&mut self, snapshot: &Snapshot) {
        self.last_user=None;self.last_assistant=None;self.assistant_user=None;self.recoverable_users.clear();
        let mut prompt=None;
        for item in &snapshot.items {
            if item.role != chat::Role::User {prompt=None;}
            if let ItemContent::Image {path,..}=&item.content
                && (path.is_empty() || path.starts_with("data:") || path.contains("://"))
                && let Some(id)=&prompt {self.recoverable_users.remove(id);}
            if matches!(item.content,ItemContent::Text {thinking:false,..}) {
                match item.role {
                    chat::Role::User=>{self.last_user=Some(item.id.clone());self.recoverable_users.insert(item.id.clone());prompt=Some(item.id.clone());},
                    chat::Role::Assistant=>{self.last_assistant=Some(item.id.clone());self.assistant_user=self.last_user.clone();},
                    _=>{},
                }
            }
        }
        if let Some(submitted) = &mut self.submitted
            && submitted.user_id.is_none()
            && let Some(user) = latest_user(snapshot)
            && submitted.prior_user_id.as_deref() != Some(user.id.as_str())
            && matches!(&user.content, ItemContent::Text { text, .. } if text.trim_end() == submitted.text.trim_end()) {
            submitted.user_id = Some(user.id.clone());
        }
        for item in &snapshot.items {
            if let Some((root,_))=item.id.rsplit_once(":native-part:") {
                self.recoverable_users.remove(root);
                self.recoverable_users.remove(&item.id);
            }
        }
        if let Some(id)=self.submitted.as_ref().and_then(|prompt|prompt.user_id.as_ref()) {
            self.recoverable_users.insert(id.clone());
        }
        if self.retried.as_ref().is_some_and(|(generation, _)| *generation != snapshot.turn_generation) {
            self.retried = None;
        }
    }

    pub fn for_user(&self, snapshot: &Snapshot, id: &str) -> Option<Prompt> {
        if let Some(submitted) = self.submitted.as_ref().filter(|prompt| prompt.user_id.as_deref() == Some(id)) {
            let mut prompt=submitted.clone();prompt.origin_backend=true;
            return Some(prompt);
        }
        let index = snapshot.items.iter().position(|item| item.id == id && item.role == chat::Role::User)?;
        let ItemContent::Text { text, thinking:false, .. } = &snapshot.items[index].content else { return None; };
        let mut attachments = Vec::new();
        if id.contains(":native-part:") {return None;}
        for item in snapshot.items[index+1..].iter().take_while(|item| item.role == chat::Role::User) {
            if matches!(item.content,ItemContent::Text {thinking:false,..}) {
                if prompt_source(&item.id) == prompt_source(id) || item.id.starts_with(&format!("{id}:native-part:")) {
                    // Extra text parts or paged fragments cannot be omitted
                    // from an edit pretending to contain the whole prompt.
                    return None;
                }
                break;
            }
            match &item.content {
                ItemContent::Image { path, .. } if !path.is_empty() && !path.starts_with("data:") && !path.contains("://") => {
                    attachments.push(Attachment { path:path.into() });
                }
                ItemContent::Image { .. } => return None,
                _ => {},
            }
        }
        Some(Prompt { user_id:Some(id.into()), prior_user_id:None, text:text.clone(), attachments,attachment_labels:HashMap::new(),account_hint:None,origin_backend:true,
            settings:Settings::capture(snapshot) })
    }

    pub fn begin_retry(&mut self, snapshot: &Snapshot, user_id: &str) -> bool {
        if !self.retry_available(snapshot,user_id) || !self.commands.is_empty() || self.inflight.is_some() { return false; }
        let Some(prompt) = self.for_user(snapshot,user_id) else { return false; };
        if let Some((model,reasoning)) = &prompt.settings.model
            && (snapshot.selected_model.as_ref() != Some(model) || snapshot.reasoning != *reasoning) {
            if !snapshot.capabilities.model_selection { return false; }
            self.commands.push_back(Command::SetModel {model:model.clone(),reasoning:reasoning.clone()});
        }
        if let Some(enabled) = prompt.settings.fast
            && snapshot.fast_mode.as_ref().is_none_or(|mode|mode.enabled() != enabled) {
            if !snapshot.capabilities.fast_mode { self.commands.clear(); return false; }
            self.commands.push_back(Command::SetFastMode {enabled});
        }
        if let Some(mode)=&prompt.settings.permission && snapshot.permission_mode.as_ref() != Some(mode) {
            if !snapshot.capabilities.permission_modes.contains(mode) {self.commands.clear();return false;}
            self.commands.push_back(Command::SetPermissionMode {mode:mode.clone()});
        }
        self.replay_settings=Some(prompt.settings.clone());
        self.retried = Some((snapshot.turn_generation,user_id.into()));
        self.commands.push_back(Command::Prompt {text:prompt.text,attachments:prompt.attachments,mode:PromptMode::Send});
        true
    }

    pub fn retry_available(&self, snapshot: &Snapshot, user_id: &str) -> bool {
        self.commands.is_empty() && self.inflight.is_none()
            && self.retried.as_ref() != Some(&(snapshot.turn_generation, user_id.to_owned()))
            && latest_user(snapshot).is_some_and(|item| item.id == user_id)
            && self.recoverable_users.contains(user_id)
            && self.submitted.as_ref().is_some_and(|prompt|prompt.user_id.as_deref() == Some(user_id) && prompt.settings.exact)
    }
}

fn prompt_source(id:&str)->&str {
    if let Some((root,_))=id.split_once("/input/") {return root;}
    id.rsplit_once(':').filter(|(_,part)|part.parse::<usize>().is_ok()).map_or(id,|(root,_)|root)
}

pub(super) fn latest_user(snapshot: &Snapshot) -> Option<&chat::Item> {
    snapshot.items.iter().rev().map(Arc::as_ref).find(|item| item.role == chat::Role::User
        && matches!(item.content,ItemContent::Text { thinking:false,.. }))
}

pub(super) fn branch_point(provider: Provider, snapshot: &Snapshot, item: &chat::Item) -> Option<String> {
    if !snapshot.capabilities.branch { return None; }
    match provider {
        Provider::Claude => None,
        Provider::Codex => {
            let (turn,_) = item.id.split_once('/')?;
            snapshot.branch_points.iter().find(|point| point.id == turn).map(|point|point.id.clone())
        }
        Provider::Omp => {
            let ItemContent::Text { text,.. } = &item.content else { return None; };
            let label = crate::account::clean_label(text)?;
            let mut matches = snapshot.branch_points.iter().filter(|point|point.label == label);
            let point = matches.next()?;
            matches.next().is_none().then(||point.id.clone())
        }
    }
}

pub(super) fn retriable(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    if ["unauthorized","forbidden","invalid api key","authentication","permission denied","invalid request","context length"]
        .iter().any(|needle|lower.contains(needle)) { return false; }
    lower.split(|character:char|!character.is_ascii_digit()).any(|code|matches!(code,"429"|"500"|"502"|"503"|"504"|"529"))
        || ["overloaded","rate limit","temporarily unavailable","service unavailable","connection reset","connection closed",
            "connection error","failed to connect","broken pipe","unexpected eof","timed out","timeout","network error"]
            .iter().any(|needle|lower.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn user(id:&str,text:&str)->Arc<chat::Item> { Arc::new(chat::Item {
        id:id.into(),role:chat::Role::User,started_at:None,at:None,
        content:ItemContent::Text {text:text.into(),thinking:false,phase:chat::Phase::Final},
    }) }
    #[test]
    fn transient_errors_are_distinct_from_auth_and_request_failures() {
        assert!(retriable("API Error: 529 Overloaded... try again."));
        assert!(retriable("HTTP 429 rate limit"));
        assert!(retriable("network error: connection reset"));
        assert!(!retriable("HTTP 401: invalid API key"));
        assert!(!retriable("400 invalid request"));
        assert!(!retriable("context length exceeded"));
    }
    #[test]
    fn retry_keeps_prompt_attachments_and_settings_and_binds_repeated_text_to_new_id() {
        let mut snapshot=Snapshot { selected_model:Some("model".into()),reasoning:Some("low".into()),
            items:vec![user("old","prompt")],..Snapshot::default() };
        let command=Command::Prompt {text:"prompt".into(),attachments:vec![Attachment {path:"image.png".into()}],mode:PromptMode::Send};
        let mut state=State::default();state.submitting(&snapshot,&command);
        state.observe(&snapshot);assert!(state.submitted.as_ref().unwrap().user_id.is_none());
        snapshot.items.push(user("new","prompt"));state.observe(&snapshot);
        snapshot.selected_model=Some("changed".into());
        let retry=state.for_user(&snapshot,"new").unwrap();
        assert_eq!(retry.text,"prompt");assert_eq!(retry.attachments,vec![Attachment {path:"image.png".into()}]);
        assert_eq!(retry.settings.model,Some(("model".into(),Some("low".into()))));
        assert!(state.retry_available(&snapshot,"new"));
        state.retried=Some((snapshot.turn_generation,"new".into()));
        assert!(!state.retry_available(&snapshot,"new"));
        snapshot.turn_generation+=1;state.observe(&snapshot);assert!(state.retry_available(&snapshot,"new"));
    }

    #[test]
    fn historical_edits_do_not_take_attachments_from_the_next_prompt() {
        let image=|id:&str,path:&str|Arc::new(chat::Item {id:id.into(),role:chat::Role::User,started_at:None,at:None,
            content:ItemContent::Image {path:path.into(),description:String::new()}});
        let snapshot=Snapshot {items:vec![user("first","one"),image("image1","first.png"),
            user("second","two"),image("image2","second.png")],..Default::default()};
        let mut state=State::default();state.observe(&snapshot);
        let edit=state.for_user(&snapshot,"first").unwrap();
        assert_eq!(edit.attachments,vec![Attachment {path:"first.png".into()}]);
        assert!(!state.retry_available(&snapshot,"second"),"restored turns have no original settings proof");
    }
    #[test]
    fn branch_actions_require_provider_identity_and_unique_omp_prompt() {
        let item=user("turn/item","prompt");
        let mut snapshot=Snapshot { capabilities:chat::Capabilities {branch:true,..Default::default()},
            branch_points:vec![chat::BranchPoint {id:"turn".into(),label:"prompt".into()}],..Snapshot::default() };
        assert_eq!(branch_point(Provider::Codex,&snapshot,&item),Some("turn".into()));
        assert_eq!(branch_point(Provider::Claude,&snapshot,&item),None);
        assert_eq!(branch_point(Provider::Omp,&snapshot,&item),Some("turn".into()));
        snapshot.branch_points.push(chat::BranchPoint {id:"other".into(),label:"prompt".into()});
        assert_eq!(branch_point(Provider::Omp,&snapshot,&item),None);
        snapshot.capabilities.branch=false;assert_eq!(branch_point(Provider::Codex,&snapshot,&item),None);
    }
    #[test]
    fn unknown_original_supported_settings_do_not_offer_inexact_retry() {
        let mut snapshot=Snapshot {items:vec![user("u","prompt")],..Snapshot::default()};
        snapshot.capabilities.model_selection=true;
        let mut state=State::default();state.submitting(&Snapshot {capabilities:snapshot.capabilities.clone(),..Snapshot::default()},
            &Command::Prompt {text:"prompt".into(),attachments:vec![],mode:PromptMode::Send});
        state.observe(&snapshot);
        assert!(!state.retry_available(&snapshot,"u"));
        snapshot.selected_model=Some("later-model".into());
        assert!(!state.retry_available(&snapshot,"u"),"later model observation is not original metadata");
    }

    #[test]
    fn a_replacement_connection_cannot_reuse_local_prompt_or_pending_command_proof() {
        let mut state=State::default();
        let command=Command::Prompt {text:"prompt".into(),attachments:Vec::new(),mode:PromptMode::Send};
        state.submitting(&Snapshot::default(),&command);
        let snapshot=Snapshot {items:vec![user("u","prompt")],..Snapshot::default()};
        state.observe(&snapshot);
        assert!(state.begin_retry(&snapshot,"u"));
        state.inflight=Some(1);
        state.prepared_prompt=state.submitted.clone().map(Arc::new);
        state.connection_replaced();
        assert!(state.submitted.is_none());assert!(state.commands.is_empty());assert!(state.inflight.is_none());
        assert!(state.retried.is_none());assert!(state.prepared_prompt.is_some());
        state.observe(&snapshot);
        assert!(!state.retry_available(&snapshot,"u"),"even reused user IDs need new qualified metadata");
    }

}

impl Runtime {
    /// Resident recovery data is deliberately separate from hydrated transcript
    /// bodies. Attachment paths are retained; file bytes remain on disk.
    pub(super) fn recovery_retained_bytes(&self)->(usize,usize) {
        let prompts=self.recovery.submitted.iter().chain(self.recovery.branch_draft.iter()).chain(self.recovery.prepared_prompt.iter().map(Arc::as_ref));
        let (mut text,mut paths)=prompts.fold((0,0),|(text,paths),prompt| (
            text+prompt.text.len(),paths+prompt.attachments.iter().map(|attachment|attachment.path.as_os_str().len()).sum::<usize>()
                +prompt.attachment_labels.iter().map(|(path,label)|path.as_os_str().len()+label.as_os_str().len()).sum::<usize>()));
        paths+=self.recovery.attachment_labels.iter().map(|(path,label)|path.as_os_str().len()+label.as_os_str().len()).sum::<usize>();
        for command in &self.recovery.commands {
            if let Command::Prompt {text:body,attachments,..}=command {
                text+=body.len();paths+=attachments.iter().map(|attachment|attachment.path.as_os_str().len()).sum::<usize>();
            }
        }
        (text,paths)
    }

    pub(super) fn recovery_available(&self) -> bool {
        self.snapshot.ready && !self.snapshot.busy && !self.snapshot.exited
            && self.snapshot.read_only_reason.is_none() && !self.snapshot.has_later
            && !self.disconnected && self.pending.is_none() && self.queued.is_none()
            && self.selected_subagent.is_none() && self.recovery.commands.is_empty()
            && self.snapshot.queued_messages.is_empty() && self.followups.is_empty()
    }

    pub(super) fn retry_user_for_error(&self, message: &str, anchor: Option<&str>) -> Option<String> {
        if !self.recovery_available() || !retriable(message)
            || self.snapshot.error.as_deref() != Some(message) { return None; }
        let user=self.original_retry_user()?;
        if self.recovery.retried.as_ref()==Some(&(self.snapshot.turn_generation,user.into())) {return None;}
        if let Some(anchor)=anchor {
            // Old equal error strings cannot acquire a current-turn Retry.
            let latest=self.failures.iter().rfind(|(_,error)|error==message)?;
            if latest.0.as_deref()!=Some(anchor) {return None;}
        }
        Some(user.into())
    }

    pub(super) fn edit_supported(&self,item:&chat::Item)->bool {
        item.role == chat::Role::User && !self.recovery.originals.editing()
            && (self.session.launch.provider == Provider::Claude
                || branch_point(self.session.launch.provider,&self.snapshot,item).is_some())
    }
    pub(super) fn recovery_composer_empty(&self)->bool {
        self.composer.is_empty() && self.attachments.is_empty()
            && self.pastes_pending == 0 && self.image_pastes_saving == 0 && !self.attachment_chooser_pending
    }
    pub(super) fn can_edit_prompt(&self, item:&chat::Item) -> bool {
        self.recovery_available() && self.recovery_composer_empty() && self.edit_supported(item)
    }

    pub(super) fn retry_for_assistant(&self,item:&chat::Item)->Option<String> {
        if !self.recovery_available() || item.role != chat::Role::Assistant
            || self.recovery.last_assistant.as_deref() != Some(item.id.as_str()) {return None;}
        let user=self.original_retry_user()?;
        (self.recovery.retried.as_ref()!=Some(&(self.snapshot.turn_generation,user.into()))).then(||user.into())
    }

    fn edit_prompt(&self, id: &str) -> Option<(Prompt, Option<String>)> {
        if !self.recovery_available() || !self.recovery_composer_empty() { return None; }
        let prompt=self.recovery.for_user(&self.snapshot,id)?;
        let provider=self.session.launch.provider;
        let branch=if provider == Provider::Claude { None } else {
            let item=self.snapshot.items.iter().find(|item|item.id == id)?;
            Some(branch_point(provider,&self.snapshot,item)?)
        };
        Some((prompt,branch))
    }

    pub(super) fn attachment_display_path<'a>(&'a self,attachment:&'a Attachment)->&'a Path {
        self.recovery.attachment_labels.get(&attachment.path).map_or(attachment.path.as_path(),PathBuf::as_path)
    }
    pub(super) fn load_recovery_prompt(&mut self, prompt: Prompt, notice: &str) {
        if !self.recovery_composer_empty() {
            self.recovery.prepared_prompt=Some(Arc::new(prompt));
            self.recovery.notice=Some("The branch is ready. Send or clear your current draft before loading its prompt.".into());
            return;
        }
        self.replace_draft(text_editor::Content::with_text(&prompt.text));
        self.recovery.attachment_labels=prompt.attachment_labels;
        self.replace_attachments(prompt.attachments);
        self.recovery.notice=Some(notice.into());
    }
}

impl Cagents {
    pub(super) fn dispatch_chat_recovery(&mut self, target: Target) -> Task<Message> {
        let Some(runtime)=self.chat_runtime_mut(target) else { return Task::none(); };
        if runtime.pending.is_some() || runtime.disconnected || runtime.snapshot.busy
            || runtime.snapshot.exited || !runtime.snapshot.ready || runtime.queued.is_some()
            || runtime.recovery.inflight.is_some() { return Task::none(); }
        let prime=runtime.recovery.commands.is_empty() && runtime.snapshot.capabilities.branch
            && runtime.snapshot.branch_points.is_empty() && !runtime.recovery.branches_requested;
        let command=if prime {
            runtime.recovery.branches_requested=true;
            Command::ListBranches
        } else if let Some(command)=runtime.recovery.commands.front().cloned() {command}
        else {return Task::none();};
        let task=self.chat_send(target,command,false);
        if let Some(runtime)=self.chat_runtime_mut(target) {
            if !prime {runtime.recovery.inflight=runtime.pending.as_ref().map(|pending|pending.id);}
            else if runtime.pending.is_none() {runtime.recovery.branches_requested=false;}
        }
        task
    }

    pub(super) fn retry_chat_turn(&mut self, target: Target, user_id: &str) -> Task<Message> {
        self.begin_original_retry(target,user_id)
    }

    pub(super) fn edit_chat_prompt(&mut self, target: Target, id: &str) -> Task<Message> {
        self.begin_original_edit(target,id)
    }
}

/// Copy displayed text without Markdown styling. Lists and tables retain
/// their readable structure, while code is copied exactly without fences.
pub(super) fn plain_text(source: &str) -> String {
    fn text(value:&markdown::Text,style:markdown::Style)->String {
        value.spans(style).iter().map(|span|span.text.as_ref()).collect()
    }
    fn items(values:&[markdown::Item],style:markdown::Style)->String {
        values.iter().map(|item| match item {
            markdown::Item::Paragraph(value)|markdown::Item::Heading(_,value)=>text(value,style),
            markdown::Item::CodeBlock {code,..}=>code.trim_end_matches('\n').into(),
            markdown::Item::Quote(values)=>items(values,style),
            markdown::Item::Rule=>String::new(),
            markdown::Item::Image {alt,url,..}=>format!("{} ({url})",text(alt,style)),
            markdown::Item::List {start,bullets}=>bullets.iter().enumerate().map(|(index,bullet)| {
                let (values,task)=match bullet {
                    markdown::Bullet::Point {items}=>(items,None),
                    markdown::Bullet::Task {items,done}=>(items,Some(*done)),
                };
                let marker=match task {Some(true)=>"[x]".into(),Some(false)=>"[ ]".into(),None=>start.map(|start|format!("{}.",start+index as u64)).unwrap_or_else(||"•".into())};
                format!("{marker} {}",items(values,style))
            }).collect::<Vec<_>>().join("\n"),
            markdown::Item::Table {columns,rows}=>std::iter::once(columns.iter().map(|column|items(&column.header,style)).collect::<Vec<_>>().join("\t"))
                .chain(rows.iter().map(|row|row.cells.iter().map(|cell|items(cell,style)).collect::<Vec<_>>().join("\t")))
                .collect::<Vec<_>>().join("\n"),
        }).filter(|text|!text.is_empty()).collect::<Vec<_>>().join("\n\n")
    }
    let source=match markdown_source::Source::default().advance(source,false) {
        markdown_source::Step::Append(text)|markdown_source::Step::Restart(text)=>text,
    };
    let content=markdown::Content::parse(&source);
    items(content.items(),design::markdown_settings(&cosmic::Theme::dark(),design::TEXT_PROSE).style)
}

#[cfg(test)]
mod presentation_tests {
    use super::*;
    #[test]
    fn plain_copy_preserves_content_and_code_without_markdown_styling() {
        let copied=plain_text("# Title\n\n**Bold** and [label](https://example.com)\n\n```rust\nlet x = 1;\n```\n\n- one\n- two");
        assert!(copied.contains("Title"));assert!(copied.contains("Bold and label"));
        assert!(copied.contains("let x = 1;"));assert!(copied.contains("• one"));
        assert!(!copied.contains("**"));assert!(!copied.contains("```"));
    }
    #[test]
    fn replay_restores_settings_before_dispatching_the_exact_prompt() {
        let mut state=State::default();
        let mut snapshot=Snapshot {selected_model:Some("first".into()),reasoning:Some("low".into()),
            capabilities:chat::Capabilities {model_selection:true,fast_mode:true,..Default::default()},
            fast_mode:Some(chat::FastMode {state:chat::FastModeState::On,disabled_reason:None}),..Default::default()};
        let prompt=Command::Prompt {text:"/literal prompt".into(),attachments:vec![Attachment {path:"image.png".into()}],mode:PromptMode::Send};
        state.submitting(&snapshot,&prompt);
        snapshot.items.push(Arc::new(chat::Item {id:"user".into(),role:chat::Role::User,started_at:None,at:None,
            content:ItemContent::Text {text:"/literal prompt".into(),thinking:false,phase:chat::Phase::Final}}));
        state.observe(&snapshot);
        snapshot.selected_model=Some("second".into());snapshot.reasoning=Some("high".into());
        snapshot.fast_mode.as_mut().unwrap().state=chat::FastModeState::Off;
        assert!(state.begin_retry(&snapshot,"user"));
        assert_eq!(state.commands.iter().cloned().collect::<Vec<_>>(),vec![
            Command::SetModel {model:"first".into(),reasoning:Some("low".into())},
            Command::SetFastMode {enabled:true},prompt]);
        assert!(!state.begin_retry(&snapshot,"user"));
    }
}
