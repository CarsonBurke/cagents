use super::{ActivityState, Provider, ProviderAdapter};
use crate::model::LaunchRecipe;
use std::path::{Path, PathBuf};

pub(super) const VALUE_OPTIONS: &[&str] = &[
    "--model",
    "--mode",
    "--effort",
    "--agent",
    "--add-dir",
    "--project",
    "--settings",
    "--log-file",
];

pub(super) const SWITCHES: &[&str] = &[
    "--sandbox",
    "--new-project",
    "--dangerously-skip-permissions",
];

pub(super) const ADAPTER: ProviderAdapter = ProviderAdapter {
    provider: Provider::Antigravity,
    executable: "agy",
    label: "Antigravity",
    recognizes_script: |path| path.file_name().and_then(|name| name.to_str()) == Some("agy"),
    // Conversations are directories, and nothing in a transcript's own name
    // identifies them.
    session_file_id: |_| None,
    session_root,
    #[cfg(feature = "host")]
    session_directory: "brain",
    activity_from_title,
    identity_options: &["--conversation", "--continue", "-c"],
    cwd_options: &[],
    value_options: VALUE_OPTIONS,
    switches: SWITCHES,
    resume_flag: "--conversation",
    resume_precedes_options: false,
    #[cfg(feature = "host")]
    forwards_to_native_child: false,
    #[cfg(feature = "host")]
    create_chat: crate::chat::protocol::antigravity::create,
};

/// The CLI has no setting that moves its data, which follows `HOME` alone.
fn session_root(_launch: &LaunchRecipe, home: &Path) -> Option<PathBuf> {
    Some(home.join(".gemini/antigravity-cli"))
}

/// The CLI reports its lifecycle over OSC 7501 rather than its title, so a
/// title says nothing about whether it is working.
fn activity_from_title(_title: &str) -> Option<ActivityState> {
    None
}
