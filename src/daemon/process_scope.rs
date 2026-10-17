//! Give a process owner and its future descendants their own OOM selection unit.
//! Holders enroll before spawning their provider/PTY root. Direct providers
//! enroll before receiving their first prompt.
use std::{
    fs, io,
    os::unix::process::CommandExt,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(10);

fn cgroup(pid: u32) -> io::Result<String> {
    let contents = fs::read_to_string(format!("/proc/{pid}/cgroup"))?;
    contents
        .lines()
        .find_map(|line| line.strip_prefix("0::"))
        .map(str::to_owned)
        .ok_or_else(|| io::Error::other("unified cgroup membership unavailable"))
}

fn manager_runtime() -> std::path::PathBuf {
    // The OS user manager is independent of cagents' isolated application/test
    // runtime namespace and the provider's account environment.
    std::path::PathBuf::from(format!("/run/user/{}", super::current_uid()))
}

pub(crate) fn manager_available() -> bool {
    cfg!(target_os = "linux") && manager_runtime().join("systemd/private").exists()
}

/// Failure keeps the existing launch path. No actor has started yet and no
/// provider command, identity, environment or inherited descriptor is changed.
pub(crate) fn enroll_holder() -> io::Result<()> {
    if !manager_available() {
        return Ok(());
    }
    enroll_process(std::process::id())
}

/// Enroll an unreaped owned provider before its first prompt is submitted.
/// The caller keeps the Child alive until this operation has completed.
pub(crate) fn enroll_process(pid: u32) -> io::Result<()> {
    enroll_process_as(pid, "cagents-session")
}

/// Upgrade launchers must retain their inherited admission descriptor, which
/// systemd-run cannot forward. Move the admitted daemon itself before it takes
/// the inventory lease or restores any conversation, preserving that authority.
pub(crate) fn enroll_daemon() -> io::Result<()> {
    enroll_process_as(std::process::id(), "cagentsd")
}

fn enroll_process_as(pid: u32, prefix: &str) -> io::Result<()> {
    if !manager_available() {
        return Ok(());
    }
    let unit = format!("{prefix}-{}.scope", uuid::Uuid::new_v4());
    let mut command = Command::new("busctl");
    command
        .env("XDG_RUNTIME_DIR", manager_runtime())
        .env_remove("DBUS_SESSION_BUS_ADDRESS");
    command
        .args([
            "--user",
            "--timeout=1s",
            "call",
            "org.freedesktop.systemd1",
            "/org/freedesktop/systemd1",
            "org.freedesktop.systemd1.Manager",
            "StartTransientUnit",
            "ssa(sv)a(sa(sv))",
            &unit,
            "fail",
            "3",
            "PIDs",
            "au",
            "1",
            &pid.to_string(),
            "Slice",
            "s",
            "app.slice",
            "CollectMode",
            "s",
            "inactive-or-failed",
            "0",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    // SAFETY: only the existing close_range syscall runs after fork. In
    // particular busctl never receives the holder's IPC or cwd capabilities.
    unsafe {
        command.pre_exec(super::seal_inherited_descriptors);
    }
    enroll_with(command, &unit, TIMEOUT, || cgroup(pid))
}

fn enroll_with(
    mut command: Command,
    unit: &str,
    timeout: Duration,
    mut membership: impl FnMut() -> io::Result<String>,
) -> io::Result<()> {
    let started = Instant::now();
    let mut child = command.spawn()?;
    loop {
        if let Some(status) = child.try_wait()? {
            if !status.success() {
                return Err(io::Error::other(format!(
                    "user scope request failed: {status}"
                )));
            }
            break;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "user scope request timed out",
            ));
        }
        thread::sleep(POLL.min(timeout.saturating_sub(started.elapsed())));
    }
    // StartTransientUnit acknowledges a queued job, not completed migration.
    // Verify the exact unique unit before any provider can inherit this scope.
    loop {
        if membership()?.rsplit('/').next() == Some(unit) {
            return Ok(());
        }
        if started.elapsed() >= timeout {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "user scope membership was not confirmed",
            ));
        }
        thread::sleep(POLL.min(timeout.saturating_sub(started.elapsed())));
    }
}

/// Holder constructors have no tracing subscriber. Their daemon logs degraded
/// isolation after startup ACK instead; include the actual root to detect a
/// manager migration that completed too late for the provider fork.
pub(crate) fn warn_if_shared(holder: u32, root: u32) {
    if !manager_available() {
        return;
    }
    let isolated = (|| {
        let caller = cgroup(std::process::id())?;
        let owner = cgroup(holder)?;
        let provider = cgroup(root)?;
        Ok::<_, io::Error>(
            owner != caller
                && provider == owner
                && owner.rsplit('/').next().is_some_and(|unit| {
                    unit.starts_with("cagents-session-") && unit.ends_with(".scope")
                }),
        )
    })();
    if !matches!(isolated, Ok(true)) {
        tracing::warn!(
            holder,
            root,
            "holder scope isolation unavailable; provider uses existing launch fallback"
        );
    }
}

/// Direct providers may create bootstrap helpers before their first prompt.
/// Moving the unreaped root fences future forks; diagnose any already-created
/// descendants rather than migrating unowned/reusable numeric PIDs.
pub(crate) fn warn_if_bootstrap_shared(root: u32) {
    if !manager_available() {
        return;
    }
    let Ok(expected) = cgroup(root) else {
        return;
    };
    let mut seen = std::collections::BTreeSet::from([root]);
    let mut pending = vec![root];
    while let Some(pid) = pending.pop() {
        let Ok(tasks) = fs::read_dir(format!("/proc/{pid}/task")) else {
            continue;
        };
        for task in tasks.flatten() {
            let Ok(children) = fs::read_to_string(task.path().join("children")) else {
                continue;
            };
            for child in children
                .split_whitespace()
                .filter_map(|pid| pid.parse::<u32>().ok())
            {
                if !seen.insert(child) {
                    continue;
                }
                if cgroup(child).is_ok_and(|membership| membership != expected) {
                    tracing::warn!(
                        root,
                        child,
                        "provider bootstrap descendant remains outside its session scope"
                    );
                }
                if seen.len() >= 4096 {
                    tracing::warn!(root, "provider startup scope census exceeded its bound");
                    return;
                }
                pending.push(child);
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    #[test]
    fn daemon_enrollment_uses_an_independent_user_scope() {
        const CHILD: &str = "CAGENTS_DAEMON_SCOPE_TEST_CHILD";
        if std::env::var_os(CHILD).is_some() {
            enroll_daemon().unwrap();
            let membership = cgroup(std::process::id()).unwrap();
            let unit = membership.rsplit('/').next().unwrap();
            assert!(
                unit.starts_with("cagentsd-") && unit.ends_with(".scope"),
                "{membership}"
            );
            return;
        }
        if !manager_available() {
            return;
        }
        assert!(Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "daemon::process_scope::tests::daemon_enrollment_uses_an_independent_user_scope", "--nocapture"])
            .env(CHILD, "1").status().unwrap().success());
    }

    #[test]
    fn rejected_scope_request_preserves_fallback() {
        let error = enroll_with(
            Command::new("/bin/false"),
            "fixture.scope",
            Duration::from_secs(1),
            || panic!("failed request must not verify membership"),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }
    #[test]
    fn successful_request_requires_exact_confirmed_membership() {
        let error = enroll_with(
            Command::new("/bin/true"),
            "fixture.scope",
            Duration::from_millis(30),
            || Ok("/app.slice/other.scope".into()),
        )
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        enroll_with(
            Command::new("/bin/true"),
            "fixture.scope",
            Duration::from_secs(1),
            || Ok("/app.slice/fixture.scope".into()),
        )
        .unwrap();
    }
    #[test]
    fn hung_scope_request_is_bounded_and_reaped() {
        let mut command = Command::new("/bin/sleep");
        command.arg("30");
        let started = Instant::now();
        assert_eq!(
            enroll_with(
                command,
                "fixture.scope",
                Duration::from_millis(30),
                || unreachable!()
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::TimedOut
        );
        assert!(started.elapsed() < Duration::from_secs(1));
    }
    pub(crate) fn assert_separate(holder: u32, root: u32) {
        if !manager_available() {
            return;
        }
        let owner = cgroup(holder).unwrap();
        assert_ne!(owner, cgroup(std::process::id()).unwrap());
        assert_eq!(owner, cgroup(root).unwrap());
        assert!(
            owner
                .rsplit('/')
                .next()
                .unwrap()
                .starts_with("cagents-session-")
        );
    }
}
