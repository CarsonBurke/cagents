//! Synchronous daemon discovery/startup shared by the lean CLI and UI.
//! Attachment reactors are compiled only in the host and UI lanes.
use std::{io, os::unix::{net::UnixStream, process::CommandExt}, path::{Path, PathBuf}, process::{Child, Command, ExitStatus, Stdio}, thread, time::{Duration, Instant}};
use thiserror::Error;
use super::ipc::{PROTOCOL_VERSION, daemon_socket_path};
const DAEMON_READY_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Error)]
pub enum HostedTerminalStartError {
    #[error("the expected cagents daemon proof is incomplete")]
    InvalidProof,
    #[error("could not locate the cagents daemon socket: {0}")]
    SocketPath(#[source] io::Error),
    #[error("could not start the hosted terminal client: {0}")]
    Worker(#[source] io::Error),
    #[error("could not locate the running application binary: {0}")]
    CurrentExecutable(#[source] io::Error),
    #[error("the sibling cagentsd binary does not exist at {0}")]
    HostBinaryMissing(PathBuf),
    #[error("could not launch cagentsd at {path}: {source}")]
    LaunchHost {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("cagents daemon did not become ready at {path} within {timeout:?}")]
    HostReadyTimeout { path: PathBuf, timeout: Duration },
    #[error("could not complete cagents daemon startup: {0}")]
    HostStartup(String),
    #[error("could not verify the running cagents daemon: {0}")]
    HostProtocol(#[source] crate::control::ControlError),
    #[error(
        "the running daemon uses protocol {actual}; this executable expects {PROTOCOL_VERSION}. Rebuild the matched executables; for a development session, run scripts/run-development --fresh-daemon (this stops its jobs and background subagents)"
    )]
    HostIncompatible { actual: u16 },
    #[error("cagents daemon exited before becoming ready at {path}: {status}")]
    HostExited { path: PathBuf, status: ExitStatus },
    #[error("could not connect to the cagents daemon socket at {path}: {source}")]
    HostSocket {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
}

/// Returns the per-user daemon socket, starting the sibling `cagentsd` if necessary.
///
/// Ordinary startup is bounded to five seconds; upgrade restoration receives a
/// bounded per-session allowance. On systemd hosts the daemon runs in its own
/// user scope, keeping UI cgroup shutdown and memory-pressure kills away from
/// hosted sessions. Other hosts retain the detached process-group fallback.
pub fn ensure_daemon_ready() -> Result<PathBuf, HostedTerminalStartError> {
    ensure_daemon_ready_for(crate::notifications::Application::Cagents)
}

pub fn ensure_daemon_ready_for(
    application: crate::notifications::Application,
) -> Result<PathBuf, HostedTerminalStartError> {
    let socket = daemon_socket_path().map_err(HostedTerminalStartError::SocketPath)?;
    if !crate::device::is_remote() {
        ensure_daemon_ready_at(&socket, application)?;
    }
    Ok(socket)
}

fn ensure_daemon_ready_at(
    socket: &Path,
    application: crate::notifications::Application,
) -> Result<(), HostedTerminalStartError> {
    match UnixStream::connect(socket) {
        Ok(stream) => {
            // A listening socket can belong to an older development build.
            // Verify the wire protocol before opening a window whose actions
            // would otherwise all fail against that daemon.
            return verify_daemon_protocol(stream);
        }
        Err(error)
            if !matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Err(HostedTerminalStartError::HostSocket {
                path: socket.to_owned(),
                source: error,
            });
        }
        Err(_) => {}
    }

    let timeout = crate::upgrade::handoff::startup_timeout()
        .map_err(HostedTerminalStartError::HostStartup)?;
    let current_exe =
        std::env::current_exe().map_err(HostedTerminalStartError::CurrentExecutable)?;
    let daemon_binary = crate::companion::Product::Daemon
        .beside(&current_exe)
        .map_err(|_| HostedTerminalStartError::HostBinaryMissing(current_exe))?;
    if !daemon_binary.is_file() {
        return Err(HostedTerminalStartError::HostBinaryMissing(daemon_binary));
    }

    start_daemon_at(
        socket,
        timeout,
        &daemon_binary,
        systemd_user_manager_available()
            && !crate::instance::has_inherited_daemon_fence()
                .map_err(|error| HostedTerminalStartError::HostStartup(error.to_string()))?,
        |scoped| spawn_daemon(&daemon_binary, application, scoped),
    )?;
    // Another launcher can win the daemon lease while we wait for readiness;
    // it must satisfy the same protocol contract as an existing daemon.
    let stream =
        UnixStream::connect(socket).map_err(|source| HostedTerminalStartError::HostSocket {
            path: socket.to_owned(),
            source,
        })?;
    verify_daemon_protocol(stream)
}

fn verify_daemon_protocol(stream: UnixStream) -> Result<(), HostedTerminalStartError> {
    crate::control::ControlClient::from_stream(stream)
        .map(|_| ())
        .map_err(|error| match error {
            crate::control::ControlError::UnsupportedVersion { actual } => {
                HostedTerminalStartError::HostIncompatible { actual }
            }
            error => HostedTerminalStartError::HostProtocol(error),
        })
}

fn start_daemon_at(
    socket: &Path,
    timeout: Duration,
    daemon_binary: &Path,
    mut scoped: bool,
    mut launch: impl FnMut(bool) -> io::Result<Child>,
) -> Result<(), HostedTerminalStartError> {
    let launch_error = |source| HostedTerminalStartError::LaunchHost {
        path: daemon_binary.to_owned(),
        source,
    };
    let mut child = match launch(scoped) {
        Ok(child) => child,
        Err(source) if scoped => {
            tracing::warn!(%source, "could not launch daemon user scope; using process-group fallback");
            scoped = false;
            launch(false).map_err(launch_error)?
        }
        Err(source) => return Err(launch_error(source)),
    };
    let started = Instant::now();
    let result = loop {
        let status = match child.try_wait() {
            Ok(status) => status,
            Err(error) => break Err(HostedTerminalStartError::HostStartup(error.to_string())),
        };
        match daemon_startup_ready(socket, timeout, started.elapsed(), status) {
            Ok(true) => break Ok(()),
            Ok(false) => thread::sleep(
                DAEMON_READY_POLL_INTERVAL.min(timeout.saturating_sub(started.elapsed())),
            ),
            // A user manager may disappear or reject transient units after
            // our availability check. Fall back only after the launcher has
            // exited and no concurrent daemon owns the lease. Never terminate
            // an existing daemon or retry while restoration owns its lease.
            Err(HostedTerminalStartError::HostExited { status, .. })
                if scoped && started.elapsed() < timeout =>
            {
                tracing::warn!(%status, "daemon user scope exited before readiness; using process-group fallback");
                scoped = false;
                let _ = child.wait();
                child = match launch(false) {
                    Ok(child) => child,
                    Err(source) => break Err(launch_error(source)),
                };
            }
            Err(error) => break Err(error),
        }
    };
    let _ = thread::Builder::new()
        .name("cagentsd-reaper".into())
        .spawn(move || {
            let _ = child.wait();
        });
    result
}

fn systemd_user_manager_available() -> bool {
    // The private manager socket exists independently of a graphical session's
    // D-Bus environment. No shell or global systemd environment is involved.
    cfg!(target_os = "linux")
        && std::env::var_os("XDG_RUNTIME_DIR")
            .map(PathBuf::from)
            .is_some_and(|runtime| runtime.join("systemd/private").exists())
}

fn daemon_launch_command(
    daemon_binary: &Path,
    application: crate::notifications::Application,
    scoped: bool,
) -> Command {
    let mut command = if scoped {
        let mut command = Command::new("systemd-run");
        command.args([
            "--user",
            "--scope",
            "--quiet",
            "--collect",
            "--expand-environment=no",
        ]);
        // Scope names are per launch rather than per installation: detached
        // provider/terminal holders can outlive their daemon during upgrades.
        command.arg(format!("--unit=cagentsd-{}", uuid::Uuid::new_v4()));
        command.arg("--").arg(daemon_binary);
        command
    } else {
        Command::new(daemon_binary)
    };
    crate::install_guard::sanitize_child_environment(&mut command);
    if application == crate::notifications::Application::Ceditor {
        command.arg(crate::instance::CEDITOR_NOTIFICATION_ARGUMENT);
    }
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .process_group(0)
        // A scope executes locally, preserving this exact sanitized environment
        // instead of inheriting stale variables from the user service manager.
        .env_remove(crate::instance::UI_LAUNCH_HANDSHAKE_ENV);
    command
}

fn spawn_daemon(
    daemon_binary: &Path,
    application: crate::notifications::Application,
    scoped: bool,
) -> io::Result<Child> {
    let mut command = daemon_launch_command(daemon_binary, application, scoped);
    // SAFETY: the hook performs only the close_range syscall before exec.
    unsafe {
        command.pre_exec(crate::daemon::seal_inherited_descriptors);
    }
    crate::instance::inherit_daemon_fence(&mut command)?;
    command.spawn()
}

fn daemon_startup_ready(
    socket: &Path,
    timeout: Duration,
    elapsed: Duration,
    child_status: Option<ExitStatus>,
) -> Result<bool, HostedTerminalStartError> {
    match UnixStream::connect(socket) {
        Ok(_) => return Ok(true),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
            ) => {}
        Err(source) => {
            return Err(HostedTerminalStartError::HostSocket {
                path: socket.to_owned(),
                source,
            });
        }
    }
    if let Some(status) = child_status {
        // A concurrent launch can win the lease while our child exits. Its
        // listener is deliberately absent until every session has restored.
        if !crate::daemon::HostLease::is_held_at(socket)
            .map_err(|error| HostedTerminalStartError::HostStartup(error.to_string()))?
        {
            return Err(HostedTerminalStartError::HostExited {
                path: socket.to_owned(),
                status,
            });
        }
    }
    if elapsed >= timeout {
        return Err(HostedTerminalStartError::HostReadyTimeout {
            path: socket.to_owned(),
            timeout,
        });
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::net::UnixListener;
    use crate::model::{HostSessionProof, HostSessionId, BootId, HostIncarnation};
    use super::super::ipc::{ClientEnvelope, ServerEnvelope, ServerMessage, ServerResponse, read_frame, write_frame};
    const DAEMON_READY_TIMEOUT: Duration = Duration::from_secs(5);

    #[test]
    fn startup_waits_for_six_second_restoration_before_accepting_listener() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let timeout = Duration::from_secs(35);
        assert!(!daemon_startup_ready(&socket, timeout, Duration::from_secs(6), None).unwrap());
        let _listener = UnixListener::bind(&socket).unwrap();
        assert!(daemon_startup_ready(&socket, timeout, Duration::from_secs(6), None).unwrap());
    }

    fn hold_startup_lease(socket: &Path) -> crate::daemon::HostLease {
        use std::os::unix::fs::OpenOptionsExt;

        let paths = crate::daemon::RuntimePaths {
            #[cfg(feature = "host")]
            directory: socket.parent().unwrap().to_owned(),
            socket: socket.to_owned(),
            lock: socket.with_file_name("cagentsd.lock"),
        };
        std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .mode(0o600)
            .open(&paths.lock)
            .unwrap();
        // Match the daemon's explicit unlock: a concurrent fork can still own
        // a copy of the descriptor after this thread drops its copy.
        crate::daemon::HostLease::acquire_for_maintenance(&paths).unwrap()
    }

    #[test]
    fn unavailable_scope_launcher_falls_back_once() {
        for spawn_error in [false, true] {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("daemon.sock");
            let mut attempts = Vec::new();
            let mut listener = None;
            start_daemon_at(
                &socket,
                Duration::from_secs(2),
                Path::new("fixture-daemon"),
                true,
                |scoped| {
                    attempts.push(scoped);
                    if scoped && spawn_error {
                        return Err(io::Error::from(io::ErrorKind::NotFound));
                    }
                    if !scoped {
                        listener = Some(UnixListener::bind(&socket).unwrap());
                    }
                    Command::new("/bin/false").spawn()
                },
            )
            .unwrap();
            assert_eq!(attempts, [true, false]);
        }
    }

    #[test]
    fn failed_scope_and_direct_launch_stop_after_one_fallback() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let mut attempts = Vec::new();
        let result = start_daemon_at(
            &socket,
            Duration::from_secs(2),
            Path::new("fixture-daemon"),
            true,
            |scoped| {
                attempts.push(scoped);
                Command::new("/bin/false").spawn()
            },
        );
        assert!(matches!(
            result,
            Err(HostedTerminalStartError::HostExited { .. })
        ));
        assert_eq!(attempts, [true, false]);
    }

    #[test]
    fn scoped_launch_failure_does_not_replace_a_restoring_daemon() {
        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let _lease = hold_startup_lease(&socket);
        let mut attempts = Vec::new();
        let result = start_daemon_at(
            &socket,
            Duration::from_millis(75),
            Path::new("fixture-daemon"),
            true,
            |scoped| {
                attempts.push(scoped);
                Command::new("/bin/false").spawn()
            },
        );
        assert!(matches!(
            result,
            Err(HostedTerminalStartError::HostReadyTimeout { .. })
        ));
        assert_eq!(attempts, [true]);
    }

    #[test]
    fn existing_daemon_is_preserved_before_launch_selection() {
        for version in [PROTOCOL_VERSION, PROTOCOL_VERSION - 1] {
            let directory = tempfile::tempdir().unwrap();
            let socket = directory.path().join("daemon.sock");
            let listener = UnixListener::bind(&socket).unwrap();
            let server = thread::spawn(move || {
                let mut stream = accept_connection(&listener);
                let request: ClientEnvelope = read_frame(&mut stream).unwrap().unwrap();
                let proof = proof("existing");
                write_frame(
                    &mut stream,
                    &ServerEnvelope {
                        version,
                        request_id: Some(request.request_id),
                        proof: proof.clone(),
                        message: ServerMessage::Response {
                            response: ServerResponse::Connected {
                                proof,
                                resumed: Vec::new(),
                            },
                        },
                    },
                )
                .unwrap();
            });
            let result =
                ensure_daemon_ready_at(&socket, crate::notifications::Application::Cagents);
            if version == PROTOCOL_VERSION {
                result.unwrap();
            } else {
                assert!(
                    matches!(result, Err(HostedTerminalStartError::HostIncompatible {
                    actual
                }) if actual == version)
                );
            }
            server.join().unwrap();
        }
    }

    #[test]
    fn systemd_scope_preserves_environment_arguments_and_isolates_cgroup() {
        if !systemd_user_manager_available() {
            return;
        }
        let directory = tempfile::tempdir().unwrap();
        let output = directory.path().join("scope-evidence");
        let mut command = daemon_launch_command(
            Path::new("/bin/sh"),
            crate::notifications::Application::Cagents,
            true,
        );
        // This argument and environment value must survive systemd without
        // manager-side variable expansion or replacement by manager state.
        command
            .env("CAGENTS_SCOPE_TEST", "caller-value")
            .args(["-c", "cat /proc/self/cgroup > \"$1\"; printf '%s\\n%s\\n' \"$2\" \"$CAGENTS_SCOPE_TEST\" >> \"$1\"; sleep 30 & echo $! >> \"$1\"", "fixture"])
            .arg(&output)
            .arg("$HOME");
        let status = command.status().unwrap();
        assert!(status.success(), "scope launch failed: {status}");
        let evidence = std::fs::read_to_string(output).unwrap();
        // A detached holder must survive a daemon handoff. Scope launchers
        // return when the main process exits and leave its children in place.
        let holder_pid: i32 = evidence.lines().nth(3).unwrap().parse().unwrap();
        let holder_cgroup = std::fs::read_to_string(format!("/proc/{holder_pid}/cgroup"));
        // SAFETY: this PID belongs to the fixture's own background sleep.
        unsafe { libc::kill(holder_pid, libc::SIGTERM) };
        let holder_cgroup = holder_cgroup.unwrap();
        let caller = std::fs::read_to_string("/proc/self/cgroup").unwrap();
        let mut lines = evidence.lines();
        let scope = lines.next().unwrap();
        assert_ne!(scope, caller.trim());
        assert!(
            scope.contains("cagentsd-") && scope.ends_with(".scope"),
            "{scope}"
        );
        assert_eq!(lines.next(), Some("$HOME"));
        assert_eq!(lines.next(), Some("caller-value"));
        assert_eq!(holder_cgroup.trim(), scope);
    }

    #[test]
    fn exited_launcher_waits_for_winners_sequential_restoration() {
        use std::os::unix::process::ExitStatusExt;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let _lease = hold_startup_lease(&socket);
        let timeout = Duration::from_secs(65);
        let loser = Some(ExitStatus::from_raw(1 << 8));
        // Two providers may each consume thirty seconds. The winner owns the
        // inventory throughout; even incomplete lease metadata is not failure.
        assert!(!daemon_startup_ready(&socket, timeout, Duration::from_secs(30), loser).unwrap());
        assert!(!daemon_startup_ready(&socket, timeout, Duration::from_secs(60), loser).unwrap());
        let _listener = UnixListener::bind(&socket).unwrap();
        assert!(daemon_startup_ready(&socket, timeout, Duration::from_secs(61), loser).unwrap());
    }

    #[test]
    fn exited_startup_fails_without_owner_and_owner_cannot_extend_deadline() {
        use std::os::unix::process::ExitStatusExt;

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let timeout = Duration::from_secs(35);
        let exited = Some(ExitStatus::from_raw(1 << 8));
        assert!(matches!(
            daemon_startup_ready(&socket, timeout, Duration::from_secs(1), exited),
            Err(HostedTerminalStartError::HostExited { .. })
        ));
        let lease = hold_startup_lease(&socket);
        assert!(matches!(
            daemon_startup_ready(&socket, timeout, timeout, exited),
            Err(HostedTerminalStartError::HostReadyTimeout { .. })
        ));
        drop(lease);
        // A stale lock file must not disguise a failed restoration as a winner.
        assert!(matches!(
            daemon_startup_ready(&socket, timeout, Duration::from_secs(2), exited),
            Err(HostedTerminalStartError::HostExited { .. })
        ));
        assert!(matches!(
            daemon_startup_ready(&socket, DAEMON_READY_TIMEOUT, DAEMON_READY_TIMEOUT, None,),
            Err(HostedTerminalStartError::HostReadyTimeout { .. })
        ));
    }

    #[test]
    fn startup_lease_release_does_not_wait_for_concurrent_pre_exec() {
        use std::io::{Read, Write};
        use std::os::{fd::AsRawFd, unix::process::ExitStatusExt};

        let directory = tempfile::tempdir().unwrap();
        let socket = directory.path().join("daemon.sock");
        let lease = hold_startup_lease(&socket);
        let (ready_write, mut ready_read) = UnixStream::pair().unwrap();
        let (gate_read, mut gate_write) = UnixStream::pair().unwrap();
        ready_read
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        let mut command = Command::new("/bin/true");
        // SAFETY: the forked child uses only async-signal-safe syscalls and
        // initialized stack buffers until exec. Bound its wait even if the
        // parent test fails before releasing it.
        unsafe {
            command.pre_exec(move || {
                let mut byte = [1_u8];
                loop {
                    if libc::write(ready_write.as_raw_fd(), byte.as_ptr().cast(), 1) == 1 {
                        break;
                    }
                    let error = io::Error::last_os_error();
                    if error.kind() != io::ErrorKind::Interrupted {
                        return Err(error);
                    }
                }
                let mut gate = libc::pollfd {
                    fd: gate_read.as_raw_fd(),
                    events: libc::POLLIN,
                    revents: 0,
                };
                // Instant uses the async-signal-safe monotonic clock on Unix.
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    let remaining = deadline.saturating_duration_since(Instant::now());
                    if remaining.is_zero() {
                        return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT));
                    }
                    match libc::poll(&mut gate, 1, remaining.as_millis().max(1) as i32) {
                        1 => break,
                        0 => return Err(io::Error::from_raw_os_error(libc::ETIMEDOUT)),
                        _ => {
                            let error = io::Error::last_os_error();
                            if error.kind() != io::ErrorKind::Interrupted {
                                return Err(error);
                            }
                        }
                    }
                }
                loop {
                    match libc::read(gate.fd, byte.as_mut_ptr().cast(), 1) {
                        1 => break,
                        0 => return Err(io::Error::from_raw_os_error(libc::EPIPE)),
                        _ => {
                            let error = io::Error::last_os_error();
                            if error.kind() != io::ErrorKind::Interrupted {
                                return Err(error);
                            }
                        }
                    }
                }
                Ok(())
            });
        }
        let child = thread::spawn(move || command.status().unwrap());
        ready_read.read_exact(&mut [0_u8]).unwrap();
        drop(lease);
        // The child still holds the inherited descriptor here. A bare File
        // drop would leave the lease locked and incorrectly keep startup alive.
        let result = daemon_startup_ready(
            &socket,
            Duration::from_secs(35),
            Duration::from_secs(1),
            Some(ExitStatus::from_raw(1 << 8)),
        );
        gate_write.write_all(&[1]).unwrap();
        assert!(child.join().unwrap().success());
        assert!(
            matches!(result, Err(HostedTerminalStartError::HostExited { .. })),
            "{result:?}"
        );
    }

    fn proof(incarnation: &str) -> HostSessionProof {
        HostSessionProof {
            host_session_id: HostSessionId::new("session"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new(incarnation),
        }
    }

    fn accept_connection(listener: &UnixListener) -> UnixStream {
        listener.set_nonblocking(true).unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match listener.accept() {
                Ok((stream, _)) => {
                    stream
                        .set_read_timeout(Some(Duration::from_secs(2)))
                        .unwrap();
                    return stream;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    assert!(Instant::now() < deadline, "client did not connect");
                    thread::sleep(Duration::from_millis(1));
                }
                Err(error) => panic!("could not accept client: {error}"),
            }
        }
    }

}
