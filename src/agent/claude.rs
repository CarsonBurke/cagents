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

/// Claude Code's project-key and candidate-directory rules, shared by the
/// presence probe and the stricter reader. They intentionally apply different
/// file policies: presence includes unreadable or symlinked transcripts.
#[cfg(feature = "host")]
struct TranscriptLookup {
    project: String,
    filename: String,
}
#[cfg(feature = "host")]
impl TranscriptLookup {
    fn new(cwd: &Path, id: &str) -> Result<Self, String> {
        if id.len() != 36 || uuid::Uuid::parse_str(id).is_err() {
            return Err("Claude session ID is not an exact UUID".into());
        }
        let cwd = cwd
            .to_str()
            .ok_or("Claude history directory is not UTF-8")?;
        Ok(Self {
            project: cwd
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect(),
            filename: format!("{id}.jsonl"),
        })
    }
    fn hashed(&self) -> bool {
        self.project.len() > 200
    }
    fn projects(
        &self,
        root: &Path,
    ) -> std::io::Result<Box<dyn Iterator<Item = std::io::Result<PathBuf>>>> {
        if !self.hashed() {
            return Ok(Box::new(std::iter::once(Ok(root.join(&self.project)))));
        }
        let prefix = format!("{}-", &self.project[..200]);
        Ok(Box::new(std::fs::read_dir(root)?.filter_map(
            move |entry| match entry {
                Ok(entry) if entry.file_name().to_string_lossy().starts_with(&prefix) => {
                    Some(Ok(entry.path()))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            },
        )))
    }
}

/// Whether Claude Code has a transcript for `id` in `cwd`, even one that
/// [`transcript`] refuses, such as a symlink. Claude Code resumes from any
/// file it finds there; with none, it has nothing to resume.
#[cfg(feature = "host")]
pub(crate) fn transcript_exists(config: &Path, cwd: &Path, id: &str) -> bool {
    let Ok(lookup) = TranscriptLookup::new(cwd, id) else {
        return false;
    };
    lookup
        .projects(&config.join("projects"))
        .is_ok_and(|projects| {
            projects
                .filter_map(Result::ok)
                .any(|project| std::fs::symlink_metadata(project.join(&lookup.filename)).is_ok())
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
    let lookup = TranscriptLookup::new(cwd, id).map_err(Invalid)?;
    let root = config.join("projects").canonicalize().map_err(|e| {
        Unavailable(format!(
            "Claude history project directory is unavailable: {e}"
        ))
    })?;
    let mut projects = lookup
        .projects(&root)
        .map_err(|e| Unavailable(e.to_string()))?;
    let project = if lookup.hashed() {
        let mut candidate = None;
        let mut dangling_transcript = false;
        for project in projects {
            let project = project.map_err(|e| Unavailable(e.to_string()))?;
            let path = project.join(&lookup.filename);
            // Match Claude's file selection before applying the strict reader's
            // symlink policy. An unrelated dangling link is not a second match.
            match std::fs::metadata(&path) {
                Ok(metadata) if metadata.is_file() => {
                    if candidate.replace(project).is_some() {
                        return Err(Invalid(
                            "Ambiguous Claude project hash; refusing to choose a history directory"
                                .into(),
                        ));
                    }
                }
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    match std::fs::symlink_metadata(&path) {
                        Ok(metadata) => dangling_transcript |= metadata.file_type().is_symlink(),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(Unavailable(error.to_string())),
                    }
                }
                Err(error) => return Err(Unavailable(error.to_string())),
            }
        }
        candidate.ok_or_else(|| {
            let message = "Exact Claude transcript not found for this long project path".into();
            if dangling_transcript {
                Invalid(message)
            } else {
                Unavailable(message)
            }
        })?
    } else {
        projects
            .next()
            .expect("an exact project has one candidate")
            .map_err(|e| Unavailable(e.to_string()))?
    };
    let project = project
        .canonicalize()
        .map_err(|e| Unavailable(format!("Claude project history is unavailable: {e}")))?;
    if !project.starts_with(&root) {
        return Err(Invalid(
            "Claude project history escapes its configuration directory".into(),
        ));
    }
    let path = project.join(lookup.filename);
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
    fn hashed_claude_history_directory_io_is_unavailable_until_repaired() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("claude");
        let cwd = root.path().join("a".repeat(210));
        std::fs::create_dir(&config).unwrap();
        let projects = config.join("projects");
        std::fs::write(&projects, b"not a directory").unwrap();
        assert!(matches!(
            transcript_for_start(&config, &cwd, SESSION),
            Err(TranscriptError::Unavailable(_))
        ));
        std::fs::remove_file(&projects).unwrap();
        let sanitized = project(&config, &cwd);
        let prefix = &sanitized.file_name().unwrap().to_str().unwrap()[..200];
        let directory = projects.join(format!("{prefix}-repaired"));
        std::fs::create_dir_all(&directory).unwrap();
        let file = directory.join(format!("{SESSION}.jsonl"));
        std::fs::write(&file, b"{}").unwrap();
        assert_eq!(transcript_for_start(&config, &cwd, SESSION).unwrap(), file);
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

#[cfg(all(test, feature = "host"))]
mod transcript_lookup_tests {
    use super::*;
    const ID: &str = "00000000-0000-4000-8000-000000000001";

    #[test]
    fn transcript_presence_and_reader_share_exact_session_validation() {
        let root = tempfile::tempdir().unwrap();
        let directory = root.path().join("projects/-project");
        std::fs::create_dir_all(&directory).unwrap();
        std::fs::write(directory.join("not-a-uuid.jsonl"), b"{}").unwrap();
        assert!(transcript(root.path(), Path::new("/project"), "not-a-uuid").is_err());
        assert!(!transcript_exists(
            root.path(),
            Path::new("/project"),
            "not-a-uuid"
        ));
    }

    #[test]
    fn transcript_presence_and_reader_agree_on_unicode_and_long_path_candidates() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let unicode = root.path().join("projects/-work-na-ve-app-project");
        std::fs::create_dir_all(&unicode).unwrap();
        let exact = unicode.join(format!("{ID}.jsonl"));
        std::fs::write(&exact, b"{}").unwrap();
        assert!(transcript_exists(
            root.path(),
            Path::new("/work/naïve app/project"),
            ID
        ));
        assert_eq!(
            transcript(root.path(), Path::new("/work/naïve app/project"), ID).unwrap(),
            exact.canonicalize().unwrap()
        );
        let cwd = format!("/{}", "a".repeat(210));
        let prefix = format!("-{}-", "a".repeat(199));
        let first = root.path().join("projects").join(format!("{prefix}first"));
        let second = root.path().join("projects").join(format!("{prefix}second"));
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let file = first.join(format!("{ID}.jsonl"));
        std::fs::write(&file, b"{}").unwrap();
        symlink("missing.jsonl", second.join(format!("{ID}.jsonl"))).unwrap();
        assert!(transcript_exists(root.path(), Path::new(&cwd), ID));
        assert_eq!(
            transcript(root.path(), Path::new(&cwd), ID).unwrap(),
            file.canonicalize().unwrap()
        );
        std::fs::remove_file(&file).unwrap();
        assert!(
            transcript_exists(root.path(), Path::new(&cwd), ID),
            "presence includes a dangling transcript link"
        );
        assert!(matches!(
            transcript_for_start(root.path(), Path::new(&cwd), ID),
            Err(TranscriptError::Invalid(message))
                if message == "Exact Claude transcript not found for this long project path"
        ));
        assert_eq!(
            transcript(root.path(), Path::new(&cwd), ID).unwrap_err(),
            "Exact Claude transcript not found for this long project path"
        );
        std::fs::remove_file(second.join(format!("{ID}.jsonl"))).unwrap();
        assert!(!transcript_exists(root.path(), Path::new(&cwd), ID));
        assert!(matches!(
            transcript_for_start(root.path(), Path::new(&cwd), ID),
            Err(TranscriptError::Unavailable(_))
        ));
    }

    #[test]
    fn transcript_reader_keeps_long_path_ambiguity_and_escape_checks() {
        use std::os::unix::fs::symlink;
        let root = tempfile::tempdir().unwrap();
        let cwd = format!("/{}", "a".repeat(210));
        let prefix = format!("-{}-", "a".repeat(199));
        let projects = root.path().join("projects");
        std::fs::create_dir(&projects).unwrap();
        for suffix in ["first", "second"] {
            let project = projects.join(format!("{prefix}{suffix}"));
            std::fs::create_dir(&project).unwrap();
            std::fs::write(project.join(format!("{ID}.jsonl")), b"{}").unwrap();
        }
        assert!(transcript_exists(root.path(), Path::new(&cwd), ID));
        assert!(
            transcript(root.path(), Path::new(&cwd), ID)
                .unwrap_err()
                .contains("Ambiguous")
        );
        for suffix in ["first", "second"] {
            std::fs::remove_dir_all(projects.join(format!("{prefix}{suffix}"))).unwrap();
        }
        let outside = root.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join(format!("{ID}.jsonl")), b"{}").unwrap();
        symlink(&outside, projects.join(format!("{prefix}escape"))).unwrap();
        assert!(transcript_exists(root.path(), Path::new(&cwd), ID));
        assert!(
            transcript(root.path(), Path::new(&cwd), ID)
                .unwrap_err()
                .contains("escapes")
        );
    }
}
