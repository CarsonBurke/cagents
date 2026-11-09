//! GitHub rate-limit response classification, shared by the CLI and app.
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimit {
    pub retry_at: u64,
}
impl std::fmt::Display for RateLimit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // UTC is explicit so the reset time remains unambiguous without a
        // platform-specific clock dependency in the shared CLI boundary.
        let time = format!(
            "{:02}:{:02} UTC",
            (self.retry_at / 3600) % 24,
            (self.retry_at / 60) % 60
        );
        write!(f, "GitHub rate limit reached. Retrying at {time}")
    }
}
pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}
impl RateLimit {
    pub fn classify(text: &str, now: u64) -> Option<Self> {
        let lower = text.to_ascii_lowercase();
        let header = |name: &str| {
            lower.lines().find_map(|line| {
                let (key, value) = line.trim().split_once(':')?;
                (key.trim() == name)
                    .then(|| value.trim().parse::<u64>().ok())
                    .flatten()
            })
        };
        let retry_after = header("retry-after");
        // Access-Control-Expose-Headers lists Retry-After on ordinary GitHub
        // responses; only a real Retry-After header is a pacing instruction.
        if ![
            "rate limit",
            "rate_limit",
            "rate-limited",
            "abuse detection",
        ]
        .iter()
        .any(|word| lower.contains(word))
            && header("x-ratelimit-remaining") != Some(0)
            && retry_after.is_none()
        {
            return None;
        }
        Some(Self {
            retry_at: (if header("x-ratelimit-remaining") == Some(0) {
                header("x-ratelimit-reset")
                    .into_iter()
                    .chain(retry_after.map(|seconds| now.saturating_add(seconds)))
                    .max()
            } else {
                retry_after.map(|seconds| now.saturating_add(seconds))
            })
            // A secondary limit does not exhaust the primary budget. Its
            // reset is not a retry instruction while quota remains.
            .or_else(|| {
                header("x-ratelimit-remaining")
                    .is_none()
                    .then(|| header("x-ratelimit-reset"))
                    .flatten()
            })
            .unwrap_or_else(|| now.saturating_add(60))
            .max(now.saturating_add(1)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn secondary_without_retry_after_does_not_wait_for_unused_primary_reset() {
        let limit = RateLimit::classify(
            "secondary rate limit\nX-RateLimit-Remaining: 4000\nX-RateLimit-Reset: 3600",
            100,
        )
        .unwrap();
        assert_eq!(limit.retry_at, 160);
        let exhausted = RateLimit::classify(
            "secondary rate limit\nX-RateLimit-Remaining: 0\nX-RateLimit-Reset: 3600",
            100,
        )
        .unwrap();
        assert_eq!(exhausted.retry_at, 3600);
    }
}
