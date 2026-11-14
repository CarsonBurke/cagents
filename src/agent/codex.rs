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
    // What Codex exports to the commands a session runs: its thread, its
    // sandbox and the socket through which they ask it to escalate.
    session_environment: |name| {
        matches!(
            name,
            "CODEX_THREAD_ID"
                | "CODEX_SESSION_ID"
                | "CODEX_SANDBOX"
                | "CODEX_SANDBOX_NETWORK_DISABLED"
                | "CODEX_ESCALATE_SOCKET"
                | "CODEX_CI"
        )
    },
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
/// as `sessions/YYYY/MM/DD/rollout-<time>-<id>.jsonl`, dated locally. A
/// time-ordered thread ID names its day, so the days around it are searched
/// first; otherwise, or when they hold none, every day is. The home itself
/// may be a link, as when it is kept with other configuration; links inside
/// it are refused and the scan is bounded, so a damaged or hostile history
/// cannot redirect or stall it.
#[cfg(feature = "host")]
pub(crate) fn rollout(home: &std::path::Path, id: &str) -> Result<PathBuf, String> {
    let suffix = rollout_suffix(id)?;
    let sessions = resolved_home(home)?.join("sessions");
    let mut budget = ROLLOUT_SCAN;
    let mut found = Vec::new();
    for day in creation_days(id) {
        find_rollouts(&sessions.join(day), 1, &suffix, &mut budget, &mut found)?;
    }
    if !found.is_empty() {
        return one_rollout(found);
    }
    let mut found = Vec::new();
    find_rollouts(&sessions, 4, &suffix, &mut budget, &mut found)?;
    one_rollout(found)
}

/// A rollout Codex archived, which it moves out of the dated tree into one
/// flat directory.
#[cfg(feature = "host")]
pub(crate) fn archived_rollout(home: &std::path::Path, id: &str) -> Result<PathBuf, String> {
    let suffix = rollout_suffix(id)?;
    let mut found = Vec::new();
    let mut budget = ROLLOUT_SCAN;
    find_rollouts(
        &resolved_home(home)?.join("archived_sessions"),
        1,
        &suffix,
        &mut budget,
        &mut found,
    )?;
    one_rollout(found)
}

/// The Codex home with its links resolved, or a missing history when there
/// is none.
#[cfg(feature = "host")]
fn resolved_home(home: &std::path::Path) -> Result<PathBuf, String> {
    match home.canonicalize() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err("The Codex thread's rollout is missing".into())
        }
        result => result.map_err(|error| format!("The Codex history is unavailable: {error}")),
    }
}

#[cfg(feature = "host")]
fn rollout_suffix(id: &str) -> Result<String, String> {
    if id.len() != 36 || uuid::Uuid::parse_str(id).is_err() {
        return Err("Codex thread ID is not an exact UUID".into());
    }
    Ok(format!("-{id}.jsonl"))
}

#[cfg(feature = "host")]
fn one_rollout(found: Vec<PathBuf>) -> Result<PathBuf, String> {
    match <[PathBuf; 1]>::try_from(found) {
        Ok([path]) => Ok(path),
        Err(found) if found.is_empty() => Err("The Codex thread's rollout is missing".into()),
        Err(_) => Err("The Codex thread has more than one rollout".into()),
    }
}

/// `YYYY/MM/DD` for the UTC day a version-7 thread ID was minted and the
/// days either side, which cover every local time zone.
#[cfg(feature = "host")]
fn creation_days(id: &str) -> Vec<String> {
    let Ok(uuid) = uuid::Uuid::parse_str(id) else {
        return Vec::new();
    };
    let Some((seconds, _)) = uuid
        .get_timestamp()
        .filter(|_| uuid.get_version_num() == 7)
        .map(|time| time.to_unix())
    else {
        return Vec::new();
    };
    let Ok(at) = jiff::Timestamp::from_second(seconds as i64) else {
        return Vec::new();
    };
    let day = at.to_zoned(jiff::tz::TimeZone::UTC).date();
    [day.yesterday(), Ok(day), day.tomorrow()]
        .into_iter()
        .flatten()
        .map(|day| day.strftime("%Y/%m/%d").to_string())
        .collect()
}

#[cfg(feature = "host")]
fn find_rollouts(
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
    let entries = match std::fs::read_dir(root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        entries => entries.map_err(|error| error.to_string())?,
    };
    for entry in entries {
        if *budget == 0 {
            return Err("The Codex history is too large to search".into());
        }
        *budget -= 1;
        let entry = entry.map_err(|error| error.to_string())?;
        let kind = entry.file_type().map_err(|error| error.to_string())?;
        if kind.is_dir() {
            find_rollouts(&entry.path(), depth - 1, suffix, budget, found)?;
        } else if kind.is_file() && entry.file_name().to_string_lossy().ends_with(suffix) {
            found.push(entry.path());
        }
    }
    Ok(())
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;

    #[test]
    fn rollout_finds_one_dated_thread_file_and_refuses_ambiguity_and_links() {
        let id = "019a0000-0000-7000-8000-000000000001";
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().canonicalize().unwrap();
        let day = home.join("sessions/2026/11/06");
        std::fs::create_dir_all(&day).unwrap();
        assert!(rollout(&home, id).unwrap_err().contains("missing"));
        let path = day.join(format!("rollout-2026-11-06T10-00-00-{id}.jsonl"));
        std::fs::write(&path, "").unwrap();
        std::fs::write(day.join("rollout-2026-11-06T10-00-00-other.jsonl"), "").unwrap();
        assert_eq!(rollout(&home, id).unwrap(), path);
        assert!(rollout(&home, "../sessions").unwrap_err().contains("UUID"));

        let later = home.join("sessions/2026/11/07");
        std::fs::create_dir_all(&later).unwrap();
        let copy = later.join(format!("rollout-2026-11-07T10-00-00-{id}.jsonl"));
        std::fs::write(&copy, "").unwrap();
        assert!(rollout(&home, id).unwrap_err().contains("more than one"));
        std::fs::remove_file(&copy).unwrap();
        std::os::unix::fs::symlink(&path, &copy).unwrap();
        assert_eq!(rollout(&home, id).unwrap(), path, "a link is not a rollout");

        // A time-ordered ID is found on the days around its creation, even
        // when the full tree would be too large to search.
        let dated = "01a1b0ba-e200-7000-8000-000000000002";
        assert_eq!(
            creation_days(dated),
            ["2026/11/05", "2026/11/06", "2026/11/07"]
        );
        let near = later.join(format!("rollout-2026-11-07T01-00-00-{dated}.jsonl"));
        std::fs::write(&near, "").unwrap();
        assert_eq!(rollout(&home, dated).unwrap(), near);
        assert!(creation_days("00000000-0000-4000-8000-000000000001").is_empty());

        let archived = home.join("archived_sessions");
        std::fs::create_dir(&archived).unwrap();
        let old = archived.join(format!("rollout-2026-11-01T10-00-00-{id}.jsonl"));
        std::fs::write(&old, "").unwrap();
        assert_eq!(archived_rollout(&home, id).unwrap(), old);
        assert!(
            archived_rollout(&home, dated)
                .unwrap_err()
                .contains("missing")
        );

        let elsewhere = tempfile::tempdir().unwrap();
        let linked = elsewhere.path().join("sessions");
        std::os::unix::fs::symlink(home.join("sessions"), &linked).unwrap();
        assert!(
            rollout(elsewhere.path(), id).is_err(),
            "a linked history root is refused"
        );
        // A home kept elsewhere through a link, as with dotfiles, is read.
        let link = elsewhere.path().join("codex");
        std::os::unix::fs::symlink(&home, &link).unwrap();
        assert_eq!(rollout(&link, id).unwrap(), path);
        assert!(
            rollout(&elsewhere.path().join("absent"), id)
                .unwrap_err()
                .contains("missing")
        );
    }
}
