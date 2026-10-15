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
    parent: Root,
    name: OsString,
    original: Option<AuthenticatedLeaf>,
    pre_acquire_source: Option<File>,
    temporary: Option<OwnedTemporary>,
    pub(super) restore_original: bool,
    slot: Option<RecoverySlot>,
}
impl PresenceHolding {
    pub(super) fn create(parent: &Root, name: &std::ffi::OsStr) -> Result<Self, FileIoError> {
        let protection = Arc::new(crate::worktree_cleanup::protect(parent.path()).map_err(io_error)?);
        let context = recovery_context_at(parent, protection)?;
        Self::create_in_context(parent, name, &context)
    }
    pub(super) fn create_in_context(parent: &Root, name: &std::ffi::OsStr, context: &RecoveryContext) -> Result<Self, FileIoError> {
        Self::create_in_context_with(parent, name, context, |namespace, parent, leaf| fs_safety::create_directory_in_namespace(namespace, parent, leaf, 0o700))
    }
    fn create_in_context_with(parent: &Root, name: &std::ffi::OsStr, context: &RecoveryContext, create: impl FnOnce(&fs_safety::DirectoryNamespace, &Root, &Path) -> Result<fs_safety::DirectoryCreation, fs_safety::DirectoryCreateError>) -> Result<Self, FileIoError> {
        let slot = context.reserve().map_err(FileIoError::Io)?;
        let parent = parent.try_clone().map_err(io_error)?;
        // Published only covers this exact protected parent. Shared or unknown
        // parents can return an unverified mkdir receipt; retain it without
        // chmod or guessed cleanup rather than claiming no artifact exists.
        let namespace = fs_safety::DirectoryNamespace::acquire(&parent).map_err(io_error)?;
        let leaf = OsString::from(format!(".cagents-editor-{}", uuid::Uuid::new_v4()));
        let creation = match create(&namespace, &parent, Path::new(&leaf)) {
            Ok(creation) => creation,
            Err(mut error) => {
                let message = error.to_string();
                if let Some(mut preparation) = error.take_recovery() {
                    // Preparation owns at most3 roots, plus this exact parent.
                    // Capture the whole typed object before converting cause
                    // text; a failed private bootstrap is still an owner.
                    let authority = slot.retain(message.clone(), move || {
                        preparation.retry_cleanup().map_err(|error| error.to_string())?;
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
                let authority = slot.retain(message.clone(), move || {
                    // Ownership was never established. Never chmod/delete or
                    // adopt a replacement from a diagnostic public spelling.
                    if directory.metadata().map_err(|error| error.to_string())?.nlink() != 0 {
                        return Err("Creation ownership remains unverified. Inspect and remove the retained entry manually before retrying; no entry was changed.".into());
                    }
                    fs_safety::sync_directory(&parent).map_err(|error| error.to_string())
                });
                return Err(FileIoError::Recovery { message, authority });
            },
        };
        let holding = Self { publication, parent, name: name.to_os_string(), original: None, pre_acquire_source: None, temporary: None, restore_original: false, slot: Some(slot) };
        let prepared = (|| {
            if let Some(error) = holding.publication.publication_error() { return Err(FileIoError::Io(error.into())); }
            fs_safety::set_directory_permissions(holding.directory(), 0o700).map_err(io_error)?;
            if holding.directory().metadata().map_err(io_error)?.mode() & 0o777 != 0o700 {
                return Err(FileIoError::Io("The editor staging directory is not private.".into()));
            }
            Ok(())
        })();
        match prepared { Ok(()) => Ok(holding), Err(error) => Err(holding.finish_error(error)) }
    }
    pub(super) fn directory(&self) -> &Root { self.publication.directory() }
    pub(super) fn create_temporary(&mut self) -> Result<(), FileIoError> {
        if self.temporary.is_some() { return Err(FileIoError::Io("An editor temporary file is already retained.".into())); }
        let (file, name) = fs_safety::create_temporary_in(self.directory(), 0o600).map_err(resolve_error)?;
        self.temporary = Some(OwnedTemporary { file, name });
        Ok(())
    }
    pub(super) fn temporary(&self) -> &File { &self.temporary.as_ref().expect("temporary created").file }
    pub(super) fn temporary_mut(&mut self) -> &mut File { &mut self.temporary.as_mut().expect("temporary created").file }
    pub(super) fn temporary_name(&self) -> &std::ffi::OsStr { &self.temporary.as_ref().expect("temporary created").name }
    fn verify_temporary(&self) -> Result<(), FileIoError> {
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
        let file = open(self.directory())?;
        // Pin first: metadata/read/coherence failure must never drop the
        // acquired descriptor or retry by adopting a newly opened witness.
        self.original = Some(AuthenticatedLeaf { file, witness: None });
        after_pin(&self.original.as_ref().unwrap().file)?;
        self.original.as_mut().unwrap().capture(self.publication.directory(), std::ffi::OsStr::new("file"))
    }
    pub(super) fn remove_original(&mut self) -> Result<(), FileIoError> {
        if let Some(original) = &self.original {
            original.verify(self.directory(), std::ffi::OsStr::new("file"))?;
            fs_safety::unlink_within(self.directory(), std::ffi::OsStr::new("file")).map_err(resolve_error)?;
            self.original = None;
        }
        Ok(())
    }
    fn retry_cleanup(&mut self) -> Result<(), String> {
        // Restore the producer's exact pending mode witness before applying
        // this consumer's explicit private0700 policy to its original inode.
        // This step never acquires/removes a public recovery entry.
        self.publication.retry_publication_steps().map_err(|error| error.to_string())?;
        fs_safety::set_directory_permissions(self.directory(), 0o700).map_err(|error| error.to_string())?;
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
    fn unverified_actual_mkdir_keeps_typed_receipt_without_chmod_or_guessed_cleanup() {
        let directory = tempfile::tempdir().unwrap(); let other = tempfile::tempdir().unwrap();
        let root = Root::open(directory.path()).unwrap(); let other = Root::open(other.path()).unwrap();
        let namespace = fs_safety::DirectoryNamespace::acquire(&other).unwrap();
        let context = RecoveryContext::with_guard((), 0).unwrap(); let mut created = None;
        let result = PresenceHolding::create_in_context_with(&root, std::ffi::OsStr::new("file"), &context, |_, parent, leaf| {
            created = Some(leaf.to_path_buf());
            fs_safety::create_directory_in_namespace(&namespace, parent, leaf, 0o700)
        });
        let error = match result { Err(error) => error, Ok(_) => panic!("different namespace cannot prove creation") };
        let authority = error.recovery_authority().expect("unverified native mkdir keeps a live carrier");
        let leaf = created.unwrap(); let metadata = fs_safety::metadata_beneath(&root, &leaf).unwrap();
        assert!(authority.retry().is_err());
        let after = fs_safety::metadata_beneath(&root, &leaf).unwrap();
        assert_eq!((metadata.dev(), metadata.ino(), metadata.mode()), (after.dev(), after.ino(), after.mode()));
        fs_safety::remove_directory_within(&root, leaf.as_os_str()).unwrap();
        // A replacement at the same spelling must remain untouched. Only the
        // retained original's unlink state permits authority resolution.
        fs::create_dir(directory.path().join(&leaf)).unwrap(); fs::write(directory.path().join(&leaf).join("foreign"), "keep").unwrap();
        authority.retry().unwrap(); assert!(authority.is_resolved());
        assert_eq!(fs::read(directory.path().join(leaf).join("foreign")).unwrap(), b"keep");
    }

    #[test]
    fn actual_prepublication_error_retains_preparation_and_cleanup_lease() {
        let directory = tempfile::tempdir().unwrap(); let runtime = tempfile::tempdir().unwrap();
        let named = directory.path().join("root"); let moved = directory.path().join("original");
        fs::create_dir(&named).unwrap(); let root = Root::open(&named).unwrap();
        let protection = crate::worktree_cleanup::fixture_protection(runtime.path(), &named, false).unwrap();
        let context = recovery_context_at(&root, Arc::new(Some(protection))).unwrap();
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
        holding.create_temporary().unwrap(); holding.temporary_mut().write_all(b"ours").unwrap();
        let descriptor = holding.temporary().as_raw_fd();
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
