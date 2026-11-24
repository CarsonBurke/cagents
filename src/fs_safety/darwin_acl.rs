//! The extended ACL of a file on macOS, captured and applied through its
//! descriptor.
//!
//! Darwin keeps a file's ACL apart from its extended attributes and its mode,
//! so [`AccessAcl`] carries it as the ACL library's own external form: the
//! bytes `acl_copy_ext_native` writes and `acl_copy_int_native` reads back,
//! with no interpretation here.

use super::AccessAcl;
use std::{ffi::c_void, fs::File, io, os::fd::AsRawFd};

/// `acl_t`, an opaque pointer owned by the ACL library.
type Acl = *mut c_void;

/// `ACL_TYPE_EXTENDED` from `<sys/acl.h>`, the only type Darwin implements.
const ACL_TYPE_EXTENDED: libc::c_uint = 0x0000_0100;

unsafe extern "C" {
    fn acl_get_fd_np(fd: libc::c_int, kind: libc::c_uint) -> Acl;
    fn acl_set_fd_np(fd: libc::c_int, acl: Acl, kind: libc::c_uint) -> libc::c_int;
    fn acl_size(acl: Acl) -> libc::ssize_t;
    fn acl_copy_ext_native(buffer: *mut c_void, acl: Acl, size: libc::ssize_t) -> libc::ssize_t;
    fn acl_copy_int_native(buffer: *const c_void) -> Acl;
    fn acl_init(count: libc::c_int) -> Acl;
    fn acl_free(object: *mut c_void) -> libc::c_int;
}

/// An ACL the library allocated, freed when dropped.
struct Owned(Acl);

impl Owned {
    /// Takes ownership of a library result, or reports `errno` for a null one.
    fn new(acl: Acl) -> io::Result<Self> {
        if acl.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(acl))
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        // SAFETY: the pointer came from the ACL library, is non-null, and is
        // freed exactly once, here.
        unsafe { acl_free(self.0) };
    }
}

/// Whether a failed ACL read means the file simply has none.
fn absent(error: &io::Error) -> bool {
    // `ENOENT` is how `acl_get_fd_np` reports a file without an ACL;
    // a filesystem that has no ACLs at all answers `ENOTSUP`.
    matches!(error.raw_os_error(), Some(libc::ENOENT | libc::ENOTSUP))
}

impl AccessAcl {
    pub fn capture(source: &File) -> io::Result<Self> {
        // SAFETY: the descriptor is live for the call, and the result is
        // either null or an ACL this call now owns.
        let acl = match Owned::new(unsafe { acl_get_fd_np(source.as_raw_fd(), ACL_TYPE_EXTENDED) })
        {
            Ok(acl) => acl,
            Err(error) if absent(&error) => return Ok(Self(None)),
            Err(error) => return Err(error),
        };
        // SAFETY: `acl` is a live ACL.
        let size = unsafe { acl_size(acl.0) };
        let size = usize::try_from(size).map_err(|_| io::Error::last_os_error())?;
        let mut external = vec![0_u8; size];
        // SAFETY: the buffer is exactly `size` bytes, the size the library
        // asked for, and outlives the call.
        let written = unsafe {
            acl_copy_ext_native(
                external.as_mut_ptr().cast::<c_void>(),
                acl.0,
                size as libc::ssize_t,
            )
        };
        if written < 0 {
            return Err(io::Error::last_os_error());
        }
        external.truncate(written as usize);
        Ok(Self(Some(external)))
    }

    pub fn apply(&self, destination: &File) -> io::Result<()> {
        let acl = match &self.0 {
            // SAFETY: the bytes are an external form this library wrote, and
            // they outlive the call; the result is null or a new owned ACL.
            Some(external) => Owned::new(unsafe { acl_copy_int_native(external.as_ptr().cast()) })?,
            // An empty ACL replaces whatever the destination inherited from
            // its parent directory, as removing the POSIX attribute does.
            // SAFETY: the result is null or a new owned ACL.
            None => Owned::new(unsafe { acl_init(0) })?,
        };
        // SAFETY: the descriptor and the ACL are both live for the call.
        if unsafe { acl_set_fd_np(destination.as_raw_fd(), acl.0, ACL_TYPE_EXTENDED) } < 0 {
            let error = io::Error::last_os_error();
            if self.0.is_none() && error.raw_os_error() == Some(libc::ENOTSUP) {
                // Nothing to remove on a filesystem without ACLs.
                return Ok(());
            }
            return Err(error);
        }
        Ok(())
    }
}
