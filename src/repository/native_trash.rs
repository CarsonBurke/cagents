//! Native freedesktop Trash with original descriptor custody. Public Trash
//! names are acquired into our private holding before verification/removal.
//! This is a local worker receipt, not global future-root registration.
use super::*;
use crate::{
    filesystem_recovery_context::{FilesystemRecoveryContext, FilesystemRecoveryReservation},
    recovery_authority::RecoverySlot,
};
use native_preimage::NativePreimage;
use std::collections::VecDeque;
use std::os::fd::FromRawFd;

pub(in crate::repository) struct Batch {
    prepared: VecDeque<Prepared>,
    files: Arc<fs_safety::Root>,
    info: Arc<fs_safety::Root>,
    files_metadata: fs::Metadata,
    info_metadata: fs::Metadata,
    mount: Option<PathBuf>,
    transaction: Option<FilesystemRecoveryReservation>,
    // Last: every outstanding preimage/receipt keeps the entire pin charge.
    _pins: Arc<PinBudget>,
}
struct PinBudget {
    _slots: Vec<RecoverySlot>,
}
struct Prepared {
    proof: NativePreimage,
    pins: Arc<PinBudget>,
}
#[derive(Clone)]
pub struct TrashReceipt {
    pub path: RepoPath,
    pub warning: Option<String>,
    owner: Arc<Mutex<Owner>>,
}
impl fmt::Debug for TrashReceipt {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TrashReceipt")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}
impl PartialEq for TrashReceipt {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.owner, &other.owner)
    }
}
impl Eq for TrashReceipt {}

// All native owners precede their SAME charged slot. The whole Owner moves
// into retained retry closures; no disjoint-field capture loses original FDs.
struct Owner {
    publication: Option<fs_safety::DirectoryPublication>,
    preimage: NativePreimage,
    files: Arc<fs_safety::Root>,
    info: Arc<fs_safety::Root>,
    files_metadata: fs::Metadata,
    info_metadata: fs::Metadata,
    private_metadata: fs::Metadata,
    info_file: Option<fs::File>,
    info_metadata_proof: Option<fs::Metadata>,
    info_creator_metadata: Option<fs::Metadata>,
    info_written: usize,
    info_body: Vec<u8>,
    id: OsString,
    info_name: OsString,
    // Successful renames are recorded BEFORE any fallible observation.
    payload_in_holding: bool,
    payload_in_trash: bool,
    restored: bool,
    restored_verified: bool,
    info_in_holding: bool,
    info_removed: bool,
    restore_parent: Option<fs_safety::Root>,
    restore_name: Option<OsString>,
    context: FilesystemRecoveryContext,
    slot: Option<RecoverySlot>,
    _pins: Arc<PinBudget>,
}

fn error(error: impl fmt::Display) -> String {
    error.to_string()
}
fn leaf(root: &fs_safety::Root, name: &OsStr) -> Result<fs::Metadata, String> {
    fs_safety::metadata_beneath(root, Path::new(name)).map_err(error)
}
fn rename(
    from: &fs_safety::Root,
    name: &OsStr,
    to: &fs_safety::Root,
    new: &OsStr,
) -> Result<(), String> {
    fs_safety::rename_new_between(from, name, to, new).map_err(error)
}
fn same_renamed(a: &fs::Metadata, b: &fs::Metadata) -> bool {
    moved_entry_version(a) == moved_entry_version(b)
        && (a.uid(), a.gid(), a.nlink()) == (b.uid(), b.gid(), b.nlink())
}
fn open_info(root: &fs_safety::Root, name: &OsStr) -> io::Result<fs::File> {
    let name = std::ffi::CString::new(name.as_bytes())?;
    let fd = unsafe {
        libc::openat(
            root.descriptor(),
            name.as_ptr(),
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        )
    };
    if fd < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { fs::File::from_raw_fd(fd) })
    }
}

// An original existing native directory is never chmod-repaired. New ones
// use the real typed producer and retain every uncertain creation outcome.
fn ensure_directory(
    parent: &fs_safety::Root,
    name: &OsStr,
    context: &FilesystemRecoveryContext,
    live: &LiveDiscardRecovery,
    slot: RecoverySlot,
) -> Result<fs_safety::Root, String> {
    let parent = parent.try_clone().map_err(error)?;
    let retry_context = context.clone();
    match fs_safety::create_directory_beneath(&parent, Path::new(name), 0o700) {
        Err(mut cause) => {
            let message = cause.to_string();
            if let Some(mut preparation) = cause.take_recovery() {
                live.retain(slot.retain(message.clone(), move || {
                    retry_context.placement().validate().map_err(error)?;
                    preparation.retry_cleanup().map_err(error)?;
                    fs_safety::sync_directory(&parent).map_err(error)
                }));
            }
            Err(message)
        }
        Ok(fs_safety::DirectoryCreation::Existing(directory)) => {
            check_native(&directory, context)?;
            Ok(directory)
        }
        Ok(fs_safety::DirectoryCreation::Unverified { directory, reason }) => {
            let message = format!("Native Trash directory creation is unverified: {reason}");
            live.retain(slot.retain(message.clone(), move || {
                retry_context.placement().validate().map_err(error)?;
                if directory.metadata().map_err(error)?.nlink() != 0 {
                    return Err(
                        "Unverified native directory remains; no metadata was changed".into(),
                    );
                }
                fs_safety::sync_directory(&parent).map_err(error)
            }));
            Err(message)
        }
        Ok(fs_safety::DirectoryCreation::Published(publication)) => {
            struct Setup {
                publication: fs_safety::DirectoryPublication,
                parent: fs_safety::Root,
                context: FilesystemRecoveryContext,
                attempted: bool,
                slot: Option<RecoverySlot>,
            }
            impl Setup {
                fn finish(&mut self) -> Result<(), String> {
                    self.publication.retry_publication_steps().map_err(error)?;
                    let original = self.publication.initial_metadata();
                    let actual = self.publication.directory().metadata().map_err(error)?;
                    if (actual.uid(), actual.gid()) != (original.uid(), original.gid())
                        || (actual.mode() & 0o7777 != original.mode() & 0o7777
                            && !(self.attempted && actual.mode() & 0o7777 == 0o700))
                    {
                        return Err(
                            "New native directory metadata changed; it was preserved".into()
                        );
                    }
                    self.context.placement().validate().map_err(error)?;
                    self.attempted = true;
                    fs_safety::set_directory_permissions(self.publication.directory(), 0o700)
                        .map_err(error)?;
                    check_native(self.publication.directory(), &self.context)?;
                    fs_safety::sync_directory(self.publication.directory()).map_err(error)?;
                    fs_safety::sync_directory(&self.parent).map_err(error)
                }
            }
            let mut setup = Setup {
                publication,
                parent,
                context: context.clone(),
                attempted: false,
                slot: Some(slot),
            };
            let result = setup
                .finish()
                .and_then(|()| setup.publication.directory().try_clone().map_err(error));
            if let Err(message) = &result {
                let slot = setup.slot.take().unwrap();
                live.retain(slot.retain(message.clone(), move || setup.finish()));
            }
            result
        }
    }
}
fn verify_native_spelling(root: &fs_safety::Root) -> Result<(), String> {
    let named = fs_safety::Root::open(root.path()).map_err(error)?;
    if identity(&named)? != identity(root)? {
        return Err(
            "The native Trash directory or an ancestor was replaced; no replacement was changed"
                .into(),
        );
    }
    Ok(())
}

fn check_native(
    root: &fs_safety::Root,
    context: &FilesystemRecoveryContext,
) -> Result<fs::Metadata, String> {
    verify_native_spelling(root)?;
    context.validate_payload_root(root).map_err(error)?;
    let metadata = root.metadata().map_err(error)?;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o7777 != 0o700 {
        return Err("Native Trash directories must already be owned and private0700; existing permissions were preserved".into());
    }
    // Native ACLs must not widen the established private namespace.
    drop(fs_safety::DirectoryNamespace::acquire(root).map_err(error)?);
    Ok(metadata)
}

pub(super) fn prepare(
    root: Arc<fs_safety::Root>,
    paths: &[RepoPath],
    context: &FilesystemRecoveryContext,
    live: &LiveDiscardRecovery,
    cancellation: &CancellationToken,
) -> Result<Option<Batch>, String> {
    if paths.is_empty() {
        return Ok(None);
    }
    let data = std::env::var_os("XDG_DATA_HOME")
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| std::env::var_os("HOME").map(|p| PathBuf::from(p).join(".local/share")))
        .ok_or("No native Trash data directory is configured")?;
    prepare_in(root, paths, context, live, cancellation, data)
}
fn prepare_in(
    root: Arc<fs_safety::Root>,
    paths: &[RepoPath],
    context: &FilesystemRecoveryContext,
    live: &LiveDiscardRecovery,
    cancellation: &CancellationToken,
    data: PathBuf,
) -> Result<Option<Batch>, String> {
    if paths.is_empty() {
        return Ok(None);
    }
    if paths.len() > 128 {
        return Err(
            "Select at most128 untracked files per recoverable discard; no file was changed".into(),
        );
    }
    // Two pins per entry: before effects, source parent+payload; after a
    // fully synced move, original payload+info. The shared Trash root pair
    // adds two. One separate slot covers the active staging transaction.
    let mut slots = Vec::new();
    for _ in 0..(2 * paths.len() + 2).div_ceil(crate::recovery_authority::MAX_RECOVERY_DESCRIPTORS)
    {
        slots.push(context.reserve()?);
    }
    let pins = Arc::new(PinBudget { _slots: slots });
    let transaction = context.reserve_directory()?;
    let mut bootstrap = VecDeque::new();
    for _ in 0..8 {
        bootstrap.push_back(context.reserve()?);
    }
    let mut prepared = VecDeque::new();
    for path in paths {
        let proof = NativePreimage::capture(root.clone(), path.clone(), cancellation)?;
        prepared.push_back(Prepared {
            proof,
            pins: pins.clone(),
        });
    }
    if !data.is_absolute() {
        return Err("Native Trash data directory must be absolute".into());
    }
    let mut base = data.join("Trash");
    let source_metadata = root.metadata().map_err(error)?;
    // Locate an existing ancestor read-only, then choose the native per-mount
    // fallback before creating any directory on a different filesystem.
    let mut existing = base.clone();
    let ancestor = loop {
        match fs_safety::Root::open(&existing) {
            Ok(r) => break r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                existing = existing
                    .parent()
                    .ok_or("Trash has no existing ancestor")?
                    .to_owned();
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    let mount = if ancestor.metadata().map_err(error)?.dev() != source_metadata.dev() {
        let mut current = root.path().to_owned();
        while let Some(next) = current.parent() {
            let next_root = fs_safety::Root::open(next).map_err(error)?;
            if next_root.metadata().map_err(error)?.dev() != source_metadata.dev() {
                break;
            }
            current = next.to_owned();
        }
        base = current.join(format!(".Trash-{}", unsafe { libc::geteuid() }));
        Some(current)
    } else {
        None
    };
    let mut existing = base.clone();
    let mut missing = Vec::new();
    let mut parent = loop {
        match fs_safety::Root::open(&existing) {
            Ok(r) => break r,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                missing.push(
                    existing
                        .file_name()
                        .ok_or("Invalid native Trash component")?
                        .to_owned(),
                );
                existing = existing
                    .parent()
                    .ok_or("Invalid native Trash parent")?
                    .to_owned();
            }
            Err(e) => return Err(e.to_string()),
        }
    };
    if missing.len() + 2 > bootstrap.len() {
        return Err("Native Trash bootstrap exceeds the bounded missing-directory chain; no directory was created".into());
    }
    context.validate_payload_root(&parent).map_err(error)?;
    for name in missing.into_iter().rev() {
        if cancellation.is_cancelled() {
            return Err("Trash bootstrap cancelled before creation".into());
        }
        parent = ensure_directory(
            &parent,
            &name,
            context,
            live,
            bootstrap.pop_front().unwrap(),
        )?;
    }
    check_native(&parent, context)?;
    let files = Arc::new(ensure_directory(
        &parent,
        OsStr::new("files"),
        context,
        live,
        bootstrap.pop_front().unwrap(),
    )?);
    let info = Arc::new(ensure_directory(
        &parent,
        OsStr::new("info"),
        context,
        live,
        bootstrap.pop_front().unwrap(),
    )?);
    let files_metadata = check_native(&files, context)?;
    let info_metadata = check_native(&info, context)?;
    Ok(Some(Batch {
        prepared,
        files,
        info,
        files_metadata,
        info_metadata,
        mount,
        transaction: Some(transaction),
        _pins: pins,
    }))
}

impl Batch {
    #[cfg(test)]
    pub(in crate::repository) fn discard(
        &mut self,
        path: &RepoPath,
        live: &LiveDiscardRecovery,
        cancellation: &CancellationToken,
    ) -> Result<TrashReceipt, String> {
        self.discard_observing(path, live, cancellation, || {})
    }

    pub(in crate::repository) fn discard_observing(
        &mut self,
        path: &RepoPath,
        live: &LiveDiscardRecovery,
        cancellation: &CancellationToken,
        after_source_acquire: impl FnOnce(),
    ) -> Result<TrashReceipt, String> {
        let position = self
            .prepared
            .iter()
            .position(|p| p.proof.path() == path)
            .ok_or("No original native Trash preimage was reserved")?;
        let prepared = self.prepared.remove(position).unwrap();
        prepared.proof.verify_source(cancellation)?;
        let id = OsString::from(Uuid::new_v4().to_string());
        let info_name = OsString::from(format!("{}.trashinfo", id.to_string_lossy()));
        let original = prepared.proof.parent().path().join(prepared.proof.name());
        let original = if let Some(mount) = &self.mount {
            original.strip_prefix(mount).map_err(error)?.to_owned()
        } else {
            original
        };
        let escaped = original
            .as_os_str()
            .as_bytes()
            .iter()
            .map(|b| {
                if b.is_ascii_alphanumeric() || b"/-_.~".contains(b) {
                    char::from(*b).to_string()
                } else {
                    format!("%{b:02X}")
                }
            })
            .collect::<String>();
        let body = format!(
            "[Trash Info]\nPath={escaped}\nDeletionDate={}\n",
            trash_date()
        )
        .into_bytes();
        if body.len() > 16 * 1024 {
            return Err("Native Trash original-path metadata exceeds the bounded limit".into());
        }
        let directory = fs_safety::prepare_recovery_directory_with_owner(
            self.transaction
                .take()
                .ok_or("Resolve the previous native Trash transaction before continuing")?,
            OsStr::new(&format!(".cagents-trash-{}", Uuid::new_v4())),
            || cancellation.is_cancelled(),
            (prepared.proof, prepared.pins),
        )
        .map_err(|cause| match cause {
            fs_safety::RecoveryDirectoryError::Rejected(e) => e.to_string(),
            fs_safety::RecoveryDirectoryError::Retained { cause, recovery } => {
                live.retain(recovery);
                cause.to_string()
            }
        })?;
        let (publication, (preimage, pins), private_metadata, context, slot) =
            directory.into_parts();
        let mut owner = Owner {
            publication: Some(publication),
            preimage,
            files: self.files.clone(),
            info: self.info.clone(),
            files_metadata: self.files_metadata.clone(),
            info_metadata: self.info_metadata.clone(),
            private_metadata,
            info_file: None,
            info_metadata_proof: None,
            info_creator_metadata: None,
            info_written: 0,
            info_body: body,
            id,
            info_name,
            payload_in_holding: false,
            payload_in_trash: false,
            restored: false,
            restored_verified: false,
            info_in_holding: false,
            info_removed: false,
            restore_parent: None,
            restore_name: None,
            context,
            slot: Some(slot),
            _pins: pins,
        };
        match owner.move_to_trash(cancellation, after_source_acquire) {
            Ok(warning) => {
                if warning.is_none() {
                    owner.compact_completed();
                    let slot = owner.slot.take().expect("active transaction charge");
                    self.transaction = Some(
                        owner
                            .context
                            .recycle_directory_slot(slot)
                            .unwrap_or_else(|_| unreachable!("same original context")),
                    );
                }
                let receipt = TrashReceipt {
                    path: path.clone(),
                    warning: warning.clone(),
                    owner: Arc::new(Mutex::new(owner)),
                };
                if warning.is_some() {
                    retain_shared(&receipt, live);
                }
                Ok(receipt)
            }
            Err(reason) if owner.payload_in_trash => {
                let reason = owner.recovery_diagnostic(&reason);
                live.set_uncertain(true);
                let receipt = TrashReceipt {
                    path: path.clone(),
                    warning: Some(reason),
                    owner: Arc::new(Mutex::new(owner)),
                };
                retain_shared(&receipt, live);
                Ok(receipt)
            }
            Err(reason) => {
                let reason = owner.recovery_diagnostic(&reason);
                live.set_uncertain(true);
                let slot = owner.slot.take().expect("active transaction charge");
                live.retain(slot.retain(
                    format!("Native Trash recovery for {path}: {reason}"),
                    move || owner.retry(),
                ));
                Err(reason)
            }
        }
    }
}

fn retain_shared(receipt: &TrashReceipt, live: &LiveDiscardRecovery) {
    let owner = receipt.owner.clone();
    let slot = owner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .slot
        .take()
        .expect("active transaction charge");
    let authority = slot.retain(
        format!("Native Trash cleanup for {}", receipt.path),
        move || owner.lock().unwrap_or_else(|e| e.into_inner()).retry(),
    );
    live.retain(authority);
}

impl Owner {
    fn recovery_diagnostic(&self, reason: &str) -> String {
        // These are opened spellings for inspection, never retry authority.
        // Actual Resolve continues to use only this owner's retained handles.
        format!(
            "{reason}. Recovery locations (opened spellings; directories may have moved): private holding {}; native Trash {}; original-path metadata {}",
            self.holding().path().display(),
            self.files.path().join(&self.id).display(),
            self.info.path().join(&self.info_name).display(),
        )
    }
    fn holding(&self) -> &fs_safety::Root {
        self.publication
            .as_ref()
            .expect("active private holding")
            .directory()
    }
    fn validate_native(&self) -> Result<(), String> {
        verify_native_spelling(&self.files)?;
        verify_native_spelling(&self.info)?;
        fs_safety::verify_recovery_directory_private(&self.files, &self.files_metadata)
            .map_err(error)?;
        fs_safety::verify_recovery_directory_private(&self.info, &self.info_metadata)
            .map_err(error)?;
        self.context
            .validate_payload_root(&self.files)
            .map_err(error)?;
        self.context
            .validate_payload_root(&self.info)
            .map_err(error)?;
        verify_native_spelling(&self.files)?;
        verify_native_spelling(&self.info)
    }
    fn validate_holding(&self) -> Result<(), String> {
        fs_safety::verify_recovery_directory_private(self.holding(), &self.private_metadata)
            .map_err(error)?;
        self.context
            .validate_payload_root(self.holding())
            .map_err(error)
    }
    fn verify_info(
        &self,
        root: &fs_safety::Root,
        name: &OsStr,
        own_rename: bool,
    ) -> Result<(), String> {
        use std::os::unix::fs::FileExt;
        let file = self
            .info_file
            .as_ref()
            .ok_or("Original native metadata creator is unavailable")?;
        let expected = self
            .info_metadata_proof
            .as_ref()
            .ok_or("Native metadata write was not verified; original creator retained")?;
        let before = file.metadata().map_err(error)?;
        let named = leaf(root, name)?;
        let matches = |m: &fs::Metadata| {
            if own_rename {
                same_renamed(expected, m)
            } else {
                publication::full_version(expected) == publication::full_version(m)
                    && expected.nlink() == m.nlink()
            }
        };
        if !matches(&before) || !matches(&named) {
            return Err("Native Trash metadata changed; the entry was preserved".into());
        }
        let mut body = vec![0; self.info_body.len() + 1];
        let mut offset = 0;
        while offset < body.len() {
            let n = file
                .read_at(&mut body[offset..], offset as u64)
                .map_err(error)?;
            if n == 0 {
                break;
            }
            offset += n;
        }
        body.truncate(offset);
        if body != self.info_body
            || publication::full_version(&before)
                != publication::full_version(&file.metadata().map_err(error)?)
            || publication::full_version(&before) != publication::full_version(&leaf(root, name)?)
        {
            return Err("Native Trash original-path metadata changed during verification".into());
        }
        Ok(())
    }
    fn verify_partial_info(&self, root: &fs_safety::Root, name: &OsStr) -> Result<(), String> {
        use std::os::unix::fs::FileExt;
        let file = self
            .info_file
            .as_ref()
            .ok_or("No original metadata creator")?;
        let original = self
            .info_creator_metadata
            .as_ref()
            .ok_or("Native metadata private setup is unconfirmed; manual disposal is required")?;
        let before = file.metadata().map_err(error)?;
        let named = leaf(root, name)?;
        if (
            before.dev(),
            before.ino(),
            before.uid(),
            before.gid(),
            before.mode(),
            before.nlink(),
        ) != (
            original.dev(),
            original.ino(),
            original.uid(),
            original.gid(),
            original.mode(),
            original.nlink(),
        ) || before.len() != self.info_written as u64
            || publication::full_version(&before) != publication::full_version(&named)
        {
            return Err(
                "Partial native metadata differs from its original creator/write witness".into(),
            );
        }
        let mut bytes = vec![0; self.info_written];
        let mut offset = 0;
        while offset < bytes.len() {
            let count = file
                .read_at(&mut bytes[offset..], offset as u64)
                .map_err(error)?;
            if count == 0 {
                return Err("Partial native metadata changed during verification".into());
            }
            offset += count;
        }
        if bytes != self.info_body[..self.info_written]
            || publication::full_version(&before)
                != publication::full_version(&file.metadata().map_err(error)?)
            || publication::full_version(&before) != publication::full_version(&leaf(root, name)?)
        {
            return Err(
                "Partial native metadata body changed; original custody was retained".into(),
            );
        }
        Ok(())
    }
    fn move_to_trash(
        &mut self,
        cancellation: &CancellationToken,
        after_source_acquire: impl FnOnce(),
    ) -> Result<Option<String>, String> {
        self.validate_native()?;
        self.validate_holding()?;
        self.preimage.verify_source(cancellation)?;
        // Store the actual O_EXCL creator before chmod/write/sync can fail.
        self.info_file = Some(open_info(&self.info, &self.info_name).map_err(error)?);
        let file = self.info_file.as_mut().unwrap();
        let created = file.metadata().map_err(error)?;
        if !created.is_file() || created.uid() != unsafe { libc::geteuid() } || created.len() != 0 {
            return Err("Native metadata creator is not the original owned empty file".into());
        }
        file.set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(error)?;
        let ready = file.metadata().map_err(error)?;
        if (created.dev(), created.ino(), created.uid(), created.gid())
            != (ready.dev(), ready.ino(), ready.uid(), ready.gid())
            || ready.mode() & 0o7777 != 0o600
        {
            return Err("Native metadata creator changed during private setup".into());
        }
        self.info_creator_metadata = Some(ready);
        while self.info_written < self.info_body.len() {
            let written = file
                .write(&self.info_body[self.info_written..])
                .map_err(error)?;
            if written == 0 {
                return Err("Native metadata write made no progress".into());
            }
            self.info_written += written;
        }
        file.sync_all().map_err(error)?;
        self.verify_partial_info(&self.info, &self.info_name)?;
        self.info_metadata_proof =
            Some(self.info_file.as_ref().unwrap().metadata().map_err(error)?);
        self.verify_info(&self.info, &self.info_name, false)?;
        fs_safety::sync_directory(&self.info).map_err(error)?;
        self.validate_native()?;
        self.validate_holding()?;
        self.preimage.verify_source(cancellation)?;
        rename(
            self.preimage.parent(),
            self.preimage.name(),
            self.holding(),
            OsStr::new("payload"),
        )?;
        self.payload_in_holding = true;
        after_source_acquire();
        let h = self.holding().try_clone().map_err(error)?;
        self.preimage
            .verify_at(&h, OsStr::new("payload"), true, cancellation)?;
        self.validate_native()?;
        self.validate_holding()?;
        self.verify_info(&self.info, &self.info_name, false)?;
        rename(self.holding(), OsStr::new("payload"), &self.files, &self.id)?;
        self.payload_in_holding = false;
        self.payload_in_trash = true;
        self.preimage
            .verify_at(&self.files, &self.id, true, cancellation)?;
        let cleanup = self.finish_trash();
        Ok(cleanup
            .err()
            .map(|e| format!("Moved to native Trash; cleanup or durability needs resolution: {e}")))
    }
    fn finish_trash(&mut self) -> Result<(), String> {
        self.validate_native()?;
        self.preimage
            .verify_at(&self.files, &self.id, false, &CancellationToken::new())?;
        self.verify_info(&self.info, &self.info_name, false)?;
        fs_safety::sync_directory(&self.files).map_err(error)?;
        fs_safety::sync_directory(&self.info).map_err(error)?;
        fs_safety::sync_directory(self.preimage.parent()).map_err(error)?;
        self.remove_empty_holding()
    }
    fn remove_empty_holding(&mut self) -> Result<(), String> {
        if self.publication.as_ref().is_some_and(|p| !p.is_removed()) {
            self.validate_holding()?;
            let publication = self.publication.as_mut().unwrap();
            publication.resolve_recovery().map_err(error)?;
            publication.cleanup_empty().map_err(error)?;
        }
        fs_safety::sync_directory(self.context.placement().directory()).map_err(error)
    }
    fn remove_info(&mut self) -> Result<(), String> {
        if self.info_removed {
            return Ok(());
        }
        self.validate_native()?;
        self.validate_holding()?;
        if !self.info_in_holding {
            // Even a partial failed write is removable only by its original
            // creator identity, not a freshly reopened public metadata name.
            let file = self
                .info_file
                .as_ref()
                .ok_or("No native metadata creator is retained")?;
            let actual = file.metadata().map_err(error)?;
            let named = leaf(&self.info, &self.info_name)?;
            if publication::full_version(&actual) != publication::full_version(&named)
                || actual.nlink() != named.nlink()
            {
                return Err(
                    "Native metadata name no longer selects its creator; foreign entry preserved"
                        .into(),
                );
            }
            if self.info_metadata_proof.is_some() {
                self.verify_info(&self.info, &self.info_name, false)?;
            } else {
                self.verify_partial_info(&self.info, &self.info_name)?;
            }
            rename(
                &self.info,
                &self.info_name,
                self.holding(),
                OsStr::new("info"),
            )?;
            self.info_in_holding = true;
        }
        let file = self.info_file.as_ref().unwrap();
        let actual = file.metadata().map_err(error)?;
        let named = leaf(self.holding(), OsStr::new("info"))?;
        if publication::full_version(&actual) != publication::full_version(&named)
            || actual.nlink() != named.nlink()
        {
            return Err("Acquired native metadata differs from the original creator".into());
        }
        if self.info_metadata_proof.is_some() {
            self.verify_info(self.holding(), OsStr::new("info"), true)?;
        } else {
            self.verify_partial_info(self.holding(), OsStr::new("info"))?;
        }
        fs_safety::unlink_within(self.holding(), OsStr::new("info")).map_err(error)?;
        self.info_removed = true;
        self.info_in_holding = false;
        fs_safety::sync_directory(&self.info).map_err(error)
    }
    // Called only after all native/source and holding-parent syncs succeeded.
    // Every exact payload/info pin stays in this owner, charged by pins LAST.
    fn compact_completed(&mut self) {
        debug_assert!(self.publication.as_ref().is_none_or(|p| p.is_removed()));
        self.publication = None;
        self.preimage.retire_source_parent();
        self.restore_parent = None;
    }
    fn retry(&mut self) -> Result<(), String> {
        self.retry_active()?;
        self.compact_completed();
        Ok(())
    }
    fn retry_active(&mut self) -> Result<(), String> {
        // Explicit manual disposal may retire uncertainty only when BOTH
        // original native inodes are positively unlinked; moving them out of
        // the holding or observing pathname absence does not establish this.
        let payload_removed = self.preimage.payload().metadata().map_err(error)?.nlink() == 0;
        let info_removed = match &self.info_file {
            Some(file) => file.metadata().map_err(error)?.nlink() == 0,
            None => true,
        };
        if payload_removed && info_removed {
            self.remove_empty_holding()?;
            fs_safety::sync_directory(&self.files).map_err(error)?;
            fs_safety::sync_directory(&self.info).map_err(error)?;
            return fs_safety::sync_directory(&self.preimage.source_root()).map_err(error);
        }
        if self.payload_in_trash {
            // An uncertain post-rename observation verifies the SAME original
            // bytes/access and only the ctime introduced by our recorded move.
            self.validate_native()?;
            if self.restore_parent.is_some() {
                // The original discard/source sync was already settled and
                // compacted. This Undo installed a new holding but never
                // acquired the native payload. Dispose only that empty H;
                // the original native entry remains available for Undo.
                // No payload rename was dispatched by this Undo. Preserve
                // the settled original version, including ctime.
                self.preimage
                    .verify_at(&self.files, &self.id, false, &CancellationToken::new())?;
                self.verify_info(&self.info, &self.info_name, false)?;
                return self.remove_empty_holding();
            }
            self.preimage
                .verify_at(&self.files, &self.id, true, &CancellationToken::new())?;
            return self.finish_trash();
        }
        if self.payload_in_holding {
            self.validate_holding()?;
            let h = self.holding().try_clone().map_err(error)?;
            self.preimage
                .verify_at(&h, OsStr::new("payload"), true, &CancellationToken::new())?;
            verify_recovery_root(&self.preimage.source_root())?;
            let target = self
                .restore_parent
                .as_ref()
                .unwrap_or_else(|| self.preimage.parent());
            let name = self.restore_name.as_deref().unwrap_or(self.preimage.name());
            verify_source_target(&self.preimage, target, name)?;
            rename(self.holding(), OsStr::new("payload"), target, name)?;
            self.payload_in_holding = false;
            self.restored = true;
            let parent = self
                .restore_parent
                .as_ref()
                .unwrap_or_else(|| self.preimage.parent())
                .try_clone()
                .map_err(error)?;
            let name = self
                .restore_name
                .as_deref()
                .unwrap_or(self.preimage.name())
                .to_owned();
            self.preimage
                .verify_at(&parent, &name, true, &CancellationToken::new())?;
            self.restored_verified = true;
        }
        // Before a successful source acquisition, cancellation/stale source
        // edits did not authorize a source effect. Dispose only our metadata
        // and empty private holding; never roll back the user's newer bytes.
        if self.restored {
            let parent = self
                .restore_parent
                .as_ref()
                .unwrap_or_else(|| self.preimage.parent())
                .try_clone()
                .map_err(error)?;
            let name = self
                .restore_name
                .as_deref()
                .unwrap_or(self.preimage.name())
                .to_owned();
            verify_source_target(&self.preimage, &parent, &name)?;
            self.preimage.verify_at(
                &parent,
                &name,
                !self.restored_verified,
                &CancellationToken::new(),
            )?;
            self.restored_verified = true;
        }
        if self.info_file.is_some() {
            self.remove_info()?;
        }
        if self.restored {
            let target = self
                .restore_parent
                .as_ref()
                .unwrap_or_else(|| self.preimage.parent());
            fs_safety::sync_directory(target).map_err(error)?;
        }
        self.remove_empty_holding()
    }
}

pub(super) fn restore(
    receipt: &TrashReceipt,
    recovery: &DiscardRecovery,
    parent: &fs_safety::Root,
    name: &OsStr,
    cancellation: &CancellationToken,
    context: &FilesystemRecoveryContext,
) -> Result<Option<String>, String> {
    restore_observing(
        receipt,
        recovery,
        parent,
        name,
        cancellation,
        context,
        || Ok(()),
        || Ok(()),
    )
}
fn restore_observing(
    receipt: &TrashReceipt,
    recovery: &DiscardRecovery,
    parent: &fs_safety::Root,
    name: &OsStr,
    cancellation: &CancellationToken,
    context: &FilesystemRecoveryContext,
    before_native_acquire: impl FnOnce() -> Result<(), String>,
    after_source_rename: impl FnOnce() -> Result<(), String>,
) -> Result<Option<String>, String> {
    let mut owner = receipt.owner.lock().unwrap_or_else(|e| e.into_inner());
    if owner.restored {
        return Err("This native Trash entry was already restored".into());
    }
    owner.validate_native()?;
    context
        .validate_for_source(&owner.preimage.source_root())
        .map_err(error)?;
    match fs_safety::metadata_beneath(parent, Path::new(name)) {
        Ok(_) => {
            return Err("A newer source entry exists; native Trash contents were preserved".into());
        }
        Err(cause) if cause.is_not_found() => {}
        Err(cause) => {
            return Err(format!(
                "Source destination cannot be verified: {cause}; native Trash contents were preserved"
            ));
        }
    }
    let native_files = owner.files.clone();
    let native_name = owner.id.clone();
    owner
        .preimage
        .verify_at(&native_files, &native_name, false, cancellation)?;
    owner.verify_info(&owner.info, &owner.info_name, false)?;
    // The previous empty holding may have been positively removed. Acquire a
    // new local holding with capacity reserved BEFORE any public Trash move.
    if owner.publication.as_ref().is_some_and(|p| !p.is_removed()) {
        return Err("Resolve previous native Trash holding before Undo".into());
    }
    let admission = context.reserve_directory()?;
    let new = fs_safety::prepare_recovery_directory_with_owner(
        admission,
        OsStr::new(&format!(".cagents-trash-undo-{}", Uuid::new_v4())),
        || cancellation.is_cancelled(),
        parent.try_clone().map_err(error)?,
    )
    .map_err(|cause| match cause {
        fs_safety::RecoveryDirectoryError::Rejected(e) => e.to_string(),
        fs_safety::RecoveryDirectoryError::Retained {
            cause,
            recovery: authority,
        } => {
            recovery.live_recovery.retain(authority);
            cause.to_string()
        }
    })?;
    let (publication, restore_parent, private_metadata, new_context, slot) = new.into_parts();
    // Hold both old custody and the new publication aggregate through errors.
    struct RestoreHolding {
        publication: fs_safety::DirectoryPublication,
        parent: fs_safety::Root,
        private: fs::Metadata,
        context: FilesystemRecoveryContext,
        slot: RecoverySlot,
    }
    let h = RestoreHolding {
        publication,
        parent: restore_parent,
        private: private_metadata,
        context: new_context,
        slot,
    };
    // Immediately transfer the complete new native aggregate BEFORE effects;
    // all refusal paths retain this actual holding in the original receipt.
    owner.publication = Some(h.publication);
    owner.private_metadata = h.private;
    owner.restore_parent = Some(h.parent);
    owner.restore_name = Some(name.to_owned());
    owner.context = h.context;
    owner.slot = Some(h.slot);
    let target = parent;
    let result = (|| {
        fs_safety::verify_recovery_directory_private(owner.holding(), &owner.private_metadata)
            .map_err(error)?;
        owner
            .context
            .validate_payload_root(owner.holding())
            .map_err(error)?;
        owner.validate_native()?;
        before_native_acquire()?;
        rename(
            &owner.files,
            &owner.id,
            owner.holding(),
            OsStr::new("payload"),
        )?;
        owner.payload_in_trash = false;
        owner.payload_in_holding = true;
        let held = owner.holding().try_clone().map_err(error)?;
        owner
            .preimage
            .verify_at(&held, OsStr::new("payload"), true, cancellation)?;
        // Revalidate original source/root-relative target after the long hash.
        verify_recovery_root(&owner.preimage.source_root())?;
        let (actual, actual_name) =
            fs_safety::open_parent_beneath(&owner.preimage.source_root(), receipt.path.as_path())
                .map_err(error)?;
        if actual_name != name || identity(&actual)? != identity(&target)? {
            return Err("Original source restore parent changed; newer work was preserved".into());
        }
        rename(owner.holding(), OsStr::new("payload"), &target, name)?;
        owner.payload_in_holding = false;
        owner.restored = true;
        after_source_rename()?;
        owner
            .preimage
            .verify_at(&target, name, true, cancellation)?;
        owner.restored_verified = true;
        owner.remove_info()?;
        fs_safety::sync_directory(&target).map_err(error)?;
        owner.remove_empty_holding()
    })();
    match result {
        Ok(()) => {
            owner.compact_completed();
            owner.slot = None;
            Ok(None)
        }
        Err(reason) => {
            // Original receipt stays installed in the UI before this task.
            // Retain the actual new holding even when publication is uncertain.
            recovery
                .live_recovery
                .set_uncertain(!owner.restored_verified);
            let shared = receipt.owner.clone();
            let slot = owner
                .slot
                .take()
                .expect("active restore transaction charge");
            recovery
                .live_recovery
                .retain(slot.retain(reason.clone(), move || {
                    shared.lock().unwrap_or_else(|e| e.into_inner()).retry()
                }));
            if owner.restored_verified {
                Ok(Some(reason))
            } else {
                Err(reason)
            }
        }
    }
}
pub(super) fn confirmed_restore(receipt: &TrashReceipt) -> bool {
    receipt
        .owner
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .restored_verified
}

fn verify_source_target(
    proof: &NativePreimage,
    parent: &fs_safety::Root,
    name: &OsStr,
) -> Result<(), String> {
    let source = proof.source_root();
    verify_recovery_root(&source)?;
    let (actual, actual_name) =
        fs_safety::open_parent_beneath(&source, proof.path().as_path()).map_err(error)?;
    if actual_name != name || identity(&actual)? != identity(parent)? {
        return Err(
            "The original source-relative parent changed; no replacement was modified".into(),
        );
    }
    Ok(())
}

fn identity(root: &fs_safety::Root) -> Result<(u64, u64), String> {
    let m = root.metadata().map_err(error)?;
    Ok((m.dev(), m.ino()))
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    #[test]
    fn native_batch_capacity_refusal_precedes_trash_bootstrap_and_all_source_effects() {
        // A 128-path batch needs 26 of the process-wide 32 recovery slots.
        // Isolate before fixture setup so parallel tests cannot spend its headroom.
        if crate::repository::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::native_batch_capacity_refusal_precedes_trash_bootstrap_and_all_source_effects"
        )) {
            return;
        }
        let source = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = Arc::new(fs_safety::Root::open(source.path()).unwrap());
        let paths = (0..128)
            .map(|index| RepoPath::parse(format!("entry-{index}")).unwrap())
            .collect::<Vec<_>>();
        for path in &paths {
            fs::write(source.path().join(path.as_path()), b"still original").unwrap();
        }
        let context = crate::filesystem_recovery_context::fixtures::fixture_context(&root).unwrap();
        let live = LiveDiscardRecovery::default();
        let occupied = (0..7)
            .map(|_| context.reserve().unwrap())
            .collect::<Vec<_>>();
        assert!(
            prepare_in(
                root.clone(),
                &paths,
                &context,
                &live,
                &CancellationToken::new(),
                data.path().to_owned()
            )
            .is_err()
        );
        assert!(!live.needs_resolution());
        assert_eq!(fs::read_dir(data.path()).unwrap().count(), 0);
        assert_eq!(
            fs::read_dir(context.placement().directory().path())
                .unwrap()
                .count(),
            0
        );
        for path in &paths {
            assert_eq!(
                fs::read(source.path().join(path.as_path())).unwrap(),
                b"still original"
            );
        }
        drop(occupied);
        let batch = prepare_in(
            root,
            &paths,
            &context,
            &live,
            &CancellationToken::new(),
            data.path().to_owned(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(batch.prepared.len(), 128);
    }

    #[test]
    fn native_batch_128_compacts_only_completed_entries_and_keeps_shared_pins_through_clones() {
        if crate::repository::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::native_batch_128_compacts_only_completed_entries_and_keeps_shared_pins_through_clones"
        )) {
            return;
        }
        let source = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let root = Arc::new(fs_safety::Root::open(source.path()).unwrap());
        let paths = (0..128)
            .map(|index| RepoPath::parse(format!("entry-{index}")).unwrap())
            .collect::<Vec<_>>();
        let originals = paths
            .iter()
            .enumerate()
            .map(|(index, path)| {
                fs::write(
                    source.path().join(path.as_path()),
                    format!("original-{index}"),
                )
                .unwrap();
                fs::symlink_metadata(source.path().join(path.as_path())).unwrap()
            })
            .collect::<Vec<_>>();
        let context = crate::filesystem_recovery_context::fixtures::fixture_context(&root).unwrap();
        let live = LiveDiscardRecovery::default();
        let mut batch = prepare_in(
            root.clone(),
            &paths,
            &context,
            &live,
            &CancellationToken::new(),
            data.path().to_owned(),
        )
        .unwrap()
        .unwrap();
        assert_eq!(batch._pins._slots.len(), 17);
        let pins = Arc::downgrade(&batch._pins);
        let mut receipts = Vec::new();
        for (path, original) in paths.iter().zip(&originals) {
            let receipt = batch
                .discard(path, &live, &CancellationToken::new())
                .unwrap();
            assert!(receipt.warning.is_none());
            assert!(batch.transaction.is_some());
            let owner = receipt.owner.lock().unwrap();
            assert!(owner.publication.is_none());
            assert!(owner.slot.is_none());
            assert!(!owner.preimage.has_source_parent());
            let actual = owner.preimage.payload().metadata().unwrap();
            assert_eq!(
                (actual.dev(), actual.ino()),
                (original.dev(), original.ino())
            );
            assert_eq!(
                owner
                    .info_file
                    .as_ref()
                    .unwrap()
                    .metadata()
                    .unwrap()
                    .nlink(),
                1
            );
            drop(owner);
            receipts.push(receipt);
        }
        assert!(!live.needs_resolution());
        assert_eq!(
            fs::read_dir(context.placement().directory().path())
                .unwrap()
                .count(),
            0
        );
        assert_eq!(
            fs::read_dir(data.path().join("Trash/files"))
                .unwrap()
                .count(),
            128
        );
        assert_eq!(
            fs::read_dir(data.path().join("Trash/info"))
                .unwrap()
                .count(),
            128
        );
        let clone = receipts[0].clone();
        drop(receipts);
        drop(batch);
        assert!(
            pins.upgrade().is_some(),
            "indexed receipt clone owns the entire shared charge"
        );
        drop(clone);
        assert!(pins.upgrade().is_none());
    }

    #[test]
    fn native_discard_acquired_writer_change_reports_unknown_and_stops_remaining_paths() {
        if crate::repository::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::native_discard_acquired_writer_change_reports_unknown_and_stops_remaining_paths"
        )) {
            return;
        }
        use std::os::unix::fs::FileExt;

        let source = tempfile::tempdir().unwrap();
        let data = tempfile::tempdir().unwrap();
        let base = RepositoryService::system("git");
        let mut init = base.git_mutation(source.path(), ["init", "--quiet"]);
        for (key, _) in std::env::vars_os() {
            if key.as_bytes().starts_with(b"GIT_") {
                init = init.env_remove(key);
            }
        }
        init = init
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null");
        base.run_checked(&init, &CancellationToken::new()).unwrap();
        let authority = WorktreeAuthority::capture(source.path()).unwrap();
        let root = authority.retained_worktree_root();
        let service = base.with_worktree_authority(authority).unwrap();
        let path = RepoPath::parse("original").unwrap();
        let later = RepoPath::parse("later").unwrap();
        let file = source.path().join(path.as_path());
        fs::write(&file, b"original data\n").unwrap();
        fs::write(source.path().join(later.as_path()), b"later untouched\n").unwrap();
        let original = fs::metadata(&file).unwrap();
        // An independently held writer remains an in-scope content change,
        // even after our source acquisition has moved the original inode.
        let writer = fs::File::options().write(true).open(&file).unwrap();
        let context = crate::filesystem_recovery_context::fixtures::fixture_context(&root).unwrap();
        let live = LiveDiscardRecovery::default();
        let mut paths = vec![path.clone(), later.clone()];
        for index in 2..128 {
            let next = RepoPath::parse(format!("later-{index}")).unwrap();
            fs::write(source.path().join(next.as_path()), b"later untouched\n").unwrap();
            paths.push(next);
        }
        let mut batch = prepare_in(
            root.clone(),
            &paths,
            &context,
            &live,
            &CancellationToken::new(),
            data.path().to_owned(),
        )
        .unwrap();
        let mut trashed = Vec::new();
        let pins = Arc::downgrade(&batch.as_ref().unwrap()._pins);
        assert_eq!(pins.upgrade().unwrap()._slots.len(), 17);
        let mut acquired = Vec::new();
        let outcome = service
            .discard_recovery_mutation_observing(
                &DiscardRequest {
                    worktree: source.path().to_owned(),
                    tracked: Vec::new(),
                    untracked: paths.clone(),
                    confirmed: true,
                },
                &CancellationToken::new(),
                &mut trashed,
                &root,
                &BTreeMap::new(),
                &mut batch,
                &live,
                &mut |acquired_path| {
                    acquired.push(acquired_path.clone());
                    assert!(!file.exists());
                    writer.write_all_at(b"newer FD edit\n", 0).unwrap();
                    writer.sync_all().unwrap();
                },
            )
            .unwrap();
        assert!(
            matches!(outcome, MutationOutcome::OutcomeUnknown { .. }),
            "{outcome:?}"
        );
        assert_eq!(acquired, vec![path]);
        assert!(trashed.is_empty());
        assert!(live.needs_resolution());
        assert!(live.is_uncertain());
        assert!(!file.exists());
        assert_eq!(
            fs::read(source.path().join(later.as_path())).unwrap(),
            b"later untouched\n"
        );
        assert!(!source.path().join(".git/index.lock").exists());
        let holding = fs::read_dir(context.placement().directory().path())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .as_bytes()
                    .starts_with(b".cagents-trash-")
            })
            .unwrap();
        let report = match &outcome {
            MutationOutcome::OutcomeUnknown { failure } => failure.to_string(),
            _ => unreachable!(),
        };
        assert!(report.contains(holding.to_string_lossy().as_ref()));
        assert!(report.contains("opened spellings"));
        let payload = holding.join("payload");
        let retained = fs::metadata(&payload).unwrap();
        assert_eq!(
            (retained.dev(), retained.ino()),
            (original.dev(), original.ino())
        );
        assert_eq!(fs::read(&payload).unwrap(), b"newer FD edit\n");
        drop(batch);
        assert!(
            pins.upgrade().is_some(),
            "active failed entry keeps the full batch pin charge"
        );
        for path in &paths[1..] {
            assert_eq!(
                fs::read(source.path().join(path.as_path())).unwrap(),
                b"later untouched\n"
            );
        }
        let retry = live.authority().unwrap();
        assert!(
            retry.retry().is_err(),
            "newer original bytes must not be adopted or rolled back"
        );
        assert_eq!(fs::read(&payload).unwrap(), b"newer FD edit\n");
        assert!(!file.exists());
        // Explicit operator disposal of BOTH originals is a supported terminal
        // route; moving or modifying the retained payload is not settlement.
        fs::remove_file(&payload).unwrap();
        let info = fs::read_dir(data.path().join("Trash/info"))
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .collect::<Vec<_>>();
        assert_eq!(info.len(), 1);
        fs::remove_file(&info[0]).unwrap();
        retry.retry().unwrap();
        assert!(retry.is_resolved());
        assert!(
            pins.upgrade().is_none(),
            "only actual settlement releases original failed owner and shared charge"
        );
        assert!(!holding.exists());
        assert_eq!(
            fs::read(source.path().join(later.as_path())).unwrap(),
            b"later untouched\n"
        );
    }

    #[test]
    fn native_compact_receipt_resolves_preacquisition_undo_failure_then_retries_same_public_undo() {
        for change_settled_payload_ctime in [false, true] {
            let source = tempfile::tempdir().unwrap();
            let data = tempfile::tempdir().unwrap();
            let base = RepositoryService::system("git");
            let mut init = base.git_mutation(source.path(), ["init", "--quiet"]);
            for (key, _) in std::env::vars_os() {
                if key.as_bytes().starts_with(b"GIT_") {
                    init = init.env_remove(key);
                }
            }
            init = init
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .env("GIT_CONFIG_GLOBAL", "/dev/null");
            base.run_checked(&init, &CancellationToken::new()).unwrap();
            let authority = WorktreeAuthority::capture(source.path()).unwrap();
            let root = authority.retained_worktree_root();
            let service = base.with_worktree_authority(authority.clone()).unwrap();
            let file = source.path().join("original");
            fs::write(&file, b"original native bytes\n").unwrap();
            let original = fs::metadata(&file).unwrap();
            let context =
                crate::filesystem_recovery_context::fixtures::fixture_context(&root).unwrap();
            let live = LiveDiscardRecovery::default();
            let path = RepoPath::parse("original").unwrap();
            let mut batch = prepare_in(
                root.clone(),
                &[path.clone()],
                &context,
                &live,
                &CancellationToken::new(),
                data.path().to_owned(),
            )
            .unwrap()
            .unwrap();
            let receipt = batch
                .discard(&path, &live, &CancellationToken::new())
                .unwrap();
            drop(batch);
            assert!(!receipt.owner.lock().unwrap().preimage.has_source_parent());
            let directory = root.metadata().unwrap();
            let recovery = DiscardRecovery {
                live_recovery: live,
                custody: Some(Arc::new(DiscardCustody {
                    git: service.git.clone(),
                    executor: service.creation_executor.clone(),
                    source: authority,
                    context: context.clone(),
                })),
                worktree: source.path().to_owned(),
                worktree_identity: (directory.dev(), directory.ino()),
                directories: BTreeMap::new(),
                entries: vec![DiscardRecoveryEntry {
                    path: path.clone(),
                    before: SavedEntry::Trashed,
                    after: SavedEntry::Absent,
                    after_version: None,
                    trash: Some(receipt.clone()),
                }],
                expires_unix_ms: u64::MAX,
            };
            let failed = restore_observing(
                &receipt,
                &recovery,
                &root,
                OsStr::new("original"),
                &CancellationToken::new(),
                &context,
                || Err("injected failure after new holding, before native acquisition".into()),
                || Ok(()),
            );
            assert!(failed.is_err());
            assert!(!file.exists());
            assert!(recovery.live_recovery.needs_resolution());
            {
                let owner = receipt.owner.lock().unwrap();
                assert!(owner.publication.is_some());
                assert!(owner.restore_parent.is_some());
                assert!(owner.payload_in_trash);
                assert!(!owner.preimage.has_source_parent());
            }
            if change_settled_payload_ctime {
                let pin = receipt.owner.lock().unwrap().preimage.payload();
                let settled = pin.metadata().unwrap();
                pin.set_permissions(fs::Permissions::from_mode(
                    (settled.mode() & 0o7777) ^ 0o100,
                ))
                .unwrap();
                pin.set_permissions(settled.permissions()).unwrap();
                let changed = pin.metadata().unwrap();
                assert_eq!(changed.mode(), settled.mode());
                assert_ne!(
                    (changed.ctime(), changed.ctime_nsec()),
                    (settled.ctime(), settled.ctime_nsec())
                );
                for _ in 0..2 {
                    assert!(
                        service
                            .resolve_discard_recovery(&recovery, &CancellationToken::new())
                            .is_err()
                    );
                    assert!(
                        receipt
                            .owner
                            .lock()
                            .unwrap()
                            .preimage
                            .matches_recorded_metadata(&settled)
                    );
                    assert_eq!(recovery.remaining_paths(), 1);
                    assert!(recovery.live_recovery.needs_resolution());
                    assert!(!confirmed_restore(&receipt));
                    assert!(!file.exists());
                }
                continue;
            }
            let resolved = service
                .resolve_discard_recovery(&recovery, &CancellationToken::new())
                .unwrap();
            assert_eq!(resolved.remaining_paths, 1);
            assert!(!recovery.live_recovery.needs_resolution());
            assert!(!confirmed_restore(&receipt));
            assert!(!file.exists());
            assert_eq!(
                fs::read_dir(data.path().join("Trash/files"))
                    .unwrap()
                    .count(),
                1
            );
            assert_eq!(
                fs::read_dir(context.placement().directory().path())
                    .unwrap()
                    .count(),
                0
            );
            let undo = service
                .undo_discard(&recovery, &CancellationToken::new())
                .unwrap();
            assert!(matches!(undo, MutationOutcome::Applied { .. }), "{undo:?}");
            assert_eq!(recovery.remaining_paths(), 0);
            assert_eq!(fs::read(&file).unwrap(), b"original native bytes\n");
            let restored = fs::metadata(file).unwrap();
            assert_eq!(
                (restored.dev(), restored.ino()),
                (original.dev(), original.ino())
            );
            assert_eq!(
                fs::read_dir(data.path().join("Trash/files"))
                    .unwrap()
                    .count(),
                0
            );
            assert_eq!(
                fs::read_dir(data.path().join("Trash/info"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }

    #[test]
    fn native_restore_post_rename_cancellation_retries_original_proof_and_acknowledges_exact_path()
    {
        for change_after_rename in [false, true] {
            let source = tempfile::tempdir().unwrap();
            let data = tempfile::tempdir().unwrap();
            let root = Arc::new(fs_safety::Root::open(source.path()).unwrap());
            let file = source.path().join("original");
            fs::write(&file, b"original data\n").unwrap();
            fs::set_permissions(&file, fs::Permissions::from_mode(0o640)).unwrap();
            let original = fs::metadata(&file).unwrap();
            let original_time = original.modified().unwrap();
            let context =
                crate::filesystem_recovery_context::fixtures::fixture_context(&root).unwrap();
            let live = LiveDiscardRecovery::default();
            let path = RepoPath::parse("original").unwrap();
            let mut batch = prepare_in(
                root.clone(),
                &[path.clone()],
                &context,
                &live,
                &CancellationToken::new(),
                data.path().to_owned(),
            )
            .unwrap()
            .unwrap();
            let receipt = batch
                .discard(&path, &live, &CancellationToken::new())
                .unwrap();
            assert!(!file.exists());
            let recovery = DiscardRecovery {
                live_recovery: live,
                custody: None,
                worktree: source.path().to_owned(),
                worktree_identity: (original.dev(), root.metadata().unwrap().ino()),
                directories: BTreeMap::new(),
                entries: vec![DiscardRecoveryEntry {
                    path: path.clone(),
                    before: SavedEntry::Trashed,
                    after: SavedEntry::Absent,
                    after_version: None,
                    trash: Some(receipt.clone()),
                }],
                expires_unix_ms: u64::MAX,
            };
            let cancelled = CancellationToken::new();
            let result = restore_observing(
                &receipt,
                &recovery,
                &root,
                OsStr::new("original"),
                &cancelled,
                &context,
                || Ok(()),
                || {
                    cancelled.cancel();
                    Ok(())
                },
            );
            assert!(result.is_err());
            assert!(file.exists());
            assert_eq!(fs::read(&file).unwrap(), b"original data\n");
            assert!(recovery.live_recovery.needs_resolution());
            assert_eq!(recovery.remaining_paths(), 1);
            assert!(!confirmed_restore(&receipt));
            let service = RepositoryService::system("git");
            if change_after_rename {
                fs::write(&file, b"user's newer contents\n").unwrap();
                assert!(
                    service
                        .resolve_discard_recovery(&recovery, &CancellationToken::new())
                        .is_err()
                );
                assert_eq!(fs::read(&file).unwrap(), b"user's newer contents\n");
                assert_eq!(recovery.remaining_paths(), 1);
                assert!(!confirmed_restore(&receipt));
                // Explicit user repair of the original held inode, preserving
                // newer data until that deliberate repair, enables retry.
                fs::write(&file, b"original data\n").unwrap();
                fs::File::options()
                    .write(true)
                    .open(&file)
                    .unwrap()
                    .set_times(fs::FileTimes::new().set_modified(original_time))
                    .unwrap();
            }
            let resolution = service
                .resolve_discard_recovery(&recovery, &CancellationToken::new())
                .unwrap();
            assert_eq!(resolution.remaining_paths, 0);
            assert_eq!(recovery.remaining_paths(), 0);
            assert!(!recovery.live_recovery.needs_resolution());
            assert!(confirmed_restore(&receipt));
            let restored = fs::metadata(&file).unwrap();
            assert_eq!(
                (original.dev(), original.ino()),
                (restored.dev(), restored.ino())
            );
            assert_eq!(restored.mode() & 0o7777, 0o640);
            assert_eq!(
                fs::read_dir(data.path().join("Trash/files"))
                    .unwrap()
                    .count(),
                0
            );
            assert_eq!(
                fs::read_dir(data.path().join("Trash/info"))
                    .unwrap()
                    .count(),
                0
            );
        }
    }
}
