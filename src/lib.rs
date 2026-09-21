/// The version this build reports about itself: the product version plus the
/// build metadata `build.rs` resolved (`0.1.3+local.3d80e6d`), or the bare
/// product version for a release build. Use `CARGO_PKG_VERSION` only where a
/// release is being chosen or compared; use this wherever a build identifies
/// itself to a person or a log.
pub const VERSION: &str = env!("CAGENTS_VERSION");

pub mod agent;
#[cfg(feature = "ui")]
pub mod app;
#[cfg(feature = "integrated-browser")]
pub mod browser;
#[cfg(feature = "ui")]
pub mod capability;
pub mod chat;
pub mod control;
pub(crate) mod daemon;
#[cfg(feature = "ui")]
pub mod design;
#[cfg(feature = "ui")]
pub mod diff_view;
#[cfg(feature = "ui")]
pub mod document_link;
#[cfg(feature = "ui")]
pub mod document_render;
#[cfg(feature = "ui")]
pub mod editor;
#[cfg(feature = "ui")]
pub mod external_editor;
#[cfg(feature = "ui")]
pub mod file_icon;
pub mod fs_safety;
#[cfg(feature = "ui")]
pub mod github;
#[cfg(feature = "host")]
pub mod host;
#[cfg(feature = "ui")]
pub mod inspector;
pub mod instance;
#[cfg(feature = "ui")]
pub mod interaction;
#[cfg(feature = "latency-harness")]
pub mod latency_fixture;
#[cfg(feature = "ui")]
pub mod lifecycle;
pub mod location;
#[cfg(feature = "ui")]
pub mod lsp;
pub mod model;
pub mod performance;
#[cfg(feature = "ui")]
pub mod persistence;
#[cfg(feature = "ui")]
pub mod repository;
#[cfg(feature = "ui")]
pub mod repository_widget;
mod secure_file;
#[cfg(feature = "ui")]
mod separator;
#[cfg(feature = "ui")]
pub mod session;
#[cfg(feature = "ui")]
pub mod settings;
#[cfg(feature = "ui")]
pub mod sidebar;
#[cfg(feature = "ui")]
pub mod splitter;
#[cfg(feature = "ui")]
pub mod tab_strip;
pub mod terminal;
#[cfg(feature = "host")]
pub mod tray;
#[cfg(feature = "ui")]
pub mod ui_controls;
pub mod update;
pub mod upgrade;
pub use terminal::ipc as terminal_ipc;
#[cfg(feature = "ui")]
pub mod workspace;
/// The shared half of the sidebar model. Ungated: the CLI mutates it, the
/// daemon owns it and the UI mirrors it, so all three lanes name these types.
pub mod workspace_model;
#[cfg(feature = "ui")]
pub mod workspace_widget;
