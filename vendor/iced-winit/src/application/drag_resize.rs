use cursor_icon::CursorIcon;
use winit::window::ResizeDirection;

#[cfg(any(
    all(
        unix,
        not(target_vendor = "apple"),
        not(target_os = "android"),
        not(target_os = "emscripten"),
    ),
    target_os = "windows",
))]
const DRAG_RESIZE_SUPPORTED: bool = true;

#[cfg(not(any(
    all(
        unix,
        not(target_vendor = "apple"),
        not(target_os = "android"),
        not(target_os = "emscripten"),
    ),
    target_os = "windows",
)))]
const DRAG_RESIZE_SUPPORTED: bool = false;

/// If supported by winit, returns a closure that implements cursor resize support.
///
/// `resize_border` is in logical pixels. It is scaled by the window's current
/// scale factor on every event, so the border follows the window between
/// outputs of different scales.
pub fn event_func(
    window: &dyn winit::window::Window,
    resize_border: f64,
) -> Option<
    Box<
        dyn FnMut(
            &dyn winit::window::Window,
            &winit::event::WindowEvent,
        ) -> bool,
    >,
> {
    if DRAG_RESIZE_SUPPORTED {
        // Keep track of cursor when it is within a resizeable border.
        let mut cursor_prev_resize_direction = None;

        Some(Box::new(
            move |window: &dyn winit::window::Window,
                  window_event: &winit::event::WindowEvent|
                  -> bool {
                let border_size = resize_border * window.scale_factor();

                // Keep track of border resize state and set cursor icon when in range
                match window_event {
                    winit::event::WindowEvent::PointerMoved {
                        position,
                        ..
                    } => {
                        let location =
                            resize_direction(window, *position, border_size);
                        if location != cursor_prev_resize_direction {
                            window.set_cursor(
                                resize_direction_cursor_icon(location).into(),
                            );
                            cursor_prev_resize_direction = location;
                            return true;
                        }
                    }
                    winit::event::WindowEvent::PointerButton {
                        state: winit::event::ElementState::Pressed,
                        button:
                            winit::event::ButtonSource::Mouse(
                                winit::event::MouseButton::Left,
                            )
                            | winit::event::ButtonSource::Touch {
                                finger_id: _, ..
                            },
                        primary: true,
                        position,
                        ..
                    } => {
                        // The press is located by its own position, not the
                        // last motion: the pointer can re-enter the window
                        // without moving, and the window state can change
                        // under a still pointer.
                        if let Some(direction) =
                            resize_direction(window, *position, border_size)
                        {
                            let _res = window.drag_resize_window(direction);
                            return true;
                        }
                    }
                    _ => (),
                }

                false
            },
        ))
    } else {
        None
    }
}

/// The window edges the application may resize from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Edges {
    left: bool,
    right: bool,
    top: bool,
    bottom: bool,
}

impl Edges {
    const ALL: Self = Self {
        left: true,
        right: true,
        top: true,
        bottom: true,
    };
}

/// The resize direction under `position`, if that point lies on an edge the
/// application may resize from.
fn resize_direction(
    window: &dyn winit::window::Window,
    position: winit::dpi::PhysicalPosition<f64>,
    border_size: f64,
) -> Option<ResizeDirection> {
    cursor_resize_direction(
        window.surface_size(),
        position,
        border_size,
        resizable_edges(window)?,
    )
}

/// A maximized or fullscreen window has no resizable edge. On Wayland, an
/// edge the compositor reports as tiled borders another part of its layout:
/// xdg-shell asks clients not to offer resizing there, and doing so would take
/// presses from content that reaches that edge, such as a scrollbar.
fn resizable_edges(window: &dyn winit::window::Window) -> Option<Edges> {
    if window.is_decorated()
        || window.is_maximized()
        || window.fullscreen().is_some()
    {
        return None;
    }

    #[cfg(wayland_platform)]
    {
        use winit::platform::wayland::WindowExtWayland;

        if let Some(state) = window.window_state() {
            return wayland_edges(state);
        }
    }

    Some(Edges::ALL)
}

#[cfg(wayland_platform)]
fn wayland_edges(
    state: cctk::sctk::reexports::csd_frame::WindowState,
) -> Option<Edges> {
    use cctk::sctk::reexports::csd_frame::WindowState;

    if state.intersects(WindowState::MAXIMIZED | WindowState::FULLSCREEN) {
        return None;
    }
    let edges = Edges {
        left: !state.contains(WindowState::TILED_LEFT),
        right: !state.contains(WindowState::TILED_RIGHT),
        top: !state.contains(WindowState::TILED_TOP),
        bottom: !state.contains(WindowState::TILED_BOTTOM),
    };
    (edges.left || edges.right || edges.top || edges.bottom).then_some(edges)
}

/// Get the cursor icon that corresponds to the resize direction.
fn resize_direction_cursor_icon(
    resize_direction: Option<ResizeDirection>,
) -> CursorIcon {
    match resize_direction {
        Some(resize_direction) => match resize_direction {
            ResizeDirection::East => CursorIcon::EResize,
            ResizeDirection::North => CursorIcon::NResize,
            ResizeDirection::NorthEast => CursorIcon::NeResize,
            ResizeDirection::NorthWest => CursorIcon::NwResize,
            ResizeDirection::South => CursorIcon::SResize,
            ResizeDirection::SouthEast => CursorIcon::SeResize,
            ResizeDirection::SouthWest => CursorIcon::SwResize,
            ResizeDirection::West => CursorIcon::WResize,
        },
        None => CursorIcon::Default,
    }
}

/// Identifies resize direction based on cursor position, window dimensions
/// and the edges that may be resized.
#[allow(clippy::similar_names)]
fn cursor_resize_direction(
    win_size: winit::dpi::PhysicalSize<u32>,
    position: winit::dpi::PhysicalPosition<f64>,
    border_size: f64,
    edges: Edges,
) -> Option<ResizeDirection> {
    enum XDirection {
        West,
        East,
        Default,
    }

    enum YDirection {
        North,
        South,
        Default,
    }

    let xdir = if edges.left && position.x < border_size {
        XDirection::West
    } else if edges.right && position.x > (win_size.width as f64 - border_size)
    {
        XDirection::East
    } else {
        XDirection::Default
    };

    let ydir = if edges.top && position.y < border_size {
        YDirection::North
    } else if edges.bottom
        && position.y > (win_size.height as f64 - border_size)
    {
        YDirection::South
    } else {
        YDirection::Default
    };

    Some(match xdir {
        XDirection::West => match ydir {
            YDirection::North => ResizeDirection::NorthWest,
            YDirection::South => ResizeDirection::SouthWest,
            YDirection::Default => ResizeDirection::West,
        },

        XDirection::East => match ydir {
            YDirection::North => ResizeDirection::NorthEast,
            YDirection::South => ResizeDirection::SouthEast,
            YDirection::Default => ResizeDirection::East,
        },

        XDirection::Default => match ydir {
            YDirection::North => ResizeDirection::North,
            YDirection::South => ResizeDirection::South,
            YDirection::Default => return None,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use winit::dpi::{PhysicalPosition, PhysicalSize};

    const SIZE: PhysicalSize<u32> = PhysicalSize::new(1920, 1200);
    const BORDER: f64 = 8.0;

    fn at(x: f64, y: f64, edges: Edges) -> Option<ResizeDirection> {
        cursor_resize_direction(
            SIZE,
            PhysicalPosition::new(x, y),
            BORDER,
            edges,
        )
    }

    #[test]
    fn a_floating_window_resizes_from_every_edge_and_corner() {
        let edges = Edges::ALL;
        assert_eq!(at(1915.0, 600.0, edges), Some(ResizeDirection::East));
        assert_eq!(at(3.0, 600.0, edges), Some(ResizeDirection::West));
        assert_eq!(at(900.0, 3.0, edges), Some(ResizeDirection::North));
        assert_eq!(at(900.0, 1196.0, edges), Some(ResizeDirection::South));
        assert_eq!(at(1915.0, 1196.0, edges), Some(ResizeDirection::SouthEast));
        assert_eq!(at(3.0, 3.0, edges), Some(ResizeDirection::NorthWest));
        assert_eq!(at(1900.0, 600.0, edges), None);
    }

    #[test]
    fn a_tiled_edge_leaves_its_border_to_the_content() {
        let right_tiled = Edges {
            right: false,
            ..Edges::ALL
        };
        // A scrollbar at the right edge keeps its presses.
        assert_eq!(at(1915.0, 600.0, right_tiled), None);
        // The corner falls back to the edge that is still free.
        assert_eq!(
            at(1915.0, 1196.0, right_tiled),
            Some(ResizeDirection::South)
        );
        assert_eq!(at(3.0, 600.0, right_tiled), Some(ResizeDirection::West));

        let tiled = Edges {
            left: false,
            right: false,
            top: false,
            bottom: false,
        };
        for (x, y) in [(1915.0, 600.0), (3.0, 3.0), (900.0, 1196.0)] {
            assert_eq!(at(x, y, tiled), None);
        }
    }

    #[cfg(wayland_platform)]
    #[test]
    fn wayland_states_map_to_resizable_edges() {
        use cctk::sctk::reexports::csd_frame::WindowState;

        assert_eq!(wayland_edges(WindowState::empty()), Some(Edges::ALL));
        assert_eq!(wayland_edges(WindowState::ACTIVATED), Some(Edges::ALL));
        assert_eq!(
            wayland_edges(WindowState::TILED_RIGHT | WindowState::TILED_TOP),
            Some(Edges {
                right: false,
                top: false,
                ..Edges::ALL
            })
        );
        assert_eq!(wayland_edges(WindowState::TILED), None);
        assert_eq!(wayland_edges(WindowState::MAXIMIZED), None);
        assert_eq!(wayland_edges(WindowState::FULLSCREEN), None);
    }
}
