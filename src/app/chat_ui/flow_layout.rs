//! Full-context shaping and bounded display projection prototype.
//!
//! Active CachedItem keeps weak geometry descriptors; evicted row owners release
//! full snapshots while measured heights remain in the virtual column. Cold full
//! shaping runs on a bounded worker; it remains a measured cost, not a claim of
//! viewport-only shaping or a fulfilled huge-paragraph performance budget.
use super::*;
use iced::advanced::graphics::text::{self as graphics_text, cosmic_text};
#[cfg(test)]
use iced::advanced::text::Paragraph;
use iced::widget::text::Span;

#[cfg(test)]
type ParagraphBuffer = <cosmic::Renderer as iced::advanced::text::Renderer>::Paragraph;

use std::{cell::RefCell, rc::Rc};
use unicode_segmentation::UnicodeSegmentation;

#[derive(Clone, Default)]
pub(super) struct Cache(Rc<RefCell<Entries>>);
#[derive(Default)]
struct Entries {
    logical: HashMap<u64, std::rc::Weak<RefCell<Entry>>>,
    requests: HashMap<u64, std::rc::Weak<RefCell<Entry>>>,
}
pub(super) type Handle = Rc<RefCell<Entry>>;

thread_local! {
    // Weak UI-thread handles let temporary approval/readers receive results
    // without retaining parked paragraphs or requiring a CachedItem owner.
    static REQUESTS: RefCell<HashMap<u64,std::rc::Weak<RefCell<Entry>>>> = RefCell::default();
}
pub(super) fn find(target: Target, request: &Request) -> Option<Handle> {
    REQUESTS
        .with(|requests| {
            requests
                .borrow()
                .get(&request.id)
                .and_then(std::rc::Weak::upgrade)
        })
        .filter(|entry| entry.borrow().target == Some(target) && entry.borrow().accepts(request))
}
pub(super) struct Entry {
    id: u64,
    target: Option<Target>,
    source: Source,
    source_revision: u64,
    source_len: usize,
    ready_revision: Option<u64>,
    key: Option<Key>,
    attempt: u64,
    requested: bool,
    deferred: Option<u64>,
    failed: bool,
    ready: Option<Arc<Prepared>>,
    cancelled: Arc<AtomicBool>,
    find_highlight: Option<find_navigation::Highlight>,
    find_requested: Option<(Arc<AtomicU64>, u64, std::sync::Weak<str>, Arc<AtomicBool>)>,
    find_touched: u64,
    find_visible: bool,
    find_deferred: Option<u64>,
}
impl Drop for Entry {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Release);
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
struct Key {
    width: f32,
    size: iced::Pixels,
    font: iced::Font,
    version: graphics_text::Version,
    unwrapped: bool,
}

#[derive(Clone)]
struct Source(Arc<SourceOwner>);
struct SourceOwner(Option<RawSource>);
impl Drop for SourceOwner {
    fn drop(&mut self) {
        if let Some(source) = self.0.take() {
            preparation::retire_flow_source(source);
        }
    }
}
impl std::ops::Deref for Source {
    type Target = SourceData;
    fn deref(&self) -> &SourceData {
        self.0
            .0
            .as_ref()
            .expect("live Flow owner")
            .data
            .as_ref()
            .expect("live Flow source")
    }
}
impl Source {
    fn new(data: SourceData) -> Self {
        Self(Arc::new(SourceOwner(Some(RawSource {
            data: Some(data),
            #[cfg(test)]
            disposed: None,
        }))))
    }
    #[cfg(test)]
    fn watch_disposal(
        &mut self,
        disposed: std::sync::mpsc::Sender<(std::thread::ThreadId, usize)>,
    ) {
        Arc::get_mut(&mut self.0)
            .expect("observe before cloning source")
            .0
            .as_mut()
            .unwrap()
            .disposed = Some(disposed);
    }
}
/// Raw Send payload, destroyed inline by the reclaimer. In particular, it
/// never queues a second retirement behind an already waiting FIFO barrier.
pub(super) struct RawSource {
    data: Option<SourceData>,
    #[cfg(test)]
    disposed: Option<std::sync::mpsc::Sender<(std::thread::ThreadId, usize)>>,
}
impl Drop for RawSource {
    fn drop(&mut self) {
        let source = self.data.take().expect("owned Flow source");
        #[cfg(test)]
        let bytes = source.len();
        drop(source);
        #[cfg(test)]
        if let Some(disposed) = self.disposed.take() {
            let _ = disposed.send((std::thread::current().id(), bytes));
        }
    }
}
enum SourceData {
    Markdown {
        text: markdown::FlowText,
        style: markdown::Style,
    },
    Code {
        text: Arc<str>,
        range: std::ops::Range<usize>,
        spans: Arc<[Span<'static, markdown::Uri>]>,
        starts: Arc<[usize]>,
        source: reader_source::Source,
    },
    Styled(Arc<[Span<'static, markdown::Uri>]>),
}
// Requests and Action clones share immutable backing with one final-owner
// retirement. No unsafe Sync assertion and no UI/materialization mutex.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Source>();
    send_sync::<Request>();
};
impl SourceData {
    fn len(&self) -> usize {
        match self {
            Self::Markdown { text, .. } => text.len(),
            Self::Styled(spans) => spans.iter().map(|span| span.text.len()).sum(),
            Self::Code { range, .. } => range.len(),
        }
    }
    fn extends(&self, previous: &Self) -> bool {
        match (self, previous) {
            (
                Self::Markdown { text, style },
                Self::Markdown {
                    text: old,
                    style: old_style,
                },
            ) => style == old_style && text.append_proven(old),
            (Self::Styled(spans), Self::Styled(previous)) => Arc::ptr_eq(spans, previous),
            (
                Self::Code { range, source, .. },
                Self::Code {
                    range: old,
                    source: previous,
                    ..
                },
            ) => {
                range.start == old.start
                    && range.end >= old.end
                    && source.accepts(previous.digest())
            }
            _ => false,
        }
    }
    fn unwrapped(&self) -> bool {
        matches!(self, Self::Code { .. })
    }
    fn presentation_changed(&self, previous: &Self) -> bool {
        match (self, previous) {
            (
                Self::Code { range, spans, .. },
                Self::Code {
                    range: old,
                    spans: old_spans,
                    ..
                },
            ) => {
                range.end != old.end
                    || (!Arc::ptr_eq(spans, old_spans)
                        && (spans.len() != old_spans.len()
                            || !spans.iter().zip(old_spans.iter()).all(|(a, b)| {
                                a == b
                                    && match (&a.link, &b.link) {
                                        (None, None) => true,
                                        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                                        _ => false,
                                    }
                                    && a.highlight == b.highlight
                                    && a.padding == b.padding
                                    && a.underline == b.underline
                                    && a.strikethrough == b.strikethrough
                            })))
            }
            _ => self.len() != previous.len(),
        }
    }
    fn materialize(&self) -> (Arc<[Span<'static, markdown::Uri>]>, Arc<str>) {
        match self {
            Self::Markdown { text, style } => (text.spans(*style), text.plain()),
            Self::Styled(spans) => (
                spans.clone(),
                spans
                    .iter()
                    .map(|span| span.text.as_ref())
                    .collect::<String>()
                    .into(),
            ),
            Self::Code {
                text,
                range,
                spans,
                starts,
                ..
            } => {
                let plain: Arc<str> = Arc::from(&text[range.clone()]);
                let spans = if spans.is_empty() {
                    vec![Span::new(plain.to_string())].into()
                } else {
                    prose::sliced_spans(spans, starts, range.clone())
                };
                (spans, plain)
            }
        }
    }
}

impl Cache {
    #[cfg(test)]
    pub(super) fn entry(
        &self,
        identity: u64,
        spans: Arc<[Span<'static, markdown::Uri>]>,
    ) -> Handle {
        self.source_entry(identity, SourceData::Styled(spans))
    }
    pub(super) fn markdown_entry(
        &self,
        identity: u64,
        text: markdown::Text,
        style: markdown::Style,
    ) -> Handle {
        self.source_entry(
            identity,
            SourceData::Markdown {
                text: text.flow_source(),
                style,
            },
        )
    }
    pub(super) fn code_entry(
        &self,
        identity: u64,
        text: Arc<str>,
        range: std::ops::Range<usize>,
        spans: Arc<[Span<'static, markdown::Uri>]>,
        starts: Arc<[usize]>,
        source: reader_source::Source,
    ) -> Handle {
        self.source_entry(
            identity,
            SourceData::Code {
                text,
                range,
                spans,
                starts,
                source,
            },
        )
    }
    fn source_entry(&self, identity: u64, source: SourceData) -> Handle {
        let mut entries = self.0.borrow_mut();
        if let Some(entry) = entries
            .logical
            .get(&identity)
            .and_then(std::rc::Weak::upgrade)
        {
            let reusable = source.extends(&entry.borrow().source);
            if reusable {
                let mut current = entry.borrow_mut();
                let length = source.len();
                if source.presentation_changed(&current.source) {
                    current.source_revision = current.source_revision.wrapping_add(1);
                    current.source_len = length;
                    current.failed = false;
                }
                current.source = Source::new(source);
                drop(current);
                return entry;
            }
        }
        static NEXT: AtomicU64 = AtomicU64::new(1);
        let source_len = source.len();
        let entry = Rc::new(RefCell::new(Entry {
            id: NEXT.fetch_add(1, Ordering::Relaxed),
            target: None,
            source: Source::new(source),
            source_len,
            source_revision: 0,
            ready_revision: None,
            key: None,
            attempt: 0,
            requested: false,
            deferred: None,
            failed: false,
            ready: None,
            cancelled: Arc::new(AtomicBool::new(false)),
            find_highlight: None,
            find_requested: None,
            find_touched: 0,
            find_visible: false,
            find_deferred: None,
        }));
        // Prune in batches rather than walking visited history on each
        // request or keeping an unbounded set of dead presentation headers.
        if entries.logical.len() >= 256 || entries.requests.len() >= 256 {
            entries.logical.retain(|_, entry| entry.strong_count() != 0);
            entries
                .requests
                .retain(|_, entry| entry.strong_count() != 0);
        }
        if let Some(previous) = entries
            .logical
            .insert(identity, Rc::downgrade(&entry))
            .and_then(|old| old.upgrade())
        {
            let previous = previous.borrow();
            previous.cancelled.store(true, Ordering::Release);
            entries.requests.remove(&previous.id);
        }
        entries
            .requests
            .insert(entry.borrow().id, Rc::downgrade(&entry));
        REQUESTS.with(|requests| {
            let mut requests = requests.borrow_mut();
            if requests.len() >= 256 {
                requests.retain(|_, entry| entry.strong_count() != 0);
            }
            requests.insert(entry.borrow().id, Rc::downgrade(&entry));
        });
        entry
    }
    pub(super) fn find(&self, id: u64) -> Option<Handle> {
        self.0
            .borrow()
            .requests
            .get(&id)
            .and_then(std::rc::Weak::upgrade)
    }
    #[cfg(test)]
    pub(super) fn complete_fixture(&self) -> bool {
        let entries: Vec<_> = self
            .0
            .borrow()
            .logical
            .values()
            .filter_map(std::rc::Weak::upgrade)
            .collect();
        let mut changed = false;
        for entry in entries {
            let Some(request) = entry.borrow_mut().request() else {
                continue;
            };
            let (spans, plain) = request.source.materialize();
            let mut prepared = Prepared::new(
                &spans,
                request.key.size,
                request.key.font,
                request.key.width,
                if request.key.unwrapped {
                    iced::widget::text::Wrapping::None
                } else {
                    iced::widget::text::Wrapping::WordOrGlyph
                },
            );
            prepared.plain = plain;
            changed |= entry
                .borrow_mut()
                .complete(&request, Some(Arc::new(prepared)));
        }
        changed
    }
    pub(super) fn retained_bytes(&self) -> usize {
        self.0
            .borrow()
            .logical
            .values()
            .filter_map(std::rc::Weak::upgrade)
            .filter_map(|entry| entry.borrow().ready.clone())
            .map(|prepared| prepared.snapshot.retained_bytes())
            .sum()
    }
}

/// Body-free identity of mounted geometry work. Verified appends retain it;
/// source replacement creates another Entry and needs its own realization.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PendingGeometry(u64);

impl Entry {
    pub(super) fn bind_target(&mut self, target: Target) {
        if self.target.is_some_and(|previous| previous != target) {
            self.attempt = self.attempt.wrapping_add(1);
            self.ready = None;
            self.ready_revision = None;
            self.requested = false;
            self.failed = false;
            self.deferred = None;
            self.cancelled.store(true, Ordering::Release);
            self.cancelled = Arc::new(AtomicBool::new(false));
        }
        self.target = Some(target);
    }
    /// A temporary view may recreate its cache while its widget tree still
    /// owns the prior paragraph. Adopt only compatible source state.
    pub(super) fn adopt(&mut self, current: &Entry) -> bool {
        if self.cancelled.load(Ordering::Acquire) || !current.source.extends(&self.source) {
            return false;
        }
        if current.source.presentation_changed(&self.source) {
            self.source_revision = self.source_revision.wrapping_add(1);
            self.source_len = current.source_len;
            self.failed = false;
        }
        self.source = current.source.clone();
        true
    }
    pub(super) fn configure(&mut self, width: f32, size: iced::Pixels, font: iced::Font) {
        let width = if self.source.unwrapped() && !width.is_finite() {
            design::READABLE_MEASURE
        } else {
            width
        };
        let key = Key {
            width,
            size,
            font,
            unwrapped: self.source.unwrapped(),
            version: graphics_text::font_system()
                .read()
                .expect("font system")
                .version(),
        };
        if self.key != Some(key) {
            self.key = Some(key);
            self.attempt = self.attempt.wrapping_add(1);
            self.requested = false;
            self.deferred = None;
            self.failed = false;
            self.ready = None;
            self.ready_revision = None;
            self.cancelled.store(true, Ordering::Release);
            self.cancelled = Arc::new(AtomicBool::new(false));
        }
    }
    pub(super) fn failed(&self) -> bool {
        self.failed
    }
    pub(super) fn geometry_pending(&self, byte: usize) -> Option<PendingGeometry> {
        (byte <= self.source_len
            && self.ready_revision != Some(self.source_revision)
            && !self.failed)
            .then_some(PendingGeometry(self.id))
    }
    pub(super) fn prepared(&self) -> Option<Arc<Prepared>> {
        self.ready.clone()
    }
    pub(super) fn bounds(&self, _bytes: usize) -> iced::Size {
        self.ready.as_ref().map_or_else(
            || {
                let key = self.key.expect("configured layout");
                let lines = if key.unwrapped {
                    1.0
                } else {
                    (self.source_len as f32 * key.size.0 * 0.5 / key.width.max(1.0))
                        .ceil()
                        .max(1.0)
                };
                iced::Size::new(key.width, lines * key.size.0 * 1.3)
            },
            |ready| ready.bounds,
        )
    }
    pub(super) fn request(&mut self) -> Option<Request> {
        let key = self.key?;
        if self.ready_revision == Some(self.source_revision)
            || self.requested
            || self.failed
            || self.deferred == Some(COMPLETED.load(Ordering::Acquire))
            || !key.width.is_finite()
            || key.width <= 0.0
        {
            return None;
        }
        self.requested = true;
        self.deferred = None;
        Some(Request {
            id: self.id,
            target: self.target,
            attempt: self.attempt,
            key,
            source: self.source.clone(),
            source_revision: self.source_revision,
            cancelled: self.cancelled.clone(),
            admission: None,
            #[cfg(test)]
            outcome: None,
        })
    }
    pub(super) fn accepts(&self, request: &Request) -> bool {
        self.id == request.id
            && self.target == request.target
            && self.attempt == request.attempt
            && self.key == Some(request.key)
            && request.source_revision <= self.source_revision
            && !request.cancelled.load(Ordering::Acquire)
    }
    pub(super) fn defer(&mut self, request: &Request, epoch: u64) {
        if self.accepts(request) {
            self.requested = false;
            self.deferred = Some(epoch);
        }
    }
    pub(super) fn complete(&mut self, request: &Request, ready: Option<Arc<Prepared>>) -> bool {
        if !self.accepts(request) {
            return false;
        }
        let version = graphics_text::font_system()
            .read()
            .expect("font system")
            .version();
        if version != request.key.version {
            let old = request.key;
            self.key = Some(Key { version, ..old });
            self.attempt = self.attempt.wrapping_add(1);
            self.cancelled.store(true, Ordering::Release);
            self.cancelled = Arc::new(AtomicBool::new(false));
            self.requested = false;
            self.deferred = None;
            self.ready = None;
            self.ready_revision = None;
            return true;
        }
        self.requested = false;
        self.failed = ready.is_none();
        self.ready_revision = ready.as_ref().map(|_| request.source_revision);
        if let Some(ready) = &ready {
            ready
                .admission
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
        }
        self.ready = ready;
        true
    }
}

#[derive(Clone)]
pub(super) struct Request {
    pub(super) id: u64,
    target: Option<Target>,
    attempt: u64,
    key: Key,
    source: Source,
    source_revision: u64,
    cancelled: Arc<AtomicBool>,
    // Last: a final Request drops its own source before the output guard.
    // Every admitted outcome, including None and JoinError, carries this.
    admission: Option<Arc<OutputAdmission>>,
    #[cfg(test)]
    outcome: Option<WorkerOutcome>,
}
#[cfg(test)]
#[derive(Clone, Copy, PartialEq, Eq)]
enum WorkerOutcome {
    ReclamationError,
    Panic,
    CancelAfterShape,
}
impl std::fmt::Debug for Request {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlowRequest")
            .field("id", &self.id)
            .field("attempt", &self.attempt)
            .field("key", &self.key)
            .finish()
    }
}

static COMPLETED: AtomicU64 = AtomicU64::new(0);
fn available() -> &'static tokio::sync::watch::Sender<u64> {
    static AVAILABLE: std::sync::OnceLock<tokio::sync::watch::Sender<u64>> =
        std::sync::OnceLock::new();
    AVAILABLE.get_or_init(|| tokio::sync::watch::channel(0).0)
}
/// One coalesced event source, including a current-state event on subscription:
/// permit release after stale-output reclamation must wake an otherwise idle UI.
pub(super) fn availability() -> Subscription<Message> {
    Subscription::run(availability_stream)
}
pub(super) fn availability_stream() -> impl iced::futures::Stream<Item = Message> + Send {
    iced::futures::stream::unfold(
        (available().subscribe(), true),
        |(mut receiver, initial)| async move {
            if !initial {
                receiver.changed().await.ok()?;
            }
            receiver.borrow_and_update();
            Some((action(Action::FlowAvailable), (receiver, false)))
        },
    )
}
pub(super) fn deferred_requests(target: Target) -> Vec<Request> {
    REQUESTS.with(|requests| {
        requests
            .borrow()
            .values()
            .filter_map(|entry| {
                let entry = entry.upgrade()?;
                let mut entry = entry.borrow_mut();
                if entry.target != Some(target) || entry.deferred.is_none() {
                    return None;
                }
                entry.request()
            })
            .collect()
    })
}

#[derive(Debug)]
pub(super) struct WorkSlot(Option<tokio::sync::OwnedSemaphorePermit>);
impl Drop for WorkSlot {
    fn drop(&mut self) {
        self.0.take();
        let epoch = COMPLETED.fetch_add(1, Ordering::AcqRel).wrapping_add(1);
        available().send_replace(epoch);
    }
}

pub(super) struct OutputAdmission {
    owned: Option<OutputRetirement>,
}
impl std::fmt::Debug for OutputAdmission {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("FlowOutputAdmission")
            .finish_non_exhaustive()
    }
}
impl Drop for OutputAdmission {
    fn drop(&mut self) {
        if let Some(owned) = self.owned.take() {
            preparation::retire_flow_output(owned);
        }
    }
}
/// Field order matters: release the final raw source before admission and its
/// availability wake. Reclaimer-context source release destroys raw data inline.
pub(super) struct OutputRetirement {
    _source: Source,
    _slot: WorkSlot,
}
impl OutputAdmission {
    fn new(source: Source, slot: WorkSlot) -> Arc<Self> {
        Arc::new(Self {
            owned: Some(OutputRetirement {
                _source: source,
                _slot: slot,
            }),
        })
    }
}

pub(super) fn run(target: Target, request: Request) -> Result<Task<Message>, u64> {
    // One shaping job, no waiting worker queue. Deferred live entries become
    // eligible when that job completes; redraw alone cannot create a busy loop.
    static SLOTS: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    let slots = SLOTS
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone();
    run_with_slots(target, request, slots)
}
fn run_with_slots(
    target: Target,
    mut request: Request,
    slots: Arc<tokio::sync::Semaphore>,
) -> Result<Task<Message>, u64> {
    let epoch = COMPLETED.load(Ordering::Acquire);
    let permit = WorkSlot(Some(slots.try_acquire_owned().map_err(|_| epoch)?));
    request.admission = Some(OutputAdmission::new(request.source.clone(), permit));
    // Share this font-version input snapshot. Repeated paragraph requests
    // clone one Arc on the UI; each worker's private font DB clone happens below.
    struct Fonts {
        db: cosmic_text::fontdb::Database,
        locale: String,
    }
    static FONTS: std::sync::OnceLock<Mutex<Option<(graphics_text::Version, Arc<Fonts>)>>> =
        std::sync::OnceLock::new();
    let fonts = {
        let mut cached = FONTS
            .get_or_init(Default::default)
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if cached
            .as_ref()
            .is_none_or(|(version, _)| *version != request.key.version)
        {
            let mut system = graphics_text::font_system().write().expect("font system");
            *cached = Some((
                system.version(),
                Arc::new(Fonts {
                    db: system.db().clone(),
                    locale: system.raw().locale().to_owned(),
                }),
            ));
        }
        cached.as_ref().expect("font inputs").1.clone()
    };
    Ok(cosmic::task::future(async move {
        let work = request.clone();
        let ready = tokio::task::spawn_blocking(move || {
            // Admission remains attached to the output until UI consumption;
            // a stalled event loop cannot queue full-context glyph snapshots.
            let reclaimed = preparation::reclaim_before_work();
            #[cfg(test)]
            let reclaimed = if work.outcome == Some(WorkerOutcome::ReclamationError) {
                Err("fixture reclamation error at the real admission boundary".to_owned())
            } else {
                reclaimed
            };
            if reclaimed.is_err() || work.cancelled.load(Ordering::Acquire) {
                return None;
            }
            #[cfg(test)]
            assert!(
                work.outcome != Some(WorkerOutcome::Panic),
                "deliberate private Flow worker panic"
            );
            let mut fonts = cosmic_text::FontSystem::new_with_locale_and_db(
                fonts.locale.clone(),
                fonts.db.clone(),
            );
            let (spans, plain) = work.source.materialize();
            let prepared = Prepared::with_fonts(
                spans,
                plain,
                work.key.size,
                work.key.font,
                work.key.width,
                if work.key.unwrapped {
                    iced::widget::text::Wrapping::None
                } else {
                    iced::widget::text::Wrapping::WordOrGlyph
                },
                &mut fonts,
            );
            *prepared.admission.lock().unwrap_or_else(|e| e.into_inner()) = work.admission.clone();
            #[cfg(test)]
            if work.outcome == Some(WorkerOutcome::CancelAfterShape) {
                work.cancelled.store(true, Ordering::Release);
            }
            if work.cancelled.load(Ordering::Acquire) {
                // Geometry and the source-bearing None reply share admission.
                // Neither worker return nor stale geometry retirement releases
                // it before that reply is consumed/discarded.
                drop(prepared);
                None
            } else {
                Some(Arc::new(prepared))
            }
        })
        .await
        .ok()
        .flatten();
        action(Action::FlowReady(target, request, ready))
    }))
}

/// Immutable source/width/font-specific geometry. Retaining this is more
/// expensive than a scroll bookmark; release it with active transcript caches.
#[derive(Debug)]
pub(super) struct Prepared {
    owned: Option<Box<Geometry>>,
    admission: Mutex<Option<Arc<OutputAdmission>>>,
}

// A widget or result may own the final Arc. Its last release must remain
// cheap on the event thread, including resize, stale completion and hide.
impl std::ops::Deref for Prepared {
    type Target = Geometry;
    fn deref(&self) -> &Geometry {
        self.owned.as_deref().expect("live geometry")
    }
}
#[cfg(test)]
impl std::ops::DerefMut for Prepared {
    fn deref_mut(&mut self) -> &mut Geometry {
        self.owned.as_deref_mut().expect("live geometry")
    }
}
impl Drop for Prepared {
    fn drop(&mut self) {
        if let Some(geometry) = self.owned.take() {
            let admission = self
                .admission
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            preparation::retire_geometry(geometry, admission);
        }
    }
}

#[derive(Debug)]
pub(super) struct Geometry {
    #[cfg(test)]
    retired_on: Option<std::sync::mpsc::Sender<std::thread::ThreadId>>,
    pub(super) snapshot: cosmic_text::LayoutSnapshot,
    pub(super) bounds: iced::Size,
    pub(super) spans: Arc<[Span<'static, markdown::Uri>]>,
    pub(super) plain: Arc<str>,
    unwrapped: bool,
}
#[cfg(test)]
impl Drop for Geometry {
    fn drop(&mut self) {
        if let Some(sender) = self.retired_on.take() {
            let _ = sender.send(std::thread::current().id());
        }
    }
}

impl Prepared {
    /// Use the same rich-text shaping attributes as iced's Paragraph. Font
    /// changes, width, source/span changes and theme colors invalidate this.
    #[cfg(test)]
    fn new(
        spans: &[Span<'static, markdown::Uri>],
        size: iced::Pixels,
        font: iced::Font,
        width: f32,
        wrapping: iced::widget::text::Wrapping,
    ) -> Self {
        let (db, locale) = {
            let mut fonts = graphics_text::font_system().write().expect("font system");
            (fonts.db().clone(), fonts.raw().locale().to_owned())
        };
        let mut fonts = cosmic_text::FontSystem::new_with_locale_and_db(locale, db);
        Self::with_fonts(
            spans.to_vec().into(),
            spans
                .iter()
                .map(|span| span.text.as_ref())
                .collect::<String>()
                .into(),
            size,
            font,
            width,
            wrapping,
            &mut fonts,
        )
    }

    fn with_fonts(
        spans: Arc<[Span<'static, markdown::Uri>]>,
        plain: Arc<str>,
        size: iced::Pixels,
        font: iced::Font,
        width: f32,
        wrapping: iced::widget::text::Wrapping,
        fonts: &mut cosmic_text::FontSystem,
    ) -> Self {
        let line_height = iced::widget::text::LineHeight::default();
        let mut buffer = cosmic_text::Buffer::new(
            fonts,
            cosmic_text::Metrics::new(size.0, line_height.to_absolute(size).0),
        );
        buffer.set_size(Some(width), None);
        buffer.set_wrap(graphics_text::to_wrap(wrapping));
        buffer.set_rich_text(
            spans.iter().enumerate().map(|(index, span)| {
                let mut attrs = graphics_text::to_attributes(span.font.unwrap_or(font));
                if span.size.is_some() || span.line_height.is_some() {
                    let span_size = span.size.unwrap_or(size);
                    attrs = attrs.metrics(cosmic_text::Metrics::new(
                        span_size.0,
                        span.line_height
                            .unwrap_or(line_height)
                            .to_absolute(span_size)
                            .0,
                    ));
                }
                if let Some(color) = span.color {
                    attrs = attrs.color(graphics_text::to_color(color));
                }
                (span.text.as_ref(), attrs.metadata(index))
            }),
            &graphics_text::to_attributes(font),
            graphics_text::to_shaping_ascii(iced::widget::text::Shaping::Advanced, false),
            None,
        );
        buffer.shape_until_scroll(fonts, false);
        let bounds = graphics_text::align(&mut buffer, fonts, Default::default());
        let snapshot = buffer
            .into_layout_snapshot()
            .expect("complete paragraph layout");
        Self {
            admission: Mutex::new(None),
            owned: Some(Box::new(Geometry {
                #[cfg(test)]
                retired_on: None,
                snapshot,
                bounds,
                spans,
                plain,
                unwrapped: wrapping == iced::widget::text::Wrapping::None,
            })),
        }
    }
}

#[cfg(test)]
fn reference(
    spans: &[Span<'static, markdown::Uri>],
    size: iced::Pixels,
    font: iced::Font,
    width: f32,
    wrapping: iced::widget::text::Wrapping,
) -> ParagraphBuffer {
    ParagraphBuffer::with_spans(iced::advanced::Text {
        content: spans,
        bounds: iced::Size::new(width, f32::INFINITY),
        size,
        line_height: Default::default(),
        font,
        align_x: Default::default(),
        align_y: iced::alignment::Vertical::Top,
        shaping: iced::widget::text::Shaping::Advanced,
        wrapping,
        ellipsize: Default::default(),
    })
}

#[test]
fn visual_projection_matches_actual_paragraph_across_wrap_ligature_and_bidi_context() {
    let width = 720.0;
    let size = iced::Pixels(16.0);
    let sources = [
        "word ".repeat(3000),
        format!(
            "{}ffi office efficient {}",
            "x".repeat(2047),
            " ligature ffi ".repeat(600)
        ),
        "العربية نص عربي يجمع اتجاهين English עברית שלום 123 ".repeat(500),
        "e\u{301} 👩‍🔬 👨‍👩‍👧‍👦 combining mixed نص ".repeat(500),
    ];
    for source in sources {
        let spans = vec![Span::new(source)];
        let full = reference(
            &spans,
            size,
            iced::Font::DEFAULT,
            width,
            iced::widget::text::Wrapping::WordOrGlyph,
        );
        let prepared = Prepared::new(
            &spans,
            size,
            iced::Font::DEFAULT,
            width,
            iced::widget::text::Wrapping::WordOrGlyph,
        );
        assert_eq!(prepared.bounds, full.min_bounds());
        assert_eq!(prepared.snapshot.height(), full.min_height());
        let original = full.buffer().layout_runs().collect::<Vec<_>>();
        assert_eq!(prepared.snapshot.visual_line_count(), original.len());
        for top in [
            0.0,
            full.min_height() * 0.5,
            (full.min_height() - 400.0).max(0.0),
        ] {
            let range = prepared.snapshot.visible_lines(top, top + 400.0);
            let projected = prepared
                .snapshot
                .project(range.clone())
                .expect("visible range");
            let actual = projected.buffer().layout_runs().collect::<Vec<_>>();
            assert_eq!(actual.len(), range.len());
            assert!(actual.len() < 32, "viewport glyph owners remain bounded");
            for (local, visual) in range.enumerate() {
                let before = &original[visual];
                let after = &actual[local];
                assert_eq!(before.rtl, after.rtl);
                assert_eq!(before.line_w.to_bits(), after.line_w.to_bits());
                assert_eq!(before.decorations, after.decorations);
                assert!((before.line_y - (after.line_y + projected.top())).abs() <= 0.02);
                assert!(
                    (before.line_top - (after.line_top + projected.top())).abs() <= 0.02,
                    "translation differs beyond f32 accumulation tolerance"
                );
                assert_eq!(before.glyphs.len(), after.glyphs.len());
                for (expected, actual) in before.glyphs.iter().zip(after.glyphs) {
                    assert_eq!(
                        (
                            expected.font_id,
                            expected.glyph_id,
                            expected.level,
                            expected.metadata
                        ),
                        (
                            actual.font_id,
                            actual.glyph_id,
                            actual.level,
                            actual.metadata
                        )
                    );
                    assert_eq!(
                        (
                            expected.x.to_bits(),
                            expected.y.to_bits(),
                            expected.w.to_bits(),
                            expected.x_offset.to_bits(),
                            expected.y_offset.to_bits()
                        ),
                        (
                            actual.x.to_bits(),
                            actual.y.to_bits(),
                            actual.w.to_bits(),
                            actual.x_offset.to_bits(),
                            actual.y_offset.to_bits()
                        )
                    );
                    assert_eq!(
                        projected.source_byte(local, actual.start),
                        Some(expected.start)
                    );
                    assert_eq!(projected.source_byte(local, actual.end), Some(expected.end));
                }
            }
        }
    }
}

#[test]
fn rich_font_span_metadata_and_unwrapped_horizontal_geometry_are_preserved() {
    let spans = vec![
        Span::new("office ffi ".repeat(500)).font(iced::Font::MONOSPACE),
        Span::new("نص عربي mixed ".repeat(500)).color(iced::Color::from_rgb(0.3, 0.5, 0.7)),
        Span::new("end".to_owned()),
    ];
    for wrapping in [
        iced::widget::text::Wrapping::WordOrGlyph,
        iced::widget::text::Wrapping::None,
    ] {
        let full = reference(
            &spans,
            iced::Pixels(16.0),
            iced::Font::DEFAULT,
            720.0,
            wrapping,
        );
        let prepared = Prepared::new(
            &spans,
            iced::Pixels(16.0),
            iced::Font::DEFAULT,
            720.0,
            wrapping,
        );
        assert_eq!(prepared.bounds, full.min_bounds());
        let projection = prepared
            .snapshot
            .project(0..prepared.snapshot.visual_line_count())
            .expect("all visual lines");
        for (before, after) in full
            .buffer()
            .layout_runs()
            .zip(projection.buffer().layout_runs())
        {
            assert_eq!(before.line_w.to_bits(), after.line_w.to_bits());
            for (before, after) in before.glyphs.iter().zip(after.glyphs) {
                assert_eq!(
                    (
                        before.x.to_bits(),
                        before.w.to_bits(),
                        before.metadata,
                        before.color_opt
                    ),
                    (
                        after.x.to_bits(),
                        after.w.to_bits(),
                        after.metadata,
                        after.color_opt
                    )
                );
            }
        }
    }
}

pub(super) struct Projection {
    pub(super) buffer: Arc<cosmic_text::Buffer>,
    pub(super) source_maps: Vec<cosmic_text::ProjectedSource>,
    pub(super) top: f32,
}
impl Prepared {
    pub(super) fn projection(&self, top: f32, bottom: f32) -> Option<Projection> {
        let projection = self
            .snapshot
            .project(self.snapshot.visible_lines(top, bottom))?;
        let source_maps = projection.source_maps().to_vec();
        let top = projection.top();
        Some(Projection {
            buffer: Arc::new(projection.into_buffer()),
            source_maps,
            top,
        })
    }
    pub(super) fn projection_rect(
        &self,
        top: f32,
        bottom: f32,
        left: f32,
        right: f32,
    ) -> Option<Projection> {
        let projection =
            self.snapshot
                .project_rect(self.snapshot.visible_lines(top, bottom), left, right)?;
        let source_maps = projection.source_maps().to_vec();
        let top = projection.top();
        Some(Projection {
            buffer: Arc::new(projection.into_buffer()),
            source_maps,
            top,
        })
    }
    pub(super) fn hit(&self, position: iced::Point, length: usize) -> usize {
        if position.y < 0.0 {
            return 0;
        }
        if position.y >= self.bounds.height {
            return length;
        }
        let visual = self
            .snapshot
            .visible_lines(position.y, position.y + 0.1)
            .start;
        let advance_hit = self
            .snapshot
            .glyphs_in_x(visual, position.x, position.x)
            .is_some_and(|mut glyphs| {
                glyphs.any(|(_, glyph)| position.x >= glyph.x && position.x <= glyph.x + glyph.w)
            });
        if !advance_hit {
            return self
                .snapshot
                .caret_outside_x(visual, position.x)
                .unwrap_or(0)
                .min(length);
        }
        // A small glyph envelope preserves the engine's cluster caret affinity
        // without projecting a full million-glyph horizontal row per move.
        let margin = self.snapshot.line_height(visual).unwrap_or(20.0) * 4.0;
        let projected = self.projection_rect(
            position.y,
            position.y + 0.1,
            position.x - margin,
            position.x + margin,
        );
        let map_hit = |projection: Projection| {
            let cursor = projection
                .buffer
                .hit(position.x, position.y - projection.top)?;
            projection
                .source_maps
                .get(cursor.line)?
                .source_byte(cursor.index)
                .map(|byte| byte.min(length))
        };
        projected
            .and_then(map_hit)
            .or_else(|| {
                // A short last wrapped row may end far before the pane edge.
                // Its complete visual row is viewport-bounded; no-wrap code uses
                // the indexed projection above and never this full-row fallback.
                (!self.unwrapped)
                    .then(|| self.projection(position.y, position.y + 0.1))
                    .flatten()
                    .and_then(map_hit)
            })
            .unwrap_or(if position.x >= self.bounds.width {
                length
            } else {
                0
            })
    }

    pub(super) fn link(&self, position: iced::Point) -> Option<usize> {
        let visual = self.snapshot.visible_lines(position.y, position.y + 0.1);
        for row in visual {
            if let Some((_, glyph)) = self
                .snapshot
                .glyphs_in_x(row, position.x, position.x)?
                .find(|(_, glyph)| {
                    let left = glyph.x.min(glyph.x + glyph.w);
                    let right = glyph.x.max(glyph.x + glyph.w);
                    left <= position.x && position.x < right
                })
            {
                return Some(glyph.metadata);
            }
        }
        None
    }
    pub(super) fn byte_bounds(&self, byte: usize, length: usize) -> Option<iced::Rectangle> {
        let query = if byte == length {
            // Physical code rows retain their original source ending. Native
            // rich shaping gives that LF/CRLF no glyph or extra visual row;
            // resolve its end caret against the preceding real cluster.
            let plain = self.plain.get(..length)?;
            let end = if plain.ends_with("\r\n") {
                length.saturating_sub(2)
            } else if plain.ends_with('\n') {
                length.saturating_sub(1)
            } else {
                length
            };
            end.saturating_sub(1)
        } else {
            byte
        };
        for visual in self.snapshot.source_visual_lines(query) {
            for (prefix, glyph) in self.snapshot.glyphs_at_byte(visual, query)? {
                if prefix + glyph.start <= query && query < prefix + glyph.end {
                    return Some(iced::Rectangle::new(
                        iced::Point::new(
                            glyph.x.min(glyph.x + glyph.w),
                            self.snapshot.line_top(visual)?,
                        ),
                        iced::Size::new(glyph.w.abs(), self.snapshot.line_height(visual)?),
                    ));
                }
            }
        }
        None
    }
}

fn visit_span_bounds(projection: &Projection, mut visit: impl FnMut(usize, iced::Rectangle)) {
    for run in projection.buffer.layout_runs() {
        let mut start = 0;
        while let Some(first) = run.glyphs.get(start) {
            let end = start
                + run.glyphs[start..]
                    .iter()
                    .position(|glyph| glyph.metadata != first.metadata)
                    .unwrap_or(run.glyphs.len() - start);
            let end = end.max(start + 1);
            let group = &run.glyphs[start..end];
            let left = group
                .iter()
                .map(|glyph| glyph.x.min(glyph.x + glyph.w))
                .fold(f32::INFINITY, f32::min);
            let right = group
                .iter()
                .map(|glyph| glyph.x.max(glyph.x + glyph.w))
                .fold(f32::NEG_INFINITY, f32::max);
            visit(
                first.metadata,
                iced::Rectangle::new(
                    iced::Point::new(left, run.line_top),
                    iced::Size::new(right - left, run.line_height),
                ),
            );
            start = end;
        }
    }
}

fn visit_range_bounds(
    projection: &Projection,
    ranges: &[std::ops::Range<usize>],
    mut paint: impl FnMut(iced::Rectangle),
) {
    if ranges.is_empty() {
        return;
    }
    for run in projection.buffer.layout_runs() {
        let Some(source_map) = projection.source_maps.get(run.line_i) else {
            continue;
        };
        let mut active: Option<(f32, f32)> = None;
        let mut flush = |active: &mut Option<(f32, f32)>| {
            if let Some((left, right)) = active.take() {
                paint(iced::Rectangle::new(
                    iced::Point::new(left, run.line_top),
                    iced::Size::new(right - left, run.line_height),
                ));
            }
        };
        for glyph in run.glyphs {
            // Match Buffer::hit's grapheme caret regions even when a sparse
            // projection has packed distant original source intervals.
            let cluster = &run.text[glyph.start..glyph.end];
            let width = glyph.w / cluster.graphemes(true).count().max(1) as f32;
            for (index, (byte, grapheme)) in cluster.grapheme_indices(true).enumerate() {
                let (Some(begin), Some(finish)) = (
                    source_map.source_byte(glyph.start + byte),
                    source_map.source_byte(glyph.start + byte + grapheme.len()),
                ) else {
                    continue;
                };
                let found = ranges.partition_point(|range| range.end <= begin);
                if ranges
                    .get(found)
                    .is_some_and(|range| range.start < finish && !range.is_empty())
                {
                    let left = glyph.x + index as f32 * width;
                    let right = left + width;
                    active = Some(
                        active.map_or((left, right), |(lo, hi)| (lo.min(left), hi.max(right))),
                    );
                } else {
                    flush(&mut active);
                }
            }
        }
        flush(&mut active);
    }
}

pub(super) fn paint(
    renderer: &mut cosmic::Renderer,
    projection: &Projection,
    spans: &[Span<'static, markdown::Uri>],
    matches: &[std::ops::Range<usize>],
    selection: std::ops::Range<usize>,
    size: iced::Pixels,
    position: iced::Point,
    theme: &cosmic::Theme,
    defaults: &iced::advanced::renderer::Style,
    viewport: &iced::Rectangle,
) {
    use iced::advanced::Renderer as _;
    use iced::advanced::text::Renderer as _;
    let translation = iced::Vector::new(position.x, position.y + projection.top);
    visit_span_bounds(projection, |index, bounds| {
        if let Some(highlight) = spans.get(index).and_then(|span| span.highlight) {
            let span = &spans[index];
            renderer.fill_quad(
                iced::advanced::renderer::Quad {
                    bounds: iced::Rectangle::new(
                        bounds.position() + translation
                            - iced::Vector::new(span.padding.left, span.padding.top),
                        bounds.size() + iced::Size::new(span.padding.x(), span.padding.y()),
                    ),
                    border: highlight.border,
                    ..Default::default()
                },
                highlight.background,
            );
        }
    });
    for (ranges, color) in [
        (matches, design::accent_wash(theme)),
        (
            std::slice::from_ref(&selection),
            design::accent_color(theme),
        ),
    ] {
        visit_range_bounds(projection, ranges, |bounds| {
            renderer.fill_quad(
                iced::advanced::renderer::Quad {
                    bounds: bounds + translation,
                    ..Default::default()
                },
                color,
            );
        });
    }
    // Decorations must stay visible above opaque selection and Find surfaces.
    visit_span_bounds(projection, |index, bounds| {
        let Some(span) = spans.get(index) else {
            return;
        };
        let font_size = span.size.unwrap_or(size);
        let height = span
            .line_height
            .unwrap_or_default()
            .to_absolute(font_size)
            .0;
        let baseline = font_size.0 + (height - font_size.0) / 2.0;
        for (draw, shift) in [
            (span.underline, font_size.0 * 0.08),
            (span.strikethrough, font_size.0 / 2.0),
        ] {
            if draw {
                renderer.fill_quad(
                    iced::advanced::renderer::Quad {
                        bounds: iced::Rectangle::new(
                            bounds.position()
                                + translation
                                + iced::Vector::new(0.0, baseline - shift),
                            iced::Size::new(bounds.width, 1.0),
                        ),
                        ..Default::default()
                    },
                    span.color.unwrap_or(defaults.text_color),
                );
            }
        }
    });
    renderer.fill_raw(graphics_text::Raw {
        buffer: Arc::downgrade(&projection.buffer),
        position: position + iced::Vector::new(0.0, projection.top),
        color: defaults.text_color,
        clip_bounds: *viewport,
    });
}

#[test]
fn cache_rejects_stale_source_and_width_and_releases_owned_layout() {
    let spans: Arc<[Span<'static, markdown::Uri>]> = vec![Span::new("word ".repeat(1000))].into();
    let cache = Cache::default();
    let entry = cache.entry(7, spans.clone());
    entry
        .borrow_mut()
        .configure(720.0, iced::Pixels(16.0), iced::Font::DEFAULT);
    let request = entry.borrow_mut().request().expect("first request");
    let prepared = Arc::new(Prepared::new(
        &spans,
        request.key.size,
        request.key.font,
        request.key.width,
        iced::widget::text::Wrapping::WordOrGlyph,
    ));
    let weak = Arc::downgrade(&prepared);
    assert!(entry.borrow_mut().complete(&request, Some(prepared)));
    assert!(entry.borrow().prepared().is_some());
    entry
        .borrow_mut()
        .configure(300.0, iced::Pixels(16.0), iced::Font::DEFAULT);
    assert!(
        weak.upgrade().is_none(),
        "width replacement releases old glyph owner"
    );
    assert!(!entry.borrow_mut().complete(&request, None));
    let current = entry.borrow_mut().request().expect("new width request");
    let replacement: Arc<[Span<'static, markdown::Uri>]> =
        vec![Span::new("replacement".to_owned())].into();
    let _new = cache.entry(7, replacement);
    assert!(
        cache.find(current.id).is_none(),
        "old source cannot receive completion"
    );
    drop(entry);
    assert!(current.cancelled.load(Ordering::Acquire));
}

#[test]
fn flow_hits_links_and_find_bounds_use_the_original_full_context_source() {
    let text = format!(
        "{} office ffi العربية עברית {}",
        "word ".repeat(1000),
        "suffix ".repeat(500)
    );
    let spans: Arc<[Span<'static, markdown::Uri>]> =
        vec![Span::new(text.clone()).link(Arc::from("https://example.com"))].into();
    let prepared = Prepared::new(
        &spans,
        iced::Pixels(16.0),
        iced::Font::DEFAULT,
        720.0,
        iced::widget::text::Wrapping::WordOrGlyph,
    );
    for visual in [
        0,
        prepared.snapshot.visual_line_count() / 2,
        prepared.snapshot.visual_line_count() - 1,
    ] {
        for (prefix, glyph) in prepared.snapshot.glyphs(visual).unwrap().take(3) {
            let position = iced::Point::new(
                glyph.x + glyph.w * 0.25,
                prepared.snapshot.line_top(visual).unwrap() + 2.0,
            );
            assert_eq!(prepared.link(position), Some(glyph.metadata));
            let hit = prepared.hit(position, text.len());
            assert!(text.is_char_boundary(hit));
            let bounds = prepared
                .byte_bounds(prefix + glyph.start, text.len())
                .expect("source glyph bound");
            assert_eq!(bounds.y, prepared.snapshot.line_top(visual).unwrap());
            assert_eq!(bounds.x.to_bits(), glyph.x.min(glyph.x + glyph.w).to_bits());
        }
    }
}

#[test]
fn virtual_row_eviction_releases_flow_glyphs_without_hiding_the_chat() {
    use iced::advanced::{layout, widget::Tree};
    use std::cell::Cell;
    let renderer = crate::app::tests::headless_renderer();
    let cache = Cache::default();
    let owners = Rc::new(RefCell::new(Vec::new()));
    let spans: Arc<[Span<'static, markdown::Uri>]> = vec![Span::new("word ".repeat(800))].into();
    let window = Rc::new(Cell::new(Some(virtualized::Viewport {
        top: 0.0,
        height: 200.0,
        bottom: false,
    })));
    let rows = (0..200)
        .map(|identity| {
            let cache = cache.clone();
            let owners = owners.clone();
            let spans = spans.clone();
            virtualized::Row::new(identity, 0, 1000.0, None, move || {
                let flow = cache.entry(identity, spans.clone());
                flow.borrow_mut()
                    .configure(720.0, iced::Pixels(16.0), iced::Font::DEFAULT);
                let request = flow.borrow_mut().request().unwrap();
                let ready = Arc::new(Prepared::new(
                    &spans,
                    request.key.size,
                    request.key.font,
                    request.key.width,
                    iced::widget::text::Wrapping::WordOrGlyph,
                ));
                owners.borrow_mut().push(Arc::downgrade(&ready));
                assert!(flow.borrow_mut().complete(&request, Some(ready)));
                selectable_prose::flow_text(
                    spans.clone(),
                    iced::Pixels(16.0),
                    iced::Font::DEFAULT,
                    Target {
                        card: CardId::new(),
                        generation: 0,
                    },
                    None,
                    selectable_prose::Logical { identity, start: 0 },
                    flow,
                )
            })
        })
        .collect();
    let mut element = virtualized::column(rows, window.clone());
    let mut tree = Tree::new(&element);
    let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(720.0, f32::INFINITY));
    element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    let first = owners.borrow().clone();
    assert!(!first.is_empty());
    assert!(first.iter().all(|owner| owner.upgrade().is_some()));
    window.set(Some(virtualized::Viewport {
        top: 100000.0,
        height: 200.0,
        bottom: false,
    }));
    element
        .as_widget_mut()
        .layout(&mut tree, &renderer, &limits);
    assert!(
        first.iter().all(|owner| owner.upgrade().is_none()),
        "cached item descriptors must not retain evicted glyph snapshots"
    );
    assert!(
        cache.retained_bytes() > 0,
        "the same active chat still owns visible row geometry"
    );
}

#[test]
fn continuous_append_publishes_versioned_progress_and_eventually_exact_final_geometry() {
    fn text(content: &markdown::Content) -> markdown::Text {
        let markdown::Item::Paragraph(text) = &content.items()[0] else {
            panic!("paragraph fixture")
        };
        text.clone()
    }
    fn finish(request: &Request) -> Arc<Prepared> {
        let (spans, plain) = request.source.materialize();
        let mut result = Prepared::new(
            &spans,
            request.key.size,
            request.key.font,
            request.key.width,
            iced::widget::text::Wrapping::WordOrGlyph,
        );
        result.plain = plain;
        Arc::new(result)
    }
    let cache = Cache::default();
    let style = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE).style;
    let mut content = markdown::Content::parse(&"word ".repeat(600));
    let mut entry = cache.markdown_entry(7, text(&content), style);
    entry
        .borrow_mut()
        .configure(720.0, iced::Pixels(16.0), iced::Font::DEFAULT);
    let mut pending = entry.borrow_mut().request().unwrap();
    let mut visible_bytes = 0;
    let mut published = 0;
    // Controlled scheduler: one completion per 20 append ticks. A tick models
    // a 12.5ms source update, not a wall-clock benchmark or a timing assertion.
    for tick in 1..=240 {
        content.push_str(" next");
        let next = cache.markdown_entry(7, text(&content), style);
        assert!(
            Rc::ptr_eq(&entry, &next),
            "proven append keeps in-flight work live"
        );
        entry = next;
        assert!(
            entry.borrow_mut().request().is_none(),
            "no replacement job queues behind a full paragraph"
        );
        if tick % 20 == 0 {
            let ready = finish(&pending);
            assert!(
                ready.plain.len() > visible_bytes,
                "displayed text progresses during uninterrupted input"
            );
            assert!(text(&content).plain().starts_with(ready.plain.as_ref()));
            visible_bytes = ready.plain.len();
            assert!(entry.borrow_mut().complete(&pending, Some(ready)));
            let shown = entry.borrow().prepared().unwrap();
            assert_eq!(
                shown.plain.len(),
                visible_bytes,
                "snapshot retains its own source version"
            );
            assert!(shown.plain.len() < text(&content).len());
            published += 1;
            assert!(
                entry.borrow().geometry_pending(0).is_some(),
                "visible prefix geometry cannot certify the current revision before redraw requests it"
            );
            pending = entry
                .borrow_mut()
                .request()
                .expect("newest source still needs exact layout");
            assert!(entry.borrow().geometry_pending(0).is_some());
        }
    }
    assert_eq!(published, 12);
    let final_ready = finish(&pending);
    assert!(entry.borrow_mut().complete(&pending, Some(final_ready)));
    assert!(entry.borrow_mut().request().is_none());
    assert!(entry.borrow().geometry_pending(0).is_none());
    let ready = entry.borrow().prepared().unwrap();
    assert_eq!(ready.plain.as_ref(), text(&content).plain().as_ref());
    let reference = reference(
        &ready.spans,
        iced::Pixels(16.0),
        iced::Font::DEFAULT,
        720.0,
        iced::widget::text::Wrapping::WordOrGlyph,
    );
    assert_eq!(ready.bounds, reference.min_bounds());
    let all = ready
        .snapshot
        .project(0..ready.snapshot.visual_line_count())
        .unwrap();
    for (expected, actual) in reference
        .buffer()
        .layout_runs()
        .zip(all.buffer().layout_runs())
    {
        assert_eq!(expected.glyphs.len(), actual.glyphs.len());
        assert_eq!(expected.line_w.to_bits(), actual.line_w.to_bits());
        for (expected, actual) in expected.glyphs.iter().zip(actual.glyphs) {
            assert_eq!(
                (
                    expected.font_id,
                    expected.glyph_id,
                    expected.x.to_bits(),
                    expected.w.to_bits()
                ),
                (
                    actual.font_id,
                    actual.glyph_id,
                    actual.x.to_bits(),
                    actual.w.to_bits()
                )
            );
        }
    }
    let rewrite = markdown::Content::parse("**replacement changes style and content**");
    let replacement = cache.markdown_entry(7, text(&rewrite), style);
    assert!(!Rc::ptr_eq(&entry, &replacement));
    assert!(
        !replacement.borrow().accepts(&pending),
        "old rendered source cannot cross a rewrite"
    );
}

#[test]
fn request_lookup_does_not_retain_visited_flow_owners() {
    let cache = Cache::default();
    let active = cache.entry(0, vec![Span::new("active")].into());
    let id = active.borrow().id;
    for identity in 1..10_000 {
        drop(cache.entry(identity, vec![Span::new("visited")].into()));
    }
    assert!(Rc::ptr_eq(
        &cache.find(id).expect("active request"),
        &active
    ));
    assert!(cache.0.borrow().logical.len() <= 256);
    assert!(cache.0.borrow().requests.len() <= 256);
    let weak = Rc::downgrade(&active);
    drop(active);
    assert!(weak.upgrade().is_none());
    assert!(cache.find(id).is_none());
}

#[test]
fn code_flow_preserves_full_physical_line_context_without_sync_shaping() {
    let size = iced::Pixels(16.0);
    for source in [
        format!("{}ffi office", "x".repeat(2047)),
        format!("{}\tend", "x".repeat(2047)),
        "العربية עברית e\u{301} 👩‍🔬 mixed ".repeat(200),
    ] {
        let text: Arc<str> = Arc::from(source.as_str());
        let cache = Cache::default();
        let flow = cache.code_entry(
            42,
            text.clone(),
            0..text.len(),
            Arc::from([]),
            vec![0].into(),
            reader_source::Source::new(&source),
        );
        flow.borrow_mut()
            .configure(f32::INFINITY, size, iced::Font::MONOSPACE);
        let request = flow.borrow_mut().request().expect("bounded worker request");
        assert!(request.key.unwrapped);
        assert_eq!(
            flow.borrow().bounds(source.len()).width,
            design::READABLE_MEASURE,
            "unmeasured width must not pin an oversized estimate in reader memory"
        );
        let (spans, plain) = request.source.materialize();
        assert_eq!(&*plain, source);
        let full = reference(
            &spans,
            size,
            iced::Font::MONOSPACE,
            request.key.width,
            iced::widget::text::Wrapping::None,
        );
        let ready = Prepared::new(
            &spans,
            size,
            iced::Font::MONOSPACE,
            request.key.width,
            iced::widget::text::Wrapping::None,
        );
        assert_eq!(ready.bounds, full.min_bounds());
        assert_eq!(ready.snapshot.visual_line_count(), 1);
        let original = full.buffer().layout_runs().next().expect("physical line");
        for ((_, actual), expected) in ready.snapshot.glyphs(0).unwrap().zip(original.glyphs) {
            assert_eq!(
                (
                    actual.glyph_id,
                    actual.font_id,
                    actual.level,
                    actual.metadata
                ),
                (
                    expected.glyph_id,
                    expected.font_id,
                    expected.level,
                    expected.metadata
                )
            );
            assert_eq!(
                (actual.x.to_bits(), actual.y.to_bits(), actual.w.to_bits()),
                (
                    expected.x.to_bits(),
                    expected.y.to_bits(),
                    expected.w.to_bits()
                )
            );
        }
    }
}

#[test]
fn horizontal_flow_projects_sparse_glyphs_and_keeps_source_hit_and_find_coordinates() {
    let source = format!("{} ffi العربية עברית end", "office ".repeat(2000));
    let spans = vec![Span::new(source.clone()).link(Arc::from("https://example.com"))];
    let prepared = Prepared::new(
        &spans,
        iced::Pixels(16.0),
        iced::Font::MONOSPACE,
        720.0,
        iced::widget::text::Wrapping::None,
    );
    for left in [
        0.0,
        prepared.bounds.width * 0.5,
        (prepared.bounds.width - 400.0).max(0.0),
    ] {
        let projection = prepared
            .projection_rect(0.0, 40.0, left - 64.0, left + 464.0)
            .unwrap();
        let run = projection.buffer.layout_runs().next().unwrap();
        assert!(
            run.glyphs.len() < 512,
            "raw display ownership follows horizontal viewport"
        );
        for glyph in run.glyphs {
            let start = projection.source_maps[0].source_byte(glyph.start).unwrap();
            let end = projection.source_maps[0].source_byte(glyph.end).unwrap();
            let point = iced::Point::new(glyph.x + glyph.w * 0.25, 2.0);
            let hit = prepared.hit(point, source.len());
            assert!(
                (start..=end).contains(&hit),
                "caret remains in original cluster {start}..{end}, got{hit}"
            );
            assert_eq!(prepared.link(point), Some(glyph.metadata));
            let bounds = prepared.byte_bounds(start, source.len()).unwrap();
            assert_eq!(bounds.x.to_bits(), glyph.x.min(glyph.x + glyph.w).to_bits());
        }
    }
    let wrapped = Prepared::new(
        &[Span::new("short last row".to_owned())],
        iced::Pixels(16.0),
        iced::Font::DEFAULT,
        720.0,
        iced::widget::text::Wrapping::WordOrGlyph,
    );
    assert_eq!(wrapped.hit(iced::Point::new(700.0, 2.0), 14), 14);
}

#[test]
fn flow_caret_external_edges_match_full_engine_in_rtl_and_mixed_rows() {
    for source in [
        "שלום עברית ".repeat(400),
        "العربية نص عربي ".repeat(400),
        "abc العربية עברית mixed ".repeat(400),
    ] {
        let spans = vec![Span::new(source.clone())];
        let size = iced::Pixels(16.0);
        let full = reference(
            &spans,
            size,
            iced::Font::DEFAULT,
            720.0,
            iced::widget::text::Wrapping::None,
        );
        let prepared = Prepared::new(
            &spans,
            size,
            iced::Font::DEFAULT,
            720.0,
            iced::widget::text::Wrapping::None,
        );
        for x in [
            -10_000.0,
            -1.0,
            0.0,
            prepared.bounds.width + 1.0,
            prepared.bounds.width + 10_000.0,
        ] {
            let expected = full.buffer().hit(x, 2.0).unwrap().index;
            assert_eq!(
                prepared.hit(iced::Point::new(x, 2.0), source.len()),
                expected,
                "external caret x{x}"
            );
        }
    }
}

#[test]
fn projected_selection_and_find_keep_native_ligature_caret_regions() {
    use iced::advanced::renderer::{Headless, Renderer};
    for (source, expected) in [("office affinity", "f"), ("لا", "ل")] {
        let spans = vec![Span::new(source)];
        let prepared = Prepared::new(
            &spans,
            iced::Pixels(32.0),
            iced::Font::with_name("DejaVu Sans"),
            400.0,
            iced::widget::text::Wrapping::WordOrGlyph,
        );
        let projection = prepared.projection_rect(0.0, 100.0, 0.0, 400.0).unwrap();
        let (start, end, x, y, width, count) = projection
            .buffer
            .layout_runs()
            .find_map(|run| {
                run.glyphs.iter().find_map(|glyph| {
                    let cluster = &run.text[glyph.start..glyph.end];
                    let count = cluster.graphemes(true).count();
                    (count > 1).then(|| {
                        (
                            projection.source_maps[run.line_i]
                                .source_byte(glyph.start)
                                .unwrap(),
                            projection.source_maps[run.line_i]
                                .source_byte(
                                    glyph.start + cluster.graphemes(true).next().unwrap().len(),
                                )
                                .unwrap(),
                            glyph.x,
                            run.line_top,
                            glyph.w / count as f32,
                            count,
                        )
                    })
                })
            })
            .expect("fixture font must shape a real ligature");
        assert_eq!(&source[start..end], expected);
        for find in [false, true] {
            let mut renderer = crate::app::tests::headless_renderer();
            let viewport = iced::Rectangle::with_size(iced::Size::new(400.0, 100.0));
            renderer.reset(viewport);
            let ranges = vec![start..end];
            paint(
                &mut renderer,
                &projection,
                &prepared.spans,
                if find { &ranges } else { &[] },
                if find { 0..0 } else { start..end },
                iced::Pixels(32.0),
                iced::Point::ORIGIN,
                &cosmic::theme::active(),
                &Default::default(),
                &viewport,
            );
            let pixels = renderer.screenshot(iced::Size::new(400, 100), 1.0, iced::Color::WHITE);
            let color = if find {
                design::accent_wash(&cosmic::theme::active())
            } else {
                design::accent_color(&cosmic::theme::active())
            }
            .into_rgba8();
            for part in 0..count {
                let px = (x + width * (part as f32 + 0.5)) as usize;
                let py = (y + 1.0) as usize;
                let painted = pixels[(py * 400 + px) * 4..][..4]
                    .iter()
                    .zip(color)
                    .all(|(actual, expected)| actual.abs_diff(expected) <= 1);
                assert_eq!(
                    painted,
                    part == 0,
                    "projected ligature component {part}, find={find}"
                );
            }
        }
    }
}

#[test]
fn projected_decorations_stay_above_selection_and_find() {
    use iced::advanced::renderer::{Headless, Renderer};
    for underline in [false, true] {
        let (size, line_height, stroke_y) = if underline {
            (25.0_f32, 31.0_f32, 26)
        } else {
            (20.0_f32, 24.0_f32, 12)
        };
        let mut span = Span::new("x       x");
        span.color = Some(iced::Color::BLACK);
        span.underline = underline;
        span.strikethrough = !underline;
        span.line_height = Some(iced::widget::text::LineHeight::Absolute(line_height.into()));
        let prepared = Prepared::new(
            &[span],
            size.into(),
            iced::Font::default(),
            400.0,
            iced::widget::text::Wrapping::WordOrGlyph,
        );
        let projection = prepared.projection_rect(0.0, 100.0, 0.0, 400.0).unwrap();
        let x = projection
            .buffer
            .layout_runs()
            .find_map(|run| {
                run.glyphs
                    .iter()
                    .find(|glyph| run.text[glyph.start..glyph.end].trim().is_empty())
                    .map(|glyph| (glyph.x + glyph.w * 0.5) as usize)
            })
            .unwrap();
        for mode in 0..3 {
            let mut renderer = crate::app::tests::headless_renderer();
            let viewport = iced::Rectangle::with_size(iced::Size::new(400.0, 100.0));
            renderer.reset(viewport);
            let ranges = vec![0..prepared.plain.len()];
            paint(
                &mut renderer,
                &projection,
                &prepared.spans,
                if mode == 2 { &ranges } else { &[] },
                if mode == 1 {
                    0..prepared.plain.len()
                } else {
                    0..0
                },
                size.into(),
                iced::Point::ORIGIN,
                &cosmic::theme::active(),
                &Default::default(),
                &viewport,
            );
            let pixels = renderer.screenshot(iced::Size::new(400, 100), 1.0, iced::Color::WHITE);
            assert_eq!(
                &pixels[(stroke_y * 400 + x) * 4..][..4],
                &[0, 0, 0, 255],
                "projected decoration covered: underline={underline}, mode={mode}"
            );
        }
    }
}

#[test]
fn last_geometry_owner_retires_off_thread_after_widget_and_cache_release() {
    let spans: Arc<[Span<'static, markdown::Uri>]> = vec![Span::new("word ".repeat(4000))].into();
    let mut prepared = Prepared::new(
        &spans,
        iced::Pixels(16.0),
        iced::Font::DEFAULT,
        720.0,
        iced::widget::text::Wrapping::WordOrGlyph,
    );
    let (sender, receiver) = std::sync::mpsc::channel();
    prepared.retired_on = Some(sender);
    let weak_plain = Arc::downgrade(&prepared.plain);
    let prepared = Arc::new(prepared);
    let widget_owner = prepared.clone();
    drop(prepared);
    assert!(
        receiver.try_recv().is_err(),
        "a still mounted widget owns the snapshot"
    );
    drop(widget_owner);
    let thread = receiver
        .recv_timeout(Duration::from_secs(10))
        .expect("finite background retirement");
    assert_ne!(thread, std::thread::current().id());
    preparation::retirement_barrier();
    assert!(
        weak_plain.upgrade().is_none(),
        "retirement releases the actual source owner"
    );
}

#[test]
fn last_flow_entry_request_and_action_retire_real_raw_spans_after_canonical_and_document_owners() {
    let style = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE).style;
    for final_owner in 0..3 {
        let (sent, disposed) = std::sync::mpsc::channel();
        let (canonical, document, bytes, expected, styled) = std::thread::spawn(move || {
            let mut canonical = markdown::Content::parse(
                &"**bold λ** [linked](https://example.test/source) é ".repeat(5000),
            );
            canonical.discard_continuation();
            let bytes = canonical.watch_first_raw_spans(sent);
            let [markdown::Item::Paragraph(text)] = canonical.items() else {
                panic!("one actual rich paragraph")
            };
            let expected = text.plain();
            text.plain_prefixes();
            let styled = text.spans(style);
            let document = prose_document::Data::snapshot(&canonical);
            (canonical, document, bytes, expected, styled)
        })
        .join()
        .unwrap();
        assert!(bytes > 64 * 1024);
        let document = prose_document::Document::new(document);
        let [markdown::Item::Paragraph(text)] = canonical.items() else {
            unreachable!()
        };
        let before = markdown::materialized_bytes();
        let cache = Cache::default();
        let entry = cache.markdown_entry(91, text.clone(), style);
        assert_eq!(
            markdown::materialized_bytes(),
            before,
            "Flow capture never materializes full source on UI"
        );
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        entry.borrow_mut().bind_target(target);
        entry
            .borrow_mut()
            .configure(720.0, iced::Pixels(16.0), iced::Font::DEFAULT);
        let request = entry.borrow_mut().request().unwrap();
        let shared = request.source.clone();
        let (actual_styled, actual_plain) = std::thread::spawn(move || shared.materialize())
            .join()
            .unwrap();
        assert_eq!(actual_plain.as_ref(), expected.as_ref());
        assert!(
            Arc::ptr_eq(&actual_styled, &styled),
            "exact primed rich style and source metadata are shared"
        );
        let weak_plain = Arc::downgrade(&actual_plain);
        let weak_styled = Arc::downgrade(&actual_styled);
        drop((expected, styled, actual_styled, actual_plain));
        let mut entry = Some(entry);
        let mut request = Some(request);
        let mut action = Some(Action::FlowReady(
            target,
            request.as_ref().unwrap().clone(),
            None,
        ));
        preparation::retire_presentation(preparation::Presentation {
            parsed: preparation::Parsed {
                source: Default::default(),
                markdown: canonical,
            },
            blocks: Default::default(),
            palettes: None,
            item: None,
            prepared_tools: None,
            prompt: None,
        });
        drop(document);
        preparation::retirement_barrier();
        assert!(
            disposed.try_recv().is_err(),
            "the real raw span Vec outlives canonical and document batches"
        );
        match final_owner {
            0 => {
                drop(request.take());
                drop(action.take());
            }
            1 => {
                drop(entry.take());
                drop(action.take());
            }
            _ => {
                drop(entry.take());
                drop(request.take());
            }
        }
        preparation::retirement_barrier();
        assert!(
            disposed.try_recv().is_err(),
            "the remaining actual Entry, Request or queued Action owns the raw spans"
        );
        let release = preparation::hold_retirement();
        drop((entry, request, action));
        assert!(
            disposed.try_recv().is_err(),
            "final Flow source release transfers actual raw spans instead of destructing them on UI"
        );
        release.send(()).unwrap();
        let reclaimer = preparation::retirement_barrier();
        let (thread, actual_bytes) = disposed
            .try_recv()
            .expect("raw span Vec and owned Strings destroyed before barrier");
        assert_eq!(actual_bytes, bytes);
        assert_eq!(thread, reclaimer);
        assert_ne!(thread, std::thread::current().id());
        assert!(
            weak_plain.upgrade().is_none() && weak_styled.upgrade().is_none(),
            "all primed source/style backing is released with the same raw owner"
        );
    }
}

#[test]
fn code_and_styled_flow_sources_share_one_final_retirement_across_request_clones() {
    for code in [false, true] {
        let text: Arc<str> = "actual original λ é\n".repeat(5000).into();
        let spans: Arc<[Span<'static, markdown::Uri>]> = vec![
            Span::new(text.to_string())
                .link(Arc::from("https://example.test/source"))
                .strikethrough(true),
        ]
        .into();
        let weak_text = Arc::downgrade(&text);
        let weak_spans = Arc::downgrade(&spans);
        let source = if code {
            SourceData::Code {
                text: text.clone(),
                range: 0..text.len(),
                spans: spans.clone(),
                starts: Arc::from([0]),
                source: reader_source::Source::new(&text),
            }
        } else {
            SourceData::Styled(spans.clone())
        };
        let cache = Cache::default();
        let entry = cache.source_entry(71, source);
        let (sent, disposed) = std::sync::mpsc::channel();
        entry.borrow_mut().source.watch_disposal(sent);
        entry
            .borrow_mut()
            .configure(720.0, iced::Pixels(16.0), iced::Font::DEFAULT);
        let request = entry.borrow_mut().request().unwrap();
        let last = request.clone();
        assert!(Arc::ptr_eq(&request.source.0, &last.source.0));
        let bytes = request.source.len();
        drop((text, spans, entry, request));
        preparation::retirement_barrier();
        assert!(disposed.try_recv().is_err());
        let release = preparation::hold_retirement();
        drop(last);
        assert!(
            disposed.try_recv().is_err(),
            "Code and Styled last clones transfer their actual variable-size source backing"
        );
        release.send(()).unwrap();
        let reclaimer = preparation::retirement_barrier();
        assert_eq!(disposed.try_recv().unwrap(), (reclaimer, bytes));
        assert!(weak_text.upgrade().is_none() && weak_spans.upgrade().is_none());
    }
}

#[test]
fn queued_none_flow_results_hold_admission_through_cancellation_error_and_worker_panic() {
    use iced::futures::StreamExt;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for outcome in [
        None,
        Some(WorkerOutcome::ReclamationError),
        Some(WorkerOutcome::Panic),
        Some(WorkerOutcome::CancelAfterShape),
    ] {
        let (sent, disposed) = std::sync::mpsc::channel();
        let (canonical, document, bytes) = std::thread::spawn(move || {
            let mut canonical = markdown::Content::parse(
                &"**owned λ** [source](https://example.test/source) é ".repeat(500),
            );
            canonical.discard_continuation();
            let bytes = canonical.watch_first_raw_spans(sent);
            let document = prose_document::Data::snapshot(&canonical);
            (canonical, document, bytes)
        })
        .join()
        .unwrap();
        let document = prose_document::Document::new(document);
        let [markdown::Item::Paragraph(text)] = canonical.items() else {
            unreachable!()
        };
        let style = design::markdown_settings(&cosmic::Theme::dark(), design::TEXT_PROSE).style;
        let cache = Cache::default();
        let entry = cache.markdown_entry(95, text.clone(), style);
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        entry.borrow_mut().bind_target(target);
        entry
            .borrow_mut()
            .configure(720.0, iced::Pixels(16.0), iced::Font::DEFAULT);
        let mut request = entry.borrow_mut().request().unwrap();
        request.outcome = outcome;
        if outcome.is_none() {
            request.cancelled.store(true, Ordering::Release);
        }
        preparation::retire_presentation(preparation::Presentation {
            parsed: preparation::Parsed {
                source: Default::default(),
                markdown: canonical,
            },
            blocks: Default::default(),
            palettes: None,
            item: None,
            prepared_tools: None,
            prompt: None,
        });
        drop(document);
        preparation::retirement_barrier();
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let task = run_with_slots(target, request, slots.clone()).expect("one actual private slot");
        let reply = executor.block_on(async {
            let mut effects = iced::runtime::task::into_stream(task).unwrap();
            tokio::time::timeout(Duration::from_secs(20), async {
                while let Some(effect) = effects.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                        reply,
                    ))) = effect
                    {
                        assert!(
                            matches!(&reply,Action::FlowReady(actual,_,None) if *actual==target),
                            "actual cancelled/error/panic worker returns its typed None reply"
                        );
                        return reply;
                    }
                }
                panic!("actual None result must be delivered");
            })
            .await
            .expect("finite real worker outcome")
        });
        drop(entry);
        let reclaimer = preparation::retirement_barrier();
        assert!(
            disposed.try_recv().is_err(),
            "queued None Action is the remaining actual raw source owner"
        );
        assert!(
            slots.clone().try_acquire_owned().is_err(),
            "queued None completion retains output admission until UI consumption"
        );
        let mut wakes = Box::pin(availability_stream());
        executor
            .block_on(wakes.next())
            .expect("initial state consumed before release");
        let release = preparation::hold_retirement();
        drop(reply);
        assert!(disposed.try_recv().is_err());
        assert!(
            slots.clone().try_acquire_owned().is_err(),
            "discard keeps admission while actual raw source retirement is stalled"
        );
        release.send(()).unwrap();
        let permit = executor.block_on(async {
            tokio::time::timeout(Duration::from_secs(10), async {
                loop {
                    wakes.next().await.expect("live Flow subscription");
                    // The global epoch may also advance for another parallel
                    // fixture. This private semaphore is the capacity oracle.
                    if let Ok(permit) = slots.clone().try_acquire_owned() {
                        return permit;
                    }
                }
            })
            .await
            .expect("actual private admission availability wake")
        });
        assert_eq!(
            disposed
                .try_recv()
                .expect("actual raw span disposal precedes slot availability wake"),
            (reclaimer, bytes)
        );
        drop(permit);
    }
    executor.shutdown_background();
}

#[test]
fn stale_output_reclamation_wakes_a_deferred_idle_reader_after_actual_permit_release() {
    use iced::futures::StreamExt;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let target = Target {
        card: CardId::new(),
        generation: 0,
    };
    let slots = Arc::new(tokio::sync::Semaphore::new(1));
    let stale = Prepared::new(
        &[Span::new("old geometry")],
        iced::Pixels(16.0),
        iced::Font::DEFAULT,
        400.0,
        iced::widget::text::Wrapping::WordOrGlyph,
    );
    *stale.admission.lock().unwrap() = Some(OutputAdmission::new(
        Source::new(SourceData::Styled(stale.spans.clone())),
        WorkSlot(Some(slots.clone().try_acquire_owned().unwrap())),
    ));
    let cache = Cache::default();
    let entry = cache.entry(91, vec![Span::new("new idle reader")].into());
    entry.borrow_mut().bind_target(target);
    entry
        .borrow_mut()
        .configure(400.0, iced::Pixels(16.0), iced::Font::DEFAULT);
    let request = entry.borrow_mut().request().unwrap();
    let epoch = run_with_slots(target, request.clone(), slots.clone())
        .err()
        .expect("old output owns actual slot");
    entry.borrow_mut().defer(&request, epoch);
    let events = availability_stream();
    let mut events = Box::pin(events);
    executor
        .block_on(events.next())
        .expect("initial current-state wake");
    let release = preparation::hold_retirement();
    drop(stale);
    assert!(
        slots.clone().try_acquire_owned().is_err(),
        "stale output cannot release admission before geometry retirement"
    );
    // A UI wake/redraw before the reclaimer advances cannot admit B. Other
    // tests may also advance the shared epoch; actual semaphore admission,
    // rather than a guessed epoch value, is the control's authority.
    for retry in deferred_requests(target) {
        let epoch = run_with_slots(target, retry.clone(), slots.clone())
            .err()
            .expect("stalled destructor still owns admission");
        entry.borrow_mut().defer(&retry, epoch);
    }
    release.send(()).unwrap();
    let retry = executor.block_on(async {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                assert!(matches!(
                    events.next().await,
                    Some(Message::ChatUi(Action::FlowAvailable))
                ));
                // This is the production availability dispatch contract: only
                // already deferred live entries retry, without native input,
                // provider publication, tick or an assumed extra redraw.
                for retry in deferred_requests(target) {
                    match run_with_slots(target, retry.clone(), slots.clone()) {
                        Ok(task) => return (retry, task),
                        Err(epoch) => entry.borrow_mut().defer(&retry, epoch),
                    }
                }
            }
        })
        .await
        .expect("permit release itself wakes deferred B")
    });
    let (request, task) = retry;
    let output = executor.block_on(async {
        let mut outputs = iced::runtime::task::into_stream(task).unwrap();
        tokio::time::timeout(Duration::from_secs(10), async {
            while let Some(output) = outputs.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                    Action::FlowReady(_, _, Some(ready)),
                ))) = output
                {
                    return ready;
                }
            }
            panic!("retried reader produces actual geometry");
        })
        .await
        .expect("bounded admitted retry")
    });
    assert!(entry.borrow_mut().complete(&request, Some(output)));
    assert!(entry.borrow().prepared().is_some());
}

#[test]
fn stalled_ui_delivery_holds_output_admission_until_installation_or_discard() {
    use iced::futures::StreamExt;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for install in [false, true] {
        let target = Target {
            card: CardId::new(),
            generation: 0,
        };
        let spans: Arc<[Span<'static, markdown::Uri>]> =
            vec![Span::new("streamed paragraph ".repeat(2000))].into();
        let cache = Cache::default();
        let entry = cache.entry(1, spans);
        entry.borrow_mut().bind_target(target);
        entry
            .borrow_mut()
            .configure(720.0, iced::Pixels(16.0), iced::Font::DEFAULT);
        let request = entry.borrow_mut().request().unwrap();
        let slots = Arc::new(tokio::sync::Semaphore::new(1));
        let task =
            run_with_slots(target, request.clone(), slots.clone()).expect("first worker admitted");
        let (completed_request, output) = executor.block_on(async {
            let mut stream = iced::runtime::task::into_stream(task).unwrap();
            tokio::time::timeout(Duration::from_secs(20), async {
                while let Some(effect) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                        Action::FlowReady(_, request, Some(output)),
                    ))) = effect
                    {
                        return (request, output);
                    }
                }
                panic!("real worker must produce its complete layout");
            })
            .await
            .expect("finite real worker completion")
        });
        // Receiving/queueing the message does not mean the UI consumed it.
        let duplicate_message = output.clone();
        assert!(slots.clone().try_acquire_owned().is_err());
        assert!(
            run_with_slots(target, request, slots.clone()).is_err(),
            "a stalled UI must not accumulate full-context outputs"
        );
        drop(duplicate_message);
        assert!(slots.clone().try_acquire_owned().is_err());
        if install {
            assert!(
                entry
                    .borrow_mut()
                    .complete(&completed_request, Some(output))
            );
            drop(completed_request);
            preparation::retirement_barrier();
            let next = slots
                .clone()
                .try_acquire_owned()
                .expect("installation releases admission while geometry remains resident");
            assert!(entry.borrow().prepared().is_some());
            drop(next);
        } else {
            drop(output);
            drop(completed_request);
            preparation::retirement_barrier();
            let next = slots
                .clone()
                .try_acquire_owned()
                .expect("discard releases admission after expensive retirement");
            assert!(entry.borrow().prepared().is_none());
            drop(next);
        }
    }
}

pub(super) fn find_id(target: Target, id: u64) -> Option<Handle> {
    REQUESTS
        .with(|requests| requests.borrow().get(&id).and_then(std::rc::Weak::upgrade))
        .filter(|entry| entry.borrow().target == Some(target))
}
pub(super) fn evict_find_highlights(except: u64) {
    let oldest = REQUESTS.with(|requests| {
        requests
            .borrow()
            .values()
            .filter_map(std::rc::Weak::upgrade)
            .filter(|entry| {
                let entry = entry.borrow();
                entry.id != except && !entry.find_visible && entry.find_highlight.is_some()
            })
            .min_by_key(|entry| entry.borrow().find_touched)
    });
    if let Some(entry) = oldest {
        entry.borrow_mut().find_highlight = None;
    }
}
impl Entry {
    pub(super) fn find_highlights(&mut self, target: Target) -> find_navigation::Marks {
        let Some(painted) = self.ready.as_ref().map(|ready| ready.plain.clone()) else {
            return Default::default();
        };
        self.find_highlights_for(target, &painted)
    }
    pub(super) fn find_highlights_for(
        &mut self,
        target: Target,
        painted: &Arc<str>,
    ) -> find_navigation::Marks {
        static TOUCH: AtomicU64 = AtomicU64::new(1);
        self.find_touched = TOUCH.fetch_add(1, Ordering::Relaxed);
        if self
            .find_highlight
            .as_ref()
            .is_some_and(|found| found.current(target, painted))
        {
            return self.find_highlight.as_ref().unwrap().marks.clone();
        }
        // A retained widget may still paint an older ready source. It must
        // neither borrow nor evict marks belonging to the newer Entry source.
        if self.find_highlight.as_ref().is_some_and(|found| {
            self.ready
                .as_ref()
                .is_none_or(|ready| !found.current(target, &ready.plain))
        }) {
            self.find_highlight = None;
        }
        find_navigation::Marks::default()
    }
    pub(super) fn needs_find_highlights(&mut self, target: Target) -> bool {
        if !self.find_visible || !find_navigation::has_highlight_context(target) {
            return false;
        }
        if self.find_deferred == Some(find_navigation::available_epoch()) {
            return false;
        }
        let Some(ready) = &self.ready else {
            return false;
        };
        if self
            .find_highlight
            .as_ref()
            .is_some_and(|found| found.current(target, &ready.plain))
        {
            return false;
        }
        !self
            .find_requested
            .as_ref()
            .is_some_and(|(clock, epoch, source, alive)| {
                alive.load(Ordering::Acquire)
                    && clock.load(Ordering::Acquire) == *epoch
                    && std::ptr::eq(source.as_ptr(), Arc::as_ptr(&ready.plain))
            })
    }
    pub(super) fn set_find_visible(&mut self, visible: bool) {
        if self.find_visible != visible {
            self.find_deferred = None;
        }
        self.find_visible = visible;
    }
    pub(super) fn defer_find_highlights_at(&mut self, epoch: u64) {
        self.find_deferred = Some(epoch);
    }
    pub(super) fn find_request_id(&mut self, target: Target) -> Option<u64> {
        self.needs_find_highlights(target).then_some(self.id)
    }
    pub(super) fn request_find_highlights(
        &mut self,
        clock: &Arc<AtomicU64>,
        epoch: u64,
        source: &std::sync::Weak<str>,
        alive: Arc<AtomicBool>,
    ) {
        self.find_requested = Some((clock.clone(), epoch, source.clone(), alive));
    }
    pub(super) fn complete_find_highlights(
        &mut self,
        target: Target,
        attempt: &Arc<AtomicBool>,
        result: Option<find_navigation::Highlight>,
    ) {
        if !self.matches_find_request(attempt) {
            return;
        }
        self.find_requested = None;
        self.find_deferred = None;
        if let Some(result) = result.filter(|found| {
            self.ready
                .as_ref()
                .is_some_and(|ready| found.current(target, &ready.plain))
        }) {
            self.find_highlight = Some(result);
        }
    }
    pub(super) fn matches_find_request(&self, attempt: &Arc<AtomicBool>) -> bool {
        self.find_requested
            .as_ref()
            .is_some_and(|(_, _, _, current)| Arc::ptr_eq(current, attempt))
    }
    pub(super) fn current_find_request(&self, target: Target, attempt: &Arc<AtomicBool>) -> bool {
        self.matches_find_request(attempt)
            && find_navigation::has_highlight_context(target)
            && self
                .find_requested
                .as_ref()
                .is_some_and(|(clock, epoch, source, _)| {
                    clock.load(Ordering::Acquire) == *epoch
                        && self.ready.as_ref().is_some_and(|ready| {
                            std::ptr::eq(source.as_ptr(), Arc::as_ptr(&ready.plain))
                        })
                })
    }
}

#[cfg(test)]
impl Entry {
    pub(super) fn id_for_find_test(&self) -> u64 {
        self.id
    }
    pub(super) fn find_attempt_for_test(&self) -> Option<Arc<AtomicBool>> {
        self.find_requested
            .as_ref()
            .map(|(_, _, _, attempt)| attempt.clone())
    }
}
