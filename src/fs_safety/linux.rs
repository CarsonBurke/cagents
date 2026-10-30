//! Linux ABI for the descriptor-based filesystem contracts.
use std::{ffi::CStr, io};

pub(super) const IDENTITY_OPEN: libc::c_int = libc::O_PATH;
pub(super) const METADATA_OPEN: libc::c_int = libc::O_PATH;

pub(super) fn is_content_attribute(name: &CStr) -> bool {
    let bytes = name.to_bytes();
    bytes.starts_with(b"user.") || bytes.starts_with(b"system.")
}

pub(super) fn attribute_missing(error: &io::Error) -> bool {
    error.raw_os_error() == Some(libc::ENODATA)
}

pub(super) unsafe fn rename_exclusive(
    source: libc::c_int,
    from: *const libc::c_char,
    destination: libc::c_int,
    to: *const libc::c_char,
) -> libc::c_int {
    // SAFETY: caller supplies live directory descriptors and C leaf names.
    unsafe { libc::renameat2(source, from, destination, to, libc::RENAME_NOREPLACE) }
}

pub(super) unsafe fn list_attributes(
    descriptor: libc::c_int,
    buffer: *mut libc::c_char,
    size: usize,
) -> libc::ssize_t {
    // SAFETY: caller supplies a writable buffer of size bytes, or a size probe.
    unsafe { libc::flistxattr(descriptor, buffer, size) }
}

pub(super) unsafe fn get_attribute(
    descriptor: libc::c_int,
    name: *const libc::c_char,
    buffer: *mut libc::c_void,
    size: usize,
) -> libc::ssize_t {
    // SAFETY: caller supplies a C name and writable buffer, or a size probe.
    unsafe { libc::fgetxattr(descriptor, name, buffer, size) }
}

pub(super) unsafe fn set_attribute(
    descriptor: libc::c_int,
    name: *const libc::c_char,
    buffer: *const libc::c_void,
    size: usize,
    flags: libc::c_int,
) -> libc::c_int {
    // SAFETY: caller supplies a C name and readable buffer of size bytes.
    unsafe { libc::fsetxattr(descriptor, name, buffer, size, flags) }
}

pub(super) unsafe fn remove_attribute(
    descriptor: libc::c_int,
    name: *const libc::c_char,
) -> libc::c_int {
    // SAFETY: caller supplies a live descriptor and C attribute name.
    unsafe { libc::fremovexattr(descriptor, name) }
}
