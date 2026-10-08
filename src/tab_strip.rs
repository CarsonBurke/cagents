//! Shared horizontal tab chrome used by cagents and ceditor.

use cosmic::iced::Length;
use cosmic::iced::widget::scrollable;
use cosmic::{Element, widget};

use crate::design;

/// Gap between adjacent tabs. A caller computing a reveal offset needs the
/// same pitch the strip lays out with.
pub const SPACING: f32 = 4.0;

/// Content inset at each end of the scrolling row. It scrolls with the tabs;
/// reveal and reorder coordinates include the leading inset.
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
        .padding([0.0, EDGE_INSET])
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
                    start: EDGE_INSET + index as f32 * (config.tab_width + SPACING),
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
    .center_y(Length::Fill)
    // Callers pass the nominal header height; the bar that receives the strip
    // spends `BORDER_WIDTH` of it on the rule beneath. Reserving it here keeps
    // the strip from standing one pixel taller than the bar containing it.
    .height((header_height - design::BORDER_WIDTH).max(0.0))
    .width(Length::Fill)
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::advanced::{
        Layout, layout,
        widget::{Operation, Tree, operation},
    };
    use cosmic::iced::{Rectangle, Size, Vector};

    #[test]
    fn horizontal_viewport_reaches_strip_edges_and_inset_belongs_to_content() {
        #[derive(Default)]
        struct Probe {
            bounds: Option<(Rectangle, Rectangle)>,
        }
        impl Operation for Probe {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                _: Option<&widget::Id>,
                bounds: Rectangle,
                content: Rectangle,
                _: Vector,
                _: &mut dyn operation::Scrollable,
            ) {
                self.bounds = Some((bounds, content));
            }
        }
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut strip: Element<'_, ()> = horizontal(
            widget::Id::unique(),
            vec![widget::Space::new().width(500).height(24).into()],
            24.0,
            32.0,
        );
        let mut tree = Tree::new(strip.as_widget());
        let node = strip.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(300.0, 32.0)),
        );
        let mut probe = Probe::default();
        strip
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
        let (viewport, content) = probe.bounds.expect("scrollable tab strip");
        assert_eq!(viewport.x, 0.0);
        assert_eq!(viewport.width, 300.0);
        assert_eq!(content.width, 500.0 + 2.0 * EDGE_INSET);
    }
}
