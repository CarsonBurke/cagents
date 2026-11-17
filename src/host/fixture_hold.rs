//! A development fixture that keeps one shutdown preflight in ordinary
//! admission, as a slow ordinary request does, so native evidence can make a
//! must-win request meet a daemon that is really busy: it waits at the
//! control gate for its deadline and is refused there.
//!
//! It acts only in a debug build whose `HOME` lies inside a native-evidence
//! task (an ancestor holds the task's `.native-evidence` marker) and holds the
//! [`MARKER`] file. A preflight that arrives while the marker exists holds
//! ordinary admission until the marker is removed, at most [`LIMIT`], then
//! proceeds as usual. A release build, any other home, or a home without the
//! marker is unaffected.
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Created in the fixture's `HOME` to hold the next shutdown preflight.
pub(super) const MARKER: &str = ".cagents-fixture-hold-preflight";
/// The longest a preflight holds ordinary admission, so a forgotten marker cannot
/// wedge the fixture daemon.
const LIMIT: Duration = Duration::from_secs(120);
const POLL: Duration = Duration::from_millis(100);

/// Holds ordinary admission at `gate` while the fixture asks for it.
pub(super) fn hold_preflight(gate: &super::control_gate::ControlGate) {
    if !cfg!(debug_assertions) {
        return;
    }
    let Some(marker) = std::env::var_os("HOME").and_then(|home| marker(Path::new(&home))) else {
        return;
    };
    tracing::warn!(path = %marker.display(), "native-evidence fixture is holding a shutdown preflight in ordinary admission");
    let _ordinary = gate.shared();
    let until = Instant::now() + LIMIT;
    while marker.is_file() && Instant::now() < until && !crate::daemon::termination_requested() {
        std::thread::sleep(POLL);
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
