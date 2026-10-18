//! Save-only recovery bound to the actual edited parent. The original named
//! source, retained source-relative parent resolution and cleanup leases remain
//! separate fences from that parent's filesystem/mount. Nested known checkout
//! ancestors are part of the user's selected edit scope, not a placement veto.
//!
//! Only the existing factory creates fresh Published holdings. This module
//! neither creates payloads nor adopts existing sibling directories. Context
//! owners retain fresh sticky-sibling receipts through explicit leaf/sibling
//! cleanup. External recursive deletion and process death remain limits.
use crate::{
    fs_safety::{
        DirectoryNamespace, OwnedPrivateSibling, RecoveryDirectoryStamp, Root, StickyParent,
        open_parent_beneath, recovery_directory_stamp, retained_parent_directory,
    },
    worktree_cleanup::Protection,
};
use std::{
    collections::HashSet,
    ffi::{OsStr, OsString},
    io,
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

const MAX_ANCESTORS: usize = 256;

fn refused(message: &str) -> io::Error {
    io::Error::other(message)
}

/// Original document scope plus an independently protected actual edited
/// parent. The surrounding context separately retains source cleanup leases.
#[derive(Debug)]
pub(super) struct SaveTargetAuthority {
    source: Arc<Root>,
    source_stamp: RecoveryDirectoryStamp,
    relative_path: PathBuf,
    parent: Arc<Root>,
    parent_stamp: RecoveryDirectoryStamp,
    name: OsString,
    protection: Protection,
    retained_descriptors: usize,
}
impl SaveTargetAuthority {
    pub(super) fn acquire(source: Arc<Root>, relative_path: &Path) -> io::Result<Arc<Self>> {
        Self::acquire_with_protection(source, relative_path, |parent| {
            crate::worktree_cleanup::protect_root_operation(parent)
        })
    }
    fn acquire_with_protection(
        source: Arc<Root>,
        relative_path: &Path,
        protect: impl FnOnce(&Root) -> io::Result<Protection>,
    ) -> io::Result<Arc<Self>> {
        let source_stamp = recovery_directory_stamp(&source)?;
        let (parent, name) =
            open_parent_beneath(&source, relative_path).map_err(io::Error::other)?;
        let parent = Arc::new(parent);
        let parent_stamp = recovery_directory_stamp(&parent)?;
        let protection = protect(&parent)?;
        let retained_descriptors = HashSet::from([source.descriptor(), parent.descriptor()]).len()
            + protection.retained_descriptor_count();
        let target = Arc::new(Self {
            source,
            source_stamp,
            relative_path: relative_path.to_owned(),
            parent,
            parent_stamp,
            name,
            protection,
            retained_descriptors,
        });
        target.validate()?;
        Ok(target)
    }
    pub(super) fn source(&self) -> &Arc<Root> {
        &self.source
    }
    pub(super) fn parent(&self) -> &Arc<Root> {
        &self.parent
    }
    pub(super) fn name(&self) -> &OsStr {
        &self.name
    }
    pub(super) fn relative_path(&self) -> &Path {
        &self.relative_path
    }
    pub(super) fn validate(&self) -> io::Result<()> {
        if recovery_directory_stamp(&self.source)? != self.source_stamp
            || recovery_directory_stamp(&self.parent)? != self.parent_stamp
        {
            return Err(refused(
                "Original Save source or edited parent identity changed",
            ));
        }
        // Reopen ONLY the original user-selected named source and require its
        // full retained identity. A replacement name never retargets authority.
        let named_source = Root::open(self.source.path())?;
        if recovery_directory_stamp(&named_source)? != self.source_stamp {
            return Err(refused("The original named Save source was replaced"));
        }
        drop(named_source);
        // Re-resolve from the ORIGINAL held source and ORIGINAL relative path,
        // not a reconstructed/reopened parent spelling on its own.
        let (parent, name) =
            open_parent_beneath(&self.source, &self.relative_path).map_err(io::Error::other)?;
        if name != self.name || recovery_directory_stamp(&parent)? != self.parent_stamp {
            return Err(refused(
                "The original Save target parent or name was replaced",
            ));
        }
        drop(parent);
        if !self.protection.protects_root(&self.parent)? {
            return Err(refused("The original edited-parent cleanup lease changed"));
        }
        Ok(())
    }
    pub(super) fn validate_for_source(&self, source: &Root) -> io::Result<()> {
        self.validate()?;
        if recovery_directory_stamp(source)? != self.source_stamp {
            return Err(refused(
                "Save target authority belongs to a different original source",
            ));
        }
        Ok(())
    }
    /// Source + actual edited-parent native descriptors, deduplicated by FD,
    /// plus the actual-parent lease's independent locks/directory handles.
    /// Source Arc aliases held by context/placement must not be charged twice.
    /// Source Protection is separate and remains the context owner's charge.
    pub(super) fn retained_descriptor_count(&self) -> usize {
        self.retained_descriptors
    }
}

#[derive(Debug)]
pub(super) struct SaveLocalPlacement {
    target: Arc<SaveTargetAuthority>,
    directory: Arc<Root>,
    stamp: RecoveryDirectoryStamp,
    namespace: DirectoryNamespace,
    sticky: Option<Arc<StickyParent>>,
    additional_descriptors: usize,
}
impl SaveLocalPlacement {
    /// Prefer the nearest readable protected holding on the actual file's
    /// mount. Effective access is an observation for preference only: when no
    /// such ancestor exists, an exact protected edited parent remains valid.
    pub(super) fn acquire(target: Arc<SaveTargetAuthority>) -> io::Result<Arc<Self>> {
        Self::acquire_with_readability(target, readable_holding)
    }
    fn acquire_with_readability(
        target: Arc<SaveTargetAuthority>,
        readable: impl Fn(&Root) -> bool,
    ) -> io::Result<Arc<Self>> {
        target.validate()?;
        if readable(&target.parent)
            && let Ok(placement) = Self::from_parts(target.clone(), target.parent.clone(), None)
        {
            return Ok(placement);
        }
        // Even an unqualified edited parent may have a protected ancestor.
        // Every step is through the original held directory; pathname lookup
        // cannot redirect the holding, and the file's mount is never crossed.
        let mut cursor = target.parent.clone();
        for _ in 0..MAX_ANCESTORS {
            let Ok(parent) = retained_parent_directory(&cursor) else {
                break;
            };
            let Ok(stamp) = recovery_directory_stamp(&parent) else {
                break;
            };
            let Ok(current) = recovery_directory_stamp(&cursor) else {
                break;
            };
            if stamp == current
                || stamp.device != target.parent_stamp.device
                || stamp.mount_id != target.parent_stamp.mount_id
            {
                break;
            }
            cursor = Arc::new(parent);
            if readable(&cursor)
                && let Ok(placement) = Self::from_parts(target.clone(), cursor.clone(), None)
            {
                return Ok(placement);
            }
        }
        // Do not retain an unused fallback namespace while inspecting higher
        // candidates. Requalify the original exact parent when it is needed.
        drop(cursor);
        Self::from_parts(target.clone(), target.parent.clone(), None)
    }
    /// Only a validated fresh typed sibling receipt can supply this directory.
    /// The caller retains the SAME receipt in its context guard until explicit
    /// cleanup; borrowing here neither converts it to Published nor releases it.
    pub(super) fn from_sibling(
        target: Arc<SaveTargetAuthority>,
        sibling: &OwnedPrivateSibling,
        sticky: Arc<StickyParent>,
    ) -> io::Result<Arc<Self>> {
        sibling.validate()?;
        sticky.validate()?;
        if recovery_directory_stamp(sticky.directory())? != target.parent_stamp {
            return Err(refused(
                "Sticky sibling proof belongs to another edited parent",
            ));
        }
        Self::from_parts(target, sibling.directory_arc()?, Some(sticky))
    }
    fn from_parts(
        target: Arc<SaveTargetAuthority>,
        directory: Arc<Root>,
        sticky: Option<Arc<StickyParent>>,
    ) -> io::Result<Arc<Self>> {
        target.validate()?;
        let stamp = recovery_directory_stamp(&directory)?;
        let namespace = DirectoryNamespace::acquire(&directory)?;
        let additional_descriptors =
            count_additional_descriptors(&target, &directory, &namespace, sticky.as_deref());
        let placement = Arc::new(Self {
            target,
            directory,
            stamp,
            namespace,
            sticky,
            additional_descriptors,
        });
        placement.validate()?;
        Ok(placement)
    }
    pub(super) fn target(&self) -> &Arc<SaveTargetAuthority> {
        &self.target
    }
    pub(super) fn directory(&self) -> &Root {
        &self.directory
    }
    pub(super) fn namespace(&self) -> &DirectoryNamespace {
        &self.namespace
    }
    pub(super) fn validate(&self) -> io::Result<()> {
        self.target.validate()?;
        if recovery_directory_stamp(&self.directory)? != self.stamp {
            return Err(refused("Original Save holding namespace identity changed"));
        }
        if let Some(sticky) = &self.sticky {
            sticky.validate()?;
            if recovery_directory_stamp(sticky.directory())? != self.target.parent_stamp {
                return Err(refused(
                    "Sticky Save parent no longer matches the original edited parent",
                ));
            }
            if self.directory.metadata()?.mode() & 0o7777 != 0o700 {
                return Err(refused("Original private Save sibling permissions changed"));
            }
            let actual_parent = retained_parent_directory(&self.directory)?;
            if recovery_directory_stamp(&actual_parent)? != self.target.parent_stamp
                || self.stamp.device != self.target.parent_stamp.device
                || self.stamp.mount_id != self.target.parent_stamp.mount_id
            {
                return Err(refused(
                    "Original private Save sibling moved away from its edited parent",
                ));
            }
        } else {
            // The original edited parent must still reach the SAME held
            // anchor on the SAME actual mount before every effect or retry.
            validate_local_ancestry(&self.target.parent, self.stamp)?;
        }
        // Keep original namespace authority while checking current private
        // owner/mode/ACL policy. Safe protected parents need not have mode0700.
        drop(DirectoryNamespace::acquire(&self.directory)?);
        Ok(())
    }
    pub(super) fn validate_for_source(&self, source: &Root) -> io::Result<()> {
        self.validate()?;
        self.target.validate_for_source(source)
    }
    pub(super) fn validate_payload_root(&self, payload: &Root) -> io::Result<()> {
        self.validate()?;
        validate_local_ancestry(payload, self.stamp)
    }
    /// ONLY additional native handles beyond target.retained_descriptor_count:
    /// namespace clone, held ancestor or fresh child if any, and a separately
    /// held sticky-parent FD if it does not alias a target Root. Never recount
    /// the target lease.
    /// Revalidation needs at most three temporary FDs (resolver/ancestry plus
    /// metadata clone); callers must reserve that capacity before effects.
    pub(super) fn retained_descriptor_count(&self) -> usize {
        self.additional_descriptors
    }
}

/// Retained-FD permission observation, including effective credentials,
/// supplementary groups and ACLs. A failure only declines preference; it is
/// never a promise about later readability or an eligibility gate for Save.
#[cfg(target_os = "linux")]
fn readable_holding(directory: &Root) -> bool {
    // SAFETY: Root retains the FD and the empty C string is live. AT_EMPTY_PATH
    // selects that inode; AT_EACCESS selects the worker's effective IDs.
    unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            directory.descriptor(),
            c"".as_ptr(),
            libc::R_OK | libc::W_OK | libc::X_OK,
            libc::AT_EMPTY_PATH | libc::AT_EACCESS,
        ) == 0
    }
}
#[cfg(not(target_os = "linux"))]
fn readable_holding(_directory: &Root) -> bool {
    false
}

fn count_additional_descriptors(
    target: &SaveTargetAuthority,
    directory: &Root,
    namespace: &DirectoryNamespace,
    sticky: Option<&StickyParent>,
) -> usize {
    let target_roots = HashSet::from([target.source.descriptor(), target.parent.descriptor()]);
    let mut additional =
        HashSet::from([directory.descriptor(), namespace.directory().descriptor()]);
    if let Some(sticky) = sticky {
        additional.insert(sticky.directory().descriptor());
    }
    additional.retain(|descriptor| !target_roots.contains(descriptor));
    additional.len()
}

fn validate_local_ancestry(payload: &Root, placement: RecoveryDirectoryStamp) -> io::Result<()> {
    let mut cursor = payload.try_clone()?;
    for _ in 0..MAX_ANCESTORS {
        let current = recovery_directory_stamp(&cursor)?;
        if current.device != placement.device || current.mount_id != placement.mount_id {
            return Err(refused(
                "Save recovery payload left the original edited-parent mount",
            ));
        }
        if current == placement {
            return Ok(());
        }
        let parent = retained_parent_directory(&cursor)?;
        if recovery_directory_stamp(&parent)? == current {
            return Err(refused(
                "Save recovery payload is outside its original holding namespace",
            ));
        }
        cursor = parent;
    }
    Err(refused(
        "Save recovery exceeds the bounded holding ancestry inspection",
    ))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::PermissionsExt};

    // Explicit private runtime fixture exercises capture/lease contracts only;
    // production runtime selection is covered by isolated caller tests.
    fn target(source: Arc<Root>, relative: &Path, runtime: &Path) -> Arc<SaveTargetAuthority> {
        SaveTargetAuthority::acquire_with_protection(source, relative, |parent| {
            crate::worktree_cleanup::fixture_protection(runtime, parent.path(), false)
        })
        .unwrap()
    }

    #[test]
    fn invalid_relative_paths_and_symlink_parents_are_refused_before_lease_intake() {
        let fixture = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::os::unix::fs::symlink(outside.path(), fixture.path().join("linked")).unwrap();
        let source = Arc::new(Root::open(fixture.path()).unwrap());
        for relative in [
            Path::new("../file"),
            Path::new("/file"),
            Path::new("linked/file"),
        ] {
            assert!(
                SaveTargetAuthority::acquire_with_protection(source.clone(), relative, |_| panic!(
                    "invalid resolution must precede lease intake"
                ))
                .is_err()
            );
        }
        assert_eq!(fs::read_dir(outside.path()).unwrap().count(), 0);
    }

    #[test]
    fn actual_parent_scope_allows_nested_known_checkout_ancestors() {
        let fixture = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        fs::create_dir_all(fixture.path().join("nested/.git")).unwrap();
        let source = Arc::new(Root::open(fixture.path()).unwrap());
        let target = target(source.clone(), Path::new("nested/file"), runtime.path());
        assert_eq!(target.relative_path(), Path::new("nested/file"));
        assert_eq!(target.name(), OsStr::new("file"));
        assert_eq!(target.source().descriptor(), source.descriptor());
        target.validate_for_source(&source).unwrap();
        let placement = SaveLocalPlacement::acquire(target.clone()).unwrap();
        assert_eq!(
            placement.directory().descriptor(),
            target.parent().descriptor()
        );
        assert_eq!(
            placement.retained_descriptor_count(),
            1,
            "only namespace clone is additional"
        );
        assert_eq!(
            target.retained_descriptor_count(),
            2 + target.protection.retained_descriptor_count()
        );
        fs::create_dir(fixture.path().join("nested/payload")).unwrap();
        let payload = Root::open(&fixture.path().join("nested/payload")).unwrap();
        placement.validate_payload_root(&payload).unwrap();
    }

    #[test]
    fn injected_readability_prefers_nearest_protected_actual_mount_ancestor() {
        let fixture = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        fs::create_dir_all(fixture.path().join("near/edited")).unwrap();
        let source = Arc::new(Root::open(fixture.path()).unwrap());
        let target = target(source, Path::new("near/edited/file"), runtime.path());
        let nearest = Root::open(&fixture.path().join("near")).unwrap();
        let nearest_stamp = recovery_directory_stamp(&nearest).unwrap();
        // The injected observation isolates preference from the test runner's
        // UID/capabilities. Namespace qualification and ancestry remain real.
        let placement = SaveLocalPlacement::acquire_with_readability(target.clone(), |root| {
            recovery_directory_stamp(root).unwrap() != target.parent_stamp
        })
        .unwrap();
        assert_eq!(
            recovery_directory_stamp(placement.directory()).unwrap(),
            nearest_stamp
        );
        assert_eq!(
            placement.retained_descriptor_count(),
            2,
            "ancestor and namespace clone"
        );
        placement.validate().unwrap();
        placement.validate_payload_root(target.parent()).unwrap();
    }

    #[test]
    fn injected_unreadable_observation_keeps_exact_protected_parent_eligible() {
        let fixture = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        fs::create_dir(fixture.path().join("edited")).unwrap();
        let target = target(
            Arc::new(Root::open(fixture.path()).unwrap()),
            Path::new("edited/file"),
            runtime.path(),
        );
        let placement =
            SaveLocalPlacement::acquire_with_readability(target.clone(), |_| false).unwrap();
        assert_eq!(
            placement.directory().descriptor(),
            target.parent().descriptor()
        );
        assert_eq!(placement.retained_descriptor_count(), 1);
        placement.validate().unwrap();
        assert_eq!(
            fs::read_dir(fixture.path().join("edited")).unwrap().count(),
            0
        );
    }

    #[test]
    fn qualified_ancestor_is_considered_after_unqualified_actual_parent() {
        let fixture = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        fs::create_dir(fixture.path().join("edited")).unwrap();
        fs::set_permissions(
            fixture.path().join("edited"),
            fs::Permissions::from_mode(0o777),
        )
        .unwrap();
        let source = Arc::new(Root::open(fixture.path()).unwrap());
        let source_stamp = recovery_directory_stamp(&source).unwrap();
        let target = target(source, Path::new("edited/file"), runtime.path());
        assert!(DirectoryNamespace::acquire(target.parent()).is_err());
        let placement =
            SaveLocalPlacement::acquire_with_readability(target.clone(), |_| true).unwrap();
        assert_eq!(
            recovery_directory_stamp(placement.directory()).unwrap(),
            source_stamp
        );
        placement.validate().unwrap();
        // With no readable qualified ancestor, the unqualified parent is not
        // promoted to authority; the context can consider sticky bootstrap.
        assert!(SaveLocalPlacement::acquire_with_readability(target, |_| false).is_err());
    }

    #[test]
    fn moved_actual_parent_cannot_keep_an_original_ancestor_holding() {
        let fixture = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        fs::create_dir_all(fixture.path().join("anchor/edited")).unwrap();
        fs::create_dir(fixture.path().join("outside")).unwrap();
        let source = Arc::new(Root::open(fixture.path()).unwrap());
        let target = target(source, Path::new("anchor/edited/file"), runtime.path());
        let anchor = Root::open(&fixture.path().join("anchor")).unwrap();
        let anchor_stamp = recovery_directory_stamp(&anchor).unwrap();
        let placement = SaveLocalPlacement::acquire_with_readability(target.clone(), |root| {
            recovery_directory_stamp(root).unwrap() == anchor_stamp
        })
        .unwrap();
        fs::rename(
            fixture.path().join("anchor/edited"),
            fixture.path().join("outside/moved"),
        )
        .unwrap();
        fs::create_dir(fixture.path().join("anchor/edited")).unwrap();
        fs::write(fixture.path().join("anchor/edited/foreign"), b"preserve").unwrap();
        assert!(validate_local_ancestry(target.parent(), anchor_stamp).is_err());
        assert!(placement.validate().is_err());
        assert_eq!(
            fs::read(fixture.path().join("anchor/edited/foreign")).unwrap(),
            b"preserve"
        );
        assert!(fixture.path().join("outside/moved").is_dir());
    }

    #[test]
    fn named_source_and_relative_parent_replacements_cannot_retarget_authority() {
        let fixture = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let source_path = fixture.path().join("source");
        fs::create_dir_all(source_path.join("nested")).unwrap();
        let source = Arc::new(Root::open(&source_path).unwrap());
        let target = target(source.clone(), Path::new("nested/file"), runtime.path());
        fs::rename(source_path.join("nested"), source_path.join("moved-parent")).unwrap();
        fs::create_dir(source_path.join("nested")).unwrap();
        assert!(target.validate().is_err());
        fs::remove_dir(source_path.join("nested")).unwrap();
        fs::rename(source_path.join("moved-parent"), source_path.join("nested")).unwrap();
        target.validate().unwrap();
        let moved_source = fixture.path().join("moved-source");
        fs::rename(&source_path, &moved_source).unwrap();
        fs::create_dir_all(source_path.join("nested")).unwrap();
        assert!(target.validate().is_err());
        assert!(
            target
                .validate_for_source(&Root::open(&source_path).unwrap())
                .is_err()
        );
        assert!(moved_source.join("nested").is_dir());
    }

    #[test]
    fn source_and_actual_parent_cleanup_leases_are_independent() {
        let fixture = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        fs::create_dir(fixture.path().join("nested")).unwrap();
        let source = Arc::new(Root::open(fixture.path()).unwrap());
        let target = target(source, Path::new("nested/file"), runtime.path());
        assert!(
            crate::worktree_cleanup::fixture_protection(
                runtime.path(),
                target.parent().path(),
                true
            )
            .is_err()
        );
        target.validate().unwrap();
        drop(target);
        crate::worktree_cleanup::fixture_protection(
            runtime.path(),
            &fixture.path().join("nested"),
            true,
        )
        .unwrap();
    }

    #[test]
    fn typed_fresh_sticky_sibling_is_checked_and_accounted_without_adoption() {
        let fixture = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o1777)).unwrap();
        let source = Arc::new(Root::open(fixture.path()).unwrap());
        let target = target(source, Path::new("file"), runtime.path());
        assert!(SaveLocalPlacement::acquire_with_readability(target.clone(), |_| false).is_err());
        let sticky = StickyParent::acquire(target.parent().clone()).unwrap();
        let mut sibling = sticky.clone().create_private_sibling().unwrap();
        let placement = SaveLocalPlacement::from_sibling(target.clone(), &sibling, sticky).unwrap();
        assert_eq!(
            placement.retained_descriptor_count(),
            2,
            "fresh child and namespace clone only"
        );
        fs::create_dir(sibling.diagnostic_path().join("payload")).unwrap();
        let payload = Root::open(&sibling.diagnostic_path().join("payload")).unwrap();
        placement.validate_payload_root(&payload).unwrap();
        fs::remove_dir(sibling.diagnostic_path().join("payload")).unwrap();
        drop(placement);
        sibling.cleanup_empty().unwrap();
        assert!(sibling.is_resolved());
    }

    #[test]
    fn foreign_sticky_proof_and_moved_original_sibling_are_refused() {
        let fixture = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let foreign = tempfile::tempdir().unwrap();
        fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o1777)).unwrap();
        fs::set_permissions(foreign.path(), fs::Permissions::from_mode(0o1777)).unwrap();
        let target = target(
            Arc::new(Root::open(fixture.path()).unwrap()),
            Path::new("file"),
            runtime.path(),
        );
        let sticky = StickyParent::acquire(target.parent().clone()).unwrap();
        let mut sibling = sticky.clone().create_private_sibling().unwrap();
        let foreign_proof =
            StickyParent::acquire(Arc::new(Root::open(foreign.path()).unwrap())).unwrap();
        assert!(SaveLocalPlacement::from_sibling(target.clone(), &sibling, foreign_proof).is_err());
        let placement = SaveLocalPlacement::from_sibling(target, &sibling, sticky).unwrap();
        let original = sibling.diagnostic_path();
        let outside = fixture.path().join("outside");
        fs::create_dir(&outside).unwrap();
        fs::rename(&original, outside.join("moved")).unwrap();
        fs::create_dir(&original).unwrap();
        assert!(placement.validate().is_err());
        assert!(original.is_dir());
        assert!(outside.join("moved").is_dir());
        drop(placement);
        fs::remove_dir(&original).unwrap();
        fs::rename(outside.join("moved"), &original).unwrap();
        sibling.cleanup_empty().unwrap();
    }

    #[test]
    fn separate_equivalent_sticky_parent_descriptor_is_counted() {
        let fixture = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        fs::set_permissions(fixture.path(), fs::Permissions::from_mode(0o1777)).unwrap();
        let target = target(
            Arc::new(Root::open(fixture.path()).unwrap()),
            Path::new("file"),
            runtime.path(),
        );
        let original = StickyParent::acquire(target.parent().clone()).unwrap();
        let equivalent =
            StickyParent::acquire(Arc::new(target.parent().try_clone().unwrap())).unwrap();
        let mut sibling = original.create_private_sibling().unwrap();
        let placement = SaveLocalPlacement::from_sibling(target, &sibling, equivalent).unwrap();
        assert_eq!(placement.retained_descriptor_count(), 3);
        drop(placement);
        sibling.cleanup_empty().unwrap();
    }

    #[test]
    fn moved_payload_is_refused_and_replacements_are_preserved() {
        let fixture = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        fs::create_dir_all(fixture.path().join("edited/payload")).unwrap();
        fs::create_dir(fixture.path().join("outside")).unwrap();
        let target = target(
            Arc::new(Root::open(fixture.path()).unwrap()),
            Path::new("edited/file"),
            runtime.path(),
        );
        let placement = SaveLocalPlacement::acquire(target).unwrap();
        let payload = Root::open(&fixture.path().join("edited/payload")).unwrap();
        fs::rename(
            fixture.path().join("edited/payload"),
            fixture.path().join("outside/moved"),
        )
        .unwrap();
        fs::create_dir(fixture.path().join("edited/payload")).unwrap();
        fs::write(fixture.path().join("edited/payload/foreign"), b"preserve").unwrap();
        assert!(placement.validate_payload_root(&payload).is_err());
        assert_eq!(
            fs::read(fixture.path().join("edited/payload/foreign")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    fn payload_walk_refuses_other_mounts_and_excessive_depth() {
        let fixture = tempfile::tempdir().unwrap();
        let source = Root::open(fixture.path()).unwrap();
        let stamp = recovery_directory_stamp(&source).unwrap();
        let other_mount = RecoveryDirectoryStamp {
            mount_id: stamp.mount_id + 1,
            ..stamp
        };
        assert!(validate_local_ancestry(&source, other_mount).is_err());
        let mut path = fixture.path().to_path_buf();
        for _ in 0..=MAX_ANCESTORS {
            path.push("d");
        }
        fs::create_dir_all(&path).unwrap();
        let payload = Root::open(&path).unwrap();
        assert!(validate_local_ancestry(&payload, stamp).is_err());
        assert!(path.is_dir());
    }
}
