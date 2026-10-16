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
use crate::{filesystem_recovery_context::FilesystemRecoveryContext, recovery_authority::{RecoveryAuthority, RecoverySlot}};

#[derive(Debug, Clone, thiserror::Error)]
pub enum RetainedSaveError {
    #[error("{0}")]
    Io(String),
    #[error("{message}")]
    Recovery { message: String, authority: RecoveryAuthority },
}
impl RetainedSaveError {
    pub fn recovery_authority(&self) -> Option<&RecoveryAuthority> {
        match self { Self::Recovery { authority, .. } => Some(authority), Self::Io(_) => None }
    }
}
type FileIoError = RetainedSaveError;

#[derive(Debug, Clone)]
enum SaveDurability {
    Durable,
    Unconfirmed { error: String },
    Recovery { error: String, authority: RecoveryAuthority },
}

/// Keep the actual cleanup authority until the window has accepted it, even
/// when the logical document or request is already stale.
#[derive(Debug, Clone)]
pub struct RetainedSaveOutcome {
    outcome: SaveOutcome,
    recovery: Option<RecoveryAuthority>,
}
impl RetainedSaveOutcome {
    pub fn outcome(&self) -> &SaveOutcome { &self.outcome }
    pub fn recovery_authority(&self) -> Option<&RecoveryAuthority> { self.recovery.as_ref() }
    pub(crate) fn into_parts(self) -> (SaveOutcome, Option<RecoveryAuthority>) { (self.outcome, self.recovery) }
}
fn io_error(error: impl std::fmt::Display) -> FileIoError { FileIoError::Io(error.to_string()) }
fn resolve_error(error: ResolveError) -> FileIoError { io_error(error) }

/// The blocking worker supplies the ORIGINAL read root and a real local
/// outside-checkout context. No path recapture or unprotected fallback exists.
pub(crate) fn save_file_at(
    root: &Root,
    relative_path: &Path,
    text: &str,
    expected_disk_hash: blake3::Hash,
    overwrite: bool,
    context: &FilesystemRecoveryContext,
) -> Result<RetainedSaveOutcome, RetainedSaveError> {
    save_file_with_hooks(root, relative_path, text, expected_disk_hash, overwrite, context, || Ok(()), fs_safety::sync_directory)
}
fn save_file_with_hooks(
    root: &Root,
    relative_path: &Path,
    text: &str,
    expected_disk_hash: blake3::Hash,
    overwrite: bool,
    context: &FilesystemRecoveryContext,
    before_publish: impl FnOnce() -> io::Result<()>,
    sync_parent: impl FnOnce(&Root) -> io::Result<()>,
) -> Result<RetainedSaveOutcome, RetainedSaveError> {
    save_file_with_read_hook(root, relative_path, text, expected_disk_hash, overwrite, context, || Ok(()), before_publish, sync_parent)
}
fn save_file_with_read_hook(
    root: &Root,
    relative_path: &Path,
    text: &str,
    expected_disk_hash: blake3::Hash,
    overwrite: bool,
    context: &FilesystemRecoveryContext,
    after_read: impl FnOnce() -> io::Result<()>,
    before_publish: impl FnOnce() -> io::Result<()>,
    sync_parent: impl FnOnce(&Root) -> io::Result<()>,
) -> Result<RetainedSaveOutcome, RetainedSaveError> {
    validate_repo_path(relative_path).map_err(io_error)?;
    context.validate_for_source(root).map_err(io_error)?;
    let named = Root::open(root.path()).map_err(io_error)?;
    context.validate_for_source(&named).map_err(io_error)?;
    drop(named);
    if text.len() as u64 > MAX_EDITABLE_FILE_BYTES { return Err(io_error("The edited file exceeds the save size limit")); }
    let (parent, name) = fs_safety::open_parent_beneath(root, relative_path).map_err(resolve_error)?;
    let name = Path::new(&name);
    let listed = fs_safety::metadata_beneath(&parent, name).map_err(resolve_error)?;
    if !listed.is_file() { return Err(io_error("The save target is no longer a regular file")); }
    let mut destination = fs_safety::open_beneath(&parent, name).map_err(resolve_error)?;
    let metadata = destination.metadata().map_err(io_error)?;
    if !same_file(&listed, &metadata) { return Err(changed()); }
    if !overwrite {
        let bytes = fs_safety::read_bounded(&mut destination, MAX_EDITABLE_FILE_BYTES).map_err(io_error)?;
        let disk_hash = blake3::hash(&bytes);
        if disk_hash != expected_disk_hash {
            return Ok(RetainedSaveOutcome { outcome: SaveOutcome::Conflict { disk_hash }, recovery: None });
        }
    }
    after_read().map_err(io_error)?;
    let baseline = SaveBaseline::capture(&destination, (!overwrite).then_some(expected_disk_hash))?;
    // The policy captured before hashing must still be the coherent baseline;
    // otherwise an intervening chmod could be accepted and then overwritten.
    baseline.ensure_initial(&metadata)?;
    validate_target_parent(root, relative_path, &parent).map_err(io_error)?;
    let durability = publish_with_hooks(&parent, name, &destination, text.as_bytes(), metadata.permissions(), &baseline, context, || {
        before_publish()?;
        let named = Root::open(root.path())?;
        context.validate_for_source(&named)?;
        validate_target_parent(root, relative_path, &parent)
    }, |parent, _| validate_target_parent(root, relative_path, parent), |_, _| Ok(()), sync_parent)?;
    let (durability, recovery) = match durability {
        SaveDurability::Durable => (super::SaveDurability::Durable, None),
        SaveDurability::Unconfirmed { error } => (super::SaveDurability::Unconfirmed { error }, None),
        SaveDurability::Recovery { error, authority } => (super::SaveDurability::Unconfirmed { error }, Some(authority)),
    };
    Ok(RetainedSaveOutcome {
        outcome: SaveOutcome::Saved { content_hash: blake3::hash(text.as_bytes()), durability },
        recovery,
    })
}

fn validate_target_parent(root: &Root, relative: &Path, parent: &Root) -> io::Result<()> {
    let (named_parent, _) = fs_safety::open_parent_beneath(root, relative)
        .map_err(|error| io::Error::other(error.to_string()))?;
    if fs_safety::recovery_directory_stamp(&named_parent)? != fs_safety::recovery_directory_stamp(parent)? {
        return Err(io::Error::other("The original save parent moved outside the selected path; no replacement was published"));
    }
    Ok(())
}

use std::{
    collections::BTreeMap,
    ffi::{OsStr, OsString},
    os::unix::fs::{MetadataExt, PermissionsExt},
};

fn same_file(a: &fs::Metadata, b: &fs::Metadata) -> bool {
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

struct SaveBaseline {
    metadata: fs::Metadata,
    attributes: BTreeMap<OsString, Vec<u8>>,
    hash: blake3::Hash,
    hash_limit: u64,
}
impl SaveBaseline {
    fn capture(file: &File, hash: Option<blake3::Hash>) -> Result<Self, FileIoError> {
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
    fn ensure_initial(&self, metadata: &fs::Metadata) -> Result<(), FileIoError> {
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
            message: cause.to_string(),
            authority: recovery,
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
    source_sync_pending: bool,
}
impl Holding {
    fn create(
        parent: &Root,
        source: &File,
        context: &FilesystemRecoveryContext,
    ) -> Result<Self, FileIoError> {
        // These authenticated originals precede every factory effect. The
        // factory retains this WHOLE owner even if preparation/enrollment or
        // post-publication setup fails, using the SAME reserved context/slot.
        let owner = (
            parent.try_clone().map_err(io_error)?,
            source.try_clone().map_err(io_error)?,
        );
        let admission = context.reserve_directory().map_err(FileIoError::Io)?;
        let name = OsString::from(format!(".cagents-save-{}", uuid::Uuid::new_v4()));
        let directory =
            fs_safety::prepare_recovery_directory_with_owner(admission, &name, || false, owner)
                .map_err(directory_error)?;
        let (publication, (parent, source), private_metadata, context, slot) =
            directory.into_parts();
        // The factory established actual owner-only700 on this retained inode
        // before returning. Use its receipt directly, without
        // another fallible descriptor clone or a fresh mode baseline.
        Ok(Self {
            parent,
            publication,
            private_metadata,
            context,
            slot: Some(slot),
            files: vec![source],
            cleanup_sync_complete: false,
            source_sync_pending: false,
        })
    }
    fn root(&self) -> &Root {
        self.publication.directory()
    }
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
    // All fallible authentication precedes the first writer release. The
    // caller drops its last local writer immediately after this succeeds.
    fn release_writer_pins(
        &mut self,
        writer: &File,
        readable: &File,
        baseline: &SaveBaseline,
    ) -> Result<(), FileIoError> {
        use std::os::fd::AsRawFd;
        self.validate_payload().map_err(io_error)?;
        let flags = unsafe { libc::fcntl(readable.as_raw_fd(), libc::F_GETFL) };
        if flags < 0 {
            return Err(io_error(io::Error::last_os_error()));
        }
        if flags & libc::O_ACCMODE != libc::O_RDONLY {
            return Err(changed());
        }
        baseline.ensure_initial(&writer.metadata().map_err(io_error)?)?;
        baseline.verify(readable, false)?;
        let mut release = Vec::new();
        for (index, file) in self.files.iter().enumerate() {
            let metadata = file.metadata().map_err(io_error)?;
            if same_file(&baseline.metadata, &metadata) {
                baseline.ensure_initial(&metadata)?;
                let flags = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFL) };
                if flags < 0 {
                    return Err(io_error(io::Error::last_os_error()));
                }
                if flags & libc::O_ACCMODE != libc::O_RDONLY {
                    release.push(index);
                }
            }
        }
        baseline.ensure_initial(&writer.metadata().map_err(io_error)?)?;
        // The already-retained readable pin keeps this SAME authenticated
        // inode alive; no named content/version baseline is recaptured.
        for index in release.into_iter().rev() {
            self.files.remove(index);
        }
        Ok(())
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
            self.validate_payload().map_err(|error| error.to_string())?;
            self.publication
                .resolve_recovery()
                .map_err(|error| error.to_string())?;
            // Unknown private contents remain retained for explicit resolution.
            self.publication
                .cleanup_empty()
                .map_err(|error| error.to_string())?;
        } else {
            // A completed removal may have been followed by failed fsync.
            // Its unlinked inode has no ancestry to walk; retry only the held
            // outside/source parent sync, never any reconstructed path.
            self.context
                .placement()
                .validate()
                .map_err(|error| error.to_string())?;
        }
        fs_safety::sync_directory(self.context.placement().directory())
            .map_err(|e| e.to_string())?;
        fs_safety::sync_directory(&self.parent).map_err(|e| e.to_string())?;
        self.source_sync_pending = false;
        self.cleanup_sync_complete = true;
        Ok(())
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
        return Err(io::Error::other(
            "injected outside holding parent sync failure",
        ));
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
fn publish_replacement(
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
    if holding.publication.is_removed() && holding.cleanup_sync_complete && !holding.source_sync_pending {
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
        // Establish and pin read access on the exact empty creator BEFORE
        // archived ownership/mode/ACL can remove owner read (0111 or000).
        let created = temporary.metadata().map_err(io_error)?;
        if !created.is_file() || created.uid() != unsafe { libc::geteuid() } || created.len() != 0 {
            return Err(changed());
        }
        temporary
            .set_permissions(Permissions::from_mode(0o600))
            .map_err(io_error)?;
        let ready = temporary.metadata().map_err(io_error)?;
        if !same_file(&created, &ready)
            || (ready.uid(), ready.gid(), ready.len(), ready.mode() & 0o7777)
                != (created.uid(), created.gid(), 0, 0o600)
        {
            return Err(changed());
        }
        holding.validate_payload().map_err(io_error)?;
        let readable = fs_safety::open_beneath(holding.root(), Path::new(&temporary_name))
            .map_err(resolve_error)?;
        let readable = holding.keep_file(readable)?;
        readable_temporary = Some(readable);
        let opened = readable_temporary
            .as_ref()
            .unwrap()
            .metadata()
            .map_err(io_error)?;
        if !same_file(&ready, &opened)
            || full_version(&ready) != full_version(&opened)
            || full_version(&ready) != full_version(&temporary.metadata().map_err(io_error)?)
        {
            return Err(changed());
        }
        fs_safety::copy_ownership(destination, &temporary).map_err(io_error)?;
        fs_safety::copy_access_acl(destination, &temporary).map_err(io_error)?;
        holding.validate_payload().map_err(io_error)?;
        temporary.write_all(bytes).map_err(io_error)?;
        holding.validate_payload().map_err(io_error)?;
        temporary.set_permissions(permissions).map_err(io_error)?;
        fs_safety::copy_access_acl(destination, &temporary).map_err(io_error)?;
        let lost = fs_safety::copy_extended_attributes(destination, &temporary);
        temporary.sync_all().map_err(io_error)?;
        // Reverify the SAME early read pin after the known policy changes;
        // never reopen/adopt bytes after mode000 or execute-only restoration.
        let readable = readable_temporary
            .as_ref()
            .expect("early authenticated read pin");
        let written = temporary.metadata().map_err(io_error)?;
        let opened = readable.metadata().map_err(io_error)?;
        if !same_file(&written, &opened) || full_version(&written) != full_version(&opened) {
            return Err(changed());
        }
        let mut prepared = SaveBaseline::capture(readable, Some(blake3::hash(bytes)))?;
        prepared.hash_limit = bytes.len() as u64;
        prepared.verify(readable, false)?;
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
                holding,
                &temporary_name,
                readable_temporary.as_ref().unwrap_or(&temporary),
                temporary_baseline.as_ref(),
                error,
            ));
        }
    };
    let previous = OsStr::new("previous");
    if let Err(error) = holding.release_writer_pins(
        &temporary,
        readable_temporary
            .as_ref()
            .expect("authenticated readable temporary"),
        temporary_baseline
            .as_ref()
            .expect("authenticated temporary baseline"),
    ) {
        return Err(clean_before_acquire(
            parent,
            holding,
            &temporary_name,
            readable_temporary.as_ref().unwrap_or(&temporary),
            temporary_baseline.as_ref(),
            error,
        ));
    }
    // Dropping a duplicate does not downgrade it: close BOTH the retained and
    // local O_WRONLY owners before publishing an executable pathname.
    drop(temporary);
    let temporary = readable_temporary
        .take()
        .expect("authenticated read-only custody");
    if let Err(error) = holding.validate_payload() {
        return Err(clean_before_acquire(
            parent,
            holding,
            &temporary_name,
            readable_temporary.as_ref().unwrap_or(&temporary),
            temporary_baseline.as_ref(),
            io_error(error),
        ));
    }
    if let Err(error) =
        fs_safety::rename_new_between(parent, name.as_os_str(), holding.root(), previous)
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
    holding.source_sync_pending = true;
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
                    match fs_safety::sync_directory(parent) {
                        Ok(()) => holding.source_sync_pending = false,
                        Err(error) => message.push_str(&format!("; restoration durability unconfirmed: {error}")),
                    }
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
    match sync_parent(parent) {
        Ok(()) => holding.source_sync_pending = false,
        Err(error) => warnings.push(format!("replacement durability unconfirmed: {error}")),
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

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use crate::filesystem_recovery_context::fixtures::{fixture_context, fixture_context_with_guard};
    use std::sync::{Arc, atomic::{AtomicBool, Ordering}};

    struct ResetSync;
    impl Drop for ResetSync {
        fn drop(&mut self) { FAIL_CLEANUP_PARENT_SYNC.with(|flag| flag.set(false)); }
    }
    struct Closed(Arc<AtomicBool>);
    impl Drop for Closed {
        fn drop(&mut self) { self.0.store(true, Ordering::Release); }
    }
    fn fixture() -> (tempfile::TempDir, Root) {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("file"), b"base").unwrap();
        let root = Root::open(directory.path()).unwrap();
        (directory, root)
    }
    #[test]
    fn original_root_save_preserves_mode_and_disk_cas() {
        let (directory, root) = fixture();
        fs::set_permissions(directory.path().join("file"), Permissions::from_mode(0o640)).unwrap();
        let context = fixture_context(&root).unwrap();
        let result = save_file_at(&root, Path::new("file"), "mine", blake3::hash(b"wrong"), false, &context).unwrap();
        assert!(matches!(result.outcome, SaveOutcome::Conflict { .. }));
        assert_eq!(fs::read(directory.path().join("file")).unwrap(), b"base");
        let result = save_file_at(&root, Path::new("file"), "mine", blake3::hash(b"base"), false, &context).unwrap();
        assert!(matches!(result.outcome, SaveOutcome::Saved { durability: super::super::SaveDurability::Durable, .. }));
        assert!(result.recovery_authority().is_none());
        assert_eq!(fs::read(directory.path().join("file")).unwrap(), b"mine");
        assert_eq!(fs::metadata(directory.path().join("file")).unwrap().mode() & 0o7777, 0o640);
        assert!(fs::read_dir(context.placement().directory().path()).unwrap().next().is_none());
    }
    #[test]
    fn chmod_during_hash_read_cannot_be_adopted_then_overwritten_by_old_permissions() {
        let (directory, root) = fixture();
        let path = directory.path().join("file");
        fs::set_permissions(&path, Permissions::from_mode(0o640)).unwrap();
        let context = fixture_context(&root).unwrap();
        let result = save_file_with_read_hook(&root, Path::new("file"), "mine", blake3::hash(b"base"), false, &context,
            || fs::set_permissions(&path, Permissions::from_mode(0o600)), || Ok(()), fs_safety::sync_directory);
        assert!(result.is_err());
        assert!(result.unwrap_err().recovery_authority().is_none(), "refusal precedes holding effects");
        assert_eq!(fs::metadata(&path).unwrap().mode() & 0o7777, 0o600);
        assert_eq!(fs::read(&path).unwrap(), b"base");
        assert!(fs::read_dir(context.placement().directory().path()).unwrap().next().is_none());
    }
    #[test]
    fn moved_nested_parent_cannot_publish_outside_original_root_or_into_lookalike() {
        let directory = tempfile::tempdir().unwrap();
        let source = directory.path().join("source");
        let nested = source.join("nested");
        let moved = directory.path().join("moved-parent");
        fs::create_dir_all(&nested).unwrap();
        fs::write(nested.join("file"), b"base").unwrap();
        let root = Root::open(&source).unwrap();
        let context = fixture_context(&root).unwrap();
        let result = save_file_with_hooks(&root, Path::new("nested/file"), "mine", blake3::hash(b"base"), false, &context, || {
            fs::rename(&nested, &moved)?;
            fs::create_dir(&nested)?;
            fs::write(nested.join("file"), b"replacement")
        }, fs_safety::sync_directory);
        assert!(result.is_err());
        assert_eq!(fs::read(moved.join("file")).unwrap(), b"base");
        assert_eq!(fs::read(nested.join("file")).unwrap(), b"replacement");
        assert!(fs::read_dir(context.placement().directory().path()).unwrap().next().is_none());
    }
    #[test]
    fn original_root_save_refuses_same_name_replacement_before_effects() {
        let parent = tempfile::tempdir().unwrap();
        let named = parent.path().join("checkout");
        let moved = parent.path().join("original");
        fs::create_dir(&named).unwrap();
        fs::write(named.join("file"), b"base").unwrap();
        let root = Root::open(&named).unwrap();
        let context = fixture_context(&root).unwrap();
        fs::rename(&named, &moved).unwrap();
        fs::create_dir(&named).unwrap();
        fs::write(named.join("file"), b"newer").unwrap();
        assert!(save_file_at(&root, Path::new("file"), "mine", blake3::hash(b"base"), true, &context).is_err());
        assert_eq!(fs::read(moved.join("file")).unwrap(), b"base");
        assert_eq!(fs::read(named.join("file")).unwrap(), b"newer");
        assert!(fs::read_dir(context.placement().directory().path()).unwrap().next().is_none());
    }
    #[test]
    fn removed_holding_sync_failure_keeps_original_owner_in_both_outer_results() {
        for committed in [false, true] {
            let (directory, root) = fixture();
            let released = Arc::new(AtomicBool::new(false));
            let context = fixture_context_with_guard(&root, Closed(released.clone())).unwrap();
            let outside = context.placement().directory().path().to_owned();
            FAIL_CLEANUP_PARENT_SYNC.with(|flag| flag.set(true));
            let _reset = ResetSync;
            let result = save_file_with_hooks(&root, Path::new("file"), "mine", blake3::hash(b"base"), false, &context,
                || if committed { Ok(()) } else { Err(io::Error::other("prepublication refusal")) }, fs_safety::sync_directory);
            let authority = match result {
                Ok(result) if committed => result.recovery_authority().unwrap().clone(),
                Err(error) if !committed => error.recovery_authority().unwrap().clone(),
                other => panic!("whole outer typed result required: {other:?}"),
            };
            assert!(!FAIL_CLEANUP_PARENT_SYNC.with(|flag| flag.get()));
            assert!(fs::read_dir(&outside).unwrap().next().is_none());
            drop(context);
            assert!(!released.load(Ordering::Acquire));
            assert!(!authority.is_resolved());
            authority.retry().unwrap();
            assert!(authority.is_resolved());
            assert!(released.load(Ordering::Acquire));
            assert_eq!(fs::read(directory.path().join("file")).unwrap(), if committed { &b"mine"[..] } else { &b"base"[..] });
        }
    }
    #[test]
    fn source_parent_sync_failure_retains_original_cleanup_until_actual_retry() {
        let (directory, root) = fixture();
        let released = Arc::new(AtomicBool::new(false));
        let context = fixture_context_with_guard(&root, Closed(released.clone())).unwrap();
        let result = save_file_with_hooks(&root, Path::new("file"), "mine", blake3::hash(b"base"), false, &context,
            || Ok(()), |_| Err(io::Error::other("source parent sync refused"))).unwrap();
        assert_eq!(fs::read(directory.path().join("file")).unwrap(), b"mine");
        let authority = result.recovery_authority().expect("original source parent is still unsynced");
        assert!(fs::read_dir(context.placement().directory().path()).unwrap().next().is_none());
        drop(context);
        assert!(!released.load(Ordering::Acquire));
        authority.retry().unwrap();
        assert!(authority.is_resolved());
        assert!(released.load(Ordering::Acquire));
    }
    #[test]
    fn saved_executable_runs_while_cleanup_authority_is_still_pending() {
        use crate::process::{CancellationToken, CommandSpec, ProcessExecutor, SystemProcessExecutor};
        let (directory, root) = fixture();
        fs::set_permissions(directory.path().join("file"), Permissions::from_mode(0o700)).unwrap();
        let context = fixture_context(&root).unwrap();
        FAIL_CLEANUP_PARENT_SYNC.with(|flag| flag.set(true));
        let _reset = ResetSync;
        let result = save_file_at(&root, Path::new("file"), "#!/bin/sh\nprintf retained-save", blake3::hash(b"base"), false, &context).unwrap();
        let authority = result.recovery_authority().expect("real pending sync");
        let output = SystemProcessExecutor.execute(
            &CommandSpec::new(directory.path().join("file")).timeout(std::time::Duration::from_secs(5)), &CancellationToken::new()).unwrap();
        assert!(output.success, "{}", String::from_utf8_lossy(&output.stderr));
        assert_eq!(output.stdout, b"retained-save");
        assert!(!authority.is_resolved());
        authority.retry().unwrap();
    }
}
