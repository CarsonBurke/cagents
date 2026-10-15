//! Live recovery for one presence transaction. Every retry uses the retained
//! parent and exact file witness; diagnostic paths never authorize effects.
use super::*;

struct AuthenticatedLeaf {
    file: File,
    metadata: fs::Metadata,
    attributes: std::collections::BTreeMap<OsString, Vec<u8>>,
    hash: blake3::Hash,
}
impl AuthenticatedLeaf {
    fn capture(root: &Root, name: &std::ffi::OsStr) -> Result<Self, FileIoError> {
        let mut file = fs_safety::open_beneath(root, Path::new(name)).map_err(resolve_error)?;
        let metadata = file.metadata().map_err(io_error)?;
        let attributes = fs_safety::content_attributes(&file).map_err(io_error)?;
        let bytes = fs_safety::read_bounded(&mut file, MAX_EDITABLE_FILE_BYTES).map_err(io_error)?;
        if !metadata.is_file() || version(&metadata) != version(&file.metadata().map_err(io_error)?) {
            return Err(FileIoError::Io("The acquired file changed while retaining its recovery witness.".into()));
        }
        let leaf = Self { file, metadata, attributes, hash: blake3::hash(&bytes) };
        leaf.verify(root, name)?;
        Ok(leaf)
    }
    fn verify(&self, root: &Root, name: &std::ffi::OsStr) -> Result<(), FileIoError> {
        let mut named = fs_safety::open_beneath(root, Path::new(name)).map_err(resolve_error)?;
        let before = named.metadata().map_err(io_error)?;
        let bytes = fs_safety::read_bounded(&mut named, MAX_EDITABLE_FILE_BYTES).map_err(io_error)?;
        let after = named.metadata().map_err(io_error)?;
        if !same_inode(&self.metadata, &before) || version(&self.metadata) != version(&before)
            || version(&before) != version(&after)
            || version(&self.metadata) != version(&self.file.metadata().map_err(io_error)?)
            || blake3::hash(&bytes) != self.hash
            || fs_safety::content_attributes(&named).map_err(io_error)? != self.attributes
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
        let holding = Self { publication, parent, name: name.to_os_string(), original: None, restore_original: false, slot: Some(slot) };
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
    pub(super) fn capture_original(&mut self) -> Result<(), FileIoError> {
        self.original = Some(AuthenticatedLeaf::capture(self.directory(), std::ffi::OsStr::new("file"))?);
        Ok(())
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
        self.publication.resolve_recovery().map_err(|error| error.to_string())?;
        self.publication.cleanup_empty().map_err(|error| error.to_string())
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
        // DirectoryPublication <=5 root descriptors, parent1, original1. The
        // closure captures the WHOLE owner (not disjoint fields), below16 FDs.
        let authority = slot.retain(description.clone(), move || self.retry_cleanup());
        FileIoError::Recovery { message: description, authority }
    }
}
