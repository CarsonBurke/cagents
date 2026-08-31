//! Safe single-file loading and atomic saving for native repository editors.

use std::{
    fs::{self, File, Permissions},
    io::{self, Write},
    path::{Component, Path, PathBuf},
};

use thiserror::Error;

/// Files above this size are never loaded into an editable buffer.
pub const MAX_EDITABLE_FILE_BYTES: u64 = 8 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReadOnlyReason {
    Symlink { target: PathBuf },
    Binary,
    InvalidUtf8,
    Oversize { bytes: u64, limit: u64 },
    NotRegularFile,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileContent {
    Editable {
        text: String,
        content_hash: blake3::Hash,
        markdown: bool,
    },
    ReadOnly(ReadOnlyReason),
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum FileIoError {
    #[error("repository path must be a non-empty relative path without traversal")]
    InvalidRepositoryPath,
    #[error("repository root is unavailable: {0}")]
    InvalidRepositoryRoot(String),
    #[error("path escapes the repository worktree")]
    OutsideWorktree,
    #[error("file operation failed: {0}")]
    Io(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveOutcome {
    Saved {
        content_hash: blake3::Hash,
        durability: SaveDurability,
    },
    Conflict {
        disk_hash: blake3::Hash,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SaveDurability {
    /// The replacement and its parent-directory entry were synced.
    Durable,
    /// The destination was replaced, but syncing its parent directory failed.
    /// The new bytes are the live disk baseline even though crash durability is
    /// not guaranteed.
    Unconfirmed { error: String },
}

pub fn validate_repo_path(path: &Path) -> Result<(), FileIoError> {
    if path.as_os_str().is_empty() || path.is_absolute() {
        return Err(FileIoError::InvalidRepositoryPath);
    }
    if path
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(FileIoError::InvalidRepositoryPath);
    }
    Ok(())
}

pub fn load_file(worktree: &Path, relative_path: &Path) -> Result<FileContent, FileIoError> {
    let path = checked_path(worktree, relative_path)?;
    let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
    if metadata.file_type().is_symlink() {
        return fs::read_link(&path)
            .map(|target| FileContent::ReadOnly(ReadOnlyReason::Symlink { target }))
            .map_err(io_error);
    }
    if !metadata.is_file() {
        return Ok(FileContent::ReadOnly(ReadOnlyReason::NotRegularFile));
    }
    if metadata.len() > MAX_EDITABLE_FILE_BYTES {
        return Ok(FileContent::ReadOnly(ReadOnlyReason::Oversize {
            bytes: metadata.len(),
            limit: MAX_EDITABLE_FILE_BYTES,
        }));
    }

    let bytes = fs::read(&path).map_err(io_error)?;
    if bytes.len() as u64 > MAX_EDITABLE_FILE_BYTES {
        return Ok(FileContent::ReadOnly(ReadOnlyReason::Oversize {
            bytes: bytes.len() as u64,
            limit: MAX_EDITABLE_FILE_BYTES,
        }));
    }
    if bytes.contains(&0) {
        return Ok(FileContent::ReadOnly(ReadOnlyReason::Binary));
    }
    let text = match String::from_utf8(bytes) {
        Ok(text) => text,
        Err(_) => return Ok(FileContent::ReadOnly(ReadOnlyReason::InvalidUtf8)),
    };
    let markdown = relative_path
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            matches!(extension.to_ascii_lowercase().as_str(), "md" | "markdown")
        });
    Ok(FileContent::Editable {
        content_hash: blake3::hash(text.as_bytes()),
        text,
        markdown,
    })
}

pub fn save_file(
    worktree: &Path,
    relative_path: &Path,
    text: &str,
    expected_disk_hash: blake3::Hash,
    overwrite: bool,
) -> Result<SaveOutcome, FileIoError> {
    save_file_with_atomic_replace(
        worktree,
        relative_path,
        text,
        expected_disk_hash,
        overwrite,
        atomic_replace,
    )
}

fn save_file_with_atomic_replace(
    worktree: &Path,
    relative_path: &Path,
    text: &str,
    expected_disk_hash: blake3::Hash,
    overwrite: bool,
    replace: impl FnOnce(&Path, &[u8], Permissions) -> Result<SaveDurability, FileIoError>,
) -> Result<SaveOutcome, FileIoError> {
    let path = checked_path(worktree, relative_path)?;
    let metadata = fs::symlink_metadata(&path).map_err(io_error)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(FileIoError::Io(
            "the destination is no longer a regular file".into(),
        ));
    }

    let disk = fs::read(&path).map_err(io_error)?;
    let disk_hash = blake3::hash(&disk);
    if !overwrite && disk_hash != expected_disk_hash {
        return Ok(SaveOutcome::Conflict { disk_hash });
    }

    let durability = replace(&path, text.as_bytes(), metadata.permissions())?;
    Ok(SaveOutcome::Saved {
        content_hash: blake3::hash(text.as_bytes()),
        durability,
    })
}

fn checked_path(worktree: &Path, relative_path: &Path) -> Result<PathBuf, FileIoError> {
    validate_repo_path(relative_path)?;
    let root = fs::canonicalize(worktree)
        .map_err(|error| FileIoError::InvalidRepositoryRoot(error.to_string()))?;
    let path = root.join(relative_path);
    let parent = path.parent().ok_or(FileIoError::InvalidRepositoryPath)?;
    let canonical_parent = fs::canonicalize(parent).map_err(io_error)?;
    if !canonical_parent.starts_with(&root) {
        return Err(FileIoError::OutsideWorktree);
    }
    Ok(path)
}

fn atomic_replace(
    path: &Path,
    bytes: &[u8],
    permissions: Permissions,
) -> Result<SaveDurability, FileIoError> {
    atomic_replace_with_hooks(
        path,
        bytes,
        permissions,
        || Ok(()),
        |parent| File::open(parent).and_then(|directory| directory.sync_all()),
    )
}

fn atomic_replace_with_hooks(
    path: &Path,
    bytes: &[u8],
    permissions: Permissions,
    before_persist: impl FnOnce() -> io::Result<()>,
    sync_parent: impl FnOnce(&Path) -> io::Result<()>,
) -> Result<SaveDurability, FileIoError> {
    let parent = path.parent().ok_or(FileIoError::InvalidRepositoryPath)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(io_error)?;
    temporary
        .as_file_mut()
        .set_permissions(permissions)
        .map_err(io_error)?;
    temporary.write_all(bytes).map_err(io_error)?;
    temporary.flush().map_err(io_error)?;
    temporary.as_file().sync_all().map_err(io_error)?;
    before_persist().map_err(io_error)?;
    temporary
        .persist(path)
        .map_err(|error| io_error(error.error))?;
    // `persist` has replaced the destination. From this point onward, an I/O
    // failure must be reported as a successful replacement with unconfirmed
    // durability; returning `Err` would falsely tell callers the old baseline
    // is still on disk.
    Ok(match sync_parent(parent) {
        Ok(()) => SaveDurability::Durable,
        Err(error) => SaveDurability::Unconfirmed {
            error: error.to_string(),
        },
    })
}

fn io_error(error: io::Error) -> FileIoError {
    FileIoError::Io(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::{PermissionsExt, symlink};

    use tempfile::tempdir;

    use super::*;

    #[test]
    fn classifies_unsafe_file_inputs_without_partial_text() {
        let root = tempdir().unwrap();
        fs::write(root.path().join("binary.bin"), b"hello\0world").unwrap();
        fs::write(root.path().join("invalid.txt"), [0xff, 0xfe]).unwrap();
        symlink("binary.bin", root.path().join("link.txt")).unwrap();

        assert_eq!(
            load_file(root.path(), Path::new("binary.bin")).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::Binary)
        );
        assert_eq!(
            load_file(root.path(), Path::new("invalid.txt")).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::InvalidUtf8)
        );
        assert_eq!(
            load_file(root.path(), Path::new("link.txt")).unwrap(),
            FileContent::ReadOnly(ReadOnlyReason::Symlink {
                target: PathBuf::from("binary.bin")
            })
        );
    }

    #[test]
    fn atomic_save_preserves_mode_and_detects_external_changes() {
        let root = tempdir().unwrap();
        let path = root.path().join("notes.md");
        fs::write(&path, "before\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        let expected = blake3::hash(b"before\n");

        fs::write(&path, "external\n").unwrap();
        assert!(matches!(
            save_file(
                root.path(),
                Path::new("notes.md"),
                "mine\n",
                expected,
                false
            )
            .unwrap(),
            SaveOutcome::Conflict { .. }
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "external\n");

        let outcome =
            save_file(root.path(), Path::new("notes.md"), "mine\n", expected, true).unwrap();
        assert!(matches!(
            outcome,
            SaveOutcome::Saved {
                durability: SaveDurability::Durable,
                ..
            }
        ));
        assert_eq!(fs::read_to_string(&path).unwrap(), "mine\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
    }

    #[test]
    fn failure_before_persist_leaves_destination_unchanged_and_returns_error() {
        let root = tempdir().unwrap();
        let path = root.path().join("notes.txt");
        fs::write(&path, "before\n").unwrap();
        let expected = blake3::hash(b"before\n");

        let result = save_file_with_atomic_replace(
            root.path(),
            Path::new("notes.txt"),
            "after\n",
            expected,
            false,
            |path, bytes, permissions| {
                atomic_replace_with_hooks(
                    path,
                    bytes,
                    permissions,
                    || Err(io::Error::other("injected failure before persist")),
                    |_| panic!("parent sync must not run when persist was not attempted"),
                )
            },
        );

        assert!(matches!(
            result,
            Err(FileIoError::Io(error)) if error.contains("injected failure before persist")
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "before\n");
    }

    #[test]
    fn parent_sync_failure_reports_replaced_but_unconfirmed_destination() {
        let root = tempdir().unwrap();
        let path = root.path().join("notes.txt");
        fs::write(&path, "before\n").unwrap();
        let expected = blake3::hash(b"before\n");

        let result = save_file_with_atomic_replace(
            root.path(),
            Path::new("notes.txt"),
            "after\n",
            expected,
            false,
            |path, bytes, permissions| {
                atomic_replace_with_hooks(
                    path,
                    bytes,
                    permissions,
                    || Ok(()),
                    |_| Err(io::Error::other("injected parent sync failure")),
                )
            },
        )
        .unwrap();

        assert!(matches!(
            result,
            SaveOutcome::Saved {
                content_hash,
                durability: SaveDurability::Unconfirmed { error },
            } if content_hash == blake3::hash(b"after\n")
                && error.contains("injected parent sync failure")
        ));
        assert_eq!(fs::read_to_string(path).unwrap(), "after\n");
    }

    #[test]
    fn rejects_traversal() {
        let root = tempdir().unwrap();
        assert_eq!(
            load_file(root.path(), Path::new("../outside")),
            Err(FileIoError::InvalidRepositoryPath)
        );
    }
}
