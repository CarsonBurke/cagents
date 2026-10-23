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

impl CheckReason {
    /// Human explanations are fixed product copy. Serialized reason codes
    /// remain stable and never include provider output or configuration.
    pub const fn description(self) -> &'static str {
        match self {
            Self::NotRequested => "This check was not requested",
            Self::NotChecked => "This check has not run",
            Self::InvalidRequest => "The account check request is invalid",
            Self::UnsupportedProvider => "This check supports Codex accounts",
            Self::AccountMismatch => "The selected account context could not be confirmed",
            Self::HomeUnavailable => "The selected account home is unavailable or changed",
            Self::ProviderMissing => "Codex is unavailable on the selected device",
            Self::ProbeBusy => "Another probe is using this account; try again after it finishes",
            Self::ProbeUnavailable => "The account probe could not be prepared",
            Self::ProviderStartFailed => "Codex could not start for this check",
            Self::ProviderTimeout => "Codex did not finish the check before its deadline",
            Self::ProviderProtocol => "Codex did not return a valid response for this check",
            Self::UnsupportedMethod => "This Codex version does not support the requested check",
            Self::SignedOut => "Codex needs the selected account to sign in",
            Self::NotSubscription => "Codex did not report a ChatGPT subscription login",
            Self::IdentityUnavailable => "Codex did not report an identity this check can verify",
            Self::IdentityMismatch => "The observed login does not match the expected account",
            Self::IdentityConfirmed => "The observed login matches the expected account",
            Self::PolicyReported => "Codex resolved an explicitly configured execution policy",
            Self::ImplicitPolicy => "The provider reported no explicit policy source",
            Self::PolicyUnrecognized => "Codex reported a policy this check cannot identify",
            Self::PolicyChanged => "Codex configuration changed during the check; check it again",
            Self::SandboxSetupFailed => "Codex could not prepare its sandbox",
            Self::SandboxDenied => "The execution environment denied the requested operation",
            Self::CommandFailed => "The check command did not complete successfully",
            Self::InvalidCommandReceipt => "The command did not return a verifiable check result",
            Self::WorkspaceChanged => "The selected workspace changed during the check",
            Self::WorkspaceRoundTrip => "The workspace write, read and removal were confirmed",
            Self::WorkspaceCleanupUnconfirmed => {
                "Scratch cleanup was not confirmed; inspect the scoped workspace"
            }
            Self::ProviderExecutableChanged => {
                "The Codex executable changed during the check; check it again"
            }
            Self::GitHubRepositoryAccess => "GitHub repository read access was confirmed",
            Self::GitHubRepositoryDenied => "The requested GitHub repository could not be read",
            Self::GitHubUnavailable => {
                "The GitHub CLI is unavailable in this execution environment"
            }
            Self::GitHubAuthenticationFailed => {
                "GitHub authentication was unavailable in this execution environment"
            }
            Self::GitHubNetworkFailed => "The check could not reach GitHub",
        }
    }
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
    /// The owned provider group accepted its kill signal (or was absent),
    /// and its original leader was confirmed reaped. This does not census
    /// arbitrary descendants that escaped that group. Distinct from removal
    /// of the workspace scratch file.
    pub provider_cleanup_confirmed: bool,
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
            provider_cleanup_confirmed: false,
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
    /// Bind a response to the selected request/backend and reject contradictory
    /// capability claims. This is response validation, not a future launch permit.
    pub fn matches_request(
        &self,
        request: &CheckRequest,
        host: &crate::model::HostSessionProof,
    ) -> bool {
        if self.account != request.account
            || self.requested_workspace != request.workspace
            || self.expected_email != request.expected_email
            || self.github_repository != request.github_repository
            || self.scope != CheckScope::AccountDefaultPolicy
            || self.host.as_ref() != Some(host)
            || !self.workspace.is_absolute()
            || self.authentication.status == CheckStatus::NotRequested
            || self.execution_profile_configured.status == CheckStatus::NotRequested
            || (self.execution_profile_configured.status == CheckStatus::Passed
                && self.profile.is_none())
            || (self.workspace_write.status != CheckStatus::NotRequested) != request.write
            || (self.github.status != CheckStatus::NotRequested)
                != request.github_repository.is_some()
            || (!request.write
                && (self.workspace_receipt.is_some() || self.workspace_cleanup.is_some()))
            || (request.write && self.workspace_cleanup.is_none())
            || (request.github_repository.is_none() && self.github_push_allowed.is_some())
            || (self.authentication.status != CheckStatus::Passed
                && (self.workspace_write.status == CheckStatus::Passed
                    || self.github.status == CheckStatus::Passed))
        {
            return false;
        }
        if self.authentication.status == CheckStatus::Passed
            && !self
                .observed_identity
                .as_ref()
                .and_then(|identity| identity.email.as_deref())
                .is_some_and(|email| email.trim().eq_ignore_ascii_case(&request.expected_email))
        {
            return false;
        }
        if self.workspace_write.status == CheckStatus::Passed
            && (self.workspace_cleanup != Some(super::workspace_probe::Cleanup::Removed)
                || !self
                    .workspace_receipt
                    .as_ref()
                    .is_some_and(|receipt| receipt.is_verified()))
        {
            return false;
        }
        true
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
