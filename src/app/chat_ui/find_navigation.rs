//! One bounded Find intent, from rendered source matching to mounted geometry.
use super::*;
use std::sync::Weak;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct IntentEpoch(u64);
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct DocumentEpoch(u64);
#[derive(Clone)]
pub(super) struct Intent {
    clock: Arc<AtomicU64>,
    epoch: IntentEpoch,
    document: Arc<AtomicU64>,
}
impl Intent {
    pub(super) fn current(&self) -> bool {
        self.clock.load(Ordering::Acquire) == self.epoch.0
    }
    pub(super) fn document_current(&self, epoch: DocumentEpoch) -> bool {
        self.current() && self.document.load(Ordering::Acquire) == epoch.0
    }
    pub(super) fn cancel(&self) {
        let _ = self.clock.compare_exchange(
            self.epoch.0,
            self.epoch.0.wrapping_add(1),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }
}
#[derive(Default)]
pub(super) struct State {
    clock: Arc<AtomicU64>,
    document: Arc<AtomicU64>,
    search: Arc<AtomicU64>,
    pending: Option<Pending>,
    raw: RawState,
}
impl State {
    #[cfg(test)]
    pub(super) fn pending(&self) -> bool {
        self.pending.is_some()
    }
    #[cfg(test)]
    pub(super) fn correction_count(&self) -> Option<u8> {
        self.pending.as_ref().map(|pending| pending.passes)
    }
    #[cfg(test)]
    pub(super) fn current_hit(&self) -> Option<(&str, u64, usize)> {
        let pending = self.pending.as_ref()?;
        if pending.document != Some(self.document_epoch()) {
            return None;
        }
        pending
            .hit
            .as_ref()
            .map(|hit| (hit.item.as_str(), hit.identity, hit.byte))
    }
    #[cfg(test)]
    pub(super) fn diagnostic(&self) -> String {
        self.pending.as_ref().map_or_else(
            || "complete".into(),
            |pending| {
                format!(
                    "key={:?}, hit={:?}, in_flight={}, wait_for_shape={}, passes={}, previous={:?}",
                    pending.key,
                    pending
                        .hit
                        .as_ref()
                        .map(|hit| (&hit.item, hit.identity, hit.byte)),
                    pending.in_flight,
                    pending.wait_for_shape,
                    pending.passes,
                    pending.previous,
                )
            },
        )
    }
    pub(super) fn cancel(&mut self) {
        self.cancel_navigation();
    }
    pub(super) fn cancel_all(&mut self) {
        self.search.fetch_add(1, Ordering::AcqRel);
        self.raw.cancel();
        self.cancel_navigation();
    }
    fn cancel_navigation(&mut self) {
        self.clock.fetch_add(1, Ordering::AcqRel);
        self.pending = None;
    }
    pub(super) fn document_epoch(&self) -> DocumentEpoch {
        DocumentEpoch(self.document.load(Ordering::Acquire))
    }
    pub(super) fn changed_document(&self) {
        self.document.fetch_add(1, Ordering::AcqRel);
    }
    pub(super) fn guard(&self) -> Intent {
        Intent {
            clock: self.clock.clone(),
            document: self.document.clone(),
            epoch: IntentEpoch(self.clock.load(Ordering::Acquire)),
        }
    }
}
impl Drop for State {
    fn drop(&mut self) {
        self.cancel_all();
    }
}
struct Pending {
    intent: Intent,
    context: Option<String>,
    query: String,
    key: Option<String>,
    sources: Vec<(String, Weak<chat::Item>)>,
    document: Option<DocumentEpoch>,
    hit: Option<Hit>,
    in_flight: bool,
    worker_alive: Option<Arc<AtomicBool>>,
    wait_for_shape: bool,
    passes: u8,
    previous: Option<(u8, f32)>,
    realized_pending: Option<(flow_layout::PendingGeometry, Hit)>,
    _deadline: iced::task::Handle,
}
#[derive(Clone, PartialEq, Eq)]
pub(super) struct Hit {
    item: String,
    identity: u64,
    byte: usize,
}
fn items<'a>(runtime: &'a Runtime, key: &str) -> Vec<&'a Arc<chat::Item>> {
    if runtime.selected_subagent.is_some() {
        return runtime.active_item(key).into_iter().collect();
    }
    runtime
        .turns
        .iter()
        .find(|turn| turn.key == key)
        .into_iter()
        .flat_map(|turn| {
            turn.prompt.iter().copied().chain(
                turn.steps
                    .iter()
                    .chain(&turn.prose)
                    .map(|placed| placed.index),
            )
        })
        .filter_map(|index| runtime.snapshot.items.get(index))
        .collect()
}
fn sources_match(runtime: &Runtime, pending: &Pending) -> bool {
    pending.sources.iter().all(|(id, source)| {
        runtime
            .active_item(id)
            .is_some_and(|item| source.as_ptr() == Arc::as_ptr(item))
    })
}
fn document(runtime: &Runtime) -> (DocumentEpoch, Arc<selection::Catalog>) {
    let (catalog, _) = runtime.selection_document();
    let epoch = runtime.find_navigation.document_epoch();
    (epoch, catalog)
}
fn later(target: Target, intent: Intent, geometry_changed: bool) -> Task<Message> {
    cosmic::task::future(async move {
        tokio::time::sleep(Duration::from_millis(16)).await;
        action(Action::FindResume(target, intent, geometry_changed))
    })
}
impl Cagents {
    pub(super) fn begin_find_navigation(
        &mut self,
        target: Target,
        key: Option<String>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        runtime.find_navigation.cancel_navigation();
        let intent = runtime.find_navigation.guard();
        let expiry = intent.clone();
        let (deadline, handle) = cosmic::task::future(async move {
            tokio::time::sleep(Duration::from_secs(30)).await;
            action(Action::FindExpired(target, expiry))
        })
        .abortable();
        let anchor = key.as_ref().map(|key| runtime.find_plan_anchor(key));
        runtime.find_navigation.pending = Some(Pending {
            intent: intent.clone(),
            context: runtime.selected_subagent.clone(),
            query: runtime.find.clone().unwrap_or_default(),
            key,
            sources: Vec::new(),
            document: None,
            hit: None,
            in_flight: false,
            worker_alive: None,
            wait_for_shape: false,
            passes: u8::from(anchor.is_some()),
            previous: None,
            realized_pending: None,
            _deadline: handle.abort_on_drop(),
        });
        Task::batch([
            anchor.map_or_else(Task::none, |anchor| {
                scroll::find_anchor(target, anchor, intent.clone()).map(cosmic::Action::App)
            }),
            later(target, intent, false),
            deadline,
        ])
    }
    pub(super) fn resume_find_navigation(
        &mut self,
        target: Target,
        intent: Intent,
        geometry_changed: bool,
    ) -> Task<Message> {
        if let Some(task) = self.start_raw_find(target) {
            return task;
        }
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(mut pending) = runtime.find_navigation.pending.take() else {
            return Task::none();
        };
        if pending.intent.epoch != intent.epoch {
            runtime.find_navigation.pending = Some(pending);
            return Task::none();
        }
        if !intent.current()
            || !runtime.visible.load(Ordering::Relaxed)
            || runtime.selected_subagent != pending.context
            || runtime.find.as_ref() != Some(&pending.query)
        {
            runtime.find_navigation.cancel();
            return Task::none();
        }
        if pending
            .key
            .as_ref()
            .is_some_and(|key| !runtime.find_matches.contains(key))
        {
            pending.key = None;
            pending.hit = None;
            pending.document = None;
        }
        let Some(key) = &pending.key else {
            if runtime.find_matches.is_empty() {
                runtime.find_navigation.pending = Some(pending);
                return Task::none();
            }
            runtime.find_index = (runtime.find_index + 1) % runtime.find_matches.len();
            let key = runtime.find_matches[runtime.find_index].clone();
            runtime.apply_find_plan(&key);
            let anchor = runtime.find_plan_anchor(&key);
            pending.key = Some(key);
            if pending.passes >= 16 {
                runtime.find_navigation.cancel_navigation();
                return Task::none();
            }
            pending.passes += 1;
            runtime.find_navigation.pending = Some(pending);
            return Task::batch([
                scroll::find_anchor(target, anchor, intent.clone()).map(cosmic::Action::App),
                later(target, intent, false),
            ]);
        };
        let current = items(runtime, key);
        if current.is_empty() {
            runtime.find_navigation.cancel();
            return Task::none();
        }
        if pending.sources.is_empty() || !sources_match(runtime, &pending) {
            pending.sources = current
                .iter()
                .map(|item| (item.id.clone(), Arc::downgrade(item)))
                .collect();
            pending.hit = None;
            pending.document = None;
            pending.in_flight = false;
            pending.wait_for_shape = false;
        }
        if pending
            .worker_alive
            .as_ref()
            .is_some_and(|alive| !alive.load(Ordering::Acquire))
        {
            // A finished worker may still own a queued Ready and the sole
            // slot. Preserve its token until consumption or new admission.
            pending.in_flight = false;
        }
        if pending.in_flight || (pending.wait_for_shape && !geometry_changed) {
            runtime.find_navigation.pending = Some(pending);
            return Task::none();
        }
        // Same-generation hydration and delayed parser completions are real
        // source changes. Wait for the exact current immutable item, not a
        // useful but older streaming prefix or a Loading caption.
        if current.iter().any(|item| {
            (item.role == chat::Role::User
                && matches!(&item.content, ItemContent::Text { .. })
                && runtime.prepared_prompt(item).is_none())
                || runtime
                    .presentation_cache()
                    .get(&item.id)
                    .is_none_or(|cached| {
                        cached.placeholder
                            || cached
                                .item
                                .as_ref()
                                .is_none_or(|shown| !Arc::ptr_eq(shown, item))
                    })
        }) {
            runtime.find_navigation.pending = Some(pending);
            return Task::none();
        }
        let epoch = runtime.find_navigation.document_epoch();
        if pending.document != Some(epoch) {
            pending.hit = None;
            pending.document = Some(epoch);
            pending.previous = None;
        }
        pending.wait_for_shape = false;
        let task = if let Some(hit) = &pending.hit {
            pending.worker_alive = None;
            pending.in_flight = true;
            let intent = intent.clone();
            scroll::locate_logical(target, hit.item.clone(), hit.identity, hit.byte).map(
                move |location| {
                    cosmic::Action::App(action(Action::FindLocated(
                        target,
                        intent.clone(),
                        epoch,
                        location,
                    )))
                },
            )
        } else {
            // No waiting worker queue and no source capture before admission:
            // rapid queries cannot accumulate whole-context Arc catalogs.
            let Some(permit) = admit() else {
                runtime.find_navigation.pending = Some(pending);
                return later(target, intent, false);
            };
            let ids = current.iter().map(|item| item.id.clone()).collect();
            // Building a cold selection catalog can bind a new document epoch.
            // Certify the captured catalog after that binding, under admission.
            let (epoch, catalog) = document(runtime);
            pending.document = Some(epoch);
            let work = Work::new(Input::Catalog(catalog.owned_search_blocks(&ids)), permit);
            let attempt = work.alive();
            pending.worker_alive = Some(attempt.clone());
            let plan = runtime
                .find_navigation
                .raw
                .accepted
                .as_ref()
                .expect("accepted group plan")
                .clone();
            let proof = intent.clone();
            pending.in_flight = true;
            cosmic::task::future(async move {
                let hit = tokio::task::spawn_blocking(move || {
                    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                        let Input::Catalog(blocks) = work.input() else {
                            unreachable!()
                        };
                        for (item, identity, text) in blocks {
                            let found =
                                find::matched_pattern_checked(text, &plan.pattern, 1, &|| {
                                    proof.document_current(epoch)
                                })
                                .ok_or(RawFailure::Canceled)?;
                            if let Some(range) = found.ranges.first() {
                                return Ok(Some(Hit {
                                    item: item.to_string(),
                                    identity: *identity,
                                    byte: range.start,
                                }));
                            }
                        }
                        Ok(None)
                    }))
                    .unwrap_or(Err(RawFailure::Worker));
                    work.finish(outcome)
                })
                .await
                .ok();
                action(Action::FindIndexed(target, intent, epoch, attempt, hit))
            })
        };
        runtime.find_navigation.pending = Some(pending);
        task
    }
    pub(super) fn resume_current_find(
        &mut self,
        target: Target,
        geometry_changed: bool,
    ) -> Task<Message> {
        if let Some(task) = self.start_raw_find(target) {
            return task;
        }
        let intent = self.chat_runtime_mut(target).and_then(|runtime| {
            runtime
                .find_navigation
                .pending
                .as_ref()
                .map(|pending| pending.intent.clone())
        });
        intent.map_or_else(Task::none, |intent| {
            if geometry_changed {
                later(target, intent, true)
            } else {
                self.resume_find_navigation(target, intent, false)
            }
        })
    }
    pub(super) fn find_indexed(
        &mut self,
        target: Target,
        intent: Intent,
        epoch: DocumentEpoch,
        attempt: Arc<AtomicBool>,
        hit: Option<CatalogResult>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let current = runtime
            .find_navigation
            .pending
            .as_ref()
            .is_some_and(|pending| {
                pending.intent.epoch == intent.epoch
                    && pending
                        .worker_alive
                        .as_ref()
                        .is_some_and(|current| Arc::ptr_eq(current, &attempt))
            });
        if !current {
            return self.resume_current_find(target, true);
        }
        if !intent.current() || !runtime.visible.load(Ordering::Relaxed) {
            runtime.find_navigation.cancel();
            return Task::none();
        }
        let (now, _) = document(runtime);
        let Some(pending) = runtime.find_navigation.pending.as_mut() else {
            return Task::none();
        };
        pending.in_flight = false;
        pending.worker_alive = None;
        if epoch == now && intent.current() {
            match hit.and_then(Ready::take).unwrap_or(Err(RawFailure::Worker)) {
                Ok(hit) => pending.hit = hit,
                Err(RawFailure::Canceled) => {
                    pending.hit = None;
                    pending.document = None;
                    return self.resume_current_find(target, true);
                }
                Err(_) => {
                    runtime.fail_find(RawFailure::Worker);
                    return Task::none();
                }
            }
            if pending.hit.is_none() {
                // Matching metadata has no rendered plaintext byte. Keep the
                // honest coarse anchor behavior without inventing glyph proof.
                runtime.find_navigation.pending = None;
                return Task::none();
            }
        } else {
            pending.hit = None;
            pending.document = None;
        }
        self.resume_find_navigation(target, intent, true)
    }
    pub(super) fn find_located(
        &mut self,
        target: Target,
        intent: Intent,
        epoch: DocumentEpoch,
        location: Option<scroll::LogicalLocation>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        let Some(mut pending) = runtime.find_navigation.pending.take() else {
            return Task::none();
        };
        if pending.intent.epoch != intent.epoch {
            runtime.find_navigation.pending = Some(pending);
            return Task::none();
        }
        if !intent.current()
            || !runtime.visible.load(Ordering::Relaxed)
            || runtime.selected_subagent != pending.context
            || runtime.find.as_ref() != Some(&pending.query)
        {
            runtime.find_navigation.cancel();
            return Task::none();
        }
        let (now, _) = document(runtime);
        pending.in_flight = false;
        if epoch != now || !sources_match(runtime, &pending) {
            pending.hit = None;
            pending.document = None;
            pending.wait_for_shape = false;
            runtime.find_navigation.pending = Some(pending);
            return self.resume_find_navigation(target, intent, true);
        }
        let Some(location) = location else {
            runtime.find_navigation.pending = Some(pending);
            return later(target, intent, false);
        };
        let same = pending.previous.is_some_and(|(stage, offset)| {
            stage == location.stage && (offset - location.offset).abs() <= 1.0
        });
        pending.previous = Some((location.stage, location.offset));
        // A single large paragraph has no logical row estimate until its
        // admitted full-context geometry arrives. Repeating that unchanged
        // coarse correction must not exhaust Find while the real source is
        // still shaping, even if unrelated/stale completions wake this card.
        if let Some(geometry) = location.shape_pending
            && pending
                .realized_pending
                .as_ref()
                .is_some_and(|(owner, hit)| *owner == geometry && Some(hit) == pending.hit.as_ref())
        {
            pending.wait_for_shape = true;
            runtime.find_navigation.pending = Some(pending);
            return Task::none();
        }
        if pending.passes >= 16 {
            runtime.find_navigation.cancel();
            return Task::none();
        }
        pending.passes += 1;
        if let Some(geometry) = location.shape_pending {
            // Verified appends retain the body-free geometry owner. Its one
            // initial realization survives document publications while current
            // shaping is pending; rewritten sources get another owner.
            pending.realized_pending = pending.hit.clone().map(|hit| (geometry, hit));
        }
        let scroll =
            scroll::apply_find_location(target, location.clone(), intent.clone(), Some(epoch))
                .map(cosmic::Action::App);
        if location.exact {
            return scroll;
        } // Guard remains valid until any user navigation.
        pending.wait_for_shape = location.shape_pending.is_some() || (location.stage >= 2 && same);
        let retry = if pending.wait_for_shape {
            Task::none()
        } else {
            later(target, intent, false)
        };
        runtime.find_navigation.pending = Some(pending);
        Task::batch([scroll, retry])
    }
    pub(super) fn expire_find_navigation(&mut self, target: Target, intent: Intent) {
        if let Some(runtime) = self.chat_runtime_mut(target)
            && runtime
                .find_navigation
                .pending
                .as_ref()
                .is_some_and(|pending| pending.intent.epoch == intent.epoch)
        {
            runtime.find_navigation.cancel();
        }
    }
}

// All Find phases share this one try-admitted executor. A canceled generation
// cannot release its slot while its raw final owners still await retirement.
struct Slot(Option<tokio::sync::OwnedSemaphorePermit>);
impl Drop for Slot {
    fn drop(&mut self) {
        drop(self.0.take());
        available().send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }
}
fn available() -> &'static tokio::sync::watch::Sender<u64> {
    static AVAILABLE: std::sync::OnceLock<tokio::sync::watch::Sender<u64>> =
        std::sync::OnceLock::new();
    AVAILABLE.get_or_init(|| tokio::sync::watch::channel(0).0)
}
pub(super) fn available_epoch() -> u64 {
    *available().borrow()
}
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
            Some((action(Action::FindAvailable), (receiver, false)))
        },
    )
}
fn admit() -> Option<Slot> {
    static WORKER: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    WORKER
        .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(1)))
        .clone()
        .try_acquire_owned()
        .ok()
        .map(|permit| Slot(Some(permit)))
}
enum Input {
    Raw(RawInput),
    Catalog(Vec<(Arc<str>, u64, Arc<str>)>),
    Highlight(Arc<str>),
    Output(Box<dyn Send + Sync>),
}
pub(super) struct RetiredWork {
    input: Input,
    // Field order is authority: raw allocations die before the SAME sole slot.
    _permit: Option<Slot>,
}
struct WorkOwner(Option<RetiredWork>, Arc<AtomicBool>);
impl Drop for WorkOwner {
    fn drop(&mut self) {
        self.1.store(false, Ordering::Release);
        if let Some(work) = self.0.take() {
            preparation::retire_find(work);
        }
    }
}
struct Work(Arc<WorkOwner>);
impl Work {
    fn new(input: Input, permit: Slot) -> Self {
        Self(Arc::new(WorkOwner(
            Some(RetiredWork {
                input,
                _permit: Some(permit),
            }),
            Arc::new(AtomicBool::new(true)),
        )))
    }
    fn alive(&self) -> Arc<AtomicBool> {
        self.0.1.clone()
    }
    fn input(&self) -> &Input {
        &self.0.0.as_ref().expect("live Find input").input
    }
    fn finish<T: Send + Sync + 'static>(self, value: T) -> Ready<T> {
        let mut owner = Arc::try_unwrap(self.0).ok().expect("one Find worker owner");
        let RetiredWork { input, _permit } = owner.0.take().expect("live Find work");
        drop(input);
        Ready(Arc::new(Mutex::new(Some(Admitted {
            value: Some(value),
            _permit,
        }))))
    }
}
struct SourceProof<T: ?Sized>(Weak<T>);
impl<T: ?Sized> Clone for SourceProof<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T: ?Sized> PartialEq for SourceProof<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0.ptr_eq(&other.0)
    }
}
impl<T: ?Sized> Eq for SourceProof<T> {}
#[derive(Clone, PartialEq, Eq)]
struct RawStamp {
    context: Option<String>,
    provider: Provider,
    handle: Option<SourceProof<ClientHandle>>,
    session: Option<String>,
    query: (u64, usize, usize),
    groups: Vec<(
        String,
        Vec<(SourceProof<chat::Item>, Vec<(u64, SourceProof<str>)>)>,
    )>,
}
#[derive(Default)]
struct RawState {
    clock: Arc<AtomicU64>,
    stamp: Option<RawStamp>,
    in_flight: Option<Arc<AtomicBool>>,
    failed: Option<RawFailure>,
    run: u64,
    complete: bool,
    deferred: Option<u64>,
    accepted: Option<RawOutput>,
    highlight_context: Option<Arc<HighlightContext>>,
}
impl RawState {
    fn cancel(&mut self) {
        self.clock.fetch_add(1, Ordering::AcqRel);
        self.in_flight = None;
        self.failed = None;
        self.complete = false;
        self.deferred = None;
        self.stamp = None;
        self.accepted = None;
        self.highlight_context = None;
    }
}
struct RawInput {
    query: String,
    groups: Vec<(String, Vec<RawItem>)>,
    provider: Provider,
}
struct RawItem {
    item: Arc<chat::Item>,
    raw_disclosure: bool,
    tools: Vec<(u64, Arc<str>)>,
}
#[derive(Clone)]
struct ItemPlan {
    id: String,
    source: Weak<chat::Item>,
    expand: bool,
    parts: Vec<String>,
}
pub(super) struct Ready<T: Send + Sync + 'static>(Arc<Mutex<Option<Admitted<T>>>>);
struct Admitted<T: Send + Sync + 'static> {
    value: Option<T>,
    _permit: Option<Slot>,
}
impl<T: Send + Sync + 'static> Drop for Admitted<T> {
    fn drop(&mut self) {
        if let Some(value) = self.value.take() {
            retire_value(value, self._permit.take());
        }
    }
}
fn retire_value<T: Send + Sync + 'static>(value: T, permit: Option<Slot>) {
    let work = RetiredWork {
        input: Input::Output(Box::new(value)),
        _permit: permit,
    };
    if preparation::reclaiming_find() {
        drop(work)
    } else {
        preparation::retire_find(work)
    }
}
impl<T: Send + Sync + 'static> Clone for Ready<T> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}
impl<T: Send + Sync + 'static> Ready<T> {
    fn take(self) -> Option<T> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .and_then(|mut ready| ready.value.take())
    }
}
pub(super) type RawResult = Ready<Result<RawOutput, RawFailure>>;
pub(super) type CatalogResult = Ready<Result<Option<Hit>, RawFailure>>;
#[derive(Clone)]
pub(super) struct RawOutput(Arc<RawOwner>);
struct RawOwner(Option<RawData>);
impl Drop for RawOwner {
    fn drop(&mut self) {
        if let Some(data) = self.0.take() {
            retire_value(data, None)
        }
    }
}
impl std::ops::Deref for RawOutput {
    type Target = RawData;
    fn deref(&self) -> &RawData {
        self.0.0.as_ref().expect("live Find plan")
    }
}
pub(super) struct RawData {
    epoch: u64,
    query_epoch: u64,
    pattern: Arc<find::Pattern>,
    context: Option<String>,
    groups: Vec<(String, Arc<[ItemPlan]>)>,
    tools: Vec<(u64, Weak<str>, String, Weak<chat::Item>)>,
    _metadata: MetadataBudget,
}
fn raw_groups(runtime: &Runtime) -> Vec<(String, Vec<(usize, bool)>)> {
    if let Some(child) = &runtime.selected_subagent {
        return runtime
            .snapshot
            .subagents
            .iter()
            .find(|agent| &agent.id == child)
            .into_iter()
            .flat_map(|agent| agent.items.iter().enumerate())
            .map(|(index, item)| {
                (
                    item.id.clone(),
                    vec![(
                        index,
                        item.content.is_step()
                            || turn::slot_of(item) == turn::Slot::Thinking
                            || item.role == chat::Role::User,
                    )],
                )
            })
            .collect();
    }
    runtime
        .turns
        .iter()
        .map(|turn| {
            (
                turn.key.clone(),
                turn.prompt
                    .iter()
                    .map(|index| (*index, true))
                    .chain(turn.steps.iter().map(|placed| (placed.index, true)))
                    .chain(
                        turn.prose
                            .iter()
                            .map(|placed| (placed.index, placed.slot == turn::Slot::Thinking)),
                    )
                    .collect(),
            )
        })
        .collect()
}
fn raw_items(runtime: &Runtime) -> &[Arc<chat::Item>] {
    runtime
        .selected_subagent
        .as_ref()
        .and_then(|child| {
            runtime
                .snapshot
                .subagents
                .iter()
                .find(|agent| &agent.id == child)
        })
        .map_or(&runtime.snapshot.items, |agent| &agent.items)
}
fn tools(cached: Option<&CachedItem>) -> impl Iterator<Item = &tool_text::ToolText> {
    cached.into_iter().flat_map(|cached| {
        cached
            .tool_input
            .iter()
            .chain(cached.tool_output.iter())
            .chain(cached.patches.iter())
    })
}
fn notice_reader(cached: Option<&CachedItem>) -> Option<(u64, &markdown::Text)> {
    let cached = cached?;
    let notice = cached.literal.as_ref()?.notice()?;
    let item = cached.item.as_ref()?;
    Some((
        selection::logical_identity(&item.id, &[2]),
        &notice.body.full,
    ))
}
fn reader_proofs(cached: Option<&CachedItem>) -> Vec<(u64, SourceProof<str>)> {
    let mut proofs: Vec<_> = tools(cached)
        .map(|tool| {
            (
                tool.find_identity(),
                SourceProof(Arc::downgrade(&tool.text)),
            )
        })
        .collect();
    if let Some((identity, text)) = notice_reader(cached)
        .and_then(|(id, text)| text.prepared_plain_proof().map(|source| (id, source)))
    {
        proofs.push((identity, SourceProof(text)));
    }
    proofs
}
fn raw_stamp(runtime: &Runtime) -> RawStamp {
    let items = raw_items(runtime);
    RawStamp {
        context: runtime.selected_subagent.clone(),
        provider: runtime.session.launch.provider,
        handle: runtime
            .handle()
            .map(|handle| SourceProof(Arc::downgrade(handle))),
        session: runtime.snapshot.session_id.clone(),
        query: runtime.find.as_ref().map_or(
            (runtime.find_navigation.search.load(Ordering::Acquire), 0, 0),
            |query| {
                (
                    runtime.find_navigation.search.load(Ordering::Acquire),
                    query.as_ptr() as usize,
                    query.len(),
                )
            },
        ),
        groups: raw_groups(runtime)
            .into_iter()
            .map(|(key, group)| {
                (
                    key,
                    group
                        .into_iter()
                        .filter_map(|(index, _)| items.get(index))
                        .map(|item| {
                            let cached =
                                runtime.presentation_cache().get(&item.id).filter(|cached| {
                                    cached
                                        .item
                                        .as_ref()
                                        .is_some_and(|shown| Arc::ptr_eq(shown, item))
                                });
                            (SourceProof(Arc::downgrade(item)), reader_proofs(cached))
                        })
                        .collect(),
                )
            })
            .collect(),
    }
}
impl Runtime {
    pub(super) fn refresh_find_work(&mut self) {
        if self.find.as_ref().is_none_or(|query| query.is_empty()) {
            if self.find_navigation.raw.stamp.is_some()
                || self.find_navigation.raw.accepted.is_some()
            {
                self.find_navigation.raw.cancel();
                register_highlights(self);
            }
            self.find_matches.clear();
            self.find_index = 0;
            self.find_navigation.cancel();
            return;
        }
        let mut stamp = raw_stamp(self);
        if self.find_navigation.raw.stamp.as_ref() == Some(&stamp) {
            return;
        }
        let same_scope = self.find_navigation.raw.stamp.as_ref().is_some_and(|old| {
            old.context == stamp.context
                && old.provider == stamp.provider
                && old.handle == stamp.handle
                && old.session == stamp.session
                && old.query == stamp.query
        });
        let previous = self.find_matches.get(self.find_index).cloned();
        if same_scope {
            // A source publication coalesces behind the one running query.
            // Stable item proofs remain usable while changed groups are pending.
            self.find_navigation
                .raw
                .clock
                .fetch_add(1, Ordering::AcqRel);
            self.find_navigation.raw.complete = false;
            self.find_navigation.raw.failed = None;
            self.find_navigation.raw.deferred = None;
        } else {
            self.find_navigation.search.fetch_add(1, Ordering::AcqRel);
            stamp.query.0 = self.find_navigation.search.load(Ordering::Acquire);
            self.find_navigation.raw.cancel();
        }
        self.find_navigation.raw.stamp = Some(stamp);
        self.find_matches = current_groups(self);
        self.find_index = previous
            .as_ref()
            .and_then(|key| {
                self.find_matches
                    .iter()
                    .position(|candidate| candidate == key)
            })
            .unwrap_or_else(|| self.find_matches.len().saturating_sub(1));
        register_highlights(self);
        if small_raw_work(self) {
            let input = capture_raw(self);
            let query = self.find.as_deref().unwrap_or_default();
            if !query.is_empty()
                && let Ok((groups, tools, metadata, pattern)) = raw_scan(&input, query, &|| true)
            {
                let result = RawOutput(Arc::new(RawOwner(Some(RawData {
                    epoch: self.find_navigation.raw.clock.load(Ordering::Acquire),
                    query_epoch: self.find_navigation.search.load(Ordering::Acquire),
                    pattern,
                    context: self.selected_subagent.clone(),
                    groups,
                    tools,
                    _metadata: metadata,
                }))));
                self.find_matches = result.groups.iter().map(|(key, _)| key.clone()).collect();
                self.find_index = previous
                    .as_ref()
                    .and_then(|key| {
                        self.find_matches
                            .iter()
                            .position(|candidate| candidate == key)
                    })
                    .unwrap_or_else(|| {
                        self.find_index
                            .min(self.find_matches.len().saturating_sub(1))
                    });
                self.find_navigation.raw.accepted = Some(result);
                self.find_navigation.raw.complete = true;
                register_highlights(self);
            }
        }
    }
    // The raw worker has already matched these exact immutable sources. Use
    // its plans to choose the typed diagnostic's actual mounted anchor.
    fn find_plan_anchor(&mut self, key: &str) -> iced::advanced::widget::Id {
        let plans = self
            .find_navigation
            .raw
            .accepted
            .as_ref()
            .and_then(|result| result.groups.iter().find(|(group, _)| group == key))
            .map(|(_, plans)| plans.clone())
            .unwrap_or_default();
        for plan in plans.iter() {
            if !self
                .active_item(&plan.id)
                .is_some_and(|item| plan.source.as_ptr() == Arc::as_ptr(item))
            {
                continue;
            }
            if self.selected_subagent.is_some() {
                if let Some(anchor) = self.reveal_failure_for_find(&plan.id) {
                    return self.find_anchor(&anchor);
                }
            } else if let Some(incident) = self.failure_ui.item(&plan.id) {
                return self.find_anchor(&format!("failure:{}", incident.key));
            }
        }
        self.find_anchor(key)
    }
    pub(super) fn apply_find_plan(&mut self, key: &str) {
        let plans = self
            .find_navigation
            .raw
            .accepted
            .as_ref()
            .and_then(|result| result.groups.iter().find(|(group, _)| group == key))
            .map(|(_, plans)| plans.clone())
            .unwrap_or_default();
        let child = self.selected_subagent.clone();
        if child.is_none() {
            self.find_disclosure.turn(key, &mut self.opened);
        }
        let mut prepare_reports = false;
        for plan in plans.iter() {
            if !plan_in_group(self, key, plan) {
                continue;
            }
            let Some(item) = self
                .active_item(&plan.id)
                .cloned()
                .filter(|item| plan.source.as_ptr() == Arc::as_ptr(item))
            else {
                continue;
            };
            if self.reveal_failure_for_find(&plan.id).is_some() {
                continue;
            }
            let changed = if let Some(child) = &child {
                let disclosure = self.child_find_disclosure.entry(child.clone()).or_default();
                for part in &plan.parts {
                    disclosure.part(part, self.child_unfolded.entry(child.clone()).or_default());
                }
                plan.expand
                    && disclosure.item(
                        &plan.id,
                        self.child_expanded.entry(child.clone()).or_default(),
                    )
            } else {
                for part in &plan.parts {
                    self.find_disclosure.part(part, &mut self.unfolded);
                }
                plan.expand && self.find_disclosure.item(&plan.id, &mut self.expanded)
            };
            if changed
                && matches!(
                    &item.content,
                    ItemContent::Delivery {
                        kind: chat::DeliveryKind::Report,
                        ..
                    }
                )
            {
                prepare_reports = true;
                continue;
            }
            if changed
                && !self
                    .presentation_cache()
                    .get(&plan.id)
                    .is_some_and(|cached| cached.preparing)
                && (CachedItem::prose(&item.content).is_none()
                    || self
                        .presentation_cache()
                        .get(&plan.id)
                        .is_some_and(|cached| cached.coordination.is_some()))
            {
                let theme = self.syntax_theme;
                let cache = self.presentation_cache_mut();
                let previous = cache.remove(&plan.id);
                cache.insert(
                    plan.id.clone(),
                    CachedItem::updated(&item.content, theme, previous, true, false, None),
                );
            }
        }
        if prepare_reports {
            let snapshot = self.snapshot.clone();
            self.prepare_transcript_cache(&snapshot);
        }
    }
}
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum RawFailure {
    Canceled,
    TooLarge,
    Pressure(u64),
    Worker,
}
type Scan = (
    Vec<(String, Arc<[ItemPlan]>)>,
    Vec<(u64, Weak<str>, String, Weak<chat::Item>)>,
    MetadataBudget,
    Arc<find::Pattern>,
);
fn raw_scan(
    input: &RawInput,
    query: &str,
    current: &impl Fn() -> bool,
) -> Result<Scan, RawFailure> {
    let failure = std::cell::Cell::new(RawFailure::Canceled);
    raw_scan_inner(input, query, current, &failure).ok_or(failure.get())
}
fn raw_scan_inner(
    input: &RawInput,
    query: &str,
    current: &impl Fn() -> bool,
    failure: &std::cell::Cell<RawFailure>,
) -> Option<(
    Vec<(String, Arc<[ItemPlan]>)>,
    Vec<(u64, Weak<str>, String, Weak<chat::Item>)>,
    MetadataBudget,
    Arc<find::Pattern>,
)> {
    const MAX: usize = 8 * 1024 * 1024;
    let pattern = Arc::new(find::Pattern::prepare(query, current)?);
    let mut cost = pattern.bytes().checked_mul(2)?;
    if cost > MAX {
        failure.set(RawFailure::TooLarge);
        return None;
    }
    let mut charge = |bytes: usize| {
        failure.set(RawFailure::TooLarge);
        cost = cost.checked_add(bytes.checked_mul(4)?)?;
        if cost > MAX {
            return None;
        }
        failure.set(RawFailure::Canceled);
        Some(())
    };
    let mut groups = Vec::new();
    let mut uncap = Vec::new();
    for (key, items) in &input.groups {
        charge(key.len().checked_add(128)?)?;
        let mut plans = Vec::new();
        for raw in items {
            if !current() {
                return None;
            }
            let item = &raw.item;
            let matched = contains_checked(&item.content, &pattern, current)?;
            let coordinated = chat::coordination::recognize(item, input.provider);
            if !current() {
                return None;
            }
            charge(item.id.len().checked_add(128)?)?;
            let mut parts = Vec::new();
            if let Some(reading) = &coordinated {
                use chat::coordination::Kind;
                let mut texts = Vec::new();
                match &reading.kind {
                    Kind::Message(message) => {
                        texts.extend(message.body.inline().map(|text| ("body".to_owned(), text)))
                    }
                    Kind::Spawn(spawn) => {
                        texts.push(("brief".into(), spawn.brief.as_str()));
                        texts.extend(spawn.agents.iter().enumerate().filter_map(
                            |(index, agent)| {
                                Some((format!("prompt-{index}"), agent.prompt.inline()?))
                            },
                        ));
                    }
                    Kind::Act(_) => {}
                }
                texts.extend(
                    reading
                        .outcome
                        .messages
                        .iter()
                        .enumerate()
                        .map(|(index, message)| (format!("answer-{index}"), message.body.as_str())),
                );
                texts.extend(
                    reading
                        .outcome
                        .run
                        .as_ref()
                        .map(|run| ("result".into(), run.result.as_str())),
                );
                for (part, text) in texts {
                    if matches_checked(text, &pattern, current)? {
                        charge(item.id.len().checked_add(part.len())?.checked_add(128)?)?;
                        parts.push(format!("{}#{part}", item.id));
                    }
                }
            }
            for (identity, text) in &raw.tools {
                if matches_checked(text, &pattern, current)? {
                    charge(item.id.len().checked_add(128)?)?;
                    uncap.push((
                        *identity,
                        Arc::downgrade(text),
                        item.id.clone(),
                        Arc::downgrade(item),
                    ));
                }
            }
            let raw_disclosure = !matches!(&item.content, ItemContent::Delivery { .. })
                && coordinated.as_ref().map_or(raw.raw_disclosure, |reading| {
                    if reading.is_card() {
                        parts.is_empty()
                    } else {
                        raw.raw_disclosure
                    }
                });
            let delivery = match &item.content {
                ItemContent::Delivery {
                    kind: chat::DeliveryKind::Report,
                    ..
                } => matched,
                ItemContent::Delivery { body, .. } => matches_checked(body, &pattern, current)?,
                _ => false,
            };
            if matched || !parts.is_empty() {
                plans.push(ItemPlan {
                    id: item.id.clone(),
                    source: Arc::downgrade(item),
                    expand: (raw_disclosure && matched) || delivery,
                    parts,
                });
            }
        }
        if !plans.is_empty() {
            groups.push((key.clone(), plans.into()));
        }
    }
    let attempt = metadata_epoch();
    let metadata = metadata_capacity()
        .clone()
        .try_acquire_many_owned(cost.max(1) as u32)
        .ok()
        .or_else(|| {
            failure.set(RawFailure::Pressure(attempt));
            None
        })?;
    Some((groups, uncap, MetadataBudget(Some(metadata)), pattern))
}
fn matches_checked(
    text: &str,
    pattern: &find::Pattern,
    current: &impl Fn() -> bool,
) -> Option<bool> {
    find::matched_pattern_checked(text, pattern, 0, current).map(|result| result.truncated)
}
fn contains_checked(
    content: &ItemContent,
    pattern: &find::Pattern,
    current: &impl Fn() -> bool,
) -> Option<bool> {
    let mut texts: Vec<&str> = Vec::new();
    match content {
        ItemContent::Text { text, .. } => texts.push(text),
        ItemContent::Command {
            command,
            output,
            label,
            working_directory,
            ..
        } => texts.extend([
            command.as_str(),
            output.as_str(),
            label.as_str(),
            working_directory.as_str(),
        ]),
        ItemContent::Tool {
            name,
            input,
            output,
            ..
        } => texts.extend([name.as_str(), input.as_str(), output.as_str()]),
        ItemContent::Changes { changes, .. } => {
            for change in changes {
                texts.extend([change.path.as_str(), change.diff.as_str()]);
                if let chat::ChangeKind::Renamed { from } = &change.kind {
                    texts.push(from)
                }
            }
        }
        ItemContent::WebSearch { query, results } => {
            texts.push(query);
            for result in results {
                texts.extend([
                    result.title.as_str(),
                    result.url.as_str(),
                    result.snippet.as_str(),
                ]);
            }
        }
        ItemContent::Image {
            path, description, ..
        } => texts.extend([path.as_str(), description.as_str()]),
        ItemContent::Delivery {
            from,
            sender,
            summary,
            body,
            ..
        } => texts.extend([
            from.as_str(),
            sender.as_str(),
            summary.as_str(),
            body.as_str(),
        ]),
    }
    for text in texts {
        if matches_checked(text, pattern, current)? {
            return Some(true);
        }
    }
    Some(false)
}
impl Cagents {
    fn start_raw_find(&mut self, target: Target) -> Option<Task<Message>> {
        let runtime = self.chat_runtime_mut(target)?;
        runtime.refresh_find_work();
        if runtime.find.as_ref().is_none_or(|query| query.is_empty())
            || !runtime.visible.load(Ordering::Relaxed)
        {
            return Some(Task::none());
        }
        let raw = &runtime.find_navigation.raw;
        if raw.complete {
            return None;
        }
        if raw.accepted.is_some()
            && !runtime.find_matches.is_empty()
            && runtime
                .find_navigation
                .pending
                .as_ref()
                .is_some_and(|pending| {
                    pending
                        .key
                        .as_ref()
                        .is_none_or(|key| runtime.find_matches.contains(key))
                })
        {
            return None;
        }
        if raw.failed.is_some() || raw.deferred == Some(metadata_epoch()) {
            return Some(Task::none());
        }
        if raw
            .in_flight
            .as_ref()
            .is_some_and(|alive| alive.load(Ordering::Acquire))
        {
            return if raw.accepted.is_some() {
                None
            } else {
                Some(Task::none())
            };
        }
        let Some(permit) = admit() else {
            return Some(later(target, runtime.find_navigation.guard(), false));
        };
        let context = runtime.selected_subagent.clone();
        let work = Work::new(Input::Raw(capture_raw(runtime)), permit);
        let clock = runtime.find_navigation.search.clone();
        let query_epoch = clock.load(Ordering::Acquire);
        let epoch = runtime.find_navigation.raw.clock.load(Ordering::Acquire);
        runtime.find_navigation.raw.run = runtime.find_navigation.raw.run.wrapping_add(1);
        let run = runtime.find_navigation.raw.run;
        runtime.find_navigation.raw.in_flight = Some(work.alive());
        Some(cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let Input::Raw(input) = work.input() else {
                        unreachable!()
                    };
                    raw_scan(input, &input.query, &|| {
                        clock.load(Ordering::Acquire) == query_epoch
                    })
                }))
                .unwrap_or(Err(RawFailure::Worker))
                .map(|(groups, tools, metadata, pattern)| {
                    RawOutput(Arc::new(RawOwner(Some(RawData {
                        epoch,
                        query_epoch,
                        pattern,
                        context,
                        groups,
                        tools,
                        _metadata: metadata,
                    }))))
                });
                // Every outcome drops real input on this worker, then carries
                // the SAME admission through completion consumption/rejection.
                work.finish(outcome)
            })
            .await
            .ok();
            action(Action::FindGroups(target, epoch, query_epoch, run, result))
        }))
    }
    pub(super) fn find_groups(
        &mut self,
        target: Target,
        epoch: u64,
        query_epoch: u64,
        run: u64,
        result: Option<RawResult>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        runtime.refresh_find_work();
        if query_epoch != runtime.find_navigation.search.load(Ordering::Acquire)
            || run != runtime.find_navigation.raw.run
        {
            return self.resume_current_find(target, true);
        }
        runtime.find_navigation.raw.in_flight = None;
        let outcome = result
            .and_then(Ready::take)
            .unwrap_or(Err(RawFailure::Worker));
        let result = match outcome {
            Ok(result) => result,
            Err(RawFailure::Pressure(attempt)) => {
                runtime.find_navigation.raw.accepted = None;
                runtime.find_navigation.raw.highlight_context = None;
                runtime.find_navigation.raw.complete = false;
                runtime.find_matches.clear();
                runtime.find_index = 0;
                runtime.find_navigation.raw.deferred = Some(attempt);
                register_highlights(runtime);
                return self.resume_current_find(target, true);
            }
            Err(failure @ (RawFailure::TooLarge | RawFailure::Worker)) => {
                runtime.fail_find(failure);
                return Task::none();
            }
            Err(RawFailure::Canceled) => return self.resume_current_find(target, true),
        };
        if result.query_epoch != runtime.find_navigation.search.load(Ordering::Acquire)
            || runtime.selected_subagent != result.context
            || !runtime.visible.load(Ordering::Relaxed)
        {
            return Task::none();
        }
        runtime.find_navigation.raw.complete =
            epoch == runtime.find_navigation.raw.clock.load(Ordering::Acquire);
        let previous = runtime.find_matches.get(runtime.find_index).cloned();
        runtime.find_navigation.raw.accepted = Some(result);
        runtime.find_matches = current_groups(runtime);
        runtime.find_index = previous
            .and_then(|key| {
                runtime
                    .find_matches
                    .iter()
                    .position(|candidate| candidate == &key)
            })
            .unwrap_or_else(|| runtime.find_matches.len().saturating_sub(1));
        register_highlights(runtime);
        self.resume_current_find(target, true)
    }
    pub(super) fn retry_find(&mut self, target: Target) -> Task<Message> {
        if let Some(runtime) = self.chat_runtime_mut(target) {
            runtime.find_navigation.cancel_all();
            runtime.restore_find_disclosures();
            runtime.find_matches.clear();
            runtime.find_index = 0;
            runtime.refresh_find_work();
        }
        self.begin_find_navigation(target, None)
    }
}

struct HighlightContext {
    clock: Arc<AtomicU64>,
    epoch: u64,
    render_revision: u64,
    plan: RawOutput,
    tools: Vec<(u64, Weak<str>)>,
}
impl HighlightContext {
    fn current(&self) -> bool {
        self.clock.load(Ordering::Acquire) == self.epoch
    }
}
thread_local! {
    static HIGHLIGHTS: std::cell::RefCell<HashMap<Target,std::sync::Weak<HighlightContext>>> = std::cell::RefCell::new(HashMap::new());
}
fn register_highlights(runtime: &mut Runtime) {
    static NEXT_RENDER_REVISION: AtomicU64 = AtomicU64::new(1);
    let raw = &runtime.find_navigation.raw;
    let context = raw
        .accepted
        .as_ref()
        .map(|result| {
            let clock = runtime.find_navigation.search.clone();
            let epoch = clock.load(Ordering::Acquire);
            let tools: Vec<_> = result
                .tools
                .iter()
                .filter(|(_, _, id, source)| {
                    runtime
                        .active_item(id)
                        .is_some_and(|item| source.as_ptr() == Arc::as_ptr(item))
                })
                .map(|(id, source, _, _)| (*id, source.clone()))
                .collect();
            let render_revision = raw
                .highlight_context
                .as_ref()
                .filter(|previous| {
                    Arc::ptr_eq(&previous.clock, &clock)
                        && previous.epoch == epoch
                        && previous.tools.len() == tools.len()
                        && previous.tools.iter().zip(&tools).all(
                            |((id, source), (next_id, next_source))| {
                                id == next_id && source.ptr_eq(next_source)
                            },
                        )
                })
                .map_or_else(
                    || NEXT_RENDER_REVISION.fetch_add(1, Ordering::Relaxed),
                    |previous| previous.render_revision,
                );
            Arc::new(HighlightContext {
                clock,
                epoch,
                render_revision,
                plan: result.clone(),
                tools,
            })
        })
        .or_else(|| {
            raw.highlight_context
                .as_ref()
                .filter(|context| context.current())
                .cloned()
        });
    HIGHLIGHTS.with(|registry| {
        let mut registry = registry.borrow_mut();
        registry.retain(|_, context| context.strong_count() != 0);
        if let Some(context) = &context {
            registry.insert(runtime.target, Arc::downgrade(context));
        } else {
            registry.remove(&runtime.target);
        }
    });
    runtime.find_navigation.raw.highlight_context = context;
}
fn highlight_context(target: Target) -> Option<Arc<HighlightContext>> {
    HIGHLIGHTS
        .with(|contexts| contexts.borrow().get(&target).and_then(Weak::upgrade))
        .filter(|context| context.current())
}
pub(super) fn tool_uncapped(target: Target, identity: u64, text: &Arc<str>) -> bool {
    highlight_context(target).is_some_and(|context| {
        context.tools.iter().any(|(key, source)| {
            *key == identity && std::ptr::eq(source.as_ptr(), Arc::as_ptr(text))
        })
    })
}
pub(super) struct Highlight {
    clock: Arc<AtomicU64>,
    epoch: u64,
    source: Weak<str>,
    pub(super) marks: Marks,
}
impl Highlight {
    pub(super) fn current(&self, target: Target, source: &Arc<str>) -> bool {
        self.clock.load(Ordering::Acquire) == self.epoch
            && highlight_context(target).is_some_and(|shown| {
                shown.epoch == self.epoch && Arc::ptr_eq(&shown.clock, &self.clock)
            })
            && std::ptr::eq(self.source.as_ptr(), Arc::as_ptr(source))
    }
}
pub(super) type HighlightResult = Ready<Result<Highlight, RawFailure>>;
impl Cagents {
    pub(super) fn find_highlight(&mut self, target: Target, id: u64) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if !runtime.visible.load(Ordering::Relaxed) || runtime.find_navigation.raw.failed.is_some()
        {
            return Task::none();
        }
        let Some(context) = highlight_context(target) else {
            return Task::none();
        };
        let Some(entry) = flow_layout::find_id(target, id) else {
            return Task::none();
        };
        if !entry.borrow_mut().needs_find_highlights(target) {
            return Task::none();
        }
        static BUDGET: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> =
            std::sync::OnceLock::new();
        let budget = BUDGET
            .get_or_init(|| Arc::new(tokio::sync::Semaphore::new(16 * 1024 * 1024)))
            .clone();
        let bytes = entry
            .borrow()
            .prepared()
            .map_or(0, |ready| ready.plain.len());
        let charge =
            (bytes.min(find::HIGHLIGHT_LIMIT) * std::mem::size_of::<std::ops::Range<usize>>() * 2
                + 512) as u32;
        let budget_attempt = available_epoch();
        let output = budget
            .clone()
            .try_acquire_many_owned(charge)
            .ok()
            .or_else(|| {
                flow_layout::evict_find_highlights(id);
                budget.try_acquire_many_owned(charge).ok()
            });
        let Some(output) = output else {
            entry.borrow_mut().defer_find_highlights_at(budget_attempt);
            return Task::none();
        };
        let worker_attempt = available_epoch();
        let Some(permit) = admit() else {
            entry.borrow_mut().defer_find_highlights_at(worker_attempt);
            return Task::none();
        };
        // Capture the exact PAINTED ready prefix only after both admissions.
        let Some(source) = entry.borrow().prepared().map(|ready| ready.plain.clone()) else {
            return Task::none();
        };
        let proof = Arc::downgrade(&source);
        let work = Work::new(Input::Highlight(source), permit);
        let attempt = work.alive();
        entry.borrow_mut().request_find_highlights(
            &context.clock,
            context.epoch,
            &proof,
            attempt.clone(),
        );
        cosmic::task::future(async move {
            let ready = tokio::task::spawn_blocking(move || {
                let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    let Input::Highlight(source) = work.input() else {
                        unreachable!()
                    };
                    let found = find::matched_pattern_checked(
                        source,
                        &context.plan.pattern,
                        find::HIGHLIGHT_LIMIT,
                        &|| context.current(),
                    )
                    .ok_or(RawFailure::Canceled)?;
                    Ok(Highlight {
                        clock: context.clock.clone(),
                        epoch: context.epoch,
                        source: proof,
                        marks: Marks(Arc::new(MarkData {
                            ranges: found.ranges,
                            truncated: found.truncated,
                            _budget: Some(MarkBudget(Some(output))),
                        })),
                    })
                }))
                .unwrap_or(Err(RawFailure::Worker));
                work.finish(outcome)
            })
            .await
            .ok();
            action(Action::FindHighlighted(target, id, attempt, ready))
        })
    }
    pub(super) fn find_highlighted(
        &mut self,
        target: Target,
        id: u64,
        attempt: Arc<AtomicBool>,
        ready: Option<HighlightResult>,
    ) -> Task<Message> {
        let Some(runtime) = self.chat_runtime_mut(target) else {
            return Task::none();
        };
        if !runtime.visible.load(Ordering::Relaxed) {
            return Task::none();
        }
        let Some(entry) = flow_layout::find_id(target, id) else {
            return Task::none();
        };
        if !entry.borrow().matches_find_request(&attempt) {
            return Task::none();
        }
        if !entry.borrow().current_find_request(target, &attempt) {
            entry
                .borrow_mut()
                .complete_find_highlights(target, &attempt, None);
            return self.resume_current_find(target, true);
        }
        let result = match ready
            .and_then(Ready::take)
            .unwrap_or(Err(RawFailure::Worker))
        {
            Ok(found) => Some(found),
            Err(RawFailure::Canceled) => None,
            Err(_) => {
                entry
                    .borrow_mut()
                    .complete_find_highlights(target, &attempt, None);
                runtime.fail_find(RawFailure::Worker);
                return Task::none();
            }
        };
        entry
            .borrow_mut()
            .complete_find_highlights(target, &attempt, result);
        self.resume_current_find(target, true)
    }
}

#[derive(Clone, Default)]
pub(super) struct Marks(Arc<MarkData>);
#[derive(Default)]
struct MarkData {
    ranges: Vec<std::ops::Range<usize>>,
    truncated: bool,
    // Covers complete range allocation capacity and one result descriptor;
    // retained by every painted widget clone, not just the Flow cache.
    _budget: Option<MarkBudget>,
}
impl std::ops::Deref for Marks {
    type Target = Vec<std::ops::Range<usize>>;
    fn deref(&self) -> &Self::Target {
        &self.0.ranges
    }
}
impl From<Vec<std::ops::Range<usize>>> for Marks {
    fn from(ranges: Vec<std::ops::Range<usize>>) -> Self {
        Self(Arc::new(MarkData {
            ranges,
            ..MarkData::default()
        }))
    }
}
impl Marks {
    pub(super) fn truncated(&self) -> bool {
        self.0.truncated
    }
}
pub(super) fn has_highlight_context(target: Target) -> bool {
    highlight_context(target).is_some()
}

fn capture_raw(runtime: &Runtime) -> RawInput {
    let all = raw_items(runtime);
    let groups = raw_groups(runtime)
        .into_iter()
        .map(|(key, group)| {
            (
                key,
                group
                    .into_iter()
                    .filter_map(|(index, raw_disclosure)| {
                        let item = all.get(index)?.clone();
                        let cached = runtime.presentation_cache().get(&item.id).filter(|cached| {
                            cached
                                .item
                                .as_ref()
                                .is_some_and(|shown| Arc::ptr_eq(shown, &item))
                        });
                        let mut tools: Vec<_> = tools(cached)
                            .map(|tool| (tool.find_identity(), tool.text.clone()))
                            .collect();
                        // Strong plaintext ownership is captured only after
                        // the same sole Find admission as every other reader.
                        if let Some((identity, text)) = notice_reader(cached) {
                            tools.push((identity, text.plain()));
                        }
                        Some(RawItem {
                            item,
                            raw_disclosure,
                            tools,
                        })
                    })
                    .collect(),
            )
        })
        .collect();
    RawInput {
        query: runtime.find.clone().unwrap_or_default(),
        groups,
        provider: runtime.session.launch.provider,
    }
}
/// Aggregate source AND repeated query work, decided from lengths and bounded
/// descriptors. No raw scan, JSON serialization or coordination parse chooses
/// this path. The full prepared tool source counts separately from raw input.
fn small_raw_work(runtime: &Runtime) -> bool {
    let query = runtime.find.as_ref().map_or(0, String::len);
    if query > 4096 {
        return false;
    }
    let mut work = query;
    let mut count = 0usize;
    for item in raw_items(runtime) {
        count += 1;
        if count > 16 {
            return false;
        }
        let before = work;
        let mut fields = 0usize;
        let mut add = |text: &str| {
            fields += 1;
            work = work.saturating_add(text.len()).saturating_add(query)
        };
        match &item.content {
            ItemContent::Text { text, .. } => add(text),
            ItemContent::Command {
                command,
                output,
                label,
                working_directory,
                ..
            } => {
                add(command);
                add(output);
                add(label);
                add(working_directory);
            }
            ItemContent::Tool {
                name,
                input,
                output,
                ..
            } => {
                add(name);
                add(input);
                add(output);
            }
            ItemContent::Delivery {
                from,
                sender,
                summary,
                body,
                ..
            } => {
                add(from);
                add(sender);
                add(summary);
                add(body);
            }
            ItemContent::Image {
                path, description, ..
            } => {
                add(path);
                add(description);
            }
            // Variable compound descriptors require the worker. Their byte_len
            // helpers walk collections and do not constitute O(1) admission.
            ItemContent::Changes { .. } | ItemContent::WebSearch { .. } => return false,
        }
        // Coordination unquoting can expose at most the bounded input bytes;
        // include worst-case per-byte matching/descriptor/query overhead.
        if item.content.is_step() {
            work = work.saturating_add(
                work.saturating_sub(before)
                    .saturating_mul(query.max(1))
                    .saturating_mul(8),
            );
        }
        let cached = runtime.presentation_cache().get(&item.id);
        for tool in tools(cached) {
            work = work.saturating_add(tool.text.len()).saturating_add(query);
        }
        if let Some((_, text)) = notice_reader(cached) {
            work = work.saturating_add(text.len()).saturating_add(query);
        }
        work = work
            .saturating_add(item.id.len())
            .saturating_add(fields * 32);
        if work > 4096 {
            return false;
        }
    }
    work <= 4096
}

struct MarkBudget(Option<tokio::sync::OwnedSemaphorePermit>);
impl Drop for MarkBudget {
    fn drop(&mut self) {
        drop(self.0.take());
        available().send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }
}

/// Inline transcript leaves are intrinsically at most2048 bytes; larger
/// prepared sources use Flow. The query was folded once by admitted Find work.
pub(super) fn inline_highlights(target: Target, text: &str) -> Marks {
    if text.len() > 2048 {
        return Marks::default();
    }
    highlight_context(target)
        .and_then(|context| {
            find::matched_pattern_checked(
                text,
                &context.plan.pattern,
                find::HIGHLIGHT_LIMIT,
                &|| context.current(),
            )
        })
        .map_or_else(Marks::default, |found| found.ranges.into())
}

fn current_groups(runtime: &Runtime) -> Vec<String> {
    let Some(accepted) = &runtime.find_navigation.raw.accepted else {
        return Vec::new();
    };
    let Some(stamp) = &runtime.find_navigation.raw.stamp else {
        return Vec::new();
    };
    stamp
        .groups
        .iter()
        .filter(|(key, sources)| {
            accepted
                .groups
                .iter()
                .find(|(group, _)| group == key)
                .is_some_and(|(_, plans)| {
                    plans.iter().any(|plan| {
                        sources
                            .iter()
                            .any(|(source, _)| source.0.ptr_eq(&plan.source))
                    })
                })
        })
        .map(|(key, _)| key.clone())
        .collect()
}

fn plan_in_group(runtime: &Runtime, key: &str, plan: &ItemPlan) -> bool {
    runtime
        .find_navigation
        .raw
        .stamp
        .as_ref()
        .and_then(|stamp| stamp.groups.iter().find(|(group, _)| group == key))
        .is_some_and(|(_, sources)| {
            sources
                .iter()
                .any(|(source, _)| source.0.ptr_eq(&plan.source))
        })
}

struct MetadataBudget(Option<tokio::sync::OwnedSemaphorePermit>);
impl Drop for MetadataBudget {
    fn drop(&mut self) {
        drop(self.0.take());
        METADATA_EPOCH.fetch_add(1, Ordering::AcqRel);
        available().send_modify(|epoch| *epoch = epoch.wrapping_add(1));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn runtime(body: &str) -> (Cagents, Target, Arc<ClientHandle>) {
        let account = Account::system(Provider::Claude);
        let session = fresh_chat_session(
            Provider::Claude,
            TerminalId::new(),
            "/project".into(),
            &account,
            false,
        )
        .unwrap();
        let item = Arc::new(chat::Item {
            id: "source".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Text {
                text: body.into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            at: None,
            started_at: None,
        });
        let handle = Arc::new(ClientHandle::fixture(
            session.clone(),
            Snapshot {
                ready: true,
                items: vec![item],
                ..Snapshot::default()
            },
        ));
        let target = Target {
            card: CardId::new(),
            generation: 1,
        };
        let mut runtime = Runtime::new(
            target,
            session,
            Connection::Live(handle.clone()),
            true,
            crate::settings::EditorSyntaxTheme::default(),
        );
        preparation::drain_fixture(&mut runtime);
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.cards
            .insert(target.card, CardRuntime::Chat(Box::new(runtime)));
        (app, target, handle)
    }
    fn finish_highlight(app: &mut Cagents, target: Target, id: u64) {
        use iced::futures::StreamExt;
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // The real process-wide slot can belong to another mounted reader.
        // Subscribe before attempting admission so release-before-wait is not
        // lost; only an actual availability edge permits a deferred retry.
        let mut capacity = available().subscribe();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        let effects = loop {
            assert!(
                tokio::time::Instant::now() < deadline,
                "the real Find executor did not become available within five seconds"
            );
            capacity.borrow_and_update();
            let task = app.find_highlight(target, id);
            if let Some(stream) = iced::runtime::task::into_stream(task) {
                break executor.block_on(stream.collect::<Vec<_>>());
            }
            executor.block_on(async {
                tokio::time::timeout_at(deadline, capacity.changed())
                    .await
                    .expect("actual Find availability must arrive within five seconds")
                    .expect("the process-owned availability sender remains live");
            });
        };
        let mut accepted = false;
        for effect in effects {
            if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                Action::FindHighlighted(found, key, attempt, result),
            ))) = effect
            {
                assert_eq!((found, key), (target, id));
                drop(app.find_highlighted(found, key, attempt, result));
                accepted = true;
            }
        }
        assert!(
            accepted,
            "actual admitted Find worker returned its typed result"
        );
    }
    fn completion_reply(task: Task<Message>) -> Action {
        use iced::futures::StreamExt;
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        executor.block_on(async {
            let mut stream =
                iced::runtime::task::into_stream(task).expect("actual admitted completion task");
            while let Some(effect) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                    reply @ (Action::FindIndexed(..) | Action::FindHighlighted(..)),
                ))) = effect
                {
                    return reply;
                }
            }
            panic!("admitted Catalog/Highlight returns its typed completion")
        })
    }
    #[test]
    fn typed_failure_find_uses_its_real_anchor_and_keeps_child_context_guard() {
        let (mut app, target, handle) = runtime("unrelated original text");
        let mut error = chat::Item {
            id: "diagnostic-same-id".into(),
            role: chat::Role::System,
            started_at: None,
            at: None,
            content: ItemContent::Text {
                text: "You've hit your limit · resets 1:30pm\nPARENT_TYPED_NEEDLE".into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
        };
        assert!(chat::failure::mark_provider_error(
            Provider::Claude,
            &mut error
        ));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.items = vec![Arc::new(error.clone())];
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        drop(find_for_test(
            &mut app,
            target,
            "PARENT_TYPED_NEEDLE".into(),
        ));
        let live = app.chat_runtime_mut(target).unwrap();
        let group = live.find_matches[0].clone();
        let incident = live.failure_ui.incidents[0].key.clone();
        assert_eq!(
            live.find_plan_anchor(&group),
            live.find_anchor(&format!("failure:{incident}"))
        );
        assert!(live.failure_ui.expanded.contains(&incident));
        assert!(
            live.failure_preview(&incident)
                .unwrap()
                .0
                .contains("PARENT_TYPED_NEEDLE")
        );
        let (parent_catalog, _) = live.selection_document();
        assert!(
            parent_catalog
                .text(failure::diagnostic_preview_identity(&incident))
                .unwrap()
                .contains("PARENT_TYPED_NEEDLE")
        );
        let old_intent = live.find_navigation.guard();
        let mut child_error = error;
        let ItemContent::Delivery { body, .. } = &mut child_error.content else {
            panic!("actual provider marker retains its typed delivery carrier");
        };
        *body = "You've hit your limit · resets 2:30pm\nCHILD_TYPED_NEEDLE".into();
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.subagents = vec![chat::Subagent {
            id: "typed-child".into(),
            kind: "reviewer".into(),
            description: "Scoped child".into(),
            status: chat::ToolStatus::Failed,
            model: None,
            waiting: false,
            detail: String::new(),
            background: true,
            items: vec![Arc::new(child_error)],
        }];
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        drop(app.handle_chat_ui(Action::SelectSubagent(target, Some("typed-child".into()))));
        assert!(
            !old_intent.current(),
            "old parent navigation cannot apply inside the child"
        );
        drop(find_for_test(&mut app, target, "CHILD_TYPED_NEEDLE".into()));
        let live = app.chat_runtime_mut(target).unwrap();
        let group = live.find_matches[0].clone();
        assert_eq!(
            live.find_plan_anchor(&group),
            live.find_anchor("diagnostic-same-id")
        );
        let (incident, _) = live
            .failure_ui
            .child_item("typed-child", "diagnostic-same-id")
            .unwrap();
        assert!(live.failure_ui.expanded.contains(&incident.key));
        assert!(
            live.failure_preview(&incident.key)
                .unwrap()
                .0
                .contains("CHILD_TYPED_NEEDLE")
        );
        let (child_catalog, _) = live.selection_document();
        let child_preview = child_catalog
            .text(failure::diagnostic_preview_identity(&incident.key))
            .unwrap();
        assert!(child_preview.contains("CHILD_TYPED_NEEDLE"));
        assert!(!child_preview.contains("PARENT_TYPED_NEEDLE"));
        assert!(!Arc::ptr_eq(&parent_catalog, &child_catalog));
    }

    #[test]
    fn catalog_attempt_survives_queued_ready_and_rejects_old_none_and_duplicate_some_after_rewrite()
    {
        preparation::retirement_barrier();
        let (mut app, target, handle) = runtime("original needle body");
        drop(app.handle_chat_ui(Action::Find(target, "needle".into())));
        let intent = app
            .chat_runtime_mut(target)
            .unwrap()
            .find_navigation
            .guard();
        let reply = completion_reply(app.resume_find_navigation(target, intent.clone(), false));
        let duplicate = reply.clone();
        let Action::FindIndexed(_, _, old_document, old_attempt, _) = &reply else {
            panic!("actual Catalog reply")
        };
        let (old_document, old_attempt) = (*old_document, old_attempt.clone());
        assert_eq!(
            old_document,
            app.chat_runtime_mut(target)
                .unwrap()
                .find_navigation
                .document_epoch(),
            "cold Catalog captures its epoch after lazy document binding"
        );
        assert!(!old_attempt.load(Ordering::Acquire));
        assert!(
            admit().is_none(),
            "queued Catalog Ready retains the SAME slot before UI consumption"
        );
        drop(app.resume_find_navigation(target, intent.clone(), false));
        assert!(
            Arc::ptr_eq(
                app.chat_runtime_mut(target)
                    .unwrap()
                    .find_navigation
                    .pending
                    .as_ref()
                    .unwrap()
                    .worker_alive
                    .as_ref()
                    .unwrap(),
                &old_attempt
            ),
            "a queued Ready keeps its exact token after worker_alive becomes false"
        );
        drop(app.handle_chat_ui(reply));
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .find_navigation
                .pending
                .as_ref()
                .unwrap()
                .hit
                .is_some()
        );
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        let old = snapshot.items[0].clone();
        snapshot.items[0] = Arc::new(chat::Item {
            content: ItemContent::Text {
                text: "rewritten current needle body".into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            ..(*old).clone()
        });
        handle.publish_snapshot_for_test(snapshot);
        let runtime = app.chat_runtime_mut(target).unwrap();
        runtime.refresh();
        preparation::drain_fixture(runtime);
        let newer = app.resume_find_navigation(target, intent.clone(), false);
        let pending = app
            .chat_runtime_mut(target)
            .unwrap()
            .find_navigation
            .pending
            .as_ref()
            .unwrap();
        assert_eq!(
            pending.intent.epoch, intent.epoch,
            "real source rewrite preserves the same navigation intent"
        );
        let current = pending.worker_alive.as_ref().unwrap().clone();
        let passes = pending.passes;
        assert!(!Arc::ptr_eq(&current, &old_attempt));
        assert!(current.load(Ordering::Acquire));
        // Body-free exceptional completion injection is a consumer control;
        // the runtime is not claimed to have generated a JoinError here.
        drop(app.handle_chat_ui(Action::FindIndexed(
            target,
            intent.clone(),
            old_document,
            old_attempt,
            None,
        )));
        drop(app.handle_chat_ui(duplicate));
        let pending = app
            .chat_runtime_mut(target)
            .unwrap()
            .find_navigation
            .pending
            .as_ref()
            .unwrap();
        assert!(
            pending.in_flight && Arc::ptr_eq(pending.worker_alive.as_ref().unwrap(), &current),
            "old Catalog None and duplicate consumed Some cannot clear a newer same-intent attempt"
        );
        assert_eq!(pending.passes, passes);
        let reply = completion_reply(newer);
        drop(app.handle_chat_ui(reply));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert_eq!(
            runtime
                .find_navigation
                .pending
                .as_ref()
                .unwrap()
                .hit
                .as_ref()
                .unwrap()
                .byte,
            "rewritten current ".len()
        );
        assert!(runtime.find_error().is_none());
        drop(app);
        preparation::retirement_barrier();
    }
    #[test]
    fn highlight_attempt_rejects_old_none_and_duplicate_some_before_clearing_current_request() {
        preparation::retirement_barrier();
        let (mut app, target, _) = runtime("needle");
        drop(app.handle_chat_ui(Action::Find(target, "needle".into())));
        let cache = flow_layout::Cache::default();
        let entry = cache.entry(
            991,
            vec![iced::widget::text::Span::new("needle ".repeat(80))].into(),
        );
        entry.borrow_mut().bind_target(target);
        entry
            .borrow_mut()
            .configure(600.0, iced::Pixels(14.0), iced::Font::default());
        entry.borrow_mut().set_find_visible(true);
        assert!(cache.complete_fixture());
        let id = entry.borrow_mut().find_request_id(target).unwrap();
        let reply = completion_reply(app.find_highlight(target, id));
        let duplicate = reply.clone();
        let Action::FindHighlighted(_, _, old_attempt, _) = &reply else {
            panic!("actual Highlight reply")
        };
        let old_attempt = old_attempt.clone();
        assert!(
            admit().is_none(),
            "queued Highlight Ready retains the SAME slot"
        );
        drop(app.handle_chat_ui(reply));
        assert_eq!(entry.borrow_mut().find_highlights(target).len(), 80);
        drop(app.handle_chat_ui(Action::Find(target, "NEEDLE".into())));
        let newer = app.find_highlight(target, id);
        let current = entry.borrow().find_attempt_for_test().unwrap();
        assert!(!Arc::ptr_eq(&current, &old_attempt));
        assert!(current.load(Ordering::Acquire));
        drop(app.handle_chat_ui(Action::FindHighlighted(target, id, old_attempt, None)));
        drop(app.handle_chat_ui(duplicate));
        assert!(
            entry.borrow().matches_find_request(&current),
            "old Highlight None and duplicate consumed Some cannot clear a newer request"
        );
        assert!(!entry.borrow_mut().needs_find_highlights(target));
        assert!(app.chat_runtime_mut(target).unwrap().find_error().is_none());
        let reply = completion_reply(newer);
        drop(app.handle_chat_ui(reply));
        assert_eq!(entry.borrow_mut().find_highlights(target).len(), 80);
        assert!(entry.borrow_mut().find_request_id(target).is_none());
        drop(app);
        drop(entry);
        drop(cache);
        preparation::retirement_barrier();
    }
    #[test]
    fn current_highlight_failure_suppresses_redraw_demand_until_explicit_retry() {
        preparation::retirement_barrier();
        let (mut app, target, _) = runtime("needle");
        drop(app.handle_chat_ui(Action::Find(target, "needle".into())));
        let cache = flow_layout::Cache::default();
        let entry = cache.entry(
            992,
            vec![iced::widget::text::Span::new("needle ".repeat(80))].into(),
        );
        entry.borrow_mut().bind_target(target);
        entry
            .borrow_mut()
            .configure(600.0, iced::Pixels(14.0), iced::Font::default());
        entry.borrow_mut().set_find_visible(true);
        assert!(cache.complete_fixture());
        let id = entry.borrow_mut().find_request_id(target).unwrap();
        let task = app.find_highlight(target, id);
        let attempt = entry.borrow().find_attempt_for_test().unwrap();
        drop(task); // actual unpolled cancellation releases input through FIFO
        preparation::retirement_barrier();
        // Inject the body-free outer JoinError representation at the consumer;
        // no worker panic is claimed from this normal cancellation.
        drop(app.handle_chat_ui(Action::FindHighlighted(target, id, attempt, None)));
        assert!(app.chat_runtime_mut(target).unwrap().find_error().is_some());
        assert!(!has_highlight_context(target));
        for _ in 0..4 {
            assert!(
                entry.borrow_mut().find_request_id(target).is_none(),
                "a failed current search cannot enqueue highlight work on each Redraw"
            );
            assert!(iced::runtime::task::into_stream(app.find_highlight(target, id)).is_none());
        }
        drop(app.handle_chat_ui(Action::FindRetry(target)));
        assert!(app.chat_runtime_mut(target).unwrap().find_error().is_none());
        assert!(entry.borrow_mut().find_request_id(target).is_some());
        finish_highlight(&mut app, target, id);
        assert_eq!(entry.borrow_mut().find_highlights(target).len(), 80);
        drop(app);
        drop(entry);
        drop(cache);
        preparation::retirement_barrier();
    }
    #[test]
    fn unpolled_find_input_retires_before_the_same_sole_permit() {
        preparation::retirement_barrier();
        let held = preparation::hold_retirement();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let source: Arc<str> = "huge raw input λ ".repeat(30_000).into();
        let weak = Arc::downgrade(&source);
        let work = Work::new(
            Input::Highlight(source),
            Slot(Some(semaphore.clone().try_acquire_owned().unwrap())),
        );
        let alive = work.alive();
        let future = async move { drop(work) };
        drop(future);
        assert!(!alive.load(Ordering::Acquire));
        assert!(
            weak.upgrade().is_some(),
            "the stalled FIFO owns actual raw input"
        );
        assert!(
            semaphore.clone().try_acquire_owned().is_err(),
            "canceling an unpolled task cannot amplify raw generations"
        );
        held.send(()).unwrap();
        preparation::retirement_barrier();
        assert!(weak.upgrade().is_none());
        assert_eq!(semaphore.available_permits(), 1);
    }
    #[test]
    fn completed_find_keeps_slot_through_stale_reply_retirement() {
        struct Output {
            bytes: Option<Vec<u8>>,
            sent: std::sync::mpsc::Sender<(std::thread::ThreadId, usize)>,
            semaphore: Arc<tokio::sync::Semaphore>,
        }
        impl Drop for Output {
            fn drop(&mut self) {
                drop(self.bytes.take());
                self.sent
                    .send((
                        std::thread::current().id(),
                        self.semaphore.available_permits(),
                    ))
                    .unwrap();
            }
        }
        preparation::retirement_barrier();
        let semaphore = Arc::new(tokio::sync::Semaphore::new(1));
        let source: Arc<str> = "old raw source ".repeat(20_000).into();
        let weak = Arc::downgrade(&source);
        let work = Work::new(
            Input::Highlight(source),
            Slot(Some(semaphore.clone().try_acquire_owned().unwrap())),
        );
        let (sent, observed) = std::sync::mpsc::channel();
        let output = Output {
            bytes: Some(vec![17; 128 * 1024]),
            sent,
            semaphore: semaphore.clone(),
        };
        let ready = std::thread::spawn(move || work.finish(output))
            .join()
            .unwrap();
        assert!(
            weak.upgrade().is_none(),
            "worker releases raw source before returning the same admission"
        );
        assert_eq!(
            semaphore.available_permits(),
            0,
            "queued completion retains admission"
        );
        let held = preparation::hold_retirement();
        drop(ready);
        assert!(observed.try_recv().is_err());
        assert_eq!(
            semaphore.available_permits(),
            0,
            "stale message drop keeps admission behind actual output destruction"
        );
        held.send(()).unwrap();
        let worker = preparation::retirement_barrier();
        assert_eq!(observed.recv().unwrap(), (worker, 0));
        assert_eq!(semaphore.available_permits(), 1);
    }
    #[test]
    fn raw_plan_discovers_unquoted_parts_and_exact_prepared_readers_in_order() {
        let query = "say \"NEEDLE λ\"";
        let item = Arc::new(chat::Item {
            id: "agent".into(),
            role: chat::Role::Assistant,
            content: ItemContent::Tool {
                name: "Agent".into(),
                input: serde_json::json!({"prompt":query,"run_in_background":true}).to_string(),
                output: "Background task launched".into(),
                status: chat::ToolStatus::Succeeded,
            },
            at: None,
            started_at: None,
        });
        assert!(!find::contains(
            match &item.content {
                ItemContent::Tool { input, .. } => input,
                _ => unreachable!(),
            },
            query
        ));
        let input = RawInput {
            query: query.into(),
            provider: Provider::Claude,
            groups: vec![(
                "group".into(),
                vec![RawItem {
                    item: item.clone(),
                    raw_disclosure: true,
                    tools: vec![(1, Arc::from("absent")), (2, Arc::from(query))],
                }],
            )],
        };
        let (groups, tools, _, _) = raw_scan(&input, query, &|| true).unwrap();
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].1[0].parts, vec!["agent#prompt-0"]);
        assert!(
            !groups[0].1[0].expand,
            "matching card part owns disclosure rather than raw command"
        );
        assert_eq!(
            tools.iter().map(|(id, _, _, _)| *id).collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(groups[0].1[0].source.as_ptr(), Arc::as_ptr(&item));
    }
    #[test]
    fn retained_flow_draw_paints_late_async_find_marks_without_element_rebuild() {
        use iced::advanced::{
            Layout, Shell, clipboard, layout, renderer::Renderer as _, widget::Tree,
        };
        let (mut app, target, _) = runtime("needle");
        drop(app.handle_chat_ui(Action::Find(target, "needle".into())));
        let cache = flow_layout::Cache::default();
        let text = "needle ".repeat(find::HIGHLIGHT_LIMIT + 1);
        let entries: Vec<_> = (0..2)
            .map(|index| {
                let entry = cache.entry(
                    90 + index,
                    vec![iced::widget::text::Span::new(text.clone())].into(),
                );
                entry.borrow_mut().bind_target(target);
                entry
                    .borrow_mut()
                    .configure(600.0, iced::Pixels(14.0), iced::Font::default());
                entry.borrow_mut().set_find_visible(true);
                entry
            })
            .collect();
        assert!(cache.complete_fixture());
        let ready = entries[0].borrow().prepared().unwrap();
        let other = entries[1].borrow().prepared().unwrap();
        assert_eq!(ready.plain, other.plain);
        assert!(!Arc::ptr_eq(&ready.plain, &other.plain));
        let mut element = selectable_prose::flow_source_optional(
            iced::Pixels(14.0),
            iced::Font::default(),
            target,
            Some("needle"),
            None,
            entries[0].clone(),
        );
        let mut tree = Tree::new(&element);
        let mut renderer = crate::app::tests::headless_renderer();
        let viewport = iced::Rectangle::with_size(iced::Size::new(600.0, 200.0));
        let limits = layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, f32::INFINITY));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert_eq!(node.children()[1].size().height, 0.0);
        let notice = "Find highlights limited to the first 4096 matches in this block.";
        let accessible = element.as_widget().a11y_nodes(
            Layout::with_offset(iced::Vector::new(12.0, 50.0), &node)
                .with_virtual_offset(iced::Vector::new(0.0, 37.0)),
            &tree,
            iced::mouse::Cursor::Unavailable,
        );
        let body = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .find(|node| node.node().value() == Some(text.as_str()))
            .expect("retained Flow wrapper forwards the complete native body accessibility value");
        let body_id = body.id().clone();
        let body_bounds = body.node().bounds().unwrap();
        assert_eq!(
            (body_bounds.x0, body_bounds.y0),
            (12.0, 13.0),
            "wrapper preserves parent virtual offset for native accessibility geometry"
        );
        assert!(
            !accessible
                .root()
                .iter()
                .chain(accessible.children())
                .any(|node| node.node().value() == Some(notice)
                    || node.node().label() == Some(notice))
        );
        let theme = cosmic::theme::active();
        let wash = iced::Background::Color(design::accent_wash(&theme));
        let quads = |renderer: &mut cosmic::Renderer| {
            let cosmic::Renderer::Secondary(renderer) = renderer else {
                panic!("headless renderer must use tiny-skia")
            };
            renderer
                .layers()
                .iter()
                .flat_map(|layer| layer.quads.iter())
                .filter(|(_, background)| *background == wash)
                .map(|(quad, _)| quad.bounds)
                .collect::<Vec<_>>()
        };
        renderer.reset(viewport);
        element.as_widget().draw(
            &tree,
            &mut renderer,
            &theme,
            &iced::advanced::renderer::Style::default(),
            Layout::new(&node),
            iced::mouse::Cursor::Unavailable,
            &viewport,
        );
        assert!(
            quads(&mut renderer).is_empty(),
            "retained native element was constructed before async Find marks"
        );
        let id = entries[0].borrow_mut().find_request_id(target).unwrap();
        finish_highlight(&mut app, target, id);
        assert!(entries[0].borrow_mut().find_highlights(target).truncated());
        renderer.reset(viewport);
        element.as_widget().draw(
            &tree,
            &mut renderer,
            &theme,
            &iced::advanced::renderer::Style::default(),
            Layout::new(&node),
            iced::mouse::Cursor::Unavailable,
            &viewport,
        );
        let painted = quads(&mut renderer);
        let glyph = ready.byte_bounds(0, ready.plain.len()).unwrap();
        let point = iced::Point::new(glyph.center_x(), glyph.center_y());
        assert!(
            painted.iter().any(|bounds| bounds.contains(point)),
            "the same native element/tree paints late worker marks at actual visible glyph geometry"
        );
        assert!(
            entries[0]
                .borrow_mut()
                .find_highlights_for(target, &other.plain)
                .is_empty(),
            "equal bytes from another actual Ready cannot certify the painted source"
        );
        assert_eq!(
            entries[0].borrow_mut().find_highlights(target).len(),
            find::HIGHLIGHT_LIMIT,
            "a stale retained painted source cannot evict current cached marks"
        );
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &iced::Event::Window(iced::window::Event::RedrawRequested(Instant::now())),
            Layout::new(&node),
            iced::mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut shell,
            &viewport,
        );
        assert!(
            shell.is_layout_invalid(),
            "late truncation changes the retained notice layout"
        );
        let refreshed = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert_eq!(
            refreshed.children()[0].bounds().y,
            0.0,
            "notice preserves the native reader origin"
        );
        assert!(
            refreshed.children()[1].size().height > 0.0,
            "late >4096 result shows its truthful notice without rebuilding Element"
        );
        let accessible = element.as_widget().a11y_nodes(
            Layout::with_offset(iced::Vector::new(12.0, 50.0), &refreshed)
                .with_virtual_offset(iced::Vector::new(0.0, 37.0)),
            &tree,
            iced::mouse::Cursor::Unavailable,
        );
        let body = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .find(|node| node.node().value() == Some(text.as_str()))
            .expect("late notice retains the complete native body accessibility value");
        assert_eq!(
            body.id(),
            &body_id,
            "the same retained native body keeps its accessibility identity"
        );
        assert_eq!(
            body.node().bounds(),
            Some(body_bounds),
            "late notice preserves exact body bounds and inherited virtual offset"
        );
        assert!(
            accessible
                .root()
                .iter()
                .chain(accessible.children())
                .any(|node| node.node().value() == Some(notice)
                    || node.node().label() == Some(notice)),
            "actual late truncation exposes its notice to accessibility"
        );
        let constrained = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(iced::Size::ZERO, iced::Size::new(600.0, 40.0)),
        );
        assert!(
            constrained.size().height <= 40.0,
            "late notice respects finite native layout height"
        );
        let body = &constrained.children()[0];
        let caption = &constrained.children()[1];
        assert_eq!(
            body.bounds().y,
            0.0,
            "constrained notice preserves native reader origin"
        );
        assert!(
            caption.size().height <= (40.0 - body.size().height).max(0.0),
            "caption uses only actual remaining height"
        );
        let mut messages = Vec::new();
        let mut shell = Shell::new(&mut messages);
        element.as_widget_mut().update(
            &mut tree,
            &iced::Event::Window(iced::window::Event::RedrawRequested(Instant::now())),
            Layout::new(&constrained),
            iced::mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut shell,
            &viewport,
        );
        assert!(
            !shell.is_layout_invalid(),
            "an unchanged truncated result does not relayout repeatedly when the caption is constrained to zero height"
        );
    }

    #[test]
    fn two_mounted_common_match_leaves_keep_full_per_leaf_budget_and_refuse_stale_source() {
        let (mut app, target, _) = runtime("needle");
        drop(app.handle_chat_ui(Action::Find(target, "needle".into())));
        let cache = flow_layout::Cache::default();
        let text = "needle ".repeat(find::HIGHLIGHT_LIMIT + 1);
        let entries: Vec<_> = (0..2)
            .map(|identity| {
                let entry = cache.entry(
                    identity + 70,
                    vec![iced::widget::text::Span::new(text.clone())].into(),
                );
                entry.borrow_mut().bind_target(target);
                entry
                    .borrow_mut()
                    .configure(600.0, iced::Pixels(14.0), iced::Font::default());
                entry.borrow_mut().set_find_visible(true);
                entry
            })
            .collect();
        assert!(cache.complete_fixture());
        for entry in &entries {
            let id = entry.borrow_mut().find_request_id(target).unwrap();
            finish_highlight(&mut app, target, id);
        }
        for entry in &entries {
            let marks = entry.borrow_mut().find_highlights(target);
            assert_eq!(marks.len(), find::HIGHLIGHT_LIMIT);
            assert!(marks.truncated(), "4096 is a truthful per-leaf limit");
        }
        let old = entries[0].borrow().prepared().unwrap();
        let context = highlight_context(target).unwrap();
        let found = Highlight {
            clock: context.clock.clone(),
            epoch: context.epoch,
            source: Arc::downgrade(&old.plain),
            marks: Marks::default(),
        };
        assert!(found.current(target, &old.plain));
        let rewritten: Arc<str> = text.clone().into();
        assert!(
            !found.current(target, &rewritten),
            "equal bytes in another ready allocation cannot certify painted prefix identity"
        );
        drop(app.handle_chat_ui(Action::Find(target, "NEEDLE".into())));
        assert!(
            !found.current(target, &old.plain),
            "same-length query edit has a new explicit intent epoch"
        );
    }
    #[test]
    fn many_visible_leaves_admit_without_starvation_and_nonvisible_eviction_wakes_readmission() {
        let (mut app, target, _) = runtime("needle");
        drop(app.handle_chat_ui(Action::Find(target, "needle".into())));
        let cache = flow_layout::Cache::default();
        let entries: Vec<_> = (0..70)
            .map(|identity| {
                let entry = cache.entry(
                    identity + 1000,
                    vec![iced::widget::text::Span::new("needle ".repeat(80))].into(),
                );
                entry.borrow_mut().bind_target(target);
                entry
                    .borrow_mut()
                    .configure(600.0, iced::Pixels(14.0), iced::Font::default());
                entry.borrow_mut().set_find_visible(true);
                entry
            })
            .collect();
        assert!(cache.complete_fixture());
        for entry in &entries {
            let id = entry.borrow_mut().find_request_id(target).unwrap();
            finish_highlight(&mut app, target, id);
        }
        for entry in &entries {
            assert_eq!(entry.borrow_mut().find_highlights(target).len(), 80);
            assert!(
                entry.borrow_mut().find_request_id(target).is_none(),
                "idle visible peers do not perpetually evict each other"
            );
        }
        let id = entries[0].borrow().id_for_find_test();
        let other = entries[1].borrow().id_for_find_test();
        entries[0].borrow_mut().set_find_visible(false);
        let before = available_epoch();
        flow_layout::evict_find_highlights(other);
        assert_ne!(
            available_epoch(),
            before,
            "actual bounded output retirement emits coalesced availability"
        );
        entries[0].borrow_mut().set_find_visible(true);
        let held = admit().unwrap();
        drop(app.find_highlight(target, id));
        assert!(
            entries[0].borrow_mut().find_request_id(target).is_none(),
            "busy executor demand coalesces rather than redraw-looping"
        );
        drop(held);
        assert_eq!(
            entries[0].borrow_mut().find_request_id(target),
            Some(id),
            "same slot release wakes deferred visible work"
        );
        let held = admit().unwrap();
        let attempt = available_epoch();
        assert!(admit().is_none());
        drop(held); // The actual resource releases before the UI records deferral.
        entries[0].borrow_mut().defer_find_highlights_at(attempt);
        assert_eq!(
            entries[0].borrow_mut().find_request_id(target),
            Some(id),
            "release-before-deferral cannot lose the only idle wake"
        );
        finish_highlight(&mut app, target, id);
        assert_eq!(entries[0].borrow_mut().find_highlights(target).len(), 80);
        assert!(
            entries
                .iter()
                .all(|entry| entry.borrow_mut().find_request_id(target).is_none())
        );
    }
    #[test]
    fn same_source_pointer_provider_transition_revokes_old_query_authority() {
        let (mut app, target, _) = runtime("stable needle");
        drop(app.handle_chat_ui(Action::Find(target, "needle".into())));
        let runtime = app.chat_runtime_mut(target).unwrap();
        let source = runtime.snapshot.items[0].clone();
        let old = runtime
            .find_navigation
            .raw
            .accepted
            .as_ref()
            .unwrap()
            .clone();
        let old_epoch = old.query_epoch;
        runtime.session.launch.provider = Provider::Codex;
        runtime.refresh_find_work();
        assert!(Arc::ptr_eq(&source, &runtime.snapshot.items[0]));
        assert_ne!(
            old_epoch,
            runtime.find_navigation.search.load(Ordering::Acquire),
            "provider changes revoke coordination interpretation even with identical source pointers"
        );
        assert!(
            old.query_epoch
                != runtime
                    .find_navigation
                    .raw
                    .accepted
                    .as_ref()
                    .unwrap()
                    .query_epoch
        );
    }
    #[test]
    fn stable_group_and_query_marks_survive_unrelated_source_publication_but_rewrites_refuse() {
        let (mut app, target, handle) = runtime("stable NEEDLE");
        drop(app.handle_chat_ui(Action::Find(target, "needle".into())));
        let runtime = app.chat_runtime_mut(target).unwrap();
        let stable = runtime.snapshot.items[0].clone();
        let key = runtime.find_matches[0].clone();
        let query_epoch = runtime.find_navigation.search.load(Ordering::Acquire);
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.items.push(Arc::new(chat::Item {
            id: "unrelated".into(),
            role: chat::Role::User,
            content: ItemContent::Text {
                text: "later absent".repeat(10_000),
                thinking: false,
                phase: chat::Phase::Final,
            },
            at: None,
            started_at: None,
        }));
        handle.publish_snapshot_for_test(snapshot.clone());
        app.chat_runtime_mut(target).unwrap().refresh();
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert_eq!(
            runtime.find_navigation.search.load(Ordering::Acquire),
            query_epoch
        );
        assert!(
            runtime.find_matches.contains(&key),
            "stable earlier result remains current during unrelated giant append"
        );
        assert!(
            !runtime.find_navigation.raw.complete,
            "changed/new sources remain pending"
        );
        snapshot.generation += 1;
        snapshot.items[0] = Arc::new(chat::Item {
            content: ItemContent::Text {
                text: "same-id replacement absent".into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            ..(*stable).clone()
        });
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        assert!(
            !app.chat_runtime_mut(target)
                .unwrap()
                .find_matches
                .contains(&key),
            "unknown same-ID rewrite cannot reuse old match authority"
        );
    }
    fn raw_reply(task: Task<Message>) -> Action {
        use iced::futures::StreamExt;
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        executor.block_on(async {
            let mut stream =
                iced::runtime::task::into_stream(task).expect("actual admitted raw task");
            while let Some(effect) = stream.next().await {
                if let iced::runtime::Action::Output(cosmic::Action::App(Message::ChatUi(
                    reply @ Action::FindGroups(..),
                ))) = effect
                {
                    return reply;
                }
            }
            panic!("raw task must return typed completion")
        })
    }
    #[test]
    fn pressure_reply_retains_same_slot_and_observes_release_before_delivery_without_stale_error_overwrite()
     {
        preparation::retirement_barrier();
        let (mut app, target, handle) = runtime("initial needle");
        drop(app.handle_chat_ui(Action::Find(target, "needle".into())));
        let runtime = app.chat_runtime_mut(target).unwrap();
        let own = runtime
            .find_navigation
            .raw
            .accepted
            .as_ref()
            .unwrap()
            ._metadata
            .0
            .as_ref()
            .unwrap()
            .num_permits();
        let before = metadata_capacity().available_permits();
        let blocker = MetadataBudget(Some(
            metadata_capacity()
                .clone()
                .try_acquire_many_owned(before as u32)
                .unwrap(),
        ));
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        let old = snapshot.items[0].clone();
        snapshot.items[0] = Arc::new(chat::Item {
            content: ItemContent::Text {
                text: "new needle λ ".repeat(10_000),
                thinking: false,
                phase: chat::Phase::Final,
            },
            ..(*old).clone()
        });
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        let reply = raw_reply(app.resume_current_find(target, false));
        assert!(
            admit().is_none(),
            "even failed indexing keeps SAME sole admission until error consumption"
        );
        let old_reply = reply.clone();
        let held = preparation::hold_retirement();
        let epoch = metadata_epoch();
        drop(blocker);
        assert_ne!(
            metadata_epoch(),
            epoch,
            "actual competing allocation releases BEFORE delayed completion delivery"
        );
        let task = app.handle_chat_ui(reply);
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(
            runtime.find_navigation.raw.accepted.is_none(),
            "pressure retires this Runtime's own displaced reservation rather than deadlocking replacement"
        );
        assert!(runtime.find_navigation.raw.failed.is_none());
        assert!(runtime.find_searching());
        assert_eq!(
            metadata_capacity().available_permits(),
            before,
            "old actual plan remains reserved behind stalled FIFO"
        );
        let run = runtime.find_navigation.raw.run;
        let alive = runtime
            .find_navigation
            .raw
            .in_flight
            .as_ref()
            .unwrap()
            .clone();
        assert!(
            alive.load(Ordering::Acquire),
            "release-before-delivery makes newest work immediately eligible"
        );
        drop(app.handle_chat_ui(old_reply));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert_eq!(runtime.find_navigation.raw.run, run);
        assert!(Arc::ptr_eq(
            runtime.find_navigation.raw.in_flight.as_ref().unwrap(),
            &alive
        ));
        assert!(
            runtime.find_navigation.raw.failed.is_none(),
            "late duplicate error cannot overwrite admitted newer run"
        );
        held.send(()).unwrap();
        preparation::retirement_barrier();
        assert_eq!(
            metadata_capacity().available_permits(),
            before + own,
            "own metadata capacity returns only after real old plan destruction"
        );
        let reply = raw_reply(task);
        drop(app.handle_chat_ui(reply));
        assert!(
            app.chat_runtime_mut(target)
                .unwrap()
                .find_navigation
                .raw
                .complete
        );
    }
    #[test]
    fn empty_edit_clears_matches_disclosures_and_pending_without_source_capture() {
        let (mut app, target, _) = runtime("needle");
        drop(app.handle_chat_ui(Action::Find(target, "needle".into())));
        assert_eq!(app.chat_runtime_mut(target).unwrap().find_matches.len(), 1);
        drop(app.handle_chat_ui(Action::Find(target, String::new())));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(runtime.find_matches.is_empty());
        assert_eq!(runtime.find_index, 0);
        assert!(runtime.find_navigation.pending.is_none());
        assert!(runtime.find_navigation.raw.stamp.is_none());
        assert!(runtime.find_navigation.raw.in_flight.is_none());
        assert!(runtime.find_disclosure.owned_count() == 0);
    }
    #[test]
    fn selected_group_survives_small_metadata_publication_and_next_advances() {
        let (mut app, target, handle) = runtime("first needle");
        let mut snapshot = (*handle.snapshot()).clone();
        snapshot.generation += 1;
        snapshot.items.push(Arc::new(chat::Item {
            id: "second".into(),
            role: chat::Role::User,
            content: ItemContent::Text {
                text: "second needle".into(),
                thinking: false,
                phase: chat::Phase::Final,
            },
            at: None,
            started_at: None,
        }));
        handle.publish_snapshot_for_test(snapshot.clone());
        app.chat_runtime_mut(target).unwrap().refresh();
        drop(find_for_test(&mut app, target, "needle".into()));
        let first = app.chat_runtime_mut(target).unwrap().find_matches[0].clone();
        assert_eq!(app.chat_runtime_mut(target).unwrap().find_index, 0);
        snapshot.generation += 1;
        snapshot.items[0] = Arc::new(chat::Item {
            at: Some(123),
            ..(*snapshot.items[0]).clone()
        });
        handle.publish_snapshot_for_test(snapshot);
        app.chat_runtime_mut(target).unwrap().refresh();
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert_eq!(runtime.find_matches[runtime.find_index], first);
        drop(app.handle_chat_ui(Action::FindNext(target, true)));
        assert_eq!(app.chat_runtime_mut(target).unwrap().find_index, 1);
    }
    #[test]
    fn visible_failure_is_explicit_and_retry_starts_new_intent() {
        let (mut app, target, _) = runtime("needle");
        drop(app.handle_chat_ui(Action::Find(target, "n".repeat(700_000))));
        let reply = raw_reply(app.resume_current_find(target, false));
        drop(app.handle_chat_ui(reply));
        let runtime = app.chat_runtime_mut(target).unwrap();
        let old = runtime.find_navigation.search.load(Ordering::Acquire);
        assert!(
            runtime.find_error().is_some(),
            "actual admitted query overflow is a visible refusal, never 0 successful matches"
        );
        assert!(!runtime.find_searching());
        drop(app.handle_chat_ui(Action::FindRetry(target)));
        let runtime = app.chat_runtime_mut(target).unwrap();
        assert!(runtime.find_error().is_none());
        assert_ne!(runtime.find_navigation.search.load(Ordering::Acquire), old);
    }
}

#[cfg(test)]
pub(super) fn find_for_test(app: &mut Cagents, target: Target, query: String) -> Task<Message> {
    let empty = query.is_empty();
    let task = app.handle_chat_ui(Action::Find(target, query));
    if empty {
        return task;
    }
    finish_raw_find_for_test(app, target, task)
}

/// Settle the accepted current query after a publication/context change. Do
/// not issue another Find action: query identity and disclosure ownership stay
/// those of the real user request being refreshed.
#[cfg(test)]
pub(super) fn settle_current_find_for_test(app: &mut Cagents, target: Target) -> Task<Message> {
    let task = app.resume_current_find(target, false);
    finish_raw_find_for_test(app, target, task)
}

#[cfg(test)]
fn finish_raw_find_for_test(
    app: &mut Cagents,
    target: Target,
    mut task: Task<Message>,
) -> Task<Message> {
    use iced::futures::StreamExt;
    let executor = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    for _ in 0..64 {
        if app
            .chat_runtime_mut(target)
            .is_some_and(|runtime| runtime.find_navigation.raw.complete)
        {
            let needs_reveal = app.chat_runtime_mut(target).is_some_and(|runtime| {
                !runtime.find_matches.is_empty()
                    && runtime
                        .find_navigation
                        .pending
                        .as_ref()
                        .is_some_and(|pending| pending.key.is_none())
            });
            return if needs_reveal {
                app.resume_current_find(target, false)
            } else {
                task
            };
        }
        let next = executor
            .block_on(async {
                tokio::time::timeout(Duration::from_secs(10), async {
                    let mut stream = iced::runtime::task::into_stream(task)?;
                    while let Some(effect) = stream.next().await {
                        if let iced::runtime::Action::Output(cosmic::Action::App(
                            Message::ChatUi(
                                action @ (Action::FindResume(..)
                                | Action::FindGroups(..)
                                | Action::FindAvailable),
                            ),
                        )) = effect
                        {
                            return Some(action);
                        }
                    }
                    None
                })
                .await
                .expect("real bounded Find group worker completes")
            })
            .expect("Find produces a real continuation");
        task = app.handle_chat_ui(next);
    }
    panic!("Find group preparation did not settle")
}

static METADATA_EPOCH: AtomicU64 = AtomicU64::new(0);
fn metadata_epoch() -> u64 {
    METADATA_EPOCH.load(Ordering::Acquire)
}
fn metadata_capacity() -> &'static Arc<tokio::sync::Semaphore> {
    static METADATA: std::sync::OnceLock<Arc<tokio::sync::Semaphore>> = std::sync::OnceLock::new();
    METADATA.get_or_init(|| Arc::new(tokio::sync::Semaphore::new(32 * 1024 * 1024)))
}
impl Runtime {
    fn fail_find(&mut self, failure: RawFailure) {
        self.find_navigation.raw.failed = Some(failure);
        self.find_navigation.raw.complete = false;
        self.find_navigation.raw.accepted = None;
        self.find_navigation.raw.highlight_context = None;
        HIGHLIGHTS.with(|contexts| {
            contexts.borrow_mut().remove(&self.target);
        });
        self.find_navigation.cancel_navigation();
    }
    pub(super) fn find_error(&self) -> Option<&'static str> {
        match self.find_navigation.raw.failed {
            Some(RawFailure::TooLarge) => Some(
                "Search is too large to finish. Try a shorter query or a smaller conversation.",
            ),
            Some(RawFailure::Worker) => Some("Search could not finish. Retry search."),
            _ => None,
        }
    }
    pub(super) fn find_searching(&self) -> bool {
        self.find.as_ref().is_some_and(|query| !query.is_empty())
            && !self.find_navigation.raw.complete
            && self.find_error().is_none()
    }
}

/// Rendering transports only a body-free accepted-context revision. Actual query text and
/// matching authority stay in the admitted shared Pattern context.
pub(super) fn render_revision(target: Target) -> u64 {
    highlight_context(target).map_or(0, |context| context.render_revision)
}
impl State {
    pub(super) fn search_epoch(&self) -> u64 {
        self.search.load(Ordering::Acquire)
    }
}
