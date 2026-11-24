use std::{
    fs::{self, Metadata},
    io,
    path::Path,
};

/// Whether a proposed `.git` marker indicates a checkout. Git discovery
/// skips a directory without `HEAD`. Files, symlinks (even dangling ones),
/// and unexpected entry types still reserve the path as Git metadata.
///
/// # Errors
///
/// An inspection error other than absence is returned to the caller.
pub fn present(marker: &Path) -> io::Result<bool> {
    inspect(marker, |path| match fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    })
}

/// Inspect the same marker through a pinned directory and keep traversal
/// confined even if an intermediate component is replaced by a symlink.
pub(crate) fn present_beneath(
    root: &crate::fs_safety::Root,
    marker: &Path,
) -> Result<bool, crate::fs_safety::ResolveError> {
    inspect(marker, |path| {
        match crate::fs_safety::metadata_beneath(root, path) {
            Ok(metadata) => Ok(Some(metadata)),
            Err(error) if error.is_not_found() => Ok(None),
            Err(error) => Err(error),
        }
    })
}

fn inspect<E>(
    marker: &Path,
    mut metadata: impl FnMut(&Path) -> Result<Option<Metadata>, E>,
) -> Result<bool, E> {
    match metadata(marker)? {
        None => Ok(false),
        Some(entry) if entry.is_dir() => Ok(metadata(&marker.join("HEAD"))?.is_some()),
        Some(_) => Ok(true),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_directory_is_skipped_but_head_gitfile_and_symlink_count() {
        let directory = tempfile::tempdir().unwrap();
        let root = crate::fs_safety::Root::open(directory.path()).unwrap();
        let name = Path::new(".git");
        let marker = directory.path().join(name);
        let check = |expected| {
            assert_eq!(present(&marker).unwrap(), expected);
            assert_eq!(present_beneath(&root, name).unwrap(), expected);
        };

        check(false);
        fs::create_dir(&marker).unwrap();
        check(false);
        fs::write(marker.join("HEAD"), "ref: refs/heads/main\n").unwrap();
        check(true);
        fs::remove_dir_all(&marker).unwrap();
        fs::write(&marker, "gitdir: ../repo/.git/worktrees/linked\n").unwrap();
        check(true);
        fs::remove_file(&marker).unwrap();
        std::os::unix::fs::symlink("absent", &marker).unwrap();
        check(true);
    }
}
