//! Presence-aware publication for an editor buffer whose Undo can remove a file.
use super::retained_save::{Holding, RetainedSaveError, RetainedSaveOutcome};
use super::*;
use crate::filesystem_recovery_context::FilesystemRecoveryContext;
use std::{os::unix::fs::MetadataExt, sync::Arc};
type FileIoError = RetainedSaveError;

fn io_error(error: impl std::fmt::Display) -> FileIoError {
    FileIoError::Io(error.to_string())
}
fn resolve_error(error: ResolveError) -> FileIoError {
    io_error(error)
}

/// A private unlinked metadata template retains the removed file's access
/// rules and attributes without sharing later changes through a hard link or
/// keeping a second text copy.
#[derive(Debug)]
pub struct FileRestoration {
    source: File,
    permissions: Permissions,
    _permit: RestorationPermit,
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
    pub desired_present: bool,
    pub expected_mode: Option<u32>,
    pub restoration: Option<Arc<FileRestoration>>,
}

/// Presence writes use the same original read root, pre-effect recovery
/// admission and typed completion carrier as ordinary retained Save.
pub(crate) fn save_document_file_at(
    root: &Root,
    path: &Path,
    text: &str,
    expected_hash: blake3::Hash,
    overwrite: bool,
    presence: PresenceSave,
    context: &FilesystemRecoveryContext,
) -> Result<RetainedSaveOutcome, RetainedSaveError> {
    if presence.expected_present && presence.desired_present && presence.restoration.is_none() {
        return super::retained_save::save_file_at(
            root,
            path,
            text,
            expected_hash,
            overwrite,
            context,
        );
    }
    validate_repo_path(path).map_err(io_error)?;
    context.validate_for_source(root).map_err(io_error)?;
    let named = Root::open(root.path()).map_err(io_error)?;
    context.validate_for_source(&named).map_err(io_error)?;
    drop(named);
    if text.len() as u64 > MAX_EDITABLE_FILE_BYTES {
        return Err(io_error("The edited file exceeds the save size limit"));
    }
    let (parent, name) = fs_safety::open_parent_beneath(root, path).map_err(resolve_error)?;
    context
        .validate_save_parent(&parent, path, &name)
        .map_err(io_error)?;
    if presence.expected_present && presence.desired_present {
        let restoration = presence
            .restoration
            .as_deref()
            .ok_or_else(|| io_error("The Undo metadata is unavailable."))?;
        return replace_or_remove_document_file_at(
            &parent,
            &name,
            expected_hash,
            None,
            || Ok(()),
            Some((restoration, text)),
            context,
        );
    }
    if !presence.desired_present {
        if !text.is_empty() {
            return Err(io_error(
                "Refusing to remove a buffer containing newer text.",
            ));
        }
        if presence.expected_present {
            return replace_or_remove_document_file_at(
                &parent,
                &name,
                expected_hash,
                presence.expected_mode,
                || Ok(()),
                None,
                context,
            );
        }
        require_new_destination(&parent, Path::new(&name)).map_err(io_error)?;
        let restoration = presence
            .restoration
            .ok_or_else(|| io_error("The removed file's restoration metadata is unavailable."))?;
        return Ok(RetainedSaveOutcome::without_recovery(SaveOutcome::Absent {
            restoration,
            durability: sync_durability(&parent),
        }));
    }
    let restoration = presence.restoration.ok_or_else(|| {
        io_error("The removed file's restoration metadata is unavailable. Reload before saving.")
    })?;
    require_new_destination(&parent, Path::new(&name)).map_err(io_error)?;
    let holding = Holding::create(&parent, &restoration.source, context)?;
    let result = restore_document_file(&holding, &parent, &name, text, &restoration);
    holding.finish_presence(&parent, result)
}

/// Prepare privately on the qualified same-filesystem holding. Linking the
/// exact anonymous inode is no-replace; failed work has no public temporary
/// name to unlink or reacquire. The surrounding Holding owns all recovery.
fn restore_document_file(
    holding: &Holding,
    parent: &Root,
    name: &std::ffi::OsStr,
    text: &str,
    restoration: &FileRestoration,
) -> Result<SaveOutcome, FileIoError> {
    require_new_destination(parent, Path::new(name)).map_err(io_error)?;
    holding.validate_payload().map_err(io_error)?;
    let mut temporary =
        fs_safety::create_anonymous_in(holding.root(), 0o600).map_err(resolve_error)?;
    fs_safety::copy_ownership(&restoration.source, &temporary).map_err(io_error)?;
    fs_safety::copy_access_acl(&restoration.source, &temporary).map_err(io_error)?;
    temporary.write_all(text.as_bytes()).map_err(io_error)?;
    temporary
        .set_permissions(restoration.permissions.clone())
        .map_err(io_error)?;
    fs_safety::copy_access_acl(&restoration.source, &temporary).map_err(io_error)?;
    // The private metadata template, not an external hard link, owns Undo.
    temporary
        .set_permissions(restoration.permissions.clone())
        .map_err(io_error)?;
    let lost = fs_safety::copy_extended_attributes(&restoration.source, &temporary);
    if !lost.is_empty() {
        return Err(io_error(format!(
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
        return Err(io_error(
            "The file's access metadata could not be restored exactly.",
        ));
    }
    temporary.sync_all().map_err(io_error)?;
    // Actual context validation includes the original named read root and
    // original relative target parent. A replacement attachment cannot adopt it.
    holding.validate_payload().map_err(io_error)?;
    fs_safety::link_anonymous_new(parent, &temporary, name).map_err(resolve_error)?;
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

fn verify_acquired(
    holding: &Holding,
    acquired_name: &std::ffi::OsStr,
    original: &fs::Metadata,
    expected_hash: blake3::Hash,
    attributes: &std::collections::BTreeMap<std::ffi::OsString, Vec<u8>>,
) -> Result<File, FileIoError> {
    holding.validate_payload().map_err(io_error)?;
    let acquired = fs_safety::metadata_beneath(holding.root(), Path::new(acquired_name))
        .map_err(resolve_error)?;
    if !same_inode(original, &acquired)
        || original.mode() != acquired.mode()
        || original.uid() != acquired.uid()
        || original.gid() != acquired.gid()
        || original.len() != acquired.len()
        || original.mtime() != acquired.mtime()
        || original.mtime_nsec() != acquired.mtime_nsec()
    {
        return Err(io_error("The file was replaced while Revert acquired it."));
    }
    let mut held =
        fs_safety::open_beneath(holding.root(), Path::new(acquired_name)).map_err(resolve_error)?;
    let before = held.metadata().map_err(io_error)?;
    if !same_inode(original, &before) {
        return Err(io_error("The acquired file identity changed."));
    }
    let bytes = fs_safety::read_bounded(&mut held, MAX_EDITABLE_FILE_BYTES).map_err(io_error)?;
    let after = held.metadata().map_err(io_error)?;
    if !same_inode(&before, &after)
        || version(&before) != version(&after)
        || blake3::hash(&bytes) != expected_hash
        || &fs_safety::content_attributes(&held).map_err(io_error)? != attributes
    {
        return Err(io_error(
            "The file contents changed while Revert acquired it.",
        ));
    }
    Ok(held)
}

fn replace_or_remove_document_file_at(
    parent: &Root,
    name: &std::ffi::OsStr,
    expected_hash: blake3::Hash,
    expected_mode: Option<u32>,
    before_acquire: impl FnOnce() -> io::Result<()>,
    replacement: Option<(&FileRestoration, &str)>,
    context: &FilesystemRecoveryContext,
) -> Result<RetainedSaveOutcome, RetainedSaveError> {
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
        return Err(io_error(
            "The file identity or mode changed before Revert could remove it.",
        ));
    }
    // Complete-or-refuse Main metadata budgets run before any destructive effect.
    let attributes = fs_safety::content_attributes(&source).map_err(io_error)?;
    let bytes = fs_safety::read_bounded(&mut source, MAX_EDITABLE_FILE_BYTES).map_err(io_error)?;
    let disk_hash = blake3::hash(&bytes);
    if disk_hash != expected_hash {
        return Ok(RetainedSaveOutcome::without_recovery(
            SaveOutcome::Conflict { disk_hash },
        ));
    }
    let checked = source.metadata().map_err(io_error)?;
    if version(&original) != version(&checked) {
        return Err(io_error(
            "The file changed while Revert checked its contents.",
        ));
    }
    let holding = Holding::create(parent, &source, context)?;
    let result = (|| {
        let acquired_name = std::ffi::OsStr::new("file");
        before_acquire().map_err(io_error)?;
        holding.validate_payload().map_err(io_error)?;
        fs_safety::rename_new_between(parent, name, holding.root(), acquired_name)
            .map_err(resolve_error)?;
        let put_back = |error: FileIoError| {
            let restored = holding.validate_payload().map_err(io_error).and_then(|()| {
                fs_safety::rename_new_between(holding.root(), acquired_name, parent, name)
                    .map_err(resolve_error)
            });
            let location = if restored.is_ok() {
                "The acquired file was put back without replacing newer work.".to_owned()
            } else {
                format!(
                    "The acquired file remains in retained private recovery originally named {}; newer destination contents were left intact.",
                    holding.root().path().join(acquired_name).display()
                )
            };
            io_error(format!("{error} {location}"))
        };
        let template = (|| {
            let held = verify_acquired(
                &holding,
                acquired_name,
                &original,
                expected_hash,
                &attributes,
            )?;
            let before = held.metadata().map_err(io_error)?;
            let template =
                fs_safety::create_anonymous_in(holding.root(), 0o600).map_err(resolve_error)?;
            fs_safety::copy_ownership(&held, &template).map_err(io_error)?;
            fs_safety::copy_access_acl(&held, &template).map_err(io_error)?;
            template
                .set_permissions(original.permissions())
                .map_err(io_error)?;
            fs_safety::copy_access_acl(&held, &template).map_err(io_error)?;
            let lost = fs_safety::copy_extended_attributes(&held, &template);
            if !lost.is_empty() {
                return Err(io_error(format!(
                    "Could not retain file metadata for Undo: {}",
                    lost.iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )));
            }
            template.sync_all().map_err(io_error)?;
            let captured = held.metadata().map_err(io_error)?;
            if version(&before) != version(&captured)
                || fs_safety::content_attributes(&held).map_err(io_error)? != attributes
                || fs_safety::content_attributes(&template).map_err(io_error)? != attributes
            {
                return Err(io_error(
                    "The source metadata changed while Revert captured it.",
                ));
            }
            Ok(template)
        })()
        .map_err(&put_back)?;
        if let Some((restoration, text)) = replacement {
            let outcome = restore_document_file(&holding, parent, name, text, restoration)
                .map_err(&put_back)?;
            // Once the replacement is public, an old-file verification failure
            // is a committed save with retained older work, never a false failure.
            let cleanup = verify_acquired(
                &holding,
                acquired_name,
                &original,
                expected_hash,
                &attributes,
            )
            .and_then(|_| {
                fs_safety::unlink_within(holding.root(), acquired_name).map_err(resolve_error)
            });
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
                        error: format!(
                            "The older Undo metadata was published; the previous file remains in retained recovery: {error}"
                        ),
                    },
                }),
                _ => unreachable!("restoration publishes a present file"),
            };
        }
        verify_acquired(
            &holding,
            acquired_name,
            &original,
            expected_hash,
            &attributes,
        )
        .map_err(&put_back)?;
        fs_safety::unlink_within(holding.root(), acquired_name)
            .map_err(|error| put_back(resolve_error(error)))?;
        Ok(SaveOutcome::Absent {
            restoration: Arc::new(FileRestoration {
                source: template,
                permissions: original.permissions(),
                _permit: permit.expect("a removal reserved its bounded metadata descriptor"),
            }),
            durability: sync_durability(parent),
        })
    })();
    // No directory is removed by a remembered public name. This consumes the
    // actual receipt after leaf work, or transfers its SAME slot to the result.
    holding.finish_presence(parent, result)
}

#[cfg(test)]
fn test_outcome(
    result: Result<RetainedSaveOutcome, RetainedSaveError>,
) -> Result<SaveOutcome, RetainedSaveError> {
    result.map(|outcome| {
        assert!(
            outcome.recovery_authority().is_none(),
            "owning recovery tests must inspect the typed carrier"
        );
        outcome.into_parts().0
    })
}
#[cfg(test)]
pub(crate) fn save_document_file(
    worktree: &Path,
    path: &Path,
    text: &str,
    expected_hash: blake3::Hash,
    overwrite: bool,
    presence: PresenceSave,
) -> Result<SaveOutcome, RetainedSaveError> {
    let root = Root::open(worktree).map_err(io_error)?;
    let context = FilesystemRecoveryContext::acquire_for_save(&root, path, &[worktree.to_owned()])
        .map_err(|error| match error.authority {
            Some(authority) => RetainedSaveError::Recovery {
                message: error.message,
                authority,
            },
            None => io_error(error.message),
        })?;
    let result = save_document_file_at(
        &root,
        path,
        text,
        expected_hash,
        overwrite,
        presence,
        &context,
    );
    test_outcome(super::retained_save::finish_context(&context, result))
}
#[cfg(test)]
fn remove_document_file(
    parent: &Root,
    name: &std::ffi::OsStr,
    expected_hash: blake3::Hash,
    expected_mode: Option<u32>,
    before_acquire: impl FnOnce() -> io::Result<()>,
) -> Result<SaveOutcome, RetainedSaveError> {
    let context = FilesystemRecoveryContext::acquire_for_save(
        parent,
        Path::new(name),
        &[parent.path().to_owned()],
    )
    .map_err(|error| match error.authority {
        Some(authority) => RetainedSaveError::Recovery {
            message: error.message,
            authority,
        },
        None => io_error(error.message),
    })?;
    let result = replace_or_remove_document_file_at(
        parent,
        name,
        expected_hash,
        expected_mode,
        before_acquire,
        None,
        &context,
    );
    test_outcome(super::retained_save::finish_context(&context, result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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
        let context = FilesystemRecoveryContext::acquire_for_save(
            &held_root,
            Path::new("new.txt"),
            &[root.to_owned()],
        )
        .unwrap();
        let mut holding_path = None;
        let outcome = replace_or_remove_document_file_at(
            &held_root,
            std::ffi::OsStr::new("new.txt"),
            blake3::hash(b"synthetic beta"),
            None,
            || {
                let holding = fs::read_dir(context.placement().directory().path())?
                    .find_map(|entry| {
                        let entry = entry.ok()?;
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".cagents-save-")
                            .then_some(entry.path())
                    })
                    .expect("actual factory holding");
                fs::write(holding.join("cleanup-probe"), b"retain directory")?;
                holding_path = Some(holding);
                Ok(())
            },
            Some((&restoration, "synthetic alpha")),
            &context,
        );
        let outcome = crate::editor::retained_save::finish_context(&context, outcome).unwrap();
        let authority = outcome
            .recovery_authority()
            .expect("typed live receipt")
            .clone();
        let SaveOutcome::Saved {
            durability: SaveDurability::Unconfirmed { error },
            ..
        } = outcome.outcome()
        else {
            panic!("publication remains a committed save despite cleanup failure");
        };
        assert!(error.contains("holding cleanup"));
        assert_eq!(
            fs::read_to_string(root.join("new.txt")).unwrap(),
            "synthetic alpha"
        );
        assert_eq!(
            fs::metadata(root.join("new.txt")).unwrap().mode() & 0o777,
            0o600
        );
        let holding = holding_path.unwrap();
        assert!(
            !holding.join("file").exists(),
            "the previous file was removed"
        );
        assert_eq!(
            fs::read(holding.join("cleanup-probe")).unwrap(),
            b"retain directory"
        );
        assert!(
            authority.retry().is_err(),
            "unknown content remains protected"
        );
        assert!(!authority.is_resolved());
        fs::remove_file(holding.join("cleanup-probe")).unwrap();
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(!holding.exists());
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

    #[test]
    fn presence_absent_retains_preparation_durability_warning() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("new.txt"), b"original").unwrap();
        let root = Root::open(directory.path()).unwrap();
        let path = Path::new("new.txt");
        let context = FilesystemRecoveryContext::acquire_for_save(
            &root,
            path,
            &[directory.path().to_owned()],
        )
        .unwrap();
        let _fault = fs_safety::fixture_fail_next_save_preparation_sync();
        let result = save_document_file_at(
            &root,
            path,
            "",
            blake3::hash(b"original"),
            false,
            PresenceSave {
                expected_mode: Some(0o100644),
                ..removal()
            },
            &context,
        );
        let result = crate::editor::retained_save::finish_context(&context, result).unwrap();
        assert!(
            result.recovery_authority().is_none(),
            "completed removal has no live holding"
        );
        let SaveOutcome::Absent {
            restoration,
            durability: SaveDurability::Unconfirmed { error },
        } = result.outcome()
        else {
            panic!("the committed absence keeps its preparation warning");
        };
        assert!(error.contains("private holding preparation durability unconfirmed"));
        assert_eq!(
            restoration.source.metadata().unwrap().len(),
            0,
            "Undo retains metadata, not another text body"
        );
        assert!(!directory.path().join(path).exists());
    }

    #[test]
    fn presence_before_acquire_failure_keeps_actual_holding_receipt_until_explicit_cleanup() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("new.txt"), b"original").unwrap();
        let root = Root::open(directory.path()).unwrap();
        let context = FilesystemRecoveryContext::acquire_for_save(
            &root,
            Path::new("new.txt"),
            &[directory.path().to_owned()],
        )
        .unwrap();
        let mut holding_path = None;
        let result = replace_or_remove_document_file_at(
            &root,
            std::ffi::OsStr::new("new.txt"),
            blake3::hash(b"original"),
            Some(0o100644),
            || {
                let holding = fs::read_dir(context.placement().directory().path())?
                    .find_map(|entry| {
                        let entry = entry.ok()?;
                        entry
                            .file_name()
                            .to_string_lossy()
                            .starts_with(".cagents-save-")
                            .then_some(entry.path())
                    })
                    .expect("actual pre-effect factory receipt");
                fs::write(holding.join("cleanup-probe"), b"preserve unknown contents")?;
                holding_path = Some(holding);
                Err(io::Error::other(
                    "injected refusal before original acquisition",
                ))
            },
            None,
            &context,
        );
        let error = crate::editor::retained_save::finish_context(&context, result).unwrap_err();
        let authority = error
            .recovery_authority()
            .expect("the error retains actual custody")
            .clone();
        assert!(error.to_string().contains("before original acquisition"));
        assert_eq!(
            fs::read(directory.path().join("new.txt")).unwrap(),
            b"original"
        );
        let holding = holding_path.unwrap();
        assert_eq!(
            fs::read(holding.join("cleanup-probe")).unwrap(),
            b"preserve unknown contents"
        );
        assert!(authority.retry().is_err());
        assert!(!authority.is_resolved());
        fs::remove_file(holding.join("cleanup-probe")).unwrap();
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(!holding.exists());
        assert_eq!(
            fs::read(directory.path().join("new.txt")).unwrap(),
            b"original"
        );
    }
}
