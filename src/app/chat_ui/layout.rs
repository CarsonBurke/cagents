//! One reading column, determined by available space rather than its contents.
use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer,
    widget::{Operation, Tree},
};
use iced::{Event, Point, Rectangle, Size, Vector};

pub(super) fn pane<'a>(
    body: Element<'a, Message>,
    footer: Element<'a, Message>,
    preferred: f32,
) -> Element<'a, Message> {
    Element::new(Pane {
        children: [body, footer],
        preferred,
    })
}

struct Pane<'a> {
    children: [Element<'a, Message>; 2],
    preferred: f32,
}

impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Pane<'_> {
    fn children(&self) -> Vec<Tree> {
        self.children.iter().map(Tree::new).collect()
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(&mut self.children);
    }
    fn size(&self) -> Size<Length> {
        Size::new(Length::Fill, Length::Fill)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        // Both the transcript and composer are laid out at this exact width.
        // A permission request, resumed conversation or wider status label must
        // never move the column. The footer's flex row wraps within that room.
        let width = self
            .preferred
            .min(design::READABLE_MEASURE + 2.0 * step::GAP_M)
            .min(limits.max().width)
            .max(0.0);
        let mut footer = self.children[1].as_widget_mut().layout(
            &mut tree.children[1],
            renderer,
            &layout::Limits::new(Size::ZERO, Size::new(width, limits.max().height)),
        );
        let body_height = (limits.max().height - footer.size().height - step::GAP_S).max(0.0);
        let mut body = self.children[0].as_widget_mut().layout(
            &mut tree.children[0],
            renderer,
            &layout::Limits::new(Size::new(width, body_height), Size::new(width, body_height)),
        );
        let size = limits.resolve(
            Length::Fill,
            Length::Fill,
            Size::new(width, body_height + step::GAP_S + footer.size().height),
        );
        let x = (size.width - width) / 2.0;
        body.move_to_mut(Point::new(x, 0.0));
        footer.move_to_mut(Point::new(x, body_height + step::GAP_S));
        layout::Node::with_children(size, vec![body, footer])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
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
        renderer: &cosmic::Renderer,
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
        renderer: &mut cosmic::Renderer,
        theme: &cosmic::Theme,
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
        renderer: &cosmic::Renderer,
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
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, cosmic::Theme, cosmic::Renderer>> {
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

    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        destinations: &mut iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        for ((child, state), child_layout) in self
            .children
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
        {
            child.as_widget().drag_destinations(
                state,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                renderer,
                destinations,
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contents_do_not_change_the_reading_column() {
        let renderer = crate::app::tests::headless_renderer();
        for available in [280.0, 600.0, 956.0, 1440.0] {
            let expected = available.min(design::READABLE_MEASURE + 2.0 * step::GAP_M);
            for footer_width in [0.0, 150.25, 900.0, 3000.0] {
                let footer = widget::flex_row(vec![
                    widget::Space::new().width(footer_width).height(20).into(),
                    widget::Space::new().width(150).height(20).into(),
                ])
                .spacing(10)
                .into();
                let mut element = pane(
                    widget::Space::new()
                        .width(Length::Fill)
                        .height(Length::Fill)
                        .into(),
                    footer,
                    available,
                );
                let mut tree = Tree::new(&element);
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(available, 600.0)),
                );
                let body = &node.children()[0];
                let footer = &node.children()[1];
                assert_eq!(body.size().width, expected);
                assert_eq!(body.bounds().x, (available - expected) / 2.0);
                assert_eq!(footer.bounds().x, body.bounds().x);
                assert!(footer.size().width <= expected);
                assert_eq!(footer.bounds().y + footer.size().height, 600.0);
            }
        }
    }
}
