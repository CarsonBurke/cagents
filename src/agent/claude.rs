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

/// The transcript of conversation `id` that Claude Code keeps under `config`
/// for a session started in `cwd`, which must already be canonical.
///
/// Claude names a project directory after its working directory with every
/// character but ASCII alphanumerics replaced, and cuts a long name at 200
/// characters followed by a hash this does not reproduce. Such a name is found
/// by the transcript it holds and refused when more than one does.
#[cfg(feature = "host")]
pub(crate) fn transcript(config: &Path, cwd: &Path, id: &str) -> Result<PathBuf, String> {
    // Only a hyphenated UUID names a transcript, never a path.
    if id.len() != 36 || uuid::Uuid::parse_str(id).is_err() {
        return Err("Claude session ID is not an exact UUID".into());
    }
    let cwd_text = cwd
        .to_str()
        .ok_or("Claude history directory is not UTF-8")?;
    let root = config
        .join("projects")
        .canonicalize()
        .map_err(|e| format!("Claude history project directory is unavailable: {e}"))?;
    let sanitized: String = cwd_text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    let filename = format!("{id}.jsonl");
    let project = if sanitized.len() <= 200 {
        root.join(sanitized)
    } else {
        let prefix = format!("{}-", &sanitized[..200]);
        let mut candidates = std::fs::read_dir(&root)
            .map_err(|e| e.to_string())?
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().starts_with(&prefix))
            .map(|entry| entry.path())
            .filter(|path| path.join(&filename).is_file());
        let candidate = candidates
            .next()
            .ok_or("Exact Claude transcript not found for this long project path")?;
        if candidates.next().is_some() {
            return Err(
                "Ambiguous Claude project hash; refusing to choose a history directory".into(),
            );
        }
        candidate
    };
    let project = project
        .canonicalize()
        .map_err(|e| format!("Claude project history is unavailable: {e}"))?;
    if !project.starts_with(&root) {
        return Err("Claude project history escapes its configuration directory".into());
    }
    let path = project.join(filename);
    if std::fs::symlink_metadata(&path)
        .map_err(|e| format!("Exact Claude session history is unavailable: {e}"))?
        .file_type()
        .is_symlink()
    {
        return Err("Refusing a symlinked Claude session transcript".into());
    }
    Ok(path)
}
