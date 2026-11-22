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
    workspace::{
        FocusDirection, FocusHandoff, FocusTarget, ViewportProjection, VisibilityClass, Workspace,
    },
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ViewportSample {
    pub offset: f32,
    pub width: f32,
    /// Pointer input moves the camera; native layout feedback only acknowledges it.
    pub user_input: bool,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum WorkspaceViewportEvent {
    Scrolled(ViewportSample),
    Resized(f32),
    /// A button was pressed in this pane; `primary` when it was the left one,
    /// which is the press that places a caret or picks a control.
    PointerFocused {
        card: CardId,
        primary: bool,
    },
    /// The pointer moved into this pane, and was still over it when the next
    /// frame was drawn.
    PointerEntered(CardId),
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
    ChatAccount,
    ChatModel,
    ChatPermissions,
    Dictate,
    ToggleInspector,
    OpenSettings,
    OpenWorktrees,
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
    FindInPane,
    FocusAdjacent(FocusDirection),
    CyclePaneWidth,
    /// Moves between the top-level workspaces: every project's worktrees and
    /// channels, most recently used first, then in the order the sidebar
    /// lists them.
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
    /// Anywhere in the workspace, a focused terminal included.
    Workspace,
    /// Anywhere but a focused terminal, which receives the chord as input.
    /// For an action whose chord means something to a shell and the same kind
    /// of thing in every other pane, such as `Ctrl+F`.
    OutsideTerminal,
    /// Only while a pull request pane has focus; a terminal or editor with
    /// focus receives the chord as ordinary input.
    Review,
    /// Only in a native chat pane; other panes retain the configured keys.
    Chat,
    /// Only in a pane with an inspector: not on an application page such as
    /// Settings, nor a browser outside a worktree's workspace, nor with no
    /// pane focused.
    Inspector,
}

impl ShortcutScope {
    /// Whether an action of this scope acts, and so withholds its chord from
    /// the card, while `focused` has focus.
    pub fn applies_to(self, focused: FocusedPane) -> bool {
        match self {
            Self::Workspace => true,
            Self::OutsideTerminal => focused != FocusedPane::Terminal,
            Self::Review => focused == FocusedPane::Review,
            Self::Chat => focused == FocusedPane::Chat,
            Self::Inspector => !matches!(focused, FocusedPane::NoInspector | FocusedPane::None),
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
    /// A native chat composer and transcript.
    Chat,
    /// A pane with no inspector here: an application page such as Settings,
    /// which belongs to no worktree, or a browser outside a worktree's
    /// workspace, which has no worktree to show.
    NoInspector,
    /// Any other pane.
    Other,
    /// No pane has focus.
    #[default]
    None,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShortcutDescriptor {
    pub action: GlobalShortcut,
    pub label: &'static str,
    pub repeatable: bool,
    pub scope: ShortcutScope,
}

pub const GLOBAL_SHORTCUT_DESCRIPTORS: &[ShortcutDescriptor] = &[
    ShortcutDescriptor {
        action: GlobalShortcut::ChatPermissions,
        label: "Choose chat permission mode",
        repeatable: false,
        scope: ShortcutScope::Chat,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ChatModel,
        label: "Choose chat model",
        repeatable: false,
        scope: ShortcutScope::Chat,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::ChatAccount,
        label: "Choose chat account",
        repeatable: false,
        scope: ShortcutScope::Chat,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::Dictate,
        label: "Dictate",
        repeatable: false,
        scope: ShortcutScope::Chat,
    },
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
        scope: ShortcutScope::Inspector,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::OpenSettings,
        label: "Open settings",
        repeatable: false,
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::OpenWorktrees,
        label: "Open worktrees",
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
        scope: ShortcutScope::Workspace,
    },
    ShortcutDescriptor {
        action: GlobalShortcut::FindInPane,
        label: "Find in pane",
        repeatable: false,
        scope: ShortcutScope::Workspace,
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
        GlobalShortcut::ChatPermissions => &shortcuts.chat_permissions,
        GlobalShortcut::ChatModel => &shortcuts.chat_model,
        GlobalShortcut::ChatAccount => &shortcuts.chat_account,
        GlobalShortcut::Dictate => &shortcuts.dictate,
        GlobalShortcut::ToggleInspector => &shortcuts.toggle_inspector,
        GlobalShortcut::OpenSettings => &shortcuts.open_settings,
        GlobalShortcut::OpenWorktrees => &shortcuts.open_worktrees,
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
        GlobalShortcut::FindInPane => &shortcuts.find_in_pane,
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

/// Convert the existing validated dispatch grammar to a native menu hint.
pub fn menu_keybind(raw: &str) -> Option<cosmic::widget::menu::KeyBind> {
    use cosmic::widget::menu::{KeyBind, key_bind::Modifier};
    use keyboard::{Key, key::Named};
    let binding = crate::settings::parse_shortcut(raw).ok()?;
    let modifiers = [
        (binding.control, Modifier::Ctrl),
        (binding.alt, Modifier::Alt),
        (binding.logo, Modifier::Super),
        (binding.shift, Modifier::Shift),
    ]
    .into_iter()
    .filter_map(|(held, modifier)| held.then_some(modifier))
    .collect();
    let key = match binding.key {
        ShortcutKey::Character(ch) => Key::Character(ch.to_string().into()),
        ShortcutKey::ArrowLeft => Key::Named(Named::ArrowLeft),
        ShortcutKey::ArrowRight => Key::Named(Named::ArrowRight),
        ShortcutKey::ArrowUp => Key::Named(Named::ArrowUp),
        ShortcutKey::ArrowDown => Key::Named(Named::ArrowDown),
        ShortcutKey::Tab => Key::Named(Named::Tab),
        ShortcutKey::Space => Key::Character(" ".into()),
        ShortcutKey::PageUp => Key::Named(Named::PageUp),
        ShortcutKey::PageDown => Key::Named(Named::PageDown),
        ShortcutKey::Function(number) => Key::Named(match number {
            1 => Named::F1,
            2 => Named::F2,
            3 => Named::F3,
            4 => Named::F4,
            5 => Named::F5,
            6 => Named::F6,
            7 => Named::F7,
            8 => Named::F8,
            9 => Named::F9,
            10 => Named::F10,
            11 => Named::F11,
            12 => Named::F12,
            _ => return None,
        }),
    };
    Some(KeyBind { modifiers, key })
}

/// The chords bound to `action`, in the one-line form the settings pane
/// reads back.
pub fn shortcut_chords(action: GlobalShortcut, shortcuts: &KeyboardShortcuts) -> String {
    format_chords(shortcut_keys(action, shortcuts))
}

pub fn shortcut_tooltip(action: GlobalShortcut, shortcuts: &KeyboardShortcuts) -> String {
    shortcut_tooltip_named(action, shortcuts, shortcut_descriptor(action).label)
}

pub fn shortcut_tooltip_named(
    action: GlobalShortcut,
    shortcuts: &KeyboardShortcuts,
    label: &str,
) -> String {
    if shortcut_keys(action, shortcuts).is_empty() {
        return format!("{label} — unbound");
    }
    format!("{label} — {}", shortcut_chords(action, shortcuts))
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

    /// Measures the next frame from `now`, the moment an animation began.
    ///
    /// A clock with no frame behind it spends its first one finding out when
    /// it is, so an animation started on an unset clock stood still for a
    /// frame before moving. Started here, that frame advances it by the time
    /// that has really passed since the input that began it.
    pub fn start(&mut self, now: Instant) {
        self.last = Some(now);
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

/// Complete a loading pane's focus handoff without taking focus from a
/// control the user chose while the content was arriving.
pub fn focus_after_loading<Message: Send + 'static>(
    placeholder: widget::Id,
    target: widget::Id,
) -> Task<Message> {
    use cosmic::iced::advanced::widget::operation::{self, Outcome};
    use cosmic::iced::core::id::IdEq;
    struct Restore {
        foreign_focus: bool,
        placeholder: widget::Id,
        target: widget::Id,
    }
    struct FocusLoaded(widget::Id);
    impl<Message: Send + 'static> Operation<Message> for FocusLoaded {
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            _bounds: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            if id.is_some_and(|id| IdEq::eq(&id.0, &self.0.0)) {
                state.focus();
            } else if state.is_focused() {
                // Unfocusing a fresh, already unfocused editor can publish
                // ClearSelection. A loading handoff must retain its cursor.
                state.unfocus();
            }
        }
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Message>)) {
            operate(self);
        }
    }
    impl<Message: Send + 'static> Operation<Message> for Restore {
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            _bounds: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            self.foreign_focus |=
                state.is_focused() && !id.is_some_and(|id| IdEq::eq(&id.0, &self.placeholder.0));
        }
        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Message>)) {
            operate(self);
        }
        fn finish(&self) -> Outcome<Message> {
            if self.foreign_focus {
                Outcome::None
            } else {
                Outcome::Chain(Box::new(FocusLoaded(self.target.clone())))
            }
        }
    }
    cosmic::iced::advanced::widget::operate(Restore {
        foreign_focus: false,
        placeholder,
        target,
    })
}

/// End-of-strip navigation returns the keyboard from chrome without taking
/// it from a find bar, composer, or other control already inside the pane.
pub fn focus_card_if_outside<Message: Send + 'static>(
    container: widget::Id,
    target: widget::Id,
) -> Task<Message> {
    use cosmic::iced::advanced::widget::operation::{self, Outcome, focusable};

    struct Restore {
        query: Box<dyn Operation<focusable::Count>>,
        target: widget::Id,
    }

    impl<Message: Send + 'static> Operation<Message> for Restore {
        fn container(&mut self, id: Option<&widget::Id>, bounds: Rectangle) {
            self.query.container(id, bounds);
        }

        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            self.query.focusable(id, bounds, state);
        }

        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Message>)) {
            self.query.traverse(&mut |query| {
                operate(&mut operation::black_box(query));
            });
        }

        fn finish(&self) -> Outcome<Message> {
            match self.query.finish() {
                Outcome::Some(count) if count.total > 0 && count.focused.is_none() => {
                    Outcome::Chain(Box::new(focusable::focus(self.target.clone())))
                }
                // An unmounted pane has no controls: do not blur the window
                // by targeting an input that has not been rendered yet.
                _ => Outcome::None,
            }
        }
    }

    cosmic::iced::advanced::widget::operate(Restore {
        query: Box::new(operation::scoped(container, focusable::count())),
        target,
    })
}

/// Hover activation hands the keyboard to the pane under the pointer, but
/// only from somewhere inside `workspace` or from nowhere: a field outside
/// the strip, such as sidebar search, keeps what is being typed into it.
/// Nothing happens while `target` is not in the widget tree, because a focus
/// operation that finds no target blurs every widget instead.
pub fn focus_unless_held_outside<Message: Send + 'static>(
    workspace: widget::Id,
    target: widget::Id,
) -> Task<Message> {
    use cosmic::iced::advanced::widget::operation::{self, Outcome, focusable};
    use cosmic::iced::core::id::IdEq;

    struct Probe {
        workspace: widget::Id,
        target: widget::Id,
        current: Option<widget::Id>,
        inside: bool,
        held_outside: bool,
        target_mounted: bool,
    }

    impl<Message: Send + 'static> Operation<Message> for Probe {
        fn container(&mut self, id: Option<&widget::Id>, _bounds: Rectangle) {
            self.current = id.cloned();
        }

        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            _bounds: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            self.target_mounted |= id.is_some_and(|id| IdEq::eq(&id.0, &self.target.0));
            self.held_outside |= !self.inside && state.is_focused();
        }

        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Message>)) {
            let entering = !self.inside
                && self
                    .current
                    .as_ref()
                    .is_some_and(|id| IdEq::eq(&id.0, &self.workspace.0));
            self.current = None;
            self.inside |= entering;
            operate(self);
            if entering {
                self.inside = false;
            }
        }

        fn finish(&self) -> Outcome<Message> {
            if self.target_mounted && !self.held_outside {
                Outcome::Chain(Box::new(focusable::focus(self.target.clone())))
            } else {
                Outcome::None
            }
        }
    }

    cosmic::iced::advanced::widget::operate(Probe {
        workspace,
        target,
        current: None,
        inside: false,
        held_outside: false,
        target_mounted: false,
    })
}

/// Returns the keyboard to `target` when no widget in the window holds it,
/// as after the window is activated with nothing focused. A control that
/// kept focus while the window was away keeps it.
pub fn focus_if_unheld<Message: Send + 'static>(target: widget::Id) -> Task<Message> {
    use cosmic::iced::advanced::widget::operation::{self, Outcome, focusable};
    use cosmic::iced::core::id::IdEq;

    struct Probe {
        target: widget::Id,
        held: bool,
        target_mounted: bool,
    }

    impl<Message: Send + 'static> Operation<Message> for Probe {
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            _bounds: Rectangle,
            state: &mut dyn operation::Focusable,
        ) {
            self.target_mounted |= id.is_some_and(|id| IdEq::eq(&id.0, &self.target.0));
            self.held |= state.is_focused();
        }

        fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation<Message>)) {
            operate(self);
        }

        fn finish(&self) -> Outcome<Message> {
            if self.target_mounted && !self.held {
                Outcome::Chain(Box::new(focusable::focus(self.target.clone())))
            } else {
                Outcome::None
            }
        }
    }

    cosmic::iced::advanced::widget::operate(Probe {
        target,
        held: false,
        target_mounted: false,
    })
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
/// the workspace left it to the card.
pub fn global_shortcut(
    event: &keyboard::Event,
    shortcuts: &KeyboardShortcuts,
    focused: FocusedPane,
) -> Option<GlobalShortcut> {
    let keyboard::Event::KeyPressed { repeat, .. } = event else {
        return None;
    };
    let descriptor = global_shortcut_event_descriptor(event, shortcuts)?;
    (descriptor.scope.applies_to(focused) && (!*repeat || descriptor.repeatable))
        .then_some(descriptor.action)
}

fn global_shortcut_event_descriptor(
    event: &keyboard::Event,
    shortcuts: &KeyboardShortcuts,
) -> Option<&'static ShortcutDescriptor> {
    let keyboard::Event::KeyPressed {
        key,
        modified_key,
        physical_key,
        modifiers,
        ..
    } = event
    else {
        return None;
    };
    // Preserve the existing action precedence across the whole registry before
    // considering the symbol produced by Shift (for example 1 versus !).
    global_shortcut_descriptor(key, *physical_key, *modifiers, shortcuts).or_else(|| {
        (modified_key != key)
            .then(|| global_shortcut_descriptor(modified_key, *physical_key, *modifiers, shortcuts))
            .flatten()
    })
}

fn is_global_shortcut_press(
    event: &keyboard::Event,
    shortcuts: &KeyboardShortcuts,
    focused: FocusedPane,
) -> bool {
    let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
        return false;
    };
    crate::tab_history::is_chord(key, *modifiers)
        || global_shortcut_event_descriptor(event, shortcuts)
            .is_some_and(|descriptor| descriptor.scope.applies_to(focused))
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
            // The space bar arrives as the character it types.
            ShortcutKey::Space => key.as_ref() == keyboard::Key::Character(" "),
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

/// A character chord that this physical event can actually dispatch. Number
/// bindings use the number row even when Shift changes the logical character.
pub(crate) fn recordable_shortcut_character(
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
) -> Option<char> {
    physical_digit(physical_key).or_else(|| {
        // A logical numpad digit cannot trigger a number-row binding.
        latin_key(key, physical_key).filter(|character| !character.is_ascii_digit())
    })
}

/// Whether a chord is undo or redo (Ctrl+Z, Ctrl+Shift+Z, Ctrl+Y) pressed in
/// a chat pane, whose message editor has an undo history of its own.
///
/// The pane's content sees such a chord before the workspace does: the
/// focused editor takes it, and the workspace's action bound to the same
/// chord, such as reopening a closed pane on Ctrl+Shift+Z, runs only when
/// nothing there took it ([`deferred_shortcut_taken`]).
pub(crate) fn defers_to_focused_editor(
    key: &keyboard::Key,
    physical_key: keyboard::key::Physical,
    modifiers: keyboard::Modifiers,
    focused: FocusedPane,
) -> bool {
    focused == FocusedPane::Chat
        && modifiers.command()
        && !modifiers.alt()
        && !modifiers.logo()
        && match latin_key(key, physical_key) {
            Some('z') => true,
            Some('y') => !modifiers.shift(),
            _ => false,
        }
}

/// Whether the pane's content took a chord the workspace left to it first
/// ([`defers_to_focused_editor`]), so the workspace's own action for that
/// chord does not also run.
pub(crate) fn deferred_shortcut_taken(
    event: &keyboard::Event,
    captured: bool,
    focused: FocusedPane,
) -> bool {
    match event {
        keyboard::Event::KeyPressed {
            key,
            physical_key,
            modifiers,
            ..
        } => captured && defers_to_focused_editor(key, *physical_key, *modifiers, focused),
        _ => false,
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
    if shortcut_event_matches_unmodified(binding, event) {
        return true;
    }
    let keyboard::Event::KeyPressed {
        key,
        modified_key,
        physical_key,
        modifiers,
        repeat: false,
        ..
    } = event
    else {
        return false;
    };
    modified_key != key && shortcut_matches(binding, modified_key, *physical_key, *modifiers)
}

/// Matches the unmodified/native physical form of a pressed chord. Dispatchers
/// with several actions should prefer these matches across their action list
/// before trying [`shortcut_event_matches`]'s modified-symbol fallback.
#[must_use]
pub fn shortcut_event_matches_unmodified(
    binding: ShortcutBinding,
    event: &keyboard::Event,
) -> bool {
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
            workspace.focused(),
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
                    let on_screen = mounted.visibility == VisibilityClass::Visible;
                    Some(VirtualCard {
                        id: card.id,
                        x: mounted.start,
                        width: mounted.width,
                        content_width: mounted.content_width,
                        on_screen,
                        content: CardSurface::new(content)
                            .traced_by(input)
                            .on_screen(on_screen)
                            .on_pointer_focus(
                                on_viewport(WorkspaceViewportEvent::PointerFocused {
                                    card: card.id,
                                    primary: true,
                                }),
                                on_viewport(WorkspaceViewportEvent::PointerFocused {
                                    card: card.id,
                                    primary: false,
                                }),
                            )
                            // The active pane has nothing to gain from an entry.
                            .on_pointer_enter((focus_follows_mouse && !focused).then(|| {
                                on_viewport(WorkspaceViewportEvent::PointerEntered(card.id))
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
    /// Published for a primary press and for any other press.
    on_pointer_focus: Option<(Message, Message)>,
    on_pointer_enter: Option<Message>,
    input: InputTrace,
    on_screen: bool,
}

impl<'a, Message> CardSurface<'a, Message> {
    fn new(content: impl Into<Element<'a, Message>>) -> Self {
        Self {
            content: content.into(),
            on_pointer_focus: None,
            on_pointer_enter: None,
            input: InputTrace::default(),
            on_screen: false,
        }
    }

    /// Whether the workspace classes this pane as visible, which a pane the
    /// camera is still travelling to is before any of it is in the viewport.
    /// Such a pane takes everything but the pointer; see `update`.
    fn on_screen(mut self, on_screen: bool) -> Self {
        self.on_screen = on_screen;
        self
    }

    /// The window's record of its input, which hover activation reads for
    /// where each motion began and for presses the pane never received.
    /// Without it the pane never counts the pointer as having crossed in.
    fn traced_by(mut self, input: &InputTrace) -> Self {
        self.input = input.clone();
        self
    }

    fn on_pointer_focus(mut self, primary: Message, other: Message) -> Self {
        self.on_pointer_focus = Some((primary, other));
        self
    }

    /// Published once the pointer has moved into this pane;
    /// see [`CardPointerState`] for what counts.
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
/// Hover activation follows the pointer at once, as a window manager's focus
/// follows the mouse. It is reported at the next frame rather than during
/// the motion, because only once every widget has seen the motion is it
/// known whether the pointer stopped on a resize handle at the pane's edge.
/// The pointer must still be over the pane at that frame, which a pane the
/// strip moved away meanwhile is not, and must not be on such a handle, nor
/// be composing text with an input method. A key or button pressed in
/// between keeps focus where it was. Leaving a handle into the pane counts
/// as an entry of its own.
#[derive(Default)]
struct CardPointerState {
    /// When the pointer crossed into the pane, until the next frame reports
    /// it.
    entered_at: Option<Instant>,
    selecting: bool,
}

/// How late after the crossing an entry may still be reported. A pane
/// scrolled out of view receives no frames, and its first frame back may come
/// long after the pointer passed over it.
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
                // Coming off a resize handle counts as arriving: while the
                // pointer rested on the divider it was choosing to resize,
                // not this pane.
                let crossed_in = hovered
                    && (crate::input_trace::was_on_resize_handle()
                        || self
                            .input
                            .previous_position()
                            .zip(cursor_point(cursor))
                            .zip(clip)
                            .is_some_and(|((previous, local), clip)| {
                                !clip.contains(previous + (local - *position))
                            }));
                if !hovered {
                    pointer.entered_at = None;
                } else if crossed_in && self.on_pointer_enter.is_some() {
                    let now = Instant::now();
                    pointer.entered_at = Some(now);
                    shell.request_redraw();
                }
            }
            Event::Window(window::Event::RedrawRequested(now)) => {
                if let Some(entered) = pointer.entered_at.take() {
                    let entered_now = now.saturating_duration_since(entered)
                        <= HOVER_ACTIVATION_LATENESS
                        && !self.input.pressed_since(entered)
                        && !self.input.composing()
                        && !crate::input_trace::on_resize_handle()
                        && clipped_viewport(layout.bounds(), viewport)
                            .is_some_and(|clip| cursor.is_over(clip));
                    // Gone when the pane became active in the meantime.
                    if entered_now && let Some(message) = &self.on_pointer_enter {
                        shell.publish(message.clone());
                    }
                }
            }
            _ => {}
        }
        let clipped = match clipped_viewport(layout.bounds(), viewport) {
            Some(clipped) => clipped,
            // None of this pane can be pointed at, but one the camera is
            // travelling to already holds the keyboard: what is typed during
            // the journey is meant for it.
            None if self.on_screen && !matches!(event, Event::Mouse(_) | Event::Touch(_)) => {
                Rectangle::new(layout.bounds().position(), Size::ZERO)
            }
            None => return,
        };
        // Observe before the child can capture the press, but do not capture it
        // ourselves: text selection, caret placement and controls still get it.
        if !shell.is_event_captured()
            && let Event::Mouse(mouse::Event::ButtonPressed(button)) = event
            && cursor.is_over(clipped)
            && let Some((primary, other)) = &self.on_pointer_focus
        {
            shell.publish(if *button == mouse::Button::Left {
                primary.clone()
            } else {
                other.clone()
            });
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
    content_width: f32,
    /// Whether the workspace classes the card as visible. A pane the camera
    /// is still travelling to is, without yet being inside the viewport.
    on_screen: bool,
    content: Element<'a, Message>,
}

impl<Message> VirtualCard<'_, Message> {
    fn a11y_nodes(
        &self,
        state: &Tree,
        layout: Layout<'_>,
        offset: Vector,
        cursor: mouse::Cursor,
        visible: &Rectangle,
    ) -> iced_accessibility::A11yTree {
        // `layout` carries no virtual offset, so every descendant reports its
        // bounds in strip coordinates; they are clipped to the columns the
        // camera shows and moved by the camera's `offset` once, here.
        // Retained, overscan and travelling panes stay mounted, but expose no
        // body before they enter view. Only the camera's axis is clipped:
        // what a pane scrolls out of view vertically is its own scrollables'
        // business, and stays reachable as it was.
        let bounds = layout.bounds();
        let left = bounds.x.max(visible.x);
        let right = (bounds.x + self.width).min(visible.x + visible.width);
        if right <= left {
            return iced_accessibility::A11yTree::default();
        }
        // A settled pane's body lies inside its slot, so the camera's columns
        // hide what they always did. Its popups draw over the window, not the
        // pane, and a menu reaching across a neighbour stays reachable where
        // it is painted. Only an opening pane, laid out wider than its slot,
        // keeps to the slot: past it lies its neighbour's body.
        let clip = if self.content_width > self.width {
            crate::ui_controls::accessible_bounds::columns(left, right)
        } else {
            crate::ui_controls::accessible_bounds::columns(visible.x, visible.x + visible.width)
        };
        let mut accessible = self.content.as_widget().a11y_nodes(layout, state, cursor);
        crate::ui_controls::accessible_bounds::place(&mut accessible, offset, Some(clip));
        accessible
    }

    /// The part of `viewport` this card may draw in and take pointer input
    /// from. An opening card is laid out wider than its slot, and what lies
    /// past the slot's edge belongs to its neighbour.
    fn viewport(&self, bounds: Rectangle, viewport: &Rectangle) -> Rectangle {
        if self.content_width <= self.width {
            return *viewport;
        }
        let slot = Rectangle {
            width: self.width,
            ..bounds
        };
        slot.intersection(viewport)
            .unwrap_or(Rectangle::new(slot.position(), Size::ZERO))
    }

    /// The pointer as this card may see it. Widgets test the pointer against
    /// their own bounds, which for an opening card reach under its neighbour.
    fn cursor(&self, viewport: &Rectangle, cursor: mouse::Cursor) -> mouse::Cursor {
        if self.content_width <= self.width || cursor.is_over(*viewport) {
            cursor
        } else {
            mouse::Cursor::Unavailable
        }
    }
}

struct VirtualStrip<'a, Message> {
    total_width: f32,
    viewport_width: f32,
    retained: Arc<HashMap<CardId, usize>>,
    focused: Option<CardId>,
    cards: Vec<VirtualCard<'a, Message>>,
}

impl<'a, Message> VirtualStrip<'a, Message> {
    fn new(
        projection: &ViewportProjection,
        retained: Arc<HashMap<CardId, usize>>,
        focused: Option<CardId>,
        cards: Vec<VirtualCard<'a, Message>>,
    ) -> Self {
        debug_assert_eq!(
            cards.iter().map(|card| card.id).collect::<Vec<_>>(),
            projection.mounted_ids().collect::<Vec<_>>()
        );
        Self {
            total_width: projection.total_width,
            viewport_width: projection.viewport_width,
            retained,
            focused,
            cards,
        }
    }

    /// The part of the strip the camera shows, in strip coordinates.
    fn a11y_visible(&self, layout: Layout<'_>) -> Rectangle {
        let bounds = layout.bounds();
        Rectangle {
            width: self.viewport_width.min(bounds.width).max(0.0),
            ..bounds
        } + layout.virtual_offset()
    }

    /// Whether `target` is a shown node of some card's accessibility tree.
    fn a11y_target_visible(
        &self,
        tree: &Tree,
        layout: Layout<'_>,
        cursor: mouse::Cursor,
        target: &widget::Id,
    ) -> bool {
        let visible = self.a11y_visible(layout);
        let target = iced_accessibility::A11yId::from(target.clone());
        self.cards
            .iter()
            .zip(&tree.children)
            .zip(layout.children())
            .any(|((card, state), child_layout)| {
                let accessible = card.a11y_nodes(
                    state,
                    child_layout.with_virtual_offset(Vector::ZERO),
                    layout.virtual_offset(),
                    // The published tree is built with the real pointer;
                    // hover-only nodes must resolve the same way here.
                    cursor,
                    &visible,
                );
                accessible
                    .root()
                    .iter()
                    .chain(accessible.children())
                    .any(|node| {
                        iced_accessibility::IdEq::eq(node.id(), &target) && !node.node().is_hidden()
                    })
            })
    }
}

#[derive(Default)]
struct VirtualStripState {
    mounted: Vec<CardId>,
    detached: HashMap<CardId, Tree>,
    unfocus_on_layout: std::collections::HashSet<CardId>,
    retained: Arc<HashMap<CardId, usize>>,
}

// Iced extracts named states before diffing. A departing virtual card must
// reclaim those states before being parked outside the traversed child tree;
// otherwise the next frame drops them and remounting downcasts State::None.
fn reclaim_named_states(tree: &mut Tree) {
    if let Some(widget::Id(iced_accessibility::id::Internal::Custom(_, name))) = &tree.id
        && let Some((state, children)) = tree::NAMED.with(|named| named.borrow_mut().remove(name))
    {
        tree.state = state;
        for (index, child) in children {
            if let Some(slot) = tree.children.get_mut(index) {
                *slot = child;
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
            unfocus_on_layout: Default::default(),
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
            let previous_band = mem::take(&mut state.mounted);
            for (id, mut child) in previous_band.iter().copied().zip(children.drain(..)) {
                reclaim_named_states(&mut child);
                if !self.cards.iter().any(|card| card.id == id) {
                    crate::ui_memory::release_parked(&mut child);
                }
                state.detached.insert(id, child);
            }
            for card in &mut self.cards {
                let parked = state.detached.remove(&card.id);
                if parked.is_some()
                    && !previous_band.contains(&card.id)
                    && self.focused != Some(card.id)
                {
                    // A focus operation cannot visit a parked tree. Returning
                    // an inactive pane must not revive its old input focus.
                    state.unfocus_on_layout.insert(card.id);
                }
                let mut child = parked.unwrap_or_else(|| Tree::new(&card.content));
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
            state
                .unfocus_on_layout
                .retain(|id| self.retained.contains_key(id));
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
        let pending = &mut tree
            .state
            .downcast_mut::<VirtualStripState>()
            .unfocus_on_layout;
        let children = self
            .cards
            .iter_mut()
            .zip(&mut tree.children)
            .map(|(card, state)| {
                let child_size = Size::new(card.content_width, size.height);
                let child_limits = layout::Limits::new(child_size, child_size);
                let node = card
                    .content
                    .as_widget_mut()
                    .layout(state, renderer, &child_limits)
                    .move_to(Point::new(card.x, 0.0));
                if pending.remove(&card.id) {
                    card.content.as_widget_mut().operate(
                        state,
                        Layout::new(&node),
                        renderer,
                        &mut cosmic::iced::advanced::widget::operation::focusable::unfocus::<()>(),
                    );
                }
                node
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
        // An accessibility action whose target is hidden or gone is stale:
        // the user can no longer perceive what it names. Otherwise the event
        // goes where it always went, so a pane holding a modal popover still
        // sees, and captures, actions aimed at its neighbours.
        if let Event::A11y(target, _) = event
            && !self.a11y_target_visible(tree, layout, cursor, target)
        {
            return;
        }
        for ((card, state), child_layout) in self
            .cards
            .iter_mut()
            .zip(&mut tree.children)
            .zip(layout.children())
        {
            let viewport = &card.viewport(child_layout.bounds(), viewport);
            let cursor = card.cursor(viewport, cursor);
            // A pane the camera is travelling to already holds the keyboard
            // and is about to be looked at, so it is shown events as though it
            // had arrived: keys typed during the journey reach it, and it
            // sizes itself before it is seen. Panes merely parked in the
            // overscan still get pointer events alone.
            let normally_delivered = card.on_screen
                || child_layout.bounds().intersects(viewport)
                || matches!(
                    event,
                    Event::Mouse(
                        mouse::Event::CursorMoved { .. }
                            | mouse::Event::CursorLeft
                            | mouse::Event::ButtonPressed(_)
                            | mouse::Event::ButtonReleased(_)
                    )
                );
            if normally_delivered {
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
            .filter_map(|((card, state), child_layout)| {
                let viewport = card.viewport(child_layout.bounds(), viewport);
                child_layout
                    .bounds()
                    .intersection(&viewport)
                    .is_some_and(|visible| cursor.is_over(visible))
                    .then(|| {
                        card.content.as_widget().mouse_interaction(
                            state,
                            child_layout.with_virtual_offset(layout.virtual_offset()),
                            cursor,
                            &viewport,
                            renderer,
                        )
                    })
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
        for ((card, state), child_layout) in
            self.cards.iter().zip(&tree.children).zip(layout.children())
        {
            let viewport = card.viewport(child_layout.bounds(), &clipped_viewport);
            if !child_layout.bounds().intersects(&viewport) {
                continue;
            }
            card.content.as_widget().draw(
                state,
                renderer,
                theme,
                style,
                child_layout.with_virtual_offset(layout.virtual_offset()),
                cursor,
                &viewport,
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
        let visible = self.a11y_visible(layout);
        iced_accessibility::A11yTree::join(
            self.cards
                .iter()
                .zip(&state.children)
                .zip(layout.children())
                .map(|((card, child_state), child_layout)| {
                    card.a11y_nodes(
                        child_state,
                        child_layout.with_virtual_offset(Vector::ZERO),
                        layout.virtual_offset(),
                        cursor,
                        &visible,
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
                press @ keyboard::Event::KeyPressed {
                    key,
                    physical_key,
                    modifiers,
                    ..
                },
            ) if is_global_shortcut_press(press, self.shortcuts, self.focused_pane)
                && !defers_to_focused_editor(key, *physical_key, *modifiers, self.focused_pane) =>
            {
                shell.capture_event();
                return;
            }
            Event::Keyboard(keyboard::Event::KeyReleased {
                key,
                physical_key,
                modifiers,
                ..
            }) if is_global_shortcut_chord(
                key,
                *physical_key,
                *modifiers,
                self.shortcuts,
                self.focused_pane,
            ) && !defers_to_focused_editor(
                key,
                *physical_key,
                *modifiers,
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
                Event::Mouse(mouse::Event::WheelScrolled { .. }) | Event::Touch(_)
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
pub(crate) fn clip_drag_destinations(
    rectangles: &mut Vec<DndDestinationRectangle>,
    bounds: Rectangle,
) {
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
mod tests;

#[cfg(test)]
#[path = "widget_forwarding_test.rs"]
pub(crate) mod forwarding_test;
