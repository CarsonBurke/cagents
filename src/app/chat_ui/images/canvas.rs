//! Preserve wheel zoom and drag state during ordinary redraws, but reset both
//! on Fit or navigation. The pinned iced keyed column only reconciles key
//! changes when its child count changes, so it cannot provide this contract.
use super::*;
use iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, renderer,
    widget::{Operation, Tree, tree},
};
use iced::{Event, Rectangle, Size, mouse};

#[cfg(test)]
pub(super) fn resettable<'a>(revision: u64, content: Element<'a, Message>) -> Element<'a, Message> {
    identified(0, revision, "Image viewer", content)
}
pub(super) fn identified<'a>(
    identity: u64,
    revision: u64,
    label: &str,
    content: Element<'a, Message>,
) -> Element<'a, Message> {
    Element::new(Canvas {
        identity,
        revision,
        label: label.to_owned(),
        content,
    })
}
struct Canvas<'a> {
    identity: u64,
    label: String,
    revision: u64,
    content: Element<'a, Message>,
}
struct Revision {
    identity: u64,
    revision: u64,
    focused: bool,
}
impl iced::advanced::widget::operation::Focusable for Revision {
    fn is_focused(&self) -> bool {
        self.focused
    }
    fn focus(&mut self) {
        self.focused = true;
    }
    fn unfocus(&mut self) {
        self.focused = false;
    }
}
impl Widget<Message, cosmic::Theme, cosmic::Renderer> for Canvas<'_> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<Revision>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(Revision {
            identity: self.identity,
            revision: self.revision,
            focused: false,
        })
    }
    fn children(&self) -> Vec<Tree> {
        vec![Tree::new(self.content.as_widget())]
    }
    fn diff(&mut self, tree: &mut Tree) {
        let revision = tree.state.downcast_mut::<Revision>();
        if revision.identity != self.identity || revision.revision != self.revision {
            revision.identity = self.identity;
            revision.revision = self.revision;
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
        operation.focusable(
            Some(&canvas_focus_id()),
            layout.bounds(),
            tree.state.downcast_mut::<Revision>(),
        );
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
        let state = tree.state.downcast_mut::<Revision>();
        if matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_))) {
            state.focused = cursor.is_over(layout.bounds());
        }
        let zoom = if state.focused {
            match event {
                Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key: iced::keyboard::Key::Character(key),
                    modifiers,
                    ..
                }) if !modifiers.command() && !modifiers.control() && !modifiers.alt() => {
                    match key.as_str() {
                        "+" | "=" => Some(1.0),
                        "-" | "−" => Some(-1.0),
                        _ => None,
                    }
                }
                _ => None,
            }
        } else {
            None
        };
        let zoom_event = zoom.map(|y| {
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Lines { x: 0.0, y },
            })
        });
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            zoom_event.as_ref().unwrap_or(event),
            layout,
            if zoom.is_some() {
                mouse::Cursor::Available(layout.bounds().center())
            } else {
                cursor
            },
            renderer,
            clipboard,
            shell,
            viewport,
        );
        if zoom.is_some() {
            shell.capture_event();
        }
    }
    fn id(&self) -> Option<widget::Id> {
        Some(canvas_focus_id())
    }
    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        _tree: &Tree,
        _cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        use iced_accessibility::accesskit::{Action, Node, Rect, Role};
        let bounds = layout.bounds();
        let mut node = Node::new(Role::Image);
        node.set_label(format!("Image viewer: {}", self.label));
        node.set_description("Left and Right browse images. Plus and minus zoom. 0 fits the image, 1 shows actual size, and Escape closes the viewer.");
        node.add_action(Action::Focus);
        node.set_bounds(Rect::new(
            f64::from(bounds.x),
            f64::from(bounds.y),
            f64::from(bounds.x + bounds.width),
            f64::from(bounds.y + bounds.height),
        ));
        iced_accessibility::A11yTree::leaf(node, canvas_focus_id())
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
        // A new gallery can start at the same Fit revision. Its independent
        // identity must still discard the previous image's pan and zoom.
        let mut replacement = identified(
            99,
            0,
            "Replacement image",
            Viewer::new(handle.clone())
                .width(Length::Fill)
                .height(Length::Fill)
                .content_fit(iced::ContentFit::Contain)
                .into(),
        );
        tree.diff(replacement.as_widget_mut());
        assert_eq!(scale(&tree), Size::new(200.0, 200.0));
        let accessible = replacement.as_widget().a11y_nodes(
            Layout::new(&node),
            &tree,
            mouse::Cursor::Unavailable,
        );
        assert!(
            accessible
                .root()
                .iter()
                .chain(accessible.children())
                .any(|node| node.node().label() == Some("Image viewer: Replacement image"))
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
