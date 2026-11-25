//! The kernel handles a sole reaper waits on: the original child, and the
//! executor's cancellation wake.
//!
//! The child's handle becomes readable once that exact child exits, and exit
//! observation never reaps until the executor allows it. On Linux it is a
//! pidfd, read with `waitid(P_PIDFD)`. Darwin has no pidfd, but the executor is
//! the child's sole reaper, so its numeric PID cannot be reused before the
//! final reap: a kqueue watching that PID for `NOTE_EXIT` is the readiness
//! handle, and `waitid(P_PID)` reads the status.

use std::{
    io,
    os::fd::{AsRawFd, FromRawFd, OwnedFd},
    process::ExitStatus,
};

/// Pins `child` for its executor's lifetime. Call before anything can reap it.
pub(crate) fn child_identity(child: &std::process::Child) -> io::Result<OwnedFd> {
    unreaped_child_identity(child.id())
}

/// Pins the caller's own child `pid`, which it alone reaps, for as long as it
/// stays unreaped. Call before anything can reap it; a pid reaped already may
/// name an unrelated process. The handle turns readable once that child
/// exits, and [`child_exit_status`] reads its status.
#[cfg(target_os = "linux")]
pub fn unreaped_child_identity(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: pidfd_open returns a new close-on-exec descriptor for our newly
    // spawned, unreaped child. The guard never recreates it after reaping.
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) } as i32;
    if descriptor < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(unsafe { OwnedFd::from_raw_fd(descriptor) })
    }
}

/// Pins the caller's own child `pid`, which it alone reaps, for as long as it
/// stays unreaped. Call before anything can reap it; a pid reaped already may
/// name an unrelated process. The handle turns readable once that child
/// exits, and [`child_exit_status`] reads its status.
#[cfg(target_os = "macos")]
pub fn unreaped_child_identity(pid: u32) -> io::Result<OwnedFd> {
    // SAFETY: kqueue returns a new descriptor that this function owns.
    let raw = unsafe { libc::kqueue() };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    let queue = unsafe { OwnedFd::from_raw_fd(raw) };
    crate::descriptor::set_close_on_exec(std::os::fd::AsFd::as_fd(&queue))?;
    match register(&queue, pid as usize, libc::EVFILT_PROC, libc::NOTE_EXIT) {
        Ok(()) => Ok(queue),
        // The kernel refuses to watch a child that is already a zombie. It is
        // still unreaped and ours, so confirm the exit, then make the queue
        // permanently ready the way NOTE_EXIT would have.
        Err(error) if error.raw_os_error() == Some(libc::ESRCH) => {
            match exiting_child_status(pid, libc::WEXITED | libc::WNOWAIT)? {
                Some(_) => {
                    register(&queue, 0, libc::EVFILT_USER, libc::NOTE_TRIGGER)?;
                    Ok(queue)
                }
                None => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

#[cfg(target_os = "macos")]
fn register(queue: &OwnedFd, ident: usize, filter: i16, fflags: u32) -> io::Result<()> {
    let change = libc::kevent {
        ident,
        filter,
        flags: libc::EV_ADD | libc::EV_RECEIPT,
        fflags,
        data: 0,
        udata: std::ptr::null_mut(),
    };
    // SAFETY: an all-zero kevent is a valid receipt buffer.
    let mut receipt: libc::kevent = unsafe { std::mem::zeroed() };
    // SAFETY: one change and one receipt slot, both valid for the call; a null
    // timeout is fine because EV_RECEIPT returns without waiting.
    let count = unsafe {
        libc::kevent(
            queue.as_raw_fd(),
            &change,
            1,
            &mut receipt,
            1,
            std::ptr::null(),
        )
    };
    if count < 0 {
        return Err(io::Error::last_os_error());
    }
    if count == 1 && receipt.flags & libc::EV_ERROR != 0 && receipt.data != 0 {
        return Err(io::Error::from_raw_os_error(receipt.data as i32));
    }
    Ok(())
}

/// Reads the original child's status. `flags` always includes `WEXITED`; with
/// `WNOWAIT` the child stays unreaped and its PID reserved.
#[cfg(target_os = "linux")]
pub fn child_exit_status(
    identity: &OwnedFd,
    pid: u32,
    flags: i32,
) -> io::Result<Option<ExitStatus>> {
    let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: P_PIDFD identifies the captured original child, including
        // when its old numeric ID has been reused. All observations use WNOWAIT;
        // only the worker's final call reaps after group signals are disarmed.
        if unsafe {
            libc::waitid(
                libc::P_PIDFD,
                identity.as_raw_fd() as libc::id_t,
                &mut information,
                flags,
            )
        } == 0
        {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    let (observed, status) = unsafe { (information.si_pid(), information.si_status()) };
    decode(observed, information.si_code, status, pid)
}

/// Reads the original child's status. `flags` always includes `WEXITED`; with
/// `WNOWAIT` the child stays unreaped and its PID reserved.
#[cfg(target_os = "macos")]
pub fn child_exit_status(
    identity: &OwnedFd,
    pid: u32,
    flags: i32,
) -> io::Result<Option<ExitStatus>> {
    // Darwin's waitid also reports a stopped child under WEXITED alone, so a
    // blocking waitid would return at once for a stopped child. Sleep on the
    // exit readiness instead, then read the status without blocking.
    if flags & libc::WNOHANG == 0 {
        wait_readable(identity)?;
        return exiting_child_status(pid, flags);
    }
    waited_status(pid, flags)
}

/// The status of a child whose exit the kernel has already announced, by
/// NOTE_EXIT or by refusing to watch it. XNU announces the exit while tearing
/// the process down, and only reports it to waitid once the child is a zombie,
/// so allow that transition a moment before concluding there is no status.
#[cfg(target_os = "macos")]
fn exiting_child_status(pid: u32, flags: i32) -> io::Result<Option<ExitStatus>> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(1);
    loop {
        let status = waited_status(pid, flags)?;
        if status.is_some() || std::time::Instant::now() >= deadline {
            return Ok(status);
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
}

/// One waitid that never blocks; see [`child_exit_status`] for `flags`.
#[cfg(target_os = "macos")]
fn waited_status(pid: u32, flags: i32) -> io::Result<Option<ExitStatus>> {
    let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
    loop {
        // SAFETY: this executor is the child's sole reaper, so the PID still
        // names the original child until a call without WNOWAIT reaps it.
        if unsafe {
            libc::waitid(
                libc::P_PID,
                pid as libc::id_t,
                &mut information,
                flags | libc::WNOHANG,
            )
        } == 0
        {
            break;
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    match information.si_code {
        libc::CLD_STOPPED | libc::CLD_TRAPPED | libc::CLD_CONTINUED => Ok(None),
        code => decode(information.si_pid, code, information.si_status, pid),
    }
}

#[cfg(target_os = "macos")]
fn wait_readable(descriptor: &OwnedFd) -> io::Result<()> {
    let mut watched = libc::pollfd {
        fd: descriptor.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    loop {
        // SAFETY: one owned descriptor, valid for the call.
        if unsafe { libc::poll(&mut watched, 1, -1) } >= 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

fn decode(observed: i32, code: i32, status: i32, pid: u32) -> io::Result<Option<ExitStatus>> {
    use std::os::unix::process::ExitStatusExt;
    if observed == 0 {
        return Ok(None);
    }
    if observed as u32 != pid {
        return Err(io::Error::other(
            "child exit observation returned another process",
        ));
    }
    let raw = match code {
        libc::CLD_EXITED => status << 8,
        libc::CLD_KILLED => status,
        libc::CLD_DUMPED => status | 0x80,
        _ => {
            return Err(io::Error::other(
                "child exit observation returned an unexpected status",
            ));
        }
    };
    Ok(Some(ExitStatus::from_raw(raw)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        os::unix::process::ExitStatusExt,
        process::{Command, Stdio},
        time::{Duration, Instant},
    };

    /// Waits, without reaping, until `pid` reports a state `flags` selects.
    fn await_state(pid: u32, flags: i32) -> libc::siginfo_t {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            // SAFETY: an all-zero siginfo is a valid output buffer.
            let mut information: libc::siginfo_t = unsafe { std::mem::zeroed() };
            // SAFETY: the test owns this unreaped child; WNOWAIT keeps it so.
            let status = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as libc::id_t,
                    &mut information,
                    flags | libc::WNOWAIT | libc::WNOHANG,
                )
            };
            assert_eq!(status, 0, "{}", io::Error::last_os_error());
            #[cfg(target_os = "linux")]
            let observed = unsafe { information.si_pid() };
            #[cfg(target_os = "macos")]
            let observed = information.si_pid;
            if observed != 0 {
                return information;
            }
            assert!(Instant::now() < deadline, "child never changed state");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn ready(identity: &OwnedFd, timeout: Duration) -> bool {
        let mut watched = libc::pollfd {
            fd: identity.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let milliseconds = timeout.as_millis() as i32;
        // SAFETY: one owned descriptor, valid for the call.
        unsafe { libc::poll(&mut watched, 1, milliseconds) == 1 }
    }

    #[test]
    fn a_child_that_exited_before_it_was_pinned_is_ready_and_stays_unreaped() {
        #[expect(
            clippy::zombie_processes,
            reason = "the identity under test reaps the child"
        )]
        let child = Command::new("sh")
            .args(["-c", "exit 7"])
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        await_state(pid, libc::WEXITED);
        let identity = child_identity(&child).unwrap();
        assert!(ready(&identity, Duration::ZERO));
        let observe = libc::WEXITED | libc::WNOWAIT | libc::WNOHANG;
        for _ in 0..2 {
            let status = child_exit_status(&identity, pid, observe).unwrap().unwrap();
            assert_eq!(status.code(), Some(7));
        }
        let reaped = child_exit_status(&identity, pid, libc::WEXITED).unwrap();
        assert_eq!(reaped.and_then(|status| status.code()), Some(7));
    }

    #[test]
    fn a_stopped_child_has_not_exited() {
        #[expect(
            clippy::zombie_processes,
            reason = "the identity under test reaps the child"
        )]
        let child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .spawn()
            .unwrap();
        let pid = child.id();
        let identity = child_identity(&child).unwrap();
        // SAFETY: the test owns this unreaped child.
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGSTOP) }, 0);
        await_state(pid, libc::WSTOPPED);
        let observe = libc::WEXITED | libc::WNOWAIT | libc::WNOHANG;
        assert!(
            child_exit_status(&identity, pid, observe)
                .unwrap()
                .is_none()
        );
        assert!(!ready(&identity, Duration::from_millis(50)));
        // SAFETY: as above.
        assert_eq!(unsafe { libc::kill(pid as i32, libc::SIGKILL) }, 0);
        let status = child_exit_status(&identity, pid, libc::WEXITED | libc::WNOWAIT)
            .unwrap()
            .unwrap();
        assert_eq!(status.signal(), Some(libc::SIGKILL));
        assert!(ready(&identity, Duration::ZERO));
        assert!(
            child_exit_status(&identity, pid, libc::WEXITED)
                .unwrap()
                .is_some()
        );
    }
}
