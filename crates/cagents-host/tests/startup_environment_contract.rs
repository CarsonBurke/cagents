#![cfg(target_os = "linux")]
//! Each executable forgets the provider session it was started inside before
//! it starts anything (#2699). A `cagents` typed in an agent's shell, or a
//! window or daemon launched from one, would otherwise hand that session's
//! messaging socket to every process it starts.
//!
//! Each case starts the real executable inside a planted parent session and
//! reads what one of its children was actually given, from that child's own
//! `/proc/PID/environ`. Removing a variable from a running process does not
//! change its own `/proc` entry, so a child is the only honest witness.

#[path = "support/startup_environment.rs"]
mod startup_environment;

use startup_environment::{Home, Installation, assert_detached};
use std::fs;

#[test]
fn the_command_line_never_hands_a_parent_session_to_the_daemon() {
    let home = Home::new();
    // `daemon store-schema` runs its matched daemon directly and synchronously.
    let installation = Installation::new(env!("CARGO_BIN_EXE_cagents"), "cagentsd", "exit 0");
    let output = home
        .command(installation.executable())
        .args(["daemon", "store-schema"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_detached(&fs::read(installation.recorded("cagentsd")).unwrap());
}

#[cfg(feature = "host")]
#[test]
fn the_daemon_never_hands_a_parent_session_to_an_automation_precheck() {
    use std::{
        os::unix::{
            fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
            process::CommandExt,
        },
        path::Path,
        process::Stdio,
    };
    let home = Home::new();
    // The supervisor is `cagentsd` itself, past the same startup as a serving
    // daemon; the precheck is the person's shell command it runs.
    let firing = uuid::Uuid::new_v4();
    let directory = home.path().join(firing.to_string());
    fs::DirBuilder::new()
        .mode(0o700)
        .create(&directory)
        .unwrap();
    let identity = fs::metadata(&directory).unwrap();
    let request = serde_json::json!({
        "firing": firing, "revision": 1, "directory": directory,
        "target_identity": {"device": identity.dev(), "inode": identity.ino()},
        "precheck": {"command": "cat /proc/$$/environ > environ", "timeout_seconds": 5}
    });
    fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .mode(0o600)
        .open(directory.join("request"))
        .and_then(|mut file| std::io::Write::write_all(&mut file, request.to_string().as_bytes()))
        .unwrap();
    let lease = fs::OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .mode(0o600)
        .open(directory.join("lease"))
        .unwrap();
    let fd = std::os::fd::AsRawFd::as_raw_fd(&lease);
    let mut command = home.command(Path::new(env!("CARGO_BIN_EXE_cagentsd")));
    command
        .arg("--automation-precheck-supervisor")
        .arg(&directory)
        .arg(fd.to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    // SAFETY: only an async-signal-safe fcntl between fork and exec, handing
    // the supervisor the lease it is required to own.
    unsafe {
        command.pre_exec(move || {
            if libc::fcntl(fd, libc::F_SETFD, 0) < 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let output = command.output().unwrap();
    drop(lease);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let receipt = fs::read_to_string(directory.join("receipt")).unwrap();
    assert!(receipt.contains("Passed"), "{receipt}");
    assert_detached(&fs::read(directory.join("environ")).unwrap());
}
