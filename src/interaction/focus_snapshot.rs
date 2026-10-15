//! Observe retained focus before an opening message changes the rendered view.
//! The modal popover deliberately hides its background from widget operations;
//! keeping each surface's last focused identity avoids querying the new dialog
//! when what we need is the input that was focused before it opened.
use super::*;

#[derive(Clone, Default)]
pub(crate) struct FocusSnapshot(Arc<std::sync::Mutex<Option<Id>>>);
impl FocusSnapshot {
    pub(crate) fn get(&self) -> Option<Id> {
        self.0.lock().expect("focus identity").clone()
    }
    fn record(&self, id: Id) {
        *self.0.lock().expect("focus identity") = Some(id);
    }
}
pub(crate) fn observe_focus<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    snapshot: FocusSnapshot,
) -> Element<'a, Message> {
    Element::new(FocusObserver {
        content: content.into(),
        snapshot,
    })
}
struct FocusObserver<'a, Message> {
    content: Element<'a, Message>,
    snapshot: FocusSnapshot,
}

impl<Message> Widget<Message, Theme, Renderer> for FocusObserver<'_, Message> {
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(&self.content)]
    }
    fn diff(&mut self, tree: &mut Tree) {
        tree.diff_children(std::slice::from_mut(&mut self.content));
    }
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
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
        self.content.as_widget_mut().operate(
            &mut tree.children[0],
            layout.with_virtual_offset(layout.virtual_offset()),
            renderer,
            operation,
        );
        self.sample(tree, layout, renderer);
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
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            layout.with_virtual_offset(layout.virtual_offset()),
            cursor,
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if matches!(
            event,
            Event::Keyboard(cosmic::iced::keyboard::Event::KeyPressed {
                key: cosmic::iced::keyboard::Key::Named(_),
                ..
            }) | Event::Mouse(mouse::Event::ButtonPressed(_) | mouse::Event::ButtonReleased(_))
                | Event::Touch(_)
        ) {
            self.sample(tree, layout, renderer);
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
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            layout.with_virtual_offset(layout.virtual_offset()),
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
            layout.with_virtual_offset(layout.virtual_offset()),
            cursor,
            viewport,
        )
    }
    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            layout.with_virtual_offset(layout.virtual_offset()),
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
        self.content.as_widget().a11y_nodes(
            layout.with_virtual_offset(layout.virtual_offset()),
            &tree.children[0],
            cursor,
        )
    }
    fn drag_destinations(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            layout.with_virtual_offset(layout.virtual_offset()),
            renderer,
            destinations,
        );
    }
}

impl<Message> FocusObserver<'_, Message> {
    fn sample(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer) {
        let snapshot = self.snapshot.clone();
        let mut probe = operation::map(operation::focusable::find_focused(), move |id| {
            snapshot.record(id)
        });
        self.content
            .as_widget_mut()
            .operate(&mut tree.children[0], layout, renderer, &mut probe);
        let _ = probe.finish();
    }
}
