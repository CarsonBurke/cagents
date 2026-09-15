//! Shared geometry and responsive projections for the native workbench shell.
//!
//! Color remains owned by COSMIC's semantic theme classes. These tokens cover
//! typography, dimensions, and shell mounting decisions shared by the header,
//! navigation, inspector, dock, and task surfaces.

pub const GRID_UNIT: f32 = 4.0;

pub const HEADER_HEIGHT: f32 = 44.0;
pub const ROW_HEIGHT: f32 = 32.0;
pub const DENSE_ROW_HEIGHT: f32 = 28.0;
pub const PRIMARY_CONTROL_SIZE: f32 = 30.0;
pub const SECONDARY_CONTROL_MIN_SIZE: f32 = 30.0;

/// Icons that sit inside a row, beside its text. Every surface that renders
/// this role - navigation rows, panel rows, header status - uses this token, so
/// the same role cannot be 16px in one pane and 14px in the next.
pub const ROW_ICON_SIZE: u16 = 16;
/// Icons that stand alone as the illustration of a whole pane state.
pub const STATUS_ICON_SIZE: u16 = 20;

pub const TEXT_BODY: f32 = 13.0;
pub const TEXT_CAPTION: f32 = 11.0;
pub const TEXT_HEADING: f32 = 13.0;

/// The shell's "working" tone: something is in flight and may still fail.
pub fn warning_color(theme: &cosmic::Theme) -> cosmic::iced::Color {
    theme.cosmic().warning.base.into()
}

/// A color resolved when a widget draws, rather than when a view is built.
///
/// `cosmic::theme::active()` locks the process-global theme and clones the
/// active `Theme`, so a per-row builder that reached for a semantic color paid
/// that round trip once per visible row per frame. Naming the resolver instead
/// lets the widget read the `&Theme` its own `draw` is already handed, which is
/// also the theme the frame actually renders with.
#[derive(Clone, Copy)]
pub enum Tone {
    /// A color the caller has already resolved.
    Fixed(cosmic::iced::Color),
    /// A semantic color, read from the drawing theme.
    Themed(fn(&cosmic::Theme) -> cosmic::iced::Color),
}

impl Tone {
    pub fn resolve(self, theme: &cosmic::Theme) -> cosmic::iced::Color {
        match self {
            Self::Fixed(color) => color,
            Self::Themed(resolve) => resolve(theme),
        }
    }
}

/// Secondary labels stay readable without competing with active content.
pub fn muted_color(theme: &cosmic::Theme) -> cosmic::iced::Color {
    let mut color: cosmic::iced::Color = theme.current_container().on.into();
    color.a *= 0.75;
    color
}

pub fn muted_text(theme: &cosmic::Theme) -> cosmic::iced::widget::text::Style {
    cosmic::iced::widget::text::Style {
        color: Some(muted_color(theme)),
        ..Default::default()
    }
}

/// Symbolic icons ignore alpha in their SVG recoloring. Blend them toward the
/// surface instead so muted navigation glyphs stay visibly quieter than text.
pub fn muted_icon_color(theme: &cosmic::Theme) -> cosmic::iced::Color {
    let on: cosmic::iced::Color = theme.current_container().on.into();
    let base: cosmic::iced::Color = theme.current_container().base.into();
    cosmic::iced::Color {
        r: on.r * 0.38 + base.r * 0.62,
        g: on.g * 0.38 + base.g * 0.62,
        b: on.b * 0.38 + base.b * 0.62,
        a: 1.0,
    }
}

pub fn body<'a>(
    text: impl Into<std::borrow::Cow<'a, str>> + 'a,
) -> cosmic::widget::text::Text<'a, cosmic::Theme, cosmic::Renderer> {
    cosmic::widget::text::body(text)
        .size(TEXT_BODY)
        .line_height(cosmic::iced::widget::text::LineHeight::Absolute(
            18.0.into(),
        ))
}

pub fn caption<'a>(
    text: impl Into<std::borrow::Cow<'a, str>> + 'a,
) -> cosmic::widget::text::Text<'a, cosmic::Theme, cosmic::Renderer> {
    cosmic::widget::text::caption(text)
        .size(TEXT_CAPTION)
        .class(cosmic::theme::Text::Custom(muted_text))
}

pub fn heading<'a>(
    text: impl Into<std::borrow::Cow<'a, str>> + 'a,
) -> cosmic::widget::text::Text<'a, cosmic::Theme, cosmic::Renderer> {
    body(text).size(TEXT_HEADING).font(cosmic::font::Font {
        weight: cosmic::iced::font::Weight::Medium,
        ..cosmic::font::default()
    })
}

/// The widest a column of prose is allowed to grow.
///
/// Reading a paragraph that spans a maximised window costs the eye its place
/// on every wrap. Typography practice puts the comfortable measure at roughly
/// 60 to 80 characters, which at our body size is this many logical pixels.
/// The transcript's own chrome still spans the full width; only the text is
/// held back.
pub const READABLE_MEASURE: f32 = 720.0;

/// A patch's added and removed lines, tinted rather than filled.
///
/// The semantic accent at full strength competes with the code it is meant to
/// annotate, so each tone is mixed toward the surface it draws on: strong
/// enough to read at a glance down the gutter, weak enough that the text on
/// top keeps its contrast.
fn diff_tone(theme: &cosmic::Theme, accent: cosmic::iced::Color) -> cosmic::iced::Color {
    let base: cosmic::iced::Color = theme.current_container().base.into();
    cosmic::iced::Color {
        r: accent.r * 0.22 + base.r * 0.78,
        g: accent.g * 0.22 + base.g * 0.78,
        b: accent.b * 0.22 + base.b * 0.78,
        a: 1.0,
    }
}

pub fn diff_added_color(theme: &cosmic::Theme) -> cosmic::iced::Color {
    diff_tone(theme, theme.cosmic().success.base.into())
}

pub fn diff_removed_color(theme: &cosmic::Theme) -> cosmic::iced::Color {
    diff_tone(theme, theme.cosmic().destructive.base.into())
}

pub fn success_color(theme: &cosmic::Theme) -> cosmic::iced::Color {
    theme.cosmic().success.base.into()
}

pub fn destructive_color(theme: &cosmic::Theme) -> cosmic::iced::Color {
    theme.cosmic().destructive.base.into()
}

pub const NAVIGATION_DEFAULT_WIDTH: f32 = 260.0;
pub const NAVIGATION_MIN_WIDTH: f32 = 220.0;
pub const NAVIGATION_MAX_WIDTH: f32 = 420.0;
pub const NAVIGATION_RAIL_WIDTH: f32 = 52.0;
pub const INSPECTOR_MIN_WIDTH: f32 = 320.0;
pub const INSPECTOR_DEFAULT_WIDTH: f32 = 380.0;
pub const INSPECTOR_MAX_WIDTH: f32 = 480.0;

/// Shared gutter geometry for native scrollables and custom-rendered content.
///
/// The three constants are one contract, not three independent numbers. A
/// scrollbar occupies `SCROLLBAR_GUTTER` on its axis; the visible thumb is
/// `SCROLLBAR_THUMB_WIDTH` and sits centred in that rail. Content is inset by
/// the full rail, so a floating thumb never overlaps a label, icon, badge, or
/// interactive target even at fractional display scales.
pub const SCROLLBAR_GUTTER: f32 = 12.0;
pub const SCROLLBAR_THUMB_WIDTH: f32 = 6.0;
/// Centres the thumb in the rail. Native scrollbars occupy
/// `width + margin * 2`, which this makes exactly `SCROLLBAR_GUTTER`, so a
/// native surface and a custom-drawn one reserve the same space.
pub const SCROLLBAR_MARGIN: f32 = (SCROLLBAR_GUTTER - SCROLLBAR_THUMB_WIDTH) / 2.0;
pub const BORDER_WIDTH: f32 = 1.0;

pub fn scrollbar() -> cosmic::iced::widget::scrollable::Scrollbar {
    cosmic::iced::widget::scrollable::Scrollbar::default()
        .width(SCROLLBAR_THUMB_WIDTH)
        .scroller_width(SCROLLBAR_THUMB_WIDTH)
        .margin(SCROLLBAR_MARGIN)
}

/// Reserves the vertical scrollbar's rail inside `padding`.
///
/// Every vertical scrollable insets its content this way rather than letting
/// the thumb float over it. `Scrollbar::spacing` would reserve the space for
/// us, but it also embeds the scrollbar, which then takes layout space and
/// stays visible on content that does not scroll at all.
pub fn with_vertical_rail(padding: cosmic::iced::Padding) -> cosmic::iced::Padding {
    cosmic::iced::Padding {
        right: padding.right + SCROLLBAR_GUTTER,
        ..padding
    }
}

/// Reserves the horizontal scrollbar's rail inside `padding`.
pub fn with_horizontal_rail(padding: cosmic::iced::Padding) -> cosmic::iced::Padding {
    cosmic::iced::Padding {
        bottom: padding.bottom + SCROLLBAR_GUTTER,
        ..padding
    }
}

/// Reserves both rails, leaving the bottom-right junction clear of content.
pub fn with_both_rails(padding: cosmic::iced::Padding) -> cosmic::iced::Padding {
    with_horizontal_rail(with_vertical_rail(padding))
}

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
}
