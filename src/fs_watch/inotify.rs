//! Linux: one inotify instance, watching each open file through
//! `/proc/self/fd/N`. inotify gives one watch descriptor per inode, so
//! subscribers of the same inode share it.

use super::Event;
use std::{
    ffi::CString,
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd},
};

pub(super) type Id = i32;

/// inotify keeps every watch's bookkeeping in the kernel.
#[derive(Default)]
pub(super) struct Registry;

pub(super) struct Kernel(File);

pub(super) struct Batch {
    buffer: [u8; 4096],
    len: usize,
}

impl Default for Batch {
    fn default() -> Self {
        Self {
            buffer: [0; 4096],
            len: 0,
        }
    }
}

impl Kernel {
    pub(super) fn open() -> io::Result<Self> {
        // SAFETY: a successful call gives us an exclusively owned descriptor.
        let fd = unsafe { libc::inotify_init1(libc::IN_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fd is the successful result of inotify_init1 above.
        Ok(Self(unsafe { File::from_raw_fd(fd) }))
    }

    pub(super) fn add(&self, _: &mut Registry, file: &File) -> Option<Id> {
        let pinned = CString::new(format!("/proc/self/fd/{}", file.as_raw_fd())).ok()?;
        let events = libc::IN_MODIFY | libc::IN_ATTRIB | libc::IN_MOVE_SELF | libc::IN_DELETE_SELF;
        // SAFETY: inotify is owned by self and pinned names the open file.
        let id = unsafe { libc::inotify_add_watch(self.0.as_raw_fd(), pinned.as_ptr(), events) };
        (id >= 0).then_some(id)
    }

    pub(super) fn remove(&self, _: &mut Registry, id: Id) {
        // SAFETY: inotify is live and id came from this instance.
        unsafe { libc::inotify_rm_watch(self.0.as_raw_fd(), id) };
    }

    /// Blocks for the next events; `false` once the instance has ended.
    pub(super) fn wait(&self, batch: &mut Batch) -> io::Result<bool> {
        batch.len = (&self.0).read(&mut batch.buffer)?;
        Ok(batch.len != 0)
    }
}

impl Batch {
    pub(super) fn events(&self, _: &mut Registry, mut each: impl FnMut(Event)) {
        let mut records = &self.buffer[..self.len];
        while let Some(header) = records.first_chunk::<{ size_of::<libc::inotify_event>() }>() {
            // SAFETY: header covers a complete inotify event header of integers.
            let event = unsafe {
                header
                    .as_ptr()
                    .cast::<libc::inotify_event>()
                    .read_unaligned()
            };
            records = &records[header.len() + event.len as usize..];
            each(if event.mask & libc::IN_Q_OVERFLOW != 0 {
                Event::Overflow
            } else if event.mask & libc::IN_IGNORED != 0 {
                // The kernel removed the watch itself.
                Event::Gone(event.wd)
            } else {
                Event::Changed(event.wd)
            });
        }
    }
}
