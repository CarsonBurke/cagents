use std::{
    collections::{HashMap, HashSet},
    mem,
    time::Duration,
};

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

use crate::model::CardId;
use crate::workspace::{
    FocusDirection, FocusHandoff, FocusTarget, ViewportProjection, WheelDelta, Workspace,
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
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PanGesture {
    Begin { pointer_x: f32 },
    Update { pointer_x: f32 },
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalShortcut {
    NewTerminal,
    CloseFocusedCard,
    FocusProjectSearch,
    FocusAdjacent(FocusDirection),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShortcutKey {
    Latin(char),
    ArrowLeft,
    ArrowRight,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ShortcutChord {
    modifiers: keyboard::Modifiers,
    key: ShortcutKey,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutDescriptor {
    pub action: GlobalShortcut,
    pub label: &'static str,
    pub keys: &'static str,
    pub spoken_keys: &'static str,
    pub repeatable: bool,
    chords: &'static [ShortcutChord],
}

const CTRL_T: &[ShortcutChord] = &[ShortcutChord {
    modifiers: keyboard::Modifiers::CTRL,
    key: ShortcutKey::Latin('t'),
}];
const CTRL_W: &[ShortcutChord] = &[ShortcutChord {
    modifiers: keyboard::Modifiers::CTRL,
    key: ShortcutKey::Latin('w'),
}];
const CTRL_F: &[ShortcutChord] = &[ShortcutChord {
    modifiers: keyboard::Modifiers::CTRL,
    key: ShortcutKey::Latin('f'),
}];
const PREVIOUS_COLUMN: &[ShortcutChord] = &[
    ShortcutChord {
        modifiers: keyboard::Modifiers::ALT,
        key: ShortcutKey::Latin('q'),
    },
    ShortcutChord {
        modifiers: keyboard::Modifiers::ALT,
        key: ShortcutKey::ArrowLeft,
    },
];
const NEXT_COLUMN: &[ShortcutChord] = &[
    ShortcutChord {
        modifiers: keyboard::Modifiers::ALT,
        key: ShortcutKey::Latin('w'),
    },
    ShortcutChord {
        modifiers: keyboard::Modifiers::ALT,
        key: ShortcutKey::ArrowRight,
    },
];

pub const GLOBAL_SHORTCUT_DESCRIPTORS: &[ShortcutDescriptor] = &[
    ShortcutDescriptor {
        action: GlobalShortcut::NewTerminal,
        label: "New terminal",
        keys: "Ctrl+T",
        spoken_keys: "Control plus T",
        repeatable: false,
        chords: CTRL_T,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CloseFocusedCard,
        label: "Close focused column",
        keys: "Ctrl+W",
        spoken_keys: "Control plus W",
        repeatable: false,
        chords: CTRL_W,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusProjectSearch,
        label: "Search projects",
        keys: "Ctrl+F",
        spoken_keys: "Control plus F",
        repeatable: false,
        chords: CTRL_F,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusAdjacent(FocusDirection::Left),
        label: "Previous column",
        keys: "Alt+Q / Alt+Left",
        spoken_keys: "Alt plus Q or Alt plus Left Arrow",
        repeatable: true,
        chords: PREVIOUS_COLUMN,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusAdjacent(FocusDirection::Right),
        label: "Next column",
        keys: "Alt+W / Alt+Right",
        spoken_keys: "Alt plus W or Alt plus Right Arrow",
        repeatable: true,
        chords: NEXT_COLUMN,
    },
];

pub fn shortcut_descriptor(action: GlobalShortcut) -> &'static ShortcutDescriptor {
    GLOBAL_SHORTCUT_DESCRIPTORS
        .iter()
        .find(|descriptor| descriptor.action == action)
        .expect("every dispatched global shortcut has a descriptor")
}

pub fn shortcut_tooltip(action: GlobalShortcut) -> String {
    let descriptor = shortcut_descriptor(action);
    format!("{} — {}", descriptor.label, descriptor.keys)
}

#[derive(Debug, Default)]
pub struct FrameClock {
    last: Option<Instant>,
}

impl FrameClock {
    pub fn elapsed(&mut self, frame: Instant) -> Option<Duration> {
        let previous = self.last.replace(frame)?;
        frame
            .checked_duration_since(previous)
            .filter(|elapsed| *elapsed <= Duration::from_millis(250))
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

pub fn workspace_scroll_id() -> widget::Id {
    widget::Id::new("cagents-workspace-horizontal-scroll")
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

pub fn active_animation_frames<Message, F>(
    workspace: &Workspace,
    app_window: window::Id,
    map: F,
) -> Subscription<Message>
where
    Message: Send + 'static,
    F: Fn(Instant) -> Message + Send + Clone + 'static,
{
    if workspace.camera().is_animating() {
        animation_frames(app_window, map)
    } else {
        Subscription::none()
    }
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

pub fn global_shortcut(event: &keyboard::Event) -> Option<GlobalShortcut> {
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
    let descriptor = global_shortcut_descriptor(key, *physical_key, *modifiers)?;
    (!*repeat || descriptor.repeatable).then_some(descriptor.action)
}

pub(crate) fn is_global_shortcut_chord(
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
) -> bool {
    global_shortcut_chord(key, physical_key, modifiers).is_some()
}

fn global_shortcut_chord(
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
) -> Option<GlobalShortcut> {
    global_shortcut_descriptor(key, physical_key, modifiers).map(|descriptor| descriptor.action)
}

fn global_shortcut_descriptor(
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
) -> Option<&'static ShortcutDescriptor> {
    let modifiers = modifiers.difference(keyboard::Modifiers::CAPS_LOCK);
    GLOBAL_SHORTCUT_DESCRIPTORS.iter().find(|descriptor| {
        descriptor.chords.iter().any(|chord| {
            chord.modifiers == modifiers
                && match chord.key {
                    ShortcutKey::Latin(expected) => {
                        latin_key(key, physical_key) == Some(expected)
                    }
                    ShortcutKey::ArrowLeft => {
                        matches!(
                            key.as_ref(),
                            keyboard::Key::Named(keyboard::key::Named::ArrowLeft)
                        )
                    }
                    ShortcutKey::ArrowRight => {
                        matches!(
                            key.as_ref(),
                            keyboard::Key::Named(keyboard::key::Named::ArrowRight)
                        )
                    }
                }
        })
    })
}

fn latin_key(key: &keyboard::Key, physical_key: keyboard::key::Physical) -> Option<char> {
    key.to_latin(physical_key)
        .map(|key| key.to_ascii_lowercase())
}

/// Renders the workspace strip.
///
/// `container_id` supplies each card's stable container identity, which
/// scroll-to-card operations address. It is a lookup rather than something this
/// function rebuilds because `widget::Id::new` allocates, and every mounted
/// card would pay that on every redraw; the owner is expected to return a
/// cached clone.
pub fn view<'a, Message, BuildCard, ContainerId, ViewportMessage, PanMessage, WheelMessage>(
    workspace: &'a Workspace,
    scroll_id: widget::Id,
    build_card: BuildCard,
    container_id: ContainerId,
    on_viewport: ViewportMessage,
    on_pan: PanMessage,
    on_wheel: WheelMessage,
) -> Element<'a, Message>
where
    Message: 'static,
    BuildCard: Fn(CardViewContext) -> Element<'a, Message> + 'a,
    ContainerId: Fn(CardId) -> widget::Id + 'a,
    ViewportMessage: Fn(WorkspaceViewportEvent) -> Message + Clone + 'a,
    PanMessage: Fn(PanGesture) -> Message + Clone + 'a,
    WheelMessage: Fn(WheelDelta) -> Message + Clone + 'a,
{
    widget::responsive(move |available| {
        let projection = workspace.projection(available.width);
        let strip = VirtualStrip::new(
            &projection,
            workspace.order().collect(),
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
                        content: CardSurface::new(content).into(),
                    })
                })
                .collect(),
        );
        let on_viewport_for_scroll = on_viewport.clone();
        let scrollable = cosmic::widget::scrollable::horizontal(strip)
            .id(scroll_id.clone())
            .class(cosmic::style::iced::Scrollable::Minimal)
            .width(Length::Fill)
            .height(Length::Fill)
            // Minimal native chrome auto-hides the rail when idle and omits it
            // entirely when the strip does not overflow.
            .on_scroll(move |viewport| {
                on_viewport_for_scroll(WorkspaceViewportEvent::Scrolled(ViewportSample {
                    offset: viewport.absolute_offset().x,
                    width: viewport.bounds().width,
                }))
            });

        let on_viewport_for_resize = on_viewport.clone();
        widget::sensor(PanSurface::new(
            scrollable,
            on_pan.clone(),
            on_wheel.clone(),
        ))
        .on_resize(move |size| on_viewport_for_resize(WorkspaceViewportEvent::Resized(size.width)))
        .into()
    })
    .into()
}

struct CardSurface<'a, Message> {
    content: Element<'a, Message>,
}

impl<'a, Message> CardSurface<'a, Message> {
    fn new(content: impl Into<Element<'a, Message>>) -> Self {
        Self {
            content: content.into(),
        }
    }

    fn child_layout<'layout>(layout: Layout<'layout>) -> Layout<'layout> {
        layout
            .children()
            .next()
            .expect("card surface content layout")
            .with_virtual_offset(layout.virtual_offset())
    }
}

#[derive(Debug, Default)]
enum SurfacePointer {
    #[default]
    Unseen,
    At(Point),
    Gone,
}

impl SurfacePointer {
    fn shown(&self, cursor: mouse::Cursor) -> mouse::Cursor {
        match self {
            Self::Unseen => cursor,
            Self::At(position) => mouse::Cursor::Available(*position),
            Self::Gone => mouse::Cursor::Unavailable,
        }
    }
}

fn clipped_viewport(bounds: Rectangle, viewport: &Rectangle) -> Option<Rectangle> {
    bounds.intersection(viewport)
}

fn should_forward_to_raw_children(event_captured: bool) -> bool {
    !event_captured
}

impl<Message> Widget<Message, Theme, Renderer> for CardSurface<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<SurfacePointer>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(SurfacePointer::default())
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
        let seen = match event {
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                Some(SurfacePointer::At(*position))
            }
            Event::Mouse(mouse::Event::CursorLeft) => Some(SurfacePointer::Gone),
            _ => None,
        };
        if let Some(seen) = seen {
            *tree.state.downcast_mut::<SurfacePointer>() = seen;
            shell.request_redraw();
        }

        let Some(clipped) = clipped_viewport(layout.bounds(), viewport) else {
            return;
        };
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
        self.content.as_widget().mouse_interaction(
            &tree.children[0],
            Self::child_layout(layout),
            tree.state.downcast_ref::<SurfacePointer>().shown(cursor),
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
        let shown = tree.state.downcast_ref::<SurfacePointer>().shown(cursor);
        renderer.with_layer(clipped, |renderer| {
            self.content.as_widget().draw(
                &tree.children[0],
                renderer,
                _theme,
                style,
                Self::child_layout(layout),
                shown,
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
        let clipped = clipped_viewport(layout.bounds(), viewport)?;
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

impl<'a, Message: 'a> From<CardSurface<'a, Message>> for Element<'a, Message> {
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
    retained: HashSet<CardId>,
    cards: Vec<VirtualCard<'a, Message>>,
}

impl<'a, Message> VirtualStrip<'a, Message> {
    fn new(
        projection: &ViewportProjection,
        retained: Vec<CardId>,
        cards: Vec<VirtualCard<'a, Message>>,
    ) -> Self {
        debug_assert_eq!(
            cards.iter().map(|card| card.id).collect::<Vec<_>>(),
            projection.mounted_ids().collect::<Vec<_>>()
        );
        Self {
            total_width: projection.total_width,
            retained: retained.into_iter().collect(),
            cards,
        }
    }
}

#[derive(Default)]
struct VirtualStripState {
    mounted: Vec<CardId>,
    detached: HashMap<CardId, Tree>,
}

impl<Message> Widget<Message, Theme, Renderer> for VirtualStrip<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<VirtualStripState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(VirtualStripState {
            mounted: self.cards.iter().map(|card| card.id).collect(),
            detached: HashMap::new(),
        })
    }

    fn children(&self) -> Vec<Tree> {
        self.cards
            .iter()
            .map(|card| Tree::new(&card.content))
            .collect()
    }

    fn diff(&mut self, tree: &mut Tree) {
        let old_children = mem::take(&mut tree.children);
        let state = tree.state.downcast_mut::<VirtualStripState>();
        let old_ids = mem::take(&mut state.mounted);
        for (id, child) in old_ids.into_iter().zip(old_children) {
            state.detached.insert(id, child);
        }
        state.detached.retain(|id, _| self.retained.contains(id));

        let mut children = Vec::with_capacity(self.cards.len());
        for card in &mut self.cards {
            let mut child = state
                .detached
                .remove(&card.id)
                .unwrap_or_else(|| Tree::new(&card.content));
            child.diff(&mut card.content);
            children.push(child);
        }
        state.mounted = self.cards.iter().map(|card| card.id).collect();
        tree.children = children;
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
            if child_layout.bounds().intersects(viewport) {
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
            .filter(|(_, child_layout)| child_layout.bounds().intersects(viewport))
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

struct PanSurface<'a, Message> {
    content: Element<'a, Message>,
    on_pan: Box<dyn Fn(PanGesture) -> Message + 'a>,
    on_wheel: Box<dyn Fn(WheelDelta) -> Message + 'a>,
}

impl<'a, Message> PanSurface<'a, Message> {
    fn new(
        content: impl Into<Element<'a, Message>>,
        on_pan: impl Fn(PanGesture) -> Message + 'a,
        on_wheel: impl Fn(WheelDelta) -> Message + 'a,
    ) -> Self {
        Self {
            content: content.into(),
            on_pan: Box::new(on_pan),
            on_wheel: Box::new(on_wheel),
        }
    }

    fn child_layout<'layout>(layout: Layout<'layout>) -> Layout<'layout> {
        layout
            .children()
            .next()
            .expect("pan surface content layout")
            .with_virtual_offset(layout.virtual_offset())
    }
}

#[derive(Default)]
struct PanSurfaceState {
    dragging: bool,
    modifiers: keyboard::Modifiers,
}

impl<Message> Widget<Message, Theme, Renderer> for PanSurface<'_, Message> {
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<PanSurfaceState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(PanSurfaceState::default())
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
        let state = tree.state.downcast_mut::<PanSurfaceState>();

        if let Event::Keyboard(keyboard::Event::ModifiersChanged(modifiers)) = event {
            state.modifiers = *modifiers;
        }

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
            ) if is_global_shortcut_chord(key, *physical_key, *modifiers) => {
                shell.capture_event();
                return;
            }
            Event::Mouse(mouse::Event::WheelScrolled { delta })
                if cursor.is_over(layout.bounds()) =>
            {
                let delta = match *delta {
                    mouse::ScrollDelta::Lines { x, y } => WheelDelta::Lines { x, y },
                    mouse::ScrollDelta::Pixels { x, y } => WheelDelta::Pixels { x, y },
                };
                if let Some(delta) = horizontal_pan_delta(delta, state.modifiers.shift()) {
                    shell.publish((self.on_wheel)(delta));
                    shell.capture_event();
                    return;
                }
            }
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Middle))
                if cursor.is_over(layout.bounds()) =>
            {
                if let Some(position) = cursor.position() {
                    state.dragging = true;
                    shell.publish((self.on_pan)(PanGesture::Begin {
                        pointer_x: position.x,
                    }));
                    shell.capture_event();
                    return;
                }
            }
            Event::Mouse(mouse::Event::CursorMoved { position }) if state.dragging => {
                shell.publish((self.on_pan)(PanGesture::Update {
                    pointer_x: position.x,
                }));
                shell.capture_event();
                return;
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Middle)) if state.dragging => {
                state.dragging = false;
                shell.publish((self.on_pan)(PanGesture::End));
                shell.capture_event();
                return;
            }
            Event::Window(window::Event::Unfocused) if state.dragging => {
                state.dragging = false;
                shell.publish((self.on_pan)(PanGesture::End));
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
    }

    fn mouse_interaction(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        viewport: &Rectangle,
        renderer: &Renderer,
    ) -> mouse::Interaction {
        let state = tree.state.downcast_ref::<PanSurfaceState>();
        if state.dragging {
            mouse::Interaction::Grabbing
        } else if let Some(clipped) = clipped_viewport(layout.bounds(), viewport) {
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
        let clipped = clipped_viewport(layout.bounds(), viewport)?;
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

fn horizontal_pan_delta(delta: WheelDelta, shift: bool) -> Option<WheelDelta> {
    let (x, y) = match delta {
        WheelDelta::Lines { x, y } | WheelDelta::Pixels { x, y } => (x, y),
    };
    let horizontal = x.abs() > y.abs() && x.abs() > f32::EPSILON;
    let component = if horizontal {
        x
    } else if shift && y.abs() > f32::EPSILON {
        y
    } else {
        return None;
    };
    Some(match delta {
        WheelDelta::Lines { .. } => WheelDelta::Lines {
            x: component,
            y: 0.0,
        },
        WheelDelta::Pixels { .. } => WheelDelta::Pixels {
            x: component,
            y: 0.0,
        },
    })
}

impl<'a, Message: 'a> From<PanSurface<'a, Message>> for Element<'a, Message> {
    fn from(surface: PanSurface<'a, Message>) -> Self {
        Element::new(surface)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace::{CardWidth, WorkspaceCard};

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

    #[test]
    fn wheel_arbitration_reserves_only_horizontal_or_shifted_vertical_input() {
        assert_eq!(
            horizontal_pan_delta(WheelDelta::Pixels { x: -32.0, y: 2.0 }, false,),
            Some(WheelDelta::Pixels { x: -32.0, y: 0.0 })
        );
        assert_eq!(
            horizontal_pan_delta(WheelDelta::Pixels { x: 2.0, y: -32.0 }, false,),
            None
        );
        assert_eq!(
            horizontal_pan_delta(WheelDelta::Lines { x: 0.0, y: -3.0 }, true,),
            Some(WheelDelta::Lines { x: -3.0, y: 0.0 })
        );
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

    #[test]
    fn global_shortcuts_match_native_chords_and_repeat_presses() {
        use keyboard::key::Code;

        assert_eq!(
            global_shortcut(&key_press(
                "t",
                Code::KeyT,
                keyboard::Modifiers::CTRL,
                false
            )),
            Some(GlobalShortcut::NewTerminal)
        );
        assert_eq!(
            global_shortcut(&key_press(
                "w",
                Code::KeyW,
                keyboard::Modifiers::CTRL,
                false
            )),
            Some(GlobalShortcut::CloseFocusedCard)
        );
        assert_eq!(
            global_shortcut(&key_press(
                "F",
                Code::KeyF,
                keyboard::Modifiers::CTRL | keyboard::Modifiers::CAPS_LOCK,
                false,
            )),
            Some(GlobalShortcut::FocusProjectSearch)
        );
        for (character, code) in [("t", Code::KeyT), ("w", Code::KeyW), ("f", Code::KeyF)] {
            assert_eq!(
                global_shortcut(&key_press(character, code, keyboard::Modifiers::CTRL, true,)),
                None,
                "held Ctrl shortcuts must not repeat destructive/creation actions"
            );
        }
        for repeat in [false, true] {
            assert_eq!(
                global_shortcut(&key_press(
                    "A",
                    Code::KeyA,
                    keyboard::Modifiers::ALT,
                    repeat,
                )),
                None,
                "Alt+A must be forwarded on initial and repeated key presses"
            );
        }
        assert_eq!(
            global_shortcut(&key_press("q", Code::KeyQ, keyboard::Modifiers::ALT, true)),
            Some(GlobalShortcut::FocusAdjacent(FocusDirection::Left))
        );

        assert_eq!(
            global_shortcut(&key_press("w", Code::KeyW, keyboard::Modifiers::ALT, true)),
            Some(GlobalShortcut::FocusAdjacent(FocusDirection::Right))
        );
    }

    #[test]
    fn captured_typing_is_not_forwarded_to_raw_card_children() {
        assert!(!should_forward_to_raw_children(true));
        assert!(should_forward_to_raw_children(false));
    }

    struct AccessibleProbe(u64);

    impl Widget<(), Theme, Renderer> for AccessibleProbe {
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
            _state: &Tree,
            _cursor: mouse::Cursor,
        ) -> iced_accessibility::A11yTree {
            use iced_accessibility::accesskit::{Node, Role};

            assert_eq!(layout.virtual_offset(), Vector::new(32.0, 0.0));
            iced_accessibility::A11yTree::leaf(Node::new(Role::Button), self.0)
        }
    }

    #[test]
    fn workspace_wrappers_preserve_accessible_card_descendants() {
        use iced_accessibility::A11yId;

        let cards = [CardId::new(), CardId::new()];
        let strip = VirtualStrip {
            total_width: 200.0,
            retained: cards.into_iter().collect(),
            cards: cards
                .into_iter()
                .enumerate()
                .map(|(index, id)| VirtualCard {
                    id,
                    x: index as f32 * 100.0,
                    width: 100.0,
                    content: CardSurface::new(Element::new(AccessibleProbe(index as u64 + 1)))
                        .into(),
                })
                .collect(),
        };
        let surface: Element<'_, ()> = Element::new(PanSurface::new(strip, |_| (), |_| ()));
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
    fn only_global_chords_are_reserved_from_terminal_and_browser_children() {
        use keyboard::key::{Code, Physical};

        assert!(is_global_shortcut_chord(
            &keyboard::Key::Character("t".into()),
            Physical::Code(Code::KeyT),
            keyboard::Modifiers::CTRL,
        ));
        assert!(!is_global_shortcut_chord(
            &keyboard::Key::Character("q".into()),
            Physical::Code(Code::KeyQ),
            keyboard::Modifiers::CTRL,
        ));
        assert!(is_global_shortcut_chord(
            &keyboard::Key::Character("q".into()),
            Physical::Code(Code::KeyQ),
            keyboard::Modifiers::ALT,
        ));
        assert!(!is_global_shortcut_chord(
            &keyboard::Key::Character("a".into()),
            Physical::Code(Code::KeyA),
            keyboard::Modifiers::ALT,
        ));
        assert!(is_global_shortcut_chord(
            &keyboard::Key::Character("w".into()),
            Physical::Code(Code::KeyW),
            keyboard::Modifiers::ALT,
        ));
        assert!(!is_global_shortcut_chord(
            &keyboard::Key::Character("x".into()),
            Physical::Code(Code::KeyX),
            keyboard::Modifiers::ALT,
        ));
        assert!(!is_global_shortcut_chord(
            &keyboard::Key::Character("q".into()),
            Physical::Code(Code::KeyQ),
            keyboard::Modifiers::ALT | keyboard::Modifiers::CTRL,
        ));
        assert!(!is_global_shortcut_chord(
            &keyboard::Key::Character("t".into()),
            Physical::Code(Code::KeyT),
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

        for repeat in [false, true, true, true] {
            let shortcut = global_shortcut(&key_press(
                "w",
                Code::KeyW,
                keyboard::Modifiers::ALT,
                repeat,
            ));
            let Some(GlobalShortcut::FocusAdjacent(direction)) = shortcut else {
                panic!("Alt+W must remain a navigation shortcut on repeated key presses");
            };
            workspace.focus_adjacent(direction).unwrap();
        }
        assert_eq!(workspace.focused(), Some(cards[3]));

        for repeat in [false, true, true, true] {
            let shortcut = global_shortcut(&key_press(
                "q",
                Code::KeyQ,
                keyboard::Modifiers::ALT,
                repeat,
            ));
            let Some(GlobalShortcut::FocusAdjacent(direction)) = shortcut else {
                panic!("Alt+Q must remain a navigation shortcut on repeated key presses");
            };
            workspace.focus_adjacent(direction).unwrap();
        }
        assert_eq!(workspace.focused(), Some(cards[0]));
    }

    #[test]
    fn every_shortcut_descriptor_is_the_dispatchers_single_source_of_truth() {
        use keyboard::key::{Code, Named, Physical};

        for descriptor in GLOBAL_SHORTCUT_DESCRIPTORS {
            assert!(!descriptor.label.is_empty());
            assert!(!descriptor.keys.is_empty());
            assert!(!descriptor.spoken_keys.is_empty());
            for chord in descriptor.chords {
                let (key, physical) = match chord.key {
                    ShortcutKey::Latin(character) => {
                        let code = match character {
                            't' => Code::KeyT,
                            'w' => Code::KeyW,
                            'f' => Code::KeyF,
                            'q' => Code::KeyQ,
                            other => panic!("unexpected descriptor key {other}"),
                        };
                        (keyboard::Key::Character(character.to_string().into()), Physical::Code(code))
                    }
                    ShortcutKey::ArrowLeft => (
                        keyboard::Key::Named(Named::ArrowLeft),
                        Physical::Code(Code::ArrowLeft),
                    ),
                    ShortcutKey::ArrowRight => (
                        keyboard::Key::Named(Named::ArrowRight),
                        Physical::Code(Code::ArrowRight),
                    ),
                };
                assert_eq!(
                    global_shortcut_chord(&key, physical, chord.modifiers),
                    Some(descriptor.action),
                    "{} must dispatch from its displayed descriptor",
                    descriptor.keys
                );
            }
        }
        assert!(GLOBAL_SHORTCUT_DESCRIPTORS.iter().all(|descriptor| {
            !descriptor.keys.contains("Alt+A") && !descriptor.spoken_keys.contains("Alt plus A")
        }));
    }

    #[test]
    fn frame_clock_ignores_first_and_stale_frames() {
        let start = Instant::now();
        let mut clock = FrameClock::default();
        assert_eq!(clock.elapsed(start), None);
        assert_eq!(
            clock.elapsed(start + Duration::from_millis(16)),
            Some(Duration::from_millis(16))
        );
        assert_eq!(clock.elapsed(start + Duration::from_secs(2)), None);
        assert_eq!(
            clock.elapsed(start + Duration::from_millis(2_016)),
            Some(Duration::from_millis(16))
        );
    }
}
