//! Actual daemon terminal-shutdown phase with a nonreading original PTY.
//! The ordinary outer guardian separately proves full-family retirement.
use super::*;
use crate::{
    model::{LaunchRecipe, TerminalContinuity, TerminalId, TerminalSession},
    terminal::{PasteOrigin, TerminalConfig, TerminalHandle},
};
use std::{
    collections::BTreeMap,
    fs::File,
    os::fd::AsRawFd,
    path::Path,
    time::{Duration, Instant},
};

struct RootPin {
    descriptor: File,
    settled: bool,
}
impl RootPin {
    fn capture(handle: &TerminalHandle) -> Self {
        Self {
            descriptor: handle.original_root_pidfd_for_test(),
            settled: false,
        }
    }
    fn verify_exit(&mut self) {
        let mut poll = libc::pollfd {
            fd: self.descriptor.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        assert_eq!(
            unsafe { libc::poll(&mut poll, 1, 0) },
            1,
            "the original root must actually exit"
        );
        assert_ne!(poll.revents & libc::POLLIN, 0);
        self.settled = true;
    }
}
impl Drop for RootPin {
    fn drop(&mut self) {
        if !self.settled {
            // Assertion fallback reaches only the retained ORIGINAL root. It
            // is not family proof; Holder cleanup plus outer settlement remain.
            for signal in [libc::SIGCONT, libc::SIGKILL] {
                unsafe {
                    libc::syscall(
                        libc::SYS_pidfd_send_signal,
                        self.descriptor.as_raw_fd(),
                        signal,
                        std::ptr::null::<libc::siginfo_t>(),
                        0,
                    );
                }
            }
        }
    }
}

fn session(root: &Path, script: &str) -> TerminalSession {
    let launch = LaunchRecipe {
        program: "/bin/sh".into(),
        arguments: vec!["-c".into(), script.into()],
        working_directory: root.into(),
        unset_environment: Default::default(),
        environment: BTreeMap::from([
            (
                "HOME".into(),
                root.join("home").to_string_lossy().into_owned(),
            ),
            (
                "XDG_RUNTIME_DIR".into(),
                root.join("run").to_string_lossy().into_owned(),
            ),
            (
                "XDG_CONFIG_HOME".into(),
                root.join("config").to_string_lossy().into_owned(),
            ),
            (
                "XDG_DATA_HOME".into(),
                root.join("data").to_string_lossy().into_owned(),
            ),
            (
                "XDG_CACHE_HOME".into(),
                root.join("cache").to_string_lossy().into_owned(),
            ),
            (
                "TMPDIR".into(),
                root.join("tmp").to_string_lossy().into_owned(),
            ),
            ("PATH".into(), "/usr/bin:/bin".into()),
        ]),
    };
    TerminalSession {
        id: TerminalId::new(),
        launch: launch.clone(),
        transcript_path: None,
        resume_hint: None,
        owned_codex_listener: None,
        recovery_origin: None,
        predecessor: None,
        continuity: TerminalContinuity::RelaunchAvailable {
            recipe: launch,
            last_exit: None,
        },
    }
}

fn wait_for(handle: &TerminalHandle, marker: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let frame = handle.snapshot();
        handle.acknowledge_redraw(frame.generation);
        if frame.lines.iter().any(|line| {
            line.cells
                .iter()
                .map(|cell| cell.text.as_str())
                .collect::<String>()
                .contains(marker)
        }) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "original terminal never published {marker}"
        );
        thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn daemon_terminal_shutdown_finishes_nonreading_full_input_and_another_live_owner() {
    let mut directory = tempfile::Builder::new()
        .prefix("cagents-terminal-shutdown-")
        .tempdir()
        .unwrap();
    directory.disable_cleanup(true);
    let root = directory.path();
    fs::set_permissions(root, fs::Permissions::from_mode(0o700)).unwrap();
    for child in ["home", "run", "config", "data", "cache", "tmp"] {
        fs::create_dir(root.join(child)).unwrap();
        fs::set_permissions(root.join(child), fs::Permissions::from_mode(0o700)).unwrap();
    }
    eprintln!("terminal shutdown private evidence: {}", root.display());
    let busy = TerminalHandle::spawn(
        session(
            root,
            "stty -echo -icanon; printf 'NONREADER_READY\\n'; exec sleep 600",
        ),
        TerminalConfig::default(),
        |_| {},
    )
    .unwrap();
    let mut busy_root = RootPin::capture(&busy);
    wait_for(&busy, "NONREADER_READY");
    busy.paste("x".repeat(1024 * 1024), PasteOrigin::Clipboard)
        .unwrap();
    let pending = busy.check_input_drained().unwrap_err();
    assert!(
        pending.contains("input or control is still pending"),
        "wrong pending disposition: {pending}"
    );

    let healthy = TerminalHandle::spawn(session(root, "printf 'HEALTHY_READY\\n'; IFS= read -r line; printf 'healthy:%s\\n' \"$line\"; exec sleep 600"), TerminalConfig::default(), |_| {}).unwrap();
    let mut healthy_root = RootPin::capture(&healthy);
    wait_for(&healthy, "HEALTHY_READY");
    healthy
        .input(b"progress-while-other-blocked\n".to_vec())
        .unwrap();
    wait_for(&healthy, "healthy:progress-while-other-blocked");

    let started = Instant::now();
    finish_terminal_shutdown(vec![busy.clone(), healthy.clone()]).unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(18),
        "daemon terminal phase exceeded two bounded join/cleanup waits: {elapsed:?}"
    );
    assert!(busy.is_completed() && healthy.is_completed());
    busy_root.verify_exit();
    healthy_root.verify_exit();
    fs::write(
        root.join("result.txt"),
        format!(
            "terminal_shutdown_ok elapsed_ns={} original_roots_exited=true\n",
            elapsed.as_nanos()
        ),
    )
    .unwrap();
}
