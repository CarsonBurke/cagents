//! Process-local serialization for the COMPLETE semantic delivery operation.
//! It is neither helper-retirement proof nor a cross-daemon journal lock.
use std::{collections::BTreeMap, sync::{Arc, Mutex, OnceLock, Weak}};

const MAX_LANES: usize = 128;
type Lane = Mutex<()>;
#[derive(Default)]
struct Lanes(BTreeMap<uuid::Uuid, Weak<Lane>>);
impl Lanes {
    fn acquire(&mut self, instance: uuid::Uuid) -> Result<Arc<Lane>, String> {
        if instance.is_nil() {
            return Err("The retained semantic actor identity is missing".into());
        }
        if let Some(lane) = self.0.get(&instance).and_then(Weak::upgrade) {
            return Ok(lane);
        }
        // A caller keeps its Arc until AFTER its guard drops. Thus an expired
        // weak entry can never be a still-held operation that we replace.
        self.0.retain(|_, lane| lane.strong_count() != 0);
        if self.0.len() >= MAX_LANES {
            return Err("Retained semantic delivery capacity reached; its journal remains pending".into());
        }
        let lane = Arc::new(Mutex::new(()));
        self.0.insert(instance, Arc::downgrade(&lane));
        Ok(lane)
    }
}

pub(super) fn lane(instance: uuid::Uuid) -> Result<Arc<Lane>, String> {
    static LANES: OnceLock<Mutex<Lanes>> = OnceLock::new();
    // Selecting a lane never waits behind another selector or holds HostState.
    LANES.get_or_init(|| Mutex::new(Lanes::default())).try_lock()
        .map_err(|_| "Retained semantic delivery admission is busy or unavailable".to_string())?
        .acquire(instance)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn same_actor_contends_across_callers_but_another_actor_can_settle() {
        let mut registry = Lanes::default();
        let actor = uuid::Uuid::new_v4();
        let background = registry.acquire(actor).unwrap();
        let first_page = background.try_lock().unwrap();
        let closing = registry.acquire(actor).unwrap();
        assert!(Arc::ptr_eq(&background, &closing));
        assert!(closing.try_lock().is_err());
        let other = registry.acquire(uuid::Uuid::new_v4()).unwrap();
        assert!(other.try_lock().is_ok());
        // Simulate all pages plus notification commit and helper ACK before
        // releasing: obtaining another Arc never substitutes a new gate.
        for _ in 0..65 {
            assert!(registry.acquire(actor).unwrap().try_lock().is_err());
        }
        drop(first_page);
        assert!(closing.try_lock().is_ok());
    }
    #[test]
    fn capacity_refuses_without_replacing_a_live_lane_and_reclaims_only_expired_entries() {
        let mut registry = Lanes::default();
        let mut owners = Vec::new();
        for _ in 0..MAX_LANES {
            owners.push(registry.acquire(uuid::Uuid::new_v4()).unwrap());
        }
        let (actor, original) = registry.0.iter().next().map(|(id, lane)| (*id, lane.upgrade().unwrap())).unwrap();
        assert!(Arc::ptr_eq(&registry.acquire(actor).unwrap(), &original));
        assert!(registry.acquire(uuid::Uuid::new_v4()).is_err());
        drop(owners);
        let next = registry.acquire(uuid::Uuid::new_v4()).unwrap();
        assert_eq!(registry.0.len(), 2);
        assert!(!Arc::ptr_eq(&next, &original));
        assert!(registry.acquire(uuid::Uuid::nil()).is_err());
    }
}
