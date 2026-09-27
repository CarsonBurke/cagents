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
            for pid in children {
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
