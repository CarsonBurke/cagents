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

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

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
pub struct InputTrace(
    Rc<Cell<Sample>>,
    Rc<Cell<Option<KeyboardRegion>>>,
    Rc<RefCell<Option<cosmic::iced::widget::Id>>>,
    Rc<RefCell<Option<crate::terminal::ReviewInputBarrier>>>,
);

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

    pub(crate) fn clear_keyboard_focus(&self) {
        self.1.set(None);
        self.2.borrow_mut().take();
    }

    /// Exact native focused control sampled with the current keyboard region.
    pub fn keyboard_widget_id(&self) -> Option<cosmic::iced::widget::Id> {
        self.2.borrow().clone()
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

    pub(crate) fn bind_terminal_keys(&self, barrier: crate::terminal::ReviewInputBarrier) {
        *self.3.borrow_mut() = Some(barrier);
    }
    fn finish_event<Message>(&self, event: &Event, shell: &mut Shell<'_, Message>) {
        if let Some(barrier) = self.3.borrow().as_ref() {
            barrier.finish_key_event(event);
            if barrier.flush_owned_keys() {
                shell.request_redraw_at(Instant::now() + std::time::Duration::from_millis(16));
            }
        }
    }
    /// Records one event the window delivered.
    pub(crate) fn record(&self, event: &Event) {
        if let Event::Keyboard(event) = event
            && let Some(barrier) = self.3.borrow().as_ref()
        {
            barrier.observe_key_event(event);
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

type KeyboardCallback<'a, Message> = Rc<
    dyn Fn(keyboard::Event, bool, KeyboardRegion, Option<cosmic::iced::widget::Id>) -> Message + 'a,
>;

/// Publish each keyboard event with its own native focus snapshot. Iced
/// batches widget events before subscription messages, so reading a shared
/// "latest focus" later can route an earlier chord to a different region.
pub fn observe_keyboard_events<'a, Message: 'a>(
    content: impl Into<Element<'a, Message>>,
    trace: &InputTrace,
    regions: Vec<(cosmic::iced::widget::Id, KeyboardRegion)>,
    on_key: impl Fn(keyboard::Event, bool, KeyboardRegion, Option<cosmic::iced::widget::Id>) -> Message
    + 'a,
) -> Element<'a, Message> {
    Element::new(Observed {
        content: content.into(),
        trace: trace.clone(),
        focus_regions: Some(regions),
        keyboard_callback: Some(Rc::new(on_key)),
    })
}

pub(crate) struct ScopedKeyboardRegion {
    focus: Rc<Cell<Option<KeyboardRegion>>>,
    previous: Option<KeyboardRegion>,
    widget: Rc<RefCell<Option<cosmic::iced::widget::Id>>>,
    previous_widget: Option<cosmic::iced::widget::Id>,
}
impl Drop for ScopedKeyboardRegion {
    fn drop(&mut self) {
        self.focus.set(self.previous);
        *self.widget.borrow_mut() = self.previous_widget.take();
    }
}
impl InputTrace {
    pub(crate) fn scoped_keyboard_focus(
        &self,
        region: KeyboardRegion,
        widget: Option<cosmic::iced::widget::Id>,
    ) -> ScopedKeyboardRegion {
        ScopedKeyboardRegion {
            widget: Rc::clone(&self.2),
            previous_widget: self.2.replace(widget),
            focus: Rc::clone(&self.1),
            previous: self.1.replace(Some(region)),
        }
    }
}

fn keyboard_event_needs_focus(event: &Event) -> bool {
    matches!(
        event,
        Event::Keyboard(keyboard::Event::KeyPressed { .. } | keyboard::Event::KeyReleased { .. })
            | Event::InputMethod(_)
    )
}

fn pressed_position(event: &Event, cursor: mouse::Cursor) -> Option<Point> {
    match event {
        Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => cursor.position(),
        Event::Touch(cosmic::iced::touch::Event::FingerPressed { position, .. }) => Some(*position),
        _ => None,
    }
}

/// Marks custom multiline text controls for native keyboard-focus queries.
/// Unlike text_input, iced's text_editor exposes only its focusable state.
pub(crate) struct TextEditing;

#[derive(Default)]
struct KeyboardFocus {
    focused: Option<(Option<cosmic::iced::widget::Id>, Rectangle)>,
    focused_candidates: Vec<(
        (Option<cosmic::iced::widget::Id>, Rectangle),
        Option<KeyboardRegion>,
    )>,
    inputs: Vec<(Option<cosmic::iced::widget::Id>, Rectangle)>,
    known_regions: HashMap<cosmic::iced::widget::Id, KeyboardRegion>,
    current_region: Option<KeyboardRegion>,
    pending_region: Option<Option<KeyboardRegion>>,
}
impl Operation for KeyboardFocus {
    fn container(&mut self, id: Option<&cosmic::iced::widget::Id>, _bounds: Rectangle) {
        self.pending_region = Some(
            id.and_then(|id| self.known_regions.get(id))
                .copied()
                .or(self.current_region),
        );
    }
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        let previous = self.current_region;
        self.current_region = self.pending_region.take().unwrap_or(previous);
        operate(self);
        self.current_region = previous;
        self.pending_region = None;
    }
    fn focusable(
        &mut self,
        id: Option<&cosmic::iced::widget::Id>,
        bounds: Rectangle,
        state: &mut dyn cosmic::iced::advanced::widget::operation::Focusable,
    ) {
        if state.is_focused() {
            let candidate = (id.cloned(), bounds);
            self.focused = Some(candidate.clone());
            self.focused_candidates
                .push((candidate, self.current_region));
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
        self.selected(regions)
            .map_or_else(|| FocusedPane::Other.into(), |(_, region)| region)
    }

    /// Resolve the identity and region together. Captured outside clicks can
    /// leave a raw card's old native focus flag set; that flag must not
    /// override a focused sidebar, inspector or dialog control.
    fn selected<'a>(
        &'a self,
        regions: &[(cosmic::iced::widget::Id, KeyboardRegion)],
    ) -> Option<(
        &'a (Option<cosmic::iced::widget::Id>, Rectangle),
        KeyboardRegion,
    )> {
        self.focused_candidates
            .iter()
            .rev()
            .find_map(|(candidate, inherited)| {
                let region = self.region_for(candidate, regions, *inherited);
                (region.card.is_none()
                    && matches!(
                        region.pane,
                        FocusedPane::TextInput | FocusedPane::OutsideWorkspace
                    ))
                .then_some((candidate, region))
            })
            .or_else(|| {
                self.focused_candidates
                    .iter()
                    .rev()
                    .find_map(|(candidate, inherited)| {
                        let region = self.region_for(candidate, regions, *inherited);
                        (region.pane == FocusedPane::TextInput || self.is_text_input(candidate))
                            .then_some((candidate, region))
                    })
            })
            .or_else(|| {
                self.focused_candidates
                    .iter()
                    .rev()
                    .find_map(|(candidate, inherited)| {
                        // A title can own a card logically while living
                        // outside its content. Prefer that native control
                        // over a raw pane retaining focus after a captured
                        // outside click, without overriding text controls.
                        inherited
                            .and_then(|region| region.card)
                            .is_none()
                            .then_some((candidate, self.region_for(candidate, regions, *inherited)))
                    })
            })
            .or_else(|| {
                self.focused_candidates
                    .last()
                    .map(|(candidate, inherited)| {
                        (candidate, self.region_for(candidate, regions, *inherited))
                    })
            })
    }

    fn widget_id(
        &self,
        regions: &[(cosmic::iced::widget::Id, KeyboardRegion)],
    ) -> Option<cosmic::iced::widget::Id> {
        self.selected(regions).and_then(|((id, _), _)| id.clone())
    }

    fn is_text_input(&self, candidate: &(Option<cosmic::iced::widget::Id>, Rectangle)) -> bool {
        self.inputs.iter().any(|input| input == candidate)
    }

    fn reconcile(
        &self,
        event: &Event,
        regions: &[(cosmic::iced::widget::Id, KeyboardRegion)],
    ) -> Option<RetainNativeFocus> {
        // Releases remain available for the original widget's paired-key
        // cleanup. The usual single-owner path needs no second traversal.
        if self.focused_candidates.len() <= 1
            || !matches!(
                event,
                Event::Keyboard(keyboard::Event::KeyPressed { .. }) | Event::InputMethod(_)
            )
        {
            return None;
        }
        self.selected(regions)
            .map(|(candidate, _)| RetainNativeFocus {
                owner: candidate.clone(),
                cleared: false,
            })
    }

    fn region_for(
        &self,
        (id, bounds): &(Option<cosmic::iced::widget::Id>, Rectangle),
        regions: &[(cosmic::iced::widget::Id, KeyboardRegion)],
        inherited: Option<KeyboardRegion>,
    ) -> KeyboardRegion {
        let direct = if self.known_regions.is_empty() {
            // Direct operation fixtures may supply regions after traversal.
            regions
                .iter()
                .find(|(candidate, _)| Some(candidate) == id.as_ref())
                .map(|(_, region)| *region)
        } else {
            id.as_ref()
                .and_then(|id| self.known_regions.get(id))
                .copied()
        };
        if let Some(region) = direct {
            return region;
        }
        let container = inherited.unwrap_or_else(|| FocusedPane::OutsideWorkspace.into());
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

/// Clear contradictory native flags before children can admit ignored input.
/// Do not refocus the owner: doing so can reset selection or IME state.
struct RetainNativeFocus {
    owner: (Option<cosmic::iced::widget::Id>, Rectangle),
    cleared: bool,
}

impl Operation for RetainNativeFocus {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn focusable(
        &mut self,
        id: Option<&cosmic::iced::widget::Id>,
        bounds: Rectangle,
        state: &mut dyn cosmic::iced::advanced::widget::operation::Focusable,
    ) {
        if state.is_focused() && (id != self.owner.0.as_ref() || bounds != self.owner.1) {
            state.unfocus();
            self.cleared = true;
        }
    }
}

/// Some native widgets skip outside-click handling after a sibling captures
/// the pointer. Complete only that missing unfocus step, after the chosen
/// widget has processed its own press. Never grant focus from hit testing.
struct UnfocusOutside {
    point: Point,
    cleared: bool,
}

impl Operation for UnfocusOutside {
    fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
        operate(self);
    }

    fn focusable(
        &mut self,
        _id: Option<&cosmic::iced::widget::Id>,
        bounds: Rectangle,
        state: &mut dyn cosmic::iced::advanced::widget::operation::Focusable,
    ) {
        if state.is_focused() && !bounds.contains(self.point) {
            state.unfocus();
            self.cleared = true;
        }
    }
}

struct Observed<'a, Message> {
    content: Element<'a, Message>,
    trace: InputTrace,
    focus_regions: Option<Vec<(cosmic::iced::widget::Id, KeyboardRegion)>>,
    keyboard_callback: Option<KeyboardCallback<'a, Message>>,
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
                known_regions: regions.iter().cloned().collect(),
                ..KeyboardFocus::default()
            };
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut focus,
            );
            self.trace.1.set(Some(focus.region(regions)));
            *self.trace.2.borrow_mut() = focus.widget_id(regions);
            if let Some(mut reconcile) = focus.reconcile(event, regions) {
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut reconcile,
                );
                if reconcile.cleared {
                    shell.request_redraw();
                }
            }
        } else if matches!(event, Event::Keyboard(_)) && self.focus_regions.is_some() {
            // Modifier updates without a focus sample must not reuse a
            // previous chord's pane after pointer focus changes.
            self.trace.1.set(None);
            self.trace.2.borrow_mut().take();
        }
        let region = self
            .trace
            .1
            .get()
            .unwrap_or_else(|| FocusedPane::Other.into());
        let widget = self.trace.keyboard_widget_id();
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
        if self.focus_regions.is_some()
            && let Some(point) = pressed_position(event, cursor)
        {
            let mut unfocus = UnfocusOutside {
                point,
                cleared: false,
            };
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut unfocus,
            );
            if unfocus.cleared {
                shell.request_redraw();
            }
        }
        if let Event::Keyboard(event) = event
            && let Some(callback) = &self.keyboard_callback
        {
            let child_captured = shell.is_event_captured();
            shell.publish(callback(event.clone(), child_captured, region, widget));
        }
        self.trace.finish_event(event, shell);
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
                    self.keyboard_callback.clone(),
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
    keyboard_callback: Option<KeyboardCallback<'a, Message>>,
}

impl<'a, Message: 'a> ObservedOverlay<'a, Message> {
    fn wrap(
        content: overlay::Element<'a, Message, Theme, Renderer>,
        trace: &InputTrace,
        focus_regions: Option<Vec<(cosmic::iced::widget::Id, KeyboardRegion)>>,
        keyboard_callback: Option<KeyboardCallback<'a, Message>>,
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
        if keyboard_event_needs_focus(event)
            && let Some(regions) = &self.focus_regions
        {
            let mut focus = KeyboardFocus {
                known_regions: regions.iter().cloned().collect(),
                ..KeyboardFocus::default()
            };
            self.content
                .as_overlay_mut()
                .operate(layout, renderer, &mut focus);
            owns_keyboard = focus.focused.is_some();
            self.trace.1.set(Some(focus.region(regions)));
            *self.trace.2.borrow_mut() = focus.widget_id(regions);
            if let Some(mut reconcile) = focus.reconcile(event, regions) {
                self.content
                    .as_overlay_mut()
                    .operate(layout, renderer, &mut reconcile);
                if reconcile.cleared {
                    shell.request_redraw();
                }
            }
        }
        let region = self
            .trace
            .1
            .get()
            .unwrap_or_else(|| FocusedPane::OutsideWorkspace.into());
        let widget = self.trace.keyboard_widget_id();
        self.content
            .as_overlay_mut()
            .update(event, layout, cursor, renderer, clipboard, shell);
        if self.focus_regions.is_some()
            && let Some(point) = pressed_position(event, cursor)
        {
            let mut unfocus = UnfocusOutside {
                point,
                cleared: false,
            };
            self.content
                .as_overlay_mut()
                .operate(layout, renderer, &mut unfocus);
            if unfocus.cleared {
                shell.request_redraw();
            }
        }
        // An ignored composition event still belongs to the focused overlay.
        // Read-only fields and buttons intentionally do not edit its text.
        if owns_keyboard && matches!(event, Event::InputMethod(_)) {
            shell.capture_event();
        }
        if let Event::Keyboard(event) = event
            && let Some(callback) = &self.keyboard_callback
        {
            let child_captured = shell.is_event_captured();
            // Reserve new input, while leaving an ignored release available
            // to the raw widget that admitted its corresponding press.
            if owns_keyboard && matches!(event, keyboard::Event::KeyPressed { .. }) {
                shell.capture_event();
            }
            if shell.is_event_captured() {
                shell.publish(callback(event.clone(), child_captured, region, widget));
            }
        }
        self.trace.finish_event(event, shell);
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
        let keyboard_callback = self.keyboard_callback.clone();
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

    #[test]
    fn native_controls_outrank_retained_raw_flags_without_losing_text_priority() {
        use cosmic::iced::advanced::widget::operation::Focusable;
        struct Focus;
        impl Focusable for Focus {
            fn is_focused(&self) -> bool {
                true
            }
            fn focus(&mut self) {}
            fn unfocus(&mut self) {}
        }
        let raw = cosmic::iced::widget::Id::new("conflict.raw");
        let container = cosmic::iced::widget::Id::new("conflict.card");
        let control = cosmic::iced::widget::Id::new("conflict.control");
        let text = cosmic::iced::widget::Id::new("conflict.text");
        let bounds = Rectangle::with_size(Size::new(100.0, 30.0));
        let raw_region = KeyboardRegion {
            pane: FocusedPane::Terminal,
            card: Some(CardId::new()),
        };
        for card in [None, Some(CardId::new())] {
            let control_region = KeyboardRegion {
                pane: FocusedPane::Other,
                card,
            };
            for reverse in [false, true] {
                for text_pane in [
                    None,
                    Some(FocusedPane::TextInput),
                    Some(FocusedPane::Editor),
                    Some(FocusedPane::OutsideWorkspace),
                ] {
                    let mut regions = vec![
                        (container.clone(), raw_region),
                        (raw.clone(), raw_region),
                        (control.clone(), control_region),
                    ];
                    if let Some(pane) = text_pane {
                        regions.push((text.clone(), pane.into()));
                    }
                    let mut query = KeyboardFocus {
                        known_regions: regions.iter().cloned().collect(),
                        ..KeyboardFocus::default()
                    };
                    let mut ids = vec![&raw, &control];
                    if text_pane.is_some() {
                        ids.push(&text);
                    }
                    if reverse {
                        ids.reverse();
                    }
                    for id in ids {
                        if id == &raw {
                            query.container(Some(&container), bounds);
                            query.traverse(&mut |operation| {
                                operation.focusable(Some(id), bounds, &mut Focus)
                            });
                        } else {
                            query.focusable(Some(id), bounds, &mut Focus);
                            if id == &text
                                && matches!(
                                    text_pane,
                                    Some(FocusedPane::Editor | FocusedPane::TextInput)
                                )
                            {
                                query.custom(Some(id), bounds, &mut TextEditing);
                            }
                        }
                    }
                    let expected = if text_pane.is_some() { &text } else { &control };
                    assert_eq!(
                        query.widget_id(&regions).as_ref(),
                        Some(expected),
                        "reverse={reverse} text={text_pane:?} card={card:?}"
                    );
                    if text_pane.is_none() {
                        assert_eq!(query.region(&regions), control_region);
                    }
                }
            }
        }
    }

    #[derive(Clone, Debug, PartialEq)]
    enum FocusConflictMessage {
        Click,
        Edited(String),
        RawInput,
        RawRelease,
        Key(KeyboardRegion, Option<cosmic::iced::widget::Id>),
    }

    /// Models a raw native control that missed an outside pointer event,
    /// while honoring the same explicit Focusable contract as real panes.
    struct RetainedRaw {
        id: cosmic::iced::widget::Id,
        focused: Rc<Cell<bool>>,
    }
    impl cosmic::iced::advanced::widget::operation::Focusable for RetainedRaw {
        fn is_focused(&self) -> bool {
            self.focused.get()
        }
        fn focus(&mut self) {
            self.focused.set(true);
        }
        fn unfocus(&mut self) {
            self.focused.set(false);
        }
    }
    impl Widget<FocusConflictMessage, Theme, Renderer> for RetainedRaw {
        fn size(&self) -> Size<Length> {
            Size::new(Length::Fixed(160.0), Length::Fixed(40.0))
        }
        fn layout(
            &mut self,
            _tree: &mut Tree,
            _renderer: &Renderer,
            _limits: &layout::Limits,
        ) -> layout::Node {
            layout::Node::new(Size::new(160.0, 40.0))
        }
        fn operate(
            &mut self,
            _tree: &mut Tree,
            layout: Layout<'_>,
            _renderer: &Renderer,
            operation: &mut dyn Operation,
        ) {
            operation.focusable(Some(&self.id.clone()), layout.bounds(), self);
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
            layout: Layout<'_>,
            cursor: mouse::Cursor,
            _renderer: &Renderer,
            _clipboard: &mut dyn Clipboard,
            shell: &mut Shell<'_, FocusConflictMessage>,
            _viewport: &Rectangle,
        ) {
            if matches!(
                event,
                Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left))
            ) && cursor.is_over(layout.bounds())
            {
                self.focused.set(true);
                shell.capture_event();
            }
            if self.focused.get()
                && matches!(
                    event,
                    Event::Keyboard(keyboard::Event::KeyPressed { .. }) | Event::InputMethod(_)
                )
            {
                shell.publish(FocusConflictMessage::RawInput);
            }
            if self.focused.get()
                && matches!(event, Event::Keyboard(keyboard::Event::KeyReleased { .. }))
            {
                shell.publish(FocusConflictMessage::RawRelease);
            }
        }
    }

    #[test]
    fn observer_clears_retained_raw_admission_before_native_control_input() {
        use cosmic::iced::advanced::{clipboard, widget::operation::focusable};
        use cosmic::iced::keyboard::key::{Code, Physical};
        let renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let viewport = Rectangle::with_size(Size::new(400.0, 100.0));
        for control_first in [true, false] {
            // Buttons get keyboard focus through native operations; actual
            // pointer focus is separately covered with a stock text field.
            for (text_field, card) in [(false, None), (false, Some(CardId::new())), (true, None)] {
                let trace = InputTrace::default();
                let control = cosmic::iced::widget::Id::unique();
                let raw_id = cosmic::iced::widget::Id::unique();
                let container = cosmic::iced::widget::Id::unique();
                let raw_focused = Rc::new(Cell::new(true));
                let raw: Element<'_, FocusConflictMessage> = cosmic::widget::id_container(
                    Element::new(RetainedRaw {
                        id: raw_id.clone(),
                        focused: raw_focused.clone(),
                    }),
                    container.clone(),
                )
                .into();
                let native: Element<'_, FocusConflictMessage> = if text_field {
                    cosmic::widget::text_input("Query", "retained")
                        .id(control.clone())
                        .on_input(FocusConflictMessage::Edited)
                        .width(160.0)
                        .into()
                } else {
                    cosmic::widget::button::text("Native action")
                        .id(control.clone())
                        .on_press(FocusConflictMessage::Click)
                        .width(160.0)
                        .into()
                };
                let row = cosmic::widget::row::with_children(if control_first {
                    vec![native, raw]
                } else {
                    vec![raw, native]
                });
                let control_region = KeyboardRegion {
                    pane: if text_field {
                        FocusedPane::TextInput
                    } else {
                        FocusedPane::Other
                    },
                    card,
                };
                let raw_region = KeyboardRegion {
                    pane: FocusedPane::Terminal,
                    card: Some(CardId::new()),
                };
                let mut element = observe_keyboard_events(
                    row,
                    &trace,
                    vec![
                        (control.clone(), control_region),
                        (raw_id.clone(), raw_region),
                        (container, raw_region),
                    ],
                    |_, _, region, widget| FocusConflictMessage::Key(region, widget),
                );
                let mut tree = Tree::new(element.as_widget());
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(viewport.size(), viewport.size()),
                );
                let mut messages = Vec::new();
                if text_field {
                    let point = Point::new(if control_first { 40.0 } else { 200.0 }, 15.0);
                    let mut shell = Shell::new(&mut messages);
                    element.as_widget_mut().update(
                        &mut tree,
                        &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                        Layout::new(&node),
                        mouse::Cursor::Available(point),
                        &renderer,
                        &mut clipboard::Null,
                        &mut shell,
                        &viewport,
                    );
                    assert!(
                        shell.is_event_captured(),
                        "actual field pointer focus must be captured"
                    );
                } else {
                    element.as_widget_mut().operate(
                        &mut tree,
                        Layout::new(&node),
                        &renderer,
                        &mut focusable::focus::<()>(control.clone()),
                    );
                    // Recreate the contradictory retained native flag after
                    // the ordinary focus operation, not an invented owner.
                    raw_focused.set(true);
                }
                if text_field {
                    assert!(
                        !raw_focused.get(),
                        "captured field pointer clears outside raw focus"
                    );
                    // Also cover contradictions inherited from before this
                    // observer was mounted, including parked native trees.
                    raw_focused.set(true);
                }
                assert!(
                    raw_focused.get(),
                    "fixture requires retained raw native focus"
                );
                for event in [
                    Event::Keyboard(keyboard::Event::KeyPressed {
                        key: keyboard::Key::Character("q".into()),
                        modified_key: keyboard::Key::Character("q".into()),
                        physical_key: Physical::Code(Code::KeyQ),
                        location: keyboard::Location::Standard,
                        modifiers: keyboard::Modifiers::CTRL,
                        text: None,
                        repeat: false,
                    }),
                    Event::InputMethod(cosmic::iced::advanced::input_method::Event::Commit(
                        "ime".into(),
                    )),
                ] {
                    messages.clear();
                    let had_retained_raw = raw_focused.get();
                    {
                        let mut shell = Shell::new(&mut messages);
                        element.as_widget_mut().update(
                            &mut tree,
                            &event,
                            Layout::new(&node),
                            mouse::Cursor::Unavailable,
                            &renderer,
                            &mut clipboard::Null,
                            &mut shell,
                            &viewport,
                        );
                        if had_retained_raw {
                            assert_eq!(
                                shell.redraw_request(),
                                cosmic::iced::window::RedrawRequest::NextFrame,
                                "clearing a native focus flag must refresh focus visuals"
                            );
                        }
                    }
                    assert!(
                        !messages.contains(&FocusConflictMessage::RawInput),
                        "{messages:?}"
                    );
                    if text_field && matches!(&event, Event::InputMethod(_)) {
                        let edited: Vec<_> = messages
                            .iter()
                            .filter_map(|message| {
                                if let FocusConflictMessage::Edited(value) = message {
                                    Some(value)
                                } else {
                                    None
                                }
                            })
                            .collect();
                        assert_eq!(
                            edited.len(),
                            1,
                            "native field must positively receive the IME commit: {messages:?}"
                        );
                        assert_eq!(edited[0].matches("ime").count(), 1);
                        assert_eq!(
                            edited[0].replacen("ime", "", 1),
                            "retained",
                            "native IME insertion preserves the existing field value"
                        );
                    }
                    assert!(
                        !raw_focused.get(),
                        "conflicting native flag must be cleared before child delivery"
                    );
                    assert_eq!(trace.keyboard_widget_id(), Some(control.clone()));
                    assert_eq!(trace.keyboard_focus(), Some(control_region.pane));
                    assert_eq!(trace.keyboard_card(), card);
                }
                // The selected native control remains focused, without a
                // second focus() that could disturb its selection or IME.
                let mut query = KeyboardFocus::default();
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut query,
                );
                assert_eq!(query.focused_candidates.len(), 1);
                assert_eq!(
                    query.focused.as_ref().and_then(|(id, _)| id.clone()),
                    Some(control)
                );

                // A raw pane may capture its pointer press before a later
                // native button can clear its old flag. Ownership must also
                // return in this direction, rather than pinning the toolbar.
                let raw_point = Point::new(if control_first { 200.0 } else { 40.0 }, 15.0);
                messages.clear();
                {
                    let mut shell = Shell::new(&mut messages);
                    element.as_widget_mut().update(
                        &mut tree,
                        &Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
                        Layout::new(&node),
                        mouse::Cursor::Available(raw_point),
                        &renderer,
                        &mut clipboard::Null,
                        &mut shell,
                        &viewport,
                    );
                    assert!(shell.is_event_captured());
                }
                assert!(raw_focused.get());
                messages.clear();
                let key = Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character("q".into()),
                    modified_key: keyboard::Key::Character("q".into()),
                    physical_key: Physical::Code(Code::KeyQ),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::CTRL,
                    text: None,
                    repeat: false,
                });
                element.as_widget_mut().update(
                    &mut tree,
                    &key,
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &viewport,
                );
                assert!(
                    messages.contains(&FocusConflictMessage::RawInput),
                    "{messages:?}"
                );
                assert_eq!(trace.keyboard_widget_id(), Some(raw_id));
                assert_eq!(trace.keyboard_focus(), Some(FocusedPane::Terminal));
                assert_eq!(trace.keyboard_card(), raw_region.card);
            }
        }
    }

    #[test]
    fn focused_native_overlay_owns_ignored_ime_but_leaves_unowned_input_available() {
        use cosmic::iced::advanced::{clipboard, widget::operation::focusable};
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        for kind in [
            "button",
            "editable-on-focus",
            "no-handler",
            "editable",
            "unfocused",
        ] {
            let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
            let trace = InputTrace::default();
            let raw_id = cosmic::iced::widget::Id::unique();
            let control = cosmic::iced::widget::Id::unique();
            let raw_focused = Rc::new(Cell::new(true));
            let raw = Element::new(RetainedRaw {
                id: raw_id.clone(),
                focused: raw_focused.clone(),
            });
            let popup: Element<'_, FocusConflictMessage> = match kind {
                // Cosmic focus explicitly enables editing, even when the
                // builder initially requests editing(false).
                "editable-on-focus" => cosmic::widget::text_input("Overlay", "retained")
                    .id(control.clone())
                    .editing(false)
                    .on_input(FocusConflictMessage::Edited)
                    .into(),
                "no-handler" => cosmic::widget::text_input("Overlay", "retained")
                    .id(control.clone())
                    .into(),
                "editable" => cosmic::widget::text_input("Overlay", "retained")
                    .id(control.clone())
                    .on_input(FocusConflictMessage::Edited)
                    .into(),
                _ => cosmic::widget::button::text("Overlay action")
                    .id(control.clone())
                    .on_press(FocusConflictMessage::Click)
                    .into(),
            };
            let regions = vec![
                (
                    raw_id,
                    KeyboardRegion {
                        pane: FocusedPane::Terminal,
                        card: Some(CardId::new()),
                    },
                ),
                (control.clone(), FocusedPane::OutsideWorkspace.into()),
            ];
            let element = observe_keyboard_events(
                cosmic::widget::popover(raw).popup(popup),
                &trace,
                regions.clone(),
                |_, _, region, widget| FocusConflictMessage::Key(region, widget),
            );
            let mut ui = UserInterface::build(
                element,
                Size::new(400.0, 180.0),
                Cache::new(),
                &mut renderer,
            );
            if kind != "unfocused" {
                ui.operate(&renderer, &mut focusable::focus::<()>(control.clone()));
                // Reproduce a retained raw native flag without replacing the
                // actual focusable overlay widget or bypassing its operation.
                raw_focused.set(true);
                let mut query = KeyboardFocus {
                    known_regions: regions.iter().cloned().collect(),
                    ..KeyboardFocus::default()
                };
                ui.operate(&renderer, &mut query);
                assert_eq!(query.widget_id(&regions), Some(control));
            }
            let mut messages = Vec::new();
            let _ = ui.update(
                &[Event::InputMethod(
                    cosmic::iced::advanced::input_method::Event::Commit("ime".into()),
                )],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(
                messages.contains(&FocusConflictMessage::RawInput),
                kind == "unfocused",
                "{kind}: {messages:?}"
            );
            assert!(
                raw_focused.get(),
                "the negative must be proved by overlay ownership, not by clearing the raw fixture"
            );
            let edited: Vec<_> = messages
                .iter()
                .filter_map(|message| match message {
                    FocusConflictMessage::Edited(value) => Some(value),
                    _ => None,
                })
                .collect();
            if matches!(kind, "editable" | "editable-on-focus") {
                assert_eq!(edited.len(), 1, "{messages:?}");
                assert_eq!(edited[0].matches("ime").count(), 1);
                assert_eq!(edited[0].replacen("ime", "", 1), "retained");
            } else {
                assert!(edited.is_empty(), "{kind}: {messages:?}");
            }
        }
    }

    #[test]
    fn focused_overlay_leaves_an_ignored_release_for_the_original_raw_child() {
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{
            advanced::{clipboard, widget::operation::focusable},
            keyboard::key::{Code, Physical},
        };
        let mut renderer = crate::ui_controls::accessibility_probe::headless_renderer();
        let raw = cosmic::iced::widget::Id::unique();
        let control = cosmic::iced::widget::Id::unique();
        let retained = Rc::new(Cell::new(true));
        let trace = InputTrace::default();
        let element = observe_keyboard_events(
            cosmic::widget::popover(Element::new(RetainedRaw {
                id: raw.clone(),
                focused: retained.clone(),
            }))
            .popup(
                cosmic::widget::button::text("Overlay action")
                    .id(control.clone())
                    .on_press(FocusConflictMessage::Click),
            ),
            &trace,
            vec![
                (
                    raw,
                    KeyboardRegion {
                        pane: FocusedPane::Terminal,
                        card: Some(CardId::new()),
                    },
                ),
                (control.clone(), FocusedPane::OutsideWorkspace.into()),
            ],
            |_, _, region, widget| FocusConflictMessage::Key(region, widget),
        );
        let mut ui = UserInterface::build(
            element,
            Size::new(400.0, 180.0),
            Cache::new(),
            &mut renderer,
        );
        let down = Event::Keyboard(keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("r".into()),
            modified_key: keyboard::Key::Character("r".into()),
            physical_key: Physical::Code(Code::KeyR),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some("r".into()),
            repeat: false,
        });
        let up = Event::Keyboard(keyboard::Event::KeyReleased {
            key: keyboard::Key::Character("r".into()),
            modified_key: keyboard::Key::Character("r".into()),
            physical_key: Physical::Code(Code::KeyR),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL,
        });
        let mut messages = Vec::new();
        let _ = ui.update(
            &[down],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| **message == FocusConflictMessage::RawInput)
                .count(),
            1
        );
        ui.operate(&renderer, &mut focusable::focus::<()>(control));
        // Keep the original raw consumer's ownership across overlay focus;
        // this test qualifies transport, not PTY receipt/pairing semantics.
        retained.set(true);
        messages.clear();
        let _ = ui.update(
            &[up],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(
            messages
                .iter()
                .filter(|message| **message == FocusConflictMessage::RawRelease)
                .count(),
            1,
            "{messages:?}"
        );
    }

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
        // A captured sidebar click can prevent a raw pane from seeing the
        // outside click that normally clears its old native focus flag.
        let regions = [(
            editor.clone(),
            KeyboardRegion {
                pane: FocusedPane::Editor,
                card: Some(CardId::new()),
            },
        )];
        let pane_bounds = Rectangle::new(Point::new(200., 0.), Size::new(100., 24.));
        query.focusable(Some(&editor), pane_bounds, &mut Focus(true));
        assert_eq!(query.region(&regions), FocusedPane::TextInput.into());
        assert_eq!(query.widget_id(&regions), Some(sidebar.clone()));
        let mut reversed = KeyboardFocus::default();
        reversed.focusable(Some(&editor), pane_bounds, &mut Focus(true));
        reversed.focusable(Some(&sidebar), bounds, &mut Focus(true));
        reversed.custom(Some(&sidebar), bounds, &mut TextEditing);
        assert_eq!(reversed.region(&regions), FocusedPane::TextInput.into());
        assert_eq!(reversed.widget_id(&regions), Some(sidebar));

        // Ownership follows actual traversal ancestry, even when an offscreen
        // pane's rectangle overlaps a sidebar or an overlay control.
        let container = cosmic::iced::widget::Id::new("pane.container");
        let field = cosmic::iced::widget::Id::new("pane.find");
        let owner = KeyboardRegion {
            pane: FocusedPane::Other,
            card: Some(CardId::new()),
        };
        let mut ancestry = KeyboardFocus {
            known_regions: [(container.clone(), owner)].into_iter().collect(),
            ..KeyboardFocus::default()
        };
        ancestry.container(Some(&container), bounds);
        ancestry.traverse(&mut |operation| {
            operation.focusable(Some(&field), bounds, &mut Focus(true));
            operation.custom(Some(&field), bounds, &mut TextEditing);
        });
        assert_eq!(
            ancestry.region(&[]),
            KeyboardRegion {
                pane: FocusedPane::TextInput,
                ..owner
            }
        );
        let outside = cosmic::iced::widget::Id::new("outside.same-bounds");
        ancestry.focusable(Some(&outside), bounds, &mut Focus(true));
        ancestry.custom(Some(&outside), bounds, &mut TextEditing);
        assert_eq!(ancestry.region(&[]), FocusedPane::TextInput.into());
        assert_eq!(ancestry.widget_id(&[]), Some(outside));
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
        let later = cosmic::iced::widget::Id::new("later-widget");
        let original = cosmic::iced::widget::Id::new("original-widget");
        *trace.2.borrow_mut() = Some(later.clone());
        {
            let _event = trace.scoped_keyboard_focus(editor, Some(original.clone()));
            assert_eq!(trace.keyboard_widget_id(), Some(original));
            assert_eq!(trace.keyboard_focus(), Some(FocusedPane::Editor));
            assert_eq!(trace.keyboard_card(), editor.card);
        }
        assert_eq!(trace.keyboard_focus(), Some(FocusedPane::TextInput));
        assert_eq!(trace.keyboard_card(), None);
        assert_eq!(trace.keyboard_widget_id(), Some(later));
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
