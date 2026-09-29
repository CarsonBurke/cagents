//! Shared geometry and responsive projections for the native workbench shell.
//!
//! Color remains owned by COSMIC's semantic theme classes. These tokens cover
//! typography, dimensions, and shell mounting decisions shared by the header,
//! navigation, inspector, dock, and task surfaces.

pub const GRID_UNIT: f32 = 4.0;
pub const SCRIM_ALPHA: f32 = 0.48;
pub const ATTENTION_BORDER_ALPHA: f32 = 0.35;
pub const DIFF_GUTTER_ALPHA: f32 = 0.48;
pub const DIFF_DIVIDER_ALPHA: f32 = 0.16;

pub const HEADER_HEIGHT: f32 = 44.0;
/// Every standard header includes its bottom separator in HEADER_HEIGHT.
pub const HEADER_CONTENT_HEIGHT: f32 = HEADER_HEIGHT - BORDER_WIDTH;

pub fn horizontal_rule<'a, Message: 'a>() -> cosmic::Element<'a, Message> {
    crate::separator::horizontal()
}

pub fn vertical_rule<'a, Message: 'a>() -> cosmic::Element<'a, Message> {
    crate::separator::vertical()
}
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
/// A control that sits inside prose rather than in a toolbar, and its icon.
///
/// A message's own controls belong to the message, so they are drawn at
/// caption scale. At the primary size they read as a button bar under every
/// answer and competed with the answer itself.
pub const COMPACT_CONTROL_SIZE: f32 = 20.0;
pub const COMPACT_ICON_SIZE: u16 = 12;

pub const TEXT_BODY: f32 = 13.0;
/// Prose that is read at length - an agent's answers and the draft being
/// written to it - one step above the shell's controls and captions.
pub const TEXT_PROSE: f32 = 14.0;
pub const TEXT_CAPTION: f32 = 11.0;
pub const TEXT_HEADING: f32 = 13.0;

/// Markdown shares the shell's semantic palette and a compact heading scale.
pub fn markdown_settings(theme: &cosmic::Theme, size: f32) -> cosmic::widget::markdown::Settings {
    use cosmic::{iced, widget::markdown};
    let mut style = markdown::Style::from(if theme.theme_type.is_dark() {
        iced::Theme::Dark
    } else {
        iced::Theme::Light
    });
    style.link_color = accent_color(theme);
    style.inline_code_color = theme.cosmic().background(false).on.into();
    style.inline_code_highlight.background = raised(theme).into();
    style.inline_code_highlight.border.radius = RADIUS_XS.into();
    markdown::Settings {
        h1_size: (size + GRID_UNIT).into(),
        h2_size: (size + GRID_UNIT / 2.0).into(),
        h3_size: (size + 1.0).into(),
        h4_size: size.into(),
        h5_size: size.into(),
        h6_size: size.into(),
        code_size: size.into(),
        spacing: (GRID_UNIT * 2.0).into(),
        ..markdown::Settings::with_text_size(size, style)
    }
}

pub fn comment_card<'a, Message: 'a>(
    author: &str,
    created_at: &str,
    body: cosmic::Element<'a, Message>,
    actions: cosmic::Element<'a, Message>,
) -> cosmic::Element<'a, Message> {
    use cosmic::{
        iced::{Alignment, Length},
        widget,
    };
    let initial = author
        .chars()
        .next()
        .map(|ch| ch.to_uppercase().to_string())
        .unwrap_or_else(|| "?".into());
    let age = created_at.parse::<jiff::Timestamp>().ok().map(|date| {
        jiff::Timestamp::now()
            .as_second()
            .saturating_sub(date.as_second())
            .max(0)
    });
    let time = match age {
        Some(seconds) if seconds < 60 => "Just now".to_owned(),
        Some(seconds) if seconds < 3600 => format!("{}m ago", seconds / 60),
        Some(seconds) if seconds < 86400 => format!("{}h ago", seconds / 3600),
        Some(seconds) => format!("{}d ago", seconds / 86400),
        None => created_at.to_owned(),
    };
    widget::container(
        widget::column::with_children(vec![
            widget::row::with_children(vec![
                widget::container(heading(initial))
                    .center_x(24)
                    .center_y(24)
                    .class(surface(Some(raised), [RADIUS_M; 4], false))
                    .into(),
                heading(author.to_owned()).width(Length::Fill).into(),
                caption(time).into(),
                actions,
            ])
            .spacing(2.0 * GRID_UNIT)
            .align_y(Alignment::Center)
            .into(),
            body,
        ])
        .spacing(2.0 * GRID_UNIT)
        .width(Length::Fill),
    )
    .padding(2.0 * GRID_UNIT)
    .width(Length::Fill)
    .class(surface(Some(raised), [RADIUS_S; 4], true))
    .into()
}

/// The shell's "working" tone: something is in flight and may still fail.
pub fn warning_color(theme: &cosmic::Theme) -> cosmic::iced::Color {
    theme.cosmic().warning.base.into()
}

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

/// Where a budget stops being a detail and becomes a decision.
///
/// Below the first threshold the figure is background. Above the second the
/// reader has to compact or lose the thread, so detailed budget bars use the
/// tone the shell uses for everything else that is about to go wrong.
pub const BUDGET_WARNING: f32 = 0.75;
pub const BUDGET_CRITICAL: f32 = 0.9;

/// The tone a budget that is `used` full is drawn in.
///
/// Detailed usage bars carry this urgency at every level. Compact radial
/// gauges stay neutral while healthy, so a row of accounts does not become a
/// row of competing colors, and take the warning or destructive tone only
/// once their budget is in trouble.
pub fn budget_tone(used: f32) -> fn(&cosmic::Theme) -> cosmic::iced::Color {
    if used > BUDGET_CRITICAL {
        destructive_color
    } else if used > BUDGET_WARNING {
        warning_color
    } else {
        accent_color
    }
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
///
/// The margin between this and full strength is deliberately small. At a
/// heavier mute the sidebar's own rows read as disabled controls, which is the
/// one thing a navigable row must not look like; `sidebar_button_style` keeps
/// its disabled alpha well below this so the two states stay distinguishable.
pub fn muted_color(theme: &cosmic::Theme) -> cosmic::iced::Color {
    let mut color: cosmic::iced::Color = theme.current_container().on.into();
    color.a *= 0.88;
    color
}

pub fn muted_text(theme: &cosmic::Theme) -> cosmic::iced::widget::text::Style {
    cosmic::iced::widget::text::Style {
        color: Some(muted_color(theme)),
        ..Default::default()
    }
}

/// Text that reports a failure the reader is expected to act on.
pub fn warning_text(theme: &cosmic::Theme) -> cosmic::iced::widget::text::Style {
    cosmic::iced::widget::text::Style {
        color: Some(warning_color(theme)),
        ..Default::default()
    }
}

/// Text that reports an error the reader has to correct.
pub fn destructive_text(theme: &cosmic::Theme) -> cosmic::iced::widget::text::Style {
    cosmic::iced::widget::text::Style {
        color: Some(destructive_color(theme)),
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
/// surface instead so muted navigation glyphs stay visibly quieter than text
/// while still carrying most of the foreground's contrast.
pub fn muted_icon_color(theme: &cosmic::Theme) -> cosmic::iced::Color {
    let on: cosmic::iced::Color = theme.current_container().on.into();
    let base: cosmic::iced::Color = theme.current_container().base.into();
    cosmic::iced::Color {
        r: on.r * 0.72 + base.r * 0.28,
        g: on.g * 0.72 + base.g * 0.28,
        b: on.b * 0.72 + base.b * 0.28,
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

/// The wash an editor lays under a line the unsaved buffer has added.
///
/// Translucent rather than mixed toward a surface colour, because it is
/// painted over the editor's own background rather than over the container
/// [`diff_tone`] mixes against, and the glyphs are drawn on top of it: a fill
/// strong enough to be seen down the side of the buffer still has to leave
/// the code its contrast.
pub fn added_line_wash(theme: &cosmic::Theme) -> cosmic::iced::Color {
    cosmic::iced::Color {
        a: 0.12,
        ..success_color(theme)
    }
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
    mix(
        base(theme),
        theme.current_container().on.into(),
        0.12 * rule_strength(theme),
    )
}

/// How strongly shell rules paint relative to the theme's own divider. The
/// theme's divider is tuned for cosmic's boxed widgets; between panes that
/// already differ in content it reads heavier than an edge needs to.
const RULE_STRENGTH: f32 = 0.8;

/// The color every shell rule between surfaces paints: pane separators,
/// splitters, popover and page edges.
///
/// It is opaque, so rules that cross or meet paint one solid junction rather
/// than compounding into a darker dot where two translucent lines overlap.
pub fn rule(theme: &cosmic::Theme) -> cosmic::iced::Color {
    let container = theme.current_container();
    soften(theme, base(theme), container.divider.into())
}

/// `rule` for an edge drawn on a component surface such as a popover.
pub fn component_rule(theme: &cosmic::Theme) -> cosmic::iced::Color {
    let component = &theme.current_container().component;
    soften(theme, component.base.into(), component.divider.into())
}

/// How strongly `theme` paints its rules. A high-contrast theme asked for its
/// edges at full strength, so it keeps them.
fn rule_strength(theme: &cosmic::Theme) -> f32 {
    if theme.cosmic().is_high_contrast {
        1.0
    } else {
        RULE_STRENGTH
    }
}

fn soften(
    theme: &cosmic::Theme,
    surface: cosmic::iced::Color,
    divider: cosmic::iced::Color,
) -> cosmic::iced::Color {
    softened(surface, divider, rule_strength(theme))
}

/// `divider` as painted on `surface`, moved back toward the surface by the
/// share of `strength` the rule gives up.
fn softened(
    surface: cosmic::iced::Color,
    divider: cosmic::iced::Color,
    strength: f32,
) -> cosmic::iced::Color {
    let painted = mix(surface, divider, divider.a);
    mix(surface, painted, strength)
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

/// The unfilled part of a gauge.
///
/// A gauge has to read as a whole even where it is empty, so its track is
/// tinted toward the ink rather than away from it. A track one step below the
/// surface vanished into the pane, which left a small fill looking like a
/// stray dot with nothing to measure it against.
pub fn track(theme: &cosmic::Theme) -> cosmic::iced::Color {
    mix(base(theme), theme.current_container().on.into(), 0.18)
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
pub const SCROLLBAR_GUTTER: f32 = 8.0;
pub const SCROLLBAR_THUMB_WIDTH: f32 = 4.0;
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

/// The leading and trailing inset of a header toolbar, and the gap between the
/// control groups inside it.
///
/// A header reads as a single rhythm: the inset that holds its first group off
/// the window edge is the same measure as the gap between two groups, so one
/// number describes both. Naming it here is what stops an embedding shell from
/// adding a second, different inset of its own on top of the toolbar's.
pub const TOOLBAR_INSET: f32 = GRID_UNIT * 3.0;
pub const TOOLBAR_GROUP_SPACING: f32 = TOOLBAR_INSET;

/// Room for the widest label a counter wedged between two step controls can
/// hold, so the control after it does not move as the count changes.
pub const STEPPER_LABEL_WIDTH: f32 = GRID_UNIT * 12.0;

/// A small status pill.
///
/// A condition with a name -- open, merged, failing, resolved -- is not another
/// sentence at the same weight as the column around it. Giving it a shape and a
/// tone lets the eye find the state before it reads any of the words, which is
/// the whole reason the state is on the row.
pub fn badge<'a, Message: 'a>(
    label: impl Into<std::borrow::Cow<'a, str>> + 'a,
    tone: Tone,
) -> cosmic::Element<'a, Message> {
    cosmic::widget::container(
        caption(label)
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .class(cosmic::theme::Text::Default),
    )
    .padding(cosmic::iced::Padding {
        top: 1.0,
        right: GRID_UNIT + 2.0,
        bottom: 1.0,
        left: GRID_UNIT + 2.0,
    })
    .class(cosmic::theme::Container::custom(move |theme| {
        let color = tone.resolve(theme);
        let mut wash = color;
        // Strong enough to read as a filled pill, weak enough that the label
        // on top keeps its contrast at either theme polarity.
        wash.a *= 0.18;
        cosmic::widget::container::Style {
            background: Some(cosmic::iced::Background::Color(wash)),
            text_color: Some(color),
            border: cosmic::iced::Border {
                radius: RADIUS_XS.into(),
                ..Default::default()
            },
            ..Default::default()
        }
    }))
    .into()
}

/// A `+12 −4` change count, with each side carrying its own semantic tone.
///
/// [`crate::repository::line_change_counts`] joins the two sides with a single
/// space and omits a side it measured as zero, so the position of that space
/// is all that is needed to tell the added part from the removed part. The
/// counts are the only place the shell states a magnitude of change, and a
/// monochrome `+141 −3` makes the reader parse the sign to learn which
/// direction it went.
pub fn change_counts<'a, Message: 'a>(counts: &str) -> cosmic::Element<'a, Message> {
    let tinted = |text: &str, style: fn(&cosmic::Theme) -> cosmic::iced::widget::text::Style| {
        caption(text.to_owned())
            .wrapping(cosmic::iced::widget::text::Wrapping::None)
            .class(cosmic::theme::Text::Custom(style))
    };
    match counts.split_once(' ') {
        Some((added, removed)) => cosmic::widget::row::with_children(vec![
            tinted(added, added_text).into(),
            tinted(removed, removed_text).into(),
        ])
        .spacing(GRID_UNIT)
        .align_y(cosmic::iced::Alignment::Center)
        .into(),
        None => tinted(
            counts,
            if counts.starts_with('+') {
                added_text
            } else {
                removed_text
            },
        )
        .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_sit_a_fifth_closer_to_their_surface_and_stay_opaque() {
        use cosmic::iced::Color;
        let close = |a: Color, b: Color| {
            [a.r - b.r, a.g - b.g, a.b - b.b, a.a - b.a]
                .iter()
                .all(|delta| delta.abs() < 1e-6)
        };
        let opaque = softened(Color::BLACK, Color::WHITE, RULE_STRENGTH);
        assert!(close(opaque, Color::from_rgb(0.8, 0.8, 0.8)), "{opaque:?}");
        // A translucent divider is first painted on the surface it sits on.
        let translucent = softened(
            Color::BLACK,
            Color::from_rgba(1.0, 1.0, 1.0, 0.5),
            RULE_STRENGTH,
        );
        assert!(
            close(translucent, Color::from_rgb(0.4, 0.4, 0.4)),
            "{translucent:?}"
        );
    }

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

    /// Detailed budget bars share these thresholds, so a budget that has
    /// crossed one must not still be drawn as background.
    #[test]
    fn a_budget_changes_tone_where_it_becomes_a_decision() {
        // Resolvers are compared by address, because the tone is a function of
        // the fraction and a theme is not available in a unit test.
        let tone = |used: f32| budget_tone(used) as *const () as usize;
        let accent =
            accent_color as fn(&cosmic::Theme) -> cosmic::iced::Color as *const () as usize;
        let warning =
            warning_color as fn(&cosmic::Theme) -> cosmic::iced::Color as *const () as usize;
        let destructive =
            destructive_color as fn(&cosmic::Theme) -> cosmic::iced::Color as *const () as usize;
        assert_eq!(tone(0.0), accent);
        assert_eq!(tone(BUDGET_WARNING), accent);
        assert_eq!(tone(BUDGET_WARNING + 0.01), warning);
        assert_eq!(tone(BUDGET_CRITICAL), warning);
        assert_eq!(tone(BUDGET_CRITICAL + 0.01), destructive);
        assert_eq!(tone(1.0), destructive);
    }
}
