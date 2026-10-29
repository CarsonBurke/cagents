//! Give a process owner and its future descendants their own OOM selection unit.
//! Holders enroll before spawning their provider/PTY root. Direct providers
//! enroll before receiving their first prompt.
use std::{
    fs,
    io::{self, Read},
    os::{
        fd::OwnedFd,
        unix::{net::UnixStream, process::CommandExt},
    },
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(10);
const MAX_SCOPE_STDERR: usize = 4096;

/// Drain without a reader thread or an EOF wait: the request deadline still
/// bounds a noisy command or a descendant retaining its stderr descriptor.
struct ScopeStderr {
    reader: UnixStream,
    bytes: Vec<u8>,
    truncated: bool,
    error: Option<io::Error>,
}

impl ScopeStderr {
    fn drain(&mut self) {
        let mut bytes = [0_u8; 1024];
        for _ in 0..8 {
            match self.reader.read(&mut bytes) {
                Ok(0) => break,
                Ok(count) => {
                    let keep = count.min(MAX_SCOPE_STDERR - self.bytes.len());
                    self.bytes.extend_from_slice(&bytes[..keep]);
                    self.truncated |= keep < count;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    self.error = Some(error);
                    break;
                }
            }
        }
    }

    fn diagnostic(&self) -> String {
        let mut detail = String::new();
        if !self.bytes.is_empty() {
            // Debug quoting preserves the bytes' meaning without replaying
            // terminal escapes or embedding unlabelled lines in daemon logs.
            detail.push_str(&format!(
                "; busctl stderr: {:?}",
                String::from_utf8_lossy(&self.bytes)
            ));
        }
        if self.truncated {
            detail.push_str(" [truncated at 4096 bytes]");
        }
        if let Some(error) = &self.error {
            detail.push_str(&format!("; stderr capture failed: {error}"));
        }
        detail
    }
}

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
    if !manager_available() {
        return Ok(());
    }
    let unit = format!("cagents-session-{}.scope", uuid::Uuid::new_v4());
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
        .stdout(Stdio::null());
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
    // Prepare a nonblocking reader before a child exists, so capture setup
    // cannot abandon an unreaped request. The child's writing end is blocking.
    let (reader, writer) = UnixStream::pair()?;
    reader.set_nonblocking(true)?;
    command.stderr(Stdio::from(OwnedFd::from(writer)));
    let mut stderr = ScopeStderr {
        reader,
        bytes: Vec::new(),
        truncated: false,
        error: None,
    };
    let started = Instant::now();
    let mut child = command.spawn()?;
    loop {
        stderr.drain();
        if let Some(status) = child.try_wait()? {
            stderr.drain();
            if !status.success() {
                return Err(io::Error::other(format!(
                    "user scope request failed: {status}{}",
                    stderr.diagnostic()
                )));
            }
            break;
        }
        if started.elapsed() >= timeout {
            let _ = child.kill();
            let _ = child.wait();
            stderr.drain();
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("user scope request timed out{}", stderr.diagnostic()),
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
    fn rejected_scope_request_reports_bounded_stderr() {
        let mut command = Command::new("/bin/sh");
        command.args([
            "-c",
            "printf 'fixture scope refusal\\n' >&2; printf '%16384s' '' >&2; exit 7",
        ]);
        let error = enroll_with(command, "fixture.scope", Duration::from_secs(1), || {
            panic!("failed request must not verify membership")
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        let diagnostic = error.to_string();
        assert!(diagnostic.contains("exit status: 7"), "{diagnostic}");
        assert!(
            diagnostic.contains("fixture scope refusal\\n"),
            "{diagnostic}"
        );
        assert!(
            diagnostic.contains("truncated at 4096 bytes"),
            "{diagnostic}"
        );
        assert!(
            diagnostic.len() < MAX_SCOPE_STDERR + 256,
            "{}",
            diagnostic.len()
        );
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
