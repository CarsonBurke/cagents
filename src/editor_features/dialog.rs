//! A dialog ticket owns its entire native widget state, including unnamed
//! buttons. The pinned keyed Column keeps positional children at equal count.
use super::Action;
use cosmic::{
    Element, Renderer, Theme,
    iced::advanced::{
        Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
        widget::{Operation, Tree, tree},
    },
    iced::{Event, Length, Rectangle, Size, Vector, mouse},
};

struct Ticket(u64);

struct Dialog<'a> {
    ticket: u64,
    content: Element<'a, Action>,
}
pub(super) fn scoped(ticket: u64, content: Element<'_, Action>) -> Element<'_, Action> {
    Element::new(Dialog { ticket, content })
}
impl Widget<Action, Theme, Renderer> for Dialog<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Ticket>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Ticket(self.ticket))
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.content.as_widget())]
    }
    fn diff(&mut self, tree: &mut Tree) {
        let ticket = tree.state.downcast_mut::<Ticket>();
        if ticket.0 != self.ticket {
            ticket.0 = self.ticket;
            tree.children = self.children();
        } else {
            tree.children[0].diff(self.content.as_widget_mut());
        }
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
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        self.content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
    }
    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
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
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Action>,
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
    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout,
            cursor,
            viewport,
            renderer,
        )
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
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Action, Theme, Renderer>> {
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
        state: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        self.content
            .as_widget()
            .a11y_nodes(layout, &state.children[0], cursor)
    }
    fn drag_destinations(
        &self,
        state: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        rectangles: &mut cosmic::iced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &state.children[0],
            layout,
            renderer,
            rectangles,
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::{
        iced::{
            self, Point,
            advanced::{Shell, clipboard},
        },
        widget,
    };
    #[test]
    fn unnamed_button_press_does_not_cross_dialog_tickets() {
        // Same production wrapper and native button, without named IDs that
        // could independently protect the control during lift/restore.
        use cosmic::iced::advanced::renderer::Headless;
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                cosmic::iced::Font::default(),
                cosmic::iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let mut element = scoped(
            1,
            widget::button::standard("Apply")
                .on_press(Action::Apply(1))
                .into(),
        );
        let mut tree = Tree::new(element.as_widget());
        let limits = layout::Limits::new(Size::ZERO, Size::new(500.0, 200.0));
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let point = Layout::new(&node).bounds().center();
        let pointer =
            |element: &mut Element<'_, Action>, tree: &mut Tree, node: &layout::Node, event| {
                let mut messages = Vec::new();
                element.as_widget_mut().update(
                    tree,
                    &Event::Mouse(event),
                    Layout::new(node),
                    mouse::Cursor::Available(point),
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &Rectangle::new(Point::ORIGIN, Size::new(500.0, 200.0)),
                );
                messages
            };
        assert!(
            pointer(
                &mut element,
                &mut tree,
                &node,
                mouse::Event::ButtonPressed(mouse::Button::Left)
            )
            .is_empty()
        );
        drop(element);
        let mut element = scoped(
            2,
            widget::button::standard("Apply")
                .on_press(Action::Apply(2))
                .into(),
        );
        tree::NAMED.with(|named| *named.borrow_mut() = tree.take_all_named());
        tree.diff(element.as_widget_mut());
        iced::runtime::user_interface::restore_cached_named_states(&mut tree);
        tree::NAMED.with(|named| named.borrow_mut().clear());
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        assert!(
            pointer(
                &mut element,
                &mut tree,
                &node,
                mouse::Event::ButtonReleased(mouse::Button::Left)
            )
            .is_empty()
        );
        assert!(
            pointer(
                &mut element,
                &mut tree,
                &node,
                mouse::Event::ButtonPressed(mouse::Button::Left)
            )
            .is_empty()
        );
        assert!(matches!(
            pointer(
                &mut element,
                &mut tree,
                &node,
                mouse::Event::ButtonReleased(mouse::Button::Left)
            )
            .as_slice(),
            [Action::Apply(2)]
        ));
    }
}
