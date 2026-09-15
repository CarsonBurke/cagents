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

/// The gutter marker on an added or removed patch line.
///
/// The marker is tinted rather than the whole line, so the line keeps the
/// reader's ink while the gutter says what happened to it.
pub fn added_text(theme: &cosmic::Theme) -> cosmic::iced::widget::text::Style {
    cosmic::iced::widget::text::Style {
        color: Some(success_color(theme)),
        ..Default::default()
    }
}

pub fn removed_text(theme: &cosmic::Theme) -> cosmic::iced::widget::text::Style {
    cosmic::iced::widget::text::Style {
        color: Some(destructive_color(theme)),
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

/// Corner radii, as a scale rather than one value reused everywhere.
///
/// Stock COSMIC cards answer every question with the same rectangle, which is
/// what makes a surface built out of them read as a stack of slabs. A scale
/// lets a control, a code block, and a message each carry the curvature its
/// size deserves: the larger the element, the larger the radius it needs for
/// the corner to read as deliberate.
pub const RADIUS_XS: f32 = 4.0;
pub const RADIUS_S: f32 = 6.0;
pub const RADIUS_M: f32 = 10.0;
pub const RADIUS_L: f32 = 14.0;

/// Mixes `over` into `under` by `amount`, ignoring `over`'s own alpha.
///
/// Compositing by alpha would leave the result translucent, so a widget drawn
/// over another translucent surface would darken twice. Resolving the mix to
/// an opaque color keeps a stack of surfaces predictable.
fn mix(under: cosmic::iced::Color, over: cosmic::iced::Color, amount: f32) -> cosmic::iced::Color {
    let keep = 1.0 - amount;
    cosmic::iced::Color {
        r: under.r * keep + over.r * amount,
        g: under.g * keep + over.g * amount,
        b: under.b * keep + over.b * amount,
        a: 1.0,
    }
}

/// The surface a pane draws on.
fn base(theme: &cosmic::Theme) -> cosmic::iced::Color {
    theme.current_container().base.into()
}

/// A separator that reads as an edge rather than as a frame.
///
/// A full-strength border turns every element into a box. At this weight the
/// line only defines where one surface stops, which is all a border on a
/// transcript needs to do.
pub fn hairline(theme: &cosmic::Theme) -> cosmic::iced::Color {
    mix(base(theme), theme.current_container().on.into(), 0.12)
}

/// One step above the surface: a code block, a composer, a notice.
pub fn raised(theme: &cosmic::Theme) -> cosmic::iced::Color {
    mix(base(theme), theme.current_container().on.into(), 0.045)
}

/// One step below the surface, for content quoted from elsewhere.
pub fn sunken(theme: &cosmic::Theme) -> cosmic::iced::Color {
    let on: cosmic::iced::Color = theme.current_container().on.into();
    // Darkening is only available where the surface has room to darken. On a
    // light theme the same intent is served by tinting toward the ink.
    if on.r + on.g + on.b > 1.5 {
        mix(base(theme), cosmic::iced::Color::BLACK, 0.28)
    } else {
        mix(base(theme), on, 0.06)
    }
}

/// The accent at reading strength: enough to claim a surface as the user's
/// own without turning their message into a colored block.
pub fn accent_wash(theme: &cosmic::Theme) -> cosmic::iced::Color {
    mix(base(theme), theme.cosmic().accent.base.into(), 0.14)
}

pub fn accent_color(theme: &cosmic::Theme) -> cosmic::iced::Color {
    theme.cosmic().accent.base.into()
}

/// A rounded surface with an optional hairline edge.
///
/// `radius` takes the four corners clockwise from the top left, so a shape can
/// be asymmetric where that carries meaning - a message bubble tucking its
/// corner toward its author, a header joining the block below it.
pub fn surface(
    fill: Option<fn(&cosmic::Theme) -> cosmic::iced::Color>,
    radius: [f32; 4],
    edge: bool,
) -> cosmic::theme::Container<'static> {
    cosmic::theme::Container::custom(move |theme| cosmic::widget::container::Style {
        background: fill.map(|fill| cosmic::iced::Background::Color(fill(theme))),
        border: cosmic::iced::Border {
            radius: radius.into(),
            width: if edge { BORDER_WIDTH } else { 0.0 },
            color: if edge {
                hairline(theme)
            } else {
                cosmic::iced::Color::TRANSPARENT
            },
        },
        ..Default::default()
    })
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
