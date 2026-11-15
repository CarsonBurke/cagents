//! A label with something that must stay beside it, such as a duration or
//! a picker's chevron.
//!
//! In a row sized to its contents, iced lays out each child in turn with the
//! room the earlier ones left, so a long label takes the whole width and
//! pushes whatever follows it out of view. This lays out the trailing element
//! first and gives the label what is left, while the pair still shrinks to a
//! short label rather than stretching across its row.

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
        children: [lead.into(), trail.into()],
        spacing,
    })
}

struct Trailing<'a, Message> {
    children: [Element<'a, Message>; 2],
    spacing: f32,
}

impl<Message> Widget<Message, Theme, Renderer> for Trailing<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Shrink, Length::Shrink)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
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
