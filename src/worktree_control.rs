//! Typed worktree control shared by the control CLI and its owning daemon.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    Create {
        repository: PathBuf,
        branch: String,
        base: Option<String>,
        linear: Option<String>,
        no_setup: bool,
    },
    List { repository: PathBuf },
    Remove {
        repository: PathBuf,
        target: String,
        force: bool,
        delete_branch: bool,
    },
}

impl Operation {
    pub fn repository(&self) -> &std::path::Path {
        match self {
            Self::Create { repository, .. }
            | Self::List { repository }
            | Self::Remove { repository, .. } => repository,
        }
    }
    pub fn mutates(&self) -> bool { !matches!(self, Self::List { .. }) }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub primary: bool,
    pub detached: bool,
    pub locked: bool,
    pub dirty: bool,
    /// None means that Git has no upstream comparison, never zero.
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
    pub live_sessions: Vec<crate::model::TerminalId>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    Created { worktree: Entry, warnings: Vec<String> },
    Listed { worktrees: Vec<Entry> },
    Removed { path: PathBuf, branch_deleted: Option<String> },
}

#[cfg(any(feature = "ui", feature = "host"))]
pub fn delete_branch(
    repository: &std::path::Path,
    branch: &str,
    force: bool,
    cancellation: &crate::repository::CancellationToken,
) -> Result<(), String> {
    use crate::repository::{CommandSpec, ProcessExecutor, RepositoryService, SystemProcessExecutor, ValidatedBranchName};
    let branch = ValidatedBranchName::parse(branch.trim_start_matches("refs/heads/"))
        .map_err(|error| error.to_string())?;
    let records = RepositoryService::system("git").list_worktrees(repository, cancellation)
        .map_err(|error| error.to_string())?;
    if let Some(record) = records.iter().find(|record| record.branch.as_deref() == Some(branch.full_ref().as_str())) {
        return Err(format!("Branch {} is checked out at {}", branch.as_str(), record.path.display()));
    }
    let command = CommandSpec::new("git").cwd(repository)
        .args(["branch", if force { "-D" } else { "-d" }, "--"])
        .arg(branch.as_str()).timeout(std::time::Duration::from_secs(30));
    SystemProcessExecutor.execute(&command, cancellation)
        .and_then(|output| output.require_success(&command.program))
        .map(|_| ()).map_err(|error| error.reported_detail())
}

#[cfg(feature = "host")]
pub(crate) mod service;
