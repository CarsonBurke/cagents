use std::{ops::Range, path::PathBuf};

use serde::{Deserialize, Serialize};
use url::Url;

use crate::location::SanitizedLocation;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BrowserLifecycle {
    Visible,
    OffscreenHot,
    Hibernating,
    Hibernated,
    Restoring,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct PhysicalViewport {
    pub width: u32,
    pub height: u32,
}

impl PhysicalViewport {
    pub const MIN: Self = Self {
        width: 1,
        height: 1,
    };

    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width: width.max(1),
            height: height.max(1),
        }
    }

    pub fn from_logical(width: f32, height: f32, scale_factor: f32) -> Self {
        let scale = if scale_factor.is_finite() {
            scale_factor.max(0.01)
        } else {
            1.0
        };
        Self::new(
            (width.max(0.0) * scale).round() as u32,
            (height.max(0.0) * scale).round() as u32,
        )
    }
}

impl Default for PhysicalViewport {
    fn default() -> Self {
        Self::MIN
    }
}

/// Native placement of a browser child surface: where its presented region sits
/// inside the parent surface, and which part of Servo's buffer is presented
/// there.
///
/// The widget derives this from the intersection of its layout rectangle with
/// iced's clip rectangle. Clipping is expressed as a `wp_viewport` source
/// rectangle rather than as visibility so that a partially scrolled-out card is
/// cropped instead of unmapped, and so Servo keeps laying the page out at the
/// full widget size while the card is panned.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SurfacePlacement {
    /// Logical position of the presented region inside the parent surface.
    pub logical_x: i32,
    pub logical_y: i32,
    /// Logical size of the presented region: the `wp_viewport` destination.
    pub logical_width: i32,
    pub logical_height: i32,
    /// Physical origin of the presented region inside Servo's buffer.
    pub source_x: u32,
    pub source_y: u32,
    /// Physical size of the presented region inside Servo's buffer.
    pub source_width: u32,
    pub source_height: u32,
}

impl SurfacePlacement {
    /// Presents the whole buffer at the parent surface origin.
    ///
    /// A unit buffer scale plus a viewport destination preserves the exact
    /// physical Servo buffer while supporting fractional logical scaling, so the
    /// destination is the buffer divided by the scale factor.
    pub fn unclipped(buffer: PhysicalViewport, scale_factor: f32) -> Self {
        let (logical_width, logical_height) =
            logical_extent(buffer.width, buffer.height, scale_factor);
        Self {
            logical_x: 0,
            logical_y: 0,
            logical_width,
            logical_height,
            source_x: 0,
            source_y: 0,
            source_width: buffer.width,
            source_height: buffer.height,
        }
    }
}

/// Converts a physical extent to the logical extent a `wp_viewport` destination
/// needs. Wayland rejects a non-positive destination, and the protocol caps
/// logical geometry at `i32`, so both bounds are enforced here rather than
/// surfaced as an error: every caller has already clamped its scale factor.
fn logical_extent(width: u32, height: u32, scale_factor: f32) -> (i32, i32) {
    let scale = if scale_factor.is_finite() {
        f64::from(scale_factor.max(0.01))
    } else {
        1.0
    };
    let logical = |extent: u32| {
        (f64::from(extent.max(1)) / scale)
            .round()
            .clamp(1.0, f64::from(i32::MAX)) as i32
    };
    (logical(width), logical(height))
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BrowserRestoreState {
    pub url: Url,
    pub history: Vec<Url>,
    pub history_index: usize,
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub scale_factor: f32,
    pub viewport: PhysicalViewport,
}

impl BrowserRestoreState {
    pub fn normalized(mut self) -> Self {
        if self.history.is_empty() {
            self.history.push(self.url.clone());
            self.history_index = 0;
        } else {
            self.history_index = self.history_index.min(self.history.len() - 1);
            self.url = self.history[self.history_index].clone();
        }
        self.scale_factor = if self.scale_factor.is_finite() {
            self.scale_factor.max(0.01)
        } else {
            1.0
        };
        self.viewport = PhysicalViewport::new(self.viewport.width, self.viewport.height);
        if !self.scroll_x.is_finite() {
            self.scroll_x = 0.0;
        }
        if !self.scroll_y.is_finite() {
            self.scroll_y = 0.0;
        }
        self
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserBackend {
    WaylandSubsurface,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendSelection {
    pub selected: BrowserBackend,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct DevicePoint {
    pub x: f64,
    pub y: f64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ButtonState {
    Pressed,
    Released,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserMouseButton {
    Left,
    Middle,
    Right,
    Back,
    Forward,
    Other(u16),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WheelUnit {
    Pixels,
    Lines,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct BrowserModifiers {
    pub alt: bool,
    pub control: bool,
    pub shift: bool,
    pub meta: bool,
    pub caps_lock: bool,
    pub num_lock: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BrowserKeyLocation {
    Standard,
    Left,
    Right,
    Numpad,
}

#[derive(Debug, Clone, PartialEq)]
pub enum BrowserInput {
    MouseMove {
        point: DevicePoint,
    },
    MouseLeave,
    MouseButton {
        state: ButtonState,
        button: BrowserMouseButton,
        point: DevicePoint,
    },
    Wheel {
        x: f64,
        y: f64,
        unit: WheelUnit,
        point: DevicePoint,
    },
    Touch {
        state: TouchState,
        id: i32,
        point: DevicePoint,
    },
    Key {
        state: ButtonState,
        key: String,
        code: String,
        location: BrowserKeyLocation,
        modifiers: BrowserModifiers,
        repeat: bool,
        composing: bool,
    },
    Ime(ImeInput),
    Editing(EditingAction),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TouchState {
    Down,
    Move,
    Up,
    Cancel,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImeInput {
    Opened,
    Preedit {
        text: String,
        selection: Option<Range<usize>>,
    },
    Commit(String),
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditingAction {
    Copy,
    Cut,
    Paste,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DelegateRequestId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DelegateRequest {
    ClipboardRead {
        id: DelegateRequestId,
    },
    ClipboardWrite {
        text: String,
    },
    ClipboardClear,
    NewWindow {
        url: Option<Url>,
    },
    FilePicker {
        id: DelegateRequestId,
        multiple: bool,
        current_paths: Vec<PathBuf>,
        filters: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InputMethodState {
    pub id: DelegateRequestId,
    pub text: String,
    pub insertion_point: Option<u32>,
    pub device_rect: (i32, i32, i32, i32),
    pub multiline: bool,
    pub allow_virtual_keyboard: bool,
    pub secure: bool,
}

#[derive(Debug, Clone)]
pub enum BrowserEvent {
    Ready {
        backend: BackendSelection,
    },
    UrlChanged(Url),
    HistoryChanged {
        entries: Vec<Url>,
        current: usize,
    },
    TitleChanged(Option<String>),
    LoadStateChanged(String),
    FocusChanged(bool),
    InputHandled {
        sequence: u64,
        consumed: bool,
    },
    InputMethod(Option<InputMethodState>),
    Delegate(DelegateRequest),
    LifecycleChanged(BrowserLifecycle),
    ClosedByContent,
    Error(BrowserError),
    Crashed {
        reason: String,
        backtrace: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BrowserError {
    #[error("{0}")]
    Capability(crate::capability::BrowserUnavailable),
    #[error("browser command queue is full")]
    CommandQueueFull,
    #[error("browser thread has stopped")]
    ThreadStopped,
    #[error("invalid URL: {0}")]
    InvalidUrl(SanitizedLocation),
    #[error("GPU interop unavailable: {0}")]
    Interop(String),
    #[error("Wayland subsurface fallback unavailable: {0}")]
    Wayland(String),
    #[error("Servo failed: {0}")]
    Servo(String),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_url_errors_can_only_format_a_sanitized_location() {
        let url =
            Url::parse("https://alice:password@example.test/private?oauth=secret#access-token")
                .unwrap();
        let error = BrowserError::InvalidUrl(SanitizedLocation::from(&url)).to_string();

        assert_eq!(error, "invalid URL: https://example.test/private");
        assert!(!error.contains("alice"));
        assert!(!error.contains("password"));
        assert!(!error.contains("oauth"));
        assert!(!error.contains("secret"));
        assert!(!error.contains("access-token"));
    }

    #[test]
    fn fractional_scale_maps_physical_buffer_to_logical_destination() {
        let placement = SurfacePlacement::unclipped(PhysicalViewport::new(150, 75), 1.5);
        assert_eq!(
            (placement.logical_width, placement.logical_height),
            (100, 50)
        );
        assert_eq!((placement.source_width, placement.source_height), (150, 75));
        assert_eq!((placement.source_x, placement.source_y), (0, 0));
    }

    #[test]
    fn a_degenerate_scale_factor_still_yields_a_committable_destination() {
        for scale in [0.0, -1.0, f32::NAN, f32::INFINITY] {
            let placement = SurfacePlacement::unclipped(PhysicalViewport::MIN, scale);
            assert!(placement.logical_width >= 1 && placement.logical_height >= 1);
        }
    }
}
