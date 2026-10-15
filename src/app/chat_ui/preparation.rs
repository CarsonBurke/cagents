//! Owned Markdown work. Renderer caches and reader interaction stay on the UI.
use super::*;
use std::sync::atomic::AtomicUsize;

const SOURCE_BYTES: usize = 32 * 1024 * 1024;
const QUEUED_BYTES: usize = 72 * 1024 * 1024;
const QUEUED_ITEMS: usize = 128;
static RESERVED: AtomicUsize = AtomicUsize::new(0);
static ATTEMPT: AtomicU64 = AtomicU64::new(1);

fn next_attempt() -> u64 {
    ATTEMPT.fetch_add(1, Ordering::Relaxed)
}

#[derive(Clone)]
struct Request {
    item: Arc<chat::Item>,
    growing: bool,
    version: u64,
    theme: crate::settings::EditorSyntaxTheme,
}

#[derive(Clone, Default)]
pub(super) struct Parsed {
    pub(super) source: markdown_source::Source,
    pub(super) markdown: markdown::Content,
}

// A compiler-enforced ownership contract; no unsafe Send implementation and
// no promise that a parser can be read concurrently (its Text contains cells).
const _: fn() = || {
    fn send<T: Send>() {}
    send::<Parsed>();
    send::<prose::CodeBlock>();
};

#[derive(Default)]
struct Canonical {
    parsed: Parsed,
    blocks: HashMap<u64, prose::CodeBlock>,
}

#[derive(Default)]
struct Lease(AtomicUsize);
impl Lease {
    fn resize(&self, bytes: usize) -> bool {
        let old = self.0.load(Ordering::Relaxed);
        if bytes > old {
            let extra = bytes - old;
            if RESERVED
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |total| {
                    total
                        .checked_add(extra)
                        .filter(|total| *total <= QUEUED_BYTES)
                })
                .is_err()
            {
                return false;
            }
        } else {
            RESERVED.fetch_sub(old - bytes, Ordering::Relaxed);
        }
        self.0.store(bytes, Ordering::Relaxed);
        true
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        RESERVED.fetch_sub(self.0.load(Ordering::Relaxed), Ordering::Relaxed);
    }
}

struct Shared {
    latest: Mutex<Request>,
    canonical: Mutex<Option<Canonical>>,
    cancelled: AtomicBool,
    lease: Lease,
}
impl Shared {
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

struct Entry {
    shared: Arc<Shared>,
    run: u64,
    running: bool,
}

/// Only selected transcript sources are queued. Inputs are shared immutable
/// items, not copied text; source-byte reservations are not RSS estimates.
#[derive(Default)]
pub(super) struct State {
    context: Option<(Target, Option<String>, Option<String>)>,
    entries: HashMap<String, Entry>,
    errors: HashMap<String, (std::sync::Weak<chat::Item>, String)>,
}
impl Drop for State {
    fn drop(&mut self) {
        self.clear();
    }
}

fn same_kind(previous: &chat::Item, current: &chat::Item) -> bool {
    if previous.role != current.role {
        return false;
    }
    match (&previous.content, &current.content) {
        (
            ItemContent::Text {
                thinking: a,
                phase: x,
                ..
            },
            ItemContent::Text {
                thinking: b,
                phase: y,
                ..
            },
        ) => a == b && x == y,
        (ItemContent::Delivery { kind: a, .. }, ItemContent::Delivery { kind: b, .. }) => a == b,
        _ => false,
    }
}

impl State {
    pub(super) fn clear(&mut self) {
        for (_, entry) in self.entries.drain() {
            entry.shared.cancel();
            retire_shared(entry.shared);
        }
        self.errors.clear();
        self.context = None;
    }
    pub(super) fn context(
        &mut self,
        target: Target,
        session: Option<&str>,
        selected: Option<&str>,
        items: &[Arc<chat::Item>],
    ) -> bool {
        let context = (
            target,
            session.map(str::to_owned),
            selected.map(str::to_owned),
        );
        let changed = self.context.as_ref() != Some(&context);
        if changed {
            self.clear();
            self.context = Some(context);
        }
        let ids: HashSet<_> = items.iter().map(|item| item.id.as_str()).collect();
        self.entries.retain(|id, entry| {
            let retain = ids.contains(id.as_str());
            if !retain {
                entry.shared.cancel();
                retire_shared(entry.shared.clone());
            }
            retain
        });
        self.errors.retain(|id, _| ids.contains(id.as_str()));
        changed
    }
    fn remove(&mut self, id: &str) {
        if let Some(entry) = self.entries.remove(id) {
            retire_shared(entry.shared);
        }
    }
    pub(super) fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }
    pub(super) fn queued_bytes(&self) -> usize {
        self.entries
            .values()
            .map(|entry| entry.shared.lease.0.load(Ordering::Relaxed))
            .sum()
    }
    pub(super) fn global_queued_bytes() -> usize {
        RESERVED.load(Ordering::Relaxed)
    }
    pub(super) fn error(&self) -> Option<&str> {
        self.errors.values().next().map(|(_, error)| error.as_str())
    }
    pub(super) fn retry(&mut self) {
        self.errors.clear();
    }

    /// `append` is proof from the immutable source publication, not a tail
    /// digest or a guessed matching item ID. An unproven replacement starts a
    /// new lineage and cancels both queued and currently parsing old input.
    pub(super) fn stage(
        &mut self,
        item: &Arc<chat::Item>,
        growing: bool,
        theme: crate::settings::EditorSyntaxTheme,
        append: impl Fn(&Arc<chat::Item>, &Arc<chat::Item>) -> bool,
    ) -> bool {
        let Some(body) = CachedItem::prose(&item.content) else {
            return false;
        };
        if self
            .errors
            .get(&item.id)
            .is_some_and(|(old, _)| old.as_ptr() == Arc::as_ptr(item))
        {
            return false;
        }
        self.errors.remove(&item.id);
        if body.len() > SOURCE_BYTES {
            if let Some(entry) = self.entries.get(&item.id) {
                entry.shared.cancel();
            }
            self.remove(&item.id);
            self.errors.insert(
                item.id.clone(),
                (
                    Arc::downgrade(item),
                    "This message is too large to display.".into(),
                ),
            );
            return true;
        }
        if let Some(entry) = self.entries.get_mut(&item.id) {
            let mut latest = entry
                .shared
                .latest
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let same = Arc::ptr_eq(&latest.item, item);
            if same && latest.growing == growing && latest.theme == theme {
                return false;
            }
            if latest.theme == theme
                && (same || (same_kind(&latest.item, item) && append(&latest.item, item)))
            {
                if entry.shared.lease.resize(body.len()) {
                    *latest = Request {
                        item: item.clone(),
                        growing,
                        version: latest.version.wrapping_add(1),
                        theme,
                    };
                    if !entry.running {
                        entry.run = next_attempt();
                        entry.running = true;
                    }
                    return false;
                }
            }
            entry.shared.cancel();
        }
        self.remove(&item.id);
        let lease = Lease::default();
        if self.entries.len() >= QUEUED_ITEMS || !lease.resize(body.len()) {
            self.errors.insert(item.id.clone(), (Arc::downgrade(item), "Some messages could not be loaded. Retry when another message has finished loading.".into()));
            return true;
        }
        self.entries.insert(
            item.id.clone(),
            Entry {
                run: next_attempt(),
                running: true,
                shared: Arc::new(Shared {
                    latest: Mutex::new(Request {
                        item: item.clone(),
                        growing,
                        version: 1,
                        theme,
                    }),
                    canonical: Mutex::new(None),
                    cancelled: AtomicBool::new(false),
                    lease,
                }),
            },
        );
        true
    }

    pub(super) fn subscriptions(&self, target: Target) -> Vec<Subscription<Message>> {
        self.entries
            .iter()
            .filter(|(_, entry)| entry.running)
            .map(|(id, entry)| {
                Subscription::run_with(
                    Job {
                        target,
                        id: id.clone(),
                        run: entry.run,
                        shared: entry.shared.clone(),
                    },
                    stream,
                )
            })
            .collect()
    }

    pub(super) fn accept(
        &mut self,
        target: Target,
        id: &str,
        run: u64,
        ready: Ready,
    ) -> Option<Prepared> {
        let entry = self.entries.get_mut(id)?;
        if entry.run != run
            || self
                .context
                .as_ref()
                .is_none_or(|(current, _, _)| *current != target)
            || entry.shared.cancelled.load(Ordering::Relaxed)
        {
            return None;
        }
        let result = ready.0.0.lock().unwrap_or_else(|e| e.into_inner()).take()?;
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                let item = entry
                    .shared
                    .latest
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .item
                    .clone();
                entry.shared.cancel();
                self.remove(id);
                self.errors
                    .insert(id.into(), (Arc::downgrade(&item), error));
                return None;
            }
        };
        let latest = entry
            .shared
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if latest.version != prepared.request.version {
            // Every intervening transition preserved this exact append
            // lineage. Publish useful progress and coalesce one newer job.
            entry.run = next_attempt();
            entry.running = true;
        } else {
            entry.running = false;
        }
        let finished = !latest.growing && latest.version == prepared.request.version;
        drop(latest);
        if finished {
            self.remove(id);
        }
        Some(prepared)
    }
}

pub(super) struct Prepared {
    request: Request,
    pub(super) parsed: Parsed,
    blocks: HashMap<u64, prose::CodeBlock>,
    palettes: prose::Palettes,
    #[cfg(test)]
    syntax_work: (usize, usize),
    // Retain the source lease through queued output as well as execution.
    // Evicting State cannot make the global counter forget this input owner.
    _owner: Arc<Shared>,
    // Hold admission through UI consumption: completed outputs cannot build
    // an unbounded queue while a busy UI has not handled their messages yet.
    _permit: tokio::sync::OwnedSemaphorePermit,
}
impl Prepared {
    pub(super) fn item(&self) -> &Arc<chat::Item> {
        &self.request.item
    }
    pub(super) fn growing(&self) -> bool {
        self.request.growing
    }
    pub(super) fn discard(self) {
        retire(Retired {
            _parsed: self.parsed,
            _blocks: self.blocks,
            _palettes: Some(self.palettes),
            _item: Some(self.request.item),
            _owner: self._owner,
            _permit: self._permit,
        });
    }
    pub(super) fn install(self, cached: &mut CachedItem, managed: bool) {
        // Move the old owned parser containers out before replacing them:
        // their destructors may visit thousands of blocks and free code.
        let retired = Retired {
            _parsed: Parsed {
                source: std::mem::replace(&mut cached.source, self.parsed.source),
                markdown: std::mem::replace(&mut cached.markdown, self.parsed.markdown),
            },
            _blocks: std::mem::replace(&mut cached.blocks, self.blocks),
            _palettes: std::mem::replace(&mut cached.palette_views, Some(self.palettes)),
            _item: cached.item.replace(self.request.item.clone()),
            _owner: self._owner,
            _permit: self._permit,
        };
        cached.token = ChangeToken::of(&self.request.item.content);
        cached.preparing = self.request.growing || managed;
        cached.placeholder = false;
        cached.open = None;
        cached.revision = CachedItem::next_revision();
        retire(retired);
    }
}

/// A single background reclaimer drops only Send data. Installed output
/// retirement retains one of the two global parser/output permits; detached
/// canonical owners retain their existing source/item admission until dropped.
/// Reader Rc state and renderer trees never leave the event thread.
struct Retired {
    _parsed: Parsed,
    _blocks: HashMap<u64, prose::CodeBlock>,
    _palettes: Option<prose::Palettes>,
    _item: Option<Arc<chat::Item>>,
    _owner: Arc<Shared>,
    // Declared last so retirement completes before the next parser is admitted.
    _permit: tokio::sync::OwnedSemaphorePermit,
}
enum Retirement {
    Installed(Retired),
    // Keep the existing queued-source lease until canonical parser/source
    // destructors finish, including cancelled/hidden sources without output.
    Detached(Arc<Shared>),
    #[cfg(test)]
    Barrier(std::sync::mpsc::Sender<std::thread::ThreadId>),
}
fn retire_shared(shared: Arc<Shared>) {
    enqueue_retirement(Retirement::Detached(shared));
}
fn retire(retired: Retired) {
    enqueue_retirement(Retirement::Installed(retired));
}
fn enqueue_retirement(retired: Retirement) {
    static RETIRE: std::sync::OnceLock<std::sync::mpsc::Sender<Retirement>> =
        std::sync::OnceLock::new();
    let sender = RETIRE.get_or_init(|| {
        let (sender, receiver) = std::sync::mpsc::channel::<Retirement>();
        std::thread::Builder::new()
            .name("chat-markdown-retire".into())
            .spawn(move || {
                while let Ok(retired) = receiver.recv() {
                    match retired {
                        Retirement::Installed(retired) => drop(retired),
                        Retirement::Detached(owner) => drop(owner),
                        #[cfg(test)]
                        Retirement::Barrier(reply) => {
                            let _ = reply.send(std::thread::current().id());
                        }
                    }
                }
            })
            .expect("start Markdown retirement worker");
        sender
    });
    // The receiver only drops owned standard containers and never exits.
    // An exceptional worker failure still releases ownership safely.
    if let Err(error) = sender.send(retired) {
        drop(error.0);
    }
}

#[cfg(test)]
fn retirement_barrier() -> std::thread::ThreadId {
    let (sender, receiver) = std::sync::mpsc::channel();
    enqueue_retirement(Retirement::Barrier(sender));
    receiver.recv().expect("retirement worker remains alive")
}

fn prime_plain(items: &[markdown::Item], cancelled: &AtomicBool) -> Result<(), String> {
    for item in items {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Message loading was cancelled.".into());
        }
        match item {
            markdown::Item::Paragraph(text) | markdown::Item::Heading(_, text) => {
                text.plain_prefixes();
            }
            markdown::Item::Image { alt, .. } => {
                alt.plain_prefixes();
            }
            markdown::Item::Quote(items) => prime_plain(items, cancelled)?,
            markdown::Item::List { bullets, .. } => {
                for bullet in bullets {
                    let (markdown::Bullet::Point { items } | markdown::Bullet::Task { items, .. }) =
                        bullet;
                    prime_plain(items, cancelled)?;
                }
            }
            markdown::Item::Table { columns, rows } => {
                for column in columns {
                    prime_plain(&column.header, cancelled)?;
                }
                for row in rows {
                    for cell in &row.cells {
                        prime_plain(cell, cancelled)?;
                    }
                }
            }
            _ => {}
        }
    }
    Ok(())
}

struct ReplySlot(Mutex<Option<Result<Prepared, String>>>);
impl Drop for ReplySlot {
    fn drop(&mut self) {
        if let Some(Ok(prepared)) = self
            .0
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            prepared.discard();
        }
    }
}
#[derive(Clone)]
pub(super) struct Ready(Arc<ReplySlot>);
impl Ready {
    fn new(result: Result<Prepared, String>) -> Self {
        Self(Arc::new(ReplySlot(Mutex::new(Some(result)))))
    }
}
impl std::fmt::Debug for Ready {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PreparedMarkdown")
    }
}

#[derive(Clone)]
struct Job {
    target: Target,
    id: String,
    run: u64,
    shared: Arc<Shared>,
}
impl PartialEq for Job {
    fn eq(&self, other: &Self) -> bool {
        self.target == other.target && self.id == other.id && self.run == other.run
    }
}
impl Eq for Job {}
impl std::hash::Hash for Job {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        use std::hash::Hash;
        std::any::TypeId::of::<Self>().hash(state);
        self.target.hash(state);
        self.id.hash(state);
        self.run.hash(state);
    }
}
fn workers() -> &'static Arc<tokio::sync::Semaphore> {
    static WORKERS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    WORKERS.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(2)))
}
fn stream(job: &Job) -> Pin<Box<dyn iced::futures::Stream<Item = Message> + Send>> {
    let job = job.clone();
    Box::pin(iced::futures::stream::once(async move {
        let result = run(job.shared).await;
        action(Action::MarkdownPrepared(
            job.target,
            job.id,
            job.run,
            Ready::new(result),
        ))
    }))
}
async fn run(shared: Arc<Shared>) -> Result<Prepared, String> {
    let permit = workers()
        .clone()
        .acquire_owned()
        .await
        .map_err(|_| "Message loading stopped.".to_owned())?;
    if shared.cancelled.load(Ordering::Relaxed) {
        return Err("Message loading was cancelled.".into());
    }
    tokio::task::spawn_blocking(move || execute(shared, permit))
        .await
        .map_err(|_| "A message could not be loaded. Retry loading it.".to_owned())?
}

fn execute(
    shared: Arc<Shared>,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<Prepared, String> {
    let request = shared
        .latest
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let mut canonical = shared
        .canonical
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_default();
    let body = CachedItem::prose(&request.item.content).ok_or("The source is not Markdown.")?;
    parse_publication(
        &mut canonical.parsed,
        body,
        request.growing,
        &shared.cancelled,
    )?;
    prime_plain(canonical.parsed.markdown.items(), &shared.cancelled)?;
    let mut previous = std::mem::take(&mut canonical.blocks);
    #[cfg(test)]
    let mut syntax_work = (0, 0);
    for_each_fence(canonical.parsed.markdown.items(), &mut |language, code| {
        if shared.cancelled.load(Ordering::Relaxed) || code.len() > CachedItem::HIGHLIGHT_LIMIT {
            return;
        }
        let key = fence_key(language.unwrap_or_default(), code);
        canonical.blocks.entry(key).or_insert_with(|| {
            previous.remove(&key).unwrap_or_else(|| {
                #[cfg(test)]
                {
                    syntax_work.0 += 1;
                    syntax_work.1 += code.len();
                }
                prose::CodeBlock::new(
                    language.and_then(prose::fence_language),
                    code,
                    request.theme,
                )
            })
        });
    });
    if shared.cancelled.load(Ordering::Relaxed) {
        return Err("Message loading was cancelled.".into());
    }
    let (parsed, blocks) = if request.growing {
        // Cloning happens deliberately on the worker. Unchanged syntax
        // palettes, fenced bodies and inline spans share immutable Arc
        // backing. Parser continuation/raw-normalizer copies remain an
        // explicit O(source) residual.
        let parsed = canonical.parsed.clone();
        let blocks = canonical
            .blocks
            .iter()
            .map(|(key, block)| (*key, block.settled_clone()))
            .collect();
        if shared.cancelled.load(Ordering::Relaxed) {
            return Err("Message loading was cancelled.".into());
        }
        *shared.canonical.lock().unwrap_or_else(|e| e.into_inner()) = Some(canonical);
        (parsed, blocks)
    } else {
        (canonical.parsed, canonical.blocks)
    };
    let palettes = prose::palettes(&blocks);
    Ok(Prepared {
        request,
        parsed,
        blocks,
        palettes,
        #[cfg(test)]
        syntax_work,
        _owner: shared,
        _permit: permit,
    })
}

/// Rendering fixtures explicitly complete the production engine before
/// inspecting widgets. Production scheduling has no test-specific bypass.
#[cfg(test)]
pub(super) fn drain_fixture(runtime: &mut Runtime) {
    let permits = Arc::new(tokio::sync::Semaphore::new(1));
    for _ in 0..512 {
        let Some((id, run, shared)) = runtime
            .preparation
            .entries
            .iter()
            .find(|(_, entry)| entry.running)
            .map(|(id, entry)| (id.clone(), entry.run, entry.shared.clone()))
        else {
            return;
        };
        let permit = iced::futures::executor::block_on(permits.clone().acquire_owned())
            .expect("fixture retirement releases the same real admission");
        let ready = Ready::new(execute(shared, permit));
        runtime.markdown_prepared(runtime.target, &id, run, ready);
    }
    panic!("fixture preparation did not quiesce");
}

/// Parse one coalesced publication once. The parser can reparse an unfinished
/// rich block on append; feeding a cold body in artificial chunks amplifies
/// that cost. Cancellation fences ownership before and after the parser call;
/// it does not preempt pulldown-cmark inside a single rich block.
fn parse_publication(
    parsed: &mut Parsed,
    body: &str,
    growing: bool,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("Message loading was cancelled.".into());
    }
    match parsed.source.advance_append(body, growing) {
        markdown_source::Step::Append(text) => parsed.markdown.push_str(&text),
        markdown_source::Step::Restart(text) => parsed.markdown = markdown::Content::parse(&text),
    }
    if cancelled.load(Ordering::Relaxed) {
        return Err("Message loading was cancelled.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn item(body: &str) -> Arc<chat::Item> {
        Arc::new(chat::Item {
            id: "message".into(),
            role: chat::Role::Assistant,
            started_at: None,
            at: None,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
        })
    }
    fn target() -> Target {
        Target {
            card: CardId::new(),
            generation: 1,
        }
    }
    fn configured(target: Target, item: &Arc<chat::Item>) -> State {
        let mut state = State::default();
        state.context(target, Some("session"), None, std::slice::from_ref(item));
        state
    }
    fn job(state: &State) -> (u64, Arc<Shared>) {
        let entry = &state.entries["message"];
        (entry.run, entry.shared.clone())
    }
    fn permit() -> tokio::sync::OwnedSemaphorePermit {
        Arc::new(tokio::sync::Semaphore::new(1))
            .try_acquire_owned()
            .unwrap()
    }
    fn ready(prepared: Result<Prepared, String>) -> Ready {
        Ready::new(prepared)
    }

    #[test]
    fn owned_parser_moves_across_thread_and_matches_whole_document() {
        let body = format!(
            "# Owned\n\n[ref]: https://example.com\n\n```rust\n{}\n```\n\n| a | b |\n| - | - |\n| [reference][ref] | 🦀 |\n",
            "let value = 42;\n".repeat(24_000)
        );
        let expected = {
            let mut source = markdown_source::Source::default();
            match source.advance(&body, false) {
                markdown_source::Step::Append(text) | markdown_source::Step::Restart(text) => {
                    markdown::Content::parse(&text)
                }
            }
        };
        let parsed = std::thread::spawn(move || {
            let mut parsed = Parsed::default();
            parse_publication(&mut parsed, &body, false, &AtomicBool::new(false)).unwrap();
            parsed
        })
        .join()
        .unwrap();
        assert_eq!(
            format!("{:?}", parsed.markdown.items()),
            format!("{:?}", expected.items())
        );
    }
    #[test]
    fn cold_rich_paragraph_is_parsed_once_and_syntax_is_ready_before_install() {
        let body = format!(
            "A *rich* [link](https://example.com) paragraph {} tail.",
            "emphasis *word* and `code` ".repeat(40_000)
        );
        let mut parsed = Parsed::default();
        parse_publication(&mut parsed, &body, false, &AtomicBool::new(false)).unwrap();
        assert_eq!(
            parsed.markdown.parsed_work_bytes(),
            body.len(),
            "cold input must not repeatedly reparse artificial prefixes"
        );
        let body = (0..4_000)
            .map(|index| format!("```rust\nlet value_{index} = {index};\n```\n\n"))
            .collect::<String>();
        let source = item(&body);
        let target = target();
        let mut state = configured(target, &source);
        state.stage(
            &source,
            false,
            crate::settings::EditorSyntaxTheme::Mocha,
            |_, _| false,
        );
        let (_, shared) = job(&state);
        let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        assert_eq!(output.blocks.len(), 4_000);
        let key = fence_key("rust", "let value_123 = 123;\n");
        let expected = prose::CodeBlock::new(
            Some("rust"),
            "let value_123 = 123;\n",
            crate::settings::EditorSyntaxTheme::Mocha,
        );
        let dark = output.blocks[&key].spans(true).clone();
        let light = output.blocks[&key].spans(false).clone();
        assert_eq!(format!("{dark:?}"), format!("{:?}", expected.spans(true)));
        assert_eq!(format!("{light:?}"), format!("{:?}", expected.spans(false)));
        let mut cached = CachedItem::preparing(&source, crate::settings::EditorSyntaxTheme::Mocha);
        output.install(&mut cached, false);
        assert!(
            Arc::ptr_eq(&dark, cached.blocks[&key].spans(true)),
            "installation moves the worker's spans without re-highlighting"
        );
        assert!(Arc::ptr_eq(&light, cached.blocks[&key].spans(false)));
        assert!(!cached.placeholder);
    }
    #[test]
    fn streaming_tail_reuses_finished_fence_highlights_without_new_syntax_work() {
        let mut body = (0..200)
            .map(|index| format!("```rust\nlet value_{index} = {index};\n```\n\n"))
            .collect::<String>();
        body.push_str("A growing tail");
        let first = item(&body);
        let target = target();
        let mut state = configured(target, &first);
        let theme = crate::settings::EditorSyntaxTheme::Mocha;
        state.stage(&first, true, theme, |_, _| false);
        let (run, shared) = job(&state);
        let output = execute(shared, permit()).unwrap();
        assert_eq!(output.syntax_work.0, 200);
        let key = fence_key("rust", "let value_123 = 123;\n");
        let spans = output.blocks[&key].spans(true).clone();
        drop(
            state
                .accept(target, "message", run, ready(Ok(output)))
                .unwrap(),
        );
        for _ in 0..5 {
            body.push_str(" more words");
            let next = item(&body);
            state.stage(&next, true, theme, |_, _| true);
            let (run, shared) = job(&state);
            let output = execute(shared, permit()).unwrap();
            assert_eq!(
                output.syntax_work,
                (0, 0),
                "a tiny tail append does not highlight completed history again"
            );
            assert!(Arc::ptr_eq(&spans, output.blocks[&key].spans(true)));
            drop(
                state
                    .accept(target, "message", run, ready(Ok(output)))
                    .unwrap(),
            );
        }
    }
    #[test]
    fn worker_primes_plain_for_all_nested_logical_leaves() {
        fn verify(items: &[markdown::Item]) -> usize {
            items
                .iter()
                .map(|item| match item {
                    markdown::Item::Paragraph(text) | markdown::Item::Heading(_, text) => {
                        assert!(text.plain_ready());
                        1
                    }
                    markdown::Item::Image { alt, .. } => {
                        assert!(alt.plain_ready());
                        1
                    }
                    markdown::Item::Quote(items) => verify(items),
                    markdown::Item::List { bullets, .. } => bullets
                        .iter()
                        .map(|bullet| {
                            let (markdown::Bullet::Point { items }
                            | markdown::Bullet::Task { items, .. }) = bullet;
                            verify(items)
                        })
                        .sum(),
                    markdown::Item::Table { columns, rows } => {
                        columns
                            .iter()
                            .map(|column| verify(&column.header))
                            .sum::<usize>()
                            + rows
                                .iter()
                                .flat_map(|row| &row.cells)
                                .map(|cell| verify(cell))
                                .sum::<usize>()
                    }
                    _ => 0,
                })
                .sum()
        }
        let source = item(
            "# Heading\n\nA *rich* paragraph.\n\n> A quote.\n\n- A list item.\n\n| heading | second |\n| --- | --- |\n| cell | other |\n\n![image alt](https://example.com/image.png)\n",
        );
        let target = target();
        let mut state = configured(target, &source);
        state.stage(
            &source,
            true,
            crate::settings::EditorSyntaxTheme::default(),
            |_, _| false,
        );
        let (_, shared) = job(&state);
        let output = execute(shared.clone(), permit()).unwrap();
        assert!(verify(output.parsed.markdown.items()) >= 8);
        assert!(
            verify(
                shared
                    .canonical
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .parsed
                    .markdown
                    .items()
            ) >= 8
        );
    }
    #[test]
    fn installation_retires_old_owned_document_before_worker_readmission() {
        let old = item(&"```rust\nlet old = 1;\n```\n\n".repeat(2_000));
        let weak = Arc::downgrade(&old);
        let mut cached =
            CachedItem::new(&old.content, crate::settings::EditorSyntaxTheme::default());
        cached.item = Some(old.clone());
        drop(old);
        let next = item("Replacement document.");
        let target = target();
        let mut state = configured(target, &next);
        state.stage(
            &next,
            false,
            crate::settings::EditorSyntaxTheme::default(),
            |_, _| false,
        );
        let (_, shared) = job(&state);
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let output = execute(shared, permits.clone().try_acquire_owned().unwrap()).unwrap();
        output.install(&mut cached, false);
        let admitted = iced::futures::executor::block_on(permits.clone().acquire_owned()).unwrap();
        assert!(
            weak.upgrade().is_none(),
            "old input is released before replacement parsing can resume"
        );
        assert_ne!(
            retirement_barrier(),
            std::thread::current().id(),
            "destructors run on the retirement thread"
        );
        assert_eq!(cached.markdown.items().len(), 1);
        drop(admitted);
    }
    #[test]
    fn syntax_theme_change_rejects_old_worker_palette_without_losing_input() {
        let source = item("```rust\nlet value = 42;\n```\n");
        let target = target();
        let mut state = configured(target, &source);
        state.stage(
            &source,
            false,
            crate::settings::EditorSyntaxTheme::Mocha,
            |_, _| false,
        );
        let (old, shared) = job(&state);
        let output = ready(execute(shared, permit()));
        state.stage(
            &source,
            false,
            crate::settings::EditorSyntaxTheme::Ocean,
            |_, _| false,
        );
        assert!(state.accept(target, "message", old, output).is_none());
        let (run, shared) = job(&state);
        let output = execute(shared, permit()).unwrap();
        assert_eq!(
            output.request.theme,
            crate::settings::EditorSyntaxTheme::Ocean
        );
        assert!(
            state
                .accept(target, "message", run, ready(Ok(output)))
                .is_some()
        );
    }
    #[test]
    fn queued_inputs_coalesce_and_progressive_append_results_do_not_starve() {
        let target = target();
        let first = item("First prefix");
        let latest = item("First prefix and newer suffix");
        let mut state = configured(target, &first);
        state.stage(
            &first,
            true,
            crate::settings::EditorSyntaxTheme::default(),
            |_, _| false,
        );
        let (run, shared) = job(&state);
        state.stage(
            &latest,
            true,
            crate::settings::EditorSyntaxTheme::default(),
            |old, new| Arc::ptr_eq(old, &first) && Arc::ptr_eq(new, &latest),
        );
        let output = execute(shared, permit()).unwrap();
        assert!(Arc::ptr_eq(output.item(), &latest));
        let newest = item("First prefix and newer suffix plus another append");
        state.stage(
            &newest,
            true,
            crate::settings::EditorSyntaxTheme::default(),
            |old, new| Arc::ptr_eq(old, &latest) && Arc::ptr_eq(new, &newest),
        );
        let accepted = state
            .accept(target, "message", run, ready(Ok(output)))
            .unwrap();
        assert!(
            Arc::ptr_eq(accepted.item(), &latest),
            "a useful older proven prefix can be displayed"
        );
        drop(accepted);
        let (next, shared) = job(&state);
        assert_ne!(run, next);
        let output = execute(shared, permit()).unwrap();
        assert!(Arc::ptr_eq(output.item(), &newest));
        assert!(
            state
                .accept(target, "message", next, ready(Ok(output)))
                .is_some()
        );
        assert!(!state.entries["message"].running);
    }
    #[test]
    fn replacements_and_context_changes_reject_completed_old_outputs() {
        let target = target();
        let original = item("old text");
        let replacement = item("new text");
        for change_context in [false, true] {
            let mut state = configured(target, &original);
            state.stage(
                &original,
                false,
                crate::settings::EditorSyntaxTheme::default(),
                |_, _| false,
            );
            let (run, shared) = job(&state);
            let output = ready(execute(shared.clone(), permit()));
            if change_context {
                state.context(
                    target,
                    Some("other-session"),
                    Some("child"),
                    std::slice::from_ref(&replacement),
                );
            }
            state.stage(
                &replacement,
                false,
                crate::settings::EditorSyntaxTheme::default(),
                |_, _| false,
            );
            assert!(shared.cancelled.load(Ordering::Relaxed));
            assert!(state.accept(target, "message", run, output).is_none());
            let (run, shared) = job(&state);
            let output = execute(shared, permit()).unwrap();
            assert!(Arc::ptr_eq(output.item(), &replacement));
            assert!(
                state
                    .accept(target, "message", run, ready(Ok(output)))
                    .is_some()
            );
        }
    }
    #[test]
    fn output_backpressure_and_hidden_eviction_release_actual_owners() {
        let target = target();
        let original = item("a retained body");
        let weak = Arc::downgrade(&original);
        let mut state = configured(target, &original);
        state.stage(
            &original,
            true,
            crate::settings::EditorSyntaxTheme::default(),
            |_, _| false,
        );
        let (run, shared) = job(&state);
        let permits = Arc::new(tokio::sync::Semaphore::new(1));
        let output = ready(execute(
            shared.clone(),
            permits.clone().try_acquire_owned().unwrap(),
        ));
        assert_eq!(
            permits.available_permits(),
            0,
            "queued output retains worker admission"
        );
        let output_owner = Arc::downgrade(&shared);
        drop(shared);
        state.clear();
        let owner = output_owner
            .upgrade()
            .expect("queued output retains source lease after eviction");
        assert!(owner.cancelled.load(Ordering::Relaxed));
        assert_eq!(
            owner.lease.0.load(Ordering::Relaxed),
            "a retained body".len()
        );
        drop(owner);
        assert!(state.accept(target, "message", run, output).is_none());
        let retired = iced::futures::executor::block_on(permits.clone().acquire_owned()).unwrap();
        assert!(
            output_owner.upgrade().is_none(),
            "retired output releases its source lease before readmission"
        );
        drop(retired);
        assert_eq!(
            permits.available_permits(),
            1,
            "discarded stale output releases admission"
        );
        drop(original);
        assert!(weak.upgrade().is_none());
        assert_eq!(state.queued_bytes(), 0);
        let mut parsed = Parsed::default();
        assert!(
            parse_publication(&mut parsed, "must not parse", false, &AtomicBool::new(true))
                .is_err()
        );
        assert!(parsed.markdown.items().is_empty());
    }
    #[test]
    fn cold_result_reaches_the_mounted_transcript_without_a_new_snapshot() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree},
        };
        struct Plain {
            selectable: Vec<String>,
            captions: Vec<String>,
        }
        impl Operation for Plain {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn text(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                _: iced::Rectangle,
                text: &str,
            ) {
                self.captions.push(text.into());
            }
            fn custom(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(state) = state.downcast_ref::<selectable_prose::State>() {
                    self.selectable.push(state.plain.to_string());
                }
            }
        }
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
                ..Snapshot::default()
            },
        ));
        let mut runtime = Runtime::new(
            target(),
            session,
            Connection::Live(handle.clone()),
            true,
            crate::settings::EditorSyntaxTheme::default(),
        );
        let source = item("Worker result reaches the mounted transcript.");
        handle.replace_snapshot_for_test(Snapshot {
            generation: 7,
            ready: true,
            items: vec![source],
            ..Snapshot::default()
        });
        runtime.refresh();
        assert!(runtime.cache["message"].preparing);
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut tree = Tree::empty();
        let mut rendered = |runtime: &Runtime| {
            let mut element = runtime.find_turn_view_for_test();
            tree.diff(element.as_widget_mut());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(640.0, 1000.0)),
            );
            let mut plain = Plain {
                selectable: vec![],
                captions: vec![],
            };
            element
                .as_widget_mut()
                .operate(&mut tree, Layout::new(&node), &renderer, &mut plain);
            plain
        };
        assert!(
            rendered(&runtime)
                .captions
                .iter()
                .any(|text| text.contains("Loading message"))
        );
        assert!(rendered(&runtime).selectable.is_empty());
        assert!(runtime.selection_document().0.is_empty());
        let generation = runtime.snapshot.generation;
        drain_fixture(&mut runtime);
        assert_eq!(runtime.snapshot.generation, generation);
        assert!(
            rendered(&runtime)
                .selectable
                .iter()
                .any(|text| text == "Worker result reaches the mounted transcript.")
        );
        assert!(!runtime.cache["message"].preparing);
        handle.replace_snapshot_for_test(Snapshot {
            generation: 8,
            ready: true,
            items: vec![item("A replacement has different Markdown context.")],
            ..Snapshot::default()
        });
        runtime.refresh();
        assert!(
            rendered(&runtime)
                .captions
                .iter()
                .any(|text| text.contains("Loading message")),
            "unproven replacement must not display old context"
        );
        drain_fixture(&mut runtime);
        assert!(
            rendered(&runtime)
                .selectable
                .iter()
                .any(|text| text == "A replacement has different Markdown context.")
        );
    }
    #[test]
    fn pending_status_is_not_copied_and_unrelated_selection_survives_ready() {
        use iced::advanced::{
            Layout, layout,
            widget::{Operation, Tree},
        };
        #[derive(Default)]
        struct Clipboard(String);
        impl iced::advanced::Clipboard for Clipboard {
            fn read(&self, _: iced::advanced::clipboard::Kind) -> Option<String> {
                Some(self.0.clone())
            }
            fn write(&mut self, _: iced::advanced::clipboard::Kind, text: String) {
                self.0 = text;
            }
        }
        #[derive(Default)]
        struct Bounds(Vec<(String, iced::Rectangle)>);
        impl Operation for Bounds {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn custom(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                bounds: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(state) = state.downcast_ref::<selectable_prose::State>() {
                    self.0.push((state.plain.to_string(), bounds));
                }
            }
        }
        fn interact(
            runtime: &Runtime,
            tree: &mut Tree,
            clipboard: &mut Clipboard,
            select: bool,
            keys: &[&str],
        ) {
            let (catalog, bookmark) = runtime.selection_document();
            let mut element = selection::transcript_virtualized(
                runtime.find_turn_view_for_test(),
                runtime.target,
                None,
                catalog,
                bookmark,
            );
            tree.diff(element.as_widget_mut());
            let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(640.0, 1000.0));
            let mut node = element.as_widget_mut().layout(tree, &renderer, &limits);
            let mut bounds = Bounds::default();
            element
                .as_widget_mut()
                .operate(tree, Layout::new(&node), &renderer, &mut bounds);
            let first = bounds.0.first().expect("existing ready prose").1;
            let start = iced::Point::new(first.x, first.y + 3.0);
            let end = iced::Point::new(first.x + first.width + 5.0, first.y + first.height - 1.0);
            let mut events = vec![];
            if select {
                events.extend([
                    (
                        iced::Event::Mouse(iced::mouse::Event::ButtonPressed(
                            iced::mouse::Button::Left,
                        )),
                        start,
                    ),
                    (
                        iced::Event::Mouse(iced::mouse::Event::CursorMoved { position: end }),
                        end,
                    ),
                    (
                        iced::Event::Mouse(iced::mouse::Event::ButtonReleased(
                            iced::mouse::Button::Left,
                        )),
                        end,
                    ),
                ]);
            }
            events.extend(keys.iter().map(|key| {
                (
                    iced::Event::Keyboard(keyboard::Event::KeyPressed {
                        key: keyboard::Key::Character((*key).into()),
                        modified_key: keyboard::Key::Character((*key).into()),
                        physical_key: keyboard::key::Physical::Unidentified(
                            keyboard::key::NativeCode::Unidentified,
                        ),
                        location: keyboard::Location::Standard,
                        modifiers: keyboard::Modifiers::CTRL,
                        text: None,
                        repeat: false,
                    }),
                    end,
                )
            }));
            for (event, pointer) in events {
                let mut messages = vec![];
                let mut shell = iced::advanced::Shell::new(&mut messages);
                element.as_widget_mut().update(
                    tree,
                    &event,
                    Layout::new(&node),
                    iced::mouse::Cursor::Available(pointer),
                    &renderer,
                    clipboard,
                    &mut shell,
                    &iced::Rectangle::with_size(iced::Size::new(640.0, 1000.0)),
                );
                if shell.is_layout_invalid() {
                    node = element.as_widget_mut().layout(tree, &renderer, &limits);
                }
            }
        }
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
                ..Snapshot::default()
            },
        ));
        let mut runtime = Runtime::new(
            target(),
            session,
            Connection::Live(handle.clone()),
            true,
            crate::settings::EditorSyntaxTheme::default(),
        );
        let stable = item("Stable selected text.");
        handle.replace_snapshot_for_test(Snapshot {
            generation: 1,
            ready: true,
            items: vec![stable.clone()],
            ..Snapshot::default()
        });
        runtime.refresh();
        drain_fixture(&mut runtime);
        let mut pending = (*item("Newly prepared answer.")).clone();
        pending.id = "pending".into();
        handle.replace_snapshot_for_test(Snapshot {
            generation: 2,
            ready: true,
            items: vec![stable, Arc::new(pending)],
            ..Snapshot::default()
        });
        runtime.refresh();
        assert!(runtime.cache["pending"].placeholder);
        let mut tree = Tree::empty();
        let mut clipboard = Clipboard::default();
        interact(&runtime, &mut tree, &mut clipboard, true, &["a", "c"]);
        assert_eq!(clipboard.0, "Stable selected text.");
        interact(&runtime, &mut tree, &mut clipboard, true, &["c"]);
        assert_eq!(clipboard.0, "Stable selected text.");
        drain_fixture(&mut runtime);
        interact(&runtime, &mut tree, &mut clipboard, false, &["c"]);
        assert_eq!(
            clipboard.0, "Stable selected text.",
            "ready must preserve an unrelated pointer selection"
        );
        interact(&runtime, &mut tree, &mut clipboard, false, &["a", "c"]);
        assert_eq!(
            clipboard.0,
            "Stable selected text.\n\nNewly prepared answer."
        );
        assert!(!clipboard.0.contains("Loading message"));
    }
}
