//! Single-line text entry in the shell's own shape.
//!
//! COSMIC's inputs are tall, heavily bordered and, for search, fully rounded.
//! A field here is one dense row high with the same corner as the buttons
//! beside it and a hairline edge, so a form reads as one column of controls.

use std::borrow::Cow;

use cosmic::{
    Element,
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

/// The trailing control that empties a field, for `.trailing_icon(..)`.
///
/// COSMIC's own `on_clear` button is taller than a dense row and a field
/// grows to fit its trailing icon, so attaching it made a search field jump
/// in height on the first keystroke. This one fits inside the line box.
pub fn clear<'a, Message: Clone + 'a>(on_clear: Message) -> Element<'a, Message> {
    widget::button::custom(
        widget::container(widget::icon(Icon::Clear.handle()).size(design::COMPACT_ICON_SIZE))
            .class(design::transparent())
            .center(Length::Fill),
    )
    .name("Clear")
    .class(super::control_style(false, None))
    .width(LINE_HEIGHT)
    .height(LINE_HEIGHT)
    .padding(0)
    .on_press(on_clear)
    .into()
}

/// A choice among a few named options, at the shell's type size.
pub fn select<'a, S, Message>(
    options: impl Into<Cow<'a, [S]>>,
    selected: Option<usize>,
    on_select: impl Fn(usize) -> Message + Send + Sync + 'static,
) -> widget::Dropdown<'a, S, Message, Message>
where
    S: AsRef<str> + Clone + Send + Sync + 'static,
    [S]: ToOwned,
    Message: Clone + 'static,
{
    widget::dropdown(options, selected, on_select)
        .text_size(design::TEXT_BODY)
        .padding([INSET_Y, space::S])
}

/// A labelled yes/no choice. Add the label with `.label(..)`.
pub fn checkbox<'a, Message>(checked: bool) -> widget::Checkbox<'a, Message, cosmic::Theme> {
    widget::checkbox(checked)
        .size(design::ROW_ICON_SIZE)
        .text_size(design::TEXT_BODY)
        .spacing(space::S)
}

/// A setting that is on or off and takes effect at once. Add the label with
/// `.label(..)`.
pub fn toggle<'a, Message>(on: bool) -> widget::Toggler<'a, Message> {
    widget::toggler(on)
        .size(design::STATUS_ICON_SIZE)
        .text_size(design::TEXT_BODY)
        .spacing(space::M)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_search_field_stays_one_dense_row_high_with_its_clear_control() {
        use cosmic::iced::Size;
        use cosmic::iced::advanced::{layout, widget::Tree};

        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        for query in ["", "query"] {
            let mut field = search::<()>("Search", query);
            if !query.is_empty() {
                field = field.trailing_icon(clear(()));
            }
            let mut element: Element<'_, ()> = field.into();
            let mut tree = Tree::new(element.as_widget());
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(240.0, 200.0)),
            );
            assert_eq!(node.size().height, design::DENSE_ROW_HEIGHT, "{query:?}");
        }
    }
}
