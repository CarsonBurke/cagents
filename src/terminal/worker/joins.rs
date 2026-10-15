//! Actual OS-thread joins never execute on a deadline-bearing caller. Rust's
//! finished packet may be visible while a TLS destructor is still blocked.
use super::{PublishedState, WORKER_JOIN_TIMEOUT, close_processes};
use std::{
    sync::{Arc, Mutex, OnceLock, atomic::{AtomicUsize, Ordering}},
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
pub(super) struct Reservation { _private: () }
impl Drop for Reservation {
    fn drop(&mut self) { RESERVED.fetch_sub(1, Ordering::AcqRel); }
}
fn capacity() -> usize {
    #[cfg(test)]
    {
        static LIMIT: OnceLock<usize> = OnceLock::new();
        return *LIMIT.get_or_init(|| std::env::var("CAGENTS_TERMINAL_JOIN_TEST_CAPACITY").ok().and_then(|value| value.parse().ok()).filter(|limit| (1..=CAPACITY).contains(limit)).unwrap_or(CAPACITY));
    }
    #[cfg(not(test))]
    { CAPACITY }
}
pub(super) fn reserve() -> Result<Reservation, String> {
    // Service startup failure is refused before any terminal worker or child
    // exists. Every admitted worker keeps its reservation through actual join.
    service()?;
    RESERVED.fetch_update(Ordering::AcqRel, Ordering::Acquire, |count| (count < capacity()).then_some(count + 1))
        .map_err(|_| "Terminal cleanup capacity is full; wait for retained workers to finish before starting another terminal".to_owned())?;
    Ok(Reservation { _private: () })
}
static SERVICE: OnceLock<Result<crossbeam_channel::Sender<Job>, String>> = OnceLock::new();

fn service() -> Result<&'static crossbeam_channel::Sender<Job>, String> {
    SERVICE
        .get_or_init(|| {
            let (sender, receiver) = crossbeam_channel::bounded::<Job>(capacity());
            for index in 0..WORKERS {
                let receiver = receiver.clone();
                thread::Builder::new()
                    .name(format!("terminal-join-{index}"))
                    .spawn(move || {
                        while let Ok(job) = receiver.recv() {
                            // Only the shared service may wait for metadata or TLS.
                            // Keep both the exact JoinHandle and original pidfd owner.
                            let worker =
                                job.worker.lock().unwrap_or_else(|p| p.into_inner()).take();
                            let success = worker.is_none_or(|worker| worker.join().is_ok());
                            if !success {
                                job.published.completed.store(false, Ordering::Release);
                            }
                            job.published
                                .worker_join
                                .store(if success { 2 } else { 3 }, Ordering::Release);
                        }
                    })
                    .map_err(|error| format!("Terminal join service unavailable: {error}"))?;
            }
            Ok(sender)
        })
        .as_ref()
        .map_err(Clone::clone)
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
    match published
        .worker_join
        .compare_exchange(0, 1, Ordering::AcqRel, Ordering::Acquire)
    {
        Ok(_) => {}
        Err(2) => return true,
        Err(_) => return false,
    }
    let queued = service().and_then(|sender| {
        sender
            .try_send(Job {
                worker: Arc::clone(worker),
                published: Arc::clone(published),
            })
            .map_err(|_| {
                "Terminal join capacity is busy; its exact worker remains retained".to_owned()
            })
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
            if escalate && let Some(pidfd) = published.shutdown_pidfd.get() {
                let _ = close_processes::signal(pidfd, libc::SIGKILL);
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
