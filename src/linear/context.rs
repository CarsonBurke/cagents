//! Explicit worktree-local links live in Git's administrative directory. They
//! are never committed, shared with sibling worktrees, or inferred from a branch.
use super::Error;
use crate::fs_safety::{self, Root};
use serde::{Deserialize, Serialize};
use std::{
    ffi::OsStr,
    io::Write,
    path::{Path, PathBuf},
    process::Command,
};

const LINK_FILE: &str = "cagents-linear.json";
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
    issue: LinkedIssue,
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

fn read_link(root: &Root) -> Result<LinkedIssue, Error> {
    let mut file = fs_safety::open_beneath(root, Path::new(LINK_FILE)).map_err(|error| {
        if error.is_not_found() {
            Error::new(
                "linear_issue_required",
                "This worktree has no linked ticket; run cagents linear link TEAM-123",
            )
        } else {
            storage(error)
        }
    })?;
    let bytes = fs_safety::read_bounded(&mut file, LINK_LIMIT).map_err(storage)?;
    if bytes.len() as u64 > LINK_LIMIT {
        return Err(storage("Linear link exceeds size limit"));
    }
    let stored: StoredLink = serde_json::from_slice(&bytes).map_err(storage)?;
    if stored.version != 1 {
        return Err(storage("Unsupported Linear link version"));
    }
    validate(&stored.issue)?;
    Ok(stored.issue)
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
    link_at(&std::env::current_dir().map_err(storage)?, issue_id, workspace)
}

/// Link an explicit checkout without changing the process-wide current directory.
pub fn link_at(worktree: &Path, issue_id: &str, workspace: Option<&str>) -> Result<LinkedIssue, Error> {
    let issue = LinkedIssue {
        issue_id: super::issue_reference(issue_id)?,
        workspace: workspace.map(str::to_ascii_uppercase),
    };
    validate(&issue)?;
    write_link(&git_directory(worktree)?, &issue)?;
    Ok(issue)
}

fn write_link(root: &Root, issue: &LinkedIssue) -> Result<(), Error> {
    let bytes = serde_json::to_vec(&StoredLink {
        version: 1,
        issue: issue.clone(),
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
    let root = git_directory(&std::env::current_dir().map_err(storage)?)?;
    match fs_safety::unlink_within(&root, OsStr::new(LINK_FILE)) {
        Ok(()) => fs_safety::sync_directory(&root).map_err(storage),
        Err(error) if error.is_not_found() => Ok(()),
        Err(error) => Err(storage(error)),
    }
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
    }

    #[test]
    fn malformed_oversized_and_symlink_links_are_rejected() {
        let fixture = tempfile::tempdir().unwrap();
        let root = Root::open(fixture.path()).unwrap();
        let link = fixture.path().join(LINK_FILE);
        std::fs::write(
            &link,
            b"{\"version\":2,\"issue\":{\"issue_id\":\"ENG-1\",\"workspace\":null}}",
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
}
