//! Muse, Meta's terminal coding agent. It runs only in a terminal until
//! cagents speaks its Agent Client Protocol server.
use super::{Provider, ProviderAdapter};
use std::path::PathBuf;

pub(super) const ADAPTER: ProviderAdapter = ProviderAdapter {
    provider: Provider::Muse,
    executable: "muse",
    // `muse` is often a wrapper that adds options to Meta's launcher.
    aliases: &["muse-upstream"],
    label: "Muse",
    // A shell launcher and a native executable, never a script an
    // interpreter cagents recognises runs.
    recognizes_script: |_| false,
    // Servers (ACP, MSP and MXP), one-shot runs, listings, exports and
    // account management. `resume` opens a conversation and stays.
    background_command: |arg| {
        matches!(
            arg,
            "acp"
                | "attach"
                | "auth"
                | "config"
                | "exec"
                | "export"
                | "help"
                | "init"
                | "login"
                | "logout"
                | "mcp"
                | "model-profile"
                | "mxp"
                | "plugins"
                | "sandbox"
                | "schema"
                | "serve"
                | "session-message"
                | "sessions"
                | "skills"
                | "trace"
                | "usage"
                | "voice"
        )
    },
    // Sessions are directories of event logs, not one transcript file.
    session_file_id: |_| None,
    session_root: |launch, home| {
        Some(
            launch
                .environment
                .get("XDG_DATA_HOME")
                .map(PathBuf::from)
                // The base directory spec ignores a relative value.
                .filter(|data| data.is_absolute())
                .unwrap_or_else(|| home.join(".local/share"))
                .join("muse"),
        )
    },
    #[cfg(feature = "host")]
    session_directory: "sessions",
    // Its terminal title has no lifecycle grammar.
    activity_from_title: |_| None,
    identity_options: &["resume"],
    // What Muse tells the agents a session starts about the session that
    // started them.
    session_environment: |name| name.starts_with("MUSE_PARENT_SESSION_"),
    cwd_options: &[],
    value_options: &[
        "--agents",
        "--provider",
        "--preset",
        "--model",
        "--contributor-mode",
        "--config",
        "-c",
        "--reasoning-effort",
        "--effort",
        "--base-url",
        "--workspace",
        "--worktree-base",
        "--worktree-existing",
        "--approval-mode",
        "--permission-profile",
        "--approval-judge",
        "--sandbox-network",
    ],
    switches: &[
        "--parallel-tool-calls",
        "--no-parallel-tool-calls",
        "--subagent-worktree-isolation",
        "--no-session-log",
        "--yolo",
        "--disable-request-user-input",
        "--trust-workspace",
        "--disable-approval",
        "--disable-sandbox",
        "--disable-write",
        "--disable-shell",
        "--enable-shell-tool",
    ],
    resume_flag: "resume",
    resume_precedes_options: true,
    // `muse resume` alone opens the session picker for the workspace.
    resume_picker: true,
    latest_selectors: &["--last"],
    native_chat: false,
    #[cfg(feature = "host")]
    forwards_to_native_child: false,
    #[cfg(feature = "host")]
    create_chat: |_| Err(Provider::Muse.native_chat_refusal()),
};
