//! Child exits as a readable descriptor, for a process owner that reaps its
//! children without waking on a timer.
//!
//! `SIGCHLD` is blocked and read from a signalfd. Its default disposition
//! discards it unless every thread blocks it, so a process creates this before
//! starting any other thread; threads started afterwards inherit the mask.
//! Children inherit it too: [`super::adopted_orphans::spawn_owned`] unblocks it
//! for each child it starts, and any other spawn in such a process must as well.
use std::{
    fs::File,
    io::{self, Read},
    os::fd::{AsRawFd, FromRawFd, RawFd},
};

pub(crate) struct ChildEvents {
    fd: File,
    previous: libc::sigset_t,
}

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

impl Drop for ChildEvents {
    fn drop(&mut self) {
        // SAFETY: restores the creating thread's mask. Owners create and drop
        // this on the same thread.
        unsafe {
            libc::pthread_sigmask(libc::SIG_SETMASK, &self.previous, std::ptr::null_mut());
        }
    }
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

    #[test]
    fn refused_while_another_thread_would_receive_child_exits() {
        // The harness's own threads do not block SIGCHLD.
        let (started, ready) = std::sync::mpsc::channel();
        let (finish, finished) = std::sync::mpsc::channel::<()>();
        let other = std::thread::spawn(move || {
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
}
