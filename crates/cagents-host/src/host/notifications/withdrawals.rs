//! Current journal groups outrank best-effort cleanup of obsolete desktop IDs.
use std::collections::BTreeSet;
use uuid::Uuid;

#[derive(Clone, Copy)]
pub(super) enum Withdrawal {
    Group(Uuid),
    Legacy(Uuid),
}
impl Withdrawal {
    pub fn id(self) -> Uuid {
        match self {
            Self::Group(id) | Self::Legacy(id) => id,
        }
    }
}

#[derive(Default)]
pub(super) struct Withdrawals {
    groups: BTreeSet<Uuid>,
    legacy: BTreeSet<Uuid>,
    retained: BTreeSet<Uuid>,
}
impl Withdrawals {
    pub fn protect(&mut self, groups: impl IntoIterator<Item = Uuid>) {
        self.retained = groups.into_iter().collect();
        debug_assert!(self.retained.len() <= super::MAX_PENDING);
    }
    pub fn group(&mut self, id: Uuid) {
        if self.groups.contains(&id) {
            return;
        }
        if self.groups.len() == super::MAX_PENDING {
            // There are at most MAX_PENDING retained journal targets. Even
            // when obsolete group removals fill the queue, every retained
            // group can reserve its own removal; legacy IDs cannot evict it.
            let expendable = self
                .groups
                .iter()
                .find(|old| !self.retained.contains(*old))
                .copied();
            let Some(expendable) = expendable else {
                return;
            };
            self.groups.remove(&expendable);
        }
        self.groups.insert(id);
        debug_assert!(self.groups.len() + self.legacy.len() <= super::MAX_WITHDRAWALS);
    }
    pub fn legacy(&mut self, id: Uuid) {
        if self.legacy.len() == super::MAX_PENDING && !self.legacy.contains(&id) {
            self.legacy.pop_first();
        }
        self.legacy.insert(id);
        debug_assert!(self.groups.len() + self.legacy.len() <= super::MAX_WITHDRAWALS);
    }
    pub fn contains(&self, group: &Uuid) -> bool {
        self.groups.contains(group)
    }
    pub fn is_empty(&self) -> bool {
        self.groups.is_empty() && self.legacy.is_empty()
    }
    pub fn pop_first(&mut self) -> Option<Withdrawal> {
        self.groups
            .pop_first()
            .map(Withdrawal::Group)
            .or_else(|| self.legacy.pop_first().map(Withdrawal::Legacy))
    }
    pub fn requeue(&mut self, entry: Withdrawal) {
        match entry {
            Withdrawal::Group(id) => self.group(id),
            Withdrawal::Legacy(id) => self.legacy(id),
        }
    }
    #[cfg(test)]
    pub fn len(&self) -> usize {
        self.groups.len() + self.legacy.len()
    }
    #[cfg(test)]
    pub fn remove_group(&mut self, id: &Uuid) {
        self.groups.remove(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn privacy_retraction_reserves_every_retained_group_despite_a_full_legacy_backlog() {
        let mut queue = Withdrawals::default();
        for index in 0..super::super::MAX_PENDING {
            queue.group(Uuid::from_u128(index as u128));
        }
        for index in 0..super::super::MAX_PENDING * 4 {
            queue.legacy(Uuid::from_u128(index as u128));
        }
        let retained = (0..super::super::MAX_PENDING)
            .map(|index| Uuid::from_u128((index + 100_000) as u128))
            .collect::<BTreeSet<_>>();
        queue.protect(retained.iter().copied());
        for id in &retained {
            queue.group(*id);
            queue.legacy(*id);
        }
        for index in 0..super::super::MAX_PENDING * 4 {
            queue.legacy(Uuid::from_u128((index + 200_000) as u128));
            queue.group(Uuid::from_u128((index + 300_000) as u128));
        }
        assert_eq!(queue.len(), super::super::MAX_WITHDRAWALS);
        for id in &retained {
            assert!(
                queue.contains(id),
                "retained privacy retraction was evicted"
            );
        }
        let failed = queue.pop_first().unwrap();
        let failed_id = failed.id();
        queue.requeue(failed);
        assert!(
            queue.contains(&failed_id),
            "failed portal removal retains its stable group priority"
        );
        for _ in 0..retained.len() {
            assert!(
                matches!(queue.pop_first(), Some(Withdrawal::Group(id)) if retained.contains(&id)),
                "privacy/group cleanup must precede obsolete legacy cleanup"
            );
        }
        assert!(matches!(queue.pop_first(), Some(Withdrawal::Legacy(_))));
    }
}
