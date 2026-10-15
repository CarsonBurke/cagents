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
    Ok(RecoveryDirectoryStamp { device, inode, mount_id: mount_identity(root)? })
}

/// Worker-only logical registration/current-root validation. The caller must
/// retain its actual epoch/lease, reject adoption of a source enclosing pending
/// payloads, and invalidate writes when that authority changes. A fresh always-
/// successful closure is not a substitute for the domain's registry proof.
pub trait RecoveryRegistrationLease: Send + Sync {
    fn validate(&self) -> io::Result<()>;
    /// Enroll the actual retained Published holding before any payload write.
    /// Exact-identity repeats must be idempotent. Unknown handoff outcomes keep
    /// a negative obligation inside this owned lease even when returning Err.
    /// Admission/registration exclusion must span validation through effects;
    /// three sequential checks alone do not provide that domain exclusion.
    fn register_payload(&self, payload: &Root) -> io::Result<()>;
    /// Atomically admit one new holding against retirement. The actual lease
    /// seals creation admission and waits for all original guards to drop
    /// before releasing its server reservation. Existing cleanup/enrollment
    /// stays usable while sealed; linked/unknown server obligations still
    /// independently prevent release. Keep this SAME guard in the already-
    /// reserved recovery slot BEFORE mkdir, through enrollment/error custody.
    /// The guard must perform no I/O on Drop; all local native descriptors it
    /// retains must already fit descriptor_budget(). A counter-only guard adds
    /// no descriptors. The default refuses unsupported admission before effects.
    fn begin_payload_creation(&self) -> io::Result<Arc<dyn Send + Sync>> {
        Err(unsupported("This recovery lease cannot admit new holding creation; no directory was created."))
    }
    /// Hard upper bound for all LOCAL native descriptors retained by this
    /// lease/same-process guards, including future local enrollment handles.
    /// Reject growth beyond the reservation. A remote daemon must independently
    /// pre-reserve its GLOBAL descriptor budget before issuance/enrollment;
    /// its received handles are not silently treated as zero-cost remote state.
    fn descriptor_budget(&self) -> usize;
}

#[derive(Clone)]
pub struct RecoveryRegistryCheck {
    check: Arc<dyn Fn() -> io::Result<()> + Send + Sync>,
    register_payload: Option<Arc<dyn Fn(&Root) -> io::Result<()> + Send + Sync>>,
    begin_payload_creation: Option<Arc<dyn Fn() -> io::Result<Arc<dyn Send + Sync>> + Send + Sync>>,
    retained_descriptors: usize,
}
impl RecoveryRegistryCheck {
    /// `retained_descriptors` includes every native descriptor retained by the
    /// opaque callback/lease. The domain must measure it, including shared
    /// handles not present in the explicit Root set; do not charge an Arc zero.
    pub fn new(check: Arc<dyn Fn() -> io::Result<()> + Send + Sync>, retained_descriptors: usize) -> Self {
        Self { check, register_payload: None, begin_payload_creation: None, retained_descriptors }
    }
    /// Real domain issuance retains an owned registration/adoption lease.
    /// The callback-only constructor supports isolated static predicate tests;
    /// it deliberately cannot authorize payload enrollment.
    pub fn from_lease(lease: Arc<dyn RecoveryRegistrationLease>) -> Self {
        let retained_descriptors = lease.descriptor_budget();
        let validator = lease.clone();
        let creator = lease.clone();
        Self {
            check: Arc::new(move || validator.validate()),
            register_payload: Some(Arc::new(move |payload| lease.register_payload(payload))),
            begin_payload_creation: Some(Arc::new(move || creator.begin_payload_creation())),
            retained_descriptors,
        }
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
fn unsupported(message: &str) -> io::Error { io::Error::new(io::ErrorKind::Unsupported, message) }
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
    let result = unsafe { libc::statx(root.descriptor(), c"".as_ptr(), libc::AT_EMPTY_PATH | libc::AT_NO_AUTOMOUNT, libc::STATX_MNT_ID, metadata.as_mut_ptr()) };
    if result < 0 { return Err(io::Error::last_os_error()); }
    let metadata = unsafe { metadata.assume_init() };
    if metadata.stx_mask & libc::STATX_MNT_ID == 0 {
        return Err(unsupported("Recovery ancestry requires a supported retained-directory mount identity."));
    }
    Ok(metadata.stx_mnt_id)
}
fn parent_of(root: &Root) -> io::Result<Root> { retained_parent_directory(root) }
#[cfg(not(target_os = "linux"))]
fn mount_identity(_root: &Root) -> io::Result<u64> {
    Err(unsupported("Strong recovery placement is unavailable without qualified native mount-identity ancestry inspection."))
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
        let source_mount = mount_identity(&source)?;
        let parent_mount = mount_identity(&parent)?;
        if source_identity.0 != parent_identity.0 || source_mount != parent_mount {
            return Err(unsupported("Recovery requires a private outside-source directory on the same filesystem; no file was moved."));
        }
        let namespace = DirectoryNamespace::acquire(&parent)?;
        // Count actual distinct retained descriptor numbers, including the
        // namespace's clone. Arc aliases of one Root do not add native handles.
        let mut descriptors = HashSet::from([parent.descriptor(), source.descriptor(), namespace.directory().descriptor()]);
        descriptors.extend(excluded.iter().map(|root| root.descriptor()));
        let retained_descriptors = descriptors.len().checked_add(registry.retained_descriptors)
            .ok_or_else(|| unsupported("Recovery descriptor accounting overflowed."))?;
        let placement = Arc::new(Self { parent, source, excluded, namespace, registry, source_identity, parent_identity, source_mount, parent_mount, retained_descriptors });
        placement.validate()?;
        Ok(placement)
    }

    /// Revalidate in the blocking worker before every publication/acquisition
    /// and retry. Failure retains the old authority; it never authorizes a new
    /// root or replacement path. Callers must preserve typed creation failures.
    pub fn validate(&self) -> io::Result<()> {
        self.validate_against_roots(&[])
    }
    /// Worker-only current census proof for registry/adoption reservation. The
    /// caller retains these fresh Roots and its admission authority through the
    /// subsequent effect. Do not call while holding a lock/IPC turn that the
    /// registry callback itself needs; this method invokes that check twice.
    pub fn validate_against_roots(&self, current_roots: &[Arc<Root>]) -> io::Result<()> {
        self.validate_from_root(&self.parent, current_roots)
    }
    /// Check the actual retained Published holding before payload writes, file
    /// acquisition/publication and retry. Its original outside parent remaining
    /// safe does not prove that this directory has not moved into a checkout.
    pub fn validate_payload_root(&self, payload: &Root) -> io::Result<()> {
        self.validate_payload_against_roots(payload, &[])
    }
    /// Refuse a known missing enrollment capability BEFORE publication. The
    /// real issuer still owns its pre-reserved payload capacity and admission
    /// lease; this check does not manufacture an additional capacity promise.
    pub fn require_payload_registration(&self) -> io::Result<()> {
        if self.registry.register_payload.is_none() {
            return Err(unsupported("Recovery payloads require an owned registration/adoption lease; no directory was created."));
        }
        self.validate()
    }
    /// Worker-only creation admission, distinct from capability preflight.
    /// Retain the returned SAME guard in the recovery slot before any mkdir.
    /// A failed post-admission check drops this unused guard without effects.
    /// This does not resolve an existing holding or release server obligations.
    pub fn begin_payload_creation(&self) -> io::Result<Arc<dyn Send + Sync>> {
        self.require_payload_registration()?;
        let begin = self.registry.begin_payload_creation.as_ref().ok_or_else(|| unsupported("Recovery holding creation requires an owned admission lease; no directory was created."))?;
        let guard = begin()?;
        self.validate()?;
        Ok(guard)
    }
    /// The holding already exists. On any failure, the producer must retain its
    /// exact publication AND this lease in its pre-reserved recovery slot;
    /// neither an error string nor a fresh pathname grants cleanup authority.
    pub fn register_payload_root(&self, payload: &Root) -> io::Result<()> {
        self.validate_payload_root(payload)?;
        let register = self.registry.register_payload.as_ref().ok_or_else(|| unsupported("Recovery payloads require an owned registration/adoption lease; no payload was written."))?;
        register(payload)?;
        self.validate_payload_root(payload)
    }
    /// Registry/adoption companion for an actual held pending payload. Retain
    /// the exact payload Root and census through the domain's admission turn.
    pub fn validate_payload_against_roots(&self, payload: &Root, current_roots: &[Arc<Root>]) -> io::Result<()> {
        self.validate_from_root(payload, current_roots)
    }
    fn validate_from_root(&self, start: &Root, current_roots: &[Arc<Root>]) -> io::Result<()> {
        if current_roots.len() > MAX_EXCLUDED_ROOTS {
            return Err(unsupported("The current recovery root census exceeds its bounded inspection."));
        }
        (self.registry.check)()?;
        if identity(&self.source)? != self.source_identity || identity(&self.parent)? != self.parent_identity
            || mount_identity(&self.source)? != self.source_mount || mount_identity(&self.parent)? != self.parent_mount {
            return Err(unsupported("The retained recovery source or placement identity changed."));
        }
        if identity(start)?.0 != self.parent_identity.0 || mount_identity(start)? != self.parent_mount {
            return Err(unsupported("The retained recovery payload is not on the captured source mount; this step was refused."));
        }
        let mut excluded = HashSet::from([self.source_identity]);
        for root in self.excluded.iter() { excluded.insert(identity(root)?); }
        for root in current_roots { excluded.insert(identity(root)?); }
        if excluded.len() > MAX_EXCLUDED_ROOTS {
            return Err(unsupported("The combined recovery root census exceeds its bounded inspection."));
        }
        let mut cursor = start.try_clone()?;
        for _ in 0..MAX_ANCESTORS {
            let current = identity(&cursor)?;
            let current_mount = mount_identity(&cursor)?;
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
            let parent = parent_of(&cursor)?;
            if identity(&parent)? == current && mount_identity(&parent)? == current_mount {
                (self.registry.check)()?;
                return Ok(());
            }
            cursor = parent;
        }
        Err(unsupported("Recovery placement exceeds the bounded ancestor inspection; this step was refused and existing recovery was retained."))
    }
    /// The consumer's original effect Root must match the captured source;
    /// borrowing a safe placement for another source does not transfer consent.
    pub fn validate_for_source(&self, source: &Root) -> io::Result<()> {
        self.validate()?;
        if identity(source)? != self.source_identity || mount_identity(source)? != self.source_mount {
            return Err(unsupported("Recovery placement belongs to a different source directory."));
        }
        Ok(())
    }
    pub fn directory(&self) -> &Root { &self.parent }
    pub fn namespace(&self) -> &DirectoryNamespace { &self.namespace }
    /// Distinct explicit native Root handles plus the opaque lease's hard
    /// descriptor budget (including reserved future local enrollment handles).
    /// Transient ancestry/metadata inspection needs at most three extra
    /// native descriptors and must also fit the owning worker's admission.
    pub fn retained_descriptor_count(&self) -> usize { self.retained_descriptors }
}

#[cfg(all(test, target_os = "linux"))]
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
        assert!(placement.require_payload_registration().is_err(), "predicate-only fixture callback grants no payload creation capability");
        assert!(placement.begin_payload_creation().is_err());
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

    #[test]
    fn current_adoption_census_refuses_enclosing_root_but_allows_unrelated_root() {
        let tree = tempfile::tempdir().unwrap();
        for path in ["source", "outside", "unrelated"] { fs::create_dir(tree.path().join(path)).unwrap(); }
        let source = root(&tree.path().join("source"));
        let placement = RecoveryPlacement::acquire(root(&tree.path().join("outside")), source.clone(), Arc::from([]), registry()).unwrap();
        placement.validate_against_roots(&[root(&tree.path().join("unrelated"))]).unwrap();
        assert!(placement.validate_against_roots(&[root(tree.path())]).is_err());
        assert_eq!(recovery_directory_stamp(&source).unwrap(), recovery_directory_stamp(&source.try_clone().unwrap()).unwrap());
        placement.validate_for_source(&source).unwrap();
        assert!(placement.validate_for_source(&root(&tree.path().join("unrelated"))).is_err());
    }

    #[test]
    fn actual_published_payload_relocation_is_checked_separately_from_its_parent() {
        let tree = tempfile::tempdir().unwrap();
        for path in ["source", "outside", "foreign", "foreign/.git"] { fs::create_dir(tree.path().join(path)).unwrap(); }
        let placement = RecoveryPlacement::acquire(root(&tree.path().join("outside")), root(&tree.path().join("source")), Arc::from([]), registry()).unwrap();
        let DirectoryCreation::Published(publication) = create_directory_in_namespace(placement.namespace(), placement.directory(), Path::new("holding"), 0o700).unwrap() else { panic!("fixture must own the published holding"); };
        fs::write(tree.path().join("outside/holding/retained"), b"synthetic retained bytes\n").unwrap();
        placement.validate_payload_root(publication.directory()).unwrap();
        fs::rename(tree.path().join("outside/holding"), tree.path().join("source/moved")).unwrap();
        placement.validate().unwrap();
        assert!(placement.validate_payload_root(publication.directory()).is_err());
        fs::rename(tree.path().join("source/moved"), tree.path().join("foreign/moved")).unwrap();
        placement.validate().unwrap();
        assert!(placement.validate_payload_root(publication.directory()).is_err());
        assert_eq!(fs::read(tree.path().join("foreign/moved/retained")).unwrap(), b"synthetic retained bytes\n");
        assert!(!tree.path().join("foreign/moved/new-payload").exists());
        fs::rename(tree.path().join("foreign/moved"), tree.path().join("outside/holding")).unwrap();
        placement.validate_payload_root(publication.directory()).unwrap();
        assert!(placement.validate_payload_against_roots(publication.directory(), &[root(tree.path())]).is_err());
    }

    #[test]
    fn owned_payload_enrollment_retains_unknown_handoff_and_refuses_budget_growth() {
        struct Lease {
            payload: std::sync::Mutex<Option<Root>>,
            uncertain: AtomicBool,
        }
        impl RecoveryRegistrationLease for Lease {
            fn validate(&self) -> io::Result<()> { Ok(()) }
            fn descriptor_budget(&self) -> usize { 1 }
            fn register_payload(&self, payload: &Root) -> io::Result<()> {
                let mut owned = self.payload.lock().unwrap();
                match owned.as_ref() {
                    Some(root) if recovery_directory_stamp(root)? != recovery_directory_stamp(payload)? => return Err(unsupported("fixture payload descriptor reservation exhausted")),
                    Some(_) => {}
                    None => *owned = Some(payload.try_clone()?),
                }
                if self.uncertain.load(Ordering::Acquire) { Err(io::Error::other("fixture uncertain handoff")) } else { Ok(()) }
            }
        }
        let tree = tempfile::tempdir().unwrap();
        for path in ["source", "outside"] { fs::create_dir(tree.path().join(path)).unwrap(); }
        let lease = Arc::new(Lease { payload: std::sync::Mutex::new(None), uncertain: AtomicBool::new(true) });
        let placement = RecoveryPlacement::acquire(root(&tree.path().join("outside")), root(&tree.path().join("source")), Arc::from([]), RecoveryRegistryCheck::from_lease(lease.clone())).unwrap();
        placement.require_payload_registration().unwrap();
        assert!(placement.begin_payload_creation().is_err(), "enrollment alone cannot grant unsupported creation admission");
        assert_eq!(placement.retained_descriptor_count(), 4, "future local enrollment descriptor is reserved before effects");
        let DirectoryCreation::Published(publication) = create_directory_in_namespace(placement.namespace(), placement.directory(), Path::new("holding"), 0o700).unwrap() else { panic!("fixture must own the holding"); };
        assert!(placement.register_payload_root(publication.directory()).is_err());
        assert_eq!(recovery_directory_stamp(lease.payload.lock().unwrap().as_ref().unwrap()).unwrap(), recovery_directory_stamp(publication.directory()).unwrap());
        assert!(publication.directory().read_dir().unwrap().next().is_none());
        lease.uncertain.store(false, Ordering::Release);
        placement.register_payload_root(publication.directory()).unwrap();
        placement.register_payload_root(publication.directory()).unwrap();
        let DirectoryCreation::Published(other) = create_directory_in_namespace(placement.namespace(), placement.directory(), Path::new("other"), 0o700).unwrap() else { panic!("fixture must own the second holding"); };
        assert!(placement.register_payload_root(other.directory()).is_err());
        assert_eq!(recovery_directory_stamp(lease.payload.lock().unwrap().as_ref().unwrap()).unwrap(), recovery_directory_stamp(publication.directory()).unwrap());
    }

    #[test]
    fn creation_guard_fences_retirement_and_releases_failed_admission() {
        #[derive(Default)]
        struct State { sealed: bool, inflight: usize }
        struct Guard(Arc<std::sync::Mutex<State>>);
        impl Drop for Guard {
            fn drop(&mut self) { self.0.lock().unwrap().inflight -= 1; }
        }
        struct Lease {
            state: Arc<std::sync::Mutex<State>>,
            valid: AtomicBool,
            invalidate_on_begin: AtomicBool,
        }
        impl RecoveryRegistrationLease for Lease {
            fn validate(&self) -> io::Result<()> {
                if self.valid.load(Ordering::Acquire) { Ok(()) } else { Err(unsupported("fixture invalidated")) }
            }
            fn descriptor_budget(&self) -> usize { 0 }
            fn register_payload(&self, _payload: &Root) -> io::Result<()> { self.validate() }
            fn begin_payload_creation(&self) -> io::Result<Arc<dyn Send + Sync>> {
                let mut state = self.state.lock().unwrap();
                if state.sealed { return Err(unsupported("fixture sealed")); }
                state.inflight += 1;
                self.valid.store(!self.invalidate_on_begin.load(Ordering::Acquire), Ordering::Release);
                Ok(Arc::new(Guard(self.state.clone())))
            }
        }
        let tree = tempfile::tempdir().unwrap();
        for path in ["source", "outside"] { fs::create_dir(tree.path().join(path)).unwrap(); }
        let lease = Arc::new(Lease { state: Arc::default(), valid: AtomicBool::new(true), invalidate_on_begin: AtomicBool::new(false) });
        let placement = RecoveryPlacement::acquire(root(&tree.path().join("outside")), root(&tree.path().join("source")), Arc::from([]), RecoveryRegistryCheck::from_lease(lease.clone())).unwrap();
        let guard = placement.begin_payload_creation().unwrap();
        let retained = guard.clone();
        lease.state.lock().unwrap().sealed = true;
        assert!(placement.begin_payload_creation().is_err());
        drop(guard);
        assert_eq!(lease.state.lock().unwrap().inflight, 1, "retained recovery slot still prevents retirement");
        placement.validate().unwrap();
        drop(retained);
        assert_eq!(lease.state.lock().unwrap().inflight, 0);
        lease.state.lock().unwrap().sealed = false;
        lease.invalidate_on_begin.store(true, Ordering::Release);
        assert!(placement.begin_payload_creation().is_err());
        assert_eq!(lease.state.lock().unwrap().inflight, 0, "failed post-admission validation releases unused guard");
        assert!(!tree.path().join("outside/holding").exists());
        eprintln!("RECOVERY_CREATION_ADMISSION_GUARD_VERIFIED");
    }

    #[test]
    fn recovery_self_bind_mount_child() {
        let Some(case_path) = std::env::var_os("CAGENTS_TEST_RECOVERY_BIND_ROOT") else { return; };
        // The helper may mount only in the two fresh namespaces created by its
        // owning unshare child, never in the caller's original namespace.
        let original_mount = std::env::var("CAGENTS_TEST_RECOVERY_MOUNT_NS").unwrap().parse::<u64>().unwrap();
        let original_user = std::env::var("CAGENTS_TEST_RECOVERY_USER_NS").unwrap().parse::<u64>().unwrap();
        assert_ne!(fs::metadata("/proc/self/ns/mnt").unwrap().ino(), original_mount);
        assert_ne!(fs::metadata("/proc/self/ns/user").unwrap().ino(), original_user);
        let case_root = PathBuf::from(case_path);
        let cache = case_root.join("repo/cache");
        let target = cache.join("self");
        let source_name = c_path(&cache).unwrap();
        let target_name = c_path(&target).unwrap();
        assert_eq!(unsafe { libc::mount(source_name.as_ptr(), target_name.as_ptr(), std::ptr::null(), libc::MS_BIND, std::ptr::null()) }, 0, "{}", io::Error::last_os_error());
        struct Mount(CString);
        impl Drop for Mount {
            fn drop(&mut self) { unsafe { libc::umount2(self.0.as_ptr(), libc::MNT_DETACH) }; }
        }
        let _mount = Mount(target_name);
        // Both operation source and proposed placement are inside the SAME
        // bind mount, so the cross-mount admission refusal cannot mask the
        // ancestor walk regression. Its outside known repo is reached only
        // after the equal-device/inode, different-mount `..` edge.
        let source = root(&target.join("edited"));
        let candidate = root(&target.join("outside"));
        assert_eq!(mount_identity(&source).unwrap(), mount_identity(&candidate).unwrap());
        let outer = root(&case_root.join("repo"));
        let result = RecoveryPlacement::acquire(candidate, source, vec![outer].into(), registry());
        assert!(result.is_err());
        assert!(result.err().unwrap().to_string().contains("known edited or registered checkout root"));
        eprintln!("RECOVERY_SELF_BIND_ANCESTRY_VERIFIED");
    }

    #[test]
    fn placement_walk_does_not_stop_at_a_self_bind_mount_alias() {
        use std::process::{Child, Command, Stdio};
        if !Path::new("/usr/bin/unshare").is_file() {
            eprintln!("RECOVERY_SELF_BIND_UNAVAILABLE: unshare is not installed");
            return;
        }
        let tree = tempfile::tempdir().unwrap();
        for path in ["repo", "repo/cache", "repo/cache/self", "repo/cache/edited", "repo/cache/outside"] {
            fs::create_dir(tree.path().join(path)).unwrap();
        }
        struct Owner(Child);
        impl Drop for Owner {
            fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
        }
        let stdout = tree.path().join("child.stdout");
        let stderr = tree.path().join("child.stderr");
        let mut child = Owner(Command::new("/usr/bin/unshare")
            .args(["--user", "--map-root-user", "--mount", "--propagation", "private", "--"])
            .arg(std::env::current_exe().unwrap())
            .args(["fs_safety::recovery_placement::tests::recovery_self_bind_mount_child", "--exact", "--nocapture", "--test-threads=1"])
            .env("CAGENTS_TEST_RECOVERY_BIND_ROOT", tree.path())
            .env("CAGENTS_TEST_RECOVERY_MOUNT_NS", fs::metadata("/proc/self/ns/mnt").unwrap().ino().to_string())
            .env("CAGENTS_TEST_RECOVERY_USER_NS", fs::metadata("/proc/self/ns/user").unwrap().ino().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::from(fs::File::create(&stdout).unwrap()))
            .stderr(Stdio::from(fs::File::create(&stderr).unwrap()))
            .spawn().unwrap());
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        let status = loop {
            if let Some(status) = child.0.try_wait().unwrap() { break status; }
            assert!(std::time::Instant::now() < deadline, "isolated self-bind helper timed out");
            std::thread::sleep(std::time::Duration::from_millis(10));
        };
        let errors = fs::read_to_string(stderr).unwrap();
        if !status.success() && errors.contains("unshare failed: Operation not permitted") {
            eprintln!("RECOVERY_SELF_BIND_UNAVAILABLE: private user/mount namespaces denied");
            return;
        }
        assert!(status.success(), "private mount helper failed: {} {errors}", fs::read_to_string(stdout).unwrap());
        assert!(errors.contains("RECOVERY_SELF_BIND_ANCESTRY_VERIFIED"), "helper did not execute the actual bind assertion: {errors}");
        eprintln!("RECOVERY_SELF_BIND_ANCESTRY_VERIFIED");
    }
}
