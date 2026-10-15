//! Presence-aware publication for an editor buffer whose Undo can remove a file.
use super::*;
use std::{ffi::OsString, os::unix::fs::MetadataExt, sync::Arc};

/// A private unlinked metadata template retains the removed file's access
/// rules and attributes without sharing later changes through a hard link or
/// keeping a second text copy.
#[derive(Debug)]
pub struct FileRestoration {
    source: File,
    permissions: Permissions,
    _permit: RestorationPermit,
}

impl FileRestoration {
    pub(crate) fn git_mode(&self) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        if self.permissions.mode() & 0o100 != 0 { 0o100755 } else { 0o100644 }
    }
}

static RESTORATION_DESCRIPTORS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);
const MAX_RESTORATION_DESCRIPTORS: usize = 256;

#[derive(Debug)]
struct RestorationPermit;
impl RestorationPermit {
    fn acquire() -> Result<Self, FileIoError> {
        RESTORATION_DESCRIPTORS.fetch_update(std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
            |count| (count < MAX_RESTORATION_DESCRIPTORS).then_some(count + 1))
            .map(|_| Self).map_err(|_| FileIoError::Io("Undo metadata capacity is exhausted. Close or reload older editors before removing this file.".into()))
    }
}
impl Drop for RestorationPermit {
    fn drop(&mut self) {
        RESTORATION_DESCRIPTORS.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl PartialEq for FileRestoration {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(self, other)
    }
}
impl Eq for FileRestoration {}

#[derive(Clone)]
pub(crate) struct PresenceSave {
    pub expected_present: bool,
    /// Explicit Save Again after an observed external deletion; never an Undo fallback.
    pub external_missing_recreate: bool,
    pub desired_present: bool,
    pub expected_mode: Option<u32>,
    pub restoration: Option<Arc<FileRestoration>>,
}

/// Ordinary writes keep the established save contract. Existence changes use
/// acquisition/no-replace publication, never an empty write or blind unlink.
pub(crate) fn save_document_file(
    worktree: &Path,
    path: &Path,
    text: &str,
    expected_hash: blake3::Hash,
    overwrite: bool,
    presence: PresenceSave,
) -> Result<SaveOutcome, FileIoError> {
    let root = worktree_root(worktree)?;
    let context = missing_filesystem_context(&root)?;
    save_document_file_at_in_context(&root, path, text, expected_hash, overwrite, presence, &context)
}

/// Caller retains root/protection for the whole publication transaction.
pub(crate) fn save_document_file_at(root: &Root, path: &Path, text: &str, expected_hash: blake3::Hash, overwrite: bool, presence: PresenceSave) -> Result<SaveOutcome, FileIoError> {
    let context = missing_filesystem_context(root)?;
    save_document_file_at_in_context(root, path, text, expected_hash, overwrite, presence, &context)
}

pub(crate) fn save_document_file_at_in_context(root: &Root, path: &Path, text: &str, expected_hash: blake3::Hash, overwrite: bool, presence: PresenceSave, context: &FilesystemRecoveryContext) -> Result<SaveOutcome, FileIoError> {
    context.validate_for_source(root).map_err(io_error)?;
    validate_repo_path(path)?;
    if presence.expected_present && presence.desired_present && presence.restoration.is_none() {
        return save_file_at_in_context(root, path, text, expected_hash, overwrite, context);
    }
    if !presence.expected_present && presence.desired_present && presence.external_missing_recreate && presence.restoration.is_none() {
        return save_missing_file_at_in_context(root, path, text, context);
    }
    let (parent, name) = fs_safety::open_parent_beneath(root, path).map_err(resolve_error)?;
    if presence.expected_present && presence.desired_present {
        let restoration = presence
            .restoration
            .as_deref()
            .ok_or_else(|| FileIoError::Io("The Undo metadata is unavailable.".into()))?;
        return replace_or_remove_document_file_in_context(root, &parent, &name, expected_hash, None, || Ok(()), Some((restoration, text)), context);
    }
    if !presence.desired_present {
        if !text.is_empty() {
            return Err(FileIoError::Io(
                "Refusing to remove a buffer containing newer text.".into(),
            ));
        }
        if presence.expected_present {
            return replace_or_remove_document_file_in_context(root, &parent, &name, expected_hash, presence.expected_mode, || Ok(()), None, context);
        }
        require_new_destination(&parent, Path::new(&name))?;
        let restoration = presence.restoration.ok_or_else(|| {
            FileIoError::Io("The removed file's restoration metadata is unavailable.".into())
        })?;
        return Ok(SaveOutcome::Absent {
            restoration,
            durability: sync_durability(&parent),
        });
    }
    let restoration = presence.restoration.ok_or_else(|| {
        FileIoError::Io(
            "The removed file's restoration metadata is unavailable. Reload before saving.".into(),
        )
    })?;
    restore_document_file(&parent, &name, text, &restoration, context)
}

// Explicit confirmation of an external deletion may create a fresh private
// file; owned absence still requires its immutable restoration template.
pub(super) fn save_missing_file_at_in_context(root: &Root, path: &Path, text: &str, context: &FilesystemRecoveryContext) -> Result<SaveOutcome, FileIoError> {
    save_missing_file_in_context_with_hook(root, path, text, context, || Ok(()))
}
fn save_missing_file_in_context_with_hook(root: &Root, path: &Path, text: &str, context: &FilesystemRecoveryContext, before_publish: impl FnOnce() -> io::Result<()>) -> Result<SaveOutcome, FileIoError> {
    context.validate_for_source(root).map_err(io_error)?;
    validate_repo_path(path)?;
    let (parent, name) = fs_safety::open_parent_beneath(root, path).map_err(resolve_error)?;
    require_new_destination(&parent, Path::new(&name))?;
    let mut holding = PresenceHolding::create_in_context(&parent, &name, context)?;
    if let Err(error) = holding.create_temporary() { return Err(holding.finish_error(error)); }
    let written = (|| {
        let temporary = holding.temporary_mut()?;
        temporary.set_permissions(Permissions::from_mode(0o600)).map_err(io_error)?;
        temporary.write_all(text.as_bytes()).map_err(io_error)?;
        temporary.sync_all().map_err(io_error)?;
        before_publish().map_err(io_error)?;
        holding.publish_temporary()
    })();
    if let Err(error) = written { return Err(holding.finish_error(error)); }
    let durability = match holding.finish() { Ok(()) => sync_durability(&parent), Err(error) => recovery_durability(error, "The recreated file is now published.") };
    Ok(SaveOutcome::Saved { content_hash: blake3::hash(text.as_bytes()), durability })
}

fn restore_document_file(
    parent: &Root,
    name: &std::ffi::OsStr,
    text: &str,
    restoration: &FileRestoration,
    context: &FilesystemRecoveryContext,
) -> Result<SaveOutcome, FileIoError> {
    require_new_destination(parent, Path::new(name))?;
    let mut holding = PresenceHolding::create_in_context(parent, name, context)?;
    if let Err(error) = holding.create_temporary() { return Err(holding.finish_error(error)); }
    let result = (|| {
        let temporary = holding.temporary_mut()?;
        fs_safety::copy_ownership(&restoration.source, temporary).map_err(io_error)?;
        fs_safety::copy_access_acl(&restoration.source, temporary).map_err(io_error)?;
        temporary.write_all(text.as_bytes()).map_err(io_error)?;
        temporary.set_permissions(restoration.permissions.clone()).map_err(io_error)?;
        fs_safety::copy_access_acl(&restoration.source, temporary).map_err(io_error)?;
        temporary.set_permissions(restoration.permissions.clone()).map_err(io_error)?;
        let lost = fs_safety::copy_extended_attributes(&restoration.source, temporary);
        if !lost.is_empty() { return Err(FileIoError::Io(format!("Could not restore file metadata: {}", lost.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")))); }
        let restored = temporary.metadata().map_err(io_error)?;
        let template = restoration.source.metadata().map_err(io_error)?;
        if restored.mode() != template.mode() || restored.uid() != template.uid() || restored.gid() != template.gid()
            || fs_safety::content_attributes(temporary).map_err(io_error)? != fs_safety::content_attributes(&restoration.source).map_err(io_error)? {
            return Err(FileIoError::Io("The file's access metadata could not be restored exactly.".into()));
        }
        temporary.sync_all().map_err(io_error)?;
        holding.publish_temporary()
    })();
    if let Err(error) = result { return Err(holding.finish_error(error)); }
    // Publication is already known. Cleanup/sync failures preserve that fact
    // and the exact live authority instead of returning a false write refusal.
    let durability = match holding.finish() {
        Ok(()) => sync_durability(parent),
        Err(error) => recovery_durability(error, "The restored file is now published."),
    };
    Ok(SaveOutcome::Saved { content_hash: blake3::hash(text.as_bytes()), durability })
}

fn sync_durability(parent: &Root) -> SaveDurability {
    match fs_safety::sync_directory(parent) {
        Ok(()) => SaveDurability::Durable,
        Err(error) => SaveDurability::Unconfirmed {
            error: error.to_string(),
        },
    }
}

fn same_inode(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    left.dev() == right.dev() && left.ino() == right.ino() && left.is_file() && right.is_file()
}
fn version(metadata: &fs::Metadata) -> (u64, u32, u32, u32, i64, i64, i64, i64) {
    (
        metadata.len(),
        metadata.mode(),
        metadata.uid(),
        metadata.gid(),
        metadata.mtime(),
        metadata.mtime_nsec(),
        metadata.ctime(),
        metadata.ctime_nsec(),
    )
}

#[path = "presence_holding.rs"]
mod presence_holding;
use presence_holding::PresenceHolding;

fn put_back_acquired_file(mut holding: PresenceHolding, error: FileIoError) -> FileIoError {
    holding.restore_original = true;
    holding.finish_error(error)
}

fn combine_recovery_errors(first: FileIoError, second: FileIoError) -> FileIoError {
    let authority = match (first.recovery_authority(), second.recovery_authority()) {
        (Some(a), Some(b)) => Some(a.clone().combine(b.clone())),
        (Some(a), None) | (None, Some(a)) => Some(a.clone()),
        (None, None) => None,
    };
    let message = format!("{first} {second}");
    match authority { Some(authority) => FileIoError::Recovery { message, authority }, None => FileIoError::Io(message) }
}
fn recovery_durability(error: FileIoError, prefix: &str) -> SaveDurability {
    let message = format!("{prefix} {error}");
    match error.recovery_authority() {
        Some(authority) => SaveDurability::Recovery { error: message, authority: authority.clone() },
        None => SaveDurability::Unconfirmed { error: message },
    }
}

fn combine_durability(first: SaveDurability, second: SaveDurability) -> SaveDurability {
    if matches!(first, SaveDurability::Durable) { return second; }
    if matches!(second, SaveDurability::Durable) { return first; }
    let message = |value: &SaveDurability| match value {
        SaveDurability::Durable => "".to_owned(),
        SaveDurability::Unconfirmed { error } | SaveDurability::Recovery { error, .. } => error.clone(),
    };
    let first_error = match first.recovery_authority() {
        Some(authority) => FileIoError::Recovery { message: message(&first), authority: authority.clone() },
        None => FileIoError::Io(message(&first)),
    };
    let second_error = match second.recovery_authority() {
        Some(authority) => FileIoError::Recovery { message: message(&second), authority: authority.clone() },
        None => FileIoError::Io(message(&second)),
    };
    recovery_durability(combine_recovery_errors(first_error, second_error), "")
}

#[cfg(test)]
fn remove_document_file(
    namespace_root: &Root,
    parent: &Root,
    name: &std::ffi::OsStr,
    expected_hash: blake3::Hash,
    expected_mode: Option<u32>,
    before_acquire: impl FnOnce() -> io::Result<()>,
) -> Result<SaveOutcome, FileIoError> {
    replace_or_remove_document_file(
        namespace_root,
        parent,
        name,
        expected_hash,
        expected_mode,
        before_acquire,
        None,
    )
}

#[cfg(test)]
fn replace_or_remove_document_file(
    _namespace_root: &Root,
    parent: &Root,
    name: &std::ffi::OsStr,
    expected_hash: blake3::Hash,
    expected_mode: Option<u32>,
    before_acquire: impl FnOnce() -> io::Result<()>,
    replacement: Option<(&FileRestoration, &str)>,
) -> Result<SaveOutcome, FileIoError> {
    let context = crate::filesystem_recovery_context::fixture_context(_namespace_root).map_err(io_error)?;
    replace_or_remove_document_file_in_context(_namespace_root, parent, name, expected_hash, expected_mode, before_acquire, replacement, &context)
}

fn replace_or_remove_document_file_in_context(
    _namespace_root: &Root,
    parent: &Root,
    name: &std::ffi::OsStr,
    expected_hash: blake3::Hash,
    expected_mode: Option<u32>,
    before_acquire: impl FnOnce() -> io::Result<()>,
    replacement: Option<(&FileRestoration, &str)>,
    context: &FilesystemRecoveryContext,
) -> Result<SaveOutcome, FileIoError> {
    context.validate_for_source(_namespace_root).map_err(io_error)?;
    let permit = if replacement.is_none() {
        Some(RestorationPermit::acquire()?)
    } else {
        None
    };
    let listed = fs_safety::metadata_beneath(parent, Path::new(name)).map_err(resolve_error)?;
    let mut source = fs_safety::open_beneath(parent, Path::new(name)).map_err(resolve_error)?;
    let original = source.metadata().map_err(io_error)?;
    if !same_inode(&listed, &original)
        || replacement.is_none()
            && (!matches!(expected_mode, Some(0o100644 | 0o100755))
                || (original.mode() & 0o100 != 0) != (expected_mode == Some(0o100755)))
    {
        return Err(FileIoError::Io(
            "The file identity or mode changed before Revert could remove it.".into(),
        ));
    }
    let attributes = fs_safety::content_attributes(&source).map_err(io_error)?;
    let bytes = fs_safety::read_bounded(&mut source, MAX_EDITABLE_FILE_BYTES).map_err(io_error)?;
    let disk_hash = blake3::hash(&bytes);
    if disk_hash != expected_hash {
        return Ok(SaveOutcome::Conflict { disk_hash });
    }
    let checked = source.metadata().map_err(io_error)?;
    if version(&original) != version(&checked) {
        return Err(FileIoError::Io(
            "The file changed while Revert checked its contents.".into(),
        ));
    }
    let mut holding = PresenceHolding::create_in_context(parent, name, context)?;
    holding.pin_source(source);
    let acquired_name = std::ffi::OsStr::new("file");
    if let Err(error) = before_acquire().map_err(io_error).and_then(|()| {
        holding.validate_live()?;
        fs_safety::rename_new_between(parent, name, holding.directory(), acquired_name).map_err(resolve_error)
    }) {
        return Err(holding.finish_error(error));
    }
    if let Err(error) = holding.capture_original() {
        return Err(holding.finish_error(error));
    }
    let verify = (|| {
        let acquired = fs_safety::metadata_beneath(holding.directory(), Path::new(acquired_name))
            .map_err(resolve_error)?;
        if !same_inode(&original, &acquired)
            || original.mode() != acquired.mode()
            || original.uid() != acquired.uid()
            || original.gid() != acquired.gid()
            || original.len() != acquired.len()
            || original.mtime() != acquired.mtime()
            || original.mtime_nsec() != acquired.mtime_nsec()
        {
            return Err(FileIoError::Io(
                "The file was replaced while Revert acquired it.".into(),
            ));
        }
        let mut held =
            fs_safety::open_beneath(holding.directory(), Path::new(acquired_name)).map_err(resolve_error)?;
        let before = held.metadata().map_err(io_error)?;
        let bytes =
            fs_safety::read_bounded(&mut held, MAX_EDITABLE_FILE_BYTES).map_err(io_error)?;
        let after = held.metadata().map_err(io_error)?;
        if !same_inode(&before, &after)
            || version(&before) != version(&after)
            || blake3::hash(&bytes) != expected_hash
            || fs_safety::content_attributes(&held).map_err(io_error)? != attributes
        {
            return Err(FileIoError::Io(
                "The file contents changed while Revert acquired it.".into(),
            ));
        }
        immutable_metadata_template(&mut holding, &held, &before, &attributes)
    })();
    let template = match verify { Ok(template) => template, Err(error) => return Err(put_back_acquired_file(holding, error)) };
    if let Some((restoration, text)) = replacement {
        // Keep the acquired original until the replacement is published. A
        // racing new destination is refused and the original is put back or
        // retained explicitly; no newer path is overwritten.
        let outcome = match restore_document_file(parent, name, text, restoration, context) { Ok(outcome) => outcome, Err(error) => return Err(put_back_acquired_file(holding, error)) };
        let cleanup = holding.finish();
        return match (outcome, cleanup) {
            (
                SaveOutcome::Saved {
                    content_hash,
                    durability,
                },
                Ok(()),
            ) => Ok(SaveOutcome::Saved {
                content_hash,
                durability: if matches!(durability, SaveDurability::Durable) {
                    sync_durability(parent)
                } else {
                    durability
                },
            }),
            (SaveOutcome::Saved { content_hash, durability }, Err(error)) => {
                let cleanup = recovery_durability(error, "The older Undo metadata was published.");
                Ok(SaveOutcome::Saved { content_hash, durability: combine_durability(durability, cleanup) })
            },
            _ => unreachable!("restoration publishes a present file"),
        };
    }
    // Verify the acquired inode and immutable contents/access witness before
    // deleting the original. Failed verification leaves its exact authority.
    if let Err(error) = holding.remove_original() {
        return Err(put_back_acquired_file(holding, error));
    }
    let durability = match holding.finish() {
        Ok(()) => sync_durability(parent),
        Err(error) => recovery_durability(error, "The file was removed."),
    };
    Ok(SaveOutcome::Absent {
        restoration: Arc::new(FileRestoration {
            source: template,
            permissions: original.permissions(),
            _permit: permit.expect("a removal reserved its bounded metadata descriptor"),
        }),
        durability,
    })
}

/// Captures only metadata for a cross-presence Reload. Both the returned text
/// and this template must name the same exact regular-file contents.
pub(crate) fn capture_restoration_at(root: &Root, path: &Path, expected_hash: blake3::Hash) -> Result<Arc<FileRestoration>, FileIoError> {
    let context = missing_filesystem_context(root)?;
    capture_restoration_at_in_context(root, path, expected_hash, &context)
}

#[cfg(test)]
fn capture_restoration_with_hook(root: &Root, path: &Path, expected_hash: blake3::Hash, before_unlink: impl FnOnce(&Root, &std::ffi::OsStr) -> io::Result<()>) -> Result<Arc<FileRestoration>, FileIoError> {
    let context = crate::filesystem_recovery_context::fixture_context(root).map_err(io_error)?;
    capture_restoration_in_context_with_hook(root, path, expected_hash, before_unlink, &context)
}

pub(crate) fn capture_restoration_at_in_context(root: &Root, path: &Path, expected_hash: blake3::Hash, context: &FilesystemRecoveryContext) -> Result<Arc<FileRestoration>, FileIoError> {
    capture_restoration_in_context_with_hook(root, path, expected_hash, |_, _| Ok(()), context)
}

fn capture_restoration_in_context_with_hook(root: &Root, path: &Path, expected_hash: blake3::Hash, before_unlink: impl FnOnce(&Root, &std::ffi::OsStr) -> io::Result<()>, context: &FilesystemRecoveryContext) -> Result<Arc<FileRestoration>, FileIoError> {
    context.validate_for_source(root).map_err(io_error)?;
    validate_repo_path(path)?;
    let permit = RestorationPermit::acquire()?;
    let (parent, name) = fs_safety::open_parent_beneath(root, path).map_err(resolve_error)?;
    let listed = fs_safety::metadata_beneath(&parent, Path::new(&name)).map_err(resolve_error)?;
    let mut source = fs_safety::open_beneath(&parent, Path::new(&name)).map_err(resolve_error)?;
    let before = source.metadata().map_err(io_error)?;
    if !same_inode(&listed, &before) || !before.is_file() {
        return Err(FileIoError::Io("The reloaded file identity changed while retaining Undo metadata.".into()));
    }
    let attributes = fs_safety::content_attributes(&source).map_err(io_error)?;
    let bytes = fs_safety::read_bounded(&mut source, MAX_EDITABLE_FILE_BYTES).map_err(io_error)?;
    if blake3::hash(&bytes) != expected_hash || version(&before) != version(&source.metadata().map_err(io_error)?) {
        return Err(FileIoError::Io("The reloaded file changed while retaining Undo metadata.".into()));
    }
    // The metadata can be world-readable/writable. Until its temporary name
    // is removed, it must live behind a private directory, just as removal's
    // metadata capture does. Never expose it in the ordinary worktree parent.
    let mut holding = PresenceHolding::create_in_context(&parent, &name, context)?;
    let capture = immutable_metadata_template_with_hook(&mut holding, &source, &before, &attributes, before_unlink);
    let cleanup = holding.finish();
    let template = match (capture, cleanup) {
        (Ok(template), Ok(())) => template,
        (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
        (Err(error), Err(cleanup)) => return Err(combine_recovery_errors(error, cleanup)),
    };
    // Re-read the same descriptor after capture: chmod/xattrs and text races
    // cannot produce metadata for another revision of these displayed bytes.
    source.rewind().map_err(io_error)?;
    let bytes = fs_safety::read_bounded(&mut source, MAX_EDITABLE_FILE_BYTES).map_err(io_error)?;
    let named = fs_safety::metadata_beneath(&parent, Path::new(&name)).map_err(resolve_error)?;
    if blake3::hash(&bytes) != expected_hash || version(&before) != version(&source.metadata().map_err(io_error)?) || !same_inode(&before, &named) {
        return Err(FileIoError::Io("The reloaded file changed while retaining Undo metadata.".into()));
    }
    Ok(Arc::new(FileRestoration { source: template, permissions: before.permissions(), _permit: permit }))
}

fn immutable_metadata_template(holding: &mut PresenceHolding, source: &File, before: &fs::Metadata, attributes: &std::collections::BTreeMap<OsString, Vec<u8>>) -> Result<File, FileIoError> {
    immutable_metadata_template_with_hook(holding, source, before, attributes, |_, _| Ok(()))
}

fn immutable_metadata_template_with_hook(holding: &mut PresenceHolding, source: &File, before: &fs::Metadata, attributes: &std::collections::BTreeMap<OsString, Vec<u8>>, before_unlink: impl FnOnce(&Root, &std::ffi::OsStr) -> io::Result<()>) -> Result<File, FileIoError> {
    holding.create_temporary()?;
    let template = holding.temporary()?;
    let captured = (|| {
        fs_safety::copy_ownership(source, &template).map_err(io_error)?;
        fs_safety::copy_access_acl(source, &template).map_err(io_error)?;
        template.set_permissions(before.permissions()).map_err(io_error)?;
        fs_safety::copy_access_acl(source, &template).map_err(io_error)?;
        let lost = fs_safety::copy_extended_attributes(source, &template);
        if !lost.is_empty() { return Err(FileIoError::Io(format!("Could not retain file metadata for Undo: {}", lost.iter().map(ToString::to_string).collect::<Vec<_>>().join(", ")))); }
        template.sync_all().map_err(io_error)?;
        if version(before) != version(&source.metadata().map_err(io_error)?)
            || &fs_safety::content_attributes(source).map_err(io_error)? != attributes
            || &fs_safety::content_attributes(&template).map_err(io_error)? != attributes {
            return Err(FileIoError::Io("The source metadata changed while retaining Undo metadata.".into()));
        }
        before_unlink(holding.directory(), holding.temporary_name()).map_err(io_error)?;
        Ok(())
    })();
    // The owned writer descriptor stays inside the holder until exact-leaf
    // removal succeeds, including copy/hook/unlink failures.
    let template = holding.unlink_temporary()?;
    captured?;
    if template.metadata().map_err(io_error)?.nlink() != 0 {
        return Err(FileIoError::Io("The Undo metadata template acquired an outside hard link.".into()));
    }
    Ok(template)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    fn save_document_file_at(root: &Root, path: &Path, text: &str, expected_hash: blake3::Hash, overwrite: bool, presence: PresenceSave) -> Result<SaveOutcome, FileIoError> {
        let context = crate::filesystem_recovery_context::fixture_context(root).map_err(io_error)?;
        super::save_document_file_at_in_context(root, path, text, expected_hash, overwrite, presence, &context)
    }
    fn save_document_file(worktree: &Path, path: &Path, text: &str, expected_hash: blake3::Hash, overwrite: bool, presence: PresenceSave) -> Result<SaveOutcome, FileIoError> {
        save_document_file_at(&worktree_root(worktree)?, path, text, expected_hash, overwrite, presence)
    }
    fn capture_restoration_at(root: &Root, path: &Path, hash: blake3::Hash) -> Result<Arc<FileRestoration>, FileIoError> {
        let context = crate::filesystem_recovery_context::fixture_context(root).map_err(io_error)?;
        super::capture_restoration_at_in_context(root, path, hash, &context)
    }

    #[test]
    fn missing_file_private_publication_preserves_racing_public_destination() {
        let directory = tempfile::tempdir().unwrap(); let root = Root::open(directory.path()).unwrap();
        let context = crate::filesystem_recovery_context::fixture_context(&root).unwrap();
        let path = Path::new("file.rs");
        let error = save_missing_file_in_context_with_hook(&root, path, "mine", &context, || fs::write(directory.path().join(path), "foreign")).unwrap_err();
        assert_eq!(fs::read(directory.path().join(path)).unwrap(), b"foreign");
        assert!(error.recovery_authority().is_none(), "owned private cleanup can complete without deleting the public replacement");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        fs::remove_file(directory.path().join(path)).unwrap();
        let outcome = save_missing_file_at_in_context(&root, path, "confirmed", &context).unwrap();
        assert!(matches!(outcome, SaveOutcome::Saved { .. }));
        assert_eq!(fs::read(directory.path().join(path)).unwrap(), b"confirmed");
        assert_eq!(fs::metadata(directory.path().join(path)).unwrap().mode() & 0o777, 0o600);
    }

    #[test]
    fn presence_recovery_retains_exact_parent_after_replacement_and_resolves_only_original() {
        let directory = tempfile::tempdir().unwrap();
        let named = directory.path().join("parent"); let moved = directory.path().join("moved");
        fs::create_dir(&named).unwrap(); let parent = Root::open(&named).unwrap();
        let holding = PresenceHolding::create(&parent, std::ffi::OsStr::new("file")).unwrap();
        let original_holding = holding.directory().path().to_path_buf();
        fs::write(original_holding.join("unknown"), "inspect first").unwrap();
        let error = holding.finish().unwrap_err(); let authority = error.recovery_authority().unwrap().clone();
        fs::rename(&named, &moved).unwrap(); fs::create_dir(&named).unwrap();
        let foreign = named.join(original_holding.file_name().unwrap()); fs::create_dir(&foreign).unwrap();
        fs::write(foreign.join("foreign"), "preserve").unwrap();
        assert!(authority.retry().is_err()); assert!(!authority.is_resolved());
        fs::remove_file(original_holding.join("unknown")).unwrap();
        authority.retry().unwrap(); assert!(authority.is_resolved());
        assert_eq!(fs::read_to_string(foreign.join("foreign")).unwrap(), "preserve");
        assert!(!original_holding.exists());
    }

    #[test]
    fn presence_acquired_file_refuses_modified_open_descriptor_and_foreign_leaf() {
        let directory = tempfile::tempdir().unwrap(); let parent = Root::open(directory.path()).unwrap();
        fs::write(directory.path().join("file"), "original").unwrap();
        let mut writer = fs::OpenOptions::new().write(true).open(directory.path().join("file")).unwrap();
        let mut holding = PresenceHolding::create(&parent, std::ffi::OsStr::new("file")).unwrap();
        fs_safety::rename_new_between(&parent, std::ffi::OsStr::new("file"), holding.directory(), std::ffi::OsStr::new("file")).unwrap();
        holding.capture_original().unwrap();
        writer.write_all(b"changed!").unwrap(); writer.sync_all().unwrap();
        assert!(holding.remove_original().is_err());
        let held = holding.directory().path().join("file");
        let error = put_back_acquired_file(holding, FileIoError::Io("verification refused".into()));
        let authority = error.recovery_authority().unwrap();
        assert!(authority.retry().is_err());
        assert_eq!(fs::read_to_string(&held).unwrap(), "changed!");
        assert!(!directory.path().join("file").exists());
        fs::rename(&held, held.with_file_name("original-changed")).unwrap(); fs::write(&held, "foreign").unwrap();
        assert!(authority.retry().is_err());
        assert_eq!(fs::read_to_string(&held).unwrap(), "foreign");
    }

    #[test]
    fn legacy_presence_shared_source_refuses_without_registered_placement() {
        let directory = tempfile::tempdir().unwrap(); fs::create_dir(directory.path().join("shared")).unwrap();
        fs::set_permissions(directory.path().join("shared"), Permissions::from_mode(0o777)).unwrap();
        fs::write(directory.path().join("shared/file"), "original").unwrap();
        let root = Root::open(directory.path()).unwrap();
        assert!(super::save_document_file_at(&root, Path::new("shared/file"), "", blake3::hash(b"original"), false, removal()).is_err());
        assert_eq!(fs::read_to_string(directory.path().join("shared/file")).unwrap(), "original");
        assert_eq!(fs::read_dir(directory.path().join("shared")).unwrap().count(), 1);
    }

    #[test]
    fn metadata_template_unlink_failure_keeps_typed_original_authority_and_foreign_leaf() {
        let directory = tempfile::tempdir().unwrap(); let root = Root::open(directory.path()).unwrap();
        fs::write(directory.path().join("file"), "contents").unwrap();
        let mut retained = None;
        let error = capture_restoration_with_hook(&root, Path::new("file"), blake3::hash(b"contents"), |holding, name| {
            let original = holding.path().join(name); let saved = holding.path().join("original-template");
            fs::rename(&original, &saved)?; fs::write(&original, "foreign leaf")?;
            retained = Some((original, saved)); Ok(())
        }).unwrap_err();
        let authority = error.recovery_authority().expect("exact template cleanup authority");
        let (original, saved) = retained.unwrap();
        assert!(authority.retry().is_err());
        assert_eq!(fs::read_to_string(&original).unwrap(), "foreign leaf");
        assert!(saved.exists());
        // Explicit fixture repair restores the original internal name; product
        // retry itself must never discover/adopt a replacement witness.
        fs::remove_file(&original).unwrap(); fs::rename(&saved, &original).unwrap();
        authority.retry().unwrap(); assert!(authority.is_resolved());
        assert_eq!(fs::read_to_string(directory.path().join("file")).unwrap(), "contents");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn editor_file_presence_cleanup_preserves_a_replacement_holding_directory() {
        let directory = tempfile::tempdir().unwrap(); let root = Root::open(directory.path()).unwrap();
        fs::write(directory.path().join("file"), "original").unwrap();
        let context = crate::filesystem_recovery_context::fixture_context(&root).unwrap();
        let outside = context.placement().directory();
        let mut replaced = None;
        let result = replace_or_remove_document_file_in_context(&root, &root, std::ffi::OsStr::new("file"), blake3::hash(b"original"), Some(0o100644), || {
            let holding = fs::read_dir(outside.path())?.filter_map(Result::ok)
                .find(|entry| entry.file_name().to_string_lossy().starts_with(".cagents-editor-")).unwrap().path();
            fs::rename(&holding, outside.path().join("original-holding"))?;
            fs::create_dir(&holding)?; fs::set_permissions(&holding, Permissions::from_mode(0o711))?;
            fs::write(holding.join("foreign"), "preserve replacement")?;
            replaced = Some(holding); Ok(())
        }, None, &context).unwrap();
        assert!(matches!(result, SaveOutcome::Absent { durability: SaveDurability::Recovery { .. }, .. }));
        let replaced = replaced.unwrap();
        assert_eq!(fs::read_to_string(replaced.join("foreign")).unwrap(), "preserve replacement");
        assert_eq!(fs::metadata(&replaced).unwrap().mode() & 0o777, 0o711);
        assert_eq!(fs::metadata(outside.path().join("original-holding")).unwrap().mode() & 0o777, 0o700);
        assert!(!directory.path().join("file").exists());
    }

    #[test]
    fn editor_file_presence_reload_cleanup_preserves_a_replacement_holding_directory() {
        let directory = tempfile::tempdir().unwrap(); let root = Root::open(directory.path()).unwrap();
        fs::write(directory.path().join("file"), "contents").unwrap();
        let mut replaced = None;
        let result = capture_restoration_with_hook(&root, Path::new("file"), blake3::hash(b"contents"), |holding, _| {
            let path = holding.path().to_path_buf();
            fs::rename(&path, directory.path().join("original-holding"))?;
            fs::create_dir(&path)?; fs::set_permissions(&path, Permissions::from_mode(0o711))?;
            fs::write(path.join("foreign"), "preserve replacement")?;
            replaced = Some(path); Ok(())
        });
        assert!(result.is_err());
        let replaced = replaced.unwrap();
        assert_eq!(fs::read_to_string(replaced.join("foreign")).unwrap(), "preserve replacement");
        assert_eq!(fs::metadata(replaced).unwrap().mode() & 0o777, 0o711);
        assert_eq!(fs::read_to_string(directory.path().join("file")).unwrap(), "contents");
    }

    #[test]
    fn editor_file_presence_uses_the_actual_protected_nested_parent() {
        let directory = tempfile::tempdir().unwrap(); let root = Root::open(directory.path()).unwrap();
        fs::create_dir(directory.path().join("shared")).unwrap();
        fs::set_permissions(directory.path().join("shared"), Permissions::from_mode(0o755)).unwrap();
        let path = Path::new("shared/file"); fs::write(directory.path().join(path), "contents").unwrap();
        let saved = save_document_file_at(&root, path, "", blake3::hash(b"contents"), false, removal()).unwrap();
        let SaveOutcome::Absent { restoration, .. } = saved else { panic!("removed file"); };
        assert!(!directory.path().join(path).exists());
        save_document_file_at(&root, path, "contents", blake3::hash(b""), false, PresenceSave { expected_present: false, external_missing_recreate: false, desired_present: true, expected_mode: None, restoration: Some(restoration) }).unwrap();
        assert_eq!(fs::read_to_string(directory.path().join(path)).unwrap(), "contents");
        assert_eq!(fs::read_dir(directory.path().join("shared")).unwrap().count(), 1);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn legacy_presence_top_level_refuses_without_registered_placement() {
        let directory = tempfile::tempdir().unwrap(); let root = Root::open(directory.path()).unwrap();
        fs::write(directory.path().join("file"), "contents").unwrap();
        fs::set_permissions(directory.path(), Permissions::from_mode(0o777)).unwrap();
        assert!(super::save_document_file_at(&root, Path::new("file"), "", blake3::hash(b"contents"), false, removal()).is_err());
        assert_eq!(fs::read_to_string(directory.path().join("file")).unwrap(), "contents");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn editor_file_presence_private_holding_survives_restrictive_umask() {
        const CHILD: &str = "CAGENTS_PRESENCE_HOLDING_UMASK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let output = std::process::Command::new(std::env::current_exe().unwrap()).args(["--exact", "editor::presence::tests::editor_file_presence_private_holding_survives_restrictive_umask", "--test-threads=1"])
                .env(CHILD, "1").output().unwrap();
            assert!(output.status.success(), "{output:?}");
            assert!(String::from_utf8_lossy(&output.stdout).contains("1 passed; 0 failed"), "the exact owning contract must run: {output:?}");
            return;
        }
        let directory = tempfile::tempdir().unwrap(); let root = Root::open(directory.path()).unwrap();
        fs::write(directory.path().join("file"), "contents").unwrap();
        let context = crate::filesystem_recovery_context::fixture_context(&root).unwrap();
        let previous = unsafe { libc::umask(0o777) };
        let result = capture_restoration_in_context_with_hook(&root, Path::new("file"), blake3::hash(b"contents"), |holding, _| {
            assert_eq!(holding.metadata()?.mode() & 0o777, 0o700); Ok(())
        }, &context);
        unsafe { libc::umask(previous) };
        assert!(result.is_ok(), "private capture must set permissions on its retained inode: {result:?}");
        assert_eq!(fs::read_to_string(directory.path().join("file")).unwrap(), "contents");
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn reload_metadata_is_private_while_named_and_refuses_an_outside_hard_link() {
        let directory = tempfile::tempdir().unwrap(); let root = Root::open(directory.path()).unwrap();
        let path = Path::new("file"); fs::write(directory.path().join(path), "contents").unwrap();
        fs::set_permissions(directory.path().join(path), Permissions::from_mode(0o666)).unwrap();
        let restoration = capture_restoration_with_hook(&root, path, blake3::hash(b"contents"), |holding, name| {
            assert_eq!(holding.metadata()?.mode() & 0o777, 0o700);
            assert_ne!(holding.path(), directory.path());
            assert!(fs::read_dir(directory.path())?.all(|entry| { let entry = entry.unwrap(); entry.file_name() == "file" || entry.file_type().unwrap().is_dir() }));
            assert_eq!(fs::metadata(holding.path().join(name))?.mode() & 0o777, 0o666);
            Ok(())
        }).unwrap();
        assert_eq!(restoration.source.metadata().unwrap().nlink(), 0);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        let alias = directory.path().join("outside-template");
        assert!(capture_restoration_with_hook(&root, path, blake3::hash(b"contents"), |holding, name| fs::hard_link(holding.path().join(name), &alias)).is_err());
        fs::remove_file(alias).unwrap();
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        assert_eq!(fs::read_to_string(directory.path().join(path)).unwrap(), "contents");
    }

    #[test]
    fn reload_restoration_capture_refuses_changed_bytes_and_keeps_immutable_metadata() {
        let directory = tempfile::tempdir().unwrap(); let root = Root::open(directory.path()).unwrap(); let path = Path::new("file");
        fs::write(directory.path().join(path), "current").unwrap();
        fs::set_permissions(directory.path().join(path), Permissions::from_mode(0o640)).unwrap();
        assert!(capture_restoration_at(&root, path, blake3::hash(b"old")).is_err());
        let restoration = capture_restoration_at(&root, path, blake3::hash(b"current")).unwrap();
        fs::set_permissions(directory.path().join(path), Permissions::from_mode(0o700)).unwrap();
        fs::remove_file(directory.path().join(path)).unwrap();
        save_document_file_at(&root, path, "current", blake3::hash(b""), false, PresenceSave {
            external_missing_recreate: false, expected_present: false, desired_present: true,
            expected_mode: None, restoration: Some(restoration),
        }).unwrap();
        assert_eq!(fs::metadata(directory.path().join(path)).unwrap().mode() & 0o777, 0o640);
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn explicit_missing_recreation_uses_the_pinned_root_and_never_weakens_undo() {
        let directory = tempfile::tempdir().unwrap(); let named = directory.path().join("worktree"); let moved = directory.path().join("original");
        fs::create_dir(&named).unwrap(); let root = Root::open(&named).unwrap();
        fs::rename(&named, &moved).unwrap(); fs::create_dir(&named).unwrap(); fs::write(named.join("file"), "replacement").unwrap();
        let missing = PresenceSave { external_missing_recreate: false, expected_present: false, desired_present: true, expected_mode: None, restoration: None };
        assert!(save_document_file_at(&root, Path::new("file"), "mine", blake3::hash(b""), true, missing.clone()).is_err());
        save_document_file_at(&root, Path::new("file"), "mine", blake3::hash(b""), true, PresenceSave { external_missing_recreate: true, ..missing.clone() }).unwrap();
        assert_eq!(fs::read_to_string(moved.join("file")).unwrap(), "mine");
        assert_eq!(fs::read_to_string(named.join("file")).unwrap(), "replacement");
        assert!(save_document_file_at(&root, Path::new("file"), "overwrite", blake3::hash(b""), true, PresenceSave { external_missing_recreate: true, ..missing }).is_err());
        assert_eq!(fs::read_to_string(moved.join("file")).unwrap(), "mine");
    }

    fn acl(named: u16, mask: u16) -> Vec<u8> {
        let mut bytes = 2_u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in [
            (1_u16, 6_u16, u32::MAX),
            (2, named, 12345),
            (4, 0, u32::MAX),
            (16, mask, u32::MAX),
            (32, 0, u32::MAX),
        ] {
            bytes.extend_from_slice(&tag.to_le_bytes());
            bytes.extend_from_slice(&permissions.to_le_bytes());
            bytes.extend_from_slice(&id.to_le_bytes());
        }
        bytes
    }

    fn install_acl(file: &File, bytes: &[u8]) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: the held descriptor and bounded byte slice remain live for
        // fsetxattr, and the static attribute name is NUL terminated.
        let result = unsafe {
            libc::fsetxattr(
                file.as_raw_fd(),
                c"system.posix_acl_access".as_ptr(),
                bytes.as_ptr().cast(),
                bytes.len(),
                0,
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    #[test]
    fn editor_file_presence_keeps_original_acl_when_an_external_hard_link_changes_after_removal() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("new.txt"), "original").unwrap();
        let source = File::open(root.join("new.txt")).unwrap();
        if let Err(error) = install_acl(&source, &acl(4, 4)) {
            if matches!(error.raw_os_error(), Some(libc::EOPNOTSUPP | libc::EINVAL)) {
                eprintln!("PRESENCE_ACL_TEMPLATE_UNSUPPORTED: {error}");
                return;
            }
            panic!("ACL fixture failed: {error}");
        }
        let original_mode = source.metadata().unwrap().mode();
        let original_attributes = fs_safety::content_attributes(&source).unwrap();
        fs::hard_link(root.join("new.txt"), root.join("outside-link")).unwrap();
        let SaveOutcome::Absent { restoration, .. } = save_document_file(
            root,
            Path::new("new.txt"),
            "",
            blake3::hash(b"original"),
            false,
            PresenceSave {
                expected_mode: Some(0o100644),
                ..removal()
            },
        )
        .unwrap() else {
            panic!("removed");
        };
        install_acl(&source, &acl(6, 6)).unwrap();
        let newer_attributes = fs_safety::content_attributes(&source).unwrap();
        assert_ne!(original_attributes, newer_attributes);
        save_document_file(
            root,
            Path::new("new.txt"),
            "original",
            blake3::hash(b""),
            false,
            PresenceSave {
                external_missing_recreate: false,
            expected_present: false,
                desired_present: true,
                expected_mode: None,
                restoration: Some(restoration),
            },
        )
        .unwrap();
        let restored = File::open(root.join("new.txt")).unwrap();
        assert_eq!(restored.metadata().unwrap().mode(), original_mode);
        assert_eq!(
            fs_safety::content_attributes(&restored).unwrap(),
            original_attributes
        );
        assert_eq!(
            fs_safety::content_attributes(&source).unwrap(),
            newer_attributes,
            "Undo never changes the external hard link"
        );
        eprintln!("PRESENCE_ACL_TEMPLATE_VERIFIED");
    }

    #[test]
    fn editor_file_presence_refuses_same_mode_acl_changes_during_acquisition() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("new.txt"), "original").unwrap();
        let source = File::open(directory.path().join("new.txt")).unwrap();
        if let Err(error) = install_acl(&source, &acl(4, 4)) {
            if matches!(error.raw_os_error(), Some(libc::EOPNOTSUPP | libc::EINVAL)) {
                eprintln!("PRESENCE_ACL_ACQUIRE_UNSUPPORTED: {error}");
                return;
            }
            panic!("ACL fixture failed: {error}");
        }
        let before_mode = source.metadata().unwrap().mode();
        let root = Root::open(directory.path()).unwrap();
        let result = remove_document_file(
            &root,
            &root,
            std::ffi::OsStr::new("new.txt"),
            blake3::hash(b"original"),
            Some(0o100644),
            || install_acl(&source, &acl(0, 4)),
        );
        assert!(result.is_err());
        assert_eq!(
            source.metadata().unwrap().mode(),
            before_mode,
            "the ACL change deliberately keeps st_mode identical"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("new.txt")).unwrap(),
            "original"
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
        eprintln!("PRESENCE_ACL_ACQUIRE_VERIFIED");
    }

    fn removal() -> PresenceSave {
        PresenceSave {
            external_missing_recreate: false,
            expected_present: true,
            desired_present: false,
            expected_mode: Some(0o100755),
            restoration: None,
        }
    }

    #[test]
    fn editor_file_presence_reports_a_committed_replacement_when_holding_cleanup_fails() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("new.txt"), "synthetic alpha").unwrap();
        fs::set_permissions(root.join("new.txt"), Permissions::from_mode(0o600)).unwrap();
        let SaveOutcome::Absent { restoration, .. } = save_document_file(
            root,
            Path::new("new.txt"),
            "",
            blake3::hash(b"synthetic alpha"),
            false,
            PresenceSave {
                expected_mode: Some(0o100644),
                ..removal()
            },
        )
        .unwrap() else {
            panic!("removed");
        };
        fs::write(root.join("new.txt"), "synthetic beta").unwrap();
        fs::set_permissions(root.join("new.txt"), Permissions::from_mode(0o644)).unwrap();
        let held_root = Root::open(root).unwrap();
        let context = crate::filesystem_recovery_context::fixture_context(&held_root).unwrap();
        let outside = context.placement().directory();
        let outcome = replace_or_remove_document_file_in_context(
            &held_root,
            &held_root,
            std::ffi::OsStr::new("new.txt"),
            blake3::hash(b"synthetic beta"),
            None,
            || {
                let holding = fs::read_dir(outside.path())?
                    .find_map(|entry| {
                        let entry = entry.ok()?;
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".cagents-editor-")
                            .then_some(entry.path())
                    })
                    .expect("private acquisition directory");
                fs::write(holding.join("cleanup-probe"), b"retain directory")
            },
            Some((&restoration, "synthetic alpha")),
            &context,
        )
        .unwrap();
        let SaveOutcome::Saved {
            durability: SaveDurability::Recovery { error, authority },
            ..
        } = outcome
        else {
            panic!("publication remains a committed save despite cleanup failure");
        };
        assert!(error.contains("recovery"));
        assert!(!authority.is_resolved());
        assert_eq!(
            fs::read_to_string(root.join("new.txt")).unwrap(),
            "synthetic alpha"
        );
        assert_eq!(
            fs::metadata(root.join("new.txt")).unwrap().mode() & 0o777,
            0o600
        );
        let holding = fs::read_dir(outside.path())
            .unwrap()
            .filter_map(Result::ok)
            .find(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".cagents-editor-")
            })
            .unwrap()
            .path();
        assert!(
            !holding.join("file").exists(),
            "the warning must not claim this file remains"
        );
        assert!(holding.join("cleanup-probe").exists());
    }

    #[test]
    fn editor_file_presence_removes_and_restores_exact_executable_bytes_without_replacing_new_work()
    {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        let path = Path::new("new.sh");
        let original = "#!/bin/sh\r\necho original";
        fs::write(root.join(path), original).unwrap();
        fs::set_permissions(root.join(path), Permissions::from_mode(0o751)).unwrap();
        fs::hard_link(root.join(path), root.join("outside-link")).unwrap();
        let SaveOutcome::Absent { restoration, .. } = save_document_file(
            root,
            path,
            "",
            blake3::hash(original.as_bytes()),
            false,
            removal(),
        )
        .unwrap() else {
            panic!("removed");
        };
        assert!(!root.join(path).exists());
        assert_eq!(fs::read_dir(root).unwrap().count(), 1);
        fs::set_permissions(root.join("outside-link"), Permissions::from_mode(0o600)).unwrap();
        let restore = PresenceSave {
            external_missing_recreate: false,
            expected_present: false,
            desired_present: true,
            expected_mode: None,
            restoration: Some(restoration.clone()),
        };
        assert!(matches!(
            save_document_file(
                root,
                path,
                original,
                blake3::hash(b""),
                false,
                restore.clone()
            )
            .unwrap(),
            SaveOutcome::Saved { .. }
        ));
        assert_eq!(fs::read(root.join(path)).unwrap(), original.as_bytes());
        assert_eq!(
            fs::metadata(root.join(path)).unwrap().mode() & 0o7777,
            0o751
        );
        let SaveOutcome::Absent { .. } = save_document_file(
            root,
            path,
            "",
            blake3::hash(original.as_bytes()),
            false,
            removal(),
        )
        .unwrap() else {
            panic!("removed again");
        };
        fs::write(root.join(path), "newer agent work").unwrap();
        assert!(
            save_document_file(root, path, original, blake3::hash(b""), true, restore).is_err()
        );
        assert_eq!(
            fs::read_to_string(root.join(path)).unwrap(),
            "newer agent work"
        );
    }

    #[test]
    fn editor_file_presence_uses_git_owner_execute_classification_and_restores_all_permissions() {
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        fs::write(root.join("new.txt"), "bytes").unwrap();
        fs::set_permissions(root.join("new.txt"), Permissions::from_mode(0o641)).unwrap();
        let SaveOutcome::Absent { restoration, .. } = save_document_file(
            root,
            Path::new("new.txt"),
            "",
            blake3::hash(b"bytes"),
            false,
            PresenceSave {
                expected_mode: Some(0o100644),
                ..removal()
            },
        )
        .unwrap() else {
            panic!("removed non-owner-executable file");
        };
        save_document_file(
            root,
            Path::new("new.txt"),
            "bytes",
            blake3::hash(b""),
            false,
            PresenceSave {
                external_missing_recreate: false,
            expected_present: false,
                desired_present: true,
                expected_mode: None,
                restoration: Some(restoration),
            },
        )
        .unwrap();
        assert_eq!(
            fs::metadata(root.join("new.txt")).unwrap().mode() & 0o7777,
            0o641
        );
    }

    #[test]
    fn editor_file_presence_refuses_a_replaced_inode_and_puts_newer_contents_back() {
        let directory = tempfile::tempdir().unwrap();
        let root = Root::open(directory.path()).unwrap();
        fs::write(directory.path().join("new.sh"), "original").unwrap();
        fs::set_permissions(
            directory.path().join("new.sh"),
            Permissions::from_mode(0o755),
        )
        .unwrap();
        let result = remove_document_file(
            &root,
            &root,
            std::ffi::OsStr::new("new.sh"),
            blake3::hash(b"original"),
            Some(0o100755),
            || {
                fs::rename(
                    directory.path().join("new.sh"),
                    directory.path().join("original-held"),
                )?;
                fs::write(directory.path().join("new.sh"), "newer agent work")
            },
        );
        assert!(matches!(result, Err(FileIoError::Io(error)) if error.contains("put back")));
        assert_eq!(
            fs::read_to_string(directory.path().join("new.sh")).unwrap(),
            "newer agent work"
        );
        assert_eq!(
            fs::read_to_string(directory.path().join("original-held")).unwrap(),
            "original"
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 2);
    }

    #[test]
    fn editor_file_presence_refuses_writes_to_the_same_inode_during_acquisition() {
        let directory = tempfile::tempdir().unwrap();
        let root = Root::open(directory.path()).unwrap();
        fs::write(directory.path().join("new.sh"), "original").unwrap();
        fs::set_permissions(
            directory.path().join("new.sh"),
            Permissions::from_mode(0o755),
        )
        .unwrap();
        let result = remove_document_file(
            &root,
            &root,
            std::ffi::OsStr::new("new.sh"),
            blake3::hash(b"original"),
            Some(0o100755),
            || fs::write(directory.path().join("new.sh"), "newer same inode"),
        );
        assert!(result.is_err());
        assert_eq!(
            fs::read_to_string(directory.path().join("new.sh")).unwrap(),
            "newer same inode"
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }
}
