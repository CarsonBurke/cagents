//! Live recovery for one presence transaction. Every retry uses the retained
//! parent and exact file witness; diagnostic paths never authorize effects.
//!
//! The freshly owned0700 original holding Root and its INTERNAL contents use
//! the protected-private namespace assumption throughout transaction and retry:
//! noncooperating same-UID/root changes to these internal leaves are excluded.
//! Public source names, public holding directory entries/ancestors/lookalikes,
//! and mutations through pre-existing source descriptors remain in scope. Leaf
//! checks below do not claim universal atomic content CAS; all private effects
//! stay relative to the retained original directory, never a reconstructed path.
use super::*;

struct LeafWitness {
    metadata: fs::Metadata,
    attributes: std::collections::BTreeMap<OsString, Vec<u8>>,
    hash: blake3::Hash,
}
struct AuthenticatedLeaf {
    file: File,
    witness: Option<LeafWitness>,
}
impl AuthenticatedLeaf {
    fn capture(&mut self, root: &Root, name: &std::ffi::OsStr) -> Result<(), FileIoError> {
        let metadata = self.file.metadata().map_err(io_error)?;
        let attributes = fs_safety::content_attributes(&self.file).map_err(io_error)?;
        let bytes = fs_safety::read_bounded(&mut self.file, MAX_EDITABLE_FILE_BYTES).map_err(io_error)?;
        if !metadata.is_file() || version(&metadata) != version(&self.file.metadata().map_err(io_error)?) {
            return Err(FileIoError::Io("The acquired file changed while retaining its recovery witness.".into()));
        }
        self.witness = Some(LeafWitness { metadata, attributes, hash: blake3::hash(&bytes) });
        if let Err(error) = self.verify(root, name) { self.witness = None; return Err(error); }
        Ok(())
    }
    fn verify(&self, root: &Root, name: &std::ffi::OsStr) -> Result<(), FileIoError> {
        let witness = self.witness.as_ref().ok_or_else(|| FileIoError::Io("The acquired file could not be authenticated. Its original descriptor and directory were retained; recovery cannot adopt a fresh file witness.".into()))?;
        let mut named = fs_safety::open_beneath(root, Path::new(name)).map_err(resolve_error)?;
        let before = named.metadata().map_err(io_error)?;
        let bytes = fs_safety::read_bounded(&mut named, MAX_EDITABLE_FILE_BYTES).map_err(io_error)?;
        let after = named.metadata().map_err(io_error)?;
        if !same_inode(&witness.metadata, &before) || version(&witness.metadata) != version(&before)
            || version(&before) != version(&after)
            || version(&witness.metadata) != version(&self.file.metadata().map_err(io_error)?)
            || blake3::hash(&bytes) != witness.hash
            || fs_safety::content_attributes(&named).map_err(io_error)? != witness.attributes
        {
            return Err(FileIoError::Io("The retained file changed; its contents and replacement names were preserved for recovery.".into()));
        }
        Ok(())
    }
}

struct OwnedTemporary { file: File, name: OsString }

pub(super) struct PresenceHolding {
    publication: fs_safety::DirectoryPublication,
    context: FilesystemRecoveryContext,
    registered: bool,
    // None means consumer-private setup has not completed. The producer's
    // immutable initial access witness then remains the only permitted base.
    private_access: Option<(u32, u32, u32)>,
    parent: Root,
    name: OsString,
    original: Option<AuthenticatedLeaf>,
    pre_acquire_source: Option<File>,
    temporary: Option<OwnedTemporary>,
    pub(super) restore_original: bool,
    slot: Option<RecoverySlot>,
}
impl PresenceHolding {
    #[cfg(test)]
    pub(super) fn create(parent: &Root, name: &std::ffi::OsStr) -> Result<Self, FileIoError> {
        let context = crate::filesystem_recovery_context::fixture_context(parent).map_err(io_error)?;
        Self::create_in_context(parent, name, &context)
    }
    pub(super) fn create_in_context(parent: &Root, name: &std::ffi::OsStr, context: &FilesystemRecoveryContext) -> Result<Self, FileIoError> {
        Self::create_in_context_with(parent, name, context, |namespace, parent, leaf| fs_safety::create_directory_in_namespace(namespace, parent, leaf, 0o700))
    }
    fn create_in_context_with(parent: &Root, name: &std::ffi::OsStr, context: &FilesystemRecoveryContext, create: impl FnOnce(&fs_safety::DirectoryNamespace, &Root, &Path) -> Result<fs_safety::DirectoryCreation, fs_safety::DirectoryCreateError>) -> Result<Self, FileIoError> {
        let slot = context.reserve().map_err(FileIoError::Io)?;
        let parent = parent.try_clone().map_err(io_error)?;
        // The public source parent is distinct from the already-issued outside
        // placement. Never create a payload directory beside a source file.
        let namespace = context.placement().namespace();
        let outside = context.placement().directory();
        let leaf = OsString::from(format!(".cagents-editor-{}", uuid::Uuid::new_v4()));
        let creation = match create(namespace, outside, Path::new(&leaf)) {
            Ok(creation) => creation,
            Err(mut error) => {
                let message = error.to_string();
                if let Some(mut preparation) = error.take_recovery() {
                    // Preparation owns at most3 roots, plus this exact parent.
                    // Capture the whole typed object before converting cause
                    // text; a failed private bootstrap is still an owner.
                    let context = context.clone();
                    let authority = slot.retain(message.clone(), move || {
                        context.placement().validate().map_err(|error| error.to_string())?;
                        preparation.retry_cleanup().map_err(|error| error.to_string())?;
                        fs_safety::sync_directory(context.placement().directory()).map_err(|error| error.to_string())?;
                        fs_safety::sync_directory(&parent).map_err(|error| error.to_string())
                    });
                    return Err(FileIoError::Recovery { message, authority });
                }
                return Err(FileIoError::Io(message));
            }
        };
        let publication = match creation {
            fs_safety::DirectoryCreation::Published(receipt) => receipt,
            fs_safety::DirectoryCreation::Existing(_) => return Err(FileIoError::Io("The editor staging name already exists and was preserved.".into())),
            fs_safety::DirectoryCreation::Unverified { directory, reason } => {
                let message = format!("Editor staging ownership could not be verified; the entry was preserved: {reason}. Diagnostic original spelling (not verified location): {}", directory.path().display());
                let context = context.clone();
                let authority = slot.retain(message.clone(), move || {
                    context.placement().validate().map_err(|error| error.to_string())?;
                    // Ownership was never established. Never chmod/delete or
                    // adopt a replacement from a diagnostic public spelling.
                    if directory.metadata().map_err(|error| error.to_string())?.nlink() != 0 {
                        return Err("Creation ownership remains unverified. Inspect and remove the retained entry manually before retrying; no entry was changed.".into());
                    }
                    fs_safety::sync_directory(context.placement().directory()).map_err(|error| error.to_string())?;
                    fs_safety::sync_directory(&parent).map_err(|error| error.to_string())
                });
                return Err(FileIoError::Recovery { message, authority });
            },
        };
        let mut holding = Self { publication, context: context.clone(), registered: false, private_access: None, parent, name: name.to_os_string(), original: None, pre_acquire_source: None, temporary: None, restore_original: false, slot: Some(slot) };
        let prepared = (|| {
            if let Some(error) = holding.publication.publication_error() { return Err(FileIoError::Io(error.into())); }
            // A restrictive umask may leave our authenticated Published inode
            // mode000. Establish its exact witnessed private access before an
            // ancestry walk requires search permission, but write no payload.
            holding.establish_private_access().map_err(FileIoError::Io)?;
            holding.context.register_payload_root(holding.directory()).map_err(io_error)?;
            holding.registered = true;
            Ok(())
        })();
        // An enrollment error can mean unknown daemon handoff. Keep the LIVE
        // original H and lease; local empty cleanup cannot settle that handoff.
        match prepared { Ok(()) => Ok(holding), Err(error) => Err(holding.retain(error.to_string())) }
    }
    pub(super) fn directory(&self) -> &Root { self.publication.directory() }
    pub(super) fn validate_live(&self) -> Result<(), FileIoError> {
        self.context.validate_payload_root(self.directory()).map_err(io_error)?;
        if let Some(expected) = self.private_access {
            if Self::access(&self.directory().metadata().map_err(io_error)?) != expected {
                return Err(FileIoError::Io("The retained editor staging access metadata changed; its files and newer permissions were preserved.".into()));
            }
        }
        Ok(())
    }
    fn access(metadata: &fs::Metadata) -> (u32, u32, u32) { (metadata.uid(), metadata.gid(), metadata.mode()) }
    fn establish_private_access(&mut self) -> Result<(), String> {
        let current = self.directory().metadata().map_err(|error| error.to_string())?;
        if let Some(expected) = self.private_access {
            if Self::access(&current) != expected {
                return Err("The retained editor staging access metadata changed. Its newer permissions and files were preserved.".into());
            }
            return Ok(());
        }
        let initial = self.publication.initial_metadata();
        if Self::access(&current) != Self::access(initial) {
            return Err("The staging access metadata changed before private setup completed. It was preserved for recovery.".into());
        }
        let expected = (initial.uid(), initial.gid(), (initial.mode() & !0o7777) | 0o700);
        fs_safety::set_directory_permissions(self.directory(), 0o700).map_err(|error| error.to_string())?;
        // Record only the exact access state established by our successful
        // syscall, before any later fallible verification. Never adopt current
        // metadata as a replacement permission witness during retry.
        self.private_access = Some(expected);
        if Self::access(&self.directory().metadata().map_err(|error| error.to_string())?) != expected {
            return Err("The editor staging access metadata changed during private setup.".into());
        }
        Ok(())
    }
    pub(super) fn create_temporary(&mut self) -> Result<(), FileIoError> {
        self.validate_live()?;
        if self.temporary.is_some() { return Err(FileIoError::Io("An editor temporary file is already retained.".into())); }
        let (file, name) = fs_safety::create_temporary_in(self.directory(), 0o600).map_err(resolve_error)?;
        self.temporary = Some(OwnedTemporary { file, name });
        Ok(())
    }
    pub(super) fn temporary(&self) -> Result<&File, FileIoError> { self.validate_live()?; Ok(&self.temporary.as_ref().expect("temporary created").file) }
    pub(super) fn temporary_mut(&mut self) -> Result<&mut File, FileIoError> { self.validate_live()?; Ok(&mut self.temporary.as_mut().expect("temporary created").file) }
    pub(super) fn temporary_name(&self) -> &std::ffi::OsStr { &self.temporary.as_ref().expect("temporary created").name }
    fn verify_temporary(&self) -> Result<(), FileIoError> {
        self.validate_live()?;
        let Some(temporary) = &self.temporary else { return Ok(()); };
        let actual = fs_safety::metadata_beneath(self.directory(), Path::new(&temporary.name)).map_err(resolve_error)?;
        if !same_inode(&temporary.file.metadata().map_err(io_error)?, &actual) {
            return Err(FileIoError::Io("The editor temporary name was replaced; its original descriptor and foreign entry were retained.".into()));
        }
        Ok(())
    }
    pub(super) fn unlink_temporary(&mut self) -> Result<File, FileIoError> {
        self.verify_temporary()?;
        let temporary = self.temporary.as_ref().expect("temporary created");
        fs_safety::unlink_within(self.directory(), &temporary.name).map_err(resolve_error)?;
        Ok(self.temporary.take().unwrap().file)
    }
    pub(super) fn publish_temporary(&mut self) -> Result<(), FileIoError> {
        self.verify_temporary()?;
        let temporary = self.temporary.as_ref().expect("temporary created");
        fs_safety::rename_new_between(self.directory(), &temporary.name, &self.parent, &self.name).map_err(resolve_error)?;
        self.temporary = None;
        Ok(())
    }
    pub(super) fn pin_source(&mut self, source: File) { self.pre_acquire_source = Some(source); }
    pub(super) fn capture_original(&mut self) -> Result<(), FileIoError> {
        self.capture_original_with_hooks(|root| fs_safety::open_beneath(root, Path::new("file")).map_err(resolve_error), |_| Ok(()))
    }
    fn capture_original_with_hooks(&mut self, open: impl FnOnce(&Root) -> Result<File, FileIoError>, after_pin: impl FnOnce(&File) -> Result<(), FileIoError>) -> Result<(), FileIoError> {
        self.validate_live()?;
        let file = open(self.directory())?;
        // Pin first: metadata/read/coherence failure must never drop the
        // acquired descriptor or retry by adopting a newly opened witness.
        self.original = Some(AuthenticatedLeaf { file, witness: None });
        after_pin(&self.original.as_ref().unwrap().file)?;
        self.original.as_mut().unwrap().capture(self.publication.directory(), std::ffi::OsStr::new("file"))
    }
    pub(super) fn remove_original(&mut self) -> Result<(), FileIoError> {
        self.validate_live()?;
        if let Some(original) = &self.original {
            original.verify(self.directory(), std::ffi::OsStr::new("file"))?;
            fs_safety::unlink_within(self.directory(), std::ffi::OsStr::new("file")).map_err(resolve_error)?;
            self.original = None;
        }
        Ok(())
    }
    fn retry_cleanup(&mut self) -> Result<(), String> {
        self.context.placement().validate().map_err(|error| error.to_string())?;
        if self.publication.is_removed() {
            // A prior successful cleanup can be followed by fsync failure.
            // Never walk the ancestry of an authenticated unlinked holding.
            fs_safety::sync_directory(self.context.placement().directory()).map_err(|error| error.to_string())?;
            return fs_safety::sync_directory(&self.parent).map_err(|error| error.to_string());
        }
        // Restore the producer's exact pending mode witness before applying
        // this consumer's explicit private0700 policy to its original inode.
        // This step never acquires/removes a public recovery entry.
        self.publication.retry_publication_steps().map_err(|error| error.to_string())?;
        self.establish_private_access()?;
        if !self.registered {
            self.context.register_payload_root(self.directory()).map_err(|error| error.to_string())?;
            self.registered = true;
        }
        self.context.validate_payload_root(self.directory()).map_err(|error| error.to_string())?;
        if self.temporary.is_some() { self.unlink_temporary().map_err(|error| error.to_string())?; }
        if self.restore_original {
            if let Some(original) = &self.original {
                original.verify(self.directory(), std::ffi::OsStr::new("file")).map_err(|error| error.to_string())?;
                fs_safety::rename_new_between(self.directory(), std::ffi::OsStr::new("file"), &self.parent, &self.name).map_err(|error| error.to_string())?;
                self.original = None;
            }
        } else {
            self.remove_original().map_err(|error| error.to_string())?;
        }
        fs_safety::sync_directory(self.directory()).map_err(|error| error.to_string())?;
        self.publication.resolve_recovery().map_err(|error| error.to_string())?;
        self.publication.cleanup_empty().map_err(|error| error.to_string())?;
        fs_safety::sync_directory(self.context.placement().directory()).map_err(|error| error.to_string())?;
        // Explicit retries bypass the caller's original sync_durability path.
        // A sync failure keeps this same authority unresolved for another try.
        fs_safety::sync_directory(&self.parent).map_err(|error| error.to_string())
    }
    pub(super) fn finish(mut self) -> Result<(), FileIoError> {
        match self.retry_cleanup() { Ok(()) => Ok(()), Err(error) => Err(self.retain(error)) }
    }
    pub(super) fn finish_error(mut self, error: FileIoError) -> FileIoError {
        match self.retry_cleanup() {
            Ok(()) => error,
            Err(cleanup) => combine_recovery_errors(error, self.retain(cleanup)),
        }
    }
    fn retain(mut self, error: String) -> FileIoError {
        let slot = self.slot.take().expect("presence reserves recovery before effects");
        let description = format!("Editor presence staging requires recovery. {error} Diagnostic opened spelling: {}", self.directory().path().display());
        // DirectoryPublication <=5 root descriptors, parent1, original1,
        // pre-acquisition source1, temporary1. The
        // closure captures the WHOLE owner (not disjoint fields), below16 FDs.
        let authority = slot.retain(description.clone(), move || self.retry_cleanup());
        FileIoError::Recovery { message: description, authority }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;
    #[test]
    fn uncertain_payload_enrollment_retains_live_original_until_same_lease_accepts_retry() {
        use std::sync::{Mutex, atomic::{AtomicBool, Ordering}};
        #[derive(Default)]
        struct Creation { sealed: bool, inflight: usize }
        struct Admission(Arc<Mutex<Creation>>);
        impl Drop for Admission {
            fn drop(&mut self) {
                let mut state = self.0.lock().unwrap();
                state.inflight = state.inflight.checked_sub(1).expect("one original creation guard");
            }
        }
        struct Lease { payload: Mutex<Option<Root>>, uncertain: Arc<AtomicBool>, creation: Arc<Mutex<Creation>> }
        impl fs_safety::RecoveryRegistrationLease for Lease {
            fn validate(&self) -> io::Result<()> { Ok(()) }
            fn begin_payload_creation(&self) -> io::Result<Arc<dyn Send + Sync>> {
                let mut state = self.creation.lock().map_err(|_| io::Error::other("fixture creation state poisoned"))?;
                if state.sealed { return Err(io::Error::other("fixture lease is retiring")); }
                state.inflight = state.inflight.checked_add(1).ok_or_else(|| io::Error::other("fixture creation count exhausted"))?;
                Ok(Arc::new(Admission(self.creation.clone())))
            }
            fn descriptor_budget(&self) -> usize { 1 }
            fn register_payload(&self, root: &Root) -> io::Result<()> {
                let mut payload = self.payload.lock().unwrap();
                if let Some(original) = payload.as_ref() {
                    if fs_safety::recovery_directory_stamp(original)? != fs_safety::recovery_directory_stamp(root)? { return Err(io::Error::other("different original payload")); }
                } else { *payload = Some(root.try_clone()?); }
                if self.uncertain.load(Ordering::Acquire) { Err(io::Error::other("uncertain actual lease handoff")) } else { Ok(()) }
            }
        }
        let tree = tempfile::tempdir().unwrap(); let runtime = tempfile::tempdir().unwrap();
        fs::create_dir(tree.path().join("source")).unwrap(); fs::create_dir(tree.path().join("outside")).unwrap();
        let source = Arc::new(Root::open(&tree.path().join("source")).unwrap()); let outside = Arc::new(Root::open(&tree.path().join("outside")).unwrap());
        let creation = Arc::new(Mutex::new(Creation::default()));
        let uncertain = Arc::new(AtomicBool::new(true)); let lease = Arc::new(Lease { payload: Mutex::new(None), uncertain: uncertain.clone(), creation: creation.clone() }); let weak = Arc::downgrade(&lease);
        let placement = fs_safety::RecoveryPlacement::acquire(outside, source.clone(), Arc::from([source.clone()]), fs_safety::RecoveryRegistryCheck::from_lease(lease.clone())).unwrap();
        let protection = Arc::new(Some(crate::worktree_cleanup::fixture_protection(runtime.path(), source.path(), false).unwrap()));
        let context = FilesystemRecoveryContext::new(&source, protection, placement).unwrap();
        let error = match PresenceHolding::create_in_context(&source, std::ffi::OsStr::new("file"), &context) { Err(error) => error, Ok(_) => panic!("unknown handoff must refuse payload effects") };
        let authority = error.recovery_authority().expect("unknown registration keeps its original live holding").clone();
        let original = lease.payload.lock().unwrap().as_ref().unwrap().try_clone().unwrap();
        assert_eq!(creation.lock().unwrap().inflight, 1, "unknown handoff keeps the original slot's guard");
        creation.lock().unwrap().sealed = true;
        let before = context.placement().directory().read_dir().unwrap().count();
        assert!(PresenceHolding::create_in_context(&source, std::ffi::OsStr::new("second"), &context).is_err());
        assert_eq!(context.placement().directory().read_dir().unwrap().count(), before, "sealed admission refuses before mkdir");
        drop(lease); drop(context); drop(error);
        assert!(weak.upgrade().is_some()); assert!(original.metadata().unwrap().nlink() > 0); assert!(original.read_dir().unwrap().next().is_none());
        assert!(authority.retry().is_err()); assert!(original.metadata().unwrap().nlink() > 0, "local cleanup cannot erase an unknown handoff");
        assert_eq!(creation.lock().unwrap().inflight, 1, "failed retry keeps that same creation admission");
        uncertain.store(false, Ordering::Release); authority.retry().unwrap(); assert!(authority.is_resolved());
        assert_eq!(creation.lock().unwrap().inflight, 0, "only actual resolution releases the guard");
        assert_eq!(original.metadata().unwrap().nlink(), 0); assert!(weak.upgrade().is_none());
    }

    #[test]
    fn registered_presence_payload_stays_outside_actual_git_discovery_and_refuses_public_move() {
        let tree = tempfile::tempdir().unwrap(); let source = tree.path().join("source");
        fs::create_dir(&source).unwrap();
        let git = |args: &[&str]| {
            let mut command = std::process::Command::new("git"); command.current_dir(&source);
            for (key, _) in std::env::vars_os().filter(|(key, _)| key.as_encoded_bytes().starts_with(b"GIT_")) { command.env_remove(key); }
            command.env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_CONFIG_SYSTEM", "/dev/null").env("GIT_CONFIG_GLOBAL", "/dev/null");
            let output = command.args(args).output().unwrap(); assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr)); output.stdout
        };
        git(&["init", "-b", "main"]);
        let root = Root::open(&source).unwrap(); let context = crate::filesystem_recovery_context::fixture_context(&root).unwrap();
        let mut holding = PresenceHolding::create_in_context(&root, std::ffi::OsStr::new("file"), &context).unwrap();
        holding.create_temporary().unwrap(); holding.temporary_mut().unwrap().write_all(b"private recovery payload").unwrap();
        git(&["add", "--all"]); assert!(git(&["ls-files", "--stage"]).is_empty(), "same-user Git must not discover the retained payload");
        let outside = holding.directory().path().to_owned(); let moved = source.join("moved-holding");
        fs::rename(&outside, &moved).unwrap();
        assert!(context.placement().validate().is_ok(), "the original outside parent alone is insufficient");
        assert!(holding.create_temporary().is_err()); assert!(holding.publish_temporary().is_err());
        let error = holding.finish().unwrap_err(); let authority = error.recovery_authority().unwrap();
        assert!(authority.retry().is_err()); assert!(!source.join("file").exists());
        fs::create_dir(&outside).unwrap(); fs::write(outside.join("foreign"), b"preserve").unwrap();
        assert!(authority.retry().is_err()); assert_eq!(fs::read(outside.join("foreign")).unwrap(), b"preserve");
        fs::remove_file(outside.join("foreign")).unwrap(); fs::remove_dir(&outside).unwrap(); fs::rename(&moved, &outside).unwrap();
        authority.retry().unwrap(); assert!(authority.is_resolved()); assert!(!outside.exists());
    }

    #[test]
    fn recovery_does_not_overwrite_newer_original_holding_permissions() {
        let directory = tempfile::tempdir().unwrap(); let root = Root::open(directory.path()).unwrap();
        let holding = PresenceHolding::create(&root, std::ffi::OsStr::new("file")).unwrap();
        let original = holding.directory().try_clone().unwrap();
        fs_safety::set_directory_permissions(&original, 0o711).unwrap();
        let error = holding.finish().unwrap_err(); let authority = error.recovery_authority().unwrap();
        assert!(authority.retry().is_err());
        assert_eq!(original.metadata().unwrap().mode() & 0o7777, 0o711);
        fs_safety::set_directory_permissions(&original, 0o700).unwrap();
        authority.retry().unwrap(); assert!(authority.is_resolved());
    }

    #[test]
    fn unverified_actual_mkdir_keeps_typed_receipt_without_chmod_or_guessed_cleanup() {
        let directory = tempfile::tempdir().unwrap(); let other = tempfile::tempdir().unwrap();
        let root = Root::open(directory.path()).unwrap(); let other = Root::open(other.path()).unwrap();
        let namespace = fs_safety::DirectoryNamespace::acquire(&other).unwrap();
        let context = crate::filesystem_recovery_context::fixture_context(&root).unwrap(); let mut created = None;
        let result = PresenceHolding::create_in_context_with(&root, std::ffi::OsStr::new("file"), &context, |_, parent, leaf| {
            created = Some(leaf.to_path_buf());
            fs_safety::create_directory_in_namespace(&namespace, parent, leaf, 0o700)
        });
        let error = match result { Err(error) => error, Ok(_) => panic!("different namespace cannot prove creation") };
        let authority = error.recovery_authority().expect("unverified native mkdir keeps a live carrier");
        let outside = context.placement().directory();
        let leaf = created.unwrap(); let metadata = fs_safety::metadata_beneath(outside, &leaf).unwrap();
        assert!(authority.retry().is_err());
        let after = fs_safety::metadata_beneath(outside, &leaf).unwrap();
        assert_eq!((metadata.dev(), metadata.ino(), metadata.mode()), (after.dev(), after.ino(), after.mode()));
        fs_safety::remove_directory_within(outside, leaf.as_os_str()).unwrap();
        // A replacement at the same spelling must remain untouched. Only the
        // retained original's unlink state permits authority resolution.
        fs::create_dir(outside.path().join(&leaf)).unwrap(); fs::write(outside.path().join(&leaf).join("foreign"), "keep").unwrap();
        authority.retry().unwrap(); assert!(authority.is_resolved());
        assert_eq!(fs::read(outside.path().join(leaf).join("foreign")).unwrap(), b"keep");
    }

    #[test]
    fn actual_prepublication_error_retains_preparation_and_cleanup_lease() {
        let directory = tempfile::tempdir().unwrap(); let runtime = tempfile::tempdir().unwrap();
        let named = directory.path().join("root"); let moved = directory.path().join("original");
        fs::create_dir(&named).unwrap(); let root = Root::open(&named).unwrap();
        let protection = crate::worktree_cleanup::fixture_protection(runtime.path(), &named, false).unwrap();
        let fixture = crate::filesystem_recovery_context::fixture_context(&root).unwrap();
        let context = FilesystemRecoveryContext::new(&root, Arc::new(Some(protection)), fixture.placement().clone()).unwrap();
        drop(fixture);
        let mut private = None;
        let result = PresenceHolding::create_in_context_with(&root, std::ffi::OsStr::new("file"), &context, |_, parent, _| {
            let error = fs_safety::fixture_directory_preparation_failure(parent);
            private = Some(Root::open(&error.recovery().expect("real producer failure must retain preparation").diagnostic_path()).unwrap());
            Err(error)
        });
        let error = match result { Err(error) => error, Ok(_) => panic!("actual preparation fault must fail") };
        let authority = error.recovery_authority().expect("consumer must preserve typed preparation").clone();
        drop(error); drop(context);
        fs::rename(&named, &moved).unwrap(); fs::create_dir(&named).unwrap(); fs::write(named.join("foreign"), "keep").unwrap();
        assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), &moved, true).is_err());
        assert!(authority.retry().is_err());
        assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), &moved, true).is_err());
        fs_safety::unlink_within(private.as_ref().unwrap(), std::ffi::OsStr::new("directory")).unwrap();
        authority.retry().unwrap(); assert!(authority.is_resolved());
        drop(crate::worktree_cleanup::fixture_protection(runtime.path(), &moved, true).unwrap());
        assert_eq!(fs::read(named.join("foreign")).unwrap(), b"keep");
    }

    #[test]
    fn private_writer_temporary_descriptor_survives_foreign_cleanup_and_failed_retry() {
        let directory = tempfile::tempdir().unwrap(); let parent = Root::open(directory.path()).unwrap();
        let mut holding = PresenceHolding::create(&parent, std::ffi::OsStr::new("file")).unwrap();
        holding.create_temporary().unwrap(); holding.temporary_mut().unwrap().write_all(b"ours").unwrap();
        let descriptor = holding.temporary().unwrap().as_raw_fd();
        let original = holding.directory().path().join(holding.temporary_name()); let saved = original.with_file_name("saved-writer");
        fs::rename(&original, &saved).unwrap(); fs::write(&original, b"foreign").unwrap();
        let error = holding.finish().unwrap_err(); let authority = error.recovery_authority().unwrap();
        assert!(authority.retry().is_err());
        assert!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) } >= 0);
        assert_eq!(fs::read(&original).unwrap(), b"foreign");
        fs::remove_file(&original).unwrap(); fs::rename(&saved, &original).unwrap();
        authority.retry().unwrap(); assert!(authority.is_resolved());
        assert!(!original.exists());
    }
    #[test]
    fn presence_recovery_keeps_preopened_source_when_acquired_open_fails() {
        let directory = tempfile::tempdir().unwrap(); let parent = Root::open(directory.path()).unwrap();
        fs::write(directory.path().join("file"), b"original").unwrap();
        let source = fs::File::open(directory.path().join("file")).unwrap();
        let descriptor = source.as_raw_fd();
        let mut holding = PresenceHolding::create(&parent, std::ffi::OsStr::new("file")).unwrap();
        holding.pin_source(source);
        fs_safety::rename_new_between(&parent, std::ffi::OsStr::new("file"), holding.directory(), std::ffi::OsStr::new("file")).unwrap();
        let failure = holding.capture_original_with_hooks(|_| Err(FileIoError::Io("injected acquired-open failure".into())), |_| Ok(())).unwrap_err();
        let error = holding.finish_error(failure); let authority = error.recovery_authority().unwrap();
        assert!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) } >= 0);
        assert!(authority.retry().is_err());
        assert!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) } >= 0);
    }
    #[test]
    fn presence_recovery_pins_acquired_descriptor_before_fallible_witness_capture() {
        let directory = tempfile::tempdir().unwrap(); let parent = Root::open(directory.path()).unwrap();
        fs::write(directory.path().join("file"), b"original").unwrap();
        let mut holding = PresenceHolding::create(&parent, std::ffi::OsStr::new("file")).unwrap();
        fs_safety::rename_new_between(&parent, std::ffi::OsStr::new("file"), holding.directory(), std::ffi::OsStr::new("file")).unwrap();
        let mut descriptor = -1;
        let failure = holding.capture_original_with_hooks(|root| fs_safety::open_beneath(root, Path::new("file")).map_err(resolve_error), |file| {
            descriptor = file.as_raw_fd();
            Err(FileIoError::Io("injected witness read failure".into()))
        }).unwrap_err();
        assert!(holding.original.as_ref().unwrap().witness.is_none());
        let acquired = holding.original.as_ref().unwrap().file.metadata().unwrap();
        let error = holding.finish_error(failure);
        let authority = error.recovery_authority().unwrap();
        // SAFETY: F_GETFD only inspects this descriptor number. The retained
        // authority must keep the original file, including after failed retry.
        assert!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) } >= 0);
        assert!(authority.retry().is_err());
        assert!(unsafe { libc::fcntl(descriptor, libc::F_GETFD) } >= 0);
        let mut metadata: libc::stat = unsafe { std::mem::zeroed() };
        assert_eq!(unsafe { libc::fstat(descriptor, &mut metadata) }, 0);
        assert_eq!((metadata.st_dev as u64, metadata.st_ino as u64), (acquired.dev(), acquired.ino()));
        assert!(!directory.path().join("file").exists());
    }
}
