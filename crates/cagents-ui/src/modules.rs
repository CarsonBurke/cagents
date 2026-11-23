// Track the configured checkout identity in compiler dep-info, including when
// another worktree's source mtimes predate the shared cached artifact.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

// The application modules name the shared library's modules from this crate's
// root, as they did when both were one crate.
use cagents::*;

pub mod app;
#[cfg(feature = "integrated-browser")]
pub mod browser;

// The widgets and views live in cagents-ui-kit, so that editing the
// application does not recompile them. They keep their paths here, as
// `crate::sidebar` or `cagents_ui::editor`, from when they were this crate's.
#[cfg(feature = "latency-harness")]
pub use cagents_ui_kit::latency_fixture;
pub use cagents_ui_kit::{
    automation_ui, capability, dialog_ui, dictation, diff_view, document_render, editor,
    editor_search, external_editor, file_drop, file_icon, file_manager, inspector, interaction,
    lifecycle, lsp, operation_recovery, pull_request_review, question, reorder, repository_widget,
    session, settings_widgets, sidebar, startup_window, tab_history, tab_strip, ui_controls,
    ui_memory, window_system, work_ui, workspace, workspace_widget, worktrees_ui,
};
pub(crate) use cagents_ui_kit::{
    desktop_cursor_blink, editor_assistance, editor_history, fingerprint, revision,
};
