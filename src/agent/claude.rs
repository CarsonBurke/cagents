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
    // What Claude Code exports to the commands and subagents a session runs,
    // and the socket through which other sessions message that session.
    session_environment: |name| {
        matches!(
            name,
            "CLAUDECODE"
                | "CLAUDE_CODE_ENTRYPOINT"
                | "CLAUDE_CODE_SESSION_ID"
                | "CLAUDE_CODE_CHILD_SESSION"
                | "CLAUDE_CODE_SESSION_ATTENDED"
                | "CLAUDE_CODE_MESSAGING_SOCKET"
                | "CLAUDE_CODE_MESSAGING_TOKEN"
                | "CLAUDE_CODE_SSE_PORT"
                | "CLAUDE_CODE_EXECPATH"
                | "CLAUDE_CODE_TMPDIR"
                | "CLAUDE_CODE_INVOKED_SKILLS"
                | "CLAUDE_PID"
                | "CLAUDE_EFFORT"
                | "AI_AGENT"
                | "TRACEPARENT"
        )
    },
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
    fn projects(&self, root: &Path) -> Result<Box<dyn Iterator<Item = PathBuf>>, String> {
        if !self.hashed() {
            return Ok(Box::new(std::iter::once(root.join(&self.project))));
        }
        let prefix = format!("{}-", &self.project[..200]);
        Ok(Box::new(
            std::fs::read_dir(root)
                .map_err(|error| error.to_string())?
                .filter_map(Result::ok)
                .filter(move |entry| entry.file_name().to_string_lossy().starts_with(&prefix))
                .map(|entry| entry.path()),
        ))
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
        .is_ok_and(|mut projects| {
            projects
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
    let lookup = TranscriptLookup::new(cwd, id)?;
    let root = config
        .join("projects")
        .canonicalize()
        .map_err(|e| format!("Claude history project directory is unavailable: {e}"))?;
    let mut projects = lookup.projects(&root)?;
    let project = if lookup.hashed() {
        let mut candidates = projects.filter(|project| project.join(&lookup.filename).is_file());
        let candidate = candidates
            .next()
            .ok_or("Exact Claude transcript not found for this long project path")?;
        if candidates.next().is_some() {
            return Err(
                "Ambiguous Claude project hash; refusing to choose a history directory".into(),
            );
        }
        candidate
    } else {
        projects.next().expect("an exact project has one candidate")
    };
    let project = project
        .canonicalize()
        .map_err(|e| format!("Claude project history is unavailable: {e}"))?;
    if !project.starts_with(&root) {
        return Err("Claude project history escapes its configuration directory".into());
    }
    let path = project.join(lookup.filename);
    if std::fs::symlink_metadata(&path)
        .map_err(|e| format!("Exact Claude session history is unavailable: {e}"))?
        .file_type()
        .is_symlink()
    {
        return Err("Refusing a symlinked Claude session transcript".into());
    }
    Ok(path)
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
        assert!(transcript(root.path(), Path::new(&cwd), ID).is_err());
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
