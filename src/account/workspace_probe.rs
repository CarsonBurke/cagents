//! A fixed write/read/remove check in a physically identified workspace.
//!
//! The caller chooses a fresh UUID and treats a missing/incomplete receipt as
//! unconfirmed cleanup. No provider, network, account store or shell is used.
//! A private 0700 directory separates our fixed file from ordinary concurrent
//! workspace writers. Observed replacements are preserved. POSIX has no atomic
//! inode-conditional unlink, so this does not defend against a malicious process
//! with the same uid replacing entries between their final check and unlink.

use std::{
    ffi::{CStr, CString},
    fs::{File, Permissions},
    io::{self, Read, Seek, SeekFrom, Write},
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt, fs::PermissionsExt},
    },
    path::Path,
};

use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Verified,
    InvalidWorkspace,
    WorkspaceChanged,
    CreateFailed,
    Collision,
    WriteFailed,
    ReadFailed,
    ContentMismatch,
    CleanupFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Cleanup {
    NotCreated,
    Removed,
    ReplacementPreserved,
    Unconfirmed,
}

/// Fixed metadata only: never file contents, paths, credentials or OS messages.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub nonce: Uuid,
    /// Whether this invocation created its private scratch directory.
    pub created: bool,
    pub read_match: bool,
    /// Both our file and our private directory were observed removed.
    pub removed: bool,
    pub status: Status,
    pub cleanup: Cleanup,
}

impl Receipt {
    pub fn is_verified(&self) -> bool {
        self.created
            && self.read_match
            && self.removed
            && self.status == Status::Verified
            && self.cleanup == Cleanup::Removed
    }

    fn refused(nonce: Uuid, status: Status) -> Self {
        Self {
            nonce,
            created: false,
            read_match: false,
            removed: false,
            status,
            cleanup: Cleanup::NotCreated,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Identity {
    device: u64,
    inode: u64,
}

impl Identity {
    fn of(metadata: &std::fs::Metadata) -> Self {
        Self {
            device: metadata.dev(),
            inode: metadata.ino(),
        }
    }
}

/// Checks only the supplied absolute directory at the caller's physical tuple.
/// `nonce` must be fresh per attempt; a collision is never reused or removed.
pub fn probe(workspace: &Path, device: u64, inode: u64, nonce: Uuid) -> Receipt {
    probe_before_cleanup(workspace, device, inode, nonce, |_| {})
}

fn probe_before_cleanup(
    workspace: &Path,
    device: u64,
    inode: u64,
    nonce: Uuid,
    before_cleanup: impl FnOnce(&Path),
) -> Receipt {
    if !workspace.is_absolute() {
        return Receipt::refused(nonce, Status::InvalidWorkspace);
    }
    // Path's component spelling removes trailing slashes, which would otherwise
    // let open("symlink/", O_NOFOLLOW) follow the symlink before its final slash.
    let workspace = workspace.components().collect::<std::path::PathBuf>();
    let Ok(raw_workspace) = CString::new(workspace.as_os_str().as_bytes()) else {
        return Receipt::refused(nonce, Status::InvalidWorkspace);
    };
    let Ok(directory) = open_at(
        libc::AT_FDCWD,
        &raw_workspace,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    ) else {
        return Receipt::refused(nonce, Status::InvalidWorkspace);
    };
    let expected = Identity { device, inode };
    if !directory
        .metadata()
        .is_ok_and(|metadata| metadata.is_dir() && Identity::of(&metadata) == expected)
    {
        return Receipt::refused(nonce, Status::WorkspaceChanged);
    }
    let leaf = format!(".cagents-readiness-{nonce}");
    let Ok(raw_leaf) = CString::new(leaf.as_bytes()) else {
        return Receipt::refused(nonce, Status::CreateFailed);
    };
    // SAFETY: both descriptors and the NUL-terminated single leaf are valid.
    if unsafe { libc::mkdirat(directory.as_raw_fd(), raw_leaf.as_ptr(), 0o700) } != 0 {
        return Receipt::refused(
            nonce,
            if io::Error::last_os_error().kind() == io::ErrorKind::AlreadyExists {
                Status::Collision
            } else {
                Status::CreateFailed
            },
        );
    }
    let mut receipt = Receipt {
        nonce,
        created: true,
        read_match: false,
        removed: false,
        status: Status::CreateFailed,
        cleanup: Cleanup::Unconfirmed,
    };
    let Ok(private) = open_at(
        directory.as_raw_fd(),
        &raw_leaf,
        libc::O_RDONLY | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
        0,
    ) else {
        // Without a pinned created directory, do not guess which entry to remove.
        return receipt;
    };
    let Ok(metadata) = private.metadata() else {
        return receipt;
    };
    // mkdir's mode can only be restricted by umask. Never broaden an unexpected
    // replacement's permissions or write inside a shared/private-mode mismatch.
    if !metadata.is_dir() || metadata.uid() != effective_uid() || metadata.mode() & 0o077 != 0 {
        receipt.cleanup = Cleanup::ReplacementPreserved;
        return receipt;
    }
    let mut scratch = Scratch {
        workspace: &directory,
        leaf: &raw_leaf,
        directory: private,
        directory_identity: Identity::of(&metadata),
        file: None,
        attempted_cleanup: false,
    };
    let status = scratch.roundtrip();
    receipt.read_match = status == Status::Verified;
    receipt.status = status;
    before_cleanup(&workspace.join(&leaf));
    receipt.cleanup = scratch.cleanup();
    receipt.removed = receipt.cleanup == Cleanup::Removed;
    if receipt.status == Status::Verified {
        if !receipt.removed {
            receipt.status = Status::CleanupFailed;
        } else if !std::fs::symlink_metadata(&workspace)
            .is_ok_and(|metadata| metadata.is_dir() && Identity::of(&metadata) == expected)
        {
            receipt.status = Status::WorkspaceChanged;
        }
    }
    receipt
}

struct Scratch<'a> {
    workspace: &'a File,
    leaf: &'a CStr,
    directory: File,
    directory_identity: Identity,
    file: Option<(File, Option<Identity>)>,
    attempted_cleanup: bool,
}

impl Scratch<'_> {
    fn roundtrip(&mut self) -> Status {
        let mut file = match open_at(
            self.directory.as_raw_fd(),
            c"probe",
            libc::O_RDWR | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            0o600,
        ) {
            Ok(file) => file,
            Err(error) => {
                return if error.kind() == io::ErrorKind::AlreadyExists {
                    Status::Collision
                } else {
                    Status::CreateFailed
                };
            }
        };
        let Ok(metadata) = file.metadata() else {
            // Retain the FD even when its identity could not be obtained. No
            // path deletion is safe without that identity; cleanup reports it.
            self.file = Some((file, None));
            return Status::CreateFailed;
        };
        let identity = Identity::of(&metadata);
        if !metadata.is_file() || metadata.uid() != effective_uid() || metadata.nlink() != 1 {
            self.file = Some((file, Some(identity)));
            return Status::CreateFailed;
        }
        // fchmod addresses only our O_EXCL-created FD, including under a
        // restrictive inherited umask. It never changes an existing entry.
        if file.set_permissions(Permissions::from_mode(0o600)).is_err() {
            self.file = Some((file, Some(identity)));
            return Status::CreateFailed;
        }
        let payload = *Uuid::new_v4().as_bytes();
        let result = if file.write_all(&payload).is_err() {
            Status::WriteFailed
        } else if file.seek(SeekFrom::Start(0)).is_err() {
            Status::ReadFailed
        } else {
            let mut readback = [0; 16];
            let mut extra = [0; 1];
            match (file.read_exact(&mut readback), file.read(&mut extra)) {
                (Ok(()), Ok(0)) if readback == payload => Status::Verified,
                (Ok(()), Ok(_)) => Status::ContentMismatch,
                _ => Status::ReadFailed,
            }
        };
        self.file = Some((file, Some(identity)));
        result
    }

    fn cleanup(&mut self) -> Cleanup {
        self.attempted_cleanup = true;
        let file_entry = entry_identity(self.directory.as_raw_fd(), c"probe");
        match (&self.file, file_entry) {
            (Some((file, Some(identity))), Ok(Some(current))) if current == *identity => {
                if !file
                    .metadata()
                    .is_ok_and(|metadata| Identity::of(&metadata) == *identity)
                {
                    return Cleanup::Unconfirmed;
                }
                // SAFETY: the private directory FD and literal leaf are valid;
                // the entry's identity was just compared to the created FD.
                if unsafe { libc::unlinkat(self.directory.as_raw_fd(), c"probe".as_ptr(), 0) } != 0
                {
                    return Cleanup::Unconfirmed;
                }
                if !matches!(
                    entry_identity(self.directory.as_raw_fd(), c"probe"),
                    Ok(None)
                ) || !file.metadata().is_ok_and(|metadata| metadata.nlink() == 0)
                {
                    return Cleanup::Unconfirmed;
                }
            }
            (Some((file, Some(_))), Ok(None))
                if file.metadata().is_ok_and(|metadata| metadata.nlink() == 0) => {}
            (None, Ok(None)) => {}
            (Some((_, None)), _) => return Cleanup::Unconfirmed,
            (_, Ok(Some(_))) => return Cleanup::ReplacementPreserved,
            _ => return Cleanup::Unconfirmed,
        }
        match entry_identity(self.workspace.as_raw_fd(), self.leaf) {
            Ok(Some(current)) if current == self.directory_identity => {}
            Ok(Some(_)) => return Cleanup::ReplacementPreserved,
            _ => return Cleanup::Unconfirmed,
        }
        // SAFETY: the parent FD and NUL-terminated leaf remain live. rmdir
        // refuses nonempty directories; no recursive deletion is performed.
        if unsafe {
            libc::unlinkat(
                self.workspace.as_raw_fd(),
                self.leaf.as_ptr(),
                libc::AT_REMOVEDIR,
            )
        } != 0
        {
            return Cleanup::Unconfirmed;
        }
        if matches!(
            entry_identity(self.workspace.as_raw_fd(), self.leaf),
            Ok(None)
        ) && self
            .directory
            .metadata()
            .is_ok_and(|metadata| metadata.nlink() == 0)
        {
            Cleanup::Removed
        } else {
            Cleanup::Unconfirmed
        }
    }
}

impl Drop for Scratch<'_> {
    fn drop(&mut self) {
        // Unwinding can attempt bounded cleanup, but an interrupted/killed
        // helper cannot issue a success receipt. The caller must report that
        // case as unconfirmed rather than infer removal from process exit.
        if !self.attempted_cleanup {
            let _ = self.cleanup();
        }
    }
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions.
    unsafe { libc::geteuid() }
}

fn open_at(directory: i32, name: &CStr, flags: i32, mode: libc::mode_t) -> io::Result<File> {
    // SAFETY: name is NUL-terminated, and the descriptor/flags follow openat.
    let descriptor = unsafe { libc::openat(directory, name.as_ptr(), flags, mode) };
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        // SAFETY: successful openat returns a new, uniquely owned descriptor.
        Ok(unsafe { File::from_raw_fd(descriptor) })
    }
}

fn entry_identity(directory: i32, name: &CStr) -> io::Result<Option<Identity>> {
    let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: metadata is writable, name is NUL-terminated, and fstatat does
    // not follow the named entry. It initializes metadata on success only.
    let result = unsafe {
        libc::fstatat(
            directory,
            name.as_ptr(),
            metadata.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if result == 0 {
        // SAFETY: successful fstatat initialized the complete stat record.
        let metadata = unsafe { metadata.assume_init() };
        Ok(Some(Identity {
            device: metadata.st_dev as u64,
            inode: metadata.st_ino as u64,
        }))
    } else {
        let error = io::Error::last_os_error();
        if error.kind() == io::ErrorKind::NotFound {
            Ok(None)
        } else {
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, os::unix::fs::symlink};

    fn physical(path: &Path) -> (u64, u64) {
        let metadata = fs::metadata(path).unwrap();
        (metadata.dev(), metadata.ino())
    }

    #[test]
    fn real_workspace_roundtrip_removes_private_files_and_returns_fixed_receipt() {
        let workspace = tempfile::tempdir().unwrap();
        let (device, inode) = physical(workspace.path());
        let nonce = Uuid::new_v4();
        let receipt = probe_before_cleanup(workspace.path(), device, inode, nonce, |scratch| {
            let directory = fs::metadata(scratch).unwrap();
            let file = fs::metadata(scratch.join("probe")).unwrap();
            assert_eq!(directory.mode() & 0o777, 0o700);
            assert_eq!(file.mode() & 0o777, 0o600);
            assert_eq!(file.len(), 16);
        });
        assert!(receipt.is_verified(), "{receipt:?}");
        assert_eq!(receipt.nonce, nonce);
        assert_eq!(fs::read_dir(workspace.path()).unwrap().count(), 0);
        let json = serde_json::to_value(&receipt).unwrap();
        assert_eq!(json["status"], "verified");
        assert_eq!(json["cleanup"], "removed");
        assert_eq!(serde_json::from_value::<Receipt>(json).unwrap(), receipt);
    }

    #[test]
    fn symlink_workspace_and_trailing_slash_are_refused_without_writes() {
        let root = tempfile::tempdir().unwrap();
        let workspace = root.path().join("workspace");
        let linked = root.path().join("linked");
        fs::create_dir(&workspace).unwrap();
        symlink(&workspace, &linked).unwrap();
        let (device, inode) = physical(&workspace);
        for path in [linked.clone(), linked.join("")] {
            let receipt = probe(&path, device, inode, Uuid::new_v4());
            assert_eq!(receipt.status, Status::InvalidWorkspace);
            assert_eq!(receipt.cleanup, Cleanup::NotCreated);
            assert!(!receipt.created);
        }
        assert_eq!(fs::read_dir(workspace).unwrap().count(), 0);
    }

    #[test]
    fn wrong_physical_identity_is_refused_before_creating_a_leaf() {
        let workspace = tempfile::tempdir().unwrap();
        let (device, inode) = physical(workspace.path());
        let receipt = probe(
            workspace.path(),
            device,
            inode.wrapping_add(1),
            Uuid::new_v4(),
        );
        assert_eq!(receipt.status, Status::WorkspaceChanged);
        assert_eq!(receipt.cleanup, Cleanup::NotCreated);
        assert_eq!(fs::read_dir(workspace.path()).unwrap().count(), 0);
    }

    #[test]
    fn existing_colliding_file_is_kept_byte_for_byte() {
        let workspace = tempfile::tempdir().unwrap();
        let (device, inode) = physical(workspace.path());
        let nonce = Uuid::new_v4();
        let collision = workspace.path().join(format!(".cagents-readiness-{nonce}"));
        fs::write(&collision, b"existing workspace content").unwrap();
        let before = physical(&collision);
        let receipt = probe(workspace.path(), device, inode, nonce);
        assert_eq!(receipt.status, Status::Collision);
        assert_eq!(receipt.cleanup, Cleanup::NotCreated);
        assert!(!receipt.created);
        assert_eq!(physical(&collision), before);
        assert_eq!(fs::read(collision).unwrap(), b"existing workspace content");
    }

    #[test]
    fn observed_file_replacement_is_not_unlinked() {
        let workspace = tempfile::tempdir().unwrap();
        let (device, inode) = physical(workspace.path());
        let nonce = Uuid::new_v4();
        let receipt = probe_before_cleanup(workspace.path(), device, inode, nonce, |scratch| {
            fs::rename(scratch.join("probe"), scratch.join("original")).unwrap();
            fs::write(scratch.join("probe"), b"replacement must remain").unwrap();
        });
        assert_eq!(receipt.status, Status::CleanupFailed);
        assert_eq!(receipt.cleanup, Cleanup::ReplacementPreserved);
        assert!(receipt.created && receipt.read_match && !receipt.removed);
        let scratch = workspace.path().join(format!(".cagents-readiness-{nonce}"));
        assert_eq!(
            fs::read(scratch.join("probe")).unwrap(),
            b"replacement must remain"
        );
        assert_eq!(fs::metadata(scratch.join("original")).unwrap().len(), 16);
    }

    #[test]
    fn observed_directory_replacement_is_not_removed() {
        let workspace = tempfile::tempdir().unwrap();
        let (device, inode) = physical(workspace.path());
        let nonce = Uuid::new_v4();
        let moved = workspace.path().join("original-private-directory");
        let receipt = probe_before_cleanup(workspace.path(), device, inode, nonce, |scratch| {
            fs::rename(scratch, &moved).unwrap();
            fs::create_dir(scratch).unwrap();
            fs::write(scratch.join("keep"), b"replacement directory").unwrap();
        });
        assert_eq!(receipt.status, Status::CleanupFailed);
        assert_eq!(receipt.cleanup, Cleanup::ReplacementPreserved);
        assert!(!receipt.removed);
        let replacement = workspace.path().join(format!(".cagents-readiness-{nonce}"));
        assert_eq!(
            fs::read(replacement.join("keep")).unwrap(),
            b"replacement directory"
        );
        assert!(moved.is_dir());
        assert_eq!(fs::read_dir(moved).unwrap().count(), 0);
    }
}
