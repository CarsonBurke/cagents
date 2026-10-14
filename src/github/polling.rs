//! One host budget shared by review, inspector and review-request polling.
use super::*;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimit {
    pub retry_at: u64,
}
impl std::fmt::Display for RateLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let time = chrono::DateTime::from_timestamp(self.retry_at.min(i64::MAX as u64) as i64, 0)
            .map(|date| date.with_timezone(&chrono::Local).format("%H:%M").to_string())
            .unwrap_or_else(|| "the reset time".into());
        write!(f, "GitHub rate limit reached. Retrying at {time}")
    }
}
pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}
impl RateLimit {
    pub fn classify(text: &str, now: u64) -> Option<Self> {
        let lower = text.to_ascii_lowercase();
        if !["rate limit", "rate_limit", "rate-limited", "retry-after", "abuse detection"].iter().any(|s| lower.contains(s))
            && !lower.contains("x-ratelimit-remaining: 0") { return None; }
        let header = |name: &str| lower.lines().find_map(|line| {
            let (key, value) = line.trim().split_once(':')?;
            (key.trim() == name).then(|| value.trim().parse::<u64>().ok()).flatten()
        });
        Some(Self { retry_at: header("x-ratelimit-reset")
            .or_else(|| header("retry-after").map(|seconds| now.saturating_add(seconds)))
            .unwrap_or_else(|| now.saturating_add(60)).max(now.saturating_add(1)) })
    }
}
#[derive(Default)]
struct Budget { failures: u32, due: u64, rate_limited: bool }
static BUDGETS: LazyLock<Mutex<HashMap<String, Budget>>> = LazyLock::new(Default::default);
/// Manual refresh bypasses transient backoff, never a server's rate limit.
pub fn ready(host: &str, manual: bool) -> bool {
    BUDGETS.lock().unwrap_or_else(|e| e.into_inner()).get(&host.to_ascii_lowercase())
        .is_none_or(|b| b.due <= now() || (manual && !b.rate_limited))
}
pub fn failed(host: &str, error: &GitHubError) {
    let mut budgets = BUDGETS.lock().unwrap_or_else(|e| e.into_inner());
    let budget = budgets.entry(host.to_ascii_lowercase()).or_default();
    budget.failures = budget.failures.saturating_add(1);
    if let Some(limit) = error.rate_limit() {
        budget.due = budget.due.max(limit.retry_at);
        budget.rate_limited = true;
    } else if !budget.rate_limited || budget.due <= now() {
        let delay = 30u64.saturating_mul(1u64 << budget.failures.saturating_sub(1).min(6));
        // A per-host/time offset staggers windows recovering together.
        let jitter = host.bytes().fold(now(), |n, b| n.wrapping_mul(31).wrapping_add(b as u64)) % 11;
        budget.due = now().saturating_add(delay.min(1800) + jitter);
        budget.rate_limited = false;
    }
}
pub fn succeeded(host: &str) {
    let mut budgets = BUDGETS.lock().unwrap_or_else(|e| e.into_inner());
    if budgets.get(&host.to_ascii_lowercase()).is_none_or(|b| !b.rate_limited || b.due <= now()) {
        budgets.remove(&host.to_ascii_lowercase());
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn classifies_primary_secondary_graphql_and_abuse_limits() {
        for text in ["API rate limit exceeded\nX-RateLimit-Reset: 200", "X-RateLimit-Remaining: 0\nX-RateLimit-Reset: 200", "secondary rate limit\nRetry-After: 100", "GraphQL RATE_LIMITED\nRetry-After: 100", "abuse detection\nRetry-After: 100"] {
            assert_eq!(RateLimit::classify(text, 100).unwrap().retry_at, 200, "{text}");
        }
        assert!(RateLimit::classify("network disconnected", 100).is_none());
    }
    #[test]
    fn one_host_budget_pauses_all_pollers_and_manual_bypasses_only_transient_failures() {
        let host = "polling-test.invalid";
        failed(host, &GitHubError::Inspection("offline".into()));
        assert!(!ready(host, false)); assert!(ready(host, true));
        let first = BUDGETS.lock().unwrap().get(host).unwrap().due;
        failed(host, &GitHubError::Inspection("offline".into()));
        assert!(BUDGETS.lock().unwrap().get(host).unwrap().due > first);
        failed(host, &GitHubError::RateLimited { limit: RateLimit { retry_at: now()+120 } });
        assert!(!ready(host, false)); assert!(!ready(host, true));
        succeeded(host); assert!(!ready(host, false));
        BUDGETS.lock().unwrap().remove(host);
    }
}
