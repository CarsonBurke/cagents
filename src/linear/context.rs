//! Explicit worktree-local links live in Git's administrative directory. They
//! are never committed, shared with sibling worktrees, or inferred from a branch.
use super::Error;
use crate::fs_safety::{self, Root};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    fs::File,
    io::{self, Write},
    os::fd::AsRawFd,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

const LINK_FILE: &str = "cagents-linear.json";
// Git's recovery census reserves *.lock for active Git operations. This is
// a persistent advisory lease, not an outstanding Git lockfile.
const LINK_LOCK: &str = "cagents-linear.lease";
const LINK_LIMIT: u64 = 4096;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct LinkedIssue {
    pub issue_id: String,
    pub workspace: Option<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredLink {
    version: u32,
    /// Version 2 can retain an explicit unlink. Absence is reserved for a
    /// pristine worktree, so interrupted creation cannot resurrect that link.
    issue: Option<LinkedIssue>,
}

fn storage(error: impl std::fmt::Display) -> Error {
    Error::new("linear_link_storage", error)
}

fn git_directory(current: &Path) -> Result<Root, Error> {
    let output = Command::new("git")
        .current_dir(current)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .args(["rev-parse", "--is-inside-work-tree", "--absolute-git-dir"])
        .output()
        .map_err(storage)?;
    if !output.status.success() {
        return Err(Error::new(
            "linear_worktree_required",
            "Run this command inside a Git worktree",
        ));
    }
    use std::os::unix::ffi::OsStrExt;
    let bytes = output.stdout.strip_prefix(b"true\n").ok_or_else(|| {
        Error::new(
            "linear_worktree_required",
            "Linear links require a non-bare Git worktree",
        )
    })?;
    let bytes = bytes.strip_suffix(b"\n").unwrap_or(bytes);
    if bytes.is_empty() {
        return Err(storage("Git returned no administrative directory"));
    }
    Root::open(&PathBuf::from(OsStr::from_bytes(bytes))).map_err(storage)
}

pub fn resolve_current() -> Result<LinkedIssue, Error> {
    resolve_at(&std::env::current_dir().map_err(storage)?)
}

pub fn resolve_at(worktree: &Path) -> Result<LinkedIssue, Error> {
    read_link(&git_directory(worktree)?)
}

/// The ticket explicitly linked to the worktree whose Git administrative
/// directory is `git_dir`, for a caller that already knows that directory and
/// must not spawn Git per worktree. `None` when nothing is linked, including
/// after an explicit unlink.
pub fn linked_in_git_dir(git_dir: &Path) -> Result<Option<LinkedIssue>, Error> {
    let root = Root::open(git_dir).map_err(storage)?;
    Ok(read_stored_link(&root)?.and_then(|stored| stored.issue))
}

fn read_link(root: &Root) -> Result<LinkedIssue, Error> {
    read_stored_link(root)?
        .and_then(|stored| stored.issue)
        .ok_or_else(issue_required)
}

fn issue_required() -> Error {
    Error::new(
        "linear_issue_required",
        "This worktree has no linked ticket; run cagents linear link TEAM-123",
    )
}

fn read_stored_link(root: &Root) -> Result<Option<StoredLink>, Error> {
    let mut file = match fs_safety::open_beneath(root, Path::new(LINK_FILE)) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => return Ok(None),
        Err(error) => return Err(storage(error)),
    };
    if !file.metadata().map_err(storage)?.is_file() {
        return Err(storage("Linear link must be a regular file"));
    }
    let bytes = fs_safety::read_bounded(&mut file, LINK_LIMIT).map_err(storage)?;
    if bytes.len() as u64 > LINK_LIMIT {
        return Err(storage("Linear link exceeds size limit"));
    }
    let stored: StoredLink = serde_json::from_slice(&bytes).map_err(storage)?;
    if !matches!(stored.version, 1 | 2) || (stored.version == 1 && stored.issue.is_none()) {
        return Err(storage("Unsupported Linear link version"));
    }
    if let Some(issue) = &stored.issue {
        validate(issue)?;
    }
    Ok(Some(stored))
}

/// A stable lock name serializes every explicit edit with creator recovery.
/// Never unlink this file: replacing the lock inode would split authority.
struct LinkGuard(File);

impl Drop for LinkGuard {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn lock_link(root: &Root) -> Result<LinkGuard, Error> {
    // Install without replacing an existing lock, resolving every operation
    // relative to the already opened administrative directory.
    let file = match fs_safety::open_beneath(root, Path::new(LINK_LOCK)) {
        Ok(file) => file,
        Err(error) if error.is_not_found() => {
            let (file, name) = fs_safety::create_temporary_in(root, 0o600).map_err(storage)?;
            let installed = fs_safety::rename_new_within(root, &name, OsStr::new(LINK_LOCK));
            if installed.is_err() {
                let _ = fs_safety::unlink_within(root, &name);
            }
            drop(file);
            // A competing writer may have installed the same stable lock.
            fs_safety::open_beneath(root, Path::new(LINK_LOCK)).map_err(storage)?
        }
        Err(error) => return Err(storage(error)),
    };
    let metadata = file.metadata().map_err(storage)?;
    // SAFETY: geteuid has no arguments or memory-safety preconditions.
    let current_uid = unsafe { libc::geteuid() };
    if !metadata.is_file() || metadata.uid() != current_uid {
        return Err(storage("Linear link lock must be an owned regular file"));
    }
    let deadline = Instant::now() + Duration::from_secs(2);
    loop {
        // SAFETY: the file descriptor remains owned throughout the call.
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::WouldBlock && error.kind() != io::ErrorKind::Interrupted {
            return Err(storage(error));
        }
        if Instant::now() >= deadline {
            return Err(Error::new(
                "linear_link_busy",
                "Another Linear link edit is still active; retry after it finishes",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let guard = LinkGuard(file);
    let named = fs_safety::metadata_beneath(root, Path::new(LINK_LOCK)).map_err(storage)?;
    if !named.is_file() || (named.dev(), named.ino()) != (metadata.dev(), metadata.ino()) {
        return Err(storage(
            "Linear link lock changed while waiting; retry the edit",
        ));
    }
    Ok(guard)
}

fn validate(issue: &LinkedIssue) -> Result<(), Error> {
    super::issue_reference(&issue.issue_id)?;
    if let Some(workspace) = &issue.workspace {
        if workspace.is_empty()
            || !workspace
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        {
            return Err(Error::new(
                "linear_invalid_workspace",
                "Workspace aliases must contain only ASCII letters, digits, or underscores",
            ));
        }
    }
    Ok(())
}

pub fn link(issue_id: &str, workspace: Option<&str>) -> Result<LinkedIssue, Error> {
    link_at(
        &std::env::current_dir().map_err(storage)?,
        issue_id,
        workspace,
    )
}

/// Link an explicit checkout without changing the process-wide current directory.
pub fn link_at(
    worktree: &Path,
    issue_id: &str,
    workspace: Option<&str>,
) -> Result<LinkedIssue, Error> {
    let issue = linked_issue(issue_id, workspace)?;
    let root = git_directory(worktree)?;
    let _guard = lock_link(&root)?;
    write_link(&root, &issue)?;
    Ok(issue)
}

fn linked_issue(issue_id: &str, workspace: Option<&str>) -> Result<LinkedIssue, Error> {
    let issue = LinkedIssue {
        issue_id: super::issue_reference(issue_id)?,
        workspace: workspace.map(str::to_ascii_uppercase),
    };
    validate(&issue)?;
    Ok(issue)
}

/// Complete creator intent without replacing later explicit link/unlink edits.
pub fn ensure_link_at(
    worktree: &Path,
    issue_id: &str,
    workspace: Option<&str>,
) -> Result<LinkedIssue, Error> {
    let issue = linked_issue(issue_id, workspace)?;
    let root = git_directory(worktree)?;
    let _guard = lock_link(&root)?;
    ensure_link(&root, &issue)?;
    Ok(issue)
}

/// Creator authority belongs to the administrative directory retained during
/// ownership verification, not whichever checkout a path later resolves to.
#[cfg(any(feature = "ui", feature = "host", test))]
pub(crate) fn ensure_link_for_creation(
    worktree: &Path,
    expected: &Root,
    issue_id: &str,
    workspace: Option<&str>,
) -> Result<LinkedIssue, Error> {
    let issue = linked_issue(issue_id, workspace)?;
    let _guard = lock_link(expected)?;
    let actual = git_directory(worktree)?;
    let expected_identity = expected.metadata().map_err(storage)?;
    let actual_identity = actual.metadata().map_err(storage)?;
    if (expected_identity.dev(), expected_identity.ino())
        != (actual_identity.dev(), actual_identity.ino())
    {
        return Err(Error::new(
            "linear_link_context_changed",
            "The checkout's Git administrative identity changed during creation. Its Linear link was preserved; inspect the checkout before reconciling.",
        ));
    }
    // Every read/write remains relative to the proven descriptor, including
    // if an external Git edit retargets the checkout after the comparison.
    ensure_link(expected, &issue)?;
    Ok(issue)
}

fn ensure_link(root: &Root, issue: &LinkedIssue) -> Result<(), Error> {
    match read_stored_link(root)? {
        None => write_link(root, issue),
        Some(stored) if stored.issue.as_ref() == Some(issue) => {
            // A prior writer may have crashed after rename but before directory
            // sync; make the already matching record durable before Applied.
            let file = fs_safety::open_beneath(root, Path::new(LINK_FILE)).map_err(storage)?;
            file.sync_all().map_err(storage)?;
            fs_safety::sync_directory(root).map_err(storage)
        }
        Some(stored) => Err(Error::new(
            "linear_link_conflict",
            if stored.issue.is_some() {
                "A different Linear ticket or workspace was linked after creation started; the later link was preserved. Review it with cagents linear issue --current, then explicitly link the requested ticket before reconciling creation."
            } else {
                "This worktree was explicitly unlinked after creation started; that choice was preserved. Explicitly link the requested ticket before reconciling creation."
            },
        )),
    }
}

fn write_link(root: &Root, issue: &LinkedIssue) -> Result<(), Error> {
    write_stored_link(root, Some(issue))
}

fn write_stored_link(root: &Root, issue: Option<&LinkedIssue>) -> Result<(), Error> {
    let bytes = serde_json::to_vec(&StoredLink {
        version: if issue.is_some() { 1 } else { 2 },
        issue: issue.cloned(),
    })
    .map_err(storage)?;
    if bytes.len() as u64 > LINK_LIMIT {
        return Err(storage("Linear link exceeds size limit"));
    }
    let (mut file, name) = fs_safety::create_temporary_in(root, 0o600).map_err(storage)?;
    let result = (|| {
        file.write_all(&bytes).map_err(storage)?;
        file.sync_all().map_err(storage)?;
        fs_safety::rename_within(root, &name, OsStr::new(LINK_FILE)).map_err(storage)?;
        fs_safety::sync_directory(root).map_err(storage)
    })();
    if result.is_err() {
        let _ = fs_safety::unlink_within(root, &name);
    }
    result
}

pub fn unlink() -> Result<(), Error> {
    unlink_at(&std::env::current_dir().map_err(storage)?)
}

pub fn unlink_at(worktree: &Path) -> Result<(), Error> {
    let root = git_directory(worktree)?;
    let _guard = lock_link(&root)?;
    write_stored_link(&root, None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(path: &Path, args: &[&str]) {
        let output = Command::new("git")
            .current_dir(path)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    #[test]
    fn links_are_isolated_between_git_worktrees_and_survive_moves() {
        let fixture = tempfile::tempdir().unwrap();
        git(fixture.path(), &["init", "-q"]);
        git(
            fixture.path(),
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.test",
                "commit",
                "--allow-empty",
                "-qm",
                "initial",
            ],
        );
        let sibling = fixture.path().join("sibling");
        git(
            fixture.path(),
            &[
                "worktree",
                "add",
                "-qb",
                "sibling",
                sibling.to_str().unwrap(),
            ],
        );
        let main = git_directory(fixture.path()).unwrap();
        let other = git_directory(&sibling).unwrap();
        let issue = LinkedIssue {
            issue_id: "ENG-12".into(),
            workspace: Some("WORK".into()),
        };
        write_link(&main, &issue).unwrap();
        assert_eq!(read_link(&main).unwrap(), issue);
        assert_eq!(read_link(&other).unwrap_err().kind, "linear_issue_required");
        let second = LinkedIssue {
            issue_id: "ENG-13".into(),
            workspace: None,
        };
        write_link(&other, &second).unwrap();
        let moved = fixture.path().join("moved");
        git(
            fixture.path(),
            &[
                "worktree",
                "move",
                sibling.to_str().unwrap(),
                moved.to_str().unwrap(),
            ],
        );
        assert_eq!(read_link(&git_directory(&moved).unwrap()).unwrap(), second);
        assert_eq!(read_link(&main).unwrap(), issue);
        // The same links read by administrative directory, without Git.
        let admin = fixture.path().join(".git");
        assert_eq!(linked_in_git_dir(&admin).unwrap(), Some(issue));
        assert_eq!(
            linked_in_git_dir(&admin.join("worktrees/sibling")).unwrap(),
            Some(second)
        );
        let empty = tempfile::tempdir().unwrap();
        assert_eq!(linked_in_git_dir(empty.path()).unwrap(), None);
    }

    #[test]
    fn malformed_oversized_and_symlink_links_are_rejected() {
        let fixture = tempfile::tempdir().unwrap();
        let root = Root::open(fixture.path()).unwrap();
        let link = fixture.path().join(LINK_FILE);
        std::fs::write(
            &link,
            b"{\"version\":3,\"issue\":{\"issue_id\":\"ENG-1\",\"workspace\":null}}",
        )
        .unwrap();
        assert!(read_link(&root).is_err());
        let mut oversized =
            b"{\"version\":1,\"issue\":{\"issue_id\":\"ENG-1\",\"workspace\":null}}".to_vec();
        oversized.resize(LINK_LIMIT as usize + 1, b' ');
        std::fs::write(&link, oversized).unwrap();
        assert!(read_link(&root).is_err());
        std::fs::remove_file(&link).unwrap();
        let target = fixture.path().join("target");
        std::fs::write(&target, "untouched").unwrap();
        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(read_link(&root).is_err());
        write_link(
            &root,
            &LinkedIssue {
                issue_id: "ENG-1".into(),
                workspace: None,
            },
        )
        .unwrap();
        assert_eq!(std::fs::read_to_string(target).unwrap(), "untouched");
        assert_eq!(read_link(&root).unwrap().issue_id, "ENG-1");
    }

    #[test]
    fn creator_link_refuses_a_retargeted_checkout_without_touching_either_link() {
        let checkout = tempfile::tempdir().unwrap();
        let unrelated = tempfile::tempdir().unwrap();
        git(checkout.path(), &["init", "-q"]);
        git(unrelated.path(), &["init", "-q"]);
        let expected = git_directory(checkout.path()).unwrap();
        let other = git_directory(unrelated.path()).unwrap();
        let original = checkout.path().join("original-admin");
        std::fs::rename(checkout.path().join(".git"), &original).unwrap();
        std::fs::write(
            checkout.path().join(".git"),
            format!("gitdir: {}\n", other.path().display()),
        )
        .unwrap();
        let error =
            ensure_link_for_creation(checkout.path(), &expected, "ENG-123", None).unwrap_err();
        assert_eq!(error.kind, "linear_link_context_changed");
        assert!(read_stored_link(&expected).unwrap().is_none());
        assert!(read_stored_link(&other).unwrap().is_none());
        std::fs::remove_file(checkout.path().join(".git")).unwrap();
        std::fs::rename(original, checkout.path().join(".git")).unwrap();
        ensure_link_for_creation(checkout.path(), &expected, "ENG-123", None).unwrap();
        assert_eq!(resolve_at(checkout.path()).unwrap().issue_id, "ENG-123");
        assert!(read_stored_link(&other).unwrap().is_none());
    }

    #[test]
    fn creator_links_preserve_explicit_ticket_workspace_and_unlink_intent() {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "-q"]);
        let expected = LinkedIssue {
            issue_id: "ENG-123".into(),
            workspace: Some("TEST".into()),
        };
        ensure_link_at(
            directory.path(),
            &expected.issue_id,
            expected.workspace.as_deref(),
        )
        .unwrap();
        for later in [
            Some(LinkedIssue {
                issue_id: "ENG-999".into(),
                workspace: Some("TEST".into()),
            }),
            Some(LinkedIssue {
                issue_id: "ENG-123".into(),
                workspace: Some("OTHER".into()),
            }),
            None,
        ] {
            if let Some(later) = &later {
                link_at(
                    directory.path(),
                    &later.issue_id,
                    later.workspace.as_deref(),
                )
                .unwrap();
            } else {
                unlink_at(directory.path()).unwrap();
            }
            let root = git_directory(directory.path()).unwrap();
            let before = std::fs::read(root.path().join(LINK_FILE)).unwrap();
            assert_eq!(
                ensure_link_at(
                    directory.path(),
                    &expected.issue_id,
                    expected.workspace.as_deref()
                )
                .unwrap_err()
                .kind,
                "linear_link_conflict"
            );
            assert_eq!(std::fs::read(root.path().join(LINK_FILE)).unwrap(), before);
            assert_eq!(read_stored_link(&root).unwrap().unwrap().issue, later);
            if later.is_none() {
                assert_eq!(
                    resolve_at(directory.path()).unwrap_err().kind,
                    "linear_issue_required"
                );
            }
            // Explicitly choosing the requested issue reconciles the conflict.
            link_at(
                directory.path(),
                &expected.issue_id,
                expected.workspace.as_deref(),
            )
            .unwrap();
            let before = std::fs::read(root.path().join(LINK_FILE)).unwrap();
            assert_eq!(
                ensure_link_at(
                    directory.path(),
                    &expected.issue_id,
                    expected.workspace.as_deref()
                )
                .unwrap(),
                expected
            );
            assert_eq!(
                std::fs::read(root.path().join(LINK_FILE)).unwrap(),
                before,
                "same existing intent must not be rewritten"
            );
        }
    }

    #[test]
    fn creator_and_explicit_link_edits_share_one_serialization_lock() {
        for creator_first in [false, true] {
            for later in [
                Some(LinkedIssue {
                    issue_id: "ENG-999".into(),
                    workspace: None,
                }),
                Some(LinkedIssue {
                    issue_id: "ENG-123".into(),
                    workspace: Some("OTHER".into()),
                }),
                None,
            ] {
                let directory = tempfile::tempdir().unwrap();
                git(directory.path(), &["init", "-q"]);
                let root = git_directory(directory.path()).unwrap();
                let expected = LinkedIssue {
                    issue_id: "ENG-123".into(),
                    workspace: None,
                };
                let guard = lock_link(&root).unwrap();
                let path = directory.path().to_path_buf();
                let expected_worker = expected.clone();
                let later_worker = later.clone();
                let (started, ready) = std::sync::mpsc::channel();
                let worker = std::thread::spawn(move || {
                    started.send(()).unwrap();
                    if creator_first {
                        match later_worker {
                            Some(issue) => {
                                link_at(&path, &issue.issue_id, issue.workspace.as_deref())
                                    .map(|_| ())
                            }
                            None => unlink_at(&path),
                        }
                    } else {
                        ensure_link_at(
                            &path,
                            &expected_worker.issue_id,
                            expected_worker.workspace.as_deref(),
                        )
                        .map(|_| ())
                    }
                });
                ready.recv().unwrap();
                // The real lock fixes the linearization order without sleeps;
                // the worker uses the actual public API on another descriptor.
                if creator_first {
                    ensure_link(&root, &expected).unwrap();
                } else {
                    write_stored_link(&root, later.as_ref()).unwrap();
                }
                drop(guard);
                let result = worker.join().unwrap();
                if creator_first {
                    result.unwrap();
                } else {
                    assert_eq!(result.unwrap_err().kind, "linear_link_conflict");
                }
                assert_eq!(read_stored_link(&root).unwrap().unwrap().issue, later);
            }
        }
    }

    #[test]
    fn link_lock_refuses_symlinks_and_bounds_contention_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        git(directory.path(), &["init", "-q"]);
        let root = git_directory(directory.path()).unwrap();
        let outside = directory.path().join("unrelated");
        std::fs::write(&outside, "preserve").unwrap();
        std::os::unix::fs::symlink(&outside, root.path().join(LINK_LOCK)).unwrap();
        assert_eq!(
            link_at(directory.path(), "ENG-123", None).unwrap_err().kind,
            "linear_link_storage"
        );
        assert_eq!(std::fs::read_to_string(&outside).unwrap(), "preserve");
        std::fs::remove_file(root.path().join(LINK_LOCK)).unwrap();
        let guard = lock_link(&root).unwrap();
        assert_eq!(
            ensure_link_at(directory.path(), "ENG-123", None)
                .unwrap_err()
                .kind,
            "linear_link_busy"
        );
        assert_eq!(
            link_at(directory.path(), "ENG-999", None).unwrap_err().kind,
            "linear_link_busy"
        );
        assert_eq!(
            unlink_at(directory.path()).unwrap_err().kind,
            "linear_link_busy"
        );
        assert!(!root.path().join(LINK_FILE).exists());
        drop(guard);
        ensure_link_at(directory.path(), "ENG-123", None).unwrap();
    }
}
