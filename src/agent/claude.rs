use super::{Provider, ProviderAdapter};
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
