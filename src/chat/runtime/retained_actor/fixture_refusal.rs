//! Development fixtures that hold up or refuse one Claude input, so native
//! evidence can show how a conversation reads while a send is on its way, and
//! after one it declined, which the synthetic provider cannot otherwise
//! provoke.
//!
//! They are compiled only into debug builds, and act only when `HOME` lies
//! inside a native-evidence task (an ancestor holds the task's
//! `.native-evidence` marker) and holds their file. [`DELAY`] holds a number
//! of seconds, up to [`LONGEST_DELAY`]: the next input waits that long before
//! it is admitted. [`MARKER`] names one outcome of
//! [`crate::chat::failure::delivery`], such as `CROWDED_INPUT`: the next input
//! is refused with that outcome before anything is sent. Each file is removed
//! as it is used, so the input after it goes through as usual. With both, the
//! input waits and is then refused. Any other home, or a home without the
//! files, is unaffected.
use std::path::Path;
use std::time::Duration;

/// Created in the fixture's `HOME` to refuse the next input.
const MARKER: &str = ".cagents-fixture-refuse-input";

/// Created in the fixture's `HOME` to hold up the next input.
const DELAY: &str = ".cagents-fixture-delay-input";

/// The longest a fixture holds an input up, well within what the
/// application waits for an answer.
const LONGEST_DELAY: Duration = Duration::from_secs(8);

/// How long the fixture in this process's `HOME` holds up the next input,
/// if it asks to.
pub(super) fn take_delay() -> Option<Duration> {
    take_delay_in(Path::new(&std::env::var_os("HOME")?))
}

fn take_delay_in(home: &Path) -> Option<Duration> {
    let seconds = take_file(home, DELAY)?;
    let delay = seconds
        .trim()
        .parse::<f64>()
        .ok()
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
    match delay {
        Some(delay) => {
            tracing::warn!(?delay, "native-evidence fixture is holding up one input");
            Some(delay.min(LONGEST_DELAY))
        }
        None => {
            tracing::warn!(
                seconds = seconds.trim(),
                "the input delay fixture holds no number of seconds"
            );
            None
        }
    }
}

/// The contents of `name` in `home`, removed as it is read, where `home` is
/// inside a native-evidence task.
fn take_file(home: &Path, name: &str) -> Option<String> {
    let home = home.canonicalize().ok()?;
    if !home
        .ancestors()
        .skip(1)
        .any(|path| path.join(".native-evidence").is_file())
    {
        return None;
    }
    let marker = home.join(name);
    let contents = std::fs::read_to_string(&marker).ok()?;
    std::fs::remove_file(&marker).ok()?;
    Some(contents)
}

/// The outcome the fixture in this process's `HOME` refuses the next input
/// with, if it asks for one.
pub(super) fn take() -> Option<&'static str> {
    take_in(Path::new(&std::env::var_os("HOME")?))
}

fn take_in(home: &Path) -> Option<&'static str> {
    let name = take_file(home, MARKER)?;
    let outcome = crate::chat::failure::delivery::named(name.trim());
    if outcome.is_none() {
        tracing::warn!(
            name = name.trim(),
            "the input refusal fixture names no delivery outcome"
        );
    } else {
        tracing::warn!(
            name = name.trim(),
            "native-evidence fixture is refusing one input"
        );
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refuses_one_input_only_inside_a_native_evidence_task() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("fixture");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join(MARKER), "CROWDED_INPUT\n").unwrap();
        // Outside a task the marker is left alone.
        assert_eq!(take_in(&home), None);
        assert!(home.join(MARKER).is_file());

        std::fs::write(root.path().join(".native-evidence"), "").unwrap();
        assert_eq!(
            take_in(&home),
            Some(crate::chat::failure::delivery::CROWDED_INPUT)
        );
        // Once: the next input goes through.
        assert_eq!(take_in(&home), None);

        std::fs::write(home.join(MARKER), "NO_SUCH_OUTCOME").unwrap();
        assert_eq!(take_in(&home), None);
        assert!(!home.join(MARKER).exists());
    }

    #[test]
    fn holds_up_one_input_only_inside_a_native_evidence_task() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("fixture");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join(DELAY), "2.5\n").unwrap();
        assert_eq!(take_delay_in(&home), None);
        assert!(home.join(DELAY).is_file());

        std::fs::write(root.path().join(".native-evidence"), "").unwrap();
        assert_eq!(take_delay_in(&home), Some(Duration::from_millis(2500)));
        assert_eq!(take_delay_in(&home), None);

        std::fs::write(home.join(DELAY), "3600").unwrap();
        assert_eq!(take_delay_in(&home), Some(LONGEST_DELAY));
        std::fs::write(home.join(DELAY), "soon").unwrap();
        assert_eq!(take_delay_in(&home), None);
        assert!(!home.join(DELAY).exists());
    }
}
