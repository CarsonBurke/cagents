use super::*;
use crate::agent::{Activity, Provider};
use crate::chat::client::{ClientHandle, Wake};
use crate::chat::{self, Attachment, Command, ItemContent, PromptMode, Snapshot};
use base64::Engine as _;
use cosmic::widget::{markdown, text_editor};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

mod view;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Target {
    pub card: CardId,
    pub generation: u64,
}

#[derive(Clone)]
pub struct Ready(Arc<Mutex<Option<Result<Arc<ClientHandle>, String>>>>);
impl std::fmt::Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ChatReady")
    }
}

#[derive(Debug, Clone)]
pub enum CopyPart {
    Text,
    Input,
    Output,
    Code(usize),
}

#[derive(Clone)]
pub enum Action {
    Switch(CardId),
    BindingInput(CardId, String),
    BindingPreview(CardId),
    BindingPreviewed(Target, Result<chat::BindingPreview, String>),
    BindingConfirm(CardId),
    BindingCancel(CardId),
    Ready(Target, Ready),
    Wake(Target, Wake),
    UpdatesAcknowledged(Target, Result<(), String>),
    Reconnect(Target),
    Composer(Target, text_editor::Action),
    Submit(Target, PromptMode),
    AppendProviderDraft(Target),
    Command(Target, Command),
    CommandFinished(Target, u64, Result<(), String>),
    Toggle(Target, String),
    Raw(Target, String),
    Copy(Target, String, CopyPart),
    OpenUrl(String),
    Attach(Target),
    Attached(Target, Result<Option<Vec<PathBuf>>, String>),
    RemoveAttachment(Target, usize),
    Model(Target, String),
    Reasoning(Target, String, String),
    PermissionMode(Target, String),
    Field(Target, String, String, String),
    Choice(Target, String, String, String),
    Answer(Target, String, String),
    Slash(Target, String),
    HandoffFinished(Target, Result<(), String>),
    CloseFinished(Target, Result<(), String>),
    UrlFinished(Result<(), String>),
}

impl std::fmt::Debug for Action {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // App-message diagnostics must never expose secret answers, composer
        // text, attachment paths, or provider authorization URLs.
        f.debug_tuple("ChatAction")
            .field(&std::mem::discriminant(self))
            .finish()
    }
}

impl Action {
    pub(super) fn allowed_while_modal(&self) -> bool {
        matches!(
            self,
            Self::Ready(..)
                | Self::Wake(..)
                | Self::UpdatesAcknowledged(..)
                | Self::CommandFinished(..)
                | Self::Attached(..)
                | Self::HandoffFinished(..)
                | Self::CloseFinished(..)
                | Self::UrlFinished(..)
                | Self::BindingPreviewed(..)
        )
    }

    fn mutation_target(&self) -> Option<Target> {
        match self {
            Self::Composer(target, action) if action.is_edit() => Some(*target),
            Self::Reconnect(target)
            | Self::Submit(target, _)
            | Self::AppendProviderDraft(target)
            | Self::Command(target, _)
            | Self::Attach(target)
            | Self::Attached(target, _)
            | Self::RemoveAttachment(target, _)
            | Self::Model(target, _)
            | Self::Reasoning(target, _, _)
            | Self::PermissionMode(target, _)
            | Self::Field(target, _, _, _)
            | Self::Choice(target, _, _, _)
            | Self::Answer(target, _, _)
            | Self::Slash(target, _) => Some(*target),
            _ => None,
        }
    }
}

struct Attempt {
    target: Target,
    session: chat::Session,
}

struct Binding {
    target: Target,
    session: chat::Session,
    source: HostSessionProof,
    input: String,
    preview: Option<chat::BindingPreview>,
    loading: bool,
    error: Option<String>,
}

pub(super) struct State {
    sender: async_channel::Sender<(Target, Wake)>,
    receiver: async_channel::Receiver<(Target, Wake)>,
    generation: u64,
    attempts: HashMap<CardId, Attempt>,
    bindings: HashMap<CardId, Binding>,
    handoffs: HashSet<Target>,
    closing: HashSet<Target>,
    closed: HashSet<CardId>,
}
impl Default for State {
    fn default() -> Self {
        let (sender, receiver) = async_channel::unbounded();
        Self {
            sender,
            receiver,
            generation: 0,
            attempts: HashMap::new(),
            handoffs: HashSet::new(),
            bindings: HashMap::new(),
            closing: HashSet::new(),
            closed: HashSet::new(),
        }
    }
}

impl State {
    pub(super) fn transition_pending(&self) -> bool {
        !self.attempts.is_empty()
            || !self.handoffs.is_empty()
            || !self.closing.is_empty()
            || !self.bindings.is_empty()
    }
}

struct CachedItem {
    content: ItemContent,
    markdown: Vec<markdown::Item>,
    codes: Vec<String>,
    image: Option<iced::widget::image::Handle>,
}
impl CachedItem {
    fn new(content: &ItemContent) -> Self {
        let mut codes = Vec::new();
        let markdown = if let ItemContent::Text { text, .. } = content {
            let mut fence: Option<(char, usize)> = None;
            let mut code = String::new();
            for line in text.lines() {
                let trimmed = line.trim_start();
                let marker = trimmed.chars().next().unwrap_or(' ');
                let length = trimmed
                    .chars()
                    .take_while(|character| *character == marker)
                    .count();
                if matches!(marker, '`' | '~') && length >= 3 {
                    match fence {
                        None => {
                            fence = Some((marker, length));
                            continue;
                        }
                        Some((opening, count)) if opening == marker && length >= count => {
                            codes.push(std::mem::take(&mut code));
                            fence = None;
                            continue;
                        }
                        _ => {}
                    }
                }
                if fence.is_some() {
                    code.push_str(line);
                    code.push('\n');
                }
            }
            if !code.is_empty() {
                codes.push(code);
            }
            markdown::parse(text).collect()
        } else {
            Vec::new()
        };
        let image = match content {
            ItemContent::Image { path, .. } if Path::new(path).is_absolute() => {
                Some(iced::widget::image::Handle::from_path(path))
            }
            ItemContent::Image { path, .. }
                if path.len() <= 24 * 1024 * 1024 && path.starts_with("data:image/") =>
            {
                path.split_once(";base64,")
                    .and_then(|(_, encoded)| {
                        base64::engine::general_purpose::STANDARD
                            .decode(encoded)
                            .ok()
                    })
                    .map(iced::widget::image::Handle::from_bytes)
            }
            _ => None,
        };
        Self {
            content: content.clone(),
            markdown,
            codes,
            image,
        }
    }
}

struct PendingCommand {
    id: u64,
    draft_revision: Option<u64>,
    request: Option<String>,
    permission_mode: Option<String>,
}
pub(super) struct Runtime {
    pub(super) target: Target,
    pub(super) session: chat::Session,
    handle: Arc<ClientHandle>,
    snapshot: Arc<Snapshot>,
    cache: HashMap<String, CachedItem>,
    composer: text_editor::Content<cosmic::Renderer>,
    revision: u64,
    attachments: Vec<Attachment>,
    attachment_chooser_pending: bool,
    expanded: HashSet<String>,
    raw: HashSet<String>,
    answers: HashMap<(String, String), Vec<String>>,
    pending: Option<PendingCommand>,
    next_command: u64,
    disconnected: bool,
    visible: AtomicBool,
    error: Option<String>,
    permission_mode: Option<String>,
}
impl Runtime {
    fn new(
        target: Target,
        session: chat::Session,
        handle: Arc<ClientHandle>,
        visible: bool,
    ) -> Self {
        let mut runtime = Self {
            target,
            session,
            snapshot: Arc::new(Snapshot::default()),
            handle,
            cache: HashMap::new(),
            composer: text_editor::Content::new(),
            revision: 0,
            attachments: Vec::new(),
            attachment_chooser_pending: false,
            expanded: HashSet::new(),
            raw: HashSet::new(),
            answers: HashMap::new(),
            pending: None,
            next_command: 0,
            disconnected: false,
            visible: AtomicBool::new(visible),
            error: None,
            permission_mode: None,
        };
        runtime.refresh();
        runtime
    }
    fn refresh(&mut self) {
        if !self.visible.load(Ordering::Relaxed) {
            return;
        }
        let snapshot = self.handle.snapshot();
        if self.snapshot.generation == snapshot.generation && Arc::ptr_eq(&self.snapshot, &snapshot)
        {
            return;
        }
        let live_ids: HashSet<&str> = snapshot.items.iter().map(|item| item.id.as_str()).collect();
        self.cache.retain(|id, _| live_ids.contains(id.as_str()));
        for item in &snapshot.items {
            if self
                .cache
                .get(&item.id)
                .is_none_or(|cached| cached.content != item.content)
            {
                self.cache
                    .insert(item.id.clone(), CachedItem::new(&item.content));
            }
        }
        self.answers
            .retain(|(id, _), _| snapshot.requests.iter().any(|request| &request.id == id));
        for request in &snapshot.requests {
            for field in &request.fields {
                self.answers
                    .entry((request.id.clone(), field.id.clone()))
                    .or_insert_with(|| {
                        if field.initial.is_empty() {
                            Vec::new()
                        } else {
                            vec![field.initial.clone()]
                        }
                    });
            }
        }
        if snapshot.composer != self.snapshot.composer
            && self.composer.text().trim().is_empty()
            && let Some(text) = &snapshot.composer
        {
            self.composer = text_editor::Content::with_text(text);
            self.revision = self.revision.wrapping_add(1);
        }
        if snapshot.session_id.is_some() || self.snapshot.session_id.is_some() {
            self.session.launch.session_id = snapshot.session_id.clone();
        }
        self.snapshot = snapshot;
    }
    pub(super) fn set_visible(&self, visible: bool) -> Result<(), String> {
        self.visible.store(visible, Ordering::Relaxed);
        self.handle.set_visible(visible)
    }
    fn show(&mut self, visible: bool) {
        if let Err(error) = self.set_visible(visible) {
            self.error = Some(error);
        }
        if visible {
            self.refresh();
        }
    }
    pub(super) fn transition_blocker(&self) -> Option<&'static str> {
        if self.attachment_chooser_pending {
            Some("Finish or cancel the file chooser before leaving the chat.")
        } else if self.pending.is_some() {
            Some("Wait for the pending chat command to finish before leaving the chat.")
        } else if !self.composer.is_empty()
            || !self.attachments.is_empty()
            || self
                .answers
                .values()
                .any(|answers| answers.iter().any(|answer| !answer.is_empty()))
        {
            Some(
                "Submit unsent chat input and attachments, or copy and clear the input and remove attachments before leaving the chat.",
            )
        } else {
            None
        }
    }
}

#[derive(Clone)]
struct Source(async_channel::Receiver<(Target, Wake)>);
impl PartialEq for Source {
    fn eq(&self, other: &Self) -> bool {
        self.0.same_channel(&other.0)
    }
}
impl Eq for Source {}
impl std::hash::Hash for Source {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        std::any::TypeId::of::<Self>().hash(state);
    }
}
fn stream(source: &Source) -> Pin<Box<dyn iced::futures::Stream<Item = (Target, Wake)> + Send>> {
    Box::pin(iced::futures::stream::unfold(
        source.0.clone(),
        |receiver| async move { receiver.recv().await.ok().map(|wake| (wake, receiver)) },
    ))
}

fn observed_launch(session: &TerminalSession) -> Option<LaunchRecipe> {
    session
        .resume_hint
        .as_deref()
        .and_then(crate::model::agent_recovery::observed_launch_from_hint)
        .or_else(|| Provider::from_program(&session.launch.program).map(|_| session.launch.clone()))
}

pub(super) fn conversion(session: &TerminalSession) -> Result<chat::Session, String> {
    let recipe = session
        .agent_resume_recipe()
        .or_else(|| observed_launch(session))
        .ok_or_else(|| {
            "No supported foreground agent is known. Focus the running agent terminal and retry."
                .to_owned()
        })?;
    let provider = Provider::from_program(&recipe.program)
        .ok_or("This terminal is not a supported agent session.")?;
    let identity_flag = provider.resume_flag();
    let session_id = session.agent_resume_recipe().and_then(|recipe| {
        recipe
            .arguments
            .windows(2)
            .find(|pair| pair[0] == identity_flag)
            .map(|pair| pair[1].clone())
    });
    Ok(chat::Session {
        id: session.id,
        launch: chat::Launch {
            provider,
            recipe,
            session_id,
        },
        proof: None,
        terminal: Some(session.clone()),
    })
}

fn return_terminal(session: &chat::Session) -> Result<TerminalSession, String> {
    session.launch.session_id.as_ref().ok_or_else(|| "Waiting for the provider's exact conversation identity. Return is unavailable until it is known.".to_owned())?;
    if session.launch.provider == Provider::Codex
        && let Some(source) = &session.terminal
        && source.continuity.live_proof().is_some()
    {
        return Ok(source.clone());
    }
    let recipe = session
        .launch
        .terminal_recipe()
        .ok_or_else(|| "Cannot form a safe exact-session terminal resume command.".to_owned())?;
    let id = TerminalId::new();
    let mut terminal = session.terminal.clone().unwrap_or_else(|| TerminalSession {
        id,
        launch: recipe.clone(),
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: recipe.clone(),
            last_exit: None,
        },
        resume_hint: None,
        transcript_path: None,
    });
    terminal.id = id;
    terminal.launch = recipe.clone();
    terminal.resume_hint = crate::model::agent_recovery::hint_from_launch(&recipe);
    terminal.continuity = TerminalContinuity::RelaunchAvailable {
        recipe,
        last_exit: None,
    };
    Ok(terminal)
}

impl Cagents {
    pub(super) fn chat_subscription(&self) -> Subscription<Message> {
        Subscription::run_with(Source(self.chat_ui.receiver.clone()), stream)
            .map(|(target, wake)| Message::ChatUi(Action::Wake(target, wake)))
    }
    pub(super) fn chat_forget(&mut self, card: CardId) {
        self.chat_ui.attempts.remove(&card);
        self.chat_ui.bindings.remove(&card);
        self.chat_ui.handoffs.retain(|target| target.card != card);
        self.chat_ui.closing.retain(|target| target.card != card);
        self.observe_agent_activity_for_card(card, None);
    }
    pub(super) fn chat_start_pending(&self, card: CardId) -> bool {
        self.chat_ui.attempts.contains_key(&card)
            || self
                .chat_ui
                .handoffs
                .iter()
                .any(|target| target.card == card)
            || self
                .chat_ui
                .closing
                .iter()
                .any(|target| target.card == card)
    }
    pub(super) fn chat_switch_available(&self, card: CardId) -> bool {
        match self.card_state(card).map(|state| &state.kind) {
            Some(CardKind::Chat(_)) => true,
            Some(CardKind::Terminal(session)) => {
                session.agent_resume_recipe().is_some() || observed_launch(session).is_some()
            }
            _ => false,
        }
    }
    pub(super) fn chat_switch_label(&self, card: CardId) -> &'static str {
        if matches!(
            self.card_state(card).map(|state| &state.kind),
            Some(CardKind::Chat(_))
        ) {
            "Return to terminal"
        } else {
            "Switch to native chat"
        }
    }
    pub(super) fn chat_close_approved(&self, card: CardId) -> bool {
        self.chat_ui.closed.contains(&card)
    }
    pub(super) fn begin_chat_close(&mut self, card: CardId) -> Task<Message> {
        if self.chat_start_pending(card) {
            return Task::none();
        }
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return Task::none();
        };
        if runtime.attachment_chooser_pending {
            self.status = Some(TransientNotice::error(
                "Finish or cancel the file chooser before closing the chat.",
            ));
            return Task::none();
        }
        let target = runtime.target;
        let handle = runtime.handle.clone();
        self.chat_ui.closing.insert(target);
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || handle.close())
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::CloseFinished(target, result))
        })
    }
    pub(super) fn start_chat(
        &mut self,
        card: CardId,
        session: chat::Session,
        spawn: bool,
        source: Option<HostSessionProof>,
        terminate_source: bool,
    ) -> Task<Message> {
        self.chat_ui.generation = self.chat_ui.generation.wrapping_add(1).max(1);
        let selection = self
            .chat_ui
            .bindings
            .remove(&card)
            .and_then(|binding| binding.preview)
            .filter(|preview| session.launch.session_id.as_ref() == Some(&preview.session_id))
            .map(|preview| preview.token);
        let target = Target {
            card,
            generation: self.chat_ui.generation,
        };
        self.chat_ui.attempts.insert(
            card,
            Attempt {
                target,
                session: session.clone(),
            },
        );
        let sender = self.chat_ui.sender.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                ClientHandle::connect(
                    session,
                    spawn,
                    source,
                    terminate_source,
                    selection,
                    move |wake| {
                        let _ = sender.try_send((target, wake));
                    },
                )
                .map(Arc::new)
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::ChatUi(Action::Ready(
                target,
                Ready(Arc::new(Mutex::new(Some(result)))),
            ))
        })
    }
    pub(super) fn acknowledge_chat_updates(&mut self, card: CardId) -> Task<Message> {
        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
            return Task::none();
        };
        if !self
            .card_state(card)
            .is_some_and(|state| state.existence.is_open())
        {
            return Task::none();
        }
        let target = runtime.target;
        let handle = runtime.handle.clone();
        let epoch = handle.begin_acknowledgement();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || handle.acknowledge_updates(epoch))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::UpdatesAcknowledged(target, result))
        })
    }
    fn chat_runtime_mut(&mut self, target: Target) -> Option<&mut Runtime> {
        match self.cards.get_mut(&target.card) {
            Some(CardRuntime::Chat(runtime)) if runtime.target == target => Some(runtime),
            _ => None,
        }
    }
    fn chat_send(&mut self, target: Target, command: Command, clear_draft: bool) -> Task<Message> {
        if self.chat_start_pending(target.card) {
            return Task::none();
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if runtime.pending.is_some() || runtime.disconnected {
            return Task::none();
        }
        let request = if let Command::Answer {
            request_id, action, ..
        } = &command
        {
            if !runtime.snapshot.requests.iter().any(|request| {
                &request.id == request_id
                    && request.actions.iter().any(|choice| &choice.id == action)
            }) {
                return Task::none();
            }
            Some(request_id.clone())
        } else {
            None
        };
        runtime.next_command = runtime.next_command.wrapping_add(1);
        let id = runtime.next_command;
        let permission_mode = if let Command::SetPermissionMode { mode } = &command {
            Some(mode.clone())
        } else {
            None
        };
        runtime.pending = Some(PendingCommand {
            id,
            draft_revision: clear_draft.then_some(runtime.revision),
            request,
            permission_mode,
        });
        runtime.error = None;
        let handle = runtime.handle.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || handle.send(command))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::CommandFinished(target, id, result))
        })
    }
    pub(super) fn handle_chat_ui(&mut self, action: Action) -> Task<Message> {
        if let Some(target) = action.mutation_target()
            && (self.chat_start_pending(target.card) || self.chat_ui.closed.contains(&target.card))
        {
            return Task::none();
        }
        match action {
            Action::BindingInput(card, input) => {
                if let Some(binding) = self.chat_ui.bindings.get_mut(&card) {
                    binding.input = input;
                    binding.preview = None;
                    binding.error = None;
                    binding.loading = false;
                    self.chat_ui.generation = self.chat_ui.generation.wrapping_add(1).max(1);
                    binding.target.generation = self.chat_ui.generation;
                }
            }
            Action::BindingPreview(card) => {
                let Some(binding) = self.chat_ui.bindings.get_mut(&card) else {
                    return Task::none();
                };
                if binding.loading {
                    return Task::none();
                }
                let id = binding.input.trim();
                if id.len() != 36 || uuid::Uuid::parse_str(id).is_err() {
                    binding.error = Some("Enter the full conversation UUID shown by /status in the intended Codex CLI.".into());
                    return Task::none();
                }
                binding.session.launch.session_id = Some(id.to_owned());
                binding.preview = None;
                binding.error = None;
                binding.loading = true;
                let target = binding.target;
                let session = binding.session.clone();
                let source = binding.source.clone();
                return cosmic::task::future(async move {
                    let result =
                        tokio::task::spawn_blocking(move || ClientHandle::preview(session, source))
                            .await
                            .map_err(|error| error.to_string())
                            .and_then(|result| result);
                    Message::ChatUi(Action::BindingPreviewed(target, result))
                });
            }
            Action::BindingPreviewed(target, result) => {
                if let Some(binding) = self
                    .chat_ui
                    .bindings
                    .get_mut(&target.card)
                    .filter(|binding| binding.target == target)
                {
                    binding.loading = false;
                    match result {
                        Ok(preview) if binding.session.launch.session_id.as_ref() == Some(&preview.session_id) => binding.preview = Some(preview),
                        Ok(_) => binding.error = Some("The preview returned a different identity; no conversation was activated.".into()),
                        Err(error) => binding.error = Some(error),
                    }
                }
            }
            Action::BindingConfirm(card) => {
                let Some(binding) = self
                    .chat_ui
                    .bindings
                    .get(&card)
                    .filter(|binding| binding.preview.is_some() && !binding.loading)
                else {
                    return Task::none();
                };
                if !self.card_state(card).is_some_and(|state| matches!(&state.kind, CardKind::Terminal(terminal) if terminal.id == binding.session.id && terminal.continuity.live_proof().as_ref() == Some(&binding.source))) {
                    self.chat_ui.bindings.remove(&card);
                    self.status = Some(TransientNotice::error("The source terminal changed; choose its conversation again."));
                    return Task::none();
                }
                let session = binding.session.clone();
                let source = binding.source.clone();
                return self.start_chat(card, session, true, Some(source), false);
            }
            Action::BindingCancel(card) => {
                self.chat_ui.bindings.remove(&card);
            }
            Action::Switch(card) => {
                if self.chat_start_pending(card) {
                    return Task::none();
                }
                match self.card_state(card).map(|state| &state.kind) {
                    Some(CardKind::Terminal(terminal)) => {
                        let source = terminal.continuity.live_proof();
                        let session = match conversion(terminal) {
                            Ok(session) => session,
                            Err(error) => {
                                self.status = Some(TransientNotice::error(error));
                                return Task::none();
                            }
                        };
                        if source.is_none() {
                            self.status = Some(TransientNotice::error(
                                "Reconnect the original terminal before switching its active conversation.",
                            ));
                            return Task::none();
                        }
                        if session.launch.provider == Provider::Codex {
                            self.chat_ui.generation =
                                self.chat_ui.generation.wrapping_add(1).max(1);
                            let input = session.launch.session_id.clone().unwrap_or_default();
                            self.chat_ui.bindings.insert(
                                card,
                                Binding {
                                    target: Target {
                                        card,
                                        generation: self.chat_ui.generation,
                                    },
                                    session,
                                    source: source.expect("checked source"),
                                    input,
                                    preview: None,
                                    loading: false,
                                    error: None,
                                },
                            );
                            return match self.workspace.focus(card) {
                                Ok(update) => self.apply_viewport(update, true, false),
                                Err(error) => {
                                    self.status = Some(TransientNotice::error(error.to_string()));
                                    Task::none()
                                }
                            };
                        }
                        let terminate = session.launch.provider != Provider::Codex;
                        return self.start_chat(card, session, true, source, terminate);
                    }
                    Some(CardKind::Chat(_)) => {
                        let Some(CardRuntime::Chat(runtime)) = self.cards.get(&card) else {
                            return Task::none();
                        };
                        if let Some(reason) = runtime.transition_blocker() {
                            self.status = Some(TransientNotice::error(reason));
                            return Task::none();
                        }
                        let mut session = runtime.session.clone();
                        session.launch.session_id = runtime.handle.snapshot().session_id.clone();
                        let terminal = match return_terminal(&session) {
                            Ok(terminal) => terminal,
                            Err(error) => {
                                self.status = Some(TransientNotice::error(error));
                                return Task::none();
                            }
                        };
                        let target = runtime.target;
                        let handle = runtime.handle.clone();
                        let detach = session.launch.provider == Provider::Codex
                            && session
                                .terminal
                                .as_ref()
                                .is_some_and(|source| source.id == terminal.id);
                        self.chat_ui.handoffs.insert(target);
                        return cosmic::task::future(async move {
                            let result = if detach {
                                Ok(())
                            } else {
                                tokio::task::spawn_blocking(move || handle.close())
                                    .await
                                    .map_err(|error| error.to_string())
                                    .and_then(|result| result)
                            };
                            Message::ChatUi(Action::HandoffFinished(target, result))
                        });
                    }
                    _ => return Task::none(),
                }
            }
            Action::Ready(target, ready) => {
                let result = ready.0.lock().ok().and_then(|mut ready| ready.take());
                let Some(result) = result else {
                    return Task::none();
                };
                if !self
                    .chat_ui
                    .attempts
                    .get(&target.card)
                    .is_some_and(|attempt| attempt.target == target)
                {
                    return Task::none();
                }
                let attempt = self.chat_ui.attempts.remove(&target.card).unwrap();
                if !self.card_state(target.card).is_some_and(|card| {
                    card.existence.is_open()
                        && match &card.kind {
                            CardKind::Chat(session) => session.id == attempt.session.id,
                            CardKind::Terminal(session) => {
                                attempt.session.terminal.as_ref().is_some_and(|source| {
                                    source.id == session.id && source.launch == session.launch
                                })
                            }
                            _ => false,
                        }
                }) {
                    return Task::none();
                }
                match result {
                    Ok(handle) => {
                        let mut session = handle.session();
                        session.proof = handle.proof();
                        let activity = Activity {
                            provider: session.launch.provider,
                            state: handle.take_activity(),
                        };
                        handle.take_observed();
                        let visible = self.workspace.visibility(target.card)
                            == Some(VisibilityClass::Visible);
                        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&target.card) {
                            runtime.target = target;
                            runtime.handle = handle;
                            runtime.session = session.clone();
                            runtime.disconnected = false;
                            runtime.pending = None;
                            runtime.error = None;
                            runtime.show(visible);
                        } else {
                            let mut runtime =
                                Runtime::new(target, session.clone(), handle, visible);
                            runtime.show(visible);
                            if let Some(CardRuntime::Terminal(terminal)) = self
                                .cards
                                .insert(target.card, CardRuntime::Chat(Box::new(runtime)))
                            {
                                self.begin_terminal_teardown(terminal);
                            }
                        }
                        if let Some(CardRuntime::Chat(runtime)) = self.cards.get(&target.card) {
                            session = runtime.session.clone();
                        }
                        self.commit_chat_session(target.card, session);
                        self.observe_agent_activity_for_card(target.card, Some(activity));
                        return iced::widget::operation::focus(card_control_id(
                            target.card,
                            "chat-composer",
                        ))
                        .map(cosmic::Action::App);
                    }
                    Err(error) => {
                        if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&target.card) {
                            runtime.error = Some(error);
                            runtime.disconnected = true;
                            self.observe_agent_activity_for_card(target.card, None);
                        } else {
                            // Failed recognition/attachment must never replace or
                            // tear down the user's still-running terminal.
                            self.status = Some(TransientNotice::error(error));
                        }
                        return self.drain_terminal(target.card, 0);
                    }
                }
            }
            Action::UpdatesAcknowledged(target, result) => {
                if let Some(runtime) = self.chat_runtime_mut(target)
                    && let Err(error) = result
                {
                    runtime.error = Some(error);
                }
            }
            Action::Wake(target, wake) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                match wake {
                    Wake::Changed => {
                        runtime.refresh();
                        let activity = Activity {
                            provider: runtime.session.launch.provider,
                            state: runtime.handle.take_activity(),
                        };
                        let session = runtime.session.clone();
                        self.observe_agent_activity_for_card(target.card, Some(activity));
                        self.commit_chat_session(target.card, session);
                        return Task::none();
                    }
                    Wake::Observed => {
                        runtime.handle.take_observed();
                        return Task::none();
                    }
                    Wake::Activity => {
                        let activity = Activity {
                            provider: runtime.session.launch.provider,
                            state: runtime.handle.take_activity(),
                        };
                        self.observe_agent_activity_for_card(target.card, Some(activity));
                        return Task::none();
                    }
                    Wake::Disconnected(error) => {
                        runtime.disconnected = true;
                        runtime.error = Some(error);
                    }
                }
                let session = runtime.session.clone();
                self.commit_chat_session(target.card, session);
                self.observe_agent_activity_for_card(target.card, None);
            }
            Action::Reconnect(target) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if runtime.attachment_chooser_pending {
                    runtime.error = Some(
                        "Finish or cancel the file chooser before reconnecting the chat.".into(),
                    );
                    return Task::none();
                }
                let session = runtime.session.clone();
                return self.start_chat(target.card, session, false, None, false);
            }
            Action::Composer(target, action) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if action.is_edit() {
                        runtime.revision = runtime.revision.wrapping_add(1);
                    }
                    runtime.composer.perform(action);
                }
            }
            Action::AppendProviderDraft(target) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if let Some(provided) = &runtime.snapshot.composer {
                        let mut text = runtime.composer.text();
                        if !text.trim().is_empty() {
                            text.push('\n');
                        }
                        text.push_str(provided);
                        runtime.composer = text_editor::Content::with_text(&text);
                        runtime.revision = runtime.revision.wrapping_add(1);
                    }
                }
            }
            Action::Submit(target, mode) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if runtime.composer.text().trim().is_empty() && runtime.attachments.is_empty() {
                    return Task::none();
                }
                if (mode == PromptMode::Steer && !runtime.snapshot.capabilities.steer)
                    || (mode == PromptMode::Queue && !runtime.snapshot.capabilities.queue)
                    || (mode == PromptMode::Send && runtime.snapshot.busy)
                {
                    return Task::none();
                }
                let text = runtime.composer.text();
                let slash_name = text
                    .trim()
                    .strip_prefix('/')
                    .and_then(|command| command.split_whitespace().next());
                let command = if mode == PromptMode::Send
                    && slash_name.is_some_and(|name| {
                        runtime
                            .snapshot
                            .commands
                            .iter()
                            .any(|command| command.name.trim_start_matches('/') == name)
                    }) {
                    if !runtime.attachments.is_empty() {
                        runtime.error =
                            Some("Send attachments with a message, not a slash command.".into());
                        return Task::none();
                    }
                    Command::Slash {
                        command: text.trim().to_owned(),
                    }
                } else {
                    Command::Prompt {
                        text,
                        attachments: runtime.attachments.clone(),
                        mode,
                    }
                };
                return self.chat_send(target, command, true);
            }
            Action::Command(target, command) => return self.chat_send(target, command, false),
            Action::CommandFinished(target, id, result) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if !runtime
                    .pending
                    .as_ref()
                    .is_some_and(|pending| pending.id == id)
                {
                    return Task::none();
                }
                let pending = runtime.pending.take().unwrap();
                match result {
                    Ok(()) => {
                        if let Some(mode) = pending.permission_mode {
                            runtime.permission_mode = Some(mode);
                        }
                        if pending.draft_revision == Some(runtime.revision) {
                            runtime.composer = text_editor::Content::new();
                            runtime.attachments.clear();
                            runtime.revision = runtime.revision.wrapping_add(1);
                        }
                        if let Some(request) = pending.request {
                            runtime.answers.retain(|(id, _), _| id != &request);
                        }
                        runtime.refresh();
                    }
                    Err(error) => runtime.error = Some(error),
                }
            }
            Action::Toggle(target, id) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if !runtime.expanded.remove(&id) {
                        runtime.expanded.insert(id);
                    }
                }
            }
            Action::Raw(target, id) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if !runtime.raw.remove(&id) {
                        runtime.raw.insert(id);
                    }
                }
            }
            Action::Copy(target, id, part) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let Some(cached) = runtime.cache.get(&id) else {
                    return Task::none();
                };
                let text = match (part, &cached.content) {
                    (CopyPart::Text, ItemContent::Text { text, .. }) => Some(text),
                    (CopyPart::Input, ItemContent::Tool { input, .. }) => Some(input),
                    (CopyPart::Output, ItemContent::Tool { output, .. }) => Some(output),
                    (CopyPart::Code(index), _) => cached.codes.get(index),
                    _ => None,
                };
                if let Some(text) = text {
                    return iced::clipboard::write(text.clone()).map(cosmic::Action::App);
                }
            }
            Action::OpenUrl(raw) => {
                return cosmic::task::future(async move {
                    let result = match Url::parse(&raw) {
                        Ok(url) if matches!(url.scheme(), "https" | "http") => {
                            open_system_url(url).await
                        }
                        _ => Err(
                            "Only explicit HTTP and HTTPS links can be opened from chat.".into(),
                        ),
                    };
                    Message::ChatUi(Action::UrlFinished(result))
                });
            }
            Action::UrlFinished(result) => {
                if let Err(error) = result {
                    self.status = Some(TransientNotice::error(error));
                }
            }
            Action::Attach(target) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if !runtime.snapshot.capabilities.attachments || runtime.attachment_chooser_pending
                {
                    return Task::none();
                }
                runtime.attachment_chooser_pending = true;
                let directory = runtime.session.launch.recipe.working_directory.clone();
                return cosmic::task::future(async move {
                    let result = match file_chooser::open::Dialog::new()
                        .title("Attach files to chat")
                        .directory(directory)
                        .open_files()
                        .await
                    {
                        Ok(response) => Ok(Some(
                            response
                                .urls()
                                .iter()
                                .filter_map(|url| url.to_file_path().ok())
                                .collect(),
                        )),
                        Err(file_chooser::Error::Cancelled) => Ok(None),
                        Err(error) => Err(error.to_string()),
                    };
                    Message::ChatUi(Action::Attached(target, result))
                });
            }
            Action::Attached(target, result) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if !std::mem::take(&mut runtime.attachment_chooser_pending) {
                        return Task::none();
                    }
                    match result {
                        Ok(Some(paths)) => {
                            for path in paths {
                                if !runtime
                                    .attachments
                                    .iter()
                                    .any(|attachment| attachment.path == path)
                                {
                                    runtime.attachments.push(Attachment { path });
                                }
                            }
                            runtime.revision = runtime.revision.wrapping_add(1);
                        }
                        Ok(None) => {}
                        Err(error) => runtime.error = Some(error),
                    }
                }
            }
            Action::RemoveAttachment(target, index) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if index < runtime.attachments.len() {
                        runtime.attachments.remove(index);
                        runtime.revision = runtime.revision.wrapping_add(1);
                    }
                }
            }
            Action::Model(target, id) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let Some(model) = runtime.snapshot.models.iter().find(|model| model.id == id)
                else {
                    return Task::none();
                };
                let command = Command::SetModel {
                    model: model.id.clone(),
                    reasoning: runtime
                        .snapshot
                        .reasoning
                        .clone()
                        .filter(|reasoning| model.reasoning.contains(reasoning)),
                };
                return self.chat_send(target, command, false);
            }
            Action::Reasoning(target, model_id, reasoning) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let Some(model) = runtime.snapshot.models.iter().find(|model| {
                    model.id == model_id
                        && Some(&model.id) == runtime.snapshot.selected_model.as_ref()
                }) else {
                    return Task::none();
                };
                if !model.reasoning.contains(&reasoning) {
                    return Task::none();
                }
                let command = Command::SetModel {
                    model: model.id.clone(),
                    reasoning: Some(reasoning),
                };
                return self.chat_send(target, command, false);
            }
            Action::PermissionMode(target, mode) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if !runtime
                    .snapshot
                    .capabilities
                    .permission_modes
                    .contains(&mode)
                {
                    return Task::none();
                }
                return self.chat_send(target, Command::SetPermissionMode { mode }, false);
            }
            Action::Field(target, request, field, value) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if let Some(schema) = runtime
                        .snapshot
                        .requests
                        .iter()
                        .find(|candidate| candidate.id == request)
                        .and_then(|request| {
                            request
                                .fields
                                .iter()
                                .find(|candidate| candidate.id == field)
                        })
                    {
                        let values = runtime.answers.entry((request, field)).or_default();
                        if schema.multiple {
                            values.retain(|selected| {
                                schema.choices.iter().any(|choice| &choice.id == selected)
                            });
                        } else {
                            values.clear();
                        }
                        if !value.is_empty() {
                            values.push(value);
                        }
                    }
                }
            }
            Action::Choice(target, request, field, value) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if let Some(schema) = runtime
                        .snapshot
                        .requests
                        .iter()
                        .find(|candidate| candidate.id == request)
                        .and_then(|request| {
                            request
                                .fields
                                .iter()
                                .find(|candidate| candidate.id == field)
                        })
                    {
                        if schema.choices.iter().any(|choice| choice.id == value) {
                            let values = runtime.answers.entry((request, field)).or_default();
                            if schema.multiple {
                                if let Some(index) =
                                    values.iter().position(|selected| selected == &value)
                                {
                                    values.remove(index);
                                } else {
                                    values.push(value);
                                }
                            } else {
                                *values = vec![value];
                            }
                        }
                    }
                }
            }
            Action::Answer(target, request_id, action) => {
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                let Some(request) = runtime
                    .snapshot
                    .requests
                    .iter()
                    .find(|request| request.id == request_id)
                else {
                    return Task::none();
                };
                let answers = request
                    .fields
                    .iter()
                    .map(|field| chat::Answer {
                        field: field.id.clone(),
                        values: runtime
                            .answers
                            .get(&(request_id.clone(), field.id.clone()))
                            .cloned()
                            .unwrap_or_default(),
                    })
                    .collect();
                return self.chat_send(
                    target,
                    Command::Answer {
                        request_id,
                        action,
                        answers,
                    },
                    false,
                );
            }
            Action::Slash(target, name) => {
                if let Some(runtime) = self.chat_runtime_mut(target) {
                    if runtime
                        .snapshot
                        .commands
                        .iter()
                        .any(|command| command.name == name)
                    {
                        runtime.composer = text_editor::Content::with_text(&format!(
                            "/{} ",
                            name.trim_start_matches('/')
                        ));
                        runtime.revision = runtime.revision.wrapping_add(1);
                    }
                }
            }
            Action::HandoffFinished(target, result) => {
                if self.chat_runtime_mut(target).is_none() {
                    return Task::none();
                }
                if !self.chat_ui.handoffs.remove(&target) {
                    return Task::none();
                }
                if let Err(error) = result {
                    self.status = Some(TransientNotice::error(error));
                    return Task::none();
                }
                let Some(runtime) = self.chat_runtime_mut(target) else {
                    return Task::none();
                };
                if let Some(reason) = runtime.transition_blocker() {
                    self.status = Some(TransientNotice::error(reason));
                    return Task::none();
                }
                runtime.session.launch.session_id = runtime.handle.snapshot().session_id.clone();
                let session = runtime.session.clone();
                let terminal = return_terminal(&session);
                self.commit_chat_session(target.card, session);
                let terminal = match terminal {
                    Ok(terminal) => terminal,
                    Err(error) => {
                        self.status = Some(TransientNotice::error(error));
                        return Task::none();
                    }
                };
                self.chat_forget(target.card);
                return self.finish_chat_terminal_handoff(target.card, terminal);
            }
            Action::CloseFinished(target, result) => {
                if self.chat_runtime_mut(target).is_none() {
                    return Task::none();
                }
                if !self.chat_ui.closing.remove(&target) {
                    return Task::none();
                }
                if let Err(error) = result {
                    self.status = Some(TransientNotice::error(error));
                    return Task::none();
                }
                self.chat_ui.closed.insert(target.card);
                self.observe_agent_activity_for_card(target.card, None);
                return cosmic::task::message(Message::CommitClose(target.card));
            }
        }
        Task::none()
    }
}
