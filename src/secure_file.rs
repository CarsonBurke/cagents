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

pub fn reject_symlink_components(path: &Path) -> io::Result<()> {
    let mut ancestors = path
        .ancestors()
        .filter(|path| !path.as_os_str().is_empty())
        .collect::<Vec<_>>();
    ancestors.reverse();
    for component in ancestors {
        match fs::symlink_metadata(component) {
            Ok(metadata)
                if metadata.file_type().is_symlink()
                    && !is_sealed_system_link(component, &metadata) =>
            {
                return Err(storage_error(component, "symbolic links are not allowed"));
            }
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// `path` as SQLite must be given it to open a store with
/// `SQLITE_OPEN_NOFOLLOW`. That flag refuses a symbolic link anywhere in the
/// path, not only at the database itself, so a path that leads through one of
/// the sealed system links [`reject_symlink_components`] accepts, as every
/// path under Darwin's `/tmp` and `$TMPDIR` does, is named through the link's
/// target. Any other link is left for SQLite to refuse. Linux has no sealed
/// link, so there `path` is returned as it is.
pub fn sqlite_path(path: &Path) -> std::borrow::Cow<'_, Path> {
    sealed_system_path(path)
}

/// Spell a path through the target of Darwin's immutable system link, when
/// present. Other components are carried over without resolving them.
pub(crate) fn sealed_system_path(path: &Path) -> std::borrow::Cow<'_, Path> {
    #[cfg(target_os = "macos")]
    {
        let mut components = path.components();
        if let (Some(std::path::Component::RootDir), Some(std::path::Component::Normal(name))) =
            (components.next(), components.next())
        {
            let link = Path::new("/").join(name);
            if fs::symlink_metadata(&link)
                .is_ok_and(|metadata| is_sealed_system_link(&link, &metadata))
            {
                let mut target = Path::new("/private").join(name);
                let rest = components.as_path();
                if !rest.as_os_str().is_empty() {
                    target.push(rest);
                }
                return target.into();
            }
        }
    }
    path.into()
}

/// Darwin reaches `/var`, `/tmp` and `/etc` through root-owned links in the
/// sealed system volume, which even root cannot rewrite while it is mounted
/// read-only. Such a link cannot be substituted, so it is no hazard, and every
/// component after it is still checked. Only those `/<name> -> private/<name>`
/// links qualify, not other root links such as `synthetic.conf` entries.
/// Linux has no equivalent.
#[cfg(target_os = "macos")]
pub(crate) fn is_sealed_system_link(component: &Path, metadata: &fs::Metadata) -> bool {
    let Some(name) = component.file_name() else {
        return false;
    };
    if component.parent() != Some(Path::new("/"))
        || metadata.uid() != 0
        || !matches!(name.as_encoded_bytes(), b"tmp" | b"var" | b"etc")
        || fs::read_link(component).ok() != Some(Path::new("private").join(name))
    {
        return false;
    }
    let mut root = std::mem::MaybeUninit::<libc::statvfs>::zeroed();
    // SAFETY: the path literal is static and statvfs fills the buffer.
    if unsafe { libc::statvfs(c"/".as_ptr(), root.as_mut_ptr()) } != 0 {
        return false;
    }
    // SAFETY: statvfs succeeded, so the buffer is initialized.
    unsafe { root.assume_init() }.f_flag & libc::ST_RDONLY != 0
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn is_sealed_system_link(_component: &Path, _metadata: &fs::Metadata) -> bool {
    false
}

pub fn validate_owned_directory(path: &Path) -> io::Result<()> {
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

pub fn prepare_private_directory(path: &Path) -> io::Result<()> {
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
pub fn open_existing_private_file_readonly(path: &Path) -> io::Result<File> {
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
pub fn open_private_append_file(path: &Path) -> io::Result<File> {
    open_private_append(path, false)
}

#[cfg(feature = "host")]
pub(crate) fn open_private_read_append_file(path: &Path) -> io::Result<File> {
    open_private_append(path, true)
}

#[cfg(feature = "host")]
fn open_private_append(path: &Path, read: bool) -> io::Result<File> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    prepare_private_directory(parent)?;
    migrate_existing_private_file(path)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .read(read)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_paths_pass_through_every_link_but_a_sealed_system_one() {
        let directory = tempfile::tempdir().unwrap();
        // Resolve the fixture root before adding the ordinary link. The
        // temporary directory itself may be beneath a sealed system link.
        let root = directory.path().canonicalize().unwrap();
        let real = root.join("real");
        fs::create_dir(&real).unwrap();
        let link = root.join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let store = link.join("store.sqlite3");
        // An ordinary link is SQLite's to refuse.
        assert_eq!(sqlite_path(&store), store.as_path());
        assert_eq!(
            sqlite_path(Path::new("relative.sqlite3")),
            Path::new("relative.sqlite3")
        );
        #[cfg(target_os = "linux")]
        assert_eq!(
            sqlite_path(Path::new("/tmp/store.sqlite3")),
            Path::new("/tmp/store.sqlite3")
        );
        #[cfg(target_os = "macos")]
        {
            assert_eq!(
                sqlite_path(Path::new("/tmp/a/store.sqlite3")),
                Path::new("/private/tmp/a/store.sqlite3")
            );
            assert_eq!(sqlite_path(Path::new("/var")), Path::new("/private/var"));
            assert_eq!(
                sqlite_path(Path::new("/private/tmp/store.sqlite3")),
                Path::new("/private/tmp/store.sqlite3")
            );
        }
    }

    /// A store under a sealed system link, as every store a test keeps in
    /// Darwin's `$TMPDIR` is, opens with `SQLITE_OPEN_NOFOLLOW` through it.
    #[cfg(any(feature = "host", feature = "ui"))]
    #[test]
    fn a_store_opens_nofollow_through_its_sqlite_path() {
        use rusqlite::{Connection, OpenFlags};
        let directory = tempfile::tempdir_in(std::env::temp_dir()).unwrap();
        let path = directory.path().join("store.sqlite3");
        let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW;
        Connection::open_with_flags(sqlite_path(&path), flags)
            .unwrap()
            .execute_batch("CREATE TABLE t (x)")
            .unwrap();
        // The database itself must still not be a link.
        let link = directory.path().join("link.sqlite3");
        std::os::unix::fs::symlink(&path, &link).unwrap();
        assert!(Connection::open_with_flags(sqlite_path(&link), flags).is_err());
    }
}
