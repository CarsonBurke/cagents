//! Semantic color: every tone the shell paints, resolved from the active theme.
//!
//! COSMIC supplies the palette; these functions decide how much of it each
//! role takes. They are plain `fn(&Theme) -> Color` so a widget can name a
//! tone when it is built and resolve it from the theme its `draw` is handed.

use cosmic::{
    Theme,
    iced::{Color, widget::text},
};

pub const SCRIM_ALPHA: f32 = 0.48;
pub const ATTENTION_BORDER_ALPHA: f32 = 0.35;
pub const DIFF_GUTTER_ALPHA: f32 = 0.48;
pub const DIFF_DIVIDER_ALPHA: f32 = 0.16;

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
    Fixed(Color),
    /// A semantic color, read from the drawing theme.
    Themed(fn(&Theme) -> Color),
}

impl Tone {
    pub fn resolve(self, theme: &Theme) -> Color {
        match self {
            Self::Fixed(color) => color,
            Self::Themed(resolve) => resolve(theme),
        }
    }
}

impl From<Color> for Tone {
    fn from(color: Color) -> Self {
        Self::Fixed(color)
    }
}

impl From<fn(&Theme) -> Color> for Tone {
    fn from(resolve: fn(&Theme) -> Color) -> Self {
        Self::Themed(resolve)
    }
}

/// Mixes `over` into `under` by `amount`, ignoring `over`'s own alpha.
///
/// Compositing by alpha would leave the result translucent, so a widget drawn
/// over another translucent surface would darken twice. Resolving the mix to
/// an opaque color keeps a stack of surfaces predictable.
pub fn mix(under: Color, over: Color, amount: f32) -> Color {
    let keep = 1.0 - amount;
    Color {
        r: under.r * keep + over.r * amount,
        g: under.g * keep + over.g * amount,
        b: under.b * keep + over.b * amount,
        a: 1.0,
    }
}

/// The surface a pane draws on.
fn base(theme: &Theme) -> Color {
    theme.current_container().base.into()
}

/// A separator that reads as an edge rather than as a frame.
///
/// A full-strength border turns every element into a box. At this weight the
/// line only defines where one surface stops, which is all a border on a
/// transcript needs to do.
pub fn hairline(theme: &Theme) -> Color {
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
pub fn rule(theme: &Theme) -> Color {
    let container = theme.current_container();
    soften(theme, base(theme), container.divider.into())
}

/// `rule` for an edge drawn on a component surface such as a popover.
pub fn component_rule(theme: &Theme) -> Color {
    let component = &theme.current_container().component;
    soften(theme, component.base.into(), component.divider.into())
}

/// How strongly `theme` paints its rules. A high-contrast theme asked for its
/// edges at full strength, so it keeps them.
fn rule_strength(theme: &Theme) -> f32 {
    if theme.cosmic().is_high_contrast {
        1.0
    } else {
        RULE_STRENGTH
    }
}

fn soften(theme: &Theme, surface: Color, divider: Color) -> Color {
    softened(surface, divider, rule_strength(theme))
}

/// `divider` as painted on `surface`, moved back toward the surface by the
/// share of `strength` the rule gives up.
fn softened(surface: Color, divider: Color, strength: f32) -> Color {
    let painted = mix(surface, divider, divider.a);
    mix(surface, painted, strength)
}

/// One step above the surface: a code block, a composer, a notice.
pub fn raised(theme: &Theme) -> Color {
    mix(base(theme), theme.current_container().on.into(), 0.045)
}

/// One step below the surface, for content quoted from elsewhere.
pub fn sunken(theme: &Theme) -> Color {
    let on: Color = theme.current_container().on.into();
    // Darkening is only available where the surface has room to darken. On a
    // light theme the same intent is served by tinting toward the ink.
    if on.r + on.g + on.b > 1.5 {
        mix(base(theme), Color::BLACK, 0.28)
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
pub fn track(theme: &Theme) -> Color {
    mix(base(theme), theme.current_container().on.into(), 0.18)
}

/// The accent at reading strength: enough to claim a surface as the user's
/// own without turning their message into a colored block.
pub fn accent_wash(theme: &Theme) -> Color {
    mix(base(theme), theme.cosmic().accent.base.into(), 0.14)
}

pub fn accent_color(theme: &Theme) -> Color {
    theme.cosmic().accent.base.into()
}

/// The shell's "working" tone: something is in flight and may still fail.
pub fn warning_color(theme: &Theme) -> Color {
    theme.cosmic().warning.base.into()
}

pub fn success_color(theme: &Theme) -> Color {
    theme.cosmic().success.base.into()
}

pub fn destructive_color(theme: &Theme) -> Color {
    theme.cosmic().destructive.base.into()
}

/// Secondary labels stay readable without competing with active content.
///
/// The margin between this and full strength is deliberately small. At a
/// heavier mute the sidebar's own rows read as disabled controls, which is the
/// one thing a navigable row must not look like; `sidebar_button_style` keeps
/// its disabled alpha well below this so the two states stay distinguishable.
pub fn muted_color(theme: &Theme) -> Color {
    let mut color: Color = theme.current_container().on.into();
    color.a *= 0.88;
    color
}

fn ink(color: Color) -> text::Style {
    text::Style {
        color: Some(color),
        ..Default::default()
    }
}

pub fn muted_text(theme: &Theme) -> text::Style {
    ink(muted_color(theme))
}

/// Text that reports a failure the reader is expected to act on.
pub fn warning_text(theme: &Theme) -> text::Style {
    ink(warning_color(theme))
}

/// Text that reports an error the reader has to correct.
pub fn destructive_text(theme: &Theme) -> text::Style {
    ink(destructive_color(theme))
}

/// The gutter marker on an added or removed patch line.
///
/// The marker is tinted rather than the whole line, so the line keeps the
/// reader's ink while the gutter says what happened to it.
pub fn added_text(theme: &Theme) -> text::Style {
    ink(success_color(theme))
}

pub fn removed_text(theme: &Theme) -> text::Style {
    ink(destructive_color(theme))
}

/// Symbolic icons ignore alpha in their SVG recoloring. Blend them toward the
/// surface instead so muted navigation glyphs stay visibly quieter than text
/// while still carrying most of the foreground's contrast.
pub fn muted_icon_color(theme: &Theme) -> Color {
    mix(base(theme), theme.current_container().on.into(), 0.72)
}

/// A patch's added and removed lines, tinted rather than filled.
///
/// The semantic accent at full strength competes with the code it is meant to
/// annotate, so each tone is mixed toward the surface it draws on: strong
/// enough to read at a glance down the gutter, weak enough that the text on
/// top keeps its contrast.
fn diff_tone(theme: &Theme, accent: Color) -> Color {
    mix(base(theme), accent, 0.22)
}

pub fn diff_added_color(theme: &Theme) -> Color {
    diff_tone(theme, theme.cosmic().success.base.into())
}

pub fn diff_removed_color(theme: &Theme) -> Color {
    diff_tone(theme, theme.cosmic().destructive.base.into())
}

/// The wash an editor lays under a line the unsaved buffer has added.
///
/// Translucent rather than mixed toward a surface colour, because it is
/// painted over the editor's own background rather than over the container
/// [`diff_tone`] mixes against, and the glyphs are drawn on top of it: a fill
/// strong enough to be seen down the side of the buffer still has to leave
/// the code its contrast.
pub fn added_line_wash(theme: &Theme) -> Color {
    Color {
        a: 0.12,
        ..success_color(theme)
    }
}

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
pub fn budget_tone(used: f32) -> fn(&Theme) -> Color {
    if used > BUDGET_CRITICAL {
        destructive_color
    } else if used > BUDGET_WARNING {
        warning_color
    } else {
        accent_color
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_sit_a_fifth_closer_to_their_surface_and_stay_opaque() {
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

    /// Detailed budget bars share these thresholds, so a budget that has
    /// crossed one must not still be drawn as background.
    #[test]
    fn a_budget_changes_tone_where_it_becomes_a_decision() {
        // Resolvers are compared by address, because the tone is a function of
        // the fraction and a theme is not available in a unit test.
        let tone = |used: f32| budget_tone(used) as *const () as usize;
        let accent = accent_color as fn(&Theme) -> Color as *const () as usize;
        let warning = warning_color as fn(&Theme) -> Color as *const () as usize;
        let destructive = destructive_color as fn(&Theme) -> Color as *const () as usize;
        assert_eq!(tone(0.0), accent);
        assert_eq!(tone(BUDGET_WARNING), accent);
        assert_eq!(tone(BUDGET_WARNING + 0.01), warning);
        assert_eq!(tone(BUDGET_CRITICAL), warning);
        assert_eq!(tone(BUDGET_CRITICAL + 0.01), destructive);
        assert_eq!(tone(1.0), destructive);
    }
}
