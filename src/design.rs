//! The shell's design language: its metrics, its tones and its type.
//!
//! COSMIC supplies the palette and the widget toolkit; everything a surface
//! needs to look like part of this workbench is named here. Dimensions live
//! in this module, the spacing scale in [`space`], semantic color in `color`,
//! the type scale in `typography`, and the responsive mounting of the shell's
//! regions in `layout`. Composed controls live in [`crate::ui_controls`].

mod color;
mod layout;
pub mod space;
mod typography;

pub use color::*;
pub use layout::*;
pub use typography::*;

use cosmic::{
    Theme,
    iced::{self, Color, Padding},
    widget,
};

pub const GRID_UNIT: f32 = 4.0;
pub const HEADER_HEIGHT: f32 = 44.0;
/// Every standard header includes its bottom separator in HEADER_HEIGHT.
pub const HEADER_CONTENT_HEIGHT: f32 = HEADER_HEIGHT - BORDER_WIDTH;

pub const ROW_HEIGHT: f32 = 32.0;
pub const DENSE_ROW_HEIGHT: f32 = 28.0;
pub const PRIMARY_CONTROL_SIZE: f32 = 30.0;
pub const SECONDARY_CONTROL_MIN_SIZE: f32 = 30.0;

/// Icons that sit inside a row, beside its text. Every surface that renders
/// this role - navigation rows, panel rows, header status - uses this token, so
/// the same role cannot be 16px in one pane and 14px in the next.
pub const ROW_ICON_SIZE: u16 = 16;
pub const TAB_STATUS_ICON_SIZE: u16 = 14;
pub const WINDOW_CONTROL_SIZE: f32 = 32.0;
pub const COMMAND_POPOVER_WIDTH: f32 = 292.0;
pub const NOTICE_POPOVER_WIDTH: f32 = 408.0;
pub const PANE_NAVIGATOR_MIN_WIDTH: f32 = 42.0;
pub const PANE_NAVIGATOR_RESERVED_WIDTH: f32 = 110.0;
pub const TAB_MIN_WIDTH: f32 = 160.0;
pub const TAB_MAX_WIDTH: f32 = 420.0;
/// New panes fill a tiled workspace without changing existing pane widths.
pub const NARROW_WORKSPACE_WIDTH: f32 = 1_100.0;
pub const PULL_REQUEST_BROWSER_ROW_HEIGHT: f32 = 48.0;
/// Icons that stand alone as the illustration of a whole pane state.
pub const STATUS_ICON_SIZE: u16 = 20;
/// A control that sits inside prose rather than in a toolbar, and its icon.
///
/// A message's own controls belong to the message, so they are drawn at
/// caption scale. At the primary size they read as a button bar under every
/// answer and competed with the answer itself.
pub const COMPACT_CONTROL_SIZE: f32 = 20.0;
pub const COMPACT_ICON_SIZE: u16 = 12;

/// One full turn of the activity spinner.
///
/// The shell's indicator reports only that work is in flight, never how much
/// of it is left, so the arc turns at a constant rate. An indicator that eases
/// or breathes reads as progress the shell cannot actually measure.
pub const SPINNER_PERIOD: std::time::Duration = std::time::Duration::from_millis(1_200);

/// How much of the circle the spinner's arc covers, in turns.
///
/// The gap is what makes the rotation legible - a closed ring looks static
/// however fast it spins - and three quarters leaves the arc heavy enough to
/// read at row-icon size.
pub const SPINNER_ARC_SPAN: f32 = 0.75;

/// The radial budget gauge: a ring the height of a status-line row, with its
/// percentage set beside it.
///
/// Sized to the caption it sits next to rather than to a dial, because the
/// figure is what a reader reads and the ring is only what makes the
/// proportion visible without reading it. The stroke is heavy relative to the
/// radius so the first few percent of a budget still register as ink.
pub const RADIAL_GAUGE_SIZE: f32 = 16.0;
pub const RADIAL_GAUGE_STROKE: f32 = 3.0;

/// The height of a linear budget gauge: enough to read as a bar at a glance
/// without taking a row of its own.
pub const BUDGET_BAR_HEIGHT: f32 = 4.0;

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

/// A rounded surface with an optional hairline edge.
///
/// `radius` takes the four corners clockwise from the top left, so a shape can
/// be asymmetric where that carries meaning - a message bubble tucking its
/// corner toward its author, a header joining the block below it.
pub fn surface(
    fill: Option<fn(&Theme) -> Color>,
    radius: [f32; 4],
    edge: bool,
) -> cosmic::theme::Container<'static> {
    cosmic::theme::Container::custom(move |theme| widget::container::Style {
        background: fill.map(|fill| iced::Background::Color(fill(theme))),
        border: iced::Border {
            radius: radius.into(),
            width: if edge { BORDER_WIDTH } else { 0.0 },
            color: if edge {
                hairline(theme)
            } else {
                Color::TRANSPARENT
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
pub const SCROLLBAR_GUTTER: f32 = 8.0;
pub const SCROLLBAR_THUMB_WIDTH: f32 = 4.0;
/// Centres the thumb in the rail. Native scrollbars occupy
/// `width + margin * 2`, which this makes exactly `SCROLLBAR_GUTTER`, so a
/// native surface and a custom-drawn one reserve the same space.
pub const SCROLLBAR_MARGIN: f32 = (SCROLLBAR_GUTTER - SCROLLBAR_THUMB_WIDTH) / 2.0;
pub const BORDER_WIDTH: f32 = 1.0;

pub fn scrollbar() -> iced::widget::scrollable::Scrollbar {
    iced::widget::scrollable::Scrollbar::default()
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
pub fn with_vertical_rail(padding: Padding) -> Padding {
    Padding {
        right: padding.right + SCROLLBAR_GUTTER,
        ..padding
    }
}

/// Reserves the horizontal scrollbar's rail inside `padding`.
pub fn with_horizontal_rail(padding: Padding) -> Padding {
    Padding {
        bottom: padding.bottom + SCROLLBAR_GUTTER,
        ..padding
    }
}

/// Reserves both rails, leaving the bottom-right junction clear of content.
pub fn with_both_rails(padding: Padding) -> Padding {
    with_horizontal_rail(with_vertical_rail(padding))
}

/// How wide the model picker's popup is.
///
/// A picker is sized to the identifiers it lists rather than to the pane it
/// hangs off: a provider offering `claude-sonnet-4-5-20250929` needs about
/// this much, and a list that took its width from the pane put a three-word
/// label on a row several hundred points long. It is a fixed width rather
/// than a ceiling because a popup's content is free to fill the space it is
/// given, and a panel that hugged its widest row would change width with
/// every keystroke of the query.
pub const MODEL_POPUP_WIDTH: f32 = 280.0;
/// How tall the model list is allowed to grow before it scrolls.
pub const MODEL_POPUP_CEILING: f32 = 280.0;

/// The leading and trailing inset of a header toolbar, and the gap between the
/// control groups inside it.
///
/// A header reads as a single rhythm: the inset that holds its first group off
/// the window edge is the same measure as the gap between two groups, so one
/// number describes both. Naming it here is what stops an embedding shell from
/// adding a second, different inset of its own on top of the toolbar's.
pub const TOOLBAR_INSET: f32 = space::M;
pub const TOOLBAR_GROUP_SPACING: f32 = TOOLBAR_INSET;

/// Room for the widest label a counter wedged between two step controls can
/// hold, so the control after it does not move as the count changes.
pub const STEPPER_LABEL_WIDTH: f32 = GRID_UNIT * 12.0;

/// The surface of anything that floats above the workbench: a popover, a
/// menu, a tooltip.
///
/// One treatment for all of them - the component fill, a hairline edge and a
/// soft drop shadow - so a floating surface is recognised as one before its
/// content is read. Its radius is concentric with the controls it holds at
/// the [`space::XS`] inset its content is given.
pub fn elevated() -> cosmic::theme::Container<'static> {
    cosmic::theme::Container::custom(|theme| {
        let component = &theme.current_container().component;
        widget::container::Style {
            background: Some(iced::Background::Color(component.base.into())),
            text_color: Some(component.on.into()),
            icon_color: Some(component.on.into()),
            border: iced::Border {
                color: component_rule(theme),
                width: BORDER_WIDTH,
                radius: RADIUS_M.into(),
            },
            shadow: ELEVATION,
            ..Default::default()
        }
    })
}

/// The shadow under an [`elevated`] surface. Black at this strength reads as
/// depth on a dark theme and stays a soft edge on a light one.
pub const ELEVATION: iced::Shadow = iced::Shadow {
    color: Color::from_rgba(0.0, 0.0, 0.0, 0.24),
    offset: iced::Vector::new(0.0, 4.0),
    blur_radius: 16.0,
};

/// An alignment-only container class.
///
/// COSMIC's default transparent container resets the ink it inherits. A
/// wrapper that only centers or pads must instead pass its parent's text and
/// icon colors through, or a button's status tone never reaches its glyph.
pub fn transparent() -> cosmic::theme::Container<'static> {
    cosmic::theme::Container::custom(|_| widget::container::Style::default())
}

/// The blank every button style starts from.
///
/// `button::Style::new()` reads one radius from the process-global theme,
/// which costs a lock on every style resolution of every button in a frame.
/// The shell sets its own radius, so it starts from this instead.
pub fn button_style() -> widget::button::Style {
    widget::button::Style {
        shadow_offset: iced::Vector::ZERO,
        background: None,
        overlay: None,
        border_radius: RADIUS_S.into(),
        border_width: 0.0,
        border_color: Color::TRANSPARENT,
        outline_width: 0.0,
        outline_color: Color::TRANSPARENT,
        icon_color: None,
        text_color: None,
    }
}

pub fn horizontal_rule<'a, Message: 'a>() -> cosmic::Element<'a, Message> {
    crate::separator::horizontal()
}

pub fn vertical_rule<'a, Message: 'a>() -> cosmic::Element<'a, Message> {
    crate::separator::vertical()
}
