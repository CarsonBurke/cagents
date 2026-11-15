//! Small read-only pieces of a row or a card: status pills, change counts,
//! and the frame around a review comment.

use std::borrow::Cow;

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
    widget::container(
        caption(label)
            .wrapping(iced::widget::text::Wrapping::None)
            .class(cosmic::theme::Text::Default),
    )
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
