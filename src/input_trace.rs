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

use crate::{model::CardId, workspace_widget::FocusedPane};
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
pub struct InputTrace(Rc<Cell<Sample>>, Rc<Cell<Option<KeyboardRegion>>>);

#[derive(Clone, Copy, Default)]
struct Sample {
    /// Window positions of the two latest pointer samples.
    previous: Option<Point>,
    current: Option<Point>,
    /// When a key or a mouse button was last pressed.
    pressed: Option<Instant>,
}

impl InputTrace {
    /// Actual widget focus sampled immediately before this keyboard event.
    pub fn keyboard_focus(&self) -> Option<FocusedPane> {
        self.1.get().map(|region| region.pane)
    }

    pub fn keyboard_card(&self) -> Option<CardId> {
        self.1.get().and_then(|region| region.card)
    }

    #[cfg(test)]
    pub(crate) fn with_keyboard_region(region: KeyboardRegion) -> Self {
        let trace = Self::default();
        trace.1.set(Some(region));
        trace
    }

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
        focus_regions: None,
        keyboard_callback: None,
    })
}

/// Both scope and action target come from the widget holding native focus.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KeyboardRegion {
    pub pane: FocusedPane,
    pub card: Option<CardId>,
}
impl From<FocusedPane> for KeyboardRegion {
    fn from(pane: FocusedPane) -> Self {
        Self { pane, card: None }
    }
}

/// Sample native widget focus before workspace interception. IDs identify
/// custom editors and terminal surfaces; ordinary text controls identify
/// themselves through the widget operation contract.
pub fn observe_keyboard_focus<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    trace: &InputTrace,
    regions: Vec<(cosmic::iced::widget::Id, KeyboardRegion)>,
) -> Element<'a, Message> {
    Element::new(Observed {
        content: content.into(),
        trace: trace.clone(),
        focus_regions: Some(regions),
        keyboard_callback: None,
    })
}

type KeyboardCallback<Message> = fn(keyboard::Event, bool, KeyboardRegion) -> Message;

/// Publish each keyboard event with its own native focus snapshot. Iced
/// batches widget events before subscription messages, so reading a shared
/// "latest focus" later can route an earlier chord to a different region.
pub fn observe_keyboard_events<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    trace: &InputTrace,
    regions: Vec<(cosmic::iced::widget::Id, KeyboardRegion)>,
    on_key: KeyboardCallback<Message>,
) -> Element<'a, Message> {
    Element::new(Observed {
        content: content.into(),
        trace: trace.clone(),
        focus_regions: Some(regions),
        keyboard_callback: Some(on_key),
    })
}

pub(crate) struct ScopedKeyboardRegion {
    focus: Rc<Cell<Option<KeyboardRegion>>>,
    previous: Option<KeyboardRegion>,
}
impl Drop for ScopedKeyboardRegion {
    fn drop(&mut self) {
        self.focus.set(self.previous);
    }
}
impl InputTrace {
    pub(crate) fn scoped_keyboard_region(&self, region: KeyboardRegion) -> ScopedKeyboardRegion {
        ScopedKeyboardRegion {
            focus: Rc::clone(&self.1),
            previous: self.1.replace(Some(region)),
        }
    }
}

fn keyboard_event_needs_focus(event: &Event) -> bool {
    matches!(event, Event::Keyboard(keyboard::Event::KeyPressed { key, modifiers, .. } | keyboard::Event::KeyReleased { key, modifiers, .. }) if modifiers.control() || modifiers.alt() || modifiers.logo() || matches!(key, keyboard::Key::Named(_)))
}

/// Marks custom multiline text controls for native keyboard-focus queries.
/// Unlike text_input, iced's text_editor exposes only its focusable state.
pub(crate) struct TextEditing;

#[derive(Default)]
struct KeyboardFocus {
    focused: Option<(Option<cosmic::iced::widget::Id>, Rectangle)>,
    inputs: Vec<(Option<cosmic::iced::widget::Id>, Rectangle)>,
    known_regions: Vec<(cosmic::iced::widget::Id, KeyboardRegion)>,
    containers: Vec<(KeyboardRegion, Rectangle)>,
}
impl Operation for KeyboardFocus {
    fn container(&mut self, id: Option<&cosmic::iced::widget::Id>, bounds: Rectangle) {
        if let Some((_, region)) = self
            .known_regions
            .iter()
            .find(|(candidate, _)| Some(candidate) == id)
        {
            self.containers.push((*region, bounds));
        }
    }
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }
    fn focusable(
        &mut self,
        id: Option<&cosmic::iced::widget::Id>,
        bounds: Rectangle,
        state: &mut dyn cosmic::iced::advanced::widget::operation::Focusable,
    ) {
        if state.is_focused() {
            self.focused = Some((id.cloned(), bounds));
        }
    }
    fn custom(
        &mut self,
        id: Option<&cosmic::iced::widget::Id>,
        bounds: Rectangle,
        state: &mut dyn std::any::Any,
    ) {
        if state.is::<TextEditing>() {
            self.inputs.push((id.cloned(), bounds));
        }
    }
    fn text_input(
        &mut self,
        id: Option<&cosmic::iced::widget::Id>,
        bounds: Rectangle,
        _state: &mut dyn cosmic::iced::advanced::widget::operation::TextInput,
    ) {
        self.inputs.push((id.cloned(), bounds));
    }
}
impl KeyboardFocus {
    fn region(&self, regions: &[(cosmic::iced::widget::Id, KeyboardRegion)]) -> KeyboardRegion {
        let Some((id, bounds)) = &self.focused else {
            return FocusedPane::Other.into();
        };
        if let Some((_, region)) = regions
            .iter()
            .find(|(candidate, _)| Some(candidate) == id.as_ref())
        {
            return *region;
        }
        let container = self
            .containers
            .iter()
            .rev()
            .find(|(_, container)| container.contains(bounds.center()))
            .map_or_else(
                || KeyboardRegion::from(FocusedPane::OutsideWorkspace),
                |(region, _)| *region,
            );
        if self
            .inputs
            .iter()
            .any(|(candidate, rectangle)| candidate == id && rectangle == bounds)
        {
            KeyboardRegion {
                pane: FocusedPane::TextInput,
                ..container
            }
        } else {
            container
        }
    }
}

struct Observed<'a, Message> {
    content: Element<'a, Message>,
    trace: InputTrace,
    focus_regions: Option<Vec<(cosmic::iced::widget::Id, KeyboardRegion)>>,
    keyboard_callback: Option<KeyboardCallback<Message>>,
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
        if keyboard_event_needs_focus(event)
            && let Some(regions) = &self.focus_regions
        {
            let mut focus = KeyboardFocus {
                known_regions: regions.clone(),
                ..KeyboardFocus::default()
            };
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut focus,
            );
            self.trace.1.set(Some(focus.region(regions)));
        } else if matches!(event, Event::Keyboard(_)) && self.focus_regions.is_some() {
            // An ordinary key or modifier update must not reuse a previous
            // chord's pane after the user moves focus with the pointer.
            self.trace.1.set(None);
        }
        let region = self
            .trace
            .1
            .get()
            .unwrap_or_else(|| FocusedPane::Other.into());
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
        if let Event::Keyboard(event) = event
            && let Some(callback) = &self.keyboard_callback
        {
            shell.publish(callback(event.clone(), shell.is_event_captured(), region));
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
            .map(|content| {
                ObservedOverlay::wrap(
                    content,
                    &self.trace,
                    self.focus_regions.clone(),
                    self.keyboard_callback,
                )
            })
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
    focus_regions: Option<Vec<(cosmic::iced::widget::Id, KeyboardRegion)>>,
    keyboard_callback: Option<KeyboardCallback<Message>>,
}

impl<'a, Message: 'a> ObservedOverlay<'a, Message> {
    fn wrap(
        content: overlay::Element<'a, Message, Theme, Renderer>,
        trace: &InputTrace,
        focus_regions: Option<Vec<(cosmic::iced::widget::Id, KeyboardRegion)>>,
        keyboard_callback: Option<KeyboardCallback<Message>>,
    ) -> overlay::Element<'a, Message, Theme, Renderer> {
        overlay::Element::new(Box::new(Self {
            content,
            trace: trace.clone(),
            focus_regions,
            keyboard_callback,
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
        let mut owns_keyboard = false;
        if matches!(event, Event::Keyboard(_))
            && let Some(regions) = &self.focus_regions
        {
            let mut focus = KeyboardFocus {
                known_regions: regions.clone(),
                ..KeyboardFocus::default()
            };
            self.content
                .as_overlay_mut()
                .operate(layout, renderer, &mut focus);
            owns_keyboard = focus.focused.is_some();
            self.trace.1.set(Some(focus.region(regions)));
        }
        let region = self
            .trace
            .1
            .get()
            .unwrap_or_else(|| FocusedPane::OutsideWorkspace.into());
        self.content
            .as_overlay_mut()
            .update(event, layout, cursor, renderer, clipboard, shell);
        if let Event::Keyboard(event) = event
            && let Some(callback) = &self.keyboard_callback
        {
            // A focused overlay owns ignored editing chords too. Capturing
            // those prevents both background input and duplicate root routing.
            if owns_keyboard {
                shell.capture_event();
            }
            if shell.is_event_captured() {
                shell.publish(callback(event.clone(), true, region));
            }
        }
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
        let regions = self.focus_regions.clone();
        let keyboard_callback = self.keyboard_callback;
        self.content
            .as_overlay_mut()
            .overlay(layout, renderer)
            .map(|nested| ObservedOverlay::wrap(nested, &trace, regions, keyboard_callback))
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
    fn native_focus_distinguishes_external_fields_and_pane_editors() {
        use cosmic::iced::advanced::widget::operation::Focusable;
        struct Focus(bool);
        impl Focusable for Focus {
            fn is_focused(&self) -> bool {
                self.0
            }
            fn focus(&mut self) {
                self.0 = true;
            }
            fn unfocus(&mut self) {
                self.0 = false;
            }
        }
        let sidebar = cosmic::iced::widget::Id::new("sidebar.search");
        let editor = cosmic::iced::widget::Id::new("editor");
        let bounds = Rectangle::new(Point::ORIGIN, Size::new(100., 24.));
        let mut query = KeyboardFocus::default();
        query.focusable(Some(&sidebar), bounds, &mut Focus(true));
        query.custom(Some(&sidebar), bounds, &mut TextEditing);
        assert_eq!(
            query
                .region(&[(editor.clone(), FocusedPane::Editor.into())])
                .pane,
            FocusedPane::TextInput
        );
        let mut query = KeyboardFocus::default();
        query.focusable(Some(&editor), bounds, &mut Focus(true));
        assert_eq!(
            query.region(&[(editor, FocusedPane::Editor.into())]).pane,
            FocusedPane::Editor
        );
    }

    #[test]
    fn queued_events_restore_their_own_focus_snapshot() {
        let trace = InputTrace::default();
        let editor = KeyboardRegion {
            pane: FocusedPane::Editor,
            card: Some(CardId::new()),
        };
        let sidebar = KeyboardRegion::from(FocusedPane::TextInput);
        trace.1.set(Some(sidebar));
        {
            let _event = trace.scoped_keyboard_region(editor);
            assert_eq!(trace.keyboard_focus(), Some(FocusedPane::Editor));
            assert_eq!(trace.keyboard_card(), editor.card);
        }
        assert_eq!(trace.keyboard_focus(), Some(FocusedPane::TextInput));
        assert_eq!(trace.keyboard_card(), None);
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
