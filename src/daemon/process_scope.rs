//! Give a process owner and its future descendants their own OOM selection unit.
//! Holders enroll before spawning their provider/PTY root. Direct providers
//! enroll before receiving their first prompt.
use std::{
    fs,
    io::{self, Read},
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, fs::MetadataExt, net::UnixStream, process::CommandExt},
    },
    path::Path,
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

/// Positive kernel proof about the fixed OS user manager, not its bus broker.
///
/// Numeric PIDs in StartTransientUnit belong to the manager's PID namespace.
/// A visible peer PID alone does not suffice: a descendant namespace's manager
/// is visible too, but would interpret the same number differently. The peer
/// pidfd binds its lifetime without reopening a reusable numeric PID; the
/// namespace ioctl also avoids interpreting a foreign /proc mount's PID view.
/// Missing kernel support or denied proof deliberately leaves launch unchanged.
struct ManagerNamespace {
    _connection: UnixStream,
    peer: OwnedFd,
    _namespace: fs::File,
}

impl ManagerNamespace {
    fn connect() -> io::Result<Self> {
        let path = manager_runtime().join("systemd/private");
        crate::secure_file::reject_symlink_components(&path)?;
        Self::from_connection(connect_nonblocking(&path)?, super::current_uid())
    }

    fn from_connection(connection: UnixStream, uid: u32) -> io::Result<Self> {
        let mut credentials = std::mem::MaybeUninit::<libc::ucred>::uninit();
        let mut size = std::mem::size_of::<libc::ucred>() as libc::socklen_t;
        // SAFETY: the owned Unix socket and correctly sized output remain live.
        if unsafe {
            libc::getsockopt(
                connection.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERCRED,
                credentials.as_mut_ptr().cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if size as usize != std::mem::size_of::<libc::ucred>() {
            return Err(io::Error::other("incomplete manager peer credentials"));
        }
        // SAFETY: getsockopt initialized the complete ucred above.
        let credentials = unsafe { credentials.assume_init() };
        if credentials.pid <= 0 || credentials.uid != uid {
            return Err(io::Error::other(
                "user manager peer is not visible and owned",
            ));
        }
        let mut descriptor = -1;
        let mut size = std::mem::size_of::<i32>() as libc::socklen_t;
        // SAFETY: SO_PEERPIDFD returns a new descriptor for this exact peer.
        if unsafe {
            libc::getsockopt(
                connection.as_raw_fd(),
                libc::SOL_SOCKET,
                libc::SO_PEERPIDFD,
                (&mut descriptor as *mut i32).cast(),
                &mut size,
            )
        } != 0
        {
            return Err(io::Error::last_os_error());
        }
        if descriptor < 0 {
            return Err(io::Error::other("manager peer has no pidfd"));
        }
        // SAFETY: the successful getsockopt transferred this new owned fd.
        let peer = unsafe { OwnedFd::from_raw_fd(descriptor) };
        if size as usize != std::mem::size_of::<i32>() || !peer_alive(&peer) {
            return Err(io::Error::other("manager peer is no longer live"));
        }
        // SAFETY: this ioctl returns a new fd for the exact pidfd's active PID
        // namespace, or fails. Its third argument must be explicitly zero;
        // pidfs rejects nonzero arguments. No numeric /proc/PID lookup occurs.
        let namespace = unsafe {
            libc::ioctl(
                peer.as_raw_fd(),
                libc::PIDFD_GET_PID_NAMESPACE,
                0 as libc::c_ulong,
            )
        };
        if namespace < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful namespace ioctl transferred a new owned fd.
        let namespace = unsafe { fs::File::from_raw_fd(namespace) };
        let current = fs::File::open("/proc/self/ns/pid")?;
        if !same_namespace(&namespace, &current)? || !peer_alive(&peer) {
            return Err(io::Error::other("user manager uses another PID namespace"));
        }
        Ok(Self {
            _connection: connection,
            peer,
            _namespace: namespace,
        })
    }
}

fn same_namespace(left: &fs::File, right: &fs::File) -> io::Result<bool> {
    let left = left.metadata()?;
    let right = right.metadata()?;
    Ok((left.dev(), left.ino()) == (right.dev(), right.ino()))
}

fn peer_alive(peer: &OwnedFd) -> bool {
    let mut descriptor = libc::pollfd {
        fd: peer.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    // SAFETY: the owned pidfd and one initialized pollfd remain live.
    unsafe { libc::poll(&mut descriptor, 1, 0) == 0 }
}

fn connect_nonblocking(path: &Path) -> io::Result<UnixStream> {
    // Never wait on a full manager socket backlog before ordinary launch.
    // SAFETY: socket returns a fresh descriptor, with atomic CLOEXEC.
    let descriptor = unsafe {
        libc::socket(
            libc::AF_UNIX,
            libc::SOCK_STREAM | libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            0,
        )
    };
    if descriptor < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful socket transferred this new owned descriptor.
    let descriptor = unsafe { OwnedFd::from_raw_fd(descriptor) };
    // SAFETY: all-zero sockaddr_un is initialized before connect reads it.
    let mut address: libc::sockaddr_un = unsafe { std::mem::zeroed() };
    let path = path.as_os_str().as_bytes();
    if path.contains(&0) || path.len() >= address.sun_path.len() {
        return Err(io::Error::other("invalid user manager socket path"));
    }
    address.sun_family = libc::AF_UNIX as libc::sa_family_t;
    for (to, from) in address.sun_path.iter_mut().zip(path) {
        *to = *from as libc::c_char;
    }
    let size = std::mem::offset_of!(libc::sockaddr_un, sun_path) + path.len() + 1;
    // SAFETY: size covers the initialized family/path and its terminating NUL.
    if unsafe {
        libc::connect(
            descriptor.as_raw_fd(),
            (&address as *const libc::sockaddr_un).cast(),
            size as libc::socklen_t,
        )
    } != 0
    {
        return Err(io::Error::last_os_error());
    }
    Ok(UnixStream::from(descriptor))
}

fn with_verified_manager(
    proof: io::Result<ManagerNamespace>,
    request: impl FnOnce() -> io::Result<()>,
) -> io::Result<()> {
    let Ok(proof) = proof else {
        return Ok(());
    };
    if !peer_alive(&proof.peer) {
        return Ok(());
    }
    // Existing trust contract: this fixed OS manager owns the well-known
    // org.freedesktop.systemd1 service on the fixed OS user bus. We do not
    // substitute a broker's credentials for the manager's, or support a
    // hostile same-UID replacement of the OS manager/service routing. Keep
    // its exact peer and namespace proof through the unchanged busctl call.
    let result = request();
    drop(proof);
    result
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
    with_verified_manager(ManagerNamespace::connect(), || enroll_proven_process(pid))
}

fn enroll_proven_process(pid: u32) -> io::Result<()> {
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
    fn unproved_manager_never_attempts_numeric_pid_enrollment() {
        for error in [
            libc::ENOPROTOOPT,
            libc::ENOTTY,
            libc::EPERM,
            libc::EACCES,
            libc::ENOENT,
        ] {
            with_verified_manager(Err(io::Error::from_raw_os_error(error)), || {
                panic!("unproved manager must not receive a scope request")
            })
            .unwrap();
        }
    }

    #[test]
    fn kernel_namespace_identity_is_not_an_unrelated_namespace_inode() {
        let current = fs::File::open("/proc/self/ns/pid").unwrap();
        let same = fs::File::open("/proc/self/ns/pid").unwrap();
        let other = fs::File::open("/proc/self/ns/mnt").unwrap();
        assert!(same_namespace(&current, &same).unwrap());
        assert!(!same_namespace(&current, &other).unwrap());
    }

    #[test]
    fn kernel_peer_identity_refuses_an_unowned_connection_before_requesting() {
        let (connection, _peer) = UnixStream::pair().unwrap();
        let wrong_uid = super::super::current_uid().wrapping_add(1);
        let proof = ManagerNamespace::from_connection(connection, wrong_uid);
        assert!(proof.is_err());
        with_verified_manager(proof, || panic!("wrong peer must not receive a request")).unwrap();
    }

    #[test]
    fn real_same_namespace_peer_is_proved_or_optional_kernel_support_fails_closed() {
        let (connection, _peer) = UnixStream::pair().unwrap();
        let proof = ManagerNamespace::from_connection(connection, super::super::current_uid());
        match proof {
            Ok(proof) => {
                let current = fs::File::open("/proc/self/ns/pid").unwrap();
                assert!(same_namespace(&proof._namespace, &current).unwrap());
                let peer = proof.peer.as_raw_fd();
                let namespace = proof._namespace.as_raw_fd();
                let connection = proof._connection.as_raw_fd();
                let mut requested = false;
                let error = with_verified_manager(Ok(proof), || {
                    for descriptor in [peer, namespace, connection] {
                        // SAFETY: the proof must retain all owned descriptors
                        // until the request returns; F_GETFD has no side effect.
                        let flags = unsafe { libc::fcntl(descriptor, libc::F_GETFD) };
                        assert_ne!(flags, -1);
                        assert_ne!(flags & libc::FD_CLOEXEC, 0);
                    }
                    requested = true;
                    Err(io::Error::other("fixture verified request refused"))
                })
                .unwrap_err();
                assert!(requested);
                assert_eq!(error.to_string(), "fixture verified request refused");
                eprintln!("actual kernel peer pidfd/PID-namespace proof succeeded");
            }
            Err(error) => {
                // Older kernels and confinement can deny these optional APIs.
                // Verify real refusal, not a fictional positive manager proof.
                assert!(matches!(
                    error.raw_os_error(),
                    Some(libc::ENOPROTOOPT | libc::ENOTTY | libc::EPERM | libc::EACCES)
                ));
                eprintln!("kernel manager proof unavailable; no scope request: {error}");
                with_verified_manager(Err(error), || panic!("unproved peer request")).unwrap();
            }
        }
    }

    #[test]
    fn private_peer_connection_is_nonblocking_without_a_manager_request() {
        use std::os::unix::net::UnixListener;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture.sock");
        let listener = UnixListener::bind(&path).unwrap();
        let connection = connect_nonblocking(&path).unwrap();
        let (_accepted, _) = listener.accept().unwrap();
        // SAFETY: F_GETFL only observes the owned socket's status flags.
        let flags = unsafe { libc::fcntl(connection.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::O_NONBLOCK, 0);
        assert!(connect_nonblocking(&directory.path().join("absent")).is_err());
    }

    #[test]
    fn rejected_scope_request_preserves_fallback() {
        let error = rejected_scope_request().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
    }
    pub(crate) fn rejected_scope_request() -> io::Result<()> {
        enroll_with(
            Command::new("/bin/false"),
            "fixture.scope",
            Duration::from_secs(1),
            || panic!("failed request must not verify membership"),
        )
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
            eprintln!(
                "scope isolation unqualified: OS user manager unavailable; existing launch fallback"
            );
            return;
        }
        let _proof = match ManagerNamespace::connect() {
            Ok(proof) => proof,
            Err(error) => {
                eprintln!(
                    "scope isolation unqualified: manager namespace proof unavailable; existing launch fallback: {error}"
                );
                return;
            }
        };
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
