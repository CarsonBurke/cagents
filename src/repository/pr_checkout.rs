//! Reconcile an existing checkout with the exact pull-request head reviewed.
//!
//! Preparation is read-only. A plan is evidence for a confirmation, not a lease:
//! execution must repeat its identity and configuration checks before effects.
use super::*;
use crate::github::{GitHubRepoKey, remote_repository_key};
use std::{
    io::Write,
    os::unix::fs::{MetadataExt, PermissionsExt},
};
mod effect;
#[cfg(test)]
mod tests;

const CONFIG_LIMIT: u64 = 4 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrCheckoutTracking {
    RemoteBranch,
    /// A local review branch deliberately has no upstream and cannot push a PR.
    ReadOnly,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExistingPrCheckoutRequest {
    pub repository: PathBuf,
    pub worktree: PathBuf,
    pub branch: ValidatedBranchName,
    pub head_repository: GitHubRepoKey,
    pub head_branch: ValidatedBranchName,
    pub expected_head: Oid,
    pub remote: String,
    pub tracking: PrCheckoutTracking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrCheckoutDisposition {
    /// Explicitly open the captured local HEAD without changing its contents.
    /// This does not claim that it equals the reviewed PR head.
    OpenPreservingLocal,
    /// Repair only the wholly absent upstream pair, preserving local work.
    RepairTrackingPreservingLocal,
    AlreadyCurrent,
    FastForward,
    RepairTracking,
    FastForwardAndRepair,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PrCheckoutFailureKind {
    Refused,
    /// Some effects may have completed. Never retry by rolling back user work.
    PartialOrUnknown,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{detail}")]
pub struct PrCheckoutFailure {
    pub kind: PrCheckoutFailureKind,
    pub detail: String,
    pub path: PathBuf,
    /// Operation-scoped reference name for reconciliation, if allocated. It
    /// may not have been fetched yet, or may already have been cleaned up;
    /// presence here is never proof that the named ref still exists.
    pub retained_ref: Option<String>,
}

impl PrCheckoutFailure {
    fn refused(path: &Path, detail: impl fmt::Display) -> Self {
        Self {
            kind: PrCheckoutFailureKind::Refused,
            detail: detail.to_string(),
            path: path.to_owned(),
            retained_ref: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct DirectoryIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl DirectoryIdentity {
    fn read(path: &Path) -> Result<Self, String> {
        let path = fs::canonicalize(path).map_err(|error| error.to_string())?;
        let root = fs_safety::Root::open(&path).map_err(|error| error.to_string())?;
        let metadata = root.metadata().map_err(|error| error.to_string())?;
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TrackingSnapshot {
    remote: Vec<String>,
    merge: Vec<String>,
    push_remote: Vec<String>,
    push_default: Vec<String>,
}

#[derive(Clone, PartialEq, Eq)]
pub struct ExistingPrCheckoutPlan {
    request: ExistingPrCheckoutRequest,
    worktree: DirectoryIdentity,
    git_dir: DirectoryIdentity,
    common_dir: DirectoryIdentity,
    current_head: Oid,
    tracking: TrackingSnapshot,
    remote: RemoteRecord,
    remote_tracking: Option<Oid>,
    config: Vec<u8>,
    dirty: DirtyState,
    disposition: PrCheckoutDisposition,
    notice: String,
}

impl fmt::Debug for ExistingPrCheckoutPlan {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        // This plan passes through Debug application messages. Configuration
        // may contain authorization headers and remote URLs may carry tokens.
        formatter
            .debug_struct("ExistingPrCheckoutPlan")
            .field("request", &self.request)
            .field("worktree", &self.worktree)
            .field("git_dir", &self.git_dir)
            .field("common_dir", &self.common_dir)
            .field("current_head", &self.current_head)
            .field("disposition", &self.disposition)
            .field("config_bytes", &self.config.len())
            .finish_non_exhaustive()
    }
}

impl ExistingPrCheckoutPlan {
    pub fn request(&self) -> &ExistingPrCheckoutRequest {
        &self.request
    }
    pub fn path(&self) -> &Path {
        &self.worktree.path
    }
    pub fn local_head(&self) -> &Oid {
        &self.current_head
    }
    pub fn disposition(&self) -> PrCheckoutDisposition {
        self.disposition
    }
    pub fn notice(&self) -> &str {
        &self.notice
    }
}

/// Point-in-time branch, source and tracking evidence for opening a checkout.
/// This is not a process-family exit certificate or cleanup authorization.
/// Children closing inherited authority can remain untracked owned writers;
/// their containment/protection is a separate lifecycle composition contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifiedPrCheckout {
    disposition: PrCheckoutDisposition,
    path: PathBuf,
    branch: ValidatedBranchName,
    head: Oid,
    upstream: Option<String>,
}

impl VerifiedPrCheckout {
    pub fn disposition(&self) -> PrCheckoutDisposition {
        self.disposition
    }
    pub fn path(&self) -> &Path {
        &self.path
    }
    pub fn branch(&self) -> &ValidatedBranchName {
        &self.branch
    }
    pub fn head(&self) -> &Oid {
        &self.head
    }
    pub fn upstream(&self) -> Option<&str> {
        self.upstream.as_deref()
    }
}

fn validate_push_behavior(
    branch: &ValidatedBranchName,
    head_branch: &ValidatedBranchName,
    remote: &str,
    push_remote: &[String],
    push_default: &[String],
    refspecs: &[String],
    mode: &[String],
) -> Result<(), String> {
    if push_remote.len() > 1
        || push_default.len() > 1
        || push_remote
            .first()
            .or(push_default.first())
            .is_some_and(|name| name != remote)
    {
        return Err(
            "The branch's push destination differs from the PR remote; reconcile it explicitly"
                .into(),
        );
    }
    if !refspecs.is_empty() {
        return Err("The PR remote has explicit push refspecs; reconcile its push behavior before opening a writable checkout".into());
    }
    if mode.len() > 1
        || mode
            .first()
            .is_some_and(|mode| !matches!(mode.as_str(), "simple" | "upstream" | "current"))
        || (branch != head_branch && mode.first().map(String::as_str) != Some("upstream"))
    {
        return Err("The configured push mode cannot safely publish this local branch to the PR head; reconcile it explicitly".into());
    }
    Ok(())
}

impl<E: ProcessExecutor> RepositoryService<E> {
    pub(super) fn validate_pr_checkout_push_behavior(
        &self,
        worktree: &Path,
        branch: &ValidatedBranchName,
        head_branch: &ValidatedBranchName,
        remote: &str,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        validate_push_behavior(
            branch,
            head_branch,
            remote,
            &self.pr_values(
                worktree,
                &format!("branch.{}.pushremote", branch.as_str()),
                cancellation,
            )?,
            &self.pr_values(worktree, "remote.pushdefault", cancellation)?,
            &self.pr_values(worktree, &format!("remote.{remote}.push"), cancellation)?,
            &self.pr_values(worktree, "push.default", cancellation)?,
        )
    }

    /// Execute a confirmed plan. Refusal and partial outcomes never authorize
    /// opening a checkout as if it matched the reviewed pull-request head.
    pub fn reconcile_existing_pr_checkout(
        &self,
        plan: &ExistingPrCheckoutPlan,
        cancellation: &CancellationToken,
    ) -> Result<VerifiedPrCheckout, PrCheckoutFailure> {
        effect::reconcile(self, plan, cancellation)
    }

    pub(super) fn pr_values(
        &self,
        worktree: &Path,
        key: &str,
        cancellation: &CancellationToken,
    ) -> Result<Vec<String>, String> {
        let command = self.git_read(worktree, ["config", "--null", "--get-all", key]);
        let output = self
            .run(&command, cancellation)
            .map_err(|error| error.reported_detail())?;
        if output.exit_code == Some(1) {
            return Ok(Vec::new());
        }
        if !output.success {
            return Err(git_failure(&command, output).to_string());
        }
        if !output.stdout.ends_with(&[0]) {
            return Err("Unterminated Git configuration value".into());
        }
        output.stdout[..output.stdout.len() - 1]
            .split(|byte| *byte == 0)
            .map(|value| String::from_utf8(value.to_vec()).map_err(|error| error.to_string()))
            .collect()
    }

    /// Read a confirmation plan for a worktree already checked out on a PR's
    /// local branch. This never fetches, moves HEAD, or repairs configuration.
    pub fn prepare_existing_pr_checkout(
        &self,
        request: &ExistingPrCheckoutRequest,
        cancellation: &CancellationToken,
    ) -> Result<ExistingPrCheckoutPlan, PrCheckoutFailure> {
        self.prepare_existing_pr_checkout_inner(request, false, false, cancellation)
    }

    /// Prepare an explicitly acknowledged opening of the existing local HEAD.
    /// It must already track the reviewed source correctly, or be an explicitly
    /// read-only branch without tracking. No fetch, upstream repair, index
    /// replacement or branch movement is authorized by this plan.
    pub fn prepare_existing_pr_checkout_preserving_local(
        &self,
        request: &ExistingPrCheckoutRequest,
        cancellation: &CancellationToken,
    ) -> Result<ExistingPrCheckoutPlan, PrCheckoutFailure> {
        self.prepare_existing_pr_checkout_inner(request, true, false, cancellation)
    }

    /// Confirm only missing upstream configuration while opening captured local
    /// work. The existing direct tracking ref must already equal the reviewed
    /// head; this action never fetches, moves refs or replaces checkout bytes.
    pub fn prepare_existing_pr_checkout_repairing_tracking_and_preserving_local(
        &self,
        request: &ExistingPrCheckoutRequest,
        cancellation: &CancellationToken,
    ) -> Result<ExistingPrCheckoutPlan, PrCheckoutFailure> {
        let plan = self.prepare_existing_pr_checkout_inner(request, true, true, cancellation)?;
        if plan.disposition != PrCheckoutDisposition::RepairTrackingPreservingLocal {
            return Err(PrCheckoutFailure::refused(
                &request.worktree,
                "This action requires a wholly absent writable PR upstream; review the existing checkout without changing its tracking",
            ));
        }
        Ok(plan)
    }

    fn recheck_existing_pr_checkout(
        &self,
        plan: &ExistingPrCheckoutPlan,
        cancellation: &CancellationToken,
    ) -> Result<ExistingPrCheckoutPlan, PrCheckoutFailure> {
        let preserve = matches!(
            plan.disposition,
            PrCheckoutDisposition::OpenPreservingLocal
                | PrCheckoutDisposition::RepairTrackingPreservingLocal
        );
        self.prepare_existing_pr_checkout_inner(
            &plan.request,
            preserve,
            plan.disposition == PrCheckoutDisposition::RepairTrackingPreservingLocal,
            cancellation,
        )
    }

    fn prepare_existing_pr_checkout_inner(
        &self,
        request: &ExistingPrCheckoutRequest,
        preserve_local: bool,
        repair_preserving_local: bool,
        cancellation: &CancellationToken,
    ) -> Result<ExistingPrCheckoutPlan, PrCheckoutFailure> {
        let refuse = |detail: String| PrCheckoutFailure::refused(&request.worktree, detail);
        let prepare = || -> Result<ExistingPrCheckoutPlan, String> {
            ValidatedBranchName::parse(request.branch.as_str())
                .map_err(|error| error.to_string())?;
            ValidatedBranchName::parse(request.head_branch.as_str())
                .map_err(|error| error.to_string())?;
            Oid::parse_object(request.expected_head.as_str()).map_err(|error| error.to_string())?;
            let worktree = DirectoryIdentity::read(&request.worktree)?;
            let discovered = self
                .discover(&worktree.path, cancellation)
                .map_err(|error| error.to_string())?;
            if discovered.is_bare
                || discovered.worktree_root.as_deref() != Some(worktree.path.as_path())
            {
                return Err("Select the exact registered worktree root and try again".into());
            }
            let common_dir = DirectoryIdentity::read(&discovered.common_dir)?;
            let git_dir = DirectoryIdentity::read(&discovered.git_dir)?;
            let repository = self
                .discover(&request.repository, cancellation)
                .map_err(|error| error.to_string())?;
            if DirectoryIdentity::read(&repository.common_dir)? != common_dir {
                return Err("The selected worktree belongs to a different repository".into());
            }
            let registered = self
                .list_worktrees(&worktree.path, cancellation)
                .map_err(|error| error.to_string())?;
            if !registered.iter().any(|entry| {
                entry.path == worktree.path
                    && entry.branch.as_deref() == Some(request.branch.full_ref().as_str())
                    && !entry.bare
                    && !entry.lock.locked
                    && entry.prunable_reason.is_none()
            }) {
                return Err(
                    "The worktree registration or checked-out branch changed; refresh the PR"
                        .into(),
                );
            }
            for marker in [
                "MERGE_HEAD",
                "CHERRY_PICK_HEAD",
                "REVERT_HEAD",
                "rebase-merge",
                "rebase-apply",
                "sequencer",
                "MERGE_AUTOSTASH",
            ] {
                match fs::symlink_metadata(git_dir.path.join(marker)) {
                    Ok(_) => {
                        return Err(format!(
                            "An existing Git operation ({marker}) must be completed before reconciling this checkout"
                        ));
                    }
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error.to_string()),
                }
            }
            let status = self
                .status_for_removal(&worktree.path, cancellation)
                .map_err(|error| error.to_string())?;
            let current_head = match &status.head {
                HeadState::Branch { name, oid } if name == request.branch.as_str() => {
                    Oid::parse_object(oid).map_err(|error| error.to_string())?
                }
                _ => {
                    return Err(
                        "The worktree is no longer on the selected PR branch; refresh it".into(),
                    );
                }
            };
            let read = |key: &str| self.pr_values(&worktree.path, key, cancellation);
            let tracking = TrackingSnapshot {
                remote: read(&format!("branch.{}.remote", request.branch.as_str()))?,
                merge: read(&format!("branch.{}.merge", request.branch.as_str()))?,
                push_remote: read(&format!("branch.{}.pushremote", request.branch.as_str()))?,
                push_default: read("remote.pushdefault")?,
            };
            self.checked_remote(&worktree.path, &request.remote, cancellation)
                .map_err(|error| error.to_string())?;
            let remote = self
                .remotes(&worktree.path, cancellation)
                .map_err(|error| error.to_string())?
                .into_iter()
                .find(|remote| remote.name == request.remote)
                .ok_or("The selected PR remote is no longer configured")?;
            let urls = read(&format!("remote.{}.url", request.remote))?;
            let push_urls = read(&format!("remote.{}.pushurl", request.remote))?;
            if urls.len() != 1 || push_urls.len() > 1 {
                return Err("The PR remote has ambiguous fetch or push URLs; resolve its configuration first".into());
            }
            let matches = |url: &str| {
                remote_repository_key(url)
                    .is_some_and(|repo| repo.same_repository(&request.head_repository))
            };
            if !matches(&urls[0])
                || (request.tracking == PrCheckoutTracking::RemoteBranch
                    && push_urls.first().is_some_and(|url| !matches(url)))
            {
                return Err("The PR remote no longer names its reviewed head repository".into());
            }
            let absent = tracking.remote.is_empty() && tracking.merge.is_empty();
            let expected_merge = request.head_branch.full_ref();
            let correct =
                tracking.remote == [request.remote.clone()] && tracking.merge == [expected_merge];
            let repair = match request.tracking {
                PrCheckoutTracking::ReadOnly if absent => false,
                PrCheckoutTracking::ReadOnly => return Err("This read-only review branch has tracking configured; reconcile it explicitly before opening".into()),
                PrCheckoutTracking::RemoteBranch if correct => false,
                PrCheckoutTracking::RemoteBranch if absent => true,
                PrCheckoutTracking::RemoteBranch => return Err("The branch has partial or different upstream configuration; preserve it and reconcile manually".into()),
            };
            if preserve_local && repair && !repair_preserving_local {
                return Err("This branch has no PR upstream. Configure its tracking explicitly before opening preserved local work; no commits or files were changed".into());
            }
            if request.tracking == PrCheckoutTracking::RemoteBranch {
                validate_push_behavior(
                    &request.branch,
                    &request.head_branch,
                    &request.remote,
                    &tracking.push_remote,
                    &tracking.push_default,
                    &read(&format!("remote.{}.push", request.remote))?,
                    &read("push.default")?,
                )?;
            }
            if repair {
                let storage = read("extensions.refstorage")?;
                if storage.len() > 1 || storage.first().is_some_and(|value| value != "files") {
                    return Err("Automatic tracking repair requires Git's files ref storage; configure the upstream explicitly and refresh".into());
                }
                if !read("extensions.worktreeconfig")?.is_empty()
                    || git_dir.path.join("config.worktree").exists()
                    || common_dir.path.join("config.worktree").exists()
                {
                    return Err(
                        "Worktree-specific configuration requires explicit upstream reconciliation"
                            .into(),
                    );
                }
                let command = self.git_read(
                    &worktree.path,
                    [
                        "config",
                        "--name-only",
                        "--get-regexp",
                        r"^(include|includeif)\.",
                    ],
                );
                let output = self
                    .run(&command, cancellation)
                    .map_err(|error| error.reported_detail())?;
                if output.exit_code != Some(1) {
                    return Err("Included configuration cannot be repaired atomically here; configure the upstream explicitly and refresh".into());
                }
                let fetch = read(&format!("remote.{}.fetch", request.remote))?;
                let wildcard = format!("refs/heads/*:refs/remotes/{}/*", request.remote);
                let exact = format!(
                    "{}:refs/remotes/{}/{}",
                    request.head_branch.full_ref(),
                    request.remote,
                    request.head_branch.as_str()
                );
                if fetch.len() != 1
                    || ![wildcard.as_str(), exact.as_str()]
                        .contains(&fetch[0].trim_start_matches('+'))
                {
                    return Err("The remote's custom fetch mapping requires explicit upstream reconciliation".into());
                }
            }
            let remote_ref = format!(
                "refs/remotes/{}/{}",
                request.remote,
                request.head_branch.as_str()
            );
            ValidatedRefName::parse(&remote_ref).map_err(|error| error.to_string())?;
            let symbolic = self.git_read(&worktree.path, ["symbolic-ref", "--quiet", &remote_ref]);
            let symbolic = self
                .run(&symbolic, cancellation)
                .map_err(|error| error.reported_detail())?;
            if symbolic.exit_code != Some(1) {
                return Err(
                    "The PR tracking ref is symbolic or unreadable; reconcile it explicitly".into(),
                );
            }
            let remote_tracking = self
                .resolve_optional_revision(&worktree.path, &remote_ref, cancellation)
                .map_err(|error| error.to_string())?;
            if repair_preserving_local && remote_tracking.as_ref() != Some(&request.expected_head) {
                return Err("Fetch and review the PR first: this config-only action requires an existing direct tracking ref at the reviewed head; no local work or configuration was changed".into());
            }
            let command = self.git_read(
                &worktree.path,
                ["config", "--null", "--list", "--show-scope"],
            );
            let config = self
                .run_checked(&command, cancellation)
                .map_err(|error| error.reported_detail())?
                .stdout;
            if config.len() as u64 > CONFIG_LIMIT {
                return Err("Configuration is too large to reconcile safely".into());
            }
            let advance = current_head != request.expected_head;
            if !preserve_local && advance && status.dirty.is_dirty() {
                return Err("The existing worktree has local changes. Commit or stash them before updating the PR checkout; nothing was overwritten".into());
            }
            // A newly reviewed commit may not be local yet. Fetch and ancestry
            // admission belong to execution, before any checkout effects.
            if !preserve_local
                && advance
                && self
                    .resolve_optional_revision(
                        &worktree.path,
                        request.expected_head.as_str(),
                        cancellation,
                    )
                    .map_err(|error| error.to_string())?
                    .is_some()
            {
                let command = self.git_read(
                    &worktree.path,
                    [
                        "merge-base",
                        "--is-ancestor",
                        current_head.as_str(),
                        request.expected_head.as_str(),
                    ],
                );
                let output = self
                    .run(&command, cancellation)
                    .map_err(|error| error.reported_detail())?;
                if !output.success {
                    return Err("The local PR branch is ahead or has diverged. Its commits were preserved; reconcile it manually".into());
                }
            }
            let disposition = if preserve_local && repair {
                PrCheckoutDisposition::RepairTrackingPreservingLocal
            } else if preserve_local {
                PrCheckoutDisposition::OpenPreservingLocal
            } else {
                match (advance, repair) {
                    (false, false) => PrCheckoutDisposition::AlreadyCurrent,
                    (true, false) => PrCheckoutDisposition::FastForward,
                    (false, true) => PrCheckoutDisposition::RepairTracking,
                    (true, true) => PrCheckoutDisposition::FastForwardAndRepair,
                }
            };
            let notice = match disposition {
                PrCheckoutDisposition::RepairTrackingPreservingLocal => format!("Configure only branch.{}.remote={} and branch.{}.merge={}, then open local commit {} while the reviewed PR is at {}. Preserve local commits, index and files without fetching or updating any ref.", request.branch.as_str(), request.remote, request.branch.as_str(), request.head_branch.full_ref(), current_head, request.expected_head),
                PrCheckoutDisposition::OpenPreservingLocal => format!("Open existing local commit {} while the reviewed PR is at {}. Preserve local commits and uncommitted work without fetching or updating this checkout.", current_head, request.expected_head),
                PrCheckoutDisposition::AlreadyCurrent => "Open the existing worktree at the reviewed PR head.".to_owned(),
                PrCheckoutDisposition::FastForward => "Verify the fetched PR head, then fast-forward and open the existing clean worktree.".to_owned(),
                PrCheckoutDisposition::RepairTracking => "Configure the missing PR upstream and open the existing worktree.".to_owned(),
                PrCheckoutDisposition::FastForwardAndRepair => "Configure the missing upstream, verify the fetched PR head, then fast-forward and open the existing clean worktree.".to_owned(),
            };
            Ok(ExistingPrCheckoutPlan {
                request: request.clone(),
                worktree,
                git_dir,
                common_dir,
                current_head,
                tracking,
                remote,
                remote_tracking,
                config,
                dirty: status.dirty,
                disposition,
                notice,
            })
        };
        prepare().map_err(refuse)
    }
}
