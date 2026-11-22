//! What a conversation's provider requests say about its prompt cache.
//!
//! Providers cache a conversation's prompt for a while after each request
//! that reads or writes it starts. A request after the cache expired reads
//! the whole context again at full price. The adapters report two kinds of
//! evidence from live provider activity only, never from replayed history:
//! that a model request for the main conversation started, how much of a
//! request's prompt the cache served, and, where the provider says, how long
//! the cache it wrote lasts. The runtime keeps the latest request time and
//! measures, across each long idle gap, whether the cache survived it; the
//! daemon turns that into an estimate of when the cache expires.
use serde::{Deserialize, Serialize};

/// Live evidence an adapter reports about the main conversation's cache.
/// Subagents have caches of their own and report none.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Evidence {
    /// A model request for the conversation started about now, and the next
    /// reading reports what the cache served it.
    Request,
    /// A request continuing the turn started about now, unless the turn was
    /// stopped first. It refreshes the cache once the provider reports on
    /// it, but the provider may report its reading out of order, so it
    /// measures nothing.
    Continued,
    /// A request's prompt was `prompt` input tokens, of which the cache
    /// served `cached`.
    Read { prompt: u64, cached: u64 },
    /// The request wrote the prompt to a cache the provider keeps for
    /// `ttl_ms` after its latest use, as Claude reports with each request.
    Written { ttl_ms: u64 },
}

/// Whether the cache survived an idle gap: the first request after a gap of
/// `gap_ms` found `hit` its prompt cached.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Observation {
    /// Unix milliseconds of the request that measured it; it identifies the
    /// observation within its conversation.
    pub at: u64,
    pub gap_ms: u64,
    pub hit: bool,
}

/// What a conversation's runtime knows about its cache, as the helper that
/// hosts a retained conversation reports it to the daemon.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Activity {
    /// Unix milliseconds at which the latest model request for the
    /// conversation started, as the runtime observed it. A request that
    /// failed before the provider reported anything of it doesn't count, as
    /// it may not have reached the cache.
    pub requested_at: Option<u64>,
    /// The latest observations, oldest first, at most [`Tracker::KEPT`].
    pub observations: Vec<Observation>,
    /// How long the provider said it keeps the conversation's latest cache
    /// write, where it says.
    pub ttl_ms: Option<u64>,
}

/// Shorter gaps say little about how long a cache lasts, and every TTL a
/// provider documents is longer.
#[cfg(feature = "host")]
const MIN_GAP_MS: u64 = 2 * 60 * 1000;

/// A prompt shorter than this may be below the provider's minimum cacheable
/// length, so a miss proves nothing.
#[cfg(feature = "host")]
const MIN_PROMPT_TOKENS: u64 = 4096;

/// Folds evidence into [`Activity`] for one conversation.
#[cfg(feature = "host")]
#[derive(Debug, Default)]
pub(crate) struct Tracker {
    activity: Activity,
    /// When the latest request started, until the provider reports
    /// something of it.
    pending: Option<u64>,
    /// When a continuation started while a reading was still owed for an
    /// earlier request; it is pending once that reading arrives. Tools that
    /// finish together continue in one request, so only the latest is kept.
    continuation: Option<u64>,
    /// The gap before the latest request, until a reading says whether the
    /// cache survived it.
    measuring: Option<(u64, u64)>,
    /// The latest reading's prompt: what a request after a gap finds cached
    /// if the conversation's cache survived it.
    prompt: Option<u64>,
}

#[cfg(feature = "host")]
impl Tracker {
    /// How many observations a conversation keeps for the daemon to collect.
    pub(crate) const KEPT: usize = 16;

    /// Records `evidence` observed at `now`, in Unix milliseconds, and says
    /// whether the activity changed.
    pub(crate) fn observe(&mut self, evidence: Evidence, now: u64) -> bool {
        match evidence {
            Evidence::Request => {
                // A request that failed unreported, such as one a usage
                // limit refused, neither refreshed the cache nor measured it.
                self.measuring = self
                    .activity
                    .requested_at
                    .map(|previous| now.saturating_sub(previous))
                    .filter(|gap| *gap >= MIN_GAP_MS)
                    .map(|gap| (now, gap));
                self.pending = Some(now);
                self.continuation = None;
                false
            }
            // Codex reports a request's usage only after its tools finish, so
            // a continuation can start before the request it continues is
            // reported.
            Evidence::Continued if self.pending.is_some() => {
                self.continuation = Some(now);
                false
            }
            Evidence::Continued => {
                self.pending = Some(now);
                false
            }
            Evidence::Written { ttl_ms } => {
                let confirmed = self.confirm();
                let changed = self.activity.ttl_ms.replace(ttl_ms) != Some(ttl_ms);
                confirmed || changed
            }
            Evidence::Read { prompt, cached } => {
                let confirmed = self.confirm();
                let before = self.prompt.replace(prompt);
                let Some((at, gap_ms)) = self.measuring.take() else {
                    return confirmed;
                };
                // A conversation that shrank, as compaction does, starts a
                // new cache; one too small may not have been cached at all.
                let Some(before) =
                    before.filter(|before| *before >= MIN_PROMPT_TOKENS && *before <= prompt)
                else {
                    return confirmed;
                };
                let observations = &mut self.activity.observations;
                if observations.len() == Self::KEPT {
                    observations.remove(0);
                }
                observations.push(Observation {
                    at,
                    gap_ms,
                    // A hit serves the conversation's whole earlier prompt.
                    // An expired one can still serve a prefix it shares with
                    // other conversations, such as the system prompt.
                    hit: cached.saturating_mul(10) >= before.saturating_mul(9),
                });
                true
            }
        }
    }

    pub(crate) fn activity(&self) -> Activity {
        self.activity.clone()
    }

    /// The conversation moved to another model, which keeps a cache of its
    /// own, so the next request measures nothing about this one's.
    pub(crate) fn forget_prompt(&mut self) {
        self.prompt = None;
        self.measuring = None;
    }

    /// The provider reported on the pending request, so it reached the
    /// cache. Says whether the activity changed.
    fn confirm(&mut self) -> bool {
        let Some(at) = self.pending.take() else {
            return false;
        };
        self.activity.requested_at = Some(at);
        self.pending = self.continuation.take();
        true
    }
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;

    const MINUTE: u64 = 60 * 1000;

    #[test]
    fn a_read_after_a_long_gap_records_whether_the_cache_survived() {
        let mut tracker = Tracker::default();
        tracker.observe(Evidence::Request, 0);
        tracker.observe(
            Evidence::Read {
                prompt: 50_000,
                cached: 0,
            },
            1,
        );
        assert!(tracker.activity().observations.is_empty(), "no gap yet");

        tracker.observe(Evidence::Request, 30 * MINUTE);
        tracker.observe(
            Evidence::Read {
                prompt: 50_000,
                cached: 49_000,
            },
            30 * MINUTE + 1,
        );
        tracker.observe(Evidence::Request, 100 * MINUTE);
        tracker.observe(
            Evidence::Read {
                prompt: 52_000,
                cached: 0,
            },
            100 * MINUTE + 1,
        );
        let activity = tracker.activity();
        assert_eq!(activity.requested_at, Some(100 * MINUTE));
        assert_eq!(
            activity.observations,
            [
                Observation {
                    at: 30 * MINUTE,
                    gap_ms: 30 * MINUTE,
                    hit: true,
                },
                Observation {
                    at: 100 * MINUTE,
                    gap_ms: 70 * MINUTE,
                    hit: false,
                },
            ]
        );
    }

    #[test]
    fn short_gaps_small_prompts_and_unmatched_reads_are_not_observations() {
        let mut tracker = Tracker::default();
        tracker.observe(Evidence::Request, 0);
        tracker.observe(Evidence::Request, MINUTE);
        tracker.observe(
            Evidence::Read {
                prompt: 50_000,
                cached: 0,
            },
            MINUTE,
        );
        tracker.observe(Evidence::Request, 20 * MINUTE);
        tracker.observe(
            Evidence::Read {
                prompt: 1_000,
                cached: 0,
            },
            20 * MINUTE,
        );
        // Only the first reading after a request measures its gap.
        tracker.observe(
            Evidence::Read {
                prompt: 50_000,
                cached: 0,
            },
            20 * MINUTE,
        );
        assert!(tracker.activity().observations.is_empty());
        assert_eq!(tracker.activity().requested_at, Some(20 * MINUTE));
    }

    #[test]
    fn a_prefix_shared_with_other_conversations_is_not_a_hit() {
        let mut tracker = Tracker::default();
        let read = |prompt, cached| Evidence::Read { prompt, cached };
        tracker.observe(Evidence::Request, 0);
        tracker.observe(read(40_000, 39_000), 0);
        // The system prompt stayed cached for other sessions; this
        // conversation's own context did not.
        tracker.observe(Evidence::Request, 90 * MINUTE);
        tracker.observe(read(41_000, 22_000), 90 * MINUTE);
        assert_eq!(
            tracker.activity().observations,
            [Observation {
                at: 90 * MINUTE,
                gap_ms: 90 * MINUTE,
                hit: false,
            }]
        );
    }

    #[test]
    fn a_continued_request_refreshes_the_cache_but_measures_nothing() {
        let mut tracker = Tracker::default();
        let read = |prompt, cached| Evidence::Read { prompt, cached };
        tracker.observe(Evidence::Request, 0);
        tracker.observe(read(40_000, 39_000), 0);
        // A turn after an idle gap whose first request ran a long tool:
        // the reading that follows still belongs to the turn's first request.
        tracker.observe(Evidence::Request, 30 * MINUTE);
        tracker.observe(Evidence::Continued, 36 * MINUTE);
        tracker.observe(read(42_000, 40_000), 36 * MINUTE);
        tracker.observe(Evidence::Continued, 45 * MINUTE);
        tracker.observe(read(44_000, 0), 45 * MINUTE);
        // Each reading reports the request before it, so the latest
        // continuation counts once its own reading arrives.
        let activity = tracker.activity();
        assert_eq!(activity.requested_at, Some(36 * MINUTE));
        assert_eq!(
            activity.observations,
            [Observation {
                at: 30 * MINUTE,
                gap_ms: 30 * MINUTE,
                hit: true,
            }]
        );
    }

    #[test]
    fn a_request_that_failed_unreported_neither_refreshes_nor_measures() {
        let mut tracker = Tracker::default();
        let read = |prompt, cached| Evidence::Read { prompt, cached };
        tracker.observe(Evidence::Request, 0);
        tracker.observe(read(40_000, 39_000), 0);
        // Refused after an hour idle, as a usage limit refuses it.
        assert!(!tracker.observe(Evidence::Request, 60 * MINUTE));
        assert_eq!(tracker.activity().requested_at, Some(0));
        // The retry measures the whole hour, not the minutes since.
        tracker.observe(Evidence::Request, 63 * MINUTE);
        tracker.observe(read(41_000, 1_000), 63 * MINUTE);
        let activity = tracker.activity();
        assert_eq!(activity.requested_at, Some(63 * MINUTE));
        assert_eq!(
            activity.observations,
            [Observation {
                at: 63 * MINUTE,
                gap_ms: 63 * MINUTE,
                hit: false,
            }]
        );
    }

    #[test]
    fn a_continuation_counts_once_the_provider_reports_on_it() {
        let mut tracker = Tracker::default();
        let read = |prompt, cached| Evidence::Read { prompt, cached };
        tracker.observe(Evidence::Request, 0);
        tracker.observe(read(40_000, 39_000), 0);
        // A tool finished as its turn was stopped, so nothing followed.
        tracker.observe(Evidence::Request, 10 * MINUTE);
        tracker.observe(read(41_000, 40_000), 10 * MINUTE);
        tracker.observe(Evidence::Continued, 30 * MINUTE);
        assert_eq!(tracker.activity().requested_at, Some(10 * MINUTE));
        tracker.observe(read(43_000, 41_000), 31 * MINUTE);
        assert_eq!(tracker.activity().requested_at, Some(30 * MINUTE));
    }

    #[test]
    fn a_continuation_waits_for_its_own_reading_after_the_one_it_continues() {
        let mut tracker = Tracker::default();
        let read = |prompt, cached| Evidence::Read { prompt, cached };
        tracker.observe(Evidence::Request, 0);
        tracker.observe(read(40_000, 39_000), 0);
        // Codex's order: two tools finish, then the reading of the request
        // that called them arrives; the turn is stopped before more follows.
        tracker.observe(Evidence::Request, 10 * MINUTE);
        tracker.observe(Evidence::Continued, 30 * MINUTE);
        tracker.observe(Evidence::Continued, 31 * MINUTE);
        tracker.observe(read(41_000, 40_000), 31 * MINUTE);
        assert_eq!(tracker.activity().requested_at, Some(10 * MINUTE));
        // Had it continued, its own reading would have counted it.
        tracker.observe(read(42_000, 41_000), 32 * MINUTE);
        assert_eq!(tracker.activity().requested_at, Some(31 * MINUTE));
    }

    #[test]
    fn a_new_model_measures_nothing_about_the_old_cache() {
        let mut tracker = Tracker::default();
        let read = |prompt, cached| Evidence::Read { prompt, cached };
        tracker.observe(Evidence::Request, 0);
        tracker.observe(read(40_000, 39_000), 0);
        tracker.observe(Evidence::Request, 3 * MINUTE);
        tracker.forget_prompt();
        tracker.observe(read(41_000, 2_000), 3 * MINUTE);
        assert!(tracker.activity().observations.is_empty());
    }

    #[test]
    fn a_reported_lifetime_confirms_the_request_and_is_kept() {
        let mut tracker = Tracker::default();
        tracker.observe(Evidence::Request, 5);
        assert!(tracker.observe(
            Evidence::Written {
                ttl_ms: 60 * MINUTE
            },
            5
        ));
        assert!(!tracker.observe(
            Evidence::Written {
                ttl_ms: 60 * MINUTE
            },
            5
        ));
        let activity = tracker.activity();
        assert_eq!(activity.requested_at, Some(5));
        assert_eq!(activity.ttl_ms, Some(60 * MINUTE));
    }

    #[test]
    fn only_the_latest_observations_are_kept() {
        let mut tracker = Tracker::default();
        for turn in 0..=Tracker::KEPT as u64 + 2 {
            tracker.observe(Evidence::Request, turn * 10 * MINUTE);
            tracker.observe(
                Evidence::Read {
                    prompt: 50_000,
                    cached: 50_000,
                },
                turn * 10 * MINUTE,
            );
        }
        let observations = tracker.activity().observations;
        assert_eq!(observations.len(), Tracker::KEPT);
        assert_eq!(
            observations.last().map(|observation| observation.at),
            Some((Tracker::KEPT as u64 + 2) * 10 * MINUTE)
        );
    }
}
