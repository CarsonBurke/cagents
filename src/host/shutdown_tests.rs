use super::*;
use crate::{
    model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal::{TerminalConfig, TerminalHandle, TerminalSendError},
};
use std::{
    fs::File,
    os::fd::{AsRawFd, FromRawFd},
    path::Path,
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

const TEST: &str = "host::shutdown_tests::shutdown_batch_continues_after_an_unfinished_worker";
const ROOT: &str = "CAGENTS_SHUTDOWN_BATCH_ROOT";

struct TestChild(Child);

impl Drop for TestChild {
    fn drop(&mut self) {
        if self.0.try_wait().ok().flatten().is_none() {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

fn session(id: u128, root: &Path) -> TerminalSession {
    let recipe = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec![
            "-c".into(),
            "printf '%s\\n' \"$$\" > root-pid; exec /bin/sleep 600".into(),
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

// The fixture root writes its PID in its private cwd. Pin its incarnation
// before starting shutdown; failure cleanup never signals a numeric PID.
struct RootPin(File);

impl Drop for RootPin {
    fn drop(&mut self) {
        unsafe {
            libc::syscall(
                libc::SYS_pidfd_send_signal,
                self.0.as_raw_fd(),
                libc::SIGKILL,
                std::ptr::null::<libc::siginfo_t>(),
                0,
            );
        }
    }
}

fn start_time(pid: u32) -> String {
    let stat = fs::read_to_string(format!("/proc/{pid}/stat")).unwrap();
    stat.rsplit_once(')').unwrap().1.split_whitespace().nth(19).unwrap().into()
}

#[test]
fn shutdown_batch_continues_after_an_unfinished_worker() {
    let Some(root) = std::env::var_os(ROOT) else {
        // Do not mutate process-wide XDG variables in a parallel test runner.
        let directory = tempfile::tempdir_in("/var/tmp").unwrap();
        let root = directory.path().canonicalize().unwrap();
        for name in ["home", "run", "data", "config", "cache"] {
            fs::create_dir(root.join(name)).unwrap();
            fs::set_permissions(root.join(name), fs::Permissions::from_mode(0o700)).unwrap();
        }
        let stdout = root.join("test.stdout");
        let stderr = root.join("test.stderr");
        let mut child = TestChild(Command::new(std::env::current_exe().unwrap())
            .args(["--exact", TEST, "--nocapture"])
            .env_clear()
            .env(ROOT, &root)
            .env("HOME", root.join("home"))
            .env("XDG_RUNTIME_DIR", root.join("run"))
            .env("XDG_DATA_HOME", root.join("data"))
            .env("XDG_CONFIG_HOME", root.join("config"))
            .env("XDG_CACHE_HOME", root.join("cache"))
            .env("PATH", "/usr/bin:/bin")
            .stdout(Stdio::from(File::create(&stdout).unwrap()))
            .stderr(Stdio::from(File::create(&stderr).unwrap()))
            .spawn()
            .unwrap());
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
        let failure_root = if status.is_some_and(|status| status.success()) {
            None
        } else {
            // A timed-out child may still have discoverable private holders;
            // keep their evidence rather than unlinking their private runtime.
            Some(directory.keep())
        };
        assert!(
            status.is_some_and(|status| status.success()),
            "isolated shutdown batch failed or exceeded 30s; retained {failure_root:?}:\n{stdout}\n{stderr}"
        );
        assert!(stdout.contains("1 passed"));
        return;
    };
    let root = std::path::PathBuf::from(root).canonicalize().unwrap();
    let mut state = HostState::new(HostSessionProof {
        host_session_id: HostSessionId::new("shutdown-batch"),
        boot_id: crate::model::BootId::new("shutdown-batch"),
        host_incarnation: HostIncarnation::new("shutdown-batch"),
    });
    state.register_client(1);
    let (wedged, release) = TerminalHandle::spawn_with_wedged_worker_for_test();
    state.finish_spawn(1, session(1, &root), wedged.clone(), true).unwrap();
    let healthy = TerminalHandle::spawn(session(2, &root), TerminalConfig::default(), |_| {}).unwrap();
    state.finish_spawn(1, session(2, &root), healthy.clone(), true).unwrap();
    let ready_by = Instant::now() + Duration::from_secs(5);
    let pid: u32 = loop {
        if let Ok(pid) = fs::read_to_string(root.join("root-pid")) {
            if let Ok(pid) = pid.trim().parse() {
                break pid;
            }
        }
        assert!(Instant::now() < ready_by, "private PTY root did not start");
        thread::sleep(Duration::from_millis(5));
    };
    let original_start = start_time(pid);
    assert_eq!(fs::read_link(format!("/proc/{pid}/cwd")).unwrap(), root);
    let descriptor = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0) };
    assert!(descriptor >= 0, "{}", io::Error::last_os_error());
    let root_pin = RootPin(unsafe { File::from_raw_fd(descriptor as i32) });
    assert_eq!(start_time(pid), original_start);
    let (handles, _) = state.graceful_shutdown();
    assert_eq!(handles.len(), 2);

    thread::scope(move |scope| {
        // Locals drop before scope joins on panic, releasing the wedged worker
        // and terminating only the pinned private PTY root if a check fails.
        let release = release;
        let _root_pin = root_pin;
        let (finished, result) = mpsc::sync_channel(1);
        let started = Instant::now();
        let batch = scope.spawn(move || {
            let _ = finished.send(finish_terminal_shutdown(&handles));
        });
        // The wedged worker is first in HostState's terminal-ID order. Its
        // five-second join budget must not postpone the healthy PTY's request.
        let healthy_by = started + Duration::from_secs(3);
        while Path::new(&format!("/proc/{pid}")).exists() {
            assert!(Instant::now() < healthy_by, "healthy PTY root was not reaped while the first worker was blocked");
            thread::sleep(Duration::from_millis(5));
        }
        assert!(matches!(result.try_recv(), Err(mpsc::TryRecvError::Empty)));
        let outcomes = result.recv_timeout(Duration::from_secs(9)).expect("shutdown batch exceeded its bounded worker budgets");
        assert!(started.elapsed() < Duration::from_secs(12));
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0], Err(TerminalSendError::Closed));
        assert_eq!(outcomes[1], Ok(()), "healthy cleanup was skipped after the first failure");
        assert!(healthy.is_completed());
        assert!(!wedged.is_completed(), "an unfinished worker must not acquire fictional PTY completion");
        drop(release);
        batch.join().unwrap();
    });
}
