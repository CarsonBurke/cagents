//! Removing a card's worktree without losing work. A plan inspects the
//! worktree's content; a removal installs the worktree's reservation, checks
//! that nothing runs there, and removes it behind the cleanup fences once its
//! content is still what the accepted plan judged.
use std::path::{Path, PathBuf};

use super::{Handle, SharedHost};
use crate::{
    repository::CancellationToken,
    work::{Item, Keep, Removal, RemovalPlan, Removed, SavedSnapshot, Scope},
    worktree_content::{self, Inspection, SNAPSHOT_REFS, Verdict},
};

const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);
/// Deleting a large tree of build output can take minutes on a slow disk.
const GIT_REMOVE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10 * 60);
const CHANGED: &str = "The worktree changed since its removal plan; review the new plan and retry";

/// A card's worktree, with what its content is judged against.
struct Subject {
    item: Item,
    request: worktree_content::Request,
    merged_pull_request: Option<u64>,
}

impl Handle {
    pub(super) fn removal_plan(
        &self,
        host: &SharedHost,
        scope: &Scope,
        name: &str,
        fetch: bool,
    ) -> Result<RemovalPlan, String> {
        let subject = self.subject(host, scope, name)?;
        if fetch {
            self.fetch(&subject.request.path)?;
        }
        let inspection = worktree_content::inspect(&subject.request, &self.0.cancellation)
            .map_err(|error| error.to_string())?;
        Ok(subject.plan(inspection))
    }

    pub(super) fn remove(&self, host: &SharedHost, removal: &Removal) -> Result<Removed, String> {
        use crate::worktree_cleanup::{FencedTarget, WindowPolicy, remove_fenced};
        let subject = self.subject(host, &removal.scope, &removal.item)?;
        let request = &subject.request;
        let cancellation = &self.0.cancellation;
        // From here no session starts inside the worktree.
        let reservation = super::super::worktree_removal::Reservation::install(host, &request.path)
            .map_err(|error| error.message)?;
        if reservation.path() != request.path {
            return Err(CHANGED.into());
        }
        reservation.ensure_unoccupied(host)?;
        reservation.ensure_no_processes()?;
        let inspection =
            worktree_content::inspect(request, cancellation).map_err(|error| error.to_string())?;
        if inspection.token.as_deref() != Some(removal.token.as_str()) {
            return Err(CHANGED.into());
        }
        let branch = inspection.branch.clone();
        let head = inspection.head.clone();
        // Work the plan found saved needs no snapshot, whatever was asked.
        let keep = match (removal.keep, &inspection.verdict) {
            (Keep::Snapshot, Verdict::Unsaved { .. }) | (Keep::Discard, _) => removal.keep,
            (Keep::Snapshot | Keep::Nothing, _) => Keep::Nothing,
        };
        subject
            .plan(inspection)
            .permits(removal.keep, removal.delete_branch)?;
        let target = FencedTarget::capture(&subject.item.project, &request.path, branch.clone())?;
        let mut snapshot = None;
        let removed = remove_fenced(
            target,
            WindowPolicy::AllowNone,
            |_| reservation.ensure_unoccupied(host),
            || {
                match keep {
                    Keep::Snapshot => {
                        snapshot = Some(
                            worktree_content::snapshot(request, &removal.token, cancellation)
                                .map_err(|error| error.to_string())?,
                        );
                    }
                    Keep::Nothing | Keep::Discard => {
                        let again = worktree_content::inspect(request, cancellation)
                            .map_err(|error| error.to_string())?;
                        if again.token.as_deref() != Some(removal.token.as_str()) {
                            return Err(CHANGED.into());
                        }
                    }
                }
                // A process that opened a file while the content was read
                // could write after it; none may remain once Git starts. The
                // reservation, not control admission, keeps sessions out, and
                // holds off a daemon upgrade or shutdown until Git is done.
                reservation.ensure_no_processes()
            },
            true,
            GIT_REMOVE_TIMEOUT,
        );
        if let Err(error) = removed {
            return Err(match snapshot {
                Some(snapshot) => format!(
                    "{error}. The worktree stays; its unsaved work is also kept in {}",
                    snapshot.refname
                ),
                None => error,
            });
        }
        drop(reservation);
        let mut removed = Removed {
            project: subject.item.project.clone(),
            path: request.path.clone(),
            snapshot,
            deleted_branch: None,
            branch_error: None,
        };
        if removal.delete_branch
            && let Some(branch) = branch
        {
            // A squash-merged branch is not merged as far as Git can tell;
            // the plan proved its work landed, at exactly this commit.
            match crate::worktree_branch::delete_branch_checked_at(
                &subject.item.project,
                &branch,
                true,
                head.as_deref(),
                cancellation,
            ) {
                Ok(()) => removed.deleted_branch = Some(branch),
                Err(failure) => removed.branch_error = Some(failure.detail),
            }
        }
        self.rescan(&subject.item.project);
        Ok(removed)
    }

    /// Every snapshot in the projects `scope` covers, newest first.
    pub(super) fn snapshots(
        &self,
        host: &SharedHost,
        scope: &Scope,
    ) -> Result<Vec<SavedSnapshot>, String> {
        use crate::process::ProcessExecutor;
        let mut snapshots = Vec::new();
        for (repository, name) in self.resolve(host, scope)? {
            let command = crate::worktree_branch::git_command(&repository)
                .args([
                    "for-each-ref",
                    "--format=%(refname)%00%(objectname)%00%(creatordate:unix)%00%(subject)%00%(trailers:key=Cagents-Worktree,valueonly,separator=%x00)",
                    SNAPSHOT_REFS,
                ])
                .output_limits(4 * 1024 * 1024, 64 * 1024);
            let output = crate::process::SystemProcessExecutor
                .execute(&command, &self.0.cancellation)
                .and_then(|output| output.require_success(&command.program))
                .map_err(|error| format!("{name}: {}", error.reported_detail()))?;
            snapshots.extend(parse_snapshots(&output.stdout, &repository, &name));
        }
        snapshots.sort_by(|a, b| b.created_at_unix_ms.cmp(&a.created_at_unix_ms));
        Ok(snapshots)
    }

    /// The card named `name` and what its worktree is judged against: the
    /// default branch, and a merged pull request's base and exact head.
    fn subject(&self, host: &SharedHost, scope: &Scope, name: &str) -> Result<Subject, String> {
        let board = self.board(host, scope, false)?;
        let item = Self::card(&board, name)?;
        let worktree = item
            .worktree
            .as_ref()
            .ok_or_else(|| format!("{} has no worktree to remove", item.label()))?;
        if worktree.primary {
            return Err("The primary worktree is never removed".into());
        }
        let path = std::fs::canonicalize(&worktree.path)
            .map_err(|error| format!("Cannot open {}: {error}", worktree.path.display()))?;
        let merged = item.branch.as_deref().and_then(|branch| {
            let shared = self.shared();
            let snapshot = shared.projects.get(&item.project)?.snapshot.clone()?;
            let head = item.head.as_ref().map(|head| head.oid.as_str());
            snapshot
                .for_branch(branch, head)
                .filter(|pull_request| {
                    pull_request.state.0 == "MERGED"
                        && Some(pull_request.head_ref_oid.as_str()) == head
                })
                .map(|pull_request| {
                    (
                        pull_request.number,
                        pull_request.base_ref_name.clone(),
                        pull_request.head_ref_oid.clone(),
                    )
                })
        });
        let (merged_pull_request, targets, merged_head) = match merged {
            Some((number, base, head)) => {
                (Some(number), remote_branches(&path, &base)?, Some(head))
            }
            None => (None, Vec::new(), None),
        };
        Ok(Subject {
            item,
            request: worktree_content::Request {
                path,
                targets,
                merged_head,
            },
            merged_pull_request,
        })
    }

    fn fetch(&self, worktree: &Path) -> Result<(), String> {
        use crate::process::ProcessExecutor;
        let command = crate::worktree_branch::git_command(worktree)
            .args(["fetch", "--all", "--no-tags", "--quiet"])
            .timeout(FETCH_TIMEOUT)
            .output_limits(64 * 1024, 64 * 1024);
        crate::process::SystemProcessExecutor
            .execute(&command, &self.0.cancellation)
            .and_then(|output| output.require_success(&command.program))
            .map(drop)
            .map_err(|error| format!("Fetching failed: {}", error.reported_detail()))
    }
}

impl Subject {
    fn plan(&self, inspection: Inspection) -> RemovalPlan {
        RemovalPlan {
            item: self.item.label(),
            project: self.item.project.clone(),
            project_name: self.item.project_name.clone(),
            inspection,
            merged_pull_request: self.merged_pull_request,
        }
    }
}

/// Every remote's copy of `branch`, as full ref names. A pull request's base
/// lives on whichever remote it was opened against.
fn remote_branches(worktree: &Path, branch: &str) -> Result<Vec<String>, String> {
    use crate::process::ProcessExecutor;
    let command = crate::worktree_branch::git_command(worktree)
        .args(["for-each-ref", "--format=%(refname)"])
        .arg(format!("refs/remotes/*/{branch}"))
        .output_limits(64 * 1024, 64 * 1024);
    let output = crate::process::SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .and_then(|output| output.require_success(&command.program))
        .map_err(|error| error.reported_detail())?;
    Ok(String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::to_owned)
        .collect())
}

/// `for-each-ref` records: refname, object, creation time, subject and the
/// worktree trailer, NUL-separated, one per line.
fn parse_snapshots(output: &[u8], repository: &Path, name: &str) -> Vec<SavedSnapshot> {
    String::from_utf8_lossy(output)
        .lines()
        .filter_map(|line| {
            let mut fields = line.split('\0');
            let refname = fields.next()?.to_owned();
            let oid = fields.next()?.to_owned();
            let created_at_unix_ms = fields
                .next()?
                .parse::<u64>()
                .ok()
                .map(|seconds| seconds * 1000);
            let subject = fields.next()?.to_owned();
            let worktree = fields
                .next()
                .filter(|path| path.starts_with('/'))
                .map(PathBuf::from);
            Some(SavedSnapshot {
                project: repository.to_owned(),
                project_name: name.to_owned(),
                refname,
                oid,
                created_at_unix_ms,
                subject,
                worktree,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
