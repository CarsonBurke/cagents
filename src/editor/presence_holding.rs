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

pub(super) struct PresenceHolding {
    publication: fs_safety::DirectoryPublication,
    parent: Root,
    name: OsString,
    original: Option<AuthenticatedLeaf>,
    pre_acquire_source: Option<File>,
    pub(super) restore_original: bool,
    slot: Option<RecoverySlot>,
}
impl PresenceHolding {
    pub(super) fn create(parent: &Root, name: &std::ffi::OsStr) -> Result<Self, FileIoError> {
        let slot = RecoverySlot::reserve().map_err(FileIoError::Io)?;
        let parent = parent.try_clone().map_err(io_error)?;
        // Published only covers this exact protected parent. Shared or unknown
        // parents refuse before creating or acquiring any presence artifacts.
        let namespace = fs_safety::DirectoryNamespace::acquire(&parent).map_err(io_error)?;
        let leaf = OsString::from(format!(".cagents-editor-{}", uuid::Uuid::new_v4()));
        let publication = match fs_safety::create_directory_in_namespace(&namespace, &parent, Path::new(&leaf), 0o700).map_err(resolve_error)? {
            fs_safety::DirectoryCreation::Published(receipt) => receipt,
            fs_safety::DirectoryCreation::Existing(_) => return Err(FileIoError::Io("The editor staging name already exists and was preserved.".into())),
            fs_safety::DirectoryCreation::Unverified { reason, .. } => return Err(FileIoError::Io(format!("Editor staging ownership could not be verified; the entry was preserved: {reason}"))),
        };
        let holding = Self { publication, parent, name: name.to_os_string(), original: None, pre_acquire_source: None, restore_original: false, slot: Some(slot) };
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
        // pre-acquisition source1. The
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
