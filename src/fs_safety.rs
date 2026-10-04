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
//! Resolution prefers `openat2` with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`,
//! which enforces both properties in the kernel in one syscall. Kernels older
//! than 5.6 answer `ENOSYS`, and seccomp filters that do not know the syscall
//! answer `EINVAL`; those fall back to an `openat` walk that opens one
//! component at a time with `O_NOFOLLOW` and verifies each intermediate really
//! is a directory. Which of the two is in use is decided once, not per call.

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
    sync::LazyLock,
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

/// `struct open_how` from `linux/openat2.h`.
///
/// Declared here rather than reusing `libc::open_how`, which is
/// `#[non_exhaustive]` and therefore cannot be built from a literal; a zeroed
/// value would need an `unsafe` block for no benefit.
#[repr(C)]
#[derive(Default)]
struct OpenHow {
    flags: u64,
    mode: u64,
    resolve: u64,
}

/// A directory a path may be resolved beneath.
///
/// The descriptor is `O_PATH`, which needs no read permission on the directory
/// and cannot be used for anything but further fd-relative resolution — the
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
        let flags = libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC;
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

    fn descriptor(&self) -> RawFd {
        self.directory.as_raw_fd()
    }
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
        /// Whether the kernel refused write access specifically. A caller that
        /// presents an unwritable file read-only rather than failing to open it
        /// needs to tell that apart from an unsearchable parent directory,
        /// which reports the same errno from a different object.
        write_refused: bool,
    },
}

impl ResolveError {
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
            not_found: error.kind() == io::ErrorKind::NotFound,
            write_refused,
        }
    }

    fn message(path: &Path, message: &str) -> Self {
        Self::Io {
            path: path.to_path_buf(),
            message: message.to_owned(),
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
    let flags = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
    let entry = open_at(directory.descriptor(), &name, flags)
        .map_err(|error| classify(&reported, &error))?;
    metadata_of_descriptor(&entry).map_err(|error| ResolveError::io(&reported, &error))
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

/// Creates a file inside `directory` with `mode`, for the atomic
/// write-then-rename sequence. The name is generated and unpredictable.
///
/// # Errors
///
/// Returns [`ResolveError::Io`] when no name could be created, which includes
/// the case where randomness is unavailable.
pub fn create_temporary_in(directory: &Root, mode: u32) -> Result<(File, OsString), ResolveError> {
    let flags = libc::O_WRONLY | libc::O_CREAT | libc::O_EXCL | libc::O_NOFOLLOW | libc::O_CLOEXEC;
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
                libc::mode_t::from(mode),
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
    let status = unsafe {
        libc::renameat2(
            source_directory.descriptor(),
            source.as_ptr(),
            destination_directory.descriptor(),
            destination.as_ptr(),
            libc::RENAME_NOREPLACE,
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

/// Preserves the source's POSIX access ACL on a new file after its mode is set.
/// Unlike optional content attributes, losing an ACL can grant unintended
/// access, so every failure prevents publication. An absent source ACL removes
/// any access ACL inherited by the destination from its parent.
pub fn copy_access_acl(source: &File, destination: &File) -> io::Result<()> {
    let name = c"system.posix_acl_access";
    let value = match read_attribute(source, name) {
        Ok(value) => value,
        Err(error) if error.raw_os_error() == Some(libc::EOPNOTSUPP) => None,
        Err(error) => return Err(error),
    };
    if let Some(value) = value {
        return write_attribute(destination, name, &value);
    }
    // SAFETY: the descriptor is live and the attribute name is static.
    let status = unsafe { libc::fremovexattr(destination.as_raw_fd(), name.as_ptr()) };
    if status < 0 {
        let error = io::Error::last_os_error();
        if !matches!(error.raw_os_error(), Some(libc::ENODATA | libc::EOPNOTSUPP)) {
            return Err(error);
        }
    }
    Ok(())
}

/// Whether an attribute describes the file's content rather than the kernel's
/// or the filesystem's relationship with the inode. See
/// [`copy_extended_attributes`] for why the line falls here.
fn is_content_attribute(name: &CStr) -> bool {
    let bytes = name.to_bytes();
    bytes.starts_with(b"user.") || bytes.starts_with(b"system.")
}

/// Every extended-attribute name an open file carries.
fn attribute_names(file: &File) -> io::Result<Vec<CString>> {
    let mut buffer = Vec::new();
    for _ in 0..ATTRIBUTE_SIZE_ATTEMPTS {
        // SAFETY: a null buffer with a zero length asks `flistxattr` only for
        // the size the list would need, which is the documented way to query
        // it.
        let size = unsafe { libc::flistxattr(file.as_raw_fd(), ptr::null_mut(), 0) };
        if size < 0 {
            let error = io::Error::last_os_error();
            // A filesystem that has no such thing as an extended attribute
            // carries none, which is an answer and not a failure.
            if matches!(error.raw_os_error(), Some(libc::ENOTSUP | libc::ENOSYS)) {
                return Ok(Vec::new());
            }
            return Err(error);
        }
        if size == 0 {
            return Ok(Vec::new());
        }
        buffer.resize(size as usize, 0);
        // SAFETY: the write is bounded by the buffer's own length.
        let read = unsafe {
            libc::flistxattr(
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
    let mut value = Vec::new();
    for _ in 0..ATTRIBUTE_SIZE_ATTEMPTS {
        // SAFETY: `name` outlives the call, and a null buffer with a zero
        // length asks only for the size the value would need.
        let size = unsafe { libc::fgetxattr(file.as_raw_fd(), name.as_ptr(), ptr::null_mut(), 0) };
        if size < 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ENODATA) {
                return Ok(None);
            }
            return Err(error);
        }
        value.resize(size as usize, 0);
        // SAFETY: `name` outlives the call and the write is bounded by the
        // buffer's own length. An empty value passes a dangling but aligned
        // pointer with a zero length, which `fgetxattr` never dereferences.
        let read = unsafe {
            libc::fgetxattr(
                file.as_raw_fd(),
                name.as_ptr(),
                value.as_mut_ptr().cast::<libc::c_void>(),
                value.len(),
            )
        };
        if read >= 0 {
            value.truncate(read as usize);
            return Ok(Some(value));
        }
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENODATA) {
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
        libc::fsetxattr(
            file.as_raw_fd(),
            name.as_ptr(),
            value.as_ptr().cast::<libc::c_void>(),
            value.len(),
            0,
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
    /// One `openat2` call with `RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS`.
    Kernel,
    /// A component-at-a-time `openat` walk with `O_NOFOLLOW`.
    Walk,
}

/// The resolver this kernel supports, decided once.
fn resolver() -> Resolver {
    static SUPPORTED: LazyLock<Resolver> = LazyLock::new(probe_resolver);
    *SUPPORTED
}

/// Probes `openat2` by resolving `.` beneath the current directory.
///
/// Only a kernel or sandbox that does not know the syscall selects the walk;
/// an ordinary failure such as an unreadable working directory says nothing
/// about the syscall's availability and must not disable it forever.
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
        Resolver::Kernel => {
            openat2(root.descriptor(), relative, flags).map_err(|error| classify(&reported, &error))
        }
        Resolver::Walk => walk(root, &parts, flags, &reported),
    }
}

/// Resolves `relative` beneath `root` as a directory.
fn resolve_directory(root: &Root, relative: &Path) -> Result<Root, ResolveError> {
    let reported = root.path.join(relative);
    let flags = libc::O_PATH | libc::O_CLOEXEC;
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
    let intermediate = libc::O_PATH | libc::O_NOFOLLOW | libc::O_CLOEXEC;
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

/// One `openat2` call restricted to `root`.
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
fn fill_random(buffer: &mut [u8]) -> io::Result<()> {
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

#[cfg(test)]
mod tests {
    use super::*;

    use std::{
        fs,
        io::Write,
        os::unix::fs::{MetadataExt, PermissionsExt},
    };

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
            attribute_names(&destination).expect("list"),
            vec![CString::new("user.note").expect("name")],
            "the replacement carries the original's attribute",
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

    fn restrictive_access_acl() -> Vec<u8> {
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

    fn set_test_access_acl(file: &File, value: &[u8]) -> bool {
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

    #[cfg(feature = "ui")]
    #[test]
    fn editor_copy_preserves_access_acl_and_removes_unwanted_inheritance() {
        let directory = tempfile::tempdir().unwrap();
        fs::write(directory.path().join("restricted"), b"private contents").unwrap();
        fs::write(directory.path().join("plain"), b"plain contents").unwrap();
        let restricted = File::open(directory.path().join("restricted")).unwrap();
        let acl = restrictive_access_acl();
        if !set_test_access_acl(&restricted, &acl) {
            return;
        }
        assert_eq!(
            restricted.metadata().unwrap().permissions().mode() & 0o777,
            0o640
        );
        fs::create_dir(directory.path().join("copies")).unwrap();
        let parent = File::open(directory.path().join("copies")).unwrap();
        write_attribute(&parent, c"system.posix_acl_default", &acl).unwrap();
        let plain = File::open(directory.path().join("plain")).unwrap();
        plain
            .set_permissions(fs::Permissions::from_mode(0o640))
            .unwrap();

        for name in ["restricted", "plain"] {
            crate::editor::copy_file(
                directory.path(),
                Path::new(name),
                &Path::new("copies").join(name),
            )
            .unwrap();
        }
        let restricted_copy = File::open(directory.path().join("copies/restricted")).unwrap();
        let plain_copy = File::open(directory.path().join("copies/plain")).unwrap();
        assert_eq!(
            read_attribute(&restricted_copy, c"system.posix_acl_access").unwrap(),
            Some(acl)
        );
        assert_eq!(
            read_attribute(&plain_copy, c"system.posix_acl_access").unwrap(),
            None
        );
        assert_eq!(
            plain_copy.metadata().unwrap().permissions().mode() & 0o777,
            0o640
        );
        assert_eq!(
            fs::read(directory.path().join("restricted")).unwrap(),
            b"private contents"
        );
        assert_eq!(
            fs::read(directory.path().join("copies/restricted")).unwrap(),
            b"private contents"
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
