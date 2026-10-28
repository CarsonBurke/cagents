//! Real OS-worker custody must not serialize healthy terminal shutdown.
use super::*;
use crate::{
    model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal::{TerminalConfig, TerminalHandle, TerminalSendError},
};
use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

const ROOT: &str = "CAGENTS_SHUTDOWN_BATCH_ROOT";
const SCOPE: &str = "CAGENTS_SHUTDOWN_BATCH_AUTHORITY_PRESENCE";

struct TestChild(Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

// Keep private runtime paths and captured child output until the external
// command owner has checked the whole family, on success as well as failure.
struct EvidenceDirectory(Option<tempfile::TempDir>);

impl Drop for EvidenceDirectory {
    fn drop(&mut self) {
        if let Some(directory) = self.0.take() {
            let _retained = directory.keep();
        }
    }
}

fn private_runtime(test: &str) -> Option<PathBuf> {
    if let Some(root) = std::env::var_os(ROOT) {
        let expected = match std::env::var(SCOPE).as_deref() {
            Ok("present") => true,
            Ok("absent") => false,
            _ => panic!("missing or invalid shutdown fixture authority presence marker"),
        };
        assert_eq!(
            crate::agent_execution::Authority::from_environment()
                != crate::agent_execution::Authority::default(),
            expected,
            "isolated shutdown fixture lost its inherited authority scope"
        );
        return Some(PathBuf::from(root).canonicalize().unwrap());
    }

    // Honour the command owner's legitimate non-repository TMPDIR. A public
    // /tmp or /var/tmp checkout marker is neither removed nor bypassed.
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().canonicalize().unwrap();
    let _evidence = EvidenceDirectory(Some(directory));
    fs::set_permissions(&root, fs::Permissions::from_mode(0o700)).unwrap();
    for name in ["home", "run", "data", "config", "cache", "tmp"] {
        fs::create_dir(root.join(name)).unwrap();
        fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
    }
    let stdout = root.join("test.stdout");
    let stderr = root.join("test.stderr");
    let mut command = Command::new(std::env::current_exe().unwrap());
    // Leave delegation inherited and opaque. Only public Authority equality
    // crosses the boundary as a boolean assertion; no capability is copied.
    for (name, _) in std::env::vars_os() {
        if name != std::ffi::OsStr::new(crate::agent_execution::AUTHORITY_ENV) {
            command.env_remove(name);
        }
    }
    command
        .args(["--exact", test, "--nocapture", "--test-threads=1"])
        .current_dir(&root)
        .env(ROOT, &root)
        .env(
            SCOPE,
            if crate::agent_execution::Authority::from_environment()
                != crate::agent_execution::Authority::default()
            {
                "present"
            } else {
                "absent"
            },
        )
        .env("HOME", root.join("home"))
        .env("XDG_RUNTIME_DIR", root.join("run"))
        .env("XDG_DATA_HOME", root.join("data"))
        .env("XDG_CONFIG_HOME", root.join("config"))
        .env("XDG_CACHE_HOME", root.join("cache"))
        .env("TMPDIR", root.join("tmp"))
        .env("PATH", "/usr/bin:/bin")
        .stdin(Stdio::null())
        .stdout(Stdio::from(File::create(&stdout).unwrap()))
        .stderr(Stdio::from(File::create(&stderr).unwrap()));
    let mut child = TestChild(command.spawn().unwrap());
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        if let Some(status) = child.0.try_wait().unwrap() {
            break Some(status);
        }
        if Instant::now() >= deadline {
            child.0.kill().unwrap();
            child.0.wait().unwrap();
            break None;
        }
        thread::sleep(Duration::from_millis(10));
    };
    let stdout = fs::read_to_string(stdout).unwrap();
    let stderr = fs::read_to_string(stderr).unwrap();
    assert!(
        status.is_some_and(|status| status.success()),
        "isolated shutdown fixture failed or exceeded 30s; retained {}:\n{stdout}\n{stderr}",
        root.display()
    );
    assert!(
        stdout.contains(&format!("test {test} ... ok")),
        "missing exact child test; retained {}:\n{stdout}",
        root.display()
    );
    assert!(
        stdout.contains("test result: ok. 1 passed; 0 failed; 0 ignored;"),
        "zero or incorrect child selection; retained {}:\n{stdout}",
        root.display()
    );
    None
}

fn session(id: u128, root: &Path) -> TerminalSession {
    let recipe = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "printf '%s' \"$$\" > root-pid; exec /bin/sleep 600".into(),
        ],
        working_directory: root.to_owned(),
        environment: Default::default(),
        unset_environment: Default::default(),
    };
    TerminalSession {
        id: TerminalId::from_uuid(Uuid::from_u128(id)),
        launch: recipe.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe,
            last_exit: None,
        },
    }
}

fn start_time(pid: u32) -> io::Result<String> {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat"))?;
    Ok(stat
        .rsplit_once(')')
        .unwrap()
        .1
        .split_whitespace()
        .nth(19)
        .unwrap()
        .into())
}

// A failure guard addresses only the exact private root through its pidfd.
// Normal success proves the original reaped and disarms it without a signal.
struct RootPin {
    descriptor: File,
    retired: bool,
}

impl RootPin {
    fn exit_ready(&self) -> io::Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.descriptor.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(result == 1 && descriptor.revents & libc::POLLIN != 0)
        }
    }
}

impl Drop for RootPin {
    fn drop(&mut self) {
        if !self.retired && !matches!(self.exit_ready(), Ok(true)) {
            let result = unsafe {
                libc::syscall(
                    libc::SYS_pidfd_send_signal,
                    self.descriptor.as_raw_fd(),
                    libc::SIGKILL,
                    std::ptr::null::<libc::siginfo_t>(),
                    0,
                )
            };
            use std::io::Write;
            let _ = writeln!(
                io::stderr(),
                "private PTY failure cleanup used its pinned pidfd: {result}"
            );
        }
    }
}

// Sender destruction releases the actual blocking OS-thread body, including
// unwind before thread::scope waits. It never substitutes a completion bit.
struct ReleaseWorkers(Vec<crossbeam_channel::Sender<()>>);

fn unfinished_workers_do_not_starve_healthy(root: &Path, count: usize) {
    let baseline = TerminalHandle::reserved_worker_joins_for_test();
    assert_eq!(baseline, 0, "the isolated child must start without workers");
    assert_eq!(TerminalHandle::worker_join_capacity_for_test(), 1024);
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("shutdown-batch"),
        boot_id: crate::model::BootId::new("shutdown-batch"),
        host_incarnation: HostIncarnation::new("shutdown-batch"),
    });
    state.register_client(1);
    let mut blocked = Vec::new();
    let mut releases = Vec::new();
    for id in 1..=count {
        let (handle, release) = TerminalHandle::spawn_with_wedged_worker_for_test();
        state
            .finish_spawn(1, session(id as u128, root), handle.clone(), true)
            .unwrap();
        blocked.push(handle);
        releases.push(release);
    }
    let healthy_session = session((count + 1) as u128, root);
    let healthy =
        TerminalHandle::spawn(healthy_session.clone(), TerminalConfig::default(), |_| {}).unwrap();
    state
        .finish_spawn(1, healthy_session, healthy.clone(), true)
        .unwrap();
    let ready_by = Instant::now() + Duration::from_secs(5);
    let pid: u32 = loop {
        if let Ok(pid) = fs::read_to_string(root.join("root-pid"))
            && let Ok(pid) = pid.trim().parse()
        {
            break pid;
        }
        assert!(Instant::now() < ready_by, "private PTY root did not start");
        thread::sleep(Duration::from_millis(5));
    };
    let original_start = start_time(pid).unwrap();
    assert_eq!(fs::read_link(format!("/proc/{pid}/cwd")).unwrap(), root);
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(descriptor >= 0, "{}", io::Error::last_os_error());
    let root_pin = RootPin {
        descriptor: unsafe { File::from_raw_fd(descriptor as i32) },
        retired: false,
    };
    assert_eq!(start_time(pid).unwrap(), original_start);
    assert_eq!(
        TerminalHandle::reserved_worker_joins_for_test(),
        baseline + count + 1
    );
    let expected: Vec<_> = (1..=count + 1)
        .map(|id| TerminalId::from_uuid(Uuid::from_u128(id as u128)))
        .collect();
    assert_eq!(
        state.terminals.keys().copied().collect::<Vec<_>>(),
        expected
    );
    let (handles, _) = state.graceful_shutdown();
    assert_eq!(handles.len(), count + 1);

    thread::scope(|scope| {
        let mut release = ReleaseWorkers(releases);
        let mut root_pin = root_pin;
        let (finished, result) = mpsc::sync_channel(1);
        let started = Instant::now();
        let batch = scope.spawn(move || {
            let _ = finished.send(finish_terminal_shutdown(&handles));
        });
        // Both unfinished bodies precede the real PTY in terminal-ID order.
        // They must not occupy the fixed join lanes or defer its shutdown.
        let healthy_by = started + Duration::from_secs(3);
        loop {
            let reaped = match start_time(pid) {
                Ok(start) => start != original_start,
                Err(error)
                    if error.kind() == io::ErrorKind::NotFound
                        || error.raw_os_error() == Some(libc::ESRCH) =>
                {
                    true
                }
                Err(error) => panic!("could not inspect the private PTY root: {error}"),
            };
            if reaped && healthy.worker_joined_for_test() {
                assert!(root_pin.exit_ready().unwrap());
                root_pin.retired = true;
                break;
            }
            assert!(
                Instant::now() < healthy_by,
                "healthy PTY root and actual worker were not reaped while earlier bodies blocked"
            );
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
        assert!(healthy.is_completed());
        assert!(blocked.iter().all(|worker| !worker.is_completed()));
        assert!(
            blocked
                .iter()
                .all(|worker| !worker.worker_joined_for_test())
        );
        let batch_by = started + Duration::from_secs(8);
        let outcomes = result
            .recv_timeout(batch_by.saturating_duration_since(Instant::now()))
            .expect(
                "shutdown exceeded its single shared five-second deadline and settlement margin",
            );
        assert!(started.elapsed() < Duration::from_secs(8));
        assert_eq!(outcomes.len(), count + 1);
        for outcome in &outcomes[..count] {
            assert_eq!(*outcome, Err(TerminalSendError::Closed));
        }
        assert_eq!(outcomes[count], Ok(()));
        assert_eq!(
            TerminalHandle::reserved_worker_joins_for_test(),
            baseline + count
        );
        assert!(blocked.iter().all(|worker| !worker.is_completed()));
        assert!(
            blocked
                .iter()
                .all(|worker| !worker.worker_joined_for_test())
        );
        release.0.clear();
        let released_by = Instant::now() + Duration::from_secs(5);
        while blocked
            .iter()
            .any(|worker| !worker.worker_joined_for_test())
            || TerminalHandle::reserved_worker_joins_for_test() != baseline
        {
            assert!(
                Instant::now() < released_by,
                "released OS workers did not join"
            );
            thread::sleep(Duration::from_millis(5));
        }
        // These factories have no PTY completion to certify, even after their
        // actual threads joined. Cached handles do not retain admission tickets.
        for worker in &blocked {
            assert!(!worker.is_completed());
            assert_eq!(worker.shutdown(), Err(TerminalSendError::Closed));
            assert!(worker.worker_joined_for_test());
            let _cached_screen = worker.snapshot();
        }
        assert_eq!(healthy.shutdown(), Ok(()));
        assert_eq!(TerminalHandle::reserved_worker_joins_for_test(), baseline);
        batch.join().unwrap();
        fs::write(
            root.join("worker-evidence.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "blocked_workers": count,
                "healthy_root_pid": pid,
                "healthy_root_start": original_start,
                "healthy_root_reaped": true,
                "healthy_worker_joined": healthy.worker_joined_for_test(),
                "all_released_workers_joined": blocked.iter().all(|worker| worker.worker_joined_for_test()),
                "reserved_worker_joins": TerminalHandle::reserved_worker_joins_for_test(),
                "root_pin_disarmed_after_reap": root_pin.retired,
            }))
            .unwrap(),
        )
        .unwrap();
    });
}

#[test]
fn shutdown_batch_continues_after_an_unfinished_worker() {
    const TEST: &str = "host::shutdown_tests::shutdown_batch_continues_after_an_unfinished_worker";
    let Some(root) = private_runtime(TEST) else {
        return;
    };
    unfinished_workers_do_not_starve_healthy(&root, 1);
}

#[test]
fn shutdown_batch_two_blocked_workers_do_not_starve_healthy_cleanup() {
    const TEST: &str =
        "host::shutdown_tests::shutdown_batch_two_blocked_workers_do_not_starve_healthy_cleanup";
    let Some(root) = private_runtime(TEST) else {
        return;
    };
    unfinished_workers_do_not_starve_healthy(&root, 2);
}

#[test]
fn shutdown_batch_observes_a_real_worker_panic_without_retaining_its_ticket() {
    const TEST: &str = "host::shutdown_tests::shutdown_batch_observes_a_real_worker_panic_without_retaining_its_ticket";
    let Some(_root) = private_runtime(TEST) else {
        return;
    };
    let baseline = TerminalHandle::reserved_worker_joins_for_test();
    assert_eq!(baseline, 0);
    let (worker, release) = TerminalHandle::spawn_with_panicking_worker_for_test();
    let mut release = ReleaseWorkers(vec![release]);
    assert_eq!(
        TerminalHandle::reserved_worker_joins_for_test(),
        baseline + 1
    );
    assert!(!worker.is_completed());
    assert!(!worker.worker_joined_for_test());
    worker.request_shutdown();
    release.0.clear();
    let deadline = Instant::now() + Duration::from_secs(5);
    while TerminalHandle::reserved_worker_joins_for_test() != baseline {
        assert!(
            Instant::now() < deadline,
            "panicked worker join was not observed"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert!(!worker.is_completed());
    assert!(
        !worker.worker_joined_for_test(),
        "a panicked join cannot be successful"
    );
    assert_eq!(
        finish_terminal_shutdown(&[worker.clone()]),
        [Err(TerminalSendError::Closed)]
    );
    assert_eq!(worker.shutdown(), Err(TerminalSendError::Closed));
    assert!(!worker.worker_joined_for_test());
    assert_eq!(TerminalHandle::reserved_worker_joins_for_test(), baseline);
    let _cached_screen = worker.snapshot();
}

#[test]
fn shutdown_batch_requires_actual_join_after_a_completed_body_blocks_in_tls() {
    const TEST: &str = "host::shutdown_tests::shutdown_batch_requires_actual_join_after_a_completed_body_blocks_in_tls";
    let Some(root) = private_runtime(TEST) else {
        return;
    };
    let baseline = TerminalHandle::reserved_worker_joins_for_test();
    assert_eq!(baseline, 0);
    let (worker, release, entered) = TerminalHandle::spawn_with_blocked_tls_for_test();
    let mut release = ReleaseWorkers(vec![release]);
    entered
        .recv_timeout(Duration::from_secs(5))
        .expect("the actual OS thread did not enter its TLS destructor");
    // Body/process publication is not an OS-thread join receipt. This control
    // has no PTY; the preceding cases independently prove real root reaping.
    assert!(worker.is_completed());
    assert!(!worker.worker_joined_for_test());
    assert_eq!(
        TerminalHandle::reserved_worker_joins_for_test(),
        baseline + 1
    );
    let _cached_screen = worker.snapshot();
    let started = Instant::now();
    let unconfirmed = worker.close_checked(true);
    assert!(
        matches!(&unconfirmed, crate::terminal_ipc::TerminalCloseResult::Unknown(detail) if !detail.is_empty()),
        "a blocked TLS destructor cannot certify Close: {unconfirmed:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(8));
    assert!(worker.is_completed());
    assert!(!worker.worker_joined_for_test());
    assert_eq!(
        TerminalHandle::reserved_worker_joins_for_test(),
        baseline + 1
    );
    release.0.clear();
    let deadline = Instant::now() + Duration::from_secs(5);
    while !worker.worker_joined_for_test()
        || TerminalHandle::reserved_worker_joins_for_test() != baseline
    {
        assert!(
            Instant::now() < deadline,
            "released TLS thread did not actually join"
        );
        thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        worker.close_checked(true),
        crate::terminal_ipc::TerminalCloseResult::Closed
    );
    assert_eq!(worker.shutdown(), Ok(()));
    assert!(worker.worker_joined_for_test());
    assert_eq!(TerminalHandle::reserved_worker_joins_for_test(), baseline);
    fs::write(
        root.join("worker-evidence.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "real_tls_destructor_entered": true,
            "body_completed_before_join": true,
            "initial_close": format!("{unconfirmed:?}"),
            "final_worker_joined": worker.worker_joined_for_test(),
            "reserved_worker_joins": TerminalHandle::reserved_worker_joins_for_test(),
            "pty_scope": "none in this TLS-only control",
        }))
        .unwrap(),
    )
    .unwrap();
}
