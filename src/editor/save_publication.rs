//! Conditional pathname publication for ordinary Save and workspace edits.
//!
//! A private acquisition and RENAME_NOREPLACE protect the destination name.
//! They cannot exclude a noncooperating writer holding an already-open file
//! descriptor after the final content/metadata fence. The protected INTERNAL
//! namespace assumption applies to the complete transaction/retry lifetime of
//! the freshly owned0700 original directory and its private entries. Its public
//! directory name, ancestors and reconstructed lookalikes are not trusted.
//! Existing-FD content/version mutations remain in scope up to the final fence;
//! this does not claim a universal atomic content compare-and-swap protocol.
use super::*;
use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    os::unix::fs::MetadataExt,
};

pub(super) fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    a.is_file() && b.is_file() && a.dev() == b.dev() && a.ino() == b.ino()
}

fn stable_version(m: &fs::Metadata) -> (u64, u32, u32, u32, i64, i64) {
    (
        m.len(),
        m.mode(),
        m.uid(),
        m.gid(),
        m.mtime(),
        m.mtime_nsec(),
    )
}
fn full_version(m: &fs::Metadata) -> ((u64, u32, u32, u32, i64, i64), i64, i64) {
    (stable_version(m), m.ctime(), m.ctime_nsec())
}

pub(super) struct SaveBaseline {
    metadata: fs::Metadata,
    attributes: BTreeMap<OsString, Vec<u8>>,
    hash: blake3::Hash,
    hash_limit: u64,
}
impl SaveBaseline {
    pub(super) fn capture(file: &File, hash: Option<blake3::Hash>) -> Result<Self, FileIoError> {
        let metadata = file.metadata().map_err(io_error)?;
        let attributes = fs_safety::content_attributes(file).map_err(io_error)?;
        // Forced Save ignores the old loaded hash, not a concurrent writer.
        // Capture the current descriptor's coherent bytes instead. Streaming
        // uses fixed memory and stops at this observed length plus one byte.
        let hash_limit = if hash.is_some() {
            MAX_EDITABLE_FILE_BYTES
        } else {
            metadata.len()
        };
        let hash = match hash {
            Some(hash) => hash,
            None => hash_file(file, hash_limit)?,
        };
        let after = file.metadata().map_err(io_error)?;
        if !same_file(&metadata, &after) || full_version(&metadata) != full_version(&after) {
            return Err(changed());
        }
        Ok(Self {
            metadata,
            attributes,
            hash,
            hash_limit,
        })
    }
    pub(super) fn ensure_initial(&self, metadata: &fs::Metadata) -> Result<(), FileIoError> {
        if !same_file(&self.metadata, metadata)
            || full_version(&self.metadata) != full_version(metadata)
        {
            return Err(changed());
        }
        Ok(())
    }
    fn verify(&self, file: &File, renamed: bool) -> Result<(), FileIoError> {
        let before = file.metadata().map_err(io_error)?;
        if !same_file(&self.metadata, &before)
            || stable_version(&self.metadata) != stable_version(&before)
            || (!renamed && full_version(&self.metadata) != full_version(&before))
        {
            return Err(changed());
        }
        let attributes = fs_safety::content_attributes(file).map_err(io_error)?;
        if attributes != self.attributes {
            return Err(changed());
        }
        if hash_file(file, self.hash_limit)? != self.hash {
            return Err(changed());
        }
        if full_version(&before) != full_version(&file.metadata().map_err(io_error)?) {
            return Err(changed());
        }
        Ok(())
    }
}
fn hash_file(file: &File, limit: u64) -> Result<blake3::Hash, FileIoError> {
    let mut reader = file.try_clone().map_err(io_error)?;
    reader.rewind().map_err(io_error)?;
    let mut hasher = blake3::Hasher::new();
    let mut remaining = limit;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let capacity = (remaining.min(buffer.len() as u64 - 1) + 1) as usize;
        let count = reader.read(&mut buffer[..capacity]).map_err(io_error)?;
        if count == 0 {
            break;
        }
        if count as u64 > remaining {
            return Err(changed());
        }
        remaining -= count as u64;
        hasher.update(&buffer[..count]);
    }
    Ok(hasher.finalize())
}
fn changed() -> FileIoError {
    FileIoError::Io("The save target identity, contents or access metadata changed.".into())
}

fn directory_error(error: fs_safety::RecoveryDirectoryError) -> FileIoError {
    match error {
        fs_safety::RecoveryDirectoryError::Rejected(error) => io_error(error),
        fs_safety::RecoveryDirectoryError::Retained { cause, recovery } => FileIoError::Recovery {
            message: cause.to_string(), authority: recovery,
        },
    }
}

/// Owns the actual published private directory and its bounded recovery slot.
/// The receipt, rather than a diagnostic path, survives unresolved outcomes.
struct Holding {
    parent: Root,
    publication: fs_safety::DirectoryPublication,
    private_metadata: fs::Metadata,
    // Keep authenticated file identities alive with an unresolved holding.
    // This producer retains at most four file FDs plus the receipt's bounded
    // directory handles, below MAX_RECOVERY_DESCRIPTORS (16).
    files: Vec<File>,
    context: FilesystemRecoveryContext,
    // Native owners above close before their creation/private charge releases.
    slot: Option<RecoverySlot>,
    // Removal and parent durability are distinct irreversible phases.
    cleanup_sync_complete: bool,
}
impl Holding {
    fn create(parent: &Root, source: &File, context: &FilesystemRecoveryContext) -> Result<Self, FileIoError> {
        // These authenticated originals precede every factory effect. The
        // factory retains this WHOLE owner even if preparation/enrollment or
        // post-publication setup fails, using the SAME reserved context/slot.
        let owner = (parent.try_clone().map_err(io_error)?, source.try_clone().map_err(io_error)?);
        let admission = context.reserve_directory().map_err(FileIoError::Io)?;
        let name = OsString::from(format!(".cagents-save-{}", uuid::Uuid::new_v4()));
        let directory = fs_safety::prepare_recovery_directory_with_owner(
            admission, &name, || false, owner,
        ).map_err(directory_error)?;
        let (publication, (parent, source), private_metadata, context, slot) = directory.into_parts();
        // The factory established actual owner-only700 on this retained inode
        // and enrolled it before returning. Use its receipt directly, without
        // another fallible descriptor clone or a fresh mode baseline.
        Ok(Self {
            parent,
            publication,
            private_metadata,
            context,
            slot: Some(slot),
            files: vec![source],
            cleanup_sync_complete: false,
        })
    }
    fn root(&self) -> &Root { self.publication.directory() }
    fn validate_payload(&self) -> io::Result<()> {
        fs_safety::verify_recovery_directory_private(self.root(), &self.private_metadata)?;
        self.context.validate_payload_root(self.root())?;
        fs_safety::verify_recovery_directory_private(self.root(), &self.private_metadata)
    }
    fn keep_file(&mut self, file: File) -> Result<File, FileIoError> {
        match file.try_clone() {
            Ok(retained) => {
                self.files.push(retained);
                Ok(file)
            }
            Err(error) => {
                // Even descriptor exhaustion must not drop the actual newly
                // created/acquired file before returning its recovery carrier.
                self.files.push(file);
                Err(io_error(error))
            }
        }
    }
    fn remove_empty(&mut self, _parent: &Root) -> Result<(), FileIoError> {
        if !self.publication.is_removed() {
            self.validate_payload().map_err(io_error)?;
            self.publication.cleanup_empty().map_err(resolve_error)?;
        } else {
            self.context.placement().validate().map_err(io_error)?;
        }
        sync_cleanup_parent(self.context.placement().directory()).map_err(io_error)?;
        self.cleanup_sync_complete = true;
        Ok(())
    }
    fn retain(mut self) -> RecoveryAuthority {
        let description = self.location();
        let slot = self
            .slot
            .take()
            .expect("a holding reserves recovery capacity before creation");
        // A method receiver deliberately captures the WHOLE owned state.
        // Disjoint-field closure capture must not drop authenticated file FDs.
        slot.retain(description, move || self.retry_cleanup())
    }
    fn retry_cleanup(&mut self) -> Result<(), String> {
        if !self.publication.is_removed() {
            self.validate_payload().map_err(|error|error.to_string())?;
            self.publication.resolve_recovery().map_err(|error|error.to_string())?;
            // Unknown private contents remain retained for explicit resolution.
            self.publication.cleanup_empty().map_err(|error|error.to_string())?;
        } else {
            // A completed removal may have been followed by failed fsync.
            // Its unlinked inode has no ancestry to walk; retry only the held
            // outside/source parent sync, never any reconstructed path.
            self.context.placement().validate().map_err(|error|error.to_string())?;
        }
        fs_safety::sync_directory(self.context.placement().directory()).map_err(|e|e.to_string())?;
        fs_safety::sync_directory(&self.parent).map_err(|e|e.to_string())
    }
    fn location(&self) -> String {
        format!(
            "the held private directory originally named {}",
            self.root().path().display()
        )
    }
}

// Only the holding-removal durability boundary is injected in tests. Ordinary
// source-parent Saved(Unconfirmed) handling remains independent.
fn sync_cleanup_parent(parent: &Root) -> io::Result<()> {
    #[cfg(test)]
    if FAIL_CLEANUP_PARENT_SYNC.with(|flag| flag.replace(false)) {
        return Err(io::Error::other("injected outside holding parent sync failure"));
    }
    fs_safety::sync_directory(parent)
}
#[cfg(test)]
std::thread_local! {
    static FAIL_CLEANUP_PARENT_SYNC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The replacement is prepared privately, then the actual current destination
/// is acquired and verified. Publication and error restoration never replace
/// an entry another writer installed at the destination.
pub(super) fn publish_replacement(
    parent: &Root,
    name: &Path,
    destination: &File,
    bytes: &[u8],
    permissions: Permissions,
    baseline: &SaveBaseline,
    context: &FilesystemRecoveryContext,
    before_publish: impl FnOnce() -> io::Result<()>,
    sync_parent: impl FnOnce(&Root) -> io::Result<()>,
) -> Result<SaveDurability, FileIoError> {
    publish_with_hooks(
        parent,
        name,
        destination,
        bytes,
        permissions,
        baseline,
        context,
        before_publish,
        |_, _| Ok(()),
        |_, _| Ok(()),
        sync_parent,
    )
}

fn publish_with_hooks(
    parent: &Root,
    name: &Path,
    destination: &File,
    bytes: &[u8],
    permissions: Permissions,
    baseline: &SaveBaseline,
    context: &FilesystemRecoveryContext,
    before_publish: impl FnOnce() -> io::Result<()>,
    after_acquire: impl FnOnce(&Root, &Root) -> io::Result<()>,
    after_publish: impl FnOnce(&Root, &Root) -> io::Result<()>,
    sync_parent: impl FnOnce(&Root) -> io::Result<()>,
) -> Result<SaveDurability, FileIoError> {
    let mut holding = Holding::create(parent, destination, context)?;
    let result = publish_in_holding(
        &mut holding,
        parent,
        name,
        destination,
        bytes,
        permissions,
        baseline,
        before_publish,
        after_acquire,
        after_publish,
        sync_parent,
    );
    if holding.publication.is_removed() && holding.cleanup_sync_complete {
        return result;
    }
    // Publication cleanup may have acquired an unexpected entry. Preserve the
    // live receipt through the exact caller result, never only its old spelling.
    let authority = holding.retain();
    match result {
        Err(error) => {
            let authority = if let Some(previous) = error.recovery_authority() {
                authority.combine(previous.clone())
            } else {
                authority
            };
            Err(FileIoError::Recovery {
                message: error.to_string(),
                authority,
            })
        }
        Ok(SaveDurability::Durable) => Ok(SaveDurability::Recovery {
            error: "Save completed, but private recovery cleanup remains unresolved.".into(),
            authority,
        }),
        Ok(SaveDurability::Unconfirmed { error }) => {
            Ok(SaveDurability::Recovery { error, authority })
        }
        Ok(SaveDurability::Recovery {
            error,
            authority: previous,
        }) => Ok(SaveDurability::Recovery {
            error,
            authority: authority.combine(previous),
        }),
    }
}

fn publish_in_holding(
    holding: &mut Holding,
    parent: &Root,
    name: &Path,
    destination: &File,
    bytes: &[u8],
    permissions: Permissions,
    baseline: &SaveBaseline,
    before_publish: impl FnOnce() -> io::Result<()>,
    after_acquire: impl FnOnce(&Root, &Root) -> io::Result<()>,
    after_publish: impl FnOnce(&Root, &Root) -> io::Result<()>,
    sync_parent: impl FnOnce(&Root) -> io::Result<()>,
) -> Result<SaveDurability, FileIoError> {
    holding.validate_payload().map_err(io_error)?;
    let (temporary, temporary_name) = match fs_safety::create_temporary_in(holding.root(), 0o600) {
        Ok(value) => value,
        Err(error) => {
            let cleanup = holding.remove_empty(parent);
            return Err(append_cleanup(resolve_error(error), cleanup));
        }
    };
    // Pin the actual O_WRONLY created inode before ownership/ACL/write/sync or
    // readable-open can fail. Never depend on later successful preparation.
    let mut temporary = holding.keep_file(temporary)?;
    let mut temporary_baseline = None;
    let mut readable_temporary = None;
    let prepare = (|| {
        fs_safety::copy_ownership(destination, &temporary).map_err(io_error)?;
        fs_safety::copy_access_acl(destination, &temporary).map_err(io_error)?;
        holding.validate_payload().map_err(io_error)?;
        temporary.write_all(bytes).map_err(io_error)?;
        holding.validate_payload().map_err(io_error)?;
        temporary.set_permissions(permissions).map_err(io_error)?;
        fs_safety::copy_access_acl(destination, &temporary).map_err(io_error)?;
        let lost = fs_safety::copy_extended_attributes(destination, &temporary);
        temporary.sync_all().map_err(io_error)?;
        // create_temporary_in deliberately returns O_WRONLY. Authenticate a
        // separate no-follow read descriptor against that exact written inode
        // before hashing; cloning the write-only FD would fail with EBADF.
        holding.validate_payload().map_err(io_error)?;
        let readable = fs_safety::open_beneath(holding.root(), Path::new(&temporary_name))
            .map_err(resolve_error)?;
        let written = temporary.metadata().map_err(io_error)?;
        let opened = readable.metadata().map_err(io_error)?;
        if !same_file(&written, &opened) || full_version(&written) != full_version(&opened) {
            return Err(changed());
        }
        let readable = holding.keep_file(readable)?;
        let mut prepared = SaveBaseline::capture(&readable, Some(blake3::hash(bytes)))?;
        prepared.hash_limit = bytes.len() as u64;
        temporary_baseline = Some(prepared);
        readable_temporary = Some(readable);
        before_publish().map_err(io_error)?;
        // This coherency check is supplementary: the atomic acquisition below,
        // followed by verification of what it ACTUALLY moved, owns the name.
        baseline.verify(destination, false)?;
        Ok(lost)
    })();
    let lost = match prepare {
        Ok(lost) => lost,
        Err(error) => {
            return Err(clean_before_acquire(
                parent,
                holding,
                &temporary_name,
                readable_temporary.as_ref().unwrap_or(&temporary),
                temporary_baseline.as_ref(),
                error,
            ));
        }
    };
    let previous = OsStr::new("previous");
    if let Err(error) = holding.validate_payload() {
        return Err(clean_before_acquire(parent, holding, &temporary_name,
            readable_temporary.as_ref().unwrap_or(&temporary), temporary_baseline.as_ref(), io_error(error)));
    }
    if let Err(error) = fs_safety::rename_new_between(parent, name.as_os_str(), holding.root(), previous)
    {
        return Err(clean_before_acquire(
            parent,
            holding,
            &temporary_name,
            readable_temporary.as_ref().unwrap_or(&temporary),
            temporary_baseline.as_ref(),
            resolve_error(error),
        ));
    }
    let acquired_identity = fs_safety::metadata_beneath(holding.root(), Path::new(previous));
    let verify: Result<(File, fs::Metadata), FileIoError> = (|| {
        acquired_identity
            .as_ref()
            .map_err(|error| FileIoError::Io(error.to_string()))?;
        after_acquire(parent, holding.root()).map_err(io_error)?;
        let acquired =
            fs_safety::open_beneath(holding.root(), Path::new(previous)).map_err(resolve_error)?;
        let acquired = holding.keep_file(acquired)?;
        baseline.verify(&acquired, true)?;
        let acquired_version = acquired.metadata().map_err(io_error)?;
        // Sync the acquired directory entry before the replacement is exposed.
        fs_safety::sync_directory(holding.root()).map_err(io_error)?;
        verify_temporary(
            holding,
            &temporary_name,
            readable_temporary.as_ref().unwrap_or(&temporary),
            temporary_baseline.as_ref(),
        )?;
        holding.validate_payload().map_err(io_error)?;
        fs_safety::rename_new_between(holding.root(), &temporary_name, parent, name.as_os_str())
            .map_err(resolve_error)?;
        Ok((acquired, acquired_version))
    })();
    let (acquired, acquired_version) = match verify {
        Ok(proof) => proof,
        Err(error) => {
            let put_back = (|| {
                let first = acquired_identity
                    .as_ref()
                    .map_err(|error| FileIoError::Io(error.to_string()))?;
                let current = fs_safety::metadata_beneath(holding.root(), Path::new(previous))
                    .map_err(resolve_error)?;
                if first.dev() != current.dev()
                    || first.ino() != current.ino()
                    || first.file_type() != current.file_type()
                {
                    return Err(FileIoError::Io(
                        "The acquired entry was replaced; unknown recovery entry retained.".into(),
                    ));
                }
                holding.validate_payload().map_err(io_error)?;
                fs_safety::rename_new_between(holding.root(), previous, parent, name.as_os_str())
                    .map_err(resolve_error)
            })();
            let cleanup_temp = remove_temporary(
                holding,
                &temporary_name,
                readable_temporary.as_ref().unwrap_or(&temporary),
                temporary_baseline.as_ref(),
            );
            let mut message = format!("{error}; ");
            match put_back {
                Ok(()) => {
                    message.push_str("the acquired entry was restored without replacing newer work");
                    if let Err(error) = fs_safety::sync_directory(parent) { message.push_str(&format!("; restoration durability unconfirmed: {error}")); }
                    if let Err(error) = holding.remove_empty(parent) { message.push_str(&format!("; {error}")); }
                }
                Err(error) => message.push_str(&format!("the recovery entry remains as previous in {}; destination left intact: {error}", holding.location())),
            }
            if let Err(error) = cleanup_temp {
                message.push_str(&format!("; partial replacement cleanup failed: {error}"));
            }
            return Err(FileIoError::Io(message));
        }
    };
    // From here publication really succeeded. Every later failure is a saved
    // baseline with an explicit warning, including retained acquired contents.
    let mut warnings = Vec::new();
    if let Err(error) = after_publish(parent, holding.root()) {
        warnings.push(error.to_string());
    }
    let cleanup = (|| {
        let named = fs_safety::metadata_beneath(holding.root(), Path::new(previous))
            .map_err(resolve_error)?;
        if !same_file(&acquired_version, &named)
            || full_version(&acquired_version) != full_version(&named)
            || full_version(&acquired_version)
                != full_version(&acquired.metadata().map_err(io_error)?)
        {
            return Err(FileIoError::Io(
                "The acquired original changed after publication; it was retained.".into(),
            ));
        }
        baseline.verify(&acquired, true)?;
        holding.validate_payload().map_err(io_error)?;
        fs_safety::unlink_within(holding.root(), previous).map_err(resolve_error)?;
        holding.remove_empty(parent)
    })();
    if let Err(error) = cleanup {
        warnings.push(format!(
            "{error}; inspect {} before removing recovery artifacts",
            holding.location()
        ));
    }
    for attribute in lost {
        tracing::warn!(path = %parent.path().join(name).display(), %attribute, "saved file did not receive one of its extended attributes");
    }
    if let Err(error) = sync_parent(parent) {
        warnings.push(format!("replacement durability unconfirmed: {error}"));
    }
    if warnings.is_empty() {
        Ok(SaveDurability::Durable)
    } else {
        Ok(SaveDurability::Unconfirmed {
            error: warnings.join("; "),
        })
    }
}

fn append_cleanup(error: FileIoError, cleanup: Result<(), FileIoError>) -> FileIoError {
    match cleanup {
        Ok(()) => error,
        Err(cleanup) => FileIoError::Io(format!("{error}; {cleanup}")),
    }
}
fn verify_temporary(
    holding: &Holding,
    name: &OsStr,
    file: &File,
    baseline: Option<&SaveBaseline>,
) -> Result<(), FileIoError> {
    let named =
        fs_safety::metadata_beneath(holding.root(), Path::new(name)).map_err(resolve_error)?;
    if !same_file(&named, &file.metadata().map_err(io_error)?) {
        return Err(FileIoError::Io(format!(
            "The partial replacement name changed; unknown entry preserved in {}.",
            holding.location()
        )));
    }
    if let Some(baseline) = baseline {
        baseline.verify(file, false)?;
    }
    Ok(())
}
fn remove_temporary(
    holding: &Holding,
    name: &OsStr,
    file: &File,
    baseline: Option<&SaveBaseline>,
) -> Result<(), FileIoError> {
    holding.validate_payload().map_err(io_error)?;
    verify_temporary(holding, name, file, baseline)?;
    fs_safety::unlink_within(holding.root(), name).map_err(resolve_error)
}
fn clean_before_acquire(
    parent: &Root,
    holding: &mut Holding,
    name: &OsStr,
    file: &File,
    baseline: Option<&SaveBaseline>,
    error: FileIoError,
) -> FileIoError {
    let cleanup =
        remove_temporary(holding, name, file, baseline).and_then(|()| holding.remove_empty(parent));
    append_cleanup(error, cleanup)
}

#[cfg(test)]
std::thread_local! {
    static FIXTURE_PLACEMENTS: std::cell::RefCell<BTreeMap<PathBuf, PathBuf>> = std::cell::RefCell::new(BTreeMap::new());
}
#[cfg(test)]
pub(super) fn fixture_context(root: &Root) -> FilesystemRecoveryContext {
    fixture_context_with_guard(root, ())
}
#[cfg(test)]
fn fixture_context_with_guard(root: &Root, guard: impl Send + Sync + 'static) -> FilesystemRecoveryContext {
    let context = crate::filesystem_recovery_context::fixtures::fixture_context_with_guard(root, guard).unwrap();
    FIXTURE_PLACEMENTS.with(|placements| placements.borrow_mut().insert(root.path().to_owned(), context.placement().directory().path().to_owned()));
    context
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn hook_save(
        root: &Root,
        hook: impl FnOnce() -> io::Result<()>,
        overwrite: bool,
    ) -> Result<SaveOutcome, FileIoError> {
        save_file_at_with_hooks_in_context(
            root,
            Path::new("file"),
            "mine",
            blake3::hash(b"base"),
            overwrite,
            hook,
            fs_safety::sync_directory,
            &fixture_context(root),
        )
    }
    fn prepared_save(
        root: &Root,
        after_acquire: impl FnOnce(&Root, &Root) -> io::Result<()>,
        after_publish: impl FnOnce(&Root, &Root) -> io::Result<()>,
    ) -> Result<SaveDurability, FileIoError> {
        let source = fs_safety::open_beneath(root, Path::new("file")).unwrap();
        let baseline = SaveBaseline::capture(&source, Some(blake3::hash(b"base"))).unwrap();
        publish_with_hooks(
            root,
            Path::new("file"),
            &source,
            b"mine",
            source.metadata().unwrap().permissions(),
            &baseline,
            &fixture_context(&root),
            || Ok(()),
            after_acquire,
            after_publish,
            fs_safety::sync_directory,
        )
    }
    fn fixture() -> (tempfile::TempDir, Root) {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("file"), b"base").unwrap();
        let root = Root::open(dir.path()).unwrap();
        (dir, root)
    }
    fn holding(dir: &Path) -> PathBuf {
        let outside = FIXTURE_PLACEMENTS.with(|placements| placements.borrow().get(dir).cloned()).expect("fixture captured an outside placement");
        fs::read_dir(outside)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap()
    }

    #[test]
    fn save_refuses_changed_holding_permissions_without_publish_or_permission_repair() {
        let (directory, root) = fixture();
        let result = hook_save(&root, || {
            fs::set_permissions(holding(directory.path()), fs::Permissions::from_mode(0o711))
        }, false);
        let error = result.unwrap_err();
        let authority = error.recovery_authority().expect("changed H retains exact original owners").clone();
        let private = holding(directory.path());
        assert_eq!(fs::read(directory.path().join("file")).unwrap(), b"base");
        assert_eq!(fs::metadata(&private).unwrap().mode() & 0o7777, 0o711);
        assert!(authority.retry().is_err(), "cleanup may not trust the changed private boundary");
        assert_eq!(fs::metadata(&private).unwrap().mode() & 0o7777, 0o711);
        // Explicit fixture repair of the SAME original inode restores the
        // cached policy; no replacement or new metadata baseline is adopted.
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        for entry in fs::read_dir(&private).unwrap() {
            let entry = entry.unwrap(); assert!(entry.file_type().unwrap().is_file());
            fs::remove_file(entry.path()).unwrap();
        }
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(!private.exists());
        assert_eq!(fs::read(directory.path().join("file")).unwrap(), b"base");
    }

    #[test]
    fn missing_payload_enrollment_refuses_before_holding_creation() {
        use std::sync::Arc;
        let source_directory = tempfile::tempdir().unwrap();
        let root = Root::open(source_directory.path()).unwrap();
        fs::write(root.path().join("file"), b"original").unwrap();
        let source = fs_safety::open_beneath(&root, Path::new("file")).unwrap();
        let fixture = fixture_context(&root);
        let runtime = tempfile::tempdir().unwrap();
        let protection = Arc::new(Some(crate::worktree_cleanup::fixture_protection(runtime.path(), root.path(), false).unwrap()));
        let source_root = Arc::new(root.try_clone().unwrap());
        let placement = fs_safety::RecoveryPlacement::acquire(
            Arc::new(fixture.placement().directory().try_clone().unwrap()), source_root.clone(), Arc::from([source_root]),
            fs_safety::RecoveryRegistryCheck::new(Arc::new(|| Ok(())), 0),
        ).unwrap();
        // Callback-only placement explicitly has no owned enrollment authority.
        let context = FilesystemRecoveryContext::new(&root, protection, placement).unwrap();
        let error = match Holding::create(&root, &source, &context) { Ok(_) => panic!("missing enrollment must refuse"), Err(error) => error };
        assert!(error.to_string().contains("owned registration/adoption lease"));
        assert!(error.recovery_authority().is_none(), "a definite pre-effect refusal needs no artifact custody");
        assert!(fs::read_dir(context.placement().directory().path()).unwrap().next().is_none());
        assert_eq!(fs::read(root.path().join("file")).unwrap(), b"original");
    }

    #[test]
    fn save_acquisition_before_publish_edit_preserves_external_bytes_even_on_forced_save() {
        for overwrite in [false, true] {
            let (dir, root) = fixture();
            let path = dir.path().join("file");
            let times = fs::metadata(&path).unwrap().modified().unwrap();
            let result = hook_save(
                &root,
                || {
                    fs::write(&path, b"late")?;
                    // Same-size edits with a restored mtime still change ctime.
                    File::options()
                        .write(true)
                        .open(&path)?
                        .set_times(fs::FileTimes::new().set_modified(times))
                },
                overwrite,
            );
            assert!(result.is_err(), "{result:?}");
            assert_eq!(fs::read(&path).unwrap(), b"late");
            assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
        }
    }

    #[test]
    fn save_acquisition_forced_save_fences_current_bytes_after_acquire_even_with_restored_mtime() {
        let (dir, root) = fixture();
        let source = fs_safety::open_beneath(&root, Path::new("file")).unwrap();
        let baseline = SaveBaseline::capture(&source, None).unwrap();
        let modified = source.metadata().unwrap().modified().unwrap();
        let error = publish_with_hooks(
            &root,
            Path::new("file"),
            &source,
            b"mine",
            source.metadata().unwrap().permissions(),
            &baseline,
            &fixture_context(&root),
            || Ok(()),
            |_, private| {
                let path = private.path().join("previous");
                fs::write(&path, b"late")?;
                File::options()
                    .write(true)
                    .open(path)?
                    .set_times(fs::FileTimes::new().set_modified(modified))
            },
            |_, _| panic!("changed acquired contents must never publish"),
            fs_safety::sync_directory,
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("restored without replacing"),
            "{error}"
        );
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"late");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn save_acquisition_before_publish_atomic_replacement_symlink_and_deletion_are_preserved() {
        for operation in 0..3 {
            let (dir, root) = fixture();
            let path = dir.path().join("file");
            let outside = tempfile::tempdir().unwrap();
            fs::write(outside.path().join("secret"), b"outside").unwrap();
            let result = hook_save(
                &root,
                || {
                    fs::remove_file(&path)?;
                    match operation {
                        0 => fs::write(&path, b"replacement"),
                        1 => symlink(outside.path().join("secret"), &path),
                        _ => Ok(()),
                    }
                },
                false,
            );
            assert!(result.is_err(), "{result:?}");
            match operation {
                0 => assert_eq!(fs::read(&path).unwrap(), b"replacement"),
                1 => assert!(
                    fs::symlink_metadata(&path)
                        .unwrap()
                        .file_type()
                        .is_symlink()
                ),
                _ => assert!(!path.exists()),
            }
            assert_eq!(fs::read(outside.path().join("secret")).unwrap(), b"outside");
            assert!(
                fs::read_dir(dir.path())
                    .unwrap()
                    .all(|entry| entry.unwrap().file_name() == "file")
            );
        }
    }

    #[test]
    fn save_acquisition_new_destination_refuses_publication_and_retains_original() {
        let (dir, root) = fixture();
        let error = prepared_save(
            &root,
            |parent, private| {
                assert_eq!(private.metadata()?.mode() & 0o777, 0o700);
                assert!(!parent.path().join("file").exists());
                fs::write(parent.path().join("file"), b"competitor")
            },
            |_, _| panic!("refused publication must not reach success hook"),
        )
        .unwrap_err();
        assert!(error.to_string().contains("remains as previous"), "{error}");
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"competitor");
        let private = holding(dir.path());
        assert_eq!(fs::read(private.join("previous")).unwrap(), b"base");
        assert_eq!(fs::read_dir(&private).unwrap().count(), 1);
    }

    #[test]
    fn save_acquisition_changed_acquired_entry_is_not_published_and_putback_keeps_latest_bytes() {
        let (dir, root) = fixture();
        let error = prepared_save(
            &root,
            |_, private| fs::write(private.path().join("previous"), b"late"),
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("restored without replacing"),
            "{error}"
        );
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"late");
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }

    #[test]
    fn save_acquisition_unknown_holding_entry_is_retained_without_restoration() {
        let (dir, root) = fixture();
        let error = prepared_save(
            &root,
            |_, private| {
                fs::rename(
                    private.path().join("previous"),
                    private.path().join("moved-original"),
                )?;
                fs::write(private.path().join("previous"), b"unknown")
            },
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("unknown recovery entry retained"),
            "{error}"
        );
        assert!(!dir.path().join("file").exists());
        let private = holding(dir.path());
        assert_eq!(fs::read(private.join("previous")).unwrap(), b"unknown");
        assert_eq!(fs::read(private.join("moved-original")).unwrap(), b"base");
    }

    #[test]
    fn save_acquisition_unknown_temporary_is_retained_and_original_restored() {
        let (dir, root) = fixture();
        let error = prepared_save(
            &root,
            |_, private| {
                let temporary = fs::read_dir(private.path())?
                    .map(|entry| entry.unwrap().path())
                    .find(|path| path.file_name().unwrap() != "previous")
                    .unwrap();
                fs::remove_file(&temporary)?;
                fs::write(temporary, b"unknown")
            },
            |_, _| Ok(()),
        )
        .unwrap_err();
        assert!(
            error.to_string().contains("unknown entry preserved"),
            "{error}"
        );
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"base");
        let private = holding(dir.path());
        assert!(
            fs::read_dir(private)
                .unwrap()
                .any(|entry| fs::read(entry.unwrap().path()).unwrap() == b"unknown")
        );
    }

    #[test]
    fn save_acquisition_committed_save_retains_changed_original_and_reports_unconfirmed() {
        let (dir, root) = fixture();
        let result = prepared_save(
            &root,
            |_, _| Ok(()),
            |_, private| fs::write(private.path().join("previous"), b"late"),
        )
        .unwrap();
        assert!(
            matches!(result, SaveDurability::Recovery { ref error, .. } if error.contains("retained")),
            "{result:?}"
        );
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"mine");
        assert_eq!(
            fs::read(holding(dir.path()).join("previous")).unwrap(),
            b"late"
        );
    }

    #[test]
    fn save_acquisition_committed_save_reports_holding_cleanup_failure_as_saved() {
        let (dir, root) = fixture();
        let result = prepared_save(
            &root,
            |_, _| Ok(()),
            |_, private| fs::write(private.path().join("retained"), b"artifact"),
        )
        .unwrap();
        assert!(
            matches!(result, SaveDurability::Recovery { .. }),
            "{result:?}"
        );
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"mine");
        let private = holding(dir.path());
        assert_eq!(fs::read(private.join("retained")).unwrap(), b"artifact");
        assert!(!private.join("previous").exists());
    }

    #[test]
    fn save_acquisition_pinned_root_cannot_publish_into_replacement_worktree() {
        let dir = tempfile::tempdir().unwrap();
        let named = dir.path().join("root");
        let moved = dir.path().join("moved");
        fs::create_dir(&named).unwrap();
        fs::write(named.join("file"), b"base").unwrap();
        let root = Root::open(&named).unwrap();
        let result = hook_save(
            &root,
            || {
                fs::rename(&named, &moved)?;
                fs::create_dir(&named)?;
                fs::write(named.join("file"), b"replacement")
            },
            false,
        )
        .unwrap();
        assert!(matches!(result, SaveOutcome::Saved { .. }));
        assert_eq!(fs::read(moved.join("file")).unwrap(), b"mine");
        assert_eq!(fs::read(named.join("file")).unwrap(), b"replacement");
    }

    #[test]
    fn save_acquisition_unresolved_holding_retains_original_context_until_resolution() {
        use std::sync::{Arc, atomic::{AtomicBool, Ordering}};
        struct Guard(Arc<AtomicBool>);
        impl Drop for Guard {
            fn drop(&mut self) { self.0.store(true, Ordering::Release); }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let (_directory, root) = fixture();
        let context = fixture_context_with_guard(&root, Guard(dropped.clone()));
        let source = fs_safety::open_beneath(&root, Path::new("file")).unwrap();
        let holding = Holding::create(&root, &source, &context).unwrap();
        let private_path = holding.root().path().to_owned();
        fs::write(private_path.join("unresolved"), b"retained").unwrap();
        let authority = holding.retain();
        drop(context);
        assert!(!dropped.load(Ordering::Acquire));
        assert!(authority.retry().is_err());
        assert!(!dropped.load(Ordering::Acquire));
        fs::remove_file(private_path.join("unresolved")).unwrap();
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn save_acquisition_live_recovery_retains_actual_cleanup_lease_after_root_rename() {
        use std::sync::Arc;
        let directory = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let named = directory.path().join("root");
        let moved = directory.path().join("moved");
        fs::create_dir(&named).unwrap();
        fs::write(named.join("file"), b"base").unwrap();
        let root = Root::open(&named).unwrap();
        let protection = crate::worktree_cleanup::fixture_protection(runtime.path(), &named, false).unwrap();
        assert!(protection.retained_descriptor_count() >= 3);
        let fixture = fixture_context(&root);
        let context = FilesystemRecoveryContext::new(&root, Arc::new(Some(protection)), fixture.placement().clone()).unwrap();
        drop(fixture);
        let source = fs_safety::open_beneath(&root, Path::new("file")).unwrap();
        let holding = Holding::create(&root, &source, &context).unwrap();
        let private = holding.root().try_clone().unwrap();
        fs::write(private.path().join("unresolved"), b"preserve").unwrap();
        let authority = holding.retain();
        drop(context);
        fs::rename(&named, &moved).unwrap();
        fs::create_dir(&named).unwrap();
        fs::write(named.join("foreign"), b"new actor").unwrap();
        assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), &moved, true).is_err(), "the original inode lease must survive the old caller/context drop");
        assert!(authority.retry().is_err());
        assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), &moved, true).is_err());
        fs_safety::unlink_within(&private, OsStr::new("unresolved")).unwrap();
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        let exclusive = crate::worktree_cleanup::fixture_protection(runtime.path(), &moved, true).unwrap();
        drop(exclusive);
        assert_eq!(fs::read(moved.join("file")).unwrap(), b"base");
        assert_eq!(fs::read(named.join("foreign")).unwrap(), b"new actor");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn save_acquisition_real_preparation_failure_keeps_exact_lease_and_source_until_cleanup() {
        use std::sync::Arc;
        let (directory, root) = fixture();
        let runtime = tempfile::tempdir().unwrap();
        let protection = crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), false).unwrap();
        let fixture = fixture_context(&root);
        let context = FilesystemRecoveryContext::new(&root, Arc::new(Some(protection)), fixture.placement().clone()).unwrap();
        drop(fixture);
        let source = fs_safety::open_beneath(&root, Path::new("file")).unwrap();
        let expected = source.metadata().unwrap();
        let expected = (expected.dev(), expected.ino());
        let has_original_file = || fs::read_dir("/proc/self/fd").unwrap().any(|entry| {
            fs::metadata(entry.unwrap().path()).is_ok_and(|metadata| metadata.is_file()
                && (metadata.dev(), metadata.ino()) == expected)
        });
        let outside = context.placement().directory().path().to_owned();
        let owner = (root.try_clone().unwrap(), source.try_clone().unwrap());
        let error = directory_error(fs_safety::fixture_recovery_directory_preparation_failure(
            context.reserve_directory().unwrap(), owner,
        ));
        let authority = error.recovery_authority().expect("prepublication error must retain live authority").clone();
        drop(error);
        drop(context);
        drop(source);
        assert!(has_original_file(), "factory error owns the authenticated source, not only its old name");
        assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), true).is_err());
        assert!(authority.retry().is_err(), "unknown private contents must remain intact");
        assert!(has_original_file(), "failed retry keeps the same lower owner");
        let staging = fs::read_dir(&outside).unwrap().next().unwrap().unwrap().path();
        assert_eq!(fs::read(staging.join("directory")).unwrap(), b"retained fixture artifact");
        fs::remove_file(staging.join("directory")).unwrap();
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(!has_original_file(), "actual resolution releases the lower file owner");
        assert!(!staging.exists());
        let exclusive = crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), true).unwrap();
        drop(exclusive);
        assert_eq!(fs::read(directory.path().join("file")).unwrap(), b"base");
    }

    #[test]
    fn save_acquisition_recovery_keeps_authenticated_file_descriptors_until_actual_resolution() {
        use std::os::fd::AsRawFd;
        let (dir, root) = fixture();
        let source = fs_safety::open_beneath(&root, Path::new("file")).unwrap();
        let holding = Holding::create(&root, &source, &fixture_context(&root)).unwrap();
        let descriptor = holding.files[0].as_raw_fd();
        let expected = source.metadata().unwrap();
        let expected = (expected.dev(), expected.ino());
        let descriptor_identity = |fd| {
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            if unsafe { libc::fstat(fd, metadata.as_mut_ptr()) } != 0 {
                assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EBADF));
                return None;
            }
            let metadata = unsafe { metadata.assume_init() };
            Some((metadata.st_dev as u64, metadata.st_ino as u64))
        };
        fs::write(holding.root().path().join("unresolved"), b"preserve").unwrap();
        drop(source);
        let private_path = holding.root().path().to_owned();
        let authority = holding.retain();
        assert_eq!(
            descriptor_identity(descriptor),
            Some(expected),
            "the returned authority must own the authenticated file descriptor"
        );
        assert!(authority.retry().is_err());
        assert_eq!(
            descriptor_identity(descriptor),
            Some(expected),
            "failed retry must retain all owned authority"
        );
        fs::remove_file(private_path.join("unresolved")).unwrap();
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        // Another parallel test may reuse the numeric FD. The fixture's
        // still-linked unique inode cannot be reused, so compare identity.
        assert_ne!(
            descriptor_identity(descriptor),
            Some(expected),
            "actual resolution releases the captured file descriptor"
        );
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"base");
    }

    #[test]
    fn save_acquisition_recovery_retains_exact_holding_after_parent_replacement_until_resolution() {
        let (dir, root) = fixture();
        let result = prepared_save(
            &root,
            |parent, _holding| fs::write(parent.path().join("file"), b"newer"),
            |_, _| Ok(()),
        )
        .unwrap_err();
        let authority = result
            .recovery_authority()
            .expect("unresolved acquired original must retain live authority")
            .clone();
        let held_path = holding(dir.path());
        assert_eq!(fs::read(held_path.join("previous")).unwrap(), b"base");
        assert!(
            authority.retry().is_err(),
            "retry cannot delete nonempty recovery contents or replace newer work"
        );
        assert!(!authority.is_resolved());
        let parked = dir
            .path()
            .with_extension(format!("parked-{}", uuid::Uuid::new_v4()));
        fs::rename(dir.path(), &parked).unwrap();
        fs::create_dir(dir.path()).unwrap();
        fs::write(dir.path().join("file"), b"replacement checkout").unwrap();
        let holding_name = held_path.file_name().unwrap();
        fs::create_dir(dir.path().join(holding_name)).unwrap();
        fs::write(dir.path().join(holding_name).join("foreign"), b"preserve").unwrap();
        // Explicit user resolution moves the retained original out. Retry then
        // cleans the exact original holding under the retained parent, never B.
        fs::rename(
            held_path.join("previous"),
            parked.join("recovered-original"),
        )
        .unwrap();
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert_eq!(
            fs::read(dir.path().join("file")).unwrap(),
            b"replacement checkout"
        );
        assert_eq!(
            fs::read(dir.path().join(holding_name).join("foreign")).unwrap(),
            b"preserve"
        );
        assert_eq!(fs::read(parked.join("file")).unwrap(), b"newer");
        assert_eq!(
            fs::read(parked.join("recovered-original")).unwrap(),
            b"base"
        );
        fs::remove_dir_all(parked).unwrap();
    }

    #[test]
    fn save_acquisition_committed_recovery_is_typed_and_never_reverts_published_bytes() {
        let (dir, root) = fixture();
        let result = prepared_save(
            &root,
            |_, _| Ok(()),
            |_, private| fs::write(private.path().join("previous"), b"late"),
        )
        .unwrap();
        let authority = result
            .recovery_authority()
            .expect("committed cleanup failure needs retained authority");
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"mine");
        assert!(authority.retry().is_err());
        assert_eq!(fs::read(dir.path().join("file")).unwrap(), b"mine");
        assert_eq!(
            fs::read(holding(dir.path()).join("previous")).unwrap(),
            b"late"
        );
        assert!(!authority.is_resolved());
    }

    #[test]
    fn save_acquisition_refuses_actual_holding_moved_into_source_before_acquisition() {
        let (directory, root) = fixture();
        let moved = directory.path().join("moved-private");
        let mut original = None;
        let error = hook_save(&root, || {
            let outside = holding(directory.path());
            original = Some(outside.clone());
            fs::rename(&outside, &moved)?;
            fs::create_dir(&outside)?;
            fs::write(outside.join("foreign"), b"preserve")
        }, false).unwrap_err();
        assert!(error.recovery_authority().is_some());
        assert_eq!(fs::read(directory.path().join("file")).unwrap(), b"base");
        assert!(!moved.join("previous").exists(), "refusal must precede public source acquisition");
        assert!(error.recovery_authority().unwrap().retry().is_err());
        let original = original.unwrap();
        let foreign = original.with_extension("foreign");
        fs::rename(&original, &foreign).unwrap();
        fs::rename(&moved, &original).unwrap();
        // Explicit fixture/user resolution of our retained private replacement.
        for entry in fs::read_dir(&original).unwrap() { fs::remove_file(entry.unwrap().path()).unwrap(); }
        error.recovery_authority().unwrap().retry().unwrap();
        assert_eq!(fs::read(foreign.join("foreign")).unwrap(), b"preserve");
        assert_eq!(fs::read(directory.path().join("file")).unwrap(), b"base");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn save_acquisition_outer_result_keeps_removed_holding_parent_sync_custody() {
        struct Reset;
        impl Drop for Reset {
            fn drop(&mut self) { FAIL_CLEANUP_PARENT_SYNC.with(|flag| flag.set(false)); }
        }
        for committed in [false, true] {
            let (directory, root) = fixture();
            let runtime = tempfile::tempdir().unwrap();
            // Keep the unique old inode linked after committed replacement so
            // parallel tests cannot reuse its inode number after FD release.
            fs::hard_link(directory.path().join("file"), directory.path().join("inode-witness")).unwrap();
            let original = fs::metadata(directory.path().join("file")).unwrap();
            let original = (original.dev(), original.ino());
            let original_file_is_pinned = || fs::read_dir("/proc/self/fd").unwrap().any(|entry| {
                fs::metadata(entry.unwrap().path()).is_ok_and(|metadata| metadata.is_file()
                    && (metadata.dev(), metadata.ino()) == original)
            });
            let protection = crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), false).unwrap();
            let fixture = fixture_context(&root);
            let context = FilesystemRecoveryContext::new(&root, std::sync::Arc::new(Some(protection)), fixture.placement().clone()).unwrap();
            drop(fixture);
            let outside = context.placement().directory().path().to_owned();
            FAIL_CLEANUP_PARENT_SYNC.with(|flag| flag.set(true));
            let _reset = Reset;
            let result = save_file_at_with_hooks_in_context(
                &root, Path::new("file"), "mine", blake3::hash(b"base"), false,
                || if committed { Ok(()) } else { Err(io::Error::other("before publication refusal")) },
                fs_safety::sync_directory, &context,
            );
            assert!(!FAIL_CLEANUP_PARENT_SYNC.with(|flag| flag.get()), "the real holding-removal sync boundary must be reached");
            let authority = match result {
                Ok(SaveOutcome::Saved { durability: SaveDurability::Recovery { authority, .. }, .. }) if committed => authority,
                Err(FileIoError::Recovery { authority, .. }) if !committed => authority,
                other => panic!("the actual outer save result must retain pending cleanup: {other:?}"),
            };
            assert!(fs::read_dir(&outside).unwrap().next().is_none(), "the actual holding was already removed");
            assert_eq!(fs::read(directory.path().join("file")).unwrap(), if committed { b"mine" } else { b"base" });
            drop(context);
            assert!(original_file_is_pinned(), "unresolved removal durability keeps original authenticated FD");
            assert!(crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), true).is_err());
            authority.retry().unwrap();
            assert!(authority.is_resolved());
            assert!(!original_file_is_pinned());
            drop(crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), true).unwrap());
            assert_eq!(fs::read(directory.path().join("file")).unwrap(), if committed { b"mine" } else { b"base" });
        }
    }

    #[test]
    fn save_acquisition_removed_holding_retry_only_syncs_retained_parents() {
        let (_directory, root) = fixture();
        let context = fixture_context(&root);
        let source = fs_safety::open_beneath(&root, Path::new("file")).unwrap();
        let mut holding = Holding::create(&root, &source, &context).unwrap();
        holding.publication.cleanup_empty().unwrap();
        assert!(holding.publication.is_removed());
        assert_eq!(holding.root().metadata().unwrap().nlink(), 0);
        // Model a failed parent fsync after known removal: an unlinked inode
        // must not strand the retained sync retry by demanding live ancestry.
        holding.retry_cleanup().unwrap();
    }

    #[test]
    fn save_acquisition_private_payload_cannot_be_included_by_git_add_all() {
        use crate::process::{CommandSpec, ProcessExecutor, SystemProcessExecutor, CancellationToken};
        let (directory, root) = fixture();
        let git = |arguments: &[&str]| {
            let mut command = CommandSpec::new("git");
            for (name, _) in std::env::vars_os() {
                if name.as_encoded_bytes().starts_with(b"GIT_") { command = command.env_remove(name); }
            }
            command = command.arg("-C").arg(directory.path())
                .args(["-c", "core.fsmonitor=false", "-c", "core.hooksPath=/dev/null", "-c", "init.templateDir="])
                .args(arguments.iter().copied())
                .env("GIT_CONFIG_GLOBAL", "/dev/null").env("GIT_CONFIG_SYSTEM", "/dev/null")
                .env("GIT_CONFIG_NOSYSTEM", "1").env("GIT_TERMINAL_PROMPT", "0");
            let output = SystemProcessExecutor.execute(&command, &CancellationToken::new()).unwrap();
            assert!(output.success, "{}", String::from_utf8_lossy(&output.stderr));
            output.stdout
        };
        git(&["init", "--quiet"]);
        git(&["add", "--", "file"]);
        let error = prepared_save(&root, |parent, _| fs::write(parent.path().join("file"), b"competitor"), |_, _| Ok(())).unwrap_err();
        assert!(error.recovery_authority().is_some());
        let private = holding(directory.path());
        assert!(!private.starts_with(directory.path()));
        assert_eq!(fs::read(private.join("previous")).unwrap(), b"base");
        git(&["add", "-A"]);
        assert_eq!(git(&["ls-files", "-z"]), b"file\0");
        assert_eq!(fs::read(directory.path().join("file")).unwrap(), b"competitor");
        assert_eq!(fs::read(private.join("previous")).unwrap(), b"base");
    }

    #[test]
    fn save_acquisition_requires_real_context_and_refuses_a_foreign_source_before_effects() {
        let (directory, root) = fixture();
        let error = save_file_at_with_hooks(&root, Path::new("file"), "mine", blake3::hash(b"base"), false,
            || panic!("a path-only caller has no registration proof"), |_| panic!("no publication occurred")).unwrap_err();
        assert!(error.to_string().contains("registered filesystem recovery context"));
        let (other_directory, other_root) = fixture();
        let context = fixture_context(&root);
        assert!(save_file_at_in_context(&other_root, Path::new("file"), "mine", blake3::hash(b"base"), false, &context).is_err());
        assert_eq!(fs::read(directory.path().join("file")).unwrap(), b"base");
        assert_eq!(fs::read(other_directory.path().join("file")).unwrap(), b"base");
        assert_eq!(fs::read_dir(context.placement().directory().path()).unwrap().count(), 0);
    }

    #[test]
    fn save_acquisition_success_preserves_mode_and_does_not_leave_holding() {
        let (dir, root) = fixture();
        fs::set_permissions(dir.path().join("file"), Permissions::from_mode(0o640)).unwrap();
        assert!(matches!(
            hook_save(&root, || Ok(()), false).unwrap(),
            SaveOutcome::Saved {
                durability: SaveDurability::Durable,
                ..
            }
        ));
        assert_eq!(
            fs::metadata(dir.path().join("file")).unwrap().mode() & 0o777,
            0o640
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
}
