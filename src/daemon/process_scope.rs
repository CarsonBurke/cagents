//! Give a process owner and its future descendants their own OOM selection unit.
//! Holders enroll before spawning their provider/PTY root. Direct providers
//! enroll before receiving their first prompt.
use std::{
    fs, io,
    os::{
        fd::{AsRawFd, FromRawFd, OwnedFd},
        unix::{ffi::OsStrExt, net::UnixStream, process::CommandExt},
    },
    path::Path,
    process::{Command, Stdio},
    thread,
    time::{Duration, Instant},
};

const TIMEOUT: Duration = Duration::from_secs(2);
const POLL: Duration = Duration::from_millis(10);
const INVISIBLE_MANAGER: &str = "user manager peer PID is unavailable";
const UNOWNED_MANAGER: &str = "user manager peer has an unexpected owner";

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
/// pidfd binds its lifetime without reopening a reusable numeric PID, and its
/// procfs fdinfo names the namespaces it occupies. The fdinfo needs no ptrace
/// access, unlike PIDFD_GET_PID_NAMESPACE or /proc/PID/ns, which the OS user
/// manager denies us because it holds CAP_WAKE_ALARM. Missing kernel support or
/// denied proof deliberately leaves launch unchanged.
struct ManagerNamespace {
    _connection: UnixStream,
    peer: OwnedFd,
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
        if credentials.pid <= 0 {
            return Err(io::Error::other(INVISIBLE_MANAGER));
        }
        if credentials.uid != uid {
            return Err(io::Error::other(UNOWNED_MANAGER));
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
        let procfs = Procfs::open()?;
        let own = procfs.read("self/status")?;
        let peer_info = procfs.read(&format!("self/fdinfo/{}", peer.as_raw_fd()))?;
        if !same_namespace(&own, std::process::id(), &peer_info, credentials.pid)
            || !peer_alive(&peer)
        {
            return Err(io::Error::other("user manager uses another PID namespace"));
        }
        Ok(Self {
            _connection: connection,
            peer,
        })
    }
}

/// One procfs instance that every namespace report is read from. Only the
/// kernel's procfs reports namespaces, and entries bind-mounted over it from
/// another instance could disagree, so every entry resolves beneath this one
/// mount without crossing into another.
struct Procfs(OwnedFd);

impl Procfs {
    fn open() -> io::Result<Self> {
        Self::open_at(c"/proc")
    }

    fn open_at(path: &std::ffi::CStr) -> io::Result<Self> {
        // SAFETY: the path is NUL-terminated and outlives the call.
        let root = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_CLOEXEC,
            )
        };
        if root < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful open transferred this new owned descriptor.
        let root = unsafe { OwnedFd::from_raw_fd(root) };
        let mut statfs = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: the owned descriptor and correctly sized output remain live.
        if unsafe { libc::fstatfs(root.as_raw_fd(), statfs.as_mut_ptr()) } != 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: fstatfs initialized the complete statfs above.
        if unsafe { statfs.assume_init() }.f_type != libc::PROC_SUPER_MAGIC {
            return Err(io::Error::other("/proc is not procfs"));
        }
        Ok(Self(root))
    }

    fn read(&self, entry: &str) -> io::Result<String> {
        #[repr(C)]
        struct OpenHow {
            flags: u64,
            mode: u64,
            resolve: u64,
        }
        let how = OpenHow {
            flags: (libc::O_RDONLY | libc::O_CLOEXEC) as u64,
            mode: 0,
            resolve: libc::RESOLVE_BENEATH | libc::RESOLVE_NO_XDEV | libc::RESOLVE_NO_MAGICLINKS,
        };
        let entry = std::ffi::CString::new(entry).map_err(io::Error::other)?;
        // SAFETY: the root descriptor, NUL-terminated entry and open_how of the
        // stated size remain live for the call.
        let file = unsafe {
            libc::syscall(
                libc::SYS_openat2,
                self.0.as_raw_fd(),
                entry.as_ptr(),
                &how as *const OpenHow,
                std::mem::size_of::<OpenHow>(),
            )
        };
        if file < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful openat2 transferred this new owned descriptor.
        let mut file = unsafe { fs::File::from_raw_fd(file as i32) };
        let mut contents = String::new();
        io::Read::read_to_string(&mut file, &mut contents)?;
        Ok(contents)
    }
}

/// procfs lists NSpid from its own PID namespace down to the process's. One
/// entry for ourselves proves /proc views our namespace rather than an
/// ancestor's, and then one entry for the peer, matching the PID the kernel
/// translated into our namespace, proves the peer shares it.
fn same_namespace(own: &str, own_pid: u32, peer: &str, peer_pid: i32) -> bool {
    let nspid = |status: &str| {
        let mut fields = status
            .lines()
            .filter_map(|line| line.strip_prefix("NSpid:"));
        let field = fields.next().filter(|_| fields.next().is_none());
        field.map(|pids| {
            pids.split_whitespace()
                .map(str::parse::<i64>)
                .collect::<Result<Vec<_>, _>>()
        })
    };
    matches!(nspid(own), Some(Ok(pids)) if pids == [i64::from(own_pid)])
        && matches!(nspid(peer), Some(Ok(pids)) if pids == [i64::from(peer_pid)])
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
    // An unproved manager receives no request. Its reason still reaches the
    // caller, which keeps the existing launch and reports the lost isolation.
    let proof = proof.map_err(|error| {
        io::Error::new(error.kind(), format!("user manager proof failed: {error}"))
    })?;
    if !peer_alive(&proof.peer) {
        return Err(io::Error::other(
            "user manager proof failed: manager peer is no longer live",
        ));
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

/// Required enrollment evidence for an owned build before it replaces itself.
/// Keeping this value alive retains the original manager connection and pidfd;
/// dropping it only closes those descriptors, never stops the enrolled scope.
pub(crate) struct ConfirmedScope {
    _manager: ManagerNamespace,
}

/// Unlike optional provider enrollment, an unavailable or unproved manager is
/// an error. Reuse the exact unique-unit membership confirmation while keeping
/// the original namespace proof live, without changing the caller's launch.
pub(crate) fn require_scope_for_current_process() -> io::Result<ConfirmedScope> {
    let proof = ManagerNamespace::connect()?;
    if !peer_alive(&proof.peer) {
        return Err(io::Error::other("user manager peer is no longer live"));
    }
    enroll_proven_process(std::process::id())?;
    if !peer_alive(&proof.peer) {
        return Err(io::Error::other("user manager peer is no longer live"));
    }
    Ok(ConfirmedScope { _manager: proof })
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
    // Held until this function's final wait. A retained actor's orphan sweep
    // must not consume the status this scope request is waiting for.
    let (mut child, _registration) = super::adopted_orphans::spawn_owned(&mut command)?;
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
    fn unproved_manager_never_attempts_numeric_pid_enrollment() {
        for error in [libc::ENOPROTOOPT, libc::EPERM, libc::EACCES, libc::ENOENT] {
            let refused = with_verified_manager(Err(io::Error::from_raw_os_error(error)), || {
                panic!("unproved manager must not receive a scope request")
            })
            .unwrap_err();
            assert!(
                refused
                    .to_string()
                    .starts_with("user manager proof failed: ")
            );
        }
    }

    #[test]
    fn a_manager_that_exited_after_its_proof_receives_no_request() {
        let mut child = Command::new("/bin/true").spawn().unwrap();
        // SAFETY: pidfd_open returns a new descriptor for this unreaped child.
        let peer = unsafe { libc::syscall(libc::SYS_pidfd_open, child.id(), 0) };
        assert!(peer >= 0);
        // SAFETY: the successful syscall transferred this new owned descriptor.
        let peer = unsafe { OwnedFd::from_raw_fd(peer as i32) };
        child.wait().unwrap();
        let proof = ManagerNamespace {
            _connection: UnixStream::pair().unwrap().0,
            peer,
        };
        let refused = with_verified_manager(Ok(proof), || panic!("dead manager request"));
        assert_eq!(
            refused.unwrap_err().to_string(),
            "user manager proof failed: manager peer is no longer live"
        );
    }

    #[test]
    fn a_directory_outside_procfs_cannot_supply_namespace_reports() {
        let directory = tempfile::tempdir().unwrap();
        let path = std::ffi::CString::new(directory.path().as_os_str().as_bytes()).unwrap();
        assert_eq!(
            Procfs::open_at(&path).err().unwrap().to_string(),
            "/proc is not procfs"
        );
    }

    /// Binds a forged report over a real entry, in a mount namespace of a child
    /// test process where unprivileged namespaces allow.
    #[test]
    fn a_report_bind_mounted_over_procfs_is_refused() {
        const CHILD: &str = "CAGENTS_SCOPE_PROCFS_MOUNT_TEST";
        let unshare = || {
            let mut command = Command::new("unshare");
            command.args(["--user", "--map-root-user", "--mount"]);
            command
        };
        if std::env::var_os(CHILD).is_none() {
            if !unshare()
                .arg("true")
                .status()
                .is_ok_and(|status| status.success())
            {
                eprintln!("skipped: no unprivileged mount namespace");
                return;
            }
            let name = module_path!().split_once("::").unwrap().1;
            let output = unshare()
                .arg(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    &format!("{name}::a_report_bind_mounted_over_procfs_is_refused"),
                ])
                .env(CHILD, "1")
                .output()
                .unwrap();
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(
                output.status.success() && stdout.contains("1 passed"),
                "the namespaced fixture failed: {stdout}{}",
                String::from_utf8_lossy(&output.stderr)
            );
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let forged = directory.path().join("status");
        fs::write(&forged, "NSpid:\t1\n").unwrap();
        let entry = format!("{}/status", std::process::id());
        let status = Command::new("mount")
            .arg("--bind")
            .arg(&forged)
            .arg(format!("/proc/{entry}"))
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            Procfs::open()
                .unwrap()
                .read(&entry)
                .unwrap_err()
                .raw_os_error(),
            Some(libc::EXDEV)
        );
    }

    #[test]
    fn namespace_proof_requires_one_shared_namespace_view() {
        let own = "Name:\tcagentsd\nNSpid:\t40\n";
        let peer = "pos:\t0\nPid:\t7\nNSpid:\t7\n";
        let cases = [
            (own, 40, peer, 7, true),
            // /proc belongs to an ancestor namespace.
            ("NSpid:\t900\t40\n", 40, peer, 7, false),
            // The peer lives in a descendant namespace.
            (own, 40, "NSpid:\t7\t1\n", 7, false),
            (own, 40, "NSpid:\t8\n", 7, false),
            // An exited peer, or a kernel without NSpid in pidfd fdinfo.
            (own, 40, "Pid:\t-1\nNSpid:\t-1\n", 7, false),
            (own, 40, "pos:\t0\nPid:\t7\n", 7, false),
            (own, 41, peer, 7, false),
            ("Name:\tcagentsd\n", 40, peer, 7, false),
            ("NSpid:\t40\nNSpid:\t40\n", 40, peer, 7, false),
        ];
        for (own, own_pid, peer, peer_pid, proved) in cases {
            assert_eq!(
                same_namespace(own, own_pid, peer, peer_pid),
                proved,
                "{own:?} {peer:?}"
            );
        }
    }

    #[test]
    fn namespace_reports_resolve_only_within_one_procfs_mount() {
        let procfs = Procfs::open().unwrap();
        assert!(procfs.read("self/status").unwrap().contains("NSpid:"));
        assert_eq!(
            procfs.read("../etc/hostname").unwrap_err().raw_os_error(),
            Some(libc::EXDEV)
        );
        // A magic link leads out of this mount just as a bind mount would.
        assert_eq!(
            procfs
                .read("self/cwd/Cargo.toml")
                .unwrap_err()
                .raw_os_error(),
            Some(libc::ELOOP)
        );
    }

    #[test]
    fn the_os_user_manager_is_proved_without_ptrace_access() {
        if !manager_available() {
            eprintln!("no OS user manager; scope isolation unqualified here");
            return;
        }
        let own = Procfs::open().unwrap().read("self/status").unwrap();
        if !own.lines().any(|line| {
            line.strip_prefix("NSpid:")
                .is_some_and(|pids| pids.split_whitespace().count() == 1)
        }) {
            eprintln!("/proc views an ancestor PID namespace; scope isolation unqualified here");
            return;
        }
        match ManagerNamespace::connect() {
            Ok(_) => {}
            // An unavailable peer PID does not establish its underlying cause.
            Err(error) if error.to_string() == INVISIBLE_MANAGER => {
                eprintln!(
                    "OS user manager peer PID is unavailable; scope isolation unqualified here"
                );
            }
            Err(error) => panic!("OS user manager proof failed: {error}"),
        }
    }

    #[test]
    fn kernel_peer_identity_refuses_an_unowned_connection_before_requesting() {
        let (connection, _peer) = UnixStream::pair().unwrap();
        let wrong_uid = super::super::current_uid().wrapping_add(1);
        let proof = ManagerNamespace::from_connection(connection, wrong_uid);
        assert_eq!(proof.as_ref().err().unwrap().to_string(), UNOWNED_MANAGER);
        let refused =
            with_verified_manager(proof, || panic!("wrong peer must not receive a request"))
                .unwrap_err();
        assert_eq!(refused.kind(), io::ErrorKind::Other);
        assert_eq!(refused.raw_os_error(), None);
        assert_eq!(
            refused.to_string(),
            format!("user manager proof failed: {UNOWNED_MANAGER}")
        );
    }

    #[test]
    fn real_same_namespace_peer_is_proved_or_optional_kernel_support_fails_closed() {
        let (connection, _peer) = UnixStream::pair().unwrap();
        let proof = ManagerNamespace::from_connection(connection, super::super::current_uid());
        match proof {
            Ok(proof) => {
                let peer = proof.peer.as_raw_fd();
                let connection = proof._connection.as_raw_fd();
                let mut requested = false;
                let error = with_verified_manager(Ok(proof), || {
                    for descriptor in [peer, connection] {
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
                assert!(matches!(error.raw_os_error(), Some(libc::ENOPROTOOPT)));
                eprintln!("kernel manager proof unavailable; no scope request: {error}");
                with_verified_manager(Err(error), || panic!("unproved peer request")).unwrap_err();
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
