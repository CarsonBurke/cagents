//! Logical message Copy reads canonical source pages off the UI thread.
use super::*;

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
}

impl Request {
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
        }
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
    pub(super) fn start_message_copy(
        &mut self,
        target: Target,
        id: String,
        source: Option<chat::TextCopySource>,
    ) -> Task<Message> {
        let context = current_context();
        let intent = context.advance();
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(source) = source.filter(|source| runtime.contains_copy_source(source)) else {
            runtime.error = Some("This message is unavailable in the local copy cache. Open the provider transcript to recover its text.".into());
            return Task::none();
        };
        let Some(handle) = runtime.handle().cloned() else {
            runtime.error =
                Some("Reconnect this conversation before copying its full message.".into());
            return Task::none();
        };
        let request = Request {
            context,
            intent,
            handle: handle.clone(),
            source: source.clone(),
            conversation_revision: runtime.snapshot.text_copy_sources.conversation_revision,
        };
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || handle.copy_text(source))
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result);
            Message::ChatUi(Action::MessageCopied(target, id, request, result))
        })
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
        if !scope_is_current || !request.snapshot_is_current(&current_snapshot) {
            return Task::none();
        }
        match result {
            Ok(text) => {
                let at = Instant::now();
                runtime.copied = Some((id, at));
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

        // Use the actual palette/native input wrapper which records every
        // consumed key, rather than manufacturing an untracked App chord.
        use cosmic::iced::runtime::core::keyboard::input::{self, PressCycle, Source, Transition};
        use iced::advanced::{clipboard, widget::operation::focusable};
        for (character, code) in [
            ("c", keyboard::key::Code::KeyC),
            ("x", keyboard::key::Code::KeyX),
        ] {
            let old_intent = context.advance();
            let old = clipboard_write(context.clone(), old_intent, "older message".into(), || true);
            let mut app = crate::app::tests::context_inspector_test_app(false);
            app.window_width = 1600.0;
            app.window_height = 1000.0;
            app.refresh_ui.focused = true;
            app.context_inspector_open = false;
            drop(app.update_palette(palette_ui::Action::Open));
            let event = keyboard::Event::KeyPressed {
                key: keyboard::Key::Character(character.into()),
                modified_key: keyboard::Key::Character(character.into()),
                physical_key: keyboard::key::Physical::Code(code),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::CTRL,
                text: None,
                repeat: false,
            };
            let source = Source::tracked(
                PressCycle::new(keyboard::key::Physical::Code(code)),
                Transition::Press,
            );
            let mut renderer = crate::app::tests::headless_renderer();
            let mut ui = iced::runtime::UserInterface::build(
                app.view(),
                iced::Size::new(1600.0, 1000.0),
                iced::runtime::user_interface::Cache::default(),
                &mut renderer,
            );
            ui.operate(
                &renderer,
                &mut focusable::focus::<()>(widget::Id::new("palette.query")),
            );
            let mut messages = Vec::new();
            let (_, statuses) = ui.update_with_sources(
                &[iced::Event::Keyboard(event.clone())],
                &[source.clone()],
                iced::mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(statuses, [iced::event::Status::Captured]);
            drop(ui);
            {
                let _native = input::scope(source.clone());
                assert!(
                    app.terminal_review_input
                        .owns_keyboard(&iced::Event::Keyboard(event.clone()))
                );
            }
            for message in messages {
                drop(app.update(message));
            }
            drop(app.update(Message::TaggedKeyEvent {
                event: event.clone(),
                captured: true,
                source: source.clone(),
            }));
            assert!(
                !context.is_current(old_intent),
                "consumed native Copy/Cut supersedes the older message intent"
            );
            let old_effects = iced::futures::executor::block_on(async {
                iced::runtime::task::into_stream(old)
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await
            });
            assert!(
                old_effects.is_empty(),
                "the real delayed clipboard write must be cancelled"
            );

            let current = write_shared_literal(Arc::from("newer native selection"));
            let current_effects = iced::futures::executor::block_on(async {
                iced::runtime::task::into_stream(current)
                    .unwrap()
                    .collect::<Vec<_>>()
                    .await
            });
            assert!(current_effects.iter().any(|effect| matches!(effect,
                iced::runtime::Action::Clipboard(iced::runtime::clipboard::Action::Write { contents, .. })
                    if contents == "newer native selection"
            )), "the newer clipboard intent remains emittable");
        }
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
