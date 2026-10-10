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
