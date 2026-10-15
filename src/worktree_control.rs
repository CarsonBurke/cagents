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
            | Self::Inspect { repository, .. }
            | Self::SetDisposableIgnoredDirectories { repository, .. }
            | Self::DeleteBranch {repository,..} => Some(repository),
        }
    }
    pub fn mutates(&self) -> bool { !matches!(self, Self::List { .. } | Self::Inspect { .. } | Self::ResolveProject { .. }) }
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
    Removed { path: PathBuf, branch_deleted: Option<String> },
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

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum BranchDeleteFailureKind {
    CheckedOutElsewhere,
    Unmerged,
    Other,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[error("{detail}")]
pub struct BranchDeleteFailure {
    pub kind: BranchDeleteFailureKind,
    pub detail: String,
}

#[cfg(any(feature = "ui", feature = "host"))]
pub(crate) fn git_command(repository: &std::path::Path) -> crate::repository::CommandSpec {
    crate::repository::CommandSpec::new("git")
        .cwd(repository)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .args(["--no-pager", "-c", "color.ui=false"])
        .timeout(std::time::Duration::from_secs(30))
}

#[cfg(any(feature = "ui", feature = "host"))]
pub fn delete_branch(
    repository: &std::path::Path,
    branch: &str,
    force: bool,
    cancellation: &crate::repository::CancellationToken,
) -> Result<(), String> {
    delete_branch_checked(repository, branch, force, cancellation)
        .map_err(|error| error.to_string())
}

#[cfg(any(feature = "ui", feature = "host"))]
pub fn delete_branch_checked(
    repository: &std::path::Path,
    branch: &str,
    force: bool,
    cancellation: &crate::repository::CancellationToken,
) -> Result<(), BranchDeleteFailure> {
    delete_branch_checked_at(repository, branch, force, None, cancellation)
}

/// A force confirmation can bind its consent to the branch commit it showed.
/// The forced deletion uses Git's ref transaction compare-and-swap, rather than
/// a second read followed by an unconditional `branch -D`.
#[cfg(any(feature = "ui", feature = "host"))]
pub fn delete_branch_checked_at(
    repository: &std::path::Path,
    branch: &str,
    force: bool,
    expected_oid: Option<&str>,
    cancellation: &crate::repository::CancellationToken,
) -> Result<(), BranchDeleteFailure> {
    use crate::repository::{Oid, ProcessExecutor, RepositoryService, SystemProcessExecutor, ValidatedBranchName};
    let other = |detail: String| BranchDeleteFailure { kind: BranchDeleteFailureKind::Other, detail };
    let branch = ValidatedBranchName::parse(branch.trim_start_matches("refs/heads/"))
        .map_err(|error| other(error.to_string()))?;
    let service = RepositoryService::system("git");
    let checked_out = || -> Result<(), BranchDeleteFailure> {
        let records = service.list_worktrees(repository, cancellation)
            .map_err(|error| other(error.to_string()))?;
        if let Some(record) = records.iter().find(|record| record.branch.as_deref() == Some(branch.full_ref().as_str())) {
            return Err(BranchDeleteFailure { kind: BranchDeleteFailureKind::CheckedOutElsewhere,
                detail: format!("Branch {} is checked out at {}", branch.as_str(), record.path.display()) });
        }
        Ok(())
    };
    checked_out()?;
    let read_oid = |revision: &str| -> Result<Oid, BranchDeleteFailure> {
        let command = git_command(repository).args(["rev-parse", "--verify", "--end-of-options", revision]);
        let output = SystemProcessExecutor.execute(&command, cancellation)
            .and_then(|output| output.require_success(&command.program))
            .map_err(|error| other(error.reported_detail()))?;
        Oid::parse_object(String::from_utf8(output.stdout).map_err(|error| other(error.to_string()))?.trim())
            .map_err(|error| other(error.to_string()))
    };
    let full_ref = branch.full_ref();
    let observed = read_oid(&full_ref)?;
    if expected_oid.is_some_and(|expected| expected != observed.as_str()) {
        return Err(other("The branch changed after confirmation; review it again".into()));
    }
    if !force {
        let upstream_revision = format!("{}@{{upstream}}", branch.as_str());
        let upstream_command = git_command(repository)
            .args(["rev-parse", "--verify", "--quiet", "--end-of-options", &upstream_revision]);
        let upstream = SystemProcessExecutor.execute(&upstream_command, cancellation)
            .map_err(|error| other(error.reported_detail()))?;
        let target = if upstream.success {
            Oid::parse_object(String::from_utf8(upstream.stdout).map_err(|error| other(error.to_string()))?.trim())
                .map_err(|error| other(error.to_string()))?
        } else if matches!(upstream.exit_code, Some(1 | 128)) {
            read_oid("HEAD")?
        } else {
            return Err(other("Could not determine the branch's merge target; it was retained".into()));
        };
        let command = git_command(repository).args(["merge-base", "--is-ancestor", observed.as_str(), target.as_str()]);
        let output = SystemProcessExecutor.execute(&command, cancellation)
            .map_err(|error| other(error.reported_detail()))?;
        if output.exit_code == Some(1) {
            return Err(BranchDeleteFailure { kind: BranchDeleteFailureKind::Unmerged,
                detail: format!("Branch {} contains commits not merged into its upstream or HEAD", branch.as_str()) });
        }
        output.require_success(&command.program).map_err(|error| other(error.reported_detail()))?;
    }
    checked_out()?;
    let command = if force {
        git_command(repository).args(["update-ref", "--no-deref", "-d", &full_ref, observed.as_str()])
    } else {
        git_command(repository).args(["branch", "-d", "--", branch.as_str()])
    };
    SystemProcessExecutor.execute(&command, cancellation)
        .and_then(|output| output.require_success(&command.program))
        .map_err(|error| other(error.reported_detail()))?;
    if force {
        // update-ref atomically deletes the ref and its reflog. Unlike branch
        // -d, it does not remove the optional clone-local branch subsection.
        let section = format!("branch.{}", branch.as_str());
        let escaped: String = section.chars().flat_map(|character| {
            if "\\.^$|?*+()[]{}".contains(character) { vec!['\\', character] } else { vec![character] }
        }).collect();
        let pattern = format!("^{escaped}\\.");
        let query = git_command(repository).args(["config", "--local", "--get-regexp", &pattern]);
        let output = SystemProcessExecutor.execute(&query, cancellation)
            .map_err(|error| other(format!("Branch was deleted, but its configuration was retained: {}", error.reported_detail())))?;
        if output.success {
            // Preserve a same-named branch recreated concurrently by another Git
            // caller. The ref deletion itself is already committed at this point.
            let check = git_command(repository).args(["show-ref", "--verify", "--quiet", &full_ref]);
            let current = SystemProcessExecutor.execute(&check, cancellation)
                .map_err(|error| other(format!("Branch was deleted, but its configuration was retained: {}", error.reported_detail())))?;
            if current.exit_code == Some(1) {
                let remove = git_command(repository).args(["config", "--local", "--remove-section", &section]);
                SystemProcessExecutor.execute(&remove, cancellation)
                    .and_then(|output| output.require_success(&remove.program))
                    .map_err(|error| other(format!("Branch was deleted; branch configuration cleanup could not be confirmed: {}", error.reported_detail())))?;
            } else {
                return Err(other("Branch was deleted, then a branch with the same name appeared; its configuration was preserved".into()));
            }
        } else if output.exit_code != Some(1) {
            return Err(other("Branch was deleted, but its configuration could not be inspected and was retained".into()));
        }
    }
    Ok(())
}

#[cfg(feature = "host")]
pub(crate) mod service;
#[cfg(all(test, any(feature = "ui", feature = "host")))]
mod branch_delete_tests {
    use super::*;
    use crate::repository::CancellationToken;
    use std::{fs, path::Path, process::Command};

    fn git(path:&Path,args:&[&str])->String {
        let output=Command::new("git").current_dir(path)
            .env("GIT_CONFIG_GLOBAL","/dev/null").env("GIT_CONFIG_SYSTEM","/dev/null")
            .args(["-c","user.name=Branch Contract","-c","user.email=test@example.invalid","-c","commit.gpgsign=false","-c","core.hooksPath=/dev/null"])
            .args(args).output().unwrap();
        assert!(output.status.success(),"{args:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap().trim().into()
    }

    #[test]
    fn branch_delete_distinguishes_checkout_unmerged_and_stale_force_consent() {
        let directory=tempfile::tempdir().unwrap();let path=directory.path();
        git(path,&["init","-q","-b","main"]);
        fs::write(path.join("file"),"base").unwrap();git(path,&["add","file"]);git(path,&["commit","-qm","base"]);
        let cancel=CancellationToken::new();
        let checked=delete_branch_checked(path,"main",true,&cancel).unwrap_err();
        assert_eq!(checked.kind,BranchDeleteFailureKind::CheckedOutElsewhere);
        git(path,&["branch","merged"]);
        let base=git(path,&["rev-parse","refs/heads/merged"]);
        delete_branch_checked_at(path,"merged",false,Some(&base),&cancel).unwrap();
        assert!(git(path,&["branch","--list","merged"]).is_empty());
        git(path,&["checkout","-qb","unmerged"]);
        fs::write(path.join("file"),"unmerged").unwrap();git(path,&["commit","-qam","new"]);
        let first=git(path,&["rev-parse","HEAD"]);git(path,&["checkout","-q","main"]);
        let error=delete_branch_checked_at(path,"unmerged",false,Some(&first),&cancel).unwrap_err();
        assert_eq!(error.kind,BranchDeleteFailureKind::Unmerged);
        git(path,&["checkout","-q","unmerged"]);fs::write(path.join("file"),"advanced").unwrap();git(path,&["commit","-qam","advance"]);
        let latest=git(path,&["rev-parse","HEAD"]);git(path,&["checkout","-q","main"]);
        let error=delete_branch_checked_at(path,"unmerged",true,Some(&first),&cancel).unwrap_err();
        assert_eq!(error.kind,BranchDeleteFailureKind::Other);
        assert_eq!(git(path,&["rev-parse","refs/heads/unmerged"]),latest);
        git(path,&["config","--local","branch.unmerged.description","discard only after confirmed"]);
        git(path,&["pack-refs","--all"]);
        delete_branch_checked_at(path,"unmerged",true,Some(&latest),&cancel).unwrap();
        assert!(git(path,&["branch","--list","unmerged"]).is_empty());
        let config=Command::new("git").current_dir(path).args(["config","--local","--get","branch.unmerged.description"]).output().unwrap();
        assert_eq!(config.status.code(),Some(1));
    }
}
