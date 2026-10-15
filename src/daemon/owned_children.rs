//! Descendant cleanup for dedicated process owners, never the daemon itself.
use std::{
    io,
    process::Child,
    time::{Duration, Instant},
};

/// Used only in the dedicated, single-threaded subreaper process. It owns every
/// child of that process; this cleanup must never run inside the daemon.
pub(crate) struct OwnedChildren {
    child: Option<Child>,
    group_signalled: bool,
}

impl OwnedChildren {
    /// The caller must be the sole child reaper in a dedicated subreaper process.
    /// No other thread or signal handler may reap this child or descendants.
    pub(crate) fn new(child: Child) -> Self {
        Self {
            child: Some(child),
            group_signalled: false,
        }
    }

    pub(crate) fn id(&self) -> i32 {
        self.child.as_ref().expect("owned provider").id() as i32
    }

    pub(crate) fn take_stderr(&mut self) -> Option<std::process::ChildStderr> {
        self.child.as_mut()?.stderr.take()
    }

    pub(crate) fn exit_status(&self) -> io::Result<Option<std::process::ExitStatus>> {
        use std::os::unix::process::ExitStatusExt;
        let Some(child) = &self.child else { return Ok(None); };
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        if unsafe { libc::waitid(libc::P_PID, child.id(), &mut status, libc::WEXITED | libc::WNOHANG | libc::WNOWAIT) } == -1 {
            return Err(io::Error::last_os_error());
        }
        if unsafe { status.si_pid() } == 0 { return Ok(None); }
        let code = unsafe { status.si_status() };
        let raw = match status.si_code {
            libc::CLD_EXITED => code << 8,
            libc::CLD_KILLED => code,
            libc::CLD_DUMPED => code | 0x80,
            _ => return Err(io::Error::other("Unexpected owned provider exit status")),
        };
        Ok(Some(std::process::ExitStatus::from_raw(raw)))
    }

    pub(crate) fn exited(&self) -> bool {
        let Some(child) = &self.child else {
            return true;
        };
        let mut status: libc::siginfo_t = unsafe { std::mem::zeroed() };
        unsafe {
            libc::waitid(
                libc::P_PID,
                child.id(),
                &mut status,
                libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
            ) == 0
                && status.si_pid() != 0
        }
    }

    /// Reap adopted tools without consuming the provider leader's exit status.
    /// Keeping that leader unreaped also reserves its process-group identifier
    /// until `terminate` has sent the group's final signal.
    pub(crate) fn reap_exited_descendants(&self) -> io::Result<()> {
        for pid in direct_children()? {
            if self
                .child
                .as_ref()
                .is_some_and(|child| child.id() == pid as u32)
            {
                continue;
            }
            loop {
                // This dedicated owner is the only reaper. Live children are
                // untouched, and an exited child's PID cannot be reused before
                // this wait consumes its status.
                match unsafe { libc::waitpid(pid, std::ptr::null_mut(), libc::WNOHANG) } {
                    result if result >= 0 => break,
                    _ => {
                        let error = io::Error::last_os_error();
                        match error.raw_os_error() {
                            Some(libc::EINTR) => continue,
                            Some(libc::ECHILD) => break,
                            _ => return Err(error),
                        }
                    }
                }
            }
        }
        Ok(())
    }

    pub(crate) fn terminate(&mut self, timeout: Duration) -> io::Result<bool> {
        if !self.group_signalled {
            // The group leader is still our unreaped child, so this number
            // cannot have been recycled. Never use it again after reaping.
            unsafe {
                libc::kill(-self.id(), libc::SIGKILL);
            }
            self.group_signalled = true;
        }
        let deadline = Instant::now() + timeout;
        loop {
            for pid in direct_children()? {
                // No other thread or handler reaps children in this process.
                // A child that exits stays a zombie until the loop below.
                if unsafe { libc::kill(pid, libc::SIGKILL) } == -1
                    && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                {
                    return Err(io::Error::last_os_error());
                }
            }
            loop {
                let mut status = 0;
                match unsafe { libc::waitpid(-1, &mut status, libc::WNOHANG) } {
                    pid if pid > 0 => {
                        if self
                            .child
                            .as_ref()
                            .is_some_and(|child| child.id() == pid as u32)
                        {
                            self.child.take();
                        }
                    }
                    0 => break,
                    _ if io::Error::last_os_error().raw_os_error() == Some(libc::ECHILD) => {
                        return Ok(true);
                    }
                    _ if io::Error::last_os_error().kind() == io::ErrorKind::Interrupted => {
                        continue;
                    }
                    _ => return Err(io::Error::last_os_error()),
                }
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

fn direct_children() -> io::Result<std::collections::BTreeSet<i32>> {
    let mut children = std::collections::BTreeSet::new();
    for task in std::fs::read_dir("/proc/self/task")? {
        let path = task?.path().join("children");
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) => raw,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        children.extend(
            raw.split_whitespace()
                .filter_map(|id| id.parse::<i32>().ok()),
        );
    }
    Ok(children)
}

/// Proof available only after the dedicated sole reaper has no children.
/// A root exit or an empty process census cannot construct this receipt.
pub(crate) struct ReapedFamily(());

/// Kill/reap children of a dedicated single-threaded subreaper. Never call in
/// the daemon. Orphans of killed children become this owner's children, so
/// repeated pinned direct-child passes cover detached sessions without PGIDs.
pub(crate) fn terminate_owned_family(deadline: Instant) -> io::Result<Option<ReapedFamily>> {
    use std::os::fd::{AsRawFd, FromRawFd};
    let mut subreaper = 0i32;
    if unsafe {libc::prctl(libc::PR_GET_CHILD_SUBREAPER, &raw mut subreaper, 0, 0, 0)} != 0 {
        return Err(io::Error::last_os_error());
    }
    if subreaper != 1 || std::fs::read_dir("/proc/self/task")?.collect::<io::Result<Vec<_>>>()?.len() != 1 {
        return Err(io::Error::new(io::ErrorKind::PermissionDenied,"Family cleanup requires its dedicated single-threaded subreaper"));
    }
    loop {
        for pid in direct_children()? {
            if Instant::now() >= deadline { return Ok(None); }
            let descriptor = unsafe {libc::syscall(libc::SYS_pidfd_open, pid, 0)};
            if descriptor < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() == Some(libc::ESRCH) {continue;}
                return Err(error);
            }
            let descriptor = unsafe {std::fs::File::from_raw_fd(descriptor as i32)};
            // This helper is the only reaper. The child cannot recycle its PID
            // between the kernel's children snapshot and opening its pidfd.
            if unsafe {libc::syscall(libc::SYS_pidfd_send_signal, descriptor.as_raw_fd(), libc::SIGKILL, std::ptr::null::<libc::siginfo_t>(), 0)} < 0 {
                let error = io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::ESRCH) {return Err(error);}
            }
        }
        loop {
            match unsafe {libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG)} {
                pid if pid > 0 => continue,
                0 => break,
                _ => {
                    let error = io::Error::last_os_error();
                    match error.raw_os_error() {
                        Some(libc::ECHILD) => return Ok(Some(ReapedFamily(()))),
                        Some(libc::EINTR) => continue,
                        _ => return Err(error),
                    }
                }
            }
        }
        if Instant::now() >= deadline {return Ok(None);}
        std::thread::sleep(Duration::from_millis(5));
    }
}


/// Test-owned signal authority, captured before lifecycle/failure scheduling.
/// A recorded PID/start alone never authorizes a later numeric signal. This
/// pin establishes identity from capture onward, not historical child reaping.
#[cfg(all(test, feature = "host"))]
pub(crate) struct FixtureProcessPin {
    file: std::fs::File,
    expected: super::holder_registry::Process,
}
#[cfg(all(test, feature = "host"))]
impl FixtureProcessPin {
    pub(crate) fn capture(expected: &super::holder_registry::Process) -> io::Result<Self> {
        use std::os::fd::FromRawFd;
        // SAFETY: pidfd_open returns a new owned descriptor, or -1. Opening
        // comes BEFORE rereading the expected incarnation, so PID reuse across
        // capture cannot authorize a different kernel process.
        let descriptor = unsafe {libc::syscall(libc::SYS_pidfd_open, expected.pid, 0)};
        if descriptor < 0 {return Err(io::Error::last_os_error());}
        // SAFETY: successful pidfd_open returned a unique owned descriptor.
        let file = unsafe {std::fs::File::from_raw_fd(descriptor as i32)};
        let current = super::holder_registry::Process::read(expected.pid)?;
        if current != *expected {return Err(io::Error::other("fixture process incarnation changed before pin capture"));}
        Ok(Self {file, expected:expected.clone()})
    }
    pub(crate) fn process(&self) -> &super::holder_registry::Process { &self.expected }
    pub(crate) fn signal(&self, signal: i32) -> io::Result<()> {
        use std::os::fd::AsRawFd;
        // SAFETY: this retained pidfd names only the positively verified
        // original incarnation. ESRCH is absence, never a numeric fallback.
        let result = unsafe {libc::syscall(libc::SYS_pidfd_send_signal, self.file.as_raw_fd(), signal,
            std::ptr::null::<libc::siginfo_t>(), 0)};
        if result < 0 {return Err(io::Error::last_os_error());}
        Ok(())
    }
}
#[cfg(all(test, feature = "host"))]
mod fixture_pin_tests {
    use super::*;
    #[test]
    fn process_pin_refuses_mismatched_start_and_retains_cloexec_identity() {
        use std::os::fd::AsRawFd;
        let current = super::super::holder_registry::Process::read(std::process::id() as i32).unwrap();
        let mut wrong = current.clone(); wrong.start = wrong.start.wrapping_add(1);
        assert!(FixtureProcessPin::capture(&wrong).is_err());
        let pin = FixtureProcessPin::capture(&current).unwrap();
        assert_eq!(pin.process(), &current);
        // Null signal checks this owned self-pin without affecting a process.
        pin.signal(0).unwrap();
        let flags = unsafe {libc::fcntl(pin.file.as_raw_fd(), libc::F_GETFD)};
        assert!(flags >= 0 && flags & libc::FD_CLOEXEC != 0);
    }
}
