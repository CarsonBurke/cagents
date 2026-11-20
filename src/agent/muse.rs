//! Muse, Meta's terminal coding agent. Its native chat is its Agent Client
//! Protocol server, `muse acp` (see `chat::protocol::acp::muse`).
use super::{Provider, ProviderAdapter};
use std::path::{Path, PathBuf};

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
    native_chat: true,
    // What each of its session modes means; "Deny unmatched actions"
    // (dontAsk) has no terminal option, but a native conversation sets it.
    permission_modes: &["manual", "auto", "dontAsk", "bypassPermissions"],
    // Its terminal's `--approval-mode` names, the own names of `acp.modes`.
    permission_mode_aliases: &["untrusted", "on-request", "never"],
    permission_sandboxes: &[],
    acp: Some(super::Acp {
        mode_option: "--approval-mode",
        effort_option: "--reasoning-effort",
        // Its terminal's approval modes, which its sessions call "Ask before
        // unmatched actions", "Ask on request" and "Allow all". The session's
        // fourth, "Deny unmatched actions", has no terminal option.
        modes: &[
            ("untrusted", "manual"),
            ("on-request", "auto"),
            ("never", "bypassPermissions"),
        ],
        default_mode: "on-request",
    }),
    #[cfg(feature = "host")]
    forwards_to_native_child: false,
    #[cfg(feature = "host")]
    create_chat: |launch| {
        crate::chat::protocol::acp::create(&crate::chat::protocol::acp::muse::PROFILE, launch)
    },
};

/// The executable a wrapper named `muse` conventionally runs.
const UPSTREAM: &str = "muse-upstream";

/// The Muse executable that takes a subcommand first, for `program`. Muse
/// refuses a root option before a subcommand, so a `muse` that is a wrapper
/// adding root options cannot run `acp`, `login` or `usage`; the launcher it
/// wraps, `muse-upstream`, can. That is chosen when it is beside `program`
/// or on `path`; otherwise `program` is. A relative `program` or `path` entry
/// is taken from `directory`, as a launch's is.
pub(super) fn subcommand_program(program: &str, directory: &Path, path: &str) -> String {
    let named = Path::new(program);
    if named.file_name().and_then(|name| name.to_str()) != Some(ADAPTER.executable) {
        return program.to_owned();
    }
    let runnable = |path: &Path| {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(directory.join(path))
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    };
    if program.contains('/') {
        let beside = named.with_file_name(UPSTREAM);
        if runnable(&beside) {
            return beside.to_string_lossy().into_owned();
        }
    }
    if std::env::split_paths(path)
        .map(|directory| directory.join(UPSTREAM))
        .any(|candidate| runnable(&candidate))
    {
        return UPSTREAM.to_owned();
    }
    program.to_owned()
}

/// Models Muse marks "contributor": cheaper because Meta may use the
/// conversation, including messages between sessions, for product
/// improvement. cagents shares no data unless asked, so it never offers them.
#[cfg(any(feature = "ui", feature = "host"))]
pub fn offers_model(model: &str) -> bool {
    !model.ends_with("-contributor")
}
