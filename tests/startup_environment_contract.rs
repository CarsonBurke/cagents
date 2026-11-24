// The shipped `cagents` command has no features, so only the cli lane runs
// this target; the daemon's own case is in crates/cagents-host.
#![cfg(all(target_os = "linux", not(any(feature = "host", feature = "ui"))))]
//! Each executable forgets the provider session it was started inside before
//! it starts anything (#2699). A `cagents` typed in an agent's shell, or a
//! window or daemon launched from one, would otherwise hand that session's
//! messaging socket to every process it starts.
//!
//! Each case starts the real executable inside a planted parent session and
//! reads what one of its children was actually given, from that child's own
//! `/proc/PID/environ`. Removing a variable from a running process does not
//! change its own `/proc` entry, so a child is the only honest witness.

#[allow(dead_code, reason = "each contract uses part of the shared support")]
#[path = "support/startup_environment.rs"]
mod startup_environment;

use startup_environment::{Home, Installation, assert_detached};
use std::fs;

#[test]
fn the_command_line_never_hands_a_parent_session_to_the_daemon() {
    let home = Home::new();
    // `daemon store-schema` runs its matched daemon directly and synchronously.
    let installation = Installation::new(
        std::path::Path::new(env!("CARGO_BIN_EXE_cagents")),
        "cagentsd",
        "exit 0",
    );
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
