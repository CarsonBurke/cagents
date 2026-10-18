//! Bounded live authority for filesystem recovery across isolated build lanes. Descriptions are diagnostic
//! spellings; only the producer's retained descriptors authorize retry.
use std::sync::{
    Arc, Mutex, OnceLock,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};

// Each admitted producer may retain at most sixteen descriptors. Reserving
// before any effect bounds unresolved work without a global authority registry.
const MAX_RECOVERIES: usize = 32;
pub(crate) const MAX_RECOVERY_DESCRIPTORS: usize = 16;
struct Budget {
    used: AtomicUsize,
    limit: usize,
}
fn budget() -> Arc<Budget> {
    static BUDGET: OnceLock<Arc<Budget>> = OnceLock::new();
    Arc::clone(BUDGET.get_or_init(|| {
        Arc::new(Budget {
            used: AtomicUsize::new(0),
            limit: MAX_RECOVERIES,
        })
    }))
}

/// Shared already-acquired cleanup guards have their own measured FD budget,
/// separate from each producer's <=16 private descriptors. Sharing a context
/// across all leaves counts its native descriptors once, not once per clone.
const MAX_RECOVERY_CONTEXTS: usize = 32;
const MAX_RECOVERY_CONTEXT_DESCRIPTORS: usize = 128;
#[derive(Default)]
struct ContextUsage {
    contexts: usize,
    descriptors: usize,
}
fn context_budget() -> Arc<Mutex<ContextUsage>> {
    static BUDGET: OnceLock<Arc<Mutex<ContextUsage>>> = OnceLock::new();
    Arc::clone(BUDGET.get_or_init(|| Arc::new(Mutex::new(ContextUsage::default()))))
}
struct ContextInner {
    // Dropping an owned lease closes its FDs; no namespace mutation runs here.
    guard: Option<Box<dyn Send + Sync>>,
    descriptors: usize,
    budget: Arc<Mutex<ContextUsage>>,
}
impl Drop for ContextInner {
    fn drop(&mut self) {
        // Keep charged capacity until the actual native guards have closed.
        // Never hold the budget mutex across a producer's guard destructor.
        drop(self.guard.take());
        let mut used = self
            .budget
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        used.contexts -= 1;
        used.descriptors -= self.descriptors;
    }
}
/// Pre-effect ownership of an exact cleanup lease. No path-based reacquisition
/// occurs on failure. Static guards charge their actual retained FD count;
/// an effectful bootstrap reserves a documented upper bound before creating
/// handles and keeps that capacity charged through all pending cleanup.
#[derive(Clone)]
pub(crate) struct RecoveryContext(Arc<ContextInner>);
impl RecoveryContext {
    pub(crate) fn with_guard(
        guard: impl Send + Sync + 'static,
        descriptors: usize,
    ) -> Result<Self, String> {
        Self::from_budget(guard, descriptors, context_budget())
    }
    fn from_budget(
        guard: impl Send + Sync + 'static,
        descriptors: usize,
        budget: Arc<Mutex<ContextUsage>>,
    ) -> Result<Self, String> {
        let mut used = budget
            .lock()
            .map_err(|_| "Recovery cleanup-lease budget is unavailable".to_owned())?;
        let total = used
            .descriptors
            .checked_add(descriptors)
            .ok_or("Recovery cleanup-lease descriptor budget is exhausted")?;
        if used.contexts >= MAX_RECOVERY_CONTEXTS || total > MAX_RECOVERY_CONTEXT_DESCRIPTORS {
            return Err("Recovery cleanup-lease capacity is exhausted. Resolve earlier recovery before changing files.".into());
        }
        used.contexts += 1;
        used.descriptors = total;
        drop(used);
        Ok(Self(Arc::new(ContextInner {
            guard: Some(Box::new(guard)),
            descriptors,
            budget,
        })))
    }
    /// Reserve before producer effects. The resulting unresolved leaf owns
    /// this exact pre-acquired context until success or its final owner drop.
    pub(crate) fn reserve(&self) -> Result<RecoverySlot, String> {
        let mut slot = RecoverySlot::reserve()?;
        slot.context = Some(self.clone());
        Ok(slot)
    }
}

/// Reserve one bounded recovery payload before touching the filesystem.
/// Producers must retain no more than MAX_RECOVERY_DESCRIPTORS descriptors.
pub(crate) struct RecoverySlot {
    budget: Arc<Budget>,
    context: Option<RecoveryContext>,
    // Exact creation admission, with no filesystem mutation or IPC in Drop.
    // The actual issuer uses FD-free counter bookkeeping. Any native owners
    // supplied by another lease must already be charged in this context.
    creation_guard: Option<Arc<dyn Send + Sync>>,
}
impl RecoverySlot {
    pub(crate) fn reserve() -> Result<Self, String> {
        Self::from_budget(budget())
    }
    fn from_budget(budget: Arc<Budget>) -> Result<Self, String> {
        budget
            .used
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                (used < budget.limit).then_some(used + 1)
            })
            .map_err(|_| {
                "Recovery capacity is exhausted. Resolve an earlier recovery before changing files."
                    .to_owned()
            })?;
        Ok(Self {
            budget,
            context: None,
            creation_guard: None,
        })
    }
    pub(crate) fn with_creation_guard(mut self, guard: Arc<dyn Send + Sync>) -> Self {
        // Only fresh pre-effect slots enter this constructor. The same guard
        // then travels through the factory, producer and recovery authority.
        debug_assert!(self.creation_guard.is_none());
        self.creation_guard = Some(guard);
        self
    }
    pub(crate) fn retain(
        self,
        description: String,
        retry: impl FnMut() -> Result<(), String> + Send + 'static,
    ) -> RecoveryAuthority {
        RecoveryAuthority(Arc::new(RecoveryInner {
            description: description.chars().take(2048).collect(),
            resolved: AtomicBool::new(false),
            leaves: None,
            work: Mutex::new(RecoveryWork {
                retry: Some(Box::new(retry)),
                slot: Some(self),
            }),
        }))
    }
}
impl Drop for RecoverySlot {
    fn drop(&mut self) {
        // The current issuer's creation guard is FD-free bookkeeping. The
        // shared lease contract also permits already-accounted native owners:
        // destroy that guard while its context is still charged, then release
        // the context and private reservation. No budget mutex spans Drop.
        drop(self.creation_guard.take());
        drop(self.context.take());
        self.budget.used.fetch_sub(1, Ordering::AcqRel);
    }
}

struct RecoveryWork {
    retry: Option<Box<dyn FnMut() -> Result<(), String> + Send>>,
    slot: Option<RecoverySlot>,
}
struct RecoveryInner {
    description: String,
    resolved: AtomicBool,
    work: Mutex<RecoveryWork>,
    // A group retains only reserved leaves, never another unreserved group.
    leaves: Option<Vec<RecoveryAuthority>>,
}

/// A producer-owned recovery operation, retained by its exact UI error/result.
/// Clones share one payload and one budget slot; there is no Drop filesystem IO.
#[derive(Clone)]
pub struct RecoveryAuthority(Arc<RecoveryInner>);
impl RecoveryAuthority {
    /// Join existing bounded payloads without reserving a new slot after an
    /// effect. Nested cleanup errors must not discard an earlier authority.
    pub(crate) fn combine(self, other: Self) -> Self {
        if self == other {
            return self;
        }
        let mut leaves = Vec::new();
        for authority in [&self, &other] {
            let candidates = authority
                .0
                .leaves
                .as_deref()
                .unwrap_or_else(|| std::slice::from_ref(authority));
            for leaf in candidates {
                if !leaf.is_resolved() && !leaves.contains(leaf) {
                    leaves.push(leaf.clone());
                }
            }
        }
        if leaves.is_empty() {
            return self;
        }
        if leaves.len() == 1 {
            return leaves.pop().unwrap();
        }
        // Every unresolved leaf owns a pre-effect reservation. Production's
        // single shared budget bounds this flat list by MAX_RECOVERIES, even
        // when callers repeatedly combine overlapping groups.
        let description = leaves
            .iter()
            .map(Self::description)
            .collect::<Vec<_>>()
            .join("; ")
            .chars()
            .take(2048)
            .collect();
        let retries = leaves.clone();
        Self(Arc::new(RecoveryInner {
            description,
            resolved: AtomicBool::new(false),
            leaves: Some(leaves),
            work: Mutex::new(RecoveryWork {
                retry: Some(Box::new(move || {
                    for leaf in &retries {
                        leaf.retry()?;
                    }
                    Ok(())
                })),
                slot: None,
            }),
        }))
    }
    pub fn description(&self) -> &str {
        &self.0.description
    }
    pub fn is_resolved(&self) -> bool {
        // Groups are immutable flat views of reserved leaves. A newer group
        // can resolve those same leaves without retrying this exact older Arc.
        // Old refusal markers must then observe completion as well.
        match &self.0.leaves {
            Some(leaves) => leaves
                .iter()
                .all(|leaf| leaf.0.resolved.load(Ordering::Acquire)),
            None => self.0.resolved.load(Ordering::Acquire),
        }
    }
    /// Blocking worker only. The producer revalidates retained authority and
    /// must refuse foreign names/no-clobber collisions on every retry.
    pub(crate) fn retry(&self) -> Result<(), String> {
        let mut work = self.0.work.lock().map_err(|_| {
            "Editor recovery previously failed unexpectedly; its retained authority was kept."
                .to_owned()
        })?;
        if let Some(retry) = work.retry.as_mut() {
            retry()?;
        }
        work.retry = None;
        work.slot = None;
        self.0.resolved.store(true, Ordering::Release);
        Ok(())
    }
}
impl std::fmt::Debug for RecoveryAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryAuthority")
            .field("description", &self.description())
            .field("resolved", &self.is_resolved())
            .finish()
    }
}
impl PartialEq for RecoveryAuthority {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}
impl Eq for RecoveryAuthority {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn editor_recovery_budget_and_clones_keep_one_payload_until_actual_resolution() {
        let budget = Arc::new(Budget {
            used: AtomicUsize::new(0),
            limit: 1,
        });
        let slot = RecoverySlot::from_budget(Arc::clone(&budget)).unwrap();
        assert!(RecoverySlot::from_budget(Arc::clone(&budget)).is_err());
        let attempts = Arc::new(AtomicUsize::new(0));
        let count = Arc::clone(&attempts);
        let authority = slot.retain("diagnostic spelling only".into(), move || {
            if count.fetch_add(1, Ordering::AcqRel) == 0 {
                Err("foreign destination was preserved".into())
            } else {
                Ok(())
            }
        });
        let clone = authority.clone();
        assert_eq!(authority, clone);
        assert!(authority.retry().is_err());
        assert!(!clone.is_resolved());
        assert!(RecoverySlot::from_budget(Arc::clone(&budget)).is_err());
        clone.retry().unwrap();
        assert!(authority.is_resolved());
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        authority.retry().unwrap();
        assert_eq!(attempts.load(Ordering::Acquire), 2);
    }
    #[test]
    fn same_slot_creation_guard_survives_failed_retry_until_producer_resolution() {
        let budget = Arc::new(Budget {
            used: AtomicUsize::new(0),
            limit: 1,
        });
        let guard = Arc::new(());
        let weak = Arc::downgrade(&guard);
        let attempts = Arc::new(AtomicUsize::new(0));
        let captured = attempts.clone();
        let authority = RecoverySlot::from_budget(budget.clone())
            .unwrap()
            .with_creation_guard(guard)
            .retain("original admitted creation".into(), move || {
                if captured.fetch_add(1, Ordering::AcqRel) == 0 {
                    Err("retained operation is still unresolved".into())
                } else {
                    Ok(())
                }
            });
        assert!(weak.upgrade().is_some());
        assert!(authority.retry().is_err());
        assert!(weak.upgrade().is_some());
        assert!(RecoverySlot::from_budget(budget.clone()).is_err());
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(weak.upgrade().is_none());
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }

    #[test]
    fn creation_guard_destruction_cannot_outlive_its_charged_context() {
        struct BlockingDrop {
            entered: Arc<std::sync::Barrier>,
            release: Arc<std::sync::Barrier>,
        }
        impl Drop for BlockingDrop {
            fn drop(&mut self) {
                self.entered.wait();
                self.release.wait();
            }
        }
        let used = Arc::new(Mutex::new(ContextUsage::default()));
        let context =
            RecoveryContext::from_budget((), MAX_RECOVERY_CONTEXT_DESCRIPTORS, used.clone())
                .unwrap();
        let entered = Arc::new(std::sync::Barrier::new(2));
        let release = Arc::new(std::sync::Barrier::new(2));
        let mut slot = RecoverySlot::from_budget(Arc::new(Budget {
            used: AtomicUsize::new(0),
            limit: 1,
        }))
        .unwrap();
        slot.context = Some(context);
        let slot = slot.with_creation_guard(Arc::new(BlockingDrop {
            entered: entered.clone(),
            release: release.clone(),
        }));
        let worker = std::thread::spawn(move || drop(slot));
        entered.wait();
        let refused_while_guard_drops = RecoveryContext::from_budget((), 1, used.clone()).is_err();
        // Always unblock/join before asserting, including the failing ordering.
        release.wait();
        worker.join().unwrap();
        assert!(
            refused_while_guard_drops,
            "guard-held native owners remain charged until destruction completes"
        );
        assert!(RecoveryContext::from_budget((), 1, used).is_ok());
    }

    #[test]
    fn recovery_combine_flattens_and_deduplicates_repeated_overlapping_groups() {
        let budget = Arc::new(Budget {
            used: AtomicUsize::new(0),
            limit: 2,
        });
        let a = RecoverySlot::from_budget(Arc::clone(&budget))
            .unwrap()
            .retain("a".into(), || Ok(()));
        let b = RecoverySlot::from_budget(Arc::clone(&budget))
            .unwrap()
            .retain("b".into(), || Ok(()));
        let mut combined = a.clone().combine(b.clone());
        for _ in 0..10_000 {
            combined = combined.combine(a.clone()).combine(b.clone());
            let leaves = combined.0.leaves.as_ref().unwrap();
            assert_eq!(leaves.len(), 2);
            assert!(leaves.iter().all(|leaf| leaf.0.leaves.is_none()));
            assert_eq!(combined.description(), "a; b");
        }
        assert_eq!(budget.used.load(Ordering::Acquire), 2);
        a.retry().unwrap();
        assert_eq!(combined.combine(b.clone()), b);
        b.retry().unwrap();
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }
    #[test]
    fn older_exact_group_observes_its_leaves_resolving_through_a_newer_group() {
        let budget = Arc::new(Budget {
            used: AtomicUsize::new(0),
            limit: 3,
        });
        let a = RecoverySlot::from_budget(budget.clone())
            .unwrap()
            .retain("a".into(), || Ok(()));
        let b = RecoverySlot::from_budget(budget.clone())
            .unwrap()
            .retain("b".into(), || Ok(()));
        let attempts = Arc::new(AtomicUsize::new(0));
        let captured = attempts.clone();
        let c = RecoverySlot::from_budget(budget.clone())
            .unwrap()
            .retain("c".into(), move || {
                if captured.fetch_add(1, Ordering::AcqRel) == 0 {
                    Err("c remains pending".into())
                } else {
                    Ok(())
                }
            });
        let original = a.combine(b);
        let marker = original.clone();
        let newer = original.combine(c);
        assert!(!marker.is_resolved());
        assert!(newer.retry().is_err());
        assert!(
            marker.is_resolved(),
            "the exact original refusal marker must not wait for an unrelated later leaf"
        );
        assert!(!newer.is_resolved());
        assert_eq!(budget.used.load(Ordering::Acquire), 1);
        newer.retry().unwrap();
        assert!(newer.is_resolved());
        assert!(marker.is_resolved());
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
    }

    #[test]
    fn cleanup_context_capacity_stays_charged_until_guard_destructor_finishes() {
        struct Guard {
            started: std::sync::mpsc::Sender<()>,
            release: Arc<std::sync::Barrier>,
        }
        impl Drop for Guard {
            fn drop(&mut self) {
                self.started.send(()).unwrap();
                self.release.wait();
            }
        }
        let budget = Arc::new(Mutex::new(ContextUsage::default()));
        let (started, signal) = std::sync::mpsc::channel();
        let release = Arc::new(std::sync::Barrier::new(2));
        let context = RecoveryContext::from_budget(
            Guard {
                started,
                release: release.clone(),
            },
            128,
            budget.clone(),
        )
        .unwrap();
        let cleanup = std::thread::spawn(move || drop(context));
        signal
            .recv_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        assert!(RecoveryContext::from_budget((), 1, budget.clone()).is_err());
        assert_eq!(budget.lock().unwrap().descriptors, 128);
        release.wait();
        cleanup.join().unwrap();
        let replacement = RecoveryContext::from_budget((), 128, budget.clone()).unwrap();
        drop(replacement);
        assert_eq!(budget.lock().unwrap().descriptors, 0);
    }

    #[test]
    fn cleanup_context_is_shared_bounded_and_survives_parent_drop_until_resolution() {
        struct Guard(Arc<AtomicUsize>);
        impl Drop for Guard {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::AcqRel);
            }
        }
        let budget = Arc::new(Mutex::new(ContextUsage::default()));
        let drops = Arc::new(AtomicUsize::new(0));
        let context =
            RecoveryContext::from_budget(Guard(drops.clone()), 3, budget.clone()).unwrap();
        let a = context.reserve().unwrap().retain("a".into(), || Ok(()));
        let b = context.reserve().unwrap().retain("b".into(), || Ok(()));
        drop(context);
        assert_eq!(budget.lock().unwrap().descriptors, 3);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        a.retry().unwrap();
        assert_eq!(drops.load(Ordering::Acquire), 0);
        b.retry().unwrap();
        assert_eq!(drops.load(Ordering::Acquire), 1);
        assert_eq!(budget.lock().unwrap().descriptors, 0);
        assert!(RecoveryContext::from_budget((), 129, budget).is_err());
    }

    #[test]
    fn editor_recovery_last_owner_drop_releases_budget_without_running_effects() {
        let budget = Arc::new(Budget {
            used: AtomicUsize::new(0),
            limit: 1,
        });
        let count = Arc::new(AtomicUsize::new(0));
        let captured = Arc::clone(&count);
        let authority = RecoverySlot::from_budget(Arc::clone(&budget))
            .unwrap()
            .retain("retained".into(), move || {
                captured.fetch_add(1, Ordering::AcqRel);
                Ok(())
            });
        let clone = authority.clone();
        drop(authority);
        assert_eq!(budget.used.load(Ordering::Acquire), 1);
        drop(clone);
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        assert_eq!(count.load(Ordering::Acquire), 0);
    }
}
