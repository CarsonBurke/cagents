//! Rendered structure visible only to widget operations. Layout and input are
//! delegated without an extra layout node or a new selectable endpoint.
use super::*;

#[derive(Clone)]
pub(in crate::app::chat_ui) enum Kind {
    ListItem(String),
    Table { columns: usize, rows: usize },
    Cell { row: usize, column: usize },
}
pub(super) struct Enter {
    pub identity: u64,
    pub kind: Kind,
}
pub(super) struct Exit;
struct Identity(u64);
struct Boundary<'a> {
    content: Element<'a, Message>,
    kind: Kind,
}
pub(in crate::app::chat_ui) fn wrap<'a>(
    content: impl Into<Element<'a, Message>>,
    kind: Kind,
) -> Element<'a, Message> {
    Element::new(Boundary {
        content: content.into(),
        kind,
    })
}
impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Boundary<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Identity>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Identity(NEXT.fetch_add(1, Ordering::Relaxed)))
    }
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
    fn id(&self) -> Option<Id> {
        self.content.as_widget().id()
    }
    fn set_id(&mut self, id: Id) {
        self.content.as_widget_mut().set_id(id);
    }
    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &cosmic::Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
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
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &cosmic::Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.custom(
            None,
            layout.bounds(),
            &mut Enter {
                identity: tree.state.downcast_ref::<Identity>().0,
                kind: self.kind.clone(),
            },
        );
        operation.traverse(&mut |operation| {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                operation,
            );
        });
        operation.custom(None, layout.bounds(), &mut Exit);
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
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &cosmic::Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, cosmic::Theme, cosmic::Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout,
            renderer,
            viewport,
            translation,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn boundary_preserves_accessible_child_state_and_virtual_offset() {
        let mut element = wrap(
            Element::new(crate::ui_controls::accessibility_probe::Probe),
            Kind::Cell { row: 0, column: 0 },
        );
        let mut tree = Tree::new(element.as_widget());
        let node = layout::Node::new(Size::new(80.0, 40.0));
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(&node).with_virtual_offset(Vector::new(0.0, 37.0)),
            &tree,
            mouse::Cursor::Unavailable,
        );
        let identity = accessible.root()[0].id().clone();
        assert_eq!(accessible.root()[0].node().label(), Some("Retained child"));
        tree.diff(&mut element);
        let accessible = element.as_widget().a11y_nodes(
            Layout::new(&node).with_virtual_offset(Vector::new(0.0, 37.0)),
            &tree,
            mouse::Cursor::Unavailable,
        );
        assert_eq!(*accessible.root()[0].id(), identity);
    }
}
