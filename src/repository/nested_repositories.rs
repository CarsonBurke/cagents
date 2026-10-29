//! The containing repository's Git listing cannot see foreign worktrees.
//! Inspect ignored directories too before closing sessions or removing files.

use super::{CancellationToken, fs_safety};
use std::{
    ffi::OsString,
    os::unix::fs::MetadataExt,
    path::{Component, Path, PathBuf},
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

/// The selected pathname is observed beneath a retained filesystem root.
/// A missing selection must stay absent; an existing selection must keep its
/// directory identity while the scan operates on retained descriptors.
struct SelectedDirectory {
    anchor: fs_safety::Root,
    relative: PathBuf,
    identity: Option<(u64, u64)>,
}

impl SelectedDirectory {
    fn capture(path: &Path, allow_absent: bool) -> Result<(Self, Option<fs_safety::Root>), String> {
        let relative = path
            .strip_prefix("/")
            .map_err(|_| "the selected worktree path is not absolute".to_owned())?;
        if relative.as_os_str().is_empty()
            || relative
                .components()
                .any(|part| !matches!(part, Component::Normal(_)))
        {
            return Err("the selected worktree path is not a normalized directory".into());
        }
        let anchor = fs_safety::Root::open(Path::new("/"))
            .map_err(|error| format!("cannot retain the worktree's filesystem root: {error}"))?;
        let identity = match fs_safety::metadata_beneath(&anchor, relative) {
            Ok(_) if allow_absent => {
                return Err("the prunable selected worktree directory is present; refresh its ownership before deleting".into());
            }
            Ok(metadata) if metadata.is_dir() => Some((metadata.dev(), metadata.ino())),
            Ok(_) => return Err("the selected worktree is not a real directory".into()),
            Err(error) if allow_absent && error.is_not_found() => None,
            Err(error) => return Err(format!("cannot inspect the selected worktree: {error}")),
        };
        let root = if let Some(identity) = identity {
            let root = fs_safety::open_directory_beneath(&anchor, relative)
                .map_err(|error| format!("cannot open the selected worktree: {error}"))?;
            let metadata = root
                .metadata()
                .map_err(|error| format!("cannot identify the selected worktree: {error}"))?;
            if (metadata.dev(), metadata.ino()) != identity {
                return Err("the selected worktree directory changed before inspection".into());
            }
            Some(root)
        } else {
            None
        };
        Ok((
            Self {
                anchor,
                relative: relative.to_path_buf(),
                identity,
            },
            root,
        ))
    }

    fn verify(&self) -> Result<(), String> {
        match (self.identity, fs_safety::metadata_beneath(&self.anchor, &self.relative)) {
            (None, Err(error)) if error.is_not_found() => Ok(()),
            (Some(identity), Ok(metadata)) if metadata.is_dir() && (metadata.dev(), metadata.ino()) == identity => Ok(()),
            _ => Err("the selected worktree directory changed during inspection; refresh before deleting".into()),
        }
    }
}

struct Directory {
    root: fs_safety::Root,
    relative: PathBuf,
    entries: std::vec::IntoIter<OsString>,
    head: bool,
    objects: bool,
    refs: bool,
}

impl Directory {
    fn open(
        root: fs_safety::Root,
        relative: PathBuf,
        remaining: &mut usize,
    ) -> Result<Self, String> {
        // The shared API enumerates the retained descriptor on each platform.
        // Charge every fetched name immediately, including pending names in
        // ancestor frames, so DFS depth cannot multiply the global allocation.
        let entries = fs_safety::directory_entries_beneath(&root, Path::new(""), *remaining)
            .map_err(|error| {
                format!(
                    "cannot read {} within {remaining} remaining directory entries: {error}",
                    root.path().display()
                )
            })?;
        *remaining = (*remaining).checked_sub(entries.len()).ok_or_else(|| {
            "checking for nested repositories exceeded its entry limit".to_owned()
        })?;
        Ok(Self {
            root,
            relative,
            entries: entries.into_iter(),
            head: false,
            objects: false,
            refs: false,
        })
    }
}

pub(super) fn refusal(
    path: &Path,
    cancellation: &CancellationToken,
    allow_absent: bool,
) -> Option<String> {
    check(path, cancellation, LIMITS, allow_absent).err()
}

fn check(
    path: &Path,
    cancellation: &CancellationToken,
    limits: Limits,
    allow_absent: bool,
) -> Result<(), String> {
    scan(path, cancellation, limits, allow_absent).map_err(|reason| {
        format!(
            "Cannot safely delete this worktree: {reason}. Move nested repositories out, \
             or resolve the inspection problem and retry."
        )
    })
}

fn scan(
    path: &Path,
    cancellation: &CancellationToken,
    limits: Limits,
    allow_absent: bool,
) -> Result<(), String> {
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
    let (selection, root) = SelectedDirectory::capture(path, allow_absent)?;
    budget()?;
    let Some(root) = root else {
        // Only the caller's exact prunable registration permits this path.
        // A second no-follow observation must still confirm actual absence.
        selection.verify()?;
        return budget();
    };
    let mut remaining = limits.entries;
    let mut stack = vec![Directory::open(root, PathBuf::new(), &mut remaining)?];
    while let Some(directory) = stack.last_mut() {
        budget()?;
        let Some(name) = directory.entries.next() else {
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
        let metadata =
            fs_safety::metadata_beneath(&directory.root, Path::new(&name)).map_err(|error| {
                format!(
                    "cannot inspect {}: {error}",
                    directory.root.path().join(&name).display()
                )
            })?;
        let kind = metadata.file_type();
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
            let opened = child
                .metadata()
                .map_err(|error| format!("cannot identify {}: {error}", relative.display()))?;
            if (metadata.dev(), metadata.ino()) != (opened.dev(), opened.ino()) {
                return Err(format!("{} changed during inspection", relative.display()));
            }
            if stack.len() >= limits.depth {
                return Err(
                    "checking for nested repositories exceeded its directory depth limit".into(),
                );
            }
            budget()?;
            stack.push(Directory::open(child, relative, &mut remaining)?);
        }
        // Git removes a symlink itself; following it would inspect content
        // outside the directory being removed and could loop indefinitely.
    }
    budget()?;
    selection.verify()?;
    budget()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    fn refusal(path: &Path, cancellation: &CancellationToken) -> Option<String> {
        super::refusal(path, cancellation, false)
    }
    fn check(path: &Path, cancellation: &CancellationToken, limits: Limits) -> Result<(), String> {
        super::check(path, cancellation, limits, false)
    }
    fn directory() -> tempfile::TempDir {
        tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap()
    }

    #[test]
    fn selected_absence_requires_authority_and_stays_bound_to_its_observation() {
        let root = directory();
        let selected = root.path().join("selected");
        let cancellation = CancellationToken::default();
        assert!(super::refusal(&selected, &cancellation, false).is_some());
        assert!(super::refusal(&selected, &cancellation, true).is_none());
        let (absent, opened) = SelectedDirectory::capture(&selected, true).unwrap();
        assert!(opened.is_none());
        fs::create_dir(&selected).unwrap();
        fs::write(selected.join("replacement"), "retain").unwrap();
        assert!(absent.verify().is_err());
        assert!(super::refusal(&selected, &cancellation, true).is_some());
        assert_eq!(fs::read(selected.join("replacement")).unwrap(), b"retain");

        let (present, opened) = SelectedDirectory::capture(&selected, false).unwrap();
        assert!(opened.is_some());
        fs::rename(&selected, root.path().join("original")).unwrap();
        fs::create_dir(&selected).unwrap();
        assert!(present.verify().is_err());
        fs::remove_dir(&selected).unwrap();
        symlink(root.path().join("missing-target"), &selected).unwrap();
        assert!(super::refusal(&selected, &cancellation, true).is_some());
        fs::remove_file(&selected).unwrap();
        cancellation.cancel();
        assert!(
            super::refusal(&selected, &cancellation, true)
                .unwrap()
                .contains("cancelled")
        );
    }

    #[test]
    fn pending_directory_names_share_one_global_entry_budget() {
        let root = directory();
        for child in ["first", "second"] {
            fs::create_dir(root.path().join(child)).unwrap();
            fs::write(root.path().join(child).join("file"), "retain").unwrap();
        }
        let reason = check(
            root.path(),
            &CancellationToken::default(),
            Limits {
                entries: 3,
                ..LIMITS
            },
        )
        .unwrap_err();
        assert!(reason.contains("directory entries"), "{reason}");
    }

    #[test]
    fn ignored_directories_and_broken_repository_markers_are_preserved() {
        for marker in ["file", "directory", "symlink"] {
            let root = directory();
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
        let root = directory();
        let outside = directory();
        fs::create_dir_all(root.path().join(".git/worktrees/metadata/.git")).unwrap();
        fs::create_dir(outside.path().join(".git")).unwrap();
        symlink(outside.path(), root.path().join("linked-cache")).unwrap();
        assert!(refusal(root.path(), &CancellationToken::default()).is_none());
    }

    #[test]
    fn nested_bare_repositories_are_preserved_with_both_ref_formats() {
        for refs in ["refs", "reftable"] {
            let root = directory();
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
        let root = directory();
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
