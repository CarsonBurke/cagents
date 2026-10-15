//! Daemon-owned removal reads and effects; creation/setup keeps its existing main implementation.
use super::*;
use crate::repository::{
    CancellationToken, ProcessExecutor, RepositoryService, SystemProcessExecutor, TrackingState,
    WorktreeRecord,
};
use std::path::Path;
type Service = RepositoryService<SystemProcessExecutor>;
pub(crate) struct Execution {
    pub(crate) reply: Reply,
}

fn entry(
    service: &Service,
    record: &WorktreeRecord,
    primary: bool,
    cancellation: &CancellationToken,
) -> Result<Entry, String> {
    let status = service
        .status(&record.path, cancellation)
        .map_err(|error| error.to_string())?;
    let (ahead, behind) = match status.tracking {
        TrackingState::Compared { ahead, behind, .. } => (Some(ahead), Some(behind)),
        _ => (None, None),
    };
    Ok(Entry {
        path: record.path.clone(),
        branch: record.branch.as_deref().map(|branch| {
            branch
                .strip_prefix("refs/heads/")
                .unwrap_or(branch)
                .to_owned()
        }),
        head_oid: record.head_oid.clone(),
        primary,
        detached: record.detached,
        locked: record.lock.locked,
        dirty: status.dirty.is_dirty(),
        ahead,
        behind,
        live_sessions: Vec::new(),
    })
}

pub(crate) fn execute(
    operation: Operation,
    mut live_sessions: impl FnMut(&Path) -> Result<Vec<crate::model::TerminalId>, String>,
) -> Result<Execution, String> {
    let cancellation = CancellationToken::new();
    let service = RepositoryService::system("git");
    let reply = match operation {
        Operation::List { repository } => {
            let records = service
                .list_worktrees(&repository, &cancellation)
                .map_err(|error| error.to_string())?;
            let worktrees = records
                .iter()
                .enumerate()
                .filter(|(_, record)| !record.bare)
                .map(|(index, record)| {
                    let mut entry = entry(&service, record, index == 0, &cancellation)?;
                    entry.live_sessions = live_sessions(&record.path)?;
                    Ok(entry)
                })
                .collect::<Result<Vec<_>, String>>()?;
            Ok(Reply::Listed { worktrees })
        }
        operation @ Operation::Remove { .. } => {
            let store = journal_store()?;
            remove_with_executor(
                operation,
                &service,
                &SystemProcessExecutor,
                &store,
                &mut live_sessions,
                |_| Ok(()),
                || {},
            )
        }
        Operation::StaleLock {
            repository,
            path,
            unlock,
        } => {
            let store = journal_store()?;
            same_repository(&service, &repository, &path, &cancellation)?;
            if unlock {
                service.unlock_stale_worktree(&store, &repository, &path, &cancellation)?;
            } else {
                service.stale_worktree_lock(&store, &repository, &path, &cancellation)?;
            }
            Ok(Reply::StaleLock { unlocked: unlock })
        }

        Operation::DeleteBranch {
            repository,
            branch,
            force,
            expected_oid,
            continuation: _,
        } => {
            if force && expected_oid.is_none() {
                Ok(Reply::BranchDeleteRefused {
                    continuation: None,
                    failure: BranchDeleteFailure {
                        kind: BranchDeleteFailureKind::Other,
                        detail: "Force deletion requires the branch commit shown in confirmation"
                            .into(),
                    },
                })
            } else {
                Ok(
                    match delete_branch_checked_at(
                        &repository,
                        &branch,
                        force,
                        expected_oid.as_deref(),
                        &cancellation,
                    ) {
                        Ok(()) => Reply::BranchDeleted { branch },
                        Err(failure) => Reply::BranchDeleteRefused {
                            failure,
                            continuation: None,
                        },
                    },
                )
            }
        }
        Operation::Inspect { repository, path } => {
            inspect(&service, &repository, &path, &cancellation)
        }
        Operation::SetDisposableIgnoredDirectories {
            repository,
            path,
            directories,
        } => {
            same_repository(&service, &repository, &path, &cancellation)?;
            service
                .set_disposable_ignored_directories(&path, &directories, &cancellation)
                .map_err(|error| error.to_string())?;
            Ok(Reply::DisposableIgnoredDirectories {
                directories: service
                    .disposable_ignored_directories(&path, &cancellation)
                    .map_err(|error| error.to_string())?,
            })
        }
        Operation::GetDisposableIgnoredDirectories { repository, path } => {
            same_repository(&service, &repository, &path, &cancellation)?;
            Ok(Reply::DisposableIgnoredDirectories {
                directories: service
                    .disposable_ignored_directories(&path, &cancellation)
                    .map_err(|error| error.to_string())?,
            })
        }
        Operation::BusySessions { .. }
        | Operation::CloseSessions { .. }
        | Operation::StopSessions { .. }
        | Operation::InspectForSessionStop { .. } => {
            Err("Session census and inspected stopping require the owning daemon".into())
        }
    }?;
    Ok(Execution { reply })
}

fn same_repository(
    service: &Service,
    repository: &Path,
    path: &Path,
    cancel: &CancellationToken,
) -> Result<(), String> {
    let common = service
        .common_dir(repository, cancel)
        .map_err(|error| error.to_string())?;
    if service
        .common_dir(path, cancel)
        .map_err(|error| error.to_string())?
        != common
    {
        return Err("The selected worktree belongs to another repository".into());
    }
    Ok(())
}
fn inspect(
    service: &Service,
    repository: &Path,
    path: &Path,
    cancel: &CancellationToken,
) -> Result<Reply, String> {
    same_repository(service, repository, path, cancel)?;
    let snapshot = service
        .worktree_safety(path, cancel)
        .map_err(|error| error.to_string())?;
    Ok(Reply::Inspected {
        stop_authority: None,
        snapshot: serde_json::to_string(&snapshot).map_err(|error| error.to_string())?,
        disposable_ignored_directories: service
            .disposable_ignored_directories(path, cancel)
            .map_err(|error| error.to_string())?,
    })
}

fn journal_store() -> Result<std::sync::Arc<crate::persistence::StateStore>, String> {
    let directories = crate::instance::project_dirs().ok_or("Data directory is unavailable")?;
    crate::persistence::StateStore::open(
        directories.data_dir().join("worktree-operations.sqlite3"),
        crate::daemon::read_boot_id().map_err(|error| error.to_string())?,
    )
    .map(std::sync::Arc::new)
    .map_err(|error| error.to_string())
}

/// The inspected route uses the same removal semantics and an executor holding
/// the original directories. It does not replay unrelated creation journals.
pub(crate) fn execute_inspected_remove<E: ProcessExecutor + Clone + 'static>(
    operation: Operation,
    executor: E,
    mut live_sessions: impl FnMut(&Path) -> Result<Vec<crate::model::TerminalId>, String>,
    before_effect: impl Fn(&Path) -> Result<(), String>,
    after_effect: impl Fn(),
) -> Result<Execution, String> {
    let service = RepositoryService::new("git", std::sync::Arc::new(executor.clone()));
    let store = journal_store()?;
    let reply = remove_with_executor(
        operation,
        &service,
        &executor,
        &store,
        &mut live_sessions,
        before_effect,
        after_effect,
    )?;
    Ok(Execution { reply })
}
fn remove_with_executor<E: ProcessExecutor + Clone + 'static>(
    operation: Operation,
    service: &RepositoryService<E>,
    executor: &E,
    store: &std::sync::Arc<crate::persistence::StateStore>,
    live_sessions: &mut impl FnMut(&Path) -> Result<Vec<crate::model::TerminalId>, String>,
    before_effect: impl Fn(&Path) -> Result<(), String>,
    after_effect: impl Fn(),
) -> Result<Reply, String> {
    let Operation::Remove {
        repository,
        target,
        force,
        delete_branch: delete,
        expected_fingerprint,
        resolved_path,
        continuation: _,
    } = operation
    else {
        return Err("Expected removal".into());
    };
    let cancellation = CancellationToken::new();

    let records = service
        .list_worktrees(&repository, &cancellation)
        .map_err(|error| error.to_string())?;
    let full_branch = format!("refs/heads/{target}");
    let candidate = if let Some(path) = resolved_path {
        path
    } else if Path::new(&target).is_absolute() {
        PathBuf::from(&target)
    } else {
        records
            .first()
            .map(|record| record.path.join(&target))
            .ok_or("Repository has no primary checkout")?
    };
    let canonical = std::fs::canonicalize(&candidate).ok();
    let matches = records
        .iter()
        .enumerate()
        .filter(|(_, record)| {
            record.branch.as_deref() == Some(&full_branch)
                || canonical.as_ref() == Some(&record.path)
        })
        .collect::<Vec<_>>();
    let (index, record) = match matches.as_slice() {
        [record] => *record,
        [] => return Err("No matching worktree; use an exact branch or worktree path".into()),
        _ => return Err(
            "The target matches different branch and path worktrees; use the exact absolute path"
                .into(),
        ),
    };
    if index == 0 || record.bare {
        return Err("The primary worktree cannot be removed".into());
    }
    if record.lock.locked {
        return Err("The worktree is locked; unlock it before removing it".into());
    }
    let sessions = live_sessions(&record.path)?;
    if !sessions.is_empty() {
        return Err(format!(
            "{} live session(s) use this worktree; close them before removing it",
            sessions.len()
        ));
    }
    let _directory=crate::worktree_cleanup::removal_protection(&record.path)
                .map_err(|error|format!("Worktree is open in a window/card, editor or setup preparation; close its scopes before removal: {error}"))?;
    let safety = service
        .worktree_safety(&record.path, &cancellation)
        .map_err(|error| error.to_string())?;
    if expected_fingerprint
        .as_ref()
        .is_some_and(|fingerprint| fingerprint != &safety.fingerprint)
    {
        return Err("The worktree changed after confirmation; review its safety again".into());
    }
    if !force
        && (safety.files_to_discard() > 0
            || (expected_fingerprint.is_none() && safety.requires_confirmation()))
    {
        return Err(format!(
            "Worktree contains {} modified, {} untracked, {} notable ignored file(s), {} unpushed and {} detached-only commit(s); pass --force to confirm discarding this work",
            safety.modified,
            safety.untracked,
            safety.notable_ignored.len(),
            safety.unpushed_commits,
            safety.detached_only_commits
        ));
    }
    before_effect(&record.path)?;
    if safety.detached_only_commits > 0 && (force || expected_fingerprint.is_some()) {
        service
            .rescue_detached_worktree(&record.path, &safety, &cancellation)
            .map_err(|error| error.to_string())?;
    }
    let removed = crate::worktree_cleanup::RemovedWorktree {
        common_dir: service
            .common_dir(&repository, &cancellation)
            .map_err(|error| error.to_string())?,
        admin_dir: service
            .git_dir(&record.path, &cancellation)
            .map_err(|error| error.to_string())?,
        path: record.path.clone(),
        branch: record.branch.clone(),
    };
    // Metadata candidates are fixed before the effect. New creations that
    // reuse these names before the reply can never enter this cleanup set.
    let retirements =
        crate::repository::capture_worktree_creation_retirements(store.clone(), &removed);
    let command = super::git_command(&repository).args(["worktree", "remove"]);
    let command = if force && safety.files_to_discard() > 0 {
        command.arg("--force")
    } else {
        command
    };
    let command = command
        .arg("--")
        .arg(&record.path)
        .timeout(std::time::Duration::from_secs(30));
    before_effect(&record.path)?;
    executor
        .execute(&command, &cancellation)
        .and_then(|output| output.require_success(&command.program))
        .map_err(|error| error.reported_detail())?;
    // The checkout/admin names cease to be validation operands at
    // this point, before receipt cleanup or response construction.
    after_effect();
    let mut warnings = Vec::new();
    if let Err(failures) = retirements
        .map_err(|error| vec![error])
        .and_then(|capture| capture.finish(&removed))
    {
        warnings.push(format!(
            "Worktree was removed, but its creation receipt could not be retired: {}",
            failures
                .into_iter()
                .map(|error| error.to_string())
                .collect::<Vec<_>>()
                .join("; ")
        ));
    }
    let mut branch_error = None;
    let branch_deleted = if delete {
        if let Some(branch) = &record.branch {
            let branch = branch.strip_prefix("refs/heads/").unwrap_or(branch);
            match crate::worktree_branch::delete_branch_checked_with(
                &repository,
                branch,
                false,
                None,
                &cancellation,
                executor,
            ) {
                Ok(()) => Some(branch.to_owned()),
                Err(error) => {
                    branch_error = Some(format!(
                        "Worktree was removed, but its branch was retained: {error}"
                    ));
                    None
                }
            }
        } else {
            None
        }
    } else {
        None
    };
    Ok(Reply::Removed {
        path: record.path.clone(),
        branch_deleted,
        warnings,
        branch_error,
        continuation: None,
    })
}
