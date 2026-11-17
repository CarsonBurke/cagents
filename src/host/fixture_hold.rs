//! A development fixture that keeps one shutdown preflight in the control
//! gate, so native evidence can make a request meet a daemon that is really
//! busy. Held in ordinary admission, as a slow ordinary request is, a
//! must-win request waits at the gate for its deadline and is refused there.
//! Held exclusively, as a long operation such as an update is, even a new
//! connection's handshake waits.
//!
//! It acts only in a debug build whose `HOME` lies inside a native-evidence
//! task (an ancestor holds the task's `.native-evidence` marker) and holds the
//! [`MARKER`] file. A preflight that arrives while the marker exists holds
//! ordinary admission, or exclusive admission when the marker reads
//! `exclusive`, until the marker is removed, at most [`LIMIT`], then proceeds
//! as usual. A release build, any other home, or a home without the
//! marker is unaffected.
use super::control_gate::ControlGate;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Created in the fixture's `HOME` to hold the next shutdown preflight.
pub(super) const MARKER: &str = ".cagents-fixture-hold-preflight";
/// The longest a preflight holds the gate, so a forgotten marker cannot
/// wedge the fixture daemon.
const LIMIT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(100);

/// Holds ordinary admission at `gate` while the fixture asks for it.
pub(super) fn hold_preflight(gate: &ControlGate) {
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(marker) = std::env::var_os("HOME").and_then(|home| marker(Path::new(&home))) else {
        return;
    };
    tracing::warn!(path = %marker.display(), "native-evidence fixture is holding a shutdown preflight in the control gate");
    hold(gate, &marker, Instant::now() + LIMIT);
}

/// Holds admission at `gate` while `marker` exists, until `until`:
/// exclusive when the marker reads `exclusive`, otherwise ordinary.
fn hold(gate: &ControlGate, marker: &Path, until: Instant) {
    let wait = || {
        while marker.is_file() && Instant::now() < until && !crate::daemon::termination_requested()
        {
            std::thread::sleep(POLL);
        }
    };
    if std::fs::read_to_string(marker).is_ok_and(|mode| mode.trim() == "exclusive") {
        let _exclusive = gate.exclusive_until(until);
        wait();
    } else {
        let _ordinary = gate.shared();
        wait();
    }
}

/// The marker that asks `home`'s daemon to hold, when `home` is inside a
/// native-evidence task and the marker is present.
fn marker(home: &Path) -> Option<PathBuf> {
    let home = home.canonicalize().ok()?;
    let marker = home.join(MARKER);
    (marker.is_file()
        && home
            .ancestors()
            .skip(1)
            .any(|path| path.join(".native-evidence").is_file()))
    .then_some(marker)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_held_preflight_makes_a_must_win_request_wait_out_its_deadline() {
        let home = tempfile::tempdir().unwrap();
        let marker = home.path().join(MARKER);
        std::fs::write(&marker, "").unwrap();
        let gate = ControlGate::default();
        std::thread::scope(|scope| {
            let held = scope.spawn(|| hold(&gate, &marker, Instant::now() + LIMIT));
            // Give the hold its admission before a must-win request queues.
            while gate.try_exclusive().is_some() {
                std::thread::yield_now();
            }
            let waited = Instant::now();
            assert!(
                gate.exclusive_until(Instant::now() + Duration::from_millis(300))
                    .is_none(),
                "refused at its deadline while the preflight is held"
            );
            assert!(waited.elapsed() >= Duration::from_millis(300));
            std::fs::remove_file(&marker).unwrap();
            held.join().unwrap();
        });
        assert!(
            gate.try_exclusive().is_some(),
            "removing the marker frees the gate"
        );
    }

    #[test]
    fn an_exclusive_hold_keeps_out_even_ordinary_requests() {
        let home = tempfile::tempdir().unwrap();
        let marker = home.path().join(MARKER);
        std::fs::write(&marker, "exclusive\n").unwrap();
        let gate = ControlGate::default();
        std::thread::scope(|scope| {
            let held = scope.spawn(|| hold(&gate, &marker, Instant::now() + LIMIT));
            while gate.try_shared().is_some() {
                std::thread::yield_now();
            }
            assert!(gate.try_shared().is_none(), "held exclusively");
            std::fs::remove_file(&marker).unwrap();
            held.join().unwrap();
        });
        assert!(
            gate.try_shared().is_some(),
            "removing the marker frees the gate"
        );
    }

    #[test]
    fn only_a_native_evidence_home_with_the_marker_holds() {
        let task = tempfile::tempdir().unwrap();
        let home = task.path().join("fixture/home");
        std::fs::create_dir_all(&home).unwrap();
        // Neither the task marker nor the hold marker.
        assert_eq!(marker(&home), None);
        // A home that asks to hold, outside any native-evidence task.
        std::fs::write(home.join(MARKER), "").unwrap();
        assert_eq!(marker(&home), None);
        // A native-evidence task whose home has not asked.
        std::fs::write(task.path().join(".native-evidence"), "").unwrap();
        std::fs::remove_file(home.join(MARKER)).unwrap();
        assert_eq!(marker(&home), None);
        // Both.
        std::fs::write(home.join(MARKER), "").unwrap();
        assert_eq!(
            marker(&home),
            Some(home.canonicalize().unwrap().join(MARKER))
        );
    }
}
