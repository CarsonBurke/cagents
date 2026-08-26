use std::{
    collections::{HashMap, VecDeque},
    time::{Duration, Instant},
};

use uuid::Uuid;

const DEFAULT_SAMPLE_CAPACITY: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InteractionId(Uuid);

impl InteractionId {
    pub fn new() -> Self {
        Self(Uuid::new_v4())
    }
}

impl Default for InteractionId {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencySummary {
    pub samples: usize,
    pub median: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub worst: Duration,
}

#[derive(Debug)]
pub struct InteractionLatency {
    pending: HashMap<InteractionId, Instant>,
    samples: VecDeque<Duration>,
    capacity: usize,
}

impl Default for InteractionLatency {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_SAMPLE_CAPACITY)
    }
}

impl InteractionLatency {
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "latency sample capacity must be non-zero");
        Self {
            pending: HashMap::new(),
            samples: VecDeque::with_capacity(capacity),
            capacity,
        }
    }

    pub fn begin(&mut self) -> InteractionId {
        self.begin_at(Instant::now())
    }

    pub fn begin_at(&mut self, started: Instant) -> InteractionId {
        let id = InteractionId::new();
        self.pending.insert(id, started);
        id
    }

    pub fn presented(&mut self, interaction: InteractionId) -> Option<Duration> {
        self.presented_at(interaction, Instant::now())
    }

    pub fn presented_at(
        &mut self,
        interaction: InteractionId,
        presented: Instant,
    ) -> Option<Duration> {
        let started = self.pending.remove(&interaction)?;
        let latency = presented.saturating_duration_since(started);
        if self.samples.len() == self.capacity {
            self.samples.pop_front();
        }
        self.samples.push_back(latency);
        Some(latency)
    }

    pub fn abandon(&mut self, interaction: InteractionId) -> bool {
        self.pending.remove(&interaction).is_some()
    }

    pub fn summary(&self) -> Option<LatencySummary> {
        if self.samples.is_empty() {
            return None;
        }

        let mut sorted: Vec<_> = self.samples.iter().copied().collect();
        sorted.sort_unstable();
        let percentile = |numerator: usize| {
            let index = ((sorted.len() - 1) * numerator).div_ceil(100);
            sorted[index.min(sorted.len() - 1)]
        };

        Some(LatencySummary {
            samples: sorted.len(),
            median: percentile(50),
            p95: percentile(95),
            p99: percentile(99),
            worst: *sorted.last().expect("samples checked non-empty"),
        })
    }

    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PerformanceBudget {
    pub interaction_p95: Duration,
    pub interaction_p99: Duration,
    pub max_visible_cards: usize,
    pub max_prefetch_cards: usize,
}

impl Default for PerformanceBudget {
    fn default() -> Self {
        Self {
            interaction_p95: Duration::from_millis(50),
            interaction_p99: Duration::from_millis(100),
            max_visible_cards: 4,
            max_prefetch_cards: 2,
        }
    }
}

impl PerformanceBudget {
    pub fn accepts(self, summary: LatencySummary) -> bool {
        summary.p95 <= self.interaction_p95 && summary.p99 <= self.interaction_p99
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_only_completed_interactions() {
        let origin = Instant::now();
        let mut latency = InteractionLatency::with_capacity(2);
        let first = latency.begin_at(origin);
        let abandoned = latency.begin_at(origin);

        assert_eq!(
            latency.presented_at(first, origin + Duration::from_millis(7)),
            Some(Duration::from_millis(7))
        );
        assert!(latency.abandon(abandoned));
        assert_eq!(latency.summary().unwrap().samples, 1);
        assert_eq!(latency.pending_count(), 0);
    }

    #[test]
    fn capacity_keeps_recent_samples() {
        let origin = Instant::now();
        let mut latency = InteractionLatency::with_capacity(2);
        for millis in [100, 20, 30] {
            let interaction = latency.begin_at(origin);
            latency.presented_at(interaction, origin + Duration::from_millis(millis));
        }

        let summary = latency.summary().unwrap();
        assert_eq!(summary.samples, 2);
        assert_eq!(summary.worst, Duration::from_millis(30));
    }
}
