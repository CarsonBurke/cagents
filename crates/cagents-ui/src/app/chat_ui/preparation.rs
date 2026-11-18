//! Owned Markdown work. Renderer caches and reader interaction stay on the UI.
use super::*;
use std::sync::atomic::AtomicUsize;

pub(super) const SOURCE_BYTES: usize = 32 * 1024 * 1024;
const QUEUED_BYTES: usize = 72 * 1024 * 1024;
const QUEUED_ITEMS: usize = 128;
// Bytes leased across the application, bounded by QUEUED_BYTES.
application_atomic!(fn reserved() -> AtomicUsize);
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
    /// How pull request links read in what this parses.
    labels: Arc<pull_request_links::Labels>,
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

/// Bytes charged to the application's budget. The budget is captured on the
/// UI thread at creation, since a lease may drop on the reclaimer.
pub(super) struct Lease {
    bytes: AtomicUsize,
    reserved: &'static AtomicUsize,
}
impl Default for Lease {
    fn default() -> Self {
        Self {
            bytes: AtomicUsize::new(0),
            reserved: reserved(),
        }
    }
}
impl Lease {
    fn bytes(&self) -> usize {
        self.bytes.load(Ordering::Relaxed)
    }
    fn resize(&self, bytes: usize) -> bool {
        let old = self.bytes();
        if bytes > old {
            let extra = bytes - old;
            if self
                .reserved
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
            self.reserved.fetch_sub(old - bytes, Ordering::Relaxed);
            if bytes < old {
                notify_available();
            }
        }
        self.bytes.store(bytes, Ordering::Relaxed);
        true
    }
}
impl Drop for Lease {
    fn drop(&mut self) {
        let bytes = self.bytes();
        self.reserved.fetch_sub(bytes, Ordering::Relaxed);
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
            .resize(owned.lease.bytes().saturating_add(bytes))
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

struct Shared {
    data: Option<Box<SharedData>>,
    // Capture admission on the UI thread where the application owns the job.
    // Its future may be polled on another executor thread.
    workers: Arc<Pool>,
}
struct SharedData {
    latest: Mutex<Request>,
    canonical: Mutex<Option<Canonical>>,
    cancelled: AtomicBool,
    // Where this input waits for a parser; see [`Pool`].
    priority: AtomicU64,
    place: Place,
    // Actual request/canonical allocations are destroyed before this lease.
    lease: Arc<Lease>,
}
impl std::ops::Deref for Shared {
    type Target = SharedData;
    fn deref(&self) -> &Self::Target {
        self.data.as_deref().expect("live preparation owner")
    }
}
impl Drop for Shared {
    fn drop(&mut self) {
        if let Some(data) = self.data.take() {
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
        Self {
            data: Some(Box::new(data)),
            workers: workers(),
        }
    }
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

struct Entry {
    shared: Arc<Shared>,
    run: u64,
    running: bool,
    /// What parses this run.
    claim: std::cell::Cell<Claim>,
}
/// What parses an entry's current run.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Claim {
    /// Nothing yet.
    Unclaimed,
    /// Its subscription, which may already hold a parser.
    Subscribed,
    /// The update that staged it handed it to a parser (`State::settle_tail`).
    HandedOver,
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
    labels: Arc<pull_request_links::Labels>,
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
                && Arc::ptr_eq(&latest.labels, &self.labels)
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
                claim: std::cell::Cell::new(Claim::Unclaimed),
                shared: Arc::new(Shared::new(SharedData {
                    latest: Mutex::new(Request {
                        input: Input::Local(source.clone()),
                        provider: Provider::Claude,
                        growing: false,
                        version: 1,
                        theme,
                        reader_proofs: Arc::default(),
                        // The prompt just sent reads its links as the
                        // transcript does, so they don't change on arrival.
                        labels: self.labels.clone(),
                    }),
                    // The prompt just sent shows where the reader is, so it
                    // ranks with the growing answer, and like it never takes
                    // the last parser while history waits: a short wait
                    // behind one history parse, rather than none left for it.
                    priority: AtomicU64::new(TAIL),
                    place: Place::default(),
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
    /// How pull request links read in what is parsed from now on, including
    /// what is staged but not yet started, so a long transcript staged just
    /// before the labels arrive is parsed once. A publication already parsed,
    /// or being parsed, keeps its labels until it changes or is
    /// [forgotten](Self::forget); the caller relabels it on arrival.
    pub(super) fn set_labels(&mut self, labels: Arc<pull_request_links::Labels>) {
        for entry in self
            .entries
            .values()
            .chain(self.local.as_ref().map(|(_, entry)| entry))
        {
            entry
                .shared
                .latest
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .labels = labels.clone();
        }
        self.labels = labels;
    }
    /// Drops `id`'s preparation, running or done, so that staging it again
    /// parses its whole text afresh.
    pub(super) fn forget(&mut self, id: &str) {
        if let Some(entry) = self.entries.get(id) {
            entry.shared.cancel();
        }
        self.remove(id);
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
    /// Rank a staged item's wait for a parser: lower ranks are parsed first.
    /// One already waiting moves to its new place; one already parsing keeps
    /// its parser.
    pub(super) fn rank(&self, id: &str, rank: u64) {
        if let Some(entry) = self.entries.get(id) {
            entry
                .shared
                .workers
                .rank(&entry.shared.priority, &entry.shared.place, rank);
        }
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
    #[cfg(test)]
    pub(super) fn queued_bytes(&self) -> usize {
        self.entries
            .values()
            .map(|entry| entry.shared.lease.bytes())
            .chain(
                self.local
                    .iter()
                    .map(|(_, entry)| entry.shared.lease.bytes()),
            )
            .sum()
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
                        labels: self.labels.clone(),
                    };
                    if !entry.running {
                        entry.run = next_attempt();
                        entry.running = true;
                        entry.claim.set(Claim::Unclaimed);
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
                claim: std::cell::Cell::new(Claim::Unclaimed),
                shared: Arc::new(Shared::new(SharedData {
                    latest: Mutex::new(Request {
                        input: Input::Transcript(item.clone()),
                        provider: self.provider.unwrap_or(Provider::Claude),
                        growing,
                        version: 1,
                        theme,
                        reader_proofs: self.reader_proofs.clone(),
                        labels: self.labels.clone(),
                    }),
                    priority: AtomicU64::new(UNRANKED),
                    place: Place::default(),
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

    /// Parse the growing answer's newest publication now and wait up to
    /// `budget` for it, so the update that staged it can also install it.
    ///
    /// A streamed chunk otherwise costs two updates and two frames: the
    /// wake that stages the publication draws the old text while its parse
    /// runs, and the prepared message draws again. The parse usually takes
    /// well under a millisecond, far less than the frame between them.
    /// A parse that outlasts the budget keeps going and arrives as an
    /// ordinary `MarkdownPrepared`.
    pub(super) fn settle_tail(&mut self, target: Target, budget: Duration) -> Settled {
        let Some((id, entry)) = self.entries.iter_mut().find(|(_, entry)| {
            // A run its subscription has seen may already be parsing.
            entry.running
                && entry.claim.get() == Claim::Unclaimed
                && entry.shared.priority.load(Ordering::Relaxed) == TAIL
        }) else {
            return Settled::Idle;
        };
        if entry.shared.cancelled.load(Ordering::Relaxed) {
            return Settled::Idle;
        }
        // Outside the application's executor (as in most tests) the
        // subscription parses it.
        let Ok(executor) = tokio::runtime::Handle::try_current() else {
            return Settled::Idle;
        };
        let Some(permit) = entry.shared.workers.try_acquire_tail() else {
            return Settled::Idle;
        };
        entry.claim.set(Claim::HandedOver);
        let handoff = Arc::new(Handoff::default());
        executor.spawn_blocking({
            let handoff = handoff.clone();
            let shared = entry.shared.clone();
            move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    execute(shared, permit)
                }))
                .unwrap_or_else(|_| Err("A message could not be loaded. Retry loading it.".into()));
                handoff.fill(result);
            }
        });
        let (id, run) = (id.clone(), entry.run);
        match handoff.wait(budget) {
            Some(result) => Settled::Ready(id, run, Ready::new(result)),
            None => Settled::Pending(cosmic::task::future(async move {
                let result = handoff.arrival().await;
                action(Action::MarkdownPrepared(
                    target,
                    id,
                    run,
                    Ready::new(result),
                ))
            })),
        }
    }

    /// The handed-over parse of `run` was not accepted, so the run parses
    /// again once a subscription or another wake picks it up.
    pub(super) fn unclaim(&self, id: &str, run: u64) {
        if let Some(entry) = self.entries.get(id)
            && entry.run == run
            && entry.claim.get() == Claim::HandedOver
        {
            entry.claim.set(Claim::Unclaimed);
        }
    }

    pub(super) fn subscriptions(&self, target: Target) -> Vec<Subscription<Message>> {
        let mut subscriptions: Vec<_> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.running && entry.claim.get() != Claim::HandedOver)
            .map(|(id, entry)| {
                entry.claim.set(Claim::Subscribed);
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
        entry.claim.set(Claim::Unclaimed);
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
    /// The pull requests the prepared text links to or mentions by number.
    links: Box<[pull_request_links::Named]>,
    document: prose_document::Data,
    reasoning: Option<super::reasoning::Preview>,
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
    _permit: Permit,
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
    /// The labels the text was parsed with.
    pub(super) fn labels(&self) -> &Arc<pull_request_links::Labels> {
        &self.request.labels
    }
    pub(super) fn item(&self) -> &Arc<chat::Item> {
        self.request.input.transcript().expect("transcript output")
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
            // The retained item now names the new content, so a title read
            // from the old one must not pass for it.
            previous.title = None;
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
        cached.reasoning = self.reasoning;
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
        cached.pull_request_links = self.links;
        cached.preparing = self.request.growing || managed;
        cached.placeholder = false;
        cached.open = None;
        cached.revision = CachedItem::next_revision();
        retire(retired);
    }
}

/// A single background reclaimer drops only Send data. Installed output
/// retirement retains one of its application's parser/output permits; detached
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
    _permit: Permit,
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
/// What `State::settle_tail` did with the growing answer.
pub(super) enum Settled {
    /// Nothing was handed over; any staged input parses through its
    /// subscription.
    Idle,
    /// Parsed within the budget, to accept now.
    Ready(String, u64, Ready),
    /// Still parsing; the task delivers it.
    Pending(Task<Message>),
}

/// A parse handed over outside a subscription: the update that started it
/// waits a little for the result, and otherwise a task does.
#[derive(Default)]
struct Handoff {
    result: Mutex<Option<Result<Prepared, String>>>,
    filled: std::sync::Condvar,
    arrived: tokio::sync::Notify,
}
impl Handoff {
    fn fill(&self, result: Result<Prepared, String>) {
        *self.result.lock().unwrap_or_else(|e| e.into_inner()) = Some(result);
        self.filled.notify_all();
        // Stores a permit when nobody waits yet, so a task that starts
        // waiting after the parse finished still wakes.
        self.arrived.notify_one();
    }
    fn wait(&self, budget: Duration) -> Option<Result<Prepared, String>> {
        let result = self.result.lock().unwrap_or_else(|e| e.into_inner());
        let (mut result, _) = self
            .filled
            .wait_timeout_while(result, budget, |result| result.is_none())
            .unwrap_or_else(|e| e.into_inner());
        result.take()
    }
    async fn arrival(&self) -> Result<Prepared, String> {
        loop {
            if let Some(result) = self.result.lock().unwrap_or_else(|e| e.into_inner()).take() {
                return result;
            }
            self.arrived.notified().await;
        }
    }
}
impl Drop for Handoff {
    fn drop(&mut self) {
        if let Some(Ok(prepared)) = self
            .result
            .get_mut()
            .unwrap_or_else(|error| error.into_inner())
            .take()
        {
            prepared.discard();
        }
    }
}
#[derive(Clone)]
pub struct Ready(Arc<ReplySlot>);
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
        std::any::TypeId::of::<Self>().hash(state);
        self.target.hash(state);
        self.id.hash(state);
        self.run.hash(state);
    }
}
/// The rank of a staged input no caller has ranked.
const UNRANKED: u64 = u64::MAX;

/// Parser admission for one application, granted in rank order.
///
/// Every queued message is its own subscription, and subscriptions start in
/// no particular order, so a plain semaphore parsed a returning transcript in
/// whatever order its entries hashed to: the rows on screen could wait behind
/// the whole history. Waiters here queue by the rank their transcript gives
/// them, ties in arrival order, and ranking a waiting input again moves it.
/// Only the waiter at the head contends for a permit, and only a change of
/// head wakes anyone, so a queue of n waiters costs O(n log n), not O(n²).
///
/// The growing tail ranks first, but never holds every parser while others
/// wait: with two or more parsers, one is always left for the rest, so a
/// streaming chat cannot stall a transcript's history however long it runs.
struct Pool {
    permits: Arc<tokio::sync::Semaphore>,
    capacity: usize,
    queue: Mutex<Queue>,
    #[cfg(test)]
    wakes: AtomicUsize,
    /// Whether ranks order the queue; measurements compare arrival order.
    #[cfg(test)]
    ranked: bool,
    /// Runs as a waiter arrives, before it takes its place in the queue.
    #[cfg(test)]
    arriving: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}
#[derive(Default)]
struct Queue {
    waiting: std::collections::BTreeMap<(u64, u64), Arc<Ticket>>,
    arrivals: u64,
    /// The waiter contending for the next permit.
    head: Option<(u64, u64)>,
    /// Parsers the growing tail holds.
    tail_held: usize,
}
/// One waiter: its place in the queue, which ranking can move, and how it
/// is told it reached or lost the head.
struct Ticket {
    key: Mutex<(u64, u64)>,
    turn: tokio::sync::Notify,
}
/// Where a staged input's waiter is, so ranking it again finds it.
#[derive(Default)]
pub(super) struct Place(Mutex<std::sync::Weak<Ticket>>);
/// A parser's permit. Held through UI consumption of what it prepared.
pub(super) struct Permit {
    tail: Option<Arc<Pool>>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}
/// A permit outside any application's queue, as fixtures grant.
impl From<tokio::sync::OwnedSemaphorePermit> for Permit {
    fn from(permit: tokio::sync::OwnedSemaphorePermit) -> Self {
        Self {
            tail: None,
            _permit: permit,
        }
    }
}
impl Drop for Permit {
    fn drop(&mut self) {
        if let Some(pool) = self.tail.take() {
            let mut queue = pool.lock();
            queue.tail_held -= 1;
            queue.advance(pool.capacity);
        }
    }
}
const TAIL: u64 = 0;
impl Queue {
    /// Who should contend next: the best-ranked waiter, unless it is the
    /// tail and the tail already holds all but one parser while other work
    /// waits.
    fn eligible(&self, capacity: usize) -> Option<(u64, u64)> {
        let first = *self.waiting.keys().next()?;
        if first.0 == TAIL && capacity > 1 && self.tail_held + 1 >= capacity {
            if let Some(other) = self.waiting.keys().find(|key| key.0 != TAIL) {
                return Some(*other);
            }
        }
        Some(first)
    }
    /// Wake the new head, and the old one so it stops contending, when the
    /// head changed.
    fn advance(&mut self, capacity: usize) {
        let next = self.eligible(capacity);
        if next == self.head {
            return;
        }
        for key in [self.head, next].into_iter().flatten() {
            if let Some(ticket) = self.waiting.get(&key) {
                ticket.turn.notify_one();
            }
        }
        self.head = next;
    }
}
impl Pool {
    fn new(permits: usize) -> Self {
        Self {
            permits: Arc::new(tokio::sync::Semaphore::new(permits)),
            capacity: permits,
            queue: Mutex::default(),
            #[cfg(test)]
            wakes: AtomicUsize::new(0),
            #[cfg(test)]
            ranked: true,
            #[cfg(test)]
            arriving: Mutex::default(),
        }
    }
    /// Two parsers. Ranking, not parallelism, is what shows the reader's rows
    /// sooner: more parsers only finish off-screen history sooner, while each
    /// holds its message's working set and installs more outputs before one
    /// frame (`frame_cost::a_cold_open_parses_what_is_read_first`).
    const PARSERS: usize = 2;

    fn sized() -> Self {
        #[cfg(test)]
        if let Some((capacity, ranked)) = POLICY.get() {
            return Self {
                ranked,
                ..Self::new(capacity)
            };
        }
        Self::new(Self::PARSERS)
    }
    fn lock(&self) -> std::sync::MutexGuard<'_, Queue> {
        self.queue.lock().unwrap_or_else(|e| e.into_inner())
    }
    #[cfg(test)]
    fn first(&self) -> Option<(u64, u64)> {
        self.lock().waiting.keys().next().copied()
    }
    /// Rank the input whose wait for a parser `priority` and `place` record.
    ///
    /// A waiter reads its rank under the queue's lock as it joins, and a
    /// waiter in the queue moves under that lock, so a rank given while the
    /// input joins is either read as it joins or moves it once it is there.
    fn rank(&self, priority: &AtomicU64, place: &Place, rank: u64) {
        if priority.swap(rank, Ordering::Relaxed) != rank {
            self.rerank(place, rank);
        }
    }
    /// Move the waiter at `place`, if it is still waiting, to `rank`.
    fn rerank(&self, place: &Place, rank: u64) {
        #[cfg(test)]
        if !self.ranked {
            return;
        }
        // The queue's lock first: a waiter joining now has either taken its
        // place by then or will read the rank just stored.
        let mut queue = self.lock();
        let Some(ticket) = place.0.lock().unwrap_or_else(|e| e.into_inner()).upgrade() else {
            return;
        };
        let mut key = ticket.key.lock().unwrap_or_else(|e| e.into_inner());
        if key.0 == rank {
            return;
        }
        let Some(ticket) = queue.waiting.remove(&key) else {
            return;
        };
        let was_head = queue.head == Some(*key);
        key.0 = rank;
        let moved = *key;
        drop(key);
        queue.waiting.insert(moved, ticket.clone());
        if was_head {
            if queue.eligible(self.capacity) == Some(moved) {
                // Still first, as when an append shifts every rank alike:
                // it keeps contending, and nobody is woken.
                queue.head = Some(moved);
                return;
            }
            // Someone else is eligible now: it stops contending, and
            // `advance` tells the new head.
            queue.head = None;
            ticket.turn.notify_one();
        }
        queue.advance(self.capacity);
    }
    /// Wait for a parser at the rank `priority` holds when this joins the
    /// queue, or any rank it is given later through `place`.
    async fn acquire(
        self: &Arc<Self>,
        priority: &AtomicU64,
        place: Option<&Place>,
    ) -> Result<Permit, tokio::sync::AcquireError> {
        #[cfg(test)]
        if let Some(arriving) = self
            .arriving
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            arriving();
        }
        struct Waiting<'a>(&'a Pool, &'a Ticket, bool);
        impl Drop for Waiting<'_> {
            fn drop(&mut self) {
                if self.2 {
                    return;
                }
                let mut queue = self.0.lock();
                let key = *self.1.key.lock().unwrap_or_else(|e| e.into_inner());
                queue.waiting.remove(&key);
                queue.advance(self.0.capacity);
            }
        }
        let ticket = {
            let mut queue = self.lock();
            let rank = priority.load(Ordering::Relaxed);
            #[cfg(test)]
            let rank = if self.ranked { rank } else { UNRANKED };
            let key = (rank, queue.arrivals);
            queue.arrivals += 1;
            let ticket = Arc::new(Ticket {
                key: Mutex::new(key),
                turn: tokio::sync::Notify::new(),
            });
            queue.waiting.insert(key, ticket.clone());
            if let Some(place) = place {
                *place.0.lock().unwrap_or_else(|e| e.into_inner()) = Arc::downgrade(&ticket);
            }
            ticket
        };
        let mut waiting = Waiting(self, &ticket, false);
        // Subscriptions started together all queue before any contends, so
        // the first parsers go by rank too rather than by start order.
        tokio::task::yield_now().await;
        self.lock().advance(self.capacity);
        loop {
            let turn = ticket.turn.notified();
            let turn = std::pin::pin!(turn);
            let mine = {
                let queue = self.lock();
                queue.head == Some(*ticket.key.lock().unwrap_or_else(|e| e.into_inner()))
            };
            if !mine {
                turn.await;
                #[cfg(test)]
                self.wakes.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            let acquire = std::pin::pin!(self.permits.clone().acquire_owned());
            if let iced::futures::future::Either::Left((permit, _)) =
                iced::futures::future::select(acquire, turn).await
            {
                let permit = permit?;
                let mut queue = self.lock();
                let key = *ticket.key.lock().unwrap_or_else(|e| e.into_inner());
                if queue.head != Some(key) {
                    // It lost the head while the semaphore had already
                    // queued it for this permit; the permit goes back for
                    // the waiter that is ahead now.
                    drop(queue);
                    drop(permit);
                    continue;
                }
                queue.waiting.remove(&key);
                let tail = key.0 == TAIL;
                if tail {
                    queue.tail_held += 1;
                }
                queue.advance(self.capacity);
                waiting.2 = true;
                return Ok(Permit {
                    tail: tail.then(|| self.clone()),
                    _permit: permit,
                });
            }
            // Told while contending: it may have lost the head.
            #[cfg(test)]
            self.wakes.fetch_add(1, Ordering::Relaxed);
        }
    }
}
impl Pool {
    /// A parser for the growing tail at once, if one is free and no other
    /// input waits for it. Nothing is overtaken: with a waiter queued this
    /// declines, and the tail waits its turn through the queue as usual.
    fn try_acquire_tail(self: &Arc<Self>) -> Option<Permit> {
        let mut queue = self.lock();
        if !queue.waiting.is_empty() {
            return None;
        }
        let permit = self.permits.clone().try_acquire_owned().ok()?;
        queue.tail_held += 1;
        Some(Permit {
            tail: Some(self.clone()),
            _permit: permit,
        })
    }
}
#[cfg(test)]
thread_local! {
    /// The parser count and whether ranks apply, for pools this thread
    /// creates from now on; measurements compare policies on fresh threads.
    pub(super) static POLICY: std::cell::Cell<Option<(usize, bool)>> =
        const { std::cell::Cell::new(None) };
}
fn workers() -> Arc<Pool> {
    application_shared!(Pool, Pool::sized())
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
    let permit = shared
        .workers
        .acquire(&shared.priority, Some(&shared.place))
        .await
        .map_err(|_| "Message loading stopped.".to_owned())?;
    if shared.cancelled.load(Ordering::Relaxed) {
        return Err("Message loading was cancelled.".into());
    }
    tokio::task::spawn_blocking(move || execute(shared, permit))
        .await
        .map_err(|_| "A message could not be loaded. Retry loading it.".to_owned())?
}

fn execute(shared: Arc<Shared>, permit: impl Into<Permit>) -> Result<Prepared, String> {
    let permit = permit.into();
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
            reasoning: None,
            prompt: Some(prompt),
            literal: None,
            links: Box::default(),
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
            &request.labels,
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
        .map(|reading| coordination::Prepared::prepare(reading, &request.labels, &shared.cancelled))
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
    // What the message links to, so a change of labels finds the messages it
    // reads otherwise in without reading their text again.
    let mut links = CachedItem::prose(&item.content)
        .filter(|_| prompt.is_none() && literal.is_none())
        .map(pull_request_links::named)
        .unwrap_or_default();
    if let Some(coordinated) = &coordinated {
        links.extend(coordinated.links());
    }
    pull_request_links::deduplicate(&mut links);
    let reasoning = super::reasoning::Preview::of(&item.content);
    Ok(Prepared {
        request,
        parsed,
        document,
        reasoning,
        prompt,
        literal,
        links: links.into_boxed_slice(),
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
    for _ in 0..4096 {
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

/// Drive the production parser admission as the application's subscriptions
/// do: every running entry starts at once, in the order the subscriptions
/// list them. Results are installed on this thread in UI updates: each takes
/// every result that arrived since the last, then `frame` draws, as the
/// event loop handles all queued messages before a redraw.
#[cfg(test)]
pub(super) fn drain_concurrently<T>(
    owner: &mut T,
    runtime: impl Fn(&mut T) -> &mut Runtime,
    mut frame: impl FnMut(&mut T),
) -> Drained {
    let executor = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let start = std::time::Instant::now();
    let (sent, mut arrivals) = tokio::sync::mpsc::unbounded_channel();
    let mut started = std::collections::HashSet::new();
    let mut drained = Drained::default();
    let mut pending = 0usize;
    loop {
        for (id, entry) in runtime(owner)
            .preparation
            .entries
            .iter()
            .filter(|(_, entry)| entry.running)
        {
            if started.insert((id.clone(), entry.run)) {
                let (id, generation, shared) = (id.clone(), entry.run, entry.shared.clone());
                let sent = sent.clone();
                pending += 1;
                executor.spawn(async move {
                    let _ = sent.send((id, generation, run(shared).await));
                });
            }
        }
        if pending == 0 {
            return drained;
        }
        // One UI update: everything that finished since the last one, as the
        // event loop handles every queued message before it draws.
        let first = executor.block_on(arrivals.recv()).expect("a parser result");
        let mut batch = vec![first];
        while let Ok(next) = arrivals.try_recv() {
            batch.push(next);
        }
        pending -= batch.len();
        let clock = std::time::Instant::now();
        let count = batch.len();
        for (id, generation, result) in batch {
            let runtime = runtime(owner);
            runtime.markdown_prepared(runtime.target, &id, generation, Ready::new(result));
            drained.installed.push((id, start.elapsed()));
        }
        let installing = clock.elapsed();
        let clock = std::time::Instant::now();
        frame(owner);
        let framing = clock.elapsed();
        drained.updates.push(Update {
            outputs: count,
            installing,
            framing,
            drawn: start.elapsed(),
        });
    }
}

/// What [`drain_concurrently`] saw: when each output was installed, and each
/// UI update that installed some.
#[cfg(test)]
#[derive(Default)]
pub(super) struct Drained {
    pub(super) installed: Vec<(String, std::time::Duration)>,
    pub(super) updates: Vec<Update>,
}
#[cfg(test)]
pub(super) struct Update {
    pub(super) outputs: usize,
    pub(super) installing: std::time::Duration,
    pub(super) framing: std::time::Duration,
    /// When the frame that shows this update's outputs was done.
    pub(super) drawn: std::time::Duration,
}

/// Parse one coalesced publication once. The parser can reparse an unfinished
/// rich block on append; feeding a cold body in artificial chunks amplifies
/// that cost. Cancellation fences ownership before and after the parser call;
/// it does not preempt pulldown-cmark inside a single rich block.
///
/// Text parsed before keeps the labels it was parsed with. When `labels`
/// read a pull request that `body` names differently, the whole body is
/// parsed again; otherwise only what is appended reads them.
fn parse_publication(
    parsed: &mut Parsed,
    body: &str,
    growing: bool,
    labels: &Arc<pull_request_links::Labels>,
    cancelled: &AtomicBool,
) -> Result<(), String> {
    if cancelled.load(Ordering::Relaxed) {
        return Err("Message loading was cancelled.".into());
    }
    let previous = parsed.markdown.labels();
    // A fresh parser has parsed nothing to relabel.
    if parsed.markdown.items().is_empty() {
        parsed.markdown.set_labels(labels.clone());
    } else if !Arc::ptr_eq(previous, labels) {
        if pull_request_links::named(body)
            .iter()
            .any(|key| !previous.reads_alike(labels, key))
        {
            *parsed = Parsed::default();
        }
        parsed.markdown.set_labels(labels.clone());
    }
    match parsed.source.advance_append(body, growing) {
        markdown_source::Step::Append(text) => parsed.markdown.push_str(&text),
        markdown_source::Step::Restart(text) => {
            parsed.markdown = markdown::Content::parse_labelled(&text, labels.clone());
        }
    }
    if cancelled.load(Ordering::Relaxed) {
        return Err("Message loading was cancelled.".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
