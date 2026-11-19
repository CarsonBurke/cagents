//! Logical message Copy reads canonical source pages off the UI thread.
use super::*;

const CHANGED_WHILE_COPYING: &str = "Changed while copying; copy again";

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
    retried: bool,
    /// What the clipboard holds ahead of the source's text, such as the
    /// command a step's whole copy starts with.
    prefix: String,
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

    fn retry(mut self, snapshot: &Snapshot) -> Option<Self> {
        if self.retried {
            return None;
        }
        let sources = match &self.source.scope {
            chat::TextCopyScope::Conversation => &snapshot.text_copy_sources.conversation,
            chat::TextCopyScope::Subagent { id } => snapshot.text_copy_sources.children.get(id)?,
        };
        self.source = sources
            .iter()
            .flatten()
            .find(|source| {
                source.cache == self.source.cache
                    && source.scope == self.source.scope
                    && source.token == self.source.token
            })?
            .clone();
        self.conversation_revision = snapshot.text_copy_sources.conversation_revision;
        self.retried = true;
        Some(self)
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
            retried: false,
            prefix: String::new(),
        }
    }
    pub(super) fn with_prefix(mut self, prefix: &str) -> Self {
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

    pub(super) fn start_message_copy(
        &mut self,
        target: Target,
        id: String,
        source: Option<chat::TextCopySource>,
        prefix: String,
    ) -> Task<Message> {
        let context = current_context();
        let intent = context.advance();
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(source) = source.filter(|source| runtime.contains_copy_source(source)) else {
            runtime.error = Some("This text is unavailable in the local copy cache. Open the provider transcript to recover it.".into());
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
            retried: false,
            prefix,
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
        if !request.snapshot_is_current(&current_snapshot) {
            if let Some(retry) = request.retry(&current_snapshot) {
                return retry.read(target, id);
            }
            runtime.error = Some(CHANGED_WHILE_COPYING.into());
            return Task::none();
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
                runtime.error = Some(if error == "This text changed; copy it again" {
                    CHANGED_WHILE_COPYING.into()
                } else {
                    error
                });
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

    fn copy_fixture(
        scope: chat::TextCopyScope,
    ) -> (Cagents, Target, Request, chat::TextCopySource) {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let (card, target, handle) = super::super::tests::history_fixture(&mut app);
        app.workspace.focus(card).unwrap();
        let source = chat::TextCopySource {
            cache: uuid::Uuid::new_v4(),
            scope,
            token: 7,
            revision: 9,
            bytes: 300 * 1024,
        };
        publish_copy_source(&handle, source.clone());
        if let chat::TextCopyScope::Subagent { id } = &source.scope {
            app.chat_runtime_mut(target).unwrap().selected_subagent = Some(id.clone());
        }
        let request = Request::for_test(handle, source.clone()).with_prefix("cargo build");
        (app, target, request, source)
    }

    fn publish_copy_source(handle: &ClientHandle, source: chat::TextCopySource) {
        let mut snapshot = (*handle.snapshot()).clone();
        match &source.scope {
            chat::TextCopyScope::Conversation => {
                snapshot.text_copy_sources.conversation = vec![Some(source)];
                snapshot.text_copy_sources.conversation_revision += 1;
            }
            chat::TextCopyScope::Subagent { id } => {
                snapshot
                    .text_copy_sources
                    .children
                    .insert(id.clone(), vec![Some(source)]);
            }
        }
        snapshot.generation += 1;
        handle.publish_snapshot_for_test(snapshot);
    }

    fn retry_request(task: Task<Message>, target: Target) -> Request {
        use iced::futures::StreamExt;
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap();
        executor.block_on(async {
            let mut effects = iced::runtime::task::into_stream(task).expect("a retry must read");
            let effect = tokio::time::timeout(Duration::from_secs(1), effects.next())
                .await
                .expect("fixture retry must settle")
                .expect("retry completion");
            let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                Action::MessageCopied(actual, id, request, result),
            ))) = effect
            else {
                panic!("a retry must emit its read completion, never stale clipboard bytes");
            };
            assert_eq!(actual, target);
            assert_eq!(id, "output");
            assert_eq!(
                result,
                Err("Chat is disconnected; text copy is unavailable".into())
            );
            request
        })
    }

    #[test]
    fn stale_snapshot_retries_once_against_current_source_and_succeeds() {
        use iced::futures::StreamExt;
        let _context = isolated_context();
        for scope in [
            chat::TextCopyScope::Conversation,
            chat::TextCopyScope::Subagent { id: "child".into() },
        ] {
            let (mut app, target, request, mut current) = copy_fixture(scope);
            let handle = request.handle.clone();
            let intent = request.intent;
            let context = request.context.clone();
            current.revision += 1;
            current.bytes = "current raw output λ\r\n".len() as u64;
            publish_copy_source(&handle, current.clone());
            // Observe the actual retry task without a live peer, then supply
            // its successful read completion to exercise the clipboard path.
            handle.disconnect_for_test();
            let task = app.finish_message_copy(target, "output".into(), request, Ok("old".into()));
            assert!(app.chat_runtime_mut(target).unwrap().copied.is_none());
            let retry = retry_request(task, target);
            assert!(retry.retried);
            assert_eq!(retry.source, current);
            assert_eq!(retry.intent, intent);
            assert!(Arc::ptr_eq(&retry.context.0, &context.0));
            assert_eq!(
                retry.conversation_revision,
                handle.snapshot().text_copy_sources.conversation_revision
            );
            let task = app.finish_message_copy(
                target,
                "output".into(),
                retry,
                Ok("current raw output λ\r\n".into()),
            );
            assert!(app.chat_runtime_mut(target).unwrap().copied.is_some());
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_time()
                .build()
                .unwrap();
            executor.block_on(async {
                let mut effects = iced::runtime::task::into_stream(task).unwrap();
                let effect = tokio::time::timeout(Duration::from_secs(1), effects.next())
                    .await
                    .unwrap()
                    .unwrap();
                let iced::runtime::Action::Clipboard(iced::runtime::clipboard::Action::Write {
                    contents,
                    ..
                }) = effect
                else {
                    panic!("the current retry must write the clipboard");
                };
                assert_eq!(contents, "cargo build\n\ncurrent raw output λ\r\n");
            });
        }
    }

    #[test]
    fn stale_retry_shows_changed_while_copying_without_another_read() {
        let _context = isolated_context();
        for scope in [
            chat::TextCopyScope::Conversation,
            chat::TextCopyScope::Subagent { id: "child".into() },
        ] {
            let (mut app, target, request, mut current) = copy_fixture(scope);
            let handle = request.handle.clone();
            current.revision += 1;
            publish_copy_source(&handle, current.clone());
            handle.disconnect_for_test();
            let retry = retry_request(
                app.finish_message_copy(target, "output".into(), request, Ok("old".into())),
                target,
            );
            current.revision += 1;
            publish_copy_source(&handle, current);
            let task = app.finish_message_copy(target, "output".into(), retry, Ok("stale".into()));
            assert!(iced::runtime::task::into_stream(task).is_none());
            let runtime = app.chat_runtime_mut(target).unwrap();
            assert_eq!(
                runtime.error.as_deref(),
                Some("Changed while copying; copy again")
            );
            assert!(runtime.copied.is_none());
        }
    }

    #[test]
    fn store_revision_moved_refusal_shows_changed_while_copying() {
        let _context = isolated_context();
        let (mut app, target, request, _) = copy_fixture(chat::TextCopyScope::Conversation);
        let task = app.finish_message_copy(
            target,
            "output".into(),
            request,
            Err("This text changed; copy it again".into()),
        );
        assert!(iced::runtime::task::into_stream(task).is_none());
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert_eq!(
            runtime.error.as_deref(),
            Some("Changed while copying; copy again")
        );
        assert!(runtime.copied.is_none());
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
