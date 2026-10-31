//! A directory is owned before its requested public name is published.
//!
//! mkdir returns no descriptor. Bootstrap acquisition therefore depends on a
//! protected namespace: other UIDs cannot change its entries, and cooperating
//! same-UID writers do not modify our random internal staging names. This does
//! NOT exclude concurrent same-UID replacement of the requested public name.
//! Hostile same-UID/root interference inside the protected namespace is outside
//! this contract. No post-hoc stat can make that bootstrap assumption absolute.
use super::*;
use std::{os::unix::fs::MetadataExt, sync::Arc};

/// A retained namespace whose entries other UIDs cannot replace.
///
/// Linux POSIX ACL write grants are constrained by the group mode mask. Darwin
/// additionally requires successfully observed native ACL absence. A writable
/// shared namespace is not silently promoted to private acquisition authority.
#[derive(Debug)]
pub struct DirectoryNamespace {
    directory: Root,
}
impl DirectoryNamespace {
    pub fn acquire(directory: &Root) -> io::Result<Self> {
        namespace_protected(directory)?;
        Ok(Self {
            directory: directory.try_clone()?,
        })
    }
    pub fn directory(&self) -> &Root {
        &self.directory
    }
}

/// A retained root- or effective-user-owned sticky namespace. This proof is
/// only for fresh random internal entries owned by this effective user; it is
/// never a DirectoryNamespace or authority over an existing public entry.
/// Root and noncooperating same-UID interference remain outside this contract.
#[derive(Debug)]
pub(crate) struct StickyParent {
    directory: Arc<Root>,
    stamp: RecoveryDirectoryStamp,
    uid: u32,
    mode: u32,
}
/// Kernel permission observation on the retained inode, using effective
/// credentials, supplementary groups and ACLs. This is a pre-effect check,
/// never a write probe or a promise against later permission/topology changes.
#[cfg(target_os = "linux")]
fn sticky_parent_effective_access(directory: &Root) -> io::Result<()> {
    // SAFETY: the Root retains its descriptor and the empty C string is live.
    // AT_EMPTY_PATH selects that descriptor; AT_EACCESS uses effective IDs.
    let status = unsafe {
        libc::syscall(
            libc::SYS_faccessat2,
            directory.descriptor(),
            c"".as_ptr(),
            libc::W_OK | libc::X_OK,
            libc::AT_EMPTY_PATH | libc::AT_EACCESS,
        )
    };
    if status == 0 {
        return Ok(());
    }
    let error = io::Error::last_os_error();
    if matches!(
        error.raw_os_error(),
        Some(libc::ENOSYS) | Some(libc::EINVAL)
    ) {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "Sticky-parent qualification requires retained-FD effective permission observation",
        ));
    }
    Err(error)
}
#[cfg(not(target_os = "linux"))]
fn sticky_parent_effective_access(_directory: &Root) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "Retained sticky-parent effective permission observation is unavailable on this platform",
    ))
}
impl StickyParent {
    pub(crate) fn acquire(directory: Arc<Root>) -> io::Result<Arc<Self>> {
        let metadata = directory.metadata()?;
        let euid = unsafe { libc::geteuid() };
        if !metadata.is_dir()
            || (metadata.uid() != 0 && metadata.uid() != euid)
            || metadata.mode() & 0o1000 == 0
        {
            #[cfg(test)]
            {
                use std::io::Write as _;
                let _ = writeln!(
                    std::io::stderr().lock(),
                    "retained-save diagnostic: stage=sticky-parent-acquire-guard; parent_is_dir={}; \
                     parent_uid={}; effective_uid={euid}; parent_mode={:#o}; \
                     parent_dev={}; parent_ino={}",
                    metadata.is_dir(),
                    metadata.uid(),
                    metadata.mode() & 0o7777,
                    metadata.dev(),
                    metadata.ino()
                );
            }
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "Private sibling creation requires a root- or user-owned sticky parent",
            ));
        }
        let parent = Arc::new(Self {
            stamp: recovery_directory_stamp(&directory)?,
            directory,
            uid: metadata.uid(),
            mode: metadata.mode() & 0o7777,
        });
        parent.validate()?;
        Ok(parent)
    }
    pub(crate) fn validate(&self) -> io::Result<()> {
        let current = self.directory.metadata()?;
        let euid = unsafe { libc::geteuid() };
        if recovery_directory_stamp(&self.directory)? != self.stamp
            || current.uid() != self.uid
            || (current.uid() != 0 && current.uid() != euid)
            || current.mode() & 0o7777 != self.mode
            || current.mode() & 0o1000 == 0
        {
            return Err(io::Error::other(
                "Retained sticky parent protection changed",
            ));
        }
        sticky_parent_effective_access(&self.directory)
    }
    /// The exact retained parent proof; Arc aliases add no native handle.
    pub(crate) fn directory(&self) -> &Arc<Root> {
        &self.directory
    }
    /// One native parent handle; Arc aliases do not add descriptors.
    pub(crate) fn retained_descriptor_count(&self) -> usize {
        1
    }
    /// Callers must reserve their bounded recovery slot and charge retained
    /// native owners before this first effect. No existing entry is adopted.
    pub(crate) fn create_private_sibling(
        self: Arc<Self>,
    ) -> Result<OwnedPrivateSibling, PrivateSiblingCreateError> {
        self.create_with_hook(|_, _| Ok(()))
    }
    /// Save-only live custody after strict fresh identity/private qualification.
    /// Directory sync failure is reported separately and must remain a warning
    /// through the final Save outcome. Callers retain the same pre-effect charge.
    pub(crate) fn create_save_private_sibling(
        self: Arc<Self>,
    ) -> Result<(OwnedPrivateSibling, SaveSiblingDurability), PrivateSiblingCreateError> {
        let name = temporary_name().map_err(PrivateSiblingCreateError::Rejected)?;
        self.create_named_using(
            name,
            |_, _| Ok(()),
            SiblingPreparationPolicy::SaveLiveCustody,
            sync_directory,
        )
    }
    fn create_with_hook(
        self: Arc<Self>,
        after_mkdir: impl FnOnce(&Root, &OsStr) -> io::Result<()>,
    ) -> Result<OwnedPrivateSibling, PrivateSiblingCreateError> {
        let name = temporary_name().map_err(PrivateSiblingCreateError::Rejected)?;
        self.create_named_with_hook(name, after_mkdir)
    }
    fn create_named_with_hook(
        self: Arc<Self>,
        name: OsString,
        after_mkdir: impl FnOnce(&Root, &OsStr) -> io::Result<()>,
    ) -> Result<OwnedPrivateSibling, PrivateSiblingCreateError> {
        self.create_named_using(
            name,
            after_mkdir,
            SiblingPreparationPolicy::Strict,
            sync_directory,
        )
        .map(|(sibling, _)| sibling)
    }
    fn create_named_using(
        self: Arc<Self>,
        name: OsString,
        after_mkdir: impl FnOnce(&Root, &OsStr) -> io::Result<()>,
        policy: SiblingPreparationPolicy,
        mut sync: impl FnMut(&Root) -> io::Result<()>,
    ) -> Result<(OwnedPrivateSibling, SaveSiblingDurability), PrivateSiblingCreateError> {
        self.validate()
            .map_err(PrivateSiblingCreateError::Rejected)?;
        let leaf = c_component(&name, &self.directory.path)
            .map_err(|e| PrivateSiblingCreateError::Rejected(io::Error::other(e)))?;
        let mut sibling = OwnedPrivateSibling {
            parent: self,
            name,
            directory: None,
            stamp: None,
            quarantined: None,
            removed: false,
            synced: false,
        };
        // Exclusive success under the pinned sticky namespace is the creation
        // provenance. EEXIST never confers custody of the occupied entry.
        if unsafe { libc::mkdirat(sibling.parent.directory.descriptor(), leaf.as_ptr(), 0o700) }
            != 0
        {
            return Err(PrivateSiblingCreateError::Rejected(
                io::Error::last_os_error(),
            ));
        }
        let setup = (|| {
            after_mkdir(&sibling.parent.directory, &sibling.name)?;
            sibling.acquire_original()?;
            let directory = sibling.directory.as_ref().unwrap();
            // Restrictive umasks can remove owner access. Only repair the
            // exact fresh EUID-owned inode after mount/identity qualification.
            set_directory_permissions(directory, 0o700)?;
            sibling.validate()
        })();
        if let Err(cause) = setup {
            return Err(PrivateSiblingCreateError::Retained { cause, sibling });
        }
        let holding = sync(sibling.directory.as_ref().unwrap());
        let confirmation = if matches!(policy, SiblingPreparationPolicy::Strict) {
            holding.and_then(|()| sync(&sibling.parent.directory))
        } else {
            // Save still observes both directory confirmation steps, retaining
            // the first error without weakening any pre-sync qualification.
            let parent = sync(&sibling.parent.directory);
            holding.and(parent)
        };
        match confirmation {
            Ok(()) => Ok((sibling, SaveSiblingDurability::Confirmed)),
            Err(cause) => match policy {
                SiblingPreparationPolicy::Strict => {
                    Err(PrivateSiblingCreateError::Retained { cause, sibling })
                }
                SiblingPreparationPolicy::SaveLiveCustody => {
                    Ok((sibling, SaveSiblingDurability::Unconfirmed(cause)))
                }
            },
        }
    }
}

#[derive(Clone, Copy)]
enum SiblingPreparationPolicy {
    Strict,
    SaveLiveCustody,
}

/// Save-only directory durability, separate from live fresh custody and exact
/// namespace removal. Later successful syncs do not erase an earlier warning
/// without qualified proof. General sibling operations retain strict errors.
#[derive(Debug)]
#[must_use = "sibling directory durability must propagate to the final Save outcome"]
pub(crate) enum SaveSiblingDurability {
    Confirmed,
    Unconfirmed(io::Error),
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum PrivateSiblingCreateError {
    #[error("Private sibling creation refused: {0}")]
    Rejected(#[source] io::Error),
    #[error("Private sibling preparation failed: {cause}; original cleanup custody retained")]
    Retained {
        #[source]
        cause: io::Error,
        sibling: OwnedPrivateSibling,
    },
}

/// Exact fresh private sibling custody, including incomplete initial FD
/// acquisition. No Drop performs filesystem IO. A successful caller must
/// explicitly remove this empty parent after all leaf holdings have retired;
/// cleanup errors retain this receipt and its pre-effect reservation.
#[derive(Debug)]
#[must_use = "retain the sibling through explicit cleanup and parent sync"]
pub(crate) struct OwnedPrivateSibling {
    parent: Arc<StickyParent>,
    name: OsString,
    directory: Option<Arc<Root>>,
    stamp: Option<RecoveryDirectoryStamp>,
    quarantined: Option<OsString>,
    removed: bool,
    synced: bool,
}
impl OwnedPrivateSibling {
    fn acquire_original(&mut self) -> io::Result<()> {
        self.parent.validate()?;
        if self.directory.is_none() {
            // A pending first acquisition is permitted only by the ORIGINAL
            // fresh mkdir + sticky namespace custody, never pathname existence.
            let directory = resolve_directory(&self.parent.directory, Path::new(&self.name))
                .map_err(io::Error::other)?;
            self.directory = Some(Arc::new(directory));
        }
        let directory = self.directory.as_ref().unwrap();
        let metadata = directory.metadata()?;
        let stamp = recovery_directory_stamp(directory)?;
        if !metadata.is_dir()
            || metadata.uid() != unsafe { libc::geteuid() }
            || stamp.device != self.parent.stamp.device
            || stamp.mount_id != self.parent.stamp.mount_id
            || self.stamp.is_some_and(|expected| expected != stamp)
        {
            return Err(io::Error::other(
                "Fresh private sibling ownership or retained mount identity changed; entry preserved",
            ));
        }
        self.stamp = Some(stamp);
        Ok(())
    }
    pub(crate) fn directory(&self) -> io::Result<&Root> {
        self.validate()?;
        Ok(self.directory.as_ref().unwrap())
    }
    pub(crate) fn directory_arc(&self) -> io::Result<Arc<Root>> {
        self.validate()?;
        Ok(self.directory.as_ref().unwrap().clone())
    }
    pub(crate) fn validate(&self) -> io::Result<()> {
        self.parent.validate()?;
        let directory = self.directory.as_ref().ok_or_else(|| {
            io::Error::other("Private sibling initial acquisition remains unresolved")
        })?;
        let metadata = directory.metadata()?;
        if self.removed
            || self.quarantined.is_some()
            || self.stamp != Some(recovery_directory_stamp(directory)?)
            || metadata.uid() != unsafe { libc::geteuid() }
            || metadata.mode() & 0o7777 != 0o700
        {
            return Err(io::Error::other(
                "Original private sibling is no longer qualified",
            ));
        }
        namespace_protected(directory)
    }
    /// Actual native handles, including the pinned original sticky parent.
    /// Arc aliases of the parent/child must be deduplicated by the owner.
    /// Quarantine is a name under that same parent and owns no extra FD.
    pub(crate) fn retained_descriptor_count(&self) -> usize {
        1 + usize::from(self.directory.is_some())
    }
    pub(crate) fn is_resolved(&self) -> bool {
        self.removed && self.synced
    }
    pub(crate) fn diagnostic_path(&self) -> PathBuf {
        self.parent
            .directory
            .path
            .join(self.quarantined.as_ref().unwrap_or(&self.name))
    }
    pub(crate) fn is_removed(&self) -> bool {
        self.removed
    }
    pub(crate) fn cleanup_empty(&mut self) -> io::Result<()> {
        self.cleanup_with_hooks(|| {}, sync_directory)
    }
    /// Save may retire positively removed sibling custody with a durability
    /// warning. All namespace/quarantine/identity/nonempty failures stay errors.
    pub(crate) fn cleanup_save_empty(&mut self) -> io::Result<SaveSiblingDurability> {
        self.cleanup_save_with_hooks(|| {}, sync_directory)
    }
    fn cleanup_save_with_hooks(
        &mut self,
        after_acquire: impl FnOnce(),
        sync_parent: impl FnOnce(&Root) -> io::Result<()>,
    ) -> io::Result<SaveSiblingDurability> {
        self.remove_empty_with_hook(after_acquire)?;
        // The removal phase succeeded; only this explicit final sync phase
        // can become Unconfirmed. No matching of arbitrary cleanup errors.
        debug_assert!(self.removed);
        Ok(match self.confirm_removed_parent(sync_parent) {
            Ok(()) => SaveSiblingDurability::Confirmed,
            Err(error) => SaveSiblingDurability::Unconfirmed(error),
        })
    }
    fn cleanup_with_hooks(
        &mut self,
        after_acquire: impl FnOnce(),
        sync_parent: impl FnOnce(&Root) -> io::Result<()>,
    ) -> io::Result<()> {
        self.remove_empty_with_hook(after_acquire)?;
        self.confirm_removed_parent(sync_parent)
    }
    fn confirm_removed_parent(
        &mut self,
        sync_parent: impl FnOnce(&Root) -> io::Result<()>,
    ) -> io::Result<()> {
        if !self.synced {
            sync_parent(&self.parent.directory)?;
            self.synced = true;
        }
        Ok(())
    }
    fn remove_empty_with_hook(&mut self, after_acquire: impl FnOnce()) -> io::Result<()> {
        if self.removed {
            return Ok(());
        }
        self.acquire_original()?;
        if self.quarantined.is_none() {
            let quarantine = temporary_name()?;
            rename_new_between(
                &self.parent.directory,
                &self.name,
                &self.parent.directory,
                &quarantine,
            )
            .map_err(io::Error::other)?;
            self.quarantined = Some(quarantine);
        }
        after_acquire();
        self.parent.validate()?;
        let quarantine = self.quarantined.as_ref().unwrap();
        let removal = (|| {
            let acquired = resolve_directory(&self.parent.directory, Path::new(quarantine))
                .map_err(io::Error::other)?;
            if Some(recovery_directory_stamp(&acquired)?) != self.stamp
                || acquired.metadata()?.uid() != unsafe { libc::geteuid() }
            {
                return Err(io::Error::other(
                    "Sibling cleanup acquired a replacement; it was preserved",
                ));
            }
            remove_directory_within(&self.parent.directory, quarantine).map_err(io::Error::other)
        })();
        if let Err(cause) = removal {
            rename_new_between(
                &self.parent.directory,
                quarantine,
                &self.parent.directory,
                &self.name,
            ).map_err(|restore| io::Error::other(format!(
                "{cause}; no-clobber sibling restoration failed: {restore}; acquired entry retained",
            )))?;
            self.quarantined = None;
            return Err(cause);
        }
        self.quarantined = None;
        self.removed = true;
        Ok(())
    }
}

/// Existing/unverified directories grant traversal, never creation ownership.
#[must_use = "creation provenance must be handled before mutating permissions or cleanup"]
#[derive(Debug)]
pub enum DirectoryCreation {
    Existing(Root),
    Published(DirectoryPublication),
    Unverified { directory: Root, reason: String },
}
impl DirectoryCreation {
    pub fn directory(&self) -> &Root {
        match self {
            Self::Existing(root)
            | Self::Unverified {
                directory: root, ..
            } => root,
            Self::Published(published) => published.directory(),
        }
    }
    /// Consumes any creation receipt. Keep Published itself when cleanup or
    /// exact-created-object bookkeeping is needed later.
    pub fn into_directory(self) -> Root {
        match self {
            Self::Existing(root)
            | Self::Unverified {
                directory: root, ..
            } => root,
            Self::Published(published) => published.directory,
        }
    }
}

/// An unsuccessful creation can still own private staging. Keep this error
/// until its recovery has been transferred to a live carrier or explicitly
/// resolved. There is no conversion back to ResolveError that discards it.
#[derive(Debug)]
#[must_use = "creation failures may retain live staging authority"]
pub struct DirectoryCreateError {
    cause: ResolveError,
    recovery: Option<DirectoryPreparation>,
}
impl DirectoryCreateError {
    pub fn cause(&self) -> &ResolveError {
        &self.cause
    }
    pub fn recovery(&self) -> Option<&DirectoryPreparation> {
        self.recovery.as_ref()
    }
    pub fn take_recovery(&mut self) -> Option<DirectoryPreparation> {
        self.recovery.take()
    }
}
impl std::fmt::Display for DirectoryCreateError {
    fn fmt(&self, out: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        std::fmt::Display::fmt(&self.cause, out)?;
        if let Some(recovery) = &self.recovery {
            write!(
                out,
                "; private staging retained (diagnostic spelling {}): {}",
                recovery.diagnostic_path().display(),
                recovery
                    .cleanup_error
                    .as_deref()
                    .unwrap_or("cleanup unresolved")
            )?;
        }
        Ok(())
    }
}
impl std::error::Error for DirectoryCreateError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}
impl From<ResolveError> for DirectoryCreateError {
    fn from(cause: ResolveError) -> Self {
        Self {
            cause,
            recovery: None,
        }
    }
}

/// Permission restoration retains the exact prepublication mode and the
/// temporary state it introduced. Observably newer ownership/mode changes
/// refuse restoration. fstat+fchmod is not an atomic metadata compare-and-swap;
/// concurrent noncooperating chmod on the SAME inode remains an OS API limit.
#[derive(Debug, Clone, Copy)]
struct ModeRestoration {
    original: u32,
    temporary: u32,
    uid: u32,
    gid: u32,
}
impl ModeRestoration {
    fn restore(self, directory: &Root) -> io::Result<()> {
        let current = directory.metadata()?;
        if current.uid() != self.uid || current.gid() != self.gid {
            return Err(io::Error::other(
                "directory ownership changed; pending mode restoration retained",
            ));
        }
        let mode = current.mode() & 0o7777;
        if mode == self.original {
            return Ok(());
        }
        if mode != self.temporary {
            return Err(io::Error::other(
                "directory permissions changed after publication; newer observed mode preserved",
            ));
        }
        restore_directory_mode(directory, self.original)
    }
}

/// Retained INTERNAL staging after a failed prepublication operation. If mkdir
/// succeeded but descriptor acquisition failed, namespace/name acquisition is
/// pending under the protected internal bootstrap contract stated above. This
/// never grants creation authority over a public target. No Drop mutates disk.
#[derive(Debug)]
#[must_use = "retain unresolved staging until explicit worker cleanup succeeds"]
pub struct DirectoryPreparation {
    namespace: DirectoryNamespace,
    name: OsString,
    outer: Option<Root>,
    payload: Option<Root>,
    payload_created: bool,
    pending_mode: Option<ModeRestoration>,
    created: bool,
    cleanup_error: Option<String>,
}
impl DirectoryPreparation {
    /// Opened spelling only, possibly stale after an ancestor rename.
    pub fn diagnostic_path(&self) -> PathBuf {
        self.namespace.directory.path.join(&self.name)
    }
    pub fn is_resolved(&self) -> bool {
        !self.created
    }
    pub fn retry_cleanup(&mut self) -> Result<(), ResolveError> {
        self.retry_cleanup_with_hook(|_| Ok(()))
    }
    fn retry_cleanup_with_hook(
        &mut self,
        mut after_acquire: impl FnMut(&Root) -> Result<(), ResolveError>,
    ) -> Result<(), ResolveError> {
        if !self.created {
            return Ok(());
        }
        namespace_protected(&self.namespace.directory)
            .map_err(|e| ResolveError::io(&self.namespace.directory.path, &e))?;
        let outer = match resolve_directory(&self.namespace.directory, Path::new(&self.name)) {
            Ok(outer) => outer,
            Err(error) if error.is_not_found() => {
                if let Some(held) = &self.outer {
                    if held
                        .metadata()
                        .map_err(|e| ResolveError::io(&held.path, &e))?
                        .nlink()
                        != 0
                    {
                        return Err(ResolveError::message(
                            &held.path,
                            "retained staging is no longer at its internal name; authority preserved for resolution",
                        ));
                    }
                }
                self.created = false;
                self.cleanup_error = None;
                return Ok(());
            }
            Err(error) => return Err(error),
        };
        if let Some(held) = &self.outer {
            let expected = held
                .metadata()
                .map_err(|e| ResolveError::io(&held.path, &e))?;
            let actual = outer
                .metadata()
                .map_err(|e| ResolveError::io(&outer.path, &e))?;
            if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
                return Err(ResolveError::message(
                    &outer.path,
                    "internal staging identity changed; replacement preserved",
                ));
            }
        } else {
            // Store the first acquired capability before fstat, callbacks or
            // any other fallible operation. Retrying cannot adopt a successor.
            self.outer = Some(outer);
            after_acquire(self.outer.as_ref().unwrap())?;
        }
        let outer = self.outer.as_ref().unwrap();
        outer
            .metadata()
            .map_err(|e| ResolveError::io(&outer.path, &e))?;
        if self.payload_created {
            match resolve_directory(outer, Path::new("directory")) {
                Ok(payload) => {
                    if let Some(held) = &self.payload {
                        let expected = held
                            .metadata()
                            .map_err(|e| ResolveError::io(&held.path, &e))?;
                        let actual = payload
                            .metadata()
                            .map_err(|e| ResolveError::io(&payload.path, &e))?;
                        if (actual.dev(), actual.ino()) != (expected.dev(), expected.ino()) {
                            return Err(ResolveError::message(
                                &payload.path,
                                "internal payload identity changed; replacement preserved",
                            ));
                        }
                    } else {
                        self.payload = Some(payload);
                        after_acquire(self.payload.as_ref().unwrap())?;
                    }
                    let payload = self.payload.as_ref().unwrap();
                    payload
                        .metadata()
                        .map_err(|e| ResolveError::io(&payload.path, &e))?;
                    if let Some(mode) = self.pending_mode {
                        mode.restore(payload)
                            .map_err(|e| ResolveError::io(&payload.path, &e))?;
                        self.pending_mode = None;
                    }
                    remove_directory_within(outer, OsStr::new("directory"))?;
                }
                Err(error) if error.is_not_found() => {
                    if let Some(held) = &self.payload {
                        if held
                            .metadata()
                            .map_err(|e| ResolveError::io(&held.path, &e))?
                            .nlink()
                            != 0
                        {
                            return Err(ResolveError::message(
                                &held.path,
                                "retained payload is no longer at its internal name; authority preserved for resolution",
                            ));
                        }
                    }
                }
                Err(error) => return Err(error),
            }
            self.payload_created = false;
        }
        remove_directory_within(&self.namespace.directory, &self.name)?;
        self.created = false;
        self.cleanup_error = None;
        Ok(())
    }
    fn fail(mut self, cause: ResolveError) -> DirectoryCreateError {
        match self.retry_cleanup() {
            Ok(()) => cause.into(),
            Err(error) => {
                self.cleanup_error = Some(error.to_string());
                DirectoryCreateError {
                    cause,
                    recovery: Some(self),
                }
            }
        }
    }
}

/// Result of explicitly resolving an acquired cleanup entry. Restored does not
/// mean the original directory was removed; callers must preserve that distinction.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DirectoryRecovery {
    OriginalRemoved,
    OriginalRestored,
    ReplacementRestored,
}

/// Prepublication identity, retained even if later private staging cleanup fails.
///
/// Publication is not an fsync/durability claim. Callers must sync the relevant
/// directories and retain this receipt if subsequent fallible steps fail.
/// There is deliberately no automatic Drop cleanup of public or recovery names.
#[must_use = "retain the publication receipt through completion and cleanup errors"]
#[derive(Debug)]
pub struct DirectoryPublication {
    directory: Root,
    parent: Root,
    name: OsString,
    namespace: DirectoryNamespace,
    identity: Metadata,
    publication_error: Option<String>,
    pending_mode: Option<ModeRestoration>,
    private_staging: Option<(Root, OsString)>,
    quarantined: Option<OsString>,
    removed: bool,
}
impl DirectoryPublication {
    pub fn directory(&self) -> &Root {
        &self.directory
    }
    /// Captured before public publication; reading this cannot lose a known
    /// publication because a later fstat fails.
    pub fn identity(&self) -> (u64, u64) {
        (self.identity.dev(), self.identity.ino())
    }
    pub fn initial_metadata(&self) -> &Metadata {
        &self.identity
    }
    pub fn publication_error(&self) -> Option<&str> {
        self.publication_error.as_deref()
    }
    /// Worker-only retry of pending mode/private-staging steps. This does not
    /// remove the public payload or resolve an acquired public recovery entry.
    /// Callers retain any separate quarantine/durability obligations.
    pub fn retry_publication_steps(&mut self) -> Result<(), ResolveError> {
        self.restore_pending_mode()?;
        self.cleanup_private_staging()
    }
    /// Diagnostic opened spelling only; namespace moves can make it stale.
    /// The retained namespace and private leaf, not this path, are authority.
    pub fn pending_staging_path(&self) -> Option<PathBuf> {
        self.private_staging
            .as_ref()
            .map(|(_, name)| self.namespace.directory.path.join(name))
    }
    /// Diagnostic opened spelling of an unresolved acquired name. Namespace
    /// renames can make this stale; it is not a verified current absolute path
    /// or permission to delete its current entry.
    pub fn recovery_path(&self) -> Option<PathBuf> {
        self.quarantined
            .as_ref()
            .map(|name| self.namespace.directory.path.join(name))
    }
    pub fn is_removed(&self) -> bool {
        self.removed
    }

    fn restore_pending_mode(&mut self) -> Result<(), ResolveError> {
        if let Some(mode) = self.pending_mode {
            mode.restore(&self.directory)
                .map_err(|e| ResolveError::io(&self.directory.path, &e))?;
            self.pending_mode = None;
            if self.private_staging.is_none() {
                self.publication_error = None;
            }
        }
        Ok(())
    }
    fn cleanup_private_staging(&mut self) -> Result<(), ResolveError> {
        let Some((held, name)) = &self.private_staging else {
            return Ok(());
        };
        namespace_protected(&self.namespace.directory)
            .map_err(|e| ResolveError::io(&self.namespace.directory.path, &e))?;
        let named = resolve_directory(&self.namespace.directory, Path::new(name))?;
        let expected = held
            .metadata()
            .map_err(|e| ResolveError::io(&held.path, &e))?;
        let actual = named
            .metadata()
            .map_err(|e| ResolveError::io(&named.path, &e))?;
        if (expected.dev(), expected.ino()) != (actual.dev(), actual.ino()) {
            return Err(ResolveError::message(
                &named.path,
                "private staging name no longer identifies the retained directory; no replacement was removed",
            ));
        }
        // Protected INTERNAL namespace bootstrap contract applies here; public
        // cleanup below uses acquire-before-check instead of this name check.
        remove_directory_within(&self.namespace.directory, name)?;
        self.private_staging = None;
        self.publication_error = None;
        Ok(())
    }

    /// Worker-only retry using the retained receipt, never a diagnostic path.
    /// Removes only the exact empty original; all other acquired entries are
    /// restored with no-clobber. A collision/error retains recovery ownership.
    /// OriginalRestored can still require user resolution of nonempty contents.
    pub fn resolve_recovery(&mut self) -> Result<Option<DirectoryRecovery>, ResolveError> {
        self.restore_pending_mode()?;
        self.cleanup_private_staging()?;
        let Some(name) = self.quarantined.clone() else {
            return Ok(None);
        };
        namespace_protected(&self.namespace.directory)
            .map_err(|e| ResolveError::io(&self.namespace.directory.path, &e))?;
        let acquired = metadata_beneath(&self.namespace.directory, Path::new(&name))?;
        let original = (acquired.dev(), acquired.ino()) == self.identity() && acquired.is_dir();
        if original && remove_directory_within(&self.namespace.directory, &name).is_ok() {
            self.quarantined = None;
            self.removed = true;
            return Ok(Some(DirectoryRecovery::OriginalRemoved));
        }
        rename_new_between(&self.namespace.directory, &name, &self.parent, &self.name)?;
        self.quarantined = None;
        Ok(Some(if original {
            DirectoryRecovery::OriginalRestored
        } else {
            DirectoryRecovery::ReplacementRestored
        }))
    }

    /// Acquire the public entry into the protected namespace before checking
    /// identity. A mismatched replacement is never chmod'ed or removed. If a
    /// no-clobber restoration is impossible, preserve it at recovery_path and
    /// report the failure. Keep this receipt/journal that path on error.
    pub fn cleanup_empty(&mut self) -> Result<(), ResolveError> {
        self.cleanup_empty_with_hooks(|| {}, || {})
    }
    fn cleanup_empty_with_hooks(
        &mut self,
        before_acquire: impl FnOnce(),
        after_acquire: impl FnOnce(),
    ) -> Result<(), ResolveError> {
        self.restore_pending_mode()?;
        self.cleanup_private_staging()?;
        if self.removed {
            return Ok(());
        }
        if self.quarantined.is_some() {
            return Err(ResolveError::message(
                &self.parent.path.join(&self.name),
                "directory cleanup has an unresolved retained recovery entry",
            ));
        }
        namespace_protected(&self.namespace.directory)
            .map_err(|error| ResolveError::io(&self.namespace.directory.path, &error))?;
        let quarantine = unused_name(&self.namespace.directory)?;
        before_acquire();
        rename_new_between(
            &self.parent,
            &self.name,
            &self.namespace.directory,
            &quarantine,
        )?;
        self.quarantined = Some(quarantine.clone());
        after_acquire();
        let acquired = resolve_directory(&self.namespace.directory, Path::new(&quarantine));
        let checked = acquired.and_then(|acquired| {
            let current = acquired
                .metadata()
                .map_err(|e| ResolveError::io(&acquired.path, &e))?;
            if self.identity() != (current.dev(), current.ino()) {
                return Err(ResolveError::message(
                    &acquired.path,
                    "directory cleanup acquired a replacement; it was preserved",
                ));
            }
            remove_directory_within(&self.namespace.directory, &quarantine)
        });
        match checked {
            Ok(()) => {
                self.quarantined = None;
                self.removed = true;
                Ok(())
            }
            Err(error) => {
                match rename_new_between(
                    &self.namespace.directory,
                    &quarantine,
                    &self.parent,
                    &self.name,
                ) {
                    Ok(()) => {
                        self.quarantined = None;
                        Err(error)
                    }
                    Err(restore) => Err(ResolveError::message(
                        &self.namespace.directory.path.join(&quarantine),
                        &format!(
                            "{error}; could not restore acquired directory: {restore}; retained recovery entry was not removed"
                        ),
                    )),
                }
            }
        }
    }
}

/// Preserve basic creation/traversal in shared namespaces without inventing
/// ownership. Private holding callers must require Published and handle its
/// publication_error; recovery may traverse Existing/Unverified without chmod.
/// Public payload permissions obey mkdir umask. Exact chmod, when required,
/// is a separate caller action on the Published receipt's retained Root.
pub fn create_directory_beneath(
    root: &Root,
    relative: &Path,
    mode: u32,
) -> Result<DirectoryCreation, DirectoryCreateError> {
    if let Some(existing) = existing_directory(root, relative)? {
        return Ok(DirectoryCreation::Existing(existing));
    }
    let (parent, name) = open_parent_beneath(root, relative)?;
    match DirectoryNamespace::acquire(&parent) {
        Ok(namespace) => create_directory_in_namespace(&namespace, &parent, Path::new(&name), mode),
        Err(reason) => create_unverified(&parent, &name, mode, reason.to_string()),
    }
}
fn create_unverified(
    parent: &Root,
    name: &OsStr,
    mode: u32,
    reason: String,
) -> Result<DirectoryCreation, DirectoryCreateError> {
    let leaf = c_component(name, &parent.path)?;
    let mode = native_mode(mode).map_err(|error| ResolveError::io(&parent.path, &error))?;
    // No creation authority is inferred from mkdir followed by name resolution.
    let status = unsafe { libc::mkdirat(parent.descriptor(), leaf.as_ptr(), mode) };
    if status < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(ResolveError::io(&parent.path.join(name), &error).into());
        }
        return resolve_directory(parent, Path::new(name))
            .map(DirectoryCreation::Existing)
            .map_err(Into::into);
    }
    let directory = resolve_directory(parent, Path::new(name))?;
    Ok(DirectoryCreation::Unverified { directory, reason })
}

/// Publish a directory captured in an explicitly retained protected namespace.
/// The namespace must be the exact retained actual parent: unrelated staging
/// cannot reproduce every native directory inheritance policy. Different
/// parents create with ordinary mkdir semantics and return Unverified, never
/// a falsely owned receipt. Callers may acquire the actual protected parent.
pub fn create_directory_in_namespace(
    namespace: &DirectoryNamespace,
    root: &Root,
    relative: &Path,
    mode: u32,
) -> Result<DirectoryCreation, DirectoryCreateError> {
    create_with_hooks(namespace, root, relative, mode, |_, _| {}, |_, _| {})
}
fn create_with_hooks(
    namespace: &DirectoryNamespace,
    root: &Root,
    relative: &Path,
    mode: u32,
    after_capture: impl FnOnce(&Root, &OsStr),
    after_publish: impl FnOnce(&Root, &OsStr),
) -> Result<DirectoryCreation, DirectoryCreateError> {
    create_with_preparation_hook(
        namespace,
        root,
        relative,
        mode,
        |_, _| {},
        after_capture,
        after_publish,
    )
}
fn create_with_preparation_hook(
    namespace: &DirectoryNamespace,
    root: &Root,
    relative: &Path,
    mode: u32,
    after_stage: impl FnOnce(&Root, &OsStr),
    after_capture: impl FnOnce(&Root, &OsStr),
    after_publish: impl FnOnce(&Root, &OsStr),
) -> Result<DirectoryCreation, DirectoryCreateError> {
    // Borrowing an existing target never requires staging write permission.
    if let Some(existing) = existing_directory(root, relative)? {
        return Ok(DirectoryCreation::Existing(existing));
    }
    let (parent, name) = open_parent_beneath(root, relative)?;
    let parent_metadata = parent
        .metadata()
        .map_err(|e| ResolveError::io(&parent.path, &e))?;
    let namespace_metadata = namespace
        .directory
        .metadata()
        .map_err(|e| ResolveError::io(&namespace.directory.path, &e))?;
    if (parent_metadata.dev(), parent_metadata.ino())
        != (namespace_metadata.dev(), namespace_metadata.ino())
    {
        return create_unverified(&parent,&name,mode,
            "owned directory publication requires the exact protected destination parent; different-parent inheritance remains unverified".into());
    }
    let Some(stage) = prepare_private_stage(namespace, mode, after_stage)? else {
        return create_unverified(&parent,&name,mode,
            "internal owner-access repair could not preserve destination gid/SGID; creation ownership is unverified".into());
    };
    after_capture(&parent, &name);
    match stage.publish_to(parent, name.clone(), after_publish) {
        Ok(publication) => Ok(DirectoryCreation::Published(publication)),
        Err((failure, true)) if failure.recovery.is_none() => resolve_directory(root, relative)
            .map(DirectoryCreation::Existing)
            .map_err(Into::into),
        Err((failure, _)) => Err(failure),
    }
}

fn prepare_private_stage(
    namespace: &DirectoryNamespace,
    mode: u32,
    after_stage: impl FnOnce(&Root, &OsStr),
) -> Result<Option<DirectoryStage>, DirectoryCreateError> {
    let native_mode =
        native_mode(mode).map_err(|error| ResolveError::io(&namespace.directory.path, &error))?;
    let parent_metadata = namespace
        .directory
        .metadata()
        .map_err(|e| ResolveError::io(&namespace.directory.path, &e))?;
    namespace_protected(&namespace.directory)
        .map_err(|e| ResolveError::io(&namespace.directory.path, &e))?;
    // Retain the namespace before the first effect, including failures before
    // a payload/publication receipt exists.
    let mut preparation = DirectoryPreparation {
        namespace: DirectoryNamespace {
            directory: namespace
                .directory
                .try_clone()
                .map_err(|e| ResolveError::io(&namespace.directory.path, &e))?,
        },
        name: unused_name(&namespace.directory)?,
        outer: None,
        payload: None,
        payload_created: false,
        pending_mode: None,
        created: false,
        cleanup_error: None,
    };
    let leaf = c_component(&preparation.name, &namespace.directory.path)?;
    if unsafe { libc::mkdirat(namespace.directory.descriptor(), leaf.as_ptr(), 0o700) } != 0 {
        return Err(ResolveError::io(
            &namespace.directory.path.join(&preparation.name),
            &io::Error::last_os_error(),
        )
        .into());
    }
    preparation.created = true;
    after_stage(&namespace.directory, &preparation.name);
    let mut identity = None;
    let prepare = (|| -> Result<bool, ResolveError> {
        let outer = resolve_directory(&namespace.directory, Path::new(&preparation.name))?;
        preparation.outer = Some(outer);
        let outer = preparation.outer.as_ref().unwrap();
        let inherited = outer
            .metadata()
            .map_err(|e| ResolveError::io(&outer.path, &e))?;
        set_directory_permissions(outer, 0o700 | (inherited.mode() & 0o2000))
            .map_err(|e| ResolveError::io(&outer.path, &e))?;
        namespace_protected(outer).map_err(|e| ResolveError::io(&outer.path, &e))?;
        let repaired = outer
            .metadata()
            .map_err(|e| ResolveError::io(&outer.path, &e))?;
        if repaired.gid() != parent_metadata.gid()
            || repaired.mode() & 0o2000 != parent_metadata.mode() & 0o2000
        {
            return Ok(false);
        }
        if unsafe { libc::mkdirat(outer.descriptor(), c"directory".as_ptr(), native_mode) } != 0 {
            return Err(ResolveError::io(
                &outer.path.join("directory"),
                &io::Error::last_os_error(),
            ));
        }
        preparation.payload_created = true;
        let payload = resolve_directory(outer, Path::new("directory"))?;
        preparation.payload = Some(payload);
        let payload = preparation.payload.as_ref().unwrap();
        identity = Some(
            payload
                .metadata()
                .map_err(|e| ResolveError::io(&payload.path, &e))?,
        );
        Ok(true)
    })();
    match prepare {
        Err(error) => return Err(preparation.fail(error)),
        Ok(false) => {
            if let Err(error) = preparation.retry_cleanup() {
                return Err(preparation.fail(error));
            }
            return Ok(None);
        }
        Ok(true) => {}
    }
    Ok(Some(DirectoryStage {
        preparation,
        identity: identity.unwrap(),
    }))
}

/// A private payload retained before its requested public name exists. The
/// protected INTERNAL namespace assumption must hold for the entire staging
/// interval. This API alone does not confine programs that canonicalize cwd;
/// use a separately qualified fixed-mount command boundary for such programs.
#[derive(Debug)]
#[must_use = "retain the staged directory until publication or explicit cleanup"]
pub struct DirectoryStage {
    preparation: DirectoryPreparation,
    identity: Metadata,
}
impl DirectoryStage {
    pub fn directory(&self) -> &Root {
        self.preparation.payload.as_ref().expect("prepared payload")
    }
    pub fn initial_metadata(&self) -> &Metadata {
        &self.identity
    }
    pub fn identity(&self) -> (u64, u64) {
        (self.identity.dev(), self.identity.ino())
    }
    /// A failed/canceled operation retains its nonempty payload for explicit
    /// recovery; retry_cleanup removes only empty original internal entries.
    pub fn into_preparation(self) -> DirectoryPreparation {
        self.preparation
    }
    /// Publishes into the same exact retained destination parent. No-clobber
    /// failures return live staging authority when contents cannot be removed.
    /// Ancestor spelling verification and directory fsync remain caller duties.
    pub fn publish_new(
        self,
        root: &Root,
        relative: &Path,
    ) -> Result<DirectoryPublication, DirectoryCreateError> {
        let destination = (|| -> Result<_, ResolveError> {
            let (parent, name) = open_parent_beneath(root, relative)?;
            let expected = self
                .preparation
                .namespace
                .directory
                .metadata()
                .map_err(|e| ResolveError::io(&self.preparation.namespace.directory.path, &e))?;
            let actual = parent
                .metadata()
                .map_err(|e| ResolveError::io(&parent.path, &e))?;
            if (expected.dev(), expected.ino()) != (actual.dev(), actual.ino()) {
                return Err(ResolveError::message(
                    &parent.path,
                    "staged directory publication requires the original exact parent; no cross-filesystem or inheritance fallback",
                ));
            }
            namespace_protected(&self.preparation.namespace.directory)
                .map_err(|e| ResolveError::io(&self.preparation.namespace.directory.path, &e))?;
            Ok((parent, name))
        })();
        let (parent, name) = match destination {
            Ok(destination) => destination,
            Err(error) => return Err(self.preparation.fail(error)),
        };
        self.publish_to(parent, name, |_, _| {})
            .map_err(|(error, _)| error)
    }
    fn publish_to(
        self,
        parent: Root,
        name: OsString,
        after_publish: impl FnOnce(&Root, &OsStr),
    ) -> Result<DirectoryPublication, (DirectoryCreateError, bool)> {
        self.publish_to_with_restore(parent, name, after_publish, |root, pending| {
            pending.restore(root)
        })
    }
    fn publish_to_with_restore(
        self,
        parent: Root,
        name: OsString,
        after_publish: impl FnOnce(&Root, &OsStr),
        restore_mode: impl FnOnce(&Root, ModeRestoration) -> io::Result<()>,
    ) -> Result<DirectoryPublication, (DirectoryCreateError, bool)> {
        let DirectoryStage {
            mut preparation,
            identity,
        } = self;
        // Linux requires write permission on a directory when changing its '..'
        // parent during rename. Widen only owner-write on the retained private
        // payload, then restore its exact observed mode through the same descriptor.
        // No group/other grants or POSIX ACL mask bits are widened.
        let payload = preparation.payload.as_ref().unwrap();
        let metadata = match payload.metadata() {
            Ok(metadata) => metadata,
            Err(error) => {
                return Err((
                    preparation.fail(ResolveError::io(&parent.path.join(&name), &error)),
                    false,
                ));
            }
        };
        let mode = metadata.mode() & 0o7777;
        if mode & 0o200 == 0 {
            preparation.pending_mode = Some(ModeRestoration {
                original: mode,
                temporary: mode | 0o200,
                uid: metadata.uid(),
                gid: metadata.gid(),
            });
            if let Err(error) = set_directory_permissions(payload, mode | 0o200) {
                return Err((
                    preparation.fail(ResolveError::io(&parent.path.join(&name), &error)),
                    false,
                ));
            }
            let expected = preparation.pending_mode.unwrap();
            let checked=payload.metadata().and_then(|current| {
            if current.mode()&0o7777!=expected.temporary || current.uid()!=expected.uid || current.gid()!=expected.gid {
                Err(io::Error::other("kernel did not retain exact temporary mode/ownership; refusing publication"))
            } else {Ok(())}
        });
            if let Err(error) = checked {
                return Err((
                    preparation.fail(ResolveError::io(&parent.path.join(&name), &error)),
                    false,
                ));
            }
        }
        let outer = preparation.outer.as_ref().unwrap();
        if let Err(error) = rename_exclusive(outer, OsStr::new("directory"), &parent, &name) {
            let exists = error.kind() == io::ErrorKind::AlreadyExists;
            return Err((
                preparation.fail(ResolveError::io(&parent.path.join(&name), &error)),
                exists,
            ));
        }
        // From here onward the published receipt must retain known identity even
        // when private outer cleanup fails.
        preparation.payload_created = false;
        let mut directory = preparation.payload.take().unwrap();
        let outer = preparation.outer.take().unwrap();
        let stage = preparation.name;
        let retained_namespace = preparation.namespace;
        directory.path = parent.path.join(&name);
        after_publish(&parent, &name);
        let mode_restore = preparation.pending_mode.and_then(|mode| {
            restore_mode(&directory, mode)
                .err()
                .map(|error| (mode, error))
        });
        let pending_mode = mode_restore.as_ref().map(|(mode, _)| *mode);
        // Nothing after successful publication can erase the known identity from
        // the returned receipt. Cleanup failure retains the private staging root.
        let cleanup = remove_directory_within(&retained_namespace.directory, &stage);
        let (mut publication_error, private_staging) = match cleanup {
            Ok(()) => (None, None),
            Err(error) => (Some(error.to_string()), Some((outer, stage))),
        };
        if let Some((_, error)) = mode_restore {
            publication_error = Some(match publication_error {
                Some(cleanup) => format!(
                    "payload mode restoration failed: {error}; private staging cleanup failed: {cleanup}"
                ),
                None => format!("payload mode restoration failed: {error}"),
            });
        }
        Ok(DirectoryPublication {
            directory,
            parent,
            name,
            namespace: retained_namespace,
            identity,
            publication_error,
            pending_mode,
            private_staging,
            quarantined: None,
            removed: false,
        })
    }
}

/// Prepare a payload using the exact parent's native mkdir inheritance. No
/// requested public name is created. Unsupported ownership policy fails
/// explicitly; there is no unverified staging fallback.
pub fn stage_directory_in_namespace(
    namespace: &DirectoryNamespace,
    mode: u32,
) -> Result<DirectoryStage, DirectoryCreateError> {
    prepare_private_stage(namespace, mode, |_, _| {})?.ok_or_else(|| {
        ResolveError::message(
            &namespace.directory.path,
            "private directory staging could not preserve actual parent gid/SGID",
        )
        .into()
    })
}

fn restore_directory_mode(directory: &Root, mode: u32) -> io::Result<()> {
    set_directory_permissions(directory, mode)?;
    if directory.metadata()?.mode() & 0o7777 != mode {
        return Err(io::Error::other(
            "kernel did not retain the original directory mode (including SGID)",
        ));
    }
    Ok(())
}

fn existing_directory(root: &Root, relative: &Path) -> Result<Option<Root>, ResolveError> {
    match resolve_directory(root, relative) {
        Ok(existing) => Ok(Some(existing)),
        Err(error) if error.is_not_found() => Ok(None),
        Err(error) => Err(error),
    }
}
fn rename_exclusive(source: &Root, from: &OsStr, destination: &Root, to: &OsStr) -> io::Result<()> {
    let from = c_name(from)?;
    let to = c_name(to)?;
    // SAFETY: descriptors are retained and names are valid leaf C strings.
    #[cfg(target_os = "linux")]
    let status = unsafe {
        libc::renameat2(
            source.descriptor(),
            from.as_ptr(),
            destination.descriptor(),
            to.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let status = unsafe {
        libc::renameatx_np(
            source.descriptor(),
            from.as_ptr(),
            destination.descriptor(),
            to.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    return Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "exclusive directory publication unavailable",
    ));
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    if status == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}
fn unused_name(namespace: &Root) -> Result<OsString, ResolveError> {
    // No existence check is authority. Every actual mkdir/rename is exclusive.
    temporary_name().map_err(|e| ResolveError::io(&namespace.path, &e))
}
fn namespace_protected(root: &Root) -> io::Result<()> {
    let metadata = root.metadata()?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o022 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory creation ownership requires an owned namespace without group/other write access",
        ));
    }
    if !native_acl_absent(root)? {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "directory creation ownership requires proven native ACL absence",
        ));
    }
    Ok(())
}
#[cfg(target_os = "linux")]
fn native_acl_absent(_: &Root) -> io::Result<bool> {
    Ok(true)
}
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn native_acl_absent(_: &Root) -> io::Result<bool> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "protected creation namespaces are unavailable on this platform",
    ))
}
#[cfg(target_os = "macos")]
fn native_acl_absent(root: &Root) -> io::Result<bool> {
    unsafe extern "C" {
        fn filesec_init() -> *mut libc::c_void;
        fn filesec_free(value: *mut libc::c_void);
        fn filesec_get_property(
            value: *mut libc::c_void,
            property: libc::c_int,
            output: *mut libc::c_void,
        ) -> libc::c_int;
        #[cfg_attr(not(target_arch = "aarch64"), link_name = "fstatx_np$INODE64")]
        fn fstatx_np(
            fd: libc::c_int,
            metadata: *mut libc::stat,
            security: *mut libc::c_void,
        ) -> libc::c_int;
        fn acl_free(value: *mut libc::c_void) -> libc::c_int;
    }
    struct Security(*mut libc::c_void);
    impl Drop for Security {
        fn drop(&mut self) {
            unsafe { filesec_free(self.0) }
        }
    }
    let raw = unsafe { filesec_init() };
    if raw.is_null() {
        return Err(io::Error::last_os_error());
    }
    let security = Security(raw);
    let mut stat = std::mem::MaybeUninit::<libc::stat>::uninit();
    if unsafe { fstatx_np(root.descriptor(), stat.as_mut_ptr(), security.0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut acl: *mut libc::c_void = ptr::null_mut();
    let status =
        unsafe { filesec_get_property(security.0, 5, (&mut acl as *mut *mut libc::c_void).cast()) };
    if status == 0 {
        if !acl.is_null() && acl as usize != 1 {
            unsafe { acl_free(acl) };
        }
        return Ok(acl as usize == 1);
    }
    let error = io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        Ok(true)
    } else {
        Err(error)
    }
}

/// A real producer failure fixture for carrier consumer tests. It goes through
/// the same mkdir/preparation/error-retention path; no authority is fabricated.
#[cfg(test)]
pub(crate) fn fixture_directory_preparation_failure(root: &Root) -> DirectoryCreateError {
    let namespace = DirectoryNamespace::acquire(root).expect("protected failure fixture root");
    let target = temporary_name().expect("fixture target name");
    create_with_preparation_hook(
        &namespace,
        root,
        Path::new(&target),
        0o700,
        |parent, name| {
            std::fs::write(
                parent.path.join(name).join("directory"),
                b"retained fixture artifact",
            )
            .expect("fixture interference")
        },
        |_, _| panic!("fixture must fail before publication"),
        |_, _| panic!("fixture must not publish"),
    )
    .unwrap_err()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{DirBuilderExt, PermissionsExt},
    };
    fn sticky_fixture() -> (tempfile::TempDir, Arc<StickyParent>) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o1777))
            .unwrap();
        let root = Arc::new(Root::open(directory.path()).unwrap());
        let parent = StickyParent::acquire(root).unwrap();
        (directory, parent)
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sticky_parent_effective_access_checks_the_retained_original_before_creation() {
        // A root process may bypass DAC checks. This fixture needs no chown or
        // privileged child, so exercise denial under an ordinary effective UID.
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let fixture = tempfile::tempdir().unwrap();
        let original = fixture.path().join("sticky");
        let moved = fixture.path().join("moved");
        std::fs::create_dir(&original).unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o1555)).unwrap();
        let retained = Arc::new(Root::open(&original).unwrap());
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        std::fs::set_permissions(&original, std::fs::Permissions::from_mode(0o1777)).unwrap();
        let error = StickyParent::acquire(retained).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(std::fs::read_dir(&moved).unwrap().count(), 0);
        assert_eq!(std::fs::read_dir(&original).unwrap().count(), 0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sibling_directory_confirmation_is_strict_generally_and_typed_for_save() {
        for failure_on in [1, 2] {
            for policy in [
                SiblingPreparationPolicy::Strict,
                SiblingPreparationPolicy::SaveLiveCustody,
            ] {
                let (_fixture, parent) = sticky_fixture();
                let mut calls = 0;
                let result = parent.create_named_using(
                    temporary_name().unwrap(),
                    |_, _| Ok(()),
                    policy,
                    |_| {
                        calls += 1;
                        if calls == failure_on {
                            Err(io::Error::other("injected sibling directory sync failure"))
                        } else {
                            Ok(())
                        }
                    },
                );
                match policy {
                    SiblingPreparationPolicy::Strict => {
                        let PrivateSiblingCreateError::Retained { mut sibling, .. } =
                            result.unwrap_err()
                        else {
                            panic!("strict directory-sync failure must retain original sibling");
                        };
                        assert_eq!(calls, failure_on);
                        sibling.validate().unwrap();
                        sibling.cleanup_empty().unwrap();
                        assert!(sibling.is_resolved());
                    }
                    SiblingPreparationPolicy::SaveLiveCustody => {
                        let (mut sibling, durability) = result.unwrap();
                        assert_eq!(calls, 2, "Save observes both directory confirmations");
                        sibling.validate().unwrap();
                        assert!(
                            matches!(&durability, SaveSiblingDurability::Unconfirmed(error)
                            if error.to_string() == "injected sibling directory sync failure")
                        );
                        sync_directory(sibling.directory().unwrap()).unwrap();
                        sync_directory(&sibling.parent.directory).unwrap();
                        assert!(
                            matches!(durability, SaveSiblingDurability::Unconfirmed(_)),
                            "later sync success does not rewrite preparation proof"
                        );
                        assert!(matches!(
                            sibling.cleanup_save_empty().unwrap(),
                            SaveSiblingDurability::Confirmed
                        ));
                        assert!(sibling.is_removed());
                        assert!(sibling.is_resolved());
                    }
                }
            }
        }
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn save_sibling_incomplete_acquisition_and_foreign_entry_failures_remain_retained() {
        let (_fixture, parent) = sticky_fixture();
        let error = parent
            .create_named_using(
                temporary_name().unwrap(),
                |_, _| Err(io::Error::other("initial acquisition unavailable")),
                SiblingPreparationPolicy::SaveLiveCustody,
                |_| panic!("incomplete fresh qualification cannot become a sync warning"),
            )
            .unwrap_err();
        let PrivateSiblingCreateError::Retained { mut sibling, .. } = error else {
            panic!("post-mkdir failure must retain original custody");
        };
        assert_eq!(sibling.retained_descriptor_count(), 1);
        assert!(matches!(
            sibling.cleanup_save_empty().unwrap(),
            SaveSiblingDurability::Confirmed
        ));
        assert!(sibling.is_removed());

        let (_fixture, parent) = sticky_fixture();
        let error = parent
            .create_named_using(
                temporary_name().unwrap(),
                |parent, name| {
                    std::fs::remove_dir(parent.path.join(name)).unwrap();
                    std::os::unix::fs::symlink("foreign-target", parent.path.join(name)).unwrap();
                    Ok(())
                },
                SiblingPreparationPolicy::SaveLiveCustody,
                |_| panic!("foreign entry cannot become a sync warning"),
            )
            .unwrap_err();
        let PrivateSiblingCreateError::Retained { mut sibling, .. } = error else {
            panic!("foreign entry must retain original custody for explicit resolution");
        };
        assert!(sibling.cleanup_save_empty().is_err());
        assert!(!sibling.is_removed());
        assert!(
            std::fs::symlink_metadata(sibling.diagnostic_path())
                .unwrap()
                .is_symlink()
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn save_sibling_cleanup_softens_only_sync_after_positive_removal() {
        let (_fixture, parent) = sticky_fixture();
        let mut sibling = parent.create_private_sibling().unwrap();
        let original = sibling.diagnostic_path();
        let durability = sibling
            .cleanup_save_with_hooks(
                || {},
                |_| Err(io::Error::other("removed sibling parent sync unavailable")),
            )
            .unwrap();
        assert!(matches!(durability, SaveSiblingDurability::Unconfirmed(_)));
        assert!(sibling.is_removed());
        assert!(
            !sibling.is_resolved(),
            "strict resolution still requires parent sync"
        );
        assert!(!original.exists());
        std::fs::create_dir(&original).unwrap();
        assert!(matches!(
            sibling.cleanup_save_empty().unwrap(),
            SaveSiblingDurability::Confirmed
        ));
        assert!(sibling.is_resolved());
        assert!(
            original.is_dir(),
            "sync-only retry must preserve new original-name entry"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn save_sibling_cleanup_never_softens_nonempty_or_replacement_errors() {
        let (fixture, parent) = sticky_fixture();
        let mut sibling = parent.create_private_sibling().unwrap();
        let original = sibling.diagnostic_path();
        std::fs::write(original.join("leaf"), b"preserve").unwrap();
        assert!(
            sibling
                .cleanup_save_with_hooks(
                    || {},
                    |_| { panic!("nonempty removal must fail before parent sync") }
                )
                .is_err()
        );
        assert!(!sibling.is_removed());
        assert_eq!(std::fs::read(original.join("leaf")).unwrap(), b"preserve");
        std::fs::remove_file(original.join("leaf")).unwrap();
        let moved = fixture.path().join("moved-original");
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();
        assert!(
            sibling
                .cleanup_save_with_hooks(
                    || {},
                    |_| { panic!("replacement identity must fail before parent sync") }
                )
                .is_err()
        );
        assert!(!sibling.is_removed());
        assert!(moved.is_dir());
        assert!(original.is_dir());
        std::fs::remove_dir(&original).unwrap();
        std::fs::rename(&moved, &original).unwrap();
        assert!(matches!(
            sibling.cleanup_save_empty().unwrap(),
            SaveSiblingDurability::Confirmed
        ));
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sticky_private_sibling_is_owned_without_broadening_namespace_or_publication() {
        let (fixture, parent) = sticky_fixture();
        assert!(DirectoryNamespace::acquire(&parent.directory).is_err());
        assert_eq!(parent.retained_descriptor_count(), 1);
        let mut sibling = parent.clone().create_private_sibling().unwrap();
        assert_eq!(sibling.retained_descriptor_count(), 2);
        assert_eq!(
            sibling.directory().unwrap().metadata().unwrap().mode() & 0o7777,
            0o700
        );
        let child = sibling.directory_arc().unwrap();
        assert_eq!(
            child.descriptor(),
            sibling.directory().unwrap().descriptor()
        );
        DirectoryNamespace::acquire(&child).unwrap();
        sibling.cleanup_empty().unwrap();
        assert!(sibling.is_resolved());
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sticky_private_sibling_never_adopts_an_existing_entry() {
        let (fixture, parent) = sticky_fixture();
        let name = OsString::from("occupied");
        std::fs::create_dir(fixture.path().join(&name)).unwrap();
        std::fs::write(fixture.path().join(&name).join("foreign"), b"preserve").unwrap();
        let error = parent
            .create_named_with_hook(name, |_, _| Ok(()))
            .unwrap_err();
        assert!(
            matches!(error, PrivateSiblingCreateError::Rejected(ref error)
            if error.kind() == io::ErrorKind::AlreadyExists)
        );
        assert_eq!(
            std::fs::read(fixture.path().join("occupied/foreign")).unwrap(),
            b"preserve"
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sticky_private_sibling_partial_acquisition_retains_cleanup_custody() {
        let (fixture, parent) = sticky_fixture();
        let error = parent
            .create_with_hook(|_, _| Err(io::Error::other("initial open unavailable")))
            .unwrap_err();
        let PrivateSiblingCreateError::Retained { mut sibling, .. } = error else {
            panic!("post-mkdir failure must retain original custody");
        };
        assert_eq!(sibling.retained_descriptor_count(), 1);
        assert!(sibling.directory().is_err());
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 1);
        sibling.cleanup_empty().unwrap();
        assert!(sibling.is_resolved());
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sticky_private_sibling_partial_custody_refuses_a_symlink_replacement() {
        let (fixture, parent) = sticky_fixture();
        let error = parent
            .create_with_hook(|parent, name| {
                std::fs::remove_dir(parent.path.join(name))?;
                std::os::unix::fs::symlink("foreign-target", parent.path.join(name))?;
                Err(io::Error::other("initial acquisition interrupted"))
            })
            .unwrap_err();
        let PrivateSiblingCreateError::Retained { mut sibling, .. } = error else {
            panic!("post-mkdir failure must retain custody");
        };
        assert!(sibling.cleanup_empty().is_err());
        assert_eq!(sibling.retained_descriptor_count(), 1);
        assert!(
            std::fs::symlink_metadata(sibling.diagnostic_path())
                .unwrap()
                .is_symlink()
        );
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 1);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sticky_private_sibling_repairs_restrictive_creation_mode_on_original_only() {
        let (_fixture, parent) = sticky_fixture();
        let mut sibling = parent
            .create_with_hook(|parent, name| {
                let directory =
                    resolve_directory(parent, Path::new(name)).map_err(io::Error::other)?;
                set_directory_permissions(&directory, 0)
            })
            .unwrap();
        assert_eq!(
            sibling.directory().unwrap().metadata().unwrap().mode() & 0o7777,
            0o700
        );
        sibling.cleanup_empty().unwrap();
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sticky_private_sibling_refuses_changed_parent_proof() {
        let (fixture, parent) = sticky_fixture();
        std::fs::set_permissions(fixture.path(), std::fs::Permissions::from_mode(0o777)).unwrap();
        assert!(matches!(
            parent.create_private_sibling(),
            Err(PrivateSiblingCreateError::Rejected(_))
        ));
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sticky_private_sibling_retains_nonempty_original_until_explicit_retry() {
        let (_fixture, parent) = sticky_fixture();
        let mut sibling = parent.create_private_sibling().unwrap();
        let file = sibling.diagnostic_path().join("leaf");
        std::fs::write(&file, b"preserve").unwrap();
        assert!(sibling.cleanup_empty().is_err());
        assert_eq!(std::fs::read(&file).unwrap(), b"preserve");
        assert!(!sibling.is_resolved());
        std::fs::remove_file(file).unwrap();
        sibling.cleanup_empty().unwrap();
        assert!(sibling.is_resolved());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sticky_private_sibling_preserves_replacement_and_retains_original_fd() {
        let (fixture, parent) = sticky_fixture();
        let mut sibling = parent.create_private_sibling().unwrap();
        let name = sibling.diagnostic_path();
        let moved = fixture.path().join("original-moved");
        std::fs::rename(&name, &moved).unwrap();
        std::fs::create_dir(&name).unwrap();
        assert!(sibling.cleanup_empty().is_err());
        assert!(name.is_dir());
        assert!(moved.is_dir());
        std::fs::remove_dir(&name).unwrap();
        std::fs::rename(&moved, &name).unwrap();
        sibling.cleanup_empty().unwrap();
        assert!(sibling.is_resolved());
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sticky_private_sibling_quarantine_collision_is_retained_and_retryable() {
        let (fixture, parent) = sticky_fixture();
        let mut sibling = parent.create_private_sibling().unwrap();
        let original = sibling.diagnostic_path();
        std::fs::write(original.join("leaf"), b"preserve").unwrap();
        assert!(
            sibling
                .cleanup_with_hooks(|| std::fs::create_dir(&original).unwrap(), sync_directory)
                .is_err()
        );
        let quarantine = sibling.diagnostic_path();
        assert_ne!(quarantine, original);
        assert_eq!(std::fs::read(quarantine.join("leaf")).unwrap(), b"preserve");
        std::fs::remove_file(quarantine.join("leaf")).unwrap();
        sibling.cleanup_empty().unwrap();
        assert!(sibling.is_resolved());
        assert!(original.is_dir(), "no-clobber collision entry is preserved");
        std::fs::remove_dir(original).unwrap();
        assert_eq!(std::fs::read_dir(fixture.path()).unwrap().count(), 0);
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn sticky_private_sibling_parent_sync_failure_retries_without_reopening_removed_name() {
        let (_fixture, parent) = sticky_fixture();
        let mut sibling = parent.create_private_sibling().unwrap();
        let original = sibling.diagnostic_path();
        assert!(
            sibling
                .cleanup_with_hooks(|| {}, |_| Err(io::Error::other("parent sync unavailable")))
                .is_err()
        );
        assert!(!sibling.is_resolved());
        assert!(!original.exists());
        std::fs::create_dir(&original).unwrap();
        sibling.cleanup_empty().unwrap();
        assert!(sibling.is_resolved());
        assert!(original.is_dir());
    }

    fn published(result: DirectoryCreation) -> DirectoryPublication {
        match result {
            DirectoryCreation::Published(p) => p,
            other => panic!("expected published receipt, got {other:?}"),
        }
    }
    #[test]
    fn publication_retains_original_before_public_target_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let namespace = DirectoryNamespace::acquire(&root).unwrap();
        let receipt = published(
            create_with_hooks(
                &namespace,
                &root,
                Path::new("target"),
                0o750,
                |_, _| {},
                |_, _| {
                    fs::rename(dir.path().join("target"), dir.path().join("retained")).unwrap();
                    fs::create_dir(dir.path().join("target")).unwrap();
                    fs::set_permissions(
                        dir.path().join("target"),
                        fs::Permissions::from_mode(0o711),
                    )
                    .unwrap();
                },
            )
            .unwrap(),
        );
        assert!(receipt.publication_error().is_none());
        assert_eq!(
            receipt.directory().metadata().unwrap().ino(),
            fs::metadata(dir.path().join("retained")).unwrap().ino()
        );
        assert_eq!(
            fs::metadata(dir.path().join("retained")).unwrap().mode() & 0o777,
            receipt.initial_metadata().mode() & 0o777
        );
        assert_eq!(
            fs::metadata(dir.path().join("target")).unwrap().mode() & 0o777,
            0o711
        );
    }
    #[test]
    fn occupied_public_target_is_existing_never_owned() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let namespace = DirectoryNamespace::acquire(&root).unwrap();
        let result = create_with_hooks(
            &namespace,
            &root,
            Path::new("target"),
            0o777,
            |_, _| {
                fs::create_dir(dir.path().join("target")).unwrap();
                fs::set_permissions(dir.path().join("target"), fs::Permissions::from_mode(0o710))
                    .unwrap();
            },
            |_, _| panic!("occupied target must not publish"),
        )
        .unwrap();
        assert!(matches!(result, DirectoryCreation::Existing(_)));
        assert_eq!(
            fs::metadata(dir.path().join("target")).unwrap().mode() & 0o777,
            0o710
        );
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn shared_namespace_returns_unverified_without_permission_claim() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o777)).unwrap();
        let root = Root::open(dir.path()).unwrap();
        assert!(DirectoryNamespace::acquire(&root).is_err());
        assert!(matches!(
            create_directory_beneath(&root, Path::new("target"), 0o700).unwrap(),
            DirectoryCreation::Unverified { .. }
        ));
    }
    #[test]
    fn cleanup_restores_replacement_instead_of_removing_it() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let mut receipt =
            published(create_directory_beneath(&root, Path::new("target"), 0o700).unwrap());
        fs::rename(dir.path().join("target"), dir.path().join("original")).unwrap();
        fs::create_dir(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target/foreign"), b"preserve").unwrap();
        assert!(receipt.cleanup_empty().is_err());
        assert!(receipt.recovery_path().is_none());
        assert_eq!(
            fs::read(dir.path().join("target/foreign")).unwrap(),
            b"preserve"
        );
        assert!(dir.path().join("original").is_dir());
    }
    #[test]
    fn cleanup_checks_after_acquisition_and_removes_only_exact_empty_original() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let mut receipt =
            published(create_directory_beneath(&root, Path::new("target"), 0o700).unwrap());
        receipt.cleanup_empty().unwrap();
        assert!(receipt.is_removed());
        assert!(!dir.path().join("target").exists());
        receipt.cleanup_empty().unwrap();
    }
    #[test]
    fn cleanup_late_replacement_is_preserved_and_restored() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let mut receipt =
            published(create_directory_beneath(&root, Path::new("target"), 0o700).unwrap());
        assert!(
            receipt
                .cleanup_empty_with_hooks(
                    || {
                        fs::rename(dir.path().join("target"), dir.path().join("original")).unwrap();
                        fs::create_dir(dir.path().join("target")).unwrap();
                    },
                    || {}
                )
                .is_err()
        );
        assert!(dir.path().join("target").is_dir());
        assert!(dir.path().join("original").is_dir());
    }
    #[test]
    fn different_parent_preserves_native_creation_as_unverified() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("shared")).unwrap();
        fs::set_permissions(
            dir.path().join("shared"),
            fs::Permissions::from_mode(0o2777),
        )
        .unwrap();
        let root = Root::open(dir.path()).unwrap();
        let namespace = DirectoryNamespace::acquire(&root).unwrap();
        let shared = open_directory_beneath(&root, Path::new("shared")).unwrap();
        let result =
            create_directory_in_namespace(&namespace, &shared, Path::new("target"), 0o750).unwrap();
        assert!(matches!(result, DirectoryCreation::Unverified { .. }));
        fs::DirBuilder::new()
            .mode(0o750)
            .create(dir.path().join("shared/native-reference"))
            .unwrap();
        let actual = fs::metadata(dir.path().join("shared/target")).unwrap();
        let reference = fs::metadata(dir.path().join("shared/native-reference")).unwrap();
        assert_eq!(actual.gid(), reference.gid());
        assert_eq!(actual.mode() & 0o7777, reference.mode() & 0o7777);
    }
    #[test]
    fn same_parent_published_payload_preserves_native_gid_and_setgid() {
        let dir = tempfile::tempdir().unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o2755)).unwrap();
        let root = Root::open(dir.path()).unwrap();
        if root.metadata().unwrap().mode() & 0o2000 == 0 {
            eprintln!(
                "SGID fixture unavailable: kernel cleared requested flag (group membership/platform policy)"
            );
            return;
        }
        fs::create_dir(dir.path().join("native-reference")).unwrap();
        let receipt =
            published(create_directory_beneath(&root, Path::new("target"), 0o777).unwrap());
        let reference = fs::metadata(dir.path().join("native-reference")).unwrap();
        let actual = receipt.directory().metadata().unwrap();
        assert_eq!(actual.gid(), reference.gid());
        assert_eq!(actual.mode() & 0o7777, reference.mode() & 0o7777);
    }
    #[cfg(target_os = "linux")]
    #[test]
    fn same_parent_published_payload_preserves_native_default_acl() {
        let dir = tempfile::tempdir().unwrap();
        let cpath = CString::new(dir.path().as_os_str().as_bytes()).unwrap();
        // Linux POSIX ACL xattr v2, independently seeded through the path API:
        // user_obj=rwx, group_obj=r-x, other=---. Not a production writer seam.
        let mut acl = 2u32.to_le_bytes().to_vec();
        for (tag, permissions) in [(1u16, 7u16), (4, 5), (32, 0)] {
            acl.extend(tag.to_le_bytes());
            acl.extend(permissions.to_le_bytes());
            acl.extend(u32::MAX.to_le_bytes());
        }
        if unsafe {
            libc::setxattr(
                cpath.as_ptr(),
                c"system.posix_acl_default".as_ptr(),
                acl.as_ptr().cast(),
                acl.len(),
                0,
            )
        } != 0
        {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::EOPNOTSUPP) {
                eprintln!("default ACL fixture unavailable on this filesystem: {error}");
                return;
            }
            panic!("seed independent native default ACL: {error}");
        }
        fn observed(path: &Path) -> Vec<u8> {
            let path = CString::new(path.as_os_str().as_bytes()).unwrap();
            let mut bytes = vec![0; 65536];
            let size = unsafe {
                libc::getxattr(
                    path.as_ptr(),
                    c"system.posix_acl_default".as_ptr(),
                    bytes.as_mut_ptr().cast(),
                    bytes.len(),
                )
            };
            assert!(
                size > 0,
                "fixture must retain an actual default ACL: {}",
                io::Error::last_os_error()
            );
            bytes.truncate(size as usize);
            bytes
        }
        assert_eq!(observed(dir.path()), acl);
        fs::create_dir(dir.path().join("native-reference")).unwrap();
        let root = Root::open(dir.path()).unwrap();
        let receipt =
            published(create_directory_beneath(&root, Path::new("target"), 0o777).unwrap());
        let reference = fs::metadata(dir.path().join("native-reference")).unwrap();
        let actual = receipt.directory().metadata().unwrap();
        assert_eq!(actual.gid(), reference.gid());
        assert_eq!(actual.mode() & 0o7777, reference.mode() & 0o7777);
        assert_eq!(
            observed(&dir.path().join("target")),
            observed(&dir.path().join("native-reference"))
        );
    }
    #[cfg(target_os = "macos")]
    #[test]
    fn darwin_present_native_acl_is_not_private_namespace_authority() {
        let dir = tempfile::tempdir().unwrap();
        let identity = std::process::Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .unwrap();
        assert!(identity.status.success());
        let username = String::from_utf8(identity.stdout).unwrap();
        let username = username.trim_end_matches(['\n', '\r']);
        assert!(!username.is_empty() && !username.contains([':', '\n', '\r']));
        assert!(
            std::process::Command::new("/bin/chmod")
                .args(["+a", &format!("user:{username}:allow:readattr")])
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        let root = Root::open(dir.path()).unwrap();
        assert!(!native_acl_absent(&root).unwrap());
        assert!(DirectoryNamespace::acquire(&root).is_err());
        assert!(
            std::process::Command::new("/bin/chmod")
                .arg("-N")
                .arg(dir.path())
                .status()
                .unwrap()
                .success()
        );
        assert!(native_acl_absent(&root).unwrap());
        assert!(DirectoryNamespace::acquire(&root).is_ok());
    }
    #[test]
    fn existing_target_in_read_only_root_needs_no_staging_write() {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir(dir.path().join("existing")).unwrap();
        let root = Root::open(dir.path()).unwrap();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let result = create_directory_beneath(&root, Path::new("existing"), 0o777);
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(result.unwrap(), DirectoryCreation::Existing(_)));
    }
    #[test]
    fn existing_target_does_not_need_explicit_namespace_write() {
        let namespace_dir = tempfile::tempdir().unwrap();
        let parent_dir = tempfile::tempdir().unwrap();
        fs::create_dir(parent_dir.path().join("existing")).unwrap();
        let namespace_root = Root::open(namespace_dir.path()).unwrap();
        let namespace = DirectoryNamespace::acquire(&namespace_root).unwrap();
        let parent = Root::open(parent_dir.path()).unwrap();
        fs::set_permissions(namespace_dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
        let result =
            create_directory_in_namespace(&namespace, &parent, Path::new("existing"), 0o777);
        fs::set_permissions(namespace_dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(matches!(result.unwrap(), DirectoryCreation::Existing(_)));
    }
    #[test]
    fn cleanup_restores_nonempty_original_without_removing_contents() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let mut receipt =
            published(create_directory_beneath(&root, Path::new("target"), 0o700).unwrap());
        fs::write(dir.path().join("target/keep"), b"original").unwrap();
        assert!(receipt.cleanup_empty().is_err());
        assert!(receipt.recovery_path().is_none());
        assert_eq!(
            fs::read(dir.path().join("target/keep")).unwrap(),
            b"original"
        );
        assert!(!receipt.is_removed());
    }
    #[test]
    fn cleanup_restore_collision_preserves_quarantined_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let mut receipt =
            published(create_directory_beneath(&root, Path::new("target"), 0o700).unwrap());
        fs::rename(dir.path().join("target"), dir.path().join("original")).unwrap();
        fs::create_dir(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target/foreign"), b"foreign").unwrap();
        let result = receipt.cleanup_empty_with_hooks(
            || {},
            || {
                fs::create_dir(dir.path().join("target")).unwrap();
                fs::write(dir.path().join("target/newer"), b"newer").unwrap();
            },
        );
        assert!(result.is_err());
        assert_eq!(
            fs::read(receipt.recovery_path().unwrap().join("foreign")).unwrap(),
            b"foreign"
        );
        assert_eq!(fs::read(dir.path().join("target/newer")).unwrap(), b"newer");
        assert!(dir.path().join("original").is_dir());
        assert!(
            receipt.cleanup_empty().is_err(),
            "unresolved acquired entry cannot be replayed"
        );
        assert!(
            receipt.resolve_recovery().is_err(),
            "collision remains authoritative"
        );
        fs::remove_file(dir.path().join("target/newer")).unwrap();
        fs::remove_dir(dir.path().join("target")).unwrap();
        assert_eq!(
            receipt.resolve_recovery().unwrap(),
            Some(DirectoryRecovery::ReplacementRestored)
        );
        assert!(receipt.recovery_path().is_none());
        assert_eq!(
            fs::read(dir.path().join("target/foreign")).unwrap(),
            b"foreign"
        );
        assert!(
            !receipt.is_removed(),
            "restoring a replacement never claims original cleanup"
        );
    }
    #[test]
    fn retained_nonempty_original_can_restore_then_retry_after_contents_resolved() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let mut receipt =
            published(create_directory_beneath(&root, Path::new("target"), 0o700).unwrap());
        fs::write(dir.path().join("target/keep"), b"owned").unwrap();
        assert!(
            receipt
                .cleanup_empty_with_hooks(
                    || {},
                    || {
                        fs::create_dir(dir.path().join("target")).unwrap();
                    }
                )
                .is_err()
        );
        fs::remove_dir(dir.path().join("target")).unwrap();
        assert_eq!(
            receipt.resolve_recovery().unwrap(),
            Some(DirectoryRecovery::OriginalRestored)
        );
        assert_eq!(fs::read(dir.path().join("target/keep")).unwrap(), b"owned");
        assert!(!receipt.is_removed());
        fs::remove_file(dir.path().join("target/keep")).unwrap();
        receipt.cleanup_empty().unwrap();
        assert!(receipt.is_removed());
    }
    #[test]
    fn postpublication_cleanup_error_retains_cached_identity_and_stage() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        } // EACCES fixture requires an unprivileged caller.
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let namespace = DirectoryNamespace::acquire(&root).unwrap();
        let result = create_with_hooks(
            &namespace,
            &root,
            Path::new("target"),
            0o700,
            |_, _| {},
            |_, _| {
                fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
            },
        );
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let mut receipt = published(result.unwrap());
        assert!(receipt.publication_error().is_some());
        assert!(receipt.pending_staging_path().unwrap().is_dir());
        let actual = fs::metadata(dir.path().join("target")).unwrap();
        assert_eq!(receipt.identity(), (actual.dev(), actual.ino()));
        assert_eq!(receipt.initial_metadata().ino(), actual.ino());
        receipt.cleanup_empty().unwrap();
        assert!(receipt.is_removed());
        assert!(receipt.pending_staging_path().is_none());
        assert!(receipt.publication_error().is_none());
    }
    #[test]
    fn restrictive_umask_creation_uses_retained_descriptor() {
        const CHILD: &str = "CAGENTS_DIRECTORY_CREATION_UMASK_CHILD";
        if std::env::var_os(CHILD).is_none() {
            let status=std::process::Command::new(std::env::current_exe().unwrap())
                .args(["--exact","fs_safety::directory_creation::tests::restrictive_umask_creation_uses_retained_descriptor","--test-threads=1"])
                .env(CHILD,"1").status().unwrap();
            assert!(status.success());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let previous = unsafe { libc::umask(0o777) };
        let result = create_directory_beneath(&root, Path::new("target"), 0o750);
        unsafe { libc::umask(previous) };
        let receipt = published(result.unwrap());
        assert!(receipt.publication_error().is_none());
        assert_eq!(receipt.directory().metadata().unwrap().mode() & 0o777, 0);
        set_directory_permissions(receipt.directory(), 0o750).unwrap();
        assert_eq!(
            receipt.directory().metadata().unwrap().mode() & 0o777,
            0o750
        );
    }
    #[test]
    fn failed_preparation_retains_private_stage_until_explicit_retry() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let namespace = DirectoryNamespace::acquire(&root).unwrap();
        let mut failure = create_with_preparation_hook(
            &namespace,
            &root,
            Path::new("target"),
            0o700,
            |_, name| {
                fs::write(dir.path().join(name).join("directory"), b"do not discard").unwrap();
            },
            |_, _| panic!("preparation must fail"),
            |_, _| panic!("must not publish"),
        )
        .unwrap_err();
        assert!(failure.recovery().is_some());
        let mut retained = failure.take_recovery().unwrap();
        assert!(failure.recovery().is_none());
        assert!(!retained.is_resolved());
        assert!(retained.retry_cleanup().is_err());
        assert_eq!(
            fs::read(retained.diagnostic_path().join("directory")).unwrap(),
            b"do not discard"
        );
        fs::remove_file(retained.diagnostic_path().join("directory")).unwrap();
        retained.retry_cleanup().unwrap();
        assert!(retained.is_resolved());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
        assert!(!dir.path().join("target").exists());
    }
    #[test]
    fn failed_publication_retains_prepared_identity_and_retries_cleanup() {
        if unsafe { libc::geteuid() } == 0 {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let namespace = DirectoryNamespace::acquire(&root).unwrap();
        let mut failure = create_with_hooks(
            &namespace,
            &root,
            Path::new("target"),
            0o700,
            |_, _| {
                fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o555)).unwrap();
            },
            |_, _| panic!("must not publish through nonwritable parent"),
        )
        .unwrap_err();
        fs::set_permissions(dir.path(), fs::Permissions::from_mode(0o700)).unwrap();
        assert!(!dir.path().join("target").exists());
        let mut retained = failure.take_recovery().unwrap();
        assert!(retained.outer.is_some());
        retained.retry_cleanup().unwrap();
        assert!(retained.is_resolved());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 0);
    }
    #[test]
    fn retained_preparation_preserves_changed_internal_identity() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let namespace = DirectoryNamespace::acquire(&root).unwrap();
        let mut failure = create_with_preparation_hook(
            &namespace,
            &root,
            Path::new("target"),
            0o700,
            |_, name| {
                fs::write(dir.path().join(name).join("directory"), b"original").unwrap();
            },
            |_, _| {},
            |_, _| {},
        )
        .unwrap_err();
        let mut retained = failure.take_recovery().unwrap();
        let old = retained.diagnostic_path();
        fs::rename(&old, dir.path().join("original-stage")).unwrap();
        fs::create_dir(&old).unwrap();
        fs::write(old.join("foreign"), b"preserve").unwrap();
        assert!(retained.retry_cleanup().is_err());
        assert!(!retained.is_resolved());
        assert_eq!(fs::read(old.join("foreign")).unwrap(), b"preserve");
        assert_eq!(
            fs::read(dir.path().join("original-stage/directory")).unwrap(),
            b"original"
        );
    }

    #[test]
    fn retry_first_acquisition_is_retained_before_fallible_metadata_boundary() {
        for payload_failure in [false, true] {
            let dir = tempfile::tempdir().unwrap();
            fs::create_dir(dir.path().join("stage")).unwrap();
            fs::create_dir(dir.path().join("stage/directory")).unwrap();
            let root = Root::open(dir.path()).unwrap();
            let mut preparation = DirectoryPreparation {
                namespace: DirectoryNamespace::acquire(&root).unwrap(),
                name: OsString::from("stage"),
                outer: None,
                payload: None,
                payload_created: true,
                pending_mode: None,
                created: true,
                cleanup_error: None,
            };
            let mut acquisitions = 0;
            let failed = preparation.retry_cleanup_with_hook(|held| {
                acquisitions += 1;
                if acquisitions == if payload_failure { 2 } else { 1 } {
                    return Err(ResolveError::message(
                        &held.path,
                        "injected failure immediately after exact acquisition, before metadata",
                    ));
                }
                Ok(())
            });
            assert!(failed.is_err());
            assert!(preparation.outer.is_some());
            let original = if payload_failure {
                assert!(preparation.payload.is_some());
                dir.path().join("stage/directory")
            } else {
                dir.path().join("stage")
            };
            let retained = dir.path().join("retained");
            fs::rename(&original, &retained).unwrap();
            fs::create_dir(&original).unwrap();
            fs::write(original.join("foreign"), b"preserve").unwrap();
            assert!(preparation.retry_cleanup().is_err());
            assert!(!preparation.is_resolved());
            assert_eq!(fs::read(original.join("foreign")).unwrap(), b"preserve");
            assert!(retained.is_dir());
        }
    }

    #[test]
    fn private_stage_is_populated_before_exclusive_publication() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let namespace = DirectoryNamespace::acquire(&root).unwrap();
        let stage = stage_directory_in_namespace(&namespace, 0o755).unwrap();
        let original = stage.identity();
        assert!(!dir.path().join("target").exists());
        fs::write(
            stage.directory().path.join("owned"),
            b"completed private operation",
        )
        .unwrap();
        let publication = stage.publish_new(&root, Path::new("target")).unwrap();
        assert_eq!(publication.identity(), original);
        assert_eq!(
            fs::read(dir.path().join("target/owned")).unwrap(),
            b"completed private operation"
        );
        assert!(publication.publication_error().is_none());
        assert_eq!(fs::read_dir(dir.path()).unwrap().count(), 1);
    }
    #[test]
    fn occupied_stage_destination_retains_nonempty_original_without_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let namespace = DirectoryNamespace::acquire(&root).unwrap();
        let stage = stage_directory_in_namespace(&namespace, 0o755).unwrap();
        fs::write(stage.directory().path.join("owned"), b"private data").unwrap();
        fs::create_dir(dir.path().join("target")).unwrap();
        fs::write(dir.path().join("target/foreign"), b"foreign data").unwrap();
        let mut failure = stage.publish_new(&root, Path::new("target")).unwrap_err();
        let mut preparation = failure.take_recovery().unwrap();
        assert!(
            preparation.retry_cleanup().is_err(),
            "nonempty private data requires explicit recovery"
        );
        assert_eq!(
            fs::read(preparation.payload.as_ref().unwrap().path.join("owned")).unwrap(),
            b"private data"
        );
        assert_eq!(
            fs::read(dir.path().join("target/foreign")).unwrap(),
            b"foreign data"
        );
    }
    #[test]
    fn private_stage_refuses_changed_parent_and_retains_nonempty_data() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let namespace = DirectoryNamespace::acquire(&root).unwrap();
        let stage = stage_directory_in_namespace(&namespace, 0o755).unwrap();
        fs::write(stage.directory().path.join("owned"), b"private data").unwrap();
        fs::create_dir(dir.path().join("other")).unwrap();
        let other = Root::open(&dir.path().join("other")).unwrap();
        let mut error = stage.publish_new(&other, Path::new("target")).unwrap_err();
        assert!(!dir.path().join("other/target").exists());
        let preparation = error.take_recovery().unwrap();
        assert!(!preparation.is_resolved());
        assert_eq!(
            fs::read(preparation.payload.as_ref().unwrap().path.join("owned")).unwrap(),
            b"private data"
        );
    }

    #[test]
    fn mode_restore_failure_keeps_published_identity_and_explicit_retry() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let namespace = DirectoryNamespace::acquire(&root).unwrap();
        let stage = stage_directory_in_namespace(&namespace, 0o700).unwrap();
        set_directory_permissions(stage.directory(), 0).unwrap();
        let original = stage.identity();
        let mut receipt = stage
            .publish_to_with_restore(
                root.try_clone().unwrap(),
                OsString::from("target"),
                |_, _| {},
                |_, _| Err(io::Error::from_raw_os_error(libc::EIO)),
            )
            .unwrap();
        assert_eq!(receipt.identity(), original);
        assert!(
            receipt
                .publication_error()
                .unwrap()
                .contains("mode restoration")
        );
        assert_eq!(receipt.pending_mode.unwrap().original, 0);
        assert_eq!(
            receipt.directory().metadata().unwrap().mode() & 0o777,
            0o200
        );
        receipt.restore_pending_mode().unwrap();
        assert_eq!(receipt.directory().metadata().unwrap().mode() & 0o777, 0);
        assert!(receipt.pending_mode.is_none());
        assert!(receipt.publication_error().is_none());
        receipt.cleanup_empty().unwrap();
        assert!(receipt.is_removed());
    }

    #[test]
    fn pending_mode_retry_preserves_observed_newer_public_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let root = Root::open(dir.path()).unwrap();
        let namespace = DirectoryNamespace::acquire(&root).unwrap();
        let stage = stage_directory_in_namespace(&namespace, 0o700).unwrap();
        set_directory_permissions(stage.directory(), 0).unwrap();
        let mut receipt = stage
            .publish_to_with_restore(
                root.try_clone().unwrap(),
                OsString::from("target"),
                |_, _| {},
                |_, _| Err(io::Error::from_raw_os_error(libc::EIO)),
            )
            .unwrap();
        set_directory_permissions(receipt.directory(), 0o750).unwrap();
        assert!(receipt.restore_pending_mode().is_err());
        assert_eq!(
            receipt.directory().metadata().unwrap().mode() & 0o777,
            0o750
        );
        assert!(receipt.pending_mode.is_some());
        assert!(receipt.publication_error().is_some());
        set_directory_permissions(receipt.directory(), 0o200).unwrap();
        receipt.restore_pending_mode().unwrap();
        assert_eq!(receipt.directory().metadata().unwrap().mode() & 0o777, 0);
    }
}
