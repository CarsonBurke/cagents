// Track the configured checkout identity in compiler dep-info, including when
// another worktree's source mtimes predate the shared cached artifact.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

/// The version this build reports about itself: the product version plus the
/// build metadata `build.rs` resolved (`0.1.3+local.3d80e6d`), or the bare
/// product version for a release build. Use this to verify an installed build's
/// identity; ignore its metadata only when ordering published release versions.
pub const VERSION: &str = env!("CAGENTS_VERSION");

mod names;
pub(crate) mod native_chat_capacity;
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
#[cfg(feature = "ui")]
pub(crate) mod change_list;
pub mod channel_work;
pub mod channel_work_api;
#[cfg(feature = "ui")]
pub mod channel_work_ui;
pub mod chat;
pub mod companion;
pub mod control;
pub(crate) mod daemon;
#[cfg(feature = "ui")]
pub(crate) mod daemon_health;
#[cfg(any(feature = "host", all(test, target_os = "linux")))]
pub(crate) mod descriptor_frame;
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
#[cfg(any(feature = "ui", feature = "host"))]
pub(crate) mod file_url;
mod filesystem_recovery_context;
pub mod fs_safety;
#[cfg(feature = "ui")]
pub mod git_patch;
#[cfg(any(feature = "ui", feature = "host"))]
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
pub mod layout_metrics;
#[cfg(feature = "ui")]
pub mod lifecycle;
pub mod linear;
pub mod location;
#[cfg(feature = "ui")]
pub mod lsp;
pub mod model;
pub mod notifications;
#[cfg(feature = "ui")]
pub mod operation_recovery;
pub mod performance;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod persistence;
#[cfg(any(feature = "ui", feature = "host", target_os = "linux"))]
pub mod process;
pub mod project_location;
#[cfg(feature = "ui")]
pub mod pull_request_review;
pub mod recovery_authority;
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
#[cfg(feature = "ui")]
pub mod ui_memory;
pub mod update;
pub mod upgrade;
pub mod work;
#[cfg(feature = "ui")]
pub mod work_ui;
pub mod worktree_branch;
pub mod worktree_cleanup;
pub mod worktree_content;
#[cfg(any(feature = "ui", feature = "host"))]
pub mod worktree_setup;
pub use terminal::ipc as terminal_ipc;
#[cfg(feature = "ui")]
pub mod workspace;
/// The shared half of the sidebar model. Ungated: the CLI mutates it, the
/// daemon owns it and the UI mirrors it, so all three lanes name these types.
pub mod workspace_model;
#[cfg(feature = "ui")]
pub mod workspace_widget;
