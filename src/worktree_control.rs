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
    StaleLock { repository:PathBuf, path:PathBuf, unlock:bool },
    SetDisposableIgnoredDirectories { repository:PathBuf, path:PathBuf, directories:Vec<String> },
    GetDisposableIgnoredDirectories {repository:PathBuf,path:PathBuf},
    DeleteBranch { repository:PathBuf, branch:String, force:bool, expected_oid:Option<String> },
    SetupIntent {repository:PathBuf,path:PathBuf},
    ResumeSetup {repository:PathBuf,path:PathBuf,expected_run_id:Option<crate::model::OperationId>,expected_plan_fingerprint:Option<String>,trust:bool,skip:bool},
    BusySessions { repository:PathBuf, path:PathBuf },
    CloseSessions { repository:PathBuf, path:PathBuf, agents:Vec<BusyAgent>, census:Vec<BusyAgent> },
}

impl Operation {
    pub fn repository(&self) -> Option<&std::path::Path> {
        match self {
            Self::ResolveProject {..}=>None,
            Self::Create { repository, .. }
            | Self::List { repository }
            | Self::Remove { repository, .. }
            | Self::Inspect { repository, .. }
            | Self::StaleLock { repository, .. }
            | Self::SetDisposableIgnoredDirectories { repository, .. }
            | Self::GetDisposableIgnoredDirectories {repository,..}
            | Self::BusySessions {repository,..}
            | Self::CloseSessions {repository,..}
            | Self::SetupIntent {repository,..}
            | Self::ResumeSetup {repository,..}
            | Self::DeleteBranch {repository,..} => Some(repository),
        }
    }
    pub fn mutates(&self) -> bool { !matches!(self, Self::List { .. } | Self::BusySessions {..} | Self::Inspect { .. } | Self::ResolveProject { .. } | Self::StaleLock {unlock:false,..} | Self::GetDisposableIgnoredDirectories {..} | Self::SetupIntent {..}) }
}

/// Daemon-observed actor identity and close revision; the full census includes
/// idle actors while `agents` identifies the subset requiring interruption.
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
    /// Repository snapshots keep their byte-exact path schema without pulling
    /// the daemon's Git/SQLite implementation into the control CLI.
    Inspected { snapshot:String, disposable_ignored_directories:Vec<String> },
    DisposableIgnoredDirectories {directories:Vec<String>},
    BranchDeleted {branch:String},
    BranchDeleteRefused {failure:BranchDeleteFailure},
    SetupIntent {creation_id:crate::model::OperationId,primary:PathBuf,intent:String,current_plan:Option<String>,plan_error:Option<String>,trusted:bool,active:bool},
    SetupPrepared {path:PathBuf,setup:Option<Setup>,warnings:Vec<String>,run_id:crate::model::OperationId},
    BusySessions { agents:Vec<BusyAgent>, census:Vec<BusyAgent> },
    SessionsClosed { path:PathBuf },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setup {
    pub command: String,
    pub run_id: Option<crate::model::OperationId>,
    pub terminal: Option<crate::model::TerminalSession>,
    pub error: Option<String>,
}

pub use crate::worktree_branch::{BranchDeleteFailure,BranchDeleteFailureKind};
#[cfg(any(feature = "ui", feature = "host"))]
pub use crate::worktree_branch::{delete_branch,delete_branch_checked,delete_branch_checked_at};
#[cfg(feature = "host")]
pub(crate) use crate::worktree_branch::git_command;

