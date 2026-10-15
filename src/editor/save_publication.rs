//! Conditional pathname publication for ordinary Save and workspace edits.
//!
//! A private acquisition and RENAME_NOREPLACE protect the destination name.
//! They cannot exclude a noncooperating writer holding an already-open file
//! descriptor after the final content/metadata fence. Such writers do not
//! participate in a filesystem pathname compare-and-swap protocol.
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
    hash: Option<blake3::Hash>,
    hash_limit: u64,
}
impl SaveBaseline {
    pub(super) fn capture(file: &File, hash: Option<blake3::Hash>) -> Result<Self, FileIoError> {
        let metadata = file.metadata().map_err(io_error)?;
        let attributes = fs_safety::content_attributes(file).map_err(io_error)?;
        let after = file.metadata().map_err(io_error)?;
        if !same_file(&metadata, &after) || full_version(&metadata) != full_version(&after) {
            return Err(changed());
        }
        Ok(Self {
            metadata,
            attributes,
            hash,
            hash_limit: MAX_EDITABLE_FILE_BYTES,
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
        if let Some(expected) = self.hash {
            let mut reader = file.try_clone().map_err(io_error)?;
            reader.rewind().map_err(io_error)?;
            let mut hasher = blake3::Hasher::new();
            let mut remaining = self.hash_limit;
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
            if hasher.finalize() != expected {
                return Err(changed());
            }
        }
        if full_version(&before) != full_version(&file.metadata().map_err(io_error)?) {
            return Err(changed());
        }
        Ok(())
    }
}
fn changed() -> FileIoError {
    FileIoError::Io("The save target identity, contents or access metadata changed; no replacement was published.".into())
}

struct Holding {
    root: Root,
    name: OsString,
}
impl Holding {
    fn create(parent: &Root) -> Result<Self, FileIoError> {
        let name = OsString::from(format!(".cagents-save-{}", uuid::Uuid::new_v4()));
        let (root, created) = fs_safety::create_directory_beneath(parent, Path::new(&name), 0o700)
            .map_err(resolve_error)?;
        if !created {
            return Err(FileIoError::Io(
                "The private save holding directory already exists.".into(),
            ));
        }
        // mkdir's requested mode cannot grant group/other access even with an
        // inherited default ACL. Refuse restrictive/nonprivate surprises; do
        // not chmod another directory after a mutable-name reopening.
        let m = root.metadata().map_err(io_error)?;
        if m.mode() & 0o777 != 0o700 || m.uid() != unsafe { libc::geteuid() } {
            return Err(FileIoError::Io(format!(
                "The save holding directory is not private; preserved at {}.",
                root.path().display()
            )));
        }
        Ok(Self { root, name })
    }
    fn remove_empty(&self, parent: &Root) -> Result<(), FileIoError> {
        let named =
            fs_safety::metadata_beneath(parent, Path::new(&self.name)).map_err(resolve_error)?;
        let held = self.root.metadata().map_err(io_error)?;
        if !named.is_dir() || named.dev() != held.dev() || named.ino() != held.ino() {
            return Err(FileIoError::Io("The private save directory was moved or replaced; its replacement was left intact.".into()));
        }
        fs_safety::remove_directory_within(parent, &self.name).map_err(resolve_error)
    }
    fn location(&self) -> String {
        // This is the original name, not a claim that an external directory
        // rename cannot have changed where the held directory is reachable.
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
    before_publish: impl FnOnce() -> io::Result<()>,
    after_acquire: impl FnOnce(&Root, &Root) -> io::Result<()>,
    after_publish: impl FnOnce(&Root, &Root) -> io::Result<()>,
    sync_parent: impl FnOnce(&Root) -> io::Result<()>,
) -> Result<SaveDurability, FileIoError> {
    let holding = Holding::create(parent)?;
    let (mut temporary, temporary_name) = match fs_safety::create_temporary_in(&holding.root, 0o600)
    {
        Ok(value) => value,
        Err(error) => {
            let cleanup = holding.remove_empty(parent);
            return Err(append_cleanup(resolve_error(error), cleanup));
        }
    };
    let mut temporary_baseline = None;
    let prepare = (|| {
        fs_safety::copy_ownership(destination, &temporary).map_err(io_error)?;
        fs_safety::copy_access_acl(destination, &temporary).map_err(io_error)?;
        temporary.write_all(bytes).map_err(io_error)?;
        temporary.set_permissions(permissions).map_err(io_error)?;
        fs_safety::copy_access_acl(destination, &temporary).map_err(io_error)?;
        let lost = fs_safety::copy_extended_attributes(destination, &temporary);
        temporary.sync_all().map_err(io_error)?;
        let mut prepared = SaveBaseline::capture(&temporary, Some(blake3::hash(bytes)))?;
        prepared.hash_limit = bytes.len() as u64;
        temporary_baseline = Some(prepared);
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
                &holding,
                &temporary_name,
                &temporary,
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
            &holding,
            &temporary_name,
            &temporary,
            temporary_baseline.as_ref(),
            resolve_error(error),
        ));
    }
    let acquired_identity = fs_safety::metadata_beneath(&holding.root, Path::new(previous));
    let verify = (|| {
        acquired_identity
            .as_ref()
            .map_err(|error| FileIoError::Io(error.to_string()))?;
        after_acquire(parent, &holding.root).map_err(io_error)?;
        let acquired =
            fs_safety::open_beneath(&holding.root, Path::new(previous)).map_err(resolve_error)?;
        baseline.verify(&acquired, true)?;
        let acquired_version = acquired.metadata().map_err(io_error)?;
        // Sync the acquired directory entry before the replacement is exposed.
        fs_safety::sync_directory(&holding.root).map_err(io_error)?;
        verify_temporary(
            &holding,
            &temporary_name,
            &temporary,
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
                &holding,
                &temporary_name,
                &temporary,
                temporary_baseline.as_ref(),
            );
            let mut message = format!("{error}; ");
            match put_back {
                Ok(()) => {
                    message.push_str("the acquired entry was restored without replacing newer work");
                    if let Err(error) = fs_safety::sync_directory(parent) { message.push_str(&format!("; restoration durability unconfirmed: {error}")); }
                    if let Err(error) = holding.remove_empty(parent) { message.push_str(&format!("; {error}")); }
                }
                Err(error) => message.push_str(&format!("the acquired entry remains as previous in {}; destination left intact: {error}", holding.location())),
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
    holding: &Holding,
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
            matches!(result, SaveDurability::Unconfirmed { ref error } if error.contains("retained")),
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
            matches!(result, SaveDurability::Unconfirmed { .. }),
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
