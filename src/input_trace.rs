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

use std::{cell::Cell, collections::HashMap, rc::Rc, sync::Arc};

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
pub struct InputTrace(Rc<Cell<Sample>>, Rc<Cell<Option<KeyboardOwner>>>);

/// Immutable native focus at one key event, rather than the last card selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct KeyboardOwner {
    pub pane: FocusedPane,
    pub card: Option<CardId>,
    pub generation: Option<u64>,
}
impl KeyboardOwner {
    pub(crate) const UNKNOWN: Self = Self {
        pane: FocusedPane::OutsideWorkspace,
        card: None,
        generation: None,
    };
}
/// These keys retain the subscription's existing captured chrome/modal
/// handling, on both press and release.
pub(crate) fn keyboard_uses_legacy_fallback(event: &keyboard::Event) -> bool {
    match event {
        keyboard::Event::ModifiersChanged(_) => true,
        keyboard::Event::KeyPressed { key, .. } | keyboard::Event::KeyReleased { key, .. } => {
            matches!(
                key,
                keyboard::Key::Named(keyboard::key::Named::Escape | keyboard::key::Named::Tab)
            )
        }
    }
}

type KeyCallback<'a, Message> = Rc<dyn Fn(keyboard::Event, bool, KeyboardOwner) -> Message + 'a>;

#[derive(Default)]
struct KeyboardFocus {
    regions: Arc<HashMap<cosmic::iced::widget::Id, KeyboardOwner>>,
    region: Option<KeyboardOwner>,
    pending: Option<KeyboardOwner>,
    focused: Vec<(Option<cosmic::iced::widget::Id>, Rectangle, KeyboardOwner)>,
    inputs: Vec<(Option<cosmic::iced::widget::Id>, Rectangle)>,
}
impl Operation for KeyboardFocus {
    fn container(&mut self, id: Option<&cosmic::iced::widget::Id>, _bounds: Rectangle) {
        self.pending = id
            .and_then(|id| self.regions.get(id).copied())
            .or(self.region);
    }
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        let previous = self.region;
        self.region = self.pending.take().or(previous);
        operate(self);
        self.region = previous;
        self.pending = None;
    }
    fn focusable(
        &mut self,
        id: Option<&cosmic::iced::widget::Id>,
        bounds: Rectangle,
        state: &mut dyn cosmic::iced::advanced::widget::operation::Focusable,
    ) {
        if state.is_focused() {
            let owner = id
                .and_then(|id| self.regions.get(id).copied())
                .or(self.region)
                .unwrap_or_else(|| {
                    if id.is_some() {
                        KeyboardOwner {
                            pane: FocusedPane::Other,
                            card: None,
                            generation: None,
                        }
                    } else {
                        KeyboardOwner::UNKNOWN
                    }
                });
            self.focused.push((id.cloned(), bounds, owner));
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
    fn selected(&self) -> Option<(Option<cosmic::iced::widget::Id>, KeyboardOwner)> {
        // A field outside a raw pane can hold native focus while the raw pane
        // still reports its old flag. Prefer the positively identified field.
        let text = |candidate: &&(Option<cosmic::iced::widget::Id>, Rectangle, KeyboardOwner)| {
            self.inputs
                .iter()
                .any(|(id, bounds)| id == &candidate.0 && bounds == &candidate.1)
        };
        let mut inputs = self.focused.iter().filter(text);
        if let Some((id, _, owner)) = inputs.next() {
            return if inputs.next().is_none() {
                Some((
                    id.clone(),
                    KeyboardOwner {
                        pane: FocusedPane::TextInput,
                        ..*owner
                    },
                ))
            } else {
                None
            };
        }
        match self.focused.as_slice() {
            [(id, _, owner)] => Some((id.clone(), *owner)),
            _ => None, // Ambiguous or absent native focus never targets a pane.
        }
    }
}

#[derive(Clone, Copy, Default)]
struct Sample {
    /// Window positions of the two latest pointer samples.
    previous: Option<Point>,
    current: Option<Point>,
    /// When a key or a mouse button was last pressed.
    pressed: Option<Instant>,
}

impl InputTrace {
    pub(crate) fn keyboard_owner(&self) -> Option<KeyboardOwner> {
        self.1.get()
    }
    fn publish_key<Message>(
        event: &keyboard::Event,
        captured: bool,
        owner: KeyboardOwner,
        callback: &KeyCallback<'_, Message>,
        shell: &mut Shell<'_, Message>,
    ) {
        shell.publish(callback(event.clone(), captured, owner));
    }
    /// Records one event the window delivered.
    pub(crate) fn record(&self, event: &Event) {
        if matches!(event, Event::Window(cosmic::iced::window::Event::Unfocused)) {
            self.1.set(None);
        }
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
        regions: None,
        raw_fallback: None,
        on_key: None,
    })
}

/// Observe the same content and overlays, with one event-time native owner.
pub(crate) fn observe_keyboard<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    trace: &InputTrace,
    regions: Vec<(cosmic::iced::widget::Id, KeyboardOwner)>,
    raw_fallback: Option<KeyboardOwner>,
    enabled: bool,
    on_key: impl Fn(keyboard::Event, bool, KeyboardOwner) -> Message + 'a,
) -> Element<'a, Message> {
    Element::new(Observed {
        content: content.into(),
        trace: trace.clone(),
        regions: Some(Arc::new(regions.into_iter().collect())),
        raw_fallback,
        on_key: enabled.then(|| Rc::new(on_key) as KeyCallback<'a, Message>),
    })
}

struct Observed<'a, Message> {
    content: Element<'a, Message>,
    trace: InputTrace,
    regions: Option<Arc<HashMap<cosmic::iced::widget::Id, KeyboardOwner>>>,
    raw_fallback: Option<KeyboardOwner>,
    on_key: Option<KeyCallback<'a, Message>>,
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
        let owner = if let (Event::Keyboard(_key), Some(regions)) = (event, &self.regions) {
            let mut focus = KeyboardFocus {
                regions: regions.clone(),
                ..KeyboardFocus::default()
            };
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut focus,
            );
            let selected = focus.selected().or_else(|| {
                // The existing integrated browser owns its input through its
                // model flag, not Iced Focusable. Never override real native
                // focus (including an ambiguous set) with that raw fallback.
                focus
                    .focused
                    .is_empty()
                    .then_some(self.raw_fallback)
                    .flatten()
                    .map(|owner| (None, owner))
            });
            if focus.focused.len() > 1
                && let Some((Some(id), _)) = &selected
            {
                // Use the existing focus operation to prevent an obsolete raw
                // focus flag from admitting this field's key into a terminal.
                let mut retain =
                    cosmic::iced::advanced::widget::operation::focusable::focus::<()>(id.clone());
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut retain,
                );
            }
            let owner = selected.map_or(KeyboardOwner::UNKNOWN, |(_, owner)| owner);
            self.trace.1.set(Some(owner));
            Some(owner)
        } else {
            None
        };
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
        if let (Event::Keyboard(key), Some(owner), Some(callback)) = (event, owner, &self.on_key)
            && !keyboard_uses_legacy_fallback(key)
        {
            let child_captured = shell.is_event_captured();
            shell.capture_event();
            InputTrace::publish_key(key, child_captured, owner, callback, shell);
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
        let trace = self.trace.clone();
        let regions = self.regions.clone();
        let on_key = self.on_key.clone();
        self.content
            .as_widget_mut()
            .overlay(
                &mut tree.children[0],
                layout,
                renderer,
                viewport,
                translation,
            )
            .map(|content| ObservedOverlay::wrap(content, &trace, regions, on_key))
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
    regions: Option<Arc<HashMap<cosmic::iced::widget::Id, KeyboardOwner>>>,
    on_key: Option<KeyCallback<'a, Message>>,
}

impl<'a, Message: 'a> ObservedOverlay<'a, Message> {
    fn wrap(
        content: overlay::Element<'a, Message, Theme, Renderer>,
        trace: &InputTrace,
        regions: Option<Arc<HashMap<cosmic::iced::widget::Id, KeyboardOwner>>>,
        on_key: Option<KeyCallback<'a, Message>>,
    ) -> overlay::Element<'a, Message, Theme, Renderer> {
        overlay::Element::new(Box::new(Self {
            content,
            trace: trace.clone(),
            regions,
            on_key,
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
        let mut focused = false;
        let owner = if let (Event::Keyboard(_key), Some(regions)) = (event, &self.regions) {
            let mut focus = KeyboardFocus {
                regions: regions.clone(),
                ..KeyboardFocus::default()
            };
            self.content
                .as_overlay_mut()
                .operate(layout, renderer, &mut focus);
            focused = !focus.focused.is_empty();
            Some(
                focus
                    .selected()
                    .map_or(KeyboardOwner::UNKNOWN, |(_, owner)| owner),
            )
        } else {
            None
        };
        self.content
            .as_overlay_mut()
            .update(event, layout, cursor, renderer, clipboard, shell);
        if let (Event::Keyboard(key), Some(owner), Some(callback)) = (event, owner, &self.on_key)
            && !keyboard_uses_legacy_fallback(key)
        {
            let captured = shell.is_event_captured();
            if focused {
                shell.capture_event();
            }
            if shell.is_event_captured() {
                self.trace.1.set(Some(owner));
                InputTrace::publish_key(key, captured, owner, callback, shell);
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
        let regions = self.regions.clone();
        let on_key = self.on_key.clone();
        self.content
            .as_overlay_mut()
            .overlay(layout, renderer)
            .map(|nested| ObservedOverlay::wrap(nested, &trace, regions, on_key))
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
    struct OverlayKeyProbe {
        focus: bool,
        capture: bool,
        id: cosmic::iced::widget::Id,
    }
    impl cosmic::iced::advanced::widget::operation::Focusable for OverlayKeyProbe {
        fn is_focused(&self) -> bool {
            self.focus
        }
        fn focus(&mut self) {
            self.focus = true;
        }
        fn unfocus(&mut self) {
            self.focus = false;
        }
    }
    impl overlay::Overlay<(KeyboardOwner, bool), Theme, Renderer> for OverlayKeyProbe {
        fn layout(&mut self, _renderer: &Renderer, bounds: Size) -> layout::Node {
            layout::Node::new(bounds)
        }
        fn draw(
            &self,
            _renderer: &mut Renderer,
            _theme: &Theme,
            _style: &renderer::Style,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
        ) {
        }
        fn operate(
            &mut self,
            layout: Layout<'_>,
            _renderer: &Renderer,
            operation: &mut dyn Operation,
        ) {
            let id = self.id.clone();
            operation.focusable(Some(&id), layout.bounds(), self);
        }
        fn update(
            &mut self,
            _event: &Event,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _renderer: &Renderer,
            _clipboard: &mut dyn Clipboard,
            shell: &mut Shell<'_, (KeyboardOwner, bool)>,
        ) {
            if self.capture {
                shell.capture_event();
            }
        }
    }

    #[test]
    fn keyboard_overlay_capture_and_ignored_fallthrough_publish_exactly_one_owner() {
        use cosmic::iced::advanced::{clipboard, renderer::Headless};
        use cosmic::iced::{Font, Pixels};
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                Font::default(),
                Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let key = keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("z".into()),
            modified_key: keyboard::Key::Character("z".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyZ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            text: None,
            repeat: false,
        };
        for (focused, captured) in [(false, false), (false, true), (true, false), (true, true)] {
            let trace = InputTrace::default();
            let owner = KeyboardOwner {
                pane: FocusedPane::TextInput,
                card: None,
                generation: None,
            };
            let id = cosmic::iced::widget::Id::new("owned-overlay");
            let callback: KeyCallback<'_, (KeyboardOwner, bool)> =
                Rc::new(|_, captured, owner| (owner, captured));
            let mut overlay = ObservedOverlay::wrap(
                overlay::Element::new(Box::new(OverlayKeyProbe {
                    focus: focused,
                    capture: captured,
                    id: id.clone(),
                })),
                &trace,
                Some(Rc::new(HashMap::from([(id, owner)]))),
                Some(callback),
            );
            let node = overlay
                .as_overlay_mut()
                .layout(&renderer, Size::new(400.0, 200.0));
            let mut messages = Vec::new();
            let stopped = {
                let mut shell = Shell::new(&mut messages);
                overlay.as_overlay_mut().update(
                    &Event::Keyboard(key.clone()),
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut clipboard::Null,
                    &mut shell,
                );
                shell.is_event_captured()
            };
            if !stopped {
                let mut base = observe_keyboard(
                    cosmic::iced::widget::Space::new(),
                    &trace,
                    Vec::new(),
                    None,
                    true,
                    |_, captured, owner| (owner, captured),
                );
                let mut tree = Tree::new(&base);
                let node = base.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, Size::new(400.0, 200.0)),
                );
                base.as_widget_mut().update(
                    &mut tree,
                    &Event::Keyboard(key.clone()),
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &Rectangle::with_size(Size::new(400.0, 200.0)),
                );
            }
            assert_eq!(messages.len(), 1, "focused={focused} captured={captured}");
            assert_eq!(
                messages[0].0,
                if focused {
                    owner
                } else {
                    KeyboardOwner::UNKNOWN
                }
            );
            assert_eq!(messages[0].1, captured);
        }
    }
    #[derive(Debug, Clone)]
    enum MixedKey {
        Base(char),
        Native(keyboard::Event, bool),
    }
    struct MixedBase;
    struct SelectiveOverlay;
    impl overlay::Overlay<MixedKey, Theme, Renderer> for SelectiveOverlay {
        fn layout(&mut self, _renderer: &Renderer, bounds: Size) -> layout::Node {
            layout::Node::new(bounds)
        }
        fn draw(
            &self,
            _renderer: &mut Renderer,
            _theme: &Theme,
            _style: &renderer::Style,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
        ) {
        }
        fn update(
            &mut self,
            event: &Event,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _renderer: &Renderer,
            _clipboard: &mut dyn Clipboard,
            shell: &mut Shell<'_, MixedKey>,
        ) {
            if matches!(event, Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Character(c), .. }) if c == "b")
            {
                shell.capture_event();
            }
        }
    }
    impl Widget<MixedKey, Theme, Renderer> for MixedBase {
        fn size(&self) -> Size<Length> {
            Size::new(Length::Fill, Length::Fill)
        }
        fn layout(
            &mut self,
            _tree: &mut Tree,
            _renderer: &Renderer,
            limits: &layout::Limits,
        ) -> layout::Node {
            layout::Node::new(limits.max())
        }
        fn draw(
            &self,
            _tree: &Tree,
            _renderer: &mut Renderer,
            _theme: &Theme,
            _style: &renderer::Style,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _viewport: &Rectangle,
        ) {
        }
        fn update(
            &mut self,
            _tree: &mut Tree,
            event: &Event,
            _layout: Layout<'_>,
            _cursor: mouse::Cursor,
            _renderer: &Renderer,
            _clipboard: &mut dyn Clipboard,
            shell: &mut Shell<'_, MixedKey>,
            _viewport: &Rectangle,
        ) {
            if matches!(event, Event::Keyboard(keyboard::Event::KeyPressed { key: keyboard::Key::Character(c), .. }) if c == "a")
            {
                shell.publish(MixedKey::Base('a'));
            }
        }
        fn overlay<'a>(
            &'a mut self,
            _tree: &'a mut Tree,
            _layout: Layout<'a>,
            _renderer: &Renderer,
            _viewport: &Rectangle,
            _translation: Vector,
        ) -> Option<overlay::Element<'a, MixedKey, Theme, Renderer>> {
            Some(overlay::Element::new(Box::new(SelectiveOverlay)))
        }
    }

    #[test]
    fn actual_mixed_overlay_batch_has_one_native_message_per_key_without_order_or_delivery_ledger()
    {
        use cosmic::iced::advanced::{clipboard, renderer::Headless};
        use cosmic::iced::{
            Font, Pixels, event,
            runtime::{UserInterface, user_interface::Cache},
        };
        let mut renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(Renderer::new(
                Font::default(),
                Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let press = |c: &str, code| keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(c.into()),
            modified_key: keyboard::Key::Character(c.into()),
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some(c.into()),
            repeat: false,
        };
        let a = press("a", keyboard::key::Code::KeyA);
        let b = press("b", keyboard::key::Code::KeyB);
        let trace = InputTrace::default();
        let element = observe_keyboard(
            Element::new(MixedBase),
            &trace,
            Vec::new(),
            None,
            true,
            |event, captured, _| MixedKey::Native(event, captured),
        );
        let mut ui = UserInterface::build(
            element,
            Size::new(400.0, 200.0),
            Cache::new(),
            &mut renderer,
        );
        let mut messages = Vec::new();
        let (_, statuses) = ui.update(
            &[
                Event::Keyboard(a.clone()),
                Event::Keyboard(a.clone()),
                Event::Keyboard(b.clone()),
            ],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(statuses, vec![event::Status::Captured; 3]);
        // The actual pinned runtime processes captured B in the overlay pass
        // BEFORE the two earlier ignored As reach the base pass.
        assert!(matches!(messages.first(), Some(MixedKey::Native(event, true)) if event == &b));
        assert_eq!(
            messages
                .iter()
                .filter(|m| matches!(m, MixedKey::Base('a')))
                .count(),
            2
        );
        assert_eq!(
            messages
                .iter()
                .filter(|m| matches!(m, MixedKey::Native(event, false) if event == &a))
                .count(),
            2,
            "identical events retain multiplicity"
        );
        assert_eq!(
            messages
                .iter()
                .filter(|m| matches!(m, MixedKey::Native(_, _)))
                .count(),
            3
        );
        assert!(!keyboard_uses_legacy_fallback(&a) && !keyboard_uses_legacy_fallback(&b));
        // Captured non-special subscription copies are statelessly refused,
        // even if a copy is dropped or delivered long after another batch.
        assert!(
            statuses
                .iter()
                .all(|status| *status == event::Status::Captured)
        );

        let escape = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        let release = keyboard::Event::KeyReleased {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
        };
        let modifiers = keyboard::Event::ModifiersChanged(keyboard::Modifiers::CTRL);
        messages.clear();
        let (_, statuses) = ui.update(
            &[
                Event::Keyboard(escape.clone()),
                Event::Keyboard(release.clone()),
                Event::Keyboard(modifiers.clone()),
            ],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert!(
            messages.is_empty(),
            "legacy Escape press/release and modifiers never gain a native duplicate"
        );
        assert_eq!(statuses, vec![event::Status::Ignored; 3]);
        assert!(
            [escape, release, modifiers]
                .iter()
                .all(keyboard_uses_legacy_fallback)
        );
    }
}
