//! A current host receipt authorizes one private transcript, never an arbitrary
//! saved path. Every activation opens and checks the directory and file again.

use std::{
    ffi::CString,
    fs::File,
    os::{
        fd::{AsRawFd, FromRawFd},
        unix::{ffi::OsStrExt, fs::MetadataExt},
    },
    path::{Component, Path, PathBuf},
};

use crate::model::{TerminalId, TranscriptLocationReceipt, TranscriptMetadata};

#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
pub enum Unavailable {
    #[error("Current transcript evidence is unavailable")]
    MetadataMissing,
    #[error("The transcript is not in safe private storage")]
    UnsafeStorage,
    #[error("The transcript location has changed")]
    Changed,
    #[error("The desktop could not open the transcript folder")]
    DesktopUnavailable,
}

fn root() -> Result<PathBuf, Unavailable> {
    crate::instance::project_dirs()
        .map(|dirs| dirs.data_dir().join("transcripts"))
        .ok_or(Unavailable::MetadataMissing)
}

fn open_at(parent: &File, name: &[u8], directory: bool) -> Result<File, Unavailable> {
    let name = CString::new(name).map_err(|_| Unavailable::UnsafeStorage)?;
    let flags = libc::O_RDONLY
        | libc::O_CLOEXEC
        | libc::O_NOFOLLOW
        | libc::O_NONBLOCK
        | if directory { libc::O_DIRECTORY } else { 0 };
    // SAFETY: parent and name remain valid throughout openat; the returned
    // descriptor is transferred exactly once to File.
    let fd = unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags) };
    if fd < 0 {
        return Err(Unavailable::UnsafeStorage);
    }
    Ok(unsafe { File::from_raw_fd(fd) })
}

fn directory(path: &Path) -> Result<File, Unavailable> {
    if !path.is_absolute() {
        return Err(Unavailable::UnsafeStorage);
    }
    let mut current = File::open("/").map_err(|_| Unavailable::UnsafeStorage)?;
    for component in path.components() {
        match component {
            Component::RootDir => {}
            Component::Normal(name) => current = open_at(&current, name.as_bytes(), true)?,
            _ => return Err(Unavailable::UnsafeStorage),
        }
    }
    Ok(current)
}

/// Keeps both descriptors alive through desktop activation. The portal receives
/// the transcript file descriptor: OpenDirectory opens its containing folder.
pub struct ValidatedTranscript {
    _root: File,
    _file: File,
    #[cfg(feature = "ui")]
    terminal: TerminalId,
    #[cfg(feature = "ui")]
    path: PathBuf,
    receipt: TranscriptLocationReceipt,
}

fn open_in(
    root_path: &Path,
    terminal: TerminalId,
    path: &Path,
) -> Result<ValidatedTranscript, Unavailable> {
    let name = format!("{terminal}.log");
    if path != root_path.join(&name) {
        return Err(Unavailable::UnsafeStorage);
    }
    let root = directory(root_path)?;
    let root_metadata = root.metadata().map_err(|_| Unavailable::UnsafeStorage)?;
    // SAFETY: geteuid has no preconditions.
    let uid = unsafe { libc::geteuid() };
    if !root_metadata.is_dir()
        || root_metadata.uid() != uid
        || root_metadata.mode() & 0o7777 != 0o700
    {
        return Err(Unavailable::UnsafeStorage);
    }
    let file = open_at(&root, name.as_bytes(), false)?;
    let file_metadata = file.metadata().map_err(|_| Unavailable::UnsafeStorage)?;
    if !file_metadata.is_file()
        || file_metadata.uid() != uid
        || file_metadata.mode() & 0o7777 != 0o600
        || file_metadata.nlink() != 1
    {
        return Err(Unavailable::UnsafeStorage);
    }
    let receipt = TranscriptLocationReceipt {
        root_device: root_metadata.dev(),
        root_inode: root_metadata.ino(),
        file_device: file_metadata.dev(),
        file_inode: file_metadata.ino(),
    };
    Ok(ValidatedTranscript {
        _root: root,
        _file: file,
        #[cfg(feature = "ui")]
        terminal,
        #[cfg(feature = "ui")]
        path: path.to_owned(),
        receipt,
    })
}

#[cfg(any(feature = "host", test))]
pub(crate) fn capture(
    terminal: TerminalId,
    path: &Path,
    writer: &File,
) -> Option<ValidatedTranscript> {
    capture_in(&root().ok()?, terminal, path, writer)
}

#[cfg(any(feature = "host", test))]
fn capture_in(
    root: &Path,
    terminal: TerminalId,
    path: &Path,
    writer: &File,
) -> Option<ValidatedTranscript> {
    let validated = open_in(root, terminal, path).ok()?;
    let original = writer.metadata().ok()?;
    // The pathname is not the source of truth: a replacement must never
    // receive a newly minted receipt for a different writer's transcript.
    if (original.dev(), original.ino())
        != (validated.receipt.file_device, validated.receipt.file_inode)
    {
        return None;
    }
    Some(validated)
}

pub fn open(
    terminal: TerminalId,
    metadata: &TranscriptMetadata,
) -> Result<ValidatedTranscript, Unavailable> {
    open_with_receipt(&root()?, terminal, metadata)
}

fn open_with_receipt(
    root: &Path,
    terminal: TerminalId,
    metadata: &TranscriptMetadata,
) -> Result<ValidatedTranscript, Unavailable> {
    let receipt = metadata.location.ok_or(Unavailable::MetadataMissing)?;
    let validated = open_in(root, terminal, &metadata.path)?;
    if validated.receipt != receipt {
        return Err(Unavailable::Changed);
    }
    Ok(validated)
}

impl ValidatedTranscript {
    #[cfg(any(feature = "host", test))]
    pub(crate) fn receipt(&self) -> TranscriptLocationReceipt {
        self.receipt
    }
    #[cfg(feature = "ui")]
    pub async fn reveal(self) -> Result<(), Unavailable> {
        use std::time::Duration;
        // Recheck immediately before activation; creating a task is not a
        // reservation of the directory or pathname.
        let current = open_in(&root()?, self.terminal, &self.path)?;
        if current.receipt != self.receipt {
            return Err(Unavailable::Changed);
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            let request = ashpd::desktop::open_uri::OpenDirectoryRequest::default()
                .send(&current._file)
                .await
                .map_err(|_| Unavailable::DesktopUnavailable)?;
            request
                .response()
                .map_err(|_| Unavailable::DesktopUnavailable)
        })
        .await
        .map_err(|_| Unavailable::DesktopUnavailable)?
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        os::unix::fs::{PermissionsExt, symlink},
    };

    fn fixture() -> (tempfile::TempDir, TerminalId, PathBuf) {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        let terminal = TerminalId::new();
        let file = root.path().join(format!("{terminal}.log"));
        fs::write(&file, "private transcript contents").unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        (root, terminal, file)
    }

    #[test]
    fn exact_private_file_keeps_root_and_file_descriptors() {
        let (root, terminal, path) = fixture();
        let opened = open_in(root.path(), terminal, &path).unwrap();
        assert_eq!(
            opened._root.metadata().unwrap().ino(),
            opened.receipt.root_inode
        );
        assert_eq!(
            opened._file.metadata().unwrap().ino(),
            opened.receipt.file_inode
        );
        assert!(open_in(root.path(), TerminalId::new(), &path).is_err());
        assert!(open_in(root.path(), terminal, &root.path().join("other.log")).is_err());
    }

    #[test]
    fn symbolic_links_hard_links_and_public_storage_are_unavailable() {
        let (root, terminal, path) = fixture();
        let original = root.path().join("original");
        fs::rename(&path, &original).unwrap();
        symlink(&original, &path).unwrap();
        assert!(open_in(root.path(), terminal, &path).is_err());
        fs::remove_file(&path).unwrap();
        fs::hard_link(&original, &path).unwrap();
        assert!(open_in(root.path(), terminal, &path).is_err());
        fs::remove_file(&original).unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open_in(root.path(), terminal, &path).is_err());
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o755)).unwrap();
        assert!(open_in(root.path(), terminal, &path).is_err());
    }

    #[test]
    fn replacing_file_or_root_changes_receipt_even_with_same_contents() {
        let (root, terminal, path) = fixture();
        let before = open_in(root.path(), terminal, &path).unwrap();
        let mut metadata = TranscriptMetadata {
            path: path.clone(),
            byte_len: 0,
            durable_offset: 0,
            updated_at_unix_ms: 0,
            location: None,
        };
        assert!(matches!(
            open_with_receipt(root.path(), terminal, &metadata),
            Err(Unavailable::MetadataMissing)
        ));
        metadata.location = Some(before.receipt);
        assert!(open_with_receipt(root.path(), terminal, &metadata).is_ok());
        let old = root.path().join("old");
        fs::rename(&path, &old).unwrap();
        fs::copy(&old, &path).unwrap();
        assert_ne!(
            before.receipt,
            open_in(root.path(), terminal, &path).unwrap().receipt
        );
        assert!(matches!(
            open_with_receipt(root.path(), terminal, &metadata),
            Err(Unavailable::Changed)
        ));
        let parent = tempfile::tempdir().unwrap();
        let alias = parent.path().join("transcripts");
        symlink(root.path(), &alias).unwrap();
        assert!(open_in(&alias, terminal, &alias.join(path.file_name().unwrap())).is_err());
    }

    #[test]
    fn replacing_private_root_invalidates_receipt() {
        let parent = tempfile::tempdir().unwrap();
        let root = parent.path().join("transcripts");
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        let terminal = TerminalId::new();
        let path = root.join(format!("{terminal}.log"));
        fs::write(&path, "same bytes").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let receipt = open_in(&root, terminal, &path).unwrap().receipt;
        let metadata = TranscriptMetadata {
            path: path.clone(),
            byte_len: 10,
            durable_offset: 0,
            updated_at_unix_ms: 0,
            location: Some(receipt),
        };
        let previous = parent.path().join("previous");
        fs::rename(&root, &previous).unwrap();
        fs::create_dir(&root).unwrap();
        fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
        fs::rename(previous.join(path.file_name().unwrap()), &path).unwrap();
        assert!(matches!(
            open_with_receipt(&root, terminal, &metadata),
            Err(Unavailable::Changed)
        ));
    }

    #[test]
    fn replacing_path_cannot_mint_a_receipt_for_the_original_writer() {
        let (root, terminal, path) = fixture();
        let original = File::open(&path).unwrap();
        let anchor = capture_in(root.path(), terminal, &path, &original).unwrap();
        fs::rename(&path, root.path().join("previous.log")).unwrap();
        fs::write(&path, "replacement contents").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        assert!(capture_in(root.path(), terminal, &path, &original).is_none());
        let metadata = TranscriptMetadata {
            path,
            byte_len: 0,
            durable_offset: 0,
            updated_at_unix_ms: 0,
            location: Some(anchor.receipt()),
        };
        assert!(matches!(
            open_with_receipt(root.path(), terminal, &metadata),
            Err(Unavailable::Changed)
        ));
        fs::remove_file(root.path().join("previous.log")).unwrap();
        drop(original);
        assert_eq!(anchor._file.metadata().unwrap().nlink(), 0);
        assert_eq!(
            anchor._file.metadata().unwrap().ino(),
            anchor.receipt().file_inode
        );
    }
}
