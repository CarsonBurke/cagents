use super::{Provider, ProviderAdapter};
use std::path::PathBuf;

pub(super) const ADAPTER: ProviderAdapter = ProviderAdapter {
    provider: Provider::Codex,
    executable: "codex",
    label: "Codex",
    recognizes_script: |path| {
        matches!(
            path.file_name().and_then(|name| name.to_str()),
            Some("codex" | "codex.js")
        )
    },
    session_file_id: |stem| {
        (stem.starts_with("rollout-") && stem.len() >= 36).then_some(())?;
        stem.get(stem.len() - 36..)
    },
    session_root: |launch, home| {
        Some(
            launch
                .environment
                .get("CODEX_HOME")
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".codex")),
        )
    },
    #[cfg(feature = "host")]
    session_directory: "sessions",
    // Configurable titles do not provide an unconditional lifecycle grammar.
    activity_from_title: |_| None,
    identity_options: &["resume"],
    cwd_options: &["-C", "--cd"],
    value_options: &[
        "--model",
        "-m",
        "--profile",
        "-p",
        "--config",
        "-c",
        "--sandbox",
        "-s",
        "--ask-for-approval",
        "-a",
        "--add-dir",
        "--remote",
        "--remote-auth-token-env",
    ],
    switches: &[
        "--no-alt-screen",
        "--search",
        "--dangerously-bypass-approvals-and-sandbox",
    ],
    resume_flag: "resume",
    resume_precedes_options: true,
    #[cfg(feature = "host")]
    forwards_to_native_child: true,
    #[cfg(feature = "host")]
    create_chat: crate::chat::protocol::codex::create,
};

/// Directory entries a rollout search visits before giving up.
#[cfg(feature = "host")]
const ROLLOUT_SCAN: usize = 262_144;

/// The one rollout recording thread `id` under a Codex home: Codex files each
/// as `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`. Links are refused and
/// the scan is bounded, so a damaged or hostile home cannot redirect or stall it.
#[cfg(feature = "host")]
pub(crate) fn rollout(home: &std::path::Path, id: &str) -> Result<PathBuf, String> {
    fn find(
        root: &std::path::Path,
        depth: usize,
        suffix: &str,
        budget: &mut usize,
        found: &mut Vec<PathBuf>,
    ) -> Result<(), String> {
        if depth == 0 {
            return Ok(());
        }
        crate::secure_file::reject_symlink_components(root).map_err(|error| error.to_string())?;
        for entry in std::fs::read_dir(root).map_err(|error| error.to_string())? {
            if *budget == 0 {
                return Err("The Codex history is too large to search".into());
            }
            *budget -= 1;
            let entry = entry.map_err(|error| error.to_string())?;
            let kind = entry.file_type().map_err(|error| error.to_string())?;
            if kind.is_dir() {
                find(&entry.path(), depth - 1, suffix, budget, found)?;
            } else if kind.is_file() && entry.file_name().to_string_lossy().ends_with(suffix) {
                found.push(entry.path());
            }
        }
        Ok(())
    }
    if uuid::Uuid::parse_str(id).is_err() || id.len() != 36 {
        return Err("Codex thread ID is not an exact UUID".into());
    }
    let mut found = Vec::new();
    let mut budget = ROLLOUT_SCAN;
    find(
        &home.join("sessions"),
        4,
        &format!("-{id}.jsonl"),
        &mut budget,
        &mut found,
    )?;
    match <[PathBuf; 1]>::try_from(found) {
        Ok([path]) => Ok(path),
        Err(found) if found.is_empty() => Err("The Codex thread's rollout is missing".into()),
        Err(_) => Err("The Codex thread has more than one rollout".into()),
    }
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;

    #[test]
    fn rollout_finds_one_dated_thread_file_and_refuses_ambiguity_and_links() {
        let id = "019a0000-0000-7000-8000-000000000001";
        let home = tempfile::tempdir().unwrap();
        let day = home.path().join("sessions/2026/11/06");
        std::fs::create_dir_all(&day).unwrap();
        assert!(rollout(home.path(), id).unwrap_err().contains("missing"));
        let path = day.join(format!("rollout-2026-11-06T10-00-00-{id}.jsonl"));
        std::fs::write(&path, "").unwrap();
        std::fs::write(day.join("rollout-2026-11-06T10-00-00-other.jsonl"), "").unwrap();
        assert_eq!(rollout(home.path(), id).unwrap(), path);
        assert!(
            rollout(home.path(), "../sessions")
                .unwrap_err()
                .contains("UUID")
        );

        let later = home.path().join("sessions/2026/11/07");
        std::fs::create_dir_all(&later).unwrap();
        let copy = later.join(format!("rollout-2026-11-07T10-00-00-{id}.jsonl"));
        std::fs::write(&copy, "").unwrap();
        assert!(
            rollout(home.path(), id)
                .unwrap_err()
                .contains("more than one")
        );
        std::fs::remove_file(&copy).unwrap();
        std::os::unix::fs::symlink(&path, &copy).unwrap();
        assert_eq!(
            rollout(home.path(), id).unwrap(),
            path,
            "a link is not a rollout"
        );

        let elsewhere = tempfile::tempdir().unwrap();
        let linked = elsewhere.path().join("sessions");
        std::os::unix::fs::symlink(home.path().join("sessions"), &linked).unwrap();
        assert!(
            rollout(elsewhere.path(), id).is_err(),
            "a linked history root is refused"
        );
    }
}
