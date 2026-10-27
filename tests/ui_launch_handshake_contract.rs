#![cfg(target_os = "linux")]

use cagents::process::{
    CancellationToken, CommandSpec, ProcessExecutor, ProcessOutput, SystemProcessExecutor,
};
use std::{fs, io::Read, os::fd::AsRawFd, path::Path, time::Duration};

const PROGRESS_CAPABILITY: &str = "CAGENTS_UI_LAUNCH_RECOVERY_PROGRESS";
const FIXTURE_OUTPUT: &str = "CAGENTS_TEST_STARTUP_REPORT_OUTPUT";
const FIXTURE_COUNT: &str = "CAGENTS_TEST_STARTUP_REPORT_COUNT";
const FIXTURE_FAILURE: &str = "CAGENTS_TEST_STARTUP_REPORT_FAILURE";
const PROTOCOL_LIMIT: usize = 4096;
const REPORT_ERROR_EXIT: i32 = 65;
const INVENTORY_LIMIT_ERROR: &str = "saved conversation inventory exceeds the recovery limit";

fn fixture_command(output: &Path, state: &Path) -> CommandSpec {
    CommandSpec::new(std::env::current_exe().unwrap())
        .args(["--exact", "startup_report_fixture", "--nocapture"])
        .env(FIXTURE_OUTPUT, output)
        .env("XDG_STATE_HOME", state)
        .env_remove(FIXTURE_COUNT)
        .env_remove(FIXTURE_FAILURE)
        .env_remove(cagents::instance::UI_LAUNCH_HANDSHAKE_ENV)
        .env_remove(PROGRESS_CAPABILITY)
        .timeout(Duration::from_secs(10))
        .output_limits(64 * 1024, 64 * 1024)
}

fn capture(command: CommandSpec) -> ProcessOutput {
    // Executor failures, including timeout and output overflow, must never
    // satisfy the excessive-inventory case's expected application rejection.
    SystemProcessExecutor
        .execute(&command, &CancellationToken::new())
        .expect("startup reporting fixture failed bounded execution")
}

fn protocol_output(path: &Path) -> String {
    let mut bytes = Vec::new();
    fs::File::open(path)
        .unwrap()
        .take((PROTOCOL_LIMIT + 1) as u64)
        .read_to_end(&mut bytes)
        .unwrap();
    assert!(bytes.len() <= PROTOCOL_LIMIT, "startup protocol is oversized");
    String::from_utf8(bytes).unwrap()
}

/// Exercise the public UI reporting functions in a fresh process, including
/// their once-only state. Redirect only protocol output, excluding libtest's
/// own startup banner, and exit before its final test summary is printed.
#[test]
fn startup_report_fixture() {
    let Some(path) = std::env::var_os(FIXTURE_OUTPUT) else {
        return;
    };
    // dup2 bypasses the executor's stdout pipe. Bound regular-file writes in
    // this disposable child as well as the parent's protocol-file reads.
    let mut limit = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: getrlimit writes this valid child-local struct.
    assert_eq!(unsafe { libc::getrlimit(libc::RLIMIT_FSIZE, &mut limit) }, 0);
    limit.rlim_max = limit.rlim_max.min(PROTOCOL_LIMIT as libc::rlim_t);
    limit.rlim_cur = limit.rlim_cur.min(limit.rlim_max);
    // SAFETY: setrlimit reads this valid struct and changes only this child.
    assert_eq!(unsafe { libc::setrlimit(libc::RLIMIT_FSIZE, &limit) }, 0);
    let output = fs::File::create_new(path).unwrap();
    assert_ne!(
        unsafe { libc::dup2(output.as_raw_fd(), libc::STDOUT_FILENO) },
        -1
    );
    let count = std::env::var(FIXTURE_COUNT)
        .ok()
        .map(|value| value.parse().unwrap())
        .unwrap_or(1);
    if let Err(error) = cagents::instance::report_ui_recovery_progress(count) {
        eprintln!("{error}");
        std::process::exit(REPORT_ERROR_EXIT);
    }
    cagents::instance::report_ui_recovery_progress(1).unwrap();
    let result = if std::env::var_os(FIXTURE_FAILURE).is_some() {
        Err("recovery-incomplete: retained pane is still reconnecting".into())
    } else {
        Ok(())
    };
    cagents::instance::report_ui_startup(&result).unwrap();
    std::process::exit(0);
}

#[test]
fn startup_progress_requires_an_explicit_launcher_capability() {
    let directory = tempfile::tempdir().unwrap();
    for (index, (handshake, capability, failed, expected)) in [
        (None, Some("1"), false, ""),
        (Some("1"), None, false, "ready\n"),
        (Some("1"), Some("0"), false, "ready\n"),
        (Some("1"), Some("unknown"), false, "ready\n"),
        (Some("1"), Some("1"), false, "recovering: 1\nready\n"),
        (
            Some("1"),
            None,
            true,
            "error: recovery-incomplete: retained pane is still reconnecting\n",
        ),
        (
            Some("1"),
            Some("1"),
            true,
            "recovering: 1\nerror: recovery-incomplete: retained pane is still reconnecting\n",
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let output = directory.path().join(format!("protocol-{index}"));
        let mut command = fixture_command(&output, &directory.path().join("state"));
        if let Some(handshake) = handshake {
            command = command.env(cagents::instance::UI_LAUNCH_HANDSHAKE_ENV, handshake);
        }
        if let Some(capability) = capability {
            command = command.env(PROGRESS_CAPABILITY, capability);
        }
        if failed {
            command = command.env(FIXTURE_FAILURE, "1");
        }
        let child = capture(command);
        assert!(child.success, "{child:?}");
        assert_eq!(
            protocol_output(&output),
            expected,
            "handshake={handshake:?}, capability={capability:?}, failed={failed}"
        );
    }
}

#[test]
fn startup_progress_accepts_supported_inventory_and_refuses_excess() {
    let directory = tempfile::tempdir().unwrap();
    for count in [129, 4096, 5120, 5121] {
        let output = directory.path().join(format!("count-{count}"));
        let child = capture(
            fixture_command(&output, &directory.path().join("state"))
                .env(FIXTURE_COUNT, count.to_string())
                .env(cagents::instance::UI_LAUNCH_HANDSHAKE_ENV, "1")
                .env(PROGRESS_CAPABILITY, "1"),
        );
        if count <= 5120 {
            assert!(child.success, "{child:?}");
            assert_eq!(
                protocol_output(&output),
                format!("recovering: {count}\nready\n")
            );
        } else {
            assert_eq!(child.exit_code, Some(REPORT_ERROR_EXIT), "{child:?}");
            assert_eq!(child.signal, None, "{child:?}");
            assert_eq!(
                String::from_utf8(child.stderr).unwrap(),
                format!("{INVENTORY_LIMIT_ERROR}\n")
            );
            assert_eq!(protocol_output(&output), "");
        }
    }
}
