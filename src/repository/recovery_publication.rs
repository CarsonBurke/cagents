//! Private materialization and conditional publication of saved Undo entries.
//!
//! The protected internal namespace contract belongs to DirectoryPublication.
//! Pathname acquisition does not exclude writes through an already-open FD
//! after the final content fence. Unknown entries are retained, never removed
//! using a diagnostic spelling or a guessed Git temporary filename.
use super::*;
use crate::recovery_authority::RecoverySlot;
use std::os::unix::fs::FileExt;

struct Holding {
    publication: fs_safety::DirectoryPublication,
    root: fs_safety::Root,
    parent: fs_safety::Root,
    slot: Option<RecoverySlot>,
    files: Vec<Arc<fs::File>>,
}

impl Holding {
    fn create(parent: &fs_safety::Root, recovery: &DiscardRecovery) -> Result<Self, String> {
        let slot = RecoverySlot::reserve()?;
        let held_parent = parent.try_clone().map_err(|error| error.to_string())?;
        let name = PathBuf::from(format!(".cagents-undo-{}", Uuid::new_v4()));
        let mut publication = match fs_safety::create_directory_beneath(parent, &name, 0o700)
            .map_err(|error| error.to_string())? {
            fs_safety::DirectoryCreation::Published(publication) => publication,
            fs_safety::DirectoryCreation::Existing(_) => return Err("The private Undo holding name already exists; it was preserved".into()),
            fs_safety::DirectoryCreation::Unverified { directory, reason } => {
                let description = format!("Unverified Undo holding originally named {}", directory.path().display());
                recovery.live_recovery.retain(slot.retain(description, move || {
                    if directory.metadata().map_err(|error|error.to_string())?.nlink() == 0 { Ok(()) }
                    else { Err("The unverified Undo holding was preserved; resolve it before retrying".into()) }
                }));
                return Err(format!("Private Undo creation ownership is unverified; no saved entry was published: {reason}"));
            }
        };
        let checked = (|| {
            set_exact_directory_mode(publication.directory(), 0o700).map_err(|error|error.to_string())?;
            let metadata = publication.directory().metadata().map_err(|error|error.to_string())?;
            if metadata.uid() != unsafe { libc::geteuid() } { return Err("Undo holding ownership changed".into()); }
            if let Some(error) = publication.publication_error() { return Err(error.to_owned()); }
            publication.directory().try_clone().map_err(|error|error.to_string())
        })();
        let root = match checked {
            Ok(root) => root,
            Err(error) => {
                let description = format!("Undo holding originally named {}", publication.directory().path().display());
                recovery.live_recovery.retain(slot.retain(description, move || {
                    publication.resolve_recovery().map_err(|error|error.to_string())?;
                    publication.cleanup_empty().map_err(|error|error.to_string())?;
                    fs_safety::sync_directory(&held_parent).map_err(|error|error.to_string())
                }));
                return Err(error);
            }
        };
        Ok(Self { publication, root, parent: held_parent, slot: Some(slot), files: vec![] })
    }

    fn open(&mut self, name: &Path) -> Result<Arc<fs::File>, String> {
        let file = Arc::new(fs_safety::open_beneath(&self.root, name).map_err(|error|error.to_string())?);
        // Arc clones do not allocate another FD. Keep the actual opened inode
        // before any verification can fail, including a wrong Git output name.
        self.files.push(file.clone());
        Ok(file)
    }

    fn retain(mut self, recovery: &DiscardRecovery) {
        let description = format!("Undo recovery holding originally named {}", self.root.path().display());
        let slot = self.slot.take().expect("reserved before Undo filesystem effects");
        // Whole receiver capture retains root, files, parent and publication.
        recovery.live_recovery.retain(slot.retain(description, move || self.retry()));
    }

    fn retry(&mut self) -> Result<(), String> {
        self.publication.resolve_recovery().map_err(|error|error.to_string())?;
        // Manual resolution may move preserved contents out. Only the exact
        // empty original directory can then be removed; unknown leaf entries
        // and newer destinations are never destroyed by a cleanup retry.
        self.publication.cleanup_empty().map_err(|error|error.to_string())?;
        fs_safety::sync_directory(&self.parent).map_err(|error|error.to_string())
    }
}

fn full_version(metadata: &fs::Metadata) -> ((u64,u64,u64,i64,i64,i64,i64),u32,u32,u32) {
    (metadata_version(metadata), metadata.mode(), metadata.uid(), metadata.gid())
}

fn digest_file(file: &fs::File, length: u64, cancellation: &CancellationToken) -> Result<[u8;32], String> {
    use sha2::Digest;
    let mut hash = sha2::Sha256::new();
    let mut offset = 0;
    let mut buffer = [0; 64 * 1024];
    while offset < length {
        if cancellation.is_cancelled() { return Err("Undo cancelled during content verification".into()); }
        let limit = (length - offset).min(buffer.len() as u64) as usize;
        let count = file.read_at(&mut buffer[..limit], offset).map_err(|error|error.to_string())?;
        if count == 0 { return Err("Undo content changed while being verified".into()); }
        hash.update(&buffer[..count]);
        offset += count as u64;
    }
    if file.read_at(&mut buffer[..1], offset).map_err(|error|error.to_string())? != 0 {
        return Err("Undo content grew while being verified".into());
    }
    Ok(hash.finalize().into())
}

struct Proof {
    metadata: fs::Metadata,
    file: Option<Arc<fs::File>>,
    digest: Option<[u8;32]>,
    target: Option<PathBuf>,
}

impl Proof {
    fn capture(holding: &mut Holding, name: &Path, cancellation: &CancellationToken) -> Result<Self, String> {
        let metadata = fs_safety::metadata_beneath(&holding.root, name).map_err(|error|error.to_string())?;
        let (file, digest, target) = if metadata.is_file() {
            let file = holding.open(name)?;
            if full_version(&metadata) != full_version(&file.metadata().map_err(|error|error.to_string())?) {
                return Err("Undo entry changed while being opened".into());
            }
            let digest = digest_file(&file, metadata.len(), cancellation)?;
            (Some(file), Some(digest), None)
        } else if metadata.file_type().is_symlink() {
            let target = fs_safety::read_link_beneath(&holding.root, name).map_err(|error|error.to_string())?;
            (None, None, Some(target))
        } else { return Err("Undo entry is not a regular file or symlink".into()); };
        let proof = Self { metadata, file, digest, target };
        proof.verify(holding, name, cancellation)?;
        Ok(proof)
    }

    fn same_identity(&self, holding: &Holding, name: &Path) -> Result<(), String> {
        let named = fs_safety::metadata_beneath(&holding.root, name).map_err(|error|error.to_string())?;
        if (named.dev(), named.ino(), named.file_type()) != (self.metadata.dev(), self.metadata.ino(), self.metadata.file_type()) {
            return Err("The acquired Undo entry was replaced; its replacement was retained".into());
        }
        Ok(())
    }

    fn verify(&self, holding: &Holding, name: &Path, cancellation: &CancellationToken) -> Result<(), String> {
        self.same_identity(holding, name)?;
        let named = fs_safety::metadata_beneath(&holding.root, name).map_err(|error|error.to_string())?;
        if full_version(&named) != full_version(&self.metadata) { return Err("Undo entry changed after verification".into()); }
        if let Some(file) = &self.file {
            if full_version(&file.metadata().map_err(|error|error.to_string())?) != full_version(&self.metadata)
                || self.digest != Some(digest_file(file, self.metadata.len(), cancellation)?)
                || full_version(&file.metadata().map_err(|error|error.to_string())?) != full_version(&self.metadata)
            { return Err("Undo file contents changed after verification".into()); }
        }
        if let Some(target) = &self.target {
            if fs_safety::read_link_beneath(&holding.root, name).map_err(|error|error.to_string())? != *target {
                return Err("Undo symlink changed after verification".into());
            }
        }
        Ok(())
    }
}

impl<E: ProcessExecutor> RepositoryService<E> {
    pub(super) fn undo_saved_entry(
        &self, recovery: &DiscardRecovery, root: &fs_safety::Root,
        entry: &DiscardRecoveryEntry, parent: &fs_safety::Root, name: &OsStr,
        cancellation: &CancellationToken,
    ) -> Result<Option<String>, String> {
        let source = self.recovery_source_authority(&recovery.worktree, root).map_err(|error|error.to_string())?;
        let mut holding = Holding::create(parent, recovery)?;
        let result = self.publish_saved_entry(&source, &mut holding, recovery, entry, name, cancellation);
        if !holding.publication.is_removed() { holding.retain(recovery); }
        result
    }

    fn publish_saved_entry(
        &self, source: &WorktreeAuthority, holding: &mut Holding, recovery: &DiscardRecovery,
        entry: &DiscardRecoveryEntry, name: &OsStr, cancellation: &CancellationToken,
    ) -> Result<Option<String>, String> {
        let materialized = match &entry.before {
            SavedEntry::Absent => None,
            SavedEntry::Symlink { target } => {
                let generated = PathBuf::from("restored-symlink");
                fs_safety::symlink_within(&holding.root, target, generated.as_os_str()).map_err(|error|error.to_string())?;
                let proof = Proof::capture(holding, &generated, cancellation)?;
                Some((generated, proof))
            }
            SavedEntry::Blob { oid, mode, bytes } => {
                let output = self.unpack_recovery_object(source, &holding.publication, oid, cancellation).map_err(|error|error.to_string())?;
                let bytes_name = output.stdout.strip_suffix(b"\n").unwrap_or(&output.stdout);
                let generated = RepoPath::parse(Path::new(OsStr::from_bytes(bytes_name))).map_err(|error|error.to_string())?;
                if generated.as_path().components().count() != 1 || !bytes_name.starts_with(b".merge_file_") {
                    return Err("Git returned an invalid recovery temporary name; unknown entries were preserved".into());
                }
                let file = holding.open(generated.as_path())?;
                let before = file.metadata().map_err(|error|error.to_string())?;
                if !before.is_file() || before.len() != *bytes { return Err("Recovery blob has an unexpected type or size".into()); }
                let actual = self.hash_recovery_object(source, &holding.publication, &file, cancellation).map_err(|error|error.to_string())?;
                if actual != *oid || full_version(&before) != full_version(&file.metadata().map_err(|error|error.to_string())?) {
                    return Err("Recovery blob does not match its saved object; the unknown entry was preserved".into());
                }
                let named = fs_safety::metadata_beneath(&holding.root, generated.as_path()).map_err(|error|error.to_string())?;
                if full_version(&named) != full_version(&before) { return Err("Recovery temporary name changed; it was preserved".into()); }
                file.set_permissions(fs::Permissions::from_mode(*mode)).map_err(|error|error.to_string())?;
                if file.metadata().map_err(|error|error.to_string())?.mode() & 0o7777 != *mode & 0o7777 { return Err("Recovery file permissions could not be restored exactly".into()); }
                file.sync_all().map_err(|error|error.to_string())?;
                let generated = generated.as_path().to_owned();
                let proof = Proof::capture(holding, &generated, cancellation)?;
                Some((generated, proof))
            }
            SavedEntry::Trashed => return Err("The Trash receipt is missing".into()),
            SavedEntry::Unavailable { reason } => return Err(format!("The backup is unavailable: {reason}")),
        };
        let current = match fs_safety::metadata_beneath(&holding.parent, Path::new(name)) {
            Ok(metadata) => Some(metadata),
            Err(error) if error.is_not_found() => None,
            Err(error) => return Err(error.to_string()),
        };
        if current.as_ref().map(metadata_version) != entry.after_version {
            return Err("The destination changed before Undo publication; newer work was preserved".into());
        }
        if cancellation.is_cancelled() { return Err("Undo cancelled before destination acquisition".into()); }
        let previous = Path::new("previous");
        let mut acquired = None;
        let mut moved = false;
        let before_publication = (|| {
            if current.is_some() {
                fs_safety::rename_new_between(&holding.parent, name, &holding.root, previous.as_os_str()).map_err(|error|error.to_string())?;
                moved = true;
                let proof = Proof::capture(holding, previous, cancellation)?;
                acquired = Some(proof);
                let proof = acquired.as_ref().unwrap();
                let prior = current.as_ref().unwrap();
                if moved_entry_version(prior) != moved_entry_version(&proof.metadata)
                    || (prior.uid(),prior.gid()) != (proof.metadata.uid(),proof.metadata.gid())
                { return Err("The actual acquired destination changed; it was preserved".into()); }
                match (&entry.after, &proof.file, &proof.target) {
                    (SavedEntry::Blob { oid, mode, bytes }, Some(file), _) => {
                        if proof.metadata.len() != *bytes || proof.metadata.mode() & 0o7777 != *mode & 0o7777
                            || self.hash_recovery_object(source, &holding.publication, file, cancellation).map_err(|error|error.to_string())? != *oid
                        { return Err("The acquired destination no longer matches its post-discard contents".into()); }
                    }
                    (SavedEntry::Symlink { target }, _, Some(actual)) if target == actual => {}
                    _ => return Err("The acquired destination has an unexpected type or contents".into()),
                }
                proof.verify(holding, previous, cancellation)?;
            }
            if let Some((generated, proof)) = &materialized { proof.verify(holding, generated, cancellation)?; }
            if cancellation.is_cancelled() { return Err("Undo cancelled before publication".into()); }
            fs_safety::sync_directory(&holding.root).map_err(|error|error.to_string())?;
            if let Some((generated, _)) = &materialized {
                fs_safety::rename_new_between(&holding.root, generated.as_os_str(), &holding.parent, name).map_err(|error|error.to_string())?;
            }
            Ok(())
        })();
        if let Err(reason) = before_publication {
            let rollback = if !moved { Ok(()) } else if let Some(proof) = &acquired {
                proof.same_identity(holding, previous).and_then(|()| {
                    fs_safety::rename_new_between(&holding.root, previous.as_os_str(), &holding.parent, name).map_err(|error|error.to_string())
                })
            } else { Err("The acquired entry has no authenticated proof; it remains in the held directory".into()) };
            return Err(match rollback {
                Ok(()) => format!("{reason}; no restored entry was published; acquired contents were put back without overwriting newer work"),
                Err(error) => {
                    recovery.live_recovery.set_uncertain(true);
                    format!("{reason}; conditional putback remains unresolved: {error}; the live recovery receipt was retained")
                },
            });
        }
        // The publication (or intended absence) is known. Cleanup errors may
        // not become a NotApplied report or discard live recovery ownership.
        let cleanup = (|| {
            if let Some(proof) = &acquired {
                proof.verify(holding, previous, cancellation)?;
                fs_safety::unlink_within(&holding.root, previous.as_os_str()).map_err(|error|error.to_string())?;
            }
            holding.publication.cleanup_empty().map_err(|error|error.to_string())?;
            fs_safety::sync_directory(&holding.parent).map_err(|error|error.to_string())
        })();
        Ok(cleanup.err().map(|error| format!("Undo was applied, but cleanup or durability is unconfirmed: {error}")))
    }
}
