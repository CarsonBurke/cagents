//! Child exits as a readable descriptor, for a process owner that reaps its
//! children without waking on a timer.
//!
//! On Linux, `SIGCHLD` is blocked and read from a signalfd. Its default
//! disposition discards it unless every thread blocks it, so a process creates
//! this before starting any other thread; threads started afterwards inherit
//! the mask. Children inherit it too: [`super::adopted_orphans::spawn_owned`]
//! unblocks it for each child it starts, and any other spawn in such a process
//! must as well.
//!
//! On macOS, a kqueue `EVFILT_SIGNAL` filter records `SIGCHLD` as it is
//! generated for the process, before its disposition or any thread's mask is
//! consulted, so nothing is blocked and no thread can take an exit from it.
//! It needs only that the process keeps `SIGCHLD` from being suppressed:
//! `SIG_IGN` or `SA_NOCLDWAIT` makes the kernel reap children itself and send
//! no signal at all. The default disposition, which discards the signal, or a
//! handler both leave it recorded.
use std::io;
#[cfg(target_os = "macos")]
use std::os::fd::{AsFd, AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(target_os = "linux")]
use std::{
    fs::File,
    io::Read,
    os::fd::{AsRawFd, FromRawFd, RawFd},
};

#[cfg(target_os = "linux")]
pub(crate) struct ChildEvents {
    fd: File,
    previous: libc::sigset_t,
}

#[cfg(target_os = "macos")]
pub(crate) struct ChildEvents {
    /// A kqueue holding only the `SIGCHLD` filter.
    queue: OwnedFd,
}

#[cfg(target_os = "linux")]
impl ChildEvents {
    /// Blocks `SIGCHLD` in the calling thread and reads it from a descriptor.
    /// Refused when another thread of this process does not block it, since
    /// a child exit delivered there would be lost.
    pub(crate) fn new() -> io::Result<Self> {
        let mask = sigchld();
        // SAFETY: both signal sets are live, initialized objects.
        let mut previous = unsafe { std::mem::zeroed() };
        let result = unsafe { libc::pthread_sigmask(libc::SIG_BLOCK, &mask, &mut previous) };
        if result != 0 {
            return Err(io::Error::from_raw_os_error(result));
        }
        let restore = |error| {
            // SAFETY: restores this thread's own previous mask.
            unsafe { libc::pthread_sigmask(libc::SIG_SETMASK, &previous, std::ptr::null_mut()) };
            Err(error)
        };
        if let Err(error) = every_thread_blocks_sigchld() {
            return restore(error);
        }
        // SAFETY: the mask is initialized; the returned descriptor is new.
        let fd = unsafe { libc::signalfd(-1, &mask, libc::SFD_CLOEXEC | libc::SFD_NONBLOCK) };
        if fd < 0 {
            return restore(io::Error::last_os_error());
        }
        Ok(Self {
            // SAFETY: signalfd returned a descriptor owned only here.
            fd: unsafe { File::from_raw_fd(fd) },
            previous,
        })
    }

    /// Readable once a child has exited since the last [`Self::drain`].
    pub(crate) fn fd(&self) -> RawFd {
        self.fd.as_raw_fd()
    }

    /// Consumes pending notifications. Exits coalesce, so a caller reaps
    /// every exited child it may, not one per notification.
    pub(crate) fn drain(&self) -> io::Result<()> {
        let mut event = [0_u8; std::mem::size_of::<libc::signalfd_siginfo>()];
        loop {
            match (&self.fd).read(&mut event) {
                Ok(0) => return Ok(()),
                Ok(_) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for ChildEvents {
    fn drop(&mut self) {
        // SAFETY: restores the creating thread's mask. Owners create and drop
        // this on the same thread.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
        }
    }
}

#[cfg(target_os = "macos")]
impl ChildEvents {
    /// Records `SIGCHLD` in a kqueue, leaving every thread's mask unchanged.
    /// Refused while this process suppresses the signal, since no child exit
    /// would then be reported.
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: an all-zero sigaction is a valid value to overwrite.
        let mut action = unsafe { std::mem::zeroed() };
        // SAFETY: a null new action only queries the current one into a live
        // object.
        if unsafe { libc::sigaction(libc::SIGCHLD, std::ptr::null(), &mut action) } != 0 {
            return Err(io::Error::last_os_error());
        }
        reports_child_exits(&action)?;
        // SAFETY: kqueue takes no arguments and returns a new descriptor.
        let queue = unsafe { libc::kqueue() };
        if queue < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: kqueue returned a descriptor owned only here.
        let queue = unsafe { OwnedFd::from_raw_fd(queue) };
        // A fork does not inherit a kqueue, but an exec of this process would.
        crate::descriptor::set_close_on_exec(queue.as_fd())?;
        let filter = libc::kevent {
            ident: libc::SIGCHLD as libc::uintptr_t,
            filter: libc::EVFILT_SIGNAL,
            // The kernel adds EV_CLEAR: collecting the event resets it.
            flags: libc::EV_ADD,
            fflags: 0,
            data: 0,
            udata: std::ptr::null_mut(),
        };
        // SAFETY: the change list is one initialized kevent and the event
        // list is empty, so the kernel writes nothing back.
        if unsafe {
            libc::kevent(
                queue.as_raw_fd(),
                &filter,
                1,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        } < 0
        {
            return Err(io::Error::last_os_error());
        }
        Ok(Self { queue })
    }

    /// Readable once a child has exited since the last [`Self::drain`].
    pub(crate) fn fd(&self) -> RawFd {
        self.queue.as_raw_fd()
    }

    /// Consumes pending notifications. Exits coalesce into one event that
    /// counts them, so a caller reaps every exited child it may, not one per
    /// notification.
    pub(crate) fn drain(&self) -> io::Result<()> {
        let mut event = std::mem::MaybeUninit::<libc::kevent>::uninit();
        let immediately = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        loop {
            // SAFETY: the event list is one writable kevent and the timeout
            // is initialized; the queue is owned by `self`. The filter is
            // the queue's only one, so one collection empties it.
            if unsafe {
                libc::kevent(
                    self.queue.as_raw_fd(),
                    std::ptr::null(),
                    0,
                    event.as_mut_ptr(),
                    1,
                    &immediately,
                )
            } >= 0
            {
                return Ok(());
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
}

/// Whether `action`, this process's `SIGCHLD` disposition, lets the kernel
/// send the signal at all: ignoring it, or asking not to wait for children,
/// hands exited children to the kernel to reap without a signal.
#[cfg(target_os = "macos")]
fn reports_child_exits(action: &libc::sigaction) -> io::Result<()> {
    if action.sa_sigaction == libc::SIG_IGN || action.sa_flags & libc::SA_NOCLDWAIT != 0 {
        return Err(io::Error::other(
            "SIGCHLD is ignored, so child exits would go unreported",
        ));
    }
    Ok(())
}

fn sigchld() -> libc::sigset_t {
    // SAFETY: the set is initialized by sigemptyset before use.
    unsafe {
        let mut mask = std::mem::zeroed();
        libc::sigemptyset(&mut mask);
        libc::sigaddset(&mut mask, libc::SIGCHLD);
        mask
    }
}

/// Undo, in a child between fork and exec, the mask [`ChildEvents`] set, so
/// the program it runs receives its own children's exits. Async-signal-safe.
pub(crate) fn unblock_sigchld_for_exec() -> io::Result<()> {
    let mask = sigchld();
    // SAFETY: sigprocmask is async-signal-safe and the mask is initialized.
    if unsafe { libc::sigprocmask(libc::SIG_UNBLOCK, &mask, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// For a test fixture process that creates [`ChildEvents`] beside libtest's
/// own threads: every thread of the program `exec` starts inherits the block.
#[cfg(test)]
pub(crate) fn block_sigchld_for_exec() -> io::Result<()> {
    let mask = sigchld();
    // SAFETY: sigprocmask is async-signal-safe and the mask is initialized.
    if unsafe { libc::sigprocmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut()) } != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Whether the calling thread blocks `SIGCHLD`, so children it starts would
/// inherit the block.
pub(crate) fn calling_thread_blocks_sigchld() -> bool {
    // SAFETY: a null new set only queries this thread's mask.
    unsafe {
        let mut current = std::mem::zeroed();
        libc::pthread_sigmask(libc::SIG_BLOCK, std::ptr::null(), &mut current) == 0
            && libc::sigismember(&current, libc::SIGCHLD) == 1
    }
}

#[cfg(target_os = "linux")]
fn every_thread_blocks_sigchld() -> io::Result<()> {
    let bit = 1_u64 << (libc::SIGCHLD - 1);
    for task in std::fs::read_dir("/proc/self/task")? {
        let status = match std::fs::read_to_string(task?.path().join("status")) {
            Ok(status) => status,
            // The thread exited meanwhile.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        let blocked = status
            .lines()
            .find_map(|line| line.strip_prefix("SigBlk:"))
            .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
            .ok_or_else(|| io::Error::other("thread signal mask is unreadable"))?;
        if blocked & bit == 0 {
            return Err(io::Error::other(
                "another thread receives SIGCHLD, so a descriptor would miss child exits",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn refused_while_another_thread_would_receive_child_exits() {
        // The harness's own threads do not block SIGCHLD.
        let (started, ready) = std::sync::mpsc::channel();
        let (finish, finished) = std::sync::mpsc::channel::<()>();
        let other = std::thread::spawn(move || {
            // Whatever mask the harness inherited, this thread receives it.
            unblock_sigchld_for_exec().unwrap();
            started.send(()).unwrap();
            let _ = finished.recv();
        });
        ready.recv().unwrap();
        let refused = std::thread::spawn(|| {
            let result = ChildEvents::new().map(drop);
            (result, calling_thread_blocks_sigchld())
        })
        .join()
        .unwrap();
        finish.send(()).unwrap();
        other.join().unwrap();
        assert!(refused.0.is_err());
        assert!(!refused.1, "a refusal restores the caller's mask");
    }

    /// libtest's threads leave `SIGCHLD` unblocked, which the kqueue filter
    /// does not need.
    #[cfg(target_os = "macos")]
    #[test]
    fn a_child_exit_is_readable_while_every_thread_receives_sigchld() {
        let events = ChildEvents::new().unwrap();
        assert!(!calling_thread_blocks_sigchld());
        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        // The kernel posts SIGCHLD before it wakes the waiting parent.
        child.wait().unwrap();
        let mut ready = libc::pollfd {
            fd: events.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one live pollfd naming the queue `events` owns.
        assert_eq!(unsafe { libc::poll(&mut ready, 1, 0) }, 1);
        events.drain().unwrap();
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn refused_while_sigchld_is_suppressed() {
        // SAFETY: an all-zero sigaction is the default disposition.
        let default: libc::sigaction = unsafe { std::mem::zeroed() };
        reports_child_exits(&default).unwrap();
        extern "C" fn handler(_: libc::c_int) {}
        let handled = libc::sigaction {
            sa_sigaction: handler as extern "C" fn(libc::c_int) as libc::sighandler_t,
            ..default
        };
        reports_child_exits(&handled).unwrap();
        let ignored = libc::sigaction {
            sa_sigaction: libc::SIG_IGN,
            ..default
        };
        assert!(reports_child_exits(&ignored).is_err());
        let not_waited = libc::sigaction {
            sa_flags: libc::SA_NOCLDWAIT,
            ..default
        };
        assert!(reports_child_exits(&not_waited).is_err());
    }
}
