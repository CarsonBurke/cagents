//! One blocking wait for courier edges and absolute Unix deadlines.

use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
};

pub(super) struct Wait {
    edge: OwnedFd,
    deadline: OwnedFd,
}

impl Wait {
    pub(super) fn new() -> io::Result<Self> {
        // SAFETY: Each call returns a fresh descriptor or -1. Neither takes
        // pointers, and both descriptors are private to the courier.
        let edge = owned(unsafe { libc::eventfd(0, libc::EFD_CLOEXEC | libc::EFD_NONBLOCK) })?;
        let deadline =
            owned(unsafe { libc::timerfd_create(libc::CLOCK_REALTIME, libc::TFD_CLOEXEC) })?;
        Ok(Self { edge, deadline })
    }

    pub(super) fn wake(&self) {
        // SAFETY: the eventfd is live. An already full counter stays readable.
        while unsafe { libc::eventfd_write(self.edge.as_raw_fd(), 1) } < 0
            && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
        {}
    }

    /// Drains coalesced edges under the courier's state lock before taking
    /// the generation for the next scan. A later edge stays readable.
    pub(super) fn begin_pass(&self) {
        let mut count = 0;
        // SAFETY: the eventfd is nonblocking and `count` can hold its value.
        while unsafe { libc::eventfd_read(self.edge.as_raw_fd(), &mut count) } < 0
            && io::Error::last_os_error().kind() == io::ErrorKind::Interrupted
        {}
    }

    pub(super) fn until(&self, unix_ms: u64) -> io::Result<()> {
        let deadline = libc::itimerspec {
            it_interval: libc::timespec {
                tv_sec: 0,
                tv_nsec: 0,
            },
            it_value: libc::timespec {
                tv_sec: (unix_ms / 1000).try_into().unwrap_or(libc::time_t::MAX),
                tv_nsec: ((unix_ms % 1000) * 1_000_000) as libc::c_long,
            },
        };
        // An absolute realtime deadline is already due on resume if it
        // elapsed during suspend. A discontinuous clock change also wakes
        // the scan, which recomputes the deadlines from the new time.
        // SAFETY: the descriptor is live and `deadline` is initialized.
        let armed = unsafe {
            libc::timerfd_settime(
                self.deadline.as_raw_fd(),
                libc::TFD_TIMER_ABSTIME | libc::TFD_TIMER_CANCEL_ON_SET,
                &deadline,
                std::ptr::null_mut(),
            )
        };
        if armed < 0 {
            let error = io::Error::last_os_error();
            // Setting the new deadline can report that the previous one
            // was cancelled. The caller should recompute after that change.
            return if error.raw_os_error() == Some(libc::ECANCELED) {
                Ok(())
            } else {
                Err(error)
            };
        }
        let mut descriptors = [
            libc::pollfd {
                fd: self.edge.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: self.deadline.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        loop {
            // SAFETY: poll borrows both live descriptors and writes only
            // into the supplied initialized array.
            if unsafe {
                libc::poll(
                    descriptors.as_mut_ptr(),
                    descriptors.len() as libc::nfds_t,
                    -1,
                )
            } > 0
            {
                if descriptors
                    .iter()
                    .any(|descriptor| descriptor.revents & libc::POLLIN != 0)
                {
                    return Ok(());
                }
                return Err(io::Error::other(
                    "courier wait descriptor became unavailable",
                ));
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

fn owned(fd: libc::c_int) -> io::Result<OwnedFd> {
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the call produced a fresh descriptor and transfers it here.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}
