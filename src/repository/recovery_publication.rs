//! Private materialization and conditional publication of saved Undo entries.
//!
//! The protected internal namespace contract belongs to DirectoryPublication.
//! Pathname acquisition does not exclude writes through an already-open FD
//! after the final content fence. The protected internal-name assumption lasts
//! for the complete transaction/retry lifetime of the original held directory;
//! its public directory entry, ancestors and reconstructed lookalikes are NOT
//! trusted. Unknown entries are retained, never removed
//! using a diagnostic spelling or a guessed Git temporary filename.
use super::*;
use crate::{
    filesystem_recovery_context::FilesystemRecoveryContext, recovery_authority::RecoverySlot,
};
use std::os::unix::fs::FileExt;

/// Create and retain the actual output inode before giving it to Git. Git
/// receives only its stdout descriptor; it never creates or opens a pathname.
/// The private name is fixed and initially absent within a new held directory.
pub(super) fn create_blob_file(root: &fs_safety::Root) -> Result<Arc<fs::File>, String> {
    use std::os::fd::FromRawFd;
    let name = c"restored-blob";
    // SAFETY: the Root owns its directory FD and the fixed leaf has no slash.
    // O_EXCL|O_NOFOLLOW refuses any existing entry. The returned File owns the
    // exact newly-created descriptor, with read/write access for later proofs.
    let fd = unsafe {
        libc::openat(
            root.descriptor(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd == -1 {
        return Err(std::io::Error::last_os_error().to_string());
    }
    Ok(Arc::new(unsafe { fs::File::from_raw_fd(fd) }))
}

struct Holding {
    publication: fs_safety::DirectoryPublication,
    parent: fs_safety::Root,
    files: Vec<Arc<fs::File>>,
    private_metadata: fs::Metadata,
    context: FilesystemRecoveryContext,
    // Native owners above close before their creation/private charge releases.
    slot: Option<RecoverySlot>,
}

fn remember_directory_error(
    recovery: &DiscardRecovery,
    error: fs_safety::RecoveryDirectoryError,
) -> String {
    match error {
        fs_safety::RecoveryDirectoryError::Rejected(error) => error.to_string(),
        fs_safety::RecoveryDirectoryError::Retained {
            cause,
            recovery: authority,
        } => {
            recovery.live_recovery.retain(authority);
            cause.to_string()
        }
    }
}
impl Holding {
    fn create(
        parent: &fs_safety::Root,
        recovery: &DiscardRecovery,
        cancellation: &CancellationToken,
        context: &FilesystemRecoveryContext,
    ) -> Result<Self, String> {
        let original_parent = parent.try_clone().map_err(|error| error.to_string())?;
        let admission = context.reserve_directory()?;
        let name = OsString::from(format!(".cagents-undo-{}", Uuid::new_v4()));
        let directory = fs_safety::prepare_recovery_directory_with_owner(
            admission,
            &name,
            || cancellation.is_cancelled(),
            original_parent,
        )
        .map_err(|error| remember_directory_error(recovery, error))?;
        let (publication, parent, private_metadata, context, slot) = directory.into_parts();
        // SAME pre-effect admission and parent survive factory errors; success
        // uses its already enrolled original700 inode without cloning/reopening.
        Ok(Self {
            publication,
            parent,
            private_metadata,
            slot: Some(slot),
            files: vec![],
            context,
        })
    }
    fn root(&self) -> &fs_safety::Root {
        self.publication.directory()
    }
    fn create_private_blob(&mut self) -> Result<Arc<fs::File>, String> {
        self.validate()?;
        let file = create_blob_file(self.root())?;
        // Preserve the actual creator before chmod or metadata observation can
        // fail. mkdir/open mode arguments alone do not override a strict umask.
        self.files.push(file.clone());
        self.validate()?;
        let created = file.metadata().map_err(|error| error.to_string())?;
        if !created.is_file() || created.uid() != unsafe { libc::geteuid() } {
            return Err("The recovery blob creator is not an owned regular file".into());
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|error| error.to_string())?;
        let ready = file.metadata().map_err(|error| error.to_string())?;
        if (
            ready.dev(),
            ready.ino(),
            ready.uid(),
            ready.gid(),
            ready.len(),
        ) != (
            created.dev(),
            created.ino(),
            created.uid(),
            created.gid(),
            0,
        ) || ready.mode() & 0o7777 != 0o600
        {
            return Err(
                "The recovery blob creator changed while establishing private read access".into(),
            );
        }
        // The fixed hash-object operation reopens this SAME inherited FD via
        // procfs. Establish readable owner access before that native reopen;
        // later archived-mode restoration retains its original OID/digest proof.
        Ok(file)
    }

    fn validate(&self) -> Result<(), String> {
        self.verify_private()?;
        self.context
            .validate_payload_root(self.root())
            .map_err(|error| error.to_string())?;
        self.verify_private()
    }

    fn verify_private(&self) -> Result<(), String> {
        fs_safety::verify_recovery_directory_private(self.root(), &self.private_metadata)
            .map_err(|error| error.to_string())
    }

    fn open(&mut self, name: &Path) -> Result<Arc<fs::File>, String> {
        self.validate()?;
        let file = Arc::new(
            fs_safety::open_beneath(self.root(), name).map_err(|error| error.to_string())?,
        );
        // Arc clones do not allocate another FD. Keep the actual opened inode
        // before any verification can fail, including a wrong Git output name.
        self.files.push(file.clone());
        Ok(file)
    }

    fn retain(mut self, recovery: &DiscardRecovery) {
        let description = format!(
            "Undo recovery holding originally named {}",
            self.root().path().display()
        );
        let slot = self
            .slot
            .take()
            .expect("reserved before Undo filesystem effects");
        // Whole receiver capture retains root, files, parent and publication.
        recovery
            .live_recovery
            .retain(slot.retain(description, move || self.retry()));
    }

    fn retry(&mut self) -> Result<(), String> {
        if !self.publication.is_removed() {
            self.validate()?;
            self.publication
                .resolve_recovery()
                .map_err(|error| error.to_string())?;
            self.publication
                .cleanup_empty()
                .map_err(|error| error.to_string())?;
        } else {
            self.context
                .placement()
                .validate()
                .map_err(|error| error.to_string())?;
        }
        fs_safety::sync_directory(self.context.placement().directory())
            .map_err(|error| error.to_string())?;
        fs_safety::sync_directory(&self.parent).map_err(|error| error.to_string())
    }
}

fn full_version(metadata: &fs::Metadata) -> ((u64, u64, u64, i64, i64, i64, i64), u32, u32, u32) {
    (
        metadata_version(metadata),
        metadata.mode(),
        metadata.uid(),
        metadata.gid(),
    )
}

fn digest_file(
    file: &fs::File,
    length: u64,
    cancellation: &CancellationToken,
) -> Result<[u8; 32], String> {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    let mut offset = 0;
    let mut buffer = [0; 64 * 1024];
    while offset < length {
        if cancellation.is_cancelled() {
            return Err("Undo cancelled during content verification".into());
        }
        let limit = (length - offset).min(buffer.len() as u64) as usize;
        let count = file
            .read_at(&mut buffer[..limit], offset)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("Undo content changed while being verified".into());
        }
        hash.update(&buffer[..count]);
        offset += count as u64;
    }
    if file
        .read_at(&mut buffer[..1], offset)
        .map_err(|error| error.to_string())?
        != 0
    {
        return Err("Undo content grew while being verified".into());
    }
    Ok(hash.finalize().into())
}

#[cfg(target_os = "linux")]
fn pin_symlink(root: &fs_safety::Root, name: &Path) -> Result<fs::File, String> {
    use std::os::fd::FromRawFd;
    if name.components().count() != 1 || name.file_name() != Some(name.as_os_str()) {
        return Err("Undo metadata pin requires one private leaf".into());
    }
    let name =
        std::ffi::CString::new(name.as_os_str().as_bytes()).map_err(|error| error.to_string())?;
    // SAFETY: the Root owns its directory FD, the component has no NUL/slash,
    // and the returned FD has exactly one File owner with CLOEXEC preserved.
    let descriptor = unsafe {
        libc::openat(
            root.descriptor(),
            name.as_ptr(),
            libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error().to_string());
    }
    Ok(unsafe { fs::File::from_raw_fd(descriptor) })
}
#[cfg(not(target_os = "linux"))]
fn pin_symlink(_root: &fs_safety::Root, _name: &Path) -> Result<fs::File, String> {
    Err("Strong Undo publication requires Linux retained metadata handles".into())
}

struct Proof {
    metadata: fs::Metadata,
    file: Option<Arc<fs::File>>,
    // O_PATH pins symlink identity too; naked dev/ino cannot survive unlink/ABA.
    identity_pin: Arc<fs::File>,
    digest: Option<[u8; 32]>,
    target: Option<PathBuf>,
    access: Option<SavedFileAccess>,
}

impl Proof {
    fn capture(
        holding: &mut Holding,
        name: &Path,
        cancellation: &CancellationToken,
    ) -> Result<Self, String> {
        holding.verify_private()?;
        let metadata =
            fs_safety::metadata_beneath(holding.root(), name).map_err(|error| error.to_string())?;
        let (file, digest, target, identity_pin) = if metadata.is_file() {
            let file = holding.open(name)?;
            if full_version(&metadata)
                != full_version(&file.metadata().map_err(|error| error.to_string())?)
            {
                return Err("Undo entry changed while being opened".into());
            }
            let digest = digest_file(&file, metadata.len(), cancellation)?;
            (Some(file.clone()), Some(digest), None, file)
        } else if metadata.file_type().is_symlink() {
            let pin = Arc::new(pin_symlink(holding.root(), name)?);
            holding.files.push(pin.clone());
            if full_version(&pin.metadata().map_err(|error| error.to_string())?)
                != full_version(&metadata)
            {
                return Err("Undo symlink changed while being pinned".into());
            }
            let target = fs_safety::read_link_beneath(holding.root(), name)
                .map_err(|error| error.to_string())?;
            (None, None, Some(target), pin)
        } else {
            return Err("Undo entry is not a regular file or symlink".into());
        };
        let access = file
            .as_ref()
            .map(|file| SavedFileAccess::capture(file))
            .transpose()?;
        let proof = Self {
            metadata,
            file,
            identity_pin,
            digest,
            target,
            access,
        };
        proof.verify(holding, name, cancellation)?;
        Ok(proof)
    }

    fn after_blob_access_change(
        holding: &Holding,
        name: &Path,
        access: SavedFileAccess,
        file: Arc<fs::File>,
        authenticated: fs::Metadata,
        digest: [u8; 32],
        mode: u32,
        cancellation: &CancellationToken,
        after_mode: impl FnOnce(&Arc<fs::File>) -> Result<(), String>,
    ) -> Result<Self, String> {
        holding.validate()?;
        if full_version(&authenticated)
            != full_version(&file.metadata().map_err(|error| error.to_string())?)
        {
            return Err("Recovery blob changed before original access restoration".into());
        }
        access.install(&file, mode)?;
        file.sync_all().map_err(|error| error.to_string())?;
        after_mode(&file)?;
        let metadata = file.metadata().map_err(|error| error.to_string())?;
        if (
            metadata.dev(),
            metadata.ino(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
        ) != (
            authenticated.dev(),
            authenticated.ino(),
            authenticated.len(),
            authenticated.mtime(),
            authenticated.mtime_nsec(),
        ) || metadata.mode() & 0o7777 != mode & 0o7777
            || (metadata.uid(), metadata.gid()) != (access.uid, access.gid)
        {
            return Err("Recovery blob changed during permission restoration".into());
        }
        // Ownership, mode and ACL restoration change ctime. Normalize only
        // those known authenticated policy transitions;
        // the original expected-object digest is NEVER replaced by fresh data.
        let proof = Self {
            metadata,
            file: Some(file.clone()),
            identity_pin: file,
            digest: Some(digest),
            target: None,
            access: Some(access),
        };
        proof.verify(holding, name, cancellation)?;
        Ok(proof)
    }

    fn same_identity(&self, holding: &Holding, name: &Path) -> Result<(), String> {
        let named =
            fs_safety::metadata_beneath(holding.root(), name).map_err(|error| error.to_string())?;
        let pinned = self
            .identity_pin
            .metadata()
            .map_err(|error| error.to_string())?;
        if (named.dev(), named.ino(), named.file_type())
            != (
                self.metadata.dev(),
                self.metadata.ino(),
                self.metadata.file_type(),
            )
            || (pinned.dev(), pinned.ino(), pinned.file_type())
                != (
                    self.metadata.dev(),
                    self.metadata.ino(),
                    self.metadata.file_type(),
                )
        {
            return Err(
                "The acquired Undo entry was replaced; its replacement was retained".into(),
            );
        }
        Ok(())
    }

    fn verify(
        &self,
        holding: &Holding,
        name: &Path,
        cancellation: &CancellationToken,
    ) -> Result<(), String> {
        holding.verify_private()?;
        self.same_identity(holding, name)?;
        let named =
            fs_safety::metadata_beneath(holding.root(), name).map_err(|error| error.to_string())?;
        if full_version(&named) != full_version(&self.metadata) {
            return Err("Undo entry changed after verification".into());
        }
        if let Some(file) = &self.file {
            self.access
                .as_ref()
                .ok_or("The original access policy is unavailable")?
                .verify(file)?;
            if full_version(&file.metadata().map_err(|error| error.to_string())?)
                != full_version(&self.metadata)
                || self.digest != Some(digest_file(file, self.metadata.len(), cancellation)?)
                || full_version(&file.metadata().map_err(|error| error.to_string())?)
                    != full_version(&self.metadata)
            {
                return Err("Undo file contents changed after verification".into());
            }
        }
        if let Some(target) = &self.target {
            if fs_safety::read_link_beneath(holding.root(), name)
                .map_err(|error| error.to_string())?
                != *target
            {
                return Err("Undo symlink changed after verification".into());
            }
        }
        holding.verify_private()
    }
}

impl<E: ProcessExecutor> RepositoryService<E> {
    pub(super) fn undo_saved_entry(
        &self,
        recovery: &DiscardRecovery,
        source: &WorktreeAuthority,
        entry: &DiscardRecoveryEntry,
        parent: &fs_safety::Root,
        name: &OsStr,
        cancellation: &CancellationToken,
        context: &FilesystemRecoveryContext,
    ) -> Result<Option<String>, String> {
        let mut holding = Holding::create(parent, recovery, cancellation, context)?;
        let result =
            self.publish_saved_entry(source, &mut holding, recovery, entry, name, cancellation);
        if !holding.publication.is_removed()
            || result.as_ref().is_ok_and(|warning| warning.is_some())
        {
            holding.retain(recovery);
        }
        result
    }

    fn publish_saved_entry(
        &self,
        source: &WorktreeAuthority,
        holding: &mut Holding,
        recovery: &DiscardRecovery,
        entry: &DiscardRecoveryEntry,
        name: &OsStr,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, String> {
        holding.validate()?;
        let materialized = match &entry.before {
            SavedEntry::Absent => None,
            SavedEntry::Symlink { target } => {
                let generated = PathBuf::from("restored-symlink");
                fs_safety::symlink_within(holding.root(), target, generated.as_os_str())
                    .map_err(|error| error.to_string())?;
                let proof = Proof::capture(holding, &generated, cancellation)?;
                if proof.target.as_ref() != Some(target) {
                    return Err("The restored symlink no longer matches the saved target".into());
                }
                Some((generated, proof))
            }
            SavedEntry::Blob {
                oid,
                mode,
                bytes,
                access,
            } => {
                let generated = PathBuf::from("restored-blob");
                let file = holding.create_private_blob()?;
                // The creator's exact FD survives every error, including a
                // failed/partial cat-file write. No returned Git name is trusted.
                holding.validate()?;
                self.stream_recovery_object(
                    source,
                    &holding.publication,
                    oid,
                    &file,
                    *bytes,
                    cancellation,
                )
                .map_err(|error| error.to_string())?;
                let before = file.metadata().map_err(|error| error.to_string())?;
                if !before.is_file() || before.len() != *bytes {
                    return Err("Recovery blob has an unexpected type or size".into());
                }
                let digest = digest_file(&file, before.len(), cancellation)?;
                let actual = self
                    .hash_recovery_object(source, &holding.publication, &file, cancellation)
                    .map_err(|error| error.to_string())?;
                if actual != *oid
                    || full_version(&before)
                        != full_version(&file.metadata().map_err(|error| error.to_string())?)
                {
                    return Err("Recovery blob does not match its saved object; the unknown entry was preserved".into());
                }
                let named = fs_safety::metadata_beneath(holding.root(), &generated)
                    .map_err(|error| error.to_string())?;
                if full_version(&named) != full_version(&before) {
                    return Err("Recovery temporary name changed; it was preserved".into());
                }
                let proof = Proof::after_blob_access_change(
                    holding,
                    &generated,
                    access.clone(),
                    file,
                    before,
                    digest,
                    *mode,
                    cancellation,
                    |_| Ok(()),
                )?;
                Some((generated, proof))
            }
            SavedEntry::Trashed => return Err("The Trash receipt is missing".into()),
            SavedEntry::Unavailable { reason } => {
                return Err(format!("The backup is unavailable: {reason}"));
            }
        };
        let current = match fs_safety::metadata_beneath(&holding.parent, Path::new(name)) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.is_not_found() => None,
            Err(error) => return Err(error.to_string()),
        };
        if current.as_ref().map(metadata_version) != entry.after_version {
            return Err(
                "The destination changed before Undo publication; newer work was preserved".into(),
            );
        }
        if cancellation.is_cancelled() {
            return Err("Undo cancelled before destination acquisition".into());
        }
        let previous = Path::new("previous");
        let mut acquired = None;
        let mut moved = false;
        let before_publication: Result<(), String> = (|| {
            if current.is_some() {
                holding.validate()?;
                fs_safety::rename_new_between(
                    &holding.parent,
                    name,
                    holding.root(),
                    previous.as_os_str(),
                )
                .map_err(|error| error.to_string())?;
                moved = true;
                let proof = Proof::capture(holding, previous, cancellation)?;
                acquired = Some(proof);
                let proof = acquired.as_ref().unwrap();
                let prior = current.as_ref().unwrap();
                if moved_entry_version(prior) != moved_entry_version(&proof.metadata)
                    || (prior.uid(), prior.gid()) != (proof.metadata.uid(), proof.metadata.gid())
                {
                    return Err("The actual acquired destination changed; it was preserved".into());
                }
                match (&entry.after, &proof.file, &proof.target) {
                    (
                        SavedEntry::Blob {
                            oid,
                            mode,
                            bytes,
                            access,
                        },
                        Some(file),
                        _,
                    ) => {
                        access.verify(file)?;
                        if proof.metadata.len() != *bytes
                            || proof.metadata.mode() & 0o7777 != *mode & 0o7777
                            || self
                                .hash_recovery_object(
                                    source,
                                    &holding.publication,
                                    file,
                                    cancellation,
                                )
                                .map_err(|error| error.to_string())?
                                != *oid
                        {
                            return Err("The acquired destination no longer matches its post-discard contents".into());
                        }
                    }
                    (SavedEntry::Symlink { target }, _, Some(actual)) if target == actual => {}
                    _ => {
                        return Err(
                            "The acquired destination has an unexpected type or contents".into(),
                        );
                    }
                }
                proof.verify(holding, previous, cancellation)?;
            }
            if let Some((generated, proof)) = &materialized {
                proof.verify(holding, generated, cancellation)?;
            }
            if cancellation.is_cancelled() {
                return Err("Undo cancelled before publication".into());
            }
            holding.validate()?;
            fs_safety::sync_directory(holding.root()).map_err(|error| error.to_string())?;
            if let Some((generated, _)) = &materialized {
                fs_safety::rename_new_between(
                    holding.root(),
                    generated.as_os_str(),
                    &holding.parent,
                    name,
                )
                .map_err(|error| error.to_string())?;
            }
            Ok(())
        })();
        if let Err(reason) = before_publication {
            let rollback = if !moved {
                Ok(())
            } else if let Some(proof) = &acquired {
                holding
                    .validate()
                    .and_then(|()| proof.same_identity(holding, previous))
                    .and_then(|()| {
                        fs_safety::rename_new_between(
                            holding.root(),
                            previous.as_os_str(),
                            &holding.parent,
                            name,
                        )
                        .map_err(|error| error.to_string())
                    })
            } else {
                Err("The acquired entry has no authenticated proof; it remains in the held directory".into())
            };
            return Err(match rollback {
                Ok(()) => format!(
                    "{reason}; no restored entry was published; acquired contents were put back without overwriting newer work"
                ),
                Err(error) => {
                    recovery.live_recovery.set_uncertain(true);
                    format!(
                        "{reason}; conditional putback remains unresolved: {error}; the live recovery receipt was retained"
                    )
                }
            });
        }
        // The publication (or intended absence) is known. Cleanup errors may
        // not become a NotApplied report or discard live recovery ownership.
        let cleanup = (|| {
            holding.validate()?;
            if let Some(proof) = &acquired {
                proof.verify(holding, previous, cancellation)?;
                fs_safety::unlink_within(holding.root(), previous.as_os_str())
                    .map_err(|error| error.to_string())?;
            }
            holding
                .publication
                .cleanup_empty()
                .map_err(|error| error.to_string())?;
            fs_safety::sync_directory(holding.context.placement().directory())
                .map_err(|error| error.to_string())?;
            fs_safety::sync_directory(&holding.parent).map_err(|error| error.to_string())
        })();
        Ok(cleanup.err().map(|error| {
            format!("Undo was applied, but cleanup or durability is unconfirmed: {error}")
        }))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, DiscardRecovery, Holding) {
        let directory = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(directory.path()).unwrap();
        let metadata = root.metadata().unwrap();
        let recovery = DiscardRecovery {
            worktree: directory.path().to_owned(),
            worktree_identity: (metadata.dev(), metadata.ino()),
            live_recovery: LiveDiscardRecovery::default(),
            directories: BTreeMap::new(),
            entries: vec![],
            expires_unix_ms: u64::MAX,
        };
        let holding = Holding::create(
            &root,
            &recovery,
            &CancellationToken::new(),
            &crate::filesystem_recovery_context::fixtures::fixture_context(&root).unwrap(),
        )
        .unwrap();
        (directory, recovery, holding)
    }

    #[test]
    fn undo_refuses_changed_private_permissions_before_payload_and_during_live_retry() {
        let (directory, recovery, mut holding) = fixture();
        fs::write(directory.path().join("original"), b"preserved").unwrap();
        let private = holding.root().path().to_owned();
        fs_safety::set_directory_permissions(holding.root(), 0o711).unwrap();
        assert!(holding.create_private_blob().is_err());
        assert_eq!(
            holding.root().read_dir().unwrap().count(),
            0,
            "no payload is created under a changed boundary"
        );
        assert_eq!(holding.root().metadata().unwrap().mode() & 0o7777, 0o711);
        holding.retain(&recovery);
        let authority = recovery.live_recovery.authority().unwrap();
        assert!(authority.retry().is_err());
        assert_eq!(
            fs::metadata(&private).unwrap().mode() & 0o7777,
            0o711,
            "retry never repairs newer permissions implicitly"
        );
        fs::set_permissions(&private, fs::Permissions::from_mode(0o700)).unwrap();
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(!private.exists());
        assert_eq!(
            fs::read(directory.path().join("original")).unwrap(),
            b"preserved"
        );
    }

    #[test]
    fn undo_factory_cancelled_admission_creates_no_holding_or_live_obligation() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(directory.path()).unwrap();
        let metadata = root.metadata().unwrap();
        let recovery = DiscardRecovery {
            worktree: directory.path().to_owned(),
            worktree_identity: (metadata.dev(), metadata.ino()),
            live_recovery: LiveDiscardRecovery::default(),
            directories: BTreeMap::new(),
            entries: vec![],
            expires_unix_ms: u64::MAX,
        };
        let context = crate::filesystem_recovery_context::fixtures::fixture_context(&root).unwrap();
        let cancellation = CancellationToken::new();
        cancellation.cancel();
        assert!(Holding::create(&root, &recovery, &cancellation, &context).is_err());
        assert!(
            context
                .placement()
                .directory()
                .read_dir()
                .unwrap()
                .next()
                .is_none()
        );
        assert!(!recovery.live_recovery.needs_resolution());
    }

    #[test]
    fn undo_factory_real_preparation_error_retains_same_receipt_and_cleanup_lease() {
        let directory = tempfile::tempdir().unwrap();
        let runtime = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(directory.path()).unwrap();
        let metadata = root.metadata().unwrap();
        let recovery = DiscardRecovery {
            worktree: directory.path().to_owned(),
            worktree_identity: (metadata.dev(), metadata.ino()),
            live_recovery: LiveDiscardRecovery::default(),
            directories: BTreeMap::new(),
            entries: vec![],
            expires_unix_ms: u64::MAX,
        };
        let protection =
            crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), false)
                .unwrap();
        let fixture = crate::filesystem_recovery_context::fixtures::fixture_context(&root).unwrap();
        let context = FilesystemRecoveryContext::new(
            &root,
            Arc::new(Some(protection)),
            fixture.placement().clone(),
        )
        .unwrap();
        drop(fixture);
        let outside = context.placement().directory().path().to_owned();
        let error = fs_safety::fixture_recovery_directory_preparation_failure(
            context.reserve_directory().unwrap(),
            root.try_clone().unwrap(),
        );
        let _message = remember_directory_error(&recovery, error);
        assert!(recovery.live_recovery.needs_resolution());
        drop(context);
        let service = RepositoryService::system("git");
        assert!(
            crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), true)
                .is_err()
        );
        assert!(
            service
                .resolve_discard_recovery(&recovery, &CancellationToken::new())
                .is_err()
        );
        assert!(
            crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), true)
                .is_err()
        );
        let staging = fs::read_dir(&outside)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(
            fs::read(staging.join("directory")).unwrap(),
            b"retained fixture artifact"
        );
        fs::remove_file(staging.join("directory")).unwrap();
        let result = service
            .resolve_discard_recovery(&recovery, &CancellationToken::new())
            .unwrap();
        assert_eq!(result.remaining_paths, 0);
        assert!(!recovery.live_recovery.needs_resolution());
        assert!(!staging.exists());
        drop(
            crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), true)
                .unwrap(),
        );
    }

    #[test]
    fn undo_holding_is_outside_source_and_refuses_actual_payload_relocation() {
        let (directory, recovery, holding) = fixture();
        let source_file = directory.path().join("current");
        fs::write(&source_file, b"newer source").unwrap();
        let original = holding.root().path().to_owned();
        assert!(!original.starts_with(directory.path()));
        let relocated = directory.path().join("moved-holding");
        fs::rename(&original, &relocated).unwrap();
        holding.context.placement().validate().unwrap();
        assert!(
            holding.validate().is_err(),
            "actual H ancestry, not just safe outside parent, must be fenced"
        );
        let private = holding.root().try_clone().unwrap();
        holding.retain(&recovery);
        let service = RepositoryService::system("git");
        assert!(
            service
                .resolve_discard_recovery(&recovery, &CancellationToken::new())
                .is_err()
        );
        assert_eq!(fs::read(&source_file).unwrap(), b"newer source");
        assert!(private.read_dir().unwrap().next().is_none());
        fs::rename(&relocated, &original).unwrap();
        service
            .resolve_discard_recovery(&recovery, &CancellationToken::new())
            .unwrap();
        assert!(!recovery.live_recovery.needs_resolution());
        assert_eq!(fs::read(&source_file).unwrap(), b"newer source");
    }

    #[test]
    fn undo_without_original_context_refuses_before_any_filesystem_effect() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("current"), b"newer source").unwrap();
        let root = fs_safety::Root::open(directory.path()).unwrap();
        let metadata = root.metadata().unwrap();
        let recovery = DiscardRecovery {
            worktree: directory.path().to_owned(),
            worktree_identity: (metadata.dev(), metadata.ino()),
            live_recovery: LiveDiscardRecovery::default(),
            directories: BTreeMap::new(),
            entries: vec![DiscardRecoveryEntry {
                path: RepoPath::parse("restored").unwrap(),
                before: SavedEntry::Symlink {
                    target: PathBuf::from("original-target"),
                },
                after: SavedEntry::Absent,
                after_version: None,
                trash: None,
            }],
            expires_unix_ms: u64::MAX,
        };
        // Exercise actual missing-context admission, not the earlier no-Undo
        // fast path for a receipt with no remaining entries.
        assert!(recovery.can_undo());
        let error = RepositoryService::system("git")
            .undo_discard(&recovery, &CancellationToken::new())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("original protected outside-checkout recovery context")
        );
        assert_eq!(
            fs::read(directory.path().join("current")).unwrap(),
            b"newer source"
        );
        assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
    }

    #[test]
    fn undo_context_does_not_recapture_missing_git_metadata_authority() {
        let directory = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(directory.path()).unwrap();
        let metadata = root.metadata().unwrap();
        let context = crate::filesystem_recovery_context::fixtures::fixture_context(&root).unwrap();
        let outside = context.placement().directory().path().to_owned();
        let recovery = DiscardRecovery {
            worktree: directory.path().to_owned(),
            worktree_identity: (metadata.dev(), metadata.ino()),
            live_recovery: LiveDiscardRecovery::default(),
            directories: BTreeMap::new(),
            entries: vec![DiscardRecoveryEntry {
                path: RepoPath::parse("nested/file").unwrap(),
                before: SavedEntry::Absent,
                after: SavedEntry::Absent,
                after_version: None,
                trash: None,
            }],
            expires_unix_ms: u64::MAX,
        };
        let error = RepositoryService::system("git")
            .with_recovery_context(context)
            .undo_discard(&recovery, &CancellationToken::new())
            .unwrap_err();
        assert!(
            error
                .to_string()
                .contains("original retained Git metadata authority")
        );
        assert!(fs::read_dir(directory.path()).unwrap().next().is_none());
        // The service has dropped its context, whose test lease may remove the
        // empty outside fixture directory; no holding/payload was created.
        assert!(!outside.exists() || fs::read_dir(outside).unwrap().next().is_none());
    }

    #[test]
    fn undo_pending_holding_keeps_exact_cleanup_lease_until_explicit_resolution() {
        let (directory, recovery, holding) = fixture();
        holding.publication.directory().metadata().unwrap();
        // Retire the fixture's empty holding before acquiring a real protected
        // context; every new effect below uses that context's reservation.
        let mut original = holding;
        original.publication.cleanup_empty().unwrap();
        drop(original);
        let runtime = tempfile::tempdir().unwrap();
        let root = fs_safety::Root::open(directory.path()).unwrap();
        let protection =
            crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), false)
                .unwrap();
        assert!(protection.protects_root(&root).unwrap());
        let fixture = crate::filesystem_recovery_context::fixtures::fixture_context(&root).unwrap();
        let context = FilesystemRecoveryContext::new(
            &root,
            Arc::new(Some(protection)),
            fixture.placement().clone(),
        )
        .unwrap();
        drop(fixture);
        let holding =
            Holding::create(&root, &recovery, &CancellationToken::new(), &context).unwrap();
        let private = holding.root().try_clone().unwrap();
        fs::write(private.path().join("unresolved"), b"preserve").unwrap();
        holding.retain(&recovery);
        drop(context);
        assert!(
            crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), true)
                .is_err()
        );
        let service = RepositoryService::system("git");
        assert!(
            service
                .resolve_discard_recovery(&recovery, &CancellationToken::new())
                .is_err()
        );
        assert!(
            crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), true)
                .is_err()
        );
        fs_safety::unlink_within(&private, OsStr::new("unresolved")).unwrap();
        service
            .resolve_discard_recovery(&recovery, &CancellationToken::new())
            .unwrap();
        assert!(!recovery.live_recovery.needs_resolution());
        let exclusive =
            crate::worktree_cleanup::fixture_protection(runtime.path(), directory.path(), true)
                .unwrap();
        drop(exclusive);
    }

    fn named_access_acl(named_uid: u32) -> Vec<u8> {
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in [
            (0x01u16, 6u16, u32::MAX),
            (0x02, 4, named_uid),
            (0x04, 0, u32::MAX),
            (0x10, 4, u32::MAX),
            (0x20, 0, u32::MAX),
        ] {
            acl.extend_from_slice(&tag.to_le_bytes());
            acl.extend_from_slice(&permissions.to_le_bytes());
            acl.extend_from_slice(&id.to_le_bytes());
        }
        acl
    }

    #[test]
    fn undo_blob_preserves_original_acl_and_removes_inherited_named_access() {
        for original_has_acl in [false, true] {
            let (directory, _recovery, mut holding) = fixture();
            fs::write(directory.path().join("original"), b"work").unwrap();
            let original = fs::File::open(directory.path().join("original")).unwrap();
            original
                .set_permissions(fs::Permissions::from_mode(0o640))
                .unwrap();
            let original_acl = named_access_acl(12_345);
            fs_safety::restore_access_acl(
                &original,
                original_has_acl.then_some(original_acl.as_slice()),
            )
            .unwrap();
            let access = SavedFileAccess::capture(&original).unwrap();
            assert_eq!(access.acl.is_some(), original_has_acl);
            // A default ACL in the outside staging parent can carry another
            // named reader even while a fresh0600 child masks it out. chmod640
            // would activate that reader unless original absence is restored.
            let parent = open_directory_sync_handle(holding.root()).unwrap();
            let inherited = named_access_acl(23_456);
            let result = unsafe {
                libc::fsetxattr(
                    parent.as_raw_fd(),
                    c"system.posix_acl_default".as_ptr(),
                    inherited.as_ptr().cast(),
                    inherited.len(),
                    0,
                )
            };
            assert_eq!(result, 0, "{}", io::Error::last_os_error());
            let file = holding.create_private_blob().unwrap();
            file.write_at(b"work", 0).unwrap();
            assert!(fs_safety::capture_access_acl(&file).unwrap().is_some());
            let before = file.metadata().unwrap();
            let digest = digest_file(&file, before.len(), &CancellationToken::new()).unwrap();
            let proof = Proof::after_blob_access_change(
                &holding,
                Path::new("restored-blob"),
                access.clone(),
                file.clone(),
                before,
                digest,
                0o640,
                &CancellationToken::new(),
                |_| Ok(()),
            )
            .unwrap();
            proof
                .verify(
                    &holding,
                    Path::new("restored-blob"),
                    &CancellationToken::new(),
                )
                .unwrap();
            assert_eq!(
                fs_safety::capture_access_acl(&file).unwrap().as_deref(),
                access.acl.as_deref()
            );
            assert_eq!(file.metadata().unwrap().mode() & 0o7777, 0o640);
            assert_eq!(
                (
                    file.metadata().unwrap().uid(),
                    file.metadata().unwrap().gid()
                ),
                (access.uid, access.gid)
            );
        }
    }

    #[test]
    fn undo_blob_original_policy_refuses_after_restoration_acl_replacement() {
        let (_directory, _recovery, mut holding) = fixture();
        let file = holding.create_private_blob().unwrap();
        file.write_at(b"work", 0).unwrap();
        let access = SavedFileAccess::capture(&file).unwrap();
        assert!(access.acl.is_none());
        let before = file.metadata().unwrap();
        let digest = digest_file(&file, before.len(), &CancellationToken::new()).unwrap();
        let result = Proof::after_blob_access_change(
            &holding,
            Path::new("restored-blob"),
            access,
            file,
            before,
            digest,
            0o640,
            &CancellationToken::new(),
            |file| {
                fs_safety::restore_access_acl(file, Some(&named_access_acl(12_345)))
                    .map_err(|error| error.to_string())
            },
        );
        assert!(
            result.is_err(),
            "a fresh post-restoration ACL must not replace the archived policy"
        );
    }

    #[test]
    fn undo_blob_preserves_recorded_group_or_refuses_unavailable_ownership() {
        let (directory, recovery, mut holding) = fixture();
        fs::write(directory.path().join("original"), b"work").unwrap();
        let original = fs::File::open(directory.path().join("original")).unwrap();
        original
            .set_permissions(fs::Permissions::from_mode(0o640))
            .unwrap();
        let uid = unsafe { libc::geteuid() };
        let primary = unsafe { libc::getegid() };
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        assert!(count >= 0);
        let mut groups = vec![0; count as usize];
        assert_eq!(
            unsafe { libc::getgroups(count, groups.as_mut_ptr()) },
            count
        );
        let alternate = groups
            .into_iter()
            .find(|gid| *gid != primary)
            .or_else(|| (uid == 0).then_some(if primary == 23_456 { 23_457 } else { 23_456 }));
        // Root UID is not proof of CAP_CHOWN (e.g. a user namespace or a
        // capability-dropped runner). Require the actual positive native step.
        let alternate =
            alternate.filter(|gid| fs_safety::restore_ownership(&original, uid, *gid).is_ok());
        if alternate.is_none() {
            // No restorable secondary group is available in this runner.
            // Exercise actual post-fchown verification refusal instead and
            // never report this branch as cross-GID restoration coverage.
            eprintln!(
                "NO_RESTORABLE_SECONDARY_GID: testing unavailable ownership refusal, not cross-GID restoration"
            );
        }
        let mut access = SavedFileAccess::capture(&original).unwrap();
        let file = holding.create_private_blob().unwrap();
        file.write_at(b"work", 0).unwrap();
        let before = file.metadata().unwrap();
        let digest = digest_file(&file, before.len(), &CancellationToken::new()).unwrap();
        // uid_t::MAX is fchown's no-change sentinel, never an installable
        // owner. The helper must reject its actual post-syscall mismatch even
        // when the runner has CAP_CHOWN; no euid-only capability assumption.
        if alternate.is_none() {
            access.uid = u32::MAX;
        }
        let result = Proof::after_blob_access_change(
            &holding,
            Path::new("restored-blob"),
            access.clone(),
            file.clone(),
            before,
            digest,
            0o640,
            &CancellationToken::new(),
            |_| Ok(()),
        );
        if alternate.is_some() {
            result.unwrap();
            assert_ne!(access.gid, primary);
            assert_eq!(file.metadata().unwrap().gid(), access.gid);
            assert_eq!(file.metadata().unwrap().mode() & 0o7777, 0o640);
        } else {
            assert!(result.is_err());
            let expected = file.metadata().unwrap();
            holding.retain(&recovery);
            assert!(recovery.live_recovery.needs_resolution());
            assert_eq!(file.metadata().unwrap().ino(), expected.ino());
            assert_eq!(
                fs::read(directory.path().join("original")).unwrap(),
                b"work"
            );
        }
    }

    #[test]
    fn undo_blob_authenticated_proof_refuses_existing_fd_write_during_own_chmod_window() {
        let (_directory, _recovery, mut holding) = fixture();
        let path = Path::new("generated");
        fs::write(holding.root().path().join(path), b"work").unwrap();
        let file = holding.open(path).unwrap();
        let metadata = file.metadata().unwrap();
        let modified = metadata.modified().unwrap();
        let digest = digest_file(&file, metadata.len(), &CancellationToken::new()).unwrap();
        // This writer already owns its FD before the permission transition;
        // private namespace trust does not exclude its subsequent content write.
        let writer = fs::OpenOptions::new()
            .write(true)
            .open(holding.root().path().join(path))
            .unwrap();
        let result = Proof::after_blob_access_change(
            &holding,
            path,
            SavedFileAccess::capture(&file).unwrap(),
            file,
            metadata,
            digest,
            0o640,
            &CancellationToken::new(),
            |_| {
                assert_eq!(writer.write_at(b"EVIL", 0).unwrap(), 4);
                writer
                    .set_times(fs::FileTimes::new().set_modified(modified))
                    .map_err(|error| error.to_string())
            },
        );
        assert!(
            result.is_err(),
            "the original authenticated digest must survive our own ctime change"
        );
        assert_eq!(fs::read(holding.root().path().join(path)).unwrap(), b"EVIL");
    }

    #[test]
    fn undo_blob_authenticated_proof_never_adopts_a_fresh_named_replacement() {
        let (_directory, _recovery, mut holding) = fixture();
        let path = Path::new("generated");
        fs::write(holding.root().path().join(path), b"work").unwrap();
        let file = holding.open(path).unwrap();
        let original = file.clone();
        let metadata = file.metadata().unwrap();
        let digest = digest_file(&file, metadata.len(), &CancellationToken::new()).unwrap();
        let result = Proof::after_blob_access_change(
            &holding,
            path,
            SavedFileAccess::capture(&file).unwrap(),
            file,
            metadata,
            digest,
            0o640,
            &CancellationToken::new(),
            |_| {
                fs::rename(
                    holding.root().path().join(path),
                    holding.root().path().join("original"),
                )
                .unwrap();
                fs::write(holding.root().path().join(path), b"EVIL")
                    .map_err(|error| error.to_string())
            },
        );
        assert!(result.is_err());
        let mut bytes = [0; 4];
        original.read_at(&mut bytes, 0).unwrap();
        assert_eq!(&bytes, b"work");
        assert_eq!(fs::read(holding.root().path().join(path)).unwrap(), b"EVIL");
    }

    #[test]
    fn undo_symlink_proof_pins_unlinked_metadata_instead_of_naked_inode_numbers() {
        let (_directory, _recovery, mut holding) = fixture();
        let path = Path::new("link");
        fs_safety::symlink_within(holding.root(), Path::new("saved-target"), path.as_os_str())
            .unwrap();
        let proof = Proof::capture(&mut holding, path, &CancellationToken::new()).unwrap();
        assert!(
            proof
                .identity_pin
                .metadata()
                .unwrap()
                .file_type()
                .is_symlink()
        );
        fs_safety::unlink_within(holding.root(), path.as_os_str()).unwrap();
        let unlinked = proof.identity_pin.metadata().unwrap();
        assert_eq!(unlinked.nlink(), 0);
        assert_eq!(
            (unlinked.dev(), unlinked.ino()),
            (proof.metadata.dev(), proof.metadata.ino())
        );
        fs_safety::symlink_within(
            holding.root(),
            Path::new("foreign-target"),
            path.as_os_str(),
        )
        .unwrap();
        assert!(proof.same_identity(&holding, path).is_err());
        assert_eq!(
            fs_safety::read_link_beneath(holding.root(), path).unwrap(),
            Path::new("foreign-target")
        );
    }
}
