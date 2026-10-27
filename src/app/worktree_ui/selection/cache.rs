//! Persistent native-watch custody, separate from UI metadata and preparation.
//!
//! There is one watcher slot per window. The UI owns only senders and atomic
//! proof metadata; the admitted filesystem worker alone registers or replaces
//! the watcher. The idle custodian never resolves a path. Its final drain waits
//! for all senders, then the original abandoned worker handle, before dropping
//! the native watcher. A watcher callback never owns a custody sender.
use super::Invalidation;
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicU64, Ordering},
};

pub(super) const MAX_TARGETS: usize = 512;
pub(super) const MAX_PATH_BYTES: usize = 4096;
pub(super) const MAX_METADATA_ROWS: usize = 512;
pub(super) const MAX_METADATA_BYTES: usize = 256 * 1024;
pub(super) const MAX_REGISTRATIONS: usize = 4096;

pub(super) struct WatchState {
    pub epoch: AtomicU64,
    pub healthy: AtomicBool,
    wake: async_channel::Sender<()>,
    #[cfg(test)]
    pub retired: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl WatchState {
    pub fn changed(&self) {
        self.epoch.fetch_add(1, Ordering::AcqRel);
        let _ = self.wake.try_send(());
    }
    pub fn failed(&self) {
        self.healthy.store(false, Ordering::Release);
        // An unavailable backend invalidates every cache hit, but is not a
        // fabricated path change. Fresh double-checked physical preparation
        // can still finish in degraded mode instead of retrying forever.
        let _ = self.wake.try_send(());
    }
}

// At most one admitted ticket can request the slot; Take is acknowledged before
// Replace. Only abandonment can transfer a Join, and that ticket cannot clear
// its active slot to admit another. Thus at most two messages and one abandoned
// original handle are resident; this is not a per-event/history queue.
enum Request {
    Take(crossbeam_channel::Sender<Option<Invalidation>>),
    Replace(
        Option<Invalidation>,
        crossbeam_channel::Sender<Option<Invalidation>>,
    ),
    Join(tokio::task::JoinHandle<()>),
    #[cfg(test)]
    Panic(crossbeam_channel::Sender<()>),
    #[cfg(test)]
    Snapshot(crossbeam_channel::Sender<(std::collections::HashSet<std::path::PathBuf>, usize)>),
}

#[derive(Clone)]
pub(super) struct Handle {
    sender: crossbeam_channel::Sender<Request>,
    alive: Arc<AtomicBool>,
    wake: async_channel::Sender<()>,
    events: async_channel::Receiver<()>,
}

/// A cancelled await must transfer the SAME original handle, not detach it.
/// This contains no native resource and can be dropped on the caller thread.
pub(super) struct Joining {
    original: Option<tokio::task::JoinHandle<()>>,
    cache: Handle,
}

impl Joining {
    pub fn new(original: tokio::task::JoinHandle<()>, cache: Handle) -> Self {
        Self {
            original: Some(original),
            cache,
        }
    }
    pub async fn complete(mut self) -> bool {
        let completed = self.original.as_mut().expect("original join").await.is_ok();
        self.original = None;
        completed
    }
}

impl Drop for Joining {
    fn drop(&mut self) {
        if let Some(original) = self.original.take() {
            self.cache.abandoned(original);
        }
    }
}

pub(super) struct Owner {
    receiver: crossbeam_channel::Receiver<Request>,
    alive: Arc<AtomicBool>,
    cached: Option<Invalidation>,
    // At most one returning watcher if its preparation worker panics before
    // receiving the acknowledgement. It is not an additional cache entry.
    returning: Option<Invalidation>,
    abandoned: Vec<tokio::task::JoinHandle<()>>,
}

impl Handle {
    pub fn channel() -> (Self, Owner) {
        let (sender, receiver) = crossbeam_channel::unbounded();
        let (wake, events) = async_channel::bounded(1);
        let alive = Arc::new(AtomicBool::new(true));
        (
            Self {
                sender,
                alive: alive.clone(),
                wake,
                events,
            },
            Owner {
                receiver,
                alive,
                cached: None,
                returning: None,
                abandoned: Vec::with_capacity(1),
            },
        )
    }

    pub fn alive(&self) -> bool {
        self.alive.load(Ordering::Acquire)
    }

    pub fn watch_state(
        &self,
        #[cfg(test)] retired: Option<Arc<dyn Fn() + Send + Sync>>,
        #[cfg(test)] unavailable: bool,
    ) -> Arc<WatchState> {
        #[cfg(not(test))]
        let unavailable = false;
        Arc::new(WatchState {
            epoch: AtomicU64::new(0),
            healthy: AtomicBool::new(!unavailable),
            wake: self.wake.clone(),
            #[cfg(test)]
            retired,
        })
    }

    pub fn events(&self) -> async_channel::Receiver<()> {
        self.events.clone()
    }

    /// Worker-only. The previous watcher is returned to this same admitted
    /// worker for retirement; no registration/drop is delegated to the UI.
    pub fn replace(&self, next: Option<Invalidation>) {
        let (send, receive) = crossbeam_channel::bounded(1);
        if !self.alive() || self.sender.send(Request::Take(send)).is_err() {
            drop(next);
            return;
        }
        match receive.recv() {
            Ok(previous) => {
                drop(previous);
                if self.alive() {
                    // Acknowledgement returns any rejected native resource to
                    // this worker, rather than destroying it on the caller.
                    let (reply, ack) = crossbeam_channel::bounded(1);
                    if self.sender.send(Request::Replace(next, reply)).is_ok() {
                        if let Ok(rejected) = ack.recv() {
                            drop(rejected);
                        }
                    }
                }
            }
            Err(_) => {
                self.alive.store(false, Ordering::Release);
            }
        }
    }

    #[cfg(test)]
    pub fn snapshot(&self) -> (std::collections::HashSet<std::path::PathBuf>, usize) {
        let (send, receive) = crossbeam_channel::bounded(1);
        self.sender.send(Request::Snapshot(send)).unwrap();
        receive
            .recv_timeout(std::time::Duration::from_secs(3))
            .unwrap()
    }

    /// Drop of the last Client is metadata-only. This send never waits for a
    /// receiver; no callback or preparation loop can enqueue additional Joins.
    pub fn abandoned(&self, join: tokio::task::JoinHandle<()>) {
        if let Err(error) = self.sender.send(Request::Join(join)) {
            // Only an unstarted/failed owner can disconnect while a sender is
            // alive. It has never accepted a native watcher. The original
            // per-ticket worker still owns/drains its temporary resources;
            // dropping this handle detaches it, not a joined/settled receipt.
            // Cache hits are already disabled by Owner::drop's alive=false.
            drop(error);
        }
    }
}

impl Owner {
    /// Called inside the existing blocking preparation worker, never App.update.
    pub fn start(self) {
        // On spawn failure, the unstarted owner contains no native watcher.
        let _ = std::thread::Builder::new()
            .name("worktree-cache-custody".into())
            .spawn(move || self.run());
    }

    fn run(mut self) {
        while let Ok(request) = self.receiver.recv() {
            // Keep the owned slot/receiver/Join outside the unwind boundary.
            // A processing panic disables hits but still services the active
            // worker's Take reply, avoiding a worker/custodian shutdown deadlock.
            #[cfg(test)]
            let panic_ack = match &request {
                Request::Panic(reply) => Some(reply.clone()),
                _ => None,
            };
            let handled = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                match request {
                    Request::Take(reply) => {
                        let value = if self.alive.load(Ordering::Acquire) {
                            self.cached.take()
                        } else {
                            None
                        };
                        if let Err(error) = reply.send(value) {
                            // A disabled custodian returned None while still
                            // retaining its old slot. Never overwrite/drop that
                            // native resource because a None reply was lost.
                            if let Some(returned) = error.0 {
                                if self.cached.is_none() {
                                    self.cached = Some(returned);
                                } else {
                                    self.returning = Some(returned);
                                }
                            }
                            self.alive.store(false, Ordering::Release);
                        }
                    }
                    Request::Replace(value, reply) => {
                        // The sole worker took/retired the previous slot first.
                        let rejected =
                            if self.alive.load(Ordering::Acquire) && self.cached.is_none() {
                                self.cached = value;
                                None
                            } else {
                                self.alive.store(false, Ordering::Release);
                                value
                            };
                        if let Err(error) = reply.send(rejected) {
                            // Receiver loss means the admitted worker unwound.
                            // Preserve the one returning resource until the
                            // same original worker's join, not an actor drop.
                            self.returning = error.0;
                            self.alive.store(false, Ordering::Release);
                        }
                    }
                    Request::Join(join) => {
                        // Normal retirements await/take their own handle. An
                        // abandoned active ticket cannot admit a successor, so
                        // this vector has at most one entry, not job history.
                        self.abandoned.push(join);
                    }
                    #[cfg(test)]
                    Request::Panic(_) => panic!("cache custody control"),
                    #[cfg(test)]
                    Request::Snapshot(reply) => {
                        let snapshot = self
                            .cached
                            .as_ref()
                            .map(|cached| {
                                (
                                    cached
                                        .targets
                                        .lock()
                                        .unwrap_or_else(|error| error.into_inner())
                                        .clone(),
                                    cached.registered.len(),
                                )
                            })
                            .unwrap_or_default();
                        let _ = reply.send(snapshot);
                    }
                }
            }));
            if handled.is_err() {
                self.alive.store(false, Ordering::Release);
            }
            #[cfg(test)]
            if let Some(ack) = panic_ack {
                let _ = ack.send(());
            }
        }
        // Receiver closure proves every UI and admitted-worker sender is gone.
        // Never await a worker while it might still be awaiting a Take reply.
        for join in self.abandoned.drain(..) {
            let _ = iced::futures::executor::block_on(join);
        }
        drop(self.cached.take());
        drop(self.returning.take());
    }
}

impl Drop for Owner {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{thread, time::Duration};

    fn original_join_before_native_drop(panic: bool) {
        let ui = thread::current().id();
        let (handle, owner) = Handle::channel();
        // This control creates/installs a real native watcher on an off-UI
        // preparation thread, then uses the actual custody channel/Join path.
        owner.start();
        let (dropped, observed) = crossbeam_channel::bounded(1);
        let state = handle.watch_state(
            Some(Arc::new(move || {
                dropped.send(thread::current().id()).unwrap();
            })),
            false,
        );
        let installer = handle.clone();
        thread::spawn(move || installer.replace(Some(Invalidation::new(state))))
            .join()
            .unwrap();
        assert!(observed.try_recv().is_err());
        if panic {
            let (ack, done) = crossbeam_channel::bounded(1);
            handle.sender.send(Request::Panic(ack)).unwrap();
            done.recv_timeout(Duration::from_secs(3)).unwrap();
            assert!(!handle.alive());
            assert!(observed.try_recv().is_err());
            let (reply, lost) = crossbeam_channel::bounded(1);
            drop(lost);
            // A failed None reply must not overwrite the retained old slot.
            handle.sender.send(Request::Take(reply)).unwrap();
        }
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        let (past_sender, at_join) = crossbeam_channel::bounded(1);
        let (release, wait) = crossbeam_channel::bounded(1);
        let worker_sender = handle.clone();
        let original = runtime.spawn_blocking(move || {
            drop(worker_sender);
            past_sender.send(()).unwrap();
            wait.recv_timeout(Duration::from_secs(10)).unwrap();
        });
        handle.abandoned(original);
        drop(handle);
        at_join.recv_timeout(Duration::from_secs(3)).unwrap();
        // All custody senders are gone, but the original worker is still live.
        // A detached handle/closure-only proof would incorrectly drop here.
        assert!(observed.recv_timeout(Duration::from_millis(40)).is_err());
        release.send(()).unwrap();
        assert_ne!(observed.recv_timeout(Duration::from_secs(3)).unwrap(), ui);
        runtime.shutdown_timeout(Duration::from_secs(3));
    }

    #[test]
    fn idle_custodian_waits_for_actual_abandoned_worker_join_before_watcher_destruction() {
        original_join_before_native_drop(false);
    }

    #[test]
    fn custody_panic_and_lost_slot_reply_retain_watcher_until_original_join() {
        original_join_before_native_drop(true);
    }

    #[test]
    fn target_overflow_disables_cache_while_native_registration_and_inspection_stay_bounded() {
        thread::spawn(|| {
            let directory = tempfile::tempdir().unwrap();
            let (handle, _unstarted) = Handle::channel();
            let state = handle.watch_state(None, false);
            let mut watcher = Invalidation::new(state.clone());
            watcher
                .register((0..=MAX_TARGETS).map(|i| directory.path().join(format!("missing-{i}"))))
                .unwrap();
            assert!(!state.healthy.load(Ordering::Acquire));
            assert_eq!(watcher.targets.lock().unwrap().len(), MAX_TARGETS);
            assert!(watcher.registered.len() <= MAX_REGISTRATIONS);
            assert!(watcher.inspected.len() <= MAX_REGISTRATIONS);
        })
        .join()
        .unwrap();
    }

    #[test]
    fn cancelled_unstarted_owner_never_publishes_native_custody_or_joined_cleanup() {
        let (handle, owner) = Handle::channel();
        // The initial navigation Task can be dropped before its first poll.
        // Its unstarted receiver owns no native slot and disables all hits.
        drop(owner);
        assert!(!handle.alive());
        assert!(handle.sender.is_empty());
    }
}
