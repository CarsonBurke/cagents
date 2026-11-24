//! Consent follows a canonical directory object, not merely its old pathname.
use std::{
    fs::{File, OpenOptions},
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

use crate::automations::DirectoryIdentity;

fn open(path: &Path) -> io::Result<File> {
    if !path.is_absolute() || std::fs::canonicalize(path)? != path {
        return Err(io::Error::other(
            "Automation target is no longer its canonical directory",
        ));
    }
    OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
}

pub(super) fn capture(path: &Path) -> io::Result<DirectoryIdentity> {
    let metadata = open(path)?.metadata()?;
    Ok(DirectoryIdentity {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

pub(super) fn verify(path: &Path, expected: &DirectoryIdentity) -> io::Result<File> {
    let directory = open(path)?;
    let metadata = directory.metadata()?;
    if metadata.dev() != expected.device || metadata.ino() != expected.inode {
        return Err(io::Error::other(
            "Automation target was replaced; review and save its target again before running",
        ));
    }
    Ok(directory)
}

/// Check project membership without Git discovery changing a durable Folder
/// project into a Git project. Linked worktrees may live outside the main root.
pub(super) fn project(
    directory: &Path,
    repository: &Path,
    kind: crate::model::ProjectKind,
    executor: &dyn crate::process::ProcessExecutor,
) -> Result<(), String> {
    use crate::process::CommandSpec;
    use std::{ffi::OsString, os::unix::ffi::OsStringExt, time::Duration};

    let repository = std::fs::canonicalize(repository).map_err(|e| e.to_string())?;
    if kind == crate::model::ProjectKind::Folder {
        return if directory.starts_with(&repository) {
            Ok(())
        } else {
            Err("The automation directory is outside its folder project".into())
        };
    }
    let mut command = CommandSpec::new("git")
        .args(["rev-parse", "--path-format=absolute", "--git-common-dir"])
        .cwd(directory)
        .timeout(Duration::from_secs(5))
        .output_limits(16 * 1024, 16 * 1024);
    // Discovery must describe this directory, not a caller's Git overrides.
    for name in [
        "GIT_DIR",
        "GIT_WORK_TREE",
        "GIT_COMMON_DIR",
        "GIT_INDEX_FILE",
        "GIT_OBJECT_DIRECTORY",
        "GIT_ALTERNATE_OBJECT_DIRECTORIES",
        "GIT_CEILING_DIRECTORIES",
        "GIT_DISCOVERY_ACROSS_FILESYSTEM",
    ] {
        command = command.env_remove(name);
    }
    let output = executor
        .execute(&command, &crate::process::CancellationToken::new())
        .map_err(|error| format!("Cannot verify the automation worktree: {error}"))?;
    let mut bytes = output.stdout;
    if bytes.last() == Some(&b'\n') {
        bytes.pop();
    }
    let common = std::path::PathBuf::from(OsString::from_vec(bytes));
    if !common.is_absolute()
        || std::fs::canonicalize(common).map_err(|e| e.to_string())? != repository
    {
        return Err("The automation worktree no longer belongs to its selected project".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_path_replacement_and_symlink_do_not_inherit_directory_consent() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("worktree");
        std::fs::create_dir(&path).unwrap();
        let identity = capture(&path).unwrap();
        let held = verify(&path, &identity).unwrap();
        std::fs::rename(&path, root.path().join("old")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(verify(&path, &identity).is_err());
        // The already checked handle still names the original target.
        assert_eq!(held.metadata().unwrap().ino(), identity.inode);
        std::fs::remove_dir(&path).unwrap();
        std::os::unix::fs::symlink(root.path().join("old"), &path).unwrap();
        assert!(verify(&path, &identity).is_err());
    }

    #[test]
    fn folder_membership_remains_explicit_after_git_init_and_linked_git_roots_match() {
        use crate::{
            model::ProjectKind,
            process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor},
        };
        let root = tempfile::tempdir().unwrap();
        let repository = root.path().join("project");
        std::fs::create_dir(&repository).unwrap();
        let executor = SystemProcessExecutor;
        let git = |arguments: &[&str]| {
            executor
                .execute(
                    &CommandSpec::new("git")
                        .args(arguments.iter().copied())
                        .cwd(&repository),
                    &CancellationToken::new(),
                )
                .unwrap()
        };
        git(&["init", "--quiet"]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "--quiet",
            "--allow-empty",
            "-m",
            "initial",
        ]);
        let linked = root.path().join("linked");
        git(&[
            "worktree",
            "add",
            "--quiet",
            "-b",
            "linked",
            linked.to_str().unwrap(),
        ]);
        assert!(project(&repository, &repository, ProjectKind::Folder, &executor).is_ok());
        assert!(project(&linked, &repository, ProjectKind::Folder, &executor).is_err());
        assert!(
            project(
                &linked,
                &repository.join(".git"),
                ProjectKind::Git,
                &executor
            )
            .is_ok()
        );
        assert!(
            project(
                root.path(),
                &repository.join(".git"),
                ProjectKind::Git,
                &executor
            )
            .is_err()
        );
    }
}
