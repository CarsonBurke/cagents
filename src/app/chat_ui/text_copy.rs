//! Logical message Copy reads canonical source pages off the UI thread.
use super::*;

static INTENT: AtomicU64 = AtomicU64::new(0);

/// A newer chat copy (including widget-owned selection) supersedes an in-flight read.
pub(super) fn invalidate() -> u64 {
    INTENT.fetch_add(1, Ordering::Relaxed).wrapping_add(1)
}

#[derive(Clone)]
pub struct Request {
    pub(super) intent: u64,
    pub(super) handle: Arc<ClientHandle>,
    pub(super) source: chat::TextCopySource,
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
        let intent = invalidate();
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
            intent,
            handle: handle.clone(),
            source: source.clone(),
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
        if INTENT.load(Ordering::Relaxed) != request.intent
            || self.workspace.focused() != Some(target.card)
        {
            return Task::none();
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if !runtime
            .handle()
            .is_some_and(|handle| Arc::ptr_eq(handle, &request.handle))
            || !runtime.contains_copy_source(&request.source)
        {
            return Task::none();
        }
        match result {
            Ok(text) => {
                let at = Instant::now();
                runtime.copied = Some((id, at));
                Task::batch([
                    clipboard_write(request.intent, text),
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

/// Check again when the returned task is polled, not just when completion is
/// handled: another copy may be requested while this effect is still pending.
fn clipboard_write(intent: u64, text: String) -> Task<Message> {
    let mut text = Some(text);
    Task::done(()).then(move |_| {
        if INTENT.load(Ordering::Relaxed) == intent {
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
        let task = clipboard_write(invalidate(), "old logical message".into());
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
}
