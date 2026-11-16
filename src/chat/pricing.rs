//! API-equivalent text token estimates, never provider billing or plan credits.
//!
//! Rates verified 2026-10-12; checked primary sources and estimator limitations
//! are recorded in docs/codex-cost-estimates.md. Unknown models and tiers
//! deliberately have no inferred fallback price.
use serde::{Deserialize, Serialize};

// Only the daemon prices the usage its providers report; every lane shows the result.
#[cfg(feature = "host")]
mod tracker;
#[cfg(feature = "host")]
pub(crate) use tracker::{CostTracker, Tokens};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CostEstimate {
    pub microusd: u64,
    /// Some usage was unobserved, unpriced, or could not be reconciled.
    pub partial: bool,
}
