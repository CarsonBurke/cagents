//! Shared horizontal tab chrome used by cagents and ceditor.

use cosmic::iced::Length;
use cosmic::iced::widget::scrollable;
use cosmic::{Element, widget};

use crate::design;

/// Gap between adjacent tabs. A caller computing a reveal offset needs the
/// same pitch the strip lays out with.
pub const SPACING: f32 = 4.0;

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
    strip(id, tab_row, row_height, header_height, None)
}

/// [`horizontal`], plus the scroll offset reported back on every change.
///
/// A caller that reveals a tab needs the offset it is revealing from: the
/// native scrollable only accepts absolute offsets, so without this a "reveal"
/// snaps an already-visible tab to the viewport edge.
pub fn horizontal_with_scroll<'a, Message: 'a>(
    id: widget::Id,
    tab_row: Vec<Element<'a, Message>>,
    row_height: f32,
    header_height: f32,
    on_scroll: impl Fn(scrollable::Viewport) -> Message + 'a,
) -> Element<'a, Message> {
    strip(
        id,
        tab_row,
        row_height,
        header_height,
        Some(Box::new(on_scroll)),
    )
}

type OnScroll<'a, Message> = Box<dyn Fn(scrollable::Viewport) -> Message + 'a>;

fn strip<'a, Message: 'a>(
    id: widget::Id,
    tab_row: Vec<Element<'a, Message>>,
    row_height: f32,
    header_height: f32,
    on_scroll: Option<OnScroll<'a, Message>>,
) -> Element<'a, Message> {
    let mut tabs = widget::scrollable::horizontal(
        widget::row::with_children(tab_row)
            .spacing(SPACING)
            .align_y(cosmic::iced::Alignment::Center)
            .height(row_height),
    )
    .id(id)
    .direction(scrollable::Direction::Horizontal(
        scrollable::Scrollbar::hidden(),
    ))
    .width(Length::Fill)
    .height(row_height);
    if let Some(on_scroll) = on_scroll {
        tabs = tabs.on_scroll(on_scroll);
    }
    widget::container(crate::interaction::overflow_fade_x(
        crate::interaction::horizontal_wheel(tabs),
    ))
    .padding([0.0, 8.0])
    .center_y(Length::Fill)
    // Callers pass the nominal header height; the bar that receives the strip
    // spends `BORDER_WIDTH` of it on the rule beneath. Reserving it here keeps
    // the strip from standing one pixel taller than the bar containing it.
    .height((header_height - design::BORDER_WIDTH).max(0.0))
    .width(Length::Fill)
    .into()
}
