//! Logical message Copy reads canonical source pages off the UI thread.
use super::*;

pub(super) const UNAVAILABLE: &str = chat::TEXT_COPY_UNAVAILABLE;

/// A details copy whose diagnostic changed between the click and the read.
const DETAILS_CHANGED: &str = "These details changed while copying. Copy them again.";

/// How long a copy's control shows that it copied. Tests let it lapse at
/// once, so a copy's task settles as soon as its work does.
const COPIED_FOR: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_millis(1500)
};
/// How long it shows why it didn't: long enough to read the sentence.
const REFUSED_FOR: Duration = if cfg!(test) {
    Duration::ZERO
} else {
    Duration::from_secs(6)
};

/// The copy control a copy answers at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum CopyKey {
    /// An item's own Copy, in the row under it.
    Item(String),
    /// The Copy on one block of a step: its input, its output or a change.
    Block(String, CopyPart),
    /// A failure card's Copy details.
    Failure(String),
}

impl CopyKey {
    /// The control a copy of item `id`, or of its `part`, was made from.
    pub(super) fn of(id: String, part: Option<&CopyPart>) -> Self {
        match part {
            None | Some(CopyPart::Text) => Self::Item(id),
            Some(part) => Self::Block(id, part.clone()),
        }
    }

    /// Whether this is the control for item `id`, or for its `part`.
    pub(super) fn is(&self, id: &str, part: Option<&CopyPart>) -> bool {
        match (self, part) {
            (Self::Item(item), None | Some(CopyPart::Text)) => item == id,
            (Self::Block(item, block), Some(part)) => item == id && block == part,
            _ => false,
        }
    }

    /// The item whose control this is, or "" for a failure card's.
    pub(super) fn item(&self) -> &str {
        match self {
            Self::Item(id) | Self::Block(id, _) => id,
            Self::Failure(_) => "",
        }
    }
}

/// What a copy control shows: ready to copy, that it just copied, or why it
/// didn't. Pressing it again is how a refused copy is retried.
#[derive(Debug, Clone, Default)]
pub(super) enum CopyState {
    #[default]
    Ready,
    Copied,
    Refused(Arc<str>),
}

impl CopyState {
    pub(super) fn icon(&self) -> Icon {
        match self {
            Self::Ready => Icon::Copy,
            Self::Copied => Icon::Check,
            Self::Refused(_) => Icon::Warning,
        }
    }

    pub(super) fn refusal(&self) -> Option<&Arc<str>> {
        match self {
            Self::Refused(refusal) => Some(refusal),
            _ => None,
        }
    }
}

/// What the last copy said at the control it was made from: that it copied,
/// or why it didn't. A copy is the reader's own action, so its outcome is told
/// there for a moment and is never the conversation's state.
#[derive(Debug, Clone)]
pub(super) struct CopyFeedback {
    pub(super) key: CopyKey,
    /// Why it didn't copy, or `None` once it did.
    pub(super) refusal: Option<Arc<str>>,
    pub(super) at: Instant,
}

impl Runtime {
    /// Says at `key` that its copy went through, or with `refusal` why it
    /// didn't, until a later copy says something else or the moment passes.
    pub(super) fn tell_copy(&mut self, key: CopyKey, refusal: Option<String>) -> Task<Message> {
        let at = Instant::now();
        let shown = if refusal.is_some() {
            REFUSED_FOR
        } else {
            COPIED_FOR
        };
        self.copy_feedback = Some(CopyFeedback {
            key,
            refusal: refusal.map(Arc::from),
            at,
        });
        let target = self.target;
        cosmic::task::future(async move {
            tokio::time::sleep(shown).await;
            Message::ChatUi(Action::ClearCopied(target, at))
        })
    }

    /// What the last copy says at `key`, if it was made there.
    pub(super) fn copy_feedback_at(&self, key: &CopyKey) -> Option<&CopyFeedback> {
        self.copy_feedback
            .as_ref()
            .filter(|feedback| feedback.key == *key)
    }

    /// What the Copy for item `id`, or for its `part`, shows.
    pub(super) fn copy_state(&self, id: &str, part: Option<&CopyPart>) -> CopyState {
        self.copy_state_where(|key| key.is(id, part))
    }

    /// What failure `key`'s Copy details shows.
    pub(super) fn details_copy_state(&self, key: &str) -> CopyState {
        self.copy_state_where(
            |copied| matches!(copied, CopyKey::Failure(failure) if failure == key),
        )
    }

    fn copy_state_where(&self, at: impl Fn(&CopyKey) -> bool) -> CopyState {
        match &self.copy_feedback {
            Some(feedback) if at(&feedback.key) => match &feedback.refusal {
                Some(refusal) => CopyState::Refused(refusal.clone()),
                None => CopyState::Copied,
            },
            _ => CopyState::Ready,
        }
    }
}

/// All production UI surfaces share one clipboard intent context. A request
/// owns this context so async task polling never depends on thread-local state.
#[derive(Clone, Default)]
struct ClipboardContext(Arc<AtomicU64>);

impl ClipboardContext {
    fn advance(&self) -> u64 {
        self.0.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
    }
    fn is_current(&self, intent: u64) -> bool {
        self.0.load(Ordering::Relaxed) == intent
    }
}

fn current_context() -> ClipboardContext {
    #[cfg(test)]
    if let Some(context) = TEST_CONTEXT.with(|slot| slot.borrow().clone()) {
        return context;
    }
    static PROCESS: std::sync::OnceLock<ClipboardContext> = std::sync::OnceLock::new();
    PROCESS.get_or_init(ClipboardContext::default).clone()
}

/// A newer copy (including widget-owned selection) supersedes an in-flight read.
pub(super) fn invalidate() -> u64 {
    current_context().advance()
}

#[cfg(test)]
thread_local! {
    static TEST_CONTEXT: std::cell::RefCell<Option<ClipboardContext>> = const { std::cell::RefCell::new(None) };
}

/// Scope only UI event entrypoints; asynchronous work captures the owned
/// context. The guard cannot move to a different thread and restores nesting.
#[cfg(test)]
pub(super) struct TestContext {
    previous: Option<ClipboardContext>,
    _thread: std::marker::PhantomData<std::rc::Rc<()>>,
}
#[cfg(test)]
pub(super) fn isolated_context() -> TestContext {
    TestContext {
        previous: TEST_CONTEXT.with(|slot| slot.replace(Some(ClipboardContext::default()))),
        _thread: std::marker::PhantomData,
    }
}
#[cfg(test)]
impl Drop for TestContext {
    fn drop(&mut self) {
        TEST_CONTEXT.with(|slot| {
            slot.replace(self.previous.take());
        });
    }
}

#[derive(Clone)]
pub struct Request {
    context: ClipboardContext,
    intent: u64,
    handle: Arc<ClientHandle>,
    source: chat::TextCopySource,
    conversation_revision: u64,
    /// The part of a step this copies, or `None` for a whole message.
    part: Option<CopyPart>,
    /// What the clipboard holds ahead of the source's text, such as the
    /// command a step's whole copy starts with.
    prefix: String,
    /// Whether this already read the item again after its text changed.
    retried: bool,
}

#[derive(Clone)]
pub struct FailureRequest {
    context: ClipboardContext,
    intent: u64,
    handle: Arc<ClientHandle>,
    scope: Option<String>,
    plan: failure::CopyPlan,
}

#[cfg(test)]
impl FailureRequest {
    pub(super) fn for_test(
        handle: Arc<ClientHandle>,
        scope: Option<String>,
        plan: failure::CopyPlan,
    ) -> Self {
        let context = current_context();
        let intent = context.advance();
        Self {
            context,
            intent,
            handle,
            scope,
            plan,
        }
    }
}

/// The item of `snapshot` whose whole text `source` copies. Its text may
/// have grown since `source` was captured.
fn item_with_source<'a>(
    snapshot: &'a Snapshot,
    source: &chat::TextCopySource,
) -> Option<&'a chat::Item> {
    let (items, sources) = match &source.scope {
        chat::TextCopyScope::Conversation => {
            (&snapshot.items, &snapshot.text_copy_sources.conversation)
        }
        chat::TextCopyScope::Subagent { id } => (
            &snapshot
                .subagents
                .iter()
                .find(|agent| agent.id == *id)?
                .items,
            snapshot.text_copy_sources.children.get(id)?,
        ),
    };
    let index = sources.iter().position(|candidate| {
        candidate
            .as_ref()
            .is_some_and(|current| compatible(source, current))
    })?;
    items.get(index).map(AsRef::as_ref)
}

impl Request {
    fn read(self, target: Target, id: String) -> Task<Message> {
        let handle = self.handle.clone();
        let source = self.source.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || handle.copy_text(source))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::MessageCopied(target, id, self, result))
        })
    }

    /// This copy of item `id` again, from what `snapshot` now holds for it,
    /// or `None` once the item or its whole text is gone.
    fn reread(&self, snapshot: &Snapshot, id: &str) -> Option<Self> {
        let (items, sources, index) = match &self.source.scope {
            // The store keeps a conversation item's cache and token when it
            // rewrites the item. Its ID is no key: a long message's parts and
            // a provider item can share one.
            chat::TextCopyScope::Conversation => {
                let sources = &snapshot.text_copy_sources.conversation;
                let index = sources.iter().position(|source| {
                    source.as_ref().is_some_and(|source| {
                        source.cache == self.source.cache && source.token == self.source.token
                    })
                })?;
                (&snapshot.items, sources, index)
            }
            // A rewritten child item gets a new source, so it is found by ID.
            chat::TextCopyScope::Subagent { id: child } => {
                let items = &snapshot
                    .subagents
                    .iter()
                    .find(|agent| agent.id == *child)?
                    .items;
                let index = items.iter().position(|item| item.id == id)?;
                (
                    items,
                    snapshot.text_copy_sources.children.get(child)?,
                    index,
                )
            }
        };
        let source = sources
            .get(index)?
            .clone()
            .filter(|source| source.scope == self.source.scope)?;
        let prefix = match &self.part {
            Some(part) => step_copy_prefix(part, &items[index].content)?,
            None => String::new(),
        };
        Some(Self {
            source,
            conversation_revision: snapshot.text_copy_sources.conversation_revision,
            prefix,
            retried: true,
            ..self.clone()
        })
    }

    fn snapshot_is_current(&self, snapshot: &Snapshot) -> bool {
        match &self.source.scope {
            // Paging and verified appends do not revoke the captured prefix.
            chat::TextCopyScope::Conversation => {
                snapshot.text_copy_sources.conversation_revision == self.conversation_revision
            }
            // Child authority belongs to its bounded resident items.
            chat::TextCopyScope::Subagent { id } => snapshot
                .text_copy_sources
                .children
                .get(id)
                .is_some_and(|sources| {
                    sources
                        .iter()
                        .flatten()
                        .any(|source| compatible(&self.source, source))
                }),
        }
    }
}

#[cfg(test)]
impl Request {
    pub(super) fn for_test(handle: Arc<ClientHandle>, source: chat::TextCopySource) -> Self {
        let conversation_revision = handle.snapshot().text_copy_sources.conversation_revision;
        let context = current_context();
        let intent = context.advance();
        Self {
            context,
            intent,
            handle,
            source,
            conversation_revision,
            part: None,
            prefix: String::new(),
            retried: false,
        }
    }
    /// The same request as the copy of a step's `part`, with the prefix
    /// that part puts ahead of its source.
    pub(super) fn for_step(mut self, part: CopyPart, prefix: &str) -> Self {
        self.part = Some(part);
        self.prefix = prefix.into();
        self
    }
}

impl Runtime {
    pub(super) fn message_copy_source(&self, item: &chat::Item) -> Option<chat::TextCopySource> {
        let (items, sources) = match self.selected_subagent.as_deref() {
            Some(child) => (
                &self
                    .snapshot
                    .subagents
                    .iter()
                    .find(|agent| agent.id == child)?
                    .items,
                self.snapshot.text_copy_sources.children.get(child)?,
            ),
            None => (
                &self.snapshot.items,
                &self.snapshot.text_copy_sources.conversation,
            ),
        };
        let index = items
            .iter()
            .position(|candidate| std::ptr::eq(candidate.as_ref(), item))?;
        sources
            .get(index)?
            .clone()
            .filter(|source| match &source.scope {
                chat::TextCopyScope::Conversation => self.selected_subagent.is_none(),
                chat::TextCopyScope::Subagent { id } => self.selected_subagent.as_ref() == Some(id),
            })
    }
    /// The source of a step's whole output when its row shows only an
    /// excerpt of it, which the store does for a long output. An output too
    /// large for the clipboard has none: its Copy, and the note beneath it,
    /// are about the excerpt it shows.
    pub(super) fn excerpted_output_source(
        &self,
        item: &chat::Item,
    ) -> Option<chat::TextCopySource> {
        let output = match &item.content {
            ItemContent::Command { output, .. } | ItemContent::Tool { output, .. } => output,
            _ => return None,
        };
        self.message_copy_source(item).filter(|source| {
            source.bytes > output.len() as u64 && source.bytes <= chat::TEXT_COPY_LIMIT_BYTES as u64
        })
    }
    fn contains_copy_source(&self, captured: &chat::TextCopySource) -> bool {
        let sources = match (&captured.scope, self.selected_subagent.as_deref()) {
            (chat::TextCopyScope::Conversation, None) => {
                Some(&self.snapshot.text_copy_sources.conversation)
            }
            (chat::TextCopyScope::Subagent { id }, Some(selected)) if id == selected => {
                self.snapshot.text_copy_sources.children.get(id)
            }
            _ => None,
        };
        sources.is_some_and(|sources| {
            sources
                .iter()
                .flatten()
                .any(|current| compatible(captured, current))
        })
    }
}

impl Cagents {
    pub(super) fn start_failure_copy(&mut self, target: Target, key: String) -> Task<Message> {
        let context = current_context();
        let intent = context.advance();
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let plan = match runtime.failure_copy_plan(&key) {
            Ok(Some(plan)) => plan,
            Ok(None) => return Task::none(),
            Err(error) => return runtime.tell_copy(CopyKey::Failure(key), Some(error)),
        };
        let Some(handle) = runtime.handle().cloned() else {
            return runtime.tell_copy(
                CopyKey::Failure(key),
                Some(
                    "Original diagnostic unavailable: reconnect this conversation before copying"
                        .into(),
                ),
            );
        };
        let request = FailureRequest {
            context: context.clone(),
            intent,
            handle: handle.clone(),
            scope: runtime.selected_subagent.clone(),
            plan: plan.clone(),
        };
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                plan.read(&handle, || context.is_current(intent))
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::ChatUi(Action::FailureCopied(target, key, request, result))
        })
    }

    pub(super) fn finish_failure_copy(
        &mut self,
        target: Target,
        key: String,
        request: FailureRequest,
        result: Result<String, String>,
    ) -> Task<Message> {
        if !request.context.is_current(request.intent)
            || self.workspace.focused() != Some(target.card)
        {
            return Task::none();
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if runtime.selected_subagent != request.scope
            || !runtime
                .handle()
                .is_some_and(|handle| Arc::ptr_eq(handle, &request.handle))
        {
            return Task::none();
        }
        let details = CopyKey::Failure(key.clone());
        match result {
            Ok(text) => {
                if runtime.failure_copy_plan(&key).ok().flatten().as_ref() != Some(&request.plan)
                    || !request.plan.is_current(&request.handle.snapshot())
                {
                    return runtime.tell_copy(details, Some(DETAILS_CHANGED.into()));
                }
                let context = request.context.clone();
                let intent = request.intent;
                let (plan, handle) = (request.plan.clone(), request.handle.clone());
                Task::batch([
                    clipboard_write(
                        context,
                        intent,
                        text,
                        move || plan.is_current(&handle.snapshot()),
                        move |text| {
                            Message::ChatUi(Action::FailureCopied(target, key, request, Ok(text)))
                        },
                    ),
                    runtime.tell_copy(details, None),
                ])
            }
            Err(error) => {
                let refusal = if runtime.failure_plan_still_shown(&key, &request.plan)
                    && request
                        .plan
                        .is_current_or_withdrawn(&request.handle.snapshot())
                {
                    const PREFIX: &str = "Original diagnostic unavailable: ";
                    if error.starts_with(PREFIX) {
                        error
                    } else {
                        format!("{PREFIX}{error}")
                    }
                } else {
                    DETAILS_CHANGED.into()
                };
                runtime.tell_copy(details, Some(refusal))
            }
        }
    }

    /// Copies the whole text of item `id`, or of its step `part`, from
    /// `source`.
    pub(super) fn start_message_copy(
        &mut self,
        target: Target,
        id: String,
        part: Option<CopyPart>,
        source: Option<chat::TextCopySource>,
    ) -> Task<Message> {
        let context = current_context();
        let intent = context.advance();
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let key = CopyKey::of(id.clone(), part.as_ref());
        let Some(source) = source.filter(|source| runtime.contains_copy_source(source)) else {
            return runtime.tell_copy(key, Some(UNAVAILABLE.into()));
        };
        let prefix = match &part {
            // By its source: a step's ID may also be a long message's
            // generated part ID.
            Some(part) => match item_with_source(&runtime.snapshot, &source)
                .and_then(|item| step_copy_prefix(part, &item.content))
            {
                Some(prefix) => prefix,
                None => return runtime.tell_copy(key, Some(UNAVAILABLE.into())),
            },
            None => String::new(),
        };
        let Some(handle) = runtime.handle().cloned() else {
            return runtime.tell_copy(
                key,
                Some("Reconnect this conversation before copying all of this text.".into()),
            );
        };
        let request = Request {
            context,
            intent,
            handle: handle.clone(),
            source: source.clone(),
            conversation_revision: runtime.snapshot.text_copy_sources.conversation_revision,
            part,
            prefix,
            retried: false,
        };
        request.read(target, id)
    }

    pub(super) fn finish_message_copy(
        &mut self,
        target: Target,
        id: String,
        request: Request,
        result: Result<String, String>,
    ) -> Task<Message> {
        if !request.context.is_current(request.intent)
            || self.workspace.focused() != Some(target.card)
        {
            return Task::none();
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(handle) = runtime
            .handle()
            .filter(|handle| Arc::ptr_eq(handle, &request.handle))
        else {
            return Task::none();
        };
        // A publication may have reached the client before the next UI refresh.
        let current_snapshot = handle.snapshot();
        let scope_is_current = match &request.source.scope {
            chat::TextCopyScope::Conversation => runtime.selected_subagent.is_none(),
            chat::TextCopyScope::Subagent { id } => runtime.selected_subagent.as_ref() == Some(id),
        };
        if !scope_is_current {
            return Task::none();
        }
        // The text changed under the read, whether this client saw it first or
        // the store did. Read the item once more as it is now, rather than
        // dropping the copy without a word. A source the store refused, and
        // this client has nothing newer for, would only be refused again.
        let refused = result
            .as_ref()
            .is_err_and(|error| error == chat::TEXT_COPY_CHANGED);
        let key = CopyKey::of(id.clone(), request.part.as_ref());
        if refused || !request.snapshot_is_current(&current_snapshot) {
            // It may have been accepted once, before its write found the
            // source changed.
            if runtime.copy_feedback_at(&key).is_some() {
                runtime.copy_feedback = None;
            }
            return match request.reread(&current_snapshot, &id) {
                Some(reread)
                    if !request.retried && !(refused && reread.source == request.source) =>
                {
                    reread.read(target, id)
                }
                Some(_) => runtime.tell_copy(key, Some(chat::TEXT_COPY_CHANGED.into())),
                None => runtime.tell_copy(key, Some(UNAVAILABLE.into())),
            };
        }
        match result {
            Ok(text) => Task::batch([
                message_clipboard_write(target, id, request, text),
                runtime.tell_copy(key, None),
            ]),
            Err(error) => runtime.tell_copy(key, Some(error)),
        }
    }
}

/// A native code button retains immutable raw bytes, never a view-time String.
/// It supersedes older logical reads and checks newer clipboard intent at the
/// actual effect boundary before materializing bytes exactly once.
pub(super) fn write_shared_literal(text: Arc<str>) -> Task<Message> {
    let context = current_context();
    let intent = context.advance();
    iced::Task::done(()).then(move |_| {
        if context.is_current(intent) {
            iced::clipboard::write(text.to_string()).map(cosmic::Action::App)
        } else {
            Task::none()
        }
    })
}

/// A copy whose clipboard contents are produced later, off the UI thread. It
/// joins the shared intent order: starting one supersedes older copies, and
/// any newer copy (including a widget's own selection copy) supersedes it.
#[derive(Clone)]
pub struct Intent {
    context: ClipboardContext,
    intent: u64,
}

impl Intent {
    pub(super) fn begin() -> Self {
        let context = current_context();
        let intent = context.advance();
        Self { context, intent }
    }

    pub(super) fn is_current(&self) -> bool {
        self.context.is_current(self.intent)
    }

    /// Like [`clipboard_write`], the gate runs when the effect is polled.
    pub(super) fn write_data(
        self,
        contents: impl iced::clipboard::mime::AsMimeTypes + Send + Sync + 'static,
    ) -> Task<Message> {
        let mut contents = Some(contents);
        iced::Task::done(()).then(move |_| match contents.take() {
            Some(contents) if self.is_current() => {
                iced::clipboard::write_data(contents).map(cosmic::Action::App)
            }
            _ => Task::none(),
        })
    }
}

/// Check again when the returned task is polled, not just when completion is
/// handled: clipboard intent or source authority may change while this effect
/// is still pending. Keep both checks next to the emitted clipboard effect.
/// A source that changed hands the text to `stale`, whose message handles
/// the read as if the change had come first, rather than dropping the copy
/// without a word.
fn clipboard_write(
    context: ClipboardContext,
    intent: u64,
    text: String,
    authority_is_current: impl Fn() -> bool + Send + 'static,
    stale: impl FnOnce(String) -> Message + Send + 'static,
) -> Task<Message> {
    let mut pending = Some((text, stale));
    iced::Task::done(()).then(move |_| {
        let Some((text, stale)) = pending.take() else {
            return Task::none();
        };
        if !context.is_current(intent) {
            Task::none()
        } else if authority_is_current() {
            iced::clipboard::write(text).map(cosmic::Action::App)
        } else {
            cosmic::task::message(stale(text))
        }
    })
}

/// Writes the text `request` read for item `id`, checked like
/// [`clipboard_write`] when the effect is polled. A source that changed after
/// the read was accepted is read again, as if the change had come first,
/// rather than dropping the copy without a word.
fn message_clipboard_write(
    target: Target,
    id: String,
    request: Request,
    text: String,
) -> Task<Message> {
    let mut read = Some((id, request, text));
    iced::Task::done(()).then(move |_| {
        let Some((id, request, text)) = read.take() else {
            return Task::none();
        };
        if !request.context.is_current(request.intent) {
            return Task::none();
        }
        if !request.snapshot_is_current(&request.handle.snapshot()) {
            return cosmic::task::message(Message::ChatUi(Action::MessageCopied(
                target,
                id,
                request,
                Ok(text),
            )));
        }
        let text = match request.prefix.as_str() {
            "" => text,
            prefix if text.is_empty() => prefix.to_owned(),
            prefix => format!("{prefix}\n\n{text}"),
        };
        iced::clipboard::write(text).map(cosmic::Action::App)
    })
}

fn compatible(captured: &chat::TextCopySource, current: &chat::TextCopySource) -> bool {
    captured.cache == current.cache
        && captured.scope == current.scope
        && captured.token == current.token
        && captured.revision == current.revision
        && captured.bytes <= current.bytes
}

#[cfg(test)]
mod tests {
    use super::super::tests::{assert_chat_copy_task_has_no_effects, copy_refusal, shows_copied};
    use super::*;

    const CHILD: &str = "child";

    fn step(command: &str) -> Arc<chat::Item> {
        Arc::new(chat::Item {
            id: "step".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Command {
                label: String::new(),
                command: command.into(),
                working_directory: String::new(),
                action: chat::CommandAction::Run,
                status: chat::ToolStatus::Running,
                exit_code: None,
                duration_ms: None,
                output: "excerpt".into(),
            },
            at: None,
            started_at: None,
        })
    }

    /// The source of the step at `revision`. The store keeps a conversation
    /// item's cache and token when it rewrites the item; a child item's
    /// rewrite gets a new cache.
    fn source(scope: &chat::TextCopyScope, revision: u64) -> chat::TextCopySource {
        token_source(scope, 7, revision)
    }

    fn token_source(
        scope: &chat::TextCopyScope,
        token: u64,
        revision: u64,
    ) -> chat::TextCopySource {
        chat::TextCopySource {
            cache: match scope {
                chat::TextCopyScope::Conversation => uuid::Uuid::from_u128(1),
                chat::TextCopyScope::Subagent { .. } => uuid::Uuid::new_v4(),
            },
            scope: scope.clone(),
            token,
            revision,
            bytes: 300 * 1024,
        }
    }

    /// Publishes `item` with `source` as the only item of `scope`, or none
    /// at all, as the store does when it rewrites or removes it.
    fn publish(
        handle: &ClientHandle,
        scope: &chat::TextCopyScope,
        item: Option<(Arc<chat::Item>, chat::TextCopySource)>,
    ) {
        let mut snapshot = (*handle.snapshot()).clone();
        let (items, sources) = item
            .map(|(item, source)| (vec![item], vec![Some(source)]))
            .unzip();
        let (items, sources) = (items.unwrap_or_default(), sources.unwrap_or_default());
        match scope {
            chat::TextCopyScope::Conversation => {
                snapshot.items = items;
                snapshot.text_copy_sources.conversation = sources;
                snapshot.text_copy_sources.conversation_revision += 1;
            }
            chat::TextCopyScope::Subagent { id } => {
                snapshot.subagents = vec![chat::Subagent {
                    id: id.clone(),
                    kind: "Scout".into(),
                    description: String::new(),
                    status: chat::ToolStatus::Running,
                    model: None,
                    waiting: false,
                    detail: String::new(),
                    background: true,
                    items,
                }];
                snapshot
                    .text_copy_sources
                    .children
                    .insert(id.clone(), sources);
            }
        }
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
    }

    /// A focused chat showing a running step in `scope`, and a request to
    /// copy that step with its command.
    fn copy_fixture(scope: chat::TextCopyScope) -> (Cagents, Target, Arc<ClientHandle>, Request) {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = super::super::tests::history_fixture(&mut app);
        app.workspace.focus(card).unwrap();
        let source = source(&scope, 1);
        publish(&handle, &scope, Some((step("cargo build"), source.clone())));
        let runtime = app.chat_runtime_mut(target).unwrap();
        if let chat::TextCopyScope::Subagent { id } = &scope {
            runtime.selected_subagent = Some(id.clone());
        }
        runtime.refresh();
        let request =
            Request::for_test(handle.clone(), source).for_step(CopyPart::Text, "cargo build");
        // A read reaches no daemon here: it fails at once, and its completion
        // carries the request it read with.
        handle.disconnect_for_test();
        (app, target, handle, request)
    }

    /// The read `task` starts, which must be the only thing it does.
    fn read_by(task: Task<Message>, target: Target) -> Request {
        use iced::futures::StreamExt;
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let mut effects = iced::runtime::task::into_stream(task).expect("a read");
                let effect = tokio::time::timeout(Duration::from_secs(1), effects.next())
                    .await
                    .expect("the read settles")
                    .expect("the read completes");
                let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                    Action::MessageCopied(read_target, id, request, _),
                ))) = effect
                else {
                    panic!("a changed copy reads again and writes nothing yet");
                };
                assert_eq!((read_target, id.as_str()), (target, "step"));
                assert!(effects.next().await.is_none());
                request
            })
    }

    fn written(task: Task<Message>) -> Option<String> {
        use iced::futures::StreamExt;
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(async {
                let mut effects = iced::runtime::task::into_stream(task)?;
                while let Some(effect) = effects.next().await {
                    if let iced::runtime::Action::Clipboard(
                        iced::runtime::clipboard::Action::Write { contents, .. },
                    ) = effect
                    {
                        return Some(contents);
                    }
                }
                None
            })
    }

    fn scopes() -> [chat::TextCopyScope; 2] {
        [
            chat::TextCopyScope::Conversation,
            chat::TextCopyScope::Subagent { id: CHILD.into() },
        ]
    }

    #[test]
    fn a_copy_whose_text_changed_reads_the_step_again_as_it_is_now() {
        let _context = isolated_context();
        for scope in scopes() {
            let (mut app, target, handle, request) = copy_fixture(scope.clone());
            // A child item the provider rewrites gets a new source, with a
            // new cache, so the step is found again by its ID.
            let rewritten = source(&scope, 2);
            publish(
                &handle,
                &scope,
                Some((step("cargo build --release"), rewritten.clone())),
            );
            let task = app.finish_message_copy(target, "step".into(), request, Ok("old".into()));
            let reread = read_by(task, target);
            assert_eq!(reread.source, rewritten, "{scope:?}");
            assert!(!shows_copied(app.chat_runtime_mut(target).unwrap()));
            assert!(app.chat_runtime_mut(target).unwrap().error.is_none());

            let task = app.finish_message_copy(target, "step".into(), reread, Ok("new".into()));
            assert_eq!(
                written(task).as_deref(),
                Some("cargo build --release\n\nnew"),
                "the clipboard holds the step as it is now: {scope:?}"
            );
        }
    }

    #[test]
    fn a_copy_whose_text_changes_again_while_it_reads_again_says_so() {
        let _context = isolated_context();
        for scope in scopes() {
            let (mut app, target, handle, request) = copy_fixture(scope.clone());
            publish(
                &handle,
                &scope,
                Some((step("cargo build"), source(&scope, 2))),
            );
            let task = app.finish_message_copy(target, "step".into(), request, Ok("old".into()));
            let reread = read_by(task, target);
            publish(
                &handle,
                &scope,
                Some((step("cargo build"), source(&scope, 3))),
            );
            let task = app.finish_message_copy(target, "step".into(), reread, Ok("stale".into()));
            assert_chat_copy_task_has_no_effects(task, &format!("{scope:?}"));
            let runtime = app.chat_runtime_mut(target).unwrap();
            assert_eq!(copy_refusal(runtime), Some(chat::TEXT_COPY_CHANGED));
            assert!(!shows_copied(runtime));
            assert!(
                runtime.error.is_none(),
                "a copy is not the conversation's failure"
            );
        }
    }

    /// A copy that can't go through says why at its own control. It is the
    /// reader's action, so the conversation reports no error for it, and its
    /// outcome is never "Finished with error".
    #[test]
    fn a_refused_copy_says_why_at_its_control_not_as_the_conversation() {
        let _context = isolated_context();
        for scope in scopes() {
            let (mut app, target, _, request) = copy_fixture(scope.clone());
            let task = app.finish_message_copy(
                target,
                "step".into(),
                request,
                Err(chat::TEXT_COPY_CHANGED.to_owned()),
            );
            assert_chat_copy_task_has_no_effects(task, &format!("{scope:?}"));
            let runtime = app.chat_runtime_mut(target).unwrap();
            // The step's own Copy says it, not its blocks'.
            assert!(matches!(
                runtime.copy_state("step", Some(&CopyPart::Text)),
                CopyState::Refused(refusal) if &*refusal == chat::TEXT_COPY_CHANGED
            ));
            assert!(runtime.copy_note_shown("step"));
            assert!(matches!(
                runtime.copy_state("step", Some(&CopyPart::Output)),
                CopyState::Ready
            ));
            assert!(runtime.error.is_none(), "{scope:?}");
            assert_eq!(runtime.reported_errors().count(), 0, "{scope:?}");
        }
    }

    /// A block's Copy answers at that block: copying a step's output does not
    /// light the step's own Copy.
    #[test]
    fn a_copy_key_belongs_to_the_control_it_was_made_from() {
        let item = CopyKey::of("step".into(), Some(&CopyPart::Text));
        let output = CopyKey::of("step".into(), Some(&CopyPart::Output));
        assert_eq!(item, CopyKey::of("step".into(), None));
        assert!(item.is("step", None) && !item.is("step", Some(&CopyPart::Output)));
        assert!(output.is("step", Some(&CopyPart::Output)) && !output.is("step", None));
        assert!(!output.is("other", Some(&CopyPart::Output)));
        assert!(!CopyKey::Failure("step".into()).is("step", None));
    }

    /// The store can see the change before this client's snapshot does. Its
    /// refusal reads the step again once this client has the change, and
    /// says so at once while it has nothing newer to read.
    #[test]
    fn a_copy_the_store_refuses_as_changed_reads_again_only_from_a_newer_source() {
        let _context = isolated_context();
        for scope in scopes() {
            let changed = || Err(chat::TEXT_COPY_CHANGED.to_owned());
            let (mut app, target, _, request) = copy_fixture(scope.clone());
            let task = app.finish_message_copy(target, "step".into(), request, changed());
            assert_chat_copy_task_has_no_effects(task, &format!("{scope:?}"));
            assert_eq!(
                copy_refusal(app.chat_runtime_mut(target).unwrap()),
                Some(chat::TEXT_COPY_CHANGED)
            );

            let (mut app, target, handle, request) = copy_fixture(scope.clone());
            let rewritten = source(&scope, 2);
            publish(
                &handle,
                &scope,
                Some((step("cargo build"), rewritten.clone())),
            );
            let task = app.finish_message_copy(target, "step".into(), request, changed());
            let reread = read_by(task, target);
            assert_eq!(reread.source, rewritten, "{scope:?}");
            assert!(app.chat_runtime_mut(target).unwrap().error.is_none());

            // The store refuses the second read too: it says so, rather than
            // reading a third time.
            let task = app.finish_message_copy(target, "step".into(), reread, changed());
            assert_chat_copy_task_has_no_effects(task, &format!("{scope:?}"));
            assert_eq!(
                copy_refusal(app.chat_runtime_mut(target).unwrap()),
                Some(chat::TEXT_COPY_CHANGED)
            );
        }
    }

    /// A step's copy starts with its command, found by its source even when
    /// an earlier row, such as a long message's part, has the step's ID.
    #[test]
    fn a_step_copy_takes_its_command_from_its_own_row_when_an_id_repeats() {
        let _context = isolated_context();
        let scope = chat::TextCopyScope::Conversation;
        let (mut app, target, handle, request) = copy_fixture(scope.clone());
        let text = Arc::new(chat::Item {
            content: ItemContent::Text {
                text: "a part of a long answer".into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            ..(*step("")).clone()
        });
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.items = vec![text, step("cargo build")];
        snapshot.text_copy_sources.conversation = vec![
            Some(token_source(&scope, 3, 1)),
            Some(request.source.clone()),
        ];
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        let task = app.start_message_copy(
            target,
            "step".into(),
            Some(CopyPart::Text),
            Some(request.source.clone()),
        );
        let started = read_by(task, target);
        assert_eq!(started.source, request.source);
        assert_eq!(started.prefix, "cargo build");
    }

    /// A publication can be handled before a copy captured from the
    /// interface it replaced. Output appended in it leaves the copy valid.
    #[test]
    fn a_step_copy_starts_when_its_output_grew_before_the_copy_was_handled() {
        let _context = isolated_context();
        for scope in scopes() {
            let (mut app, target, handle, request) = copy_fixture(scope.clone());
            let grown = chat::TextCopySource {
                bytes: request.source.bytes + 1024,
                ..request.source.clone()
            };
            publish(&handle, &scope, Some((step("cargo build"), grown)));
            app.chat_runtime_mut(target).unwrap().refresh();
            let task = app.start_message_copy(
                target,
                "step".into(),
                Some(CopyPart::Text),
                Some(request.source.clone()),
            );
            let started = read_by(task, target);
            assert_eq!(started.source, request.source, "{scope:?}");
            assert_eq!(started.prefix, "cargo build", "{scope:?}");
        }
    }

    /// A step copy captured before a rewrite it is handled after says its
    /// text is unavailable rather than doing nothing.
    #[test]
    fn a_step_copy_rewritten_before_the_copy_was_handled_says_it_is_unavailable() {
        let _context = isolated_context();
        for scope in scopes() {
            let (mut app, target, handle, request) = copy_fixture(scope.clone());
            publish(
                &handle,
                &scope,
                Some((step("cargo build"), source(&scope, 2))),
            );
            app.chat_runtime_mut(target).unwrap().refresh();
            let task = app.start_message_copy(
                target,
                "step".into(),
                Some(CopyPart::Text),
                Some(request.source.clone()),
            );
            assert_chat_copy_task_has_no_effects(task, &format!("{scope:?}"));
            let runtime = app.chat_runtime_mut(target).unwrap();
            assert_eq!(copy_refusal(runtime), Some(UNAVAILABLE), "{scope:?}");
        }
    }

    /// A conversation item's ID can repeat in the window: a long message's
    /// parts get generated IDs, and a provider item may have one of them.
    /// Its reread follows its own source, not the first row with its ID.
    #[test]
    fn a_changed_conversation_copy_reads_its_own_item_again_when_an_id_repeats() {
        let _context = isolated_context();
        let scope = chat::TextCopyScope::Conversation;
        let (mut app, target, handle, request) = copy_fixture(scope.clone());
        let other = token_source(&scope, 3, 1);
        let rewritten = source(&scope, 2);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.items = vec![step("cargo test"), step("cargo build --release")];
        snapshot.text_copy_sources.conversation = vec![Some(other), Some(rewritten.clone())];
        snapshot.text_copy_sources.conversation_revision += 1;
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
        let task = app.finish_message_copy(target, "step".into(), request, Ok("old".into()));
        let reread = read_by(task, target);
        assert_eq!(reread.source, rewritten);
        assert_eq!(reread.prefix, "cargo build --release");
    }

    #[test]
    fn a_copy_whose_step_was_removed_says_its_text_is_unavailable() {
        let _context = isolated_context();
        for scope in scopes() {
            let (mut app, target, handle, request) = copy_fixture(scope.clone());
            publish(&handle, &scope, None);
            let task = app.finish_message_copy(target, "step".into(), request, Ok("old".into()));
            assert_chat_copy_task_has_no_effects(task, &format!("{scope:?}"));
            let runtime = app.chat_runtime_mut(target).unwrap();
            assert_eq!(copy_refusal(runtime), Some(UNAVAILABLE));
            assert!(!shows_copied(runtime));
        }
    }

    #[test]
    fn appended_prefix_is_valid_but_rewrite_scope_and_cache_changes_are_not() {
        let _context = isolated_context();
        let captured = chat::TextCopySource {
            cache: uuid::Uuid::new_v4(),
            scope: chat::TextCopyScope::Conversation,
            token: 1,
            revision: 2,
            bytes: 300 * 1024,
        };
        let mut current = captured.clone();
        current.bytes += 15;
        assert!(compatible(&captured, &current));
        current.revision += 1;
        assert!(!compatible(&captured, &current));
        current = captured.clone();
        current.scope = chat::TextCopyScope::Subagent {
            id: "same-id".into(),
        };
        assert!(!compatible(&captured, &current));
        current = captured.clone();
        current.cache = uuid::Uuid::new_v4();
        assert!(!compatible(&captured, &current));
        current = captured.clone();
        current.bytes -= 1;
        assert!(!compatible(&captured, &current));
    }
    #[test]
    fn cross_surface_copy_invalidates_an_already_scheduled_chat_write() {
        use iced::futures::StreamExt;
        let _context = isolated_context();
        let context = current_context();
        let task = clipboard_write(
            context.clone(),
            context.advance(),
            "old logical message".into(),
            || true,
            |_| unreachable!("the source is current"),
        );
        let mut app = crate::app::tests::context_inspector_test_app(false);
        drop(app.update(Message::Repository(RepositoryAction::Clipboard {
            card: CardId::new(),
            op: repository_widget::ClipboardOp::Copy,
        })));
        let effects = iced::futures::executor::block_on(async {
            iced::runtime::task::into_stream(task)
                .unwrap()
                .collect::<Vec<_>>()
                .await
        });
        assert!(
            effects.is_empty(),
            "the gate must run when the clipboard task is polled"
        );
    }
    #[test]
    fn isolated_contexts_do_not_cancel_each_other_and_captured_context_survives_thread_change() {
        use iced::futures::StreamExt;
        let _scope = isolated_context();
        let first = current_context();
        let intent = first.advance();
        {
            let _other_scope = isolated_context();
            invalidate();
            assert!(first.is_current(intent));
        }
        let task = clipboard_write(
            first.clone(),
            intent,
            "first context source".into(),
            || true,
            |_| unreachable!("the source is current"),
        );
        let copied = std::thread::spawn(move || {
            // This thread has no injected context. The task must use its
            // captured first context rather than the process default.
            iced::futures::executor::block_on(async {
                let mut effects = iced::runtime::task::into_stream(task).unwrap();
                match effects.next().await {
                    Some(iced::runtime::Action::Clipboard(
                        iced::runtime::clipboard::Action::Write { contents, .. },
                    )) => Some(contents),
                    _ => None,
                }
            })
        })
        .join()
        .unwrap();
        assert_eq!(copied.as_deref(), Some("first context source"));
        invalidate();
        assert!(
            !first.is_current(intent),
            "the restored shared UI context must cancel its own request"
        );
    }
}
