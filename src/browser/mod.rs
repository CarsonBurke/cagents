//! GPU-only Servo browser embedding for libcosmic on Wayland.
//!
//! One shared, dedicated engine thread owns a single `servo::Servo` and one
//! `WebView` per browser card. Each webview keeps its own rendering context, so
//! it renders directly into its own child `wl_surface` while the constellation,
//! resource threads and font/image caches behind the engine are shared. The
//! iced widget supplies input, IME, visibility, and logical geometry but does
//! not import or draw browser pixels.

mod fallback;
mod handle;
mod import;
mod input;
mod lifecycle;
mod runtime;
mod types;
mod widget;

pub use fallback::{
    CapabilityProbe, RawWaylandSubsurfaceFactory, WaylandSubsurfaceFactory, WaylandSubsurfaceLease,
};
pub use handle::BrowserHandle;
pub use import::{ImportReport, ImportRequest};
pub use types::{
    BackendSelection, BrowserBackend, BrowserError, BrowserEvent, BrowserInput, BrowserKeyLocation,
    BrowserLifecycle, BrowserModifiers, BrowserMouseButton, BrowserRestoreState, ButtonState,
    DelegateRequest, DelegateRequestId, DevicePoint, EditingAction, ImeInput, InputMethodState,
    PhysicalViewport, SurfacePlacement, TouchState, WheelUnit,
};
pub use widget::BrowserWidget;
