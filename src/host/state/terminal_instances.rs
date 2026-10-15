//! Last published worker generation survives resident retirement. An absence
//! never authenticates an older worker; bounded eviction loses authority.
use crate::model::TerminalId;
use std::collections::{BTreeMap, VecDeque};
use uuid::Uuid;
const CAPACITY: usize = 8192;
#[derive(Default)]
pub(super) struct PublishedWorkers {
    latest: BTreeMap<TerminalId, Uuid>,
    order: VecDeque<(TerminalId, Uuid)>,
}
impl PublishedWorkers {
    pub(super) fn observe(&mut self, id: TerminalId, instance: Uuid) {
        if instance.is_nil() || self.latest.get(&id) == Some(&instance) {
            return;
        }
        self.latest.insert(id, instance);
        self.order.push_back((id, instance));
        while self.order.len() > CAPACITY {
            let (old, instance) = self.order.pop_front().unwrap();
            if self.latest.get(&old) == Some(&instance) {
                self.latest.remove(&old);
            }
        }
    }
    pub(super) fn latest(&self, id: TerminalId) -> Option<Uuid> {
        self.latest.get(&id).copied()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn last_published_worker_survives_retirement_and_eviction_removes_authority() {
        let mut workers = PublishedWorkers::default();
        let id = TerminalId::new();
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        assert_eq!(workers.latest(id), None);
        workers.observe(id, Uuid::nil());
        assert_eq!(workers.latest(id), None);
        workers.observe(id, a);
        workers.observe(id, a);
        assert_eq!(workers.order.len(), 1);
        workers.observe(id, b);
        assert_eq!(workers.latest(id), Some(b));
        for _ in 0..CAPACITY {
            workers.observe(TerminalId::new(), Uuid::new_v4());
        }
        assert!(workers.order.len() <= CAPACITY);
        assert!(workers.latest.len() <= CAPACITY);
        assert_eq!(workers.latest(id), None);
    }
}
