//! Local worker-owned recovery placement outside the captured checkout census.
//!
//! This is physical descriptor/ancestry validation, not a global registration
//! or future-root adoption grant. A new external enclosing repository, explicit
//! external --work-tree, or topology change after the final check remains an
//! operating-system namespace limit. Callers recheck before every effect/retry.
use super::*;
use std::{collections::HashSet, os::unix::fs::MetadataExt, sync::Arc};
const MAX_EXCLUDED_ROOTS: usize = 256;
const MAX_ANCESTORS: usize = 256;
/// An observation of an already-held directory, not creation/effect authority.
/// Keep its original Root alive to prevent inode reuse while comparing stamps.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct RecoveryDirectoryStamp {
    pub device: u64,
    pub inode: u64,
    pub mount_id: u64,
}
pub fn recovery_directory_stamp(root: &Root) -> io::Result<RecoveryDirectoryStamp> {
    let (device, inode) = identity(root)?;
    Ok(RecoveryDirectoryStamp {
        device,
        inode,
        mount_id: mount_identity(root)?,
    })
}

pub struct RecoveryPlacement {
    parent: Arc<Root>,
    source: Arc<Root>,
    excluded: Arc<[Arc<Root>]>,
    namespace: DirectoryNamespace,
    source_identity: (u64, u64),
    parent_identity: (u64, u64),
    source_mount: u64,
    parent_mount: u64,
    retained_descriptors: usize,
}
impl fmt::Debug for RecoveryPlacement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryPlacement")
            .field("source_identity", &self.source_identity)
            .field("parent_identity", &self.parent_identity)
            .field("source_mount", &self.source_mount)
            .field("parent_mount", &self.parent_mount)
            .field("excluded_roots", &self.excluded.len())
            .field("retained_descriptors", &self.retained_descriptors)
            .finish_non_exhaustive()
    }
}
fn unsupported(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::Unsupported, message)
}
fn identity(root: &Root) -> io::Result<(u64, u64)> {
    let metadata = root.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}
#[cfg(target_os = "linux")]
fn mount_identity(root: &Root) -> io::Result<u64> {
    // Device/inode alone cannot terminate an ancestor walk: a self bind mount
    // can expose the same inode through a different mount before the real
    // checkout ancestor. statx's retained-FD mount ID distinguishes that edge.
    let mut metadata = std::mem::MaybeUninit::<libc::statx>::zeroed();
    let result = unsafe {
        libc::statx(
            root.descriptor(),
            c"".as_ptr(),
            libc::AT_EMPTY_PATH | libc::AT_NO_AUTOMOUNT,
            libc::STATX_MNT_ID,
            metadata.as_mut_ptr(),
        )
    };
    if result < 0 {
        return Err(io::Error::last_os_error());
    }
    let metadata = unsafe { metadata.assume_init() };
    if metadata.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(unsupported(
            "Recovery ancestry requires a supported retained-directory mount identity.",
        ));
    }
    Ok(metadata.stx_mnt_id)
}
fn parent_of(root: &Root) -> io::Result<Root> {
    retained_parent_directory(root)
}
#[cfg(not(target_os = "linux"))]
fn mount_identity(_root: &Root) -> io::Result<u64> {
    Err(unsupported(
        "Strong recovery placement is unavailable without qualified native mount-identity ancestry inspection.",
    ))
}
/// Pre-effect qualification of a retained outside candidate. Unlike final
/// placement this does not claim a private namespace, create anything, or
/// authorize payloads. Sticky bootstrap callers must run this before mkdir,
/// then acquire RecoveryPlacement on the actual fresh private sibling.
pub(crate) fn validate_recovery_parent(
    parent: &Root,
    source: &Root,
    known_roots: &[Arc<Root>],
) -> io::Result<()> {
    if known_roots.len() > MAX_EXCLUDED_ROOTS {
        return Err(unsupported(
            "Recovery parent exceeds the bounded checkout census",
        ));
    }
    let source_identity = identity(source)?;
    if identity(parent)?.0 != source_identity.0
        || mount_identity(parent)? != mount_identity(source)?
    {
        return Err(unsupported(
            "Recovery parent must be on the original source mount",
        ));
    }
    validate_recovery_ancestry(parent, source_identity, known_roots, &[])
}

fn validate_recovery_ancestry(
    start: &Root,
    source_identity: (u64, u64),
    known_roots: &[Arc<Root>],
    current_roots: &[Arc<Root>],
) -> io::Result<()> {
    let mut excluded = HashSet::from([source_identity]);
    for root in known_roots.iter() {
        excluded.insert(identity(root)?);
    }
    for root in current_roots {
        excluded.insert(identity(root)?);
    }
    if excluded.len() > MAX_EXCLUDED_ROOTS {
        return Err(unsupported(
            "The combined recovery root census exceeds its bounded inspection.",
        ));
    }
    let mut cursor = start.try_clone()?;
    for _ in 0..MAX_ANCESTORS {
        let current = identity(&cursor)?;
        let current_mount = mount_identity(&cursor)?;
        if excluded.contains(&current) {
            return Err(unsupported(
                "Recovery payloads would be inside a captured known checkout root; this step was refused and existing recovery was retained.",
            ));
        }
        // Presence alone refuses placement. Do not read an attacker/FIFO
        // locator, follow a symlink, or adopt unknown foreign Git metadata.
        match metadata_beneath(&cursor, Path::new(".git")) {
            Err(error) if error.is_not_found() => {}
            Ok(_) => {
                return Err(unsupported(
                    "Recovery placement has an enclosing Git checkout locator; this step was refused and existing recovery was retained.",
                ));
            }
            Err(error) => {
                return Err(unsupported(&format!(
                    "Recovery checkout ancestry could not be established: {error}"
                )));
            }
        }
        let parent = parent_of(&cursor)?;
        if identity(&parent)? == current && mount_identity(&parent)? == current_mount {
            return Ok(());
        }
        cursor = parent;
    }
    Err(unsupported(
        "Recovery placement exceeds the bounded ancestor inspection; this step was refused and existing recovery was retained.",
    ))
}

impl RecoveryPlacement {
    /// Retain the actual same-filesystem outside parent and captured known
    /// roots. No path-only consent, registry callback or adoption grant exists.
    pub fn acquire(
        parent: Arc<Root>,
        source: Arc<Root>,
        excluded: Arc<[Arc<Root>]>,
    ) -> io::Result<Arc<Self>> {
        if excluded.len() > MAX_EXCLUDED_ROOTS {
            return Err(unsupported(
                "Recovery placement exceeds the bounded known-root census; no file was moved.",
            ));
        }
        let source_identity = identity(&source)?;
        let parent_identity = identity(&parent)?;
        let source_mount = mount_identity(&source)?;
        let parent_mount = mount_identity(&parent)?;
        if source_identity.0 != parent_identity.0 || source_mount != parent_mount {
            return Err(unsupported(
                "Recovery requires a private outside-source directory on the same filesystem; no file was moved.",
            ));
        }
        let namespace = DirectoryNamespace::acquire(&parent)?;
        // Count actual distinct retained descriptor numbers, including the
        // namespace's clone. Arc aliases of one Root do not add native handles.
        let mut descriptors = HashSet::from([
            parent.descriptor(),
            source.descriptor(),
            namespace.directory().descriptor(),
        ]);
        descriptors.extend(excluded.iter().map(|root| root.descriptor()));
        let retained_descriptors = descriptors.len();
        let placement = Arc::new(Self {
            parent,
            source,
            excluded,
            namespace,
            source_identity,
            parent_identity,
            source_mount,
            parent_mount,
            retained_descriptors,
        });
        placement.validate()?;
        Ok(placement)
    }

    /// Revalidate in the blocking worker before every publication/acquisition
    /// and retry. Failure retains the old authority; it never authorizes a new
    /// root or replacement path. Callers must preserve typed creation failures.
    pub fn validate(&self) -> io::Result<()> {
        self.validate_against_roots(&[])
    }
    /// Include a fresh held negative census without replacing original roots.
    pub fn validate_against_roots(&self, current_roots: &[Arc<Root>]) -> io::Result<()> {
        self.validate_from_root(&self.parent, current_roots)
    }
    /// Check the actual retained Published holding before payload writes, file
    /// acquisition/publication and retry. Its original outside parent remaining
    /// safe does not prove that this directory has not moved into a checkout.
    pub fn validate_payload_root(&self, payload: &Root) -> io::Result<()> {
        self.validate_payload_against_roots(payload, &[])
    }
    /// Include additional held roots when checking an existing payload.
    pub fn validate_payload_against_roots(
        &self,
        payload: &Root,
        current_roots: &[Arc<Root>],
    ) -> io::Result<()> {
        self.validate_from_root(payload, current_roots)
    }
    fn validate_from_root(&self, start: &Root, current_roots: &[Arc<Root>]) -> io::Result<()> {
        if current_roots.len() > MAX_EXCLUDED_ROOTS {
            return Err(unsupported(
                "The current recovery root census exceeds its bounded inspection.",
            ));
        }
        if identity(&self.source)? != self.source_identity
            || identity(&self.parent)? != self.parent_identity
            || mount_identity(&self.source)? != self.source_mount
            || mount_identity(&self.parent)? != self.parent_mount
        {
            return Err(unsupported(
                "The retained recovery source or placement identity changed.",
            ));
        }
        if identity(start)?.0 != self.parent_identity.0
            || mount_identity(start)? != self.parent_mount
        {
            return Err(unsupported(
                "The retained recovery payload is not on the captured source mount; this step was refused.",
            ));
        }
        validate_recovery_ancestry(start, self.source_identity, &self.excluded, current_roots)
    }
    /// The consumer's original effect Root must match the captured source;
    /// borrowing a safe placement for another source does not transfer consent.
    pub fn validate_for_source(&self, source: &Root) -> io::Result<()> {
        self.validate()?;
        if identity(source)? != self.source_identity || mount_identity(source)? != self.source_mount
        {
            return Err(unsupported(
                "Recovery placement belongs to a different source directory.",
            ));
        }
        Ok(())
    }
    pub fn directory(&self) -> &Root {
        &self.parent
    }
    pub fn namespace(&self) -> &DirectoryNamespace {
        &self.namespace
    }
    /// Distinct retained Root handles, including the original namespace clone.
    /// Transient ancestry/metadata inspection needs at most three extra
    /// native descriptors and must also fit the owning worker's admission.
    pub fn retained_descriptor_count(&self) -> usize {
        self.retained_descriptors
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use std::os::unix::fs::{FileTypeExt, PermissionsExt};

    fn roots() -> (tempfile::TempDir, Arc<Root>, Arc<Root>) {
        let directory = tempfile::Builder::new()
            .permissions(std::fs::Permissions::from_mode(0o700))
            .tempdir()
            .unwrap();
        std::fs::create_dir(directory.path().join("source")).unwrap();
        let source = Arc::new(Root::open(&directory.path().join("source")).unwrap());
        let outside = Arc::new(Root::open(directory.path()).unwrap());
        (directory, source, outside)
    }
    #[test]
    fn sticky_sibling_qualifies_direct_temporary_directory_sources() {
        // Two independently owned sticky parents retain the two positive
        // cases without assuming public /tmp or /var/tmp checkout ancestry.
        for _ in 0..2 {
            let temporary_parent = tempfile::tempdir().unwrap();
            std::fs::set_permissions(
                temporary_parent.path(),
                std::fs::Permissions::from_mode(0o1777),
            )
            .unwrap();
            let source = tempfile::tempdir_in(temporary_parent.path()).unwrap();
            let source_root = Arc::new(Root::open(source.path()).unwrap());
            let parent_root = Arc::new(Root::open(temporary_parent.path()).unwrap());
            let known = Arc::from([source_root.clone()]);
            validate_recovery_parent(&parent_root, &source_root, &known).unwrap();
            let parent = StickyParent::acquire(parent_root).unwrap();
            let mut sibling = parent.create_private_sibling().unwrap();
            let placement =
                RecoveryPlacement::acquire(sibling.directory_arc().unwrap(), source_root, known)
                    .unwrap();
            placement.validate().unwrap();
            assert_eq!(
                placement.retained_descriptor_count(),
                3,
                "same source Arc is deduplicated, child plus namespace plus source"
            );
            drop(placement);
            sibling.cleanup_empty().unwrap();
            assert!(sibling.is_resolved());
        }
    }

    #[test]
    fn sticky_parent_preflight_refuses_source_and_captured_ancestors_before_creation() {
        let (directory, source, outside) = roots();
        let initial = std::fs::read_dir(directory.path()).unwrap().count();
        assert!(validate_recovery_parent(&source, &source, &[]).is_err());
        assert!(validate_recovery_parent(&outside, &source, &[outside.clone()]).is_err());
        assert_eq!(
            std::fs::read_dir(directory.path()).unwrap().count(),
            initial
        );
    }

    #[test]
    fn local_recovery_parent_refuses_source_and_known_checkout_ancestors() {
        let (_directory, source, outside) = roots();
        assert!(RecoveryPlacement::acquire(source.clone(), source.clone(), Arc::from([])).is_err());
        assert!(
            RecoveryPlacement::acquire(
                outside.clone(),
                source.clone(),
                Arc::from([outside.clone()])
            )
            .is_err()
        );
        let placement =
            RecoveryPlacement::acquire(outside, source.clone(), Arc::from([source])).unwrap();
        placement.validate().unwrap();
    }
    #[test]
    fn local_recovery_refuses_later_enclosing_git_locator_without_opening_fifo() {
        let (directory, source, outside) = roots();
        let placement = RecoveryPlacement::acquire(outside, source, Arc::from([])).unwrap();
        let marker = directory.path().join(".git");
        let name = CString::new(marker.as_os_str().as_encoded_bytes()).unwrap();
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        assert!(placement.validate().is_err());
        assert!(
            std::fs::symlink_metadata(marker)
                .unwrap()
                .file_type()
                .is_fifo()
        );
    }
    #[test]
    fn local_recovery_checks_actual_moved_holding_and_preserves_lookalike() {
        let (directory, source, outside) = roots();
        let placement = RecoveryPlacement::acquire(outside, source.clone(), Arc::from([])).unwrap();
        let mut publication =
            match create_directory_beneath(placement.directory(), Path::new("holding"), 0o700)
                .unwrap()
            {
                DirectoryCreation::Published(publication) => publication,
                other => panic!("expected owned publication, got {other:?}"),
            };
        placement
            .validate_payload_root(publication.directory())
            .unwrap();
        std::fs::rename(
            directory.path().join("holding"),
            source.path().join("moved"),
        )
        .unwrap();
        std::fs::create_dir(directory.path().join("holding")).unwrap();
        std::fs::write(directory.path().join("holding/foreign"), b"preserve").unwrap();
        assert!(
            placement.validate().is_ok(),
            "original parent alone still passes"
        );
        assert!(
            placement
                .validate_payload_root(publication.directory())
                .is_err()
        );
        assert_eq!(
            std::fs::read(directory.path().join("holding/foreign")).unwrap(),
            b"preserve"
        );
        std::fs::remove_file(directory.path().join("holding/foreign")).unwrap();
        std::fs::remove_dir(directory.path().join("holding")).unwrap();
        std::fs::rename(
            source.path().join("moved"),
            directory.path().join("holding"),
        )
        .unwrap();
        publication.cleanup_empty().unwrap();
    }
}
