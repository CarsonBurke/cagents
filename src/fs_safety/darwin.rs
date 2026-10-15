//! Darwin APIs retain the same already-open-object and no-replace contracts.
use std::{ffi::CStr, fs::File, io, mem::MaybeUninit, os::fd::AsRawFd, ptr};

// Event-only descriptors support metadata and fd-relative traversal without
// opening the contents. Nonblocking mode also avoids waiting on special files.
pub(super) const IDENTITY_OPEN: libc::c_int = libc::O_EVTONLY | libc::O_NONBLOCK;
// Only metadata_beneath permits opening a final symlink as the link itself.
pub(super) const METADATA_OPEN: libc::c_int = IDENTITY_OPEN | libc::O_SYMLINK;

const FILESEC_ACL: libc::c_int = 5;
const SAVE_ATTRIBUTE_INTENT: libc::c_uint = 2;

unsafe extern "C" {
    fn xattr_preserve_for_intent(name: *const libc::c_char, intent: libc::c_uint) -> libc::c_int;
    fn filesec_init() -> *mut libc::c_void;
    fn filesec_free(security: *mut libc::c_void);
    fn filesec_get_property(
        security: *mut libc::c_void,
        property: libc::c_int,
        value: *mut libc::c_void,
    ) -> libc::c_int;
    fn filesec_set_property(
        security: *mut libc::c_void,
        property: libc::c_int,
        value: *const libc::c_void,
    ) -> libc::c_int;
    #[cfg_attr(not(target_arch = "aarch64"), link_name = "fstatx_np$INODE64")]
    fn fstatx_np(
        descriptor: libc::c_int,
        metadata: *mut libc::stat,
        security: *mut libc::c_void,
    ) -> libc::c_int;
    fn fchmodx_np(descriptor: libc::c_int, security: *mut libc::c_void) -> libc::c_int;
    fn acl_free(acl: *mut libc::c_void) -> libc::c_int;
}

pub(super) fn is_content_attribute(name: &CStr) -> bool {
    // Access control is mandatory and copied through its native ACL API below.
    // Ask the OS's safe-save policy which other metadata survives new contents;
    // this includes restrictive quarantine metadata without copying stale
    // content signatures or inventing Linux namespace rules on Darwin.
    if name.to_bytes() == b"com.apple.system.Security" {
        return false;
    }
    // SAFETY: name is a live C string; this API reads it without retaining it.
    unsafe { xattr_preserve_for_intent(name.as_ptr(), SAVE_ATTRIBUTE_INTENT) != 0 }
}

pub(super) fn attribute_missing(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENOATTR)
}

pub(super) unsafe fn rename_exclusive(
    source: libc::c_int,
    from: *const libc::c_char,
    destination: libc::c_int,
    to: *const libc::c_char,
) -> libc::c_int {
    // SAFETY: caller supplies live directory descriptors and C leaf names.
    unsafe { libc::renameatx_np(source, from, destination, to, libc::RENAME_EXCL) }
}

pub(super) unsafe fn list_attributes(
    descriptor: libc::c_int,
    buffer: *mut libc::c_char,
    size: usize,
) -> libc::ssize_t {
    // SAFETY: caller supplies a writable buffer of size bytes, or a size probe.
    unsafe { libc::flistxattr(descriptor, buffer, size, 0) }
}

pub(super) unsafe fn get_attribute(
    descriptor: libc::c_int,
    name: *const libc::c_char,
    buffer: *mut libc::c_void,
    size: usize,
) -> libc::ssize_t {
    // SAFETY: caller supplies a C name and writable buffer, or a size probe.
    unsafe { libc::fgetxattr(descriptor, name, buffer, size, 0, 0) }
}

pub(super) unsafe fn set_attribute(
    descriptor: libc::c_int,
    name: *const libc::c_char,
    buffer: *const libc::c_void,
    size: usize,
    flags: libc::c_int,
) -> libc::c_int {
    // SAFETY: caller supplies a C name and readable buffer of size bytes.
    unsafe { libc::fsetxattr(descriptor, name, buffer, size, 0, flags) }
}

struct FileSecurity(*mut libc::c_void);
impl FileSecurity {
    fn new() -> io::Result<Self> {
        // SAFETY: returns a fresh opaque allocation, or null on failure.
        let value = unsafe { filesec_init() };
        if value.is_null() {
            Err(io::Error::last_os_error())
        } else {
            Ok(Self(value))
        }
    }
}
impl Drop for FileSecurity {
    fn drop(&mut self) {
        // SAFETY: this wrapper exclusively owns the allocation.
        unsafe { filesec_free(self.0) };
    }
}

struct Acl(*mut libc::c_void);
impl Drop for Acl {
    fn drop(&mut self) {
        // The native API uses pointer1 as the remove-ACL marker, not an
        // allocation. No marker or null pointer is passed to the allocator.
        if !self.0.is_null() && self.0 as usize != 1 {
            // SAFETY: filesec_get_property allocated this independent ACL.
            unsafe { acl_free(self.0) };
        }
    }
}

pub(super) fn copy_access_acl(source: &File, destination: &File) -> io::Result<()> {
    let original = FileSecurity::new()?;
    let replacement = FileSecurity::new()?;
    let mut metadata = MaybeUninit::<libc::stat>::uninit();
    // Read failure must not be confused with a successfully observed absent
    // ACL. fstatx fills only caller-owned storage through the retained source fd.
    // SAFETY: all output allocations and the source descriptor remain live.
    if unsafe { fstatx_np(source.as_raw_fd(), metadata.as_mut_ptr(), original.0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let mut acl = Acl(ptr::null_mut());
    // SAFETY: the output points to an opaque pointer-sized ACL slot.
    let status = unsafe {
        filesec_get_property(
            original.0,
            FILESEC_ACL,
            (&mut acl.0 as *mut *mut libc::c_void).cast(),
        )
    };
    let property = if status == 0 {
        if acl.0.is_null() {
            return Err(io::Error::other(
                "the filesystem returned an invalid access ACL",
            ));
        }
        if acl.0 as usize == 1 {
            ptr::without_provenance(1)
        } else {
            (&acl.0 as *const *mut libc::c_void).cast()
        }
    } else {
        let error = io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ENOENT) {
            return Err(error);
        }
        // After successful source stat, ENOENT means this property is absent.
        // Explicit removal prevents inheriting the temporary parent's ACL.
        ptr::without_provenance(1)
    };
    // A fresh security object contains only the ACL, so applying it cannot
    // restore set-ID mode bits or overwrite ownership already checked by caller.
    // SAFETY: property is an ACL pointer slot or the documented remove marker.
    if unsafe { filesec_set_property(replacement.0, FILESEC_ACL, property) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: destination is retained; replacement owns its independent ACL.
    if unsafe { fchmodx_np(destination.as_raw_fd(), replacement.0) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}
