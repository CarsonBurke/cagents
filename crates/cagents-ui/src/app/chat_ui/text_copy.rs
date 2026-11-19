//! Logical message Copy reads canonical source pages off the UI thread.
use super::*;

pub(super) const UNAVAILABLE: &str =
    "This text is unavailable in the local copy cache. Open the provider transcript to recover it.";

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
    /// or `None` once the item or its whole text is gone. The item is found
    /// by its ID, because a rewritten child item gets a new source.
    fn reread(&self, snapshot: &Snapshot, id: &str) -> Option<Self> {
        let (items, sources) = match &self.source.scope {
            chat::TextCopyScope::Conversation => {
                (&snapshot.items, &snapshot.text_copy_sources.conversation)
            }
            chat::TextCopyScope::Subagent { id: child } => (
                &snapshot
                    .subagents
                    .iter()
                    .find(|agent| agent.id == *child)?
                    .items,
                snapshot.text_copy_sources.children.get(child)?,
            ),
        };
        let index = items.iter().position(|item| item.id == id)?;
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
            Err(error) => {
                runtime.error = Some(error);
                return Task::none();
            }
        };
        let Some(handle) = runtime.handle().cloned() else {
            runtime.error = Some(
                "Original diagnostic unavailable: reconnect this conversation before copying"
                    .into(),
            );
            return Task::none();
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
        match result {
            Ok(text) => {
                if runtime.failure_copy_plan(&key).ok().flatten().as_ref() != Some(&request.plan)
                    || !request.plan.is_current(&request.handle.snapshot())
                {
                    return Task::none();
                }
                let context = request.context.clone();
                let intent = request.intent;
                clipboard_write(context, intent, text, move || {
                    request.plan.is_current(&request.handle.snapshot())
                })
            }
            Err(error) => {
                if runtime.failure_plan_still_shown(&key, &request.plan)
                    && request
                        .plan
                        .is_current_or_withdrawn(&request.handle.snapshot())
                {
                    const PREFIX: &str = "Original diagnostic unavailable: ";
                    runtime.error = Some(if error.starts_with(PREFIX) {
                        error
                    } else {
                        format!("{PREFIX}{error}")
                    });
                }
                Task::none()
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
        let prefix = match &part {
            Some(part) => {
                let Some(prefix) = self
                    .chat_runtime_mut(target)
                    .and_then(|runtime| runtime.active_item(&id))
                    .and_then(|item| step_copy_prefix(part, &item.content))
                else {
                    return Task::none();
                };
                prefix
            }
            None => String::new(),
        };
        let context = current_context();
        let intent = context.advance();
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(source) = source.filter(|source| runtime.contains_copy_source(source)) else {
            runtime.error = Some(UNAVAILABLE.into());
            return Task::none();
        };
        let Some(handle) = runtime.handle().cloned() else {
            runtime.error =
                Some("Reconnect this conversation before copying all of this text.".into());
            return Task::none();
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
        if refused || !request.snapshot_is_current(&current_snapshot) {
            return match request.reread(&current_snapshot, &id) {
                Some(reread)
                    if !request.retried && !(refused && reread.source == request.source) =>
                {
                    reread.read(target, id)
                }
                Some(_) => {
                    runtime.error = Some(chat::TEXT_COPY_CHANGED.into());
                    Task::none()
                }
                None => {
                    runtime.error = Some(UNAVAILABLE.into());
                    Task::none()
                }
            };
        }
        match result {
            Ok(text) => {
                let at = Instant::now();
                runtime.copied = Some((id, at));
                let text = match request.prefix.as_str() {
                    "" => text,
                    prefix if text.is_empty() => prefix.to_owned(),
                    prefix => format!("{prefix}\n\n{text}"),
                };
                let context = request.context.clone();
                let intent = request.intent;
                Task::batch([
                    clipboard_write(context, intent, text, move || {
                        request.snapshot_is_current(&request.handle.snapshot())
                    }),
                    cosmic::task::future(async move {
                        tokio::time::sleep(Duration::from_millis(1500)).await;
                        Message::ChatUi(Action::ClearCopied(target, at))
                    }),
                ])
            }
            Err(error) => {
                runtime.error = Some(error);
                Task::none()
            }
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
fn clipboard_write(
    context: ClipboardContext,
    intent: u64,
    text: String,
    authority_is_current: impl Fn() -> bool + Send + 'static,
) -> Task<Message> {
    let mut text = Some(text);
    iced::Task::done(()).then(move |_| {
        if authority_is_current() && context.is_current(intent) {
            iced::clipboard::write(text.take().unwrap_or_default()).map(cosmic::Action::App)
        } else {
            Task::none()
        }
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

    fn source(scope: &chat::TextCopyScope, revision: u64) -> chat::TextCopySource {
        chat::TextCopySource {
            cache: uuid::Uuid::new_v4(),
            scope: scope.clone(),
            token: 7,
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
            assert!(app.chat_runtime_mut(target).unwrap().copied.is_none());
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
            assert!(
                iced::runtime::task::into_stream(task).is_none(),
                "{scope:?}"
            );
            let runtime = app.chat_runtime_mut(target).unwrap();
            assert_eq!(runtime.error.as_deref(), Some(chat::TEXT_COPY_CHANGED));
            assert!(runtime.copied.is_none());
        }
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
            assert!(
                iced::runtime::task::into_stream(task).is_none(),
                "{scope:?}"
            );
            assert_eq!(
                app.chat_runtime_mut(target).unwrap().error.as_deref(),
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
            assert_eq!(read_by(task, target).source, rewritten, "{scope:?}");
            assert!(app.chat_runtime_mut(target).unwrap().error.is_none());
        }
    }

    #[test]
    fn a_copy_whose_step_was_removed_says_its_text_is_unavailable() {
        let _context = isolated_context();
        for scope in scopes() {
            let (mut app, target, handle, request) = copy_fixture(scope.clone());
            publish(&handle, &scope, None);
            let task = app.finish_message_copy(target, "step".into(), request, Ok("old".into()));
            assert!(
                iced::runtime::task::into_stream(task).is_none(),
                "{scope:?}"
            );
            let runtime = app.chat_runtime_mut(target).unwrap();
            assert_eq!(runtime.error.as_deref(), Some(UNAVAILABLE));
            assert!(runtime.copied.is_none());
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
        let task = clipboard_write(first.clone(), intent, "first context source".into(), || {
            true
        });
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
