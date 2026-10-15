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
            if Instant::now() >= deadline {
                return Ok(false);
            }
            for pid in direct_children()? {
                if Instant::now() >= deadline {
                    return Ok(false);
                }
                // No other thread or handler reaps children in this process.
                // A child that exits stays a zombie until the loop below.
                if unsafe { libc::kill(pid, libc::SIGKILL) } == -1
                    && io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
                {
                    return Err(io::Error::last_os_error());
                }
            }
            match drain_children_until(deadline, |pid| {
                if self.child.as_ref().is_some_and(|child| child.id() == pid as u32) {
                    self.child.take();
                }
            })? {
                ChildDrain::Empty => return Ok(true),
                ChildDrain::Expired => return Ok(false),
                ChildDrain::Pending => {}
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Ok(false);
            };
            std::thread::sleep(remaining.min(Duration::from_millis(10)));
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
        if Instant::now() >= deadline {
            return Ok(None);
        }
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
        match drain_children_until(deadline, |_| {})? {
            ChildDrain::Empty => return Ok(Some(ReapedFamily(()))),
            ChildDrain::Expired => return Ok(None),
            ChildDrain::Pending => {}
        }
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            return Ok(None);
        };
        std::thread::sleep(remaining.min(Duration::from_millis(5)));
    }
}


#[derive(Debug, PartialEq, Eq)]
enum ChildDrain {
    Empty,
    Pending,
    Expired,
}

// Only the dedicated owner may wait for all its children. An uninterrupted
// stream of completed descendants or EINTR must not extend its caller's budget.
fn drain_children_until(
    deadline: Instant,
    reaped: impl FnMut(i32),
) -> io::Result<ChildDrain> {
    drain_children_with(
        deadline,
        || {
            let pid = unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) };
            if pid < 0 { Err(io::Error::last_os_error()) } else { Ok(pid) }
        },
        Instant::now,
        reaped,
    )
}

fn drain_children_with(
    deadline: Instant,
    mut wait: impl FnMut() -> io::Result<i32>,
    mut now: impl FnMut() -> Instant,
    mut reaped: impl FnMut(i32),
) -> io::Result<ChildDrain> {
    loop {
        if now() >= deadline {
            return Ok(ChildDrain::Expired);
        }
        let observed = wait();
        // A positive wait already consumed this child's status. Record that
        // fact even if the syscall returned after its caller's deadline.
        if let Ok(pid) = &observed {
            if *pid > 0 {
                reaped(*pid);
            }
        }
        if now() >= deadline {
            return Ok(ChildDrain::Expired);
        }
        match observed {
            Ok(pid) if pid > 0 => {}
            Ok(0) => return Ok(ChildDrain::Pending),
            Ok(_) => return Err(io::Error::other("Invalid owned-child wait result")),
            Err(error) if error.raw_os_error() == Some(libc::ECHILD) => {
                return Ok(ChildDrain::Empty);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;
    use std::cell::Cell;

    #[test]
    fn repeated_positive_waits_stop_at_the_original_deadline() {
        let start = Instant::now();
        let elapsed = Cell::new(0);
        let mut observed = Vec::new();
        let result = drain_children_with(
            start + Duration::from_millis(3),
            || { elapsed.set(elapsed.get() + 1); Ok(elapsed.get()) },
            || start + Duration::from_millis(elapsed.get() as u64),
            |pid| observed.push(pid),
        ).unwrap();
        assert_eq!(result, ChildDrain::Expired);
        // The final positive status was actually consumed; leader bookkeeping
        // must survive expiry instead of retaining a stale original Child.
        assert_eq!(observed, [1, 2, 3]);
    }

    #[test]
    fn repeated_interrupted_waits_stop_at_the_original_deadline() {
        let start = Instant::now();
        let elapsed = Cell::new(0);
        let result = drain_children_with(
            start + Duration::from_millis(3),
            || {
                elapsed.set(elapsed.get() + 1);
                Err(io::Error::from_raw_os_error(libc::EINTR))
            },
            || start + Duration::from_millis(elapsed.get()),
            |_| panic!("interrupted waits cannot consume a child"),
        ).unwrap();
        assert_eq!(result, ChildDrain::Expired);
        assert_eq!(elapsed.get(), 3);
    }

    #[test]
    fn no_children_after_expiry_does_not_produce_cleanup_proof() {
        let start = Instant::now();
        let elapsed = Cell::new(false);
        let result = drain_children_with(
            start + Duration::from_millis(1),
            || { elapsed.set(true); Err(io::Error::from_raw_os_error(libc::ECHILD)) },
            || start + Duration::from_millis(u64::from(elapsed.get())),
            |_| unreachable!(),
        ).unwrap();
        assert_eq!(result, ChildDrain::Expired);
    }

    #[test]
    fn only_no_children_before_expiry_completes_the_drain() {
        let now = Instant::now();
        for (wait, expected) in [(Ok(0), ChildDrain::Pending),
            (Err(io::Error::from_raw_os_error(libc::ECHILD)), ChildDrain::Empty)] {
            let mut wait = Some(wait);
            assert_eq!(drain_children_with(now + Duration::from_secs(1),
                || wait.take().unwrap(), || now, |_| unreachable!()).unwrap(), expected);
        }
    }

    #[test]
    fn failed_wait_is_not_empty_family_proof() {
        let now = Instant::now();
        let error = drain_children_with(
            now + Duration::from_secs(1),
            || Err(io::Error::from_raw_os_error(libc::EACCES)),
            || now,
            |_| unreachable!(),
        ).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::EACCES));
    }

    #[test]
    fn expired_deadline_does_not_attempt_another_wait() {
        let now = Instant::now();
        assert_eq!(drain_children_with(now, || panic!("expired wait attempted"),
            || now, |_| unreachable!()).unwrap(), ChildDrain::Expired);
    }
}
