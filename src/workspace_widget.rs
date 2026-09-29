use std::{cell::Cell, collections::HashMap, mem, rc::Rc, sync::Arc, time::Duration};

use cosmic::iced::advanced::Renderer as _;
use cosmic::iced::advanced::{
    Clipboard, Layout, Shell, Widget, layout, overlay, renderer,
    widget::{Operation, Tree, tree},
};
use cosmic::iced::clipboard::dnd::DndDestinationRectangle;
use cosmic::iced::widget;
use cosmic::iced::{
    Event, Length, Point, Rectangle, Size, Subscription, Vector, keyboard, mouse, time::Instant,
    window,
};
use cosmic::{Element, Renderer, Task, Theme};

use crate::{
    input_trace::InputTrace,
    model::CardId,
    settings::{KeyboardShortcuts, ShortcutBinding, ShortcutKey, format_chords, parse_shortcut},
    workspace::{FocusDirection, FocusHandoff, FocusTarget, ViewportProjection, Workspace},
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportSample {
    pub offset: f32,
    pub width: f32,
    /// Wheel input moves the camera; native layout feedback only acknowledges it.
    pub user_input: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkspaceViewportEvent {
    Scrolled(ViewportSample),
    Resized(f32),
    PointerFocused(CardId),
    /// The pointer moved into this pane and rested there for
    /// [`HOVER_ACTIVATION_DWELL`] without a click or a key press in between.
    PointerRested(CardId),
    Reordered {
        card: CardId,
        before: Option<CardId>,
    },
    DragScrolled(f32),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlobalShortcut {
    ToggleProjects,
    RenamePane,
    CommandPalette,
    NewThread,
    ToggleInspector,
    OpenSettings,
    MovePaneLeft,
    MovePaneRight,
    ReopenPane,
    FocusPane1,
    FocusPane2,
    FocusPane3,
    FocusPane4,
    FocusPane5,
    FocusPane6,
    FocusPane7,
    FocusPane8,
    FocusPane9,
    NewTerminal,
    NewChannel,
    ClonePane,
    CloseFocusedCard,
    FocusProjectSearch,
    FocusAdjacent(FocusDirection),
    CyclePaneWidth,
    /// Moves between the top-level workspaces: every project's worktrees and
    /// channels, in the order the sidebar lists them.
    CycleWorkspace(FocusDirection),
    /// Flips the viewed mark on the file the focused review pane shows.
    ReviewToggleViewed,
    ReviewNextFile,
    ReviewPreviousFile,
    ReviewNextUnviewed,
    ReviewNextChange,
    ReviewPreviousChange,
}

/// Where a shortcut applies, which decides when its chord is withheld from
/// the process inside a card.
///
/// This is the workspace's answer to VS Code's "commands to skip shell": a
/// focused terminal receives every chord except those of the actions whose
/// scope reaches over it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShortcutScope {
    /// Anywhere in the workspace, a focused terminal included. Its default
    /// chords are ones a shell has no use for.
    Workspace,
    /// Anywhere but a focused terminal, which receives the chord as input.
    /// For an action whose chord means something to a shell and the same kind
    /// of thing in every other pane, such as `Ctrl+F`.
    OutsideTerminal,
    /// Only while a pull request pane has focus; a terminal or editor with
    /// focus receives the chord as ordinary input.
    Review,
}

impl ShortcutScope {
    /// Whether an action of this scope acts, and so withholds its chord from
    /// the card, while `focused` has focus.
    pub fn applies_to(self, focused: FocusedPane) -> bool {
        match self {
            Self::Workspace => true,
            Self::OutsideTerminal => focused != FocusedPane::Terminal,
            Self::Review => focused == FocusedPane::Review,
        }
    }
}

/// The kind of the focused pane, as far as shortcut scopes care.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum FocusedPane {
    /// A terminal, which forwards the keys it is given to its process.
    Terminal,
    /// A pull request review pane.
    Review,
    /// Any other pane, or none.
    #[default]
    Other,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutDescriptor {
    pub action: GlobalShortcut,
    pub label: &'static str,
    pub repeatable: bool,
    pub scope: ShortcutScope,
}

/// How long the pointer must rest in a pane it moved into before hover
/// activation gives that pane focus. Long enough that sweeping across the
/// strip - toward the inspector, say - activates nothing on the way; short
/// enough to read as a response to stopping there. It matches the tooltip
/// delay, the app's other reading of a pointer that stopped on purpose.
pub const HOVER_ACTIVATION_DWELL: Duration = Duration::from_millis(300);

pub const GLOBAL_SHORTCUT_DESCRIPTORS: &[ShortcutDescriptor] = &[
    ShortcutDescriptor {
        action: GlobalShortcut::ToggleProjects,
        label: "Toggle Projects sidebar",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::RenamePane,
        label: "Rename focused pane",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CommandPalette,
        label: "Open command palette",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::NewThread,
        label: "New thread",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ToggleInspector,
        label: "Toggle inspector",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::OpenSettings,
        label: "Open settings",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::MovePaneLeft,
        label: "Move pane left",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::MovePaneRight,
        label: "Move pane right",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReopenPane,
        label: "Reopen closed pane",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane1,
        label: "Focus pane 1",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane2,
        label: "Focus pane 2",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane3,
        label: "Focus pane 3",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane4,
        label: "Focus pane 4",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane5,
        label: "Focus pane 5",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane6,
        label: "Focus pane 6",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane7,
        label: "Focus pane 7",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane8,
        label: "Focus pane 8",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusPane9,
        label: "Focus pane 9",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::NewTerminal,
        label: "New terminal",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::NewChannel,
        label: "New channel",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ClonePane,
        label: "Clone focused pane",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CloseFocusedCard,
        label: "Close focused pane",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusProjectSearch,
        label: "Search projects",
        repeatable: false,
        scope: ShortcutScope::OutsideTerminal,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusAdjacent(FocusDirection::Left),
        label: "Previous pane",
        repeatable: true,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FocusAdjacent(FocusDirection::Right),
        label: "Next pane",
        repeatable: true,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CyclePaneWidth,
        label: "Cycle pane width",
        repeatable: true,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CycleWorkspace(FocusDirection::Left),
        label: "Previous workspace",
        repeatable: true,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::CycleWorkspace(FocusDirection::Right),
        label: "Next workspace",
        repeatable: true,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReviewToggleViewed,
        label: "Mark reviewed file viewed",
        repeatable: false,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReviewNextFile,
        label: "Next reviewed file",
        repeatable: true,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReviewPreviousFile,
        label: "Previous reviewed file",
        repeatable: true,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReviewNextUnviewed,
        label: "Next unviewed file",
        repeatable: true,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReviewNextChange,
        label: "Next changed hunk",
        repeatable: true,
        scope: ShortcutScope::Review,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ReviewPreviousChange,
        label: "Previous changed hunk",
        repeatable: true,
        scope: ShortcutScope::Review,
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
        GlobalShortcut::ToggleProjects => &shortcuts.toggle_projects,
        GlobalShortcut::RenamePane => &shortcuts.rename_pane,
        GlobalShortcut::CommandPalette => &shortcuts.command_palette,
        GlobalShortcut::NewThread => &shortcuts.new_native_thread,
        GlobalShortcut::ToggleInspector => &shortcuts.toggle_inspector,
        GlobalShortcut::OpenSettings => &shortcuts.open_settings,
        GlobalShortcut::MovePaneLeft => &shortcuts.move_pane_left,
        GlobalShortcut::MovePaneRight => &shortcuts.move_pane_right,
        GlobalShortcut::ReopenPane => &shortcuts.reopen_pane,
        GlobalShortcut::FocusPane1 => &shortcuts.focus_pane_1,
        GlobalShortcut::FocusPane2 => &shortcuts.focus_pane_2,
        GlobalShortcut::FocusPane3 => &shortcuts.focus_pane_3,
        GlobalShortcut::FocusPane4 => &shortcuts.focus_pane_4,
        GlobalShortcut::FocusPane5 => &shortcuts.focus_pane_5,
        GlobalShortcut::FocusPane6 => &shortcuts.focus_pane_6,
        GlobalShortcut::FocusPane7 => &shortcuts.focus_pane_7,
        GlobalShortcut::FocusPane8 => &shortcuts.focus_pane_8,
        GlobalShortcut::FocusPane9 => &shortcuts.focus_pane_9,
        GlobalShortcut::NewTerminal => &shortcuts.new_terminal,
        GlobalShortcut::NewChannel => &shortcuts.new_channel,
        GlobalShortcut::ClonePane => &shortcuts.clone_pane,
        GlobalShortcut::CloseFocusedCard => &shortcuts.close_focused_column,
        GlobalShortcut::FocusProjectSearch => &shortcuts.search_projects,
        GlobalShortcut::FocusAdjacent(FocusDirection::Left) => &shortcuts.previous_column,
        GlobalShortcut::FocusAdjacent(FocusDirection::Right) => &shortcuts.next_column,
        GlobalShortcut::CyclePaneWidth => &shortcuts.cycle_pane_width,
        GlobalShortcut::CycleWorkspace(FocusDirection::Left) => &shortcuts.previous_workspace,
        GlobalShortcut::CycleWorkspace(FocusDirection::Right) => &shortcuts.next_workspace,
        GlobalShortcut::ReviewToggleViewed => &shortcuts.review_toggle_viewed,
        GlobalShortcut::ReviewNextFile => &shortcuts.review_next_file,
        GlobalShortcut::ReviewPreviousFile => &shortcuts.review_previous_file,
        GlobalShortcut::ReviewNextUnviewed => &shortcuts.review_next_unviewed,
        GlobalShortcut::ReviewNextChange => &shortcuts.review_next_change,
        GlobalShortcut::ReviewPreviousChange => &shortcuts.review_previous_change,
    }
}

/// The chords bound to `action`, in the one-line form the settings pane
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

pub fn focus_handoff<Message, F>(
    handoff: FocusHandoff,
    toolbar_focus_id: widget::Id,
    card_focus: F,
) -> Task<Message>
where
    Message: Send + 'static,
    F: FnOnce(CardId) -> widget::Id,
{
    match handoff.target {
        FocusTarget::Card(id) => widget::operation::focus(card_focus(id)),
        FocusTarget::WorkspaceToolbar => widget::operation::focus(toolbar_focus_id),
        FocusTarget::Unchanged => Task::none(),
    }
}

pub fn focus_handoff_then<Message, F>(
    handoff: FocusHandoff,
    toolbar_focus_id: widget::Id,
    card_focus: F,
    after_focus: Message,
) -> Task<Message>
where
    Message: Send + 'static,
    F: FnOnce(CardId) -> widget::Id,
{
    focus_handoff(handoff, toolbar_focus_id, card_focus).chain(Task::done(after_focus))
}

/// The action a key press triggers while `focused` has focus. A chord whose
/// action does not apply there triggers nothing, since
/// [`is_global_shortcut_chord`] left it to the card.
pub fn global_shortcut(
    event: &keyboard::Event,
    shortcuts: &KeyboardShortcuts,
    focused: FocusedPane,
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
    (descriptor.scope.applies_to(focused) && (!*repeat || descriptor.repeatable))
        .then_some(descriptor.action)
}

/// Whether a chord is withheld from the process inside a card: whether it is
/// bound to an action whose scope applies to the `focused` pane. So a terminal
/// never loses an arrow chord to a review pane that is not there, nor its
/// forward-char to project search.
pub(crate) fn is_global_shortcut_chord(
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
    shortcuts: &KeyboardShortcuts,
    focused: FocusedPane,
) -> bool {
    crate::tab_history::is_chord(key, modifiers)
        || global_shortcut_descriptor(key, physical_key, modifiers, shortcuts)
            .is_some_and(|descriptor| descriptor.scope.applies_to(focused))
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
            ShortcutKey::ArrowUp => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::ArrowUp)
            ),
            ShortcutKey::ArrowDown => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::ArrowDown)
            ),
            ShortcutKey::Tab => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::Tab)
            ),
            ShortcutKey::PageUp => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::PageUp)
            ),
            ShortcutKey::PageDown => matches!(
                key.as_ref(),
                keyboard::Key::Named(keyboard::key::Named::PageDown)
            ),
            ShortcutKey::Function(number) => {
                use keyboard::key::Named;
                let keys = [
                    Named::F1,
                    Named::F2,
                    Named::F3,
                    Named::F4,
                    Named::F5,
                    Named::F6,
                    Named::F7,
                    Named::F8,
                    Named::F9,
                    Named::F10,
                    Named::F11,
                    Named::F12,
                ];
                keys.get(usize::from(number).saturating_sub(1))
                    .is_some_and(|expected| key.as_ref() == keyboard::Key::Named(*expected))
            }
        }
}

fn latin_key(key: &keyboard::Key, physical_key: keyboard::key::Physical) -> Option<char> {
    if let keyboard::Key::Character(value) = key.as_ref() {
        let mut characters = value.chars();
        if let (Some(character), None) = (characters.next(), characters.next())
            && character.is_ascii_graphic()
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
///
/// `input` is the trace the owner's [`crate::input_trace::observe`] keeps for
/// the whole window, which hover activation reads.
pub fn view<'a, Message, BuildCard, ContainerId, ViewportMessage>(
    workspace: &'a Workspace,
    shortcuts: &'a KeyboardShortcuts,
    focused_pane: FocusedPane,
    focus_follows_mouse: bool,
    input: &InputTrace,
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
                    let focused = workspace.focused() == Some(card.id);
                    let context = CardViewContext {
                        id: card.id,
                        focused,
                    };
                    let content =
                        cosmic::widget::id_container(build_card(context), container_id(card.id));
                    Some(VirtualCard {
                        id: card.id,
                        x: mounted.start,
                        width: mounted.width,
                        content: CardSurface::new(content)
                            .traced_by(input)
                            .on_pointer_focus(on_viewport(WorkspaceViewportEvent::PointerFocused(
                                card.id,
                            )))
                            // The active pane has nothing to gain from a rest.
                            .on_pointer_rest((focus_follows_mouse && !focused).then(|| {
                                on_viewport(WorkspaceViewportEvent::PointerRested(card.id))
                            }))
                            .into(),
                    })
                })
                .collect(),
        );
    let on_viewport_for_scroll = on_viewport.clone();
    let scroll_input = Rc::new(Cell::new(false));
    let input_for_scroll = Rc::clone(&scroll_input);
    // Reordering belongs to the tab strip's explicit title handles. A wrapper
    // around pane bodies intercepts Alt drags before terminals can route them
    // to mouse-tracking applications.
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
                user_input: input_for_scroll.get(),
            }))
        });

    // Scrollable exposes a composite ID that Iced inherits during diff even
    // when its requested ID changes. Key its entire subtree by worktree so
    // programmatic scroll operations cannot keep targeting the departed strip.
    WorkspaceSurface::new(
        cosmic::widget::id_container(scrollable, scroll_id.clone()),
        shortcuts,
        focused_pane,
        viewport_width,
        move |width: f32| on_viewport(WorkspaceViewportEvent::Resized(width)),
    )
    .with_scroll_input(scroll_input)
    .with_initial_scroll(scroll_id, workspace.camera().offset())
    .into()
}

struct CardSurface<'a, Message> {
    content: Element<'a, Message>,
    on_pointer_focus: Option<Message>,
    on_pointer_rest: Option<Message>,
    input: InputTrace,
}

impl<'a, Message> CardSurface<'a, Message> {
    fn new(content: impl Into<Element<'a, Message>>) -> Self {
        Self {
            content: content.into(),
            on_pointer_focus: None,
            on_pointer_rest: None,
            input: InputTrace::default(),
        }
    }

    /// The window's record of its input, which hover activation reads for
    /// where each motion began and for presses the pane never received.
    /// Without it the pane never counts the pointer as having crossed in.
    fn traced_by(mut self, input: &InputTrace) -> Self {
        self.input = input.clone();
        self
    }

    fn on_pointer_focus(mut self, message: Message) -> Self {
        self.on_pointer_focus = Some(message);
        self
    }

    /// Published once the pointer has moved into this pane and rested there;
    /// see [`CardPointerState`] for what counts.
    fn on_pointer_rest(mut self, message: Option<Message>) -> Self {
        self.on_pointer_rest = message;
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

/// Hover activation follows deliberate pointer movement only.
///
/// An entry counts when the pointer itself crossed into the pane: where the
/// motion began, according to the window's [`InputTrace`] and placed against
/// the pane where the pane is now, lay outside it. The trace, not the pane,
/// remembers that start, because a pane misses motions: over an overlay that
/// captured them, or while it was scrolled out of the mounted band. Being
/// uncovered is not an entry - a menu closing over the pane, the pointer
/// returning from a popup surface, the strip scrolling under a still pointer,
/// or a pane mounting beneath it all leave the pointer where it was.
///
/// An entry must then rest for [`HOVER_ACTIVATION_DWELL`], so sweeping across
/// the strip activates nothing on the way. A key or button pressed anywhere
/// before then cancels it, so a pointer that drifts while the reader types or
/// runs a shortcut leaves their keystrokes going where they were going. The
/// pointer must still be over the pane when the dwell ends, which a pane the
/// strip moved away meanwhile is not.
#[derive(Default)]
struct CardPointerState {
    /// When a crossing into the pane began the dwell it is still serving.
    entered_at: Option<Instant>,
    selecting: bool,
}

/// How late past its dwell a rest may still be reported. A pane scrolled out
/// of view receives no frames, so its dwell ends at the first frame it gets
/// back, however long after the pointer stopped there that is.
const HOVER_ACTIVATION_LATENESS: Duration = Duration::from_secs(1);

/// The cursor's position in the pane's coordinates, including while the
/// strip's own edge keeps it from counting as over the pane.
fn cursor_point(cursor: mouse::Cursor) -> Option<Point> {
    match cursor {
        mouse::Cursor::Available(point) | mouse::Cursor::Levitating(point) => Some(point),
        mouse::Cursor::Unavailable => None,
    }
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
            Event::Mouse(mouse::Event::ButtonPressed(button)) => {
                pointer.selecting |= *button == mouse::Button::Left;
                pointer.entered_at = None;
            }
            Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Left)) => {
                pointer.selecting = false
            }
            Event::Mouse(mouse::Event::CursorLeft) => *pointer = CardPointerState::default(),
            Event::Mouse(mouse::Event::CursorMoved { position }) => {
                let clip = clipped_viewport(layout.bounds(), viewport);
                let hovered = !pointer.selecting
                    && !shell.is_event_captured()
                    && clip.is_some_and(|clip| cursor.is_over(clip));
                // Where the motion began is replayed through the current
                // translation, so content scrolled under a still pointer did
                // not cross anything.
                let crossed_in = hovered
                    && self
                        .input
                        .previous_position()
                        .zip(cursor_point(cursor))
                        .zip(clip)
                        .is_some_and(|((previous, local), clip)| {
                            !clip.contains(previous + (local - *position))
                        });
                if !hovered {
                    pointer.entered_at = None;
                } else if crossed_in && self.on_pointer_rest.is_some() {
                    let now = Instant::now();
                    pointer.entered_at = Some(now);
                    shell.request_redraw_at(now + HOVER_ACTIVATION_DWELL);
                }
            }
            Event::Window(window::Event::RedrawRequested(now)) => {
                if let Some(entered) = pointer.entered_at {
                    let due = entered + HOVER_ACTIVATION_DWELL;
                    if *now < due {
                        shell.request_redraw_at(due);
                    } else {
                        pointer.entered_at = None;
                        let rested = now.saturating_duration_since(due)
                            <= HOVER_ACTIVATION_LATENESS
                            && !self.input.pressed_since(entered)
                            && clipped_viewport(layout.bounds(), viewport)
                                .is_some_and(|clip| cursor.is_over(clip));
                        // Gone when the pane became active in the meantime.
                        if rested && let Some(message) = &self.on_pointer_rest {
                            shell.publish(message.clone());
                        }
                    }
                }
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
    /// The kind of the focused card; see [`is_global_shortcut_chord`].
    focused_pane: FocusedPane,
    /// The strip viewport width the owner already holds, which is what the
    /// enclosing `view` laid this surface's content out against.
    ///
    /// Size reporting lives in this widget instead of a `widget::sensor`
    /// wrapper because `sensor` reports no accessibility nodes, and wrapping
    /// the strip in one hid every card from screen readers.
    reported_width: f32,
    on_resize: Box<dyn Fn(f32) -> Message + 'a>,
    initial_scroll: Option<(widget::Id, f32)>,
    scroll_input: Option<Rc<Cell<bool>>>,
}

#[derive(Default)]
struct WorkspaceSurfaceState {
    initialized_scroll: Option<widget::Id>,
}

impl<'a, Message> WorkspaceSurface<'a, Message> {
    fn new(
        content: impl Into<Element<'a, Message>>,
        shortcuts: &'a KeyboardShortcuts,
        focused_pane: FocusedPane,
        reported_width: f32,
        on_resize: impl Fn(f32) -> Message + 'a,
    ) -> Self {
        Self {
            content: content.into(),
            shortcuts,
            focused_pane,
            reported_width,
            on_resize: Box::new(on_resize),
            initial_scroll: None,
            scroll_input: None,
        }
    }

    fn with_initial_scroll(mut self, id: widget::Id, offset: f32) -> Self {
        self.initial_scroll = Some((id, offset));
        self
    }

    fn with_scroll_input(mut self, input: Rc<Cell<bool>>) -> Self {
        self.scroll_input = Some(input);
        self
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
    fn tag(&self) -> tree::Tag {
        tree::Tag::of::<WorkspaceSurfaceState>()
    }

    fn state(&self) -> tree::State {
        tree::State::new(WorkspaceSurfaceState::default())
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
        if let Some((id, offset)) = &self.initial_scroll {
            let state = tree.state.downcast_mut::<WorkspaceSurfaceState>();
            if state.initialized_scroll.as_ref() != Some(id) {
                // Startup operations may run before the strip exists. Seed the
                // new native scroll state after layout, even when its width
                // already equals the restored model's width. Do this only once
                // per strip; later user scrolling remains native state.
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    Layout::new(&content),
                    renderer,
                    &mut cosmic::iced::advanced::widget::operation::scrollable::scroll_to::<()>(
                        id.clone(),
                        widget::scrollable::AbsoluteOffset {
                            x: Some(offset.max(0.0)),
                            y: None,
                        },
                    ),
                );
                state.initialized_scroll = Some(id.clone());
            }
        }
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
            ) if is_global_shortcut_chord(
                key,
                *physical_key,
                *modifiers,
                self.shortcuts,
                self.focused_pane,
            ) =>
            {
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
        if let Some(input) = &self.scroll_input {
            input.set(matches!(
                event,
                Event::Mouse(mouse::Event::WheelScrolled { .. })
            ));
        }
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
        if let Some(input) = &self.scroll_input {
            input.set(false);
        }
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
        let mut inner = cosmic::iced::advanced::clipboard::DndDestinationRectangles::new();
        self.content.as_widget().drag_destinations(
            &state.children[0],
            Self::child_layout(layout),
            renderer,
            &mut inner,
        );
        let mut rectangles = inner.into_rectangles();
        clip_drag_destinations(&mut rectangles, layout.bounds());
        destinations.append(&mut rectangles);
    }
}

/// Keeps only the part of each drop target that the strip shows.
///
/// The strip mounts cards past its edges and scrolls them without clipping
/// their drop targets, so a card scrolled half out of view would otherwise
/// reach under the sidebar and take a drop aimed there.
fn clip_drag_destinations(rectangles: &mut Vec<DndDestinationRectangle>, bounds: Rectangle) {
    let (left, top) = (f64::from(bounds.x), f64::from(bounds.y));
    let (right, bottom) = (
        left + f64::from(bounds.width),
        top + f64::from(bounds.height),
    );
    rectangles.retain_mut(|destination| {
        let rectangle = &mut destination.rectangle;
        let x = rectangle.x.max(left);
        let y = rectangle.y.max(top);
        let width = (rectangle.x + rectangle.width).min(right) - x;
        let height = (rectangle.y + rectangle.height).min(bottom) - y;
        *rectangle = cosmic::iced::clipboard::dnd::Rectangle {
            x,
            y,
            width,
            height,
        };
        width > 0.0 && height > 0.0
    });
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
    fn drop_targets_end_at_the_edges_of_the_strip() {
        use cosmic::iced::clipboard::dnd::{DndAction, Rectangle as DndRectangle};

        let destination = |id, x| DndDestinationRectangle {
            id,
            rectangle: DndRectangle {
                x,
                y: 0.0,
                width: 400.0,
                height: 600.0,
            },
            mime_types: Vec::new(),
            actions: DndAction::Copy,
            preferred: DndAction::Copy,
        };
        // A strip beside a 240-wide sidebar, under a 40-high header.
        let strip = Rectangle::new(Point::new(240.0, 40.0), Size::new(800.0, 500.0));
        let mut rectangles = vec![
            destination(1, 0.0),
            destination(2, 400.0),
            destination(3, 1040.0),
        ];
        clip_drag_destinations(&mut rectangles, strip);
        let kept: Vec<_> = rectangles
            .iter()
            .map(|destination| {
                let rectangle = &destination.rectangle;
                (
                    destination.id,
                    rectangle.x,
                    rectangle.y,
                    rectangle.width,
                    rectangle.height,
                )
            })
            .collect();
        assert_eq!(
            kept,
            // The card scrolled half out of view keeps only its visible part;
            // the one wholly past the edge takes no drops at all.
            vec![
                (1, 240.0, 40.0, 160.0, 500.0),
                (2, 400.0, 40.0, 400.0, 500.0)
            ]
        );
    }

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
            CardSurface::new(row).on_pointer_rest(Some("rest")).into();
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
        // Moving about inside the pane never crossed into it.
        assert!(messages.is_empty());
    }

    /// Drives one [`CardSurface`] spanning x 100..300 of the window, beneath
    /// the window's input trace, in the window's own coordinates unless a
    /// sample says otherwise.
    struct RestProbe {
        renderer: Renderer,
        surface: Element<'static, &'static str>,
        tree: Tree,
        node: layout::Node,
        messages: Vec<&'static str>,
        input: InputTrace,
        /// The cursor as the pane last saw it, which frames report too.
        cursor: mouse::Cursor,
    }

    impl RestProbe {
        const OUTSIDE: Point = Point::new(50.0, 40.0);
        const INSIDE: Point = Point::new(150.0, 40.0);
        /// Where the pointer is over the next pane along.
        const NEIGHBOUR: Point = Point::new(350.0, 40.0);

        fn new() -> Self {
            use cosmic::iced::advanced::renderer::Headless;
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
            let input = InputTrace::default();
            let mut surface = crate::input_trace::observe(
                CardSurface::new(
                    widget::Space::new()
                        .width(Length::Fill)
                        .height(Length::Fill),
                )
                .traced_by(&input)
                .on_pointer_focus("click")
                .on_pointer_rest(Some("rest")),
                &input,
            );
            let mut tree = Tree::new(&surface);
            let node = surface
                .as_widget_mut()
                .layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::new(200.0, 80.0), Size::new(200.0, 80.0)),
                )
                .move_to(Point::new(100.0, 0.0));
            Self {
                renderer,
                surface,
                tree,
                node,
                messages: Vec::new(),
                input,
                cursor: mouse::Cursor::Unavailable,
            }
        }

        fn send(&mut self, event: Event, cursor: mouse::Cursor) {
            use cosmic::iced::advanced::clipboard;
            self.surface.as_widget_mut().update(
                &mut self.tree,
                &event,
                Layout::new(&self.node),
                cursor,
                &self.renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut self.messages),
                &Rectangle::new(Point::new(100.0, 0.0), Size::new(200.0, 80.0)),
            );
        }

        /// Input the window delivered somewhere other than the pane: to an
        /// overlay that captured it, or to a widget that captured it before
        /// the strip, or while the pane was not mounted.
        fn elsewhere(&self, event: &Event) {
            self.input.record(event);
        }

        /// The pointer at `window` in the window, which the pane sees as
        /// `local` once the strip's scroll translation is applied.
        fn move_translated(&mut self, window: Point, local: mouse::Cursor) {
            self.cursor = local;
            self.send(
                Event::Mouse(mouse::Event::CursorMoved { position: window }),
                local,
            );
        }

        fn move_to(&mut self, position: Point) {
            self.move_translated(position, mouse::Cursor::Available(position));
        }

        /// Delivers the frame `after` now.
        fn frame(&mut self, after: Duration) {
            self.frame_with(after, self.cursor);
        }

        /// Delivers the frame `after` now, with the pane seeing the cursor at
        /// `cursor` by then.
        fn frame_with(&mut self, after: Duration, cursor: mouse::Cursor) {
            self.send(
                Event::Window(window::Event::RedrawRequested(Instant::now() + after)),
                cursor,
            );
        }

        /// Frames before and after the dwell elapses, and everything the pane
        /// published since the last look.
        fn rest(&mut self) -> Vec<&'static str> {
            self.frame(Duration::ZERO);
            self.frame(HOVER_ACTIVATION_DWELL * 2);
            self.frame(HOVER_ACTIVATION_DWELL * 3);
            mem::take(&mut self.messages)
        }
    }

    fn moved(position: Point) -> Event {
        Event::Mouse(mouse::Event::CursorMoved { position })
    }

    #[test]
    fn a_pane_activates_on_hover_only_after_the_pointer_crosses_in_and_rests() {
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.frame(Duration::ZERO);
        assert!(probe.messages.is_empty(), "activation waits for the dwell");
        // Moving about inside the pane keeps the dwell it started, and the
        // rest is reported once however many frames follow.
        probe.move_to(Point::new(180.0, 60.0));
        assert_eq!(probe.rest(), ["rest"]);

        // Sweeping through on the way somewhere else activates nothing.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.move_to(RestProbe::NEIGHBOUR);
        assert!(probe.rest().is_empty());
    }

    #[test]
    fn typing_or_clicking_before_the_dwell_keeps_focus_where_it_is() {
        let key = Event::Keyboard(key_press(
            "a",
            keyboard::key::Code::KeyA,
            keyboard::Modifiers::empty(),
            false,
        ));
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.send(key.clone(), mouse::Cursor::Available(RestProbe::INSIDE));
        // Further drift inside the pane does not re-arm what typing cancelled.
        probe.move_to(Point::new(160.0, 40.0));
        assert!(probe.rest().is_empty());

        // A shortcut the workspace captures, or a key a focused input
        // elsewhere takes, never reaches the pane, and cancels it all the same.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.elsewhere(&key);
        assert!(probe.rest().is_empty());

        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.send(
            Event::Mouse(mouse::Event::ButtonPressed(mouse::Button::Left)),
            mouse::Cursor::Available(RestProbe::INSIDE),
        );
        // The click activates the pane itself; the dwell adds nothing.
        assert_eq!(probe.rest(), ["click"]);
    }

    /// The pane was entered, but by the time the dwell ends it is no longer
    /// where the pointer rests.
    #[test]
    fn a_pane_that_left_the_pointer_during_the_dwell_is_not_activated() {
        // Keyboard navigation or a new pane moves the strip, and the pointer,
        // which never moved, now rests over a neighbour.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.frame_with(
            HOVER_ACTIVATION_DWELL * 2,
            mouse::Cursor::Available(RestProbe::NEIGHBOUR),
        );
        // Nor does it activate once scrolled back under the pointer.
        assert!(probe.rest().is_empty());

        // The pane scrolled out of view, where it receives no frames, and its
        // first one back comes long after the pointer stopped over it.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_to(RestProbe::INSIDE);
        probe.frame(HOVER_ACTIVATION_DWELL + HOVER_ACTIVATION_LATENESS * 2);
        assert!(probe.rest().is_empty());
    }

    /// A pane that comes out from under something the pointer was already
    /// over was not entered: the reader never moved toward it.
    #[test]
    fn uncovering_a_pane_under_a_still_pointer_is_not_an_entry() {
        // A dropdown drawn over the pane leaves it the motions beneath, but
        // with no cursor, and then closes under the pointer.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.move_translated(RestProbe::INSIDE, mouse::Cursor::Unavailable);
        assert!(probe.rest().is_empty(), "an overlay covers the pane");
        probe.move_to(Point::new(151.0, 40.0));
        assert!(probe.rest().is_empty());

        // A context menu opened from outside the pane captures every motion
        // over it, and then an item is chosen over the pane.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.elsewhere(&moved(RestProbe::INSIDE));
        probe.move_to(Point::new(151.0, 40.0));
        assert!(probe.rest().is_empty());

        // A popup surface outside the window closes, returning the pointer.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.send(
            Event::Mouse(mouse::Event::CursorLeft),
            mouse::Cursor::Unavailable,
        );
        probe.move_to(RestProbe::INSIDE);
        assert!(probe.rest().is_empty());

        // The strip scrolls the pane under a pointer that stays put.
        let mut probe = RestProbe::new();
        probe.move_translated(
            RestProbe::INSIDE,
            mouse::Cursor::Available(RestProbe::NEIGHBOUR),
        );
        probe.move_to(Point::new(151.0, 40.0));
        assert!(probe.rest().is_empty());

        // A pane mounted beneath the pointer has no sample to cross from.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::INSIDE);
        assert!(probe.rest().is_empty());

        // A pane remounted after a far jump last saw the pointer where it was
        // before; the pointer has since moved on beneath other panes and
        // stopped where the pane now comes to lie.
        let mut probe = RestProbe::new();
        probe.move_to(RestProbe::OUTSIDE);
        probe.elsewhere(&moved(Point::new(149.0, 40.0)));
        probe.elsewhere(&moved(RestProbe::INSIDE));
        probe.move_to(Point::new(151.0, 40.0));
        assert!(probe.rest().is_empty());
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
            user_input: true,
        });
        let resized = WorkspaceViewportEvent::Resized(800.0);

        assert_eq!(
            scrolled,
            WorkspaceViewportEvent::Scrolled(ViewportSample {
                offset: 240.0,
                width: 800.0,
                user_input: true,
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
        let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
        for (character, code, action) in [
            ("T", Code::KeyT, GlobalShortcut::NewTerminal),
            ("G", Code::KeyG, GlobalShortcut::NewChannel),
            ("D", Code::KeyD, GlobalShortcut::ClonePane),
            ("W", Code::KeyW, GlobalShortcut::CloseFocusedCard),
        ] {
            assert_eq!(
                global_shortcut(
                    &key_press(character, code, ctrl_shift, false),
                    &shortcuts,
                    FocusedPane::Terminal,
                ),
                Some(action)
            );
            assert_eq!(
                global_shortcut(
                    &key_press(character, code, ctrl_shift, true),
                    &shortcuts,
                    FocusedPane::Terminal,
                ),
                None,
                "holding a creation or close shortcut must not repeat it"
            );
            assert!(is_global_shortcut_chord(
                &keyboard::Key::Character(character.into()),
                keyboard::key::Physical::Code(code),
                ctrl_shift,
                &shortcuts,
                FocusedPane::Terminal,
            ));
        }
        assert_eq!(
            global_shortcut(
                &key_press(
                    "F",
                    Code::KeyF,
                    keyboard::Modifiers::CTRL | keyboard::Modifiers::CAPS_LOCK,
                    false,
                ),
                &shortcuts,
                FocusedPane::Other,
            ),
            Some(GlobalShortcut::FocusProjectSearch)
        );
        assert_eq!(
            global_shortcut(
                &key_press("f", Code::KeyF, keyboard::Modifiers::CTRL, true),
                &shortcuts,
                FocusedPane::Other,
            ),
            None,
            "held Ctrl shortcuts must not repeat"
        );
        for repeat in [false, true] {
            assert_eq!(
                global_shortcut(
                    &key_press("A", Code::KeyA, keyboard::Modifiers::ALT, repeat),
                    &shortcuts,
                    FocusedPane::Other,
                ),
                None,
                "Alt+A must be forwarded on initial and repeated key presses"
            );
        }

        // Column navigation repeats while held, and is reserved against the
        // process inside a card.
        for (named, code, direction) in [
            (
                keyboard::key::Named::PageUp,
                Code::PageUp,
                FocusDirection::Left,
            ),
            (
                keyboard::key::Named::PageDown,
                Code::PageDown,
                FocusDirection::Right,
            ),
        ] {
            for repeat in [false, true] {
                let event = named_key_press(named, code, keyboard::Modifiers::CTRL, repeat);
                assert_eq!(
                    global_shortcut(&event, &shortcuts, FocusedPane::Terminal),
                    Some(GlobalShortcut::FocusAdjacent(direction)),
                    "Ctrl+Page must navigate on initial and repeated presses"
                );
                assert!(is_global_shortcut_chord(
                    &keyboard::Key::Named(named),
                    keyboard::key::Physical::Code(code),
                    keyboard::Modifiers::CTRL,
                    &shortcuts,
                    FocusedPane::Terminal,
                ));
            }
            // The unmodified and shifted paging keys stay the terminal's.
            for modifiers in [keyboard::Modifiers::empty(), keyboard::Modifiers::SHIFT] {
                assert!(!is_global_shortcut_chord(
                    &keyboard::Key::Named(named),
                    keyboard::key::Physical::Code(code),
                    modifiers,
                    &shortcuts,
                    FocusedPane::Terminal,
                ));
            }
        }

        let mut customized = shortcuts;
        customized.new_terminal = vec!["Ctrl+Shift+N".to_owned()];
        customized.new_native_thread.clear();
        assert_eq!(
            global_shortcut(
                &key_press("n", Code::KeyN, ctrl_shift, false),
                &customized,
                FocusedPane::Other,
            ),
            Some(GlobalShortcut::NewTerminal)
        );
        customized.new_terminal = vec!["Ctrl+Shift+1".to_owned()];
        assert_eq!(
            global_shortcut(
                &key_press("!", Code::Digit1, ctrl_shift, false),
                &customized,
                FocusedPane::Other,
            ),
            Some(GlobalShortcut::NewTerminal)
        );
    }

    /// The keys a shell, an editor, or an agent CLI in a terminal relies on
    /// reach it: no default binds them, so a focused terminal receives every
    /// one. `Ctrl+W` deleting a word used to close the pane instead.
    #[test]
    fn default_shortcuts_leave_shell_keys_to_a_focused_terminal() {
        use keyboard::key::{Code, Named};

        let shortcuts = KeyboardShortcuts::default();
        let character = |character: &str, code, modifiers| {
            (
                keyboard::Key::Character(character.into()),
                keyboard::key::Physical::Code(code),
                modifiers,
            )
        };
        let named = |named, code, modifiers| {
            (
                keyboard::Key::Named(named),
                keyboard::key::Physical::Code(code),
                modifiers,
            )
        };
        let ctrl = keyboard::Modifiers::CTRL;
        let alt = keyboard::Modifiers::ALT;
        for (key, physical, modifiers) in [
            character("w", Code::KeyW, ctrl),
            character("t", Code::KeyT, ctrl),
            character("f", Code::KeyF, ctrl),
            character("r", Code::KeyR, ctrl),
            character("e", Code::KeyE, ctrl),
            character("g", Code::KeyG, ctrl),
            character("p", Code::KeyP, ctrl),
            character("q", Code::KeyQ, alt),
            character("w", Code::KeyW, alt),
            character("e", Code::KeyE, alt),
            character("r", Code::KeyR, alt),
            character("g", Code::KeyG, alt),
            character("f", Code::KeyF, alt),
            character("b", Code::KeyB, alt),
            character(".", Code::Period, alt),
            named(Named::ArrowLeft, Code::ArrowLeft, alt),
            named(Named::ArrowRight, Code::ArrowRight, alt),
            named(Named::ArrowLeft, Code::ArrowLeft, ctrl),
            named(Named::ArrowRight, Code::ArrowRight, ctrl),
            named(Named::ArrowUp, Code::ArrowUp, alt),
            named(Named::ArrowDown, Code::ArrowDown, alt),
        ] {
            assert!(
                !is_global_shortcut_chord(
                    &key,
                    physical,
                    modifiers,
                    &shortcuts,
                    FocusedPane::Terminal
                ),
                "{modifiers:?}+{key:?} was withheld from a focused terminal"
            );
            let press = keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key.clone(),
                physical_key: physical,
                location: keyboard::Location::Standard,
                modifiers,
                text: None,
                repeat: false,
            };
            assert_eq!(
                global_shortcut(&press, &shortcuts, FocusedPane::Terminal),
                None,
                "{modifiers:?}+{key:?} acted over a focused terminal"
            );
        }
    }

    /// The search chord belongs to whatever has focus: a focused terminal
    /// receives it as input, and any other pane takes it as its search. Chords
    /// of workspace scope act over a terminal as everywhere else.
    #[test]
    fn a_shell_key_bound_outside_terminals_reaches_a_focused_terminal() {
        use keyboard::key::Code;

        let shortcuts = KeyboardShortcuts::default();
        let search = key_press("f", Code::KeyF, keyboard::Modifiers::CTRL, false);
        let reserved = |focused| {
            is_global_shortcut_chord(
                &keyboard::Key::Character("f".into()),
                keyboard::key::Physical::Code(Code::KeyF),
                keyboard::Modifiers::CTRL,
                &shortcuts,
                focused,
            )
        };
        assert!(!reserved(FocusedPane::Terminal));
        assert_eq!(
            global_shortcut(&search, &shortcuts, FocusedPane::Terminal),
            None
        );
        for focused in [FocusedPane::Other, FocusedPane::Review] {
            assert!(reserved(focused));
            assert_eq!(
                global_shortcut(&search, &shortcuts, focused),
                Some(GlobalShortcut::FocusProjectSearch)
            );
        }

        // A user who binds a shell key to a workspace action asked for it.
        let customized = KeyboardShortcuts {
            close_focused_column: vec!["Ctrl+W".into()],
            ..KeyboardShortcuts::default()
        };
        assert_eq!(
            global_shortcut(
                &key_press("w", Code::KeyW, keyboard::Modifiers::CTRL, false),
                &customized,
                FocusedPane::Terminal,
            ),
            Some(GlobalShortcut::CloseFocusedCard)
        );
    }

    #[test]
    fn customized_creation_shortcuts_release_defaults_and_reserve_only_bound_chords() {
        use keyboard::key::Code;

        let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
        let mut shortcuts = KeyboardShortcuts {
            clone_pane: vec!["Alt+Shift+E".into()],
            new_channel: vec!["Ctrl+Shift+N".into()],
            new_native_thread: vec![],
            ..KeyboardShortcuts::default()
        };
        for (character, code, default_character, default_code, action) in [
            ("e", Code::KeyE, "d", Code::KeyD, GlobalShortcut::ClonePane),
            ("n", Code::KeyN, "g", Code::KeyG, GlobalShortcut::NewChannel),
        ] {
            let modifiers = if action == GlobalShortcut::ClonePane {
                keyboard::Modifiers::ALT | keyboard::Modifiers::SHIFT
            } else {
                ctrl_shift
            };
            assert_eq!(
                global_shortcut(
                    &key_press(character, code, modifiers, false),
                    &shortcuts,
                    FocusedPane::Other,
                ),
                Some(action)
            );
            assert_eq!(
                global_shortcut(
                    &key_press(character, code, modifiers, true),
                    &shortcuts,
                    FocusedPane::Other,
                ),
                None
            );
            assert!(is_global_shortcut_chord(
                &keyboard::Key::Character(character.into()),
                keyboard::key::Physical::Code(code),
                modifiers,
                &shortcuts,
                FocusedPane::Terminal,
            ));
            assert!(!is_global_shortcut_chord(
                &keyboard::Key::Character(default_character.into()),
                keyboard::key::Physical::Code(default_code),
                ctrl_shift,
                &shortcuts,
                FocusedPane::Terminal,
            ));
        }

        shortcuts.clone_pane.clear();
        shortcuts.new_channel.clear();
        for (character, code, modifiers) in [
            (
                "e",
                Code::KeyE,
                keyboard::Modifiers::ALT | keyboard::Modifiers::SHIFT,
            ),
            ("n", Code::KeyN, ctrl_shift),
        ] {
            assert!(!is_global_shortcut_chord(
                &keyboard::Key::Character(character.into()),
                keyboard::key::Physical::Code(code),
                modifiers,
                &shortcuts,
                FocusedPane::Terminal,
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
            FocusedPane::Other,
            true,
            &InputTrace::default(),
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
    fn native_scroll_samples_distinguish_redraw_feedback_from_user_wheels() {
        use cosmic::iced::advanced::{clipboard, renderer::Headless};
        use cosmic::iced::{Font, Pixels};
        use std::time::Instant;

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
        for index in 0..4 {
            workspace
                .add_card(
                    WorkspaceCard::new(CardId::new(), "Pane", CardWidth::Fixed(320.0)).unwrap(),
                    index,
                    index == 0,
                )
                .unwrap();
        }
        let shortcuts = KeyboardShortcuts::default();
        let mut element = view(
            &workspace,
            &shortcuts,
            FocusedPane::Other,
            false,
            &InputTrace::default(),
            400.0,
            widget::Id::unique(),
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
        );
        let mut tree = Tree::new(&element);
        let viewport = Rectangle::with_size(Size::new(400.0, 300.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(viewport.size(), viewport.size()),
        );
        let mut messages = Vec::new();
        for event in [
            Event::Window(window::Event::RedrawRequested(Instant::now())),
            Event::Mouse(mouse::Event::WheelScrolled {
                delta: mouse::ScrollDelta::Pixels { x: -120.0, y: 0.0 },
            }),
        ] {
            element.as_widget_mut().update(
                &mut tree,
                &event,
                Layout::new(&node),
                mouse::Cursor::Available(Point::new(100.0, 100.0)),
                &renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        let samples: Vec<_> = messages
            .into_iter()
            .filter_map(|event| match event {
                WorkspaceViewportEvent::Scrolled(sample) => Some(sample),
                _ => None,
            })
            .collect();
        assert!(samples.iter().any(|sample| !sample.user_input));
        assert!(
            samples
                .iter()
                .any(|sample| sample.user_input && sample.offset > 0.0)
        );
    }

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
                    FocusedPane::Other,
                    false,
                    &InputTrace::default(),
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
        let surface: Element<'_, ()> = Element::new(WorkspaceSurface::new(
            strip,
            &shortcuts,
            FocusedPane::Other,
            0.0,
            |_| (),
        ));
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
            FocusedPane::Other,
            true,
            &InputTrace::default(),
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
                FocusedPane::Other,
            )
        };
        let ctrl_shift = keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT;
        assert!(reserved("t".into(), Code::KeyT, ctrl_shift));
        assert!(!reserved("t".into(), Code::KeyT, keyboard::Modifiers::CTRL));
        assert!(!reserved("q".into(), Code::KeyQ, keyboard::Modifiers::CTRL));
        assert!(!reserved("q".into(), Code::KeyQ, keyboard::Modifiers::ALT));
        assert!(!reserved("a".into(), Code::KeyA, ctrl_shift));
        assert!(reserved("w".into(), Code::KeyW, ctrl_shift));
        assert!(!reserved("w".into(), Code::KeyW, keyboard::Modifiers::ALT));
        assert!(!reserved(
            "t".into(),
            Code::KeyT,
            keyboard::Modifiers::CTRL | keyboard::Modifiers::ALT,
        ));
        assert!(!reserved(
            "t".into(),
            Code::KeyT,
            ctrl_shift | keyboard::Modifiers::ALT,
        ));
    }

    /// The review chords are arrow chords a terminal program may well use,
    /// so they are withheld from a card's process only while a pull request
    /// pane, which is the only thing that acts on them, has focus.
    #[test]
    fn review_chords_are_reserved_only_while_a_review_pane_has_focus() {
        use keyboard::key::{Code, Physical};

        let shortcuts = KeyboardShortcuts::default();
        let reserved = |named, code, modifiers, focused| {
            is_global_shortcut_chord(
                &keyboard::Key::Named(named),
                Physical::Code(code),
                modifiers,
                &shortcuts,
                focused,
            )
        };
        for (named, code) in [
            (keyboard::key::Named::ArrowUp, Code::ArrowUp),
            (keyboard::key::Named::ArrowDown, Code::ArrowDown),
        ] {
            for focused in [FocusedPane::Terminal, FocusedPane::Other] {
                assert!(
                    !reserved(named, code, keyboard::Modifiers::ALT, focused),
                    "{named:?} reached no {focused:?} pane"
                );
            }
            assert!(
                reserved(named, code, keyboard::Modifiers::ALT, FocusedPane::Review),
                "{named:?} bypassed the review pane"
            );
        }
        // Workspace chords stay reserved either way.
        for focused in [
            FocusedPane::Terminal,
            FocusedPane::Review,
            FocusedPane::Other,
        ] {
            assert!(reserved(
                keyboard::key::Named::PageUp,
                Code::PageUp,
                keyboard::Modifiers::CTRL,
                focused,
            ));
        }
        let step = named_key_press(
            keyboard::key::Named::ArrowDown,
            Code::ArrowDown,
            keyboard::Modifiers::ALT,
            true,
        );
        assert_eq!(
            global_shortcut(&step, &shortcuts, FocusedPane::Review),
            Some(GlobalShortcut::ReviewNextFile),
            "stepping files repeats while held"
        );
        assert_eq!(
            global_shortcut(&step, &shortcuts, FocusedPane::Terminal),
            None,
            "a review chord acts on no other pane"
        );
    }

    #[test]
    fn held_page_navigation_crosses_four_cards_and_stops_at_edges() {
        use keyboard::key::{Code, Named};

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
                &named_key_press(
                    Named::PageDown,
                    Code::PageDown,
                    keyboard::Modifiers::CTRL,
                    repeat,
                ),
                &shortcuts,
                FocusedPane::Terminal,
            );
            let Some(GlobalShortcut::FocusAdjacent(direction)) = shortcut else {
                panic!("Ctrl+PageDown must remain a navigation shortcut on repeated key presses");
            };
            workspace.focus_adjacent(direction).unwrap();
        }
        assert_eq!(workspace.focused(), Some(cards[3]));

        for repeat in [false, true, true, true] {
            let shortcut = global_shortcut(
                &named_key_press(
                    Named::PageUp,
                    Code::PageUp,
                    keyboard::Modifiers::CTRL,
                    repeat,
                ),
                &shortcuts,
                FocusedPane::Terminal,
            );
            let Some(GlobalShortcut::FocusAdjacent(direction)) = shortcut else {
                panic!("Ctrl+PageUp must remain a navigation shortcut on repeated key presses");
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
    fn restored_camera_initializes_native_scroll_once_before_the_first_frame() {
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
            FocusedPane::Other,
            false,
            &InputTrace::default(),
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
        // No startup scroll task or resize message is delivered. The initial
        // width already matches the model, as it can after restoring a window.
        let visible = Rectangle::new(Point::ORIGIN, Size::new(VIEWPORT, HEIGHT));
        let mut restored_overlay = element
            .as_widget_mut()
            .overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &visible,
                Vector::ZERO,
            )
            .expect("a mounted card raised an overlay");
        let raised = restored_overlay
            .as_overlay_mut()
            .layout(&renderer, visible.size());
        let mut positions: Vec<f32> = raised
            .children()
            .iter()
            .map(|child| child.bounds().x)
            .collect();
        positions.sort_by(f32::total_cmp);
        assert_eq!(positions, expected);
        drop(raised);
        drop(restored_overlay);

        // A later native scroll must survive relayout with the same widget
        // state, even before its feedback has reached the workspace model.
        let moved_offset = offset - 100.0;
        element.as_widget_mut().operate(
            &mut tree,
            Layout::new(&node),
            &renderer,
            &mut operation::scrollable::scroll_to::<()>(
                scroll_id,
                operation::scrollable::AbsoluteOffset {
                    x: Some(moved_offset),
                    y: None,
                },
            ),
        );
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(VIEWPORT, HEIGHT)),
        );
        let mut raised = element
            .as_widget_mut()
            .overlay(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &visible,
                Vector::ZERO,
            )
            .expect("native-scrolled panes retain their overlays");
        let raised = raised.as_overlay_mut().layout(&renderer, visible.size());
        let mut positions: Vec<f32> = raised
            .children()
            .iter()
            .map(|child| child.bounds().x)
            .collect();
        positions.sort_by(f32::total_cmp);
        let mut moved = projection;
        moved.offset = moved_offset;
        assert_eq!(positions, expected_overlay_positions(&moved, VIEWPORT));
    }
}
