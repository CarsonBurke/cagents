//! CLI removal preserves the current per-target host and Git/directory fences.
use super::*;
use crate::{
    fs_safety,
    process::ProcessExecutor,
    worktree_cleanup::{FencedTarget, WindowPolicy, remove_fenced},
    worktree_content::{self, Inspection, Verdict},
};
use std::os::unix::fs::MetadataExt;

pub(crate) struct PreparedRemoval {
    target: FencedTarget,
    common: fs_safety::Root,
    admin: fs_safety::Root,
    repository_root: fs_safety::Root,
    request: worktree_content::Request,
    inspection: Inspection,
    unpushed: u64,
    detached_only: u64,
    force: bool,
    delete_branch: bool,
    selected: super::inspection::Selection,
    policy: Vec<String>,
    notable: Vec<crate::repository::RepoPath>,
    fingerprint: String,
    confirmed: bool,
}

impl PreparedRemoval {
    pub(crate) fn path(&self) -> &Path {
        &self.target.path
    }

    #[cfg(test)]
    pub(super) fn prepare<E: ProcessExecutor>(
        service: &Service<E>,
        repository: PathBuf,
        target: String,
        resolved_path: Option<PathBuf>,
        force: bool,
        delete_branch: bool,
        cancellation: &CancellationToken,
    ) -> Result<Self, String> {
        Self::prepare_confirmed(
            service,
            repository,
            target,
            resolved_path,
            force,
            delete_branch,
            None,
            cancellation,
        )
    }

    pub(super) fn prepare_confirmed<E: ProcessExecutor>(
        service: &Service<E>,
        repository: PathBuf,
        target: String,
        resolved_path: Option<PathBuf>,
        force: bool,
        delete_branch: bool,
        expected_fingerprint: Option<&str>,
        cancellation: &CancellationToken,
    ) -> Result<Self, String> {
        let repository = repository.canonicalize().map_err(|e| e.to_string())?;
        let repository_root = fs_safety::Root::open(&repository).map_err(|e| e.to_string())?;
        let records = service
            .list_worktrees(&repository, cancellation)
            .map_err(|e| e.to_string())?;
        let primary = service
            .managed_primary_checkout(&repository, cancellation)
            .map_err(|error| error.to_string())?;
        if primary.as_ref().is_some_and(|primary| {
            resolved_path.as_ref() == Some(primary)
                || Path::new(&target).canonicalize().ok().as_ref() == Some(primary)
        }) {
            return Err("The primary checkout cannot be removed".into());
        }
        let candidate = resolved_path.or_else(|| {
            if Path::new(&target).is_absolute() {
                Some(PathBuf::from(&target))
            } else {
                primary.as_ref().map(|primary| primary.join(&target))
            }
        });
        let canonical = candidate.as_ref().and_then(|path| path.canonicalize().ok());
        let full_branch = format!("refs/heads/{target}");
        let matching = records
            .iter()
            .enumerate()
            .filter(|(_, record)| {
                record.branch.as_deref() == Some(full_branch.as_str())
                    || canonical.as_ref() == Some(&record.path)
            })
            .collect::<Vec<_>>();
        let (index, record) = match matching.as_slice() {
            [one] => *one,
            [] => {
                return Err(
                    "No matching worktree; use an exact branch or absolute worktree path".into(),
                );
            }
            _ => return Err(
                "The target names different branch and path worktrees; use the exact absolute path"
                    .into(),
            ),
        };
        if index == 0 || record.bare {
            return Err("The primary checkout cannot be removed".into());
        }
        if record.lock.locked || record.prunable_reason.is_some() {
            return Err("Locked or unavailable worktrees are preserved".into());
        }
        if record.path.to_str().is_none() {
            return Err("The selected worktree path is not UTF-8; refusing a lossy target".into());
        }
        let target = FencedTarget::capture(&repository, &record.path, short_branch(record))?;
        let common = fs_safety::Root::open(&target.common_dir).map_err(|e| e.to_string())?;
        let admin = fs_safety::Root::open(&target.admin_dir).map_err(|error| error.to_string())?;
        let selected = super::inspection::Selection::capture(
            service,
            &repository,
            &record.path,
            cancellation,
        )?;
        crate::repository::worktree_setup_removal_is_quiescent(&admin)
            .map_err(|error| error.to_string())?;
        let request = worktree_content::Request {
            path: record.path.clone(),
            targets: Vec::new(),
            merged_head: None,
        };
        let inspection =
            worktree_content::inspect(&request, cancellation).map_err(|e| e.to_string())?;
        let policy = selected.policy(cancellation)?;
        let notable = selected.notable(&policy, cancellation)?;
        let unpushed = selected.commit_count(false, cancellation)?;
        let detached_only = if record.detached {
            selected.commit_count(true, cancellation)?
        } else {
            0
        };
        let fingerprint =
            selected.fingerprint(&inspection, &policy, &notable, unpushed, detached_only)?;
        if expected_fingerprint.is_some_and(|expected| expected != fingerprint) {
            return Err(
                "The worktree or its deletion policy changed after confirmation; review it again"
                    .into(),
            );
        }
        selected.verify(cancellation)?;
        permits(
            &inspection,
            notable.len(),
            unpushed,
            detached_only,
            force,
            expected_fingerprint.is_some(),
        )?;
        Ok(Self {
            target,
            common,
            admin,
            repository_root,
            request,
            inspection,
            unpushed,
            detached_only,
            force,
            delete_branch,
            selected,
            policy,
            notable,
            fingerprint,
            confirmed: expected_fingerprint.is_some(),
        })
    }

    /// Caller has installed the daemon's existing worktree_removal::Reservation
    /// for this exact path. Both callbacks run outside HostState: admission is
    /// checked after the window census; final_check also proves no process can
    /// still write the directory immediately before Git removes it.
    pub(crate) fn remove(
        self,
        store: &StateStore,
        admit: impl FnOnce(&[crate::instance::VerifiedWindow]) -> Result<(), String>,
        final_check: impl FnOnce() -> Result<(), String>,
    ) -> Result<Removed, RemovalFailure> {
        self.remove_with_post_git_checkpoint(store, admit, final_check, || {})
    }

    pub(super) fn remove_with_post_git_checkpoint(
        self,
        store: &StateStore,
        admit: impl FnOnce(&[crate::instance::VerifiedWindow]) -> Result<(), String>,
        final_check: impl FnOnce() -> Result<(), String>,
        post_git: impl FnOnce(),
    ) -> Result<Removed, RemovalFailure> {
        // A known retained setup cannot become a legacy checkout when both
        // administrative files disappear. Consult canonical durable intent
        // before removing anything, then repeat its generation check at commit.
        let expected_creation = (|| -> Result<Option<OperationId>, String> {
            let service = RepositoryService::system("git");
            let mut known = Vec::new();
            for row in store.journaled_worktree_operations().map_err(|error| error.to_string())? {
                let Ok((common, report)) = service.worktree_setup_creation_from_row(&row) else { continue };
                if common == self.common.path() && report.request.as_ref().is_some_and(|request| request.destination == self.target.path) {
                    known.push(report.operation_id);
                }
            }
            if known.is_empty() { return Ok(None); }
            // Journal order is not generation order: removed checkouts retain
            // their durable receipts, and the same managed path may be reused.
            let marker = crate::repository::worktree_creation_marker(&self.admin)
                .map_err(|error| format!("Original setup generation evidence is unavailable; retain this checkout for recovery: {error}"))?;
            if !known.contains(&marker) {
                return Err("Original setup generation evidence does not match a retained creation; retain this checkout for recovery".into());
            }
            Ok(Some(marker))
        })().map_err(|error| RemovalFailure { path: self.target.path.clone(), rescue_branch: None, error, outcome_unknown: false })?;
        let Self {
            target,
            common,
            admin,
            repository_root,
            request,
            inspection,
            unpushed,
            detached_only,
            force,
            delete_branch,
            selected,
            policy,
            notable,
            fingerprint,
            confirmed,
        } = self;
        let cancellation = CancellationToken::new();
        let path = target.path.clone();
        let branch = target.branch.clone();
        let mut rescue_branch = None;
        let removed = remove_fenced(
            target,
            WindowPolicy::AllowNone,
            admit,
            || {
                same_named_root(&common)?;
                if let Some(expected) = expected_creation
                    && crate::repository::worktree_creation_marker(&admin)
                        .map_err(|error| error.to_string())?
                        != expected
                {
                    return Err("Original setup generation changed before removal".into());
                }
                same_named_root(&admin)?;
                same_named_root(&repository_root)?;
                crate::repository::worktree_setup_removal_is_quiescent(&admin)
                    .map_err(|error| error.to_string())?;
                let current = worktree_content::inspect(&request, &cancellation)
                    .map_err(|e| e.to_string())?;
                if current.token.is_none()
                    || current.token != inspection.token
                    || current.head != inspection.head
                    || current.branch != inspection.branch
                {
                    return Err(
                        "The worktree changed during removal; inspect it again before retrying"
                            .into(),
                    );
                }
                selected.verify(&cancellation)?;
                let current_policy = selected.policy(&cancellation)?;
                let current_notable = selected.notable(&current_policy, &cancellation)?;
                if current_policy != policy || current_notable != notable {
                    return Err(
                        "The worktree's ignored-file policy or inventory changed; review it again"
                            .into(),
                    );
                }
                let current_unpushed = selected.commit_count(false, &cancellation)?;
                let current_detached = if branch.is_none() {
                    selected.commit_count(true, &cancellation)?
                } else {
                    0
                };
                if current_unpushed != unpushed || current_detached != detached_only {
                    return Err(
                        "Worktree reachability changed; inspect it again before retrying".into(),
                    );
                }
                if selected.fingerprint(
                    &current,
                    &current_policy,
                    &current_notable,
                    current_unpushed,
                    current_detached,
                )? != fingerprint
                {
                    return Err("The worktree's ignored-file evidence changed after confirmation; review it again".into());
                }
                permits(
                    &current,
                    current_notable.len(),
                    current_unpushed,
                    current_detached,
                    force,
                    confirmed,
                )?;
                selected.verify(&cancellation)?;
                final_check()?;
                if current_detached != 0 {
                    let head = current
                        .head
                        .as_deref()
                        .ok_or("Detached history has no exact commit")?;
                    let name = format!("cagents-rescue/{}", uuid::Uuid::new_v4());
                    let command = crate::worktree_branch::git_command(&common.process_path())
                        .args([
                            "update-ref",
                            &format!("refs/heads/{name}"),
                            head,
                            &"0".repeat(head.len()),
                        ])
                        .timeout(std::time::Duration::from_secs(10));
                    SystemProcessExecutor
                        .execute(&command, &cancellation)
                        .and_then(|output| output.require_success(&command.program))
                        .map_err(|e| e.reported_detail())?;
                    rescue_branch = Some(name);
                }
                Ok(())
            },
            force,
            std::time::Duration::from_secs(10 * 60),
        );
        let removed = match removed {
            Ok(removed) => removed,
            Err(error) => {
                return Err(RemovalFailure {
                    path,
                    rescue_branch,
                    error,
                    outcome_unknown: true,
                });
            }
        };
        post_git();
        // A successful remove_fenced result proves Git registration and exact
        // directory removal even if the original Git return was nonzero.
        // A failure above never certifies rollback or permits automatic retry.
        let mut warnings = Vec::new();
        if let Err(failures) =
            crate::repository::release_deleted_worktree_creations(store, &removed)
        {
            warnings.push(format!(
                "Worktree removed; creation receipt retirement needs review: {}",
                failures
                    .into_iter()
                    .map(|e| e.to_string())
                    .collect::<Vec<_>>()
                    .join("; ")
            ));
        }
        let mut result = Removed {
            path,
            branch_deleted: None,
            rescue_branch,
            warnings,
            branch_error: None,
        };
        if delete_branch && let Some(branch) = branch {
            // Separate safe branch deletion never inherits --force's file-loss
            // consent. Retain original common/repository descriptors throughout.
            let deletion = same_named_root(&common)
                .and_then(|_| same_named_root(&repository_root))
                .and_then(|_| {
                    crate::worktree_branch::delete_branch_checked_at(
                        &common.process_path(),
                        &branch,
                        false,
                        inspection.head.as_deref(),
                        &cancellation,
                    )
                    .map_err(|e| e.detail)
                });
            match deletion {
                Ok(()) => result.branch_deleted = Some(branch),
                Err(error) => {
                    result.branch_error =
                        Some(format!("Worktree removed; branch retained: {error}"))
                }
            }
        }
        Ok(result)
    }
}

fn same_named_root(root: &fs_safety::Root) -> Result<(), String> {
    let held = root.metadata().map_err(|e| e.to_string())?;
    let named = std::fs::symlink_metadata(root.path()).map_err(|e| e.to_string())?;
    if !named.is_dir() || (held.dev(), held.ino()) != (named.dev(), named.ino()) {
        return Err(
            "The original repository directory was replaced; no new mutation was authorized".into(),
        );
    }
    Ok(())
}
fn permits(
    inspection: &Inspection,
    notable_ignored: usize,
    unpushed: u64,
    detached: u64,
    force: bool,
    confirmed: bool,
) -> Result<(), String> {
    if let Verdict::Blocked(reason) = &inspection.verdict {
        return Err(format!("Worktree removal refused: {reason}"));
    }
    if inspection.token.is_none() {
        return Err("No complete content inspection is available; worktree retained".into());
    }
    if !force
        && (inspection.staged != 0
            || inspection.modified != 0
            || inspection.untracked != 0
            || notable_ignored != 0
            || matches!(
                inspection.verdict,
                Verdict::Unsaved {
                    uncommitted: true,
                    ..
                } | Verdict::Unsaved {
                    beyond_snapshot: true,
                    ..
                }
            )
            || (!confirmed
                && (unpushed != 0
                    || detached != 0
                    || matches!(inspection.verdict, Verdict::Unsaved { .. }))))
    {
        return Err(format!(
            "Worktree contains {} staged, {} modified, {} untracked, {} notable ignored files, {} unpushed and {} detached-only commits; pass --force to confirm discarding this work",
            inspection.staged,
            inspection.modified,
            inspection.untracked,
            notable_ignored,
            unpushed,
            detached
        ));
    }
    Ok(())
}
