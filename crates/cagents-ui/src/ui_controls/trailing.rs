//! A label with something that must stay beside it, such as a duration or
//! a picker's chevron.
//!
//! In a row sized to its contents, iced lays out each child in turn with the
//! room the earlier ones left, so a long label takes the whole width and
//! pushes whatever follows it out of view. This lays out the trailing element
//! first and gives the label what is left, while the pair still shrinks to a
//! short label rather than stretching across its row.
//!
//! [`shared_line`] is the same for a line whose label and trailing element may
//! both run long, such as a name and a status: they split the room between
//! them, and what sits between them has only what both leave.

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree},
};
use cosmic::iced::{Event, Length, Point, Rectangle, Size, Vector};
use cosmic::{Element, Renderer, Theme};

/// `lead`, held to the room `trail` leaves it, followed by `trail`, the two
/// centred on one another vertically. Give `lead` a
/// [`FadeText`](super::FadeText), which fades where it is held short.
pub fn trailing<'a, Message: 'a>(
    lead: impl Into<Element<'a, Message>>,
    trail: impl Into<Element<'a, Message>>,
    spacing: f32,
) -> Element<'a, Message> {
    Element::new(Trailing {
        children: vec![lead.into(), trail.into()],
        spacing,
        shared: false,
    })
}

/// `lead` at the line's start and `trail` at its end, across the room they
/// are given, with `middle` filling what they leave between them.
///
/// Each of `lead` and `trail` keeps its own width where both fit. Where they
/// do not, neither takes the other's half: one narrower than half keeps its
/// width and the other has the rest, and two wider than half are held to
/// half each. Give both a [`FadeText`](super::FadeText), which fades where it
/// is held short, and `middle` what the line can do without.
pub fn shared_line<'a, Message: 'a>(
    lead: impl Into<Element<'a, Message>>,
    middle: impl Into<Element<'a, Message>>,
    trail: impl Into<Element<'a, Message>>,
    spacing: f32,
) -> Element<'a, Message> {
    Element::new(Trailing {
        children: vec![lead.into(), middle.into(), trail.into()],
        spacing,
        shared: true,
    })
}

/// How [`shared_line`] splits `room` between a lead and a trail that would like
/// `lead` and `trail` of it, with `spacing` between them.
fn split(room: f32, lead: f32, trail: f32, spacing: f32) -> (f32, f32) {
    let room = (room - spacing).max(0.0);
    let half = room / 2.0;
    if lead + trail <= room {
        (lead, trail)
    } else if lead <= half {
        (lead, room - lead)
    } else if trail <= half {
        (room - trail, trail)
    } else {
        (half, half)
    }
}

struct Trailing<'a, Message> {
    children: Vec<Element<'a, Message>>,
    spacing: f32,
    /// Laid out by [`shared_line`], with a middle child.
    shared: bool,
}

impl<Message> Trailing<'_, Message> {
    fn layout_shared(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let max = limits.max();
        let compression = limits.compression();
        let mut lay = |index: usize, width: f32| {
            self.children[index].as_widget_mut().layout(
                &mut tree.children[index],
                renderer,
                &layout::Limits::with_compression(
                    Size::ZERO,
                    Size::new(width, max.height),
                    compression,
                ),
            )
        };
        // What each end would take with all the room it wants; unwrapped
        // text only re-flows for that, it is not shaped again.
        let wanted = |node: layout::Node| node.size().width;
        let lead_wants = wanted(lay(0, f32::INFINITY));
        let trail_wants = wanted(lay(2, f32::INFINITY));
        let (lead_room, trail_room) = split(max.width, lead_wants, trail_wants, self.spacing);
        let lead = lay(0, lead_room);
        let trail = lay(2, trail_room);
        // However wide an end reports itself, it ends where its room does.
        let lead_width = lead.size().width.min(lead_room);
        let trail_width = trail.size().width.min(trail_room);
        let middle_x = lead_width + self.spacing;
        let middle_room = (max.width - trail_width - self.spacing - middle_x).max(0.0);
        let middle = lay(1, middle_room);
        let height = [&lead, &middle, &trail]
            .iter()
            .map(|node| node.size().height)
            .fold(0.0, f32::max);
        let size = limits.resolve(Length::Fill, Length::Shrink, Size::new(max.width, height));
        let centred = |node: &layout::Node| (size.height - node.size().height) / 2.0;
        let (lead_y, middle_y, trail_y) = (centred(&lead), centred(&middle), centred(&trail));
        layout::Node::with_children(
            size,
            vec![
                lead.move_to(Point::new(0.0, lead_y)),
                middle.move_to(Point::new(middle_x, middle_y)),
                trail.move_to(Point::new(size.width - trail_width, trail_y)),
            ],
        )
    }
}

impl<Message> Widget<Message, Theme, Renderer> for Trailing<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
    }

    fn size(&self) -> Size<Length> {
        let width = if self.shared {
            Length::Fill
        } else {
            Length::Shrink
        };
        Size::new(width, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        if self.shared {
            return self.layout_shared(tree, renderer, limits);
        }
        let max = limits.max();
        let compression = limits.compression();
        let trail = self.children[1].as_widget_mut().layout(
            &mut tree.children[1],
            renderer,
            &layout::Limits::with_compression(Size::ZERO, max, compression),
        );
        let room = (max.width - trail.size().width - self.spacing).max(0.0);
        let lead = self.children[0].as_widget_mut().layout(
            &mut tree.children[0],
            renderer,
            &layout::Limits::with_compression(Size::ZERO, Size::new(room, max.height), compression),
        );
        // However wide the lead reports itself, the trail starts where the
        // room given to the lead ends at the latest.
        let lead_width = lead.size().width.min(room);
        let trail_x = lead_width + self.spacing;
        let height = lead.size().height.max(trail.size().height);
        let size = limits.resolve(
            Length::Shrink,
            Length::Shrink,
            Size::new(trail_x + trail.size().width, height),
        );
        let centred = |node: &layout::Node| (size.height - node.size().height) / 2.0;
        let lead_y = centred(&lead);
        let trail_y = centred(&trail);
        layout::Node::with_children(
            size,
            vec![
                lead.move_to(Point::new(0.0, lead_y)),
                trail.move_to(Point::new(trail_x, trail_y)),
            ],
        )
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.traverse(&mut |operation| {
            for ((child, state), child_layout) in self
                .children
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                child.as_widget_mut().operate(
                    state,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    renderer,
                    operation,
                );
            }
        });
    }

    fn update(
        &mut self,
        tree: &mut Tree,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        for ((child, state), child_layout) in self
            .children
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            child.as_widget_mut().update(
                state,
                event,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                cursor,
                renderer,
                clipboard,
                shell,
                viewport,
            );
        }
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        for ((child, state), child_layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child.as_widget().draw(
                state,
                renderer,
                theme,
                style,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                cursor,
                viewport,
            );
        }
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .map(|((child, state), child_layout)| {
                child.as_widget().mouse_interaction(
                    state,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    cursor,
                    viewport,
                    renderer,
                )
            })
            .max()
            .unwrap_or_default()
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        overlay::from_children(
            &mut self.children,
            tree,
            layout,
            renderer,
            viewport,
            translation,
        )
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        tree: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        iced_accessibility::A11yTree::join(
            self.children
                .iter()
                .zip(&tree.children)
                .zip(layout.children())
                .map(|((child, state), child_layout)| {
                    child.as_widget().a11y_nodes(
                        child_layout.with_virtual_offset(layout.virtual_offset()),
                        state,
                        cursor,
                    )
                }),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use cosmic::widget;

    fn fixed<'a>(width: f32, height: f32) -> Element<'a, ()> {
        widget::Space::new().width(width).height(height).into()
    }

    /// Lays out `lead` and a 40-wide trail in a row sized to its contents,
    /// as a group's header is, `available` wide: the lead's and the trail's
    /// bounds.
    fn header(lead: Element<'static, ()>, available: f32) -> (Rectangle, Rectangle) {
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let mut element: Element<'_, ()> = widget::container(
            widget::row::with_children(vec![
                fixed(16.0, 16.0),
                trailing(lead, fixed(40.0, 16.0), 8.0),
            ])
            .spacing(8.0),
        )
        .width(Length::Shrink)
        .into();
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(available, 600.0)),
        );
        let row = Layout::new(&node).children().next().unwrap();
        let pair = row.children().nth(1).unwrap();
        let mut parts = pair.children().map(|part| part.bounds());
        (parts.next().unwrap(), parts.next().unwrap())
    }

    /// A heading wider than the pane, even one that reports more than the
    /// room it was given, leaves the trail in view at the pane's edge.
    #[test]
    fn a_long_heading_leaves_its_trail_in_view() {
        for available in [200.0_f32, 320.0, 900.0] {
            for lead in [
                widget::container(fixed(5000.0, 16.0))
                    .width(Length::Fill)
                    .clip(true)
                    .into(),
                fixed(5000.0, 16.0),
            ] {
                let (lead, trail) = header(lead, available);
                assert!(lead.x + lead.width <= trail.x - 8.0 + 0.01, "{available}");
                assert_eq!(trail.x + trail.width, available, "{available}");
            }
        }
    }

    /// The ends of a shared line split the room: each keeps its width where
    /// both fit or it is the narrower, and neither takes the other's half.
    #[test]
    fn a_shared_line_splits_its_room_between_its_ends() {
        assert_eq!(split(300.0, 100.0, 100.0, 10.0), (100.0, 100.0));
        assert_eq!(split(210.0, 60.0, 300.0, 10.0), (60.0, 140.0));
        assert_eq!(split(210.0, 300.0, 60.0, 10.0), (140.0, 60.0));
        assert_eq!(split(210.0, 300.0, 300.0, 10.0), (100.0, 100.0));
        assert_eq!(split(0.0, 300.0, 300.0, 10.0), (0.0, 0.0));
    }

    /// A shared line fills its room, keeps its trail at the end and its
    /// middle between the ends, and gives the middle only what they leave.
    #[test]
    fn a_shared_line_keeps_its_trail_at_the_end_and_its_middle_between() {
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let laid = |lead: f32, trail: f32, available: f32| {
            let mut element: Element<'_, ()> = shared_line(
                fixed(lead, 16.0),
                widget::Space::new().width(Length::Fill).height(16.0),
                fixed(trail, 16.0),
                8.0,
            );
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(available, 600.0)),
            );
            let parts: Vec<_> = Layout::new(&node)
                .children()
                .map(|part| part.bounds())
                .collect();
            (node.size().width, parts)
        };
        let (width, parts) = laid(60.0, 40.0, 400.0);
        assert_eq!(width, 400.0);
        assert_eq!(parts[0].width, 60.0);
        assert_eq!(parts[1].x, 68.0);
        assert_eq!(parts[1].width, 400.0 - 60.0 - 40.0 - 16.0);
        assert_eq!(parts[2].x + parts[2].width, 400.0);
        // Too narrow for both: the middle is gone and the ends share.
        let (_, parts) = laid(5000.0, 5000.0, 208.0);
        assert!(parts[0].width <= 100.0, "{parts:?}");
        assert_eq!(parts[1].width, 0.0);
        assert_eq!(parts[2].x + parts[2].width, 208.0);
        assert!(
            parts[0].x + parts[0].width <= parts[2].x - 8.0 + 0.01,
            "{parts:?}"
        );
    }

    /// A short heading keeps the pair to its own width, so the header does
    /// not stretch across the pane.
    #[test]
    fn a_short_heading_keeps_its_trail_beside_it() {
        let (lead, trail) = header(fixed(60.0, 16.0), 900.0);
        assert_eq!(lead.width, 60.0);
        assert_eq!(trail.x, lead.x + 60.0 + 8.0);
        assert!(trail.x + trail.width < 200.0);
    }
}
