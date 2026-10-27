//! What removing a linked worktree would lose, decided from its content.
//!
//! Removing a worktree keeps its branch, but that alone protects nothing that
//! is not committed. An inspection works on a copy of the worktree's index and
//! writes every object to a quarantine, so it never changes the repository:
//!
//! - `I` is the index as staged.
//! - `W` adds tracked changes (`add -u`).
//! - `F` adds untracked, non-ignored files (`add -A`).
//!
//! The commits are landed in a target when HEAD is its ancestor, when merging
//! HEAD into it changes nothing (squash and rebase merges), or when HEAD is the
//! exact head of a pull request GitHub reports merged. The uncommitted changes
//! are landed when `F` is HEAD's tree, or when applying the HEAD-to-`F` diff to
//! the target changes nothing. A custom merge driver voids a merge proof.
//!
//! Ignored files are always deleted with the worktree; an inspection lists
//! them, with ignored directories collapsed. Its token binds the content it
//! judged, so the daemon can refuse a removal when anything changed after the
//! user saw the plan; changes inside a collapsed ignored directory are not
//! bound, nor are the contents of nested repositories. Removal also drops the
//! worktree's admin directory: its HEAD reflog, so commits reachable only from
//! it, such as those a `reset --hard` left behind, are not judged; and its
//! `config.worktree` and sparse-checkout patterns.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// What to inspect, and the refs whose content counts as landed besides the
/// repository's default branch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Request {
    /// The worktree's canonical top-level directory.
    pub path: PathBuf,
    /// Further landing targets, such as a merged pull request's base, as full
    /// ref names.
    pub targets: Vec<String>,
    /// The head commit of a pull request GitHub reports merged into one of
    /// `targets`.
    pub merged_head: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Inspection {
    pub path: PathBuf,
    /// The checked-out branch's short name; `None` when detached.
    pub branch: Option<String>,
    pub head: Option<String>,
    pub verdict: Verdict,
    /// The target that holds both the commits and the uncommitted changes.
    pub landed_in: Option<String>,
    /// Paths whose staged content differs from HEAD.
    pub staged: u32,
    /// Tracked paths whose working content differs from the index.
    pub modified: u32,
    pub untracked: u32,
    pub untracked_bytes: u64,
    /// The first files a clean filter, `ident` or a working-tree encoding
    /// changes on the way into Git, such as notebooks under nbstripout. What
    /// Git stores does not give them back, so no snapshot keeps them. When
    /// such files are too large to compare, all of them are listed.
    pub filtered: Vec<String>,
    /// The first repositories below the top level, ignored directories
    /// included, such as a tool's cached clone. Removal deletes them whole.
    pub nested: Vec<String>,
    /// The first ignored entries removal deletes, directories collapsed.
    pub ignored: Vec<IgnoredEntry>,
    pub ignored_total: u32,
    /// Binds the inspected content. Absent for a blocked worktree, which is
    /// never removed.
    pub token: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct IgnoredEntry {
    pub path: String,
    pub directory: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Verdict {
    /// Everything is in a landing target: remove the worktree, and the branch
    /// may go too.
    Landed,
    /// Nothing uncommitted would be lost and HEAD stays reachable from a
    /// shared ref: remove the worktree, keep the branch.
    CommitsKept,
    /// Removal loses work unless it is snapshotted or explicitly discarded.
    Unsaved {
        /// Staged, uncommitted or untracked changes are not in any target,
        /// or there are [`Inspection::filtered`] files or
        /// [`Inspection::nested`] repositories.
        uncommitted: bool,
        /// No shared ref reaches HEAD, as for a detached HEAD's own commits.
        unreachable: bool,
        /// Some of it is filtered content or nested repositories, which a
        /// snapshot cannot keep: only an explicit discard permits removal.
        beyond_snapshot: bool,
    },
    /// Removal is refused.
    Blocked(Blocker),
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
pub enum Blocker {
    #[error("Git {version} is older than 2.40")]
    GitTooOld { version: String },
    #[error("the repository's reference storage is unsupported")]
    RefStorage,
    #[error("the worktree is not registered with its repository")]
    NotRegistered,
    #[error("the primary worktree is never removed")]
    Primary,
    #[error("the worktree is bare")]
    Bare,
    #[error("the worktree is locked")]
    Locked,
    #[error("the worktree is prunable")]
    Prunable,
    #[error("HEAD has no commit")]
    UnbornHead,
    #[error("a {0} is in progress")]
    Operation(Operation),
    #[error("the index has unmerged paths")]
    Unmerged,
    #[error("the worktree has its own ref {0}")]
    WorktreeRefs(String),
    #[error("submodule directory {0} has files Git does not track")]
    Submodule(String),
    #[error("worktree {0} is registered inside this one")]
    NestedWorktree(String),
    #[error("{0} is present but marked skip-worktree")]
    SkipWorktree(String),
    #[error("{0} cannot be read or changed, so removal could not empty it")]
    Unremovable(String),
    #[error("{0} is another file system or subvolume, which removal would empty")]
    MountPoint(String),
    #[error("untracked files exceed {bytes} bytes")]
    TooLarge { bytes: u64 },
    #[error("the worktree holds more than {limit} files")]
    TooManyFiles { limit: u64 },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Operation {
    Merge,
    Rebase,
    CherryPick,
    Revert,
    Bisect,
    Sequencer,
}

impl std::fmt::Display for Operation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Merge => "merge",
            Self::Rebase => "rebase",
            Self::CherryPick => "cherry-pick",
            Self::Revert => "revert",
            Self::Bisect => "bisect",
            Self::Sequencer => "sequence of picks",
        })
    }
}

/// A stash-shaped commit holding a worktree's uncommitted content: its tree is
/// `W`, and its parents are HEAD, the index and, when there are any, the
/// untracked files. `git stash apply --index <refname>` restores it. A Git
/// LFS file is kept as its pointer; `git lfs prune` may delete an object only
/// a snapshot refers to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub refname: String,
    pub oid: String,
}

/// Where snapshots live. Shared by every worktree, unlike `refs/worktree/`.
pub const SNAPSHOT_REFS: &str = "refs/cagents/snapshots/";

#[derive(Debug, thiserror::Error)]
pub enum ContentError {
    #[error("{0} is not a canonical absolute path")]
    Path(PathBuf),
    #[error("{0} is not the top level of a worktree")]
    NotTopLevel(PathBuf),
    #[error("{0}")]
    Git(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("removal is refused: {0}")]
    Blocked(Blocker),
    #[error("the worktree changed since it was inspected")]
    Changed,
    #[error("the inspection was cancelled")]
    Cancelled,
}

#[cfg(any(feature = "ui", feature = "host"))]
mod analysis;
#[cfg(any(feature = "ui", feature = "host"))]
pub use analysis::{inspect, snapshot};
#[cfg(all(test, any(feature = "ui", feature = "host")))]
mod tests;
