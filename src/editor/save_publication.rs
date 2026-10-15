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

enum FailedHoldingDirectory {
    Preparation(fs_safety::DirectoryPreparation),
    Publication(fs_safety::DirectoryPublication),
    Unverified(Root),
}

/// A preparation failure keeps the original authenticated source alive too;
/// the retry only touches its own held directory capability, never that file.
struct FailedHolding {
    directory: FailedHoldingDirectory,
    parent: Root,
    _source: File,
}
impl FailedHolding {
    fn retry(&mut self) -> Result<(), String> {
        match &mut self.directory {
            FailedHoldingDirectory::Preparation(preparation) => preparation
                .retry_cleanup()
                .map_err(|error| error.to_string())?,
            FailedHoldingDirectory::Publication(publication) => {
                publication
                    .resolve_recovery()
                    .map_err(|error| error.to_string())?;
                publication
                    .cleanup_empty()
                    .map_err(|error| error.to_string())?;
            }
            FailedHoldingDirectory::Unverified(directory) => {
                if directory
                    .metadata()
                    .map_err(|error| error.to_string())?
                    .nlink()
                    != 0
                {
                    return Err("Creation ownership remains unverified; the retained directory was not changed. Resolve the entry before retrying.".into());
                }
            }
        }
        fs_safety::sync_directory(&self.parent).map_err(|error| error.to_string())
    }
}

/// Owns the actual published private directory and its bounded recovery slot.
/// The receipt, rather than a diagnostic path, survives unresolved outcomes.
struct Holding {
    root: Root,
    parent: Root,
    publication: fs_safety::DirectoryPublication,
    slot: Option<RecoverySlot>,
    // Keep authenticated file identities alive with an unresolved holding.
    // This producer retains at most four file FDs plus the receipt's bounded
    // directory handles, below MAX_RECOVERY_DESCRIPTORS (16).
    files: Vec<File>,
}
impl Holding {
    fn create(parent: &Root, source: &File, context: &RecoveryContext) -> Result<Self, FileIoError> {
        let slot = context.reserve().map_err(FileIoError::Io)?;
        let retained_parent = parent.try_clone().map_err(io_error)?;
        // Duplicate the authenticated source BEFORE mkdir/acquisition. Even a
        // later verification error keeps this inode pinned in the result.
        let retained_source = source.try_clone().map_err(io_error)?;
        let name = OsString::from(format!(".cagents-save-{}", uuid::Uuid::new_v4()));
        let creation = match fs_safety::create_directory_beneath(parent, Path::new(&name), 0o700) {
            Ok(creation) => creation,
            Err(mut error) => {
                let message = error.to_string();
                if let Some(preparation) = error.take_recovery() {
                    let description = format!(
                        "Private save preparation originally named {}",
                        preparation.diagnostic_path().display()
                    );
                    let mut state = FailedHolding {
                        directory: FailedHoldingDirectory::Preparation(preparation),
                        parent: retained_parent,
                        _source: retained_source,
                    };
                    let authority = slot.retain(description, move || state.retry());
                    return Err(FileIoError::Recovery { message, authority });
                }
                return Err(FileIoError::Io(message));
            }
        };
        let publication = match creation {
            fs_safety::DirectoryCreation::Published(receipt) => receipt,
            fs_safety::DirectoryCreation::Existing(_) => {
                return Err(FileIoError::Io(
                    "The private save holding directory already exists; it was left intact.".into(),
                ));
            }
            fs_safety::DirectoryCreation::Unverified { directory, reason } => {
                let message = format!(
                    "Private save holding ownership could not be established; no file was published: {reason}"
                );
                let description = format!(
                    "Unverified save directory originally named {}",
                    directory.path().display()
                );
                let mut state = FailedHolding {
                    directory: FailedHoldingDirectory::Unverified(directory),
                    parent: retained_parent,
                    _source: retained_source,
                };
                let authority = slot.retain(description, move || state.retry());
                return Err(FileIoError::Recovery { message, authority });
            }
        };
        // Exact chmod is authorized only for Published's original held inode.
        let checked = (|| {
            // A producer may still own a pending permission restoration.
            // Preserve its original witness before any consumer chmod.
            if let Some(error) = publication.publication_error() {
                return Err(FileIoError::Io(error.into()));
            }
            fs_safety::set_directory_permissions(publication.directory(), 0o700)
                .map_err(io_error)?;
            let metadata = publication.directory().metadata().map_err(io_error)?;
            if metadata.mode() & 0o7777 != 0o700 || metadata.uid() != unsafe { libc::geteuid() } {
                return Err(FileIoError::Io(
                    "The save holding directory is not private; it was preserved.".into(),
                ));
            }
            publication.directory().try_clone().map_err(io_error)
        })();
        let root = match checked {
            Ok(root) => root,
            Err(error) => {
                let description = format!(
                    "Save holding originally named {}",
                    publication.directory().path().display()
                );
                let mut state = FailedHolding {
                    directory: FailedHoldingDirectory::Publication(publication),
                    parent: retained_parent,
                    _source: retained_source,
                };
                let authority = slot.retain(description, move || state.retry());
                return Err(FileIoError::Recovery {
                    message: error.to_string(),
                    authority,
                });
            }
        };
        Ok(Self {
            root,
            parent: retained_parent,
            publication,
            slot: Some(slot),
            files: vec![retained_source],
        })
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
        self.publication.cleanup_empty().map_err(resolve_error)
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
        self.publication
            .resolve_recovery()
            .map_err(|e| e.to_string())?;
        // Unknown/nonempty contents are preserved. A retry may clean up only
        // an empty exact original; it never deletes an arbitrary leaf or
        // overwrites a newer destination in pursuit of cleanup.
        self.publication
            .cleanup_empty()
            .map_err(|e| e.to_string())?;
        fs_safety::sync_directory(&self.parent).map_err(|e| e.to_string())
    }
    fn location(&self) -> String {
        format!(
            "the held private directory originally named {}",
            self.root.path().display()
        )
    }
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
    context: &RecoveryContext,
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
    context: &RecoveryContext,
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
    if holding.publication.is_removed() {
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
    let (temporary, temporary_name) = match fs_safety::create_temporary_in(&holding.root, 0o600) {
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
        temporary.write_all(bytes).map_err(io_error)?;
        temporary.set_permissions(permissions).map_err(io_error)?;
        fs_safety::copy_access_acl(destination, &temporary).map_err(io_error)?;
        let lost = fs_safety::copy_extended_attributes(destination, &temporary);
        temporary.sync_all().map_err(io_error)?;
        // create_temporary_in deliberately returns O_WRONLY. Authenticate a
        // separate no-follow read descriptor against that exact written inode
        // before hashing; cloning the write-only FD would fail with EBADF.
        let readable = fs_safety::open_beneath(&holding.root, Path::new(&temporary_name))
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
    if let Err(error) =
        fs_safety::rename_new_between(parent, name.as_os_str(), &holding.root, previous)
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
    let acquired_identity = fs_safety::metadata_beneath(&holding.root, Path::new(previous));
    let verify: Result<(File, fs::Metadata), FileIoError> = (|| {
        acquired_identity
            .as_ref()
            .map_err(|error| FileIoError::Io(error.to_string()))?;
        after_acquire(parent, &holding.root).map_err(io_error)?;
        let acquired =
            fs_safety::open_beneath(&holding.root, Path::new(previous)).map_err(resolve_error)?;
        let acquired = holding.keep_file(acquired)?;
        baseline.verify(&acquired, true)?;
        let acquired_version = acquired.metadata().map_err(io_error)?;
        // Sync the acquired directory entry before the replacement is exposed.
        fs_safety::sync_directory(&holding.root).map_err(io_error)?;
        verify_temporary(
            holding,
            &temporary_name,
            readable_temporary.as_ref().unwrap_or(&temporary),
            temporary_baseline.as_ref(),
        )?;
        fs_safety::rename_new_between(&holding.root, &temporary_name, parent, name.as_os_str())
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
                let current = fs_safety::metadata_beneath(&holding.root, Path::new(previous))
                    .map_err(resolve_error)?;
                if first.dev() != current.dev()
                    || first.ino() != current.ino()
                    || first.file_type() != current.file_type()
                {
                    return Err(FileIoError::Io(
                        "The acquired entry was replaced; unknown recovery entry retained.".into(),
                    ));
                }
                fs_safety::rename_new_between(&holding.root, previous, parent, name.as_os_str())
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
    if let Err(error) = after_publish(parent, &holding.root) {
        warnings.push(error.to_string());
    }
    let cleanup = (|| {
        let named = fs_safety::metadata_beneath(&holding.root, Path::new(previous))
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
        fs_safety::unlink_within(&holding.root, previous).map_err(resolve_error)?;
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
        fs_safety::metadata_beneath(&holding.root, Path::new(name)).map_err(resolve_error)?;
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
    verify_temporary(holding, name, file, baseline)?;
    fs_safety::unlink_within(&holding.root, name).map_err(resolve_error)
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
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn hook_save(
        root: &Root,
        hook: impl FnOnce() -> io::Result<()>,
        overwrite: bool,
    ) -> Result<SaveOutcome, FileIoError> {
        save_file_at_with_hooks(
            root,
            Path::new("file"),
            "mine",
            blake3::hash(b"base"),
            overwrite,
            hook,
            fs_safety::sync_directory,
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
            &RecoveryContext::with_guard((), 0).unwrap(),
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
        fs::read_dir(dir)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap()
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
            &RecoveryContext::with_guard((), 0).unwrap(),
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
        let context = RecoveryContext::with_guard(Guard(dropped.clone()), 0).unwrap();
        let (_directory, root) = fixture();
        let source = fs_safety::open_beneath(&root, Path::new("file")).unwrap();
        let holding = Holding::create(&root, &source, &context).unwrap();
        let private_path = holding.root.path().to_owned();
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
        let context = recovery_context_at(&root, Arc::new(Some(protection))).unwrap();
        let source = fs_safety::open_beneath(&root, Path::new("file")).unwrap();
        let holding = Holding::create(&root, &source, &context).unwrap();
        let private = holding.root.try_clone().unwrap();
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

    #[test]
    fn save_acquisition_recovery_keeps_authenticated_file_descriptors_until_actual_resolution() {
        use std::os::fd::AsRawFd;
        let (dir, root) = fixture();
        let source = fs_safety::open_beneath(&root, Path::new("file")).unwrap();
        let holding = Holding::create(&root, &source, &RecoveryContext::with_guard((), 0).unwrap()).unwrap();
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
        fs::write(holding.root.path().join("unresolved"), b"preserve").unwrap();
        drop(source);
        let private_path = holding.root.path().to_owned();
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
            parked.join(holding_name).join("previous"),
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
