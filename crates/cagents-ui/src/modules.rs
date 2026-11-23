// Track the configured checkout identity in compiler dep-info, including when
// another worktree's source mtimes predate the shared cached artifact.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

// The application modules name the shared library's modules from this crate's
// root, as they did when both were one crate.
use cagents::*;

pub mod app;
pub mod automation_ui;
#[cfg(feature = "integrated-browser")]
pub mod browser;
pub mod capability;
pub(crate) mod desktop_cursor_blink;
pub mod dialog_ui;
pub mod dictation;
pub mod diff_view;
pub mod document_render;
pub mod editor;
pub(crate) mod editor_assistance;
pub(crate) mod editor_highlight;
pub(crate) mod editor_history;
pub mod editor_search;
pub mod external_editor;
pub mod file_drop;
pub mod file_icon;
pub mod file_manager;
pub(crate) mod fingerprint;
pub mod inspector;
pub mod interaction;
#[cfg(feature = "latency-harness")]
pub mod latency_fixture;
pub mod lifecycle;
pub mod lsp;
pub mod operation_recovery;
pub mod pull_request_review;
pub mod reorder;
pub mod repository_widget;
pub(crate) mod revision;
pub mod session;
pub mod settings_widgets;
pub mod sidebar;
pub mod startup_window;
mod sticky_tree;
pub mod tab_history;
pub mod tab_strip;
pub mod ui_controls;
pub mod ui_memory;
pub mod work_ui;
pub mod workspace;
pub mod workspace_widget;
pub mod worktrees_ui;
