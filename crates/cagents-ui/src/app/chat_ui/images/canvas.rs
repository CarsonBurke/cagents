//! Preserve wheel zoom and drag state during ordinary redraws, but reset both
//! on Fit or navigation. The pinned iced keyed column only reconciles key
//! changes when its child count changes, so it cannot provide this contract.
use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, renderer,
    widget::{Operation, Tree, tree},
};
use iced::{Event, Rectangle, Size, mouse};

pub(super) fn resettable<'a>(revision: u64, content: Element<'a, Message>) -> Element<'a, Message> {
    Element::new(Canvas { revision, content })
}
struct Canvas<'a> {
    revision: u64,
    content: Element<'a, Message>,
}
struct Revision(u64);
impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Canvas<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Revision>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Revision(self.revision))
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.content.as_widget())]
    }
    fn diff(&mut self, tree: &mut Tree) {
        let revision = tree.state.downcast_mut::<Revision>();
        if revision.0 != self.revision {
            revision.0 = self.revision;
            tree.children = self.children();
        } else {
            tree.children[0].diff(self.content.as_widget_mut());
        }
    }
    fn size(&self) -> Size<Length> {
        self.content.as_widget().size()
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
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn redraw_keeps_zoom_and_pan_but_fit_and_navigation_reset_them() {
        use iced::advanced::renderer::Headless;
        use iced::widget::image::{Handle, Viewer, viewer};
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cosmic::Renderer::new(
                iced::Font::default(),
                iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let handle = Handle::from_rgba(1, 1, vec![0, 0, 0, 255]);
        let element = |revision| {
            resettable(
                revision,
                Viewer::new(handle.clone())
                    .width(Length::Fill)
                    .height(Length::Fill)
                    .content_fit(iced::ContentFit::Contain)
                    .into(),
            )
        };
        let mut first = element(0);
        let mut tree = Tree::new(first.as_widget());
        let bounds = Rectangle::new(iced::Point::ORIGIN, Size::new(300.0, 200.0));
        let node = first.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        let cursor = mouse::Cursor::Available(iced::Point::new(50.0, 50.0));
        let mut messages = Vec::new();
        for event in [
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y: 1.0 },
            }),
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
        ] {
            first.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                cursor,
                &renderer,
                &mut iced::advanced::clipboard::Null,
                &mut Shell::new(&mut messages),
                &bounds,
            );
        }
        let scale = |tree: &Tree| {
            viewer::scaled_image_size(
                &renderer,
                &handle,
                tree.children[0].state.downcast_ref::<viewer::State>(),
                bounds.size(),
                iced::ContentFit::Contain,
            )
        };
        let zoomed = scale(&tree);
        assert!(zoomed.width > 200.0);
        let mut redraw = element(0);
        tree.diff(redraw.as_widget_mut());
        assert_eq!(scale(&tree), zoomed);
        assert!(
            tree.children[0]
                .state
                .downcast_ref::<viewer::State>()
                .is_cursor_grabbed()
        );
        let mut reset = element(1);
        tree.diff(reset.as_widget_mut());
        assert_eq!(scale(&tree), Size::new(200.0, 200.0));
        assert!(
            !tree.children[0]
                .state
                .downcast_ref::<viewer::State>()
                .is_cursor_grabbed()
        );
    }
}
