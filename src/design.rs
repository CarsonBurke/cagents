//! Shared geometry and responsive projections for the native workbench shell.
//!
//! Color remains owned by COSMIC's semantic theme classes. These tokens cover
//! only dimensions and shell mounting decisions that must stay consistent
//! across the header, navigation, inspector, dock, and task surfaces.

pub const GRID_UNIT: f32 = 4.0;

pub const HEADER_HEIGHT: f32 = 40.0;
pub const ROW_HEIGHT: f32 = 32.0;
pub const DENSE_ROW_HEIGHT: f32 = 28.0;
pub const PRIMARY_CONTROL_SIZE: f32 = 28.0;
pub const SECONDARY_CONTROL_MIN_SIZE: f32 = 24.0;

pub const NAVIGATION_DEFAULT_WIDTH: f32 = 260.0;
pub const NAVIGATION_MIN_WIDTH: f32 = 220.0;
pub const NAVIGATION_MAX_WIDTH: f32 = 420.0;
pub const NAVIGATION_RAIL_WIDTH: f32 = 52.0;
pub const INSPECTOR_MIN_WIDTH: f32 = 320.0;
pub const INSPECTOR_DEFAULT_WIDTH: f32 = 380.0;
pub const INSPECTOR_MAX_WIDTH: f32 = 480.0;

pub const CONTROL_RADIUS: f32 = 6.0;
pub const FLOATING_SURFACE_RADIUS: f32 = 8.0;
pub const BORDER_WIDTH: f32 = 1.0;

pub const WIDE_BREAKPOINT: f32 = 1_440.0;
pub const EXPANDED_NAVIGATION_BREAKPOINT: f32 = 1_200.0;
pub const OVERLAY_DOCK_BREAKPOINT: f32 = 900.0;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NavigationLayout {
    Expanded,
    Rail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InspectorLayout {
    Docked,
    Overlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DockLayout {
    Split,
    Overlay,
}

/// The complete responsive mounting decision for a logical window width.
///
/// Invalid widths use the most constrained projection. The application keeps
/// its last valid reported width, but this fallback prevents a future caller
/// from exposing controls that may not fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShellLayout {
    pub navigation: NavigationLayout,
    pub inspector: InspectorLayout,
    pub dock: DockLayout,
}

impl ShellLayout {
    pub fn for_width(width: f32) -> Self {
        if !width.is_finite() || width <= 0.0 {
            return Self {
                navigation: NavigationLayout::Rail,
                inspector: InspectorLayout::Overlay,
                dock: DockLayout::Overlay,
            };
        }
        if width >= WIDE_BREAKPOINT {
            Self {
                navigation: NavigationLayout::Expanded,
                inspector: InspectorLayout::Docked,
                dock: DockLayout::Split,
            }
        } else if width >= EXPANDED_NAVIGATION_BREAKPOINT {
            Self {
                navigation: NavigationLayout::Expanded,
                inspector: InspectorLayout::Overlay,
                dock: DockLayout::Split,
            }
        } else if width >= OVERLAY_DOCK_BREAKPOINT {
            Self {
                navigation: NavigationLayout::Rail,
                inspector: InspectorLayout::Overlay,
                dock: DockLayout::Split,
            }
        } else {
            Self {
                navigation: NavigationLayout::Rail,
                inspector: InspectorLayout::Overlay,
                dock: DockLayout::Overlay,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn responsive_boundaries_match_the_workbench_contract() {
        assert_eq!(
            ShellLayout::for_width(1_440.0),
            ShellLayout {
                navigation: NavigationLayout::Expanded,
                inspector: InspectorLayout::Docked,
                dock: DockLayout::Split,
            }
        );
        assert_eq!(
            ShellLayout::for_width(1_439.0),
            ShellLayout {
                navigation: NavigationLayout::Expanded,
                inspector: InspectorLayout::Overlay,
                dock: DockLayout::Split,
            }
        );
        assert_eq!(
            ShellLayout::for_width(1_200.0),
            ShellLayout {
                navigation: NavigationLayout::Expanded,
                inspector: InspectorLayout::Overlay,
                dock: DockLayout::Split,
            }
        );
        assert_eq!(
            ShellLayout::for_width(1_199.0),
            ShellLayout {
                navigation: NavigationLayout::Rail,
                inspector: InspectorLayout::Overlay,
                dock: DockLayout::Split,
            }
        );
        assert_eq!(
            ShellLayout::for_width(900.0),
            ShellLayout {
                navigation: NavigationLayout::Rail,
                inspector: InspectorLayout::Overlay,
                dock: DockLayout::Split,
            }
        );
        assert_eq!(
            ShellLayout::for_width(899.0),
            ShellLayout {
                navigation: NavigationLayout::Rail,
                inspector: InspectorLayout::Overlay,
                dock: DockLayout::Overlay,
            }
        );
    }

    #[test]
    fn invalid_widths_choose_the_constrained_projection() {
        let constrained = ShellLayout {
            navigation: NavigationLayout::Rail,
            inspector: InspectorLayout::Overlay,
            dock: DockLayout::Overlay,
        };
        assert_eq!(ShellLayout::for_width(f32::NAN), constrained);
        assert_eq!(ShellLayout::for_width(f32::NEG_INFINITY), constrained);
        assert_eq!(ShellLayout::for_width(f32::INFINITY), constrained);
        assert_eq!(ShellLayout::for_width(-1.0), constrained);
    }

    #[test]
    fn geometry_uses_the_four_pixel_grid_and_accessible_targets() {
        const {
            assert!(PRIMARY_CONTROL_SIZE >= SECONDARY_CONTROL_MIN_SIZE);
            assert!(SECONDARY_CONTROL_MIN_SIZE >= 24.0);
        }
        for value in [
            HEADER_HEIGHT,
            ROW_HEIGHT,
            DENSE_ROW_HEIGHT,
            PRIMARY_CONTROL_SIZE,
            SECONDARY_CONTROL_MIN_SIZE,
            NAVIGATION_DEFAULT_WIDTH,
            NAVIGATION_MIN_WIDTH,
            NAVIGATION_MAX_WIDTH,
            NAVIGATION_RAIL_WIDTH,
            INSPECTOR_MIN_WIDTH,
            INSPECTOR_DEFAULT_WIDTH,
            INSPECTOR_MAX_WIDTH,
        ] {
            assert_eq!(value % GRID_UNIT, 0.0, "{value} is off-grid");
        }
    }
}
