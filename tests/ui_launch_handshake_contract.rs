#![cfg(target_os = "linux")]

use std::{fs, os::fd::AsRawFd, process::Command};

const PROGRESS_CAPABILITY: &str = "CAGENTS_UI_LAUNCH_RECOVERY_PROGRESS";
const FIXTURE_OUTPUT: &str = "CAGENTS_TEST_STARTUP_REPORT_OUTPUT";
const FIXTURE_COUNT: &str = "CAGENTS_TEST_STARTUP_REPORT_COUNT";
const FIXTURE_FAILURE: &str = "CAGENTS_TEST_STARTUP_REPORT_FAILURE";

/// Exercise the public UI reporting functions in a fresh process, including
/// their once-only state. Redirect only protocol output, excluding libtest's
/// own startup banner, and exit before its final test summary is printed.
#[test]
fn startup_report_fixture() {
    let Some(path) = std::env::var_os(FIXTURE_OUTPUT) else {
        return;
    };
    let output = fs::File::create_new(path).unwrap();
    assert_ne!(
        unsafe { libc::dup2(output.as_raw_fd(), libc::STDOUT_FILENO) },
        -1
    );
    let count = std::env::var(FIXTURE_COUNT)
        .ok()
        .map(|value| value.parse().unwrap())
        .unwrap_or(1);
    cagents::instance::report_ui_recovery_progress(count).unwrap();
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
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .args(["--exact", "startup_report_fixture", "--nocapture"])
            .env(FIXTURE_OUTPUT, &output)
            .env("XDG_STATE_HOME", directory.path().join("state"))
            .env_remove(FIXTURE_COUNT)
            .env_remove(FIXTURE_FAILURE)
            .env_remove(cagents::instance::UI_LAUNCH_HANDSHAKE_ENV)
            .env_remove(PROGRESS_CAPABILITY);
        if let Some(handshake) = handshake {
            command.env(cagents::instance::UI_LAUNCH_HANDSHAKE_ENV, handshake);
        }
        if let Some(capability) = capability {
            command.env(PROGRESS_CAPABILITY, capability);
        }
        if failed {
            command.env(FIXTURE_FAILURE, "1");
        }
        let child = command.output().unwrap();
        assert!(child.status.success(), "{child:?}");
        assert_eq!(
            fs::read_to_string(output).unwrap(),
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
        let child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "startup_report_fixture", "--nocapture"])
            .env(FIXTURE_OUTPUT, &output)
            .env(FIXTURE_COUNT, count.to_string())
            .env("XDG_STATE_HOME", directory.path().join("state"))
            .env(cagents::instance::UI_LAUNCH_HANDSHAKE_ENV, "1")
            .env(PROGRESS_CAPABILITY, "1")
            .env_remove(FIXTURE_FAILURE)
            .output()
            .unwrap();
        if count <= 5120 {
            assert!(child.status.success(), "{child:?}");
            assert_eq!(
                fs::read_to_string(output).unwrap(),
                format!("recovering: {count}\nready\n")
            );
        } else {
            assert!(!child.status.success());
            assert!(!fs::read_to_string(output).unwrap().contains("ready"));
        }
    }
}
