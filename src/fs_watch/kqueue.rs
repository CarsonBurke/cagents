//! Darwin: one kqueue, with an `EVFILT_VNODE` filter on a duplicate of each
//! watched file's open descriptor and the watch's id as its `udata`.
//!
//! kqueue binds a filter to a descriptor, not to an inode, and removes it when
//! that descriptor closes. So the registry keeps one duplicate per watched
//! inode, shared by every subscriber of that inode as an inotify watch
//! descriptor is on Linux. Holding the descriptor keeps a deleted inode's
//! storage until its last subscriber drops the watch, which subscribers do on
//! the wake that deletion raises, and keeps the volume busy against an
//! unforced unmount while it is watched.
//!
//! Darwin reports every unlink as `NOTE_DELETE`, including a rename over the
//! file and the removal of one of several hard links. inotify reports
//! `IN_DELETE_SELF` and drops the watch only once the last link is gone, so a
//! deletion here drops the watch only when the held file has no links left.

use super::Event;
use std::{
    collections::HashMap,
    fs::File,
    io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::fs::MetadataExt,
    },
};

/// Never reused, so an event still queued for a removed watch names no
/// other watch.
pub(super) type Id = usize;

const CHANGES: u32 = libc::NOTE_WRITE
    | libc::NOTE_EXTEND
    | libc::NOTE_ATTRIB
    | libc::NOTE_RENAME
    | libc::NOTE_DELETE
    | libc::NOTE_REVOKE;

struct Vnode {
    /// The descriptor the filter is attached to.
    file: File,
    inode: (u64, u64),
}

#[derive(Default)]
pub(super) struct Registry {
    vnodes: HashMap<Id, Vnode>,
    inodes: HashMap<(u64, u64), Id>,
    next: Id,
}

impl Registry {
    /// Closing the descriptor removes its filter and any queued events.
    fn forget(&mut self, id: Id) {
        if let Some(vnode) = self.vnodes.remove(&id) {
            self.inodes.remove(&vnode.inode);
        }
    }

    /// Whether the watched inode has lost its last link. An unreadable
    /// status counts as unlinked, so the subscriber watches again.
    fn unlinked(&self, id: Id) -> bool {
        self.vnodes
            .get(&id)
            .is_none_or(|vnode| vnode.file.metadata().map_or(true, |m| m.nlink() == 0))
    }
}

pub(super) struct Kernel(OwnedFd);

pub(super) struct Batch {
    events: [libc::kevent; 64],
    len: usize,
}

impl Default for Batch {
    fn default() -> Self {
        Self {
            // SAFETY: an all-zero `kevent` is a valid output record.
            events: unsafe { std::mem::zeroed() },
            len: 0,
        }
    }
}

impl Kernel {
    pub(super) fn open() -> io::Result<Self> {
        // SAFETY: kqueue takes no arguments and returns a new owned
        // descriptor or -1.
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: a successful kqueue returned a fresh descriptor.
        let queue = unsafe { OwnedFd::from_raw_fd(queue) };
        // kqueue descriptors are not inherited across fork, but mark it
        // close-on-exec as well so no exec'd child sees it.
        crate::descriptor::set_close_on_exec(std::os::fd::AsFd::as_fd(&queue))?;
        Ok(Self(queue))
    }

    pub(super) fn add(&self, registry: &mut Registry, file: &File) -> Option<Id> {
        let metadata = file.metadata().ok()?;
        let inode = (metadata.dev(), metadata.ino());
        if let Some(&id) = registry.inodes.get(&inode) {
            return Some(id);
        }
        // A close-on-exec duplicate the caller's close cannot end.
        let held = file.try_clone().ok()?;
        let id = registry.next;
        let change = libc::kevent {
            ident: held.as_raw_fd() as libc::uintptr_t,
            filter: libc::EVFILT_VNODE,
            flags: libc::EV_ADD | libc::EV_CLEAR | libc::EV_RECEIPT,
            fflags: CHANGES,
            data: 0,
            udata: std::ptr::without_provenance_mut(id),
        };
        // SAFETY: an all-zero `kevent` is a valid output record.
        let mut receipt: libc::kevent = unsafe { std::mem::zeroed() };
        // SAFETY: one input and one output record, both live for the call,
        // and a null timeout is not consulted for an EV_RECEIPT change.
        let count = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                &change,
                1,
                &mut receipt,
                1,
                std::ptr::null(),
            )
        };
        // EV_RECEIPT reports the registration's own result in `data`.
        if count < 0 || (count == 1 && receipt.flags & libc::EV_ERROR != 0 && receipt.data != 0) {
            return None;
        }
        registry.next += 1;
        registry.vnodes.insert(id, Vnode { file: held, inode });
        registry.inodes.insert(inode, id);
        Some(id)
    }

    pub(super) fn remove(&self, registry: &mut Registry, id: Id) {
        registry.forget(id);
    }

    /// Blocks for the next events. A kqueue never ends on its own.
    pub(super) fn wait(&self, batch: &mut Batch) -> io::Result<bool> {
        // SAFETY: the output records are live and sized for the call, which
        // writes at most that many; no changes are passed, and a null
        // timeout blocks until an event arrives.
        let count = unsafe {
            libc::kevent(
                self.0.as_raw_fd(),
                std::ptr::null(),
                0,
                batch.events.as_mut_ptr(),
                batch.events.len() as libc::c_int,
                std::ptr::null(),
            )
        };
        batch.len = usize::try_from(count).map_err(|_| io::Error::last_os_error())?;
        Ok(true)
    }
}

impl Batch {
    pub(super) fn events(&self, registry: &mut Registry, mut each: impl FnMut(Event)) {
        for event in &self.events[..self.len] {
            let id = event.udata.addr();
            // A revoked or failed filter is already gone; a deletion ends the
            // watch once no link is left, as on Linux.
            let gone = event.flags & (libc::EV_EOF | libc::EV_ERROR) != 0
                || event.fflags & libc::NOTE_REVOKE != 0
                || (event.fflags & libc::NOTE_DELETE != 0 && registry.unlinked(id));
            each(if gone {
                registry.forget(id);
                Event::Gone(id)
            } else {
                Event::Changed(id)
            });
        }
    }
}
