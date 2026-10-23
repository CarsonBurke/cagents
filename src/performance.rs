//! Requested state commit -> matching view's first RedrawRequested.
//! This is redraw scheduling, NOT submission, presentation, or scanout.

#[cfg(feature = "ui")]
pub mod chat_stage;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Generation(u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    CardFocus,
    PaletteOpen,
    PaletteQuery,
    /// One terminal viewport scroll command, from the request to the frame the
    /// worker publishes for it.
    ///
    /// Committed by the worker-side scroll workload, which is where the
    /// pipeline this names actually lives: the UI's own wheel handling defers
    /// and coalesces into `RedrawRequested`, so it is measured by the redraw
    /// it schedules rather than by an action of its own.
    TerminalScroll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LatencySummary {
    pub samples: usize,
    pub median: Duration,
    pub p95: Duration,
    pub p99: Duration,
    pub worst: Duration,
}

impl LatencySummary {
    /// Summarizes `durations` in place, which is why it takes them mutably:
    /// percentiles need the sorted order and a workload has no use for the
    /// original arrival order once it is reporting.
    pub fn from_durations(durations: &mut [Duration]) -> Option<Self> {
        if durations.is_empty() {
            return None;
        }
        durations.sort_unstable();
        // Nearest-rank on the inclusive index range, matching the percentile
        // the latency fixture and `scripts/run-redraw-latency` already report
        // so numbers from the two harnesses can be compared directly.
        let percentile =
            |percent: usize| durations[((durations.len() - 1) * percent).div_ceil(100)];
        Some(Self {
            samples: durations.len(),
            median: percentile(50),
            p95: percentile(95),
            p99: percentile(99),
            worst: durations[durations.len() - 1],
        })
    }
}

/// Compile-time recorder for ordinary builds. No state, clock, storage, or IDs.
#[derive(Debug, Default)]
pub struct NoopRecorder;

impl NoopRecorder {
    #[inline(always)]
    pub fn commit(&mut self, action: Action) -> Option<Generation> {
        self.commit_with_clock(action, Instant::now)
    }

    #[inline(always)]
    pub fn commit_with_clock(
        &mut self,
        _action: Action,
        _clock: impl FnOnce() -> Instant,
    ) -> Option<Generation> {
        None
    }

    #[inline(always)]
    pub fn pending_generation(&self) -> Option<Generation> {
        None
    }

    #[inline(always)]
    pub fn redraw_scheduled(&mut self, _generation: Generation, _at: Instant) -> bool {
        false
    }

    #[inline(always)]
    pub fn abandon(&mut self, _generation: Generation) -> bool {
        false
    }

    #[inline(always)]
    pub fn abandon_pending(&mut self) {}
}

#[cfg(not(feature = "latency-harness"))]
pub type InteractionLatency = NoopRecorder;
#[cfg(feature = "latency-harness")]
pub use recording::Recorder as InteractionLatency;
// Workloads that are not the UI fixture - the terminal scroll workload in
// `terminal::worker` - drive the recorder directly from a test build.
#[cfg(any(feature = "latency-harness", test))]
pub use recording::{Outcome, Recorder, Sample};

#[cfg(any(feature = "latency-harness", test))]
mod recording {
    use super::*;
    use std::collections::VecDeque;

    const CAPACITY: usize = 4096;

    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
    #[serde(rename_all = "snake_case")]
    pub enum Outcome {
        RedrawScheduled,
        Abandoned,
    }

    /// Strict allowlist: only process-local sequence, action, settlement and duration.
    #[derive(Debug, Clone, Copy, serde::Serialize)]
    pub struct Sample {
        pub sequence: u64,
        pub action: Action,
        pub outcome: Outcome,
        pub redraw_scheduling_ns: Option<u64>,
    }

    #[derive(Debug)]
    pub struct Recorder {
        next: u64,
        pending: Option<(Generation, Action, Instant)>,
        samples: VecDeque<Sample>,
        settled: u64,
    }

    impl Default for Recorder {
        fn default() -> Self {
            Self {
                next: 0,
                pending: None,
                samples: VecDeque::with_capacity(CAPACITY),
                settled: 0,
            }
        }
    }

    impl Recorder {
        pub fn commit(&mut self, action: Action) -> Option<Generation> {
            self.commit_with_clock(action, Instant::now)
        }

        pub fn commit_with_clock(
            &mut self,
            action: Action,
            clock: impl FnOnce() -> Instant,
        ) -> Option<Generation> {
            self.abandon_pending();
            self.next = self.next.checked_add(1)?;
            let generation = Generation(self.next);
            self.pending = Some((generation, action, clock()));
            Some(generation)
        }

        pub fn pending_generation(&self) -> Option<Generation> {
            self.pending.map(|pending| pending.0)
        }

        /// `generation` must come from the widget tree receiving this redraw,
        /// never from the current mutable model or a global frame subscription.
        pub fn redraw_scheduled(&mut self, generation: Generation, at: Instant) -> bool {
            let Some((pending, action, started)) = self.pending else {
                return false;
            };
            if pending != generation {
                return false;
            }
            let Some(elapsed) = at.checked_duration_since(started) else {
                return false;
            };
            self.pending = None;
            self.push(Sample {
                sequence: generation.0,
                action,
                outcome: Outcome::RedrawScheduled,
                redraw_scheduling_ns: Some(elapsed.as_nanos().min(u64::MAX as u128) as u64),
            });
            true
        }

        pub fn abandon(&mut self, generation: Generation) -> bool {
            let Some((pending, action, _)) = self.pending else {
                return false;
            };
            if pending != generation {
                return false;
            }
            self.pending = None;
            self.push(Sample {
                sequence: generation.0,
                action,
                outcome: Outcome::Abandoned,
                redraw_scheduling_ns: None,
            });
            true
        }

        pub fn abandon_pending(&mut self) {
            if let Some(generation) = self.pending_generation() {
                self.abandon(generation);
            }
        }

        fn push(&mut self, sample: Sample) {
            if self.samples.len() == CAPACITY {
                self.samples.pop_front();
            }
            self.samples.push_back(sample);
            self.settled += 1;
        }

        pub fn samples(&self) -> impl Iterator<Item = &Sample> {
            self.samples.iter()
        }
        pub fn settled_count(&self) -> u64 {
            self.settled
        }
    }

    impl Drop for Recorder {
        fn drop(&mut self) {
            self.abandon_pending();
        }
    }
}

/// Provisional, NON-GATING until a named reference-machine baseline exists.
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
    use recording::{Outcome, Recorder};

    #[test]
    fn stale_duplicate_and_precommit_redraws_cannot_settle_a_generation() {
        let origin = Instant::now();
        let mut recorder = Recorder::default();
        let stale = recorder
            .commit_with_clock(Action::CardFocus, || origin)
            .unwrap();
        let current = recorder
            .commit_with_clock(Action::PaletteOpen, || origin + Duration::from_millis(1))
            .unwrap();
        assert!(!recorder.redraw_scheduled(stale, origin + Duration::from_millis(8)));
        assert!(!recorder.redraw_scheduled(current, origin));
        assert!(recorder.redraw_scheduled(current, origin + Duration::from_millis(8)));
        assert!(!recorder.redraw_scheduled(current, origin + Duration::from_millis(9)));
        assert!(!recorder.abandon(current));
        let samples: Vec<_> = recorder.samples().collect();
        assert_eq!(samples[0].outcome, Outcome::Abandoned);
        assert_eq!(samples[1].action, Action::PaletteOpen);
        assert_eq!(samples[1].redraw_scheduling_ns, Some(7_000_000));
        assert_eq!(recorder.settled_count(), 2);
        assert_eq!(recorder.pending_generation(), None);
    }

    #[test]
    fn abandonment_and_bounded_history_preserve_exact_settlement_counts() {
        let origin = Instant::now();
        let mut recorder = Recorder::default();
        for _ in 0..5000 {
            let generation = recorder
                .commit_with_clock(Action::PaletteQuery, || origin)
                .unwrap();
            assert!(recorder.abandon(generation));
            assert!(!recorder.abandon(generation));
        }
        assert_eq!(recorder.settled_count(), 5000);
        assert_eq!(recorder.samples().count(), 4096);
        assert_eq!(recorder.samples().next().unwrap().sequence, 905);
    }

    #[test]
    fn disabled_recorder_never_calls_clock() {
        let mut recorder = NoopRecorder;
        assert_eq!(
            recorder
                .commit_with_clock(Action::CardFocus, || panic!("disabled recorder read clock")),
            None
        );
        assert_eq!(std::mem::size_of::<NoopRecorder>(), 0);
        assert_eq!(recorder.pending_generation(), None);
    }
}
