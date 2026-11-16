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

#[path = "../../../tests/support/startup_environment.rs"]
mod startup_environment;

use startup_environment::{Home, Installation, assert_detached};
use std::{fs, path::Path, process::Stdio};

#[test]
fn a_window_never_hands_a_parent_session_to_the_daemon_it_starts() {
    use std::{
        process::Child,
        time::{Duration, Instant},
    };
    const DEADLINE: Duration = Duration::from_secs(30);
    const POLL: Duration = Duration::from_millis(20);
    fn wait_for(path: &Path, owner: &mut Child) -> Vec<u8> {
        let deadline = Instant::now() + DEADLINE;
        loop {
            if let Ok(bytes) = fs::read(path) {
                return bytes;
            }
            if let Some(status) = owner.try_wait().unwrap() {
                // A child can publish just as its owner exits.
                if let Ok(bytes) = fs::read(path) {
                    return bytes;
                }
                panic!("the executable exited with {status} before starting a child");
            }
            assert!(
                Instant::now() < deadline,
                "the executable started no child within {DEADLINE:?}"
            );
            std::thread::sleep(POLL);
        }
    }
    struct Window(Child);
    impl Drop for Window {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let home = Home::new();
    // The stand-in daemon stays until the record is collected, so the window
    // keeps waiting for it instead of reporting a failed start.
    let installation = Installation::new(
        env!("CARGO_BIN_EXE_cagents-ui"),
        "cagentsd",
        "for _ in $(seq 600); do [ -e \"$0.environ\" ] || exit 0; sleep 0.05; done",
    );
    let mut window = Window(
        home.command(installation.executable())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );
    let recorded = installation.recorded("cagentsd");
    let environ = wait_for(&recorded, &mut window.0);
    drop(window);
    fs::remove_file(recorded).unwrap();
    assert_detached(&environ);
}
