use std::{
    fs::{self, DirBuilder},
    io,
    os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt},
    path::Path,
};
#[cfg(any(feature = "host", feature = "ui"))]
use std::{
    fs::{File, OpenOptions},
    os::unix::fs::OpenOptionsExt,
};

const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
#[cfg(any(feature = "host", feature = "ui"))]
const PRIVATE_FILE_MODE: u32 = 0o600;
#[cfg(any(feature = "host", feature = "ui"))]
const O_NOFOLLOW: i32 = libc::O_NOFOLLOW;
#[cfg(any(feature = "host", feature = "ui"))]
const O_NONBLOCK: i32 = libc::O_NONBLOCK;

fn storage_error(path: &Path, message: impl std::fmt::Display) -> io::Error {
    io::Error::new(
        io::ErrorKind::PermissionDenied,
        format!("unsafe storage path {}: {message}", path.display()),
    )
}

fn effective_uid() -> u32 {
    // SAFETY: geteuid has no preconditions and does not modify memory.
    unsafe { libc::geteuid() }
}

pub(crate) fn reject_symlink_components(path: &Path) -> io::Result<()> {
    let mut ancestors = path
        .ancestors()
        .filter(|path| !path.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    for component in ancestors {
        match fs::symlink_metadata(component) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(storage_error(component, "symbolic links are not allowed"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

pub(crate) fn validate_owned_directory(path: &Path) -> io::Result<()> {
    let metadata = fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(storage_error(path, "expected a real directory"));
    }
    let expected_uid = effective_uid();
    if metadata.uid() != expected_uid {
        return Err(storage_error(
            path,
            format!(
                "directory is owned by uid {}, expected {expected_uid}",
                metadata.uid()
            ),
        ));
    }
    Ok(())
}

pub(crate) fn prepare_private_directory(path: &Path) -> io::Result<()> {
    let path = if path.as_os_str().is_empty() {
        Path::new(".")
    } else {
        path
    };
    reject_symlink_components(path)?;
    match fs::symlink_metadata(path) {
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            let mut builder = DirBuilder::new();
            builder.recursive(true).mode(PRIVATE_DIRECTORY_MODE);
            builder.create(path)?;
        }
        Err(error) => return Err(error),
    }
    reject_symlink_components(path)?;
    validate_owned_directory(path)?;
    fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE))?;
    validate_owned_directory(path)
}

#[cfg(any(feature = "host", feature = "ui"))]
fn validate_owned_regular_file(path: &Path, metadata: &fs::Metadata) -> io::Result<()> {
    if !metadata.is_file() {
        return Err(storage_error(path, "expected a regular file"));
    }
    let expected_uid = effective_uid();
    if metadata.uid() != expected_uid {
        return Err(storage_error(
            path,
            format!(
                "file is owned by uid {}, expected {expected_uid}",
                metadata.uid()
            ),
        ));
    }
    Ok(())
}

#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn migrate_existing_private_file(path: &Path) -> io::Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() {
                return Err(storage_error(path, "symbolic links are not allowed"));
            }
            validate_owned_regular_file(path, &metadata)?;
            fs::set_permissions(path, fs::Permissions::from_mode(PRIVATE_FILE_MODE))?;
            Ok(true)
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(any(feature = "host", feature = "ui"))]
pub(crate) fn validate_and_privatize_open_file(path: &Path, file: &File) -> io::Result<()> {
    let metadata = file.metadata()?;
    validate_owned_regular_file(path, &metadata)?;
    file.set_permissions(fs::Permissions::from_mode(PRIVATE_FILE_MODE))
}

#[cfg(any(feature = "ui", feature = "host"))]
pub(crate) fn open_existing_private_file(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)?;
    validate_and_privatize_open_file(path, &file)?;
    Ok(file)
}

/// Inspection must not repair permissions or change the receipt's metadata.
/// An unsafe or multiply linked file is unavailable, never silently migrated.
#[cfg(any(feature = "ui", feature = "host"))]
pub(crate) fn open_existing_private_file_readonly(path: &Path) -> io::Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK | libc::O_CLOEXEC)
        .open(path)?;
    let metadata = file.metadata()?;
    validate_owned_regular_file(path, &metadata)?;
    if metadata.mode() & 0o077 != 0 || metadata.nlink() != 1 {
        return Err(storage_error(
            path,
            "inspection requires a singly linked private file",
        ));
    }
    Ok(file)
}

#[cfg(feature = "host")]
pub(crate) fn open_private_append_file(path: &Path) -> io::Result<File> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    prepare_private_directory(parent)?;
    migrate_existing_private_file(path)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(PRIVATE_FILE_MODE)
        .custom_flags(O_NOFOLLOW | O_NONBLOCK)
        .open(path)?;
    validate_and_privatize_open_file(path, &file)?;
    Ok(file)
}

#[cfg(all(test, any(feature = "ui", feature = "host")))]
mod readonly_tests {
    use super::*;

    #[test]
    fn inspecting_a_private_receipt_does_not_rewrite_permissions_or_metadata() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("receipt");
        fs::write(&path, b"exact saved receipt").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let before = fs::metadata(&path).unwrap();
        let file = open_existing_private_file_readonly(&path).unwrap();
        let after = file.metadata().unwrap();
        assert_eq!(before.mode(), after.mode());
        assert_eq!(
            (before.ctime(), before.ctime_nsec()),
            (after.ctime(), after.ctime_nsec())
        );
        // SAFETY: scalar fcntl query on the live descriptor.
        use std::os::fd::AsRawFd;
        assert_ne!(
            unsafe { libc::fcntl(file.as_raw_fd(), libc::F_GETFD) } & libc::FD_CLOEXEC,
            0
        );
        drop(file);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
        assert!(open_existing_private_file_readonly(&path).is_err());
        assert_eq!(
            fs::metadata(&path).unwrap().mode() & 0o777,
            0o644,
            "inspection refuses instead of repairing"
        );
    }

    #[test]
    fn receipt_inspection_refuses_symlinks_and_extra_links() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("receipt");
        fs::write(&path, b"receipt").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let alias = directory.path().join("alias");
        std::os::unix::fs::symlink(&path, &alias).unwrap();
        assert!(open_existing_private_file_readonly(&alias).is_err());
        fs::remove_file(&alias).unwrap();
        fs::hard_link(&path, &alias).unwrap();
        assert!(open_existing_private_file_readonly(&path).is_err());
    }
}
