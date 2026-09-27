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
}
