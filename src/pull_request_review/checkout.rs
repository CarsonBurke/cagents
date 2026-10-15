//! The standalone shell uses the same durable worktree journal as the ADE.
//! UI state owns cancellation; dropping a review never erases an accepted job.

use crate::{
    persistence::StateStore,
    repository::{
        CancellationToken, CreateWorktreeRequest, RepositoryService, WorktreeOperationReport,
    },
};

/// Executes an already confirmed immutable request. The journal records every
/// effect before Git starts and reconciles an interrupted checkout on reopen.
pub fn create(
    store: &StateStore,
    request: &CreateWorktreeRequest,
    cancellation: &CancellationToken,
) -> WorktreeOperationReport {
    let (progress, _receiver) = async_channel::bounded(8);
    RepositoryService::system("git").create_worktree_journaled(
        store,
        request,
        1,
        cancellation,
        &progress,
    )
}

use crate::{
    github::GitHubService,
    repository::{
        CommandSpec, LocalRefRecord, ProcessExecutor, RemoteRecord, SystemProcessExecutor,
        WorktreeRecord,
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
    pub notice: String,
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
    if head.oid != target.head.as_str() {
        return Err(
            "The pull request head changed. Refresh its review before checking out.".into(),
        );
    }
    plan(
        target,
        &head,
        &LocalGit {
            repository: &target.repository,
            cancellation,
        },
    )
}

/// Resolve identity before creating anything. Existing branches are never reset;
/// the durable worktree service later verifies and fast-forwards them safely.
pub fn plan(
    target: &super::CheckoutTarget,
    head: &crate::github::checkout::CheckoutHead,
    git: &impl CheckoutGit,
) -> Result<PreparedCheckout, String> {
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
    let existing_worktree = refs
        .iter()
        .filter(|local| {
            local.upstream.as_ref().is_some_and(|reference| {
                upstreams
                    .iter()
                    .any(|upstream| reference.as_str() == upstream.as_str())
            }) || (local.name.as_str() == format!("refs/heads/{}", head.branch)
                && local.upstream.is_none()
                && local.oid.as_str() == head.oid)
        })
        .find_map(|local| {
            worktrees
                .iter()
                .find(|worktree| worktree.branch.as_deref() == Some(local.name.as_str()))
        })
        .map(|worktree| worktree.path.clone());
    if let Some(path) = existing_worktree {
        return Ok(PreparedCheckout {
            repository: target.repository.clone(),
            branch_input: head.branch.clone(),
            base: String::new(),
            existing_worktree: Some(path),
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
        return Ok(PreparedCheckout { repository: target.repository.clone(), branch_input: format!("pr/{}", target.number), base: target.head.as_str().to_owned(),
            existing_worktree: worktrees.iter().find(|worktree| worktree.branch.as_deref() == Some(format!("refs/heads/pr/{}", target.number).as_str())).map(|worktree| worktree.path.clone()),
            notice: "You do not have permission to push to the PR head. This checkout uses a read-only local branch; commits cannot be pushed to the PR.".into() });
    }
    if existing_remote.is_none() {
        let ssh = remotes
            .iter()
            .filter_map(|remote| remote.fetch_url.as_deref())
            .any(|url| url.starts_with("git@") || url.starts_with("ssh://"));
        git.add_remote(&remote, if ssh { &head.ssh_url } else { &head.clone_url })?;
    }
    Ok(PreparedCheckout {
        repository: target.repository.clone(),
        branch_input: format!("{remote}/{}", head.branch),
        base: String::new(),
        existing_worktree: None,
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
        let (target, mut head, mut git) = fixture();
        head.repository.owner = "contributor".into();
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
        let plan = plan(&target, &head, &git).unwrap();
        assert_eq!(plan.branch_input, "pr/7");
        assert_eq!(plan.base, target.head.as_str());
        assert!(plan.notice.contains("do not have permission"));
        assert!(git.added.borrow().is_empty());
    }
}
