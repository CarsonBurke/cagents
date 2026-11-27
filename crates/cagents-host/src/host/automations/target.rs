//! Consent follows a canonical directory object, not merely its old pathname.
//! How a directory is opened, and what makes it part of a project, are the
//! agent service's: `agent_execution::working_directory`.
use std::{fs::File, io, path::Path, sync::Arc};

use crate::{automations::DirectoryIdentity, host::agent_execution::working_directory};

fn open(path: &Path) -> io::Result<(Arc<File>, DirectoryIdentity)> {
    let directory = working_directory::open(path)
        .map_err(|error| io::Error::other(format!("Automation target cannot be used: {error}")))?;
    let identity = working_directory::identity(&directory).map_err(io::Error::other)?;
    Ok((
        directory,
        DirectoryIdentity {
            device: identity.device,
            inode: identity.inode,
        },
    ))
}

pub(super) fn capture(path: &Path) -> io::Result<DirectoryIdentity> {
    open(path).map(|(_, identity)| identity)
}

pub(super) fn verify(path: &Path, expected: &DirectoryIdentity) -> io::Result<Arc<File>> {
    let (directory, identity) = open(path)?;
    if identity != *expected {
        return Err(io::Error::other(
            "Automation target was replaced; review and save its target again before running",
        ));
    }
    Ok(directory)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::MetadataExt;

    #[test]
    fn same_path_replacement_and_symlink_do_not_inherit_directory_consent() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path().canonicalize().unwrap();
        let path = root.join("worktree");
        std::fs::create_dir(&path).unwrap();
        let identity = capture(&path).unwrap();
        let held = verify(&path, &identity).unwrap();
        std::fs::rename(&path, root.join("old")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(verify(&path, &identity).is_err());
        // The already checked handle still names the original target.
        assert_eq!(held.metadata().unwrap().ino(), identity.inode);
        std::fs::remove_dir(&path).unwrap();
        std::os::unix::fs::symlink(root.join("old"), &path).unwrap();
        assert!(verify(&path, &identity).is_err());
    }
}
