use super::{Provider, ProviderAdapter};
#[cfg(feature = "host")]
use std::path::Path;
use std::path::PathBuf;

pub(super) const ADAPTER: ProviderAdapter = ProviderAdapter {
    provider: Provider::Claude,
    executable: "claude",
    label: "Claude Code",
    recognizes_script: |path| match path.file_name().and_then(|name| name.to_str()) {
        Some("claude") => true,
        Some("cli.js") => path
            .to_str()
            .is_some_and(|path| path.contains("@anthropic-ai/claude-code/")),
        _ => false,
    },
    session_file_id: |stem| Some(stem),
    session_root: |launch, home| {
        Some(
            launch
                .environment
                .get("CLAUDE_CONFIG_DIR")
                .filter(|config| !config.is_empty())
                .map(PathBuf::from)
                .unwrap_or_else(|| home.join(".claude")),
        )
    },
    #[cfg(feature = "host")]
    session_directory: "projects",
    // No authoritative lifecycle title grammar: CLI activity uses its agents API.
    activity_from_title: |_| None,
    identity_options: &["--resume", "-r", "--session-id"],
    cwd_options: &[],
    value_options: &[
        "--model",
        "--agent",
        "--permission-mode",
        "--settings",
        "--setting-sources",
        "--effort",
    ],
    switches: &[
        "--dangerously-skip-permissions",
        "--bare",
        "--safe-mode",
        "--no-chrome",
    ],
    resume_flag: "--resume",
    resume_precedes_options: false,
    #[cfg(feature = "host")]
    forwards_to_native_child: false,
    #[cfg(feature = "host")]
    create_chat: crate::chat::protocol::claude::create,
};

/// Whether Claude Code has a transcript for `id` in `cwd`, even one that
/// [`transcript`] refuses, such as a symlink. Claude Code resumes from any
/// file it finds there; with none, it has nothing to resume.
#[cfg(feature = "host")]
pub(crate) fn transcript_exists(config: &Path, cwd: &Path, id: &str) -> bool {
    let Some(cwd) = cwd.to_str() else {
        return false;
    };
    let sanitized: String = cwd
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let root = config.join("projects");
    let filename = format!("{id}.jsonl");
    if sanitized.len() <= 200 {
        return std::fs::symlink_metadata(root.join(sanitized).join(filename)).is_ok();
    }
    let prefix = format!("{}-", &sanitized[..200]);
    std::fs::read_dir(&root).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|entry| {
            entry.file_name().to_string_lossy().starts_with(&prefix)
                && std::fs::symlink_metadata(entry.path().join(&filename)).is_ok()
        })
    })
}

/// The transcript of conversation `id` that Claude Code keeps under `config`
/// for a session started in `cwd`, which must already be canonical.
///
/// Claude names a project directory after its working directory with every
/// character but ASCII alphanumerics replaced, and cuts a long name at 200
/// characters followed by a hash this does not reproduce. Such a name is found
/// by the transcript it holds and refused when more than one does.
#[cfg(feature = "host")]
pub(crate) fn transcript(config: &Path, cwd: &Path, id: &str) -> Result<PathBuf, String> {
    transcript_for_start(config, cwd, id).map_err(|error| error.to_string())
}

/// Invalid identity/path evidence cannot be repaired by repeating this exact
/// lookup. Missing or inaccessible history can become available without
/// changing the selected conversation.
#[cfg(feature = "host")]
#[derive(Debug)]
pub(crate) enum TranscriptError {
    Invalid(String),
    Unavailable(String),
}

#[cfg(feature = "host")]
impl std::fmt::Display for TranscriptError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(message) | Self::Unavailable(message) => message.fmt(formatter),
        }
    }
}

#[cfg(feature = "host")]
pub(crate) fn transcript_for_start(
    config: &Path,
    cwd: &Path,
    id: &str,
) -> Result<PathBuf, TranscriptError> {
    use TranscriptError::{Invalid, Unavailable};
    // Only a hyphenated UUID names a transcript, never a path.
    if id.len() != 36 || uuid::Uuid::parse_str(id).is_err() {
        return Err(Invalid("Claude session ID is not an exact UUID".into()));
    }
    let cwd_text = cwd
        .to_str()
        .ok_or_else(|| Invalid("Claude history directory is not UTF-8".into()))?;
    let root = config.join("projects").canonicalize().map_err(|e| {
        Unavailable(format!(
            "Claude history project directory is unavailable: {e}"
        ))
    })?;
    let sanitized: String = cwd_text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let filename = format!("{id}.jsonl");
    let project = if sanitized.len() <= 200 {
        root.join(sanitized)
    } else {
        let prefix = format!("{}-", &sanitized[..200]);
        let entries = std::fs::read_dir(&root).map_err(|e| Unavailable(e.to_string()))?;
        let mut candidate = None;
        for entry in entries {
            let entry = entry.map_err(|e| Unavailable(e.to_string()))?;
            if !entry.file_name().to_string_lossy().starts_with(&prefix) {
                continue;
            }
            match std::fs::symlink_metadata(entry.path().join(&filename)) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    return Err(Invalid(
                        "Refusing a symlinked Claude session transcript".into(),
                    ));
                }
                Ok(metadata) if metadata.is_file() => {
                    if candidate.replace(entry.path()).is_some() {
                        return Err(Invalid(
                            "Ambiguous Claude project hash; refusing to choose a history directory"
                                .into(),
                        ));
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(Unavailable(error.to_string())),
            }
        }
        candidate.ok_or_else(|| {
            Unavailable("Exact Claude transcript not found for this long project path".into())
        })?
    };
    let project = project
        .canonicalize()
        .map_err(|e| Unavailable(format!("Claude project history is unavailable: {e}")))?;
    if !project.starts_with(&root) {
        return Err(Invalid(
            "Claude project history escapes its configuration directory".into(),
        ));
    }
    let path = project.join(filename);
    if std::fs::symlink_metadata(&path)
        .map_err(|e| Unavailable(format!("Exact Claude session history is unavailable: {e}")))?
        .file_type()
        .is_symlink()
    {
        return Err(Invalid(
            "Refusing a symlinked Claude session transcript".into(),
        ));
    }
    Ok(path)
}

#[cfg(all(test, feature = "host"))]
mod tests {
    use super::*;
    use std::os::unix::{ffi::OsStringExt, fs::symlink};

    const SESSION: &str = "d1ffb0e0-d588-478a-94e4-be7d954d3c5b";

    fn project(config: &Path, cwd: &Path) -> PathBuf {
        config.join("projects").join(
            cwd.to_str()
                .unwrap()
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect::<String>(),
        )
    }

    #[test]
    fn missing_claude_history_is_unavailable_until_the_same_path_is_restored() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("claude");
        let cwd = root.path().join("project");
        std::fs::create_dir(&cwd).unwrap();
        let directory = project(&config, &cwd);
        let file = directory.join(format!("{SESSION}.jsonl"));
        for repair in 0..3 {
            let failure = transcript_for_start(&config, &cwd, SESSION).unwrap_err();
            assert!(matches!(failure, TranscriptError::Unavailable(_)));
            assert_eq!(
                transcript(&config, &cwd, SESSION).unwrap_err(),
                failure.to_string(),
                "the existing string API preserves the diagnostic"
            );
            match repair {
                0 => std::fs::create_dir_all(config.join("projects")).unwrap(),
                1 => std::fs::create_dir(&directory).unwrap(),
                2 => std::fs::write(&file, b"{}").unwrap(),
                _ => unreachable!(),
            }
        }
        assert_eq!(transcript_for_start(&config, &cwd, SESSION).unwrap(), file);
    }

    #[test]
    fn invalid_claude_history_identity_and_symlinks_remain_invalid() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("claude");
        let cwd = root.path().join("project");
        std::fs::create_dir(&cwd).unwrap();
        assert!(matches!(
            transcript_for_start(&config, &cwd, "../not-a-session"),
            Err(TranscriptError::Invalid(_))
        ));
        let non_utf8 = PathBuf::from(std::ffi::OsString::from_vec(vec![b'/', 0xff]));
        assert!(matches!(
            transcript_for_start(&config, &non_utf8, SESSION),
            Err(TranscriptError::Invalid(_))
        ));

        let directory = project(&config, &cwd);
        std::fs::create_dir_all(&directory).unwrap();
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        let file = outside.join("saved.jsonl");
        std::fs::write(&file, b"{}").unwrap();
        let selected = directory.join(format!("{SESSION}.jsonl"));
        symlink(&file, &selected).unwrap();
        assert!(matches!(
            transcript_for_start(&config, &cwd, SESSION),
            Err(TranscriptError::Invalid(message)) if message.contains("symlinked")
        ));
        std::fs::remove_file(&selected).unwrap();
        std::fs::remove_dir(&directory).unwrap();
        symlink(&outside, &directory).unwrap();
        assert!(matches!(
            transcript_for_start(&config, &cwd, SESSION),
            Err(TranscriptError::Invalid(message)) if message.contains("escapes")
        ));
    }

    #[test]
    fn missing_hashed_claude_history_is_unavailable_but_ambiguity_is_invalid() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("claude");
        let cwd = root.path().join("a".repeat(210));
        std::fs::create_dir(&cwd).unwrap();
        let sanitized = project(&config, &cwd);
        let prefix = &sanitized.file_name().unwrap().to_str().unwrap()[..200];
        std::fs::create_dir_all(config.join("projects")).unwrap();
        assert!(matches!(
            transcript_for_start(&config, &cwd, SESSION),
            Err(TranscriptError::Unavailable(_))
        ));
        let first = config.join("projects").join(format!("{prefix}-first"));
        let second = config.join("projects").join(format!("{prefix}-second"));
        for directory in [&first, &second] {
            std::fs::create_dir(directory).unwrap();
            std::fs::write(directory.join(format!("{SESSION}.jsonl")), b"{}").unwrap();
            if directory == &first {
                assert_eq!(
                    transcript_for_start(&config, &cwd, SESSION).unwrap(),
                    first.join(format!("{SESSION}.jsonl"))
                );
            }
        }
        assert!(matches!(
            transcript_for_start(&config, &cwd, SESSION),
            Err(TranscriptError::Invalid(message)) if message.contains("Ambiguous")
        ));
    }
}
