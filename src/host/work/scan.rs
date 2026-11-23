//! One project's local facts, read from Git alone. A scan reads only what its
//! change can have moved: the worktree list after a topology change, the ref
//! census after a ref change, and the status of each worktree that changed or
//! whose head moved. Ahead and behind counts are cached by commit pair, and
//! ignored trees such as `node_modules` are never read. A scan woken by an
//! edit in one worktree runs one `git status`.
use std::{
    collections::{BTreeMap, BTreeSet, HashMap},
    ffi::{OsStr, OsString},
    os::unix::ffi::OsStrExt,
    path::{Path, PathBuf},
    sync::Arc,
    time::Instant,
};

use crate::{
    git_watch,
    linear::context::LinkedIssue,
    model::{ProjectId, WorktreeId},
    repository::{
        CancellationToken, Divergence, HeadState, RefTip, RepositoryService, StatusSnapshot,
        WorktreeRecord,
    },
    work::{Comparison, Dirty, Head, branch_identity, branch_name},
};

/// Dirty paths whose modification time is read per scan. A worktree with more
/// changes than this is being worked on, and the newest of a sample still dates
/// the activity closely enough for a board.
const MAX_TIMED_PATHS: usize = 2048;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Base {
    pub refname: String,
    pub oid: String,
}

#[derive(Debug, Clone)]
pub(super) struct Worktree {
    pub id: WorktreeId,
    pub path: PathBuf,
    pub primary: bool,
    pub locked: bool,
    pub created_at_unix_ms: Option<u64>,
    pub branch: Option<String>,
    pub head: Option<Head>,
    pub comparison: Option<Comparison>,
    pub dirty: Option<Dirty>,
    pub link: Option<LinkedIssue>,
    /// Why this worktree's status could not be read; its siblings still were.
    pub error: Option<String>,
    /// When `dirty` was last read from Git, so a poll can re-read only the
    /// worktrees that are due.
    pub status_read_at: Option<Instant>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Branch {
    pub oid: String,
    pub committed_at_unix_ms: Option<u64>,
    pub worktree: Option<PathBuf>,
    pub comparison: Option<Comparison>,
}

/// What the forge already knows, so the scan measures the branches it names.
#[derive(Debug, Default, PartialEq, Eq)]
pub(super) struct ForgeHints {
    /// Branches the user's pull requests come from.
    pub branches: BTreeSet<String>,
    /// Branch to the head commits of its merged pull requests.
    pub merged: BTreeMap<String, BTreeSet<String>>,
}

#[derive(Debug, Clone, Default)]
pub(super) struct Local {
    pub scanned_at_unix_ms: u64,
    pub root: Option<PathBuf>,
    pub base: Option<Base>,
    pub worktrees: Vec<Worktree>,
    /// Branches checked out in worktrees or named by the user's pull requests:
    /// the branches that can earn a card.
    pub branches: BTreeMap<String, Branch>,
    /// Every local branch name, for telling remote-only pull requests apart.
    pub branch_names: BTreeSet<String>,
    /// Whether a local commit is the merged pull request head or its
    /// ancestor, by (local commit, pull request head).
    pub ancestry: BTreeMap<(String, String), bool>,
    pub plan: Option<Arc<git_watch::Project>>,
    pub plan_error: Option<String>,
    /// Checkouts a failed plan read was asked to re-read. A retry re-reads
    /// them again while reusing every other retained census.
    plan_retry: BTreeSet<WorktreeId>,
    divergence: HashMap<(String, String), Divergence>,
    /// The worktree list and ref census as last read, reused until a
    /// topology or ref change says they moved.
    records: Arc<[WorktreeRecord]>,
    base_ref: Option<String>,
    tips: Arc<[RefTip]>,
    /// When the worktree list was last read; a project the watcher cannot
    /// see re-reads it on a timer.
    pub listed_at: Option<Instant>,
}

pub(super) struct Input<'a> {
    pub id: ProjectId,
    pub repository: &'a Path,
    pub previous: Option<&'a Local>,
    /// What changed since `previous`: which worktrees to re-read, and
    /// whether the worktree list or the refs moved.
    pub change: &'a git_watch::Change,
    /// Re-read every worktree's status whatever changed, as a first scan
    /// or a request for a fresh board does.
    pub full: bool,
    /// Worktrees someone works in now: a live session runs in them. Their
    /// files are watched first; the rest are watched for Git's bookkeeping
    /// alone, apart from the primary checkout.
    pub in_use: &'a BTreeSet<PathBuf>,
    pub hints: &'a ForgeHints,
    pub now_unix_ms: u64,
    pub cancellation: &'a CancellationToken,
}

fn text(error: impl std::fmt::Display) -> String {
    error.to_string()
}

pub(super) fn scan(input: Input<'_>) -> Result<Local, String> {
    let git = RepositoryService::system("git");
    let Input {
        id,
        repository,
        previous,
        change,
        full,
        in_use,
        hints,
        now_unix_ms,
        cancellation,
    } = input;
    let full = full || previous.is_none();

    // The worktree list moves only with Git's worktree bookkeeping, which
    // the watcher reports as a topology change, as it does when it cannot
    // see the bookkeeping at all.
    let listed = match previous {
        Some(previous) if !full && !change.topology => None,
        _ => Some(
            git.list_worktrees(repository, cancellation)
                .map_err(text)?
                .into_iter()
                .filter(|record| record.prunable_reason.is_none())
                .collect::<Arc<[_]>>(),
        ),
    };
    let listed_at = match &listed {
        Some(_) => Some(Instant::now()),
        None => previous.and_then(|previous| previous.listed_at),
    };
    let records = match (&listed, previous) {
        (Some(records), _) => records.clone(),
        (None, Some(previous)) => previous.records.clone(),
        (None, None) => unreachable!("a first scan lists its worktrees"),
    };
    let listed = listed.is_some();
    let primary_path = records
        .first()
        .filter(|record| !record.bare)
        .map(|record| record.path.clone());
    // The default branch and the branch tips move only with the refs, and
    // with the configuration that names a remote's default branch.
    let (base_ref, tips) = match previous {
        Some(previous) if !listed && !change.references && !change.configuration => {
            (previous.base_ref.clone(), previous.tips.clone())
        }
        _ => {
            let base_ref = git
                .default_branch_ref(repository, cancellation)
                .map_err(text)?;
            let mut patterns = vec!["refs/heads"];
            if let Some(base) = &base_ref {
                patterns.push(base);
            }
            let tips = git
                .ref_tips(repository, &patterns, cancellation)
                .map_err(text)?
                .into();
            (base_ref, tips)
        }
    };
    let base = base_ref.as_ref().and_then(|refname| {
        tips.iter()
            .find(|tip| tip.refname.as_os_str() == OsStr::new(refname))
            .map(|tip| Base {
                refname: refname.clone(),
                oid: tip.oid.clone(),
            })
    });
    let local_tips = tips
        .iter()
        .filter_map(|tip| {
            let name = tip
                .refname
                .as_encoded_bytes()
                .strip_prefix(b"refs/heads/")?;
            Some((branch_identity(OsStr::from_bytes(name)), tip))
        })
        .collect::<BTreeMap<_, _>>();

    // Earlier facts by worktree, and the Git directory the retained watch
    // plan read for each: looked up once per record, not searched for.
    let retained = previous
        .into_iter()
        .flat_map(|previous| &previous.worktrees)
        .map(|old| (old.id, old))
        .collect::<HashMap<_, _>>();
    let planned = previous
        .and_then(|previous| previous.plan.as_ref())
        .into_iter()
        .flat_map(|plan| &plan.worktrees)
        .map(|worktree| (worktree.id, worktree.git_dir.as_path()))
        .collect::<HashMap<_, _>>();
    let mut worktrees = Vec::new();
    for record in records.iter().filter(|record| !record.bare) {
        let old = retained.get(&record.id).copied();
        // A shared ref change names no worktree; a commit, reset or branch
        // switch moves the head `git worktree list` reports, or the tip of
        // the branch checked out. A detached head moves only with the
        // worktree's own `HEAD`, which names it.
        let moved = |old: &Worktree| {
            let head = old.head.as_ref().map(|head| &head.oid);
            if listed {
                head != record.head_oid.as_ref()
            } else {
                old.branch
                    .as_deref()
                    .is_some_and(|branch| local_tips.get(branch).map(|tip| &tip.oid) != head)
            }
        };
        let reread = full
            || change.worktrees.contains(&record.id)
            || old.is_none_or(|old| old.error.is_some() || moved(old));
        if !reread
            && !listed
            && let Some(old) = old
        {
            // Nothing this scan read can have moved it.
            let mut worktree = old.clone();
            worktree.comparison = None;
            worktrees.push(worktree);
            continue;
        }
        let mut worktree = Worktree {
            id: record.id,
            path: record.path.clone(),
            primary: primary_path.as_ref() == Some(&record.path),
            locked: record.lock.locked,
            created_at_unix_ms: record
                .created_at_unix_ms
                .and_then(|at| u64::try_from(at).ok()),
            branch: record.branch.as_deref().map(|branch| {
                branch
                    .strip_prefix("refs/heads/")
                    .unwrap_or(branch)
                    .to_owned()
            }),
            head: record.head_oid.clone().map(|oid| Head {
                oid,
                committed_at_unix_ms: None,
            }),
            comparison: None,
            dirty: None,
            link: None,
            error: None,
            status_read_at: None,
        };
        if !listed && let Some(old) = old {
            // The list was not read again: the head and branch last read
            // stand until this worktree's status replaces them.
            worktree.branch = old.branch.clone();
            worktree.head = old.head.clone();
        }
        if reread {
            match git.activity_status(&record.path, cancellation) {
                Ok(status) => {
                    apply_status(&mut worktree, &status);
                    worktree.status_read_at = Some(Instant::now());
                }
                // Its own removal is often what the scan was woken for, and
                // one missing checkout must not fail its siblings.
                Err(_) if checkout_removed(&record.path) => continue,
                Err(error) => worktree.error = Some(error.to_string()),
            }
        } else if let Some(old) = old {
            worktree.dirty = old.dirty.clone();
            worktree.status_read_at = old.status_read_at;
            if worktree.head.is_none() {
                worktree.head = old.head.clone();
            }
        }
        // Worktree listing and status decode names for display. Resolve an
        // ambiguous replacement character only after applying status, so an
        // invalid name never acquires a literal U+FFFD branch's identity.
        if worktree
            .branch
            .as_deref()
            .is_some_and(|branch| branch.contains('\u{fffd}'))
        {
            match git.symbolic_branch(&record.path, cancellation) {
                Ok(branch) => worktree.branch = branch.as_deref().map(branch_identity),
                Err(error) => {
                    worktree.branch = None;
                    worktree.error = Some(error.to_string());
                }
            }
        }
        // The link lives in the worktree's Git directory, whose writes name
        // the worktree, so an unread worktree keeps the one it had.
        worktree.link = match old {
            Some(old) if !reread => old.link.clone(),
            _ => planned
                .get(&record.id)
                .map(|git_dir| git_dir.to_path_buf())
                .or_else(|| {
                    crate::project_location::git_directories(&record.path)
                        .map(|directories| directories.private)
                })
                .and_then(|git_dir| crate::linear::context::linked_in_git_dir(&git_dir).ok())
                .flatten(),
        };
        worktrees.push(worktree);
    }

    // Head commit times: a branch's from the census, a detached head's from
    // one batched log read, reused while the head does not move.
    let dated = retained
        .values()
        .filter_map(|old| old.head.as_ref())
        .filter_map(|head| Some((head.oid.as_str(), head.committed_at_unix_ms?)))
        .collect::<HashMap<_, _>>();
    let mut missing_times = Vec::new();
    for worktree in &mut worktrees {
        let Some(head) = &mut worktree.head else {
            continue;
        };
        let tip = worktree
            .branch
            .as_deref()
            .and_then(|branch| local_tips.get(branch))
            .filter(|tip| tip.oid == head.oid);
        head.committed_at_unix_ms = tip
            .and_then(|tip| tip.committed_at_unix_ms)
            .or_else(|| dated.get(head.oid.as_str()).copied());
        if head.committed_at_unix_ms.is_none() {
            missing_times.push(head.oid.clone());
        }
    }
    if !missing_times.is_empty() {
        missing_times.sort();
        missing_times.dedup();
        // A head Git cannot date (a shallow boundary, say) only loses its
        // commit time; the rest of the card stands.
        let times = git
            .commit_times(repository, &missing_times, cancellation)
            .unwrap_or_default();
        for head in worktrees
            .iter_mut()
            .filter_map(|worktree| worktree.head.as_mut())
        {
            if head.committed_at_unix_ms.is_none() {
                head.committed_at_unix_ms = times.get(&head.oid).copied();
            }
        }
    }

    // Only checked-out branches or those named by pull requests might earn a
    // card, regardless of name encoding. Measuring every branch of a large
    // repository costs seconds; these are a handful.
    let checked_out = worktrees
        .iter()
        .filter_map(|worktree| worktree.branch.as_deref())
        .collect::<BTreeSet<_>>();
    let mut branches = BTreeMap::new();
    for (name, tip) in &local_tips {
        if checked_out.contains(name.as_str()) || hints.branches.contains(name) {
            branches.insert(
                name.to_owned(),
                Branch {
                    oid: tip.oid.clone(),
                    committed_at_unix_ms: tip.committed_at_unix_ms,
                    worktree: tip.worktree.clone(),
                    comparison: None,
                },
            );
        }
    }

    let mut divergence = HashMap::new();
    if let Some(base) = &base {
        let cached = |oid: &str| {
            previous.and_then(|previous| {
                previous
                    .divergence
                    .get(&(oid.to_owned(), base.oid.clone()))
                    .copied()
            })
        };
        let unmeasured = branches
            .iter()
            .filter(|(_, branch)| cached(&branch.oid).is_none())
            .map(|(name, _)| branch_ref(name))
            .collect::<Vec<_>>();
        let measured = if unmeasured.is_empty() {
            BTreeMap::new()
        } else {
            match git.divergence(repository, &base.oid, &unmeasured, cancellation) {
                Ok(measured) => measured,
                // `%(ahead-behind)` needs Git 2.41 and the documented floor
                // is 2.36. The candidates are a handful, so each can be
                // measured on its own; one that cannot is only uncompared.
                Err(error) => {
                    tracing::debug!(%error, "measuring branches one at a time");
                    unmeasured
                        .iter()
                        .filter_map(|refname| {
                            let counts = git
                                .revision_divergence(repository, &base.oid, refname, cancellation)
                                .ok()?;
                            Some((refname.clone(), counts))
                        })
                        .collect()
                }
            }
        };
        for (name, branch) in &mut branches {
            let counts = cached(&branch.oid).or_else(|| measured.get(&branch_ref(name)).copied());
            if let Some(counts) = counts {
                divergence.insert((branch.oid.clone(), base.oid.clone()), counts);
                branch.comparison = Some(comparison(base, counts));
            }
        }
        for worktree in &mut worktrees {
            let Some(head) = &worktree.head else {
                continue;
            };
            let counts = match worktree
                .branch
                .as_deref()
                .and_then(|name| branches.get(name))
                .filter(|branch| branch.oid == head.oid)
            {
                Some(branch) => branch.comparison.as_ref().map(|comparison| Divergence {
                    ahead: comparison.ahead,
                    behind: comparison.behind,
                }),
                None => cached(&head.oid).or_else(|| {
                    git.revision_divergence(repository, &base.oid, &head.oid, cancellation)
                        .ok()
                }),
            };
            if let Some(counts) = counts {
                divergence.insert((head.oid.clone(), base.oid.clone()), counts);
                worktree.comparison = Some(comparison(base, counts));
            }
        }
    }

    // A merged pull request lands a card only when nothing local remains
    // beyond its head. A head that is not present locally (GitHub's "Update
    // branch", say) is not an answer; the default branch containing the local
    // head is one.
    let mut ancestry = BTreeMap::new();
    for (name, heads) in &hints.merged {
        let Some(branch) = branches.get(name) else {
            continue;
        };
        for head in heads {
            let key = (branch.oid.clone(), head.clone());
            let known = (branch.oid == *head)
                .then_some(true)
                .or_else(|| previous.and_then(|previous| previous.ancestry.get(&key).copied()));
            let contained = match known {
                Some(contained) => Some(contained),
                None => git
                    .is_ancestor(repository, &branch.oid, head, cancellation)
                    .ok()
                    .or_else(|| {
                        let base = base.as_ref()?;
                        git.is_ancestor(repository, &branch.oid, &base.oid, cancellation)
                            .ok()
                            .filter(|merged| *merged)
                    }),
            };
            if let Some(contained) = contained {
                ancestry.insert(key, contained);
            }
        }
    }

    // A periodic scan also repairs topology events the watcher missed. Compare
    // the plan with the checkouts this scan actually accepted, rather than
    // only looking for vanished records: newly discovered or moved checkouts
    // otherwise get cards without ever joining the watch plan. Comparing the
    // retained plan also retries a membership change whose plan read failed.
    let roots = worktrees
        .iter()
        .map(|worktree| (worktree.id, &worktree.path))
        .collect::<BTreeMap<_, _>>();
    let previous_plan = previous.and_then(|previous| previous.plan.clone());
    let membership_changed = previous_plan.as_ref().is_none_or(|plan| {
        plan.worktrees.len() != roots.len()
            || plan
                .worktrees
                .iter()
                .any(|worktree| roots.get(&worktree.id).copied() != Some(&worktree.root))
    });
    let retry_plan = previous.is_some_and(|previous| previous.plan_error.is_some());
    // Files are watched first in the worktrees someone works in and the
    // primary checkout, then, within the budget, in those with uncommitted
    // changes or recent work, where an agent elsewhere may be editing by
    // path. Git's bookkeeping is watched in every one.
    let recent = |worktree: &Worktree| {
        worktree.dirty.as_ref().is_some_and(|dirty| dirty.paths > 0)
            || [
                worktree.created_at_unix_ms,
                worktree
                    .head
                    .as_ref()
                    .and_then(|head| head.committed_at_unix_ms),
                worktree
                    .dirty
                    .as_ref()
                    .and_then(|dirty| dirty.newest_modified_at_unix_ms),
            ]
            .into_iter()
            .flatten()
            .any(|at| now_unix_ms.saturating_sub(at) <= crate::work::ACTIVE_WINDOW_MS)
    };
    let interests = worktrees
        .iter()
        .map(|worktree| {
            let interest = if in_use.contains(&worktree.path) {
                git_watch::Interest::Active
            } else if worktree.primary {
                git_watch::Interest::Shown
            } else if recent(worktree) {
                git_watch::Interest::Background
            } else {
                git_watch::Interest::Metadata
            };
            (worktree.id, interest)
        })
        .collect::<HashMap<_, _>>();
    let interest = |id: WorktreeId| {
        interests
            .get(&id)
            .copied()
            .unwrap_or(git_watch::Interest::Metadata)
    };
    let interest_changed = previous_plan.as_ref().is_some_and(|plan| {
        plan.worktrees
            .iter()
            .any(|worktree| worktree.interest() != interest(worktree.id))
    });
    let mut plan_retry = BTreeSet::new();
    let (plan, plan_error) = if membership_changed
        || change.membership
        || change.topology
        || retry_plan
        || interest_changed
    {
        let checkouts = records
            .iter()
            .filter(|record| roots.contains_key(&record.id))
            .cloned()
            .collect::<Vec<_>>();
        // A failed read may have been the only one told which checkouts
        // changed. Carry those IDs until a read succeeds, so a retry
        // refreshes them without discarding every other census.
        let changed = previous
            .map(|previous| &previous.plan_retry)
            .into_iter()
            .flatten()
            .chain(&change.worktrees)
            .copied()
            .collect::<BTreeSet<_>>();
        match git_watch::Project::read_changed_with_interest(
            id,
            repository.to_path_buf(),
            &checkouts,
            cancellation,
            previous_plan.as_deref(),
            &changed,
            &interest,
        ) {
            Ok(plan) => (Some(Arc::new(plan)), None),
            // Without a plan the project is polled; its cards still stand.
            Err(error) => {
                plan_retry = changed;
                (previous_plan, Some(error))
            }
        }
    } else {
        (
            previous_plan,
            previous.and_then(|previous| previous.plan_error.clone()),
        )
    };

    Ok(Local {
        scanned_at_unix_ms: now_unix_ms,
        root: primary_path,
        base,
        worktrees,
        branches,
        branch_names: local_tips.into_keys().collect(),
        ancestry,
        plan,
        plan_error,
        plan_retry,
        divergence,
        records,
        base_ref,
        tips,
        listed_at,
    })
}

fn branch_ref(identity: &str) -> OsString {
    let mut refname = OsString::from("refs/heads/");
    refname.push(branch_name(identity));
    refname
}

fn comparison(base: &Base, counts: Divergence) -> Comparison {
    Comparison {
        base: base
            .refname
            .strip_prefix("refs/remotes/")
            .or_else(|| base.refname.strip_prefix("refs/heads/"))
            .unwrap_or(&base.refname)
            .to_owned(),
        ahead: counts.ahead,
        behind: counts.behind,
    }
}

fn apply_status(worktree: &mut Worktree, status: &StatusSnapshot) {
    match &status.head {
        HeadState::Branch { name, oid } => {
            worktree.branch = Some(name.clone());
            worktree.head = Some(Head {
                oid: oid.clone(),
                committed_at_unix_ms: None,
            });
        }
        HeadState::Unborn { branch } => {
            worktree.branch = branch.clone();
            worktree.head = None;
        }
        HeadState::Detached { oid } => {
            worktree.branch = None;
            worktree.head = Some(Head {
                oid: oid.clone(),
                committed_at_unix_ms: None,
            });
        }
        HeadState::Unknown => {}
    }
    worktree.dirty = Some(dirty(&worktree.path, status));
}

/// Changed paths counted once each, whichever areas they changed in, and the
/// newest of their modification times. A deleted path dates by its directory,
/// whose entry list changed when it went.
fn dirty(root: &Path, status: &StatusSnapshot) -> Dirty {
    let paths = status
        .entries
        .iter()
        .map(|entry| entry.path.as_bytes())
        .collect::<BTreeSet<_>>();
    let conflicted = status
        .entries
        .iter()
        .filter(|entry| entry.conflict.is_some())
        .map(|entry| entry.path.as_bytes())
        .collect::<BTreeSet<_>>();
    let newest = status
        .entries
        .iter()
        .take(MAX_TIMED_PATHS)
        .filter_map(|entry| {
            let path = entry.path.resolve(root);
            std::fs::symlink_metadata(&path)
                .ok()
                .or_else(|| std::fs::metadata(path.parent()?).ok())?
                .modified()
                .ok()
        })
        .max()
        .and_then(|modified| modified.duration_since(std::time::UNIX_EPOCH).ok())
        .and_then(|elapsed| u64::try_from(elapsed.as_millis()).ok());
    Dirty {
        paths: u32::try_from(paths.len()).unwrap_or(u32::MAX),
        conflicted: u32::try_from(conflicted.len()).unwrap_or(u32::MAX),
        newest_modified_at_unix_ms: newest,
    }
}

/// Whether a failed read is explained by the directory no longer being a
/// checkout, as during `git worktree remove`; a permission or transient I/O
/// failure is not evidence of that.
fn checkout_removed(root: &Path) -> bool {
    std::fs::symlink_metadata(root.join(".git")).is_err_and(|error| {
        matches!(
            error.kind(),
            std::io::ErrorKind::NotFound | std::io::ErrorKind::NotADirectory
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _directory: tempfile::TempDir,
        root: PathBuf,
        linked: PathBuf,
    }

    fn git(root: &Path, arguments: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(root)
            .args(["-c", "user.name=Test", "-c", "commit.gpgsign=false"])
            .args(["-c", "init.defaultBranch=main"])
            .args(arguments)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(output.status.success(), "git {arguments:?}: {output:?}");
        String::from_utf8(output.stdout).unwrap()
    }

    fn fixture() -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        git(&root, &["config", "user.email", "me@example.com"]);
        std::fs::write(root.join("README"), "one\n").unwrap();
        std::fs::write(root.join(".gitignore"), "node_modules/\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-q", "-m", "one"]);
        git(&root, &["update-ref", "refs/remotes/origin/main", "HEAD"]);
        git(
            &root,
            &[
                "symbolic-ref",
                "refs/remotes/origin/HEAD",
                "refs/remotes/origin/main",
            ],
        );
        let linked = directory.path().join("linked");
        git(
            &root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "feature",
                linked.to_str().unwrap(),
            ],
        );
        std::fs::write(linked.join("README"), "two\n").unwrap();
        git(&linked, &["commit", "-q", "-am", "two"]);
        // A branch a pull request names and one nothing names.
        git(&root, &["branch", "mine", "feature"]);
        git(&root, &["branch", "theirs", "feature"]);
        Fixture {
            root: root.canonicalize().unwrap(),
            linked: linked.canonicalize().unwrap(),
            _directory: directory,
        }
    }

    fn run(fixture: &Fixture, previous: Option<&Local>, change: &git_watch::Change) -> Local {
        run_with(fixture, previous, change, false, &BTreeSet::new())
    }

    fn run_with(
        fixture: &Fixture,
        previous: Option<&Local>,
        change: &git_watch::Change,
        full: bool,
        in_use: &BTreeSet<PathBuf>,
    ) -> Local {
        run_at(
            fixture,
            previous,
            change,
            full,
            in_use,
            crate::chat::now_ms().unwrap(),
        )
    }

    fn run_at(
        fixture: &Fixture,
        previous: Option<&Local>,
        change: &git_watch::Change,
        full: bool,
        in_use: &BTreeSet<PathBuf>,
        now_unix_ms: u64,
    ) -> Local {
        let repository = fixture.root.join(".git");
        // A project keeps its ID across scans, as the board's does.
        let id = previous
            .and_then(|previous| previous.plan.as_ref())
            .map_or_else(ProjectId::new, |plan| plan.id);
        scan(Input {
            id,
            repository: &repository,
            previous,
            change,
            full,
            in_use,
            hints: &ForgeHints {
                branches: BTreeSet::from(["mine".to_owned()]),
                merged: BTreeMap::from([(
                    "feature".to_owned(),
                    BTreeSet::from([git(&fixture.root, &["rev-parse", "feature"])
                        .trim()
                        .to_owned()]),
                )]),
            },
            now_unix_ms,
            cancellation: &CancellationToken::new(),
        })
        .unwrap()
    }

    #[test]
    fn unchecked_out_branches_without_pull_requests_are_not_divergence_candidates() {
        let fixture = fixture();
        let head = git(
            &fixture.root,
            &[
                "commit-tree",
                "HEAD^{tree}",
                "-p",
                "HEAD",
                "-m",
                "unchecked-out",
            ],
        )
        .trim()
        .to_owned();
        git(&fixture.root, &["update-ref", "refs/heads/theirs", &head]);
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(&fixture.root)
            .arg("update-ref")
            .arg(OsStr::from_bytes(b"refs/heads/theirs-\xff"))
            .arg(&head)
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env_remove("GIT_DIR")
            .env_remove("GIT_WORK_TREE")
            .env_remove("GIT_INDEX_FILE")
            .output()
            .unwrap();
        assert!(output.status.success(), "git update-ref: {output:?}");

        let local = run(&fixture, None, &git_watch::Change::default());
        let invalid_name = branch_identity(OsStr::from_bytes(b"theirs-\xff"));
        for name in ["theirs", invalid_name.as_str()] {
            assert!(
                local.branch_names.contains(name),
                "{name} stays in the census"
            );
            assert!(
                !local.branches.contains_key(name),
                "{name} has neither a checkout nor a pull request"
            );
        }
        let base = local.base.as_ref().unwrap();
        assert!(!local.divergence.contains_key(&(head, base.oid.clone())));
    }

    #[test]
    fn scans_worktrees_candidate_branches_and_carry_unchanged_status_forward() {
        let fixture = fixture();
        std::fs::create_dir_all(fixture.linked.join("node_modules/pkg")).unwrap();
        std::fs::write(fixture.linked.join("node_modules/pkg/index.js"), "x").unwrap();
        std::fs::write(fixture.linked.join("new.txt"), "new").unwrap();
        let first = run(&fixture, None, &git_watch::Change::default());

        assert_eq!(first.root.as_deref(), Some(fixture.root.as_path()));
        assert_eq!(
            first.base,
            Some(Base {
                refname: "refs/remotes/origin/main".into(),
                oid: git(&fixture.root, &["rev-parse", "origin/main"])
                    .trim()
                    .into(),
            })
        );
        let linked = first
            .worktrees
            .iter()
            .find(|worktree| worktree.path == fixture.linked)
            .unwrap();
        assert!(!linked.primary);
        assert_eq!(linked.branch.as_deref(), Some("feature"));
        assert_eq!(
            linked.comparison,
            Some(Comparison {
                base: "origin/main".into(),
                ahead: 1,
                behind: 0
            })
        );
        let dirty = linked.dirty.as_ref().unwrap();
        assert_eq!(dirty.paths, 1, "ignored trees are never counted");
        assert!(dirty.newest_modified_at_unix_ms.is_some());
        assert!(linked.head.as_ref().unwrap().committed_at_unix_ms.is_some());
        assert!(first.worktrees.iter().any(|worktree| worktree.primary));

        assert_eq!(
            first
                .branches
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["feature", "main", "mine"],
            "a branch without a worktree or pull request is not a candidate"
        );
        assert_eq!(first.branches["mine"].comparison.as_ref().unwrap().ahead, 1);
        assert!(first.branch_names.contains("theirs"));
        let feature_head = first.branches["feature"].oid.clone();
        assert_eq!(
            first.ancestry.get(&(feature_head.clone(), feature_head)),
            Some(&true)
        );
        assert!(first.plan.is_some());

        // An unrelated edit re-reads only the worktree it names.
        std::fs::write(fixture.root.join("README"), "edited\n").unwrap();
        std::fs::remove_file(fixture.linked.join("new.txt")).unwrap();
        let primary = first
            .worktrees
            .iter()
            .find(|worktree| worktree.primary)
            .unwrap()
            .id;
        let change = git_watch::Change {
            worktrees: BTreeSet::from([primary]),
            ..git_watch::Change::default()
        };
        let second = run(&fixture, Some(&first), &change);
        let dirty = |local: &Local, path: &Path| {
            local
                .worktrees
                .iter()
                .find(|worktree| worktree.path == path)
                .unwrap()
                .dirty
                .clone()
                .unwrap()
                .paths
        };
        assert_eq!(dirty(&second, &fixture.root), 1);
        assert_eq!(dirty(&second, &fixture.linked), 1, "not re-read");
        // Nothing moved the worktree list or the refs, so neither is read.
        assert_eq!(second.listed_at, first.listed_at);
        assert!(Arc::ptr_eq(&second.records, &first.records));
        assert!(Arc::ptr_eq(&second.tips, &first.tips));
        // Each worktree remembers when its status was last read, which the
        // board's polls are scheduled from.
        let read_at = |local: &Local, path: &Path| {
            local
                .worktrees
                .iter()
                .find(|worktree| worktree.path == path)
                .unwrap()
                .status_read_at
                .unwrap()
        };
        assert!(read_at(&second, &fixture.root) > read_at(&first, &fixture.root));
        assert_eq!(
            read_at(&second, &fixture.linked),
            read_at(&first, &fixture.linked),
            "carried forward with the status it dates"
        );
        // A topology change lists the worktrees again, but re-reads only one
        // that is new or whose head moved.
        let listed = run(
            &fixture,
            Some(&second),
            &git_watch::Change {
                topology: true,
                ..git_watch::Change::default()
            },
        );
        assert!(listed.listed_at > second.listed_at);
        assert_eq!(dirty(&listed, &fixture.linked), 1, "nothing moved it");
        assert_eq!(
            read_at(&listed, &fixture.linked),
            read_at(&second, &fixture.linked)
        );
        // A fresh board re-reads every worktree.
        let third = run_with(
            &fixture,
            Some(&listed),
            &git_watch::Change::default(),
            true,
            &BTreeSet::new(),
        );
        assert_eq!(dirty(&third, &fixture.linked), 0);
        assert!(read_at(&third, &fixture.linked) > read_at(&second, &fixture.linked));

        // A shared ref change re-reads the worktree whose head it moved and
        // carries the others forward.
        std::fs::write(fixture.linked.join("new.txt"), "again").unwrap();
        std::fs::write(fixture.root.join("README"), "edited again\n").unwrap();
        git(&fixture.root, &["commit", "-q", "-am", "primary"]);
        std::fs::write(fixture.root.join("README"), "dirty\n").unwrap();
        let references = git_watch::Change {
            references: true,
            ..git_watch::Change::default()
        };
        let fourth = run(&fixture, Some(&third), &references);
        assert_eq!(dirty(&fourth, &fixture.root), 1, "its head moved");
        assert_eq!(dirty(&fourth, &fixture.linked), 0, "its head did not");
        assert_eq!(
            fourth.listed_at, third.listed_at,
            "refs alone move no worktree"
        );
        assert!(!Arc::ptr_eq(&fourth.tips, &third.tips));
    }

    #[test]
    fn files_are_watched_where_a_session_runs_or_work_is_recent() {
        let fixture = fixture();
        let interests = |local: &Local| {
            local
                .plan
                .as_ref()
                .unwrap()
                .worktrees
                .iter()
                .map(|worktree| (worktree.root.clone(), worktree.interest()))
                .collect::<BTreeMap<_, _>>()
        };
        let first = run(&fixture, None, &git_watch::Change::default());
        assert_eq!(
            interests(&first),
            BTreeMap::from([
                (fixture.root.clone(), git_watch::Interest::Shown),
                (fixture.linked.clone(), git_watch::Interest::Background),
            ]),
            "a worktree committed to just now is watched within the budget"
        );

        // A day later it is watched for Git's bookkeeping alone.
        let later = crate::chat::now_ms().unwrap() + 24 * 60 * 60 * 1000;
        let none = BTreeSet::new();
        let at = |previous: &Local, in_use: &BTreeSet<PathBuf>| {
            run_at(
                &fixture,
                Some(previous),
                &git_watch::Change::default(),
                false,
                in_use,
                later,
            )
        };
        let idle = at(&first, &none);
        assert_eq!(
            interests(&idle)[&fixture.linked],
            git_watch::Interest::Metadata
        );
        assert_eq!(interests(&idle)[&fixture.root], git_watch::Interest::Shown);

        // A session starting there re-plans it, with nothing else to read.
        let in_use = BTreeSet::from([fixture.linked.clone()]);
        let working = at(&idle, &in_use);
        assert_eq!(
            interests(&working)[&fixture.linked],
            git_watch::Interest::Active
        );
        assert_eq!(working.listed_at, first.listed_at);
        let again = at(&working, &in_use);
        assert!(Arc::ptr_eq(
            working.plan.as_ref().unwrap(),
            again.plan.as_ref().unwrap()
        ));

        // Once it ends, the worktree goes back to its bookkeeping alone.
        let ended = at(&again, &none);
        assert_eq!(
            interests(&ended)[&fixture.linked],
            git_watch::Interest::Metadata
        );
    }

    #[test]
    fn a_removed_worktree_leaves_the_scan_and_its_siblings_intact() {
        let fixture = fixture();
        let first = run(&fixture, None, &git_watch::Change::default());
        std::fs::remove_dir_all(&fixture.linked).unwrap();
        let second = run(
            &fixture,
            Some(&first),
            &git_watch::Change {
                topology: true,
                ..git_watch::Change::default()
            },
        );
        assert_eq!(second.worktrees.len(), 1);
        assert!(second.worktrees[0].primary);
    }

    #[test]
    fn polling_discovers_new_worktrees_in_the_watch_plan_without_topology_events() {
        let fixture = fixture();
        let first = run(&fixture, None, &git_watch::Change::default());
        let added = fixture.root.parent().unwrap().join("added");
        git(
            &fixture.root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "added",
                added.to_str().unwrap(),
            ],
        );
        // A project whose bookkeeping the watcher cannot see polls its
        // worktree list, which names a worktree no event announced.
        let poll = git_watch::Change {
            worktrees: first.worktrees.iter().map(|worktree| worktree.id).collect(),
            topology: true,
            ..git_watch::Change::default()
        };
        let second = run(&fixture, Some(&first), &poll);
        let added = added.canonicalize().unwrap();
        assert!(
            second
                .worktrees
                .iter()
                .any(|worktree| worktree.path == added)
        );
        let plan = second.plan.as_ref().unwrap();
        assert!(plan.worktrees.iter().any(|worktree| worktree.root == added));
        assert!(!Arc::ptr_eq(first.plan.as_ref().unwrap(), plan));

        // Ordinary scans must not change the plan once all discovered
        // checkouts belong to it, so the monitor is not reconfigured.
        let third = run(&fixture, Some(&second), &poll);
        assert_eq!(plan, third.plan.as_ref().unwrap());
    }

    #[test]
    fn polling_replaces_a_moved_checkout_root_in_the_watch_plan() {
        let fixture = fixture();
        let first = run(&fixture, None, &git_watch::Change::default());
        let moved = fixture.root.parent().unwrap().join("moved");
        git(
            &fixture.root,
            &[
                "worktree",
                "move",
                fixture.linked.to_str().unwrap(),
                moved.to_str().unwrap(),
            ],
        );
        let second = run(
            &fixture,
            Some(&first),
            &git_watch::Change {
                topology: true,
                ..git_watch::Change::default()
            },
        );
        let plan = second.plan.as_ref().unwrap();
        let moved = moved.canonicalize().unwrap();
        assert!(plan.worktrees.iter().any(|worktree| worktree.root == moved));
        assert!(
            !plan
                .worktrees
                .iter()
                .any(|worktree| worktree.root == fixture.linked)
        );
        assert_eq!(plan.worktrees.len(), second.worktrees.len());
    }

    #[test]
    fn polling_retries_a_discovered_checkout_after_its_plan_read_failed() {
        let fixture = fixture();
        let first = run(&fixture, None, &git_watch::Change::default());
        let added = fixture.root.parent().unwrap().join("added");
        git(
            &fixture.root,
            &[
                "worktree",
                "add",
                "-q",
                "-b",
                "added",
                added.to_str().unwrap(),
            ],
        );
        // The administrative worktree list still names this checkout, but
        // its temporarily invalid marker prevents reading its watch plan.
        let marker = added.join(".git");
        let original = std::fs::read(&marker).unwrap();
        std::fs::write(&marker, "gitdir: /nonexistent/cagents-work-scan\n").unwrap();
        let failed = run(
            &fixture,
            Some(&first),
            &git_watch::Change {
                topology: true,
                ..git_watch::Change::default()
            },
        );
        assert!(failed.plan_error.is_some());
        assert!(Arc::ptr_eq(
            first.plan.as_ref().unwrap(),
            failed.plan.as_ref().unwrap()
        ));
        assert!(
            failed
                .worktrees
                .iter()
                .any(|worktree| worktree.path == added)
        );

        // A later scan no longer has a new record relative to Local. Its
        // retained plan must remain the authority for detecting the mismatch.
        std::fs::write(&marker, original).unwrap();
        let recovered = run(&fixture, Some(&failed), &git_watch::Change::default());
        assert!(recovered.plan_error.is_none());
        assert!(
            recovered
                .plan
                .as_ref()
                .unwrap()
                .worktrees
                .iter()
                .any(|worktree| worktree.root == added)
        );
    }

    #[test]
    fn polling_retries_failed_plan_when_checkout_membership_is_unchanged() {
        let fixture = fixture();
        // A session in the linked worktree has its tracked files watched.
        let in_use = BTreeSet::from([fixture.linked.clone()]);
        let run = |previous: Option<&Local>, change: &git_watch::Change| {
            run_with(&fixture, previous, change, false, &in_use)
        };
        let first = run(None, &git_watch::Change::default());
        let added = fixture.linked.join("new-directory/added.rs");
        std::fs::create_dir_all(added.parent().unwrap()).unwrap();
        std::fs::write(&added, "new tracked file\n").unwrap();
        git(&fixture.linked, &["add", "new-directory/added.rs"]);
        let marker = fixture.linked.join(".git");
        let original = std::fs::read(&marker).unwrap();
        std::fs::write(&marker, "gitdir: /nonexistent/cagents-work-scan\n").unwrap();
        let failed = run(
            Some(&first),
            &git_watch::Change {
                membership: true,
                worktrees: first.worktrees.iter().map(|worktree| worktree.id).collect(),
                ..Default::default()
            },
        );
        assert!(failed.plan_error.is_some());
        assert!(Arc::ptr_eq(
            first.plan.as_ref().unwrap(),
            failed.plan.as_ref().unwrap()
        ));
        assert_eq!(
            failed
                .worktrees
                .iter()
                .map(|worktree| (&worktree.id, &worktree.path))
                .collect::<Vec<_>>(),
            first
                .worktrees
                .iter()
                .map(|worktree| (&worktree.id, &worktree.path))
                .collect::<Vec<_>>()
        );
        std::fs::write(&marker, original).unwrap();
        // Shared refs can wake the next scan without checkout IDs. The lost
        // membership edge still needs a fresh census before its error clears.
        let recovered = run(
            Some(&failed),
            &git_watch::Change {
                references: true,
                ..Default::default()
            },
        );
        assert!(recovered.plan_error.is_none());
        let plan = recovered.plan.as_ref().unwrap();
        assert!(!Arc::ptr_eq(first.plan.as_ref().unwrap(), plan));
        assert_ne!(first.plan.as_ref().unwrap().worktrees, plan.worktrees);
        let cancellation = CancellationToken::new();
        let repository = fixture.root.join(".git");
        let records = RepositoryService::system("git")
            .list_worktrees(&repository, &cancellation)
            .unwrap();
        let interest = |id| {
            plan.worktrees
                .iter()
                .find(|worktree| worktree.id == id)
                .unwrap()
                .interest()
        };
        let fresh = git_watch::Project::read_changed_with_interest(
            plan.id,
            repository,
            &records,
            &cancellation,
            None,
            &BTreeSet::new(),
            &interest,
        )
        .unwrap();
        assert_eq!(plan.worktrees, fresh.worktrees);
        let unchanged = run(Some(&recovered), &git_watch::Change::default());
        assert!(unchanged.plan_error.is_none());
        assert!(
            Arc::ptr_eq(plan, unchanged.plan.as_ref().unwrap()),
            "successful recovery restores ordinary reuse"
        );
    }

    #[test]
    fn persistent_plan_failure_retries_only_the_checkouts_it_could_not_read() {
        let fixture = fixture();
        let first = run(&fixture, None, &git_watch::Change::default());
        let linked = first
            .worktrees
            .iter()
            .find(|worktree| worktree.path == fixture.linked)
            .unwrap()
            .id;
        let marker = fixture.linked.join(".git");
        let original = std::fs::read(&marker).unwrap();
        std::fs::write(&marker, "gitdir: /nonexistent/cagents-work-scan\n").unwrap();
        let only_linked = git_watch::Change {
            membership: true,
            worktrees: BTreeSet::from([linked]),
            ..Default::default()
        };
        let failed = run(&fixture, Some(&first), &only_linked);
        assert!(failed.plan_error.is_some());
        assert_eq!(failed.plan_retry, BTreeSet::from([linked]));

        // While the checkout stays unreadable, unrelated scans retry it alone
        // rather than rebuilding every retained census each time.
        let still = run(&fixture, Some(&failed), &git_watch::Change::default());
        assert!(still.plan_error.is_some());
        assert_eq!(still.plan_retry, BTreeSet::from([linked]));

        std::fs::write(&marker, original).unwrap();
        let recovered = run(&fixture, Some(&still), &git_watch::Change::default());
        assert!(recovered.plan_error.is_none());
        assert!(recovered.plan_retry.is_empty());
    }
}
