//! Exact GitHub review-request authority, independent of a provider session.
use crate::github_identity::GitHubRepoKey;
use serde::{Deserialize, Serialize};

fn valid_node(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 256
        && id.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'+' | b'/' | b'=')
        })
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct ReviewRequest {
    pub host: String,
    pub port: Option<u16>,
    pub owner: String,
    pub repository: String,
    pub number: u64,
    pub viewer: String,
    pub request: String,
}

impl ReviewRequest {
    pub fn validate(&self) -> Result<(), String> {
        let component = |s: &str| {
            !s.is_empty()
                && s.len() <= 100
                && s != "."
                && s != ".."
                && s.bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
        };
        if self.host.is_empty()
            || self.host.len() > 253
            || self.host != self.host.to_ascii_lowercase()
            || self.owner != self.owner.to_ascii_lowercase()
            || self.repository != self.repository.to_ascii_lowercase()
            || !component(&self.owner)
            || !component(&self.repository)
            || self.number == 0
            || self.number > i32::MAX as u64
            || !valid_node(&self.viewer)
            || !valid_node(&self.request)
        {
            return Err("Invalid review-request identity".into());
        }
        let url = url::Url::parse(&self.url()).map_err(|_| "Invalid review repository host")?;
        if url.host_str() != Some(self.host.as_str())
            || url.username() != ""
            || url.password().is_some()
            || url.port_or_known_default() != Some(self.port.unwrap_or(443))
        {
            return Err("Invalid review repository host".into());
        }
        Ok(())
    }

    pub fn url(&self) -> String {
        let authority = self
            .port
            .map_or_else(|| self.host.clone(), |port| format!("{}:{port}", self.host));
        format!(
            "https://{authority}/{}/{}/pull/{}",
            self.owner, self.repository, self.number
        )
    }

    /// Request IDs are immutable GitHub nodes. Route text may change (repository
    /// rename), but the same viewer/request must never be charged a second alert.
    #[cfg(any(feature = "host", test))]
    pub(crate) fn storage_key(&self) -> String {
        use sha2::{Digest, Sha256};
        let mut digest = Sha256::new();
        for part in [
            self.host.as_bytes(),
            self.viewer.as_bytes(),
            self.request.as_bytes(),
        ] {
            digest.update((part.len() as u64).to_le_bytes());
            digest.update(part);
        }
        digest.update(self.port.unwrap_or(443).to_le_bytes());
        format!("review:{:x}", digest.finalize())
    }
}

/// The point in the credential-scoped shared cache that supplied evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScanPoint {
    pub incarnation: String,
    pub revision: u64,
    pub at: u64,
}

impl ScanPoint {
    const CLOCK_ONLY: &str = "00000000-0000-0000-0000-000000000000";

    /// Older shared caches record when a settled scan began, but have no
    /// stored revision checkpoint for that start.
    #[cfg(any(feature = "ui", feature = "host"))]
    pub(crate) fn legacy_census(at: u64) -> Self {
        Self {
            incarnation: Self::CLOCK_ONLY.into(),
            revision: 0,
            at,
        }
    }

    fn valid(&self) -> bool {
        self.at > 0 && uuid::Uuid::parse_str(&self.incarnation).is_ok_and(|id| !id.is_nil())
    }

    fn valid_census(&self) -> bool {
        self.valid() || self.at > 0 && self.incarnation == Self::CLOCK_ONLY && self.revision == 0
    }

    pub fn precedes_census(&self, start: &Self) -> bool {
        if self.incarnation == start.incarnation && self.incarnation != Self::CLOCK_ONLY {
            self.revision <= start.revision
        } else {
            self.at < start.at
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedRequest {
    pub request: ReviewRequest,
    pub point: ScanPoint,
}

// Kept at its original postcard shape for older carried clients.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedRequests {
    pub destination: super::Destination,
    pub requests: Vec<ReviewRequest>,
}

impl ObservedRequests {
    pub fn validate(&self) -> Result<(), String> {
        validate_observed(&self.destination, &self.requests, self.requests.len())
    }

    pub fn events(self) -> impl Iterator<Item = super::Event> {
        self.requests
            .into_iter()
            .map(move |request| request.notification(self.destination.clone()))
    }
}

fn validate_observed<'a>(
    destination: &super::Destination,
    requests: impl IntoIterator<Item = &'a ReviewRequest>,
    count: usize,
) -> Result<(), String> {
    destination.validate()?;
    if count > 50 {
        return Err("Review-request batch exceeds one page".into());
    }
    for request in requests {
        request.validate()?;
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StampedRequests {
    pub destination: super::Destination,
    pub requests: Vec<ObservedRequest>,
}

impl StampedRequests {
    pub fn validate(&self) -> Result<(), String> {
        validate_observed(
            &self.destination,
            self.requests.iter().map(|request| &request.request),
            self.requests.len(),
        )?;
        if self.requests.iter().any(|request| !request.point.valid()) {
            return Err("Review observation has no cache identity".into());
        }
        Ok(())
    }
}

impl StampedRequests {
    pub fn events(self) -> impl Iterator<Item = (super::Event, ScanPoint)> {
        self.requests
            .into_iter()
            .map(move |ObservedRequest { request, point }| {
                (request.notification(self.destination.clone()), point)
            })
    }
}

impl ReviewRequest {
    pub fn notification(self, destination: super::Destination) -> super::Event {
        super::Event {
            content: None,
            evidence: super::Evidence {
                key: self.request.clone(),
                kind: super::Kind::ReviewRequested,
            },
            target: super::Target::PullRequest(self),
            source_session: None,
            provider: None,
            directory: None,
            destination: Some(destination),
        }
    }

    pub fn repository_key(&self) -> GitHubRepoKey {
        GitHubRepoKey {
            host: self.host.clone(),
            port: self.port,
            owner: self.owner.clone(),
            name: self.repository.clone(),
        }
    }

    /// Activation is a fresh authorization read, not a replay of notification
    /// content under a potentially different GitHub account.
    #[cfg(feature = "ui")]
    pub fn verify_current(&self) -> Result<(), String> {
        self.validate()?;
        let key = crate::github::PullRequestKey {
            repository: self.repository_key(),
            number: self.number,
        };
        let page = crate::github::GitHubService::system("gh")
            .review_request_history_page(&key, None, &crate::repository::CancellationToken::new())
            .map_err(|error| error.to_string())?;
        if page.viewer.id != self.viewer {
            return Err("This review alert belongs to a different GitHub account. Refresh review requests with the intended account.".into());
        }
        if page
            .current
            .as_ref()
            .is_none_or(|request| request.id != self.request)
        {
            return Err("This review request is no longer current. Refresh the repository's review requests.".into());
        }
        Ok(())
    }
}

/// A monitor's scope is the repository behind the exact GitHub viewer,
/// independent of which window has the monitor open.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum MonitorUpdate {
    Watching(Vec<GitHubRepoKey>),
    Completed(Census),
    Observations(StampedRequests),
}

/// Complete GitHub pagination, cached in the same credential scope as its pages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Census {
    pub repository: GitHubRepoKey,
    pub start: ScanPoint,
    pub viewer: String,
    pub current: std::collections::BTreeMap<u64, String>,
}

impl MonitorUpdate {
    pub fn validate(&self) -> Result<(), String> {
        match self {
            Self::Watching(repositories) => {
                if repositories.len() > 256 {
                    return Err("Review watches exceed one batch".into());
                }
                for repository in repositories {
                    repository.validate().map_err(|error| error.to_string())?;
                }
            }
            Self::Observations(observed) => observed.validate()?,
            Self::Completed(census) => {
                census
                    .repository
                    .validate()
                    .map_err(|error| error.to_string())?;
                if !census.start.valid_census()
                    || !valid_node(&census.viewer)
                    || census.current.len() > 4096
                    || census
                        .current
                        .iter()
                        .any(|(&number, request)| number == 0 || !valid_node(request))
                {
                    return Err(
                        "Review census has invalid cache, viewer or request identities".into(),
                    );
                }
            }
        }
        Ok(())
    }
}

#[cfg(feature = "ui")]
pub fn report_monitor(
    application: super::Application,
    update: MonitorUpdate,
) -> Result<(), String> {
    crate::terminal::connect_ready_daemon_for(application)
        .map_err(|error| error.to_string())?
        .client
        .observe_review_monitor(update)
        .map_err(|error| error.to_string())
}

#[cfg(feature = "ui")]
pub async fn report(observed: StampedRequests) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        observed.validate()?;
        crate::terminal::connect_ready_daemon_for(observed.destination.application)
            .map_err(|error| error.to_string())?
            .client
            .observe_review_monitor(MonitorUpdate::Observations(observed))
            .map_err(|error| error.to_string())
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(feature = "ui")]
pub async fn verify(request: ReviewRequest) -> Result<ReviewRequest, String> {
    tokio::task::spawn_blocking(move || {
        request.verify_current()?;
        Ok(request)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;
    fn request() -> ReviewRequest {
        ReviewRequest {
            host: "github.com".into(),
            port: None,
            owner: "owner".into(),
            repository: "repo".into(),
            number: 51,
            viewer: "viewer-node".into(),
            request: "request-node".into(),
        }
    }

    fn observed(request: ReviewRequest) -> ObservedRequest {
        ObservedRequest {
            request,
            point: ScanPoint {
                incarnation: uuid::Uuid::new_v4().to_string(),
                revision: 1,
                at: 1,
            },
        }
    }

    #[test]
    fn exact_review_target_round_trips_without_process_context() {
        let batch = StampedRequests {
            destination: crate::notifications::Destination {
                application: crate::notifications::Application::Ceditor,
                window: "editor-window".into(),
            },
            requests: vec![observed(request())],
        };
        batch.validate().unwrap();
        let (event, _) = batch.events().next().unwrap();
        event.validate().unwrap();
        assert!(
            event.provider.is_none() && event.source_session.is_none() && event.directory.is_none()
        );
        let bytes = postcard::to_stdvec(&event).unwrap();
        assert_eq!(
            postcard::from_bytes::<crate::notifications::Event>(&bytes).unwrap(),
            event
        );
        assert_eq!(
            crate::notifications::Event::from_journal(&serde_json::to_string(&event).unwrap())
                .unwrap(),
            event
        );
        let mut malformed = event;
        malformed.source_session = Some(crate::model::TerminalId::new());
        assert!(malformed.validate().is_err());
    }

    #[test]
    fn immutable_request_ledger_distinguishes_viewers_and_rerequests() {
        let original = request();
        let mut changed = original.clone();
        changed.repository = "renamed".into();
        assert_eq!(original.storage_key(), changed.storage_key());
        changed.request = "another-request".into();
        assert_ne!(original.storage_key(), changed.storage_key());
        changed = original.clone();
        changed.viewer = "another-viewer".into();
        assert_ne!(original.storage_key(), changed.storage_key());
        changed = original.clone();
        changed.host = "github.example.com".into();
        assert_ne!(original.storage_key(), changed.storage_key());
    }

    #[test]
    fn malformed_review_authority_is_rejected_before_delivery() {
        for host in [
            "github.com/other",
            "user@github.com",
            "github.com?x=1",
            "GITHUB.COM",
        ] {
            let mut invalid = request();
            invalid.host = host.into();
            assert!(invalid.validate().is_err(), "{host}");
        }
        let mut invalid = request();
        invalid.owner = "..".into();
        assert!(invalid.validate().is_err());
        invalid = request();
        invalid.viewer = "viewer\nnode".into();
        assert!(invalid.validate().is_err());
        let too_many = StampedRequests {
            destination: crate::notifications::Destination {
                application: crate::notifications::Application::Cagents,
                window: "primary".into(),
            },
            requests: vec![observed(request()); 51],
        };
        assert!(too_many.validate().is_err());
    }
}
