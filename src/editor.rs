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
    Saved { content_hash: blake3::Hash },
    Conflict { disk_hash: blake3::Hash },
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

    atomic_replace(&path, text.as_bytes(), metadata.permissions())?;
    Ok(SaveOutcome::Saved {
        content_hash: blake3::hash(text.as_bytes()),
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

fn atomic_replace(path: &Path, bytes: &[u8], permissions: Permissions) -> Result<(), FileIoError> {
    let parent = path.parent().ok_or(FileIoError::InvalidRepositoryPath)?;
    let mut temporary = tempfile::NamedTempFile::new_in(parent).map_err(io_error)?;
    temporary
        .as_file_mut()
        .set_permissions(permissions)
        .map_err(io_error)?;
    temporary.write_all(bytes).map_err(io_error)?;
    temporary.flush().map_err(io_error)?;
    temporary.as_file().sync_all().map_err(io_error)?;
    temporary
        .persist(path)
        .map_err(|error| io_error(error.error))?;
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(io_error)
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
        assert!(matches!(outcome, SaveOutcome::Saved { .. }));
        assert_eq!(fs::read_to_string(&path).unwrap(), "mine\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );
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
