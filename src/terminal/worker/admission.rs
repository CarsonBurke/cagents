//! Credits follow accepted commands through every worker-owned queue.
use std::sync::{Arc, Condvar, Mutex};
use std::time::Instant;

#[derive(Debug)]
pub(super) struct Budget {
    capacity: usize,
    available: Mutex<usize>,
    changed: Condvar,
}

#[derive(Debug)]
pub(super) struct Permit {
    budget: Arc<Budget>,
    weight: usize,
    storage: Option<Arc<StoragePermit>>,
}

impl Budget {
    pub(super) fn new(capacity: usize) -> Arc<Self> {
        let capacity = capacity.max(1);
        Arc::new(Self {
            capacity,
            available: Mutex::new(capacity),
            changed: Condvar::new(),
        })
    }

    pub(super) fn try_acquire(self: &Arc<Self>) -> Option<Arc<Permit>> {
        self.try_weighted(1)
    }
    fn try_weighted(self: &Arc<Self>, weight: usize) -> Option<Arc<Permit>> {
        let mut available = self.available.lock().unwrap_or_else(|p| p.into_inner());
        if *available < weight {
            return None;
        }
        *available -= weight;
        Some(Arc::new(Permit {
            budget: self.clone(),
            weight,
            storage: None,
        }))
    }
    pub(super) fn acquire_until(self: &Arc<Self>, deadline: Instant) -> Option<Arc<Permit>> {
        let mut available = self.available.lock().unwrap_or_else(|p| p.into_inner());
        loop {
            if Instant::now() >= deadline {
                return None;
            }
            if *available != 0 {
                *available -= 1;
                return Some(Arc::new(Permit {
                    budget: Arc::clone(self),
                    weight: 1,
                    storage: None,
                }));
            }
            let remaining = deadline.checked_duration_since(Instant::now())?;
            let (next, _) = self
                .changed
                .wait_timeout(available, remaining)
                .unwrap_or_else(|p| p.into_inner());
            available = next;
        }
    }

    #[cfg(test)]
    pub(super) fn used(&self) -> usize {
        self.capacity - *self.available.lock().unwrap_or_else(|p| p.into_inner())
    }
}

impl Drop for Permit {
    fn drop(&mut self) {
        let mut available = self
            .budget
            .available
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        debug_assert!(*available + self.weight <= self.budget.capacity);
        *available += self.weight;
        self.budget.changed.notify_one();
    }
}

// Source-pinned encoders emit <=8 bytes per associated text byte plus
// <128 fixed bytes. Mouse/release formats contain only fixed numeric fields.
// Admission reserves both bytes and frames before acknowledging user input.
pub(super) const ORDINARY_BYTES: usize = 2 * 1024 * 1024 + 32;
const ORDINARY_FRAMES: usize = 1024;
const CLEANUP_BYTES: usize = 512;
const CLEANUP_FRAMES: usize = 4;
#[derive(Debug)]
pub(super) struct StoragePool {
    bytes: Arc<Budget>,
    frames: Arc<Budget>,
}
#[derive(Debug)]
pub(super) struct StoragePermit {
    _bytes: Arc<Permit>,
    _frames: Arc<Permit>,
}
impl StoragePool {
    pub(super) fn ordinary() -> Self {
        Self::new(ORDINARY_BYTES, ORDINARY_FRAMES)
    }
    fn new(bytes: usize, frames: usize) -> Self {
        Self {
            bytes: Budget::new(bytes),
            frames: Budget::new(frames),
        }
    }
    pub(super) fn reserve(&self, bytes: usize, frames: usize) -> Option<Arc<StoragePermit>> {
        Some(Arc::new(StoragePermit {
            _bytes: self.bytes.try_weighted(bytes)?,
            _frames: self.frames.try_weighted(frames)?,
        }))
    }
}
impl Permit {
    pub(super) fn attach_storage(permit: &mut Arc<Self>, storage: Arc<StoragePermit>) {
        Arc::get_mut(permit)
            .expect("new admission permit is unshared")
            .storage = Some(storage);
    }
    pub(super) fn has_storage(&self) -> bool {
        self.storage.is_some()
    }
    pub(super) fn generated(storage: Arc<StoragePermit>) -> Arc<Self> {
        let mut permit = Budget::new(1).try_acquire().unwrap();
        Self::attach_storage(&mut permit, storage);
        permit
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn storage_refusal_releases_partial_reservations_and_transfer_stays_booked() {
        let pool = StoragePool::new(128, 1);
        let storage = pool.reserve(64, 1).unwrap();
        assert!(pool.reserve(64, 1).is_none());
        assert_eq!(pool.bytes.used(), 64);
        let mut command = Budget::new(1).try_acquire().unwrap();
        Permit::attach_storage(&mut command, storage);
        let frame = command.clone();
        drop(command);
        assert!(pool.reserve(65, 1).is_none());
        drop(frame);
        assert_eq!(pool.bytes.used(), 0);
        assert_eq!(pool.frames.used(), 0);
        assert!(pool.reserve(128, 1).is_some());
    }
    #[test]
    fn sharing_one_credit_across_stash_and_bytes_cannot_reopen_admission_early() {
        let budget = Budget::new(1);
        let command = budget.try_acquire().unwrap();
        let pending_bytes = Arc::clone(&command);
        drop(command);
        assert_eq!(budget.used(), 1);
        assert!(budget.try_acquire().is_none());
        drop(pending_bytes);
        assert_eq!(budget.used(), 0);
        assert!(budget.try_acquire().is_some());
    }

    #[test]
    fn blocking_ownership_admission_has_a_bounded_deadline() {
        let budget = Budget::new(1);
        let _owner = budget.try_acquire().unwrap();
        assert!(
            budget
                .acquire_until(Instant::now() + Duration::from_millis(2))
                .is_none()
        );
        assert_eq!(budget.used(), 1);
    }
}

// Cleanup capacity belongs to an admitted press, never to arbitrary later
// control traffic. Its one release can enter even when bulk input owns every
// ordinary command credit.
pub(super) const CLEANUP_LIMIT: usize = 512;
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Identity {
    Key(super::TerminalKey, Option<char>),
    Mouse(super::MouseButton),
}
impl Identity {
    pub(super) fn key(input: &super::KeyInput) -> Self {
        let key = match input.key {
            super::TerminalKey::Character(value) => {
                super::TerminalKey::Character(input.unshifted_codepoint.unwrap_or(value))
            }
            key => key,
        };
        Self::Key(key, input.unshifted_codepoint)
    }
}
#[derive(Debug)]
pub(super) struct Cleanup {
    identity: Identity,
    phase: std::sync::atomic::AtomicU8,
    pending: std::sync::atomic::AtomicUsize,
    enqueued: std::sync::atomic::AtomicBool,
    wake: crossbeam_channel::Sender<()>,
    _permit: Arc<Permit>,
    _storage: Arc<StoragePermit>,
}
#[derive(Debug)]
pub(super) struct QueuedPress {
    claim: Arc<Cleanup>,
}
#[derive(Debug)]
pub(super) struct CleanupPool {
    budget: Arc<Budget>,
    claims: Mutex<Vec<std::sync::Weak<Cleanup>>>,
    wake: crossbeam_channel::Sender<()>,
    storage: StoragePool,
}
impl CleanupPool {
    pub(super) fn new(wake: crossbeam_channel::Sender<()>) -> Self {
        Self {
            budget: Budget::new(CLEANUP_LIMIT),
            storage: StoragePool::new(
                CLEANUP_LIMIT * CLEANUP_BYTES,
                CLEANUP_LIMIT * CLEANUP_FRAMES,
            ),
            claims: Mutex::new(Vec::new()),
            wake,
        }
    }
    pub(super) fn find(&self, identity: Identity) -> Option<Arc<Cleanup>> {
        let mut claims = self.claims.lock().unwrap_or_else(|p| p.into_inner());
        claims.retain(|claim| claim.strong_count() != 0);
        let available = claims
            .iter()
            .rev()
            .filter_map(std::sync::Weak::upgrade)
            .find(|claim| {
                claim.identity == identity
                    && claim.needs_cleanup()
                    && !claim.enqueued.load(std::sync::atomic::Ordering::Acquire)
            });
        available.or_else(|| {
            claims
                .iter()
                .rev()
                .filter_map(std::sync::Weak::upgrade)
                .find(|claim| claim.identity == identity && claim.needs_cleanup())
        })
    }
    pub(super) fn press(&self, identity: Identity) -> Option<(Arc<Cleanup>, Option<QueuedPress>)> {
        let mut claims = self.claims.lock().unwrap_or_else(|p| p.into_inner());
        claims.retain(|claim| claim.strong_count() != 0);
        if let Some(claim) = claims
            .iter()
            .filter_map(std::sync::Weak::upgrade)
            .find(|claim| {
                claim.identity == identity
                    && claim.needs_cleanup()
                    && !claim.enqueued.load(std::sync::atomic::Ordering::Acquire)
            })
        {
            let ticket = claim.queue_press()?;
            return Some((claim, Some(ticket)));
        }
        let permit = self.budget.try_acquire()?;
        let claim = Arc::new(Cleanup {
            identity,
            phase: std::sync::atomic::AtomicU8::new(0),
            pending: std::sync::atomic::AtomicUsize::new(0),
            enqueued: std::sync::atomic::AtomicBool::new(false),
            wake: self.wake.clone(),
            _permit: permit,
            _storage: self.storage.reserve(CLEANUP_BYTES, CLEANUP_FRAMES)?,
        });
        claims.push(Arc::downgrade(&claim));
        let ticket = claim.queue_press()?;
        Some((claim, Some(ticket)))
    }
}
impl Cleanup {
    fn needs_cleanup(&self) -> bool {
        // Cancellation can remove the older unsent down while a newer, already
        // accepted repeat is still queued. Its reserved up must remain reachable.
        self.phase.load(std::sync::atomic::Ordering::Acquire) < 2
            || self.pending.load(std::sync::atomic::Ordering::Acquire) != 0
    }
    pub(super) fn ready(&self) -> bool {
        self.phase.load(std::sync::atomic::Ordering::Acquire) != 0
            && self.pending.load(std::sync::atomic::Ordering::Acquire) == 0
    }
    pub(super) fn queue_press(self: &Arc<Self>) -> Option<QueuedPress> {
        if self.enqueued.load(std::sync::atomic::Ordering::Acquire) {
            return None;
        }
        self.pending
            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Some(QueuedPress {
            claim: self.clone(),
        })
    }
    pub(super) fn enqueue_once(&self) -> bool {
        !self
            .enqueued
            .swap(true, std::sync::atomic::Ordering::AcqRel)
    }
    pub(super) fn encoded(&self) {
        self.phase.store(1, std::sync::atomic::Ordering::Release);
        let _ = self.wake.try_send(());
    }
    pub(super) fn canceled(&self) {
        self.phase.store(2, std::sync::atomic::Ordering::Release);
        let _ = self.wake.try_send(());
    }
    pub(super) fn released(&self) {
        self.phase.store(3, std::sync::atomic::Ordering::Release);
    }
}
impl Drop for QueuedPress {
    fn drop(&mut self) {
        let before = self
            .claim
            .pending
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
        debug_assert!(before != 0);
        if before == 1 {
            let _ = self.claim.phase.compare_exchange(
                0,
                2,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            );
            let _ = self.claim.wake.try_send(());
        }
    }
}

#[cfg(test)]
mod cleanup_tests {
    use super::*;
    fn identity() -> Identity {
        Identity::Key(super::super::TerminalKey::Character('a'), Some('a'))
    }
    #[test]
    fn release_readiness_waits_for_all_accepted_downs_and_repeats() {
        let (wake, _rx) = crossbeam_channel::bounded(1);
        let pool = CleanupPool::new(wake);
        let (claim, first) = pool.press(identity()).unwrap();
        let later_repeat = claim.queue_press().unwrap();
        assert!(claim.enqueue_once());
        drop(first);
        assert!(
            !claim.ready(),
            "a canceled earlier down cannot let up overtake a retained later repeat"
        );
        claim.encoded();
        assert!(!claim.ready());
        drop(later_repeat);
        assert!(claim.ready());
        assert!(!claim.enqueue_once());
    }
    #[test]
    fn coalesced_priority_requests_share_one_reserved_cleanup_capacity() {
        let (wake, _rx) = crossbeam_channel::bounded(1);
        let pool = CleanupPool::new(wake);
        let (claim, mut ticket) = pool.press(identity()).unwrap();
        for _ in 0..100 {
            let (next, next_ticket) = pool.press(identity()).unwrap();
            assert!(Arc::ptr_eq(&claim, &next));
            ticket = next_ticket;
            assert!(!claim.ready());
            assert_eq!(pool.budget.used(), 1);
        }
        claim.encoded();
        drop(ticket);
        assert!(claim.ready());
    }
    #[test]
    fn cleanup_capacity_is_reserved_before_accepting_new_presses() {
        let (wake, _rx) = crossbeam_channel::bounded(1);
        let pool = CleanupPool::new(wake);
        let owners = (0..CLEANUP_LIMIT)
            .map(|index| {
                pool.press(Identity::Key(
                    super::super::TerminalKey::Character(
                        char::from_u32(0x1000 + index as u32).unwrap(),
                    ),
                    None,
                ))
                .unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(pool.budget.used(), CLEANUP_LIMIT);
        assert!(pool.press(identity()).is_none());
        drop(owners);
        assert!(pool.press(identity()).is_some());
    }
}
