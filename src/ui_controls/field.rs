//! Single-line text entry in the shell's own shape.
//!
//! COSMIC's inputs are tall, heavily bordered and, for search, fully rounded.
//! A field here is one dense row high with the same corner as the buttons
//! beside it and a hairline edge, so a form reads as one column of controls.

use std::borrow::Cow;

use cosmic::{
    iced::{self, Length},
    widget,
};

use super::Icon;
use crate::design::{self, space};

/// The line box of a field's text, which with [`INSET_Y`] makes the field one
/// dense row high.
const LINE_HEIGHT: f32 = 18.0;
const INSET_Y: f32 = (design::DENSE_ROW_HEIGHT - LINE_HEIGHT) / 2.0;

/// A single-line text field.
///
/// Returns COSMIC's input so callers keep its whole builder: `on_input`,
/// `on_submit`, `id`, `width`.
pub fn text<'a, Message: Clone + 'static>(
    placeholder: impl Into<Cow<'a, str>>,
    value: impl Into<Cow<'a, str>>,
) -> widget::TextInput<'a, Message> {
    widget::text_input(placeholder, value)
        .size(design::TEXT_BODY)
        .line_height(iced::widget::text::LineHeight::Absolute(LINE_HEIGHT.into()))
        .padding([INSET_Y, space::S])
        .style(style())
}

/// A field that filters or finds: a [`text`] field led by the search glyph.
pub fn search<'a, Message: Clone + 'static>(
    placeholder: impl Into<Cow<'a, str>>,
    value: impl Into<Cow<'a, str>>,
) -> widget::TextInput<'a, Message> {
    text(placeholder, value)
        .padding([INSET_Y, space::S, INSET_Y, space::XS])
        .leading_icon(
            widget::container(
                widget::icon(Icon::Search.handle()).size(design::TAB_STATUS_ICON_SIZE),
            )
            .center_x(Length::Fixed(design::DENSE_ROW_HEIGHT - space::XS))
            .into(),
        )
}

fn style() -> cosmic::theme::TextInput {
    use cosmic::widget::text_input::Appearance;

    #[derive(Clone, Copy)]
    enum Status {
        Active,
        Hovered,
        Focused,
        Error,
        Disabled,
    }
    let appearance = |status: Status, theme: &cosmic::Theme| {
        let container = theme.current_container();
        let surface: iced::Color = container.base.into();
        let on: iced::Color = container.on.into();
        let cosmic = theme.cosmic();
        Appearance {
            // A field is a well in the surface rather than a raised control:
            // it holds the user's text, it is not pressed.
            background: iced::Background::Color(match status {
                Status::Disabled => surface,
                _ => design::mix(surface, on, 0.04),
            }),
            border_radius: design::RADIUS_S.into(),
            border_offset: None,
            border_width: design::BORDER_WIDTH,
            border_color: match status {
                Status::Focused => design::accent_color(theme),
                Status::Error => design::destructive_color(theme),
                Status::Hovered => design::mix(surface, on, 0.24),
                Status::Active | Status::Disabled => design::hairline(theme),
            },
            label_color: on,
            placeholder_color: design::mix(surface, on, 0.5),
            icon_color: Some(design::muted_icon_color(theme)),
            text_color: Some(match status {
                Status::Disabled => design::mix(surface, on, 0.42),
                _ => on,
            }),
            selected_text_color: cosmic.on_accent_color().into(),
            selected_fill: cosmic.accent_color().into(),
        }
    };
    cosmic::theme::TextInput::Custom {
        active: Box::new(move |theme| appearance(Status::Active, theme)),
        error: Box::new(move |theme| appearance(Status::Error, theme)),
        hovered: Box::new(move |theme| appearance(Status::Hovered, theme)),
        focused: Box::new(move |theme| appearance(Status::Focused, theme)),
        disabled: Box::new(move |theme| appearance(Status::Disabled, theme)),
    }
}
