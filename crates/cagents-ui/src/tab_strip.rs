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
    fn view_tab_click_survives_rebuild_between_press_and_release_inside_horizontal_strip() {
        use cosmic::iced::advanced::{Shell, clipboard};
        use cosmic::iced::{Event, Point, mouse};
        fn tabs() -> Element<'static, u8> {
            horizontal(
                widget::Id::new("fixture-native-tabs"),
                vec![
                    crate::ui_controls::view_tab(
                        crate::ui_controls::ViewTab {
                            label: "Conversation".into(),
                            description: "Main conversation".into(),
                            count: None,
                            selected: true,
                            id: None,
                            on_press: Some(0),
                        },
                        true,
                        None,
                    ),
                    crate::ui_controls::view_tab(
                        crate::ui_controls::ViewTab {
                            label: "Native · Reviewer".into(),
                            description: "Native child".into(),
                            count: None,
                            selected: false,
                            id: None,
                            on_press: Some(1),
                        },
                        true,
                        Some(crate::ui_controls::passive_icon(
                            crate::ui_controls::Icon::AgentWaiting,
                            design::COMPACT_ICON_SIZE,
                        )),
                    ),
                ],
                crate::ui_controls::view_tab_height(true),
                crate::ui_controls::view_tab_height(true) + design::BORDER_WIDTH,
            )
        }
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut view = tabs();
        let mut tree = Tree::new(view.as_widget());
        let limits = layout::Limits::new(Size::ZERO, Size::new(600.0, 800.0));
        let mut node = view
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits)
            .move_to(Point::new(0.0, 500.0));
        let accessible =
            view.as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let bounds = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .find(|node| node.node().label() == Some("Native child"))
            .and_then(|node| node.node().bounds())
            .expect("native child button is mounted");
        let pointer = Point::new(
            (bounds.x0 + bounds.x1) as f32 / 2.0,
            (bounds.y0 + bounds.y1) as f32 / 2.0,
        );
        let viewport = Rectangle::with_size(Size::new(600.0, 800.0));
        let mut messages = Vec::new();
        for event in [
            Event::Mouse(mouse::Event::CursorMoved { position: pointer }),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        ] {
            view.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(pointer),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        assert!(messages.is_empty());
        view = tabs();
        tree.diff(view.as_widget_mut());
        node = view
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits)
            .move_to(Point::new(0.0, 500.0));
        view.as_widget_mut().update(
            &mut tree,
            &Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)),
            Layout::new(&node),
            mouse::Cursor::Available(pointer),
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        assert_eq!(
            messages,
            vec![1],
            "a native child tab remains clickable across the redraw caused by pressing it"
        );
    }

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
