//! Authority retained by one accepted worktree-creation journal.
//!
//! An existing source scope does not authorize an arbitrary second checkout.
//! The destination must be created through a held parent and retained before
//! Git receives it; only its verified registration can grant destination scope.

use super::*;
use std::os::unix::fs::MetadataExt;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct DirectoryWitness {
    path: PathBuf,
    device: u64,
    inode: u64,
}

fn invalid(message: impl Into<String>) -> RepositoryError {
    ValidationError::Io(message.into()).into()
}

fn filesystem(error: impl std::fmt::Display) -> RepositoryError {
    invalid(error.to_string())
}

pub(super) fn anchor(root: &fs_safety::Root) -> PathBuf {
    PathBuf::from(format!("/proc/{}/fd/{}/.", std::process::id(), root.descriptor()))
}

impl DirectoryWitness {
    fn held(root: &fs_safety::Root) -> Result<Self, RepositoryError> {
        let metadata = root.metadata().map_err(filesystem)?;
        Ok(Self { path: root.path().to_owned(), device: metadata.dev(), inode: metadata.ino() })
    }

    fn validate(&self, root: &fs_safety::Root) -> Result<(), RepositoryError> {
        if Self::held(root)? != *self {
            return Err(invalid("The retained creation directory identity changed"));
        }
        let named = fs_safety::Root::open(&self.path).map_err(filesystem)?;
        let named = Self::held(&named)?;
        if named.device != self.device || named.inode != self.inode {
            return Err(invalid("A creation directory was replaced; the replacement was not authorized"));
        }
        Ok(())
    }
}

/// Captured before creation effects. Missing components are created only below
/// this retained ancestor, including managed directories outside the source.
pub(super) struct DestinationPlan {
    ancestor: fs_safety::Root,
    witness: DirectoryWitness,
    managed_relative: PathBuf,
    destination_relative: PathBuf,
    destination: PathBuf,
}

impl DestinationPlan {
    pub(super) fn capture(
        source: &fs_safety::Root,
        managed_root: &Path,
        destination: &Path,
    ) -> Result<Self, RepositoryError> {
        if !destination_within_managed_root(destination, managed_root)
            || !managed_root.is_absolute()
            || [managed_root, destination].iter().any(|path| path.components().any(|part| {
                matches!(part, std::path::Component::ParentDir)
            }))
        {
            return Err(ValidationError::DestinationOutsideRoot.into());
        }
        let mut ancestor = managed_root.to_owned();
        while !ancestor.try_exists().map_err(filesystem)? {
            ancestor = ancestor.parent().ok_or(ValidationError::InvalidDestinationRoot)?.to_owned();
        }
        let held = if let Ok(relative) = ancestor.strip_prefix(source.path()) {
            if relative.as_os_str().is_empty() {
                source.try_clone().map_err(filesystem)?
            } else {
                fs_safety::open_directory_beneath(source, relative).map_err(filesystem)?
            }
        } else {
            fs_safety::Root::open(&ancestor).map_err(filesystem)?
        };
        let witness = DirectoryWitness::held(&held)?;
        witness.validate(&held)?;
        Ok(Self {
            managed_relative: managed_root.strip_prefix(&ancestor)
                .map_err(|_| ValidationError::DestinationOutsideRoot)?.to_owned(),
            destination_relative: destination.strip_prefix(&ancestor)
                .map_err(|_| ValidationError::DestinationOutsideRoot)?.to_owned(),
            destination: destination.to_owned(),
            ancestor: held,
            witness,
        })
    }

    pub(super) fn ensure_managed_root(&self) -> Result<(), RepositoryError> {
        self.witness.validate(&self.ancestor)?;
        let probe = self.managed_relative.join(".cagents-parent-probe");
        fs_safety::create_parent_beneath(&self.ancestor, &probe).map_err(filesystem)?;
        self.witness.validate(&self.ancestor)
    }

    /// Publish an empty, already-held directory without replacing any entry.
    /// Git may populate this owned directory; its argv never reopens the public
    /// destination name after an executor launch delay.
    pub(super) fn publish(&self) -> Result<OwnedDestination, RepositoryError> {
        self.witness.validate(&self.ancestor)?;
        let (parent, leaf) = fs_safety::create_parent_beneath(&self.ancestor, &self.destination_relative)
            .map_err(filesystem)?;
        let parent_witness = DirectoryWitness::held(&parent)?;
        parent_witness.validate(&parent)?;
        let staging = tempfile::Builder::new().prefix(".cagents-create-")
            .tempdir_in(anchor(&parent)).map_err(filesystem)?;
        // Retain ownership ourselves; cleanup below removes only an empty
        // staging directory through its original parent, never recursively.
        let staging_path = staging.keep();
        let staging_name = staging_path.file_name().ok_or_else(|| invalid("Missing staging name"))?.to_owned();
        let staging_root = fs_safety::open_directory_beneath(&parent, Path::new(&staging_name))
            .map_err(filesystem)?;
        let cleanup_parent = parent.try_clone().map_err(filesystem)?;
        let prepared = (|| {
            let (held, created) = fs_safety::create_directory_beneath(&staging_root, Path::new("checkout"), 0o777)
                .map_err(filesystem)?;
            if !created { return Err(invalid("The private creation directory was already occupied")); }
            let original = held.metadata().map_err(filesystem)?;
            parent_witness.validate(&parent)?;
            fs_safety::rename_new_between(&staging_root, OsStr::new("checkout"), &parent, &leaf)
                .map_err(filesystem)?;
            fs_safety::sync_directory(&parent).map_err(filesystem)?;
            let public = fs_safety::Root::open(&self.destination).map_err(filesystem)?;
            let published = public.metadata().map_err(filesystem)?;
            if (published.dev(), published.ino()) != (original.dev(), original.ino()) {
                return Err(invalid("The new checkout directory was replaced during publication"));
            }
            let witness = DirectoryWitness::held(&public)?;
            witness.validate(&public)?;
            Ok(OwnedDestination { root: public, witness, parent, parent_witness, leaf })
        })();
        let _ = fs_safety::remove_directory_within(&staging_root, OsStr::new("checkout"));
        let same_staging = staging_root.metadata().ok().zip(
            fs_safety::metadata_beneath(&cleanup_parent, Path::new(&staging_name)).ok()
        ).is_some_and(|(held, named)| named.is_dir()
            && (held.dev(), held.ino()) == (named.dev(), named.ino()));
        if same_staging {
            let _ = fs_safety::remove_directory_within(&cleanup_parent, &staging_name);
        }
        prepared
    }
}

pub(super) struct OwnedDestination {
    root: fs_safety::Root,
    witness: DirectoryWitness,
    parent: fs_safety::Root,
    parent_witness: DirectoryWitness,
    leaf: OsString,
}

impl OwnedDestination {
    pub(super) fn validate(&self) -> Result<(), RepositoryError> {
        self.parent_witness.validate(&self.parent)?;
        self.witness.validate(&self.root)
    }

    pub(super) fn operand(&self) -> Result<PathBuf, RepositoryError> {
        self.validate()?;
        Ok(anchor(&self.root))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn destination_publication_never_replaces_an_existing_entry() {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        fs::create_dir(&source_path).unwrap();
        let source = fs_safety::Root::open(&source_path).unwrap();
        let managed = source_path.join(".worktrees");
        let destination = managed.join("feature/topic");
        let plan = DestinationPlan::capture(&source, &managed, &destination).unwrap();
        assert!(!managed.exists(), "capturing creation authority is read-only");
        plan.ensure_managed_root().unwrap();
        fs::create_dir_all(&destination).unwrap();
        fs::write(destination.join("keep"), "foreign work\n").unwrap();
        assert!(plan.publish().is_err());
        assert_eq!(fs::read(destination.join("keep")).unwrap(), b"foreign work\n");
        assert_eq!(fs::read_dir(destination.parent().unwrap()).unwrap().count(), 1,
            "failed no-replace publication removes only its own empty staging directory");
    }

    #[test]
    fn held_destination_operand_cannot_write_a_replaced_source_or_managed_parent() {
        for external in [false, true] {
            let fixture = tempfile::tempdir().unwrap();
            let source_path = fixture.path().join("source");
            fs::create_dir(&source_path).unwrap();
            let source = fs_safety::Root::open(&source_path).unwrap();
            let managed = if external { fixture.path().join("external") }
                else { source_path.join(".worktrees") };
            let destination = managed.join("topic");
            let plan = DestinationPlan::capture(&source, &managed, &destination).unwrap();
            plan.ensure_managed_root().unwrap();
            let owned = plan.publish().unwrap();
            let operand = owned.operand().unwrap();
            let replaced = if external { &managed } else { &source_path };
            let parked = fixture.path().join("parked");
            fs::rename(replaced, &parked).unwrap();
            fs::create_dir_all(&destination).unwrap();
            fs::write(destination.join("keep"), "replacement\n").unwrap();
            // Model an inner executor that delays AFTER the caller validated
            // its command. The already-issued operand still addresses the
            // original held inode, while future authority reads refuse.
            fs::write(operand.join("created"), "owned output\n").unwrap();
            assert!(owned.validate().is_err());
            assert!(!destination.join("created").exists());
            assert_eq!(fs::read(destination.join("keep")).unwrap(), b"replacement\n");
            let original_destination = if external { parked.join("topic") }
                else { parked.join(".worktrees/topic") };
            assert_eq!(fs::read(original_destination.join("created")).unwrap(), b"owned output\n");
        }
    }

    #[test]
    fn created_destination_replacement_refuses_new_authority_without_touching_it() {
        let fixture = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        fs::create_dir(&source_path).unwrap();
        let source = fs_safety::Root::open(&source_path).unwrap();
        let managed = source_path.join(".worktrees");
        let destination = managed.join("topic");
        let plan = DestinationPlan::capture(&source, &managed, &destination).unwrap();
        plan.ensure_managed_root().unwrap();
        let owned = plan.publish().unwrap();
        let parked = managed.join("original-topic");
        fs::rename(&destination, &parked).unwrap();
        fs::create_dir(&destination).unwrap();
        fs::write(destination.join("keep"), "replacement\n").unwrap();
        assert!(owned.operand().is_err());
        assert_eq!(fs::read(destination.join("keep")).unwrap(), b"replacement\n");
        assert!(fs::read_dir(&parked).unwrap().next().is_none());
    }
}
