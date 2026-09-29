//! What the window last saw of the pointer and the keyboard, whichever widget
//! or overlay the input went to.
//!
//! A widget sees only the events that reach it. An overlay that captures the
//! pointer - an open context menu - keeps every motion over it from the
//! widgets beneath, so a widget that remembered its own last pointer sample
//! would still hold the one from before the menu opened. A key that a focused
//! input or a global shortcut captures never reaches the workspace at all.
//! [`observe`] wraps the window's content so that an [`InputTrace`] records
//! both whichever way they went: ahead of the content when they reach it, and
//! from within the overlays when those take them first.

use std::{cell::Cell, rc::Rc};

use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree},
};
use cosmic::iced::{Event, Length, Point, Rectangle, Size, Vector, keyboard, mouse, time::Instant};
use cosmic::{Element, Renderer, Theme};

/// A shared record of the window's latest input, kept by [`observe`].
///
/// Cloning shares the record. It is cheap to hand to every widget that reads
/// it, and costs one copy of a few words per recorded event.
#[derive(Clone, Default)]
pub struct InputTrace(Rc<Cell<Sample>>);

#[derive(Clone, Copy, Default)]
struct Sample {
    /// Window positions of the two latest pointer samples.
    previous: Option<Point>,
    current: Option<Point>,
    /// When a key or a mouse button was last pressed.
    pressed: Option<Instant>,
}

impl InputTrace {
    /// Records one event the window delivered.
    pub(crate) fn record(&self, event: &Event) {
        let mut sample = self.0.get();
        match event {
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                // An overlay and the content beneath it can both see one
                // motion; it is still one sample.
                if sample.current == Some(*position) {
                    return;
                }
                sample.previous = sample.current;
                sample.current = Some(*position);
            }
            // Where the pointer comes back is no motion from where it left.
            Event::Mouse(mouse::Event::CursorLeft) => {
                sample.previous = None;
                sample.current = None;
            }
            Event::Mouse(mouse::Event::ButtonPressed(_))
            | Event::Keyboard(keyboard::Event::KeyPressed { .. }) => {
                sample.pressed = Some(Instant::now());
            }
            _ => return,
        }
        self.0.set(sample);
    }

    /// The pointer's window position one motion before its latest, or `None`
    /// when the pointer has moved only once since it entered the window.
    ///
    /// Read while handling a motion, this is where that motion started.
    #[must_use]
    pub fn previous_position(&self) -> Option<Point> {
        self.0.get().previous
    }

    /// Whether a key or a mouse button was pressed anywhere in the window at
    /// or after `instant`.
    #[must_use]
    pub fn pressed_since(&self, instant: Instant) -> bool {
        self.0
            .get()
            .pressed
            .is_some_and(|pressed| pressed >= instant)
    }
}

/// Records into `trace` every event the window delivers to `content` or to
/// any overlay it opens.
///
/// Wrap the window's whole content, and do so unconditionally: an event that
/// reaches only a part of the window outside the wrapper goes unrecorded, and
/// adding or removing the wrapper changes the shape of the widget tree.
pub fn observe<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    trace: &InputTrace,
) -> Element<'a, Message> {
    Element::new(Observed {
        content: content.into(),
        trace: trace.clone(),
    })
}

struct Observed<'a, Message> {
    content: Element<'a, Message>,
    trace: InputTrace,
}

impl<Message> Widget<Message, Theme, Renderer> for Observed<'_, Message> {
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
        shell: &mut Shell<'_, Message>,
        viewport: &Rectangle,
    ) {
        // Before the content, so everything inside reads this event's sample
        // as the latest and the one before it as where the motion began.
        self.trace.record(event);
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
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        self.content
            .as_widget_mut()
            .overlay(
                &mut tree.children[0],
                layout,
                renderer,
                viewport,
                translation,
            )
            .map(|content| ObservedOverlay::wrap(content, &self.trace))
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
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &tree.children[0],
            layout,
            renderer,
            destinations,
        );
    }
}

/// Records the events an overlay receives, and those of every overlay nested
/// in it: the runtime hands an event to a nested overlay first and withholds
/// it from the parent once the nested one captures it.
struct ObservedOverlay<'a, Message> {
    content: overlay::Element<'a, Message, Theme, Renderer>,
    trace: InputTrace,
}

impl<'a, Message: 'a> ObservedOverlay<'a, Message> {
    fn wrap(
        content: overlay::Element<'a, Message, Theme, Renderer>,
        trace: &InputTrace,
    ) -> overlay::Element<'a, Message, Theme, Renderer> {
        overlay::Element::new(Box::new(Self {
            content,
            trace: trace.clone(),
        }))
    }
}

impl<Message> overlay::Overlay<Message, Theme, Renderer> for ObservedOverlay<'_, Message> {
    fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
        self.content.as_overlay_mut().layout(renderer, bounds)
    }

    fn draw(
        &self,
        renderer: &mut Renderer,
        theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
    ) {
        self.content
            .as_overlay()
            .draw(renderer, theme, style, layout, cursor);
    }

    fn operate(&mut self, layout: Layout<'_>, renderer: &Renderer, operation: &mut dyn Operation) {
        self.content
            .as_overlay_mut()
            .operate(layout, renderer, operation);
    }

    fn update(
        &mut self,
        event: &Event,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
        clipboard: &mut dyn Clipboard,
        shell: &mut Shell<'_, Message>,
    ) {
        self.trace.record(event);
        self.content
            .as_overlay_mut()
            .update(event, layout, cursor, renderer, clipboard, shell);
    }

    fn mouse_interaction(
        &self,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        self.content
            .as_overlay()
            .mouse_interaction(layout, cursor, renderer)
    }

    fn overlay<'b>(
        &'b mut self,
        layout: Layout<'b>,
        renderer: &Renderer,
    ) -> Option<overlay::Element<'b, Message, Theme, Renderer>> {
        let trace = self.trace.clone();
        self.content
            .as_overlay_mut()
            .overlay(layout, renderer)
            .map(|nested| ObservedOverlay::wrap(nested, &trace))
    }

    fn index(&self) -> f32 {
        self.content.as_overlay().index()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn moved(x: f32) -> Event {
        Event::Mouse(mouse::Event::CursorMoved {
            position: Point::new(x, 0.0),
        })
    }

    #[test]
    fn a_motion_seen_twice_is_one_sample() {
        let trace = InputTrace::default();
        trace.record(&moved(1.0));
        trace.record(&moved(2.0));
        // The overlay saw it, and so did the content beneath.
        trace.record(&moved(2.0));
        assert_eq!(trace.previous_position(), Some(Point::new(1.0, 0.0)));

        trace.record(&Event::Mouse(mouse::Event::CursorLeft));
        trace.record(&moved(3.0));
        assert_eq!(trace.previous_position(), None, "a return is no motion");
    }

    #[test]
    fn presses_are_timed_wherever_they_land() {
        let trace = InputTrace::default();
        let before = Instant::now();
        assert!(!trace.pressed_since(before));
        trace.record(&Event::Mouse(mouse::Event::ButtonPressed(
            mouse::Button::Right,
        )));
        assert!(trace.pressed_since(before));
        assert!(!trace.pressed_since(Instant::now() + std::time::Duration::from_secs(1)));
    }
}
