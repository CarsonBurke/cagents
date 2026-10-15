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
    let _protection = crate::worktree_cleanup::protect(worktree).map_err(io_error)?;
    let root = worktree_root(worktree)?;
    save_document_file_at(&root, path, text, expected_hash, overwrite, presence)
}

/// Caller retains root/protection for the whole publication transaction.
pub(crate) fn save_document_file_at(root: &Root, path: &Path, text: &str, expected_hash: blake3::Hash, overwrite: bool, presence: PresenceSave) -> Result<SaveOutcome, FileIoError> {
    validate_repo_path(path)?;
    if presence.expected_present && presence.desired_present && presence.restoration.is_none() {
        return save_file_at(root, path, text, expected_hash, overwrite);
    }
    if !presence.expected_present && presence.desired_present && presence.external_missing_recreate && presence.restoration.is_none() {
        return save_missing_file_at(root, path, text);
    }
    let (parent, name) = fs_safety::open_parent_beneath(root, path).map_err(resolve_error)?;
    if presence.expected_present && presence.desired_present {
        let restoration = presence
            .restoration
            .as_deref()
            .ok_or_else(|| FileIoError::Io("The Undo metadata is unavailable.".into()))?;
        return replace_or_remove_document_file(
            &parent,
            &name,
            expected_hash,
            None,
            || Ok(()),
            Some((restoration, text)),
        );
    }
    if !presence.desired_present {
        if !text.is_empty() {
            return Err(FileIoError::Io(
                "Refusing to remove a buffer containing newer text.".into(),
            ));
        }
        if presence.expected_present {
            return remove_document_file(
                &parent,
                &name,
                expected_hash,
                presence.expected_mode,
                || Ok(()),
            );
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
    restore_document_file(&parent, &name, text, &restoration)
}

fn restore_document_file(
    parent: &Root,
    name: &std::ffi::OsStr,
    text: &str,
    restoration: &FileRestoration,
) -> Result<SaveOutcome, FileIoError> {
    require_new_destination(parent, Path::new(name))?;
    let (mut temporary, temporary_name) =
        fs_safety::create_temporary_in(parent, 0o600).map_err(resolve_error)?;
    let result = (|| {
        fs_safety::copy_ownership(&restoration.source, &temporary).map_err(io_error)?;
        fs_safety::copy_access_acl(&restoration.source, &temporary).map_err(io_error)?;
        temporary.write_all(text.as_bytes()).map_err(io_error)?;
        temporary
            .set_permissions(restoration.permissions.clone())
            .map_err(io_error)?;
        fs_safety::copy_access_acl(&restoration.source, &temporary).map_err(io_error)?;
        // A hard-link writer can still chmod the removed inode. The saved
        // permissions, rather than that later mode, own this Undo restoration.
        temporary
            .set_permissions(restoration.permissions.clone())
            .map_err(io_error)?;
        let lost = fs_safety::copy_extended_attributes(&restoration.source, &temporary);
        if !lost.is_empty() {
            return Err(FileIoError::Io(format!(
                "Could not restore file metadata: {}",
                lost.iter()
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
        let restored = temporary.metadata().map_err(io_error)?;
        let template = restoration.source.metadata().map_err(io_error)?;
        if restored.mode() != template.mode()
            || restored.uid() != template.uid()
            || restored.gid() != template.gid()
            || fs_safety::content_attributes(&temporary).map_err(io_error)?
                != fs_safety::content_attributes(&restoration.source).map_err(io_error)?
        {
            return Err(FileIoError::Io(
                "The file's access metadata could not be restored exactly.".into(),
            ));
        }
        temporary.sync_all().map_err(io_error)?;
        fs_safety::rename_new_within(parent, &temporary_name, name).map_err(resolve_error)?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs_safety::unlink_within(parent, &temporary_name);
        return Err(error);
    }
    Ok(SaveOutcome::Saved {
        content_hash: blake3::hash(text.as_bytes()),
        durability: sync_durability(parent),
    })
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

fn remove_document_file(
    parent: &Root,
    name: &std::ffi::OsStr,
    expected_hash: blake3::Hash,
    expected_mode: Option<u32>,
    before_acquire: impl FnOnce() -> io::Result<()>,
) -> Result<SaveOutcome, FileIoError> {
    replace_or_remove_document_file(
        parent,
        name,
        expected_hash,
        expected_mode,
        before_acquire,
        None,
    )
}

fn replace_or_remove_document_file(
    parent: &Root,
    name: &std::ffi::OsStr,
    expected_hash: blake3::Hash,
    expected_mode: Option<u32>,
    before_acquire: impl FnOnce() -> io::Result<()>,
    replacement: Option<(&FileRestoration, &str)>,
) -> Result<SaveOutcome, FileIoError> {
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
    let holding_name = OsString::from(format!(".cagents-editor-{}", uuid::Uuid::new_v4()));
    let (holding, created) =
        fs_safety::create_directory_beneath(parent, Path::new(&holding_name), 0o700)
            .map_err(resolve_error)?;
    if !created {
        return Err(FileIoError::Io(
            "The editor holding directory already exists; no file was removed.".into(),
        ));
    }
    fs_safety::set_directory_permissions(&holding, 0o700).map_err(io_error)?;
    let acquired_name = std::ffi::OsStr::new("file");
    if let Err(error) = before_acquire().map_err(io_error).and_then(|()| {
        fs_safety::rename_new_between(parent, name, &holding, acquired_name).map_err(resolve_error)
    }) {
        let _ = fs_safety::remove_directory_within(parent, &holding_name);
        return Err(error);
    }
    let verify = (|| {
        let acquired = fs_safety::metadata_beneath(&holding, Path::new(acquired_name))
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
            fs_safety::open_beneath(&holding, Path::new(acquired_name)).map_err(resolve_error)?;
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
        immutable_metadata_template(&holding, &held, &before, &attributes)
    })();
    let put_back = |error: FileIoError| {
        let restored = fs_safety::rename_new_between(&holding, acquired_name, parent, name);
        let location = if restored.is_ok() {
            let _ = fs_safety::remove_directory_within(parent, &holding_name);
            "The acquired file was put back without replacing newer work.".to_owned()
        } else {
            format!(
                "The acquired file is preserved at {}; newer destination contents were left intact.",
                holding.path().join(acquired_name).display()
            )
        };
        FileIoError::Io(format!("{error} {location}"))
    };
    let template = verify.map_err(&put_back)?;
    if let Some((restoration, text)) = replacement {
        // Keep the acquired original until the replacement is published. A
        // racing new destination is refused and the original is put back or
        // retained explicitly; no newer path is overwritten.
        let outcome = restore_document_file(parent, name, text, restoration).map_err(&put_back)?;
        let cleanup = match fs_safety::unlink_within(&holding, acquired_name) {
            Err(error) => Err(format!(
                "The previous file remains at {}: {error}",
                holding.path().join(acquired_name).display()
            )),
            Ok(()) => fs_safety::remove_directory_within(parent, &holding_name).map_err(|error| {
                format!(
                    "The previous file was removed; the holding directory remains at {}: {error}",
                    holding.path().display()
                )
            }),
        };
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
            (SaveOutcome::Saved { content_hash, .. }, Err(error)) => Ok(SaveOutcome::Saved {
                content_hash,
                durability: SaveDurability::Unconfirmed {
                    error: format!("The older Undo metadata was published. {error}"),
                },
            }),
            _ => unreachable!("restoration publishes a present file"),
        };
    }
    fs_safety::unlink_within(&holding, acquired_name)
        .map_err(|error| put_back(resolve_error(error)))?;
    let cleanup = fs_safety::remove_directory_within(parent, &holding_name);
    let durability = match cleanup {
        Ok(()) => sync_durability(parent),
        Err(error) => SaveDurability::Unconfirmed {
            error: format!(
                "The file was removed; could not remove empty holding directory {}: {error}",
                holding.path().display()
            ),
        },
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
    capture_restoration_with_hook(root, path, expected_hash, |_, _| Ok(()))
}

fn capture_restoration_with_hook(root: &Root, path: &Path, expected_hash: blake3::Hash, before_unlink: impl FnOnce(&Root, &std::ffi::OsStr) -> io::Result<()>) -> Result<Arc<FileRestoration>, FileIoError> {
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
    let holding_name = OsString::from(format!(".cagents-editor-{}", uuid::Uuid::new_v4()));
    let (holding, created) = fs_safety::create_directory_beneath(&parent, Path::new(&holding_name), 0o700).map_err(resolve_error)?;
    if !created { return Err(FileIoError::Io("The private Undo metadata directory already exists.".into())); }
    let capture = (|| {
        fs_safety::set_directory_permissions(&holding, 0o700).map_err(io_error)?;
        if holding.metadata().map_err(io_error)?.mode() & 0o777 != 0o700 {
            return Err(FileIoError::Io("The Undo metadata directory is not private.".into()));
        }
        immutable_metadata_template_with_hook(&holding, &source, &before, &attributes, before_unlink)
    })();
    let cleanup = (|| {
        let named = fs_safety::metadata_beneath(&parent, Path::new(&holding_name)).map_err(resolve_error)?;
        if !same_inode(&named, &holding.metadata().map_err(io_error)?) {
            return Err(FileIoError::Io("The private Undo metadata directory was moved or replaced; no replacement directory was removed.".into()));
        }
        fs_safety::remove_directory_within(&parent, &holding_name).map_err(resolve_error)
    })();
    let template = match (capture, cleanup) {
        (Ok(template), Ok(())) => template,
        (Err(error), Ok(())) | (Ok(_), Err(error)) => return Err(error),
        (Err(error), Err(cleanup)) => return Err(FileIoError::Io(format!("{error} Could not clean the private metadata directory: {cleanup}"))),
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

fn immutable_metadata_template(parent: &Root, source: &File, before: &fs::Metadata, attributes: &std::collections::BTreeMap<OsString, Vec<u8>>) -> Result<File, FileIoError> {
    immutable_metadata_template_with_hook(parent, source, before, attributes, |_, _| Ok(()))
}

fn immutable_metadata_template_with_hook(parent: &Root, source: &File, before: &fs::Metadata, attributes: &std::collections::BTreeMap<OsString, Vec<u8>>, before_unlink: impl FnOnce(&Root, &std::ffi::OsStr) -> io::Result<()>) -> Result<File, FileIoError> {
    let (template, name) = fs_safety::create_temporary_in(parent, 0o600).map_err(resolve_error)?;
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
        before_unlink(parent, &name).map_err(io_error)?;
        Ok(())
    })();
    fs_safety::unlink_within(parent, &name).map_err(resolve_error)?;
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
        let outcome = replace_or_remove_document_file(
            &held_root,
            std::ffi::OsStr::new("new.txt"),
            blake3::hash(b"synthetic beta"),
            None,
            || {
                let holding = fs::read_dir(root)?
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
        )
        .unwrap();
        let SaveOutcome::Saved {
            durability: SaveDurability::Unconfirmed { error },
            ..
        } = outcome
        else {
            panic!("publication remains a committed save despite cleanup failure");
        };
        assert!(error.contains("previous file was removed"));
        assert!(error.contains("holding directory remains"));
        assert_eq!(
            fs::read_to_string(root.join("new.txt")).unwrap(),
            "synthetic alpha"
        );
        assert_eq!(
            fs::metadata(root.join("new.txt")).unwrap().mode() & 0o777,
            0o600
        );
        let holding = fs::read_dir(root)
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
