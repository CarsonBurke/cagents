//! GitHub repository and pull-request identities shared by every wire boundary.

use serde::{Deserialize, Serialize};
use thiserror::Error;
use url::Url;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct GitHubRepoKey {
    pub host: String,
    pub port: Option<u16>,
    pub owner: String,
    pub name: String,
}

impl GitHubRepoKey {
    pub fn same_repository(&self, other: &Self) -> bool {
        self.host.eq_ignore_ascii_case(&other.host)
            && self.port == other.port
            && self.owner.eq_ignore_ascii_case(&other.owner)
            && self.name.eq_ignore_ascii_case(&other.name)
    }

    pub fn new(
        host: impl Into<String>,
        port: Option<u16>,
        owner: impl Into<String>,
        name: impl Into<String>,
    ) -> Result<Self, GitHubValidationError> {
        let host = host.into().to_ascii_lowercase();
        let owner = owner.into();
        let name = name.into().trim_end_matches(".git").to_owned();
        let authority = match port {
            Some(port) => format!("{host}:{port}"),
            None => host.clone(),
        };
        let parsed = Url::parse(&format!("https://{authority}/"))
            .map_err(|_| GitHubValidationError::InvalidHost(host.clone()))?;
        if parsed.host_str() != Some(host.as_str())
            || parsed.username() != ""
            || parsed.password().is_some()
            || !valid_repo_component(&owner)
            || !valid_repo_component(&name)
        {
            return Err(GitHubValidationError::InvalidRepository { owner, name });
        }
        Ok(Self {
            host,
            port,
            owner,
            name,
        })
    }

    pub fn gh_selector(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}/{}/{}", self.host, self.owner, self.name),
            None => format!("{}/{}/{}", self.host, self.owner, self.name),
        }
    }

    pub fn authority(&self) -> String {
        match self.port {
            Some(port) => format!("{}:{port}", self.host),
            None => self.host.clone(),
        }
    }

    pub fn validate(&self) -> Result<(), GitHubValidationError> {
        let validated = Self::new(
            self.host.clone(),
            self.port,
            self.owner.clone(),
            self.name.clone(),
        )?;
        if validated == *self {
            Ok(())
        } else {
            Err(GitHubValidationError::InvalidRepository {
                owner: self.owner.clone(),
                name: self.name.clone(),
            })
        }
    }
}

fn valid_repo_component(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains('/')
        && !value.contains('\\')
        && !value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte == 0)
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum GitHubValidationError {
    #[error("invalid GitHub host: {0}")]
    InvalidHost(String),
    #[error("invalid GitHub repository {owner}/{name}")]
    InvalidRepository { owner: String, name: String },
    #[error("invalid pull request number")]
    InvalidPullRequestNumber,
    #[error("invalid pull request URL: {0}")]
    InvalidPullRequestUrl(String),
    #[error("invalid pull request field {field}: {reason}")]
    InvalidField { field: &'static str, reason: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct PullRequestKey {
    pub repository: GitHubRepoKey,
    pub number: u64,
}

impl PullRequestKey {
    /// Parse a full HTTPS PR URL without credentials, query, or fragment.
    ///
    /// Repository path components are decoded once; `+` stays literal. This
    /// preserves repository spelling. Domain-specific canonicalization and
    /// bounds still belong to the caller's existing target validation.
    pub fn from_url(value: &str) -> Result<Self, String> {
        let url =
            url::Url::parse(value).map_err(|_| "Expected an HTTPS pull request URL".to_owned())?;
        if url.scheme() != "https"
            || !url.username().is_empty()
            || url.password().is_some()
            || url.query().is_some()
            || url.fragment().is_some()
        {
            return Err("Use an HTTPS PR URL without credentials, query, or fragment".into());
        }
        let mut segments = url
            .path_segments()
            .ok_or("Expected a pull request path")?
            .collect::<Vec<_>>();
        if segments.last() == Some(&"") {
            segments.pop();
        }
        if segments.len() != 4 || segments[2] != "pull" {
            return Err("Expected /OWNER/REPOSITORY/pull/NUMBER".into());
        }
        if !segments[3].bytes().all(|byte| byte.is_ascii_digit()) {
            return Err("Expected a positive decimal PR number".into());
        }
        let number = segments[3]
            .parse::<u64>()
            .map_err(|_| "Expected a positive PR number".to_owned())?;
        let repository = GitHubRepoKey::new(
            url.host_str().ok_or("The PR URL needs a host")?,
            url.port(),
            pull_request_path_segment(segments[0])?,
            pull_request_path_segment(segments[1])?,
        )
        .map_err(|_| "The PR repository identity is invalid".to_owned())?;
        Self::new(repository, number).map_err(|_| "Expected a positive PR number".to_owned())
    }

    pub fn new(repository: GitHubRepoKey, number: u64) -> Result<Self, GitHubValidationError> {
        if number == 0 {
            Err(GitHubValidationError::InvalidPullRequestNumber)
        } else {
            Ok(Self { repository, number })
        }
    }

    pub fn validate(&self) -> Result<(), GitHubValidationError> {
        self.repository.validate()?;
        if self.number == 0 {
            Err(GitHubValidationError::InvalidPullRequestNumber)
        } else {
            Ok(())
        }
    }
}

// URL path decoding, without form-query rules (`+` remains a literal plus).
pub(crate) fn pull_request_path_segment(value: &str) -> Result<String, String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut offset = 0;
    while offset < bytes.len() {
        if bytes[offset] == b'%' {
            let pair = bytes
                .get(offset + 1..offset + 3)
                .ok_or("Invalid URL path escape")?;
            let hex = |b: u8| match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                b'A'..=b'F' => Some(b - b'A' + 10),
                _ => None,
            };
            decoded.push(
                hex(pair[0])
                    .zip(hex(pair[1]))
                    .map(|(a, b)| a * 16 + b)
                    .ok_or("Invalid URL path escape")?,
            );
            offset += 3;
        } else {
            decoded.push(bytes[offset]);
            offset += 1;
        }
    }
    String::from_utf8(decoded).map_err(|_| "The repository URL is not UTF-8".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strict_pr_url_keeps_full_fork_identity_and_decodes_repository_once() {
        let key = PullRequestKey::from_url(
            "https://ENTERPRISE.example:8443/Team+Name/Repo%2523Tag/pull/18446744073709551615/",
        )
        .unwrap();
        assert_eq!(key.repository.host, "enterprise.example");
        assert_eq!(key.repository.port, Some(8443));
        assert_eq!(key.repository.owner, "Team+Name");
        assert_eq!(key.repository.name, "Repo%23Tag");
        assert_eq!(key.number, u64::MAX);
        key.validate().unwrap();
        let fork = PullRequestKey::from_url(
            "https://enterprise.example:8443/Other/Repo%2523Tag/pull/18446744073709551615",
        )
        .unwrap();
        assert_ne!(key, fork);
        assert_ne!(
            key,
            PullRequestKey::from_url(
                "https://another.example:8443/Team+Name/Repo%2523Tag/pull/18446744073709551615",
            )
            .unwrap()
        );
    }

    #[test]
    fn strict_pr_url_refuses_credentials_ambiguous_paths_and_invalid_numbers() {
        for url in [
            "http://github.com/a/b/pull/1",
            "https://token@github.com/a/b/pull/1",
            "https://user:secret@github.com/a/b/pull/1",
            "https://github.com/a/b/pull/1?tab=files",
            "https://github.com/a/b/pull/1#discussion",
            "https://github.com/a/b/issues/1",
            "https://github.com/a/b/%70ull/1",
            "https://github.com/a/b/pull/1/files",
            "https://github.com/a/b/pull/1//",
            "https://github.com/a%2Fb/c/pull/1",
            "https://github.com/a/b%5Cc/pull/1",
            "https://github.com/a/b%00c/pull/1",
            "https://github.com/a/b%FF/pull/1",
            "https://github.com/a/b%GG/pull/1",
            "https://github.com/a/b/pull/0",
            "https://github.com/a/b/pull/-1",
            "https://github.com/a/b/pull/+1",
            "https://github.com/a/b/pull/1.5",
            "https://github.com/a/b/pull/18446744073709551616",
        ] {
            assert!(PullRequestKey::from_url(url).is_err(), "{url}");
        }
    }
}
