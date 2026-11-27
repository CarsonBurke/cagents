//! Small read-only pieces of a row or a card: status pills, change counts,
//! and the frame around a review comment.

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use cosmic::{
    Element, Theme,
    iced::{self, Alignment, Length},
    widget,
};

use crate::design::{self, Tone, caption, heading, space};

/// A small status pill.
///
/// A condition with a name -- open, merged, failing, resolved -- is not another
/// sentence at the same weight as the column around it. Giving it a shape and a
/// tone lets the eye find the state before it reads any of the words, which is
/// the whole reason the state is on the row.
pub fn badge<'a, Message: 'a>(
    label: impl Into<Cow<'a, str>> + 'a,
    tone: Tone,
) -> Element<'a, Message> {
    badge_with_width(label.into(), tone, None)
}

/// A status pill constrained to its table cell. The caller supplies the full
/// label through a tooltip or accessible name when the caption is shortened.
pub fn bounded_badge<'a, Message: 'a>(
    label: impl Into<Cow<'a, str>> + 'a,
    tone: Tone,
    width: f32,
) -> Element<'a, Message> {
    badge_with_width(label.into(), tone, Some(width.max(0.0)))
}

fn badge_with_width<'a, Message: 'a>(
    label: Cow<'a, str>,
    tone: Tone,
    width: Option<f32>,
) -> Element<'a, Message> {
    // In the pill's own colour; one that is held to its cell fades there.
    let text: Element<'a, Message> = if width.is_some() {
        crate::ui_controls::FadeText::caption(label)
            .class(cosmic::theme::Text::Default)
            .into()
    } else {
        caption(label)
            .wrapping(iced::widget::text::Wrapping::None)
            .class(cosmic::theme::Text::Default)
            .into()
    };
    let pill = widget::container(text)
        .padding(iced::Padding {
            top: 1.0,
            right: space::XS + space::XXS,
            bottom: 1.0,
            left: space::XS + space::XXS,
        })
        .class(cosmic::theme::Container::custom(move |theme| {
            let color = tone.resolve(theme);
            let mut wash = color;
            // Strong enough to read as a filled pill, weak enough that the label
            // on top keeps its contrast at either theme polarity.
            wash.a *= 0.18;
            widget::container::Style {
                background: Some(iced::Background::Color(wash)),
                text_color: Some(color),
                border: iced::Border {
                    radius: design::RADIUS_XS.into(),
                    ..Default::default()
                },
                ..Default::default()
            }
        }));
    match width {
        Some(width) => pill.max_width(width).clip(true).into(),
        None => pill.into(),
    }
}

/// What a [`count_badge`]'s count says about its section, and so its tone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CountTone {
    /// How many there are, and nothing more.
    #[default]
    Neutral,
    /// Some of them want looking at, such as warnings.
    Warning,
    /// Some of them are failures, such as errors.
    Danger,
    /// On an accent-filled control, such as a primary button, whose fill it
    /// shades rather than adding a hue of its own.
    OnAccent,
    /// News the user has not read yet, such as a sidebar row's unread
    /// conversations: filled with the accent, the one count loud enough to
    /// find in a column of names. The projects rail's unread pill wears the
    /// same fill and ink.
    Accent,
}

/// The ink of a count or figure drawn on the theme's accent fill.
pub fn on_accent_color(theme: &Theme) -> iced::Color {
    theme.cosmic().accent.on.into()
}

/// The height of a [`count_badge`], and the width of a one-figure one.
pub const COUNT_BADGE_HEIGHT: f32 = 16.0;
/// A [`count_badge`]'s inset either side of its figures.
const COUNT_BADGE_PADDING: f32 = 5.0;
/// The widest any figure or `+` is drawn in a [`count_badge`], with room to
/// spare: the font's figures are about 0.6 em at its 10.5 px.
const COUNT_BADGE_FIGURE_WIDTH: f32 = 7.5;

/// The most room a [`count_badge`] showing `count` takes, for a layout that
/// must clear the badge before it is drawn, such as a hover-revealed control
/// laid over it.
pub fn count_badge_width_bound(count: &str) -> f32 {
    match count.chars().count() {
        0 | 1 => COUNT_BADGE_HEIGHT,
        figures => 2.0 * COUNT_BADGE_PADDING + figures as f32 * COUNT_BADGE_FIGURE_WIDTH,
    }
}

/// How many items a section, tab or list holds, as a small rounded badge
/// beside its name.
///
/// A bare number after a heading reads as part of the heading, or as stray
/// text; a filled pill reads at once as a count. Counts beside a name use
/// this one shape, so a reader learns it once; the projects rail's and
/// avatars' overlaid counts keep their own ringed pills. Callers say the
/// count in the accessible name of the control it labels.
///
/// Leave the badge out at zero when the body beneath already says it is
/// empty, as an empty Work board lane does; show "0" when the zero is the
/// news, as for a search that found nothing.
///
/// A single figure is a circle, whatever its glyph's width, so a column of
/// one-digit counts lines up; longer counts grow into a pill.
pub fn count_badge<'a, Message: 'a>(
    count: impl Into<Cow<'a, str>> + 'a,
    tone: CountTone,
) -> Element<'a, Message> {
    let count = count.into();
    let width = if count.chars().count() <= 1 {
        Length::Fixed(COUNT_BADGE_HEIGHT)
    } else {
        Length::Shrink
    };
    widget::container(
        widget::text(count)
            .size(design::TEXT_CAPTION - 0.5)
            .line_height(iced::widget::text::LineHeight::Absolute(
                COUNT_BADGE_HEIGHT.into(),
            ))
            .font(cosmic::font::semibold())
            .wrapping(iced::widget::text::Wrapping::None),
    )
    .padding([0.0, COUNT_BADGE_PADDING])
    .center_y(Length::Fixed(COUNT_BADGE_HEIGHT))
    .align_x(Alignment::Center)
    .width(width)
    .class(cosmic::theme::Container::custom(move |theme| {
        count_badge_style(theme, tone)
    }))
    .into()
}

fn count_badge_style(theme: &Theme, tone: CountTone) -> widget::container::Style {
    let wash = |mut color: iced::Color, strength: f32| {
        color.a *= strength;
        color
    };
    let on: iced::Color = theme.current_container().on.into();
    let (fill, ink) = match tone {
        // A wash of the text colour, so the pill sits on any surface it is
        // drawn over, hovered and selected rows included, at either polarity.
        // Its figure is no louder than the muted name beside it.
        CountTone::Neutral => (wash(on, 0.12), Some(design::muted_color(theme))),
        // A light wash keeps the tone's ink readable on grey surfaces too.
        CountTone::Warning => {
            let color = design::warning_color(theme);
            (wash(color, 0.12), Some(color))
        }
        CountTone::Danger => {
            let color = design::destructive_color(theme);
            (wash(color, 0.12), Some(color))
        }
        // The control's own label colour carries through.
        CountTone::OnAccent => (iced::Color::from_rgba(0.0, 0.0, 0.0, 0.2), None),
        CountTone::Accent => (design::accent_color(theme), Some(on_accent_color(theme))),
    };
    widget::container::Style {
        background: Some(iced::Background::Color(fill)),
        text_color: ink,
        border: iced::Border {
            radius: (COUNT_BADGE_HEIGHT / 2.0).into(),
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A `+12 −4` change count, with each side carrying its own semantic tone.
///
/// [`crate::repository::line_change_counts`] joins the two sides with a single
/// space and omits a side it measured as zero, so the position of that space
/// is all that is needed to tell the added part from the removed part. The
/// counts are the only place the shell states a magnitude of change, and a
/// monochrome `+141 −3` makes the reader parse the sign to learn which
/// direction it went.
pub fn change_counts<'a, Message: 'a>(counts: &str) -> Element<'a, Message> {
    let tinted = |text: &str, style: fn(&Theme) -> iced::widget::text::Style| {
        caption(text.to_owned())
            .wrapping(iced::widget::text::Wrapping::None)
            .class(cosmic::theme::Text::Custom(style))
    };
    match counts.split_once(' ') {
        Some((added, removed)) => widget::row::with_children(vec![
            tinted(added, design::added_text).into(),
            tinted(removed, design::removed_text).into(),
        ])
        .spacing(space::XS)
        .align_y(iced::Alignment::Center)
        .into(),
        None => tinted(
            counts,
            if counts.starts_with('+') {
                design::added_text
            } else {
                design::removed_text
            },
        )
        .into(),
    }
}

pub fn comment_card<'a, Message: 'a>(
    author: &str,
    created_at: &str,
    body: Element<'a, Message>,
    actions: Element<'a, Message>,
) -> Element<'a, Message> {
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
                    .class(design::surface(
                        Some(design::raised),
                        [design::RADIUS_M; 4],
                        false,
                    ))
                    .into(),
                heading(author.to_owned()).width(Length::Fill).into(),
                caption(time).into(),
                actions,
            ])
            .spacing(space::S)
            .align_y(Alignment::Center)
            .into(),
            body,
        ])
        .spacing(space::S)
        .width(Length::Fill),
    )
    .padding(space::S)
    .width(Length::Fill)
    .class(design::surface(
        Some(design::raised),
        [design::RADIUS_S; 4],
        true,
    ))
    .into()
}

/// A path as a person would write it, with the home directory as `~`.
///
/// Every path this application shows lives under the user's home, so the
/// absolute prefix is noise that costs the readable tail its space when the
/// label has to fade.
pub fn display_path(path: &Path) -> Cow<'_, str> {
    static HOME: LazyLock<Option<PathBuf>> =
        LazyLock::new(|| directories::UserDirs::new().map(|dirs| dirs.home_dir().to_owned()));
    let relative = HOME
        .as_deref()
        .and_then(|home| path.strip_prefix(home).ok())
        .filter(|relative| !relative.as_os_str().is_empty());
    match relative {
        Some(relative) => Cow::Owned(format!("~/{}", relative.display())),
        None => path.to_string_lossy(),
    }
}
