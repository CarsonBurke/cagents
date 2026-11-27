//! Antigravity (`agy`), Google's terminal coding agent. Its native chat is
//! its Agent Client Protocol server, `agy acp`.
use super::{Provider, ProviderAdapter};

/// The variable that has Antigravity keep its sign-in in a file of its home,
/// `~/.gemini/antigravity-cli/antigravity-oauth-token`, rather than in the
/// desktop keyring. With it set, `agy` never consults the keyring, so a
/// keyring left locked by a headless boot cannot sign it out. See
/// [`crate::account::credential_storage`].
pub(crate) const FILE_TOKEN_STORAGE: &str = "AGY_CLI_FORCE_FILE_TOKEN_STORAGE";
pub(crate) const FILE_TOKEN_STORAGE_ON: &str = "1";

/// The earliest Antigravity known to honour [`FILE_TOKEN_STORAGE`]: 1.5.18
/// does, and 1.5.15 has no such variable. 1.5.16 and 1.5.17 were not
/// checked, so they are treated as not honouring it.
#[cfg(any(feature = "ui", feature = "host", test))]
const FILE_TOKEN_STORAGE_SINCE: [u64; 3] = [1, 5, 18];

/// The `major.minor.patch` of an Antigravity that, by the `version` its ACP
/// server's `agentInfo` names, still keeps its sign-in in the desktop keyring
/// whatever cagents asks. An unknown or unreadable version is taken for one
/// that keeps it in a file: that is every version cagents supports from here.
/// Only these numbers are ever shown, never what the agent sent.
#[cfg(any(feature = "ui", feature = "host", test))]
fn keyring_version(version: Option<&str>) -> Option<[u64; 3]> {
    let mut parts = version?
        .trim()
        .trim_start_matches('v')
        .split(|c: char| !c.is_ascii_digit())
        .map(str::parse::<u64>);
    let mut parsed = [0; 3];
    for part in &mut parsed {
        *part = parts.next()?.ok()?;
    }
    (parsed < FILE_TOKEN_STORAGE_SINCE).then_some(parsed)
}

/// Why an Antigravity is signed out for the agents cagents starts, and what
/// to do about it, given the version its ACP server reported.
#[cfg(any(feature = "ui", feature = "host", test))]
pub(crate) fn sign_in_advice(version: Option<&str>) -> String {
    match keyring_advice(version) {
        Some(advice) => advice,
        None => {
            format!("{FILE_SIGN_IN} Sign in from cagents, even if `agy` is signed in elsewhere.")
        }
    }
}

/// What a conversation that Antigravity refused for want of a sign-in tells
/// the reader to do, given the version its ACP server reported.
#[cfg(any(feature = "host", test))]
pub(crate) fn chat_sign_in_advice(version: Option<&str>) -> String {
    keyring_advice(version).unwrap_or_else(|| {
        format!("sign in to Antigravity from cagents, then try again. {FILE_SIGN_IN}")
    })
}

/// Where the agents cagents starts read Antigravity's sign-in from.
#[cfg(any(feature = "ui", feature = "host", test))]
const FILE_SIGN_IN: &str =
    "Agents in cagents read this sign-in from a file, not the desktop keyring.";

/// For an Antigravity too old to keep its sign-in in a file, why agents
/// can't read it, and how to let them.
#[cfg(any(feature = "ui", feature = "host", test))]
fn keyring_advice(version: Option<&str>) -> Option<String> {
    keyring_version(version).map(|[major, minor, patch]| {
        format!(
            "Antigravity {major}.{minor}.{patch} {KEYRING_SIGN_IN}, which agents can't read while it's locked, as it stays until someone logs in to the desktop. Update it by running agy update, then sign in from cagents."
        )
    })
}

/// How [`sign_in_advice`] says the sign-in is held where a locked keyring
/// keeps it from agents, which a failure's summary recognizes it by.
pub(crate) const KEYRING_SIGN_IN: &str = "keeps its sign-in in the desktop keyring";

/// Antigravity's permission modes beside the cagents mode each means. In its
/// own words: "Ask before edits and commands", "Approve file edits in the
/// workspace without asking", "Approve edits and safe commands; ask before
/// irreversible actions" and "Plan carefully before executing a task".
pub(crate) const MODES: &[(&str, &str)] = &[
    ("default", "default"),
    ("accept-edits", "acceptEdits"),
    ("auto", "auto"),
    ("plan", "plan"),
];

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
    native_chat: true,
    permission_modes: &["default", "acceptEdits", "auto", "plan"],
    permission_mode_aliases: &[],
    permission_sandboxes: &[],
    approves_through_questions: false,
    acp: Some(super::Acp {
        mode_option: "--mode",
        effort_option: "--effort",
        modes: MODES,
        // `--mode` takes only the other three.
        default_mode: "default",
    }),
    #[cfg(feature = "host")]
    forwards_to_native_child: false,
    #[cfg(feature = "host")]
    create_chat: |launch| {
        crate::chat::protocol::acp::create(
            &crate::chat::protocol::acp::antigravity::PROFILE,
            launch,
        )
    },
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_an_antigravity_from_before_file_storage_keeps_its_sign_in_in_the_keyring() {
        for version in ["1.5.15", "1.5.17", "v1.4.99", "0.9.0"] {
            assert!(keyring_version(Some(version)).is_some(), "{version}");
        }
        for version in ["1.5.18", "1.5.18-rc1", "1.6.0", "2.0.0", " 1.10.1 "] {
            assert!(!keyring_version(Some(version)).is_some(), "{version}");
        }
        // What cannot be read is taken for a current Antigravity.
        for version in [None, Some(""), Some("dev"), Some("1.5")] {
            assert!(!keyring_version(version).is_some(), "{version:?}");
        }
    }

    #[test]
    fn the_advice_names_the_keyring_only_where_it_still_holds_the_sign_in() {
        let old = sign_in_advice(Some("1.5.15"));
        assert!(old.contains(KEYRING_SIGN_IN), "{old}");
        assert!(old.contains("Antigravity 1.5.15 "), "{old}");
        assert!(old.contains("agy update"), "{old}");
        assert_eq!(chat_sign_in_advice(Some("1.5.15")), old);
        // Only the numbers it parsed are shown, whatever else it sent.
        let sent = sign_in_advice(Some("v1.5.15\u{1b}[31m-build with a very long tail"));
        assert!(sent.contains("Antigravity 1.5.15 keeps"), "{sent}");
        assert!(!sent.contains('\u{1b}') && !sent.contains("tail"), "{sent}");
        for version in [Some("1.5.18"), None] {
            let current = sign_in_advice(version);
            assert!(!current.contains(KEYRING_SIGN_IN), "{current}");
            assert!(current.contains("Sign in from cagents"), "{current}");
            let chat = chat_sign_in_advice(version);
            assert!(chat.starts_with("sign in to Antigravity from cagents, then try again."));
            assert!(chat.contains("not the desktop keyring"), "{chat}");
        }
    }
}
