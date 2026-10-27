//! Work board wheel policy; other horizontal surfaces keep their own input.
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Id, Operation, Tree, operation, tree},
};
use cosmic::iced::{Event, Length, Rectangle, Size, Vector, keyboard, mouse, window};
use cosmic::{Element, Renderer, Theme};

pub(super) const ID: &str = "work-board-columns";

pub(super) fn view<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
) -> Element<'a, Message> {
    Element::new(BoardWheel {
        content: content.into(),
    })
}

struct HorizontalScroll {
    amount: f32,
    changed: bool,
}

impl Operation for HorizontalScroll {
    fn traverse(&mut self, _operate: &mut dyn FnMut(&mut dyn Operation)) {}
    // The board is the first and only destination; never walk its cards.
    fn scrollable(
        &mut self,
        id: Option<&Id>,
        bounds: Rectangle,
        content: Rectangle,
        translation: Vector,
        state: &mut dyn operation::Scrollable,
    ) {
        if id != Some(&Id::new(ID)) || !self.amount.is_finite() {
            return;
        }
        let next =
            (translation.x + self.amount).clamp(0.0, (content.width - bounds.width).max(0.0));
        self.changed = next != translation.x;
        if self.changed {
            state.scroll_by(
                operation::scrollable::AbsoluteOffset {
                    x: self.amount,
                    y: 0.0,
                },
                bounds,
                content,
            );
        }
    }
}

struct BoardWheel<'a, Message> {
    content: Element<'a, Message>,
}

impl<Message> Widget<Message, Theme, Renderer> for BoardWheel<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<keyboard::Modifiers>()
    }
    fn state(&self) -> tree::State {
        tree::State::new(keyboard::Modifiers::empty())
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
        let modifiers = tree.state.downcast_mut::<keyboard::Modifiers>();
        match event {
            Event::Keyboard(keyboard::Event::ModifiersChanged(next)) => *modifiers = *next,
            Event::Window(window::Event::Unfocused) => *modifiers = keyboard::Modifiers::empty(),
            _ => {}
        }
        let shifted = modifiers.shift();
        let remapped;
        let event = match event {
            Event::Mouse(mouse::Event::WheelScrolled { delta }) => {
                if shell.is_event_captured()
                    || !cursor.is_over(layout.bounds())
                    || !cursor.is_over(*viewport)
                {
                    return;
                }
                let (x, y) = match delta {
                    mouse::ScrollDelta::Lines { x, y } => (*x * 60.0, *y * 60.0),
                    mouse::ScrollDelta::Pixels { x, y } => (*x, *y),
                };
                if shifted {
                    // A native scroll operation targets only the board. Sending
                    // a remapped wheel through a recently scrolled lane lets
                    // that lane capture even a zero-y gesture for 1.5 seconds.
                    let amount = -if x == 0.0 { y } else { x };
                    let mut scroll = HorizontalScroll {
                        amount,
                        changed: false,
                    };
                    self.content.as_widget_mut().operate(
                        &mut tree.children[0],
                        layout,
                        renderer,
                        &mut scroll,
                    );
                    if scroll.changed {
                        shell.request_redraw();
                    }
                    shell.capture_event();
                    return;
                }
                // No wheel horizontal motion without Shift, including native
                // x/diagonal trackpad deltas. Pixel y remains smooth; line y
                // uses the native scrollable's existing 60px scale. Pixels also
                // avoid a stale child modifier remapping y after focus loss.
                remapped = Event::Mouse(mouse::Event::WheelScrolled {
                    delta: mouse::ScrollDelta::Pixels { x: 0.0, y },
                });
                &remapped
            }
            other => other,
        };
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
