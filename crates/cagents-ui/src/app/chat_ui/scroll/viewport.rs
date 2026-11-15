//! Propagate native clipping/translation to nested virtual rows without
//! changing the native scroll anchor or the reader's offset.
use super::*;

pub(in crate::app::chat_ui) fn viewport<'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(Viewport {
        content: content.into(),
        minimum_width: 0.0,
    })
}
/// Keep a remembered cross extent while allowing newly measured content to grow.
/// This changes the minimum layout constraint, never the maximum or fixed width.
pub(in crate::app::chat_ui) fn minimum_width<'a>(
    content: impl Into<Element<'a, Message>>,
    width: f32,
) -> Element<'a, Message> {
    Element::new(Viewport {
        content: content.into(),
        minimum_width: width.max(0.0),
    })
}
struct Viewport<'a> {
    content: Element<'a, Message>,
    minimum_width: f32,
}
impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Viewport<'_> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
    }
    fn size_hint(&self) -> Size<Length> {
        self.content.as_widget().size_hint()
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let limits = limits.min_width(self.minimum_width);
        let mut node =
            self.content
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, &limits);
        for _ in 0..4 {
            let mut viewport = virtualized::UpdateViewport::default();
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Layout::new(&node),
                renderer,
                &mut viewport,
            );
            if !viewport.changed {
                break;
            }
            node = self
                .content
                .as_widget_mut()
                .layout(&mut tree.children[0], renderer, &limits);
        }
        node
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, operation);
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
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout,
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        let mut observed = virtualized::UpdateViewport::default();
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout,
            renderer,
            &mut observed,
        );
        if observed.changed {
            shell.invalidate_layout();
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
        self.content.as_widget().draw(
            &tree.children[0],
            renderer,
            theme,
            style,
            layout,
            cursor,
            viewport,
        );
    }
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &cosmic::Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
    }
    fn overlay<'b>(
        &'b mut self,
        tree: &'b mut Tree,
        layout: Layout<'b>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'b, Message, cosmic::Theme, cosmic::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
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
        self.content
            .as_widget()
            .a11y_nodes(layout, &tree.children[0], cursor)
    }
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        rectangles: &mut iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content
            .as_widget()
            .drag_destinations(&tree.children[0], layout, renderer, rectangles);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remembered_minimum_preserves_extent_without_capping_new_content() {
        let renderer = crate::app::tests::headless_renderer();
        let limits = layout::Limits::new(Size::ZERO, Size::new(f32::INFINITY, 200.0));
        for (natural, remembered, expected) in [(100.0, 2000.0, 2000.0), (3000.0, 2000.0, 3000.0)] {
            let mut element = minimum_width(
                widget::container(widget::Space::new().width(natural).height(20)),
                remembered,
            );
            let mut tree = Tree::new(&element);
            let node = element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
            assert_eq!(node.size().width, expected);
        }
    }
}
