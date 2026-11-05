//! Actual terminal-thread join and final-holder custody, off deadline callers.
//!
//! Running bodies park in bounded custody until their own completion signal.
//! Two fixed lanes perform actual joins; neither polling nor one cleanup thread
//! per terminal is needed. A successful join and a requested holder release are
//! separate facts, including when Stop arrives after a carry/preserve join.
use super::PublishedState;
use std::{
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::Instant,
};

// Match HostState.MAX_TERMINALS; pending retirement also consumes admission.
pub(super) const CAPACITY: usize = 1024;
const LANES: usize = 2;
static RESERVED: AtomicUsize = AtomicUsize::new(0);
static SERVICE: OnceLock<Result<Service, String>> = OnceLock::new();

pub(super) struct Reservation {
    service: &'static Service,
}
impl Drop for Reservation {
    fn drop(&mut self) {
        RESERVED.fetch_sub(1, Ordering::AcqRel);
    }
}
pub(super) fn reserve() -> Result<Reservation, String> {
    let service = service()?;
    RESERVED.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| {
        (count < CAPACITY).then_some(count + 1)
    }).map_err(|_| "Terminal cleanup capacity is full; retained workers must finish before another terminal can start".to_owned())?;
    Ok(Reservation { service })
}

struct Progress {
    reservation: Option<Reservation>,
    requested: bool,
    body_finished: bool,
    installed: bool,
    native_settled: bool,
    queued: bool,
    joined: bool,
    panicked: bool,
    release_required: bool,
    final_drop: bool,
    released: bool,
    registered: bool,
}

pub(super) struct JoinOwner {
    worker: Mutex<Option<JoinHandle<()>>>,
    published: Arc<PublishedState>,
    service: Option<&'static Service>,
    progress: Mutex<Progress>,
    changed: Condvar,
}
impl JoinOwner {
    pub(super) fn new(published: Arc<PublishedState>, reservation: Reservation) -> Arc<Self> {
        Self::make(published, Some(reservation))
    }
    pub(super) fn empty(published: Arc<PublishedState>) -> Arc<Self> {
        Self::make(published, None)
    }
    fn make(published: Arc<PublishedState>, reservation: Option<Reservation>) -> Arc<Self> {
        let empty = reservation.is_none();
        Arc::new(Self {
            worker: Mutex::new(None),
            published,
            service: reservation.as_ref().map(|r| r.service),
            progress: Mutex::new(Progress {
                reservation,
                requested: false,
                body_finished: empty,
                installed: empty,
                native_settled: empty,
                queued: false,
                joined: empty,
                panicked: false,
                release_required: false,
                final_drop: false,
                released: false,
                registered: false,
            }),
            changed: Condvar::new(),
        })
    }
    pub(super) fn install(self: &Arc<Self>, worker: JoinHandle<()>) {
        *self.worker.lock().unwrap_or_else(|p| p.into_inner()) = Some(worker);
        self.progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .installed = true;
        // Body completion can precede parent-side handle installation.
        dispatch(self);
    }
    #[cfg(test)]
    pub(super) fn joined(&self) -> bool {
        let p = self.progress.lock().unwrap_or_else(|p| p.into_inner());
        p.joined && !p.panicked
    }
    #[cfg(test)]
    pub(super) fn native_settled_for_test(&self) -> bool {
        let p = self.progress.lock().unwrap_or_else(|p| p.into_inner());
        p.joined && !p.panicked && p.native_settled && p.reservation.is_none()
    }
}

/// First local of the real worker closure; it also runs on body unwind.
/// This is readiness only: actual join (including TLS) is proved by a lane.
pub(super) struct BodyCompletion(pub(super) Arc<JoinOwner>);
impl Drop for BodyCompletion {
    fn drop(&mut self) {
        self.0
            .progress
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .body_finished = true;
        // Natural completed screens must not retain an unjoined worker merely
        // because no Stop was requested. This requests join only, never release.
        request(&self.0, false, false);
    }
}

struct Service {
    sender: crossbeam_channel::Sender<Arc<JoinOwner>>,
    // One entry per ticket, including failed enqueue and active native release.
    // The registry owns originals even after the last public handle disappears.
    pending: Mutex<Vec<Arc<JoinOwner>>>,
    _receiver: crossbeam_channel::Receiver<Arc<JoinOwner>>,
}
fn service() -> Result<&'static Service, String> {
    SERVICE
        .get_or_init(|| {
            let (sender, receiver) = crossbeam_channel::bounded::<Arc<JoinOwner>>(CAPACITY);
            for lane in 0..LANES {
                let receiver = receiver.clone();
                thread::Builder::new()
                    .name(format!("terminal-worker-join-{lane}"))
                    .spawn(move || {
                        while let Ok(owner) = receiver.recv() {
                            finish(&owner);
                        }
                    })
                    .map_err(|error| format!("Terminal join service unavailable: {error}"))?;
            }
            Ok(Service {
                sender,
                pending: Mutex::new(Vec::with_capacity(CAPACITY)),
                _receiver: receiver,
            })
        })
        .as_ref()
        .map_err(Clone::clone)
}

/// `release` uses the existing completed-holder policy. Only final HandleInner
/// destruction also performs the existing last-owner drop for a failed body.
pub(super) fn request(owner: &Arc<JoinOwner>, release: bool, final_drop: bool) {
    if release {
        owner.published.request_shutdown();
    }
    {
        let mut p = owner.progress.lock().unwrap_or_else(|p| p.into_inner());
        p.requested = true;
        p.release_required |= release;
        p.final_drop |= final_drop;
        // A completed preserve/carry with no retained native holder can drop
        // its ticket. Later Stop has only the same small archive state to clear.
        // No native owner can be installed after actual join, so this proof is
        // stable; this fast path never takes/drops a Holder on the caller.
        if p.joined && p.native_settled && p.release_required && !p.released {
            // This is settled ownership HERE, not global process absence.
            // An in-flight cached reader need not delay native Stop proof.
            owner.published.publish_released_archive_if_available();
            p.released = true;
            owner.changed.notify_all();
        }
        if !p.registered && p.reservation.is_some() {
            if let Some(service) = owner.service {
                service
                    .pending
                    .lock()
                    .unwrap_or_else(|p| p.into_inner())
                    .push(owner.clone());
                p.registered = true;
            }
        }
    }
    dispatch(owner);
}

fn release_can_run(owner: &JoinOwner, p: &Progress) -> bool {
    p.release_required
        && !p.released
        && (p.final_drop
            || owner.published.completed.load(Ordering::Acquire)
            || !owner.published.has_retained_holder())
}
fn dispatch(owner: &Arc<JoinOwner>) {
    let Some(service) = owner.service else {
        return;
    };
    let failure = {
        let mut p = owner.progress.lock().unwrap_or_else(|p| p.into_inner());
        if !p.requested || p.queued || !p.body_finished || !p.installed || p.reservation.is_none() {
            return;
        }
        if p.joined && !release_can_run(owner, &p) {
            settle(owner, &mut p);
            return;
        }
        p.queued = true;
        match service.sender.try_send(owner.clone()) {
            Ok(()) => None,
            Err(error) => {
                p.queued = false;
                // Still registered with the SAME ticket and original handle.
                Some(error.to_string())
            }
        }
    };
    if let Some(error) = failure {
        tracing::error!(%error, "Terminal join admission remains unconfirmed; original worker retained");
    }
}

fn finish(owner: &Arc<JoinOwner>) {
    let needs_join = !owner
        .progress
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .joined;
    if needs_join {
        // Only a fixed lane takes and joins the original OS-thread handle.
        let worker = owner
            .worker
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .take();
        let success = worker.is_some_and(|worker| worker.join().is_ok());
        let mut p = owner.progress.lock().unwrap_or_else(|p| p.into_inner());
        p.joined = true;
        p.panicked |= !success;
        if !success {
            owner.published.completed.store(false, Ordering::Release);
        }
        owner.changed.notify_all();
    }
    let release = {
        let p = owner.progress.lock().unwrap_or_else(|p| p.into_inner());
        release_can_run(owner, &p)
    };
    if release {
        // The existing release may wait on a native holder. Run it here, never
        // after a deadline returns or on the final public handle's drop thread.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            owner.published.release_completed_holder();
        }));
        let mut p = owner.progress.lock().unwrap_or_else(|p| p.into_inner());
        p.released = result.is_ok();
        p.panicked |= result.is_err();
        if result.is_err() {
            owner.published.completed.store(false, Ordering::Release);
        }
    }
    {
        let mut p = owner.progress.lock().unwrap_or_else(|p| p.into_inner());
        p.queued = false;
        settle(owner, &mut p);
        owner.changed.notify_all();
    }
    // Stop can arrive after the join's first release decision. Both transitions
    // serialize under progress; either this dispatch or Stop's dispatch sees it.
    dispatch(owner);
}

/// Only actual join and a settled native obligation release admission. Pure
/// cached screen/publication owners do not keep a ticket after those facts.
fn settle(owner: &JoinOwner, p: &mut Progress) {
    if !p.joined
        || p.queued
        || (p.release_required && !p.released)
        || owner.published.has_retained_holder()
    {
        return;
    }
    if p.registered {
        if let Some(service) = owner.service {
            service
                .pending
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .retain(|pending| !std::ptr::eq(Arc::as_ptr(pending), owner));
        }
        p.registered = false;
    }
    p.native_settled = true;
    p.reservation.take();
}

/// A preserve caller settles its existing detached-holder obligation itself.
pub(super) fn settled(owner: &JoinOwner) {
    let mut p = owner.progress.lock().unwrap_or_else(|p| p.into_inner());
    settle(owner, &mut p);
    owner.changed.notify_all();
}

/// Handoffs retain their existing wait-for-actual-join policy; ordinary Stop
/// additionally requires the requested release, under one absolute deadline.
pub(super) fn wait_until(owner: &JoinOwner, deadline: Option<Instant>, release: bool) -> bool {
    let mut p = owner.progress.lock().unwrap_or_else(|p| p.into_inner());
    loop {
        if p.panicked {
            return false;
        }
        if p.joined && (!release || p.released) {
            return true;
        }
        p = match deadline {
            Some(deadline) => {
                let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                    return false;
                };
                owner
                    .changed
                    .wait_timeout(p, remaining)
                    .unwrap_or_else(|p| p.into_inner())
                    .0
            }
            None => owner.changed.wait(p).unwrap_or_else(|p| p.into_inner()),
        };
    }
}
#[cfg(test)]
pub(super) fn reserved() -> usize {
    RESERVED.load(Ordering::Acquire)
}
