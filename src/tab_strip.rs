//! Shared horizontal tab chrome used by cagents and ceditor.

use cosmic::iced::Length;
use cosmic::iced::widget::scrollable;
use cosmic::{Element, widget};

use crate::design;

/// Gap between adjacent tabs. A caller computing a reveal offset needs the
/// same pitch the strip lays out with.
pub const SPACING: f32 = 4.0;

/// Inset between the strip's viewport and each end of the bar holding it.
///
/// A caller that sizes the scroll viewport has to subtract this from both
/// ends, so the number lives here rather than being spelled again at the
/// call site where the two copies could drift apart.
pub const EDGE_INSET: f32 = 8.0;

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
    strip(id, tab_row, row_height, header_height, None, None)
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
        None,
    )
}

type OnScroll<'a, Message> = Box<dyn Fn(scrollable::Viewport) -> Message + 'a>;

pub struct Reordering<'a, Message> {
    pub order: Vec<crate::model::CardId>,
    pub tab_width: f32,
    pub enabled: bool,
    pub select: Box<dyn Fn(crate::model::CardId) -> Message + 'a>,
    pub dropped: Box<dyn Fn(crate::model::CardId, Option<crate::model::CardId>) -> Message + 'a>,
}

pub fn reorderable<'a, Message: 'a>(
    id: widget::Id,
    tab_row: Vec<Element<'a, Message>>,
    row_height: f32,
    header_height: f32,
    on_scroll: Option<OnScroll<'a, Message>>,
    reordering: Reordering<'a, Message>,
) -> Element<'a, Message> {
    strip(
        id,
        tab_row,
        row_height,
        header_height,
        on_scroll,
        Some(reordering),
    )
}

fn strip<'a, Message: 'a>(
    id: widget::Id,
    tab_row: Vec<Element<'a, Message>>,
    row_height: f32,
    header_height: f32,
    on_scroll: Option<OnScroll<'a, Message>>,
    reordering: Option<Reordering<'a, Message>>,
) -> Element<'a, Message> {
    let row = widget::row::with_children(tab_row)
        .spacing(SPACING)
        .align_y(cosmic::iced::Alignment::Center)
        .height(row_height);
    let row = match reordering {
        Some(config) => crate::reorder::strip(
            row,
            config
                .order
                .into_iter()
                .enumerate()
                .map(|(index, id)| crate::reorder::Item {
                    id,
                    start: index as f32 * (config.tab_width + SPACING),
                    width: config.tab_width,
                })
                .collect(),
            false,
            config.enabled,
            None,
            config.select,
            config.dropped,
        ),
        None => row.into(),
    };
    let mut tabs = widget::scrollable::horizontal(row)
        .id(id)
        // No scrollbar rail: the thumb is suppressed outright, so there is nothing
        // for a tab to be overlapped by. `overflow_fade_x` is what tells the
        // reader that the strip continues past the edge, and it needs the tabs to
        // reach that edge to fade them.
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
    .padding([0.0, EDGE_INSET])
    .center_y(Length::Fill)
    // Callers pass the nominal header height; the bar that receives the strip
    // spends `BORDER_WIDTH` of it on the rule beneath. Reserving it here keeps
    // the strip from standing one pixel taller than the bar containing it.
    .height((header_height - design::BORDER_WIDTH).max(0.0))
    .width(Length::Fill)
    .into()
}
