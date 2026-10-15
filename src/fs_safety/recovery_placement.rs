//! Retained placement for recovery payloads outside ordinary checkout discovery.
//!
//! Permissions and random names do not exclude same-user `git add --all`.
//! This is a bounded observation of known roots and ordinary ancestor `.git`
//! locators, not a global namespace lock. A new external enclosing repository,
//! explicit external `--work-tree`, or ancestry changes after the final check
//! remain topology limits. Domain callers own registration/adoption leases.
use super::*;
use std::{collections::HashSet, os::unix::fs::MetadataExt, sync::Arc};

const MAX_EXCLUDED_ROOTS: usize = 256;
const MAX_ANCESTORS: usize = 256;

/// Worker-only logical registration/current-root validation. The caller must
/// retain its actual epoch/lease, reject adoption of a source enclosing pending
/// payloads, and invalidate writes when that authority changes. A fresh always-
/// successful closure is not a substitute for the domain's registry proof.
#[derive(Clone)]
pub struct RecoveryRegistryCheck {
    check: Arc<dyn Fn() -> io::Result<()> + Send + Sync>,
    retained_descriptors: usize,
}
impl RecoveryRegistryCheck {
    /// `retained_descriptors` includes every native descriptor retained by the
    /// opaque callback/lease. The domain must measure it, including shared
    /// handles not present in the explicit Root set; do not charge an Arc zero.
    pub fn new(check: Arc<dyn Fn() -> io::Result<()> + Send + Sync>, retained_descriptors: usize) -> Self {
        Self { check, retained_descriptors }
    }
}

/// Exact original placement authority. Keep this Arc with the same protection
/// and recovery reservation as the acquired file. Its parent is distinct from
/// the source parent used for public acquisition, putback and metadata rules.
pub struct RecoveryPlacement {
    parent: Arc<Root>,
    source: Arc<Root>,
    excluded: Arc<[Arc<Root>]>,
    namespace: DirectoryNamespace,
    registry: RecoveryRegistryCheck,
    source_identity: (u64, u64),
    parent_identity: (u64, u64),
    retained_descriptors: usize,
}
impl fmt::Debug for RecoveryPlacement {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("RecoveryPlacement")
            .field("source_identity", &self.source_identity)
            .field("parent_identity", &self.parent_identity)
            .field("excluded_roots", &self.excluded.len())
            .field("retained_descriptors", &self.retained_descriptors)
            .finish_non_exhaustive()
    }
}
fn unsupported(message: &str) -> io::Error { io::Error::new(io::ErrorKind::Unsupported, message) }
fn identity(root: &Root) -> io::Result<(u64, u64)> {
    let metadata = root.metadata()?;
    Ok((metadata.dev(), metadata.ino()))
}
impl RecoveryPlacement {
    /// Captures an already-held candidate, never reconstructing an effect path.
    /// Candidate selection is domain-owned: prefer authenticated outside Git
    /// metadata, then a private persistent same-filesystem cache; refuse before
    /// effects if none qualifies. All known edited/registered roots are required.
    pub fn acquire(parent: Arc<Root>, source: Arc<Root>, excluded: Arc<[Arc<Root>]>, registry: RecoveryRegistryCheck) -> io::Result<Arc<Self>> {
        if excluded.len() > MAX_EXCLUDED_ROOTS {
            return Err(unsupported("Recovery placement exceeds the bounded known-root census; no file was moved."));
        }
        (registry.check)()?;
        let source_identity = identity(&source)?;
        let parent_identity = identity(&parent)?;
        if source_identity.0 != parent_identity.0 {
            return Err(unsupported("Recovery requires a private outside-source directory on the same filesystem; no file was moved."));
        }
        let namespace = DirectoryNamespace::acquire(&parent)?;
        // Count actual distinct retained descriptor numbers, including the
        // namespace's clone. Arc aliases of one Root do not add native handles.
        let mut descriptors = HashSet::from([parent.descriptor(), source.descriptor(), namespace.directory().descriptor()]);
        descriptors.extend(excluded.iter().map(|root| root.descriptor()));
        let retained_descriptors = descriptors.len().checked_add(registry.retained_descriptors)
            .ok_or_else(|| unsupported("Recovery descriptor accounting overflowed."))?;
        let placement = Arc::new(Self { parent, source, excluded, namespace, registry, source_identity, parent_identity, retained_descriptors });
        placement.validate()?;
        Ok(placement)
    }

    /// Revalidate in the blocking worker before every publication/acquisition
    /// and retry. Failure retains the old authority; it never authorizes a new
    /// root or replacement path. Callers must preserve typed creation failures.
    pub fn validate(&self) -> io::Result<()> {
        (self.registry.check)()?;
        if identity(&self.source)? != self.source_identity || identity(&self.parent)? != self.parent_identity {
            return Err(unsupported("The retained recovery source or placement identity changed."));
        }
        let mut excluded = HashSet::from([self.source_identity]);
        for root in self.excluded.iter() { excluded.insert(identity(root)?); }
        let mut cursor = self.parent.try_clone()?;
        for _ in 0..MAX_ANCESTORS {
            let current = identity(&cursor)?;
            if excluded.contains(&current) {
                return Err(unsupported("Recovery payloads would be inside a known edited or registered checkout root; this step was refused and existing recovery was retained."));
            }
            // Presence alone refuses placement. Do not read an attacker/FIFO
            // locator, follow a symlink, or adopt unknown foreign Git metadata.
            match metadata_beneath(&cursor, Path::new(".git")) {
                Err(error) if error.is_not_found() => {}
                Ok(_) => return Err(unsupported("Recovery placement has an enclosing Git checkout locator; this step was refused and existing recovery was retained.")),
                Err(error) => return Err(unsupported(&format!("Recovery checkout ancestry could not be established: {error}"))),
            }
            let directory = open_at(cursor.descriptor(), OsStr::new(".."), libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)?;
            let parent = Root { directory, path: cursor.path().join("..") };
            if identity(&parent)? == current {
                (self.registry.check)()?;
                return Ok(());
            }
            cursor = parent;
        }
        Err(unsupported("Recovery placement exceeds the bounded ancestor inspection; this step was refused and existing recovery was retained."))
    }
    pub fn directory(&self) -> &Root { &self.parent }
    pub fn namespace(&self) -> &DirectoryNamespace { &self.namespace }
    /// Distinct explicit native Root handles plus the callback's measured lease
    /// budget. Transient ancestry/metadata inspection needs at most three extra
    /// native descriptors and must also fit the owning worker's admission.
    pub fn retained_descriptor_count(&self) -> usize { self.retained_descriptors }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, sync::atomic::{AtomicBool, Ordering}};
    fn registry() -> RecoveryRegistryCheck { RecoveryRegistryCheck::new(Arc::new(|| Ok(())), 0) }
    fn root(path: &Path) -> Arc<Root> { Arc::new(Root::open(path).unwrap()) }

    #[test]
    fn placement_outside_source_keeps_original_parent_and_measures_arc_aliases() {
        let tree = tempfile::tempdir().unwrap();
        fs::create_dir(tree.path().join("source")).unwrap();
        fs::create_dir(tree.path().join("outside")).unwrap();
        let source = root(&tree.path().join("source"));
        let placement = RecoveryPlacement::acquire(root(&tree.path().join("outside")), source.clone(), vec![source].into(), registry()).unwrap();
        assert_eq!(placement.retained_descriptor_count(), 3);
        fs::rename(tree.path().join("outside"), tree.path().join("retired")).unwrap();
        fs::create_dir(tree.path().join("outside")).unwrap();
        placement.validate().unwrap();
        let published = create_directory_in_namespace(placement.namespace(), placement.directory(), Path::new("holding"), 0o700).unwrap();
        assert!(matches!(published, DirectoryCreation::Published(_)));
        assert!(tree.path().join("retired/holding").is_dir());
        assert!(!tree.path().join("outside/holding").exists());
    }

    #[test]
    fn placement_refuses_known_non_git_source_and_foreign_git_ancestors() {
        let tree = tempfile::tempdir().unwrap();
        for path in ["source", "foreign", "foreign/cache"] { fs::create_dir(tree.path().join(path)).unwrap(); }
        let source = root(&tree.path().join("source"));
        assert!(RecoveryPlacement::acquire(source.clone(), source.clone(), Arc::from([]), registry()).is_err());
        for locator in ["gitdir: /unknown\n", ""] {
            fs::write(tree.path().join("foreign/.git"), locator).unwrap();
            assert!(RecoveryPlacement::acquire(root(&tree.path().join("foreign/cache")), source.clone(), Arc::from([]), registry()).is_err());
            fs::remove_file(tree.path().join("foreign/.git")).unwrap();
        }
        std::os::unix::fs::symlink("/unavailable", tree.path().join("foreign/.git")).unwrap();
        assert!(RecoveryPlacement::acquire(root(&tree.path().join("foreign/cache")), source, Arc::from([]), registry()).is_err());
    }

    #[test]
    fn placement_rechecks_registry_and_new_checkout_locator_before_effect() {
        let tree = tempfile::tempdir().unwrap();
        for path in ["source", "outside"] { fs::create_dir(tree.path().join(path)).unwrap(); }
        let valid = Arc::new(AtomicBool::new(true));
        let captured = valid.clone();
        let check = RecoveryRegistryCheck::new(Arc::new(move || if captured.load(Ordering::Acquire) { Ok(()) } else { Err(unsupported("registry invalidated")) }), 0);
        let placement = RecoveryPlacement::acquire(root(&tree.path().join("outside")), root(&tree.path().join("source")), Arc::from([]), check).unwrap();
        valid.store(false, Ordering::Release);
        assert!(placement.validate().is_err());
        valid.store(true, Ordering::Release);
        fs::create_dir(tree.path().join(".git")).unwrap();
        assert!(placement.validate().is_err());
        assert!(!tree.path().join("outside/holding").exists());
    }
}
