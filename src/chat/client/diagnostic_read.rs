//! A deliberately separate owner for explicit saved diagnostic reads.
//!
//! Queue tickets, worker slots and charged allocation layouts have different
//! lifetimes. Resident text keeps its byte charge; queued completion keeps the
//! worker slot. Last heavy owners are destroyed on the diagnostic reclaimer.
//! The budget counts allocation capacities, not decoded length or process RSS.
use super::*;
use crate::model::CardId;
use std::{
    alloc::{Layout, alloc},
    ops::Deref,
    sync::atomic::AtomicUsize,
    sync::{OnceLock, Weak},
};
use tokio::sync::{Notify, OwnedSemaphorePermit, Semaphore};

const QUEUED_JOBS: usize = 16;
const WORKERS: usize = 2;
const UNIT: usize = 64 * 1024;
const CAPACITY_LIMIT: usize = 128 * 1024 * 1024;
const OPERATION_TIME: Duration = Duration::from_secs(30);
const MAX_SUMMARY: usize = 1024;
pub(super) const POLL_TIME: Duration = Duration::from_millis(5);

/// Must capture a weak source authority, never a transcript/body owner. The
/// final consumer must also validate its current borrowed receiver metadata.
pub(crate) type DiagnosticAuthority = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct DiagnosticSource {
    pub card: CardId,
    pub generation: u64,
    pub turn: u64,
    pub chat: TerminalId,
    pub log: super::super::ProviderExitLog,
}

struct Lane {
    queued: Arc<Semaphore>,
    workers: Arc<Semaphore>,
    bytes: Arc<Semaphore>,
    owned_capacity: AtomicUsize,
    peak_capacity: AtomicUsize,
    retirement_failed: AtomicBool,
    retiring_capacity: AtomicUsize,
    response_owners: AtomicUsize,
}
fn lane() -> &'static Lane {
    static LANE: OnceLock<Lane> = OnceLock::new();
    LANE.get_or_init(|| Lane {
        queued: Arc::new(Semaphore::new(QUEUED_JOBS)),
        workers: Arc::new(Semaphore::new(WORKERS)),
        bytes: Arc::new(Semaphore::new(CAPACITY_LIMIT / UNIT)),
        owned_capacity: AtomicUsize::new(0),
        peak_capacity: AtomicUsize::new(0),
        retirement_failed: AtomicBool::new(false),
        retiring_capacity: AtomicUsize::new(0),
        response_owners: AtomicUsize::new(0),
    })
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DiagnosticResourceUsage {
    pub queued_owners: usize,
    pub worker_slots: usize,
    pub weighted_capacity: usize,
    /// Includes conservative decoded-response reservations, not an RSS value.
    pub reserved_layout_capacity: usize,
    pub peak_reserved_layout_capacity: usize,
    pub retiring_capacity: usize,
    pub response_owners: usize,
    pub retirement_failed: bool,
}
pub(crate) fn diagnostic_resource_usage() -> DiagnosticResourceUsage {
    let lane = lane();
    DiagnosticResourceUsage {
        queued_owners: QUEUED_JOBS - lane.queued.available_permits(),
        worker_slots: WORKERS - lane.workers.available_permits(),
        weighted_capacity: CAPACITY_LIMIT - lane.bytes.available_permits() * UNIT,
        reserved_layout_capacity: lane.owned_capacity.load(Ordering::Acquire),
        peak_reserved_layout_capacity: lane.peak_capacity.load(Ordering::Acquire),
        retiring_capacity: lane.retiring_capacity.load(Ordering::Acquire),
        response_owners: lane.response_owners.load(Ordering::Acquire),
        retirement_failed: lane.retirement_failed.load(Ordering::Acquire),
    }
}
#[cfg(test)]
pub(crate) fn diagnostic_test_guard() -> std::sync::MutexGuard<'static, ()> {
    static GUARD: Mutex<()> = Mutex::new(());
    GUARD.lock().unwrap_or_else(|e| e.into_inner())
}

#[derive(Default)]
struct Interests {
    disclosure: bool,
    copy: Option<u64>,
    next_copy: u64,
}
struct Job {
    source: DiagnosticSource,
    receiver: Weak<Inner>,
    authority: DiagnosticAuthority,
    deadline: Instant,
    interests: Mutex<Interests>,
    cancelled: AtomicBool,
    notify: Notify,
    cancel_tx: crossbeam_channel::Sender<()>,
    cancel_rx: crossbeam_channel::Receiver<()>,
    _queued: OwnedSemaphorePermit,
    _metadata: Charge, // One64KiB unit bounds small source/summary/error metadata.
}
#[derive(Clone)]
pub(crate) struct DiagnosticReadRequest(Arc<Job>);
impl DiagnosticReadRequest {
    /// Identity of the original job, independent of UI-local request counters.
    pub(crate) fn same_job(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
    pub(crate) fn source(&self) -> &DiagnosticSource {
        &self.0.source
    }
    pub(crate) fn cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }
    pub(crate) fn cancel(&self) {
        if !self.0.cancelled.swap(true, Ordering::AcqRel) {
            let _ = self.0.cancel_tx.try_send(());
            self.0.notify.notify_one();
        }
    }
    pub(crate) fn set_disclosed(&self, disclosed: bool) {
        let mut state = self.0.interests.lock().unwrap_or_else(|e| e.into_inner());
        state.disclosure = disclosed;
        let cancelled = !state.disclosure && state.copy.is_none();
        drop(state);
        if cancelled {
            self.cancel();
        }
    }
    /// Each replacement copy gets its own interest generation. Withdrawal of
    /// the old token cannot cancel a newly coalesced copy.
    pub(crate) fn copy_interest(&self) -> Arc<DiagnosticCopyInterest> {
        let mut state = self.0.interests.lock().unwrap_or_else(|e| e.into_inner());
        state.next_copy = state.next_copy.wrapping_add(1);
        let generation = state.next_copy;
        state.copy = Some(generation);
        Arc::new(DiagnosticCopyInterest {
            job: Arc::downgrade(&self.0),
            generation,
        })
    }
    pub(super) fn check(&self) -> Result<(), String> {
        if self.cancelled() {
            return Err("Saved output reading was cancelled".into());
        }
        if Instant::now() >= self.0.deadline {
            return Err("Reading saved output timed out".into());
        }
        if !(self.0.authority)() {
            self.cancel();
            return Err("The conversation changed while its saved output was being read".into());
        }
        Ok(())
    }
    pub(super) fn remaining(&self) -> Result<Duration, String> {
        self.check()?;
        Ok(self.0.deadline.saturating_duration_since(Instant::now()))
    }
    pub(super) fn cancel_receiver(&self) -> &crossbeam_channel::Receiver<()> {
        &self.0.cancel_rx
    }
    pub(super) fn pause(&self) -> Result<(), String> {
        let wait = self.remaining()?.min(POLL_TIME);
        let _ = self.0.cancel_rx.recv_timeout(wait);
        self.check()
    }
    fn matches_receiver(&self, handle: &ClientHandle) -> bool {
        self.0
            .receiver
            .upgrade()
            .is_some_and(|inner| Arc::ptr_eq(&inner, &handle.inner))
    }
    async fn admit(&self) -> Result<Arc<Worker>, String> {
        self.check()?;
        use std::{future::Future, task::Poll};
        let mut acquire = std::pin::pin!(lane().workers.clone().acquire_owned());
        let mut cancelled = std::pin::pin!(self.0.notify.notified());
        let mut deadline = std::pin::pin!(tokio::time::sleep_until(
            tokio::time::Instant::from_std(self.0.deadline)
        ));
        let permit = std::future::poll_fn(|cx| {
            if let Err(error) = self.check() {
                return Poll::Ready(Err(error));
            }
            if let Poll::Ready(permit) = acquire.as_mut().poll(cx) {
                return Poll::Ready(
                    permit.map_err(|_| "Saved output reader is unavailable".to_owned()),
                );
            }
            if cancelled.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err("Saved output reading was cancelled".into()));
            }
            if deadline.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err("Reading saved output timed out".into()));
            }
            Poll::Pending
        })
        .await?;
        self.check()?;
        Ok(Arc::new(Worker { _permit: permit }))
    }
}

/// The UI implements clipboard_intent::Pending for a small wrapper calling
/// withdraw(). This module does not depend on an app-only clipboard hub.
pub(crate) struct DiagnosticCopyInterest {
    job: Weak<Job>,
    generation: u64,
}
impl DiagnosticCopyInterest {
    pub(crate) fn withdraw(&self) {
        let Some(job) = self.job.upgrade() else {
            return;
        };
        let mut state = job.interests.lock().unwrap_or_else(|e| e.into_inner());
        if state.copy != Some(self.generation) {
            return;
        }
        state.copy = None;
        let cancelled = !state.disclosure;
        drop(state);
        if cancelled {
            DiagnosticReadRequest(job).cancel();
        }
    }
}
impl Drop for DiagnosticCopyInterest {
    fn drop(&mut self) {
        self.withdraw();
    }
}

struct Worker {
    _permit: OwnedSemaphorePermit,
}
struct Charge {
    permit: OwnedSemaphorePermit,
    capacity: usize,
}
impl Charge {
    fn reserve(capacity: usize, request: &DiagnosticReadRequest) -> Result<Self, String> {
        request.check()?;
        Self::reserve_capacity(capacity)
    }
    fn reserve_capacity(capacity: usize) -> Result<Self, String> {
        if lane().retirement_failed.load(Ordering::Acquire) {
            return Err("Saved output cleanup is unavailable".into());
        }
        let units = capacity.max(1).div_ceil(UNIT);
        let permit = lane()
            .bytes
            .clone()
            .try_acquire_many_owned(u32::try_from(units).map_err(|_| "Saved output is too large")?)
            .map_err(|_| {
                "Saved output memory is busy; close an output panel or reconnect a stalled conversation, then retry".to_owned()
            })?;
        let current = lane().owned_capacity.fetch_add(capacity, Ordering::AcqRel) + capacity;
        lane().peak_capacity.fetch_max(current, Ordering::AcqRel);
        Ok(Self { permit, capacity })
    }
}
impl Drop for Charge {
    fn drop(&mut self) {
        lane()
            .owned_capacity
            .fetch_sub(self.capacity, Ordering::AcqRel);
    }
}
struct Retired {
    body: Box<dyn Send>,
    charge: Charge,
    worker: Option<Arc<Worker>>,
    request: DiagnosticReadRequest,
}
type Reclaimer = Result<std::sync::mpsc::Sender<Retired>, ()>;
fn start_reclaimer_with(
    spawn: impl FnOnce(Box<dyn FnOnce() + Send>) -> io::Result<()>,
) -> Reclaimer {
    let (tx, rx) = std::sync::mpsc::channel::<Retired>();
    let task = Box::new(move || {
        for value in rx {
            let Retired {
                body,
                charge,
                worker,
                request,
            } = value;
            drop(body); // Return capacity only AFTER the actual allocation.
            lane()
                .retiring_capacity
                .fetch_sub(charge.capacity, Ordering::AcqRel);
            drop(charge);
            drop(worker);
            drop(request);
        }
    });
    spawn(task).map_err(|_| ())?;
    Ok(tx)
}
fn reclaimer() -> &'static Reclaimer {
    static RETIRE: OnceLock<Reclaimer> = OnceLock::new();
    RETIRE.get_or_init(|| {
        start_reclaimer_with(|task| {
            thread::Builder::new()
                .name("diagnostic-reclaim".into())
                .spawn(task)
                .map(|_| ())
        })
    })
}
fn ensure_reclaimer() -> Result<(), String> {
    if reclaimer().is_err() {
        lane().retirement_failed.store(true, Ordering::Release);
        return Err("Saved output cleanup could not start".into());
    }
    Ok(())
}
fn try_retire(value: Retired, reclaimer: &Reclaimer) -> Result<(), Retired> {
    match reclaimer {
        Ok(sender) => sender.send(value).map_err(|error| error.0),
        Err(()) => Err(value),
    }
}
fn retire(value: Retired) {
    lane()
        .retiring_capacity
        .fetch_add(value.charge.capacity, Ordering::AcqRel);
    if let Err(value) = try_retire(value, reclaimer()) {
        // Fail closed even if initialization failed: preserve the ENTIRE
        // body/charge/worker/queue owner. Never unwind/drop heavy data inline.
        lane().retirement_failed.store(true, Ordering::Release);
        std::mem::forget(value);
    }
}

/// A fixed allocation layout, admitted before the allocator is called. This
/// deliberately avoids geometric Vec/String growth and capacity estimates.
struct Buffer<T: Send + 'static> {
    value: Option<T>,
    charge: Option<Charge>,
    worker: Mutex<Option<Arc<Worker>>>,
    request: DiagnosticReadRequest,
}
impl<T: Send + 'static> Deref for Buffer<T> {
    type Target = T;
    fn deref(&self) -> &T {
        self.value.as_ref().unwrap()
    }
}
impl<T: Send + 'static> Buffer<T> {
    fn release_on_worker(mut self) {
        drop(self.value.take());
        drop(self.charge.take());
        drop(
            self.worker
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        );
    }
    fn installed(&self) {
        self.worker.lock().unwrap_or_else(|e| e.into_inner()).take();
    }
}
impl<T: Send + 'static> Drop for Buffer<T> {
    fn drop(&mut self) {
        if let Some(body) = self.value.take() {
            retire(Retired {
                body: Box::new(body),
                charge: self.charge.take().unwrap(),
                worker: self
                    .worker
                    .get_mut()
                    .unwrap_or_else(|e| e.into_inner())
                    .take(),
                request: self.request.clone(),
            });
        }
    }
}
fn allocate_bytes(
    capacity: usize,
    request: &DiagnosticReadRequest,
    worker: &Arc<Worker>,
) -> Result<Buffer<Vec<u8>>, String> {
    request.check()?;
    ensure_reclaimer()?; // Fallible initialization precedes EVERY heavy allocation.
    let charge = Charge::reserve(capacity, request)?;
    let bytes = if capacity == 0 {
        Vec::new()
    } else {
        let layout =
            Layout::array::<u8>(capacity).map_err(|_| "Saved output allocation is too large")?;
        // SAFETY: Global's exact requested layout is also the Vec deallocation
        // layout. No elements are initialized, length is zero, and all append
        // sites check the fixed capacity before writing. The buffer never grows.
        let pointer = unsafe { alloc(layout) };
        if pointer.is_null() {
            return Err("Not enough memory to load saved output".into());
        }
        unsafe { Vec::from_raw_parts(pointer, 0, capacity) }
    };
    debug_assert_eq!(bytes.capacity(), charge.capacity);
    Ok(Buffer {
        value: Some(bytes),
        charge: Some(charge),
        worker: Mutex::new(Some(worker.clone())),
        request: request.clone(),
    })
}
fn allocate_text(
    capacity: usize,
    request: &DiagnosticReadRequest,
    worker: &Arc<Worker>,
) -> Result<Buffer<String>, String> {
    let mut bytes = allocate_bytes(capacity, request, worker)?;
    let text = String::from_utf8(bytes.value.take().unwrap()).unwrap(); // Empty, valid UTF-8.
    Ok(Buffer {
        value: Some(text),
        charge: bytes.charge.take(),
        worker: Mutex::new(
            bytes
                .worker
                .get_mut()
                .unwrap_or_else(|e| e.into_inner())
                .take(),
        ),
        request: request.clone(),
    })
}

#[derive(Clone)]
pub(crate) struct DiagnosticOutput {
    text: Arc<Buffer<String>>,
    omitted: u64,
    source: DiagnosticSource,
}
impl DiagnosticOutput {
    pub(crate) fn as_str(&self) -> &str {
        self.text.as_str()
    }
    pub(crate) fn capacity(&self) -> usize {
        self.text.capacity()
    }
    pub(crate) fn omitted(&self) -> u64 {
        self.omitted
    }
    pub(crate) fn source(&self) -> &DiagnosticSource {
        &self.source
    }
    /// Cached text belongs to its ORIGINAL receiver/source authority. A new
    /// copy deadline does not authorize an old body on a replacement receiver.
    pub(crate) fn current_for(&self, request: &DiagnosticReadRequest) -> bool {
        self.source == *request.source()
            && Weak::ptr_eq(&self.text.request.0.receiver, &request.0.receiver)
            && (self.text.request.0.authority)()
    }
}
#[derive(Clone)]
pub(crate) struct DiagnosticCompletion {
    request: DiagnosticReadRequest,
    inner: Arc<Mutex<Option<Finished>>>,
}
struct Finished {
    result: Result<DiagnosticOutput, String>,
    _worker: Option<Arc<Worker>>,
}
impl DiagnosticCompletion {
    fn new(
        request: DiagnosticReadRequest,
        result: Result<DiagnosticOutput, String>,
        worker: Option<Arc<Worker>>,
    ) -> Self {
        Self {
            request,
            inner: Arc::new(Mutex::new(Some(Finished {
                result,
                _worker: worker,
            }))),
        }
    }
    pub(crate) fn request(&self) -> &DiagnosticReadRequest {
        &self.request
    }
    /// Single consumption is safe even when iced clones the Action. This is
    /// called only after the blocking worker actually joined. Resident byte
    /// and queue charges stay with the body after its worker slot is released.
    pub(crate) fn take_result(&self) -> Option<Result<DiagnosticOutput, String>> {
        let finished = self
            .inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        if let Ok(output) = &finished.result {
            output.text.installed();
        }
        Some(finished.result)
    }
}

#[derive(Clone)]
pub(crate) struct DiagnosticCopyReceipt(Arc<CopyReceipt>);
struct CopyReceipt {
    _charge: Charge,
    _worker: Option<Arc<Worker>>,
    _request: DiagnosticReadRequest,
}
#[derive(Clone)]
pub(crate) struct DiagnosticCopyCompletion {
    request: DiagnosticReadRequest,
    result: Arc<Mutex<Option<Result<Buffer<String>, String>>>>,
}
impl DiagnosticCopyCompletion {
    fn new(request: DiagnosticReadRequest, result: Result<Buffer<String>, String>) -> Self {
        Self {
            request,
            result: Arc::new(Mutex::new(Some(result))),
        }
    }
    pub(crate) fn request(&self) -> &DiagnosticReadRequest {
        &self.request
    }
    /// The receipt remains in the chained application acknowledgement until
    /// iced executes the clipboard write; never drop it at task construction.
    pub(crate) fn take_effect(&self) -> Option<Result<(String, DiagnosticCopyReceipt), String>> {
        let result = self
            .result
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()?;
        Some(result.map(|mut value| {
            let text = value.value.take().unwrap();
            let receipt = DiagnosticCopyReceipt(Arc::new(CopyReceipt {
                _charge: value.charge.take().unwrap(),
                _worker: value
                    .worker
                    .get_mut()
                    .unwrap_or_else(|e| e.into_inner())
                    .take(),
                _request: self.request.clone(),
            }));
            (text, receipt)
        }))
    }
}

/// Small metadata is validated BEFORE async admission: never queue an
/// arbitrary spare-capacity String alongside a waiting copy job.
#[derive(Clone)]
pub(crate) struct DiagnosticCopySummary(Arc<str>);
impl DiagnosticCopySummary {
    pub(crate) fn new(summary: &str) -> Result<Self, String> {
        if summary.len() > MAX_SUMMARY {
            return Err("The saved output summary is too large".into());
        }
        Ok(Self(Arc::from(summary)))
    }
    pub(crate) fn as_str(&self) -> &str {
        &self.0
    }
}

impl ClientHandle {
    pub(crate) fn diagnostic_request(
        &self,
        card: CardId,
        generation: u64,
        turn: u64,
        log: &super::super::ProviderExitLog,
        disclosed: bool,
        copy: bool,
        authority: DiagnosticAuthority,
    ) -> Result<DiagnosticReadRequest, String> {
        // Each posted diagnostic request clones this authenticated proof.
        // Bound its borrowed fields before admission/serialization, so the
        // per-job metadata reservation covers those diagnostic-local clones.
        let proof = &self.inner.proof;
        if [
            proof.host_session_id.as_str(),
            proof.boot_id.as_str(),
            proof.host_incarnation.as_str(),
        ]
        .into_iter()
        .any(|field| field.len() > 256)
        {
            return Err(
                "Saved output is unavailable on this connection; reconnect the conversation".into(),
            );
        }
        if log.bytes > provider_exit::MAX_OUTPUT_BYTES as u64
            || log.id.is_empty()
            || log.id.len() > 256
        {
            return Err("The retained provider output is too large or unavailable".into());
        }
        let queued = lane().queued.clone().try_acquire_owned().map_err(|_| {
            "Too many saved output requests; close an output panel or reconnect a stalled conversation, then retry".to_owned()
        })?;
        let metadata = Charge::reserve_capacity(0)?;
        let log = super::super::ProviderExitLog {
            id: log.id.as_str().to_owned(),
            bytes: log.bytes,
            omitted: log.omitted,
        };
        let (cancel_tx, cancel_rx) = crossbeam_channel::bounded(1);
        let request = DiagnosticReadRequest(Arc::new(Job {
            source: DiagnosticSource {
                card,
                generation,
                turn,
                chat: self.inner.id,
                log,
            },
            receiver: Arc::downgrade(&self.inner),
            authority,
            deadline: Instant::now() + OPERATION_TIME,
            interests: Mutex::new(Interests {
                disclosure: disclosed,
                copy: copy.then_some(0),
                next_copy: 0,
            }),
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
            cancel_tx,
            cancel_rx,
            _queued: queued,
            _metadata: metadata,
        }));
        request.check()?;
        if !disclosed && !copy {
            request.cancel();
        }
        Ok(request)
    }
    pub(crate) async fn read_provider_exit_job(
        &self,
        request: DiagnosticReadRequest,
    ) -> DiagnosticCompletion {
        if !request.matches_receiver(self) {
            return DiagnosticCompletion::new(
                request,
                Err("The saved output belongs to a different conversation".into()),
                None,
            );
        }
        let worker = match request.admit().await {
            Ok(worker) => worker,
            Err(error) => return DiagnosticCompletion::new(request, Err(error), None),
        };
        let handle = self.clone();
        let job = request.clone();
        let running = worker.clone();
        let result = tokio::task::spawn_blocking(move || handle.read_diagnostic(&job, &running))
            .await
            .map_err(|_| "Saved output reader could not finish".to_owned())
            .and_then(|result| result);
        DiagnosticCompletion::new(request, result, Some(worker))
    }
    pub(crate) async fn copy_provider_exit_job(
        &self,
        request: DiagnosticReadRequest,
        output: DiagnosticOutput,
        summary: DiagnosticCopySummary,
    ) -> DiagnosticCopyCompletion {
        if !request.matches_receiver(self) || !output.current_for(&request) {
            return DiagnosticCopyCompletion::new(
                request,
                Err("The saved output belongs to a different conversation".into()),
            );
        }
        let worker = match request.admit().await {
            Ok(worker) => worker,
            Err(error) => return DiagnosticCopyCompletion::new(request, Err(error)),
        };
        let job = request.clone();
        let result = tokio::task::spawn_blocking(move || {
            copy_output(&job, &worker, output, summary.as_str())
        })
        .await
        .map_err(|_| "Saved output copy could not finish".to_owned())
        .and_then(|result| result);
        DiagnosticCopyCompletion::new(request, result)
    }
}

fn copy_output(
    request: &DiagnosticReadRequest,
    worker: &Arc<Worker>,
    output: DiagnosticOutput,
    summary: &str,
) -> Result<Buffer<String>, String> {
    request.check()?;
    if !output.current_for(request) {
        return Err("The cached saved output belongs to an older conversation source".into());
    }
    if summary.len() > MAX_SUMMARY {
        return Err("The saved output summary is too large".into());
    }
    let suffix = if output.omitted == 0 {
        String::new()
    } else {
        format!(
            "\n\n[{} raw output bytes were not retained]",
            output.omitted
        )
    };
    let capacity = summary
        .len()
        .checked_add(2)
        .and_then(|n| n.checked_add(output.as_str().len()))
        .and_then(|n| n.checked_add(suffix.len()))
        .ok_or("Saved output copy is too large")?;
    let mut text = allocate_text(capacity, request, worker)?;
    let value = text.value.as_mut().unwrap();
    value.push_str(summary);
    value.push_str("\n\n");
    value.push_str(output.as_str());
    value.push_str(&suffix);
    debug_assert_eq!(value.capacity(), capacity);
    request.check()?;
    if !output.current_for(request) {
        return Err("The conversation changed while its saved output was copied".into());
    }
    Ok(text)
}

pub(super) struct DiagnosticResponseCharge {
    _charge: Charge,
    _request: DiagnosticReadRequest,
}
impl DiagnosticResponseCharge {
    fn new(charge: Charge, request: DiagnosticReadRequest) -> Self {
        lane().response_owners.fetch_add(1, Ordering::AcqRel);
        Self {
            _charge: charge,
            _request: request,
        }
    }
}
impl Drop for DiagnosticResponseCharge {
    fn drop(&mut self) {
        lane().response_owners.fetch_sub(1, Ordering::AcqRel);
    }
}

impl ClientHandle {
    fn read_diagnostic(
        &self,
        request: &DiagnosticReadRequest,
        worker: &Arc<Worker>,
    ) -> Result<DiagnosticOutput, String> {
        request.check()?;
        let log = &request.source().log;
        let mut bytes = allocate_bytes(log.bytes as usize, request, worker)?;
        let mut offset = 0;
        loop {
            request.check()?;
            let page = self.diagnostic_page(request, offset)?;
            let capacity = bytes.capacity();
            let next = provider_exit::append_page(bytes.value.as_mut().unwrap(), log, page)?;
            assert_eq!(
                bytes.capacity(),
                capacity,
                "validated diagnostic append must not grow its allocation"
            );
            match next {
                Some(next) => offset = next,
                None => break,
            }
        }
        request.check()?;
        let text = normalize(bytes, request, worker)?;
        request.check()?;
        Ok(DiagnosticOutput {
            text: Arc::new(text),
            omitted: log.omitted,
            source: request.source().clone(),
        })
    }

    fn diagnostic_page(
        &self,
        request: &DiagnosticReadRequest,
        offset: u64,
    ) -> Result<provider_exit::Page, String> {
        request.check()?;
        if !self.inner.alive.load(Ordering::Acquire) {
            return Err("Reconnect this conversation before loading its saved output".into());
        }
        // The authenticated frame decoder is an existing connection owner.
        // Reserve for its entire permitted decoded page BEFORE requesting it;
        // transfer/retire the actual Vec before returning this charge.
        let page_charge = Charge::reserve(ipc::MAX_FRAME_BYTES, request)?;
        let id = next_request_id(&self.inner.next)? | provider_exit::REQUEST_NAMESPACE;
        let (reply, response) = crossbeam_channel::bounded(1);
        {
            let mut pending = self
                .inner
                .diagnostic_pending
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if pending.len() >= 8 {
                return Err("Provider output requests are busy; try again shortly".into());
            }
            pending.insert(
                id,
                provider_exit::PendingReply {
                    reply: Some(reply),
                    ownership: Some(DiagnosticResponseCharge::new(page_charge, request.clone())),
                },
            );
        }
        let posted = std::cell::Cell::new(false);
        struct PendingPage<'a> {
            pending: &'a provider_exit::Pending,
            id: u64,
            posted: &'a std::cell::Cell<bool>,
        }
        impl Drop for PendingPage<'_> {
            fn drop(&mut self) {
                let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
                if self.posted.get() {
                    if let Some(retired) = pending.get_mut(&self.id) {
                        retired.reply = None;
                    }
                    // A typed retired tombstone owns admission until the
                    // actual late decoded reply or reader disconnect drains it.
                } else {
                    pending.remove(&self.id);
                }
            }
        }
        let _pending = PendingPage {
            pending: &self.inner.diagnostic_pending,
            id,
            posted: &posted,
        };
        self.transmit_diagnostic(
            request,
            id,
            ClientRequest::ChatProviderExitOutput {
                chat_id: self.inner.id,
                log_id: request.source().log.id.clone(),
                offset,
            },
            &posted,
        )?;
        let wait = request.remaining()?.min(Duration::from_secs(5));
        let page = crossbeam_channel::select! {
            recv(request.cancel_receiver()) -> _ => return Err("Saved output reading was cancelled".into()),
            recv(response) -> page => page.map_err(|_|"The saved output connection closed".to_owned())??,
            default(wait) => return Err("Loading provider output timed out".into()),
        };
        request.check()?;
        if page.bytes.capacity() > ipc::MAX_FRAME_BYTES {
            return Err("The daemon returned an oversized saved output allocation".into());
        }
        Ok(page)
    }

    fn transmit_diagnostic(
        &self,
        request: &DiagnosticReadRequest,
        id: u64,
        message: ClientRequest,
        posted: &std::cell::Cell<bool>,
    ) -> Result<(), String> {
        // Diagnostic requests have small bounded metadata. A stack frame avoids
        // a second geometrically grown codec buffer and never mutates the
        // connection codec's retained scratch capacity.
        let mut frame = [0u8; 16 * 1024];
        request.check()?;
        let payload = postcard::to_slice(
            &ClientEnvelope {
                version: PROTOCOL_VERSION,
                request_id: id,
                proof: Some(self.inner.proof.clone()),
                request: message,
            },
            &mut frame[4..],
        )
        .map_err(|_| "Saved output request metadata is too large".to_owned())?
        .len();
        frame[..4].copy_from_slice(&(payload as u32).to_be_bytes());
        let wire = lock_wire(&self.inner.wire, request)?;
        send_frame(&wire.stream, &frame[..payload + 4], request)?;
        posted.set(true);
        request.check()
    }
}

fn lock_wire<'a>(
    wire: &'a Mutex<Wire>,
    request: &DiagnosticReadRequest,
) -> Result<std::sync::MutexGuard<'a, Wire>, String> {
    loop {
        request.check()?;
        match wire.try_lock() {
            Ok(wire) => return Ok(wire),
            Err(std::sync::TryLockError::Poisoned(error)) => return Ok(error.into_inner()),
            Err(std::sync::TryLockError::WouldBlock) => request.pause()?,
        }
    }
}

fn send_frame(
    stream: &UnixStream,
    frame: &[u8],
    request: &DiagnosticReadRequest,
) -> Result<(), String> {
    use std::os::fd::AsRawFd;
    let mut sent = 0;
    while sent < frame.len() {
        if sent == 0 {
            request.check()?;
        }
        let remaining = request.0.deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            if sent != 0 {
                let _ = stream.shutdown(Shutdown::Both);
            }
            return Err("Writing the saved output request timed out".into());
        }
        // SAFETY: the stream owns a live fd; the borrowed frame slice remains
        // initialized/live throughout send. Per-call nonblocking flags do not
        // change the shared socket's flags or other clients' timeouts.
        let wrote = unsafe {
            libc::send(
                stream.as_raw_fd(),
                frame[sent..].as_ptr().cast(),
                frame.len() - sent,
                libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
            )
        };
        if wrote > 0 {
            sent += wrote as usize;
            continue;
        }
        let error = if wrote == 0 {
            io::Error::from(io::ErrorKind::WriteZero)
        } else {
            io::Error::last_os_error()
        };
        match error.kind() {
            io::ErrorKind::Interrupted => continue,
            io::ErrorKind::WouldBlock => {
                if sent == 0 {
                    request.pause()?;
                } else {
                    // A partial frame must finish or close the connection;
                    // cancelling midway must never corrupt its next frame.
                    // Whole-operation deadline still applies to every send.
                    thread::sleep(remaining.min(POLL_TIME));
                }
            }
            _ => {
                if sent != 0 {
                    let _ = stream.shutdown(Shutdown::Both);
                }
                return Err(format!("Could not write the saved output request: {error}"));
            }
        }
    }
    Ok(())
}

fn push_fixed(value: &mut Buffer<String>, text: &str) -> Result<(), String> {
    let output = value.value.as_mut().unwrap();
    if text.len() > output.capacity() - output.len() {
        return Err("Saved output normalization exceeded its admitted allocation".into());
    }
    output.push_str(text);
    Ok(())
}
fn normalize(
    mut bytes: Buffer<Vec<u8>>,
    request: &DiagnosticReadRequest,
    worker: &Arc<Worker>,
) -> Result<Buffer<String>, String> {
    request.check()?;
    // Reuse already-valid UTF-8's fixed raw layout and its original charge.
    let raw = bytes.value.take().unwrap();
    let mut decoded = match String::from_utf8(raw) {
        Ok(text) => Buffer {
            value: Some(text),
            charge: bytes.charge.take(),
            worker: Mutex::new(
                bytes
                    .worker
                    .get_mut()
                    .unwrap_or_else(|e| e.into_inner())
                    .take(),
            ),
            request: request.clone(),
        },
        Err(error) => {
            bytes.value = Some(error.into_bytes());
            let capacity = bytes
                .len()
                .checked_mul(3)
                .ok_or("Saved output is too large")?;
            let mut decoded = allocate_text(capacity, request, worker)?;
            let mut remaining = bytes.as_slice();
            let mut checked_at = remaining.len();
            while !remaining.is_empty() {
                if checked_at.saturating_sub(remaining.len()) >= 4096 {
                    request.check()?;
                    checked_at = remaining.len();
                }
                match std::str::from_utf8(remaining) {
                    Ok(valid) => {
                        push_fixed(&mut decoded, valid)?;
                        break;
                    }
                    Err(error) => {
                        let valid = error.valid_up_to();
                        push_fixed(
                            &mut decoded,
                            std::str::from_utf8(&remaining[..valid]).unwrap(),
                        )?;
                        push_fixed(&mut decoded, "\u{fffd}")?;
                        remaining = &remaining
                            [valid + error.error_len().unwrap_or(remaining.len() - valid)..];
                    }
                }
            }
            bytes.release_on_worker();
            decoded
        }
    };
    request.check()?;
    if !decoded
        .bytes()
        .any(|byte| (byte < 0x20 && !matches!(byte, b'\n' | b'\t')) || byte == 0x7f)
    {
        decoded
            .value
            .as_mut()
            .unwrap()
            .retain(|c| !c.is_control() || matches!(c, '\n' | '\t'));
        return Ok(decoded);
    }
    let mut text = allocate_text(decoded.len(), request, worker)?;
    super::super::terminal_output::strip(&decoded, text.value.as_mut().unwrap());
    assert_eq!(text.capacity(), text.charge.as_ref().unwrap().capacity);
    decoded.release_on_worker();
    request.check()?;
    let mut shown = allocate_text(text.len(), request, worker)?;
    let (mut line, mut returned) = (0, false);
    for character in text.chars() {
        match character {
            '\r' => returned = true,
            '\n' => {
                push_fixed(&mut shown, "\n")?;
                line = shown.len();
                returned = false;
            }
            character => {
                if std::mem::take(&mut returned) {
                    shown.value.as_mut().unwrap().truncate(line);
                }
                let mut encoded = [0; 4];
                push_fixed(&mut shown, character.encode_utf8(&mut encoded))?;
            }
        }
    }
    text.release_on_worker();
    shown
        .value
        .as_mut()
        .unwrap()
        .retain(|c| !c.is_control() || matches!(c, '\n' | '\t'));
    request.check()?;
    Ok(shown)
}

#[cfg(test)]
impl ClientHandle {
    pub(crate) fn fixture_diagnostic_completion(
        &self,
        card: CardId,
        generation: u64,
        turn: u64,
        log: super::super::ProviderExitLog,
        body: &[u8],
        authority: DiagnosticAuthority,
    ) -> Result<(DiagnosticReadRequest, DiagnosticCompletion), String> {
        if log.bytes != body.len() as u64 {
            return Err("Fixture body/log size mismatch".into());
        }
        let request =
            self.diagnostic_request(card, generation, turn, &log, true, false, authority)?;
        let worker = Arc::new(Worker {
            _permit: lane()
                .workers
                .clone()
                .try_acquire_owned()
                .map_err(|_| "Fixture readers busy")?,
        });
        let running = worker.clone();
        let job = request.clone();
        let body = body.to_vec();
        let result = thread::spawn(move || {
            let mut raw = allocate_bytes(body.len(), &job, &running)?;
            raw.value.as_mut().unwrap().extend_from_slice(&body);
            let text = normalize(raw, &job, &running)?;
            Ok(DiagnosticOutput {
                text: Arc::new(text),
                omitted: job.source().log.omitted,
                source: job.source().clone(),
            })
        })
        .join()
        .map_err(|_| "Fixture reader panicked".to_owned())?;
        Ok((
            request.clone(),
            DiagnosticCompletion::new(request, result, Some(worker)),
        ))
    }
}
#[cfg(test)]
impl DiagnosticCompletion {
    pub(crate) fn contains_text_for_test(&self, expected: &str) -> bool {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|finished| {
                finished
                    .result
                    .as_ref()
                    .is_ok_and(|output| output.as_str().contains(expected))
            })
    }
}
#[cfg(test)]
impl DiagnosticCopyCompletion {
    pub(crate) fn contains_text_for_test(&self, expected: &str) -> bool {
        self.result
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .is_some_and(|result| {
                result
                    .as_ref()
                    .is_ok_and(|buffer| buffer.contains(expected))
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // Run this owning filter serially: metrics and admission are process-wide.
    fn request(time: Duration, disclosed: bool, copy: bool) -> DiagnosticReadRequest {
        let (cancel_tx, cancel_rx) = crossbeam_channel::bounded(1);
        DiagnosticReadRequest(Arc::new(Job {
            source: DiagnosticSource {
                card: CardId::new(),
                generation: 7,
                turn: 3,
                chat: TerminalId::new(),
                log: super::super::super::ProviderExitLog {
                    id: "test-log".into(),
                    bytes: 0,
                    omitted: 0,
                },
            },
            receiver: Weak::new(),
            authority: Arc::new(|| true),
            deadline: Instant::now() + time,
            interests: Mutex::new(Interests {
                disclosure: disclosed,
                copy: copy.then_some(0),
                next_copy: 0,
            }),
            cancelled: AtomicBool::new(false),
            notify: Notify::new(),
            cancel_tx,
            cancel_rx,
            _queued: lane().queued.clone().try_acquire_owned().unwrap(),
            _metadata: Charge::reserve_capacity(0).unwrap(),
        }))
    }
    fn worker() -> Arc<Worker> {
        Arc::new(Worker {
            _permit: lane().workers.clone().try_acquire_owned().unwrap(),
        })
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }
    fn wait_capacity(expected: usize) {
        let until = Instant::now() + Duration::from_secs(5);
        while (lane().owned_capacity.load(Ordering::Acquire) != expected
            || (expected == 0 && lane().workers.available_permits() != WORKERS))
            && Instant::now() < until
        {
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(lane().owned_capacity.load(Ordering::Acquire), expected);
    }
    fn raw(value: &[u8], job: &DiagnosticReadRequest, worker: &Arc<Worker>) -> Buffer<Vec<u8>> {
        let mut raw = allocate_bytes(value.len(), job, worker).unwrap();
        raw.value.as_mut().unwrap().extend_from_slice(value);
        assert_eq!(raw.capacity(), value.len());
        raw
    }
    fn rss_kib() -> Option<u64> {
        std::fs::read_to_string("/proc/self/status")
            .ok()?
            .lines()
            .find_map(|line| {
                line.strip_prefix("VmRSS:")
                    .and_then(|value| value.split_whitespace().next()?.parse().ok())
            })
    }
    #[test]
    fn cancelled_before_admission_reserves_no_raw_capacity() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        let job = request(Duration::from_secs(1), false, true);
        let metadata_only_permits = lane().bytes.available_permits();
        assert_eq!(metadata_only_permits, CAPACITY_LIMIT / UNIT - 1);
        job.cancel();
        assert!(runtime().block_on(job.admit()).is_err());
        let worker = worker();
        assert!(allocate_bytes(provider_exit::MAX_OUTPUT_BYTES, &job, &worker).is_err());
        assert_eq!(lane().owned_capacity.load(Ordering::Acquire), 0);
        assert_eq!(lane().bytes.available_permits(), metadata_only_permits);
        drop(worker);
        drop(job);
        wait_capacity(0);
        assert_eq!(lane().bytes.available_permits(), CAPACITY_LIMIT / UNIT);
    }
    #[test]
    fn queued_cancel_wakes_before_worker_and_never_allocates() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        let first = worker();
        let second = worker();
        let job = request(Duration::from_secs(2), false, true);
        let other = job.clone();
        let canceller = thread::spawn(move || {
            thread::sleep(Duration::from_millis(10));
            other.cancel();
        });
        let started = Instant::now();
        assert!(runtime().block_on(job.admit()).is_err());
        canceller.join().unwrap();
        assert!(started.elapsed() < Duration::from_secs(1));
        assert_eq!(lane().owned_capacity.load(Ordering::Acquire), 0);
        drop(first);
        drop(second);
    }
    #[test]
    fn queue_and_byte_admission_reject_before_allocator() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        let mut jobs = Vec::new();
        while lane().queued.available_permits() > 0 {
            jobs.push(request(Duration::from_secs(1), false, true));
        }
        assert!(lane().queued.clone().try_acquire_owned().is_err());
        let job = jobs.first().unwrap();
        let worker = worker();
        let admitted = lane().bytes.available_permits() * UNIT;
        let all = Charge::reserve(admitted, job).unwrap();
        assert!(allocate_bytes(1, job, &worker).is_err());
        assert_eq!(lane().owned_capacity.load(Ordering::Acquire), admitted);
        drop(all);
        drop(jobs);
        drop(worker);
        wait_capacity(0);
    }
    #[test]
    fn latest_copy_interest_withdrawal_keeps_visible_disclosure_and_ignores_old_token() {
        let _guard = diagnostic_test_guard();
        let job = request(Duration::from_secs(1), true, true);
        let old = job.copy_interest();
        let latest = job.copy_interest();
        old.withdraw();
        drop(old);
        assert!(!job.cancelled());
        latest.withdraw();
        assert!(!job.cancelled());
        job.set_disclosed(false);
        assert!(job.cancelled());
        let replacement = request(Duration::from_secs(1), false, true);
        let copy = replacement.copy_interest();
        copy.withdraw();
        assert!(replacement.cancelled());
    }
    #[test]
    fn delayed_wire_mutex_obeys_whole_operation_deadline_and_cancel() {
        let _guard = diagnostic_test_guard();
        for cancel in [false, true] {
            let (stream, _peer) = UnixStream::pair().unwrap();
            let wire = Arc::new(Mutex::new(Wire {
                stream,
                codec: FrameCodec::new(ipc::MAX_FRAME_BYTES),
            }));
            let held = wire.lock().unwrap();
            let job = request(Duration::from_millis(40), false, true);
            let other_wire = wire.clone();
            let other_job = job.clone();
            let waiter =
                thread::spawn(move || lock_wire(&other_wire, &other_job).map(|_| ()).is_err());
            if cancel {
                job.cancel();
            }
            assert!(waiter.join().unwrap());
            drop(held);
            assert_eq!(lane().owned_capacity.load(Ordering::Acquire), 0);
        }
    }
    #[test]
    fn full_socket_write_obeys_deadline_without_changing_shared_flags() {
        let _guard = diagnostic_test_guard();
        use std::os::fd::AsRawFd;
        let (stream, _peer) = UnixStream::pair().unwrap();
        let chunk = [0u8; 8192];
        loop {
            // SAFETY: valid owned fd and initialized borrowed chunk.
            let result = unsafe {
                libc::send(
                    stream.as_raw_fd(),
                    chunk.as_ptr().cast(),
                    chunk.len(),
                    libc::MSG_DONTWAIT | libc::MSG_NOSIGNAL,
                )
            };
            if result < 0 {
                assert_eq!(io::Error::last_os_error().kind(), io::ErrorKind::WouldBlock);
                break;
            }
        }
        // SAFETY: F_GETFL reads flags on this live owned descriptor.
        let flags = unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) };
        let job = request(Duration::from_millis(30), false, true);
        let began = Instant::now();
        assert!(send_frame(&stream, b"bounded-request", &job).is_err());
        assert!(began.elapsed() < Duration::from_secs(1));
        assert_eq!(
            unsafe { libc::fcntl(stream.as_raw_fd(), libc::F_GETFL) },
            flags
        );
    }
    #[test]
    fn fixed_capacity_lossy_cleanup_matches_sanitizer_and_reports_real_layouts() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        lane().peak_capacity.store(0, Ordering::Release);
        let before = rss_kib();
        let job = request(Duration::from_secs(30), true, false);
        let worker = worker();
        let mut input = vec![0xff; 1024 * 1024];
        input.extend_from_slice(b"\x1b[31mred\x1b[0m\rshown\n\x1b]unfinished");
        let expected = provider_exit::readable(input.clone());
        let text = normalize(raw(&input, &job, &worker), &job, &worker).unwrap();
        assert_eq!(text.as_str(), expected);
        assert!(text.capacity() <= input.len() * 3);
        let peak = lane().peak_capacity.load(Ordering::Acquire);
        assert!(peak <= input.len() * 6); // Each predecessor is destroyed before next cleanup stage.
        println!(
            "diagnostic-capacity raw={} final_capacity={} peak_charged_layout={} rss_before_kib={before:?} rss_after_kib={:?} (unit-process observation, not native/RSS ceiling)",
            input.len(),
            text.capacity(),
            peak,
            rss_kib()
        );
        text.release_on_worker();
        drop(worker);
        drop(job);
        wait_capacity(0);
    }
    #[test]
    fn completed_body_is_charged_until_real_last_owner_retirement() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        let job = request(Duration::from_secs(1), true, false);
        let worker = worker();
        let text = normalize(raw(b"saved body", &job, &worker), &job, &worker).unwrap();
        let capacity = text.capacity();
        let output = DiagnosticOutput {
            text: Arc::new(text),
            omitted: 0,
            source: job.source().clone(),
        };
        let completion = DiagnosticCompletion::new(job.clone(), Ok(output), Some(worker.clone()));
        let duplicate = completion.clone();
        let output = completion.take_result().unwrap().unwrap();
        assert!(duplicate.take_result().is_none());
        let second = output.clone();
        drop(output);
        assert_eq!(lane().owned_capacity.load(Ordering::Acquire), capacity);
        drop(second);
        drop(completion);
        drop(duplicate);
        drop(worker);
        drop(job);
        wait_capacity(0);
    }
    #[test]
    fn copy_completion_and_effect_receipt_keep_capacity_until_actual_release() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        let job = request(Duration::from_secs(2), true, true);
        let worker = worker();
        let text = normalize(raw(b"faithful output", &job, &worker), &job, &worker).unwrap();
        text.installed();
        let resident = text.capacity();
        let output = DiagnosticOutput {
            text: Arc::new(text),
            omitted: 17,
            source: job.source().clone(),
        };
        let copy = copy_output(&job, &worker, output.clone(), "Readable failure").unwrap();
        let copied = copy.capacity();
        let completion = DiagnosticCopyCompletion::new(job.clone(), Ok(copy));
        let duplicate = completion.clone();
        let (string, receipt) = completion.take_effect().unwrap().unwrap();
        assert!(duplicate.take_effect().is_none());
        assert!(string.ends_with("[17 raw output bytes were not retained]"));
        assert_eq!(
            lane().owned_capacity.load(Ordering::Acquire),
            resident + copied
        );
        drop(string);
        assert_eq!(
            lane().owned_capacity.load(Ordering::Acquire),
            resident + copied
        );
        drop(receipt);
        assert_eq!(lane().owned_capacity.load(Ordering::Acquire), resident);
        drop(output);
        drop(completion);
        drop(duplicate);
        drop(job);
        drop(worker);
        wait_capacity(0);
    }
    #[test]
    fn cancelled_queued_completion_retires_buffer_without_publishing_or_leaking_charge() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        let job = request(Duration::from_secs(2), false, true);
        let worker = worker();
        let mut text = allocate_text(UNIT, &job, &worker).unwrap();
        push_fixed(&mut text, "saved").unwrap();
        let completion = DiagnosticCopyCompletion::new(job.clone(), Ok(text));
        job.cancel();
        drop(completion);
        drop(worker);
        drop(job);
        wait_capacity(0);
    }
    #[test]
    fn reclaimer_spawn_failure_preserves_last_body_and_all_charges() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        struct Probe {
            _bytes: Vec<u8>,
            dropped: Arc<AtomicUsize>,
        }
        impl Drop for Probe {
            fn drop(&mut self) {
                self.dropped.fetch_add(1, Ordering::AcqRel);
            }
        }
        let failed = start_reclaimer_with(|_task| Err(io::Error::other("injected spawn failure")));
        assert!(failed.is_err());
        let job = request(Duration::from_secs(1), false, true);
        let worker = worker();
        let charge = Charge::reserve(UNIT, &job).unwrap();
        let dropped = Arc::new(AtomicUsize::new(0));
        let value = Retired {
            body: Box::new(Probe {
                _bytes: vec![0; UNIT],
                dropped: dropped.clone(),
            }),
            charge,
            worker: Some(worker.clone()),
            request: job.clone(),
        };
        let retained = match try_retire(value, &failed) {
            Ok(()) => panic!("failed reclaimer cannot consume owner"),
            Err(value) => value,
        };
        assert_eq!(dropped.load(Ordering::Acquire), 0);
        assert_eq!(lane().owned_capacity.load(Ordering::Acquire), UNIT);
        // Test cleanup explicitly happens on a joined worker; production fail
        // closed instead retains this entire charged owner until process exit.
        thread::spawn(move || drop(retained)).join().unwrap();
        assert_eq!(dropped.load(Ordering::Acquire), 1);
        drop(job);
        drop(worker);
        wait_capacity(0);
    }
    #[test]
    fn summary_is_bounded_before_async_admission_even_with_large_input_capacity() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        let mut oversized_capacity = String::with_capacity(8 * 1024 * 1024);
        oversized_capacity.push_str("Readable failure");
        let summary = DiagnosticCopySummary::new(&oversized_capacity).unwrap();
        drop(oversized_capacity);
        assert_eq!(summary.as_str(), "Readable failure");
        assert!(DiagnosticCopySummary::new(&"€".repeat(512)).is_err());
        assert_eq!(lane().owned_capacity.load(Ordering::Acquire), 0);
    }
    #[test]
    fn ready_body_copy_keeps_original_receiver_and_authority_with_fresh_deadline() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        let session = super::super::tests::omp_card(None, None);
        let original = ClientHandle::fixture(session.clone(), Snapshot::default());
        let replacement = ClientHandle::fixture(session, Snapshot::default());
        let card = CardId::new();
        let log = super::super::super::ProviderExitLog {
            id: "same-metadata".into(),
            bytes: 4,
            omitted: 0,
        };
        let original_current = Arc::new(AtomicBool::new(true));
        let original_check = original_current.clone();
        let mut old = original
            .diagnostic_request(
                card,
                7,
                3,
                &log,
                true,
                false,
                Arc::new(move || original_check.load(Ordering::Acquire)),
            )
            .unwrap();
        Arc::get_mut(&mut old.0).unwrap().deadline = Instant::now() + Duration::from_millis(20);
        let worker = worker();
        let text = normalize(raw(b"body", &old, &worker), &old, &worker).unwrap();
        text.installed();
        let output = DiagnosticOutput {
            text: Arc::new(text),
            omitted: 0,
            source: old.source().clone(),
        };
        thread::sleep(Duration::from_millis(30));
        assert!(old.check().is_err());
        let fresh = original
            .diagnostic_request(card, 7, 3, &log, false, true, Arc::new(|| true))
            .unwrap();
        assert!(
            output.current_for(&fresh),
            "fresh copy may reuse immutable body on the SAME proved source without the old deadline"
        );
        let substituted = replacement
            .diagnostic_request(card, 7, 3, &log, false, true, Arc::new(|| true))
            .unwrap();
        assert_eq!(fresh.source(), substituted.source());
        assert!(!output.current_for(&substituted));
        original_current.store(false, Ordering::Release);
        assert!(!output.current_for(&fresh));
        drop(output);
        drop(old);
        drop(fresh);
        drop(substituted);
        drop(worker);
        wait_capacity(0);
    }
    #[test]
    fn at_quota_fresh_request_refuses_before_allocate_and_retries_after_owner_release() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        let handle = ClientHandle::fixture(
            super::super::tests::omp_card(None, None),
            Snapshot::default(),
        );
        let mut owners = Vec::new();
        while lane().queued.available_permits() > 0 {
            owners.push(request(Duration::from_secs(1), true, false));
        }
        let make = || {
            handle.diagnostic_request(
                CardId::new(),
                1,
                1,
                &super::super::super::ProviderExitLog {
                    id: "ready-copy".into(),
                    bytes: 4,
                    omitted: 0,
                },
                false,
                true,
                Arc::new(|| true),
            )
        };
        assert!(make().is_err());
        assert_eq!(lane().owned_capacity.load(Ordering::Acquire), 0);
        drop(owners.pop());
        let retry = make().unwrap();
        drop(retry);
        drop(owners);
        wait_capacity(0);
    }
    #[test]
    fn short_log_id_cannot_import_arbitrary_input_capacity_into_queued_metadata() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        let handle = ClientHandle::fixture(
            super::super::tests::omp_card(None, None),
            Snapshot::default(),
        );
        let mut id = String::with_capacity(8 * 1024 * 1024);
        id.push_str("short");
        let log = super::super::super::ProviderExitLog {
            id,
            bytes: 4,
            omitted: 0,
        };
        let request = handle
            .diagnostic_request(CardId::new(), 1, 1, &log, false, true, Arc::new(|| true))
            .unwrap();
        assert_eq!(request.source().log.id, "short");
        assert!(request.source().log.id.capacity() <= 256);
        assert_eq!(
            log.id.capacity(),
            8 * 1024 * 1024,
            "the original remains caller-owned, never captured by queued request"
        );
        drop(log);
        drop(request);
        wait_capacity(0);
    }
    fn wait_diagnostic_idle() {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let usage = diagnostic_resource_usage();
            if usage.queued_owners == 0
                && usage.worker_slots == 0
                && usage.weighted_capacity == 0
                && usage.response_owners == 0
                && usage.retiring_capacity == 0
            {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "diagnostic retirement did not settle: {usage:?}"
            );
            thread::sleep(Duration::from_millis(1));
        }
    }
    #[derive(Clone, Copy, Debug)]
    enum DelayedDiagnosticReply {
        Page,
        OversizedFields,
        LargeError,
        WrongChat,
        Disconnect,
    }
    fn delayed_diagnostic_reply_preserves_reservation(
        mode: DelayedDiagnosticReply,
        cancel_before_reply: bool,
    ) {
        wait_capacity(0);
        let (client, mut server) = UnixStream::pair().unwrap();
        let (arrived_tx, arrived_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let peer = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let proof = super::super::tests::host_proof("typed-diagnostic-late-peer");
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let write = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
                codec
                    .write(
                        server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id,
                            proof: proof.clone(),
                            message,
                        },
                    )
                    .unwrap();
            };
            let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            write(
                &mut codec,
                &mut server,
                Some(connect.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                },
            );
            let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = start.request else {
                panic!("expected ChatStart")
            };
            let chat_id = session.id;
            write(
                &mut codec,
                &mut server,
                Some(start.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached {
                        session,
                        snapshot: Snapshot::default(),
                    },
                },
            );
            let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatProviderExitOutput {
                chat_id: requested_chat,
                log_id,
                offset,
            } = request.request
            else {
                panic!("expected typed diagnostic page")
            };
            assert_eq!(requested_chat, chat_id);
            assert_eq!(log_id, "late-log");
            assert_eq!(offset, 0);
            arrived_tx.send(request.request_id).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            if matches!(mode, DelayedDiagnosticReply::Disconnect) {
                return;
            }
            let message = match mode {
                DelayedDiagnosticReply::LargeError => {
                    ServerMessage::Error(ipc::ProtocolError::new(
                        ipc::ProtocolErrorCode::TerminalUnavailable,
                        "late diagnostic failure".repeat(32 * 1024),
                        false,
                    ))
                }
                DelayedDiagnosticReply::OversizedFields => ServerMessage::Response {
                    response: ServerResponse::ChatProviderExitOutput {
                        chat_id,
                        log_id: "x".repeat(6_900_000),
                        offset: 0,
                        bytes: vec![1; 1_100_000],
                        next_offset: None,
                        omitted: 0,
                    },
                },
                DelayedDiagnosticReply::Page | DelayedDiagnosticReply::WrongChat => {
                    ServerMessage::Response {
                        response: ServerResponse::ChatProviderExitOutput {
                            chat_id: if matches!(mode, DelayedDiagnosticReply::WrongChat) {
                                TerminalId::new()
                            } else {
                                chat_id
                            },
                            log_id,
                            offset: 0,
                            bytes: vec![b'x'; 256 * 1024],
                            next_offset: None,
                            omitted: 0,
                        },
                    }
                }
                DelayedDiagnosticReply::Disconnect => unreachable!(),
            };
            write(&mut codec, &mut server, Some(request.request_id), message);
            // Reader ordering proves the preceding late reply has been decoded and
            // retired before this disconnected wake; no guessed scheduling sleep.
            write(
                &mut codec,
                &mut server,
                None,
                ServerMessage::HostShuttingDown,
            );
        });
        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let handle = ClientHandle::connect_on(
            client,
            super::super::tests::omp_card(None, None),
            Intent::Start,
            None,
            false,
            None,
            None,
            move |wake| {
                let _ = wake_tx.send(wake);
            },
        )
        .unwrap();
        handle.install_snapshot_for_test(Snapshot {
            error: Some("Original provider crash".into()),
            ..Default::default()
        });
        let log = super::super::super::ProviderExitLog {
            id: "late-log".into(),
            bytes: 256 * 1024,
            omitted: 0,
        };
        let weak = Arc::downgrade(&handle.inner);
        let request = handle
            .diagnostic_request(
                CardId::new(),
                7,
                3,
                &log,
                false,
                true,
                Arc::new(move || weak.upgrade().is_some()),
            )
            .unwrap();
        let reader_handle = handle.clone();
        let reader_request = request.clone();
        let reader = thread::spawn(move || {
            runtime().block_on(reader_handle.read_provider_exit_job(reader_request))
        });
        let request_id = arrived_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        if cancel_before_reply {
            request.cancel();
            let completion = reader.join().unwrap();
            assert!(completion.take_result().unwrap().is_err());
            drop(completion);
            drop(request);
            wait_capacity(ipc::MAX_FRAME_BYTES);
            let usage = diagnostic_resource_usage();
            assert_eq!(
                usage.response_owners, 1,
                "{mode:?}: actual posted response retains its admission"
            );
            assert_eq!(usage.queued_owners, 1);
            {
                let pending = handle.inner.diagnostic_pending.lock().unwrap();
                assert!(pending[&request_id].reply.is_none());
                assert!(pending[&request_id].ownership.is_some());
            }
            release_tx.send(()).unwrap();
        } else {
            assert!(matches!(
                mode,
                DelayedDiagnosticReply::LargeError | DelayedDiagnosticReply::OversizedFields
            ));
            assert_eq!(diagnostic_resource_usage().response_owners, 1);
            release_tx.send(()).unwrap();
            let completion = reader.join().unwrap();
            let error = completion.take_result().unwrap().err().unwrap();
            assert!(error.len() <= 512);
            assert!(
                error.capacity() <= 512,
                "queued Error body must have a fresh bounded allocation"
            );
            drop(error);
            drop(completion);
            drop(request);
        }
        loop {
            if matches!(
                wake_rx.recv_timeout(Duration::from_secs(5)).unwrap(),
                Wake::Disconnected(_)
            ) {
                break;
            }
        }
        peer.join().unwrap();
        assert!(handle.inner.diagnostic_pending.lock().unwrap().is_empty());
        wait_diagnostic_idle();
        assert_eq!(diagnostic_resource_usage().response_owners, 0);
        assert_eq!(diagnostic_resource_usage().queued_owners, 0);
        assert_eq!(
            handle.snapshot().error.as_deref(),
            Some("Original provider crash")
        );
        drop(handle); // Inner joins its exact reader and visibility helper.
    }
    #[test]
    fn canceled_posted_wire_keeps_page_error_and_unexpected_payload_admission_until_retired() {
        let _guard = diagnostic_test_guard();
        for mode in [
            DelayedDiagnosticReply::Page,
            DelayedDiagnosticReply::LargeError,
            DelayedDiagnosticReply::WrongChat,
            DelayedDiagnosticReply::Disconnect,
        ] {
            delayed_diagnostic_reply_preserves_reservation(mode, true);
        }
    }
    #[test]
    fn actual_authenticated_oversized_reply_refuses_before_owned_page_allocation() {
        let _guard = diagnostic_test_guard();
        super::super::diagnostic_decode::reset_owned_page_capacity_for_test();
        delayed_diagnostic_reply_preserves_reservation(
            DelayedDiagnosticReply::OversizedFields,
            false,
        );
        assert_eq!(
            super::super::diagnostic_decode::owned_page_capacity_for_test(),
            0
        );
    }
    #[test]
    fn active_wire_error_compacts_allocation_before_queueing_completion() {
        let _guard = diagnostic_test_guard();
        delayed_diagnostic_reply_preserves_reservation(DelayedDiagnosticReply::LargeError, false);
    }
    fn small_send_buffer(stream: &UnixStream) -> usize {
        use std::os::fd::AsRawFd;
        let size: libc::c_int = 4096;
        // SAFETY: owned socket fd, initialized integer and exact integer length.
        assert_eq!(
            unsafe {
                libc::setsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&size as *const libc::c_int).cast(),
                    std::mem::size_of_val(&size) as libc::socklen_t,
                )
            },
            0
        );
        let mut actual: libc::c_int = 0;
        let mut length = std::mem::size_of_val(&actual) as libc::socklen_t;
        // SAFETY: initialized integer output and exact writable length on owned fd.
        assert_eq!(
            unsafe {
                libc::getsockopt(
                    stream.as_raw_fd(),
                    libc::SOL_SOCKET,
                    libc::SO_SNDBUF,
                    (&mut actual as *mut libc::c_int).cast(),
                    &mut length,
                )
            },
            0
        );
        assert!(actual > 0);
        actual as usize
    }
    fn fixture_frame(text: &str) -> Vec<u8> {
        let mut frame = vec![0u8; 16 * 1024];
        let length = postcard::to_slice(&text, &mut frame[4..]).unwrap().len();
        frame[..4].copy_from_slice(&(length as u32).to_be_bytes());
        frame.truncate(length + 4);
        frame
    }
    #[test]
    fn cancellation_after_actual_frame_prefix_finishes_then_preserves_next_frame() {
        use std::io::Read;
        let _guard = diagnostic_test_guard();
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let actual_send_buffer = small_send_buffer(&stream);
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let payload = "x".repeat(16_000);
        let frame = fixture_frame(&payload);
        assert!(
            frame.len() > actual_send_buffer,
            "fixture frame must exceed effective socket capacity before partial-progress cancellation"
        );
        let first = request(Duration::from_secs(2), false, true);
        let cancel = first.clone();
        let sender = thread::spawn(move || {
            send_frame(&stream, &frame, &first).unwrap();
            assert!(
                first.cancelled(),
                "fixture must cancel after first prefix and before its full frame finishes"
            );
            let next = request(Duration::from_secs(2), false, true);
            send_frame(&stream, &fixture_frame("next-valid-frame"), &next).unwrap();
        });
        let mut prefix = [0; 4];
        peer.read_exact(&mut prefix).unwrap();
        let length = u32::from_be_bytes(prefix) as usize;
        assert!(length > 8 * 1024);
        cancel.cancel();
        let mut actual = vec![0; length];
        peer.read_exact(&mut actual).unwrap();
        assert_eq!(postcard::from_bytes::<String>(&actual).unwrap(), payload);
        peer.read_exact(&mut prefix).unwrap();
        let mut next = vec![0; u32::from_be_bytes(prefix) as usize];
        peer.read_exact(&mut next).unwrap();
        assert_eq!(
            postcard::from_bytes::<String>(&next).unwrap(),
            "next-valid-frame"
        );
        sender.join().unwrap();
    }
    #[test]
    fn stalled_actual_partial_frame_closes_connection_at_whole_operation_deadline() {
        use std::io::Read;
        let _guard = diagnostic_test_guard();
        let (stream, mut peer) = UnixStream::pair().unwrap();
        let actual_send_buffer = small_send_buffer(&stream);
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let frame = fixture_frame(&"x".repeat(16_000));
        assert!(
            frame.len() > actual_send_buffer,
            "fixture frame must exceed effective socket capacity"
        );
        let first = request(Duration::from_secs(2), false, true);
        let cancel = first.clone();
        let sender = thread::spawn(move || {
            assert!(send_frame(&stream, &frame, &first).is_err());
            let fresh = request(Duration::from_secs(1), false, true);
            assert!(
                send_frame(&stream, &fixture_frame("must-not-follow-partial"), &fresh).is_err()
            );
        });
        let mut prefix = [0; 4];
        peer.read_exact(&mut prefix).unwrap();
        let length = u32::from_be_bytes(prefix) as usize;
        assert!(length > 8 * 1024);
        cancel.cancel();
        sender.join().unwrap();
        let mut tail = Vec::new();
        peer.read_to_end(&mut tail).unwrap();
        assert!(
            tail.len() < length,
            "peer received an incomplete first frame and no reusable next frame"
        );
    }
    #[test]
    fn typed_authenticated_pages_normalize_once_across_utf8_boundary_and_keep_crash_summary() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        let (client, mut server) = UnixStream::pair().unwrap();
        let raw = format!(
            "{}€\x1b[31mred\x1b[0m\nold\rkept\n",
            "x".repeat(256 * 1024 - 1)
        )
        .into_bytes();
        let expected = provider_exit::readable(raw.clone());
        let log = super::super::super::ProviderExitLog {
            id: "typed-pages".into(),
            bytes: raw.len() as u64,
            omitted: 17,
        };
        let server_log = log.clone();
        let (finish_tx, finish_rx) = std::sync::mpsc::channel();
        let peer = thread::spawn(move || {
            server
                .set_read_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            server
                .set_write_timeout(Some(Duration::from_secs(5)))
                .unwrap();
            let proof = super::super::tests::host_proof("typed-diagnostic-paged-peer");
            let mut codec = FrameCodec::new(ipc::MAX_FRAME_BYTES);
            let write = |codec: &mut FrameCodec, server: &mut UnixStream, request_id, message| {
                codec
                    .write(
                        server,
                        &ServerEnvelope {
                            version: PROTOCOL_VERSION,
                            request_id,
                            proof: proof.clone(),
                            message,
                        },
                    )
                    .unwrap();
            };
            let connect: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            write(
                &mut codec,
                &mut server,
                Some(connect.request_id),
                ServerMessage::Response {
                    response: ServerResponse::Connected {
                        proof: proof.clone(),
                        resumed: vec![],
                    },
                },
            );
            let start: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
            let ClientRequest::ChatStart { session, .. } = start.request else {
                panic!("expected ChatStart")
            };
            let chat_id = session.id;
            write(
                &mut codec,
                &mut server,
                Some(start.request_id),
                ServerMessage::Response {
                    response: ServerResponse::ChatAttached {
                        session,
                        snapshot: Snapshot {
                            error: Some("Original provider crash".into()),
                            ..Default::default()
                        },
                    },
                },
            );
            let mut offset = 0;
            for _ in 0..2 {
                let request: ClientEnvelope = codec.read(&mut server).unwrap().unwrap();
                let ClientRequest::ChatProviderExitOutput {
                    chat_id: requested_chat,
                    log_id,
                    offset: requested_offset,
                } = request.request
                else {
                    panic!("expected diagnostic page")
                };
                assert_eq!(requested_chat, chat_id);
                assert_eq!(log_id, server_log.id);
                assert_eq!(requested_offset, offset as u64);
                let end = (offset + 256 * 1024).min(raw.len());
                write(
                    &mut codec,
                    &mut server,
                    Some(request.request_id),
                    ServerMessage::Response {
                        response: ServerResponse::ChatProviderExitOutput {
                            chat_id,
                            log_id,
                            offset: requested_offset,
                            bytes: raw[offset..end].to_vec(),
                            next_offset: (end < raw.len()).then_some(end as u64),
                            omitted: server_log.omitted,
                        },
                    },
                );
                offset = end;
            }
            assert_eq!(offset, raw.len());
            finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        });
        let handle = ClientHandle::connect_on(
            client,
            super::super::tests::omp_card(None, None),
            Intent::Start,
            None,
            false,
            None,
            None,
            |_| {},
        )
        .unwrap();
        let weak = Arc::downgrade(&handle.inner);
        let request = handle
            .diagnostic_request(
                CardId::new(),
                7,
                3,
                &log,
                true,
                false,
                Arc::new(move || weak.upgrade().is_some()),
            )
            .unwrap();
        let completion = runtime().block_on(handle.read_provider_exit_job(request.clone()));
        let output = completion.take_result().unwrap().unwrap();
        assert_eq!(output.as_str(), expected);
        assert!(output.as_str().contains("€red\n"));
        assert!(output.as_str().ends_with("kept\n"));
        assert_eq!(output.omitted(), 17);
        assert!(handle.inner.diagnostic_pending.lock().unwrap().is_empty());
        assert_eq!(diagnostic_resource_usage().response_owners, 0);
        assert_eq!(
            handle.snapshot().error.as_deref(),
            Some("Original provider crash")
        );
        assert!(output.capacity() <= log.bytes as usize);
        assert_eq!(
            diagnostic_resource_usage().reserved_layout_capacity,
            output.capacity()
        );
        drop(output);
        drop(completion);
        drop(request);
        wait_capacity(0);
        finish_tx.send(()).unwrap();
        peer.join().unwrap();
        drop(handle);
    }
    #[test]
    fn retained_cap_invalid_utf8_cleanup_and_copy_report_actual_layout_and_process_rss() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        lane().peak_capacity.store(0, Ordering::Release);
        let before = rss_kib();
        let job = request(Duration::from_secs(30), true, true);
        let worker = worker();
        let suffix = b"\x1b[31mred\x1b[0m\n";
        let invalid = provider_exit::MAX_OUTPUT_BYTES - suffix.len();
        let mut input = Vec::with_capacity(provider_exit::MAX_OUTPUT_BYTES);
        input.resize(invalid, 0xff);
        input.extend_from_slice(suffix);
        assert_eq!(
            input.capacity(),
            provider_exit::MAX_OUTPUT_BYTES,
            "fixture input has no geometric spare capacity outside the charged lane"
        );
        let text = normalize(raw(&input, &job, &worker), &job, &worker).unwrap();
        drop(input);
        assert!(text.starts_with('\u{fffd}'));
        assert!(text.ends_with("red\n"));
        assert_eq!(text.len(), invalid * 3 + 4);
        let resident = text.capacity();
        text.installed();
        let normalized_rss = rss_kib();
        let output = DiagnosticOutput {
            text: Arc::new(text),
            omitted: 1024,
            source: job.source().clone(),
        };
        let copy = copy_output(&job, &worker, output.clone(), "Provider failed").unwrap();
        let copied = copy.capacity();
        let completion = DiagnosticCopyCompletion::new(job.clone(), Ok(copy));
        let (string, receipt) = completion.take_effect().unwrap().unwrap();
        assert!(string.ends_with("[1024 raw output bytes were not retained]"));
        assert_eq!(
            diagnostic_resource_usage().reserved_layout_capacity,
            resident + copied
        );
        println!(
            "diagnostic-cap-fixture raw_cap={} resident_layout={resident} copy_layout={copied} peak_reserved_layout={} rss_before_kib={before:?} rss_normalized_kib={normalized_rss:?} rss_with_copy_kib={:?} (unit-process point observations; no native RSS ceiling/OS clipboard ownership claim)",
            provider_exit::MAX_OUTPUT_BYTES,
            lane().peak_capacity.load(Ordering::Acquire),
            rss_kib()
        );
        drop(string);
        drop(receipt);
        drop(completion);
        drop(output);
        drop(worker);
        drop(job);
        wait_capacity(0);
        println!(
            "diagnostic-cap-fixture rss_after_retirement_kib={:?} (allocator retention/process state may remain)",
            rss_kib()
        );
    }
    #[test]
    fn canceled_typed_request_waiting_for_actual_wire_lock_releases_unposted_response_reservation()
    {
        let _guard = diagnostic_test_guard();
        wait_diagnostic_idle();
        let handle = ClientHandle::fixture(
            super::super::tests::omp_card(None, None),
            Snapshot::default(),
        );
        let log = super::super::super::ProviderExitLog {
            id: "locked-unposted".into(),
            bytes: 4,
            omitted: 0,
        };
        let request = handle
            .diagnostic_request(CardId::new(), 1, 1, &log, false, true, Arc::new(|| true))
            .unwrap();
        let held = handle.inner.wire.lock().unwrap();
        let other = handle.clone();
        let loading = request.clone();
        let reader =
            thread::spawn(move || runtime().block_on(other.read_provider_exit_job(loading)));
        let deadline = Instant::now() + Duration::from_secs(5);
        while handle.inner.diagnostic_pending.lock().unwrap().is_empty() {
            assert!(
                Instant::now() < deadline,
                "typed request never reached its locked transmit stage"
            );
            thread::sleep(Duration::from_millis(1));
        }
        assert_eq!(diagnostic_resource_usage().response_owners, 1);
        request.cancel();
        let completion = reader.join().unwrap();
        assert!(completion.take_result().unwrap().is_err());
        assert!(
            handle.inner.diagnostic_pending.lock().unwrap().is_empty(),
            "no complete request was posted, so no late response tombstone is needed"
        );
        drop(completion);
        drop(request);
        wait_diagnostic_idle();
        drop(held);
        drop(handle);
    }

    #[test]
    fn oversized_existing_host_proof_refuses_before_diagnostic_admission_or_clone() {
        let _guard = diagnostic_test_guard();
        wait_capacity(0);
        let session = super::super::tests::omp_card(None, None);
        let mut proof = super::super::tests::host_proof("diagnostic-proof");
        proof.host_incarnation = crate::model::HostIncarnation::new("x".repeat(1024 * 1024));
        let mut handle = ClientHandle::fixture(session, Snapshot::default());
        Arc::get_mut(&mut handle.inner).unwrap().proof = proof;
        let log = super::super::super::ProviderExitLog {
            id: "log".into(),
            bytes: 4,
            omitted: 0,
        };
        let before = diagnostic_resource_usage();
        assert!(
            handle
                .diagnostic_request(CardId::new(), 1, 1, &log, false, true, Arc::new(|| true))
                .is_err()
        );
        let after = diagnostic_resource_usage();
        assert_eq!(after.queued_owners, before.queued_owners);
        assert_eq!(after.weighted_capacity, before.weighted_capacity);
        assert_eq!(after.reserved_layout_capacity, 0);
    }
}
