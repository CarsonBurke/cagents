//! Execution exit of an exact retained helper, independent of its parent's wait.
//! This observer never signals, waits for, or acquires Child custody.
use crate::daemon::holder_registry::Process;
use std::{
    fs::File,
    io,
    os::fd::{AsRawFd, FromRawFd},
};

pub(super) enum Observation {
    Running,
    Exited(ExecutionExit),
}

/// Keep a captured kernel identity pinned through retirement validation.
/// An already absent original incarnation needs no descriptor.
pub(super) struct ExecutionExit {
    _pidfd: Option<File>,
}

pub(super) fn identity_absent(expected: &Process) -> Result<bool, String> {
    if expected.pid <= 0 || expected.start == 0 {
        return Err("Invalid retained process absence identity".into());
    }
    match Process::read(expected.pid) {
        Ok(actual) => Ok(actual != *expected),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(true),
        Err(error) => Err(format!("Cannot establish exact process absence: {error}")),
    }
}

pub(super) fn observe(expected: &Process) -> Result<Observation, String> {
    if expected.pid <= 0 || expected.start == 0 {
        return Err("Invalid retained helper execution identity".into());
    }
    // Read before AND after pidfd_open. A recycled PID must never turn a
    // descriptor for another incarnation into proof about this helper.
    if identity_absent(expected)? {
        return Ok(Observation::Exited(ExecutionExit { _pidfd: None }));
    }
    // SAFETY: scalar arguments; success returns a fresh owned descriptor.
    let raw = unsafe { libc::syscall(libc::SYS_pidfd_open, expected.pid, 0) };
    if raw < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ESRCH) && identity_absent(expected)? {
            return Ok(Observation::Exited(ExecutionExit { _pidfd: None }));
        }
        return Err(format!(
            "Cannot pin exact retained helper execution: {error}"
        ));
    }
    // SAFETY: successful pidfd_open above yielded a fresh owned descriptor.
    let pidfd = unsafe { File::from_raw_fd(raw as libc::c_int) };
    if identity_absent(expected)? {
        // The expected incarnation disappeared during capture. Do not keep a
        // descriptor that might now name a different incarnation of that PID.
        drop(pidfd);
        return Ok(Observation::Exited(ExecutionExit { _pidfd: None }));
    }
    let mut target = libc::pollfd {
        fd: pidfd.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: one initialized pollfd, no blocking or ownership transfer.
    let ready = unsafe { libc::poll(&mut target, 1, 0) };
    if ready < 0 {
        return Err(format!(
            "Cannot observe exact retained helper exit: {}",
            io::Error::last_os_error()
        ));
    }
    if target.revents & (libc::POLLNVAL | libc::POLLERR) != 0 {
        return Err("Retained helper exit descriptor reported an error".into());
    }
    if target.revents & libc::POLLIN != 0 {
        return Ok(Observation::Exited(ExecutionExit {
            _pidfd: Some(pidfd),
        }));
    }
    if target.revents != 0 {
        // A process reaped after capture is independently absent; unexpected
        // readiness while that incarnation still exists remains unknown.
        if identity_absent(expected)? {
            return Ok(Observation::Exited(ExecutionExit {
                _pidfd: Some(pidfd),
            }));
        }
        return Err("Unexpected retained helper exit descriptor readiness".into());
    }
    Ok(Observation::Running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        process::{Child, Command, Stdio},
        time::{Duration, Instant},
    };
    struct Owner(Child);
    impl Drop for Owner {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    #[test]
    fn execution_observation_preserves_running_and_unreaped_original_child_custody() {
        let mut owner = Owner(
            Command::new("/bin/cat")
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let identity = Process::read(owner.0.id() as i32).unwrap();
        assert!(matches!(observe(&identity).unwrap(), Observation::Running));
        let mut old = identity.clone();
        old.start += 1;
        assert!(matches!(observe(&old).unwrap(), Observation::Exited(_)));
        assert!(matches!(observe(&identity).unwrap(), Observation::Running));
        owner.0.kill().unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while matches!(observe(&identity).unwrap(), Observation::Running) {
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(
            Process::read(identity.pid).unwrap(),
            identity,
            "execution observation must not consume original Child custody"
        );
        assert!(owner.0.wait().unwrap().code().is_none());
        assert!(identity_absent(&identity).unwrap());
    }
    #[test]
    fn invalid_helper_identity_is_unknown_not_running_or_retired() {
        assert!(observe(&Process { pid: -1, start: 1 }).is_err());
        assert!(identity_absent(&Process { pid: -1, start: 1 }).is_err());
        assert!(
            identity_absent(&Process {
                pid: std::process::id() as i32,
                start: 0
            })
            .is_err()
        );
        assert!(
            observe(&Process {
                pid: std::process::id() as i32,
                start: 0
            })
            .is_err()
        );
    }
}
