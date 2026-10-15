//! Typed worktree control shared by the control CLI and its owning daemon.
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    ResolveProject { id:crate::model::ProjectId },
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
        expected_fingerprint: Option<String>,
    },
    Inspect { repository:PathBuf, path:PathBuf },
    BusySessions { repository:PathBuf, path:PathBuf },
    CloseSessions { repository:PathBuf, path:PathBuf, agents:Vec<BusyAgent> },
    StaleLock { repository:PathBuf, path:PathBuf, unlock:bool },
    SetDisposableIgnoredDirectories { repository:PathBuf, path:PathBuf, directories:Vec<String> },
    DeleteBranch { repository:PathBuf, branch:String, force:bool, expected_oid:Option<String> },
}

impl Operation {
    pub fn repository(&self) -> Option<&std::path::Path> {
        match self {
            Self::ResolveProject {..}=>None,
            Self::Create { repository, .. }
            | Self::List { repository }
            | Self::Remove { repository, .. }
            | Self::BusySessions { repository, .. }
            | Self::CloseSessions { repository, .. }
            | Self::Inspect { repository, .. }
            | Self::StaleLock { repository, .. }
            | Self::SetDisposableIgnoredDirectories { repository, .. }
            | Self::DeleteBranch {repository,..} => Some(repository),
        }
    }
    pub fn mutates(&self) -> bool { !matches!(self, Self::List { .. } | Self::BusySessions { .. } | Self::Inspect { .. } | Self::ResolveProject { .. } | Self::StaleLock {unlock:false,..}) }
}

/// A daemon-observed active agent, including detached and hidden sessions.
/// Native turn identity prevents consent for an earlier turn authorizing a
/// different observed turn in the same session.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct BusyAgent {
    pub session: crate::model::TerminalId,
    pub instance: uuid::Uuid,
    pub close_revision: Option<u64>,
    pub turn_generation: Option<u64>,
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
    ResolvedProject {repository:PathBuf},
    Created { worktree: Entry, warnings: Vec<String>, setup: Option<Setup> },
    Listed { worktrees: Vec<Entry> },
    Removed { path: PathBuf, branch_deleted: Option<String>, warnings:Vec<String>, branch_error:Option<String> },
    StaleLock {unlocked:bool},
    BusySessions { agents:Vec<BusyAgent> },
    SessionsClosed { path:PathBuf },
    /// Repository snapshots keep their byte-exact path schema without pulling
    /// the daemon's Git/SQLite implementation into the control CLI.
    Inspected { snapshot:String, disposable_ignored_directories:Vec<String> },
    BranchDeleted {branch:String},
    BranchDeleteRefused {failure:BranchDeleteFailure},
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setup {
    pub command: String,
    pub terminal: Option<crate::model::TerminalSession>,
    pub error: Option<String>,
}

pub use crate::worktree_branch::{BranchDeleteFailure,BranchDeleteFailureKind};
#[cfg(any(feature = "ui", feature = "host"))]
pub use crate::worktree_branch::{delete_branch,delete_branch_checked,delete_branch_checked_at};
#[cfg(feature = "host")]
pub(crate) use crate::worktree_branch::git_command;

