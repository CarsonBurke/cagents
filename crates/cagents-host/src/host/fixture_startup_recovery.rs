//! A development fixture that makes a daemon report startup recovery, so
//! native evidence can show what windows and panes say while an update's
//! restore is still running or is held for recovery, which a fixture
//! otherwise reaches only through a failed installed update.
//!
//! It is compiled only into debug builds, and acts only when `HOME` lies
//! inside a native-evidence task (an ancestor holds the task's
//! `.native-evidence` marker) and holds the [`MARKER`] file. While the file
//! exists, the daemon answers [`StartupState`] as the file asks and its
//! request gate treats startup recovery as held, exactly as it does for a real
//! one: a file reading `restoring` reports a restore still in progress, and
//! one reading `retained: DETAIL` reports a restore held for recovery with
//! that reason (`retained` alone reports a Codex listener timeout).
//! Removing the file ends it, and the daemon reports ready. No owner is
//! retained or adopted: only what clients are told and admitted changes. Any
//! other home, or a home without the marker, is unaffected.
//!
//! A second marker, [`STOPPED_MARKER`], stands in for an update whose restore
//! stopped a conversation's failed restart: see [`stopped`].
use crate::terminal_ipc::StartupState;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Created in the fixture's `HOME` to report startup recovery.
const MARKER: &str = ".cagents-fixture-startup-recovery";
/// Created in the fixture's `HOME` to stop each saved conversation's restart.
const STOPPED_MARKER: &str = ".cagents-fixture-update-stopped";
/// The reason an empty stopped-restart marker reports.
const STOPPED_DEFAULT_REASON: &str = "Codex server exited before opening its local socket";
/// The reason a bare `retained` marker reports.
const DEFAULT_DETAIL: &str = "Codex server did not open its local socket within 15 seconds";
/// What a restore still in progress reports in place of a held reason.
const RESTORING_DETAIL: &str = "Original terminal ownership is awaiting its startup outcome";

/// Startup recovery the fixture reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Held {
    Restoring,
    Retained(&'static str),
}

impl Held {
    pub(super) fn detail(self) -> &'static str {
        match self {
            Self::Restoring => RESTORING_DETAIL,
            Self::Retained(detail) => detail,
        }
    }

    pub(super) fn state(self) -> StartupState {
        match self {
            Self::Restoring => StartupState::Restoring { awaiting: 1 },
            Self::Retained(detail) => StartupState::Retained {
                detail: detail.to_owned(),
            },
        }
    }
}

/// What the fixture in this daemon's `HOME` asks for now, if anything.
///
/// Whether the home is a task's is decided once; after that, each call reads
/// only the marker, so a daemon outside a task pays nothing but a check.
pub(super) fn held() -> Option<Held> {
    static MARKER_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    static DETAIL: OnceLock<String> = OnceLock::new();
    let marker = MARKER_PATH.get_or_init(|| {
        let home = std::env::var_os("HOME")?;
        marker_in(Path::new(&home))
    });
    read(marker.as_deref()?, &DETAIL)
}

/// The reason the fixture in this daemon's `HOME` gives for an update that
/// stopped a conversation's failed restart, if it asks for one.
///
/// While [`STOPPED_MARKER`] exists, every restart an update's restore makes
/// fails with the reason the file names, before anything starts, so the
/// daemon stops and records each such conversation as it does a real failed
/// restart, and refuses its pane until it is started again.
pub(super) fn stopped() -> Option<&'static str> {
    static MARKER_PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    static REASON: OnceLock<String> = OnceLock::new();
    let marker = MARKER_PATH.get_or_init(|| {
        let home = std::env::var_os("HOME")?;
        task_home(Path::new(&home)).map(|home| home.join(STOPPED_MARKER))
    });
    read_stopped(marker.as_deref()?, &REASON)
}

/// The marker path for `home`, when `home` is inside a native-evidence task.
fn marker_in(home: &Path) -> Option<PathBuf> {
    task_home(home).map(|home| home.join(MARKER))
}

/// `home`, canonical, when it is inside a native-evidence task.
fn task_home(home: &Path) -> Option<PathBuf> {
    let home = home.canonicalize().ok()?;
    home.ancestors()
        .skip(1)
        .any(|path| path.join(".native-evidence").is_file())
        .then_some(home)
}

/// Reads the stopped-restart `marker`. Its reason is fixed by the first read,
/// as the daemon's recorded reason for a stopped restart never changes.
fn read_stopped(marker: &Path, reason: &'static OnceLock<String>) -> Option<&'static str> {
    let contents = std::fs::read_to_string(marker).ok()?;
    let named = contents.trim();
    Some(reason.get_or_init(|| {
        if named.is_empty() {
            STOPPED_DEFAULT_REASON.to_owned()
        } else {
            named.to_owned()
        }
    }))
}

/// Reads `marker`. A held reason is fixed by the first read that names one,
/// as a real retained reason never changes while it holds.
fn read(marker: &Path, detail: &'static OnceLock<String>) -> Option<Held> {
    let contents = std::fs::read_to_string(marker).ok()?;
    let contents = contents.trim();
    if contents == "restoring" {
        return Some(Held::Restoring);
    }
    let Some(rest) = contents.strip_prefix("retained") else {
        tracing::warn!(
            contents,
            "the startup recovery fixture asks for neither `restoring` nor `retained`"
        );
        return None;
    };
    let named = rest.trim_start_matches(':').trim();
    let detail = detail.get_or_init(|| {
        if named.is_empty() {
            DEFAULT_DETAIL.to_owned()
        } else {
            named.to_owned()
        }
    });
    Some(Held::Retained(detail))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reports_recovery_only_inside_a_task_home_while_the_marker_exists() {
        let task = tempfile::tempdir().unwrap();
        let home = task.path().join("fixture/home");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join(MARKER), "restoring\n").unwrap();
        // Outside a native-evidence task, the marker is ignored.
        assert_eq!(marker_in(&home), None);
        std::fs::write(task.path().join(".native-evidence"), "").unwrap();
        let marker = marker_in(&home).expect("a task home");

        let detail = Box::leak(Box::new(OnceLock::new()));
        assert_eq!(read(&marker, detail), Some(Held::Restoring));
        assert_eq!(
            Held::Restoring.state(),
            StartupState::Restoring { awaiting: 1 }
        );

        std::fs::write(&marker, "retained: the listener timed out\n").unwrap();
        let held = read(&marker, detail).unwrap();
        assert_eq!(held, Held::Retained("the listener timed out"));
        // A held reason does not change while it holds.
        std::fs::write(&marker, "retained: something else").unwrap();
        assert_eq!(read(&marker, detail), Some(held));

        std::fs::remove_file(&marker).unwrap();
        assert_eq!(read(&marker, detail), None, "removing the marker ends it");
    }

    #[test]
    fn a_stopped_marker_names_its_reason_until_it_is_removed() {
        let task = tempfile::tempdir().unwrap();
        let home = task.path().join("fixture/home");
        std::fs::create_dir_all(&home).unwrap();
        assert_eq!(task_home(&home), None, "outside a task, no marker counts");
        std::fs::write(task.path().join(".native-evidence"), "").unwrap();
        let marker = task_home(&home).unwrap().join(STOPPED_MARKER);
        let reason = Box::leak(Box::new(OnceLock::new()));
        assert_eq!(read_stopped(&marker, reason), None);
        std::fs::write(&marker, "\n").unwrap();
        assert_eq!(read_stopped(&marker, reason), Some(STOPPED_DEFAULT_REASON));
        std::fs::remove_file(&marker).unwrap();
        assert_eq!(read_stopped(&marker, reason), None);
    }

    #[test]
    fn a_bare_retained_marker_reports_the_update_reason() {
        let root = tempfile::tempdir().unwrap();
        let marker = root.path().join(MARKER);
        std::fs::write(&marker, "retained").unwrap();
        let detail = Box::leak(Box::new(OnceLock::new()));
        assert_eq!(read(&marker, detail), Some(Held::Retained(DEFAULT_DETAIL)));
        std::fs::write(&marker, "paused").unwrap();
        assert_eq!(read(&marker, detail), None);
    }
}
