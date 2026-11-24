//! The provider executable the daemon finds and the outcome of its last check.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::agent::Provider;

#[cfg(feature = "host")]
mod inspection;
#[cfg(feature = "host")]
pub use super::probe::{provider_version as read_version, run_provider_command};
#[cfg(feature = "host")]
pub use inspection::is_muse_launcher;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderInstallMethod {
    Missing,
    System,
    VersionManager(String),
    Native,
    Npm {
        #[serde(with = "crate::selected_file_path::required")]
        prefix: PathBuf,
    },
    Standalone,
    Unknown,
}

impl ProviderInstallMethod {
    pub fn label(&self) -> &str {
        match self {
            Self::Missing => "not installed",
            Self::System => "managed by your system",
            Self::VersionManager(manager) => manager,
            Self::Native => "native installer",
            Self::Npm { .. } => "npm global package",
            Self::Standalone => "standalone binary",
            Self::Unknown => "unknown install method",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ProviderUpdateResult {
    Updated,
    Current,
    Skipped,
    Failed,
    Deferred,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProviderStatus {
    pub provider: Provider,
    /// The path on the user's search path, which subsequent launches resolve again.
    #[serde(with = "crate::selected_file_path")]
    pub executable: Option<PathBuf>,
    /// The current target, including symlinks in the installation layout.
    #[serde(with = "crate::selected_file_path")]
    pub resolved_executable: Option<PathBuf>,
    pub version: Option<String>,
    pub install_method: ProviderInstallMethod,
    /// Unix time in milliseconds.
    pub last_checked_at: Option<u64>,
    pub last_result: Option<ProviderUpdateResult>,
    pub error: Option<String>,
}
