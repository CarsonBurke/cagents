//! Bounded live authority for filesystem recovery across isolated build lanes. Descriptions are diagnostic
//! spellings; only the producer's retained descriptors authorize retry.
use std::sync::{Arc, Mutex, OnceLock, atomic::{AtomicBool, AtomicUsize, Ordering}};

// Each admitted producer may retain at most sixteen descriptors. Reserving
// before any effect bounds unresolved work without a global authority registry.
const MAX_RECOVERIES: usize = 32;
pub(crate) const MAX_RECOVERY_DESCRIPTORS: usize = 16;
struct Budget { used: AtomicUsize, limit: usize }
fn budget() -> Arc<Budget> {
    static BUDGET: OnceLock<Arc<Budget>> = OnceLock::new();
    Arc::clone(BUDGET.get_or_init(|| Arc::new(Budget { used: AtomicUsize::new(0), limit: MAX_RECOVERIES })))
}

/// Reserve one bounded recovery payload before touching the filesystem.
/// Producers must retain no more than MAX_RECOVERY_DESCRIPTORS descriptors.
pub(crate) struct RecoverySlot { budget: Arc<Budget> }
impl RecoverySlot {
    pub(crate) fn reserve() -> Result<Self, String> { Self::from_budget(budget()) }
    fn from_budget(budget: Arc<Budget>) -> Result<Self, String> {
        budget.used.fetch_update(Ordering::AcqRel, Ordering::Acquire,
            |used| (used < budget.limit).then_some(used + 1))
            .map_err(|_| "Recovery capacity is exhausted. Resolve an earlier recovery before changing files.".to_owned())?;
        Ok(Self { budget })
    }
    pub(crate) fn retain(self, description: String, retry: impl FnMut() -> Result<(), String> + Send + 'static) -> RecoveryAuthority {
        RecoveryAuthority(Arc::new(RecoveryInner {
            description: description.chars().take(2048).collect(), resolved: AtomicBool::new(false),
            work: Mutex::new(RecoveryWork { retry: Some(Box::new(retry)), slot: Some(self) }),
        }))
    }
}
impl Drop for RecoverySlot { fn drop(&mut self) { self.budget.used.fetch_sub(1, Ordering::AcqRel); } }

struct RecoveryWork {
    retry: Option<Box<dyn FnMut() -> Result<(), String> + Send>>,
    slot: Option<RecoverySlot>,
}
struct RecoveryInner { description: String, resolved: AtomicBool, work: Mutex<RecoveryWork> }

/// A producer-owned recovery operation, retained by its exact UI error/result.
/// Clones share one payload and one budget slot; there is no Drop filesystem IO.
#[derive(Clone)]
pub struct RecoveryAuthority(Arc<RecoveryInner>);
impl RecoveryAuthority {
    /// Join existing bounded payloads without reserving a new slot after an
    /// effect. Nested cleanup errors must not discard an earlier authority.
    pub(crate) fn combine(self, other: Self) -> Self {
        if self == other { return self; }
        let description = format!("{}; {}", self.description(), other.description()).chars().take(2048).collect();
        Self(Arc::new(RecoveryInner { description, resolved: AtomicBool::new(false), work: Mutex::new(RecoveryWork {
            retry: Some(Box::new(move || { self.retry()?; other.retry() })), slot: None,
        }) }))
    }
    pub fn description(&self) -> &str { &self.0.description }
    pub fn is_resolved(&self) -> bool { self.0.resolved.load(Ordering::Acquire) }
    /// Blocking worker only. The producer revalidates retained authority and
    /// must refuse foreign names/no-clobber collisions on every retry.
    pub(crate) fn retry(&self) -> Result<(), String> {
        let mut work = self.0.work.lock().map_err(|_| "Editor recovery previously failed unexpectedly; its retained authority was kept.".to_owned())?;
        if let Some(retry) = work.retry.as_mut() { retry()?; }
        work.retry = None;
        work.slot = None;
        self.0.resolved.store(true, Ordering::Release);
        Ok(())
    }
}
impl std::fmt::Debug for RecoveryAuthority {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RecoveryAuthority").field("description", &self.description()).field("resolved", &self.is_resolved()).finish()
    }
}
impl PartialEq for RecoveryAuthority { fn eq(&self, other: &Self) -> bool { Arc::ptr_eq(&self.0, &other.0) } }
impl Eq for RecoveryAuthority {}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn editor_recovery_budget_and_clones_keep_one_payload_until_actual_resolution() {
        let budget = Arc::new(Budget { used: AtomicUsize::new(0), limit: 1 });
        let slot = RecoverySlot::from_budget(Arc::clone(&budget)).unwrap();
        assert!(RecoverySlot::from_budget(Arc::clone(&budget)).is_err());
        let attempts = Arc::new(AtomicUsize::new(0)); let count = Arc::clone(&attempts);
        let authority = slot.retain("diagnostic spelling only".into(), move || {
            if count.fetch_add(1, Ordering::AcqRel) == 0 { Err("foreign destination was preserved".into()) } else { Ok(()) }
        });
        let clone = authority.clone(); assert_eq!(authority, clone);
        assert!(authority.retry().is_err()); assert!(!clone.is_resolved());
        assert!(RecoverySlot::from_budget(Arc::clone(&budget)).is_err());
        clone.retry().unwrap(); assert!(authority.is_resolved());
        assert_eq!(budget.used.load(Ordering::Acquire), 0);
        authority.retry().unwrap(); assert_eq!(attempts.load(Ordering::Acquire), 2);
    }
    #[test]
    fn editor_recovery_last_owner_drop_releases_budget_without_running_effects() {
        let budget = Arc::new(Budget { used: AtomicUsize::new(0), limit: 1 });
        let count = Arc::new(AtomicUsize::new(0)); let captured = Arc::clone(&count);
        let authority = RecoverySlot::from_budget(Arc::clone(&budget)).unwrap().retain("retained".into(), move || { captured.fetch_add(1, Ordering::AcqRel); Ok(()) });
        let clone = authority.clone(); drop(authority); assert_eq!(budget.used.load(Ordering::Acquire), 1);
        drop(clone); assert_eq!(budget.used.load(Ordering::Acquire), 0); assert_eq!(count.load(Ordering::Acquire), 0);
    }
}
