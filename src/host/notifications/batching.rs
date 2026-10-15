//! One trailing-edge delivery per conversation, with a bounded maximum delay.
use crate::notifications::{Kind, Target};
use std::{collections::BTreeMap, time::{Duration, Instant}};

#[derive(Default)]
pub(super) struct Schedule(BTreeMap<Target, (Instant, Instant)>);

impl Schedule {
    pub fn update(&mut self, target: Target, kind: Kind, now: Instant) {
        let delay = if matches!(kind, Kind::Waiting | Kind::Failed | Kind::Interrupted) {
            Duration::from_millis(200)
        } else { Duration::from_millis(600) };
        let first = self.0.get(&target).map_or(now, |(first, _)| *first);
        self.0.insert(target, (first, (now + delay).min(first + Duration::from_secs(2))));
    }
    pub fn ready(&self, target: &Target, now: Instant) -> bool {
        self.0.get(target).is_none_or(|(_, due)| *due <= now)
    }
    pub fn deadline(&self) -> Option<Instant> {
        self.0.values().map(|(_, due)| *due).min()
    }
    pub fn retain(&mut self, mut keep: impl FnMut(&Target) -> bool) {
        self.0.retain(|target, _| keep(target));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn bursts_replace_same_target_without_delaying_unrelated_attention() {
        let now = Instant::now();
        let a = Target::Session(crate::model::TerminalId::new());
        let b = Target::Session(crate::model::TerminalId::new());
        let mut schedule = Schedule::default();
        for index in 0..10 { schedule.update(a.clone(), Kind::Succeeded, now + Duration::from_millis(index * 100)); }
        schedule.update(b.clone(), Kind::Waiting, now + Duration::from_millis(900));
        assert!(!schedule.ready(&a, now + Duration::from_millis(1100)));
        assert!(schedule.ready(&b, now + Duration::from_millis(1100)));
        assert!(schedule.ready(&a, now + Duration::from_millis(1500)));
        for index in 10..40 { schedule.update(a.clone(), Kind::Succeeded, now + Duration::from_millis(index * 100)); }
        assert!(schedule.ready(&a, now + Duration::from_secs(2)), "continuous updates cannot starve delivery");
    }
}
