// Track the configured checkout identity in compiler dep-info, including when
// another worktree's source mtimes predate the shared cached artifact.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

/// The version this build reports about itself: the product version plus the
/// build metadata `build.rs` resolved (`0.1.3+local.3d80e6d`), or the bare
/// product version for a release build. Use this to verify an installed build's
/// identity; ignore its metadata only when ordering published release versions.
pub const VERSION: &str = env!("CAGENTS_VERSION");

mod names;
pub mod search_match;

pub mod account;
pub mod agent;
pub mod agent_execution;
#[cfg(feature = "ui")]
pub mod app;
#[cfg(feature = "ui")]
pub mod automation_ui;
pub mod automations;
#[cfg(feature = "integrated-browser")]
pub mod browser;
#[cfg(feature = "ui")]
pub mod capability;
pub mod chat;
pub mod companion;
pub mod control;
pub(crate) mod daemon;
#[cfg(feature = "ui")]
pub(crate) mod daemon_health;
#[cfg(feature = "ui")]
pub mod design;
pub mod device;
pub mod device_service;
pub mod diagnostics;
#[cfg(feature = "ui")]
pub mod diff_interaction;
#[cfg(feature = "ui")]
pub mod diff_patch;
#[cfg(feature = "ui")]
pub mod diff_view;
#[cfg(feature = "ui")]
pub mod git_patch;
#[cfg(feature = "ui")]
pub(crate) mod change_list;
#[cfg(feature = "ui")]
pub mod document_link;
#[cfg(feature = "ui")]
pub mod document_render;
#[cfg(feature = "ui")]
pub mod editor;
#[cfg(feature = "ui")]
pub(crate) mod editor_history;
#[cfg(feature = "ui")]
pub mod editor_search;
#[cfg(feature = "ui")]
pub mod external_editor;
#[cfg(feature = "ui")]
pub mod file_drop;
#[cfg(feature = "ui")]
pub mod file_icon;
#[cfg(feature = "ui")]
pub mod file_manager;
pub mod fs_safety;
#[cfg(feature = "ui")]
pub(crate) mod git_watch;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod github;
pub mod github_identity;
#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "ui")]
pub mod input_trace;
#[cfg(feature = "ui")]
pub mod inspector;
mod install_guard;
pub mod instance;
#[cfg(feature = "ui")]
pub mod interaction;
#[cfg(feature = "latency-harness")]
pub mod latency_fixture;
#[cfg(feature = "ui")]
pub mod lifecycle;
pub mod linear;
pub mod location;
#[cfg(feature = "ui")]
pub mod lsp;
pub mod layout_metrics;
pub mod model;
pub mod notifications;
pub mod performance;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod persistence;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod process;
pub mod project_location;
#[cfg(feature = "ui")]
pub mod project_search;
#[cfg(feature = "ui")]
pub mod pull_request_review;
#[cfg(feature = "ui")]
pub mod reorder;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod repository;
#[cfg(feature = "ui")]
pub mod repository_widget;
mod secure_file;
mod selected_file_path;
#[cfg(feature = "ui")]
mod separator;
#[cfg(feature = "ui")]
pub mod session;
pub mod settings;
#[cfg(feature = "ui")]
pub mod settings_widgets;
#[cfg(feature = "ui")]
pub mod sidebar;
#[cfg(feature = "ui")]
pub mod splitter;
#[cfg(feature = "ui")]
mod sticky_tree;
#[cfg(feature = "ui")]
pub mod tab_history;
#[cfg(feature = "ui")]
pub mod tab_strip;
pub mod terminal;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod transcript_location;
#[cfg(feature = "host")]
pub mod tray;
#[cfg(feature = "ui")]
pub mod ui_controls;
pub mod update;
pub mod upgrade;
pub mod worktree_cleanup;
pub use terminal::ipc as terminal_ipc;
#[cfg(feature = "ui")]
pub mod workspace;
/// The shared half of the sidebar model. Ungated: the CLI mutates it, the
/// daemon owns it and the UI mirrors it, so all three lanes name these types.
pub mod workspace_model;
#[cfg(feature = "ui")]
pub mod workspace_widget;
