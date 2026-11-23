//! The UI's widgets and views below the application: controls, the sidebar,
//! the editor, repository and review views, and the session runtime. The
//! application (`cagents-ui`) composes them into windows; the standalone
//! editor (`ceditor`) uses them without it.

#![cfg_attr(
    test,
    allow(
        clippy::cloned_ref_to_slice_refs,
        reason = "an assertion's expected `[value.clone()]` reads as the value it expects"
    )
)]

// Track the configured checkout identity in compiler dep-info, including when
// another worktree's source mtimes predate the shared cached artifact.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

// These modules name the shared library's modules from this crate's root, as
// they did when they and the application were one crate.
use cagents::*;

pub mod automation_ui;
pub mod capability;
#[doc(hidden)]
pub mod desktop_cursor_blink;
pub mod dialog_ui;
pub mod dictation;
pub mod diff_view;
pub mod document_render;
pub mod editor;
#[doc(hidden)]
pub mod editor_assistance;
pub(crate) mod editor_highlight;
#[doc(hidden)]
pub mod editor_history;
pub mod editor_search;
pub mod external_editor;
pub mod file_drop;
pub mod file_icon;
pub mod file_manager;
#[doc(hidden)]
pub mod fingerprint;
pub mod inspector;
pub mod interaction;
#[cfg(feature = "latency-harness")]
pub mod latency_fixture;
pub mod lifecycle;
pub mod lsp;
pub mod operation_recovery;
pub mod pull_request_review;
pub mod question;
pub mod reorder;
pub mod repository_widget;
#[doc(hidden)]
pub mod revision;
pub mod session;
pub mod settings_widgets;
pub mod sidebar;
pub mod startup_window;
mod sticky_tree;
pub mod tab_history;
pub mod tab_strip;
pub mod ui_controls;
pub mod ui_memory;
pub mod window_system;
pub mod work_ui;
pub mod workspace;
pub mod workspace_widget;
pub mod worktrees_ui;

cagents::test_binary_setup!();
