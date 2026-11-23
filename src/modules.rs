// Track the configured checkout identity in compiler dep-info, including when
// another worktree's source mtimes predate the shared cached artifact.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

/// The version this executable reports about itself: the product version plus
/// the build metadata it was compiled with (`0.1.3+local.3d80e6d`), or the bare
/// product version for a release build. Use this to verify an installed build's
/// identity; ignore its metadata only when ordering published release versions.
///
/// An executable states it with [`stamp_version!`], which records it before
/// `main`; in one that never does, such as a unit test, this is
/// `<product version>+unstamped`.
pub fn version() -> &'static str {
    build_version::current()
}

#[doc(hidden)]
pub mod build_version;
#[doc(hidden)]
pub mod names;
pub(crate) mod native_chat_capacity;
pub mod search_match;
// The repository fixtures the UI crate's tests share allocate through this;
// that build uses only part of it.
#[cfg(any(test, feature = "test-support"))]
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) mod test_fixture;

pub mod account;
pub mod agent;
pub mod agent_execution;
pub mod automations;
pub mod chat;
pub mod companion;
pub mod control;
#[doc(hidden)]
pub mod daemon;
#[cfg(feature = "ui")]
#[doc(hidden)]
pub mod daemon_health;
#[cfg(any(feature = "host", all(test, target_os = "linux")))]
pub(crate) mod descriptor_frame;
#[cfg(feature = "ui")]
pub mod design;
pub mod device;
pub mod device_service;
pub mod device_trust;
pub mod diagnostics;
#[cfg(feature = "ui")]
pub mod document_link;
#[cfg(any(feature = "ui", feature = "host"))]
#[doc(hidden)]
pub mod file_url;
#[doc(hidden)]
pub mod filesystem_recovery_context;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod fixture_workspace;
mod flock;
pub mod fs_safety;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod git_watch;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod github;
pub mod github_identity;
pub(crate) mod github_rate_limit;
pub mod github_status;
#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "ui")]
pub mod input_trace;
mod install_guard;
pub mod instance;
pub mod layout_metrics;
#[cfg(any(feature = "ui", feature = "host"))]
pub(crate) mod lineage_birth;
pub mod linear;
pub mod location;
pub mod model;
pub mod notifications;
pub mod performance;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod persistence;
#[cfg(any(feature = "ui", feature = "host", target_os = "linux"))]
pub mod process;
pub mod project_location;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod pull_request_signals;
pub mod recovery_authority;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod repository;
#[cfg(feature = "ui")]
pub mod review_cache;
#[doc(hidden)]
pub mod secure_file;
mod selected_file_path;
#[cfg(feature = "ui")]
mod separator;
pub mod settings;
#[cfg(feature = "ui")]
pub mod splitter;
pub mod terminal;
#[cfg(any(test, feature = "test-support"))]
#[doc(hidden)]
pub mod test_timing;
#[cfg(all(
    any(test, feature = "test-support"),
    any(feature = "host", feature = "ui")
))]
#[doc(hidden)]
pub mod test_tracing;
pub(crate) mod transcript_cache;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod transcript_location;
#[cfg(feature = "host")]
pub mod tray;
pub mod update;
pub mod upgrade;
pub mod work;
pub mod worktree_branch;
pub mod worktree_cleanup;
pub mod worktree_content;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod worktree_setup;
pub use terminal::ipc as terminal_ipc;
/// The shared half of the sidebar model. Ungated: the CLI mutates it, the
/// daemon owns it and the UI mirrors it, so all three lanes name these types.
pub mod workspace_model;
