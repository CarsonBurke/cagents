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

/// Whether an exact retained helper and its provider ended without leaving
/// any process in either's process group or session.
///
/// The provider is spawned as its own process-group leader and the helper as
/// its own session leader. Linux never hands out a PID while a process still
/// uses it as a process-group or session ID, so a process naming either PID
/// that way is a survivor of the old actor (or, conservatively, a later one
/// that happens to reuse it); both refuse. Descendants that left the group on
/// purpose are tool work, not writers of the provider's conversation, which
/// only the provider process itself records.
pub(crate) fn abandoned(holder: &Process, root: &Process) -> Result<bool, String> {
    abandoned_in(std::path::Path::new("/proc"), holder, root)
}

fn abandoned_in(proc: &std::path::Path, holder: &Process, root: &Process) -> Result<bool, String> {
    abandoned_with_policy(proc, holder, root, false)
}

/// Codex execution needs a positive survivor tied to the pane. A reused PID
/// proves its original group/session also ended: Linux cannot reuse the PID
/// while that numeric group/session still has members.
pub(crate) fn codex_abandoned(holder: &Process, root: &Process) -> Result<bool, String> {
    abandoned_with_policy(std::path::Path::new("/proc"), holder, root, true)
}

fn abandoned_with_policy(
    proc: &std::path::Path,
    holder: &Process,
    root: &Process,
    exclude_reused: bool,
) -> Result<bool, String> {
    let mut groups = Vec::with_capacity(2);
    for expected in [holder, root] {
        if expected.pid <= 0 || expected.start == 0 {
            return Err("Invalid retained process absence identity".into());
        }
        match Process::read(expected.pid) {
            Ok(actual) if actual == *expected => return Ok(false),
            Ok(_) if exclude_reused => {}
            Ok(_) => groups.push(expected.pid),
            Err(error) if error.kind() == io::ErrorKind::NotFound => groups.push(expected.pid),
            Err(error) => return Err(format!("Cannot establish exact process absence: {error}")),
        }
    }
    let entries = std::fs::read_dir(proc)
        .map_err(|error| format!("Cannot enumerate processes for exact absence: {error}"))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("Cannot enumerate processes: {error}"))?;
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<i32>().ok())
        else {
            continue;
        };
        let stat = match std::fs::read(entry.path().join("stat")) {
            Ok(stat) => stat,
            // A process that exited during the scan holds no group: its
            // entry is gone, or was reaped between opening and reading it.
            Err(error)
                if error.kind() == io::ErrorKind::NotFound
                    || error.raw_os_error() == Some(libc::ESRCH) =>
            {
                continue;
            }
            Err(error) => {
                return Err(format!(
                    "Cannot read process {pid} for exact absence: {error}"
                ));
            }
        };
        let Some(close) = stat.iter().rposition(|&byte| byte == b')') else {
            return Err(format!("Process {pid} has an unreadable identity"));
        };
        let fields = String::from_utf8_lossy(&stat[close + 1..]);
        let mut fields = fields.split_ascii_whitespace().skip(2);
        let (Some(group), Some(session)) = (
            fields.next().and_then(|field| field.parse::<i32>().ok()),
            fields.next().and_then(|field| field.parse::<i32>().ok()),
        ) else {
            return Err(format!("Process {pid} has an unreadable group identity"));
        };
        if [group, session].iter().any(|id| groups.contains(id)) {
            return Ok(false);
        }
    }
    Ok(true)
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

#[cfg(test)]
mod abandonment_tests {
    use super::*;

    fn stat(root: &std::path::Path, pid: i32, group: i32, session: i32) {
        let directory = root.join(pid.to_string());
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(
            directory.join("stat"),
            format!("{pid} (odd ) name) S 1 {group} {session} 0 0"),
        )
        .unwrap();
    }

    #[test]
    fn codex_absence_excludes_reused_ids_but_keeps_leaderless_groups() {
        let me = Process::read(std::process::id() as i32).unwrap();
        let reused = Process {
            start: me.start + 1,
            ..me.clone()
        };
        let gone = Process {
            pid: i32::MAX - 1,
            start: 1,
        };
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        stat(root.path(), 10, reused.pid, reused.pid);
        assert!(abandoned_with_policy(root.path(), &reused, &gone, true).unwrap());
        // Retained Claude keeps its existing conservative policy.
        assert!(!abandoned_in(root.path(), &reused, &gone).unwrap());
        assert!(!abandoned_with_policy(root.path(), &me, &gone, true).unwrap());
        // The old leader is absent, yet a child still holds its numeric group.
        stat(root.path(), 10, gone.pid, 10);
        assert!(!abandoned_with_policy(root.path(), &reused, &gone, true).unwrap());
    }

    #[test]
    fn abandonment_requires_both_exits_and_no_surviving_group_or_session() {
        let me = Process::read(std::process::id() as i32).unwrap();
        let gone = |pid| Process { pid, start: 1 };
        let root = tempfile::tempdir_in("/var/tmp").unwrap();
        stat(root.path(), 10, 10, 10);
        assert!(abandoned_in(root.path(), &gone(i32::MAX - 1), &gone(i32::MAX - 2)).unwrap());
        // A live helper or provider is never abandoned.
        assert!(!abandoned_in(root.path(), &me, &gone(i32::MAX - 2)).unwrap());
        assert!(!abandoned_in(root.path(), &gone(i32::MAX - 1), &me).unwrap());
        // A process left in the provider's group or the helper's session survives it.
        stat(root.path(), 11, i32::MAX - 2, 10);
        assert!(!abandoned_in(root.path(), &gone(i32::MAX - 1), &gone(i32::MAX - 2)).unwrap());
        std::fs::remove_dir_all(root.path().join("11")).unwrap();
        stat(root.path(), 12, 12, i32::MAX - 1);
        assert!(!abandoned_in(root.path(), &gone(i32::MAX - 1), &gone(i32::MAX - 2)).unwrap());
        std::fs::remove_dir_all(root.path().join("12")).unwrap();
        // An unreadable identity is never read as absence.
        std::fs::create_dir_all(root.path().join("13")).unwrap();
        std::fs::write(root.path().join("13/stat"), "13 no-close").unwrap();
        assert!(abandoned_in(root.path(), &gone(i32::MAX - 1), &gone(i32::MAX - 2)).is_err());
    }
}
