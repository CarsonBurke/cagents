//! API-equivalent text token estimates, never provider billing or plan credits.
//!
//! Rates verified 2026-10-12; checked primary sources and estimator limitations
//! are recorded in docs/codex-cost-estimates.md. Unknown models and tiers
//! deliberately have no inferred fallback price.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostEstimate {
    pub microusd: u64,
    /// Some usage was unobserved, unpriced, or could not be reconciled.
    pub partial: bool,
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tokens {
    pub input: u64,
    pub cached_input: u64,
    pub cache_write_input: u64,
    /// Includes reasoning output; do not add reasoning tokens a second time.
    pub output: u64,
}
impl Tokens {
    fn difference(self, old: Self) -> Option<Self> {
        Some(Self {
            input: self.input.checked_sub(old.input)?,
            cached_input: self.cached_input.checked_sub(old.cached_input)?,
            cache_write_input: self.cache_write_input.checked_sub(old.cache_write_input)?,
            output: self.output.checked_sub(old.output)?,
        })
    }
    fn empty(self) -> bool {
        self.input == 0 && self.output == 0 && self.cached_input == 0 && self.cache_write_input == 0
    }
}

/// Rates in 1/10,000 microdollars per token, keeping fractions until presentation.
struct Rates {
    input: u64,
    cached: u64,
    write: u64,
    output: u64,
}
fn rates(model: &str, tier: Option<&str>, long: bool) -> Option<Rates> {
    // Recognize dated snapshots without treating arbitrary model suffixes as aliases.
    let model = if model.len() > 11 && model.as_bytes()[model.len() - 11] == b'-' {
        let suffix = &model[model.len() - 10..];
        if suffix.bytes().enumerate().all(|(index, byte)| {
            if index == 4 || index == 7 {
                byte == b'-'
            } else {
                byte.is_ascii_digit()
            }
        }) {
            &model[..model.len() - 11]
        } else {
            model
        }
    } else {
        model
    };
    let (input, cached, write, output, long_context) = match model {
        "gpt-6-astra" => (100_000, 10_000, 125_000, 500_000, true),
        "gpt-6.1-sol" => (20_000, 1_000, 25_000, 100_000, true),
        "gpt-6-sol" => (20_000, 2_000, 25_000, 100_000, true),
        "gpt-6-luna" => (1_000, 100, 1_250, 5_000, true),
        "gpt-5.6" | "gpt-5.6-sol" => (40_000, 4_000, 50_000, 200_000, true),
        "gpt-5.6-terra" => (20_000, 2_000, 25_000, 120_000, true),
        "gpt-5.6-luna" => (2_000, 200, 2_500, 12_000, true),
        "gpt-5.5" => (50_000, 5_000, 50_000, 300_000, true),
        "gpt-5.4" => (25_000, 2_500, 25_000, 150_000, true),
        "gpt-5.4-mini" => (7_500, 750, 7_500, 45_000, false),
        "gpt-5.4-nano" => (2_000, 200, 2_000, 12_500, false),
        "gpt-5.3-codex" => (17_500, 1_750, 17_500, 140_000, false),
        _ => return None,
    };
    let (numerator, denominator) = match tier {
        None | Some("default" | "standard") => (1, 1),
        Some("fast" | "priority") if !matches!(model, "gpt-5.4-mini" | "gpt-5.4-nano") => (2, 1),
        Some("flex" | "batch") if model != "gpt-5.3-codex" => (1, 2),
        Some("ultrafast") if model == "gpt-6.1-sol" || model == "gpt-6-astra" => (6, 1),
        _ => return None,
    };
    let long = long
        && long_context
        && !(matches!(model, "gpt-5.4" | "gpt-5.5") && matches!(tier, Some("fast" | "priority")));
    Some(Rates {
        input: input * numerator / denominator * if long { 2 } else { 1 },
        cached: cached * numerator / denominator * if long { 2 } else { 1 },
        write: write * numerator / denominator * if long { 2 } else { 1 },
        output: output * numerator / denominator * if long { 3 } else { 2 } / 2,
    })
}
fn cost(tokens: Tokens, rates: Rates) -> Option<u128> {
    let ordinary = tokens
        .input
        .checked_sub(tokens.cached_input)?
        .checked_sub(tokens.cache_write_input)?;
    Some(
        u128::from(ordinary) * u128::from(rates.input)
            + u128::from(tokens.cached_input) * u128::from(rates.cached)
            + u128::from(tokens.cache_write_input) * u128::from(rates.write)
            + u128::from(tokens.output) * u128::from(rates.output),
    )
}

/// Cumulative usage notifications may repeat; charge only their increments at
/// the model/tier that ran that request. Never reprice earlier turns on a toggle.
#[derive(Debug, Default)]
pub(crate) struct CostTracker {
    previous: Option<Tokens>,
    units: u128,
    priced: bool,
    partial: bool,
}
impl CostTracker {
    pub fn update(
        &mut self,
        model: Option<&str>,
        tier: Option<&str>,
        total: Tokens,
        last: Tokens,
    ) -> Option<CostEstimate> {
        if let Some(previous) = self.previous
            && total.input == previous.input
            && total.output == previous.output
            && (total.cached_input != previous.cached_input
                || total.cache_write_input != previous.cache_write_input)
        {
            // A provider revised how existing input was classified. Its
            // historical model/tier is not recoverable from cumulative totals;
            // retain the approximate amount and disclose incomplete accounting.
            self.previous = Some(total);
            self.partial = true;
            return self.estimate();
        }
        let delta = match self.previous {
            Some(previous) => match total.difference(previous) {
                Some(delta) if delta.input <= last.input && delta.output <= last.output => delta,
                Some(_) => {
                    // Missing notifications may cover different models or tariffs.
                    // Price only the request the provider actually describes.
                    self.partial = true;
                    last
                }
                None => {
                    // Late cumulative notifications must not charge an older
                    // request again or move the accounting baseline backwards.
                    return self.estimate();
                }
            },
            None => {
                // Resumed threads include history with unknown historical model/tier.
                self.partial |= total != last;
                last
            }
        };
        self.previous = Some(total);
        if !delta.empty() {
            match model
                .and_then(|model| rates(model, tier, last.input > 272_000))
                .and_then(|rates| cost(delta, rates))
            {
                Some(units) => {
                    self.units = self.units.saturating_add(units);
                    self.priced = true;
                }
                None => self.partial = true,
            }
        }
        self.estimate()
    }

    fn estimate(&self) -> Option<CostEstimate> {
        self.priced.then(|| CostEstimate {
            microusd: u64::try_from(self.units.saturating_add(5_000) / 10_000).unwrap_or(u64::MAX),
            partial: self.partial,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tokens(input: u64, cached_input: u64, cache_write_input: u64, output: u64) -> Tokens {
        Tokens {
            input,
            cached_input,
            cache_write_input,
            output,
        }
    }
    #[test]
    fn codex_estimate_counts_cache_and_output_once_and_preserves_fractional_rates() {
        let mut tracker = CostTracker::default();
        let total = tokens(1_000_000, 600_000, 100_000, 100_000);
        let estimate = tracker
            .update(Some("gpt-6.1-sol"), None, total, total)
            .unwrap();
        assert_eq!(
            estimate,
            CostEstimate {
                microusd: 3_320_000,
                partial: false
            }
        );
        assert_eq!(
            tracker.update(Some("gpt-6.1-sol"), Some("fast"), total, total),
            Some(estimate)
        );
        let lunar = tokens(1, 0, 1, 0);
        let mut tracker = CostTracker::default();
        for n in 1..=8 {
            tracker.update(Some("gpt-6-luna"), Some("flex"), tokens(n, 0, n, 0), lunar);
        }
        assert_eq!(tracker.units, 5_000);
    }
    #[test]
    fn codex_estimate_keeps_previous_model_rates_when_fast_mode_changes() {
        let mut tracker = CostTracker::default();
        let first = tokens(1_000, 0, 0, 100);
        assert_eq!(
            tracker
                .update(Some("gpt-6.1-sol"), None, first, first)
                .unwrap()
                .microusd,
            3_000
        );
        let total = tokens(2_000, 0, 0, 200);
        assert_eq!(
            tracker
                .update(Some("gpt-6-astra"), Some("fast"), total, first)
                .unwrap()
                .microusd,
            33_000
        );
    }
    #[test]
    fn codex_estimate_marks_unobserved_or_unknown_usage_partial() {
        let mut tracker = CostTracker::default();
        let last = tokens(1_000, 0, 0, 100);
        assert!(
            tracker
                .update(Some("unpriced-model"), None, last, last)
                .is_none()
        );
        let total = tokens(2_000, 0, 0, 200);
        assert_eq!(
            tracker.update(Some("gpt-6-sol"), None, total, last),
            Some(CostEstimate {
                microusd: 3_000,
                partial: true
            })
        );
        let mut resumed = CostTracker::default();
        assert!(
            resumed
                .update(Some("gpt-6-sol"), None, total, last)
                .unwrap()
                .partial
        );
        assert!(rates("gpt-6-sol-future", None, false).is_none());
        assert!(rates("gpt-6-sol", Some("unknown"), false).is_none());
        assert!(rates("gpt-6-sol-2026-10-01", None, false).is_some());
    }
    #[test]
    fn codex_estimate_marks_cache_only_corrections_partial_without_double_charging() {
        for (initial, corrected) in [
            (tokens(1_000, 0, 0, 100), tokens(1_000, 900, 0, 100)),
            (tokens(1_000, 0, 0, 100), tokens(1_000, 0, 900, 100)),
            (tokens(1_000, 900, 0, 100), tokens(1_000, 0, 0, 100)),
            (tokens(1_000, 0, 900, 100), tokens(1_000, 900, 0, 100)),
        ] {
            let mut tracker = CostTracker::default();
            let before = tracker
                .update(Some("gpt-6.1-sol"), None, initial, initial)
                .unwrap();
            assert!(!before.partial);
            let after = tracker
                .update(Some("gpt-6.1-sol"), None, corrected, corrected)
                .unwrap();
            assert_eq!(after.microusd, before.microusd);
            assert!(after.partial);
            assert_eq!(
                tracker.update(Some("gpt-6.1-sol"), None, corrected, corrected),
                Some(after)
            );
            assert_eq!(tracker.previous, Some(corrected));
            let next = tokens(
                2_000,
                corrected.cached_input,
                corrected.cache_write_input,
                200,
            );
            let estimate = tracker
                .update(Some("gpt-6.1-sol"), None, next, tokens(1_000, 0, 0, 100))
                .unwrap();
            assert_eq!(estimate.microusd, before.microusd + 3_000);
            assert!(estimate.partial);
        }
    }

    #[test]
    fn codex_estimate_ignores_late_cumulative_notifications() {
        let mut tracker = CostTracker::default();
        let first = tokens(1_000, 0, 0, 100);
        let total = tokens(2_000, 0, 0, 200);
        tracker.update(Some("gpt-6-sol"), None, first, first);
        let before = tracker.update(Some("gpt-6-sol"), None, total, first);
        assert_eq!(
            tracker.update(Some("gpt-6-astra"), Some("fast"), first, first),
            before
        );
        assert_eq!(tracker.previous, Some(total));
        let next = tokens(3_000, 0, 0, 300);
        assert_eq!(
            tracker
                .update(Some("gpt-6-sol"), None, next, first)
                .unwrap()
                .microusd,
            9_000
        );
    }

    #[test]
    fn codex_estimate_uses_request_input_for_long_context_not_cumulative_input() {
        let mut tracker = CostTracker::default();
        let last = tokens(300_000, 0, 0, 100);
        assert_eq!(
            tracker
                .update(Some("gpt-6.1-sol"), None, last, last)
                .unwrap()
                .microusd,
            1_201_500
        );
        let next = tokens(600_000, 0, 0, 200);
        let short = tokens(10_000, 0, 0, 100);
        assert_eq!(
            tracker
                .update(Some("gpt-6.1-sol"), None, next, short)
                .unwrap()
                .microusd,
            1_222_500
        );
        assert!(cost(tokens(1, 2, 0, 0), rates("gpt-6-sol", None, false).unwrap()).is_none());
    }
}
