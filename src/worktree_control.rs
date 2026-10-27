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
    SetupIntents {
        repository: PathBuf,
    },
    ReviewSetup {
        repository: PathBuf,
        path: PathBuf,
    },
    ResumeSetup {
        repository: PathBuf,
        path: PathBuf,
        creation_id: crate::model::OperationId,
        expected_run: Option<crate::model::OperationId>,
        expected_fingerprint: Option<String>,
        trust: bool,
        skip: bool,
    },
    Remove {
        repository: PathBuf,
        target: String,
        force: bool,
        delete_branch: bool,
        resolved_path: Option<PathBuf>,
        expected_fingerprint: Option<String>,
    },
    /// Fresh deletion evidence; inspecting never authorizes a later mutation.
    Inspect {
        repository: PathBuf,
        path: PathBuf,
    },
    StaleLock {
        repository: PathBuf,
        path: PathBuf,
        unlock: bool,
    },
    GetDisposableIgnoredDirectories {
        repository: PathBuf,
        path: PathBuf,
    },
    SetDisposableIgnoredDirectories {
        repository: PathBuf,
        path: PathBuf,
        directories: Vec<String>,
    },
    /// Independent branch consent, separate from discarding worktree files.
    DeleteBranch {
        repository: PathBuf,
        branch: String,
        force: bool,
        expected_oid: Option<String>,
    },
}
impl Operation {
    pub fn mutates(&self) -> bool {
        matches!(
            self,
            Self::Create { .. }
                | Self::Remove { .. }
                | Self::ResumeSetup { .. }
                | Self::StaleLock { unlock: true, .. }
                | Self::SetDisposableIgnoredDirectories { .. }
                | Self::DeleteBranch { .. }
        )
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
    /// Exact attempted actor, retained even if startup/publication is uncertain.
    pub attempted_terminal: Option<crate::model::TerminalId>,
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
pub struct CreationFailure {
    pub operation_id: crate::model::OperationId,
    pub path: PathBuf,
    pub state: String,
    pub residual: Option<serde_json::Value>,
    pub error: String,
    pub may_have_effects: bool,
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
pub struct Plan {
    pub copy: Vec<String>,
    pub commands: Vec<String>,
    pub fingerprint: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupIntent {
    pub creation_id: crate::model::OperationId,
    pub run_id: Option<crate::model::OperationId>,
    pub path: PathBuf,
    pub primary: PathBuf,
    pub retained_plan: Option<Plan>,
    pub skip: bool,
    pub copy_done: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SetupReview {
    pub intent: SetupIntent,
    pub current_plan: Option<Plan>,
    pub trusted: bool,
    pub current_error: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Reply {
    ResolvedRepository {
        repository: PathBuf,
    },
    Created(Created),
    CreationFailed(CreationFailure),
    Listed {
        worktrees: Vec<Entry>,
    },
    SetupIntents {
        intents: Vec<SetupIntent>,
        warnings: Vec<String>,
    },
    SetupReview(SetupReview),
    SetupPrepared {
        path: PathBuf,
        creation_id: crate::model::OperationId,
        run_id: crate::model::OperationId,
        skip: bool,
        setup: Option<Setup>,
        error: Option<String>,
    },
    Removed(Removed),
    RemovalFailed(RemovalFailure),
    Inspected {
        snapshot: String,
        disposable_ignored_directories: Vec<String>,
    },
    StaleLock {
        unlocked: bool,
    },
    DisposableIgnoredDirectories {
        directories: Vec<String>,
    },
    BranchDeleted {
        branch: String,
    },
    BranchDeleteFailed {
        branch: String,
        failure: crate::worktree_branch::BranchDeleteFailure,
        /// Checked Git deletion can commit before later cleanup fails.
        outcome_unknown: bool,
    },
}

#[cfg(feature = "host")]
pub(crate) mod service;
