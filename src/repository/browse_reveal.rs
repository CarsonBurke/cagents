//! Exact-path supplementation for an explicitly revealed capped file listing.

use super::{RepoPath, WorkingFiles};
use crate::fs_safety;
use std::path::Path;

/// Include only an entry verified beneath this worktree. For a deleted file,
/// include its nearest existing parent; a root-level deletion reveals the root
/// without inventing a child or treating a missing path as an existing file.
pub fn include(
    worktree: &Path,
    files: &mut WorkingFiles,
    requested: &RepoPath,
) -> Result<Option<RepoPath>, String> {
    let root = fs_safety::Root::open(worktree).map_err(|error| error.to_string())?;
    for candidate in requested.as_path().ancestors() {
        if candidate.as_os_str().is_empty() {
            return Ok(None);
        }
        let metadata = match fs_safety::metadata_beneath(&root, candidate) {
            Ok(metadata) => metadata,
            Err(error) if error.is_not_found() => continue,
            Err(error) => return Err(error.to_string()),
        };
        let candidate = RepoPath::parse(candidate).map_err(|error| error.to_string())?;
        let list = if metadata.is_dir() {
            &mut files.directories
        } else if metadata.is_file() || metadata.file_type().is_symlink() {
            &mut files.paths
        } else {
            return Err("the requested path is not a file or directory".into());
        };
        if let Err(index) = list.binary_search(&candidate) {
            list.insert(index, candidate.clone());
        }
        return Ok(Some(candidate));
    }
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn capped_reveal_adds_only_the_verified_leaf_and_preserves_listing_metadata() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("outside/capped")).unwrap();
        fs::write(root.path().join("outside/capped/target.txt"), "target").unwrap();
        let mut files = WorkingFiles {
            paths: vec![RepoPath::parse("first.txt").unwrap()],
            tracked_count: 1,
            ..Default::default()
        };
        let target = RepoPath::parse("outside/capped/target.txt").unwrap();
        for _ in 0..2 {
            assert_eq!(
                include(root.path(), &mut files, &target).unwrap(),
                Some(target.clone())
            );
        }
        assert_eq!(files.paths.len(), 2);
        assert_eq!(files.tracked_count, 1);
        assert_eq!(files.paths[1], target);
    }

    #[test]
    fn deleted_path_reveals_the_existing_parent_including_an_empty_directory() {
        let root = tempfile::tempdir().unwrap();
        fs::create_dir_all(root.path().join("parent/empty")).unwrap();
        let mut files = WorkingFiles::default();
        let requested = RepoPath::parse("parent/empty/missing/deleted.txt").unwrap();
        let parent = RepoPath::parse("parent/empty").unwrap();
        assert_eq!(
            include(root.path(), &mut files, &requested).unwrap(),
            Some(parent.clone())
        );
        assert_eq!(files.directories, [parent]);
        assert!(files.paths.is_empty());
        assert_eq!(
            include(
                root.path(),
                &mut files,
                &RepoPath::parse("deleted.txt").unwrap()
            )
            .unwrap(),
            None
        );
    }

    #[test]
    fn reveal_never_walks_a_symlinked_parent_outside_the_worktree() {
        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), "secret").unwrap();
        std::os::unix::fs::symlink(outside.path(), root.path().join("link")).unwrap();
        let mut files = WorkingFiles::default();
        assert!(
            include(
                root.path(),
                &mut files,
                &RepoPath::parse("link/secret.txt").unwrap()
            )
            .is_err()
        );
        assert!(files.paths.is_empty() && files.directories.is_empty());
    }
}
