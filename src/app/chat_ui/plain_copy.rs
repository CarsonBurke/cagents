//! Explicit message copy runs normalization and Markdown formatting away from
//! the event thread. Source/output slots retire pure owners off the UI thread.
//! The parser's temporary allocations remain separate from output-byte metrics.
use super::*;

const LIMIT: usize = 32 * 1024 * 1024;
static NEXT: AtomicU64 = AtomicU64::new(1);
static OUTPUT_BYTES: AtomicU64 = AtomicU64::new(0);

use super::clipboard_intent::Intent;

#[derive(Clone)]
struct Proof {
    source: std::sync::Weak<chat::Item>,
    actor: Option<std::sync::Weak<ClientHandle>>,
    session: Option<String>,
    selected: Option<String>,
    account_epoch: u64,
    source_incarnation: Option<uuid::Uuid>,
    intent: Intent,
}
impl Proof {
    fn current(&self, runtime: &Runtime, account_epoch: u64) -> bool {
        self.intent.current()
            && self.account_epoch == account_epoch
            && self.source_incarnation == runtime.snapshot.source_incarnation
            && self.session == runtime.snapshot.session_id
            && self.selected == runtime.selected_subagent
            && (match (&self.actor, runtime.handle()) {
                (None, None) => true,
                (Some(old), Some(new)) => old.as_ptr() == Arc::as_ptr(new),
                _ => false,
            })
            && self.source.upgrade().is_some_and(|source| {
                runtime
                    .active_item(&source.id)
                    .is_some_and(|item| Arc::ptr_eq(item, &source))
            })
    }
}
struct Attempt {
    id: u64,
    proof: Proof,
    cancelled: Arc<AtomicBool>,
    input: std::sync::Weak<InputSlot>,
}
#[derive(Default)]
pub(super) struct State {
    attempt: Option<Attempt>,
}
impl State {
    pub(super) fn clear(&mut self) {
        if let Some(attempt) = self.attempt.take() {
            attempt.cancelled.store(true, Ordering::Release);
            if let Some(input) = attempt.input.upgrade() {
                input.cancel();
            }
        }
    }
}
impl Drop for State {
    fn drop(&mut self) {
        self.clear();
    }
}

struct InputSlot {
    source: Mutex<Option<Arc<chat::Item>>>,
    cancelled: Arc<AtomicBool>,
}
impl InputSlot {
    fn take(&self) -> Option<Arc<chat::Item>> {
        self.source
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take()
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
        if let Some(source) = self.take() {
            retire(source);
        }
    }
}
impl clipboard_intent::Pending for InputSlot {
    fn withdraw(&self) {
        self.cancel();
    }
}
impl Drop for InputSlot {
    fn drop(&mut self) {
        if let Some(source) = self
            .source
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            retire(source);
        }
    }
}
struct OutputBytes(usize);
impl OutputBytes {
    fn new(bytes: usize) -> Self {
        OUTPUT_BYTES.fetch_add(bytes as u64, Ordering::Relaxed);
        trace();
        Self(bytes)
    }
}
impl Drop for OutputBytes {
    fn drop(&mut self) {
        OUTPUT_BYTES.fetch_sub(self.0 as u64, Ordering::Relaxed);
        trace();
    }
}
fn trace() {
    tracing::trace!(target: "cagents::memory", owner="chat_plain_copy_output",
        process_id=std::process::id(), retained_bytes=OUTPUT_BYTES.load(Ordering::Relaxed),
        "logical queued copy output; excludes parser temporaries and clipboard ownership");
}
struct Output {
    text: String,
    _permit: tokio::sync::OwnedSemaphorePermit,
    _bytes: OutputBytes,
}
struct ReplySlot(Mutex<Option<Result<Output, String>>>);
impl Drop for ReplySlot {
    fn drop(&mut self) {
        if let Some(value) = self
            .0
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            retire(value);
        }
    }
}
#[derive(Clone)]
pub(super) struct Reply(Arc<ReplySlot>);
impl Reply {
    fn new(value: Result<Output, String>) -> Self {
        Self(Arc::new(ReplySlot(Mutex::new(Some(value)))))
    }
}

fn format(source: &chat::Item) -> Result<String, String> {
    let ItemContent::Text { text, .. } = &source.content else {
        return Err("This message has no text to copy.".into());
    };
    if text.len() > LIMIT {
        return Err(
            "This message exceeds the 32 MiB plain-text copy limit. Copy its Markdown instead."
                .into(),
        );
    }
    // User text is already displayed literally. Assistant Markdown uses the
    // same formatter as existing copy, now entirely on the preparation worker.
    let text = if source.role == chat::Role::User {
        text.clone()
    } else {
        turn_actions::plain_text(text)
    };
    if text.len() > LIMIT {
        return Err("The formatted plain text exceeds the 32 MiB copy limit.".into());
    }
    Ok(text)
}

pub(super) fn begin(
    runtime: &mut Runtime,
    item: &str,
    account_epoch: u64,
    intent: Intent,
) -> Task<Message> {
    if !intent.current() {
        return Task::none();
    }
    let Some(source) = runtime.active_item(item).cloned() else {
        return Task::none();
    };
    if !matches!(source.content, ItemContent::Text { .. })
        || !runtime.visible.load(Ordering::Relaxed)
    {
        return Task::none();
    }
    runtime.recovery.plain_copy.clear();
    let id = NEXT.fetch_add(1, Ordering::Relaxed);
    let proof = Proof {
        source: Arc::downgrade(&source),
        actor: runtime.handle().map(Arc::downgrade),
        session: runtime.snapshot.session_id.clone(),
        selected: runtime.selected_subagent.clone(),
        account_epoch,
        source_incarnation: runtime.snapshot.source_incarnation,
        intent: intent.clone(),
    };
    let cancelled = Arc::new(AtomicBool::new(false));
    let input = Arc::new(InputSlot {
        source: Mutex::new(Some(source)),
        cancelled: cancelled.clone(),
    });
    intent.register(&input);
    runtime.recovery.plain_copy.attempt = Some(Attempt {
        id,
        proof,
        cancelled: cancelled.clone(),
        input: Arc::downgrade(&input),
    });
    let target = runtime.target;
    Task::perform(
        async move {
            let value = tokio::time::timeout(Duration::from_secs(40), async move {
                // Share original-input admission, including completed queued output.
                let permit = originals::readers()
                    .clone()
                    .acquire_owned()
                    .await
                    .map_err(|_| "Plain-text copy stopped.".to_owned())?;
                tokio::task::spawn_blocking(move || {
                    if cancelled.load(Ordering::Acquire) {
                        return Err("Plain-text copy was cancelled.".into());
                    }
                    let source = input.take().ok_or("Plain-text copy was cancelled.")?;
                    let text = format(&source)?;
                    if cancelled.load(Ordering::Acquire) {
                        return Err("Plain-text copy was cancelled.".into());
                    }
                    let bytes = OutputBytes::new(text.capacity());
                    Ok(Output {
                        text,
                        _permit: permit,
                        _bytes: bytes,
                    })
                })
                .await
                .map_err(|_| "Plain-text copy stopped.".to_owned())?
            })
            .await
            .unwrap_or_else(|_| Err("Plain-text copy took too long. Try copying Markdown.".into()));
            Reply::new(value)
        },
        move |reply| cosmic::Action::App(action(Action::PlainCopyReady(target, id, reply))),
    )
}

pub(super) fn complete(
    runtime: &mut Runtime,
    id: u64,
    account_epoch: u64,
    reply: Reply,
) -> Option<(String, String)> {
    let current = runtime.visible.load(Ordering::Relaxed)
        && runtime
            .recovery
            .plain_copy
            .attempt
            .as_ref()
            .is_some_and(|attempt| {
                attempt.id == id
                    && !attempt.cancelled.load(Ordering::Acquire)
                    && attempt.proof.current(runtime, account_epoch)
            });
    if !current {
        return None;
    }
    let attempt = runtime.recovery.plain_copy.attempt.take()?;
    let item = attempt.proof.source.upgrade()?.id.clone();
    let result = reply
        .0
        .0
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()?;
    match result {
        Ok(output) => Some((item, output.text)),
        Err(error) => {
            runtime.recovery.notice = Some(error);
            None
        }
    }
}

fn retire(value: impl Send + 'static) {
    type Retired = Box<dyn std::any::Any + Send>;
    static SENDER: std::sync::OnceLock<std::sync::mpsc::Sender<Retired>> =
        std::sync::OnceLock::new();
    let sender = SENDER.get_or_init(|| {
        let (send, receive) = std::sync::mpsc::channel::<Retired>();
        std::thread::Builder::new()
            .name("cagents-plain-copy-retire".into())
            .spawn(move || {
                for value in receive {
                    drop(value);
                }
            })
            .expect("start plain copy retirement");
        send
    });
    if let Err(value) = sender.send(Box::new(value)) {
        drop(value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use iced::futures::StreamExt as _;

    fn item(role: chat::Role, text: &str) -> Arc<chat::Item> {
        Arc::new(chat::Item {
            id: "same-opaque-message".into(),
            role,
            at: None,
            started_at: None,
            content: ItemContent::Text {
                text: text.into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
        })
    }
    fn runtime() -> Runtime {
        let session = fresh_chat_session(
            Provider::Claude,
            TerminalId::new(),
            "/project".into(),
            &Account::system(Provider::Claude),
            false,
        )
        .unwrap();
        let handle = Arc::new(ClientHandle::fixture(
            session.clone(),
            Snapshot {
                ready: true,
                session_id: Some("confirmed-session".into()),
                ..Snapshot::default()
            },
        ));
        Runtime::new(
            Target {
                card: CardId::new(),
                generation: 1,
            },
            session,
            Connection::Live(handle),
            true,
            crate::settings::EditorSyntaxTheme::default(),
        )
    }
    async fn ready(task: Task<Message>) -> (Target, u64, Reply) {
        let mut stream = iced::runtime::task::into_stream(task).expect("copy preparation task");
        while let Some(output) = stream.next().await {
            if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                Action::PlainCopyReady(target, id, reply),
            ))) = output
            {
                return (target, id, reply);
            }
        }
        panic!("copy preparation produced no result")
    }
    #[tokio::test]
    async fn actual_selected_message_copy_uses_worker_and_rejects_stale_contexts() {
        for change in 0..8 {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let mut runtime = runtime();
            let target = runtime.target;
            let parent = item(chat::Role::Assistant, "**PARENT**");
            let child = item(
                chat::Role::Assistant,
                "# Child\n\n**Only** child\n\n- one\n- two\n\n| A | B |\n|---|---|\n| x | y |\n\n```rust\nlet exact = 1;\n```",
            );
            Arc::make_mut(&mut runtime.snapshot).items = vec![parent];
            Arc::make_mut(&mut runtime.snapshot).subagents = vec![chat::Subagent {
                id: "child".into(),
                kind: String::new(),
                description: String::new(),
                status: chat::ToolStatus::Succeeded,
                model: None,
                waiting: false,
                detail: String::new(),
                background: false,
                items: vec![child.clone()],
            }];
            runtime.selected_subagent = Some("child".into());
            app.cards
                .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
            let task =
                app.handle_chat_ui(Action::Copy(target, child.id.clone(), CopyPart::PlainText));
            assert!(
                app.chat_runtime_mut(target).unwrap().copied.is_none(),
                "preparation is asynchronous"
            );
            let (_, id, reply) = ready(task).await;
            {
                let result = reply.0.0.lock().unwrap();
                let output = result.as_ref().unwrap().as_ref().unwrap();
                assert!(
                    output.text.contains("Only child")
                        && output.text.contains("• one")
                        && output.text.contains("A\tB")
                        && output.text.contains("let exact = 1;")
                );
                assert!(!output.text.contains("PARENT") && !output.text.contains("**"));
            }
            match change {
                1 => {
                    app.chat_runtime_mut(target).unwrap().selected_subagent = None;
                }
                2 => {
                    let runtime = app.chat_runtime_mut(target).unwrap();
                    let snapshot = Arc::make_mut(&mut runtime.snapshot);
                    snapshot.subagents[0].items[0] =
                        item(chat::Role::Assistant, "replacement same opaque identity");
                }
                3 => {
                    let runtime = app.chat_runtime_mut(target).unwrap();
                    Arc::make_mut(&mut runtime.snapshot).session_id =
                        Some("replacement-session".into());
                }
                4 => {
                    app.invalidate_originals_for_account(&Account::system(Provider::Claude).id());
                }
                5 => {
                    let runtime = app.chat_runtime_mut(target).unwrap();
                    runtime.visible.store(false, Ordering::Relaxed);
                    runtime.recovery.plain_copy.clear();
                }
                6 => {
                    let runtime = app.chat_runtime_mut(target).unwrap();
                    let handle = Arc::new(ClientHandle::fixture(
                        runtime.session.clone(),
                        (*runtime.snapshot).clone(),
                    ));
                    runtime.connection = Connection::Live(handle);
                }
                7 => {
                    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot)
                        .source_incarnation = Some(uuid::Uuid::new_v4());
                }
                _ => {
                    Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot)
                        .generation += 1;
                }
            }
            drop(app.handle_chat_ui(Action::PlainCopyReady(target, id, reply)));
            assert_eq!(
                app.chat_runtime_mut(target).unwrap().copied.is_some(),
                change == 0,
                "only exact current selected source accepts copy; unrelated generation alone is valid"
            );
        }
    }
    #[tokio::test]
    async fn saturated_readers_do_not_pin_cancelled_queued_message_generations() {
        let first = originals::readers().clone().acquire_owned().await.unwrap();
        let second = originals::readers().clone().acquire_owned().await.unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let runtime = runtime();
        let target = runtime.target;
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
        let mut streams = Vec::new();
        let mut owners = Vec::new();
        for generation in 0..20 {
            let source = item(
                chat::Role::Assistant,
                &format!(
                    "generation {generation}: {}",
                    "large source ".repeat(20_000)
                ),
            );
            owners.push(Arc::downgrade(&source));
            Arc::make_mut(&mut app.chat_runtime_mut(target).unwrap().snapshot).items = vec![source];
            let task = app.handle_chat_ui(Action::Copy(
                target,
                "same-opaque-message".into(),
                CopyPart::PlainText,
            ));
            let mut stream = iced::runtime::task::into_stream(task).unwrap();
            assert!(matches!(
                iced::futures::poll!(stream.next()),
                std::task::Poll::Pending
            ));
            streams.push(stream);
        }
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.recovery.plain_copy.clear();
        Arc::make_mut(&mut runtime.snapshot).items.clear();
        struct Signal(std::sync::mpsc::Sender<()>);
        impl Drop for Signal {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let (sent, received) = std::sync::mpsc::channel();
        retire(Signal(sent));
        tokio::task::spawn_blocking(move || received.recv_timeout(Duration::from_secs(5)).unwrap())
            .await
            .unwrap();
        assert!(
            owners.iter().all(|owner| owner.upgrade().is_none()),
            "all withdrawn source owners retire before semaphore admission or 40s timeout"
        );
        assert_eq!(
            originals::readers().available_permits(),
            0,
            "the readers stayed saturated throughout withdrawal"
        );
        drop(streams);
        drop(first);
        drop(second);
    }

    #[tokio::test]
    async fn newer_markdown_literal_or_other_card_copy_supersedes_ready_plain_text() {
        for newer in 0..3 {
            let mut app = crate::app::tests::context_inspector_test_app(false);
            let mut current = runtime();
            let target = current.target;
            Arc::make_mut(&mut current.snapshot).items =
                vec![item(chat::Role::Assistant, "**original formatting**")];
            app.cards
                .insert(target.card, CardRuntime::Chat(Box::new(current)));
            let (_, id, reply) = ready(app.handle_chat_ui(Action::Copy(
                target,
                "same-opaque-message".into(),
                CopyPart::PlainText,
            )))
            .await;
            let next = match newer {
                0 => Action::Copy(target, "same-opaque-message".into(), CopyPart::Text),
                1 => Action::CopyLiteral("the newer literal code copy".into()),
                _ => {
                    let mut other = runtime();
                    let other_target = other.target;
                    Arc::make_mut(&mut other.snapshot).items =
                        vec![item(chat::Role::Assistant, "newer other-card Markdown")];
                    app.cards
                        .insert(other_target.card, CardRuntime::Chat(Box::new(other)));
                    Action::Copy(other_target, "same-opaque-message".into(), CopyPart::Text)
                }
            };
            drop(app.handle_chat_ui(next));
            let copied = app.chat_runtime_mut(target).unwrap().copied.clone();
            drop(app.handle_chat_ui(Action::PlainCopyReady(target, id, reply)));
            assert_eq!(
                app.chat_runtime_mut(target).unwrap().copied,
                copied,
                "older ready output must not initiate a clipboard write after newer intent"
            );
        }
    }

    #[test]
    fn literal_user_copy_and_oversized_input_have_explicit_semantics() {
        assert_eq!(
            format(&item(chat::Role::User, "**literal user syntax**")).unwrap(),
            "**literal user syntax**"
        );
        let oversized = item(chat::Role::Assistant, &"x".repeat(LIMIT + 1));
        assert!(format(&oversized).unwrap_err().contains("32 MiB"));
    }
    #[test]
    fn abandoned_source_and_reply_slots_retire_actual_owners_and_permits_away_from_ui() {
        struct Block {
            entered: std::sync::mpsc::Sender<std::thread::ThreadId>,
            release: std::sync::mpsc::Receiver<()>,
        }
        impl Drop for Block {
            fn drop(&mut self) {
                let _ = self.entered.send(std::thread::current().id());
                let _ = self.release.recv_timeout(Duration::from_secs(5));
            }
        }
        struct Signal(std::sync::mpsc::Sender<()>);
        impl Drop for Signal {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let ui = std::thread::current().id();
        let (entered, seen) = std::sync::mpsc::channel();
        let (release, resume) = std::sync::mpsc::channel();
        retire(Block {
            entered,
            release: resume,
        });
        assert_ne!(seen.recv_timeout(Duration::from_secs(5)).unwrap(), ui);
        let source = item(chat::Role::Assistant, "last source owner");
        let weak = Arc::downgrade(&source);
        let input = InputSlot {
            source: Mutex::new(Some(source)),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        drop(input);
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let permit = permits.clone().try_acquire_owned().unwrap();
        let text = "queued completed output".to_owned();
        let bytes = OutputBytes::new(text.capacity());
        let reply = Reply::new(Ok(Output {
            text,
            _permit: permit,
            _bytes: bytes,
        }));
        let last = reply.clone();
        drop(reply);
        drop(last);
        assert!(
            weak.upgrade().is_some(),
            "body remains owned until the reclaimer actually runs"
        );
        assert_eq!(
            permits.available_permits(),
            0,
            "queued completed output retains admission through retirement"
        );
        let (done, wait) = std::sync::mpsc::channel();
        retire(Signal(done));
        release.send(()).unwrap();
        wait.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(weak.upgrade().is_none());
        assert_eq!(permits.available_permits(), 1);
    }
}
