//! The standalone shell uses the same durable worktree journal as the ADE.
//! UI state owns cancellation; dropping a review never erases an accepted job.

use crate::{
    persistence::StateStore,
    repository::{
        CancellationToken, CreateWorktreeRequest, PrWorktreeCreationConsent, RepositoryService,
        WorktreeOperationReport,
    },
};

/// Executes an already confirmed immutable request. The journal records every
/// effect before Git starts and reconciles an interrupted checkout on reopen.
pub fn create(
    store: &StateStore,
    request: &CreateWorktreeRequest,
    consent: Option<&PrWorktreeCreationConsent>,
    cancellation: &CancellationToken,
) -> WorktreeOperationReport {
    let (progress, _receiver) = async_channel::bounded(8);
    let service = RepositoryService::system("git");
    match consent {
        Some(consent) => service.create_worktree_journaled_for_pr(
            store,
            request,
            consent,
            1,
            cancellation,
            &progress,
        ),
        None => service.create_worktree_journaled(store, request, 1, cancellation, &progress),
    }
}

use crate::{
    github::GitHubService,
    repository::{
        CommandSpec, ExistingPrCheckoutPlan, ExistingPrCheckoutRequest, LocalRefRecord,
        PrCheckoutDisposition, PrCheckoutTracking, ProcessExecutor, RemoteRecord,
        SystemProcessExecutor, ValidatedBranchName, VerifiedPrCheckout, WorktreeRecord,
    },
};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedCheckout {
    pub repository: PathBuf,
    /// Remote-qualified input lets the shared journal fetch and set upstream.
    pub branch_input: String,
    pub base: String,
    pub existing_worktree: Option<PathBuf>,
    /// Existing checkouts are reconciled only after an explicit confirmation.
    pub reconciliation: Option<ExistingPrCheckoutPlan>,
    pub creation_consent: Option<PrWorktreeCreationConsent>,
    existing_request: Option<ExistingPrCheckoutRequest>,
    pub notice: String,
}

/// Both shells fence the backend's completed receipt to the review that the
/// user confirmed. No filesystem lookup or Git command runs on the UI thread.
pub fn verified_existing_path(
    target: &super::CheckoutTarget,
    plan: &ExistingPrCheckoutPlan,
    receipt: &VerifiedPrCheckout,
) -> Result<PathBuf, String> {
    let request = plan.request();
    let expected_local_head = if matches!(
        plan.disposition(),
        PrCheckoutDisposition::OpenPreservingLocal
            | PrCheckoutDisposition::RepairTrackingPreservingLocal
    ) {
        plan.local_head()
    } else {
        &request.expected_head
    };
    let upstream = match request.tracking {
        PrCheckoutTracking::RemoteBranch => Some(format!(
            "refs/remotes/{}/{}",
            request.remote,
            request.head_branch.as_str()
        )),
        PrCheckoutTracking::ReadOnly => None,
    };
    if request.repository != target.repository
        || request.expected_head != target.head
        || request.head_branch.as_str() != target.head_ref_name
        || !target
            .head_repository
            .as_ref()
            .is_some_and(|source| source.same_repository(&request.head_repository))
        || receipt.path() != plan.path()
        || receipt.disposition() != plan.disposition()
        || receipt.head() != expected_local_head
        || receipt.branch() != &request.branch
        || receipt.upstream() != upstream.as_deref()
    {
        return Err(
            "The checkout receipt no longer matches this review. Prepare checkout again.".into(),
        );
    }
    Ok(receipt.path().to_owned())
}

pub trait CheckoutGit {
    fn remotes(&self) -> Result<Vec<RemoteRecord>, String>;
    fn local_refs(&self) -> Result<Vec<LocalRefRecord>, String>;
    fn worktrees(&self) -> Result<Vec<WorktreeRecord>, String>;
    fn add_remote(&self, name: &str, url: &str) -> Result<(), String>;
}

struct LocalGit<'a> {
    repository: &'a Path,
    cancellation: &'a CancellationToken,
}
impl CheckoutGit for LocalGit<'_> {
    fn remotes(&self) -> Result<Vec<RemoteRecord>, String> {
        RepositoryService::system("git")
            .remotes(self.repository, self.cancellation)
            .map_err(|error| error.to_string())
    }
    fn local_refs(&self) -> Result<Vec<LocalRefRecord>, String> {
        let command = RepositoryService::system("git").refs_command(self.repository);
        let output = SystemProcessExecutor
            .execute(&command, self.cancellation)
            .map_err(|error| error.to_string())?;
        let output = output
            .require_success(Path::new("git"))
            .map_err(|error| error.to_string())?;
        crate::repository::parse_refs(&output.stdout)
            .map(|refs| refs.local)
            .map_err(|error| error.to_string())
    }
    fn worktrees(&self) -> Result<Vec<WorktreeRecord>, String> {
        RepositoryService::system("git")
            .list_worktrees(self.repository, self.cancellation)
            .map_err(|error| error.to_string())
    }
    fn add_remote(&self, name: &str, url: &str) -> Result<(), String> {
        let command = CommandSpec::new("git")
            .args(["-c", "core.fsmonitor=false", "remote", "add", name, url])
            .cwd(self.repository)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .env_remove("GIT_COMMON_DIR");
        let output = SystemProcessExecutor
            .execute(&command, self.cancellation)
            .map_err(|error| error.to_string())?;
        output
            .require_success(Path::new("git"))
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

pub fn prepare(
    target: &super::CheckoutTarget,
    cancellation: &CancellationToken,
) -> Result<PreparedCheckout, String> {
    if target.pull_request.is_none() {
        return Ok(PreparedCheckout {
            repository: target.repository.clone(),
            branch_input: target.suggested_branch(),
            base: target.head.as_str().to_owned(),
            existing_worktree: None,
            reconciliation: None,
            creation_consent: None,
            existing_request: None,
            notice: "Read-only commit checkout; no PR branch upstream is known.".into(),
        });
    }
    let head = GitHubService::system("gh")
        .checkout_head(
            target
                .pull_request
                .as_ref()
                .ok_or("No forge checkout identity")?,
            cancellation,
        )
        .map_err(|error| error.to_string())?;
    let mut prepared = plan(
        target,
        &head,
        &LocalGit {
            repository: &target.repository,
            cancellation,
        },
    )?;
    if let Some(request) = &prepared.existing_request {
        let reconciliation = prepare_existing_confirmation(
            &RepositoryService::system("git"),
            request,
            cancellation,
        )?;
        prepared.notice = format!("{} {}", prepared.notice, reconciliation.notice());
        prepared.reconciliation = Some(reconciliation);
    }
    Ok(prepared)
}

/// All alternatives only prepare a confirmation. Opening preserved local work
/// must remain an explicit decision and still needs a verified worker receipt.
pub(super) fn prepare_existing_confirmation<E: ProcessExecutor>(
    service: &RepositoryService<E>,
    request: &ExistingPrCheckoutRequest,
    cancellation: &CancellationToken,
) -> Result<ExistingPrCheckoutPlan, String> {
    match service.prepare_existing_pr_checkout(request, cancellation) {
        Ok(plan) => Ok(plan),
        Err(error) if cancellation.is_cancelled() => Err(error.to_string()),
        Err(error) => match service.prepare_existing_pr_checkout_preserving_local(request, cancellation) {
            Ok(plan) => Ok(plan),
            Err(preserve) if cancellation.is_cancelled() => Err(preserve.to_string()),
            Err(preserve) => service.prepare_existing_pr_checkout_repairing_tracking_and_preserving_local(request, cancellation)
                .map_err(|repair| format!("{error} Opening without changes is unavailable: {preserve} Configuring only the missing upstream while keeping local work is also unavailable: {repair}")),
        },
    }
}

/// Resolve identity before creating anything. Existing branches are never reset;
/// the durable worktree service later verifies and fast-forwards them safely.
pub fn plan(
    target: &super::CheckoutTarget,
    head: &crate::github::checkout::CheckoutHead,
    git: &impl CheckoutGit,
) -> Result<PreparedCheckout, String> {
    if head.oid != target.head.as_str()
        || head.branch != target.head_ref_name
        || !target
            .head_repository
            .as_ref()
            .is_some_and(|source| source.same_repository(&head.repository))
    {
        return Err(
            "The pull request branch, source or head changed. Refresh its review before checking out.".into(),
        );
    }
    let remotes = git.remotes()?;
    let refs = git.local_refs()?;
    let worktrees = git.worktrees()?;
    let matches_remote = |remote: &RemoteRecord| {
        let matches = |url: &str| {
            crate::github::remote_repository_key(url)
                .is_some_and(|repo| repo.same_repository(&head.repository))
        };
        remote.fetch_url.as_deref().is_some_and(matches)
            && remote.push_url.as_deref().is_none_or(matches)
    };
    let existing_remote = remotes.iter().find(|remote| matches_remote(remote));
    let remote = existing_remote
        .map(|remote| remote.name.clone())
        .unwrap_or_else(|| {
            let base = format!("pr-{}", head.repository.owner);
            let mut name = base.clone();
            let mut suffix = 2;
            while remotes.iter().any(|remote| remote.name == name) {
                name = format!("{base}-{suffix}");
                suffix += 1;
            }
            name
        });
    let upstreams = remotes
        .iter()
        .filter(|remote| matches_remote(remote))
        .map(|remote| format!("refs/remotes/{}/{}", remote.name, head.branch))
        .collect::<Vec<_>>();
    let existing = refs
        .iter()
        .filter(|local| {
            local.upstream.as_ref().is_some_and(|reference| {
                upstreams
                    .iter()
                    .any(|upstream| reference.as_str() == upstream.as_str())
            }) || (local.name.as_str() == format!("refs/heads/{}", head.branch)
                && local.upstream.is_none())
        })
        .find_map(|local| {
            worktrees
                .iter()
                .find(|worktree| worktree.branch.as_deref() == Some(local.name.as_str()))
                .map(|worktree| (local, worktree))
        });
    let ensure_remote = || {
        if existing_remote.is_none() {
            let ssh = remotes
                .iter()
                .filter_map(|remote| remote.fetch_url.as_deref())
                .any(|url| url.starts_with("git@") || url.starts_with("ssh://"));
            git.add_remote(&remote, if ssh { &head.ssh_url } else { &head.clone_url })?;
        }
        Ok::<(), String>(())
    };
    if let Some((local, worktree)) = existing {
        ensure_remote()?;
        let tracking_remote = local
            .upstream
            .as_ref()
            .and_then(|upstream| {
                remotes
                    .iter()
                    .filter(|candidate| matches_remote(candidate))
                    .find(|candidate| {
                        upstream.as_str()
                            == format!("refs/remotes/{}/{}", candidate.name, head.branch)
                    })
            })
            .map(|remote| remote.name.clone())
            .unwrap_or_else(|| remote.clone());
        let branch = local
            .name
            .as_str()
            .strip_prefix("refs/heads/")
            .ok_or("The existing checkout does not name a local branch")?;
        return Ok(PreparedCheckout {
            repository: target.repository.clone(),
            branch_input: head.branch.clone(),
            base: String::new(),
            existing_worktree: Some(worktree.path.clone()),
            reconciliation: None,
            creation_consent: None,
            existing_request: Some(ExistingPrCheckoutRequest {
                repository: target.repository.clone(),
                worktree: worktree.path.clone(),
                branch: ValidatedBranchName::parse(branch).map_err(|e| e.to_string())?,
                head_repository: head.repository.clone(),
                head_branch: ValidatedBranchName::parse(&head.branch).map_err(|e| e.to_string())?,
                expected_head: target.head.clone(),
                remote: tracking_remote,
                tracking: if head.can_push || local.upstream.is_some() {
                    PrCheckoutTracking::RemoteBranch
                } else {
                    PrCheckoutTracking::ReadOnly
                },
            }),
            notice: format!(
                "Open the existing worktree for PR branch {}.{}",
                head.branch,
                if head.can_push {
                    ""
                } else {
                    " You do not have permission to push to the PR head."
                }
            ),
        });
    }
    if !head.can_push {
        let branch = format!("pr/{}", target.number);
        let worktree = worktrees.iter().find(|worktree| {
            worktree.branch.as_deref() == Some(format!("refs/heads/{branch}").as_str())
        });
        ensure_remote()?;
        return Ok(PreparedCheckout { repository: target.repository.clone(), branch_input: branch.clone(), base: target.head.as_str().to_owned(),
            existing_worktree: worktree.map(|worktree|worktree.path.clone()), reconciliation:None,
            creation_consent: if worktree.is_none() { Some(PrWorktreeCreationConsent::new(head.repository.clone(), ValidatedBranchName::parse(&head.branch).map_err(|e|e.to_string())?, target.head.clone(), remote.clone(), false).map_err(|e|e.to_string())?) } else { None },
            existing_request: worktree.map(|worktree| Ok::<_,String>(ExistingPrCheckoutRequest {
                repository:target.repository.clone(),worktree:worktree.path.clone(),
                branch:ValidatedBranchName::parse(&branch).map_err(|e|e.to_string())?,
                head_repository:head.repository.clone(),head_branch:ValidatedBranchName::parse(&head.branch).map_err(|e|e.to_string())?,
                expected_head:target.head.clone(),remote:remote.clone(),tracking:PrCheckoutTracking::ReadOnly,
            })).transpose()?,
            notice: "You do not have permission to push to the PR head. This checkout uses a read-only local branch; commits cannot be pushed to the PR.".into() });
    }
    ensure_remote()?;
    Ok(PreparedCheckout {
        repository: target.repository.clone(),
        branch_input: format!("{remote}/{}", head.branch),
        base: String::new(),
        existing_worktree: None,
        reconciliation: None,
        creation_consent: Some(
            PrWorktreeCreationConsent::new(
                head.repository.clone(),
                ValidatedBranchName::parse(&head.branch).map_err(|e| e.to_string())?,
                target.head.clone(),
                remote.clone(),
                true,
            )
            .map_err(|e| e.to_string())?,
        ),
        existing_request: None,
        notice: format!(
            "Checks out {} tracking {remote}/{}. Commits can be pushed back to PR #{}.",
            head.branch, head.branch, target.number
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        github::{GitHubRepoKey, PullRequestKey, checkout::CheckoutHead},
        repository::{Oid, ValidatedRefName},
    };
    use std::cell::RefCell;
    struct FakeGit {
        remotes: Vec<RemoteRecord>,
        refs: Vec<LocalRefRecord>,
        worktrees: Vec<WorktreeRecord>,
        added: RefCell<Vec<(String, String)>>,
    }
    impl CheckoutGit for FakeGit {
        fn remotes(&self) -> Result<Vec<RemoteRecord>, String> {
            Ok(self.remotes.clone())
        }
        fn local_refs(&self) -> Result<Vec<LocalRefRecord>, String> {
            Ok(self.refs.clone())
        }
        fn worktrees(&self) -> Result<Vec<WorktreeRecord>, String> {
            Ok(self.worktrees.clone())
        }
        fn add_remote(&self, name: &str, url: &str) -> Result<(), String> {
            self.added.borrow_mut().push((name.into(), url.into()));
            Ok(())
        }
    }
    fn fixture() -> (super::super::CheckoutTarget, CheckoutHead, FakeGit) {
        let repo = GitHubRepoKey::new("github.com", None, "owner", "repo").unwrap();
        let mut target = super::super::CheckoutTarget::read_only(
            "/repo".into(),
            7,
            Oid::parse(&"a".repeat(40)).unwrap(),
        );
        target.pull_request = Some(PullRequestKey::new(repo.clone(), 7).unwrap());
        target.head_ref_name = "feature/x".into();
        target.head_repository = Some(repo.clone());
        let head = CheckoutHead {
            repository: repo,
            branch: "feature/x".into(),
            oid: "a".repeat(40),
            clone_url: "https://github.com/owner/repo.git".into(),
            ssh_url: "git@github.com:owner/repo.git".into(),
            can_push: true,
        };
        let git = FakeGit {
            remotes: vec![RemoteRecord {
                name: "origin".into(),
                fetch_url: Some(head.clone_url.clone()),
                push_url: None,
            }],
            refs: Vec::new(),
            worktrees: Vec::new(),
            added: RefCell::new(Vec::new()),
        };
        (target, head, git)
    }
    #[test]
    fn same_oid_branch_or_source_change_requires_a_fresh_review_before_planning() {
        for changed in ["branch", "source", "head"] {
            let (target, mut head, git) = fixture();
            match changed {
                "branch" => head.branch = "renamed".into(),
                "source" => head.repository.owner = "other".into(),
                _ => head.oid = "b".repeat(40),
            }
            assert!(plan(&target, &head, &git).is_err(), "{changed}");
            assert!(git.added.borrow().is_empty(), "{changed}");
        }
    }

    #[test]
    fn same_repository_tracks_the_real_head_branch_without_a_new_remote() {
        let (target, head, git) = fixture();
        let plan = plan(&target, &head, &git).unwrap();
        assert_eq!(target.suggested_branch(), "feature/x");
        assert_eq!(plan.branch_input, "origin/feature/x");
        assert!(plan.base.is_empty());
        assert!(git.added.borrow().is_empty());
    }
    #[test]
    fn fork_remote_is_added_once_and_reused_by_identity() {
        let (mut target, mut head, mut git) = fixture();
        head.repository.owner = "contributor".into();
        target.head_repository = Some(head.repository.clone());
        head.clone_url = "https://github.com/contributor/repo.git".into();
        let first = plan(&target, &head, &git).unwrap();
        assert_eq!(first.branch_input, "pr-contributor/feature/x");
        assert_eq!(git.added.borrow().len(), 1);
        git.remotes.push(RemoteRecord {
            name: "their-fork".into(),
            fetch_url: Some(head.clone_url.clone()),
            push_url: None,
        });
        assert_eq!(
            plan(&target, &head, &git).unwrap().branch_input,
            "their-fork/feature/x"
        );
        assert_eq!(git.added.borrow().len(), 1);
    }
    #[test]
    fn a_checked_out_tracking_branch_opens_its_existing_worktree() {
        let (target, head, mut git) = fixture();
        git.refs.push(LocalRefRecord {
            name: ValidatedRefName::parse("refs/heads/feature/x").unwrap(),
            oid: target.head.clone(),
            upstream: Some(ValidatedRefName::parse("refs/remotes/origin/feature/x").unwrap()),
            head: false,
        });
        git.worktrees = crate::repository::parse_worktree_list(
            format!(
                "worktree /existing\0HEAD {}\0branch refs/heads/feature/x\0\0",
                target.head.as_str()
            )
            .as_bytes(),
        )
        .unwrap();
        assert_eq!(
            plan(&target, &head, &git).unwrap().existing_worktree,
            Some(PathBuf::from("/existing"))
        );
        git.remotes.push(RemoteRecord {
            name: "same-repository".into(),
            fetch_url: Some(head.clone_url.clone()),
            push_url: None,
        });
        git.refs[0].upstream =
            Some(ValidatedRefName::parse("refs/remotes/same-repository/feature/x").unwrap());
        assert_eq!(
            plan(&target, &head, &git).unwrap().existing_worktree,
            Some(PathBuf::from("/existing")),
            "remote aliases identify the same PR checkout"
        );
        git.worktrees.clear();
        assert_eq!(
            plan(&target, &head, &git).unwrap().branch_input,
            "origin/feature/x"
        );
    }
    #[test]
    fn missing_push_permission_uses_a_stable_read_only_branch_and_explains_it() {
        let (target, mut head, git) = fixture();
        head.can_push = false;
        let prepared = plan(&target, &head, &git).unwrap();
        assert_eq!(prepared.branch_input, "pr/7");
        assert_eq!(prepared.base, target.head.as_str());
        assert!(prepared.notice.contains("do not have permission"));
        assert!(git.added.borrow().is_empty());
        let consent = prepared.creation_consent.as_ref().unwrap();
        assert_eq!(consent.expected_head(), &target.head);
        assert_eq!(consent.remote(), "origin");
        assert!(!consent.tracks_remote());

        let (target, mut head, mut git) = fixture();
        head.can_push = false;
        git.remotes.clear();
        let prepared = plan(&target, &head, &git).unwrap();
        assert_eq!(
            git.added.borrow().as_slice(),
            &[("pr-owner".into(), head.clone_url)]
        );
        assert_eq!(
            prepared.creation_consent.as_ref().unwrap().remote(),
            "pr-owner"
        );
    }

    #[test]
    fn untracked_and_old_existing_checkouts_require_reconciliation_before_opening() {
        for upstream in [None, Some("refs/remotes/origin/feature/x")] {
            let (target, head, mut git) = fixture();
            git.refs.push(LocalRefRecord {
                name: ValidatedRefName::parse("refs/heads/feature/x").unwrap(),
                oid: Oid::parse(&"b".repeat(40)).unwrap(),
                upstream: upstream.map(|value| ValidatedRefName::parse(value).unwrap()),
                head: false,
            });
            git.worktrees = crate::repository::parse_worktree_list(
                format!(
                    "worktree /existing\0HEAD {}\0branch refs/heads/feature/x\0\0",
                    "b".repeat(40)
                )
                .as_bytes(),
            )
            .unwrap();
            let planned = plan(&target, &head, &git).unwrap();
            let request = planned.existing_request.unwrap();
            assert_eq!(request.worktree, PathBuf::from("/existing"));
            assert_eq!(request.expected_head, target.head);
            assert_eq!(request.branch.as_str(), "feature/x");
            assert_eq!(request.tracking, PrCheckoutTracking::RemoteBranch);
            assert!(
                planned.reconciliation.is_none(),
                "planning alone cannot authorize opening"
            );
        }
    }

    #[test]
    fn an_old_readonly_pr_branch_requires_head_reconciliation_without_tracking() {
        let (target, mut head, mut git) = fixture();
        head.can_push = false;
        git.worktrees = crate::repository::parse_worktree_list(
            format!(
                "worktree /readonly\0HEAD {}\0branch refs/heads/pr/7\0\0",
                "b".repeat(40)
            )
            .as_bytes(),
        )
        .unwrap();
        let planned = plan(&target, &head, &git).unwrap();
        let request = planned.existing_request.unwrap();
        assert_eq!(request.branch.as_str(), "pr/7");
        assert_eq!(request.head_branch.as_str(), "feature/x");
        assert_eq!(request.tracking, PrCheckoutTracking::ReadOnly);
        assert_eq!(request.expected_head, target.head);
        assert!(planned.reconciliation.is_none());
    }
}
