//! A development fixture that refuses one Claude input with a chosen delivery
//! outcome, so native evidence can show how a conversation reads after a send
//! it declined, which the synthetic provider cannot otherwise provoke.
//!
//! It is compiled only into debug builds, and acts only when `HOME` lies
//! inside a native-evidence task (an ancestor holds the task's
//! `.native-evidence` marker) and holds the [`MARKER`] file. The file names
//! one outcome of [`crate::chat::failure::delivery`], such as `CROWDED_INPUT`;
//! the next input is refused with that outcome before anything is sent, and
//! the file is removed, so the input after it goes through as usual. Any
//! other home, or a home without the marker, is unaffected.
use std::path::Path;

/// Created in the fixture's `HOME` to refuse the next input.
const MARKER: &str = ".cagents-fixture-refuse-input";

/// The outcome the fixture in this process's `HOME` refuses the next input
/// with, if it asks for one.
pub(super) fn take() -> Option<&'static str> {
    take_in(Path::new(&std::env::var_os("HOME")?))
}

fn take_in(home: &Path) -> Option<&'static str> {
    let home = home.canonicalize().ok()?;
    if !home
        .ancestors()
        .skip(1)
        .any(|path| path.join(".native-evidence").is_file())
    {
        return None;
    }
    let marker = home.join(MARKER);
    let name = std::fs::read_to_string(&marker).ok()?;
    std::fs::remove_file(&marker).ok()?;
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
}
