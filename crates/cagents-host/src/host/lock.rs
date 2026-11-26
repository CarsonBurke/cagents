//! The mutex around [`HostState`](super::state::HostState), instrumented so a
//! deadlock on it cannot be silent.
//!
//! Every client, chat actor, terminal callback and scheduler serializes on this
//! one lock, so a thread that never releases it stops the whole daemon while
//! the process stays alive and looks healthy. Two failures are turned into
//! something that recovers:
//!
//! - A thread locking it again while holding it panics instead of blocking
//!   forever. Only that thread's work is lost.
//! - With [`HostMutex::watch`], a single hold that keeps another thread
//!   waiting past a limit aborts the daemon. The abort leaves a core dump
//!   naming the holder, and the next daemon adopts the surviving holders.

use std::{
    ops::{Deref, DerefMut},
    panic::Location,
    sync::{
        Arc, LockResult, Mutex, MutexGuard, OnceLock, TryLockError, Weak,
        atomic::{AtomicBool, AtomicPtr, AtomicU64, Ordering},
    },
    thread::{self, Thread},
    time::{Duration, Instant},
};

/// How long one hold may keep another thread waiting before the daemon is
/// declared wedged. Ordinary holds last microseconds; the longest deliberate
/// one waits three seconds for a terminal to finish archiving.
pub(crate) const STALL_LIMIT: Duration = Duration::from_secs(120);

/// A nonzero identity for the calling thread, never reused within the process.
fn current_thread() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    thread_local! {
        static TOKEN: u64 = NEXT.fetch_add(1, Ordering::Relaxed);
    }
    TOKEN.with(|token| *token)
}

pub(crate) struct HostMutex<T> {
    inner: Mutex<T>,
    /// The holder's [`current_thread`], or zero while unlocked.
    owner: AtomicU64,
    /// Counts acquisitions, so a watcher can tell one long hold from many
    /// short ones.
    acquisitions: AtomicU64,
    /// Where the newest acquisition was requested, for the stall report.
    site: AtomicPtr<Location<'static>>,
    watcher: OnceLock<Thread>,
}

impl<T> HostMutex<T> {
    pub(crate) fn new(value: T) -> Self {
        Self {
            inner: Mutex::new(value),
            owner: AtomicU64::new(0),
            acquisitions: AtomicU64::new(0),
            site: AtomicPtr::new(std::ptr::null_mut()),
            watcher: OnceLock::new(),
        }
    }

    /// Locks the host, recovering the state from a holder that panicked.
    ///
    /// The result is always `Ok`; it keeps the shape of [`Mutex::lock`] for
    /// its callers.
    ///
    /// # Panics
    ///
    /// When the calling thread already holds the lock, which would otherwise
    /// block it, and every other user of the host behind it, forever.
    #[track_caller]
    pub(crate) fn lock(&self) -> LockResult<HostGuard<'_, T>> {
        let thread = current_thread();
        assert!(
            self.owner.load(Ordering::Relaxed) != thread,
            "the host mutex was locked again by the thread that holds it; a callback reached \
             the host from inside a host operation"
        );
        let guard = match self.inner.try_lock() {
            Ok(guard) => guard,
            Err(TryLockError::Poisoned(poisoned)) => poisoned.into_inner(),
            Err(TryLockError::WouldBlock) => {
                // Only a waiter can be stalled, so only a waiter costs the
                // watcher a wakeup; an uncontended daemon never runs it.
                if let Some(watcher) = self.watcher.get() {
                    watcher.unpark();
                }
                self.inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
            }
        };
        self.site.store(
            std::ptr::from_ref(Location::caller()).cast_mut(),
            Ordering::Relaxed,
        );
        self.acquisitions.fetch_add(1, Ordering::Relaxed);
        self.owner.store(thread, Ordering::Relaxed);
        Ok(HostGuard {
            guard,
            owner: &self.owner,
        })
    }

    /// The acquisition currently holding the lock, if any.
    fn held(&self) -> Option<u64> {
        // Read the count on both sides of the owner, so a release and a new
        // acquisition in between are not mistaken for one continuous hold.
        let before = self.acquisitions.load(Ordering::Relaxed);
        let held = self.owner.load(Ordering::Relaxed) != 0;
        (held && self.acquisitions.load(Ordering::Relaxed) == before).then_some(before)
    }

    /// Where the newest acquisition was requested.
    fn site(&self) -> Option<&'static Location<'static>> {
        // SAFETY: the pointer is null or was stored from `Location::caller`,
        // which is valid for the whole program.
        unsafe { self.site.load(Ordering::Relaxed).as_ref() }
    }
}

impl<T: Send + 'static> HostMutex<T> {
    /// Aborts the process when one hold keeps a waiter blocked for `limit`.
    ///
    /// The watcher sleeps until a thread first has to wait for the lock and
    /// goes back to sleep once it finds the lock free or moving, so it adds no
    /// periodic wakeup to an idle or healthy daemon. Dropping the returned
    /// guard stops it.
    pub(crate) fn watch(self: &Arc<Self>, limit: Duration) -> std::io::Result<StallWatch> {
        self.watch_with(limit, || std::process::abort())
    }

    fn watch_with(
        self: &Arc<Self>,
        limit: Duration,
        stalled: impl FnOnce() + Send + 'static,
    ) -> std::io::Result<StallWatch> {
        let host = Arc::downgrade(self);
        let stopped = Arc::new(AtomicBool::new(false));
        let stop = Arc::clone(&stopped);
        let watcher = thread::Builder::new()
            .name("cagentsd-stall-watch".into())
            .spawn(move || {
                if let Some(holder) = watch_for_stall(&host, limit, &stop) {
                    tracing::error!(
                        limit_seconds = limit.as_secs(),
                        %holder,
                        "the host mutex has been held without release while another thread \
                         waits; aborting so the next daemon can adopt the hosted sessions"
                    );
                    stalled();
                }
            })?;
        let _ = self.watcher.set(watcher.thread().clone());
        Ok(StallWatch {
            stopped,
            watcher: watcher.thread().clone(),
        })
    }
}

/// How many times one hold must be observed before it counts as a stall.
const STALL_SAMPLES: u32 = 8;

/// Returns where the lock was taken once a single hold has outlasted `limit`,
/// and `None` when the watch is stopped or its mutex is gone.
fn watch_for_stall<T>(
    host: &Weak<HostMutex<T>>,
    limit: Duration,
    stopped: &AtomicBool,
) -> Option<String> {
    let interval = (limit / STALL_SAMPLES).max(Duration::from_millis(1));
    // The suspected acquisition, when it was first seen, and how often since.
    let mut suspect: Option<(u64, Instant, u32)> = None;
    loop {
        if stopped.load(Ordering::Acquire) {
            return None;
        }
        let mutex = host.upgrade()?;
        let held = mutex.held();
        match (held, &mut suspect) {
            (Some(acquisition), Some((suspected, since, samples))) if acquisition == *suspected => {
                *samples += 1;
                // Elapsed time alone would also condemn a daemon that was
                // itself stopped for the limit, by a debugger or a freezer.
                if since.elapsed() >= limit && *samples >= STALL_SAMPLES {
                    return Some(
                        mutex
                            .site()
                            .map_or_else(|| "unknown".into(), ToString::to_string),
                    );
                }
            }
            (Some(acquisition), _) => suspect = Some((acquisition, Instant::now(), 0)),
            (None, _) => suspect = None,
        }
        drop(mutex);
        if suspect.is_some() {
            thread::park_timeout(interval);
        } else {
            // Nothing is held: wait for the next thread that has to wait.
            thread::park();
        }
    }
}

/// Stops its stall watcher when dropped.
pub(crate) struct StallWatch {
    stopped: Arc<AtomicBool>,
    watcher: Thread,
}

impl Drop for StallWatch {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        self.watcher.unpark();
    }
}

pub(crate) struct HostGuard<'a, T> {
    guard: MutexGuard<'a, T>,
    owner: &'a AtomicU64,
}

impl<T> Deref for HostGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.guard
    }
}

impl<T> DerefMut for HostGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        &mut self.guard
    }
}

impl<T> Drop for HostGuard<'_, T> {
    fn drop(&mut self) {
        // Runs before the inner guard unlocks, so the next holder's identity
        // is never overwritten.
        self.owner.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::mpsc;

    use super::*;

    #[test]
    fn relocking_on_the_holding_thread_panics_and_leaves_the_host_usable() {
        let host = Arc::new(HostMutex::new(0_u32));
        let reentrant = Arc::clone(&host);
        let outcome = thread::spawn(move || {
            let _held = reentrant.lock().unwrap();
            let _ = reentrant.lock();
        })
        .join();
        assert!(outcome.is_err());

        *host.lock().unwrap() += 1;
        assert_eq!(*host.lock().unwrap(), 1);
    }

    #[test]
    fn a_hold_that_outlasts_the_limit_while_another_thread_waits_is_reported() {
        let host = Arc::new(HostMutex::new(()));
        let (stalled, reported) = mpsc::channel();
        let _watch = host
            .watch_with(Duration::from_millis(80), move || {
                let _ = stalled.send(());
            })
            .unwrap();

        let held = host.lock().unwrap();
        let waiter = {
            let host = Arc::clone(&host);
            thread::spawn(move || drop(host.lock()))
        };
        reported.recv_timeout(Duration::from_secs(5)).unwrap();
        drop(held);
        waiter.join().unwrap();
    }

    #[test]
    fn contended_but_moving_holds_are_not_a_stall() {
        let host = Arc::new(HostMutex::new(0_u64));
        let (stalled, reported) = mpsc::channel();
        let _watch = host
            .watch_with(Duration::from_secs(2), move || {
                let _ = stalled.send(());
            })
            .unwrap();

        // Far longer than the limit in total, far shorter per hold.
        let deadline = Instant::now() + Duration::from_secs(3);
        let workers = (0..4)
            .map(|_| {
                let host = Arc::clone(&host);
                thread::spawn(move || {
                    while Instant::now() < deadline {
                        let mut count = host.lock().unwrap();
                        *count += 1;
                        thread::sleep(Duration::from_millis(1));
                    }
                })
            })
            .collect::<Vec<_>>();
        for worker in workers {
            worker.join().unwrap();
        }
        assert!(reported.try_recv().is_err());
    }
}
