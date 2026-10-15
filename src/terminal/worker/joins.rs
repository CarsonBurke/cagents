//! Actual OS-thread joins never execute on a deadline-bearing caller. Rust's
//! finished packet may be visible while a TLS destructor is still blocked.
use super::{PublishedState, WORKER_JOIN_TIMEOUT, close_processes};
use std::{
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

type WorkerSlot = Arc<Mutex<Option<JoinHandle<()>>>>;
struct Job {
    worker: WorkerSlot,
    published: Arc<PublishedState>,
}
const WORKERS: usize = 2;
// Match the host's existing 1024-terminal ceiling. Reservations also count
// cleanup owners after their panes disappeared, so repeated failed cleanup
// cannot create an unlimited retirement backlog.
const CAPACITY: usize = 1024;
static RESERVED: AtomicUsize = AtomicUsize::new(0);

#[derive(Debug)]
pub(super) struct Reservation {
    _private: (),
}
impl Drop for Reservation {
    fn drop(&mut self) {
        RESERVED.fetch_sub(1, Ordering::AcqRel);
    }
}
fn capacity() -> usize {
    #[cfg(test)]
    {
        static LIMIT: OnceLock<usize> = OnceLock::new();
        return *LIMIT.get_or_init(|| {
            std::env::var("CAGENTS_TERMINAL_JOIN_TEST_CAPACITY")
                .ok()
                .and_then(|value| value.parse().ok())
                .filter(|limit| (1..=CAPACITY).contains(limit))
                .unwrap_or(CAPACITY)
        });
    }
    #[cfg(not(test))]
    {
        CAPACITY
    }
}
pub(super) fn reserve() -> Result<Reservation, String> {
    // Service startup failure is refused before any terminal worker or child
    // exists. Every admitted worker keeps its reservation through actual join.
    service()?;
    RESERVED.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| (count < capacity()).then_some(count + 1))
        .map_err(|_| "Terminal cleanup capacity is full; wait for retained workers to finish before starting another terminal".to_owned())?;
    Ok(Reservation { _private: () })
}
struct Service {
    joins: crossbeam_channel::Sender<Job>,
    deadlines: crossbeam_channel::Sender<Arc<PublishedState>>,
    changed: crossbeam_channel::Sender<()>,
}
static SERVICE: OnceLock<Result<Service, String>> = OnceLock::new();

fn service() -> Result<&'static Service, String> {
    SERVICE.get_or_init(|| {
        let (sender, receiver) = crossbeam_channel::bounded::<Job>(capacity());
        let (deadlines, pending) = crossbeam_channel::bounded::<Arc<PublishedState>>(capacity());
        let (changed, changes) = crossbeam_channel::bounded(1);
        // Actual pthread joins can remain blocked in TLS destruction. One
        // independent supervisor owns active AND queued deadlines. Admission,
        // completion and later escalation requests wake it without idle polls.
        thread::Builder::new().name("terminal-join-deadlines".into()).spawn(move || {
            let mut active: Vec<Arc<PublishedState>> = Vec::with_capacity(capacity());
            loop {
                active.extend(pending.try_iter());
                let mut next: Option<Instant> = None;
                let observed_at = Instant::now();
                let mut schedule = |deadline| next = Some(next.map_or(deadline, |old|old.min(deadline)));
                active.retain(|owner| {
                    if matches!(owner.worker_join.load(Ordering::Acquire), 2 | 3) { return false; }
                    let Some(deadline) = owner.join_escalation_at.get() else {return true;};
                    if *deadline>observed_at {schedule(*deadline);return true;}
                    match escalate_owned_root(owner) {
                        Escalation::Done => false,
                        Escalation::Contended => {schedule(observed_at+Duration::from_millis(5));true},
                        Escalation::SignalUnconfirmed => {schedule(observed_at+Duration::from_millis(250));true},
                        // A provisional handoff may fail and reopen. Keep its
                        // ticket parked until ownership changes or actual join.
                        Escalation::Handoff => true,
                    }
                });
                let timer = next.map(|deadline|crossbeam_channel::after(deadline.saturating_duration_since(Instant::now())))
                    .unwrap_or_else(crossbeam_channel::never);
                crossbeam_channel::select! {
                    recv(pending) -> owner => match owner { Ok(owner)=>active.push(owner), Err(_)=>return },
                    recv(changes) -> _ => {},
                    recv(timer) -> _ => {},
                }
            }
        }).map_err(|error| format!("Terminal deadline service unavailable: {error}"))?;
        for index in 0..WORKERS {
            let receiver = receiver.clone();
            let changed = changed.clone();
            thread::Builder::new().name(format!("terminal-join-{index}")).spawn(move || {
                while let Ok(job) = receiver.recv() {
                    // Only the shared service may wait for metadata or TLS.
                    // Keep both the exact JoinHandle and original pidfd owner.
                    let worker = job.worker.lock().unwrap_or_else(|p| p.into_inner()).take();
                    let success = worker.is_none_or(|worker| worker.join().is_ok());
                    if !success { job.published.completed.store(false, Ordering::Release); }
                    job.published.worker_join.store(if success { 2 } else { 3 }, Ordering::Release);
                    let _ = changed.try_send(());
                }
            }).map_err(|error| format!("Terminal join service unavailable: {error}"))?;
        }
        Ok(Service { joins: sender, deadlines, changed })
    }).as_ref().map_err(Clone::clone)
}

pub(super) fn initialize() -> Result<(), String> {
    service().map(|_| ())
}

/// Queue refusal leaves the original JoinHandle in its original slot. A queued
/// task retains that slot and pidfd beyond the caller's deadline. Concurrent
/// callers report pending rather than waiting behind the first caller's join.
pub(super) fn join_until(
    worker: &WorkerSlot,
    published: &Arc<PublishedState>,
    deadline: Instant,
    escalate: bool,
    wait: bool,
) -> bool {
    if escalate {
        let _ = published.join_escalation_at.set(deadline);
        ownership_changed();
    }
    match published
        .worker_join
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => {}
        Err(2) => return true,
        Err(_) => return false,
    }
    let queued = service().and_then(|service| {
        service
            .joins
            .try_send(Job {
                worker: Arc::clone(worker),
                published: Arc::clone(published),
            })
            .map_err(|_| {
                "Terminal join capacity is busy; its exact worker remains retained".to_owned()
            })?;
        // Each reserved worker admits only one join job. Queue plus active
        // tickets cannot exceed reservations, so this has a reserved slot too.
        service
            .deadlines
            .try_send(Arc::clone(published))
            .expect("a reserved terminal join must retain its escalation owner");
        Ok(())
    });
    if let Err(error) = queued {
        published.worker_join.store(0, Ordering::Release);
        tracing::warn!(%error);
        return false;
    }
    if !wait {
        return false;
    }
    loop {
        match published.worker_join.load(Ordering::Acquire) {
            2 => return true,
            3 => return false,
            _ => {}
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            if escalate {
                let _ = escalate_owned_root(published);
            }
            tracing::error!(
                escalate,
                timeout_seconds = WORKER_JOIN_TIMEOUT.as_secs(),
                "Terminal worker join remains pending; its exact owner is retained"
            );
            return false;
        };
        thread::sleep(remaining.min(Duration::from_millis(5)));
    }
}

/// Wake a parked join when a known failed handoff restores this owner.
pub(super) fn ownership_changed() {
    if let Some(Ok(service)) = SERVICE.get() {
        let _ = service.changed.try_send(());
    }
}

enum Escalation {
    Done,
    Contended,
    Handoff,
    SignalUnconfirmed,
}
// A contended fence or provisional handoff keeps its ticket until ownership
// changes. Only an actual signal admission retires escalation responsibility.
fn escalate_owned_root(published: &PublishedState) -> Escalation {
    let order = match published.input_order.try_lock() {
        Ok(order) => order,
        Err(std::sync::TryLockError::Poisoned(error)) => error.into_inner(),
        Err(std::sync::TryLockError::WouldBlock) => return Escalation::Contended,
    };
    if order.handoff {
        return Escalation::Handoff;
    }
    if let Some(pidfd) = published.shutdown_pidfd.get()
        && let Err(error) = close_processes::signal(pidfd, libc::SIGKILL)
        && error.raw_os_error() != Some(libc::ESRCH)
    {
        if published.join_signal_error.set(()).is_ok() {
            tracing::warn!(%error,"Original terminal root escalation remains unconfirmed; its bounded cleanup owner will retry");
        }
        return Escalation::SignalUnconfirmed;
    }
    Escalation::Done
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::File;

    #[test]
    fn uncertain_pidfd_signal_retains_escalation_responsibility() {
        let owner = PublishedState::new(true, Arc::new(|_| {}));
        owner
            .shutdown_pidfd
            .set(File::open("/dev/null").unwrap())
            .unwrap();
        assert!(matches!(
            escalate_owned_root(&owner),
            Escalation::SignalUnconfirmed
        ));
        assert!(owner.join_signal_error.get().is_some());
        assert!(matches!(
            escalate_owned_root(&owner),
            Escalation::SignalUnconfirmed
        ));
        assert_eq!(
            owner.worker_join.load(Ordering::Acquire),
            0,
            "signal uncertainty cannot invent actual join completion"
        );
    }
}
