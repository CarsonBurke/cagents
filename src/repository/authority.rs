//! Original physical Git roots shared by creator, readers, and receiver.
use super::*;
mod scope;
pub use scope::WorktreeAuthority;
pub(super) use scope::inherited_directory_scope;
use scope::{descriptor_path, open_metadata_root, resolve_private_git_dir, resolve_common_git_dir};

// Fresh capture belongs to the actual original-source producer, not the
// minimal imported host receiver core, which accepts only its original roles.
impl WorktreeAuthority {
    pub fn capture(path: &Path) -> Result<Self, RepositoryError> {
        if let Some(reason) = Self::unsupported_reason() {
            return Err(ValidationError::Io(reason.into()).into());
        }
        if !path.is_absolute() {
            return Err(ValidationError::InvalidWorktree(path.display().to_string()).into());
        }
        let root =
            fs_safety::Root::open(path).map_err(|error| ValidationError::Io(error.to_string()))?;
        let git_dir = open_metadata_root(&resolve_private_git_dir(&descriptor_path(&root))?)?;
        let common_dir = open_metadata_root(&resolve_common_git_dir(&descriptor_path(&git_dir))?)?;
        Self::from_retained_roots(root, git_dir, common_dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // These are filesystem scope contracts, not Git command/creation evidence.
    fn checkout(parent: &Path, name: &str) -> PathBuf {
        let root = parent.join(name);
        fs::create_dir_all(root.join(".git")).unwrap();
        fs::canonicalize(root).unwrap()
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn cloned_original_scope_refuses_replaced_checkout_and_private_metadata() {
        for replace_private in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let root = checkout(directory.path(), "original");
            let original = WorktreeAuthority::capture(&root).unwrap();
            let retained = original.clone();
            let other = WorktreeAuthority::capture(&checkout(directory.path(), "other")).unwrap();
            assert!(original.same_scope(&retained));
            assert!(!original.same_scope(&other));
            assert!(Arc::ptr_eq(&original.retained_worktree_root(), &retained.retained_worktree_root()));
            let replaced = if replace_private { root.join(".git") } else { root.clone() };
            fs::rename(&replaced, replaced.with_extension("saved")).unwrap();
            fs::create_dir_all(&replaced).unwrap();
            // A pure comparison preserves the original identity; only worker
            // validation may determine whether the name still names it.
            assert!(original.same_scope(&retained));
            assert!(original.validate().is_err());
            assert!(retained.validate().is_err());
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn original_scope_refuses_a_sibling_private_locator_in_the_same_common_directory() {
        let directory = tempfile::tempdir().unwrap();
        let root = checkout(directory.path(), "primary");
        let linked = directory.path().join("linked");
        fs::create_dir(&linked).unwrap();
        let common = root.join(".git");
        let private = common.join("worktrees/original");
        let sibling = common.join("worktrees/sibling");
        for admin in [&private, &sibling] {
            fs::create_dir_all(admin).unwrap();
            fs::write(admin.join("commondir"), "../..\n").unwrap();
        }
        fs::write(linked.join(".git"), format!("gitdir: {}\n", private.display())).unwrap();
        let original = WorktreeAuthority::capture(&linked).unwrap();
        fs::write(linked.join(".git"), format!("gitdir: {}\n", sibling.display())).unwrap();
        let redirected = WorktreeAuthority::capture(&linked).unwrap();
        assert!(!original.same_scope(&redirected));
        assert!(original.validate().is_err());
        assert!(redirected.validate().is_ok());
    }
}
