//! A development fixture that refuses one pane Close as the registry refuses
//! a Close whose pane changed, so native evidence can show what a window says
//! when a Close is refused and its pane is kept. Two windows racing an edit
//! provoke that only by chance.
//!
//! It is compiled only into debug builds, and acts only when `HOME` lies
//! inside a native-evidence task (an ancestor holds the task's
//! `.native-evidence` marker) and holds the [`MARKER`] file. The next Close
//! is refused and the file is removed, so the Close after it goes through as
//! usual. Any other home, or a home without the marker, is unaffected.
use std::path::Path;

/// Created in the fixture's `HOME` to refuse the next pane Close.
const MARKER: &str = ".cagents-fixture-refuse-close";

/// Whether the fixture in this daemon's `HOME` refuses this Close.
pub(super) fn take() -> bool {
    std::env::var_os("HOME").is_some_and(|home| take_in(Path::new(&home)))
}

fn take_in(home: &Path) -> bool {
    let Ok(home) = home.canonicalize() else {
        return false;
    };
    if !home
        .ancestors()
        .skip(1)
        .any(|path| path.join(".native-evidence").is_file())
    {
        return false;
    }
    let refused = std::fs::remove_file(home.join(MARKER)).is_ok();
    if refused {
        tracing::warn!("native-evidence fixture is refusing one pane close");
    }
    refused
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_one_close_only_inside_a_native_evidence_task() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("fixture");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join(MARKER), "").unwrap();
        // Outside a task the marker is left alone.
        assert!(!take_in(&home));
        assert!(home.join(MARKER).is_file());

        std::fs::write(root.path().join(".native-evidence"), "").unwrap();
        assert!(take_in(&home));
        // Once: the next Close goes through.
        assert!(!take_in(&home));
    }
}
