//! Non-secret, scoped observations of one account's default execution policy.
//! A completed check is not a permit for a different actor or future launch.
use super::{AccountId, Identity};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CheckRequest {
    pub account: AccountId,
    pub workspace: PathBuf,
    pub expected_email: String,
    pub write: bool,
    pub github_repository: Option<String>,
}

impl CheckRequest {
    pub fn validate(&self) -> Result<(), &'static str> {
        let workspace = self.workspace.as_os_str().as_encoded_bytes();
        if workspace.is_empty() || workspace.contains(&0) {
            return Err("Choose a workspace on the selected device");
        }
        let relative_home = self
            .workspace
            .to_str()
            .and_then(|path| path.strip_prefix("~/"));
        if !self.workspace.is_absolute()
            && !relative_home.is_some_and(|relative| !std::path::Path::new(relative).is_absolute())
        {
            return Err("Choose an absolute workspace on the selected device, or quoted ~/path");
        }
        let email = self.expected_email.trim();
        if email.is_empty() || email.len() > 320 || email.chars().any(char::is_control) {
            return Err("Provide the expected provider email explicitly");
        }
        if let Some(repository) = &self.github_repository {
            let Some((owner, name)) = repository.split_once('/') else {
                return Err("Use a GitHub repository in OWNER/REPO form");
            };
            if owner.is_empty()
                || owner.len() > 100
                || name.is_empty()
                || name.len() > 100
                || !owner.as_bytes()[0].is_ascii_alphanumeric()
                || !owner
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || c == b'-')
                || !name
                    .bytes()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
                || matches!(name, "." | "..")
            {
                return Err("Use a GitHub repository in OWNER/REPO form");
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    NotRequested,
    Passed,
    Failed,
    Unsupported,
    Unknown,
}

/// Fixed reasons only. Provider responses, process output and configuration
/// values must never be interpolated into a diagnostic.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckReason {
    NotRequested,
    NotChecked,
    InvalidRequest,
    UnsupportedProvider,
    AccountMismatch,
    HomeUnavailable,
    ProviderMissing,
    ProbeBusy,
    ProbeUnavailable,
    ProviderStartFailed,
    ProviderTimeout,
    ProviderProtocol,
    UnsupportedMethod,
    SignedOut,
    NotSubscription,
    IdentityUnavailable,
    IdentityMismatch,
    IdentityConfirmed,
    PolicyReported,
    ImplicitPolicy,
    PolicyUnrecognized,
    PolicyChanged,
    SandboxSetupFailed,
    SandboxDenied,
    CommandFailed,
    InvalidCommandReceipt,
    WorkspaceChanged,
    WorkspaceRoundTrip,
    WorkspaceCleanupUnconfirmed,
    ProviderExecutableChanged,
    GitHubRepositoryAccess,
    GitHubRepositoryDenied,
    GitHubUnavailable,
    GitHubAuthenticationFailed,
    GitHubNetworkFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Measurement {
    pub status: CheckStatus,
    pub reason: CheckReason,
}
impl Measurement {
    pub const fn new(status: CheckStatus, reason: CheckReason) -> Self {
        Self { status, reason }
    }
    pub const fn unknown(reason: CheckReason) -> Self {
        Self::new(CheckStatus::Unknown, reason)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckScope {
    AccountDefaultPolicy,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SandboxMode {
    ReadOnly,
    WorkspaceWrite,
    DangerFullAccess,
    ExternalSandbox,
    Unknown,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalPolicy {
    Untrusted,
    OnFailure,
    OnRequest,
    Never,
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicySummary {
    pub sandbox_mode: SandboxMode,
    pub approval_policy: ApprovalPolicy,
    pub network_access: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalIdentity {
    pub device: u64,
    pub inode: u64,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutableIdentity {
    pub path: PathBuf,
    pub device: u64,
    pub inode: u64,
    pub len: u64,
    pub modified_ms: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CheckReport {
    pub account: AccountId,
    pub requested_workspace: PathBuf,
    pub expected_email: String,
    pub github_repository: Option<String>,
    pub workspace: PathBuf,
    pub observed_at: u64,
    pub scope: CheckScope,
    pub host: Option<crate::model::HostSessionProof>,
    pub registry_revision: Option<u64>,
    pub account_home_identity: Option<PhysicalIdentity>,
    pub workspace_identity: Option<PhysicalIdentity>,
    pub provider_executable: Option<ExecutableIdentity>,
    pub authentication: Measurement,
    pub execution_profile_configured: Measurement,
    pub profile: Option<PolicySummary>,
    pub observed_identity: Option<Identity>,
    pub workspace_write: Measurement,
    pub workspace_receipt: Option<super::workspace_probe::Receipt>,
    pub workspace_cleanup: Option<super::workspace_probe::Cleanup>,
    pub github: Measurement,
    /// GitHub's reported permission, not a performed push or mutation.
    pub github_push_allowed: Option<bool>,
}
impl CheckReport {
    pub fn new(request: &CheckRequest) -> Self {
        Self {
            account: request.account.clone(),
            requested_workspace: request.workspace.clone(),
            expected_email: request.expected_email.clone(),
            github_repository: request.github_repository.clone(),
            workspace: request.workspace.clone(),
            observed_at: 0,
            scope: CheckScope::AccountDefaultPolicy,
            host: None,
            registry_revision: None,
            account_home_identity: None,
            workspace_identity: None,
            provider_executable: None,
            authentication: Measurement::unknown(CheckReason::NotChecked),
            execution_profile_configured: Measurement::unknown(CheckReason::NotChecked),
            profile: None,
            observed_identity: None,
            workspace_write: if request.write {
                Measurement::unknown(CheckReason::NotChecked)
            } else {
                Measurement::new(CheckStatus::NotRequested, CheckReason::NotRequested)
            },
            workspace_receipt: None,
            workspace_cleanup: request
                .write
                .then_some(super::workspace_probe::Cleanup::Unconfirmed),
            github: if request.github_repository.is_some() {
                Measurement::unknown(CheckReason::NotChecked)
            } else {
                Measurement::new(CheckStatus::NotRequested, CheckReason::NotRequested)
            },
            github_push_allowed: None,
        }
    }
    /// Invalidate requested measurements, preserving evidence already observed
    /// on other axes (for example a successful login before sandbox failure).
    pub fn invalidate_measurements(&mut self, reason: CheckReason) {
        if self.workspace_write.status != CheckStatus::NotRequested {
            self.workspace_write = Measurement::unknown(reason);
        }
        if self.github.status != CheckStatus::NotRequested {
            self.github = Measurement::unknown(reason);
        }
        self.github_push_allowed = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn requests_reject_option_url_and_path_injection_without_narrowing_destination_paths() {
        let mut request = CheckRequest {
            account: "codex/system".parse().unwrap(),
            workspace: "~/project".into(),
            expected_email: "selected@example.test".into(),
            write: true,
            github_repository: Some("owner/repo.name-1".into()),
        };
        assert!(request.validate().is_ok());
        for invalid in [
            "",
            "/repo",
            "owner/",
            "owner/../repo",
            "owner/..",
            "--help/repo",
            "https://github.com/o/r",
            "owner/repo?x",
        ] {
            request.github_repository = Some(invalid.into());
            assert!(request.validate().is_err(), "accepted {invalid}");
        }
        request.github_repository = None;
        for path in ["relative/workspace", "~//workspace", "~another/workspace"] {
            request.workspace = path.into();
            assert!(request.validate().is_err());
        }
        request.workspace = "~/workspace".into();
        request.expected_email = "\n".into();
        assert!(request.validate().is_err());
        let report = CheckReport::new(&request);
        assert_eq!(report.workspace_write.status, CheckStatus::Unknown);
        assert_eq!(report.github.status, CheckStatus::NotRequested);
    }
}
