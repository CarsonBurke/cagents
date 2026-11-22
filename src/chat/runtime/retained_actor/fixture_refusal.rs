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
//!
//! [`LATE_ACKNOWLEDGEMENT`] holds a number of seconds, up to
//! [`LONGEST_LATENESS`]: the next input the helper admits runs as usual, but
//! the helper reports it acknowledged only that long after it was admitted, as
//! a helper held up on a heavily loaded computer can. Past the application's
//! wait, the send's outcome is unknown though the message went through.
//!
//! [`UNANSWERED_READS`] holds a number of seconds, up to
//! [`LONGEST_SILENCE`]: for that long the helper doesn't answer transcript
//! reads or requests for its state, as one held up on a heavily loaded
//! computer doesn't. A read waits
//! until the silence ends, then goes through, unless its deadline comes first.
use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Created in the fixture's `HOME` to refuse the next input.
const MARKER: &str = ".cagents-fixture-refuse-input";

/// Created in the fixture's `HOME` to hold up the next input.
const DELAY: &str = ".cagents-fixture-delay-input";

/// The longest a fixture holds an input up, well within what the
/// application waits for an answer.
const LONGEST_DELAY: Duration = Duration::from_secs(8);

/// Created in the fixture's `HOME` to acknowledge the next input late.
const LATE_ACKNOWLEDGEMENT: &str = ".cagents-fixture-late-acknowledgement";

/// The latest the helper acknowledges an input, well past what the
/// application waits for an acknowledgement.
const LONGEST_LATENESS: Duration = Duration::from_secs(60);

/// Created in the fixture's `HOME` to stop transcript reads being answered.
const UNANSWERED_READS: &str = ".cagents-fixture-unanswered-reads";

/// The longest the helper stops answering transcript reads.
const LONGEST_SILENCE: Duration = Duration::from_secs(120);

/// Until when transcript reads go unanswered.
static SILENT_UNTIL: Mutex<Option<Instant>> = Mutex::new(None);

/// Hold up a transcript read while the fixture in this process's `HOME` has
/// the helper not answering, failing it as an unanswered read fails once
/// `deadline` comes first.
pub(super) fn hold_read(deadline: Instant) -> Result<(), String> {
    match std::env::var_os("HOME") {
        Some(home) => hold_read_in(Path::new(&home), &SILENT_UNTIL, deadline),
        None => Ok(()),
    }
}

/// [`hold_read`] for the fixture in `home`, keeping the silence in
/// `silent_until`.
fn hold_read_in(
    home: &Path,
    silent_until: &Mutex<Option<Instant>>,
    deadline: Instant,
) -> Result<(), String> {
    let until = {
        let mut until = silent_until.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(silence) = take_seconds(home, UNANSWERED_READS) {
            tracing::warn!(
                ?silence,
                "native-evidence fixture stops answering transcript reads"
            );
            *until = Some(Instant::now() + silence.min(LONGEST_SILENCE));
        }
        *until
    };
    let Some(until) = until.filter(|until| *until > Instant::now()) else {
        return Ok(());
    };
    std::thread::sleep(
        until
            .min(deadline)
            .saturating_duration_since(Instant::now()),
    );
    if until >= deadline {
        return Err(crate::chat::failure::actor_transport::DEADLINE.into());
    }
    Ok(())
}

/// How long the fixture in this process's `HOME` holds up the next input,
/// if it asks to.
pub(super) fn take_delay() -> Option<Duration> {
    take_delay_in(Path::new(&std::env::var_os("HOME")?))
}

fn take_delay_in(home: &Path) -> Option<Duration> {
    let delay = take_seconds(home, DELAY)?;
    tracing::warn!(?delay, "native-evidence fixture is holding up one input");
    Some(delay.min(LONGEST_DELAY))
}

/// How long after admitting it the helper with the fixture in this process's
/// `HOME` acknowledges the next input, if it asks to.
pub(super) fn take_lateness() -> Option<Duration> {
    take_lateness_in(Path::new(&std::env::var_os("HOME")?))
}

fn take_lateness_in(home: &Path) -> Option<Duration> {
    let lateness = take_seconds(home, LATE_ACKNOWLEDGEMENT)?;
    tracing::warn!(
        ?lateness,
        "native-evidence fixture acknowledges one input late"
    );
    Some(lateness.min(LONGEST_LATENESS))
}

/// The number of seconds in `name` in `home`, removed as it is read, where
/// `home` is inside a native-evidence task.
fn take_seconds(home: &Path, name: &str) -> Option<Duration> {
    let seconds = take_file(home, name)?;
    let duration = seconds
        .trim()
        .parse::<f64>()
        .ok()
        .and_then(|seconds| Duration::try_from_secs_f64(seconds).ok());
    if duration.is_none() {
        tracing::warn!(
            seconds = seconds.trim(),
            fixture = name,
            "the fixture holds no number of seconds"
        );
    }
    duration
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

    #[test]
    fn acknowledges_one_input_late_only_inside_a_native_evidence_task() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("fixture");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join(LATE_ACKNOWLEDGEMENT), "20\n").unwrap();
        assert_eq!(take_lateness_in(&home), None);
        assert!(home.join(LATE_ACKNOWLEDGEMENT).is_file());

        std::fs::write(root.path().join(".native-evidence"), "").unwrap();
        assert_eq!(take_lateness_in(&home), Some(Duration::from_secs(20)));
        assert_eq!(take_lateness_in(&home), None);

        std::fs::write(home.join(LATE_ACKNOWLEDGEMENT), "3600").unwrap();
        assert_eq!(take_lateness_in(&home), Some(LONGEST_LATENESS));
    }

    #[test]
    fn stops_answering_reads_for_a_while_only_inside_a_native_evidence_task() {
        let root = tempfile::tempdir().unwrap();
        let home = root.path().join("fixture");
        std::fs::create_dir(&home).unwrap();
        std::fs::write(home.join(UNANSWERED_READS), "60\n").unwrap();
        // Its own silence, so the helper's reads in other tests go on.
        let silent_until = Mutex::new(None);
        let soon = || Instant::now() + Duration::from_millis(50);
        assert_eq!(hold_read_in(&home, &silent_until, soon()), Ok(()));
        assert!(home.join(UNANSWERED_READS).is_file());

        std::fs::write(root.path().join(".native-evidence"), "").unwrap();
        // Every read in the silence waits for its deadline and misses it.
        for _ in 0..2 {
            let started = Instant::now();
            assert_eq!(
                hold_read_in(&home, &silent_until, soon()),
                Err(crate::chat::failure::actor_transport::DEADLINE.into())
            );
            assert!(started.elapsed() >= Duration::from_millis(50));
        }
        assert!(!home.join(UNANSWERED_READS).exists());
        // A read whose deadline outlasts the silence waits for it, then goes
        // through, as do the reads after it.
        std::fs::write(home.join(UNANSWERED_READS), "0.05").unwrap();
        assert_eq!(
            hold_read_in(
                &home,
                &silent_until,
                Instant::now() + Duration::from_secs(5)
            ),
            Ok(())
        );
        assert_eq!(hold_read_in(&home, &silent_until, soon()), Ok(()));
    }
}
