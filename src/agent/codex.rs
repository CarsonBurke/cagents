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
    forwards_to_native_child: true,
    #[cfg(feature = "host")]
    create_chat: crate::chat::protocol::codex::create,
};
