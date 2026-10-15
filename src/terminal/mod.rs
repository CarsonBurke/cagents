mod daemon_start;
#[cfg(feature = "host")]
pub(crate) mod holder;
#[cfg(any(feature = "ui", feature = "host"))]
mod hosted;
pub mod ipc;
#[cfg(feature = "host")]
pub(crate) mod reactor;
mod search;
mod snapshot;
#[cfg(any(feature = "ui", feature = "host"))]
pub(crate) mod transport;
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

pub use daemon_start::{HostedTerminalStartError, ensure_daemon_ready, ensure_daemon_ready_for};
#[cfg(any(feature = "ui", feature = "host"))]
pub use hosted::{HostedKeyReceipt, HostedTerminalBinding, HostedTerminalHandle};
pub(crate) fn local_host_name() -> Option<&'static str> {
    static NAME: std::sync::OnceLock<Option<String>> = std::sync::OnceLock::new();
    NAME.get_or_init(|| {
        let mut buffer = [0u8; 256];
        // SAFETY: gethostname receives a valid writable buffer and its size.
        if unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } != 0 {
            return None;
        }
        // An unterminated truncated hostname must not match a different host.
        std::ffi::CStr::from_bytes_until_nul(&buffer)
            .ok()?
            .to_str()
            .ok()
            .map(str::to_owned)
    })
    .as_deref()
}

pub use snapshot::{
    CellSnapshot, CellStyle, CellText, CursorShape, CursorSnapshot, HistoryPosition,
    RenderSnapshot, Rgb, RowSnapshot, ScrollSnapshot, Underline,
};
#[cfg(feature = "ui")]
pub use surface::TerminalSurface;
pub use types::{
    KeyAction, KeyInput, KeyModifiers, MouseAction, MouseButton, MouseInput, PasteOrigin,
    TerminalConfig, TerminalEvent, TerminalKey, TerminalSendError, TerminalSize,
    TerminalStartError, TerminalWake, default_scroll_to_bottom_on_input,
};
#[cfg(feature = "ui")]
pub(crate) use widget::ReviewInputBarrier;
#[cfg(all(test, feature = "ui"))]
pub(crate) use widget::accessibility_tests::Peer as TerminalTestPeer;
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
