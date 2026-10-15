//! Names from an owned directory stream, without a procfs or path fallback.
use std::{
    ffi::{CStr, OsString},
    io,
    os::{fd::{FromRawFd, IntoRawFd, OwnedFd}, unix::ffi::OsStringExt},
    ptr::NonNull,
};

/// A streaming list of names from one already-open directory.
///
/// Entries carry no path authority: resolve each name beneath the retained
/// root before opening it or observing its type. Dropping the iterator closes
/// its independent directory descriptor, including after an early limit.
#[derive(Debug)]
pub struct DirectoryNames {
    stream: NonNull<libc::DIR>,
    finished: bool,
}

impl DirectoryNames {
    pub(super) fn new(descriptor: OwnedFd) -> io::Result<Self> {
        let descriptor = descriptor.into_raw_fd();
        // SAFETY: this is a fresh readable directory descriptor. fdopendir
        // takes ownership only on success; no other object retains its offset.
        let stream = unsafe { libc::fdopendir(descriptor) };
        match NonNull::new(stream) {
            Some(stream) => Ok(Self { stream, finished: false }),
            None => {
                let error = io::Error::last_os_error();
                // SAFETY: failure left the original descriptor with the caller.
                drop(unsafe { OwnedFd::from_raw_fd(descriptor) });
                Err(error)
            }
        }
    }
}

impl Iterator for DirectoryNames {
    type Item = io::Result<OsString>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.finished {
            // readdir uses null for both EOF and failure. Reset only this
            // thread's errno immediately before the call to distinguish them.
            #[cfg(target_os = "macos")]
            let error_slot = unsafe { libc::__error() };
            #[cfg(not(target_os = "macos"))]
            let error_slot = unsafe { libc::__errno_location() };
            // SAFETY: error_slot is this thread's live errno slot; stream is
            // exclusively owned and remains valid until this iterator drops.
            let entry = unsafe {
                *error_slot = 0;
                libc::readdir(self.stream.as_ptr())
            };
            if entry.is_null() {
                let error = io::Error::last_os_error();
                self.finished = true;
                return (error.raw_os_error() != Some(0)).then_some(Err(error));
            }
            // SAFETY: readdir returned a live NUL-terminated name. Copy it
            // before the next call can overwrite the directory's buffer.
            let name = unsafe { CStr::from_ptr((*entry).d_name.as_ptr()) }.to_bytes();
            if name == b"." || name == b".." {
                continue;
            }
            return Some(Ok(OsString::from_vec(name.to_vec())));
        }
        None
    }
}

impl std::iter::FusedIterator for DirectoryNames {}

impl Drop for DirectoryNames {
    fn drop(&mut self) {
        // SAFETY: this iterator is the sole owner of the live directory stream.
        unsafe { libc::closedir(self.stream.as_ptr()) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_early_drop_closes_the_cloexec_stream_descriptor() {
        let parent = tempfile::tempdir().unwrap();
        std::fs::write(parent.path().join("one"), "contents").unwrap();
        let root = crate::fs_safety::Root::open(parent.path()).unwrap();
        let mut names = crate::fs_safety::directory_names_beneath(&root, std::path::Path::new(""))
            .unwrap();
        // SAFETY: the stream remains exclusively owned and open here.
        let descriptor = unsafe { libc::dirfd(names.stream.as_ptr()) };
        assert!(descriptor >= 0);
        // SAFETY: F_GETFD observes the live descriptor without changing it.
        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
        assert!(names.next().unwrap().is_ok());
        drop(names);
        // No descriptor-creating call intervenes: the original owned stream
        // must have closed its descriptor, rather than leaking on early exit.
        let status = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
        let error = io::Error::last_os_error();
        assert_eq!(status, -1);
        assert_eq!(error.raw_os_error(), Some(libc::EBADF));
    }
}
