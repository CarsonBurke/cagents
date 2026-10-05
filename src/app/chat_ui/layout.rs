//! Use the preferred reading width unless the footer needs more room.
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
        // Measure only the footer, not the transcript. Its widgets know their
        // actual font and control extents; character-count estimates do not.
        let natural = self.children[1].as_widget_mut().layout(
            &mut tree.children[1],
            renderer,
            &layout::Limits::new(Size::ZERO, Size::new(f32::INFINITY, limits.max().height)),
        );
        let width = self
            .preferred
            // Flex layout rounds child positions to pixels. Reserve a pixel
            // beyond the measured extent so fractional text widths still fit.
            .max(natural.size().width.ceil() + 1.0)
            .min(limits.max().width);
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
    fn footer_expands_the_reading_width_before_wrapping() {
        let renderer = crate::app::tests::headless_renderer();
        for (preferred, available, expected_width, wrapped) in [
            (400.0, 800.0, 400.0, false),
            (100.0, 800.0, 311.0, false),
            (100.0, 200.0, 200.0, true),
        ] {
            let footer = widget::flex_row(vec![
                widget::Space::new().width(150).height(20).into(),
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
                preferred,
            );
            let mut tree = Tree::new(&element);
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, Size::new(available, 600.0)),
            );
            assert_eq!(node.children()[0].size().width, expected_width);
            assert_eq!(node.children()[1].size().height > 20.0, wrapped);
            assert_eq!(
                node.children()[1].bounds().y + node.children()[1].size().height,
                600.0
            );
            assert_eq!(
                node.children()[0].bounds().x,
                (available - expected_width) / 2.0
            );
        }
    }

    #[test]
    fn fractional_footer_extents_do_not_force_a_second_row() {
        let renderer = crate::app::tests::headless_renderer();
        let footer = widget::flex_row(vec![
            widget::Space::new().width(150.25).height(20).into(),
            widget::Space::new().width(150.25).height(20).into(),
        ])
        .spacing(10)
        .into();
        let mut element = pane(widget::Space::new().into(), footer, 100.0);
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(800.0, 600.0)),
        );
        assert!(node.children()[0].size().width >= 310.5);
        assert_eq!(node.children()[1].size().height, 20.0);
    }
}
