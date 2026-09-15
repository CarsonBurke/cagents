//! Shared horizontal tab chrome used by cagents and ceditor.

use cosmic::iced::Length;
use cosmic::{Element, widget};

/// Wraps a tab so a middle click closes it without changing left-click
/// selection behavior.
pub fn middle_click_close<'a, Message: Clone + 'a>(
    content: impl Into<Element<'a, Message>>,
    close: Message,
) -> Element<'a, Message> {
    widget::mouse_area(content).on_middle_press(close).into()
}

/// Renders the shared horizontally scrolling tab strip.
pub fn horizontal<'a, Message: 'a>(
    id: widget::Id,
    tab_row: Vec<Element<'a, Message>>,
    row_height: f32,
    header_height: f32,
) -> Element<'a, Message> {
    widget::container(crate::interaction::overflow_fade_x(
        crate::interaction::horizontal_wheel(
            widget::scrollable::horizontal(
                widget::row::with_children(tab_row)
                    .spacing(4)
                    .align_y(cosmic::iced::Alignment::Center)
                    .height(row_height),
            )
            .id(id)
            .direction(cosmic::iced::widget::scrollable::Direction::Horizontal(
                cosmic::iced::widget::scrollable::Scrollbar::hidden(),
            ))
            .width(Length::Fill)
            .height(row_height),
        ),
    ))
    .padding([0.0, 8.0])
    .center_y(Length::Fill)
    .height(header_height)
    .width(Length::Fill)
    .into()
}
