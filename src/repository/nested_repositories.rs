//! The containing repository's Git listing cannot see foreign worktrees.
//! Inspect ignored directories too before closing sessions or removing files.

use super::{CancellationToken, fs_safety};
use std::{
    fs::{self, ReadDir},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
struct Limits {
    entries: usize,
    depth: usize,
    elapsed: Duration,
}

const LIMITS: Limits = Limits {
    entries: 200_000,
    depth: 128,
    elapsed: Duration::from_secs(5),
};

struct Directory {
    root: fs_safety::Root,
    relative: PathBuf,
    entries: ReadDir,
    head: bool,
    objects: bool,
    refs: bool,
}

impl Directory {
    fn open(root: fs_safety::Root, relative: PathBuf) -> Result<Self, String> {
        // Enumerate the retained directory, not a pathname another process
        // can replace with a symlink while the scan is in progress.
        let entries = fs::read_dir(format!("/proc/self/fd/{}", root.descriptor()))
            .map_err(|error| format!("cannot read {}: {error}", root.path().display()))?;
        Ok(Self {
            root,
            relative,
            entries,
            head: false,
            objects: false,
            refs: false,
        })
    }
}

pub(super) fn refusal(path: &Path, cancellation: &CancellationToken) -> Option<String> {
    check(path, cancellation, LIMITS).err()
}

fn check(path: &Path, cancellation: &CancellationToken, limits: Limits) -> Result<(), String> {
    scan(path, cancellation, limits).map_err(|reason| {
        format!(
            "Cannot safely delete this worktree: {reason}. Move nested repositories out, \
             or resolve the inspection problem and retry."
        )
    })
}

fn scan(path: &Path, cancellation: &CancellationToken, limits: Limits) -> Result<(), String> {
    let started = Instant::now();
    let budget = || {
        if cancellation.is_cancelled() {
            Err("checking for nested repositories was cancelled".to_owned())
        } else if started.elapsed() >= limits.elapsed {
            Err("checking for nested repositories exceeded its time limit".to_owned())
        } else {
            Ok(())
        }
    };
    budget()?;
    let root = fs_safety::Root::open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let mut stack = vec![Directory::open(root, PathBuf::new())?];
    let mut visited = 0usize;
    while let Some(directory) = stack.last_mut() {
        budget()?;
        if visited == limits.entries {
            return Err(format!(
                "checking for nested repositories exceeded {} directory entries",
                limits.entries
            ));
        }
        let Some(entry) = directory.entries.next() else {
            let directory = stack.pop().unwrap();
            if !directory.relative.as_os_str().is_empty()
                && directory.head
                && directory.objects
                && directory.refs
            {
                return Err(format!(
                    "{} contains another repository that Git would delete with it",
                    directory.relative.display()
                ));
            }
            continue;
        };
        visited += 1;
        let entry = entry
            .map_err(|error| format!("cannot read {}: {error}", directory.root.path().display()))?;
        let name = entry.file_name();
        if name == ".git" {
            if directory.relative.as_os_str().is_empty() {
                // The selected worktree's own metadata is not nested content.
                continue;
            }
            // A broken file, directory or symlink still asserts independent
            // repository ownership. Do not need to trust or parse its target.
            return Err(format!(
                "{} contains another repository that Git would delete with it",
                directory.relative.display()
            ));
        }
        let kind = entry.file_type().map_err(|error| {
            format!(
                "cannot inspect {}: {error}",
                directory.root.path().join(&name).display()
            )
        })?;
        let container = kind.is_dir() || kind.is_symlink();
        if name == "HEAD" {
            directory.head = !kind.is_dir();
        } else if name == "objects" {
            directory.objects = container;
        } else if name == "refs" || name == "reftable" {
            directory.refs |= container;
        }
        if kind.is_dir() {
            let relative = directory.relative.join(&name);
            let child = fs_safety::open_directory_beneath(&directory.root, Path::new(&name))
                .map_err(|error| format!("cannot inspect {}: {error}", relative.display()))?;
            if stack.len() >= limits.depth {
                return Err(
                    "checking for nested repositories exceeded its directory depth limit".into(),
                );
            }
            stack.push(Directory::open(child, relative)?);
        }
        // Git removes a symlink itself; following it would inspect content
        // outside the directory being removed and could loop indefinitely.
    }
    budget()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::{fs::PermissionsExt, fs::symlink};

    #[test]
    fn ignored_directories_and_broken_repository_markers_are_preserved() {
        for marker in ["file", "directory", "symlink"] {
            let root = tempfile::tempdir().unwrap();
            let nested = root.path().join(".ignored/deep/foreign");
            fs::create_dir_all(&nested).unwrap();
            match marker {
                "file" => fs::write(nested.join(".git"), "gitdir: /missing").unwrap(),
                "directory" => fs::create_dir(nested.join(".git")).unwrap(),
                "symlink" => symlink("/missing", nested.join(".git")).unwrap(),
                _ => unreachable!(),
            }
            fs::write(nested.join("unsaved"), "retain").unwrap();
            let reason = refusal(root.path(), &CancellationToken::default()).unwrap();
            assert!(reason.contains(".ignored/deep/foreign"), "{reason}");
            assert!(reason.contains("another repository"), "{reason}");
            assert_eq!(fs::read(nested.join("unsaved")).unwrap(), b"retain");
        }
    }

    #[test]
    fn top_level_metadata_and_symlinked_directories_are_not_traversed() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join(".git/worktrees/metadata/.git")).unwrap();
        fs::create_dir(outside.path().join(".git")).unwrap();
        symlink(outside.path(), root.path().join("linked-cache")).unwrap();
        assert!(refusal(root.path(), &CancellationToken::default()).is_none());
    }

    #[test]
    fn nested_bare_repositories_are_preserved_with_both_ref_formats() {
        for refs in ["refs", "reftable"] {
            let root = tempfile::tempdir().unwrap();
            let bare = root.path().join("cache/foreign.git");
            fs::create_dir_all(bare.join("objects")).unwrap();
            fs::create_dir(bare.join(refs)).unwrap();
            fs::write(bare.join("HEAD"), "ref: refs/heads/main\n").unwrap();
            assert!(
                refusal(root.path(), &CancellationToken::default())
                    .unwrap()
                    .contains("another repository")
            );
        }
    }

    #[test]
    fn entry_depth_time_cancellation_and_read_failures_refuse_deletion() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("one/two/three")).unwrap();
        let cancellation = CancellationToken::default();
        for (limits, reason) in [
            (
                Limits {
                    entries: 1,
                    ..LIMITS
                },
                "directory entries",
            ),
            (Limits { depth: 1, ..LIMITS }, "directory depth"),
            (
                Limits {
                    elapsed: Duration::ZERO,
                    ..LIMITS
                },
                "time limit",
            ),
        ] {
            assert!(
                check(root.path(), &cancellation, limits)
                    .unwrap_err()
                    .contains(reason)
            );
        }
        cancellation.cancel();
        assert!(
            refusal(root.path(), &cancellation)
                .unwrap()
                .contains("cancelled")
        );
        let fresh = CancellationToken::default();
        assert!(refusal(&root.path().join("missing"), &fresh).is_some());
        fs::set_permissions(root.path().join("one"), fs::Permissions::from_mode(0)).unwrap();
        let unreadable = refusal(root.path(), &fresh);
        fs::set_permissions(root.path().join("one"), fs::Permissions::from_mode(0o700)).unwrap();
        if unsafe { libc::geteuid() } != 0 {
            assert!(unreadable.unwrap().contains("cannot read"));
        }
    }
}
