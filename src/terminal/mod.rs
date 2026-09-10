mod hosted;
pub mod ipc;
mod snapshot;
#[cfg(feature = "ui")]
mod surface;
mod types;
#[cfg(feature = "ui")]
mod widget;
#[cfg(feature = "host")]
mod worker;
pub use ipc::TerminalSpawnOptions;

pub use hosted::{HostedTerminalHandle, HostedTerminalStartError, ensure_daemon_ready};
pub use snapshot::{
    CellSnapshot, CellStyle, CellText, CursorShape, CursorSnapshot, RenderSnapshot, Rgb,
    RowSnapshot, ScrollSnapshot, Underline,
};
#[cfg(feature = "ui")]
pub use surface::TerminalSurface;
pub use types::{
    KeyAction, KeyInput, KeyModifiers, MouseAction, MouseButton, MouseInput, TerminalConfig,
    TerminalEvent, TerminalKey, TerminalSendError, TerminalSize, TerminalStartError, TerminalTheme,
    TerminalWake,
};
#[cfg(feature = "ui")]
pub use widget::{CellMetrics, TerminalWidget};
#[cfg(feature = "host")]
pub use worker::TerminalHandle;
#[cfg(feature = "host")]
pub(crate) use worker::UpgradeSnapshot;
