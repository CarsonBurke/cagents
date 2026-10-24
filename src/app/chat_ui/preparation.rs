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

fn available() -> &'static tokio::sync::watch::Sender<u64> {
    static AVAILABLE: std::sync::OnceLock<tokio::sync::watch::Sender<u64>> =
        std::sync::OnceLock::new();
    AVAILABLE.get_or_init(|| tokio::sync::watch::channel(0).0)
}
fn notify_available() {
    available().send_modify(|epoch| *epoch = epoch.wrapping_add(1));
}
/// A current-state event closes the defer→subscription race. Final source
/// lease release can happen on the reclaimer after an output event/redraw.
pub(super) fn availability() -> Subscription<Message> {
    Subscription::run(availability_stream)
}
fn availability_stream() -> impl iced::futures::Stream<Item = Message> + Send {
    iced::futures::stream::unfold(
        (available().subscribe(), true),
        |(mut receiver, initial)| async move {
            if !initial {
                receiver.changed().await.ok()?;
            }
            receiver.borrow_and_update();
            Some((action(Action::PreparationAvailable), (receiver, false)))
        },
    )
}

#[derive(Clone)]
enum Input {
    Transcript(Arc<chat::Item>),
    Local(Arc<prompt::LocalSource>),
}
impl Input {
    fn transcript(&self) -> Option<&Arc<chat::Item>> {
        match self {
            Self::Transcript(item) => Some(item),
            Self::Local(_) => None,
        }
    }
}

#[derive(Clone)]
struct Request {
    input: Input,
    provider: Provider,
    growing: bool,
    version: u64,
    theme: crate::settings::EditorSyntaxTheme,
    reader_proofs: Arc<HashMap<u64, reader_source::Proof>>,
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
    send::<prompt::Prepared>();
    send::<literal_content::Prepared>();
    send::<coordination::Prepared>();
    send::<prose_document::Data>();
    send::<prose::CodeBlock>();
    send::<tool_text::PreparedSources>();
};

#[derive(Default)]
struct Canonical {
    parsed: Parsed,
    blocks: HashMap<u64, prose::CodeBlock>,
}

#[derive(Default)]
pub(super) struct Lease(AtomicUsize);
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
            if bytes < old {
                notify_available();
            }
        }
        self.0.store(bytes, Ordering::Relaxed);
        true
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let bytes = self.0.load(Ordering::Relaxed);
        RESERVED.fetch_sub(bytes, Ordering::Relaxed);
        if bytes != 0 {
            notify_available();
        }
    }
}

/// Bounded raw immutable owners displaced by authenticated metadata-only
/// publications. Hold this guard until the Runtime commits its new Snapshot,
/// then retire actual Items before releasing their source-byte reservation.
#[derive(Default)]
pub(super) struct MetadataRetirement(Option<MetadataItems>);
struct MetadataItems {
    items: Vec<Arc<chat::Item>>,
    #[cfg(test)]
    watchers: Vec<(
        std::sync::Weak<chat::Item>,
        std::sync::mpsc::Sender<(std::thread::ThreadId, bool)>,
    )>,
    // Actual Items and their String allocations drop before capacity returns.
    lease: Lease,
}
impl Drop for MetadataItems {
    fn drop(&mut self) {
        drop(std::mem::take(&mut self.items));
        #[cfg(test)]
        for (item, sender) in self.watchers.drain(..) {
            let _ = sender.send((std::thread::current().id(), item.upgrade().is_none()));
        }
    }
}
#[cfg(test)]
thread_local! {
    static METADATA_WATCH: std::cell::RefCell<HashMap<usize, std::sync::mpsc::Sender<(std::thread::ThreadId, bool)>>> = std::cell::RefCell::default();
}
#[cfg(test)]
fn watch_metadata_retirement(
    item: &Arc<chat::Item>,
) -> std::sync::mpsc::Receiver<(std::thread::ThreadId, bool)> {
    let (sender, receiver) = std::sync::mpsc::channel();
    METADATA_WATCH.with(|watch| {
        watch
            .borrow_mut()
            .insert(Arc::as_ptr(item) as usize, sender)
    });
    receiver
}
impl MetadataRetirement {
    fn reserve(&mut self, item: &Arc<chat::Item>) -> bool {
        let owned = self.0.get_or_insert_with(|| MetadataItems {
            items: Vec::new(),
            #[cfg(test)]
            watchers: Vec::new(),
            lease: Lease::default(),
        });
        // Include bounded item/vector bookkeeping even for an empty body.
        let payload = match &item.content {
            ItemContent::Text { text, .. } => text.capacity(),
            _ => unreachable!("metadata retirement only admits proven Text"),
        };
        let bytes = payload
            .saturating_add(item.id.capacity())
            .saturating_add(std::mem::size_of::<chat::Item>())
            .saturating_add(2 * std::mem::size_of::<Arc<chat::Item>>())
            .saturating_add(64);
        if !owned
            .lease
            .resize(owned.lease.0.load(Ordering::Relaxed).saturating_add(bytes))
        {
            return false;
        }
        #[cfg(test)]
        if let Some(sender) =
            METADATA_WATCH.with(|watch| watch.borrow_mut().remove(&(Arc::as_ptr(item) as usize)))
        {
            owned.watchers.push((Arc::downgrade(item), sender));
        }
        true
    }
    pub(super) fn push(&mut self, item: Arc<chat::Item>) {
        self.0
            .as_mut()
            .expect("reserved metadata retirement")
            .items
            .push(item);
    }
}
impl Drop for MetadataRetirement {
    fn drop(&mut self) {
        if let Some(owned) = self.0.take() {
            if owned.items.is_empty() {
                drop(owned);
            } else {
                #[cfg(test)]
                let observed = !owned.watchers.is_empty();
                enqueue_retirement(Retirement::Metadata(owned));
                // Deterministically expose premature guard release: a wrong
                // pre-Snapshot enqueue must observe that obsolete owner live.
                #[cfg(test)]
                if observed {
                    retirement_barrier();
                }
            }
        }
    }
}

struct Shared(Option<Box<SharedData>>);
struct SharedData {
    latest: Mutex<Request>,
    canonical: Mutex<Option<Canonical>>,
    cancelled: AtomicBool,
    // Actual request/canonical allocations are destroyed before this lease.
    lease: Arc<Lease>,
}
impl std::ops::Deref for Shared {
    type Target = SharedData;
    fn deref(&self) -> &Self::Target {
        self.0.as_deref().expect("live preparation owner")
    }
}
impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(data) = self.0.take() {
            if RECLAIMING_FLOW.with(std::cell::Cell::get) {
                drop(data);
            } else {
                enqueue_retirement(Retirement::SharedData(data));
            }
        }
    }
}
impl Shared {
    fn new(data: SharedData) -> Self {
        Self(Some(Box::new(data)))
    }
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
    local: Option<(Target, Entry)>,
    local_error: Option<(std::sync::Weak<prompt::LocalSource>, String)>,
    errors: HashMap<String, (std::sync::Weak<chat::Item>, String)>,
    reader_proofs: Arc<HashMap<u64, reader_source::Proof>>,
    provider: Option<Provider>,
    // Cursor into the authoritative selected Snapshot, never another retained
    // source queue. A pump examines at most128 metadata entries per event.
    deferred: Option<usize>,
    last_deferred: bool,
}
impl Drop for State {
    fn drop(&mut self) {
        self.clear();
    }
}

/// Reader-only work shares the parser's source and output admission. It is
/// requested only when a remounted expanded tool has remembered geometry.
pub(super) fn source_bytes(content: &ItemContent) -> Option<usize> {
    CachedItem::prose(content)
        .map(str::len)
        .or_else(|| match content {
            ItemContent::Command {
                command, output, ..
            } => Some(command.len().saturating_add(output.len())),
            ItemContent::Tool { input, output, .. } => {
                Some(input.len().saturating_add(output.len()))
            }
            ItemContent::Changes { changes, .. } => {
                Some(changes.iter().fold(0usize, |bytes, change| {
                    bytes.saturating_add(change.diff.len())
                }))
            }
            ItemContent::WebSearch { query, results } => {
                Some(results.iter().fold(query.len(), |bytes, result| {
                    bytes
                        .saturating_add(result.title.len())
                        .saturating_add(result.url.len())
                        .saturating_add(result.snippet.len())
                }))
            }
            ItemContent::Delivery {
                kind: chat::DeliveryKind::Notice,
                summary,
                body,
                ..
            } => Some(body.len().saturating_add(summary.len())),
            ItemContent::Image {
                path, description, ..
            } => Some(path.len().saturating_add(description.len())),
            _ => None,
        })
}
fn verify_reader(
    identity: u64,
    text: &str,
    proofs: &HashMap<u64, reader_source::Proof>,
    verified: &mut HashMap<u64, (reader_source::Proof, reader_source::Proof)>,
) {
    if let Some(previous) = proofs.get(&identity)
        && let Some(pair) = reader_source::verify_prefix(text, *previous)
    {
        verified.insert(identity, pair);
    }
}
// Match Renderer::items/child exactly: sources are bound to the same logical
// item/path used by mounted readers, rather than compared with every bookmark.
fn verify_markdown_readers(
    item_id: &str,
    path: &[u32],
    items: &[markdown::Item],
    proofs: &HashMap<u64, reader_source::Proof>,
    verified: &mut HashMap<u64, (reader_source::Proof, reader_source::Proof)>,
) {
    for (index, item) in items.iter().enumerate() {
        let mut path = path.to_vec();
        path.push(index as u32);
        match item {
            markdown::Item::CodeBlock { source, .. } => verify_reader(
                selection::logical_identity(item_id, &path),
                source,
                proofs,
                verified,
            ),
            markdown::Item::Quote(items) => {
                verify_markdown_readers(item_id, &path, items, proofs, verified)
            }
            markdown::Item::List { bullets, .. } => {
                for (index, bullet) in bullets.iter().enumerate() {
                    let (markdown::Bullet::Point { items } | markdown::Bullet::Task { items, .. }) =
                        bullet;
                    let mut nested = path.clone();
                    nested.push(index as u32);
                    verify_markdown_readers(item_id, &nested, items, proofs, verified);
                }
            }
            markdown::Item::Table { columns, rows } => {
                for (column, header) in columns.iter().enumerate() {
                    let mut nested = path.clone();
                    nested.extend([0, column as u32]);
                    verify_markdown_readers(item_id, &nested, &header.header, proofs, verified);
                }
                for (row, cells) in rows.iter().enumerate() {
                    for (column, cell) in cells.cells.iter().enumerate() {
                        let mut nested = path.clone();
                        nested.extend([row as u32 + 1, column as u32]);
                        verify_markdown_readers(item_id, &nested, cell, proofs, verified);
                    }
                }
            }
            _ => {}
        }
    }
}
/// Bookmarked readers need source verification even while their disclosure is
/// collapsed. Match only this item's exact paths; unrelated bookmarks must not
/// enqueue every collapsed tool in a long transcript.
pub(super) fn has_tool_reader_proofs(
    item: &chat::Item,
    proofs: &HashMap<u64, reader_source::Proof>,
) -> bool {
    let has_path = |path: &[u32]| {
        let logical = selection::logical_identity(&item.id, path);
        [scroll::ReaderAxis::Horizontal, scroll::ReaderAxis::Vertical]
            .into_iter()
            .any(|axis| proofs.contains_key(&scroll::reader_key(logical, axis)))
    };
    match &item.content {
        ItemContent::Command { .. } | ItemContent::Tool { .. } => has_path(&[4]) || has_path(&[5]),
        ItemContent::Changes { changes, .. } => {
            (0..changes.len()).any(|index| has_path(&[6, index as u32]))
        }
        _ => false,
    }
}

fn verify_tool_reader(
    item_id: &str,
    path: &[u32],
    text: &str,
    proofs: &HashMap<u64, reader_source::Proof>,
    verified: &mut HashMap<u64, (reader_source::Proof, reader_source::Proof)>,
) {
    let logical = selection::logical_identity(item_id, path);
    for axis in [scroll::ReaderAxis::Horizontal, scroll::ReaderAxis::Vertical] {
        verify_reader(scroll::reader_key(logical, axis), text, proofs, verified);
    }
}
fn verify_tool_readers(
    item: &chat::Item,
    proofs: &HashMap<u64, reader_source::Proof>,
    verified: &mut HashMap<u64, (reader_source::Proof, reader_source::Proof)>,
) {
    match &item.content {
        ItemContent::Command {
            command, output, ..
        } => {
            verify_tool_reader(&item.id, &[4], command, proofs, verified);
            verify_tool_reader(&item.id, &[5], output, proofs, verified);
        }
        ItemContent::Tool { input, output, .. } => {
            verify_tool_reader(&item.id, &[4], input, proofs, verified);
            verify_tool_reader(&item.id, &[5], output, proofs, verified);
        }
        ItemContent::Changes { changes, .. } => {
            for (index, change) in changes.iter().enumerate() {
                verify_tool_reader(&item.id, &[6, index as u32], &change.diff, proofs, verified);
            }
        }
        _ => {}
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
    pub(super) fn clear_local(&mut self) {
        if let Some((_, entry)) = self.local.take() {
            entry.shared.cancel();
            retire_shared(entry.shared);
            notify_available();
        }
    }
    pub(super) fn stage_local(
        &mut self,
        target: Target,
        source: &Arc<prompt::LocalSource>,
        theme: crate::settings::EditorSyntaxTheme,
    ) {
        if self
            .local_error
            .as_ref()
            .is_some_and(|(previous, _)| previous.as_ptr() == Arc::as_ptr(source))
        {
            return;
        }
        self.local_error = None;
        if source.allocation() > QUEUED_BYTES {
            self.clear_local();
            self.local_error = Some((
                Arc::downgrade(source),
                "This queued message is too large to display.".into(),
            ));
            return;
        }
        if self.local.as_ref().is_some_and(|(current, entry)| {
            let latest = entry
                .shared
                .latest
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            *current == target
                && latest.theme == theme
                && matches!(&latest.input, Input::Local(previous) if Arc::ptr_eq(previous, source))
        }) {
            return;
        }
        self.clear_local();
        if self.entries.len() >= QUEUED_ITEMS {
            return;
        }
        let lease = {
            let mut retained = source.lease.lock().unwrap_or_else(|e| e.into_inner());
            if let Some(lease) = &*retained {
                lease.clone()
            } else {
                let lease = Arc::new(Lease::default());
                if !lease.resize(source.allocation()) {
                    return;
                }
                *retained = Some(lease.clone());
                lease
            }
        };
        self.local = Some((
            target,
            Entry {
                run: next_attempt(),
                running: true,
                shared: Arc::new(Shared::new(SharedData {
                    latest: Mutex::new(Request {
                        input: Input::Local(source.clone()),
                        provider: Provider::Claude,
                        growing: false,
                        version: 1,
                        theme,
                        reader_proofs: Arc::default(),
                    }),
                    canonical: Mutex::default(),
                    cancelled: AtomicBool::new(false),
                    lease,
                })),
            },
        ));
    }
    pub(super) fn accept_local(
        &mut self,
        target: Target,
        run: u64,
        source: &Arc<prompt::LocalSource>,
        ready: Ready,
    ) -> Option<Prepared> {
        let (current, entry) = self.local.as_ref()?;
        let latest = entry
            .shared
            .latest
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if *current != target
            || entry.run != run
            || entry.shared.cancelled.load(Ordering::Acquire)
            || !matches!(&latest.input, Input::Local(previous) if Arc::ptr_eq(previous, source))
        {
            return None;
        }
        let result = ready.0.0.lock().unwrap_or_else(|e| e.into_inner()).take()?;
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                drop(latest);
                self.clear_local();
                self.local_error = Some((Arc::downgrade(source), error));
                return None;
            }
        };
        if prepared.request.version != latest.version
            || !matches!(&prepared.request.input, Input::Local(previous) if Arc::ptr_eq(previous, source))
        {
            prepared.discard();
            return None;
        }
        drop(latest);
        self.clear_local();
        Some(prepared)
    }

    pub(super) fn set_reader_proofs(&mut self, proofs: Arc<HashMap<u64, reader_source::Proof>>) {
        self.reader_proofs = proofs;
    }
    pub(super) fn clear(&mut self) {
        self.clear_local();
        for (_, entry) in self.entries.drain() {
            entry.shared.cancel();
            retire_shared(entry.shared);
        }
        self.errors.clear();
        self.local_error = None;
        self.context = None;
        self.deferred = None;
        self.last_deferred = false;
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
            let local = self.local.take();
            self.clear();
            self.local = local;
            if self
                .local
                .as_ref()
                .is_some_and(|(current, _)| *current != target)
            {
                self.clear_local();
            }
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
            notify_available();
        }
    }
    pub(super) fn waiting(&self) -> bool {
        self.deferred.is_some()
    }
    pub(super) fn take_deferred(&mut self) -> Option<usize> {
        self.deferred.take()
    }
    pub(super) fn defer(&mut self, index: usize) {
        self.deferred = Some(self.deferred.map_or(index, |old| old.min(index)));
    }
    pub(super) fn reschedule(&self) {
        notify_available();
    }
    pub(super) fn was_deferred(&self) -> bool {
        self.last_deferred
    }
    pub(super) fn set_provider(&mut self, provider: Provider) {
        if self.provider.is_some_and(|old| old != provider) {
            self.clear();
        }
        self.provider = Some(provider);
    }
    pub(super) fn contains(&self, id: &str) -> bool {
        self.entries.contains_key(id)
    }
    #[cfg(test)]
    pub(super) fn growing_canonical_for_test(&self, item: &Arc<chat::Item>) -> Option<usize> {
        let Some(entry) = self.entries.get(&item.id) else {
            return None;
        };
        let latest = entry.shared.latest.lock().unwrap();
        if entry.running
            || !latest.growing
            || !latest
                .input
                .transcript()
                .is_some_and(|previous| Arc::ptr_eq(previous, item))
            || entry.shared.cancelled.load(Ordering::Acquire)
        {
            return None;
        }
        entry
            .shared
            .canonical
            .lock()
            .unwrap()
            .as_ref()
            .filter(|canonical| {
                canonical.parsed.markdown.has_open_fence_for_test() && !canonical.blocks.is_empty()
            })
            .map(|canonical| canonical.parsed.markdown.parsed_work_bytes())
    }
    pub(super) fn queued_bytes(&self) -> usize {
        self.entries
            .values()
            .map(|entry| entry.shared.lease.0.load(Ordering::Relaxed))
            .chain(
                self.local
                    .iter()
                    .map(|(_, entry)| entry.shared.lease.0.load(Ordering::Relaxed)),
            )
            .sum()
    }
    pub(super) fn global_queued_bytes() -> usize {
        RESERVED.load(Ordering::Relaxed)
    }
    pub(super) fn error(&self) -> Option<&str> {
        self.errors
            .values()
            .next()
            .map(|(_, error)| error.as_str())
            .or_else(|| {
                self.local_error
                    .as_ref()
                    .filter(|(source, _)| source.strong_count() != 0)
                    .map(|(_, error)| error.as_str())
            })
    }
    pub(super) fn retry(&mut self) {
        self.errors.clear();
        self.local_error = None;
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
        self.last_deferred = false;
        let Some(bytes) = source_bytes(&item.content) else {
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
        if bytes > QUEUED_BYTES
            || (bytes > SOURCE_BYTES && CachedItem::prose(&item.content).is_some())
        {
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
            let same = latest
                .input
                .transcript()
                .is_some_and(|previous| Arc::ptr_eq(previous, item));
            if same && latest.growing == growing && latest.theme == theme {
                return false;
            }
            if latest.theme == theme
                && (same
                    || (latest.input.transcript().is_some_and(|previous| {
                        same_kind(previous, item) && append(previous, item)
                    })))
            {
                if entry.shared.lease.resize(bytes) {
                    *latest = Request {
                        input: Input::Transcript(item.clone()),
                        provider: self.provider.unwrap_or(Provider::Claude),
                        growing,
                        version: latest.version.wrapping_add(1),
                        theme,
                        reader_proofs: self.reader_proofs.clone(),
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
        let lease = Arc::new(Lease::default());
        if self
            .entries
            .len()
            .saturating_add(usize::from(self.local.is_some()))
            >= QUEUED_ITEMS
            || !lease.resize(bytes)
        {
            // A valid source waits in the selected Snapshot. Saturation is
            // temporary backpressure, never a sticky same-Arc parse error.
            self.last_deferred = true;
            return true;
        }
        self.entries.insert(
            item.id.clone(),
            Entry {
                run: next_attempt(),
                running: true,
                shared: Arc::new(Shared::new(SharedData {
                    latest: Mutex::new(Request {
                        input: Input::Transcript(item.clone()),
                        provider: self.provider.unwrap_or(Provider::Claude),
                        growing,
                        version: 1,
                        theme,
                        reader_proofs: self.reader_proofs.clone(),
                    }),
                    canonical: Mutex::new(None),
                    cancelled: AtomicBool::new(false),
                    lease,
                })),
            },
        );
        true
    }

    /// The caller already proved an authenticated equal-body Text publication
    /// in this exact selected context. A running/finalizing parser cannot be
    /// relabeled; only its accepted idle canonical input may follow metadata.
    pub(super) fn retarget_metadata(
        &mut self,
        previous: &Arc<chat::Item>,
        current: &Arc<chat::Item>,
        growing: bool,
        theme: crate::settings::EditorSyntaxTheme,
        preparing: bool,
        provisional: bool,
        retired: &mut MetadataRetirement,
    ) -> bool {
        if let Some(entry) = self.entries.get_mut(&current.id) {
            if entry.running || entry.shared.cancelled.load(Ordering::Acquire) {
                return false;
            }
            let mut latest = entry
                .shared
                .latest
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if !latest
                .input
                .transcript()
                .is_some_and(|old| Arc::ptr_eq(old, previous))
                || latest.growing != growing
                || latest.theme != theme
                || self.provider != Some(latest.provider)
                || !retired.reserve(previous)
            {
                return false;
            }
            let Input::Transcript(previous) =
                std::mem::replace(&mut latest.input, Input::Transcript(current.clone()))
            else {
                unreachable!("transcript entry")
            };
            retired.push(previous);
            true
        } else {
            !preparing && !growing && !provisional && retired.reserve(previous)
        }
    }

    pub(super) fn subscriptions(&self, target: Target) -> Vec<Subscription<Message>> {
        let mut subscriptions: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.running)
            .map(|(id, entry)| {
                Subscription::run_with(
                    Job {
                        target,
                        id: Some(id.clone()),
                        run: entry.run,
                        shared: entry.shared.clone(),
                    },
                    stream,
                )
            })
            .collect();
        if let Some((current, entry)) = &self.local
            && *current == target
            && entry.running
        {
            subscriptions.push(Subscription::run_with(
                Job {
                    target,
                    id: None,
                    run: entry.run,
                    shared: entry.shared.clone(),
                },
                stream,
            ));
        }
        subscriptions
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
                    .input
                    .transcript()
                    .expect("transcript entry")
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
    document: prose_document::Data,
    prompt: Option<Box<prompt::Prepared>>,
    literal: Option<literal_content::Prepared>,
    coordination: Option<coordination::Prepared>,
    blocks: HashMap<u64, prose::CodeBlock>,
    palettes: prose::Palettes,
    reader_verifications: HashMap<u64, (reader_source::Proof, reader_source::Proof)>,
    prepared_tools: Option<Arc<tool_text::PreparedSources>>,
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
    pub(super) fn install_local(mut self, presentation: &mut prompt::LocalPresentation) {
        let previous = std::mem::replace(&mut presentation.prepared, self.prompt.take());
        presentation.revision = CachedItem::next_revision();
        if let Some(previous) = previous {
            retire_prompt(previous);
        }
        self.discard();
    }
    pub(super) fn is_coordination(&self) -> bool {
        self.coordination.is_some()
    }
    pub(super) fn item(&self) -> &Arc<chat::Item> {
        self.request.input.transcript().expect("transcript output")
    }
    pub(super) fn growing(&self) -> bool {
        self.request.growing
    }
    pub(super) fn discard(self) {
        retire(Retired {
            _parsed: self.parsed,
            _document: Some(self.document),
            _coordination: self.coordination,
            _prompt: self.prompt,
            _literal: self.literal,
            _blocks: self.blocks,
            _palettes: Some(self.palettes),
            _item: self.request.input.transcript().cloned(),
            _local: match self.request.input {
                Input::Local(source) => Some(source),
                _ => None,
            },
            _prepared_tools: self.prepared_tools,
            _owner: self._owner,
            _permit: self._permit,
        });
    }
    pub(super) fn install(self, cached: &mut CachedItem, managed: bool, expanded: bool) {
        cached
            .reader_memory
            .verified_readers(cached.reader_context.as_deref(), self.reader_verifications);
        let previous_prompt = std::mem::replace(&mut cached.prompt, self.prompt);
        let previous_literal = cached.literal.take();
        let previous_tools = cached.prepared_tools.take();
        let previous_coordination = cached.coordination.take();
        if CachedItem::prose(
            &self
                .request
                .input
                .transcript()
                .expect("transcript output")
                .content,
        )
        .is_none()
        {
            let mut previous = std::mem::replace(
                cached,
                CachedItem::preparing(
                    self.request.input.transcript().expect("transcript output"),
                    self.request.theme,
                ),
            );
            previous.item = Some(
                self.request
                    .input
                    .transcript()
                    .expect("transcript output")
                    .clone(),
            );
            previous.prepared_tools.clone_from(&self.prepared_tools);
            *cached = CachedItem::updated(
                &self
                    .request
                    .input
                    .transcript()
                    .expect("transcript output")
                    .content,
                self.request.theme,
                Some(previous),
                expanded,
                false,
                None,
            );
        }
        // Move the old owned parser containers out before replacing them:
        // their destructors may visit thousands of blocks and free code.
        cached.document = Some(prose_document::Document::new(self.document));
        cached.literal = self.literal.map(literal_content::Prepared::install);
        drop(previous_literal);
        cached.coordination = self
            .coordination
            .map(|prepared| Box::new(coordination::Coordinated::from_prepared(prepared)));
        drop(previous_coordination);
        let retired = Retired {
            _document: None,
            _coordination: None,
            _prompt: previous_prompt,
            _literal: None,
            _parsed: Parsed {
                source: std::mem::replace(&mut cached.source, self.parsed.source),
                markdown: std::mem::replace(&mut cached.markdown, self.parsed.markdown),
            },
            _blocks: std::mem::replace(&mut cached.blocks, self.blocks),
            _palettes: std::mem::replace(&mut cached.palette_views, Some(self.palettes)),
            _item: cached.item.replace(
                self.request
                    .input
                    .transcript()
                    .expect("transcript output")
                    .clone(),
            ),
            _local: None,
            _prepared_tools: previous_tools,
            _owner: self._owner,
            _permit: self._permit,
        };
        cached.token = ChangeToken::of(
            &self
                .request
                .input
                .transcript()
                .expect("transcript output")
                .content,
        );
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
    _document: Option<prose_document::Data>,
    _coordination: Option<coordination::Prepared>,
    _prompt: Option<Box<prompt::Prepared>>,
    _literal: Option<literal_content::Prepared>,
    _blocks: HashMap<u64, prose::CodeBlock>,
    _palettes: Option<prose::Palettes>,
    _item: Option<Arc<chat::Item>>,
    _local: Option<Arc<prompt::LocalSource>>,
    _prepared_tools: Option<Arc<tool_text::PreparedSources>>,
    _owner: Arc<Shared>,
    // Declared last so retirement completes before the next parser is admitted.
    _permit: tokio::sync::OwnedSemaphorePermit,
}
#[allow(dead_code)] // Fields deliberately retain owned allocations until background drop.
pub(super) struct Presentation {
    pub(super) parsed: Parsed,
    pub(super) blocks: HashMap<u64, prose::CodeBlock>,
    pub(super) palettes: Option<prose::Palettes>,
    pub(super) item: Option<Arc<chat::Item>>,
    pub(super) prepared_tools: Option<Arc<tool_text::PreparedSources>>,
    pub(super) prompt: Option<Box<prompt::Prepared>>,
}

enum Retirement {
    Catalog(Box<selection::CatalogData>),
    Metadata(MetadataItems),
    Find(find_navigation::RetiredWork),
    LocalText(RawLocal),
    Prompt(Box<prompt::Prepared>),
    Literal(literal_content::Prepared),
    FlowSource(flow_layout::RawSource),
    FlowOutput(flow_layout::OutputRetirement),
    Document(prose_document::Data),
    Coordination(coordination::Prepared),
    Styles(HashMap<u64, prose::CodeBlock>, Option<prose::Palettes>),
    Geometry(
        Box<flow_layout::Geometry>,
        Option<Arc<flow_layout::OutputAdmission>>,
    ),
    Presentation(Presentation),
    Installed(Retired),
    // Keep the existing queued-source lease until canonical parser/source
    // destructors finish, including cancelled/hidden sources without output.
    Detached(Arc<Shared>),
    SharedData(Box<SharedData>),
    Barrier(std::sync::mpsc::Sender<std::thread::ThreadId>),
    #[cfg(test)]
    Hold(std::sync::mpsc::Sender<()>, std::sync::mpsc::Receiver<()>),
}
thread_local! {
    // Only the process-owned raw-payload reclaimer sets this. A final shared
    // Flow guard can be released by a retired Geometry; destroy its source and
    // slot inline there, before an already queued generation barrier.
    static RECLAIMING_FLOW: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
pub(super) struct RawLocal {
    pub(super) text: String,
    #[cfg(test)]
    pub(super) disposed: Option<std::sync::mpsc::Sender<(std::thread::ThreadId, usize)>>,
    // Same admitted source reservation, released after the actual String.
    #[allow(dead_code)]
    pub(super) lease: Option<Arc<Lease>>,
}
impl Drop for RawLocal {
    fn drop(&mut self) {
        let text = std::mem::take(&mut self.text);
        #[cfg(test)]
        let bytes = text.len();
        drop(text);
        #[cfg(test)]
        if let Some(disposed) = self.disposed.take() {
            let _ = disposed.send((std::thread::current().id(), bytes));
        }
    }
}
pub(super) fn retire_local_text(text: RawLocal) {
    if RECLAIMING_FLOW.with(std::cell::Cell::get) {
        drop(text);
    } else {
        enqueue_retirement(Retirement::LocalText(text));
    }
}
pub(super) fn retire_catalog(data: Box<selection::CatalogData>) {
    if RECLAIMING_FLOW.with(std::cell::Cell::get) {
        drop(data);
    } else {
        enqueue_retirement(Retirement::Catalog(data));
    }
}
pub(super) fn retire_prompt(prompt: Box<prompt::Prepared>) {
    if RECLAIMING_FLOW.with(std::cell::Cell::get) {
        drop(prompt);
    } else {
        enqueue_retirement(Retirement::Prompt(prompt));
    }
}
pub(super) fn reclaiming_find() -> bool {
    RECLAIMING_FLOW.with(std::cell::Cell::get)
}
pub(super) fn retire_literal(prepared: literal_content::Prepared) {
    if RECLAIMING_FLOW.with(std::cell::Cell::get) {
        drop(prepared);
    } else {
        enqueue_retirement(Retirement::Literal(prepared));
    }
}
pub(super) fn retire_find(work: find_navigation::RetiredWork) {
    enqueue_retirement(Retirement::Find(work));
}
pub(super) fn retire_flow_source(source: flow_layout::RawSource) {
    if RECLAIMING_FLOW.with(std::cell::Cell::get) {
        drop(source);
    } else {
        enqueue_retirement(Retirement::FlowSource(source));
    }
}
pub(super) fn retire_flow_output(output: flow_layout::OutputRetirement) {
    if RECLAIMING_FLOW.with(std::cell::Cell::get) {
        drop(output);
    } else {
        enqueue_retirement(Retirement::FlowOutput(output));
    }
}
pub(super) fn retire_styles(
    blocks: HashMap<u64, prose::CodeBlock>,
    palettes: Option<prose::Palettes>,
) {
    enqueue_retirement(Retirement::Styles(blocks, palettes));
}
pub(super) fn retire_coordination(prepared: coordination::Prepared) {
    enqueue_retirement(Retirement::Coordination(prepared));
}
pub(super) fn retire_document(document: prose_document::Data) {
    enqueue_retirement(Retirement::Document(document));
}
pub(super) fn retire_geometry(
    geometry: Box<flow_layout::Geometry>,
    admission: Option<Arc<flow_layout::OutputAdmission>>,
) {
    enqueue_retirement(Retirement::Geometry(geometry, admission));
}
pub(super) fn retire_presentation(presentation: Presentation) {
    enqueue_retirement(Retirement::Presentation(presentation));
}
fn retire_shared(shared: Arc<Shared>) {
    enqueue_retirement(Retirement::Detached(shared));
}
fn retire(retired: Retired) {
    enqueue_retirement(Retirement::Installed(retired));
}
// Ownership already resident in the active graph may retire as one batch.
// Worker producers fence reclamation before allocating another generation, so
// repeated hide/reveal cannot add generations behind an unfinished batch. The
// queue is process-owned: a stopped worker never falls back to UI destruction.
struct Reclaimer {
    pending: Mutex<std::collections::VecDeque<Retirement>>,
    wake: std::sync::Condvar,
}
static RETIRING: AtomicUsize = AtomicUsize::new(0);
fn enqueue_retirement(retired: Retirement) {
    static RETIRE: std::sync::OnceLock<Arc<Reclaimer>> = std::sync::OnceLock::new();
    let reclaimer = RETIRE.get_or_init(|| {
        let owner = Arc::new(Reclaimer { pending: Mutex::default(), wake: std::sync::Condvar::new() });
        let worker = owner.clone();
        std::thread::Builder::new().name("chat-presentation-retire".into()).spawn(move || {
          RECLAIMING_FLOW.with(|reclaiming| reclaiming.set(true));
          loop {
            let retired = {
                let mut pending = worker.pending.lock().unwrap_or_else(|e| e.into_inner());
                while pending.is_empty() {
                    pending = worker.wake.wait(pending).unwrap_or_else(|e| e.into_inner());
                }
                pending.pop_front().expect("queued retirement")
            };
            match retired {
                Retirement::Catalog(data) => drop(data),
                Retirement::Metadata(owned) => drop(owned),
                Retirement::Find(work) => drop(work),
                Retirement::LocalText(text) => drop(text),
                Retirement::Prompt(prompt) => drop(prompt),
                Retirement::Literal(prepared)=>drop(prepared),
                Retirement::FlowSource(source) => drop(source),
                Retirement::FlowOutput(output) => drop(output),
                Retirement::Document(document) => drop(document),
                Retirement::Coordination(prepared) => drop(prepared),
                Retirement::Styles(blocks, palettes) => { drop(blocks); drop(palettes); }
                Retirement::Geometry(geometry, admission) => { drop(geometry); drop(admission); }
                Retirement::Presentation(presentation) => drop(presentation),
                Retirement::Installed(retired) => drop(retired),
                Retirement::Detached(owner) => drop(owner),
                Retirement::SharedData(data) => drop(data),
                Retirement::Barrier(reply) => { let _ = reply.send(std::thread::current().id()); }
                #[cfg(test)]
                Retirement::Hold(entered, release) => { let _ = entered.send(()); let _ = release.recv_timeout(Duration::from_secs(10)); }
            }
            let remaining = RETIRING.fetch_sub(1, Ordering::AcqRel) - 1;
            tracing::trace!(target: "cagents::memory", owner = "chat_retirement", pending_items = remaining);
            if remaining == 0 {
                // A new enqueue may race this observation; this is not a
                // global quiescence or resident-memory assertion.
                crate::ui_memory::trace_allocator("retirement_queue_observed_zero");
            }
          }
        }).expect("start presentation retirement worker");
        owner
    });
    let queued = RETIRING.fetch_add(1, Ordering::AcqRel) + 1;
    reclaimer
        .pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back(retired);
    reclaimer.wake.notify_one();
    tracing::trace!(target: "cagents::memory", owner = "chat_retirement", pending_items = queued);
}

fn reclamation_barrier() -> Result<std::thread::ThreadId, String> {
    let (sender, receiver) = std::sync::mpsc::channel();
    enqueue_retirement(Retirement::Barrier(sender));
    receiver
        .recv_timeout(Duration::from_secs(30))
        .map_err(|_| "Previous message storage could not be released; retry loading it.".to_owned())
}
/// Blocking worker boundary only. Never call this from update/layout/draw or
/// while holding a parser, reader, font-system or reclaimer lock.
pub(super) fn reclaim_before_work() -> Result<(), String> {
    reclamation_barrier().map(|_| ())
}
#[cfg(test)]
pub(super) fn hold_retirement() -> std::sync::mpsc::Sender<()> {
    let (entered, observed) = std::sync::mpsc::channel();
    let (release, held) = std::sync::mpsc::channel();
    enqueue_retirement(Retirement::Hold(entered, held));
    observed
        .recv_timeout(Duration::from_secs(10))
        .expect("retirement fixture entered");
    release
}
#[cfg(test)]
pub(super) fn retirement_barrier() -> std::thread::ThreadId {
    reclamation_barrier().expect("retirement worker remains alive")
}

pub(super) fn prime_plain(items: &[markdown::Item], cancelled: &AtomicBool) -> Result<(), String> {
    for item in items {
        if cancelled.load(Ordering::Relaxed) {
            return Err("Message loading was cancelled.".into());
        }
        match item {
            markdown::Item::Paragraph(text) | markdown::Item::Heading(_, text) => {
                text.plain_prefixes();
            }
            markdown::Item::Image {
                alt, title_text, ..
            } => {
                alt.plain_prefixes();
                title_text.plain_prefixes();
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
    id: Option<String>,
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
        action(match job.id {
            Some(id) => Action::MarkdownPrepared(job.target, id, job.run, Ready::new(result)),
            None => Action::LocalPromptPrepared(job.target, job.run, Ready::new(result)),
        })
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
    reclaim_before_work()?;
    let request = shared
        .latest
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    if let Input::Local(source) = &request.input {
        let prompt = prompt::Prepared::new(source.text().trim_end(), &shared.cancelled)?;
        return Ok(Prepared {
            request,
            parsed: Parsed::default(),
            document: prose_document::Data::snapshot(&markdown::Content::default()),
            prompt: Some(prompt),
            literal: None,
            coordination: None,
            blocks: HashMap::new(),
            palettes: prose::palettes(&HashMap::new()),
            reader_verifications: HashMap::new(),
            prepared_tools: None,
            #[cfg(test)]
            syntax_work: (0, 0),
            _owner: shared,
            _permit: permit,
        });
    }
    let item = request.input.transcript().expect("transcript dispatch");
    let mut canonical = shared
        .canonical
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
        .unwrap_or_default();
    let mut reader_verifications = HashMap::new();
    let prompt = match (&item.role, &item.content) {
        (chat::Role::User, ItemContent::Text { text, .. }) => {
            Some(prompt::Prepared::new(text, &shared.cancelled)?)
        }
        _ => None,
    };
    let literal = literal_content::Prepared::new(&item.content, &shared.cancelled)?;
    if prompt.is_none()
        && literal.is_none()
        && let Some(body) = CachedItem::prose(&item.content)
    {
        parse_publication(
            &mut canonical.parsed,
            body,
            request.growing,
            &shared.cancelled,
        )?;
        prime_plain(canonical.parsed.markdown.items(), &shared.cancelled)?;
        if !request.reader_proofs.is_empty() {
            verify_markdown_readers(
                &item.id,
                &[0],
                canonical.parsed.markdown.items(),
                &request.reader_proofs,
                &mut reader_verifications,
            );
        }
    } else {
        verify_tool_readers(&item, &request.reader_proofs, &mut reader_verifications);
    }
    let coordinated = chat::coordination::recognize(&item, request.provider)
        .map(|reading| coordination::Prepared::prepare(reading, &shared.cancelled))
        .transpose()?;
    if let Some(coordinated) = &coordinated {
        for (index, content) in coordinated.contents().enumerate() {
            if shared.cancelled.load(Ordering::Relaxed) {
                return Err("Message loading was cancelled.".into());
            }
            verify_markdown_readers(
                &item.id,
                &[1, index as u32],
                content.items(),
                &request.reader_proofs,
                &mut reader_verifications,
            );
        }
    }
    let mut previous = std::mem::take(&mut canonical.blocks);
    #[cfg(test)]
    let mut syntax_work = (0, 0);
    // Settled fences drop their individual parser immediately. Keep one lazy
    // job owner across the whole pass so adjacent fences share grammar/regex
    // storage, without retaining it in finished presentation or loading it for
    // a message with no new highlighted fences.
    let mut syntax_job = None;
    for_each_fence(
        canonical.parsed.markdown.items().iter().chain(
            coordinated
                .iter()
                .flat_map(|prepared| prepared.contents())
                .flat_map(markdown::Content::items),
        ),
        &mut |language, code| {
            if shared.cancelled.load(Ordering::Relaxed) || code.len() > CachedItem::HIGHLIGHT_LIMIT
            {
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
                    let language = language.and_then(prose::fence_language);
                    if language.is_some() {
                        syntax_job.get_or_insert_with(crate::diff_view::syntax_lease);
                    }
                    prose::CodeBlock::new(language, code, request.theme)
                })
            });
        },
    );
    drop(syntax_job);
    if shared.cancelled.load(Ordering::Relaxed) {
        return Err("Message loading was cancelled.".into());
    }
    let (mut parsed, blocks) = if request.growing {
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
    if !request.growing {
        parsed.source.suspend();
        parsed.markdown.discard_continuation();
    }
    let palettes = prose::palettes(&blocks);
    let prepared_tools = CachedItem::prose(&item.content)
        .is_none()
        .then(|| Arc::new(tool_text::PreparedSources::new(&item.content)));
    let document = prose_document::Data::snapshot(&parsed.markdown);
    Ok(Prepared {
        request,
        parsed,
        document,
        prompt,
        literal,
        coordination: coordinated,
        blocks,
        palettes,
        reader_verifications,
        prepared_tools,
        #[cfg(test)]
        syntax_work,
        _owner: shared,
        _permit: permit,
    })
}

#[cfg(test)]
pub(super) fn local_reply_for_test(runtime: &mut Runtime) -> (u64, Ready) {
    runtime.prepare_local_prompt();
    let (_, entry) = runtime
        .preparation
        .local
        .as_ref()
        .expect("admitted actual local input");
    let run = entry.run;
    let shared = entry.shared.clone();
    let permit = Arc::new(tokio::sync::Semaphore::new(1))
        .try_acquire_owned()
        .unwrap();
    (
        run,
        Ready::new(
            std::thread::spawn(move || execute(shared, permit))
                .join()
                .unwrap(),
        ),
    )
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

    fn literal_runtime(content: ItemContent) -> (Runtime, Arc<ClientHandle>) {
        let (mut runtime, handle) = view::prepared_runtime_for_test(&[]);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        let mut source = (*item("")).clone();
        source.content = content;
        snapshot.items.push(Arc::new(source));
        handle.publish_delta_snapshot_for_test(snapshot);
        runtime.refresh();
        (runtime, handle)
    }

    #[test]
    fn literal_search_actual_worker_keeps_fields_and_refuses_same_id_rewrite_ready() {
        let url = format!("https://example.test/{}#L22", "u".repeat(44 * 1024));
        let content = ItemContent::WebSearch {
            query: "**literal query**".into(),
            results: vec![chat::SearchResult {
                title: String::new(),
                url: url.clone(),
                snippet: "[literal snippet](not-a-link)".into(),
            }],
        };
        let (mut runtime, handle) = literal_runtime(content);
        let source = runtime.snapshot.items[0].clone();
        assert!(runtime.prepared_literal(&source).is_none());
        let (run, shared) = job(&runtime.preparation);
        let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        assert!(output.parsed.markdown.items().is_empty());
        let literal_content::Prepared::Search(document) = output.literal.as_ref().unwrap() else {
            panic!("literal Search payload")
        };
        assert_ne!(document.created_on, std::thread::current().id());
        runtime.markdown_prepared(runtime.target, "message", run, ready(Ok(output)));
        let accepted = runtime.prepared_literal(&source).unwrap();
        let literal_content::Content::Search(document) = accepted else {
            panic!("installed Search")
        };
        let markdown::Item::Quote(fields) = &document.items()[1] else {
            panic!("result fields")
        };
        let markdown::Item::Paragraph(title) = &fields[0] else {
            panic!("fallback title")
        };
        assert_eq!(title.plain().as_ref(), url);
        assert!(title.plain_ready());
        let mut builder = selection::Catalog::builder();
        accepted.catalog(&mut builder, &source, true);
        let catalog = builder.finish();
        let (a, b) = catalog.endpoints().unwrap();
        assert_eq!(
            catalog.copy(a, b).unwrap(),
            format!("**literal query**\n\n{url}\n\n{url}\n\n[literal snippet](not-a-link)")
        );
        assert!(
            runtime.prepared_literal(&(*source).clone()).is_none(),
            "an equal item clone cannot authorize a prepared source"
        );
        let mut replacement = (*handle.snapshot()).clone();
        replacement.generation += 1;
        Arc::make_mut(&mut replacement.items[0]).content = ItemContent::WebSearch {
            query: "replacement".into(),
            results: Vec::new(),
        };
        handle.publish_snapshot_for_test(replacement);
        runtime.refresh();
        assert!(
            runtime
                .prepared_literal(&runtime.snapshot.items[0])
                .is_none()
        );
        let (old_run, old_shared) = job(&runtime.preparation);
        let old = std::thread::spawn(move || execute(old_shared, permit()).unwrap())
            .join()
            .unwrap();
        let mut rewritten = (*handle.snapshot()).clone();
        rewritten.generation += 1;
        Arc::make_mut(&mut rewritten.items[0]).content = ItemContent::WebSearch {
            query: "current".into(),
            results: Vec::new(),
        };
        handle.publish_snapshot_for_test(rewritten);
        runtime.refresh();
        runtime.markdown_prepared(runtime.target, "message", old_run, ready(Ok(old)));
        assert!(
            runtime
                .prepared_literal(&runtime.snapshot.items[0])
                .is_none(),
            "late ready cannot publish a previous same-ID result"
        );
        drain_fixture(&mut runtime);
        let literal_content::Content::Search(current) = runtime
            .prepared_literal(&runtime.snapshot.items[0])
            .unwrap()
        else {
            panic!("current Search")
        };
        let markdown::Item::Paragraph(query) = &current.items()[0] else {
            panic!("query")
        };
        assert_eq!(query.plain().as_ref(), "current");
        runtime.show(false);
        retirement_barrier();
    }

    #[test]
    fn literal_notice_preview_and_actual_ready_retire_spans_before_same_permit() {
        for body in [
            "one\r\ntwo\r\nthree\r\n",
            "one\r\ntwo\r\nthree\r\nfour",
            "single huge line",
        ] {
            let cancelled = AtomicBool::new(false);
            let prepared =
                prompt::Prepared::with_policy(body, prompt::PreviewPolicy::Notice, &cancelled)
                    .unwrap();
            let long = body.lines().count() > view::NOTICE_LINES;
            let expected = if long {
                body.lines()
                    .take(view::NOTICE_LINES)
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                body.to_owned()
            };
            assert_eq!(prepared.long, long);
            assert_eq!(prepared.shown(false).plain().as_ref(), expected);
            assert_eq!(prepared.full.plain().as_ref(), body);
        }
        let body = format!(
            "{}\r\nsecond\r\nthird\r\nlast",
            "literal **raw** λ ".repeat(65_536)
        );
        let (mut runtime, _handle) = literal_runtime(ItemContent::Delivery {
            kind: chat::DeliveryKind::Notice,
            from: String::new(),
            sender: String::new(),
            summary: "NONSELECTABLE SUMMARY".into(),
            body: body.clone(),
        });
        let (run, shared) = job(&runtime.preparation);
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let admitted = slots.clone().try_acquire_owned().unwrap();
        let mut output = std::thread::spawn(move || execute(shared, admitted).unwrap())
            .join()
            .unwrap();
        assert!(output.parsed.markdown.items().is_empty());
        let literal_content::Prepared::Notice(notice) = output.literal.as_mut().unwrap() else {
            panic!("notice payload")
        };
        assert_eq!(notice.body.full.plain().as_ref(), body);
        let (sent, disposed) = std::sync::mpsc::channel();
        notice.body.full.watch_raw_spans(sent);
        let hold = hold_retirement();
        runtime.show(false);
        runtime.markdown_prepared(runtime.target, "message", run, ready(Ok(output)));
        assert!(disposed.try_recv().is_err());
        assert!(
            slots.clone().try_acquire_owned().is_err(),
            "stale literal output keeps the same output permit until actual destruction"
        );
        drop(hold);
        let worker = retirement_barrier();
        assert_eq!(
            disposed.recv_timeout(Duration::from_secs(5)).unwrap().0,
            worker
        );
        assert!(slots.try_acquire_owned().is_ok());
    }

    #[test]
    fn managed_image_actual_worker_uses_receipt_label_and_preserves_diagnostics() {
        let reference = chat::ImageReference {
            id: uuid::Uuid::new_v4(),
            number: 59,
            device: uuid::Uuid::new_v4(),
            name: "Original screenshot.png".into(),
            sha256: "a".repeat(64),
            extension: "png".into(),
        };
        for path in [
            "/private/image-storage/private-internal-digest.png",
            "https://example.test/private-internal-digest.png",
        ] {
            let description = "The user's original screenshot description";
            let content = ItemContent::Image {
                path: path.into(),
                description: description.into(),
                reference: Some(reference.clone()),
            };
            let (mut runtime, _handle) = literal_runtime(content.clone());
            let (run, shared) = job(&runtime.preparation);
            let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
                .join()
                .unwrap();
            assert!(output.parsed.markdown.items().is_empty());
            let literal_content::Prepared::Image { document, .. } =
                output.literal.as_ref().expect("actual prepared image DTO")
            else {
                panic!("actual worker produced an image payload")
            };
            assert_ne!(document.created_on, std::thread::current().id());
            runtime.markdown_prepared(runtime.target, "message", run, ready(Ok(output)));
            let source = runtime.snapshot.items[0].clone();
            let prepared = runtime
                .prepared_literal(&source)
                .expect("installed exact image");
            let label = prepared.image_prompt().unwrap().plain();
            assert_eq!(label.as_ref(), reference.label());
            assert!(!label.contains("private-internal-digest"));
            let literal_content::Content::Image { document, url } = prepared else {
                panic!("installed image document")
            };
            let markdown::Item::Paragraph(actual_description) = &document.items()[0] else {
                panic!("original image description")
            };
            assert_eq!(actual_description.plain().as_ref(), description);
            if path.starts_with("https://") {
                assert_eq!(url.as_deref(), Some(path));
                assert!(
                    matches!(&document.items()[1], markdown::Item::Quote(items) if items.is_empty())
                );
                let markdown::Item::Paragraph(open) = &document.items()[3] else {
                    panic!("shared original image URI")
                };
                let settings =
                    design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE);
                assert!(Arc::ptr_eq(
                    open.spans(settings.style)[0].link.as_ref().unwrap(),
                    url.as_ref().unwrap(),
                ));
            } else {
                assert!(url.is_none());
                let markdown::Item::Paragraph(actual_path) = &document.items()[1] else {
                    panic!("original diagnostic image path")
                };
                assert_eq!(actual_path.plain().as_ref(), path);
            }
            runtime.show(false);
            retirement_barrier();

            // The same managed-image branch must still reject canceled work;
            // a stable label must not make stale preparation installable.
            let (runtime, _handle) = literal_runtime(content);
            let (_, shared) = job(&runtime.preparation);
            shared.cancelled.store(true, Ordering::Release);
            let result = std::thread::spawn(move || execute(shared, permit()))
                .join()
                .unwrap();
            assert!(matches!(result, Err(ref error) if error == "Message loading was cancelled."));
            drop(runtime);
            retirement_barrier();
        }
    }

    #[test]
    fn literal_image_worker_native_fields_keep_catalog_and_shared_remote_uri() {
        use iced::advanced::{
            layout,
            widget::{Operation, Tree},
        };
        let description = format!("literal **description** {}", "λ ".repeat(8_192));
        let (mut runtime, _handle) = literal_runtime(ItemContent::Image {
            path: "/tmp/very-long-path/image.png".into(),
            description: description.clone(),
            reference: None,
        });
        let (run, shared) = job(&runtime.preparation);
        let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        runtime.markdown_prepared(runtime.target, "message", run, ready(Ok(output)));
        let source = runtime.snapshot.items[0].clone();
        let cached = &runtime.cache["message"];
        let settings = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE);
        let mut element: Element<'static, Message> = widget::column::with_children(
            (0..2)
                .filter_map(|part| {
                    literal_content::image_field(&runtime, &source, cached, part, settings)
                })
                .collect::<Vec<_>>(),
        )
        .into();
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(&element);
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 100_000.0));
        let before = markdown::materialized_bytes();
        element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert_eq!(
            markdown::materialized_bytes(),
            before,
            "cold native literal Image layout cannot materialize its long field"
        );
        // complete_fixture intentionally materializes on its caller. Drive the
        // actual admitted Flow task here so this oracle measures UI work only.
        let literal_content::Content::Image { document, .. } = cached.literal.as_ref().unwrap()
        else {
            panic!("prepared Image document")
        };
        let Some(prose_document::Node::Item(markdown::Item::Paragraph(text))) = document.node(&[0])
        else {
            panic!("prepared Image description")
        };
        let mut source_style = settings.style;
        source_style.font = iced::Font::default();
        let entry = cached.flow_cache.markdown_entry(
            selection::logical_identity(&source.id, &[8, 0]),
            text.clone(),
            source_style,
        );
        let request = entry
            .borrow_mut()
            .request()
            .expect("mounted Image Flow request");
        let task = flow_layout::run(runtime.target, request).expect("admitted Image Flow worker");
        use iced::futures::StreamExt;
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (request, ready) = executor.block_on(async {
            let mut stream =
                iced::runtime::task::into_stream(task).expect("actual Image Flow task");
            tokio::time::timeout(Duration::from_secs(10), async {
                while let Some(effect) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                        Action::FlowReady(target, request, ready),
                    ))) = effect
                    {
                        assert_eq!(target, runtime.target);
                        return (request, ready);
                    }
                }
                panic!("actual Image Flow completion")
            })
            .await
            .unwrap()
        });
        assert!(ready.is_some(), "actual Image Flow worker returns geometry");
        assert!(entry.borrow_mut().complete(&request, ready));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert_eq!(
            markdown::materialized_bytes(),
            before,
            "native literal Image fields use primed stable sources"
        );
        #[derive(Default)]
        struct Fields(Vec<Arc<str>>);
        impl Operation for Fields {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn custom(
                &mut self,
                _: Option<&iced::advanced::widget::Id>,
                _: iced::Rectangle,
                state: &mut dyn std::any::Any,
            ) {
                if let Some(text) = state.downcast_ref::<selectable_prose::State>() {
                    self.0.push(text.plain.clone());
                }
            }
        }
        let mut fields = Fields::default();
        element.as_widget_mut().operate(
            &mut tree,
            iced::advanced::Layout::new(&node),
            &renderer,
            &mut fields,
        );
        assert_eq!(
            fields.0.iter().map(AsRef::as_ref).collect::<Vec<&str>>(),
            [description.as_str(), "/tmp/very-long-path/image.png"]
        );
        let mut builder = selection::Catalog::builder();
        runtime
            .prepared_literal(&source)
            .unwrap()
            .catalog(&mut builder, &source, true);
        let catalog = builder.finish();
        let (a, b) = catalog.endpoints().unwrap();
        assert_eq!(
            catalog.copy(a, b).unwrap(),
            format!("{description}\n\n/tmp/very-long-path/image.png")
        );
        assert_eq!(
            catalog
                .text(selection::logical_identity("message", &[8, 0]))
                .unwrap()
                .as_ref(),
            description
        );
        let remote = format!("https://example.test/{}", "u".repeat(48 * 1024));
        let content = ItemContent::Image {
            path: remote.clone(),
            description: String::new(),
            reference: None,
        };
        let prepared = std::thread::spawn(move || {
            literal_content::Prepared::new(&content, &AtomicBool::new(false))
                .unwrap()
                .unwrap()
        })
        .join()
        .unwrap()
        .install();
        let literal_content::Content::Image {
            document,
            url: Some(url),
        } = &prepared
        else {
            panic!("remote image URI")
        };
        let markdown::Item::Paragraph(open) = &document.items()[3] else {
            panic!("shared URI backing")
        };
        assert!(Arc::ptr_eq(
            open.spans(settings.style)[0].link.as_ref().unwrap(),
            url
        ));
        let action = Action::OpenSharedUrl(runtime.target, url.clone());
        assert!(matches!(action,Action::OpenSharedUrl(_,ref shared) if Arc::ptr_eq(shared,url)));
        assert_eq!(url.as_ref(), remote);
        drop(element);
        runtime.show(false);
        drop(prepared);
        retirement_barrier();
    }

    #[test]
    fn literal_user_source_uses_actual_worker_payload_and_refuses_rewrite_until_current_ready() {
        let body = format!(
            "**literal** [not a link](https://invalid.test)\r\n{}\r\nEND_LITERAL_λ",
            "raw `code` & <tag> ".repeat(32_768)
        );
        let (mut runtime, handle) = view::prepared_runtime_for_test(&[]);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        let mut user = (*item(&body)).clone();
        user.role = chat::Role::User;
        snapshot.items.push(Arc::new(user));
        handle.publish_delta_snapshot_for_test(snapshot);
        let materialized = markdown::materialized_bytes();
        runtime.refresh();
        let source = runtime.snapshot.items[0].clone();
        assert!(
            runtime.prepared_prompt(&source).is_none(),
            "cold literal source must wait for exact preparation"
        );
        assert_eq!(
            markdown::materialized_bytes(),
            materialized,
            "cold literal staging cannot materialize the full UI body"
        );
        let (run, shared) = job(&runtime.preparation);
        let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        assert!(
            output.parsed.markdown.items().is_empty(),
            "literal User is not Markdown parsed"
        );
        assert_eq!(output.parsed.markdown.parsed_work_bytes(), 0);
        assert_ne!(
            output.prompt.as_ref().unwrap().created_on,
            std::thread::current().id()
        );
        runtime.markdown_prepared(runtime.target, "message", run, ready(Ok(output)));
        let accepted = runtime
            .prepared_prompt(&source)
            .expect("actual worker accepted literal source");
        assert_eq!(&*accepted.full.plain(), body);
        assert_eq!(&*accepted.shown(false).plain(), prompt::preview(&body).0);
        assert!(accepted.full.plain_ready());
        let plain = accepted.full.plain();
        let revision = runtime.cache["message"].revision;
        let mut metadata = (*handle.snapshot()).clone();
        metadata.generation += 1;
        Arc::make_mut(&mut metadata.items[0]).at = Some(1_700_000_000_000);
        handle.publish_delta_snapshot_for_test(metadata);
        runtime.refresh();
        let current = &runtime.snapshot.items[0];
        assert!(Arc::ptr_eq(
            &plain,
            &runtime.prepared_prompt(current).unwrap().full.plain()
        ));
        assert_eq!(runtime.cache["message"].revision, revision);
        let mut replacement = (*handle.snapshot()).clone();
        replacement.generation += 1;
        if let ItemContent::Text { text, .. } =
            &mut Arc::make_mut(&mut replacement.items[0]).content
        {
            text.replace_range(..2, "!!");
        }
        handle.publish_snapshot_for_test(replacement);
        runtime.refresh();
        assert!(
            runtime
                .prepared_prompt(&runtime.snapshot.items[0])
                .is_none(),
            "same-ID rewrite must not relabel the old literal DTO"
        );
        drain_fixture(&mut runtime);
        let ItemContent::Text { text, .. } = &runtime.snapshot.items[0].content else {
            unreachable!()
        };
        assert_eq!(
            &*runtime
                .prepared_prompt(&runtime.snapshot.items[0])
                .unwrap()
                .full
                .plain(),
            text
        );
        runtime.show(false);
        retirement_barrier();
    }

    #[test]
    fn local_prompt_retires_actual_raw_source_and_literal_spans_before_output_readmission() {
        let source = prompt::LocalSource::new(format!(
            "literal local **source**\r\n{}",
            "λ raw queued bytes ".repeat(32_768)
        ));
        let body_len = source.text().len();
        let target = target();
        let mut state = State::default();
        state.stage_local(target, &source, crate::settings::EditorSyntaxTheme::Auto);
        let (_, entry) = state.local.as_ref().unwrap();
        let shared = entry.shared.clone();
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let admitted = slots.clone().try_acquire_owned().unwrap();
        let mut output = std::thread::spawn(move || execute(shared, admitted).unwrap())
            .join()
            .unwrap();
        let (raw_sent, raw_disposed) = std::sync::mpsc::channel();
        *source.disposed.lock().unwrap() = Some(raw_sent);
        let (spans_sent, spans_disposed) = std::sync::mpsc::channel();
        output
            .prompt
            .as_mut()
            .unwrap()
            .full
            .watch_raw_spans(spans_sent);
        assert_eq!(
            &*output.prompt.as_ref().unwrap().full.plain(),
            source.text().trim_end()
        );
        let hold = hold_retirement();
        state.clear_local();
        drop(source);
        Ready::new(Ok(output)); // discarded queued output, without UI installation
        assert!(
            slots.clone().try_acquire_owned().is_err(),
            "discarded literal reply retains output admission until actual raw retirement"
        );
        assert!(raw_disposed.try_recv().is_err());
        assert!(spans_disposed.try_recv().is_err());
        drop(hold);
        let worker = retirement_barrier();
        let (raw_thread, raw_bytes) = raw_disposed.recv_timeout(Duration::from_secs(5)).unwrap();
        let (spans_thread, _) = spans_disposed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(raw_bytes, body_len);
        assert_eq!(raw_thread, worker);
        assert_eq!(spans_thread, worker);
        assert_ne!(worker, std::thread::current().id());
        assert!(
            slots.try_acquire_owned().is_ok(),
            "actual source and span payloads drop before this private admission returns"
        );
    }

    #[test]
    fn local_prompt_acceptance_is_exact_and_real_failures_retry_without_poisoning_replacements() {
        let target = target();
        let source = prompt::LocalSource::new("original raw local\r\n".repeat(4096));
        let replacement = prompt::LocalSource::new("new source with same local role".into());
        let mut state = State::default();
        state.stage_local(target, &source, crate::settings::EditorSyntaxTheme::Auto);
        let (_, entry) = state.local.as_ref().unwrap();
        let old_run = entry.run;
        let old = Ready::new(execute(entry.shared.clone(), permit()));
        state.stage_local(
            target,
            &replacement,
            crate::settings::EditorSyntaxTheme::Auto,
        );
        assert!(
            state
                .accept_local(target, old_run, &replacement, old)
                .is_none(),
            "old queued reply cannot install on replacement source"
        );
        let (_, entry) = state.local.as_ref().unwrap();
        let run = entry.run;
        let wrong = Target {
            generation: target.generation + 1,
            ..target
        };
        assert!(
            state
                .accept_local(
                    wrong,
                    run,
                    &replacement,
                    Ready::new(Err("stale error".into()))
                )
                .is_none()
        );
        assert!(
            state.error().is_none(),
            "stale target error cannot quarantine the current queue"
        );
        assert!(
            state
                .accept_local(
                    target,
                    run,
                    &replacement,
                    Ready::new(Err("real local failure".into()))
                )
                .is_none()
        );
        assert!(state.local.is_none());
        assert_eq!(state.error(), Some("real local failure"));
        state.stage_local(
            target,
            &replacement,
            crate::settings::EditorSyntaxTheme::Auto,
        );
        assert!(
            state.local.is_none(),
            "real failure is visible, not a silently stuck once-stream"
        );
        state.retry();
        state.stage_local(
            target,
            &replacement,
            crate::settings::EditorSyntaxTheme::Auto,
        );
        let (_, entry) = state.local.as_ref().unwrap();
        let run = entry.run;
        let output = Ready::new(execute(entry.shared.clone(), permit()));
        let mut presentation = prompt::LocalPresentation::default();
        state
            .accept_local(target, run, &replacement, output)
            .unwrap()
            .install_local(&mut presentation);
        assert_eq!(
            &*presentation.prepared.as_ref().unwrap().full.plain(),
            replacement.text()
        );
        drop(presentation);
        drop(state);
        retirement_barrier();
    }

    #[test]
    fn local_input_reusing_a_retained_lease_still_obeys_shared_entry_admission() {
        let target = target();
        let source = prompt::LocalSource::new("local canonical bytes".into());
        let mut state = State::default();
        state.stage_local(target, &source, crate::settings::EditorSyntaxTheme::Auto);
        assert!(source.lease.lock().unwrap().is_some());
        state.clear_local();
        for index in 0..QUEUED_ITEMS {
            let mut next = (*item("small actual prose source")).clone();
            next.id = format!("history-{index}");
            state.stage(
                &Arc::new(next),
                false,
                crate::settings::EditorSyntaxTheme::Auto,
                |_, _| false,
            );
        }
        assert_eq!(state.entries.len(), QUEUED_ITEMS);
        state.stage_local(target, &source, crate::settings::EditorSyntaxTheme::Auto);
        assert!(
            state.local.is_none(),
            "retained local lease cannot bypass the shared entry bound"
        );
        state.remove("history-0");
        state.stage_local(target, &source, crate::settings::EditorSyntaxTheme::Auto);
        assert!(state.local.is_some());
        let mut overflow = (*item("another valid source")).clone();
        overflow.id = "overflow".into();
        state.stage(
            &Arc::new(overflow),
            false,
            crate::settings::EditorSyntaxTheme::Auto,
            |_, _| false,
        );
        assert!(
            state.was_deferred(),
            "transcript admission counts the local entry too"
        );
        assert_eq!(
            state.entries.len() + usize::from(state.local.is_some()),
            QUEUED_ITEMS
        );
        drop(state);
        drop(source);
        retirement_barrier();
    }

    #[test]
    fn unpolled_transcript_subscription_retires_final_request_and_canonical_before_lease() {
        let source =
            item(&"# Actual retained transcript\n\nlarge literal body λ\n\n".repeat(16_384));
        let raw = Arc::downgrade(&source);
        let mut state = State::default();
        state.stage(
            &source,
            true,
            crate::settings::EditorSyntaxTheme::Auto,
            |_, _| false,
        );
        let (run, shared) = job(&state);
        let lease = Arc::downgrade(&shared.lease);
        let worker = shared.clone();
        let output = std::thread::spawn(move || execute(worker, permit()).unwrap())
            .join()
            .unwrap();
        let canonical = shared
            .canonical
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .parsed
            .markdown
            .items()
            .iter()
            .find_map(|item| match item {
                markdown::Item::Heading(_, text) | markdown::Item::Paragraph(text) => {
                    Some(text.plain())
                }
                _ => None,
            })
            .expect("actual growing parser retains canonical text");
        let plain = Arc::downgrade(&canonical);
        drop(canonical);
        output.discard();
        let subscription = stream(&Job {
            target: target(),
            id: Some("message".into()),
            run,
            shared: shared.clone(),
        });
        state.clear();
        drop(source);
        drop(shared);
        retirement_barrier();
        assert!(raw.upgrade().is_some());
        assert!(plain.upgrade().is_some());
        let held = hold_retirement();
        drop(subscription); // actual never-polled stream is now the final owner
        assert!(
            raw.upgrade().is_some(),
            "UI stream cancellation cannot destroy the actual transcript request"
        );
        assert!(
            plain.upgrade().is_some(),
            "UI stream cancellation cannot destroy the canonical plain allocation"
        );
        assert!(
            lease.upgrade().is_some(),
            "same source admission survives until raw retirement"
        );
        drop(held);
        retirement_barrier();
        assert!(raw.upgrade().is_none());
        assert!(plain.upgrade().is_none());
        assert!(lease.upgrade().is_none());
    }

    #[test]
    fn unpolled_local_subscription_retires_actual_source_before_its_surviving_lease() {
        let target = target();
        let source = prompt::LocalSource::new("actual unpolled local source λ\r\n".repeat(16_384));
        let bytes = source.text().len();
        let (sent, disposed) = std::sync::mpsc::channel();
        *source.disposed.lock().unwrap() = Some(sent);
        let mut state = State::default();
        state.stage_local(target, &source, crate::settings::EditorSyntaxTheme::Auto);
        let (_, entry) = state.local.as_ref().unwrap();
        let lease = Arc::downgrade(&entry.shared.lease);
        let subscription = stream(&Job {
            target,
            id: None,
            run: entry.run,
            shared: entry.shared.clone(),
        });
        // Never poll: the real captured async input must still carry source
        // admission when its State/queued canonical owners disappear first.
        state.clear_local();
        drop(source);
        retirement_barrier();
        assert!(state.local.is_none());
        assert!(disposed.try_recv().is_err());
        assert!(
            lease.upgrade().is_some(),
            "unpolled local input still holds its actual source lease without an Entry"
        );
        let held = hold_retirement();
        drop(subscription);
        assert!(
            lease.upgrade().is_some(),
            "UI cancellation cannot release source capacity before raw retirement"
        );
        assert!(disposed.try_recv().is_err());
        drop(held);
        let worker = retirement_barrier();
        let (thread, actual) = disposed.recv_timeout(Duration::from_secs(5)).unwrap();
        assert_eq!(actual, bytes);
        assert_eq!(thread, worker);
        assert_ne!(thread, std::thread::current().id());
        assert!(
            lease.upgrade().is_none(),
            "actual local raw source drops before its final source lease"
        );
    }

    #[test]
    fn authenticated_metadata_retains_exact_graph_and_retires_old_snapshot_items_after_commit() {
        for growing in [false, true] {
            let source = item(&format!(
                "# Stable rich source\n\n{}",
                "large unchanged body ".repeat(50_000)
            ));
            let (mut runtime, handle) = view::prepared_runtime_for_test(&[(*source).clone()]);
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            snapshot.busy = growing;
            if growing {
                let ItemContent::Text { text, .. } =
                    &mut Arc::make_mut(&mut snapshot.items[0]).content
                else {
                    unreachable!()
                };
                text.push_str("\n\nActual growing tail");
            }
            handle.publish_delta_snapshot_for_test(snapshot);
            runtime.refresh();
            drain_fixture(&mut runtime);
            retirement_barrier();
            assert_eq!(
                runtime.preparation.entries.contains_key("message"),
                growing,
                "the growing case must own an actually accepted managed canonical"
            );
            let cached = &runtime.cache["message"];
            let revision = cached.revision;
            let document = cached.document.as_ref().unwrap().clone();
            let graph = document.items().as_ptr();
            let old = cached.item.as_ref().unwrap().clone();
            let old_snapshot = Arc::downgrade(&runtime.snapshot);
            let old_item = Arc::downgrade(&old);
            let disposed = watch_metadata_retirement(&old);
            let version = runtime.preparation.entries.get("message").map(|entry| {
                assert!(!entry.running);
                entry.shared.latest.lock().unwrap().version
            });
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            Arc::make_mut(&mut snapshot.items[0]).at = Some(1_700_000_000_000);
            handle.publish_delta_snapshot_for_test(snapshot);
            assert!(handle.known_append(&old, &handle.snapshot().items[0]));
            drop(old);
            let parser_before = markdown::parser_work_bytes_on_thread();
            runtime.refresh();
            assert_eq!(
                markdown::parser_work_bytes_on_thread(),
                parser_before,
                "metadata-only refresh performs no UI body parse"
            );
            let cached = &runtime.cache["message"];
            assert_eq!(
                cached.revision, revision,
                "authenticated equal-body metadata preserves the prepared revision"
            );
            assert_eq!(
                cached.document.as_ref().unwrap().items().as_ptr(),
                graph,
                "authenticated equal-body metadata preserves the actual rich graph"
            );
            assert!(
                Arc::ptr_eq(cached.item.as_ref().unwrap(), &runtime.snapshot.items[0]),
                "authenticated equal-body metadata advances only exact current item authority"
            );
            assert_eq!(cached.stamp.as_ref().unwrap().0, 1_700_000_000_000);
            assert!(runtime.preparation.subscriptions(runtime.target).is_empty());
            assert!(old_snapshot.upgrade().is_none());
            let (thread, raw_gone) = disposed.recv_timeout(Duration::from_secs(5)).unwrap();
            assert!(
                raw_gone && old_item.upgrade().is_none(),
                "actual old Snapshot and raw item owners are gone before metadata capacity returns"
            );
            assert_eq!(
                thread,
                retirement_barrier(),
                "actual old raw item retirement belongs to the reclaimer"
            );
            if let Some(version) = version {
                let entry = &runtime.preparation.entries["message"];
                assert!(!entry.running);
                let latest = entry.shared.latest.lock().unwrap();
                assert_eq!(latest.version, version);
                assert!(
                    latest
                        .input
                        .transcript()
                        .is_some_and(|item| Arc::ptr_eq(item, &runtime.snapshot.items[0]))
                );
            }
            // The managed canonical cursor must accept a later real append.
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            let ItemContent::Text { text, .. } = &mut Arc::make_mut(&mut snapshot.items[0]).content
            else {
                unreachable!()
            };
            text.push_str("\n\nFollowing actual provider append.");
            handle.publish_delta_snapshot_for_test(snapshot);
            runtime.refresh();
            drain_fixture(&mut runtime);
            let cached = &runtime.cache["message"];
            assert!(!cached.placeholder);
            assert!(Arc::ptr_eq(
                cached.item.as_ref().unwrap(),
                &runtime.snapshot.items[0]
            ));
            assert!(cached.document.as_ref().unwrap().items().len() >= 3);
            runtime.show(false);
            retirement_barrier();
        }
    }

    #[test]
    fn metadata_reuse_refuses_full_resets_rewrites_shape_changes_and_inflight_work() {
        for case in [
            "full reset",
            "same-length rewrite",
            "thinking",
            "phase",
            "role",
            "inflight palette",
            "finalization",
        ] {
            let source = item(&format!(
                "{}{}",
                "a".repeat(16_384),
                "same tail ".repeat(32)
            ));
            let (mut runtime, handle) = view::prepared_runtime_for_test(&[(*source).clone()]);
            if case == "finalization" {
                let mut snapshot = (*handle.snapshot()).clone();
                snapshot.generation += 1;
                snapshot.busy = true;
                let ItemContent::Text { text, .. } =
                    &mut Arc::make_mut(&mut snapshot.items[0]).content
                else {
                    unreachable!()
                };
                text.push_str(" actual unfinished tail");
                handle.publish_delta_snapshot_for_test(snapshot);
                runtime.refresh();
                drain_fixture(&mut runtime);
                assert!(runtime.preparation.entries.contains_key("message"));
            }
            retirement_barrier();
            let cached = &runtime.cache["message"];
            let revision = cached.revision;
            let document = cached.document.as_ref().unwrap().clone();
            let previous = cached.item.as_ref().unwrap().clone();
            if case == "inflight palette" {
                runtime.set_syntax_theme(crate::settings::EditorSyntaxTheme::SolarizedDark);
                assert!(runtime.preparation.entries["message"].running);
            }
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            if case == "finalization" {
                snapshot.busy = false;
            }
            let next = Arc::make_mut(&mut snapshot.items[0]);
            next.at = Some(1_700_000_000_000);
            match (&mut next.content, case) {
                (ItemContent::Text { text, .. }, "same-length rewrite") => {
                    text.replace_range(0..1, "b");
                }
                (ItemContent::Text { thinking, .. }, "thinking") => *thinking = true,
                (ItemContent::Text { phase, .. }, "phase") => *phase = chat::Phase::Commentary,
                (_, "role") => next.role = chat::Role::User,
                _ => {}
            }
            if case == "same-length rewrite" {
                assert_eq!(
                    ChangeToken::of(&previous.content),
                    ChangeToken::of(&next.content)
                );
            }
            if case == "full reset" {
                handle.publish_snapshot_for_test(snapshot);
            } else {
                handle.publish_delta_snapshot_for_test(snapshot);
            }
            let parser_before = markdown::parser_work_bytes_on_thread();
            runtime.refresh();
            assert_eq!(
                markdown::parser_work_bytes_on_thread(),
                parser_before,
                "metadata and finalization never parse a large body on UI"
            );
            let cached = &runtime.cache["message"];
            assert!(
                cached.preparing,
                "{case}: a new current preparation is required"
            );
            if matches!(case, "inflight palette" | "finalization") {
                assert!(
                    Arc::ptr_eq(cached.item.as_ref().unwrap(), &previous),
                    "a metadata publication cannot relabel an in-flight old graph as current"
                );
            } else {
                assert!(
                    cached.placeholder,
                    "{case}: unproved source cannot expose the old graph"
                );
            }
            drain_fixture(&mut runtime);
            let cached = &runtime.cache["message"];
            assert!(!cached.placeholder);
            assert!(Arc::ptr_eq(
                cached.item.as_ref().unwrap(),
                &runtime.snapshot.items[0]
            ));
            assert_ne!(cached.revision, revision);
            assert_ne!(
                cached.document.as_ref().unwrap().items().as_ptr(),
                document.items().as_ptr()
            );
            runtime.show(false);
            retirement_barrier();
        }
    }

    #[test]
    fn metadata_retirement_pressure_uses_normal_admission_and_idle_release_without_retry() {
        use iced::futures::StreamExt;
        for (growing, large_id) in [(false, false), (true, false), (false, true)] {
            let mut source = item(&if large_id {
                "x".into()
            } else {
                "source ".repeat(4096)
            });
            if large_id {
                Arc::make_mut(&mut source).id = "opaque".repeat(16_384);
            }
            let id = source.id.clone();
            let (mut runtime, handle) = view::prepared_runtime_for_test(&[(*source).clone()]);
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            snapshot.busy = growing;
            if growing {
                let ItemContent::Text { text, .. } =
                    &mut Arc::make_mut(&mut snapshot.items[0]).content
                else {
                    unreachable!()
                };
                text.push_str(" actual growing tail");
            }
            handle.publish_delta_snapshot_for_test(snapshot);
            runtime.refresh();
            drain_fixture(&mut runtime);
            retirement_barrier();
            let previous = runtime.cache[&id].item.as_ref().unwrap().clone();
            let pressure = Lease::default();
            let spare = if large_id { 4096 } else { 0 };
            assert!(pressure.resize(QUEUED_BYTES - RESERVED.load(Ordering::Relaxed) - spare));
            let mut snapshot = (*handle.snapshot()).clone();
            snapshot.generation += 1;
            Arc::make_mut(&mut snapshot.items[0]).at = Some(1_700_000_000_000);
            handle.publish_delta_snapshot_for_test(snapshot);
            runtime.refresh();
            assert!(
                Arc::ptr_eq(runtime.cache[&id].item.as_ref().unwrap(), &previous),
                "retirement pressure retains the proven old owner rather than promoting it"
            );
            assert!(runtime.cache[&id].preparing);
            let executor = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            let mut events = Box::pin(availability_stream());
            executor.block_on(events.next()).unwrap();
            drop(pressure);
            let message = executor
                .block_on(async {
                    tokio::time::timeout(Duration::from_secs(5), events.next()).await
                })
                .unwrap()
                .unwrap();
            let target = runtime.target;
            let mut app = crate::app::tests::context_inspector_test_app(false);
            app.cards
                .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
            drop(app.update(message));
            let runtime = app.chat_runtime_mut(target).unwrap();
            drain_fixture(runtime);
            assert!(!runtime.cache[&id].placeholder && !runtime.preparation.waiting());
            assert!(Arc::ptr_eq(
                runtime.cache[&id].item.as_ref().unwrap(),
                &runtime.snapshot.items[0]
            ));
            runtime.show(false);
            retirement_barrier();
            executor.shutdown_background();
        }
    }

    #[test]
    fn rehydrated_readers_verify_only_exact_logical_paths_and_actual_prefixes_on_worker() {
        let previous = reader_source::Source::new("wide line\n").proof();
        let code = "wide line\nnew hidden tail\n";
        let source = item(&format!("```text\n{code}```\n\n```text\n{code}```"));
        let target = target();
        let mut state = configured(target, &source);
        let identity = selection::logical_identity(&source.id, &[0, 0]);
        let unmounted = selection::logical_identity(&source.id, &[9]);
        state.set_reader_proofs(Arc::new(
            [(identity, previous), (unmounted, previous)].into(),
        ));
        assert!(state.stage(
            &source,
            false,
            crate::settings::EditorSyntaxTheme::Auto,
            |_, _| false
        ));
        let (_, shared) = job(&state);
        let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        assert_eq!(
            output.reader_verifications.len(),
            1,
            "equal text cannot bless another logical path"
        );
        assert_eq!(
            output.reader_verifications[&identity],
            (reader_source::Source::new(code).proof(), previous)
        );
        output.discard();
        drop(state);
        retirement_barrier();

        let input = "rewritten input";
        let output = "wide line\nnew hidden tail\n";
        let source = Arc::new(chat::Item {
            content: ItemContent::Tool {
                name: "fixture".into(),
                input: input.into(),
                output: output.into(),
                status: chat::ToolStatus::Succeeded,
            },
            ..(*source).clone()
        });
        let mut state = configured(target, &source);
        let mut proofs = HashMap::new();
        let old_input = reader_source::Source::new("original input").proof();
        for (part, proof) in [(4, old_input), (5, previous)] {
            let logical = selection::logical_identity(&source.id, &[part]);
            for axis in [scroll::ReaderAxis::Horizontal, scroll::ReaderAxis::Vertical] {
                proofs.insert(scroll::reader_key(logical, axis), proof);
            }
        }
        state.set_reader_proofs(Arc::new(proofs));
        assert!(state.stage(
            &source,
            false,
            crate::settings::EditorSyntaxTheme::Auto,
            |_, _| false
        ));
        let (_, shared) = job(&state);
        let prepared = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        assert!(
            prepared.parsed.markdown.items().is_empty(),
            "tool verification does not parse its output as Markdown"
        );
        assert_eq!(
            prepared.reader_verifications.len(),
            2,
            "both output axes survive; rewritten input axes fail closed"
        );
        let output_logical = selection::logical_identity(&source.id, &[5]);
        for axis in [scroll::ReaderAxis::Horizontal, scroll::ReaderAxis::Vertical] {
            assert_eq!(
                prepared.reader_verifications[&scroll::reader_key(output_logical, axis)],
                (reader_source::Source::new(output).proof(), previous)
            );
        }
        let worker_output = prepared
            .prepared_tools
            .as_ref()
            .unwrap()
            .output
            .as_ref()
            .unwrap()
            .text
            .clone();
        let mut cached = CachedItem::preparing(&source, crate::settings::EditorSyntaxTheme::Auto);
        prepared.install(&mut cached, false, true);
        assert!(
            Arc::ptr_eq(&worker_output, &cached.tool_output.as_ref().unwrap().text),
            "UI installation shares the worker allocation"
        );
        assert!(!cached.placeholder && !cached.preparing);
        assert_eq!(cached.tool_input.as_ref().unwrap().text.as_ref(), input);
        assert_eq!(cached.tool_output.as_ref().unwrap().text.as_ref(), output);
        drop(cached);
        drop(state);
        retirement_barrier();
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
        output.install(&mut cached, false, false);
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
        output.install(&mut cached, false, false);
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
    fn zero_rendered_reference_document_retires_its_actual_parser_off_thread() {
        let body: String = (0..4_000)
            .map(|index| format!("[reference-{index}]: https://example.test/{index}\n"))
            .collect();
        let source = item(&body);
        let mut state = configured(target(), &source);
        state.stage(
            &source,
            true,
            crate::settings::EditorSyntaxTheme::Auto,
            |_, _| false,
        );
        let (_, shared) = job(&state);
        let output = execute(shared, permit()).unwrap();
        let mut cached = CachedItem::preparing(&source, crate::settings::EditorSyntaxTheme::Auto);
        output.install(&mut cached, true, false);
        assert!(cached.markdown.items().is_empty());
        assert!(cached.blocks.is_empty());
        assert!(cached.markdown.reference_count() >= 3_999);
        assert!(cached.markdown.has_retained_allocations());
        // Remove the canonical copy first: the cached document is the final
        // owner, including its non-rendered reference definitions.
        drop(state);
        retirement_barrier();
        let (sender, receiver) = std::sync::mpsc::channel();
        cached.markdown.watch_disposal(sender);
        drop(cached);
        let disposed = receiver
            .recv_timeout(Duration::from_secs(5))
            .expect("actual Content disposal");
        assert_eq!(disposed, retirement_barrier());
        assert_ne!(disposed, std::thread::current().id());
    }

    #[test]
    fn retained_composite_factories_keep_one_real_worker_graph_until_last_ui_owner_drops() {
        use iced::advanced::{layout, widget::Tree};
        let body = (0..120).map(|index| format!(
            "> Quote {index} λ\n>\n> - nested **list**\n> - second item\n\n- outer {index}\n  - inner cell\n\n| header | second |\n| --- | --- |\n| row {index} | Unicode é |\n\n"
        )).collect::<String>();
        for cache_first in [true, false] {
            let source = item(&body);
            let mut state = configured(target(), &source);
            state.stage(
                &source,
                false,
                crate::settings::EditorSyntaxTheme::Auto,
                |_, _| false,
            );
            let (_, shared) = job(&state);
            let mut output = std::thread::spawn(move || execute(shared, permit()).unwrap())
                .join()
                .unwrap();
            assert_ne!(output.document.created_on, std::thread::current().id());
            let roots = output.parsed.markdown.items().len();
            assert!(roots > 4);
            assert!(
                output
                    .parsed
                    .markdown
                    .items()
                    .iter()
                    .any(|item| matches!(item, markdown::Item::Quote(_)))
            );
            assert!(
                output
                    .parsed
                    .markdown
                    .items()
                    .iter()
                    .any(|item| matches!(item, markdown::Item::List { .. }))
            );
            assert!(
                output
                    .parsed
                    .markdown
                    .items()
                    .iter()
                    .any(|item| matches!(item, markdown::Item::Table { .. }))
            );
            let (sent, disposed) = std::sync::mpsc::channel();
            output.document.watch_disposal(sent);
            let mut cached =
                CachedItem::preparing(&source, crate::settings::EditorSyntaxTheme::Auto);
            output.install(&mut cached, false, false);
            let document = cached.document.as_ref().unwrap();
            let mut leaves = Vec::new();
            fn plain(items: &[markdown::Item], leaves: &mut Vec<String>) {
                for item in items {
                    match item {
                        markdown::Item::Paragraph(text) | markdown::Item::Heading(_, text) => {
                            leaves.push(text.plain().to_string())
                        }
                        markdown::Item::Quote(items) => plain(items, leaves),
                        markdown::Item::List { bullets, .. } => {
                            for bullet in bullets {
                                let (markdown::Bullet::Point { items }
                                | markdown::Bullet::Task { items, .. }) = bullet;
                                plain(items, leaves);
                            }
                        }
                        markdown::Item::Table { columns, rows } => {
                            for column in columns {
                                plain(&column.header, leaves);
                            }
                            for row in rows {
                                for cell in &row.cells {
                                    plain(cell, leaves);
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            plain(cached.markdown.items(), &mut leaves);
            let mut shared_leaves = Vec::new();
            plain(document.items(), &mut shared_leaves);
            assert_eq!(
                shared_leaves, leaves,
                "the immutable graph preserves every rendered source in parser order"
            );
            let clones = markdown::cloned_items();
            let mut element = prose::view(
                &cached,
                design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE),
                target(),
                None,
            );
            let renderer = crate::app::tests::headless_renderer();
            let mut tree = Tree::new(&element);
            element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(iced::Size::ZERO, iced::Size::new(720.0, 900.0)),
            );
            assert_eq!(
                markdown::cloned_items(),
                clones,
                "real AST Item::clone never runs while rebuilding/mounting composite UI factories"
            );
            drop(state);
            retirement_barrier();
            if cache_first {
                drop(cached);
                retirement_barrier();
                assert!(
                    disposed.try_recv().is_err(),
                    "actual retained row factories still own the original graph"
                );
                let held = hold_retirement();
                drop(element);
                drop(tree);
                assert!(
                    disposed.try_recv().is_err(),
                    "UI eviction transfers raw graph; it does not destruct it"
                );
                drop(held);
            } else {
                drop(element);
                drop(tree);
                retirement_barrier();
                assert!(
                    disposed.try_recv().is_err(),
                    "the current cache still owns the graph after widget eviction"
                );
                drop(cached);
            }
            let (thread, actual_roots) = disposed
                .recv_timeout(Duration::from_secs(5))
                .expect("actual composite graph disposal");
            assert_eq!(actual_roots, roots);
            assert_ne!(thread, std::thread::current().id());
            assert_eq!(thread, retirement_barrier());
        }
    }

    #[test]
    fn rejected_composite_output_destroys_raw_graph_before_reclaimer_barrier_and_admission_release()
    {
        let source = item(&"> quote\n>\n> - nested list\n\n".repeat(200));
        let mut state = configured(target(), &source);
        state.stage(
            &source,
            false,
            crate::settings::EditorSyntaxTheme::Auto,
            |_, _| false,
        );
        let (_, shared) = job(&state);
        let mut output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        let (sent, disposed) = std::sync::mpsc::channel();
        output.document.watch_disposal(sent);
        let held = hold_retirement();
        output.discard();
        assert!(disposed.try_recv().is_err());
        drop(held);
        let barrier = retirement_barrier();
        let (thread, roots) = disposed.try_recv().expect(
            "outer retirement disposes its actual raw AST before acknowledging the barrier",
        );
        assert!(roots > 0);
        assert_eq!(thread, barrier);
        assert_ne!(thread, std::thread::current().id());
        drop(state);
    }

    #[test]
    fn large_image_alt_and_literal_title_use_flow_and_keep_exact_child_selection_sources() {
        use iced::advanced::{
            layout,
            widget::{Tree, tree},
        };
        let alt = format!(
            "{}**strong**{}",
            "ALT λ é office ".repeat(1800),
            " suffix".repeat(200)
        );
        let expected_alt = alt.replace("**", "");
        let title = "TITLE *literal* λ é ".repeat(1800);
        let source = item(&format!(
            "![{alt}](https://example.test/private.png \"{title}\")"
        ));
        let mut state = configured(target(), &source);
        state.stage(
            &source,
            false,
            crate::settings::EditorSyntaxTheme::Auto,
            |_, _| false,
        );
        let (_, shared) = job(&state);
        let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        let [
            markdown::Item::Image {
                alt, title_text, ..
            },
        ] = output.parsed.markdown.items()
        else {
            panic!("actual parsed image");
        };
        assert!(alt.plain_ready() && title_text.plain_ready());
        assert_eq!(alt.plain().as_ref(), expected_alt);
        assert_eq!(title_text.plain().as_ref(), title);
        let mut cached = CachedItem::preparing(&source, crate::settings::EditorSyntaxTheme::Auto);
        output.install(&mut cached, false, false);
        let before = markdown::materialized_bytes();
        let mut element = prose::view(
            &cached,
            design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE),
            target(),
            None,
        );
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(&element);
        element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(720.0, 900.0)),
        );
        assert_eq!(
            markdown::materialized_bytes(),
            before,
            "image views never style/materialize their full long alt or title on the UI thread"
        );
        fn collect(tree: &Tree, result: &mut Vec<(u64, Option<bool>)>) {
            if tree.tag == tree::Tag::of::<selectable_prose::State>() {
                let state = tree.state.downcast_ref::<selectable_prose::State>();
                result.push((
                    state.logical.as_ref().unwrap().identity,
                    state.flow_readiness(),
                ));
            }
            for child in &tree.children {
                collect(child, result);
            }
        }
        let mut leaves = Vec::new();
        collect(&tree, &mut leaves);
        assert_eq!(leaves.len(), 2);
        let alt_id = selection::logical_identity(&source.id, &[0, 0, 0]);
        let title_id = selection::logical_identity(&source.id, &[0, 0, 1]);
        assert_eq!(
            leaves,
            vec![(alt_id, Some(false)), (title_id, Some(false))],
            "both actual native image leaves await the worker, with original child0/1 identities"
        );
        let mut catalog = selection::Catalog::builder();
        catalog.markdown(&source.id, &[0], cached.markdown.items());
        let catalog = catalog.finish();
        for (identity, expected) in [(alt_id, expected_alt.as_str()), (title_id, title.as_str())] {
            let plain = catalog.text(identity).unwrap();
            assert_eq!(plain.as_ref(), expected);
            let mut leaf = selection::Catalog::builder();
            leaf.logical(identity, plain);
            let leaf = leaf.finish();
            let (first, last) = leaf.endpoints().unwrap();
            assert_eq!(
                catalog.copy(first, last).as_deref(),
                Some(expected),
                "full original image selection source survives virtual worker preparation"
            );
        }
        drop(element);
        drop(tree);
        drop(cached);
        drop(state);
        retirement_barrier();
    }

    #[test]
    fn large_coordination_runtime_admits_worker_and_retires_real_graphs() {
        let prompt = "# Task λ\n\n> quoted **work**\n>\n> - nested item\n\n| first | second |\n| --- | --- |\n| exact | é |\n\n```text\nsource line\n```\n\n".repeat(700);
        let result = "# Report\n\nExact returned rich result λ.\n\n".repeat(1000);
        for (provider, name, input, output) in [
            (Provider::Claude, "Agent", serde_json::json!({"prompt":prompt.clone(),"subagent_type":"Explore","run_in_background":false}).to_string(), result.clone()),
            (Provider::Codex, "spawn_agent", prompt.clone(), String::new()),
            (Provider::Omp, "task", serde_json::json!({"task":prompt.clone(),"context":result.clone(),"agent":"scout","name":"private-child"}).to_string(), String::new()),
        ] {
            let session = fresh_chat_session(provider, TerminalId::new(), "/project".into(), &Account::system(provider), false).unwrap();
            let handle = Arc::new(ClientHandle::fixture(session.clone(), Snapshot { ready:true, ..Snapshot::default() }));
            let mut runtime = Runtime::new(target(), session, Connection::Live(handle.clone()), true, crate::settings::EditorSyntaxTheme::Auto);
            let source = Arc::new(chat::Item { id:"message".into(), role:chat::Role::Assistant,
                content:ItemContent::Tool { name:name.into(), input, output, status:chat::ToolStatus::Succeeded }, started_at:None, at:None });
            let ui_work = coordination::preparations();
            handle.replace_snapshot_for_test(Snapshot { generation:1,ready:true,items:vec![source.clone()], ..Snapshot::default() });
            runtime.refresh();
            assert_eq!(coordination::preparations(), ui_work, "actual Runtime admission does not parse coordination on UI");
            assert!(runtime.cache["message"].preparing && runtime.cache["message"].coordination.is_none());
            assert!(runtime.preparation.queued_bytes() > 64*1024);
            let (run, shared) = job(&runtime.preparation);
            let mut output = std::thread::spawn(move || execute(shared, permit()).unwrap()).join().unwrap();
            let prepared = output.coordination.as_mut().expect("provider-specific worker recognition");
            assert_ne!(prepared.created_on, std::thread::current().id());
            assert!(prepared.contents().next().unwrap().items().len() > 4);
            let (sent, disposed) = std::sync::mpsc::channel();
            prepared.watch_disposal(sent);
            runtime.markdown_prepared(runtime.target, "message", run, ready(Ok(output)));
            let cached = &runtime.cache["message"];
            let coordinated = cached.coordination.as_ref().unwrap();
            assert!(coordinated.is_card());
            assert!(matches!(&coordinated.reading.kind, crate::chat::coordination::Kind::Spawn(spawn) if spawn.agents[0].prompt.inline()==Some(prompt.as_str())));
            assert!(runtime.turns.iter().flat_map(|turn| &turn.prose).any(|placed| placed.index==0 && placed.slot==turn::Slot::Card), "async completion regroups actual parent turns into the rich card slot");
            let address = coordinated.address();
            let contents = coordinated.contents().enumerate().map(|(index, content)| {
                let mut builder = selection::Catalog::builder();
                builder.markdown(&source.id, &[1,index as u32], content.items());
                let catalog = builder.finish();
                let (first,last) = catalog.endpoints().unwrap();
                (catalog.search_blocks(),catalog.copy(first,last).unwrap())
            }).collect::<Vec<_>>();
            let catalog = runtime.selection_document().0;
            for (blocks, text) in contents {
                for (identity, plain) in blocks {
                    assert_eq!(catalog.text(identity).as_deref(), Some(plain.as_ref()), "actual card catalog preserves every prepared logical source block");
                }
                assert!(text.contains("λ") || text.contains("Report"));
            }
            drop(catalog);
            let previous = runtime.cache.remove("message").unwrap();
            let rebuilt = CachedItem::updated(&source.content, runtime.syntax_theme, Some(previous), true, false, None);
            assert_eq!(rebuilt.coordination.as_ref().unwrap().address(), address, "disclosure reuses the same prepared canonical graphs/palettes");
            assert_eq!(coordination::preparations(), ui_work);
            runtime.cache.insert("message".into(), rebuilt);
            runtime.show(false);
            let thread = disposed.recv_timeout(Duration::from_secs(5)).expect("actual coordinated canonical graph destructor");
            assert_eq!(thread, retirement_barrier());
            assert_ne!(thread, std::thread::current().id());
        }
    }

    #[test]
    fn more_than_128_mixed_sources_settle_from_the_same_snapshot_without_retry() {
        let provider = Provider::Claude;
        let session = fresh_chat_session(
            provider,
            TerminalId::new(),
            "/project".into(),
            &Account::system(provider),
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
            crate::settings::EditorSyntaxTheme::Auto,
        );
        let sources = (0..300).map(|index| Arc::new(chat::Item {
            id:format!("source-{index}"), role:chat::Role::Assistant, started_at:None, at:None,
            content:match index % 3 {
                0 => ItemContent::Command { command:format!("cargo test package_{index}"), output:format!("exact ordinary output {index}"), status:chat::ToolStatus::Succeeded, exit_code:Some(0), label:String::new(),working_directory:"/project".into(),action:chat::CommandAction::Run,duration_ms:None },
                1 => ItemContent::Tool { name:"Agent".into(), input:serde_json::json!({"prompt":format!("# Exact prompt {index} λ\n\n> source body\n\n- nested item\n"),"subagent_type":"Explore","run_in_background":true}).to_string(), output:"Background task launched".into(), status:chat::ToolStatus::Succeeded },
                _ => ItemContent::Command { command:format!("printf '%s' '{}-{index}'", "x".repeat(3000)), output:format!("exact long-command output {index}"), status:chat::ToolStatus::Succeeded, exit_code:Some(0), label:String::new(),working_directory:"/project".into(),action:chat::CommandAction::Run,duration_ms:None },
            },
        })).collect::<Vec<_>>();
        let ui_work = coordination::preparations();
        handle.replace_snapshot_for_test(Snapshot {
            generation: 1,
            ready: true,
            items: sources.clone(),
            ..Snapshot::default()
        });
        runtime.refresh();
        assert!(runtime.preparation.waiting());
        assert!(runtime.preparation.entries.len() <= QUEUED_ITEMS);
        assert!(
            runtime.preparation.contains("source-299"),
            "the newest body has first admission ahead of historical work"
        );
        assert!(
            runtime.preparation.error().is_none(),
            "transient pressure is not a sticky source failure"
        );
        // Real worker completion and the production refill path use this same
        // immutable source generation. No new provider update, input or Retry.
        use iced::futures::StreamExt;
        let target = runtime.target;
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
        let events = availability_stream();
        let mut events = Box::pin(events);
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let Message::ChatUi(wake) = executor.block_on(events.next()).unwrap() else {
            panic!("admission wake")
        };
        drop(app.handle_chat_ui(wake));
        for _ in 0..400 {
            let runtime = app.chat_runtime_mut(target).unwrap();
            let next = runtime
                .preparation
                .entries
                .iter()
                .find(|(_, entry)| entry.running)
                .map(|(id, entry)| (id.clone(), entry.run, entry.shared.clone()));
            let Some((id, run, shared)) = next else {
                if !runtime.preparation.waiting() {
                    break;
                }
                retirement_barrier();
                let Message::ChatUi(wake) = executor.block_on(async {
                    tokio::time::timeout(Duration::from_secs(10), events.next())
                        .await
                        .expect("actual idle capacity wake")
                        .unwrap()
                }) else {
                    panic!("admission wake")
                };
                drop(app.handle_chat_ui(wake));
                continue;
            };
            let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
                .join()
                .unwrap();
            drop(app.handle_chat_ui(Action::MarkdownPrepared(target, id, run, ready(Ok(output)))));
        }
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(!runtime.preparation.waiting());
        assert!(runtime.preparation.entries.is_empty());
        assert!(runtime.preparation.error().is_none());
        for (index, source) in sources.iter().enumerate() {
            let cached = &runtime.cache[&source.id];
            assert!(!cached.preparing && !cached.placeholder);
            assert!(Arc::ptr_eq(cached.item.as_ref().unwrap(), source));
            assert_eq!(cached.coordination.is_some(), index % 3 == 1);
            if let Some(coordinated) = &cached.coordination {
                assert!(
                    matches!(&coordinated.reading.kind, crate::chat::coordination::Kind::Spawn(spawn) if spawn.agents[0].prompt.inline().unwrap().contains(&format!("Exact prompt {index} λ")))
                );
            }
        }
        runtime.refresh();
        assert!(
            runtime.preparation.entries.is_empty(),
            "unchanged accepted sources are not restaged"
        );
        assert_eq!(
            coordination::preparations(),
            ui_work,
            "neither saturation nor refill falls back to UI recognition/parsing"
        );
        runtime.show(false);
        retirement_barrier();
    }

    #[test]
    fn deferred_history_replacement_and_selected_child_reject_old_queued_output() {
        for child in [false, true] {
            let provider = Provider::Claude;
            let session = fresh_chat_session(
                provider,
                TerminalId::new(),
                "/project".into(),
                &Account::system(provider),
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
                crate::settings::EditorSyntaxTheme::Auto,
            );
            let make = |id: String, prompt: &str| {
                Arc::new(chat::Item {
                    id,
                    role: chat::Role::Assistant,
                    content: ItemContent::Tool {
                        name: "Agent".into(),
                        input: serde_json::json!({"prompt":prompt,"run_in_background":true})
                            .to_string(),
                        output: "Background task launched".into(),
                        status: chat::ToolStatus::Succeeded,
                    },
                    started_at: None,
                    at: None,
                })
            };
            let old = (0..160)
                .map(|index| {
                    make(
                        format!("old-{index}"),
                        "Old graph must never enter the next context.",
                    )
                })
                .collect::<Vec<_>>();
            handle.replace_snapshot_for_test(Snapshot {
                generation: 1,
                ready: true,
                items: old,
                ..Snapshot::default()
            });
            runtime.refresh();
            assert!(runtime.preparation.waiting());
            let (run, shared) = {
                let entry = &runtime.preparation.entries["old-159"];
                (entry.run, entry.shared.clone())
            };
            let old_output = std::thread::spawn(move || execute(shared, permit()).unwrap())
                .join()
                .unwrap();
            let source = make("old-159".into(), "# Exact replacement λ\n\nNew graph only.");
            let mut snapshot = Snapshot {
                generation: 2,
                ready: true,
                items: vec![source.clone()],
                ..Snapshot::default()
            };
            if child {
                snapshot.subagents.push(chat::Subagent {
                    id: "replacement-child".into(),
                    kind: "fixture".into(),
                    description: "Child".into(),
                    status: chat::ToolStatus::Succeeded,
                    model: None,
                    waiting: false,
                    detail: String::new(),
                    background: true,
                    items: vec![source.clone()],
                });
                runtime.selected_subagent = Some("replacement-child".into());
            }
            handle.replace_snapshot_for_test(snapshot);
            runtime.refresh();
            runtime.markdown_prepared(runtime.target, "old-159", run, ready(Ok(old_output)));
            assert!(
                runtime.presentation_cache()["old-159"]
                    .coordination
                    .is_none(),
                "old exact run cannot install into replaced source/selected context"
            );
            let (run, shared) = {
                let entry = &runtime.preparation.entries["old-159"];
                (entry.run, entry.shared.clone())
            };
            let output = std::thread::spawn(move || execute(shared, permit()).unwrap())
                .join()
                .unwrap();
            runtime.markdown_prepared(runtime.target, "old-159", run, ready(Ok(output)));
            assert!(
                !runtime.preparation.waiting(),
                "old deferred cursor is not carried into a new source context"
            );
            assert_eq!(runtime.presentation_cache().len(), 1);
            let cached = &runtime.presentation_cache()["old-159"];
            assert!(Arc::ptr_eq(cached.item.as_ref().unwrap(), &source));
            assert!(
                matches!(&cached.coordination.as_ref().unwrap().reading.kind,crate::chat::coordination::Kind::Spawn(spawn) if spawn.agents[0].prompt.inline()==Some("# Exact replacement λ\n\nNew graph only."))
            );
            runtime.show(false);
            retirement_barrier();
        }
    }

    #[test]
    fn retired_source_lease_release_emits_the_idle_admission_wake() {
        use iced::futures::StreamExt;
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let source = item("Owned source whose final lease is held by rejected output.");
        let mut state = configured(target(), &source);
        state.stage(
            &source,
            false,
            crate::settings::EditorSyntaxTheme::Auto,
            |_, _| false,
        );
        let (_, shared) = job(&state);
        let output = execute(shared, permit()).unwrap();
        let retained = Arc::downgrade(&output._owner);
        state.clear();
        retirement_barrier();
        let events = availability_stream();
        let mut events = Box::pin(events);
        executor
            .block_on(events.next())
            .expect("current-state subscription closes defer/install race");
        let release = hold_retirement();
        drop(ready(Ok(output)));
        // The source Arc and lease remain in the actual queued raw output.
        release.send(()).unwrap();
        executor.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    assert!(matches!(
                        events.next().await,
                        Some(Message::ChatUi(Action::PreparationAvailable))
                    ));
                    if retained.upgrade().is_none() {
                        break;
                    }
                }
            })
            .await
            .expect("final lease release wakes an idle UI without provider/input events");
        });
        retirement_barrier();
    }

    #[test]
    fn rejected_large_coordination_output_retires_all_raw_graphs_before_barrier() {
        let source = Arc::new(chat::Item { id:"message".into(), role:chat::Role::Assistant,
            content:ItemContent::Tool { name:"Agent".into(), input:serde_json::json!({"prompt":"> actual graph λ\n>\n> - nested item\n\n".repeat(4000),"subagent_type":"Explore"}).to_string(), output:String::new(),status:chat::ToolStatus::Succeeded },started_at:None,at:None });
        let mut state = configured(target(), &source);
        state.set_provider(Provider::Claude);
        state.stage(
            &source,
            false,
            crate::settings::EditorSyntaxTheme::Auto,
            |_, _| false,
        );
        let (_, shared) = job(&state);
        let mut output = std::thread::spawn(move || execute(shared, permit()).unwrap())
            .join()
            .unwrap();
        let (sent, disposed) = std::sync::mpsc::channel();
        output.coordination.as_mut().unwrap().watch_disposal(sent);
        let held = hold_retirement();
        drop(ready(Ok(output)));
        assert!(disposed.try_recv().is_err());
        drop(held);
        let barrier = retirement_barrier();
        assert_eq!(
            disposed
                .try_recv()
                .expect("raw coordinated output disposed within its retirement batch"),
            barrier
        );
        drop(state);
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
