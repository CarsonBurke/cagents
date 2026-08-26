mod hosted;
pub mod ipc;
mod snapshot;
mod surface;
mod widget;
mod worker;
pub use ipc::TerminalSpawnOptions;

pub use hosted::{HostedTerminalHandle, HostedTerminalStartError, ensure_daemon_ready};
pub use snapshot::{
    CellSnapshot, CellStyle, CellText, CursorShape, CursorSnapshot, RenderSnapshot, Rgb,
    RowSnapshot, Underline,
};
pub use surface::TerminalSurface;
pub use widget::{CellMetrics, TerminalWidget};
pub use worker::{
    KeyAction, KeyInput, KeyModifiers, MouseAction, MouseButton, MouseInput, TerminalConfig,
    TerminalEvent, TerminalHandle, TerminalKey, TerminalSendError, TerminalSize,
    TerminalStartError, TerminalWake,
};
