//! Journaled worktree operations owned by the selected daemon.
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    ResolveProject {
        id: crate::model::ProjectId,
    },
    ResolveRepository {
        path: PathBuf,
    },
    Create {
        repository: PathBuf,
        branch: String,
        base: Option<String>,
        linear: Option<String>,
        no_setup: bool,
    },
    List {
        repository: PathBuf,
    },
    Remove {
        repository: PathBuf,
        target: String,
        force: bool,
        delete_branch: bool,
        resolved_path: Option<PathBuf>,
    },
}
impl Operation {
    pub fn mutates(&self) -> bool {
        matches!(self, Self::Create { .. } | Self::Remove { .. })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    pub path: PathBuf,
    pub branch: Option<String>,
    pub head_oid: Option<String>,
    pub primary: bool,
    pub detached: bool,
    pub locked: bool,
    pub dirty: Option<bool>,
    pub status_error: Option<String>,
    /// Missing upstream evidence is unknown, not zero.
    pub ahead: Option<u64>,
    pub behind: Option<u64>,
    pub live_sessions: Option<Vec<crate::model::TerminalId>>,
    pub session_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Setup {
    pub command: String,
    pub run_id: crate::model::OperationId,
    pub terminal: Option<crate::model::TerminalSession>,
    pub error: Option<String>,
}

/// Creation remains committed when later status, setup or publication fails.
/// Never erase these details behind a generic retryable error.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Created {
    pub operation_id: crate::model::OperationId,
    pub path: PathBuf,
    pub branch: Option<String>,
    pub worktree: Option<Entry>,
    pub warnings: Vec<String>,
    pub setup: Option<Setup>,
    pub error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Removed {
    pub path: PathBuf,
    pub branch_deleted: Option<String>,
    pub rescue_branch: Option<String>,
    pub warnings: Vec<String>,
    pub branch_error: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemovalFailure {
    pub path: PathBuf,
    pub rescue_branch: Option<String>,
    pub error: String,
    /// A timeout or missing response never proves the Git mutation rolled back.
    pub outcome_unknown: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    ResolvedRepository { repository: PathBuf },
    Created(Created),
    Listed { worktrees: Vec<Entry> },
    Removed(Removed),
    RemovalFailed(RemovalFailure),
}

#[cfg(feature = "host")]
pub(crate) mod service;
