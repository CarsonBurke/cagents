//! GitHub rate-limit response classification, shared by the CLI and app.
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RateLimit {
    pub retry_at: u64,
    /// Resource-local pacing, including primary exhaustion and ambiguous rate
    /// errors. "unknown" must be bound to the request's resource; it does not
    /// establish a primary diagnosis. None denotes secondary/server pacing.
    pub resource: Option<String>,
    /// A response can exhaust a primary quota and also instruct all requests
    /// to wait briefly. Never extend that secondary pause to the primary reset.
    pub secondary_retry_at: Option<u64>,
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
    pub fn secondary(retry_at: u64) -> Self {
        Self {
            retry_at,
            resource: None,
            secondary_retry_at: None,
        }
    }

    pub fn primary(resource: impl Into<String>, retry_at: u64) -> Self {
        Self {
            retry_at,
            resource: Some(resource.into()),
            secondary_retry_at: None,
        }
    }

    pub fn with_resource(mut self, resource: &str) -> Self {
        if self.resource.as_deref() == Some("unknown") {
            self.resource = Some(resource.to_owned());
        }
        self
    }

    pub fn for_resource(&self, resource: &str) -> Option<Self> {
        self.for_resource_at(resource, now())
    }

    pub fn for_resource_at(&self, resource: &str, at: u64) -> Option<Self> {
        if self.retry_at > at
            && (self.resource.is_none() || self.resource.as_deref() == Some(resource))
        {
            return Some(self.clone());
        }
        self.secondary_retry_at
            .filter(|due| *due > at)
            .map(Self::secondary)
    }

    pub fn classify_for(text: &str, now: u64, resource: &str) -> Option<Self> {
        Self::classify(text, now).map(|limit| limit.with_resource(resource))
    }

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
        let secondary = lower.contains("secondary") || lower.contains("abuse detection");
        let primary = header("x-ratelimit-remaining") == Some(0)
            || (!secondary
                && retry_after.is_none()
                && header("x-ratelimit-reset").is_some()
                && header("x-ratelimit-remaining").is_none());
        let secondary_due = retry_after
            .map(|seconds| now.saturating_add(seconds))
            .or_else(|| secondary.then(|| now.saturating_add(60)));
        if primary {
            let resource = lower
                .lines()
                .find_map(|line| {
                    let (key, value) = line.trim().split_once(':')?;
                    (key.trim() == "x-ratelimit-resource").then(|| value.trim().to_owned())
                })
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| "unknown".into());
            Some(Self {
                retry_at: header("x-ratelimit-reset")
                    .into_iter()
                    .chain(secondary_due)
                    .max()
                    .unwrap_or_else(|| now.saturating_add(60))
                    .max(now.saturating_add(1)),
                resource: Some(resource),
                secondary_retry_at: secondary_due,
            })
        } else if let Some(due) = secondary_due {
            Some(Self::secondary(due.max(now.saturating_add(1))))
        } else {
            // A headerless rate error does not establish a secondary limit.
            // Briefly pause the failing resource without blocking unrelated
            // transports; classify_for binds this unknown resource to its call.
            Some(Self {
                retry_at: now.saturating_add(60),
                resource: Some("unknown".into()),
                secondary_retry_at: None,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ambiguous_headerless_rate_errors_pause_only_the_request_resource() {
        for text in [
            "GraphQL: API rate limit already exceeded",
            "API rate limit exceeded",
            "GraphQL RATE_LIMITED",
        ] {
            let unbound = RateLimit::classify(text, 100).unwrap();
            assert_eq!(unbound.resource.as_deref(), Some("unknown"));
            assert_eq!(unbound.secondary_retry_at, None);
            for resource in ["graphql", "core", "search"] {
                let limit = RateLimit::classify_for(text, 100, resource).unwrap();
                assert_eq!(limit.resource.as_deref(), Some(resource));
                assert_eq!(limit.retry_at, 160);
                assert_eq!(limit.secondary_retry_at, None);
                assert!(limit.for_resource_at(resource, 100).is_some());
                let other = if resource == "graphql" {
                    "core"
                } else {
                    "graphql"
                };
                assert!(limit.for_resource_at(other, 100).is_none());
            }
        }
        for text in [
            "GraphQL: API rate limit already exceeded\nRetry-After: 20",
            "secondary rate limit",
            "abuse detection",
        ] {
            let limit = RateLimit::classify_for(text, 100, "graphql").unwrap();
            assert!(limit.resource.is_none(), "{text}");
            assert!(limit.for_resource_at("core", 100).is_some());
        }
    }
    #[test]
    fn primary_reset_and_secondary_retry_after_have_independent_resources_and_deadlines() {
        let limit = RateLimit::classify_for(
            "X-RateLimit-Remaining: 0\nX-RateLimit-Resource: graphql\nX-RateLimit-Reset: 3600\nRetry-After: 20",
            100,
            "graphql",
        ).unwrap();
        assert_eq!(limit.resource.as_deref(), Some("graphql"));
        assert_eq!(
            limit.for_resource_at("graphql", 121).unwrap().retry_at,
            3600
        );
        assert_eq!(
            limit.for_resource_at("core", 110),
            Some(RateLimit::secondary(120))
        );
        assert!(limit.for_resource_at("core", 121).is_none());
        assert!(limit.for_resource_at("graphql", 3600).is_none());
        let core = RateLimit::classify_for(
            "X-RateLimit-Remaining: 0\nX-RateLimit-Reset: 500",
            100,
            "core",
        )
        .unwrap();
        assert_eq!(core.resource.as_deref(), Some("core"));
        assert!(core.for_resource_at("graphql", 100).is_none());
    }
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
