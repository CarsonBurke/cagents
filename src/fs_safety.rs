//! Filesystem operations that stay inside a directory tree.
//!
//! Two properties are impossible to get from path-based `std::fs` calls, and
//! both matter because a repository's contents are untrusted input.
//!
//! 1. **A check and the operation that follows it must describe the same
//!    object.** `symlink_metadata(path)` followed by `fs::read(path)` resolves
//!    the name twice, so anything that changes the name in between — a build,
//!    a `git checkout`, a hostile local process — makes the second call read
//!    something the first never classified. Every function here works from an
//!    already-open descriptor, so the object cannot be swapped underneath a
//!    decision that was made about it.
//! 2. **A repository must never redirect a read or a write outside the
//!    worktree.** `O_NOFOLLOW` rejects only a symlinked *final* component; a
//!    symlinked *parent* directory is still traversed, so replacing `src` with
//!    a link to `/etc` turns `src/passwd` into an editable — and saveable —
//!    file outside the tree. Resolution here refuses a symlink in any
//!    component and refuses to leave the root at all.
//!
//! On Linux, resolution prefers `openat2` with `RESOLVE_BENEATH |
//! RESOLVE_NO_SYMLINKS`, which enforces both properties in the kernel in one
//! syscall. Kernels older than 5.6 answer `ENOSYS`, and seccomp filters that do
//! not know the syscall answer `EINVAL`; those fall back to an `openat` walk
//! that opens one component at a time with `O_NOFOLLOW` and verifies each
//! intermediate really is a directory. Which of the two is in use is decided
//! once, not per call.
//!
//! On macOS, `openat` with `O_NOFOLLOW_ANY` refuses a symlink in any component
//! in the kernel, and [`components`] has already refused every `..`, `.` and
//! absolute component, so the one call cannot leave the root either. Directory
//! handles there are `O_SEARCH`, Darwin's nearest equivalent of `O_PATH`: they
//! grant lookup beneath the directory and nothing else.

use std::{
    ffi::{CStr, CString, OsStr, OsString},
    fmt,
    fs::{File, Metadata},
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd, RawFd},
        unix::ffi::{OsStrExt, OsStringExt},
    },
    path::{Component, Path, PathBuf},
    ptr,
};

/// Bytes of randomness in a generated temporary name.
///
/// A temporary file is created `O_EXCL` inside a directory another user may be
/// able to write, so its name must not be guessable: a predictable name lets
/// that user pre-create the entry and turn every save into a failure, or worse
/// pre-create it as something the rename then has to deal with.
const TEMPORARY_NAME_BYTES: usize = 16;

/// Attempts made to find an unused temporary name before giving up.
///
/// With [`TEMPORARY_NAME_BYTES`] of randomness a single collision is already
/// implausible, so this bound exists only so a directory that answers `EEXIST`
/// for some other reason cannot spin forever.
const TEMPORARY_NAME_ATTEMPTS: usize = 8;

/// Attempts made to read an extended-attribute list or value whose size
/// changed between being asked for and being read.
///
/// Each retry means another process wrote an attribute in the microseconds
/// between two syscalls, so this bound exists only so that a process doing
/// that in a loop cannot make a save spin.
const ATTRIBUTE_SIZE_ATTEMPTS: usize = 4;

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
compile_error!(
    "fs_safety confines paths with Linux openat2/O_PATH or macOS O_NOFOLLOW_ANY/O_SEARCH; \
     another platform needs its own backend"
);

/// `struct open_how` from `linux/openat2.h`.
///
/// Declared here rather than reusing `libc::open_how`, which is
/// `#[non_exhaustive]` and therefore cannot be built from a literal; a zeroed
/// value would need an `unsafe` block for no benefit.
#[cfg(target_os = "linux")]
#[repr(C)]
#[derive(Default)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// Flags for a directory handle that grants lookup beneath it and nothing
/// else: `O_PATH` on Linux, which needs no permission on the directory at all,
/// and `O_SEARCH` on macOS, which needs search (execute) permission. So on
/// macOS a directory without search permission cannot be held as a [`Root`],
/// and an ancestor walk stops below one; neither can be resolved beneath
/// there in any case.
#[cfg(target_os = "linux")]
const DIRECTORY_HANDLE: libc::c_int = libc::O_PATH | libc::O_DIRECTORY;
#[cfg(target_os = "macos")]
const DIRECTORY_HANDLE: libc::c_int = libc::O_SEARCH;

/// Flags for an intermediate component of the component-at-a-time walk. The
/// walk checks each one is a directory, so Linux needs no `O_DIRECTORY`; on
/// macOS `O_SEARCH` implies it, and a non-directory answers `ENOTDIR`.
#[cfg(target_os = "linux")]
const INTERMEDIATE_HANDLE: libc::c_int = libc::O_PATH;
#[cfg(target_os = "macos")]
const INTERMEDIATE_HANDLE: libc::c_int = libc::O_SEARCH;

/// The errno an extended-attribute call answers for an absent attribute.
#[cfg(target_os = "linux")]
const NO_ATTRIBUTE: libc::c_int = libc::ENODATA;
#[cfg(target_os = "macos")]
const NO_ATTRIBUTE: libc::c_int = libc::ENOATTR;

/// A directory a path may be resolved beneath.
///
/// The descriptor is a [`DIRECTORY_HANDLE`], which cannot be used for anything
/// but further fd-relative resolution and the directory's own metadata — the
/// least authority that still lets this module work.
#[derive(Debug)]
pub struct Root {
    directory: OwnedFd,
    path: PathBuf,
}

impl Root {
    /// Opens `root` itself without following a final symlink.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] when `root` cannot be opened, is
    /// not a directory, or is a symbolic link.
    pub fn open(root: &Path) -> io::Result<Self> {
        #[cfg(feature = "ui")]
        if crate::device::is_remote() {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "remote paths must be opened through the selected daemon",
            ));
        }
        let name = c_path(root)?;
        let flags = DIRECTORY_HANDLE | libc::O_NOFOLLOW | libc::O_CLOEXEC;
        // SAFETY: `name` is a NUL-terminated string that outlives the call, and
        // `open` returns either a fresh owned descriptor or -1.
        let descriptor = unsafe { libc::open(name.as_ptr(), flags) };
        if descriptor < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `descriptor` was just returned by a successful `open` and is
        // not owned anywhere else.
        let directory = unsafe { OwnedFd::from_raw_fd(descriptor) };
        Ok(Self {
            directory,
            path: root.to_path_buf(),
        })
    }

    /// The path this root was opened from, for error messages.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Duplicates the root, so a caller can keep resolving beneath it after
    /// handing one copy away.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] when the descriptor cannot be
    /// duplicated.
    pub fn try_clone(&self) -> io::Result<Self> {
        Ok(Self {
            directory: self.directory.try_clone()?,
            path: self.path.clone(),
        })
    }

    /// Duplicate the retained directory for a command that must resolve the
    /// same object through an inherited descriptor, independent of its name.
    pub(crate) fn retained_directory(&self) -> io::Result<File> {
        self.directory.try_clone().map(File::from)
    }

    /// This root's own metadata, for a caller that needs the directory's
    /// identity — its device and inode — rather than the name it was reached
    /// by. `fstat` answers through an `O_PATH` descriptor, so this needs no
    /// permission the root does not already have.
    ///
    /// # Errors
    ///
    /// Returns the underlying [`io::Error`] when the descriptor cannot be
    /// stat'ed.
    pub fn metadata(&self) -> io::Result<Metadata> {
        metadata_of_descriptor(&self.directory)
    }

    /// Address this retained directory from a synchronous child without
    /// inheriting a descriptor. The caller must keep this root alive until
    /// that child exits; `/proc/self` would name the child's different FD table.
    /// Darwin has no per-process descriptor namespace another process can
    /// address, so there is no equivalent there.
    #[cfg(target_os = "linux")]
    pub(crate) fn process_path(&self) -> PathBuf {
        PathBuf::from(format!(
            "/proc/{}/fd/{}",
            std::process::id(),
            self.descriptor()
        ))
    }

    pub(crate) fn descriptor(&self) -> RawFd {
        self.directory.as_raw_fd()
    }
}

/// Internal worker-side ancestor observation through a retained directory.
/// This intentional literal parent lookup is separate from confined public
/// component resolution: `open_at` MUST continue rejecting user-supplied `..`.
/// The returned descriptor alone does not grant placement/effect consent.
pub(crate) fn retained_parent_directory(root: &Root) -> io::Result<Root> {
    let flags = DIRECTORY_HANDLE | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    // SAFETY: the directory is retained, the ONLY lookup is this static NUL-
    // terminated parent literal, and openat returns a fresh owned descriptor.
    let descriptor = unsafe { libc::openat(root.descriptor(), c"..".as_ptr(), flags) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(Root {
        directory: unsafe { OwnedFd::from_raw_fd(descriptor) },
        path: root.path().join(".."),
    })
}

/// Why a path could not be resolved beneath a root.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ResolveError {
    /// A component escaped the root, or was a symlink.
    #[error("{} is not a path inside the directory it was resolved beneath", path.display())]
    Escaped { path: PathBuf },
    #[error("{}: {message}", path.display())]
    Io {
        path: PathBuf,
        message: String,
        /// Whether the failure was simply that nothing is there. A caller that
        /// treats absence as a state rather than a failure — a watcher-driven
        /// reload, a save target that was deleted — needs to tell the two
        /// apart, and the message text is not a contract.
        not_found: bool,
        /// Whether an exclusive create or rename refused an existing entry.
        already_exists: bool,
        /// Whether the kernel refused write access specifically. A caller that
        /// presents an unwritable file read-only rather than failing to open it
        /// needs to tell that apart from an unsearchable parent directory,
        /// which reports the same errno from a different object.
        write_refused: bool,
    },
}

impl ResolveError {
    pub fn is_already_exists(&self) -> bool {
        matches!(
            self,
            Self::Io {
                already_exists: true,
                ..
            }
        )
    }
    /// The path the failure is about.
    #[must_use]
    pub fn path(&self) -> &Path {
        match self {
            Self::Escaped { path } | Self::Io { path, .. } => path,
        }
    }

    /// Whether nothing exists at the path.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Io {
                not_found: true,
                ..
            }
        )
    }

    /// Whether the kernel refused write access to the object itself.
    #[must_use]
    pub fn is_write_refused(&self) -> bool {
        matches!(
            self,
            Self::Io {
                write_refused: true,
                ..
            }
        )
    }

    fn escaped(path: &Path) -> Self {
        Self::Escaped {
            path: path.to_path_buf(),
        }
    }

    fn io(path: &Path, error: &io::Error) -> Self {
        // `EACCES`, `EPERM` and `EROFS` are the three ways the kernel says "not
        // by you": the permission bits refuse it, a file attribute or a missing
        // capability refuses it, or the whole filesystem is mounted read-only.
        let write_refused = matches!(
            error.raw_os_error(),
            Some(libc::EACCES | libc::EPERM | libc::EROFS)
        );
        Self::Io {
            path: path.to_path_buf(),
            message: error.to_string(),
            already_exists: error.kind() == io::ErrorKind::AlreadyExists,
            not_found: error.kind() == io::ErrorKind::NotFound,
            write_refused,
        }
    }

    fn message(path: &Path, message: &str) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            message: message.to_owned(),
            already_exists: false,
            not_found: false,
            write_refused: false,
        }
    }
}

/// Opens `relative` beneath `root` for reading, following no symlink in any
/// component. `O_NONBLOCK` is set so a FIFO cannot block the caller.
///
/// # Errors
///
/// Returns [`ResolveError::Escaped`] when a component is a symlink or would
/// leave `root`, and [`ResolveError::Io`] for every other failure.
pub fn open_beneath(root: &Root, relative: &Path) -> Result<File, ResolveError> {
    let flags = libc::O_RDONLY | libc::O_NONBLOCK | libc::O_CLOEXEC;
    resolve(root, relative, flags, resolver()).map(File::from)
}

/// Opens `relative` beneath `root` for reading *and writing*, or reports that
/// this process is not allowed to write it.
///
/// Asking the kernel to open the file is the only way to learn writability that
/// is both exact and free of a second name lookup. `access`-style probes read
/// the mode bits and therefore answer wrongly for a POSIX ACL, a read-only
/// mount or a capability-bearing process, and the fd-relative form that would
/// avoid re-resolving the name — `faccessat2` with `AT_EMPTY_PATH` — needs a
/// kernel newer than the `openat` walk below exists to support.
///
/// A refusal to grant write access is answered as `Ok(None)` rather than as an
/// error, so a caller can go on to present the file read-only. That answer is
/// deliberately not the last word: an unsearchable parent directory refuses the
/// same errno from a different object, and only the caller's following
/// [`open_beneath`] can tell the two apart, because it fails for the parent and
/// succeeds for the file.
///
/// # Errors
///
/// Returns [`ResolveError::Escaped`] when a component is a symlink or would
/// leave `root`, and [`ResolveError::Io`] for every failure that is not a
/// refusal to grant write access.
pub fn open_beneath_writable(root: &Root, relative: &Path) -> Result<Option<File>, ResolveError> {
    let flags = libc::O_RDWR | libc::O_NONBLOCK | libc::O_CLOEXEC;
    match resolve(root, relative, flags, resolver()) {
        Ok(descriptor) => Ok(Some(File::from(descriptor))),
        Err(error) if error.is_write_refused() => Ok(None),
        Err(error) => Err(error),
    }
}

/// The metadata of `relative` beneath `root`, describing a final symlink as the
/// link itself rather than its target.
///
/// This is `symlink_metadata` without its hazard: the final component is opened
/// `O_PATH | O_NOFOLLOW`, which grants no access to the object at all, and
/// every component above it is still refused if it is a symlink.
///
/// # Errors
///
/// Returns [`ResolveError::Escaped`] when a component above the final one is a
/// symlink or would leave `root`, and [`ResolveError::Io`] for every other
/// failure.
pub fn metadata_beneath(root: &Root, relative: &Path) -> Result<Metadata, ResolveError> {
    let reported = root.path.join(relative);
    let (directory, name) = open_parent_beneath(root, relative)?;
    entry_metadata(&directory, &name, &reported)
}

#[cfg(target_os = "linux")]
fn entry_metadata(
    directory: &Root,
    name: &OsStr,
    reported: &Path,
) -> Result<Metadata, ResolveError> {
    let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let entry =
        open_at(directory.descriptor(), name, flags).map_err(|error| classify(reported, &error))?;
    metadata_of_descriptor(&entry).map_err(|error| ResolveError::io(reported, &error))
}

/// Darwin has no `O_PATH`: opening the entry would need read permission on it,
/// and opening a dataless file makes the file provider download it. `fstatat`
/// through the retained parent names the entry without either, but only `std`
/// can build a [`Metadata`], and it does so from a path.
///
/// So the entry's identity is taken through the retained parent, its metadata
/// through the parent's current path, and the answer is accepted only when both
/// describe the same inode. A rename or substitution anywhere along that path in
/// between yields a different inode — or none — and is reported rather than
/// answered with another object's metadata.
#[cfg(target_os = "macos")]
fn entry_metadata(
    directory: &Root,
    name: &OsStr,
    reported: &Path,
) -> Result<Metadata, ResolveError> {
    use std::os::unix::fs::MetadataExt;
    let raw = c_component(name, reported)?;
    // SAFETY: `stat` is plain data for which all-zero bytes are a valid value,
    // and `fstatat` overwrites it on success.
    let mut identity: libc::stat = unsafe { std::mem::zeroed() };
    // SAFETY: the retained descriptor and `raw` outlive the call, and the
    // output pointer addresses an owned `stat`.
    let status = unsafe {
        libc::fstatat(
            directory.descriptor(),
            raw.as_ptr(),
            &mut identity,
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if status < 0 {
        return Err(classify(reported, &io::Error::last_os_error()));
    }
    let parent = descriptor_path(directory.descriptor())
        .map_err(|error| ResolveError::io(reported, &error))?;
    let metadata = std::fs::symlink_metadata(parent.join(name))
        .map_err(|error| ResolveError::io(reported, &error))?;
    if metadata.dev() != identity.st_dev as u64 || metadata.ino() != identity.st_ino {
        return Err(ResolveError::message(
            reported,
            "the entry changed while its metadata was read",
        ));
    }
    Ok(metadata)
}

/// The path the kernel currently records for an open descriptor. It is a name,
/// not an identity: a caller must verify what it reaches through it.
#[cfg(target_os = "macos")]
fn descriptor_path(descriptor: RawFd) -> io::Result<PathBuf> {
    let mut buffer = vec![0_u8; libc::PATH_MAX as usize];
    // SAFETY: `F_GETPATH` writes at most `MAXPATHLEN` (`PATH_MAX`) bytes,
    // including the terminating NUL, into the buffer, which is that long.
    let status = unsafe { libc::fcntl(descriptor, libc::F_GETPATH, buffer.as_mut_ptr()) };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    let length = buffer
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(buffer.len());
    buffer.truncate(length);
    Ok(PathBuf::from(OsString::from_vec(buffer)))
}

/// Reads the target of the symbolic link `relative` beneath `root`.
///
/// # Errors
///
/// Returns [`ResolveError::Escaped`] when a component above the final one is a
/// symlink or would leave `root`, and [`ResolveError::Io`] when the final
/// component is not a symlink or cannot be read.
pub fn read_link_beneath(root: &Root, relative: &Path) -> Result<PathBuf, ResolveError> {
    let reported = root.path.join(relative);
    let (directory, name) = open_parent_beneath(root, relative)?;
    let name = c_component(&name, &reported)?;
    // A link target is bounded by PATH_MAX on every filesystem Linux exposes;
    // a truncated read is reported rather than silently shortened.
    let mut buffer = vec![0_u8; libc::PATH_MAX as usize];
    // SAFETY: `name` outlives the call, and the write is bounded by the
    // buffer's own length.
    let written = unsafe {
        libc::readlinkat(
            directory.descriptor(),
            name.as_ptr(),
            buffer.as_mut_ptr().cast::<libc::c_char>(),
            buffer.len(),
        )
    };
    if written < 0 {
        return Err(classify(&reported, &io::Error::last_os_error()));
    }
    let written = written as usize;
    if written == buffer.len() {
        return Err(ResolveError::message(
            &reported,
            "the symbolic link target is longer than PATH_MAX",
        ));
    }
    buffer.truncate(written);
    Ok(PathBuf::from(OsString::from_vec(buffer)))
}

/// Opens the parent directory of `relative` beneath `root`, returning it with
/// the final component's name, so a caller can create a temporary file and
/// rename it without ever re-resolving the path by name.
///
/// # Errors
///
/// Returns [`ResolveError::Escaped`] when a component is a symlink or would
/// leave `root`, and [`ResolveError::Io`] for every other failure.
pub fn open_parent_beneath(root: &Root, relative: &Path) -> Result<(Root, OsString), ResolveError> {
    let reported = root.path.join(relative);
    let parts = components(relative).ok_or_else(|| ResolveError::escaped(&reported))?;
    let (name, leading) = parts
        .split_last()
        .ok_or_else(|| ResolveError::escaped(&reported))?;
    let name = (*name).to_owned();
    if leading.is_empty() {
        let parent = root
            .try_clone()
            .map_err(|error| ResolveError::io(&reported, &error))?;
        return Ok((parent, name));
    }
    let parent: PathBuf = leading.iter().collect();
    let directory = resolve_directory(root, &parent)?;
    Ok((directory, name))
}

/// Opens one directory beneath the held root without following symlinks.
pub fn open_directory_beneath(root: &Root, relative: &Path) -> Result<Root, ResolveError> {
    resolve_directory(root, relative)
}

#[path = "fs_safety/directory_creation.rs"]
mod directory_creation;
#[cfg(test)]
pub(crate) use directory_creation::fixture_directory_preparation_failure;
pub use directory_creation::{
    DirectoryCreateError, DirectoryCreation, DirectoryNamespace, DirectoryPreparation,
    DirectoryPublication, DirectoryRecovery, DirectoryStage, create_directory_beneath,
    create_directory_in_namespace, stage_directory_in_namespace,
};
pub use directory_creation::{
    OwnedPrivateSibling, PrivateSiblingCreateError, SaveSiblingDurability, StickyParent,
};

/// Changes permissions through the exact retained directory descriptor.
///
/// Linux refuses `fchmod` on an `O_PATH` descriptor, so the kernel-owned
/// `/proc/self/fd` link names it instead; Darwin's `O_SEARCH` descriptor
/// accepts `fchmod` directly.
#[cfg(target_os = "linux")]
pub fn set_directory_permissions(directory: &Root, mode: u32) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(
        format!("/proc/self/fd/{}", directory.descriptor()),
        std::fs::Permissions::from_mode(mode),
    )
}

/// Permission bits in the kernel's `mode_t`, which is narrower than `u32` on
/// Darwin. A value that does not fit is refused rather than truncated.
pub(crate) fn kernel_mode(mode: u32) -> io::Result<libc::mode_t> {
    libc::mode_t::try_from(mode)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "mode out of range"))
}

/// Changes permissions through the exact retained directory descriptor.
#[cfg(target_os = "macos")]
pub fn set_directory_permissions(directory: &Root, mode: u32) -> io::Result<()> {
    let mode = kernel_mode(mode)?;
    // SAFETY: the Root owns a live descriptor for the duration of the call.
    if unsafe { libc::fchmod(directory.descriptor(), mode) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Removes only an empty direct child through its retained parent.
pub fn remove_directory_within(directory: &Root, name: &OsStr) -> Result<(), ResolveError> {
    let raw = c_component(name, &directory.path)?;
    // SAFETY: the held directory and C string outlive unlinkat; AT_REMOVEDIR
    // removes only an empty directory entry and never follows a symbolic link.
    let status =
        unsafe { libc::unlinkat(directory.descriptor(), raw.as_ptr(), libc::AT_REMOVEDIR) };
    if status < 0 {
        return Err(ResolveError::io(
            &directory.path.join(name),
            &io::Error::last_os_error(),
        ));
    }
    Ok(())
}

#[path = "fs_safety/recovery_placement.rs"]
mod recovery_placement;
pub use recovery_placement::{
    RecoveryDirectoryStamp, RecoveryPlacement, recovery_directory_stamp, validate_recovery_parent,
};

#[path = "fs_safety/recovery_cache.rs"]
mod recovery_cache;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub use recovery_cache::fixture_fail_next_save_preparation_sync;
#[cfg(all(test, target_os = "linux"))]
pub(crate) use recovery_cache::fixture_recovery_directory_preparation_failure;
pub use recovery_cache::{
    RecoveryDirectoryError, RecoveryParentCandidates, SavePreparationDurability,
    prepare_recovery_directory_with_owner, prepare_save_recovery_directory_with_owner,
    verify_recovery_directory_private,
};

/// Creates missing directory components without following symbolic links.
/// Existing components must be real directories beneath the retained root.
pub fn create_directories_beneath(
    root: &Root,
    relative: &Path,
    mode: u32,
) -> Result<Root, ResolveError> {
    let mut directory = root
        .try_clone()
        .map_err(|error| ResolveError::io(root.path(), &error))?;
    if relative.as_os_str().is_empty() {
        return Ok(directory);
    }
    let mode = kernel_mode(mode).map_err(|error| ResolveError::io(relative, &error))?;
    let parts = components(relative).ok_or_else(|| ResolveError::escaped(relative))?;
    for component in parts {
        let name = c_component(component, directory.path())?;
        // SAFETY: the component and retained parent descriptor outlive mkdirat.
        let result = unsafe { libc::mkdirat(directory.descriptor(), name.as_ptr(), mode) };
        if result < 0 {
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::AlreadyExists {
                return Err(ResolveError::io(&directory.path.join(component), &error));
            }
        }
        directory = resolve_directory(&directory, Path::new(component))?;
    }
    Ok(directory)
}

/// Lists direct child names through a retained directory descriptor.
/// The limit bounds memory even when an untrusted repository has huge directories.
pub fn directory_entries_beneath(
    root: &Root,
    relative: &Path,
    limit: usize,
) -> Result<Vec<OsString>, ResolveError> {
    let directory = if relative.as_os_str().is_empty() {
        root.try_clone()
            .map_err(|error| ResolveError::io(root.path(), &error))?
    } else {
        resolve_directory(root, relative)?
    };
    entry_names(&directory, limit)
}

/// Lists the direct children of a retained directory through that descriptor,
/// never through a pathname, refusing to hold more than `limit` names.
///
/// The directory is reopened for reading through its descriptor (see
/// [`reopen_directory_for_reading`]), and `fdopendir` then owns that new
/// descriptor for the whole enumeration.
pub(crate) fn entry_names(directory: &Root, limit: usize) -> Result<Vec<OsString>, ResolveError> {
    let reported = directory.path();
    let descriptor = reopen_directory_for_reading(directory)
        .map_err(|error| ResolveError::io(reported, &error))?;
    let descriptor = std::os::fd::IntoRawFd::into_raw_fd(descriptor);
    // SAFETY: `descriptor` is fresh and owned by nothing else; on success the
    // stream takes ownership of it, and on failure it is still ours to close.
    let stream = unsafe { libc::fdopendir(descriptor) };
    if stream.is_null() {
        let error = io::Error::last_os_error();
        // SAFETY: `fdopendir` failed, so `descriptor` is still owned here.
        drop(unsafe { OwnedFd::from_raw_fd(descriptor) });
        return Err(ResolveError::io(reported, &error));
    }
    let stream = DirectoryStream(stream);
    let mut names = Vec::new();
    loop {
        clear_errno();
        // SAFETY: the stream is open and owned by `stream` until it drops.
        let entry = unsafe { libc::readdir(stream.0) };
        if entry.is_null() {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(0) {
                return Ok(names);
            }
            return Err(ResolveError::io(reported, &error));
        }
        // SAFETY: a non-null `readdir` result points at an entry whose name is
        // NUL-terminated and valid until the next call on this stream.
        let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
        if name == b"." || name == b".." {
            continue;
        }
        if names.len() == limit {
            return Err(ResolveError::message(
                reported,
                "directory entry limit exceeded",
            ));
        }
        names.push(OsString::from_vec(name.to_vec()));
    }
}

/// Opens the retained directory itself for reading, without a pathname.
///
/// Linux reopens the kernel-owned `/proc/self/fd` link, which needs only read
/// permission on the directory, as the `O_PATH` handle needed none. Darwin
/// reopens `.` relative to the `O_SEARCH` handle, the one name that can never
/// be substituted; the handle already required search permission.
fn reopen_directory_for_reading(directory: &Root) -> io::Result<OwnedFd> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    #[cfg(target_os = "linux")]
    let descriptor = {
        let link = CString::new(format!("/proc/self/fd/{}", directory.descriptor()))
            .expect("a decimal descriptor contains no NUL");
        // SAFETY: the link string outlives the call, and `open` returns a
        // fresh owned descriptor or -1.
        unsafe { libc::open(link.as_ptr(), flags) }
    };
    #[cfg(target_os = "macos")]
    // SAFETY: the literal is NUL-terminated and static, and `openat` returns a
    // fresh owned descriptor or -1.
    let descriptor = unsafe { libc::openat(directory.descriptor(), c".".as_ptr(), flags) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `descriptor` was just returned by a successful open and is not
    // owned anywhere else.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

/// An open directory stream, closed when dropped.
struct DirectoryStream(*mut libc::DIR);

impl Drop for DirectoryStream {
    fn drop(&mut self) {
        // SAFETY: the stream came from a successful `fdopendir` and is closed
        // exactly once, here; `closedir` also closes its descriptor.
        unsafe { libc::closedir(self.0) };
    }
}

/// Resets `errno`, so a null `readdir` can tell the end of the stream (errno
/// still zero) from a failure.
fn clear_errno() {
    // SAFETY: the errno location is this thread's own, valid for its lifetime.
    unsafe {
        #[cfg(target_os = "linux")]
        {
            *libc::__errno_location() = 0;
        }
        #[cfg(target_os = "macos")]
        {
            *libc::__error() = 0;
        }
    }
}

/// Creates an unnamed regular file on this retained directory's filesystem.
///
/// No directory entry can be substituted or require cleanup. Unsupported
/// filesystems are an explicit refusal; there is no named temporary fallback.
#[cfg(target_os = "linux")]
pub fn create_anonymous_in(directory: &Root, mode: u32) -> Result<File, ResolveError> {
    use std::os::unix::fs::MetadataExt;
    // SAFETY: the Root owns its descriptor, the literal is NUL-terminated,
    // and a successful openat returns a fresh owned descriptor.
    let descriptor = unsafe {
        libc::openat(
            directory.descriptor(),
            c".".as_ptr(),
            libc::O_RDWR | libc::O_TMPFILE | libc::O_CLOEXEC,
            libc::mode_t::from(mode),
        )
    };
    if descriptor < 0 {
        let error = io::Error::last_os_error();
        return Err(ResolveError::message(
            directory.path(),
            &format!(
                "Cannot create an anonymous copy file; this filesystem must support O_TMPFILE: {error}"
            ),
        ));
    }
    // SAFETY: this is the fresh descriptor returned by openat above.
    let file = unsafe { File::from_raw_fd(descriptor) };
    let metadata = file
        .metadata()
        .map_err(|error| ResolveError::io(directory.path(), &error))?;
    if !metadata.is_file() || metadata.nlink() != 0 {
        return Err(ResolveError::message(
            directory.path(),
            "The copy file is not an unnamed regular inode",
        ));
    }
    Ok(file)
}

/// Publishes the exact unnamed file without replacing a destination entry.
///
/// `/proc/self/fd` denotes the retained descriptor, never a mutable worktree
/// name. AT_SYMLINK_FOLLOW follows only that kernel-owned descriptor link.
/// The caller keeps the File alive across linkat. Failure leaves the output
/// unnamed and never unlinks or overwrites anything in the destination.
#[cfg(target_os = "linux")]
pub fn link_anonymous_new(directory: &Root, file: &File, to: &OsStr) -> Result<(), ResolveError> {
    use std::os::unix::fs::MetadataExt;
    let destination = c_component(to, directory.path())?;
    let metadata = file
        .metadata()
        .map_err(|error| ResolveError::io(directory.path(), &error))?;
    if !metadata.is_file() || metadata.nlink() != 0 {
        return Err(ResolveError::message(
            directory.path(),
            "Only an unnamed regular copy file may be published",
        ));
    }
    let source = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd()))
        .expect("a decimal descriptor contains no NUL");
    // SAFETY: both strings and the owning File/Root remain alive across
    // linkat. linkat does not replace an existing destination, including links.
    let status = unsafe {
        libc::linkat(
            libc::AT_FDCWD,
            source.as_ptr(),
            directory.descriptor(),
            destination.as_ptr(),
            libc::AT_SYMLINK_FOLLOW,
        )
    };
    if status < 0 {
        return Err(ResolveError::io(
            &directory.path.join(to),
            &io::Error::last_os_error(),
        ));
    }
    Ok(())
}

/// Darwin has no unnamed file (`O_TMPFILE`), and this contract allows no named
/// temporary in its place, so the refusal is the one unsupported filesystems
/// give on Linux.
#[cfg(target_os = "macos")]
pub fn create_anonymous_in(directory: &Root, _mode: u32) -> Result<File, ResolveError> {
    Err(ResolveError::message(
        directory.path(),
        "Cannot create an anonymous copy file; this platform has no O_TMPFILE",
    ))
}

/// See the Darwin [`create_anonymous_in`]: no unnamed file can exist to publish.
#[cfg(target_os = "macos")]
pub fn link_anonymous_new(directory: &Root, _file: &File, to: &OsStr) -> Result<(), ResolveError> {
    Err(ResolveError::message(
        &directory.path.join(to),
        "Only an unnamed regular copy file may be published",
    ))
}

/// Creates a file inside `directory` with `mode`, for the atomic
/// write-then-rename sequence. The name is generated and unpredictable.
///
/// # Errors
///
/// Returns [`ResolveError::Io`] when no name could be created, which includes
/// the case where randomness is unavailable.
pub fn create_temporary_in(directory: &Root, mode: u32) -> Result<(File, OsString), ResolveError> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let mode = kernel_mode(mode).map_err(|error| ResolveError::io(&directory.path, &error))?;
    let mut last = None;
    for _ in 0..TEMPORARY_NAME_ATTEMPTS {
        let name = temporary_name().map_err(|error| ResolveError::io(&directory.path, &error))?;
        let raw = c_component(&name, &directory.path)?;
        // SAFETY: `raw` outlives the call; `openat` returns a fresh owned
        // descriptor or -1, and the variadic mode argument is required by
        // `O_CREAT`.
        let descriptor = unsafe {
            libc::openat(
                directory.descriptor(),
                raw.as_ptr(),
                flags,
                libc::c_uint::from(mode),
            )
        };
        if descriptor >= 0 {
            // SAFETY: `descriptor` was just returned by a successful `openat`
            // and is not owned anywhere else.
            let file = unsafe { File::from_raw_fd(descriptor) };
            return Ok((file, name));
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::AlreadyExists {
            return Err(ResolveError::io(&directory.path, &error));
        }
        last = Some(error);
    }
    Err(match last {
        Some(error) => ResolveError::io(&directory.path, &error),
        None => ResolveError::message(&directory.path, "no temporary name was attempted"),
    })
}

/// Opens `name` directly inside `directory` for reading and writing, creating
/// it with `mode` if it is absent, without following a symbolic link there and
/// without blocking on a FIFO. The caller validates what it opened.
///
/// # Errors
///
/// Returns [`ResolveError::Escaped`] when `name` is not a single component or
/// is a symbolic link, and [`ResolveError::Io`] for every other failure.
pub fn open_or_create_within(
    directory: &Root,
    name: &OsStr,
    mode: u32,
) -> Result<File, ResolveError> {
    let raw = c_component(name, &directory.path)?;
    let reported = directory.path.join(name);
    let flags =
        libc::O_RDWR | libc::O_CREAT | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC;
    let mode = kernel_mode(mode).map_err(|error| ResolveError::io(&reported, &error))?;
    // SAFETY: `raw` outlives the call; `openat` returns a fresh owned
    // descriptor or -1, and `O_CREAT` requires the variadic mode argument.
    let descriptor = unsafe {
        libc::openat(
            directory.descriptor(),
            raw.as_ptr(),
            flags,
            libc::c_uint::from(mode),
        )
    };
    if descriptor < 0 {
        return Err(classify(&reported, &io::Error::last_os_error()));
    }
    // SAFETY: `descriptor` was just returned by a successful `openat` and is
    // not owned anywhere else.
    Ok(unsafe { File::from_raw_fd(descriptor) })
}

/// Renames `from` to `to`, both relative to `directory`, with no path
/// re-resolution.
///
/// # Errors
///
/// Returns [`ResolveError::Io`] when the rename fails, and
/// [`ResolveError::Escaped`] when either name is not a single component.
pub fn rename_within(directory: &Root, from: &OsStr, to: &OsStr) -> Result<(), ResolveError> {
    let source = c_component(from, &directory.path)?;
    let destination = c_component(to, &directory.path)?;
    // SAFETY: both names outlive the call and both descriptors are owned by
    // `directory`.
    let status = unsafe {
        libc::renameat(
            directory.descriptor(),
            source.as_ptr(),
            directory.descriptor(),
            destination.as_ptr(),
        )
    };
    if status < 0 {
        return Err(ResolveError::io(
            &directory.path.join(to),
            &io::Error::last_os_error(),
        ));
    }
    Ok(())
}

/// Atomically moves a durable recovery record without ever replacing evidence
/// already stored at the destination. Both names stay relative to the open root.
///
/// # Errors
///
/// Returns an error for invalid names, an existing destination, or a failed
/// filesystem operation. Existing recovery evidence is never replaced.
pub fn rename_new_within(directory: &Root, from: &OsStr, to: &OsStr) -> Result<(), ResolveError> {
    rename_new_between(directory, from, directory, to)
}

/// Atomically moves an entry between already-open directories without
/// replacing any destination entry, including a dangling symbolic link.
///
/// # Errors
///
/// Returns an error for invalid leaf names, an existing destination, a
/// cross-filesystem move, or another failed filesystem operation.
pub fn rename_new_between(
    source_directory: &Root,
    from: &OsStr,
    destination_directory: &Root,
    to: &OsStr,
) -> Result<(), ResolveError> {
    let source = c_component(from, &source_directory.path)?;
    let destination = c_component(to, &destination_directory.path)?;
    // SAFETY: both names and the owned directory descriptor outlive this call.
    // `RENAME_NOREPLACE` and Darwin's `RENAME_EXCL` both refuse, atomically
    // and with `EEXIST`, any existing destination entry, including a link.
    #[cfg(target_os = "linux")]
    let status = unsafe {
        libc::renameat2(
            source_directory.descriptor(),
            source.as_ptr(),
            destination_directory.descriptor(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
        )
    };
    #[cfg(target_os = "macos")]
    let status = unsafe {
        libc::renameatx_np(
            source_directory.descriptor(),
            source.as_ptr(),
            destination_directory.descriptor(),
            destination.as_ptr(),
            libc::RENAME_EXCL,
        )
    };
    if status < 0 {
        return Err(ResolveError::io(
            &destination_directory.path.join(to),
            &io::Error::last_os_error(),
        ));
    }
    Ok(())
}

/// Unlinks `name` directly inside `directory`.
///
/// A symbolic link is removed as itself: `unlinkat` with no flags never
/// resolves the final component.
///
/// # Errors
///
/// Returns [`ResolveError::Io`] when the unlink fails, and
/// [`ResolveError::Escaped`] when `name` is not a single component.
pub fn unlink_within(directory: &Root, name: &OsStr) -> Result<(), ResolveError> {
    let raw = c_component(name, &directory.path)?;
    // SAFETY: `raw` outlives the call and the descriptor is owned by
    // `directory`.
    let status = unsafe { libc::unlinkat(directory.descriptor(), raw.as_ptr(), 0) };
    if status < 0 {
        return Err(ResolveError::io(
            &directory.path.join(name),
            &io::Error::last_os_error(),
        ));
    }
    Ok(())
}

/// `fsync`s a directory so a rename inside it is durable.
///
/// The root's own descriptor is `O_PATH` and cannot be synced, so the directory
/// is reopened through itself: `.` relative to a directory descriptor is the
/// one name that can never be a symlink.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] when the directory cannot be reopened
/// or synced.
pub fn sync_directory(directory: &Root) -> io::Result<()> {
    let flags = libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC;
    // SAFETY: the literal is NUL-terminated and static, and `openat` returns a
    // fresh owned descriptor or -1.
    let descriptor = unsafe { libc::openat(directory.descriptor(), c".".as_ptr(), flags) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `descriptor` was just returned by a successful `openat` and is
    // not owned anywhere else.
    let handle = unsafe { File::from_raw_fd(descriptor) };
    handle.sync_all()
}

/// Metadata of an already-open descriptor, so a check and the read that
/// follows it describe the same object.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] when the descriptor cannot be stat'ed.
pub fn metadata_of(file: &File) -> io::Result<Metadata> {
    file.metadata()
}

/// An extended attribute a replacement file did not receive.
///
/// A save is never failed for one of these. The bytes are the user's work and
/// the attribute is not, so losing the save to preserve a piece of metadata
/// would trade the thing that matters for the thing that does not. Reporting
/// it is the other half of that bargain: the caller must not claim a fidelity
/// it did not deliver either.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttributeNotCopied {
    /// The attribute list itself could not be read, so nothing was attempted
    /// and it is not even known what there was to lose.
    Unlistable { message: String },
    /// One named attribute could not be read from the original or written to
    /// the replacement.
    Attribute { name: OsString, message: String },
}

impl fmt::Display for AttributeNotCopied {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unlistable { message } => {
                write!(formatter, "the attribute list could not be read: {message}")
            }
            Self::Attribute { name, message } => {
                write!(formatter, "{}: {message}", Path::new(name).display())
            }
        }
    }
}

/// Copies the extended attributes that belong to a file's *content* from
/// `source` onto the `destination` that is about to take its name, so an
/// atomic replacement does not have to be abandoned to keep them.
///
/// Two of Linux's four namespaces are reproduced and two are deliberately not,
/// and the two that are skipped are skipped because the replacement gets a
/// better version of them from someone else than this function could copy:
///
/// * `user.*` is the user's own metadata and nothing in the system will put it
///   back, so it is copied and a failure to copy it is reported.
/// * `system.*` is where a POSIX ACL lives — `system.posix_acl_access` *is*
///   the ACL, its named entries and its mask — so copying the namespace is
///   what carries the ACL across, with no second ACL-specific mechanism. The
///   caller must set the replacement's mode *before* calling this, because the
///   kernel derives the mode from the ACL and a later `fchmod` would rewrite
///   the ACL's mask from the mode.
/// * `security.*` belongs to the kernel and the LSM, and reproducing it would
///   be wrong even where it is permitted. `security.selinux` is synthesised
///   for every inode from policy, so the replacement already carries the label
///   the original would have been given — which is also why the presence of
///   this namespace must never be read as "a rename would lose something", as
///   it is present on every file on an SELinux-enforcing kernel.
///   `security.capability` is a privilege grant that must not follow content
///   this editor just wrote, for the same reason setuid is not reproduced.
///   `security.ima` and `security.evm` are signatures over bytes that no
///   longer exist.
/// * `trusted.*` needs `CAP_SYS_ADMIN` and is the filesystem's own bookkeeping:
///   `trusted.overlay.*` describes one inode's role in an overlay, which a
///   different inode does not have.
///
/// Both skipped namespaces would answer `EPERM` for an unprivileged process
/// anyway, so not attempting them is also what keeps the report free of noise
/// that every ordinary save would produce.
///
/// This never returns an error, by design: `flistxattr` being unanswerable is
/// reported as a lost list rather than propagated, because refusing to write a
/// user's bytes over an unanswerable metadata probe is strictly worse than
/// either answer the probe might have given.
pub fn copy_extended_attributes(source: &File, destination: &File) -> Vec<AttributeNotCopied> {
    let names = match attribute_names(source) {
        Ok(names) => names,
        Err(error) => {
            return vec![AttributeNotCopied::Unlistable {
                message: error.to_string(),
            }];
        }
    };
    let mut lost = Vec::new();
    let mut record = |name: &CStr, error: &io::Error| {
        lost.push(AttributeNotCopied::Attribute {
            name: OsString::from_vec(name.to_bytes().to_vec()),
            message: error.to_string(),
        });
    };
    for name in names {
        if !is_content_attribute(&name) {
            continue;
        }
        let value = match read_attribute(source, &name) {
            Ok(Some(value)) => value,
            // Removed between the list and the read, so there is nothing to
            // copy and nothing was lost.
            Ok(None) => continue,
            Err(error) => {
                record(&name, &error);
                continue;
            }
        };
        if let Err(error) = write_attribute(destination, &name, &value) {
            record(&name, &error);
        }
    }
    lost
}

/// Captures the content metadata used to fence a retained editor Save.
/// Unlike best-effort copying, an incomplete snapshot refuses publication.
/// Bound both allocation and syscall work even when a file carries many
/// attributes. Other filesystem callers retain their existing copy policy.
pub fn content_attributes(
    file: &File,
) -> io::Result<std::collections::BTreeMap<OsString, Vec<u8>>> {
    const LIST_BYTES: usize = 64 * 1024;
    const MAX_ATTRIBUTES: usize = 1024;
    const SNAPSHOT_BYTES: usize = 1024 * 1024;
    let names = attribute_names_with_limit(file, LIST_BYTES)?;
    if names.len() > MAX_ATTRIBUTES {
        return Err(io::Error::other(
            "file attribute count exceeds the Save metadata budget",
        ));
    }
    let mut remaining = SNAPSHOT_BYTES;
    let mut attributes = std::collections::BTreeMap::new();
    for name in names {
        if !is_content_attribute(&name) {
            continue;
        }
        let value = read_attribute_with_limit(file, &name, remaining.min(LIST_BYTES))?
            .ok_or_else(|| io::Error::other("file attributes changed while they were read"))?;
        // Discard spare capacity from a concurrent shrink, so the retained
        // snapshot obeys the aggregate allocation budget as well as its
        // logical byte budget. At most one bounded value is transient.
        let value = value.into_boxed_slice().into_vec();
        remaining -= value.len();
        attributes.insert(OsString::from_vec(name.to_bytes().to_vec()), value);
    }
    Ok(attributes)
}

/// Preserves an existing file's owner and group on its temporary replacement.
/// Apply this before access ACLs or content: the owner/group entries in those
/// permissions must retain their original meaning. Every failure prevents
/// publication. A later chmod must restore any set-ID bits cleared by chown.
pub fn copy_ownership(source: &File, destination: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let source = source.metadata()?;
    let target = destination.metadata()?;
    if source.uid() == target.uid() && source.gid() == target.gid() {
        return Ok(());
    }
    // SAFETY: destination is a live descriptor; uid and gid come from the
    // source descriptor. No pathname is resolved again by this operation.
    let status = unsafe { libc::fchown(destination.as_raw_fd(), source.uid(), source.gid()) };
    if status < 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("cannot preserve file ownership: {error}"),
        ));
    }
    let installed = destination.metadata()?;
    if installed.uid() != source.uid() || installed.gid() != source.gid() {
        return Err(io::Error::other(
            "the filesystem did not preserve file ownership",
        ));
    }
    Ok(())
}

/// Preserve the source group on a new copy without changing its owning user.
/// A setgid destination can otherwise assign existing group permissions to a
/// different group. Failure prevents publication rather than broadening access.
pub fn copy_group(source: &File, destination: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    preserve_group(source.metadata()?.gid(), destination)
}

/// Apply a group captured with the source's other permission metadata.
pub fn preserve_group(group: libc::gid_t, destination: &File) -> io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    if destination.metadata()?.gid() == group {
        return Ok(());
    }
    // SAFETY: destination remains owned for this call; the all-ones uid keeps
    // its current owner and the group comes from the retained source descriptor.
    if unsafe { libc::fchown(destination.as_raw_fd(), libc::uid_t::MAX, group) } < 0 {
        let error = io::Error::last_os_error();
        return Err(io::Error::new(
            error.kind(),
            format!("cannot preserve source file group: {error}"),
        ));
    }
    if destination.metadata()?.gid() != group {
        return Err(io::Error::other(
            "the filesystem did not preserve the source file group",
        ));
    }
    Ok(())
}

/// Preserves the source's access ACL on a new file: the POSIX access ACL on
/// Linux, the extended (NFSv4-style) ACL on macOS.
/// Unlike optional content attributes, losing an ACL can grant unintended
/// access, so every failure prevents publication. An absent source ACL removes
/// any access ACL inherited by the destination from its parent.
pub fn copy_access_acl(source: &File, destination: &File) -> io::Result<()> {
    AccessAcl::capture(source)?.apply(destination)
}

/// A captured access ACL, including the absence of one. Copy callers can keep
/// mode, group and ACL from one verified source snapshot instead of mixing
/// metadata observed before and after a concurrent permission change.
pub struct AccessAcl(Option<Vec<u8>>);

#[cfg(target_os = "macos")]
#[path = "fs_safety/darwin_acl.rs"]
mod darwin_acl;

#[cfg(target_os = "linux")]
impl AccessAcl {
    pub fn capture(source: &File) -> io::Result<Self> {
        let name = c"system.posix_acl_access";
        let value = match read_attribute(source, name) {
            Ok(value) => value,
            Err(error) if error.raw_os_error() == Some(libc::EOPNOTSUPP) => None,
            Err(error) => return Err(error),
        };
        Ok(Self(value))
    }

    pub fn apply(&self, destination: &File) -> io::Result<()> {
        let name = c"system.posix_acl_access";
        if let Some(value) = &self.0 {
            return write_attribute(destination, name, value);
        }
        // SAFETY: the descriptor is live and the attribute name is static.
        let status = unsafe { xattr::remove(destination.as_raw_fd(), name.as_ptr()) };
        if status < 0 {
            let error = io::Error::last_os_error();
            if !matches!(error.raw_os_error(), Some(NO_ATTRIBUTE | libc::EOPNOTSUPP)) {
                return Err(error);
            }
        }
        Ok(())
    }
}

/// Whether an attribute describes the file's content rather than the kernel's
/// or the filesystem's relationship with the inode. See
/// [`copy_extended_attributes`] for why the line falls here.
#[cfg(target_os = "linux")]
fn is_content_attribute(name: &CStr) -> bool {
    let bytes = name.to_bytes();
    bytes.starts_with(b"user.") || bytes.starts_with(b"system.")
}

/// Darwin has no namespaces: every attribute is the file's own (Finder info,
/// resource forks, `com.apple.quarantine`, a user's tags) except those the
/// kernel keeps for itself. `com.apple.system.*` holds protected kernel state,
/// `com.apple.provenance` is stamped by the kernel and refused to anyone else,
/// `com.apple.macl` records sandbox grants that only the system may write, and
/// `com.apple.rootless` marks System Integrity Protection. An access ACL is not
/// an attribute there; [`AccessAcl`] carries it.
#[cfg(target_os = "macos")]
fn is_content_attribute(name: &CStr) -> bool {
    let bytes = name.to_bytes();
    !(bytes.starts_with(b"com.apple.system.")
        || bytes == b"com.apple.provenance"
        || bytes == b"com.apple.macl"
        || bytes == b"com.apple.rootless")
}

/// The descriptor forms of the extended-attribute calls, with Linux's
/// signatures. Darwin's take a resource-fork `position` and an `options` word
/// as well; zero for both is the plain behavior, and a descriptor never
/// follows a link either way.
#[cfg(target_os = "linux")]
mod xattr {
    pub(super) use libc::{
        fgetxattr as get, flistxattr as list, fremovexattr as remove, fsetxattr as set_with_flags,
    };

    /// Creates or replaces one attribute.
    pub(super) unsafe fn set(
        fd: libc::c_int,
        name: *const libc::c_char,
        value: *const libc::c_void,
        size: libc::size_t,
    ) -> libc::c_int {
        // SAFETY: forwarded under the caller's obligations.
        unsafe { set_with_flags(fd, name, value, size, 0) }
    }
}

#[cfg(target_os = "macos")]
mod xattr {
    pub(super) unsafe fn list(
        fd: libc::c_int,
        names: *mut libc::c_char,
        size: libc::size_t,
    ) -> libc::ssize_t {
        // SAFETY: forwarded under the caller's obligations.
        unsafe { libc::flistxattr(fd, names, size, 0) }
    }

    pub(super) unsafe fn get(
        fd: libc::c_int,
        name: *const libc::c_char,
        value: *mut libc::c_void,
        size: libc::size_t,
    ) -> libc::ssize_t {
        // SAFETY: forwarded under the caller's obligations.
        unsafe { libc::fgetxattr(fd, name, value, size, 0, 0) }
    }

    /// Creates or replaces one attribute.
    pub(super) unsafe fn set(
        fd: libc::c_int,
        name: *const libc::c_char,
        value: *const libc::c_void,
        size: libc::size_t,
    ) -> libc::c_int {
        // SAFETY: forwarded under the caller's obligations.
        unsafe { libc::fsetxattr(fd, name, value, size, 0, 0) }
    }

    pub(super) unsafe fn remove(fd: libc::c_int, name: *const libc::c_char) -> libc::c_int {
        // SAFETY: forwarded under the caller's obligations.
        unsafe { libc::fremovexattr(fd, name, 0) }
    }
}

/// Every extended-attribute name an open file carries.
fn attribute_names(file: &File) -> io::Result<Vec<CString>> {
    attribute_names_with_limit(file, usize::MAX)
}

fn attribute_names_with_limit(file: &File, limit: usize) -> io::Result<Vec<CString>> {
    let mut buffer = Vec::new();
    for _ in 0..ATTRIBUTE_SIZE_ATTEMPTS {
        // SAFETY: a null buffer with a zero length asks `flistxattr` only for
        // the size the list would need, which is the documented way to query
        // it.
        let size = unsafe { xattr::list(file.as_raw_fd(), ptr::null_mut(), 0) };
        if size < 0 {
            let error = io::Error::last_os_error();
            // A filesystem that has no such thing as an extended attribute
            // carries none, which is an answer and not a failure.
            if error.raw_os_error() == Some(libc::ENOSYS) || attributes_unsupported(&error) {
                return Ok(Vec::new());
            }
            return Err(error);
        }
        if size == 0 {
            return Ok(Vec::new());
        }
        if size as usize > limit {
            return Err(io::Error::other(
                "file attribute list exceeds the Save metadata budget",
            ));
        }
        buffer.resize(size as usize, 0);
        // SAFETY: the write is bounded by the buffer's own length.
        let read = unsafe {
            xattr::list(
                file.as_raw_fd(),
                buffer.as_mut_ptr().cast::<libc::c_char>(),
                buffer.len(),
            )
        };
        if read >= 0 {
            buffer.truncate(read as usize);
            return Ok(attribute_name_list(&buffer));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ERANGE) {
            return Err(error);
        }
        // The list grew between being sized and being read, so ask again.
    }
    Err(io::Error::from_raw_os_error(libc::ERANGE))
}

/// Whether a filesystem has no extended attributes at all. Linux gives
/// `ENOTSUP` and `EOPNOTSUPP` one number; Darwin gives them two.
fn attributes_unsupported(error: &io::Error) -> bool {
    let errno = error.raw_os_error();
    errno == Some(libc::ENOTSUP) || errno == Some(libc::EOPNOTSUPP)
}

/// Splits `flistxattr`'s NUL-terminated, concatenated names.
fn attribute_name_list(buffer: &[u8]) -> Vec<CString> {
    buffer
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
        .filter_map(|name| CString::new(name).ok())
        .collect()
}

/// One attribute's value, or `None` when it was removed after being listed.
fn read_attribute(file: &File, name: &CStr) -> io::Result<Option<Vec<u8>>> {
    read_attribute_with_limit(file, name, usize::MAX)
}

fn read_attribute_with_limit(
    file: &File,
    name: &CStr,
    limit: usize,
) -> io::Result<Option<Vec<u8>>> {
    let mut value = Vec::new();
    for _ in 0..ATTRIBUTE_SIZE_ATTEMPTS {
        // SAFETY: `name` outlives the call, and a null buffer with a zero
        // length asks only for the size the value would need.
        let size = unsafe { xattr::get(file.as_raw_fd(), name.as_ptr(), ptr::null_mut(), 0) };
        if size < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(NO_ATTRIBUTE) {
                return Ok(None);
            }
            return Err(error);
        }
        if size as usize > limit {
            return Err(io::Error::other(
                "file attributes exceed the Save metadata budget",
            ));
        }
        value.resize(size as usize, 0);
        // SAFETY: `name` outlives the call and the write is bounded by the
        // buffer's own length. An empty value passes a dangling but aligned
        // pointer with a zero length, which `fgetxattr` never dereferences.
        let read = unsafe {
            xattr::get(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr().cast::<libc::c_void>(),
                value.len(),
            )
        };
        if read >= 0 {
            if read as usize > value.len() {
                continue;
            }
            value.truncate(read as usize);
            return Ok(Some(value));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(NO_ATTRIBUTE) {
            return Ok(None);
        }
        if error.raw_os_error() != Some(libc::ERANGE) {
            return Err(error);
        }
        // The value grew between being sized and being read, so ask again.
    }
    Err(io::Error::from_raw_os_error(libc::ERANGE))
}

/// Sets one attribute on the replacement.
fn write_attribute(file: &File, name: &CStr, value: &[u8]) -> io::Result<()> {
    // SAFETY: `name` and `value` outlive the call and the length passed is the
    // value's own. A zero flag means "create or replace", which is what a
    // freshly created replacement needs.
    let status = unsafe {
        xattr::set(
            file.as_raw_fd(),
            name.as_ptr(),
            value.as_ptr().cast::<libc::c_void>(),
            value.len(),
        )
    };
    if status < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Reads at most `limit + 1` bytes from an open file, so the caller can tell
/// "at the limit" from "over it" without reading an unbounded amount.
///
/// # Errors
///
/// Returns the underlying [`io::Error`] when the read fails.
pub fn read_bounded(file: &mut File, limit: u64) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    file.take(limit.saturating_add(1)).read_to_end(&mut bytes)?;
    Ok(bytes)
}

// ---------------------------------------------------------------------------
// Resolution
// ---------------------------------------------------------------------------

/// Which mechanism resolves a path beneath a root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Resolver {
    /// One kernel call that refuses both escapes: `openat2` with
    /// `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS` on Linux, `openat` with
    /// `O_NOFOLLOW_ANY` on macOS.
    Kernel,
    /// A component-at-a-time `openat` walk with `O_NOFOLLOW`.
    #[cfg_attr(not(any(target_os = "linux", test)), allow(dead_code))]
    Walk,
}

/// The resolver this kernel supports, decided once.
#[cfg(target_os = "linux")]
fn resolver() -> Resolver {
    static SUPPORTED: std::sync::LazyLock<Resolver> = std::sync::LazyLock::new(probe_resolver);
    *SUPPORTED
}

/// Every macOS this builds for (11 and later) has `O_NOFOLLOW_ANY`.
#[cfg(target_os = "macos")]
fn resolver() -> Resolver {
    Resolver::Kernel
}

/// Probes `openat2` by resolving `.` beneath the current directory.
///
/// Only a kernel or sandbox that does not know the syscall selects the walk;
/// an ordinary failure such as an unreadable working directory says nothing
/// about the syscall's availability and must not disable it forever.
#[cfg(target_os = "linux")]
fn probe_resolver() -> Resolver {
    let flags = libc::O_PATH | libc::O_CLOEXEC;
    match openat2(libc::AT_FDCWD, Path::new("."), flags) {
        Ok(_) => Resolver::Kernel,
        Err(error) => match error.raw_os_error() {
            Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP | libc::EPERM) => Resolver::Walk,
            _ => Resolver::Kernel,
        },
    }
}

/// Resolves `relative` beneath `root` and opens it with `flags`.
fn resolve(
    root: &Root,
    relative: &Path,
    flags: i32,
    resolver: Resolver,
) -> Result<OwnedFd, ResolveError> {
    let reported = root.path.join(relative);
    let parts = components(relative).ok_or_else(|| ResolveError::escaped(&reported))?;
    match resolver {
        // `RESOLVE_NO_SYMLINKS` already rejects a trailing symlink, and adding
        // `O_NOFOLLOW` would re-enable the `O_PATH | O_NOFOLLOW` exception that
        // hands back the link itself.
        Resolver::Kernel => kernel_resolve(root.descriptor(), relative, flags)
            .map_err(|error| classify(&reported, &error)),
        Resolver::Walk => walk(root, &parts, flags, &reported),
    }
}

/// Resolves `relative` beneath `root` as a directory.
fn resolve_directory(root: &Root, relative: &Path) -> Result<Root, ResolveError> {
    let reported = root.path.join(relative);
    let flags = INTERMEDIATE_HANDLE | libc::O_CLOEXEC;
    let descriptor = resolve(root, relative, flags, resolver())?;
    // The walk opens the final component `O_NOFOLLOW`, which for `O_PATH`
    // yields the symlink itself rather than an error, so the kind is checked
    // here for both resolvers rather than trusted to `O_DIRECTORY`.
    let metadata =
        metadata_of_descriptor(&descriptor).map_err(|error| ResolveError::io(&reported, &error))?;
    let kind = metadata.file_type();
    if kind.is_symlink() {
        return Err(ResolveError::escaped(&reported));
    }
    if !kind.is_dir() {
        return Err(ResolveError::message(&reported, "not a directory"));
    }
    Ok(Root {
        directory: descriptor,
        path: reported,
    })
}

/// Walks `parts` one component at a time, refusing any symlink.
fn walk(
    root: &Root,
    parts: &[&OsStr],
    flags: i32,
    reported: &Path,
) -> Result<OwnedFd, ResolveError> {
    let (name, leading) = parts
        .split_last()
        .ok_or_else(|| ResolveError::escaped(reported))?;
    let mut directory = root
        .directory
        .try_clone()
        .map_err(|error| ResolveError::io(reported, &error))?;
    let intermediate = INTERMEDIATE_HANDLE | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    for part in leading {
        let child = open_at(directory.as_raw_fd(), part, intermediate)
            .map_err(|error| classify(reported, &error))?;
        let metadata =
            metadata_of_descriptor(&child).map_err(|error| ResolveError::io(reported, &error))?;
        let kind = metadata.file_type();
        if kind.is_symlink() {
            return Err(ResolveError::escaped(reported));
        }
        if !kind.is_dir() {
            return Err(ResolveError::message(reported, "not a directory"));
        }
        directory = child;
    }
    open_at(directory.as_raw_fd(), name, flags | libc::O_NOFOLLOW)
        .map_err(|error| classify(reported, &error))
}

/// One kernel call that resolves `relative` beneath `root`, refusing both a
/// symlink in any component and a component that leaves it.
#[cfg(target_os = "linux")]
fn kernel_resolve(root: RawFd, relative: &Path, flags: i32) -> io::Result<OwnedFd> {
    openat2(root, relative, flags)
}

/// `O_NOFOLLOW_ANY` refuses a symlink in any component, the final one
/// included, with `ELOOP`. It does not itself confine the walk to `root`; that
/// holds because every caller first passes `relative` through [`components`],
/// which refuses absolute paths and every `..`.
#[cfg(target_os = "macos")]
fn kernel_resolve(root: RawFd, relative: &Path, flags: i32) -> io::Result<OwnedFd> {
    let name = c_path(relative)?;
    // SAFETY: `name` outlives the call, and `openat` returns a fresh owned
    // descriptor or -1.
    let descriptor = unsafe { libc::openat(root, name.as_ptr(), flags | libc::O_NOFOLLOW_ANY) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `descriptor` was just returned by a successful `openat` and is
    // not owned anywhere else.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

/// One `openat2` call restricted to `root`.
#[cfg(target_os = "linux")]
fn openat2(root: RawFd, relative: &Path, flags: i32) -> io::Result<OwnedFd> {
    let name = c_path(relative)?;
    let how = OpenHow {
        flags: flags as u32 as u64,
        mode: 0,
        resolve: libc::RESOLVE_BENEATH | libc::RESOLVE_NO_SYMLINKS,
    };
    // SAFETY: `name` and `how` outlive the call, the size passed matches the
    // struct actually pointed at, and the syscall returns a fresh owned
    // descriptor or a negative errno.
    let descriptor = unsafe {
        libc::syscall(
            libc::SYS_openat2,
            root,
            name.as_ptr(),
            std::ptr::from_ref(&how),
            std::mem::size_of::<OpenHow>(),
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `descriptor` was just returned by a successful `openat2` and is
    // not owned anywhere else.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor as RawFd) })
}

/// One `openat` call for a single component.
fn open_at(directory: RawFd, name: &OsStr, flags: i32) -> io::Result<OwnedFd> {
    let raw = c_name(name)?;
    // SAFETY: `raw` outlives the call and `openat` returns a fresh owned
    // descriptor or -1.
    let descriptor = unsafe { libc::openat(directory, raw.as_ptr(), flags) };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: `descriptor` was just returned by a successful `openat` and is
    // not owned anywhere else.
    Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
}

/// `ELOOP` and `EXDEV` are how the kernel reports the two refusals this module
/// exists to enforce: a symlink in the path, and a component that would leave
/// the root.
fn classify(reported: &Path, error: &io::Error) -> ResolveError {
    match error.raw_os_error() {
        Some(libc::ELOOP | libc::EXDEV) => ResolveError::escaped(reported),
        _ => ResolveError::io(reported, error),
    }
}

/// Stats an owned descriptor without taking ownership of it.
///
/// `Metadata` can only be produced by `std` from a `File`, so the descriptor is
/// duplicated for the call; the duplicate closes at the end of the expression.
fn metadata_of_descriptor(descriptor: &OwnedFd) -> io::Result<Metadata> {
    File::from(descriptor.try_clone()?).metadata()
}

/// The components of a relative path, or `None` when it is not usable beneath a
/// root at all: empty, absolute, or containing `.` or `..`.
///
/// `openat2` would refuse an escape by itself, but the walk would not, and both
/// resolvers must agree on what a valid input is.
fn components(relative: &Path) -> Option<Vec<&OsStr>> {
    let mut parts = Vec::new();
    for component in relative.components() {
        match component {
            Component::Normal(part) => parts.push(part),
            Component::RootDir
            | Component::CurDir
            | Component::ParentDir
            | Component::Prefix(_) => {
                return None;
            }
        }
    }
    if parts.is_empty() { None } else { Some(parts) }
}

/// A whole path as a C string.
fn c_path(path: &Path) -> io::Result<CString> {
    CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))
}

/// A single component as a C string, rejecting anything that would make
/// `openat` resolve more than one name.
fn c_name(name: &OsStr) -> io::Result<CString> {
    let bytes = name.as_bytes();
    if bytes.is_empty() || bytes.contains(&b'/') || bytes == b"." || bytes == b".." {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "not a single path component",
        ));
    }
    CString::new(bytes)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "name contains a NUL byte"))
}

/// [`c_name`] reported as a resolution failure.
fn c_component(name: &OsStr, reported: &Path) -> Result<CString, ResolveError> {
    c_name(name).map_err(|_| ResolveError::escaped(&reported.join(name)))
}

/// An unpredictable temporary file name.
fn temporary_name() -> io::Result<OsString> {
    let mut random = [0_u8; TEMPORARY_NAME_BYTES];
    fill_random(&mut random)?;
    let mut name = String::with_capacity(".cagents-tmp-".len() + random.len() * 2);
    name.push_str(".cagents-tmp-");
    for byte in random {
        name.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        name.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    Ok(OsString::from(name))
}

/// Fills `buffer` from the kernel's random pool.
#[cfg(target_os = "macos")]
pub fn fill_random(buffer: &mut [u8]) -> io::Result<()> {
    // `getentropy` fills at most 256 bytes per call and never short-reads.
    for chunk in buffer.chunks_mut(256) {
        // SAFETY: the pointer and length describe exactly `chunk`, which
        // outlives the call.
        if unsafe { libc::getentropy(chunk.as_mut_ptr().cast::<libc::c_void>(), chunk.len()) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Fills `buffer` from the kernel's random pool.
#[cfg(target_os = "linux")]
pub fn fill_random(buffer: &mut [u8]) -> io::Result<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        let remaining = &mut buffer[filled..];
        // SAFETY: the pointer and length describe exactly `remaining`, which
        // outlives the call.
        let written = unsafe {
            libc::getrandom(
                remaining.as_mut_ptr().cast::<libc::c_void>(),
                remaining.len(),
                0,
            )
        };
        if written < 0 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(error);
        }
        if written == 0 {
            return Err(io::Error::other("the random pool returned no bytes"));
        }
        filled += written as usize;
    }
    Ok(())
}

/// Access-ACL fixtures for tests of code that copies access rules, here and
/// in the UI crate.
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod acl_fixture {
    use super::{File, io, write_attribute};
    use std::os::{fd::AsRawFd, unix::fs::MetadataExt};

    pub fn restrictive_access_acl() -> Vec<u8> {
        // Linux POSIX ACL xattr v2: the mask is readable, but the owning group
        // has no access. Copying st_mode alone would incorrectly grant it read.
        let mut value = 2_u32.to_le_bytes().to_vec();
        for (tag, permissions, id) in [
            (0x01_u16, 6_u16, u32::MAX),
            (0x02, 4, 12_345),
            (0x04, 0, u32::MAX),
            (0x10, 4, u32::MAX),
            (0x20, 0, u32::MAX),
        ] {
            value.extend_from_slice(&tag.to_le_bytes());
            value.extend_from_slice(&permissions.to_le_bytes());
            value.extend_from_slice(&id.to_le_bytes());
        }
        value
    }

    pub fn set_test_access_acl(file: &File, value: &[u8]) -> bool {
        match write_attribute(file, c"system.posix_acl_access", value) {
            Ok(()) => true,
            // tmpfs without ACL support rejects the unknown xattr name with
            // EINVAL rather than EOPNOTSUPP; both mean the fixture cannot
            // carry an ACL here, matching the Resolver::Walk fallback used
            // by production probing.
            Err(error) if matches!(error.raw_os_error(), Some(libc::EOPNOTSUPP | libc::EINVAL)) => {
                false
            }
            Err(error) => panic!("could not set fixture access ACL: {error}"),
        }
    }

    pub fn access_acl(file: &File) -> Option<Vec<u8>> {
        super::read_attribute(file, c"system.posix_acl_access").unwrap()
    }

    pub fn set_default_acl(directory: &File, value: &[u8]) {
        write_attribute(directory, c"system.posix_acl_default", value).unwrap();
    }

    /// Select an actually permitted non-primary group for owned fixtures.
    pub fn shared_group(file: &File) -> Option<libc::gid_t> {
        // SAFETY: a zero count/null buffer asks only for the required count.
        let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
        assert!(count >= 0);
        let mut groups = vec![0; count as usize];
        // SAFETY: the buffer has exactly count writable gid entries.
        let count = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
        assert!(count >= 0);
        groups.truncate(count as usize);
        let primary = file.metadata().unwrap().gid();
        if unsafe { libc::geteuid() } == 0 {
            groups.push(if primary == 0 { 1 } else { 0 });
        }
        groups.sort_unstable();
        groups.dedup();
        for group in groups.into_iter().filter(|group| *group != primary) {
            // Supplementary groups can be inherited but unmapped inside a
            // rootless user namespace. Membership alone does not establish
            // that this process can assign that group to an owned fixture.
            // SAFETY: the descriptor is live; -1 leaves its owner unchanged.
            if unsafe { libc::fchown(file.as_raw_fd(), !0, group) } == 0 {
                return Some(group);
            }
            let error = io::Error::last_os_error();
            assert!(
                matches!(error.raw_os_error(), Some(libc::EPERM | libc::EINVAL)),
                "shared ownership fixture: {error}"
            );
        }
        eprintln!("shared ownership fixture requires another permitted group");
        None
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use acl_fixture::{restrictive_access_acl, set_test_access_acl, shared_group};

    use std::{
        fs,
        io::Write,
        os::unix::fs::{MetadataExt, PermissionsExt},
    };

    #[test]
    fn ownership_transfer_uses_descriptors_and_rejects_unusable_destination() {
        let directory = tempfile::tempdir().unwrap();
        let source = File::create(directory.path().join("source")).unwrap();
        let Some(group) = shared_group(&source) else {
            return;
        };
        let destination = File::create(directory.path().join("replacement")).unwrap();
        copy_ownership(&source, &destination).unwrap();
        assert_eq!(destination.metadata().unwrap().gid(), group);
        assert_eq!(
            destination.metadata().unwrap().uid(),
            source.metadata().unwrap().uid()
        );
        let root = Root::open(directory.path()).unwrap();
        let unusable = File::from(root.directory.try_clone().unwrap());
        assert!(copy_ownership(&source, &unusable).is_err());
    }

    fn fifo(path: &Path) {
        let name = c_path(path).expect("path");
        // SAFETY: `name` outlives the call.
        let status = unsafe { libc::mkfifo(name.as_ptr(), 0o600) };
        assert_eq!(status, 0, "mkfifo: {}", io::Error::last_os_error());
    }

    fn read_all(root: &Root, relative: &str) -> Vec<u8> {
        let mut file = open_beneath(root, Path::new(relative)).expect("open");
        read_bounded(&mut file, 4096).expect("read")
    }

    /// Both resolvers must satisfy every guarantee, and the walk is only
    /// reached on kernels this test suite does not run on, so it is exercised
    /// explicitly rather than left to chance.
    const RESOLVERS: [Resolver; 2] = [Resolver::Kernel, Resolver::Walk];

    #[test]
    fn a_relative_path_resolves_and_reads() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::create_dir(directory.path().join("src")).expect("mkdir");
        fs::write(directory.path().join("src/main.rs"), b"fn main() {}\n").expect("write");
        let root = Root::open(directory.path()).expect("root");

        for resolver in RESOLVERS {
            let descriptor = resolve(&root, Path::new("src/main.rs"), libc::O_RDONLY, resolver)
                .expect("resolve");
            let mut file = File::from(descriptor);
            let bytes = read_bounded(&mut file, 4096).expect("read");
            assert_eq!(bytes, b"fn main() {}\n", "{resolver:?}");
        }
    }

    #[test]
    fn a_symlinked_final_component_is_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("real.txt"), b"real\n").expect("write");
        std::os::unix::fs::symlink("real.txt", directory.path().join("link.txt")).expect("symlink");
        let root = Root::open(directory.path()).expect("root");

        for resolver in RESOLVERS {
            let error = resolve(&root, Path::new("link.txt"), libc::O_RDONLY, resolver)
                .expect_err("a symlinked final component must be refused");
            assert!(
                matches!(error, ResolveError::Escaped { .. }),
                "{resolver:?}: {error:?}"
            );
        }
    }

    #[test]
    fn a_symlinked_intermediate_component_is_rejected() {
        let outside = tempfile::tempdir().expect("outside");
        fs::write(outside.path().join("passwd"), b"outside\n").expect("write");
        let directory = tempfile::tempdir().expect("tempdir");
        std::os::unix::fs::symlink(outside.path(), directory.path().join("src")).expect("symlink");
        let root = Root::open(directory.path()).expect("root");

        for resolver in RESOLVERS {
            let error = resolve(&root, Path::new("src/passwd"), libc::O_RDONLY, resolver)
                .expect_err("a symlinked parent directory must be refused");
            assert!(
                matches!(error, ResolveError::Escaped { .. }),
                "{resolver:?}: {error:?}"
            );
        }

        // The same shape through the public entry points, which is how the
        // editor reaches it.
        assert!(matches!(
            open_beneath(&root, Path::new("src/passwd")),
            Err(ResolveError::Escaped { .. })
        ));
        assert!(matches!(
            open_parent_beneath(&root, Path::new("src/passwd")),
            Err(ResolveError::Escaped { .. })
        ));
    }

    #[test]
    fn an_absolute_path_and_a_parent_component_are_rejected() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("inside.txt"), b"inside\n").expect("write");
        let root = Root::open(directory.path()).expect("root");

        for resolver in RESOLVERS {
            for relative in ["/etc/passwd", "../inside.txt", "src/../../inside.txt", ""] {
                let error = resolve(&root, Path::new(relative), libc::O_RDONLY, resolver)
                    .expect_err("must be refused");
                assert!(
                    matches!(error, ResolveError::Escaped { .. }),
                    "{resolver:?} {relative}: {error:?}"
                );
            }
        }
    }

    #[test]
    fn a_fifo_opens_without_blocking_and_is_identifiable() {
        let directory = tempfile::tempdir().expect("tempdir");
        fifo(&directory.path().join("pipe"));
        let root = Root::open(directory.path()).expect("root");

        // No writer exists: a blocking open would never return.
        let file = open_beneath(&root, Path::new("pipe")).expect("open");
        let metadata = metadata_of(&file).expect("metadata");

        assert!(!metadata.is_file());
        assert_eq!(metadata.mode() & libc::S_IFMT, libc::S_IFIFO);
    }

    #[test]
    fn read_bounded_distinguishes_at_the_limit_from_over_it() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("exact"), vec![b'a'; 64]).expect("write");
        fs::write(directory.path().join("over"), vec![b'b'; 65]).expect("write");
        let root = Root::open(directory.path()).expect("root");

        let mut exact = open_beneath(&root, Path::new("exact")).expect("open");
        let mut over = open_beneath(&root, Path::new("over")).expect("open");

        assert_eq!(read_bounded(&mut exact, 64).expect("read").len(), 64);
        assert_eq!(read_bounded(&mut over, 64).expect("read").len(), 65);
    }

    #[test]
    fn a_temporary_file_is_renamed_over_its_target_atomically() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::create_dir(directory.path().join("src")).expect("mkdir");
        fs::write(directory.path().join("src/main.rs"), b"before\n").expect("write");
        let root = Root::open(directory.path()).expect("root");

        let (parent, name) = open_parent_beneath(&root, Path::new("src/main.rs")).expect("parent");
        let (mut temporary, temporary_name) =
            create_temporary_in(&parent, 0o640).expect("temporary");
        temporary.write_all(b"after\n").expect("write");
        temporary.sync_all().expect("sync");
        let mode = metadata_of(&temporary)
            .expect("metadata")
            .permissions()
            .mode()
            & 0o777;
        drop(temporary);
        rename_within(&parent, &temporary_name, &name).expect("rename");
        sync_directory(&parent).expect("sync directory");

        assert_eq!(mode, 0o640);
        assert_eq!(read_all(&root, "src/main.rs"), b"after\n");
        let leftovers: Vec<_> = fs::read_dir(directory.path().join("src"))
            .expect("read_dir")
            .map(|entry| entry.expect("entry").file_name())
            .filter(|entry| entry != "main.rs")
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn two_temporary_files_never_share_a_name() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = Root::open(directory.path()).expect("root");

        let (_first, first) = create_temporary_in(&root, 0o600).expect("first");
        let (_second, second) = create_temporary_in(&root, 0o600).expect("second");

        assert_ne!(first, second);
    }

    #[test]
    fn unlink_within_removes_a_symlink_rather_than_its_target() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("target.txt"), b"target\n").expect("write");
        std::os::unix::fs::symlink("target.txt", directory.path().join("link.txt"))
            .expect("symlink");
        let root = Root::open(directory.path()).expect("root");

        unlink_within(&root, OsStr::new("link.txt")).expect("unlink");

        assert!(!directory.path().join("link.txt").exists());
        assert_eq!(
            fs::read(directory.path().join("target.txt")).expect("read"),
            b"target\n"
        );
    }

    #[test]
    fn metadata_beneath_describes_a_symlink_as_itself() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::write(directory.path().join("target.txt"), b"target\n").expect("write");
        std::os::unix::fs::symlink("target.txt", directory.path().join("link.txt"))
            .expect("symlink");
        let root = Root::open(directory.path()).expect("root");

        let metadata = metadata_beneath(&root, Path::new("link.txt")).expect("metadata");
        let target = read_link_beneath(&root, Path::new("link.txt")).expect("read_link");

        assert!(metadata.file_type().is_symlink());
        assert_eq!(target, Path::new("target.txt"));
    }

    #[test]
    fn metadata_beneath_reports_absence_separately_from_failure() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = Root::open(directory.path()).expect("root");

        let error = metadata_beneath(&root, Path::new("missing.txt")).expect_err("missing");

        assert!(error.is_not_found(), "{error:?}");
    }

    #[test]
    fn sync_directory_survives_an_unlinked_directory() {
        // An `O_PATH` descriptor keeps the inode alive, so `.` still resolves
        // through it after the name is gone and the sync still succeeds. This
        // is asserted rather than assumed because the save path syncs the
        // parent directory it captured before writing, and a spurious failure
        // there would report a durable write as failed.
        let directory = tempfile::tempdir().expect("tempdir");
        fs::create_dir(directory.path().join("gone")).expect("mkdir");
        let root = Root::open(&directory.path().join("gone")).expect("root");
        fs::remove_dir(directory.path().join("gone")).expect("rmdir");

        assert!(sync_directory(&root).is_ok());
    }

    #[test]
    fn a_writable_file_opens_read_write() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("notes.txt");
        fs::write(&path, b"one\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("chmod");
        let root = Root::open(directory.path()).expect("root");

        let mut file = open_beneath_writable(&root, Path::new("notes.txt"))
            .expect("open")
            .expect("a 0644 file this user owns must be writable");

        file.write_all(b"two\n")
            .expect("the descriptor must accept a write");
    }

    #[test]
    fn an_unwritable_file_is_reported_rather_than_failing_the_open() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("notes.txt");
        fs::write(&path, b"one\n").expect("write");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444)).expect("chmod");
        let root = Root::open(directory.path()).expect("root");

        let writable = open_beneath_writable(&root, Path::new("notes.txt")).expect("open");

        if writable.is_some() {
            // The mode bits do not restrain this user, so there is no refusal
            // to observe.
            return;
        }
        // The refusal is not the last word: the file is still perfectly
        // readable, which is how a caller tells it apart from an unsearchable
        // parent directory.
        assert_eq!(read_all(&root, "notes.txt"), b"one\n");
    }

    #[test]
    fn a_missing_file_reports_absence_from_the_writable_open() {
        let directory = tempfile::tempdir().expect("tempdir");
        let root = Root::open(directory.path()).expect("root");

        let error = open_beneath_writable(&root, Path::new("absent.txt")).expect_err("missing");

        assert!(error.is_not_found(), "{error}");
        assert!(
            !error.is_write_refused(),
            "absence is not a refusal to grant write access: {error}",
        );
    }

    /// Sets `user.note` on `path`, reporting whether the filesystem supports
    /// extended attributes at all.
    fn tag(path: &Path) -> bool {
        let name = c_path(path).expect("path");
        // SAFETY: both strings and the value outlive the call, and the length
        // passed is the value's own.
        let status = unsafe {
            libc::setxattr(
                name.as_ptr(),
                c"user.note".as_ptr(),
                b"keepme".as_ptr().cast::<libc::c_void>(),
                6,
                0,
            )
        };
        status == 0
    }

    #[test]
    fn only_the_user_and_access_control_namespaces_are_reproduced() {
        // The namespace split is the whole policy, so it is asserted directly
        // rather than inferred from a filesystem that happens to carry one of
        // these. `security.selinux` in particular is present on every inode on
        // an enforcing kernel and is reproduced there by policy, so copying it
        // is both unnecessary and — for a label this process may not set — a
        // guaranteed `EPERM` on every save.
        for name in [
            c"user.note",
            c"system.posix_acl_access",
            c"system.posix_acl_default",
        ] {
            assert!(is_content_attribute(name), "{name:?} is the file's own");
        }
        for name in [
            c"security.selinux",
            c"security.capability",
            c"security.ima",
            c"trusted.overlay.origin",
        ] {
            assert!(
                !is_content_attribute(name),
                "{name:?} belongs to the kernel or the filesystem",
            );
        }
    }

    #[test]
    fn retained_save_attribute_snapshot_reads_values_and_enforces_budget() {
        let directory = tempfile::tempdir().expect("tempdir");
        let path = directory.path().join("notes.txt");
        fs::write(&path, b"one\n").expect("write");
        if !tag(&path) {
            return;
        }
        let file = File::open(&path).expect("open");
        let attributes = content_attributes(&file).expect("snapshot");
        assert_eq!(
            attributes.get(OsStr::new("user.note")),
            Some(&b"keepme".to_vec())
        );
        assert!(read_attribute_with_limit(&file, c"user.note", 5).is_err());
        assert!(attribute_names_with_limit(&file, 1).is_err());
        write_attribute(&file, c"user.note", b"new").expect("change attribute");
        assert_ne!(
            content_attributes(&file).expect("changed snapshot"),
            attributes
        );
    }

    #[test]
    fn extended_attributes_are_copied_onto_a_replacement() {
        let directory = tempfile::tempdir().expect("tempdir");
        let original = directory.path().join("notes.txt");
        fs::write(&original, b"one\n").expect("write");
        if !tag(&original) {
            // No extended-attribute support here, so there is nothing to copy
            // and nothing this test could observe.
            return;
        }
        let replacement = directory.path().join("replacement.txt");
        fs::write(&replacement, b"two\n").expect("write");
        let root = Root::open(directory.path()).expect("root");
        let source = open_beneath(&root, Path::new("notes.txt")).expect("open");
        let destination = open_beneath_writable(&root, Path::new("replacement.txt"))
            .expect("open")
            .expect("writable");

        let lost = copy_extended_attributes(&source, &destination);

        assert!(lost.is_empty(), "{lost:?}");
        assert_eq!(
            attribute_names(&destination)
                .expect("list")
                .into_iter()
                .filter(|name| is_content_attribute(name))
                .collect::<Vec<_>>(),
            vec![CString::new("user.note").expect("name")],
            "the replacement carries the original's content attribute",
        );
        assert_eq!(
            read_attribute(&destination, c"user.note").expect("read"),
            Some(b"keepme".to_vec()),
        );
    }

    #[test]
    fn an_attribute_the_replacement_refuses_is_reported_rather_than_raised() {
        // A `user.*` attribute may only live on a regular file or a directory,
        // so a FIFO is a destination the kernel refuses on purpose — the same
        // shape as the `ENOSPC` and `EDQUOT` this must survive, which cannot be
        // provoked on demand. Losing the user's bytes over it would be the
        // wrong trade, so the refusal comes back as a report.
        let directory = tempfile::tempdir().expect("tempdir");
        let original = directory.path().join("notes.txt");
        fs::write(&original, b"one\n").expect("write");
        if !tag(&original) {
            return;
        }
        let pipe = directory.path().join("pipe");
        let name = c_path(&pipe).expect("path");
        // SAFETY: `name` is a NUL-terminated string that outlives the call.
        assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
        let root = Root::open(directory.path()).expect("root");
        let source = open_beneath(&root, Path::new("notes.txt")).expect("open");
        let destination = open_beneath_writable(&root, Path::new("pipe"))
            .expect("open")
            .expect("writable");

        let lost = copy_extended_attributes(&source, &destination);

        assert!(
            matches!(
                lost.as_slice(),
                [AttributeNotCopied::Attribute { name, .. }] if name == "user.note"
            ),
            "{lost:?}",
        );
    }

    #[test]
    fn access_acl_transfer_failure_is_an_error_not_optional_metadata_loss() {
        let directory = tempfile::tempdir().unwrap();
        let source = File::create(directory.path().join("source")).unwrap();
        if !set_test_access_acl(&source, &restrictive_access_acl()) {
            return;
        }
        let root = Root::open(directory.path()).unwrap();
        // An O_PATH descriptor is deliberately unable to receive an ACL.
        let invalid_destination = File::from(root.directory.try_clone().unwrap());
        assert!(copy_access_acl(&source, &invalid_destination).is_err());
    }

    #[test]
    fn a_root_reports_its_own_identity() {
        let directory = tempfile::tempdir().expect("tempdir");
        fs::create_dir(directory.path().join("nested")).expect("mkdir");
        std::os::unix::fs::symlink(
            directory.path().join("nested"),
            directory.path().join("link"),
        )
        .expect("symlink");
        let real = Root::open(&directory.path().join("nested")).expect("root");
        let through_link = Root::open(&directory.path().join("link"));

        let identity = real.metadata().expect("fstat");
        let named = fs::metadata(directory.path().join("nested")).expect("metadata");
        assert_eq!(identity.dev(), named.dev());
        assert_eq!(identity.ino(), named.ino());
        // `Root::open` is `O_NOFOLLOW`, so the link is not a way to reach the
        // same directory: identity is only ever taken from a descriptor that
        // already refuses one.
        assert!(through_link.is_err(), "a symlinked root must be refused");
    }

    #[test]
    fn the_probed_resolver_is_decided_once() {
        let first = resolver();
        let second = resolver();

        assert_eq!(first, second);
    }
}
