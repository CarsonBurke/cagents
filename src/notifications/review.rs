//! Exact GitHub review-request authority, independent of a provider session.
use crate::github_identity::GitHubRepoKey;
use serde::{Deserialize, Serialize};

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
        if self.number == 0
            || self.number > i32::MAX as u64
            || !valid_node(&self.viewer)
            || !valid_node(&self.request)
        {
            return Err("Invalid review-request identity".into());
        }
        validate_repository(&self.repository_key())
    }

    pub fn repository_key(&self) -> GitHubRepoKey {
        GitHubRepoKey {
            host: self.host.clone(),
            port: self.port,
            owner: self.owner.clone(),
            name: self.repository.clone(),
        }
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
        request_key(&self.host, self.port, &self.viewer, &self.request)
    }
}

fn valid_node(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 256
        && s.bytes()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'+' | b'/' | b'='))
}

fn validate_repository(repository: &GitHubRepoKey) -> Result<(), String> {
    let component = |s: &str| {
        !s.is_empty()
            && s.len() <= 100
            && s != "."
            && s != ".."
            && s.bytes().all(|c| {
                c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'-' | b'_' | b'.')
            })
    };
    if repository.host.is_empty()
        || repository.host.len() > 253
        || repository.host != repository.host.to_ascii_lowercase()
        || !component(&repository.owner)
        || !component(&repository.name)
    {
        return Err("Invalid review repository identity".into());
    }
    let url = url::Url::parse(&format!("https://{}/", repository.authority()))
        .map_err(|_| "Invalid review repository host")?;
    if url.host_str() != Some(repository.host.as_str())
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port_or_known_default() != Some(repository.port.unwrap_or(443))
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Invalid review repository host".into());
    }
    Ok(())
}

#[cfg(any(feature = "host", test))]
pub(crate) fn request_key(host: &str, port: Option<u16>, viewer: &str, request: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut digest = Sha256::new();
    for part in [host.as_bytes(), viewer.as_bytes(), request.as_bytes()] {
        digest.update((part.len() as u64).to_le_bytes());
        digest.update(part);
    }
    digest.update(port.unwrap_or(443).to_le_bytes());
    format!("review:{:x}", digest.finalize())
}

/// A complete, error-free monitor pass for one viewer. An empty set is an
/// authoritative observation. Partial, failed and cancelled reads send no set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Census {
    pub viewer: String,
    pub started_at: u64,
    pub completed_at: u64,
    pub requests: Vec<(u64, String)>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WatchUpdate {
    pub repository: GitHubRepoKey,
    pub census: Option<Census>,
}

impl WatchUpdate {
    pub fn validate(&self) -> Result<(), String> {
        validate_repository(&self.repository)?;
        if let Some(census) = &self.census
            && (!valid_node(&census.viewer)
                || census.started_at == 0
                || census.completed_at < census.started_at
                || census.requests.iter().any(|(number, request)| {
                    *number == 0 || *number > i32::MAX as u64 || !valid_node(request)
                }))
        {
            return Err("Invalid settled review-request census".into());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ObservedRequests {
    pub destination: super::Destination,
    pub requests: Vec<ReviewRequest>,
}

impl ObservedRequests {
    pub fn validate(&self) -> Result<(), String> {
        self.destination.validate()?;
        if self.requests.len() > 50 {
            return Err("Review-request batch exceeds one page".into());
        }
        for request in &self.requests {
            request.validate()?;
        }
        Ok(())
    }
}

impl ObservedRequests {
    pub fn events(self) -> impl Iterator<Item = super::Event> {
        self.requests.into_iter().map(move |request| super::Event {
            content: None,
            evidence: super::Evidence {
                key: request.request.clone(),
                kind: super::Kind::ReviewRequested,
            },
            target: super::Target::PullRequest(request),
            source_session: None,
            provider: None,
            directory: None,
            destination: Some(self.destination.clone()),
        })
    }
}

#[cfg(feature = "ui")]
impl ReviewRequest {
    /// Activation is a fresh authorization read, not a replay of notification
    /// content under a potentially different GitHub account.
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

#[cfg(feature = "ui")]
pub async fn report(observed: ObservedRequests) -> Result<(), String> {
    tokio::task::spawn_blocking(move || {
        observed.validate()?;
        crate::terminal::connect_ready_daemon_for(observed.destination.application)
            .map_err(|error| error.to_string())?
            .client
            .observe_review_requests(observed)
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

    #[test]
    fn exact_review_target_round_trips_without_process_context() {
        let batch = ObservedRequests {
            destination: crate::notifications::Destination {
                application: crate::notifications::Application::Ceditor,
                window: "editor-window".into(),
            },
            requests: vec![request()],
        };
        batch.validate().unwrap();
        let event = batch.events().next().unwrap();
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
        let too_many = ObservedRequests {
            destination: crate::notifications::Destination {
                application: crate::notifications::Application::Cagents,
                window: "primary".into(),
            },
            requests: vec![request(); 51],
        };
        assert!(too_many.validate().is_err());
    }
}
