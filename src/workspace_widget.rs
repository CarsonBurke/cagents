use std::{collections::HashMap, mem, sync::Arc, time::Duration};

use cosmic::iced::advanced::Renderer as _;
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use cosmic::iced::widget;
use cosmic::iced::{
    Event, Length, Point, Rectangle, Size, Subscription, Vector, keyboard, mouse, time::Instant,
    window,
};
use cosmic::{Element, Renderer, Task, Theme};

use crate::{
    model::CardId,
    settings::{KeyboardShortcuts, ShortcutBinding, ShortcutKey, format_chords, parse_shortcut},
    workspace::{FocusDirection, FocusHandoff, FocusTarget, ViewportProjection, Workspace},
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportSample {
    pub offset: f32,
    pub width: f32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkspaceViewportEvent {
    Scrolled(ViewportSample),
    Resized(f32),
    PointerFocused(CardId),
    PointerEntered(CardId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalShortcut {
    NewTerminal,
    NewChannel,
    ClonePane,
    CloseFocusedCard,
    FocusProjectSearch,
    FocusAdjacent(FocusDirection),
    CyclePaneWidth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutDescriptor {
    pub action: GlobalShortcut,
    pub label: &'static str,
    pub repeatable: bool,
}

pub const GLOBAL_SHORTCUT_DESCRIPTORS: &[ShortcutDescriptor] = &[
    ShortcutDescriptor {
        action: GlobalShortcut::NewTerminal,
        label: "New terminal",
        repeatable: false,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::NewChannel,
        label: "New channel",
        repeatable: false,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ClonePane,
        label: "Clone focused pane",
        repeatable: false,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CloseFocusedCard,
        label: "Close focused column",
        repeatable: false,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusProjectSearch,
        label: "Search projects",
        repeatable: false,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusAdjacent(FocusDirection::Left),
        label: "Previous column",
        repeatable: true,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusAdjacent(FocusDirection::Right),
        label: "Next column",
        repeatable: true,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CyclePaneWidth,
        label: "Cycle pane width",
        repeatable: true,
    },
];

pub fn shortcut_descriptor(action: GlobalShortcut) -> &'static ShortcutDescriptor {
    GLOBAL_SHORTCUT_DESCRIPTORS
        .iter()
        .find(|descriptor| descriptor.action == action)
        .expect("every dispatched global shortcut has a descriptor")
}

/// Every configured chord; newer actions may be explicitly unbound.
pub fn shortcut_keys(action: GlobalShortcut, shortcuts: &KeyboardShortcuts) -> &[String] {
    match action {
        GlobalShortcut::NewTerminal => &shortcuts.new_terminal,
        GlobalShortcut::NewChannel => &shortcuts.new_channel,
        GlobalShortcut::ClonePane => &shortcuts.clone_pane,
        GlobalShortcut::CloseFocusedCard => &shortcuts.close_focused_column,
        GlobalShortcut::FocusProjectSearch => &shortcuts.search_projects,
        GlobalShortcut::FocusAdjacent(FocusDirection::Left) => &shortcuts.previous_column,
        GlobalShortcut::FocusAdjacent(FocusDirection::Right) => &shortcuts.next_column,
        GlobalShortcut::CyclePaneWidth => &shortcuts.cycle_pane_width,
    }
}

/// The chords bound to `action`, in the one-line form the settings dialog
/// reads back.
pub fn shortcut_chords(action: GlobalShortcut, shortcuts: &KeyboardShortcuts) -> String {
    format_chords(shortcut_keys(action, shortcuts))
}

pub fn shortcut_tooltip(action: GlobalShortcut, shortcuts: &KeyboardShortcuts) -> String {
    let descriptor = shortcut_descriptor(action);
    if shortcut_keys(action, shortcuts).is_empty() {
        return format!("{} — unbound", descriptor.label);
    }
    format!(
        "{} — {}",
        descriptor.label,
        shortcut_chords(action, shortcuts)
    )
}

#[derive(Debug, Default)]
pub struct FrameClock {
    last: Option<Instant>,
}

impl FrameClock {
    pub fn elapsed(&mut self, frame: Instant) -> Option<Duration> {
        let Some(previous) = self.last else {
            self.last = Some(frame);
            return None;
        };
        let elapsed = frame.checked_duration_since(previous)?;
        self.last = Some(frame);
        // Occluded Wayland windows can receive frames only once per second.
        // Bound catch-up work without starving their animations indefinitely.
        Some(elapsed.min(Duration::from_millis(250)))
    }

    /// The frame this clock last advanced to.
    ///
    /// The shell re-runs the whole update-and-layout pass for one
    /// `RedrawRequested` while any pass publishes a message, rebuilding the
    /// view in between, so a view that read the wall clock would measure a
    /// different animation on every pass and never settle. Animated geometry
    /// reads the frame instead, which makes the view a function of the frame.
    pub fn frame(&self) -> Option<Instant> {
        self.last
    }

    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// What a card needs to know about itself to render.
///
/// The card's widget identities are deliberately absent: they are stable per
/// card for the app's whole lifetime, so rebuilding them here would allocate
/// once per mounted card per frame. The owner passes them in instead.
#[derive(Debug, Clone, Copy)]
pub struct CardViewContext {
    pub id: CardId,
    pub focused: bool,
}

pub fn card_widget_id(id: CardId) -> widget::Id {
    widget::Id::new(format!("cagents-workspace-card-{id}"))
}

pub fn card_focus_id(id: CardId) -> widget::Id {
    widget::Id::new(format!("cagents-workspace-card-{id}-focus"))
}

pub fn animation_frames<Message, F>(app_window: window::Id, map: F) -> Subscription<Message>
where
    Message: Send + 'static,
    F: Fn(Instant) -> Message + Send + Clone + 'static,
{
    fn matching_window(
        (expected, (actual, instant)): (window::Id, (window::Id, Instant)),
    ) -> Option<Instant> {
        (expected == actual).then_some(instant)
    }

    window::frames()
        .with(app_window)
        .filter_map(matching_window)
        .map(map)
}

pub fn scroll_to<Message: 'static>(id: widget::Id, offset: f32) -> Task<Message> {
    widget::scrollable::scroll_to(
        id,
        widget::scrollable::AbsoluteOffset {
            x: Some(offset.max(0.0)),
            y: None,
        },
    )
}

pub fn focus_card<Message: 'static>(id: CardId) -> Task<Message> {
    widget::operation::focus(card_focus_id(id))
}

pub fn focus_handoff<Message: 'static>(
    handoff: FocusHandoff,
    toolbar_focus_id: widget::Id,
) -> Task<Message> {
    match handoff.target {
        FocusTarget::Card(id) => focus_card(id),
        FocusTarget::WorkspaceToolbar => widget::operation::focus(toolbar_focus_id),
        FocusTarget::Unchanged => Task::none(),
    }
}

pub fn focus_handoff_then<Message>(
    handoff: FocusHandoff,
    toolbar_focus_id: widget::Id,
    after_focus: Message,
) -> Task<Message>
where
    Message: Send + 'static,
{
    focus_handoff(handoff, toolbar_focus_id).chain(Task::done(after_focus))
}

pub fn global_shortcut(
    event: &keyboard::Event,
    shortcuts: &KeyboardShortcuts,
) -> Option<GlobalShortcut> {
    let keyboard::Event::KeyPressed {
        key,
        physical_key,
        modifiers,
        repeat,
        ..
    } = event
    else {
        return None;
    };
    let descriptor = global_shortcut_descriptor(key, *physical_key, *modifiers, shortcuts)?;
    (!*repeat || descriptor.repeatable).then_some(descriptor.action)
}

pub(crate) fn is_global_shortcut_chord(
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
    shortcuts: &KeyboardShortcuts,
) -> bool {
    crate::tab_history::is_chord(key, modifiers)
        || global_shortcut_descriptor(key, physical_key, modifiers, shortcuts).is_some()
}

fn global_shortcut_descriptor(
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
    shortcuts: &KeyboardShortcuts,
) -> Option<&'static ShortcutDescriptor> {
    GLOBAL_SHORTCUT_DESCRIPTORS.iter().find(|descriptor| {
        shortcut_keys(descriptor.action, shortcuts)
            .iter()
            .any(|chord| {
                parse_shortcut(chord)
                    .is_ok_and(|binding| shortcut_matches(binding, key, physical_key, modifiers))
            })
    })
}

fn shortcut_matches(
    binding: ShortcutBinding,
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
) -> bool {
    // Read flag by flag, so a lock modifier the user happens to be holding is
    // simply not consulted.
    binding.control == modifiers.control()
        && binding.alt == modifiers.alt()
        && binding.shift == modifiers.shift()
        && binding.logo == modifiers.logo()
        && match binding.key {
            ShortcutKey::Character(expected) if expected.is_ascii_digit() => {
                physical_digit(physical_key) == Some(expected)
            }
            ShortcutKey::Character(expected) => latin_key(key, physical_key) == Some(expected),
            ShortcutKey::ArrowLeft => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::ArrowLeft)
            ),
            ShortcutKey::ArrowRight => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::ArrowRight)
            ),
        }
}

fn latin_key(key: &keyboard::Key, physical_key: keyboard::key::Physical) -> Option<char> {
    if let keyboard::Key::Character(value) = key.as_ref() {
        let mut characters = value.chars();
        if let (Some(character), None) = (characters.next(), characters.next())
            && character.is_ascii_alphanumeric()
        {
            return Some(character.to_ascii_lowercase());
        }
    }
    key.to_latin(physical_key)
        .map(|key| key.to_ascii_lowercase())
}

fn physical_digit(physical_key: keyboard::key::Physical) -> Option<char> {
    use keyboard::key::Code;

    let keyboard::key::Physical::Code(code) = physical_key else {
        return None;
    };
    match code {
        Code::Digit0 => Some('0'),
        Code::Digit1 => Some('1'),
        Code::Digit2 => Some('2'),
        Code::Digit3 => Some('3'),
        Code::Digit4 => Some('4'),
        Code::Digit5 => Some('5'),
        Code::Digit6 => Some('6'),
        Code::Digit7 => Some('7'),
        Code::Digit8 => Some('8'),
        Code::Digit9 => Some('9'),
        _ => None,
    }
}

/// Matches a parsed chord against a keyboard event using the product's one
/// chord grammar.
///
/// `ceditor` builds on this crate and shares [`parse_shortcut`]'s grammar, so
/// it has to share the matcher too: a forked matcher made the same configured
/// chord fire on a different physical key in each shell, and digit chords the
/// parser accepts never fired at all without the physical-digit fallback.
#[must_use]
pub fn shortcut_event_matches(binding: ShortcutBinding, event: &keyboard::Event) -> bool {
    let keyboard::Event::KeyPressed {
        key,
        physical_key,
        modifiers,
        repeat: false,
        ..
    } = event
    else {
        return false;
    };
    shortcut_matches(binding, key, *physical_key, *modifiers)
}

/// The application's constructors for the events the strip reports.
pub struct Events<ViewportMessage> {
    /// Scroll offsets and width changes, which the owner applies to the
    /// workspace so the next frame's projection matches what was drawn.
    pub viewport: ViewportMessage,
}

/// Renders the workspace strip.
///
/// `viewport_width` is the width the strip is laid out against. It is a
/// parameter rather than something this function discovers through
/// `widget::responsive` because that wrapper reports no accessibility nodes,
/// which hid the whole workspace from screen readers; the owner already tracks
/// the width through `Workspace::resize_viewport`.
///
/// `container_id` supplies each card's stable container identity, which
/// scroll-to-card operations address. It is a lookup rather than something this
/// function rebuilds because `widget::Id::new` allocates, and every mounted
/// card would pay that on every redraw; the owner is expected to return a
/// cached clone.
pub fn view<'a, Message, BuildCard, ContainerId, ViewportMessage>(
    workspace: &'a Workspace,
    shortcuts: &'a KeyboardShortcuts,
    focus_follows_mouse: bool,
    viewport_width: f32,
    scroll_id: widget::Id,
    build_card: BuildCard,
    container_id: ContainerId,
    events: Events<ViewportMessage>,
) -> Element<'a, Message>
where
    Message: Clone + 'static,
    BuildCard: Fn(CardViewContext) -> Element<'a, Message> + 'a,
    ContainerId: Fn(CardId) -> widget::Id + 'a,
    ViewportMessage: Fn(WorkspaceViewportEvent) -> Message + Clone + 'a,
{
    let Events {
        viewport: on_viewport,
    } = events;
    let projection = workspace.projection(viewport_width);
    let strip =
        VirtualStrip::new(
            &projection,
            Arc::clone(workspace.card_indices()),
            projection
                .mounted
                .iter()
                .filter_map(|mounted| {
                    let card = workspace.card(mounted.id)?;
                    let context = CardViewContext {
                        id: card.id,
                        focused: workspace.focused() == Some(card.id),
                    };
                    let content =
                        cosmic::widget::id_container(build_card(context), container_id(card.id));
                    Some(VirtualCard {
                        id: card.id,
                        x: mounted.start,
                        width: mounted.width,
                        content: CardSurface::new(content)
                            .on_pointer_focus(on_viewport(WorkspaceViewportEvent::PointerFocused(
                                card.id,
                            )))
                            .on_pointer_enter(focus_follows_mouse.then(|| {
                                on_viewport(WorkspaceViewportEvent::PointerEntered(card.id))
                            }))
                            .into(),
                    })
                })
                .collect(),
        );
    let on_viewport_for_scroll = on_viewport.clone();
    let scrollable = cosmic::widget::scrollable::horizontal(strip)
        .direction(widget::scrollable::Direction::Horizontal(
            widget::scrollable::Scrollbar::hidden(),
        ))
        .id(scroll_id.clone())
        .class(cosmic::style::iced::Scrollable::Minimal)
        .width(Length::Fill)
        .height(Length::Fill)
        .on_scroll(move |viewport| {
            on_viewport_for_scroll(WorkspaceViewportEvent::Scrolled(ViewportSample {
                offset: viewport.absolute_offset().x,
                width: viewport.bounds().width,
            }))
        });

    // Scrollable exposes a composite ID that Iced inherits during diff even
    // when its requested ID changes. Key its entire subtree by worktree so
    // programmatic scroll operations cannot keep targeting the departed strip.
    WorkspaceSurface::new(
        cosmic::widget::id_container(scrollable, scroll_id),
        shortcuts,
        viewport_width,
        move |width: f32| on_viewport(WorkspaceViewportEvent::Resized(width)),
    )
    .into()
}

struct CardSurface<'a, Message> {
    content: Element<'a, Message>,
    on_pointer_focus: Option<Message>,
    on_pointer_enter: Option<Message>,
}

impl<'a, Message> CardSurface<'a, Message> {
    fn new(content: impl Into<Element<'a, Message>>) -> Self {
        Self {
            content: content.into(),
            on_pointer_focus: None,
            on_pointer_enter: None,
        }
    }

    fn on_pointer_focus(mut self, message: Message) -> Self {
        self.on_pointer_focus = Some(message);
        self
    }

    fn on_pointer_enter(mut self, message: Option<Message>) -> Self {
        self.on_pointer_enter = message;
        self
    }

    fn child_layout<'layout>(layout: Layout<'layout>) -> Layout<'layout> {
        layout
            .children()
            .next()
            .expect("card surface content layout")
            .with_virtual_offset(layout.virtual_offset())
    }
}

#[derive(Default)]
struct CardPointerState {
    hovered: bool,
    selecting: bool,
}

fn clipped_viewport(bounds: Rectangle, viewport: &Rectangle) -> Option<Rectangle> {
    bounds.intersection(viewport)
}

fn should_forward_to_raw_children(event_captured: bool) -> bool {
    !event_captured
}

impl<Message: Clone> Widget<Message, Theme, Renderer> for CardSurface<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<CardPointerState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(CardPointerState::default())
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
        let child = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
            .move_to(Point::ORIGIN);
        layout::Node::with_children(child.size(), vec![child])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Self::child_layout(layout),
                renderer,
                operation,
            );
        });
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
        let pointer = tree.state.downcast_mut::<CardPointerState>();
        match event {
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)) => {
                pointer.selecting = true
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                pointer.selecting = false
            }
            Event::Mouse(mouse::Event::CursorLeft) => {
                pointer.hovered = false;
                pointer.selecting = false;
            }
            Event::Mouse(mouse::Event::CursorMoved { .. }) => {
                let hovered = !pointer.selecting
                    && !shell.is_event_captured()
                    && clipped_viewport(layout.bounds(), viewport)
                        .is_some_and(|clip| cursor.is_over(clip));
                if hovered
                    && !pointer.hovered
                    && let Some(message) = &self.on_pointer_enter
                {
                    shell.publish(message.clone());
                }
                pointer.hovered = hovered;
            }
            _ => {}
        }
        let Some(clipped) = clipped_viewport(layout.bounds(), viewport) else {
            return;
        };
        // Observe before the child can capture the press, but do not capture it
        // ourselves: text selection, caret placement and controls still get it.
        if !shell.is_event_captured()
            && matches!(event, Event::Mouse(mouse::Event::ButtonPressed(_)))
            && cursor.is_over(clipped)
            && let Some(message) = &self.on_pointer_focus
        {
            shell.publish(message.clone());
        }
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            Self::child_layout(layout),
            cursor,
            renderer,
            clipboard,
            shell,
            &clipped,
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
        let Some(clipped) = clipped_viewport(layout.bounds(), viewport) else {
            return mouse::Interaction::default();
        };
        if !cursor.is_over(clipped) {
            return mouse::Interaction::default();
        }
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            Self::child_layout(layout),
            cursor,
            &clipped,
            renderer,
        )
    }

    fn draw(
        &self,
        tree: &Tree,
        renderer: &mut Renderer,
        _theme: &Theme,
        style: &renderer::Style,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
    ) {
        let Some(clipped) = clipped_viewport(layout.bounds(), viewport) else {
            return;
        };
        renderer.with_layer(clipped, |renderer| {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                _theme,
                style,
                Self::child_layout(layout),
                cursor,
                &clipped,
            );
        });
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        // `overlay` is the one pass whose layout is not already on screen: the
        // workspace scrollable leaves card layouts in content space and reports
        // the camera separately as `translation`, so the card's visible
        // rectangle is its bounds plus that translation. Testing the
        // untranslated bounds against the screen-space viewport dropped every
        // overlay raised inside a card whose content-space position sat outside
        // the visible rect, which is every card past the second one however far
        // the camera had scrolled it into view.
        let clipped = clipped_viewport(layout.bounds() + translation, viewport)?;
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            Self::child_layout(layout),
            renderer,
            &clipped,
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
            .a11y_nodes(Self::child_layout(layout), &state.children[0], cursor)
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
            Self::child_layout(layout),
            renderer,
            destinations,
        );
    }
}

impl<'a, Message: Clone + 'a> From<CardSurface<'a, Message>> for Element<'a, Message> {
    fn from(surface: CardSurface<'a, Message>) -> Self {
        Element::new(surface)
    }
}

struct VirtualCard<'a, Message> {
    id: CardId,
    x: f32,
    width: f32,
    content: Element<'a, Message>,
}

struct VirtualStrip<'a, Message> {
    total_width: f32,
    retained: Arc<HashMap<CardId, usize>>,
    cards: Vec<VirtualCard<'a, Message>>,
}

impl<'a, Message> VirtualStrip<'a, Message> {
    fn new(
        projection: &ViewportProjection,
        retained: Arc<HashMap<CardId, usize>>,
        cards: Vec<VirtualCard<'a, Message>>,
    ) -> Self {
        debug_assert_eq!(
            cards.iter().map(|card| card.id).collect::<Vec<_>>(),
            projection.mounted_ids().collect::<Vec<_>>()
        );
        Self {
            total_width: projection.total_width,
            retained,
            cards,
        }
    }
}

#[derive(Default)]
struct VirtualStripState {
    mounted: Vec<CardId>,
    detached: HashMap<CardId, Tree>,
    retained: Arc<HashMap<CardId, usize>>,
}

// Iced extracts named states before diffing. A departing virtual card must
// reclaim those states before being parked outside the traversed child tree;
// otherwise the next frame drops them and remounting downcasts State::None.
fn reclaim_named_states(tree: &mut Tree) {
    if let Some(widget::Id(iced_accessibility::id::Internal::Custom(_, name))) = &tree.id {
        if let Some((state, children)) = tree::NAMED.with(|named| named.borrow_mut().remove(name)) {
            tree.state = state;
            for (index, child) in children {
                if let Some(slot) = tree.children.get_mut(index) {
                    *slot = child;
                }
            }
        }
    }
    for child in &mut tree.children {
        reclaim_named_states(child);
    }
}

impl<Message> Widget<Message, Theme, Renderer> for VirtualStrip<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<VirtualStripState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(VirtualStripState {
            mounted: self.cards.iter().map(|card| card.id).collect(),
            detached: HashMap::new(),
            retained: Arc::clone(&self.retained),
        })
    }

    fn children(&self) -> Vec<Tree> {
        self.cards
            .iter()
            .map(|card| Tree::new(&card.content))
            .collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        let state = tree.state.downcast_mut::<VirtualStripState>();
        let membership_changed = !Arc::ptr_eq(&state.retained, &self.retained);
        if state
            .mounted
            .iter()
            .copied()
            .eq(self.cards.iter().map(|card| card.id))
        {
            // Camera movement normally leaves the mounted band unchanged.
            // Diff those children in place, keeping their identity and storage.
            for (card, child) in self.cards.iter_mut().zip(&mut tree.children) {
                child.diff(&mut card.content);
            }
        } else {
            let mut children = mem::take(&mut tree.children);
            for (id, mut child) in state.mounted.drain(..).zip(children.drain(..)) {
                reclaim_named_states(&mut child);
                state.detached.insert(id, child);
            }
            for card in &mut self.cards {
                let mut child = state
                    .detached
                    .remove(&card.id)
                    .unwrap_or_else(|| Tree::new(&card.content));
                child.diff(&mut card.content);
                children.push(child);
                state.mounted.push(card.id);
            }
            tree.children = children;
        }
        // Detached trees are retained for focus/selection continuity, but need
        // pruning only after structural changes, never on animation frames.
        if membership_changed {
            state
                .detached
                .retain(|id, _| self.retained.contains_key(id));
            state.retained = Arc::clone(&self.retained);
        }
    }

    fn size(&self) -> Size<Length> {
        Size::new(Length::Fixed(self.total_width), Length::Fill)
    }

    fn layout(
        &mut self,
        tree: &mut Tree,
        renderer: &Renderer,
        limits: &layout::Limits,
    ) -> layout::Node {
        let size = limits.resolve(
            Length::Fixed(self.total_width),
            Length::Fill,
            Size::new(self.total_width, 0.0),
        );
        let children = self
            .cards
            .iter_mut()
            .zip(&mut tree.children)
            .map(|(card, state)| {
                let child_size = Size::new(card.width, size.height);
                let child_limits = layout::Limits::new(child_size, child_size);
                card.content
                    .as_widget_mut()
                    .layout(state, renderer, &child_limits)
                    .move_to(Point::new(card.x, 0.0))
            })
            .collect();
        layout::Node::with_children(size, children)
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            for ((card, state), child_layout) in self
                .cards
                .iter_mut()
                .zip(&mut tree.children)
                .zip(layout.children())
            {
                card.content.as_widget_mut().operate(
                    state,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    renderer,
                    operation,
                );
            }
        });
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
        for ((card, state), child_layout) in self
            .cards
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            if child_layout.bounds().intersects(viewport)
                || matches!(
                    event,
                    Event::Mouse(
                        mouse::Event::CursorMoved { .. }
                            | mouse::Event::CursorLeft
                            | mouse::Event::ButtonPressed(_)
                            | mouse::Event::ButtonReleased(_)
                    )
                )
            {
                card.content.as_widget_mut().update(
                    state,
                    event,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    cursor,
                    renderer,
                    clipboard,
                    shell,
                    viewport,
                );
            }
        }
        // Wheel input belongs to terminal/file content, never the outer strip.
        // Consume any remainder after children so the native scrollable cannot
        // turn a horizontal trackpad gesture into pane navigation.
        if matches!(event, Event::Mouse(mouse::Event::WheelScrolled { .. }))
            && cursor.is_over(*viewport)
        {
            shell.capture_event();
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
        self.cards
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .filter(|(_, child_layout)| {
                child_layout
                    .bounds()
                    .intersection(viewport)
                    .is_some_and(|visible| cursor.is_over(visible))
            })
            .map(|((card, state), child_layout)| {
                card.content.as_widget().mouse_interaction(
                    state,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    cursor,
                    viewport,
                    renderer,
                )
            })
            .max()
            .unwrap_or_default()
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
        let Some(clipped_viewport) = layout.bounds().intersection(viewport) else {
            return;
        };
        for ((card, state), child_layout) in self
            .cards
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .filter(|(_, child_layout)| child_layout.bounds().intersects(&clipped_viewport))
        {
            card.content.as_widget().draw(
                state,
                renderer,
                theme,
                style,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                cursor,
                &clipped_viewport,
            );
        }
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        let children = self
            .cards
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
            .filter_map(|((card, state), child_layout)| {
                card.content.as_widget_mut().overlay(
                    state,
                    child_layout.with_virtual_offset(layout.virtual_offset()),
                    renderer,
                    viewport,
                    translation,
                )
            })
            .collect::<Vec<_>>();
        (!children.is_empty()).then(|| overlay::Group::with_children(children).overlay())
    }

    fn a11y_nodes(
        &self,
        layout: Layout<'_>,
        state: &Tree,
        cursor: mouse::Cursor,
    ) -> iced_accessibility::A11yTree {
        iced_accessibility::A11yTree::join(
            self.cards
                .iter()
                .zip(&state.children)
                .zip(layout.children())
                .map(|((card, child_state), child_layout)| {
                    card.content.as_widget().a11y_nodes(
                        child_layout.with_virtual_offset(layout.virtual_offset()),
                        child_state,
                        cursor,
                    )
                }),
        )
    }

    fn drag_destinations(
        &self,
        state: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        for ((card, child_layout), child_state) in self
            .cards
            .iter()
            .zip(layout.children())
            .zip(&state.children)
        {
            card.content.as_widget().drag_destinations(
                child_state,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                renderer,
                destinations,
            );
        }
    }
}

impl<'a, Message: 'a> From<VirtualStrip<'a, Message>> for Element<'a, Message> {
    fn from(strip: VirtualStrip<'a, Message>) -> Self {
        Element::new(strip)
    }
}

struct WorkspaceSurface<'a, Message> {
    content: Element<'a, Message>,
    shortcuts: &'a KeyboardShortcuts,
    /// The strip viewport width the owner already holds, which is what the
    /// enclosing `view` laid this surface's content out against.
    ///
    /// Size reporting lives in this widget instead of a `widget::sensor`
    /// wrapper because `sensor` reports no accessibility nodes, and wrapping
    /// the strip in one hid every card from screen readers.
    reported_width: f32,
    on_resize: Box<dyn Fn(f32) -> Message + 'a>,
}

impl<'a, Message> WorkspaceSurface<'a, Message> {
    fn new(
        content: impl Into<Element<'a, Message>>,
        shortcuts: &'a KeyboardShortcuts,
        reported_width: f32,
        on_resize: impl Fn(f32) -> Message + 'a,
    ) -> Self {
        Self {
            content: content.into(),
            shortcuts,
            reported_width,
            on_resize: Box::new(on_resize),
        }
    }

    fn child_layout<'layout>(layout: Layout<'layout>) -> Layout<'layout> {
        layout
            .children()
            .next()
            .expect("workspace surface content layout")
            .with_virtual_offset(layout.virtual_offset())
    }
}

impl<Message> Widget<Message, Theme, Renderer> for WorkspaceSurface<'_, Message> {
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
        let content = self
            .content
            .as_widget_mut()
            .layout(&mut tree.children[0], renderer, limits)
            .move_to(Point::ORIGIN);
        layout::Node::with_children(content.size(), vec![content])
    }

    fn operate(
        &mut self,
        tree: &mut Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        operation: &mut dyn Operation,
    ) {
        operation.container(None, layout.bounds());
        operation.traverse(&mut |operation| {
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                Self::child_layout(layout),
                renderer,
                operation,
            );
        });
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
        let content_layout = Self::child_layout(layout);

        match event {
            Event::Keyboard(
                keyboard::Event::KeyPressed {
                    key,
                    physical_key,
                    modifiers,
                    ..
                }
                | keyboard::Event::KeyReleased {
                    key,
                    physical_key,
                    modifiers,
                    ..
                },
            ) if is_global_shortcut_chord(key, *physical_key, *modifiers, self.shortcuts) => {
                shell.capture_event();
                return;
            }
            _ => {}
        }
        if !should_forward_to_raw_children(shell.is_event_captured()) {
            return;
        }

        let Some(clipped) = clipped_viewport(layout.bounds(), viewport) else {
            return;
        };
        self.content.as_widget_mut().update(
            &mut tree.children[0],
            event,
            content_layout,
            cursor,
            renderer,
            clipboard,
            shell,
            &clipped,
        );
        // Reported on redraw rather than from `layout`, because layout runs
        // during view building where publishing a message would fight the
        // reflow it triggers.
        // Publish after native scroll feedback from the old layout, so that
        // feedback cannot cancel the newly computed focused-card reveal.
        //
        // The comparison is against the width the owner already holds rather
        // than against a width remembered here, because the shell re-runs
        // this whole pass for the same frame while any pass publishes, and
        // rebuilds the view in between. A remembered width would be reported
        // afresh by a pass that changed nothing the owner reads - a height
        // change, or a strip remounted with fresh widget state - and each
        // such report costs another full update-and-layout pass. Comparing
        // against the owner's own value makes the second pass over one frame
        // silent as soon as the owner has stored what was reported.
        if let Event::Window(window::Event::RedrawRequested(_)) = event {
            let measured = layout.bounds().width;
            if measured.is_finite() && measured >= 0.0 && measured != self.reported_width {
                shell.publish((self.on_resize)(measured));
            }
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
        if let Some(clipped) = clipped_viewport(layout.bounds(), viewport) {
            self.content.as_widget().mouse_interaction(
                &tree.children[0],
                Self::child_layout(layout),
                cursor,
                &clipped,
                renderer,
            )
        } else {
            mouse::Interaction::default()
        }
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
        let Some(clipped) = clipped_viewport(layout.bounds(), viewport) else {
            return;
        };
        renderer.with_layer(clipped, |renderer| {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                theme,
                style,
                Self::child_layout(layout),
                cursor,
                &clipped,
            );
        });
    }

    fn overlay<'a>(
        &'a mut self,
        tree: &'a mut Tree,
        layout: Layout<'a>,
        renderer: &Renderer,
        viewport: &Rectangle,
        translation: Vector,
    ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
        // Same contract as the card surface: during `overlay` the layout is in
        // the coordinate space of whatever scrolls this surface, and
        // `translation` is what puts it on screen. The strip mounts above the
        // workspace scrollable so the translation is usually zero, but reading
        // it keeps the surface correct the day something scrolls it.
        let clipped = clipped_viewport(layout.bounds() + translation, viewport)?;
        self.content.as_widget_mut().overlay(
            &mut tree.children[0],
            Self::child_layout(layout),
            renderer,
            &clipped,
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
            .a11y_nodes(Self::child_layout(layout), &state.children[0], cursor)
    }

    fn drag_destinations(
        &self,
        state: &Tree,
        layout: Layout<'_>,
        renderer: &Renderer,
        destinations: &mut cosmic::iced::advanced::clipboard::DndDestinationRectangles,
    ) {
        self.content.as_widget().drag_destinations(
            &state.children[0],
            Self::child_layout(layout),
            renderer,
            destinations,
        );
    }
}

impl<'a, Message: 'a> From<WorkspaceSurface<'a, Message>> for Element<'a, Message> {
    fn from(surface: WorkspaceSurface<'a, Message>) -> Self {
        Element::new(surface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::settings::parse_chords;
    use crate::workspace::{CardWidth, WorkspaceCard};
    use std::collections::HashSet;

    #[test]
    fn card_cursor_uses_translated_child_coordinates_without_reviving_blocked_hover() {
        use cosmic::iced::advanced::{clipboard, renderer::Headless};
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
        let content = cosmic::widget::text_editor::Content::with_text("draft");
        let editor = widget::text_editor::TextEditor::new(&content)
            .width(100.0)
            .height(Length::Fill)
            .on_action(|_| "edit");
        let button = cosmic::widget::button::text("Action")
            .width(Length::Fixed(100.0))
            .height(Length::Fill)
            .on_press("click");
        let row = cosmic::widget::row::with_children(vec![editor.into(), button.into()])
            .height(Length::Fill);
        let mut surface: Element<'_, &str> =
            CardSurface::new(row).on_pointer_enter(Some("enter")).into();
        let mut tree = Tree::new(&surface);
        let node = surface
            .as_widget_mut()
            .layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::new(200.0, 80.0), Size::new(200.0, 80.0)),
            )
            .move_to(Point::new(100.0, 0.0));
        let viewport = Rectangle::new(Point::new(100.0, 0.0), Size::new(200.0, 80.0));
        let mut messages = Vec::new();
        let row_layout = Layout::new(&node).children().next().unwrap();
        let mut controls = row_layout.children();
        let editor_center = controls.next().unwrap().bounds().center();
        let button_center = controls.next().unwrap().bounds().center();
        for (position, expected) in [
            (editor_center, mouse::Interaction::Text),
            (button_center, mouse::Interaction::Pointer),
        ] {
            let cursor = mouse::Cursor::Available(position);
            surface.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::CursorMoved {
                    position: position - Vector::new(100.0, 0.0),
                }),
                Layout::new(&node),
                cursor,
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
            assert_eq!(
                surface.as_widget().mouse_interaction(
                    &tree,
                    Layout::new(&node),
                    cursor,
                    &viewport,
                    &renderer,
                ),
                expected
            );
            for blocked in [mouse::Cursor::Unavailable, cursor.levitate()] {
                assert_eq!(
                    surface.as_widget().mouse_interaction(
                        &tree,
                        Layout::new(&node),
                        blocked,
                        &viewport,
                        &renderer,
                    ),
                    mouse::Interaction::None
                );
            }
        }
        assert_eq!(messages, ["enter"]);
    }

    #[test]
    fn parked_editor_preserves_focus_across_named_state_extraction() {
        use cosmic::iced::advanced::{renderer::Headless, widget::operation};
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
        let ids = [CardId::new(), CardId::new()];
        let retained = Arc::new(HashMap::from([(ids[0], 0), (ids[1], 1)]));
        let content = cosmic::widget::text_editor::Content::with_text("retained draft");
        let editor = widget::Id::new("parked-editor");
        let make = |index: usize| -> Element<'_, ()> {
            let child = widget::text_editor::TextEditor::new(&content)
                .id(if index == 0 {
                    editor.clone()
                } else {
                    widget::Id::new("other-editor")
                })
                .on_action(|_| ());
            VirtualStrip {
                total_width: 320.0,
                retained: retained.clone(),
                cards: vec![VirtualCard {
                    id: ids[index],
                    x: 0.0,
                    width: 320.0,
                    content: cosmic::widget::id_container(
                        child,
                        widget::Id::new(format!("parked-card-{index}")),
                    )
                    .into(),
                }],
            }
            .into()
        };
        let limits = layout::Limits::new(Size::ZERO, Size::new(320.0, 200.0));
        let mut element = make(0);
        let mut tree = Tree::new(&element);
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::focusable::focus::<()>(editor.clone()),
        );
        for index in [1, 1, 0] {
            // Reproduce the real runtime, which extracts names before every
            // view diff and drops unclaimed names at the next frame.
            tree::NAMED.with(|cache| *cache.borrow_mut() = tree.take_all_named());
            element = make(index);
            tree.diff(&mut element);
            element
                .as_widget_mut()
                .layout(&mut tree, &renderer, &limits);
        }
        let node = element
            .as_widget_mut()
            .layout(&mut tree, &renderer, &limits);
        let mut focused = operation::focusable::find_focused();
        let mut probe = operation::black_box::<_, ()>(&mut focused);
        element
            .as_widget_mut()
            .operate(&mut tree, Layout::new(&node), &renderer, &mut probe);
        drop(probe);
        assert!(matches!(focused.finish(), operation::Outcome::Some(id) if id == editor));
        tree::NAMED.with(|cache| cache.borrow_mut().clear());
    }

    #[test]
    fn projection_never_exposes_offscreen_cards_to_the_widget_builder() {
        let mut workspace = Workspace::default();
        workspace.resize_viewport(320.0);
        for index in 0..20 {
            workspace
                .add_card(
                    WorkspaceCard::new(
                        CardId::new(),
                        format!("Card {index}"),
                        CardWidth::Fixed(320.0),
                    )
                    .unwrap(),
                    index,
                    false,
                )
                .unwrap();
        }
        workspace.native_scroll(3_200.0, 320.0);
        let projection = workspace.projection(320.0);
        assert!(projection.mounted.len() < workspace.cards().len());
        assert!(
            projection
                .mounted
                .iter()
                .all(|mounted| mounted.visibility.is_mounted())
        );
    }
    #[test]
    fn mounted_column_layout_and_clips_are_pairwise_disjoint() {
        let mut workspace = Workspace::default();
        workspace.resize_viewport(700.0);
        for (index, width) in [
            CardWidth::Fraction(1.0),
            CardWidth::Fixed(900.0),
            CardWidth::Fraction(0.45),
        ]
        .into_iter()
        .enumerate()
        {
            workspace
                .add_card(
                    WorkspaceCard::new(CardId::new(), format!("Card {index}"), width).unwrap(),
                    index,
                    index == 0,
                )
                .unwrap();
        }
        workspace.native_scroll(500.0, 700.0);
        let projection = workspace.projection(700.0);
        let viewport = Rectangle::new(Point::new(500.0, 0.0), Size::new(700.0, 640.0));
        let columns: Vec<_> = projection
            .mounted
            .iter()
            .map(|card| {
                Rectangle::new(
                    Point::new(card.start, 0.0),
                    Size::new(card.width, viewport.height),
                )
            })
            .collect();

        assert!(projection.mounted_are_disjoint());
        assert!(
            columns
                .windows(2)
                .all(|pair| pair[0].x + pair[0].width <= pair[1].x)
        );
        for (column, clip) in columns
            .iter()
            .filter_map(|column| clipped_viewport(*column, &viewport).map(|clip| (column, clip)))
        {
            assert!(clip.x >= column.x);
            assert!(clip.x + clip.width <= column.x + column.width);
            assert!(clip.y >= column.y);
            assert!(clip.y + clip.height <= column.y + column.height);
            assert!(clip.x >= viewport.x);
            assert!(clip.x + clip.width <= viewport.x + viewport.width);
        }
    }

    #[test]
    fn card_body_starts_below_header_and_remains_inside_its_column_clip() {
        const HEADER_HEIGHT: f32 = 44.0;
        let column = Rectangle::new(Point::new(12.0, 0.0), Size::new(480.0, 640.0));
        let viewport = Rectangle::new(Point::new(0.0, 0.0), Size::new(700.0, 640.0));
        let card_clip = clipped_viewport(column, &viewport).unwrap();
        let body = Rectangle::new(
            Point::new(column.x, column.y + HEADER_HEIGHT),
            Size::new(column.width, column.height - HEADER_HEIGHT),
        );
        let body_clip = clipped_viewport(body, &card_clip).unwrap();

        assert!(body_clip.y >= column.y + HEADER_HEIGHT);
        assert!(body_clip.x >= card_clip.x);
        assert!(body_clip.x + body_clip.width <= card_clip.x + card_clip.width);
        assert!(body_clip.y + body_clip.height <= card_clip.y + card_clip.height);
    }

    #[test]
    fn frame_filter_rejects_other_windows() {
        let expected = window::Id::unique();
        let other = window::Id::unique();
        let instant = Instant::now();
        let match_frame = |(wanted, (actual, at)): (window::Id, (window::Id, Instant))| {
            (wanted == actual).then_some(at)
        };
        assert_eq!(match_frame((expected, (expected, instant))), Some(instant));
        assert_eq!(match_frame((expected, (other, instant))), None);
    }

    #[test]
    fn card_container_and_title_focus_ids_are_stable_and_distinct() {
        let first = CardId::new();
        let second = CardId::new();
        assert_eq!(
            card_widget_id(first).to_string(),
            card_widget_id(first).to_string()
        );
        assert_eq!(
            card_focus_id(first).to_string(),
            card_focus_id(first).to_string()
        );
        assert_ne!(
            card_widget_id(first).to_string(),
            card_focus_id(first).to_string()
        );
        assert_ne!(
            card_focus_id(first).to_string(),
            card_focus_id(second).to_string()
        );
    }

    #[test]
    fn viewport_events_keep_scroll_offsets_out_of_resize_notifications() {
        let scrolled = WorkspaceViewportEvent::Scrolled(ViewportSample {
            offset: 240.0,
            width: 800.0,
        });
        let resized = WorkspaceViewportEvent::Resized(800.0);

        assert_eq!(
            scrolled,
            WorkspaceViewportEvent::Scrolled(ViewportSample {
                offset: 240.0,
                width: 800.0,
            })
        );
        assert_eq!(resized, WorkspaceViewportEvent::Resized(800.0));
        assert_ne!(scrolled, resized);
    }

    fn key_press(
        character: &str,
        code: keyboard::key::Code,
        modifiers: keyboard::Modifiers,
        repeat: bool,
    ) -> keyboard::Event {
        let key = keyboard::Key::Character(character.into());
        keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat,
        }
    }

    fn named_key_press(
        named: keyboard::key::Named,
        code: keyboard::key::Code,
        modifiers: keyboard::Modifiers,
        repeat: bool,
    ) -> keyboard::Event {
        let key = keyboard::Key::Named(named);
        keyboard::Event::KeyPressed {
            key: key.clone(),
            modified_key: key,
            physical_key: keyboard::key::Physical::Code(code),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat,
        }
    }

    #[test]
    fn global_shortcuts_match_configured_chords_and_repeat_presses() {
        use keyboard::key::Code;

        let shortcuts = KeyboardShortcuts::default();
        assert_eq!(
            global_shortcut(
                &key_press("t", Code::KeyT, keyboard::Modifiers::CTRL, false),
                &shortcuts,
            ),
            Some(GlobalShortcut::NewTerminal)
        );
        for (character, code, action) in [
            ("g", Code::KeyG, GlobalShortcut::NewChannel),
            ("e", Code::KeyE, GlobalShortcut::ClonePane),
        ] {
            assert_eq!(
                global_shortcut(
                    &key_press(character, code, keyboard::Modifiers::ALT, false),
                    &shortcuts,
                ),
                Some(action)
            );
            assert_eq!(
                global_shortcut(
                    &key_press(character, code, keyboard::Modifiers::ALT, true),
                    &shortcuts,
                ),
                None,
                "holding a creation shortcut must not create multiple panes"
            );
            assert!(is_global_shortcut_chord(
                &keyboard::Key::Character(character.into()),
                keyboard::key::Physical::Code(code),
                keyboard::Modifiers::ALT,
                &shortcuts,
            ));
        }
        assert_eq!(
            global_shortcut(
                &key_press("w", Code::KeyW, keyboard::Modifiers::CTRL, false),
                &shortcuts,
            ),
            Some(GlobalShortcut::CloseFocusedCard)
        );
        assert_eq!(
            global_shortcut(
                &key_press(
                    "F",
                    Code::KeyF,
                    keyboard::Modifiers::CTRL | keyboard::Modifiers::CAPS_LOCK,
                    false,
                ),
                &shortcuts,
            ),
            Some(GlobalShortcut::FocusProjectSearch)
        );
        for (character, code) in [("t", Code::KeyT), ("w", Code::KeyW), ("f", Code::KeyF)] {
            assert_eq!(
                global_shortcut(
                    &key_press(character, code, keyboard::Modifiers::CTRL, true),
                    &shortcuts,
                ),
                None,
                "held Ctrl shortcuts must not repeat destructive/creation actions"
            );
        }
        for repeat in [false, true] {
            assert_eq!(
                global_shortcut(
                    &key_press("A", Code::KeyA, keyboard::Modifiers::ALT, repeat),
                    &shortcuts,
                ),
                None,
                "Alt+A must be forwarded on initial and repeated key presses"
            );
        }
        assert_eq!(
            global_shortcut(
                &key_press("q", Code::KeyQ, keyboard::Modifiers::ALT, true),
                &shortcuts,
            ),
            Some(GlobalShortcut::FocusAdjacent(FocusDirection::Left))
        );
        assert_eq!(
            global_shortcut(
                &key_press("w", Code::KeyW, keyboard::Modifiers::ALT, true),
                &shortcuts,
            ),
            Some(GlobalShortcut::FocusAdjacent(FocusDirection::Right))
        );

        // An action answers to every chord bound to it, not just the first:
        // column navigation ships a letter chord *and* an arrow, and each must
        // navigate and be reserved against the process inside a card.
        for (named, code, direction) in [
            (
                keyboard::key::Named::ArrowLeft,
                Code::ArrowLeft,
                FocusDirection::Left,
            ),
            (
                keyboard::key::Named::ArrowRight,
                Code::ArrowRight,
                FocusDirection::Right,
            ),
        ] {
            for repeat in [false, true] {
                let event = named_key_press(named, code, keyboard::Modifiers::ALT, repeat);
                assert_eq!(
                    global_shortcut(&event, &shortcuts),
                    Some(GlobalShortcut::FocusAdjacent(direction)),
                    "Alt+arrow must navigate on initial and repeated presses"
                );
                assert!(is_global_shortcut_chord(
                    &keyboard::Key::Named(named),
                    keyboard::key::Physical::Code(code),
                    keyboard::Modifiers::ALT,
                    &shortcuts,
                ));
            }
        }

        let mut customized = shortcuts;
        customized.new_terminal = vec!["Ctrl+Shift+N".to_owned()];
        assert_eq!(
            global_shortcut(
                &key_press(
                    "n",
                    Code::KeyN,
                    keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
                    false,
                ),
                &customized,
            ),
            Some(GlobalShortcut::NewTerminal)
        );
        customized.new_terminal = vec!["Ctrl+Shift+1".to_owned()];
        assert_eq!(
            global_shortcut(
                &key_press(
                    "!",
                    Code::Digit1,
                    keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
                    false,
                ),
                &customized,
            ),
            Some(GlobalShortcut::NewTerminal)
        );
    }

    #[test]
    fn customized_creation_shortcuts_release_defaults_and_reserve_only_bound_chords() {
        use keyboard::key::Code;

        let mut shortcuts = KeyboardShortcuts {
            clone_pane: vec!["Ctrl+Shift+E".into()],
            new_channel: vec!["Ctrl+Shift+T".into()],
            ..KeyboardShortcuts::default()
        };
        for (character, code, default_character, default_code, action) in [
            ("e", Code::KeyE, "e", Code::KeyE, GlobalShortcut::ClonePane),
            ("t", Code::KeyT, "g", Code::KeyG, GlobalShortcut::NewChannel),
        ] {
            let modifiers = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
            assert_eq!(
                global_shortcut(&key_press(character, code, modifiers, false), &shortcuts),
                Some(action)
            );
            assert_eq!(
                global_shortcut(&key_press(character, code, modifiers, true), &shortcuts),
                None
            );
            assert!(is_global_shortcut_chord(
                &keyboard::Key::Character(character.into()),
                keyboard::key::Physical::Code(code),
                modifiers,
                &shortcuts,
            ));
            assert!(!is_global_shortcut_chord(
                &keyboard::Key::Character(default_character.into()),
                keyboard::key::Physical::Code(default_code),
                keyboard::Modifiers::ALT,
                &shortcuts,
            ));
        }

        shortcuts.clone_pane.clear();
        shortcuts.new_channel.clear();
        for (character, code) in [("e", Code::KeyE), ("t", Code::KeyT)] {
            assert!(!is_global_shortcut_chord(
                &keyboard::Key::Character(character.into()),
                keyboard::key::Physical::Code(code),
                keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
                &shortcuts,
            ));
        }
    }

    #[test]
    fn captured_typing_is_not_forwarded_to_raw_card_children() {
        assert!(!should_forward_to_raw_children(true));
        assert!(should_forward_to_raw_children(false));
    }

    #[test]
    fn wheel_scrolls_card_content_without_moving_the_workspace() {
        use cosmic::iced::advanced::{clipboard, renderer::Headless};
        use cosmic::iced::{Font, Pixels};

        #[derive(Debug, Clone)]
        enum Observation {
            Content(f32),
            Workspace(WorkspaceViewportEvent),
        }

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
        let mut workspace = Workspace::default();
        workspace.resize_viewport(400.0);
        for index in 0..3 {
            workspace
                .add_card(
                    WorkspaceCard::new(CardId::new(), "Scrollable", CardWidth::Fixed(320.0))
                        .unwrap(),
                    index,
                    index == 0,
                )
                .unwrap();
        }
        let shortcuts = KeyboardShortcuts::default();
        let mut element = view(
            &workspace,
            &shortcuts,
            true,
            400.0,
            widget::Id::unique(),
            |_| {
                widget::scrollable(widget::Space::new().width(Length::Fill).height(2_000.0))
                    .height(Length::Fill)
                    .on_scroll(|viewport| Observation::Content(viewport.absolute_offset().y))
                    .into()
            },
            card_widget_id,
            Events {
                viewport: Observation::Workspace,
            },
        );
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(400.0, 300.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(viewport.size(), viewport.size()),
        );
        let mut messages = Vec::new();
        let mut clipboard = clipboard::Null;
        for delta in [
            mouse::ScrollDelta::Pixels {
                x: -120.0,
                y: -120.0,
            },
            mouse::ScrollDelta::Pixels { x: -120.0, y: 0.0 },
            mouse::ScrollDelta::Lines { x: -3.0, y: 0.0 },
        ] {
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &Event::Mouse(mouse::Event::WheelScrolled { delta }),
                Layout::new(&node),
                mouse::Cursor::Available(Point::new(100.0, 100.0)),
                &renderer,
                &mut clipboard,
                &mut shell,
                &viewport,
            );
        }
        assert!(
            messages
                .iter()
                .any(|message| matches!(message, Observation::Content(offset) if *offset > 0.0))
        );
        assert!(!messages.iter().any(|message| matches!(
            message,
            Observation::Workspace(WorkspaceViewportEvent::Scrolled(_))
        )));
    }

    /// The shell re-runs one `RedrawRequested` through update and layout while
    /// any pass publishes a message, rebuilding the view in between, and warns
    /// once a frame has cost three of those passes. The strip surface reports
    /// its own measured width from that pass, so the report has to be a fixed
    /// point: once the owner holds the reported width, a further pass over the
    /// same frame must publish nothing - including a pass whose height changed,
    /// and a pass whose strip was remounted with fresh widget state, neither of
    /// which changes the width the owner reads.
    #[test]
    fn a_reported_viewport_width_is_not_reported_twice_in_one_frame() {
        use cosmic::iced::advanced::{clipboard, renderer::Headless};
        use cosmic::iced::runtime::{UserInterface, user_interface::Cache};
        use cosmic::iced::{Font, Pixels};
        use std::time::Instant;

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
        let mut workspace = Workspace::default();
        for index in 0..4 {
            workspace
                .add_card(
                    WorkspaceCard::new(CardId::new(), "Card", CardWidth::Fraction(0.5)).unwrap(),
                    index,
                    index == 0,
                )
                .unwrap();
        }
        let shortcuts = KeyboardShortcuts::default();
        let scroll_id = widget::Id::unique();
        let redraw = Event::Window(window::Event::RedrawRequested(Instant::now()));

        // One pass of the shell's loop: build the view the owner would build
        // for the width it currently holds, deliver the frame to it, and
        // report back what the surface asked of the owner.
        let mut pass = |workspace: &Workspace, cache: Cache, height: f32| {
            let mut messages: Vec<WorkspaceViewportEvent> = Vec::new();
            let mut interface = UserInterface::build(
                view(
                    workspace,
                    &shortcuts,
                    false,
                    workspace.viewport_width(),
                    scroll_id.clone(),
                    |_| {
                        widget::Space::new()
                            .width(Length::Fill)
                            .height(Length::Fill)
                            .into()
                    },
                    card_widget_id,
                    Events {
                        viewport: |event| event,
                    },
                ),
                Size::new(900.0, height),
                cache,
                &mut renderer,
            );
            let _ = interface.update(
                std::slice::from_ref(&redraw),
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            let widths: Vec<f32> = messages
                .iter()
                .filter_map(|message| match message {
                    WorkspaceViewportEvent::Resized(width) => Some(*width),
                    _ => None,
                })
                .collect();
            (interface.into_cache(), widths)
        };

        let (cache, reported) = pass(&workspace, Cache::new(), 600.0);
        assert_eq!(
            reported,
            vec![900.0],
            "a surface whose measured width disagrees with the owner's has to say so once",
        );
        for width in reported {
            let _ = workspace.resize_viewport(width);
        }

        let (cache, settled) = pass(&workspace, cache, 600.0);
        assert!(
            settled.is_empty(),
            "the owner already holds this width, so a second pass over the same frame must \
             publish nothing; it reported {settled:?}",
        );

        let (_, after_height_change) = pass(&workspace, cache, 400.0);
        assert!(
            after_height_change.is_empty(),
            "a shorter surface is the same viewport width, so reporting it again buys the owner \
             nothing and costs the frame a whole extra pass; it reported {after_height_change:?}",
        );

        let (_, after_remount) = pass(&workspace, Cache::new(), 600.0);
        assert!(
            after_remount.is_empty(),
            "a strip remounted with fresh widget state still measures the width the owner holds; \
             it reported {after_remount:?}",
        );
    }
    struct AccessibleProbe {
        node: u64,
        expected_virtual_offset: Vector,
    }

    impl Widget<(), Theme, Renderer> for AccessibleProbe {
        fn tag(&self) -> tree::Tag {
            tree::Tag::of::<u64>()
        }

        fn state(&self) -> tree::State {
            tree::State::new(self.node)
        }

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

        fn a11y_nodes(
            &self,
            layout: Layout<'_>,
            state: &Tree,
            _cursor: mouse::Cursor,
        ) -> iced_accessibility::A11yTree {
            use iced_accessibility::accesskit::{Node, Role};

            assert_eq!(layout.virtual_offset(), self.expected_virtual_offset);
            iced_accessibility::A11yTree::leaf(
                Node::new(Role::Button),
                *state.state.downcast_ref::<u64>(),
            )
        }
    }

    #[test]
    fn scrolling_restores_card_state_but_closed_cards_do_not_keep_it() {
        use iced_accessibility::A11yId;

        let first = CardId::new();
        let second = CardId::new();
        let retained = Arc::new(HashMap::from([(first, 0), (second, 1)]));
        let make_strip = |retained, id, node| VirtualStrip {
            total_width: 200.0,
            retained,
            cards: vec![VirtualCard {
                id,
                x: 0.0,
                width: 100.0,
                content: Element::new(AccessibleProbe {
                    node,
                    expected_virtual_offset: Vector::ZERO,
                }),
            }],
        };
        let mut strip: Element<'_, ()> = Element::new(make_strip(Arc::clone(&retained), first, 1));
        let mut tree = Tree::new(&strip);
        let layout = layout::Node::with_children(
            Size::new(200.0, 80.0),
            vec![layout::Node::new(Size::new(100.0, 80.0))],
        );
        for (id, initial, expected) in [
            (first, 2, 1), // The same band diffs in place.
            (second, 3, 3),
            (first, 4, 1), // Remount restores the first card's state.
        ] {
            strip = Element::new(make_strip(Arc::clone(&retained), id, initial));
            tree.diff(&mut strip);
            assert!(
                strip
                    .as_widget()
                    .a11y_nodes(Layout::new(&layout), &tree, mouse::Cursor::Unavailable,)
                    .contains(&A11yId::from(expected as u64))
            );
        }

        // Close the first card while only the second is mounted. Reusing its
        // identity afterwards must create fresh state, not revive a stale tree.
        strip = Element::new(make_strip(Arc::clone(&retained), second, 5));
        tree.diff(&mut strip);
        strip = Element::new(make_strip(
            Arc::new(HashMap::from([(second, 0)])),
            second,
            6,
        ));
        tree.diff(&mut strip);
        strip = Element::new(make_strip(retained, first, 7));
        tree.diff(&mut strip);
        assert!(
            strip
                .as_widget()
                .a11y_nodes(Layout::new(&layout), &tree, mouse::Cursor::Unavailable,)
                .contains(&A11yId::from(7_u64))
        );
    }

    #[test]
    fn workspace_wrappers_preserve_accessible_card_descendants() {
        use iced_accessibility::A11yId;

        let cards = [CardId::new(), CardId::new()];
        let strip = VirtualStrip {
            total_width: 200.0,
            retained: Arc::new(
                cards
                    .into_iter()
                    .enumerate()
                    .map(|(index, id)| (id, index))
                    .collect(),
            ),
            cards: cards
                .into_iter()
                .enumerate()
                .map(|(index, id)| VirtualCard {
                    id,
                    x: index as f32 * 100.0,
                    width: 100.0,
                    content: CardSurface::new(Element::new(AccessibleProbe {
                        node: index as u64 + 1,
                        expected_virtual_offset: Vector::new(32.0, 0.0),
                    }))
                    .into(),
                })
                .collect(),
        };
        let shortcuts = KeyboardShortcuts::default();
        let surface: Element<'_, ()> =
            Element::new(WorkspaceSurface::new(strip, &shortcuts, 0.0, |_| ()));
        let tree = Tree::new(&surface);
        let card_node = || {
            layout::Node::with_children(
                Size::new(100.0, 80.0),
                vec![layout::Node::new(Size::new(100.0, 80.0))],
            )
        };
        let strip_node = layout::Node::with_children(
            Size::new(200.0, 80.0),
            vec![card_node(), card_node().move_to(Point::new(100.0, 0.0))],
        );
        let surface_node = layout::Node::with_children(Size::new(200.0, 80.0), vec![strip_node]);
        let accessible = surface.as_widget().a11y_nodes(
            Layout::new(&surface_node).with_virtual_offset(Vector::new(32.0, 0.0)),
            &tree,
            mouse::Cursor::Unavailable,
        );

        assert!(accessible.contains(&A11yId::from(1_u64)));
        assert!(accessible.contains(&A11yId::from(2_u64)));
        assert_eq!(accessible.root().len(), 2);
    }

    #[test]
    fn composed_workspace_view_exposes_every_mounted_card_to_a_screen_reader() {
        use cosmic::iced::advanced::renderer::Headless;
        use iced_accessibility::A11yId;

        const VIEWPORT_WIDTH: f32 = 700.0;
        const HEIGHT: f32 = 480.0;

        let mut workspace = Workspace::default();
        workspace.resize_viewport(VIEWPORT_WIDTH);
        let cards: Vec<CardId> = (0..8).map(|_| CardId::new()).collect();
        for (index, id) in cards.iter().copied().enumerate() {
            workspace
                .add_card(
                    WorkspaceCard::new(id, format!("Card {index}"), CardWidth::Fixed(320.0))
                        .unwrap(),
                    index,
                    index == 0,
                )
                .unwrap();
        }
        let projection = workspace.projection(VIEWPORT_WIDTH);
        let mounted: Vec<CardId> = projection.mounted_ids().collect();
        assert!(mounted.len() < cards.len());

        let probe_id = |id: CardId| {
            cards
                .iter()
                .position(|card| *card == id)
                .expect("probe for a card the workspace holds") as u64
                + 1
        };
        let shortcuts = KeyboardShortcuts::default();
        let mut composed: Element<'_, ()> = view(
            &workspace,
            &shortcuts,
            true,
            VIEWPORT_WIDTH,
            widget::Id::unique(),
            |context| {
                Element::new(AccessibleProbe {
                    node: probe_id(context.id),
                    expected_virtual_offset: Vector::ZERO,
                })
            },
            card_widget_id,
            Events { viewport: |_| () },
        );
        let mut tree = Tree::new(&composed);
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
        let surface_node = composed.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(VIEWPORT_WIDTH, HEIGHT)),
        );
        let accessible = composed.as_widget().a11y_nodes(
            Layout::new(&surface_node),
            &tree,
            mouse::Cursor::Unavailable,
        );

        assert!(
            !accessible.root().is_empty(),
            "the workspace must reach a screen reader through every wrapper the view builds"
        );
        for id in &mounted {
            assert!(
                accessible.contains(&A11yId::from(probe_id(*id))),
                "mounted card {id} is missing from the accessibility tree"
            );
        }
        assert_eq!(
            cards
                .iter()
                .filter(|id| accessible.contains(&A11yId::from(probe_id(**id))))
                .count(),
            mounted.len(),
            "the accessibility tree must expose the mounted cards and nothing else"
        );
    }

    #[test]
    fn only_configured_global_chords_are_reserved_from_card_children() {
        use keyboard::key::{Code, Physical};

        let shortcuts = KeyboardShortcuts::default();
        let reserved = |character, code, modifiers| {
            is_global_shortcut_chord(
                &keyboard::Key::Character(character),
                Physical::Code(code),
                modifiers,
                &shortcuts,
            )
        };
        assert!(reserved("t".into(), Code::KeyT, keyboard::Modifiers::CTRL));
        assert!(!reserved("q".into(), Code::KeyQ, keyboard::Modifiers::CTRL));
        assert!(reserved("q".into(), Code::KeyQ, keyboard::Modifiers::ALT));
        assert!(!reserved("a".into(), Code::KeyA, keyboard::Modifiers::ALT));
        assert!(reserved("w".into(), Code::KeyW, keyboard::Modifiers::ALT));
        assert!(!reserved("x".into(), Code::KeyX, keyboard::Modifiers::ALT));
        assert!(!reserved(
            "q".into(),
            Code::KeyQ,
            keyboard::Modifiers::ALT | keyboard::Modifiers::CTRL,
        ));
        assert!(!reserved(
            "t".into(),
            Code::KeyT,
            keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
        ));
    }

    #[test]
    fn held_q_and_w_navigation_cross_four_cards_and_stop_at_edges() {
        use keyboard::key::Code;

        let mut workspace = Workspace::default();
        let cards = [CardId::new(), CardId::new(), CardId::new(), CardId::new()];
        for (index, id) in cards.into_iter().enumerate() {
            workspace
                .add_card(
                    WorkspaceCard::new(id, format!("Card {index}"), CardWidth::Fixed(320.0))
                        .unwrap(),
                    index,
                    false,
                )
                .unwrap();
        }
        workspace.focus(cards[0]).unwrap();

        let shortcuts = KeyboardShortcuts::default();
        for repeat in [false, true, true, true] {
            let shortcut = global_shortcut(
                &key_press("w", Code::KeyW, keyboard::Modifiers::ALT, repeat),
                &shortcuts,
            );
            let Some(GlobalShortcut::FocusAdjacent(direction)) = shortcut else {
                panic!("Alt+W must remain a navigation shortcut on repeated key presses");
            };
            workspace.focus_adjacent(direction).unwrap();
        }
        assert_eq!(workspace.focused(), Some(cards[3]));

        for repeat in [false, true, true, true] {
            let shortcut = global_shortcut(
                &key_press("q", Code::KeyQ, keyboard::Modifiers::ALT, repeat),
                &shortcuts,
            );
            let Some(GlobalShortcut::FocusAdjacent(direction)) = shortcut else {
                panic!("Alt+Q must remain a navigation shortcut on repeated key presses");
            };
            workspace.focus_adjacent(direction).unwrap();
        }
        assert_eq!(workspace.focused(), Some(cards[0]));
    }

    #[test]
    fn every_shortcut_descriptor_has_a_unique_configurable_binding() {
        let shortcuts = KeyboardShortcuts::default();
        let mut bindings = HashSet::new();
        for descriptor in GLOBAL_SHORTCUT_DESCRIPTORS {
            assert!(!descriptor.label.is_empty());
            let keys = shortcut_keys(descriptor.action, &shortcuts);
            assert!(!keys.is_empty());
            for chord in keys {
                assert!(parse_shortcut(chord).is_ok());
                // Distinct across every action, not merely within one, or a
                // chord would dispatch to whichever descriptor came first.
                assert!(bindings.insert(chord.clone()));
            }
            // The one-line form the dialog shows has to read back as the same
            // chords, or editing a two-chord action there would drop one.
            assert_eq!(
                parse_chords(&shortcut_chords(descriptor.action, &shortcuts)),
                *keys
            );
        }
    }

    #[test]
    fn frame_clock_bounds_throttled_frames_without_rewinding_for_stale_frames() {
        let start = Instant::now();
        let mut clock = FrameClock::default();
        assert_eq!(clock.elapsed(start), None);
        assert_eq!(
            clock.elapsed(start + Duration::from_millis(16)),
            Some(Duration::from_millis(16))
        );
        assert_eq!(
            clock.elapsed(start + Duration::from_secs(2)),
            Some(Duration::from_millis(250))
        );
        assert_eq!(clock.elapsed(start + Duration::from_secs(1)), None);
        assert_eq!(
            clock.elapsed(start + Duration::from_millis(2_016)),
            Some(Duration::from_millis(16))
        );
    }

    /// A leaf that always raises an overlay anchored to its own translated
    /// bounds, so a test can tell whether the pane chain forwards `overlay`
    /// and with which translation.
    struct OverlayProbe;

    struct ProbeOverlay {
        anchor: Rectangle,
    }

    impl Widget<(), Theme, Renderer> for OverlayProbe {
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
        fn overlay<'a>(
            &'a mut self,
            _tree: &'a mut Tree,
            layout: Layout<'a>,
            _renderer: &Renderer,
            _viewport: &Rectangle,
            translation: Vector,
        ) -> Option<overlay::Element<'a, (), Theme, Renderer>> {
            Some(overlay::Element::new(Box::new(ProbeOverlay {
                anchor: layout.bounds() + translation,
            })))
        }
    }

    impl overlay::Overlay<(), Theme, Renderer> for ProbeOverlay {
        fn layout(&mut self, _renderer: &Renderer, _bounds: Size) -> layout::Node {
            layout::Node::new(Size::new(10.0, 10.0)).move_to(self.anchor.position())
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
    }

    /// Every card an overlay can be raised from, paired with the screen
    /// position its overlay has to land on.
    fn expected_overlay_positions(projection: &ViewportProjection, viewport: f32) -> Vec<f32> {
        let mut positions: Vec<f32> = projection
            .mounted
            .iter()
            .map(|card| card.start - projection.offset)
            .filter(|x| *x < viewport && x + 400.0 > 0.0)
            .collect();
        positions.sort_by(f32::total_cmp);
        positions
    }

    #[test]
    fn card_overlays_survive_a_camera_that_moved_them_into_view() {
        use cosmic::iced::advanced::{renderer::Headless, widget::operation};
        const VIEWPORT: f32 = 600.0;
        const HEIGHT: f32 = 400.0;
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
        let mut workspace = Workspace::default();
        workspace.resize_viewport(VIEWPORT);
        for index in 0..4 {
            workspace
                .add_card(
                    WorkspaceCard::new(
                        CardId::new(),
                        format!("Pane {index}"),
                        CardWidth::Fixed(400.0),
                    )
                    .unwrap(),
                    index,
                    false,
                )
                .unwrap();
        }
        // Park the camera at the far end. The cards now on screen sit at
        // content-space positions well outside the visible rect, which is the
        // only arrangement that tells a correct visibility test from one that
        // compares content space against screen space: a workspace of one or
        // two cards has the two spaces on top of each other.
        let offset = workspace.geometry().max_offset(VIEWPORT);
        assert!(
            offset > VIEWPORT,
            "the fixture must scroll past a screenful"
        );
        workspace.native_scroll(offset, VIEWPORT);
        let projection = workspace.projection(VIEWPORT);
        let expected = expected_overlay_positions(&projection, VIEWPORT);
        assert!(
            expected.iter().any(|x| *x > 0.0),
            "the fixture must place a card the camera moved into view"
        );

        let scroll_id = widget::Id::new("workspace.scroll");
        let shortcuts = KeyboardShortcuts::default();
        let mut element: Element<'_, ()> = view(
            &workspace,
            &shortcuts,
            false,
            VIEWPORT,
            scroll_id.clone(),
            |_| Element::new(OverlayProbe),
            card_widget_id,
            Events { viewport: |_| () },
        );
        let mut tree = Tree::new(&element);
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(VIEWPORT, HEIGHT)),
        );
        // The camera the workspace reports and the offset the native
        // scrollable holds are two halves of the same scroll; the widget chain
        // only learns about the second one through `translation`.
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::scrollable::scroll_to::<()>(
                scroll_id,
                operation::scrollable::AbsoluteOffset {
                    x: Some(offset),
                    y: None,
                },
            ),
        );

        let visible = Rectangle::new(Point::ORIGIN, Size::new(VIEWPORT, HEIGHT));
        let mut raised = element
            .as_widget_mut()
            .overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &visible,
                Vector::ZERO,
            )
            .expect("a mounted card raised an overlay");
        let raised = raised.as_overlay_mut().layout(&renderer, visible.size());
        let mut positions: Vec<f32> = raised
            .children()
            .iter()
            .map(|child| child.bounds().x)
            .collect();
        positions.sort_by(f32::total_cmp);
        assert_eq!(positions, expected);
    }
}
