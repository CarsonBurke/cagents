#[cfg(feature = "host")]
pub(crate) mod holder;
mod hosted;
pub mod ipc;
mod search;
mod snapshot;
pub use search::SearchMatch;
#[cfg(feature = "ui")]
mod surface;
mod theme;
mod types;
pub use theme::{TerminalPalette, TerminalTheme, ThemeColor};
#[cfg(feature = "ui")]
mod widget;
#[cfg(feature = "ui")]
pub(crate) use widget::monospace_font;
#[cfg(feature = "host")]
mod worker;

/// The holder may take this long to report a newly spawned process. Keep the
/// UI's attachment wait longer than this bound.
#[cfg(any(feature = "ui", feature = "host"))]
pub(crate) const SPAWN_START_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
pub use ipc::TerminalSpawnOptions;

pub use hosted::{
    HostedTerminalBinding, HostedTerminalHandle, HostedTerminalStartError, ensure_daemon_ready, ensure_daemon_ready_for,
};
pub use snapshot::{
    CellSnapshot, CellStyle, CellText, CursorShape, CursorSnapshot, RenderSnapshot, Rgb,
    RowSnapshot, ScrollSnapshot, Underline,
};
#[cfg(feature = "ui")]
pub use surface::TerminalSurface;
pub use types::{
    KeyAction, KeyInput, KeyModifiers, MouseAction, MouseButton, MouseInput, PasteOrigin,
    TerminalConfig, TerminalEvent, TerminalKey, TerminalSendError, TerminalSize,
    TerminalStartError, TerminalWake,
};
#[cfg(feature = "ui")]
pub(crate) use widget::ReviewInputBarrier;
#[cfg(feature = "ui")]
pub use widget::{CellMetrics, TerminalWidget};
#[cfg(feature = "host")]
pub use worker::TerminalHandle;
#[cfg(feature = "host")]
pub(crate) use worker::UpgradeSnapshot;
#[cfg(all(test, feature = "host"))]
pub(crate) use worker::flush_archived_notices;
#[cfg(feature = "host")]
pub(crate) use worker::holder::inspect as inspect_holder;
#[cfg(feature = "host")]
pub(crate) use worker::holder::inspect_until as inspect_holder_until;
