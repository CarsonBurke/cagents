//! Antigravity (`agy`), Google's terminal coding agent. It runs only in a
//! terminal until cagents speaks its Agent Client Protocol server.
use super::{Provider, ProviderAdapter};

pub(super) const ADAPTER: ProviderAdapter = ProviderAdapter {
    provider: Provider::Antigravity,
    executable: "agy",
    aliases: &[],
    label: "Antigravity",
    // A native executable, never a script an interpreter runs.
    recognizes_script: |_| false,
    // `agy acp` is the ACP server an editor drives; the rest list, configure
    // or update the installation.
    background_command: |arg| {
        matches!(
            arg,
            "acp"
                | "agent"
                | "agents"
                | "changelog"
                | "help"
                | "install"
                | "mcp"
                | "mic-serve"
                | "models"
                | "plugin"
                | "plugins"
                | "remote-control"
                | "update"
        )
    },
    // Conversations are databases, `conversations/<id>.db`, not transcripts.
    session_file_id: |_| None,
    session_root: |_, home| Some(home.join(".gemini/antigravity-cli")),
    #[cfg(feature = "host")]
    session_directory: "conversations",
    // Its terminal title has no lifecycle grammar.
    activity_from_title: |_| None,
    identity_options: &["--conversation"],
    // What Antigravity exports to the commands a conversation runs: the
    // conversation, its language server and the token that server expects.
    session_environment: |name| {
        matches!(
            name,
            "ANTIGRAVITY_AGENT"
                | "ANTIGRAVITY_AGENTAPI_EXE"
                | "ANTIGRAVITY_CONVERSATION_ID"
                | "ANTIGRAVITY_CSRF_TOKEN"
                | "ANTIGRAVITY_LS_ADDRESS"
                | "ANTIGRAVITY_LS_VERSION"
                | "ANTIGRAVITY_PROJECT_ID"
                | "ANTIGRAVITY_SOURCE_METADATA"
                | "ANTIGRAVITY_TRAJECTORY_ID"
        )
    },
    cwd_options: &[],
    value_options: &[
        "--add-dir",
        "--agent",
        "--effort",
        "--log-file",
        "--mode",
        "--model",
        "--project",
        "--settings",
    ],
    switches: &["--dangerously-skip-permissions", "--sandbox"],
    resume_flag: "--conversation",
    resume_precedes_options: false,
    // `--conversation` requires an ID.
    resume_picker: false,
    latest_selectors: &["--continue", "-c"],
    native_chat: false,
    acp: Some(super::Acp {
        mode_option: "--mode",
        effort_option: "--effort",
        // Its own words for them: "Ask before edits and commands", "Approve
        // file edits in the workspace without asking", "Approve edits and
        // safe commands; ask before irreversible actions" and "Plan
        // carefully before executing a task".
        modes: &[
            ("default", "default"),
            ("accept-edits", "acceptEdits"),
            ("auto", "auto"),
            ("plan", "plan"),
        ],
        // `--mode` takes only the other three.
        default_mode: "default",
    }),
    #[cfg(feature = "host")]
    forwards_to_native_child: false,
    #[cfg(feature = "host")]
    create_chat: |_| Err(Provider::Antigravity.native_chat_refusal()),
};
