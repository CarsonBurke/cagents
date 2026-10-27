use super::*;
use crate::sidebar::ProjectCommand;
use workspace_widget::GlobalShortcut as Command;

mod row;

// Tab is scoped to the palette and completes synchronously, so a later
// Enter in one native batch sees the newly focused control.
mod native_tab {
    use super::{Element, Message};
    use cosmic::{Renderer, Theme, iced};
    use iced::advanced::widget::{Operation, Tree, tree};
    use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
    use iced::{Event, Length, Rectangle, Size, Vector};
    struct TabRing<'a> {
        content: Element<'a, Message>,
        enabled: bool,
        barrier: crate::terminal::ReviewInputBarrier,
        rows: Vec<iced::advanced::widget::Id>,
    }
    pub(super) fn view<'a>(
        content: Element<'a, Message>,
        enabled: bool,
        barrier: crate::terminal::ReviewInputBarrier,
        rows: Vec<iced::advanced::widget::Id>,
    ) -> Element<'a, Message> {
        Element::new(TabRing {
            content,
            enabled,
            barrier,
            rows,
        })
    }

    // Button operations expose unscrolled content-layout bounds. Probe the
    // actual focus first, then let only the results scrollable combine those
    // bounds with its live content origin, viewport and current translation.
    struct FocusedRow<'a> {
        rows: &'a [iced::advanced::widget::Id],
        bounds: Option<Rectangle>,
    }
    impl Operation for FocusedRow<'_> {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&iced::advanced::widget::Id>,
            bounds: Rectangle,
            state: &mut dyn iced::advanced::widget::operation::Focusable,
        ) {
            if state.is_focused() && id.is_some_and(|id| self.rows.contains(id)) {
                self.bounds = Some(bounds);
            }
        }
    }
    struct RevealRow(Rectangle);
    impl Operation for RevealRow {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
            visit(self);
        }
        fn scrollable(
            &mut self,
            id: Option<&iced::advanced::widget::Id>,
            bounds: Rectangle,
            content: Rectangle,
            translation: Vector,
            state: &mut dyn iced::advanced::widget::operation::Scrollable,
        ) {
            if id != Some(&iced::advanced::widget::Id::new("palette.commands")) {
                return;
            }
            let top = self.0.y - content.y - super::ROW_FOCUS_OUTSET;
            let bottom = self.0.y - content.y + self.0.height + super::ROW_FOCUS_OUTSET;
            let offset = if top < translation.y {
                top
            } else if bottom > translation.y + bounds.height {
                bottom - bounds.height
            } else {
                translation.y
            };
            state.scroll_to(
                iced::advanced::widget::operation::scrollable::AbsoluteOffset {
                    x: None,
                    y: Some(offset.clamp(0.0, (content.height - bounds.height).max(0.0))),
                },
            );
        }
    }
    impl TabRing<'_> {
        fn reveal_focused_row(&mut self, tree: &mut Tree, layout: Layout<'_>, renderer: &Renderer) {
            let mut focused = FocusedRow {
                rows: &self.rows,
                bounds: None,
            };
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
                renderer,
                &mut focused,
            );
            if let Some(bounds) = focused.bounds {
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
                    renderer,
                    &mut RevealRow(bounds),
                );
            }
        }
    }
    impl Widget<Message, Theme, Renderer> for TabRing<'_> {
        fn tag(&self) -> tree::Tag {
            tree::Tag::of::<TabRing<'static>>()
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
            self.content.as_widget_mut().operate(
                &mut tree.children[0],
                layout,
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
            if self.enabled
                && !shell.is_event_captured()
                && let Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key: iced::keyboard::Key::Named(iced::keyboard::key::Named::Tab),
                    modifiers,
                    ..
                }) = event
                && !modifiers.control()
                && !modifiers.alt()
                && !modifiers.logo()
            {
                if !cfg!(test)
                    && iced::runtime::core::keyboard::input::current_source()
                        .cycle()
                        .is_none()
                {
                    shell.capture_event();
                    return;
                }
                if let Event::Keyboard(key) = event
                    && !self.barrier.capture_consumed_key(key)
                {
                    shell.capture_event();
                    return;
                }
                // Complete native traversal before the next event in this batch.
                // Capture prevents COSMIC's asynchronous global Tab subscription.
                let mut movement: Box<dyn Operation> = if modifiers.shift() {
                    Box::new(iced::advanced::widget::operation::focusable::focus_previous::<()>())
                } else {
                    Box::new(iced::advanced::widget::operation::focusable::focus_next::<()>())
                };
                loop {
                    self.content.as_widget_mut().operate(
                        &mut tree.children[0],
                        layout,
                        renderer,
                        movement.as_mut(),
                    );
                    match movement.finish() {
                        iced::advanced::widget::operation::Outcome::Chain(next) => movement = next,
                        _ => break,
                    }
                }
                self.reveal_focused_row(tree, layout, renderer);
                shell.capture_event();
                shell.request_redraw();
                return;
            }
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
            if self.enabled
                && shell.is_event_captured()
                && matches!(event, Event::Keyboard(iced::keyboard::Event::KeyPressed {
                    key: iced::keyboard::Key::Named(iced::keyboard::key::Named::ArrowUp | iced::keyboard::key::Named::ArrowDown | iced::keyboard::key::Named::Home | iced::keyboard::key::Named::End), modifiers, ..
                }) if modifiers.is_empty())
            {
                self.reveal_focused_row(tree, layout, renderer);
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
            destinations: &mut iced::advanced::clipboard::DndDestinationRectangles,
        ) {
            self.content.as_widget().drag_destinations(
                &tree.children[0],
                layout,
                renderer,
                destinations,
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
            self.content.as_widget_mut().overlay(
                &mut tree.children[0],
                layout,
                renderer,
                viewport,
                translation,
            )
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct State {
    native_scope: uuid::Uuid,
    recording: Option<SettingsField>,
    recording_error: Option<String>,
    reassign: Option<(SettingsField, String, usize)>,
    filtering_key: bool,
    pressed_key_filter: Option<keyboard::Event>,
    query: Option<String>,
    selected: Option<EntryIdentity>,
    move_pane: Option<CardId>,
}
impl State {
    pub(super) fn pressed_key_filter(&self) -> Option<&keyboard::Event> {
        self.pressed_key_filter.as_ref()
    }

    pub(super) fn clear_pressed_key_filter(&mut self) {
        self.pressed_key_filter = None;
    }

    pub(super) fn is_open(&self) -> bool {
        self.query.is_some() || self.recording.is_some() || self.filtering_key
    }
}
#[derive(Debug, Clone)]
pub enum Action {
    Record(SettingsField),
    Reassign,
    FilterKey,
    Open,
    MovePane(CardId),
    Close,
    CloseFromNative(uuid::Uuid),
    Query(String),
    QueryFromNative(uuid::Uuid, String),
    NavigateFromNative(uuid::Uuid, bool),
    SubmitFromNative(Box<NativeQuerySubmit>),
    Run(MessageBox),
    Submit,
    FocusPane(CardId),
    CopyBranch(ProjectId, crate::model::WorktreeId, PathBuf, String),
    TerminalReview(CardId, crate::model::TerminalId, u64, bool),
    /// Input-only acknowledgement; this never becomes a palette command.
    ConsumedActivation,
}
#[derive(Debug, Clone)]
pub struct NativeQuerySubmit {
    row: Option<MessageBox>,
    scope: uuid::Uuid,
    event: keyboard::Event,
    source: cosmic::iced::runtime::core::keyboard::input::Source,
    // Native available context, independent of the current filter. Queued
    // typing/navigation may change selection, never borrow a replacement source.
    contextual_origins: Vec<EntryIdentity>,
}

// Box breaks the recursive Message/Action representation.
#[derive(Debug, Clone)]
pub struct MessageBox(pub Box<Message>);

// Selection is an intent captured by a query or navigation key, not a live row index.
#[derive(Debug, Clone, PartialEq, Eq)]
enum EntryIdentity {
    Command(Command),
    Pane(CardId),
    Worktree(ProjectId, crate::model::WorktreeId, PathBuf),
    Channel(crate::model::ChannelId),
    Repository(ProjectId, PathBuf),
    ResumeConversation,
    Workboard,
    MovePane(CardId, crate::model::ChannelId),
    Diagnostic(CardId, bool),
    CopyBranch(ProjectId, crate::model::WorktreeId, PathBuf, String),
    TerminalReview(CardId, crate::model::TerminalId, u64, bool),
}

impl EntryIdentity {
    fn widget_id(&self) -> widget::Id {
        widget::Id::new(format!("palette.row.{self:?}"))
    }

    fn capture(message: &Message) -> Option<Self> {
        Some(match message {
            Message::RunCommand(command) => Self::Command(*command),
            Message::Palette(Action::FocusPane(card)) => Self::Pane(*card),
            Message::Sidebar(SidebarAction::OpenWorktree {
                project_id,
                worktree_id,
                path,
            }) => Self::Worktree(*project_id, *worktree_id, path.clone()),
            Message::Sidebar(SidebarAction::OpenChannel { channel_id }) => {
                Self::Channel(*channel_id)
            }
            Message::Sidebar(SidebarAction::OpenRepository {
                project_id,
                repository,
            }) => Self::Repository(*project_id, repository.clone()),
            Message::ResumeConversation(resume_ui::Action::Open) => Self::ResumeConversation,
            Message::Sidebar(SidebarAction::OpenWork) => Self::Workboard,
            Message::MovePaneToChannel { card, channel } => Self::MovePane(*card, *channel),
            Message::Repository(RepositoryAction::Language {
                card,
                action: repository_widget::EditorLspAction::Diagnostic(forward),
            }) => Self::Diagnostic(*card, *forward),
            Message::Palette(Action::CopyBranch(project, worktree, path, branch)) => {
                Self::CopyBranch(*project, *worktree, path.clone(), branch.clone())
            }
            Message::Palette(Action::TerminalReview(card, terminal, revision, active)) => {
                Self::TerminalReview(*card, *terminal, *revision, *active)
            }
            _ => return None,
        })
    }

    fn matches(&self, message: &Message) -> bool {
        if matches!(self, Self::CopyBranch(..) | Self::TerminalReview(..)) {
            return Self::capture(message).as_ref() == Some(self);
        }
        match (self, message) {
            (Self::Command(expected), Message::RunCommand(actual)) => expected == actual,
            (Self::Pane(expected), Message::Palette(Action::FocusPane(actual))) => {
                expected == actual
            }
            (
                Self::Worktree(project, worktree, path),
                Message::Sidebar(SidebarAction::OpenWorktree {
                    project_id,
                    worktree_id,
                    path: actual,
                }),
            ) => project == project_id && worktree == worktree_id && path == actual,
            (
                Self::Channel(expected),
                Message::Sidebar(SidebarAction::OpenChannel { channel_id }),
            ) => expected == channel_id,
            (
                Self::Repository(project, path),
                Message::Sidebar(SidebarAction::OpenRepository {
                    project_id,
                    repository,
                }),
            ) => project == project_id && path == repository,
            (Self::Workboard, Message::Sidebar(SidebarAction::OpenWork)) => true,
            (Self::ResumeConversation, Message::ResumeConversation(resume_ui::Action::Open)) => {
                true
            }
            (
                Self::MovePane(expected_card, expected_channel),
                Message::MovePaneToChannel { card, channel },
            ) => expected_card == card && expected_channel == channel,
            (
                Self::Diagnostic(expected_card, expected_forward),
                Message::Repository(RepositoryAction::Language {
                    card,
                    action: repository_widget::EditorLspAction::Diagnostic(forward),
                }),
            ) => expected_card == card && expected_forward == forward,
            _ => false,
        }
    }
}

const ROW_HEIGHT: f32 = 40.0;
const ROW_FOCUS_OUTSET: f32 = 2.0;
const ROW_SPACING: f32 = space::XXS;

fn palette_query(query: &str) -> (Option<char>, &str) {
    let query = query.trim();
    match query.chars().next() {
        Some(prefix @ ('>' | '@' | '#')) => (Some(prefix), query[1..].trim_start()),
        _ => (None, query),
    }
}

fn palette_mode_query(query: &str, moving: bool) -> (Option<char>, &str) {
    if moving {
        (None, query.trim())
    } else {
        palette_query(query)
    }
}

fn search_label(label: &str, moving: bool) -> &str {
    if moving {
        return label
            .strip_prefix("Move pane to channel: ")
            .unwrap_or(label);
    }
    ["Focus pane: ", "Open workspace: ", "Open channel: "]
        .into_iter()
        .find_map(|prefix| label.strip_prefix(prefix))
        .unwrap_or(label)
}

fn entry_category(message: &Message) -> char {
    match message {
        Message::Palette(Action::FocusPane(_)) => '@',
        Message::Sidebar(
            SidebarAction::OpenWorktree { .. }
            | SidebarAction::OpenChannel { .. }
            | SidebarAction::OpenRepository { .. },
        ) => '#',
        _ => '>',
    }
}

fn entry_key(message: &Message) -> Option<String> {
    match message {
        Message::RunCommand(command) => SettingsField::SHORTCUTS
            .iter()
            .position(|field| field.shortcut_action() == Some(*command))
            .and_then(|index| KeyboardShortcuts::action_names().get(index))
            .map(|name| format!("command:{name}")),
        Message::Palette(Action::FocusPane(id)) => Some(format!("pane:{id}")),
        Message::Sidebar(SidebarAction::OpenWorktree { worktree_id, .. }) => {
            Some(format!("worktree:{worktree_id}"))
        }
        Message::Sidebar(SidebarAction::OpenChannel { channel_id }) => {
            Some(format!("channel:{channel_id}"))
        }
        Message::Sidebar(SidebarAction::OpenRepository { project_id, .. }) => {
            Some(format!("folder:{project_id}"))
        }
        Message::ResumeConversation(resume_ui::Action::Open) => {
            Some("command:resume_conversation".into())
        }
        Message::Sidebar(SidebarAction::OpenWork) => Some("command:open_work".into()),
        _ => None,
    }
}

mod matching;

fn match_score(label: &str, query: &str) -> Option<usize> {
    crate::search_match::score(&matching::lowercase(label), &matching::lowercase(query))
}

fn highlighted_label(label: &str, query: &str, moving: bool) -> Element<'static, Message> {
    use iced::widget::text::Span;
    if query.is_empty() {
        return design::body(label.to_owned())
            .wrapping(iced::widget::text::Wrapping::None)
            .ellipsize(iced::widget::text::Ellipsize::End(
                iced::advanced::text::EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill)
            .into();
    }
    let accent = design::accent_color(&cosmic::theme::active());
    let font = cosmic::font::default();
    let searchable = search_label(label, moving);
    let offset = label.len() - searchable.len();
    let matches = matching::highlighted_ranges(searchable, query, offset);
    let mut spans: Vec<Span<'static, ()>> = Vec::new();
    let mut start = 0;
    for range in matches {
        if start < range.start {
            spans.push(Span::new(label[start..range.start].to_owned()));
        }
        spans.push(
            Span::new(label[range.clone()].to_owned())
                .color(accent)
                .font(iced::Font {
                    weight: iced::font::Weight::Bold,
                    ..font
                }),
        );
        start = range.end;
    }
    if start < label.len() || spans.is_empty() {
        spans.push(Span::new(label[start..].to_owned()));
    }
    widget::container(
        iced::widget::rich_text(spans)
            .size(design::TEXT_BODY)
            .font(font)
            .wrapping(iced::widget::text::Wrapping::None)
            .ellipsize(iced::widget::text::Ellipsize::End(
                iced::advanced::text::EllipsizeHeightLimit::Lines(1),
            ))
            .width(Length::Fill),
    )
    .width(Length::Fill)
    .clip(true)
    .class(design::transparent())
    .into()
}

fn palette_row_style(selected: bool) -> cosmic::theme::Button {
    fn appearance(
        selected: bool,
        hover: bool,
        focused: bool,
        theme: &cosmic::Theme,
    ) -> widget::button::Style {
        let mut accent = design::accent_color(theme);
        accent.a = if selected { 0.18 } else { 0.08 };
        widget::button::Style {
            background: (selected || hover).then_some(iced::Background::Color(accent)),
            border_radius: design::RADIUS_S.into(),
            outline_width: if focused { ROW_FOCUS_OUTSET } else { 0.0 },
            outline_color: design::accent_color(theme),
            ..design::button_style()
        }
    }
    cosmic::theme::Button::Custom {
        active: Box::new(move |focused, theme| appearance(selected, false, focused, theme)),
        disabled: Box::new(move |theme| appearance(selected, false, false, theme)),
        hovered: Box::new(move |focused, theme| appearance(selected, true, focused, theme)),
        pressed: Box::new(move |focused, theme| appearance(true, true, focused, theme)),
    }
}

impl Cagents {
    pub(super) fn update_palette(&mut self, action: Action) -> Task<Message> {
        // Every palette action is user input; none is a lifecycle completion.
        if self.update_prepared || self.exit.is_some() {
            return Task::none();
        }
        match action {
            Action::Record(field) => {
                self.palette.native_scope = uuid::Uuid::new_v4();
                self.palette.recording = Some(field);
                self.palette.filtering_key = false;
                self.palette.query = None;
                self.palette.recording_error = None;
                self.palette.reassign = None;
                Task::none()
            }
            Action::FilterKey => {
                self.palette.filtering_key = true;
                self.palette.recording = None;
                self.palette.query = None;
                self.palette.reassign = None;
                self.palette.recording_error = None;
                Task::none()
            }
            Action::Reassign => {
                let Some((field, chord, owner)) = self.palette.reassign.take() else {
                    return Task::none();
                };
                if self.palette.recording != Some(field) {
                    return Task::none();
                }
                let binding = settings::parse_shortcut(&chord).expect("recorded chord");
                let mut next = self.settings.clone();
                let drafts = self.settings_draft.keyboard_shortcuts.shortcut_values();
                let owner_action = SettingsField::SHORTCUTS[owner]
                    .shortcut_action()
                    .expect("shortcut action");
                let remaining = drafts
                    .iter()
                    .find(|(action, _)| *action == owner_action)
                    .expect("owner draft")
                    .1;
                let mut remaining = settings::parse_chords(remaining);
                remaining.retain(|chord| settings::parse_shortcut(chord).ok() != Some(binding));
                next.keyboard_shortcuts.set_action(owner, remaining.clone());
                let index = SettingsField::SHORTCUTS
                    .iter()
                    .position(|candidate| *candidate == field)
                    .expect("shortcut");
                let action = field.shortcut_action().expect("shortcut action");
                let mine = drafts
                    .iter()
                    .find(|(candidate, _)| *candidate == action)
                    .expect("edited draft")
                    .1;
                let mut mine = settings::parse_chords(mine);
                if !mine
                    .iter()
                    .any(|chord| settings::parse_shortcut(chord).ok() == Some(binding))
                {
                    mine.push(chord);
                }
                next.keyboard_shortcuts.set_action(index, mine.clone());
                let attempted = next.clone();
                match next.validate() {
                    Ok(next) => {
                        let previous = std::mem::replace(&mut self.settings, next);
                        self.settings_draft.set(
                            SettingsField::SHORTCUTS[owner],
                            settings::format_chords(&remaining),
                        );
                        self.settings_draft
                            .set(field, settings::format_chords(&mine));
                        self.settings_pending.remove(&field);
                        self.settings_pending
                            .remove(&SettingsField::SHORTCUTS[owner]);
                        self.settings_invalid.remove(&field);
                        self.settings_invalid
                            .remove(&SettingsField::SHORTCUTS[owner]);
                        self.retry_shortcut_drafts();
                        self.palette.recording = None;
                        self.palette.recording_error = None;
                        self.refresh_settings_feedback();
                        Task::batch([
                            self.save_settings_task(false),
                            self.synchronize_settings(&previous),
                        ])
                    }
                    Err(_) => {
                        self.palette.recording_error = if remaining.is_empty()
                            && settings::shortcut_required(owner)
                        {
                            Some(format!(
                                "{} requires at least one shortcut. Record another shortcut there before reassigning this one.",
                                SettingsField::SHORTCUTS[owner].label()
                            ))
                        } else {
                            refusal(field, &attempted).or_else(|| refusal(SettingsField::SHORTCUTS[owner], &attempted)).or_else(|| Some("Reassignment could not be saved. Check the other settings.".into()))
                        };
                        Task::none()
                    }
                }
            }
            Action::Open => {
                if self.shell_inputs_blocked() {
                    return Task::none();
                }
                self.chrome_ui.close();
                self.palette.native_scope = uuid::Uuid::new_v4();
                self.palette.query = Some(String::new());
                self.palette.move_pane = None;
                self.capture_first_palette_entry();
                iced::widget::operation::focus(widget::Id::new("palette.query"))
                    .map(cosmic::Action::App)
                    .chain(self.reveal_palette_selection())
            }
            Action::MovePane(card) => {
                if self.workspace_inputs_blocked() || !self.pane_has_move_destination(card) {
                    return Task::none();
                }
                let task = self.update_palette(Action::Open);
                self.palette.move_pane = Some(card);
                self.capture_first_palette_entry();
                task
            }
            Action::CloseFromNative(scope) => {
                if !self.palette_native_scope_is_current(scope) {
                    return Task::none();
                }
                self.update_palette(Action::Close)
            }
            Action::Close => {
                self.palette.native_scope = uuid::Uuid::new_v4();
                self.palette.recording = None;
                self.palette.filtering_key = false;
                self.palette.query = None;
                self.palette.reassign = None;
                self.palette.recording_error = None;
                self.palette.move_pane = None;
                self.palette.selected = None;
                self.workspace.focused().map_or_else(Task::none, |card| {
                    iced::widget::operation::focus(self.card_keyboard_focus(card))
                        .map(cosmic::Action::App)
                })
            }
            Action::QueryFromNative(scope, query) => {
                if !self.palette_native_scope_is_current(scope) {
                    return Task::none();
                }
                self.update_palette(Action::Query(query))
            }
            Action::NavigateFromNative(scope, backwards) => {
                if !self.palette_native_scope_is_current(scope) {
                    return Task::none();
                }
                self.navigate_palette_selection(backwards)
            }
            Action::SubmitFromNative(intent) => {
                if !self.palette_native_scope_is_current(intent.scope)
                    || !self
                        .terminal_review_input
                        .claim_query_submit(&intent.event, &intent.source)
                {
                    return Task::none();
                }
                if let Some(MessageBox(message)) = intent.row {
                    return self.run_palette_command(*message);
                }
                if let Some(
                    selected @ (EntryIdentity::CopyBranch(..) | EntryIdentity::TerminalReview(..)),
                ) = self.palette.selected.as_ref()
                    && !intent.contextual_origins.contains(selected)
                {
                    return Task::none();
                }
                self.update_palette(Action::Submit)
            }
            Action::Query(query) => {
                // A native query callback queued after Run/Close cannot reopen it.
                if self.palette.query.is_none() {
                    return Task::none();
                }
                self.palette.query = Some(query);
                self.capture_first_palette_entry();
                self.reveal_palette_selection()
            }
            Action::Run(MessageBox(message)) => self.run_palette_command(*message),
            Action::ConsumedActivation => Task::none(),
            action @ (Action::CopyBranch(..) | Action::TerminalReview(..)) => {
                self.run_palette_command(Message::Palette(action))
            }
            Action::FocusPane(id) => {
                if self.modal_is_open() || self.focused_repository_is_saving() {
                    return Task::none();
                }
                if self
                    .card_state(id)
                    .is_none_or(|card| !card.existence.is_open())
                    || self.refuse_for_deleting_card(id)
                {
                    return Task::none();
                }
                let card = self
                    .card_state(id)
                    .expect("the open target is still present");
                let channel = self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.cards.contains(&id))
                    .map(|channel| channel.id);
                let path = channel.map_or_else(
                    || {
                        self.card_worktree(card)
                            .map(Path::to_path_buf)
                            .or_else(|| self.active_workspace.clone())
                    },
                    |channel| self.channel_directory(channel),
                );
                if path
                    .as_deref()
                    .is_some_and(|path| self.refuse_in_deleting_worktree(path))
                {
                    return Task::none();
                }
                let navigation = if self.workspace.order().any(|candidate| candidate == id) {
                    Task::none()
                } else if let Some(channel) = channel {
                    self.select_channel(channel)
                } else if let Some(path) = path {
                    let (selected, task) = self.select_worktree_checked(path, false);
                    if !selected {
                        return task;
                    }
                    task
                } else {
                    Task::none()
                };
                navigation.chain(self.update(Message::FocusCard(id)))
            }
            Action::Submit => {
                let Some(selected) = self.palette.selected.as_ref() else {
                    return self.update_palette(Action::Close);
                };
                let entry = self
                    .palette_entries()
                    .into_iter()
                    .find(|(_, _, message)| selected.matches(message));
                match entry {
                    Some((_, _, message)) => self.run_palette_command(message),
                    None => {
                        self.status = Some(TransientNotice::warning(
                            "That palette result is no longer available. Choose another result.",
                        ));
                        Task::none()
                    }
                }
            }
        }
    }

    fn palette_native_owner_is_exposed(&self) -> bool {
        !self.update_prepared
            && self.exit.is_none()
            && self.refresh_ui.focused
            && !self.modal_is_open()
            && !self.context_inspector_is_overlay()
            && !self.chrome_ui.is_open()
    }
    fn palette_native_scope_is_current(&self, scope: uuid::Uuid) -> bool {
        self.palette.native_scope == scope
            && self.palette.query.is_some()
            && self.palette.recording.is_none()
            && self.palette_native_owner_is_exposed()
    }
    fn navigate_palette_selection(&mut self, backwards: bool) -> Task<Message> {
        let entries = self.palette_entries();
        if entries.is_empty() {
            return Task::none();
        }
        let count = entries.len();
        let selected = self.selected_palette_index(&entries);
        let next = if backwards {
            selected.map_or(count - 1, |i| (i + count - 1) % count)
        } else {
            selected.map_or(0, |i| (i + 1) % count)
        };
        self.palette.selected = EntryIdentity::capture(&entries[next].2);
        self.reveal_palette_selection()
    }

    fn run_palette_command(&mut self, message: Message) -> Task<Message> {
        if self.modal_is_open() || self.context_inspector_is_overlay() || self.chrome_ui.is_open() {
            return Task::none();
        }
        let dispatch = match &message {
            Message::Palette(Action::CopyBranch(project, worktree, _, _)) => {
                if !self.contextual_palette_action_is_current(&message) {
                    return Task::none();
                }
                Some(Message::Sidebar(SidebarAction::ProjectCommand {
                    project_id: *project,
                    worktree_id: Some(*worktree),
                    command: ProjectCommand::CopyBranch,
                }))
            }
            Message::Palette(Action::TerminalReview(card, terminal, _, active)) => {
                if !self.contextual_palette_action_is_current(&message) {
                    return Task::none();
                }
                Some(Message::TerminalFind(terminal_ui::Action::Review(
                    *card, *terminal, *active,
                )))
            }
            _ => None,
        };
        if let Some(key) = entry_key(&message) {
            self.settings
                .palette_recents
                .retain(|recent| recent != &key);
            self.settings.palette_recents.insert(0, key);
            self.settings.palette_recents.truncate(24);
        }
        self.palette.native_scope = uuid::Uuid::new_v4();
        self.palette.query = None;
        self.palette.move_pane = None;
        self.palette.selected = None;
        let command = self.update(dispatch.unwrap_or(message));
        let save = if self.settings_path.is_some() {
            self.save_settings_task(false)
        } else {
            Task::none()
        };
        let command = Task::batch([command, save]);
        // A command can move focus to another pane synchronously. Restore
        // that current pane before running the command's explicit focus
        // effects, which may instead land in a dialog or another control.
        let restore = self.workspace.focused().map_or_else(Task::none, |card| {
            iced::widget::operation::focus(self.card_keyboard_focus(card)).map(cosmic::Action::App)
        });
        restore.chain(command)
    }

    pub(super) fn palette_key_event(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        self.palette_key_event_with_source(
            event,
            &cosmic::iced::runtime::core::keyboard::input::Source::default(),
        )
    }
    pub(super) fn palette_key_event_with_source(
        &mut self,
        event: &keyboard::Event,
        source: &cosmic::iced::runtime::core::keyboard::input::Source,
    ) -> Option<Task<Message>> {
        // Contextual commands can restore a nonterminal pane and close the
        // palette before key-up. Its window-owned key still needs retiring
        // when no terminal widget is mounted to observe that release.
        if matches!(
            event,
            keyboard::Event::KeyReleased { .. } | keyboard::Event::KeyPressed { repeat: true, .. }
        ) && self
            .terminal_review_input
            .consume_keyboard_with_source(event, source)
        {
            return Some(Task::none());
        }
        if self.palette.recording.is_some() || self.palette.filtering_key {
            // Recording is globally visible, but a real dialog owns its
            // key before the recording or settings draft can change.
            if self.modal_is_open() {
                return None;
            }
            if is_escape_press(event) {
                self.palette.recording = None;
                self.palette.filtering_key = false;
                self.palette.reassign = None;
                self.palette.recording_error = None;
                return Some(Task::none());
            }
            if let keyboard::Event::KeyPressed {
                key,
                physical_key,
                modifiers,
                ..
            } = event
            {
                let name = match key {
                    keyboard::Key::Character(value) if value.chars().count() == 1 => {
                        workspace_widget::recordable_shortcut_character(key, *physical_key)
                            .map(|character| character.to_ascii_uppercase().to_string())
                            .or_else(|| self.palette.filtering_key.then(|| value.to_uppercase()))
                    }
                    keyboard::Key::Named(name) => match name {
                        keyboard::key::Named::ArrowLeft => Some("Left".into()),
                        keyboard::key::Named::ArrowRight => Some("Right".into()),
                        keyboard::key::Named::ArrowUp => Some("Up".into()),
                        keyboard::key::Named::ArrowDown => Some("Down".into()),
                        keyboard::key::Named::Tab => Some("Tab".into()),
                        keyboard::key::Named::PageUp => Some("PageUp".into()),
                        keyboard::key::Named::PageDown => Some("PageDown".into()),
                        keyboard::key::Named::F1
                        | keyboard::key::Named::F2
                        | keyboard::key::Named::F3
                        | keyboard::key::Named::F4
                        | keyboard::key::Named::F5
                        | keyboard::key::Named::F6
                        | keyboard::key::Named::F7
                        | keyboard::key::Named::F8
                        | keyboard::key::Named::F9
                        | keyboard::key::Named::F10
                        | keyboard::key::Named::F11
                        | keyboard::key::Named::F12 => Some(format!("{name:?}")),
                        _ => None,
                    },
                    _ => None,
                };
                if let Some(name) = name {
                    let mut chord = String::new();
                    for (held, name) in [
                        (modifiers.control(), "Ctrl+"),
                        (modifiers.alt(), "Alt+"),
                        (modifiers.logo(), "Super+"),
                        (modifiers.shift(), "Shift+"),
                    ] {
                        if held {
                            chord.push_str(name);
                        }
                    }
                    chord.push_str(if name == "," {
                        "Comma"
                    } else if name == "+" {
                        "Plus"
                    } else {
                        &name
                    });
                    if self.palette.filtering_key {
                        self.palette.filtering_key = false;
                        let task = self.update(Message::SettingsSearchChanged(chord));
                        // Match the event, rather than one spelling: a shifted
                        // number key can trigger both digit and symbol chords.
                        self.palette.pressed_key_filter = Some(event.clone());
                        return Some(task);
                    }
                    return Some(
                        self.record_shortcut(
                            self.palette.recording.expect("recording field"),
                            chord,
                        ),
                    );
                }
                if matches!(key, keyboard::Key::Character(_)) {
                    self.palette.recording_error = Some(
                        "This key cannot trigger a shortcut. Use the number row for digit bindings."
                            .into(),
                    );
                }
            }
            return Some(Task::none());
        }
        self.palette.query.as_ref()?;
        if self.modal_is_open() {
            return None;
        }
        if (source.cycle().is_some() || !cfg!(test))
            && matches!(event, keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::ArrowDown | keyboard::key::Named::ArrowUp | keyboard::key::Named::Enter),
                modifiers, ..
            } if modifiers.is_empty())
        {
            return Some(Task::none());
        }
        if is_escape_press(event) {
            return Some(self.update_palette(Action::Close));
        }
        if let keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key),
            modifiers,
            ..
        } = event
            && modifiers.is_empty()
        {
            let entries = self.palette_entries();
            let count = entries.len();
            match key {
                keyboard::key::Named::ArrowDown if count > 0 => {
                    return Some(self.navigate_palette_selection(false));
                }
                keyboard::key::Named::ArrowUp if count > 0 => {
                    return Some(self.navigate_palette_selection(true));
                }
                keyboard::key::Named::Enter => {
                    if matches!(
                        self.palette.selected,
                        Some(EntryIdentity::CopyBranch(..) | EntryIdentity::TerminalReview(..))
                    ) {
                        if matches!(event, keyboard::Event::KeyPressed { repeat: true, .. }) {
                            return Some(Task::none());
                        }
                        if !self
                            .terminal_review_input
                            .allow_legacy_app_activation(event, source)
                        {
                            return Some(Task::none());
                        }
                        // Valid original press may submit after quick release;
                        // recording must not acquire that released cycle again.
                        self.terminal_review_input
                            .capture_app_activation(event, source);
                    }
                    return Some(self.update_palette(Action::Submit));
                }
                _ => {}
            }
        }
        Some(Task::none())
    }

    fn record_shortcut(&mut self, field: SettingsField, chord: String) -> Task<Message> {
        self.palette.reassign = None;
        let action = field.shortcut_action().expect("shortcut field");
        let value = self
            .settings_draft
            .keyboard_shortcuts
            .shortcut_values()
            .into_iter()
            .find(|(candidate, _)| *candidate == action)
            .expect("shortcut draft")
            .1;
        let mut chords = settings::parse_chords(value);
        if !chords.iter().any(|existing| {
            settings::parse_shortcut(existing).ok() == settings::parse_shortcut(&chord).ok()
        }) {
            chords.push(chord.clone());
        }
        let value = settings::format_chords(&chords);
        match settings_update(&self.settings, field, &value) {
            SettingsDecision::Invalid(reason) => {
                self.palette.recording_error = Some(reason);
                if let Ok(binding) = settings::parse_shortcut(&chord)
                    && settings::shortcut_chord_refusal(binding).is_none()
                    && let Some(owner) =
                        self.settings
                            .keyboard_shortcuts
                            .actions()
                            .iter()
                            .position(|chords| {
                                chords.iter().any(|candidate| {
                                    settings::parse_shortcut(candidate).ok() == Some(binding)
                                })
                            })
                    && SettingsField::SHORTCUTS[owner] != field
                {
                    self.palette.reassign = Some((field, chord, owner));
                }
                Task::none()
            }
            _ => {
                self.palette.recording = None;
                self.palette.recording_error = None;
                self.apply_setting_input(field, value)
            }
        }
    }

    fn capture_first_palette_entry(&mut self) {
        self.palette.selected = self
            .palette_entries()
            .first()
            .and_then(|(_, _, message)| EntryIdentity::capture(message));
    }

    fn selected_palette_index(&self, entries: &[(String, String, Message)]) -> Option<usize> {
        let selected = self.palette.selected.as_ref()?;
        entries
            .iter()
            .position(|(_, _, message)| selected.matches(message))
    }

    fn reveal_palette_selection(&self) -> Task<Message> {
        self.selected_palette_index(&self.palette_entries())
            .map_or_else(Task::none, |index| {
                crate::interaction::reveal_list_selection(
                    widget::Id::new("palette.commands"),
                    index,
                    // Include the leading outline inset in the selected
                    // background's reveal bounds, preserving the row stride.
                    ROW_HEIGHT + ROW_FOCUS_OUTSET,
                    ROW_SPACING - ROW_FOCUS_OUTSET,
                )
            })
    }

    fn palette_pane_entries(&self) -> Vec<(String, String, Message)> {
        // All lookups live for this immutable entry build. Preserve each
        // resolver's first match without scanning memberships for every pane.
        let mut cards = HashMap::new();
        for card in &self.durable.cards {
            cards.entry(card.id).or_insert(card);
        }
        // Match card_state's validated index preference, including its fallback
        // when a restored index is absent or stale.
        for (id, index) in &self.card_indices {
            if let Some(card) = self.durable.cards.get(*index).filter(|card| card.id == *id) {
                cards.insert(*id, card);
            }
        }
        let mut projects = HashMap::new();
        for project in &self.sidebar_model.projects {
            projects.entry(project.id).or_insert(project);
        }
        let mut worktree_labels = HashMap::new();
        for project in projects.values() {
            for worktree in &project.worktrees {
                worktree_labels
                    .entry((project.id, worktree.id))
                    .or_insert_with(|| format!("{} · {}", worktree.label(), project.name));
            }
        }
        let mut path_labels = HashMap::new();
        for project in &self.sidebar_model.projects {
            // worktree_at prefers a folder binding over rows at the same path.
            // A binding without a corresponding row still reserves that path.
            if let Some(key) = project.folder_key() {
                path_labels
                    .entry(project.repository.as_path())
                    .or_insert_with(|| {
                        worktree_labels
                            .get(&(key.project_id, key.worktree_id))
                            .map(String::as_str)
                    });
            }
            for worktree in &project.worktrees {
                path_labels
                    .entry(worktree.path.as_path())
                    .or_insert_with(|| {
                        worktree_labels
                            .get(&(project.id, worktree.id))
                            .map(String::as_str)
                    });
            }
        }
        let channel_places: Vec<_> = self
            .durable
            .channels
            .iter()
            .map(|channel| {
                projects.get(&channel.project_id).map_or_else(
                    || channel.title.clone(),
                    |project| format!("{} · {}", channel.title, project.name),
                )
            })
            .collect();
        let mut pane_channels = HashMap::new();
        for (channel, place) in self.durable.channels.iter().zip(&channel_places) {
            let primary = if channel.title_source == crate::model::ChannelTitleSource::Manual {
                channel.cards.iter().copied().find(|id| {
                    cards.get(id).is_some_and(|card| {
                        card.existence.is_open() && matches!(card.kind, CardKind::Chat(_))
                    })
                })
            } else {
                None
            };
            for id in &channel.cards {
                let entry = pane_channels.entry(*id).or_insert((place.as_str(), None));
                // Empty channels count for ownership but never supply a title.
                if !channel.title.is_empty() && entry.1.is_none() {
                    entry.1 = Some((channel, primary));
                }
            }
        }
        let roots = self.indexed_worktree_roots();
        self.durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
            .map(|card| {
                let title = self.card_display_title_with_chat_title(card, || {
                    let CardKind::Chat(session) = &card.kind else {
                        return None;
                    };
                    let (channel, primary) = pane_channels.get(&card.id)?.1?;
                    (card.title == chat_ui::chat_card_title(session.launch.provider)
                        || (channel.title_source == crate::model::ChannelTitleSource::Manual
                            && primary == Some(card.id)))
                    .then_some(channel.title.as_str())
                });
                let place = pane_channels
                    .get(&card.id)
                    .map(|(place, _)| Cow::Borrowed(*place))
                    .or_else(|| {
                        self.unchanneled_card_worktree_indexed(card, &roots)
                            .map(|path| {
                                path_labels.get(path).copied().flatten().map_or_else(
                                    || {
                                        Cow::Owned(
                                            path.file_name()
                                                .unwrap_or(path.as_os_str())
                                                .to_string_lossy()
                                                .into_owned(),
                                        )
                                    },
                                    Cow::Borrowed,
                                )
                            })
                    })
                    .unwrap_or(Cow::Borrowed("Global"));
                (
                    format!("Focus pane: {title} · {place}"),
                    self.palette_pane_status(card.id).into(),
                    Message::Palette(Action::FocusPane(card.id)),
                )
            })
            .collect()
    }

    fn palette_pane_status(&self, card: CardId) -> &'static str {
        let unread = self.sidebar_state.has_card_update(card);
        match self
            .sidebar_state
            .card_activity(card)
            .map(|activity| activity.state)
        {
            Some(crate::agent::ActivityState::Working) if unread => "● Working · Unread",
            Some(crate::agent::ActivityState::Working) => "● Working",
            Some(crate::agent::ActivityState::Waiting) if unread => "◌ Waiting · Unread",
            Some(crate::agent::ActivityState::Waiting) => "◌ Waiting",
            _ if unread => "• Unread",
            _ => "Idle",
        }
    }

    fn palette_entries(&self) -> Vec<(String, String, Message)> {
        if let Some(card) = self.palette.move_pane {
            return self.filter_palette_entries(self.pane_move_palette_entries(card));
        }
        let mut entries: Vec<_> = workspace_widget::GLOBAL_SHORTCUT_DESCRIPTORS
            .iter()
            .filter(|descriptor| descriptor.action != Command::CommandPalette)
            .filter(|descriptor| descriptor.scope.applies_to(self.focused_pane()))
            .map(|descriptor| {
                (
                    descriptor.label.to_owned(),
                    workspace_widget::shortcut_chords(
                        descriptor.action,
                        &self.settings.keyboard_shortcuts,
                    ),
                    Message::RunCommand(descriptor.action),
                )
            })
            .collect();
        entries.extend(self.contextual_palette_entries());
        if self.workspace_surfaces_visible()
            && let Some(card) = self.workspace.focused()
            && matches!(self.cards.get(&card), Some(CardRuntime::Repository(runtime)) if runtime.editor_focus_target(card).is_some())
        {
            for (label, chord, forward) in [
                ("Next diagnostic", "F8", true),
                ("Previous diagnostic", "Shift+F8", false),
            ] {
                entries.push((
                    label.into(),
                    chord.into(),
                    Message::Repository(RepositoryAction::Language {
                        card,
                        action: repository_widget::EditorLspAction::Diagnostic(forward),
                    }),
                ));
            }
        }
        entries.push((
            "Resume conversation".into(),
            "Search saved native chats".into(),
            Message::ResumeConversation(resume_ui::Action::Open),
        ));
        entries.push((
            "Open Work board".into(),
            "Every worktree and branch in progress".into(),
            Message::Sidebar(SidebarAction::OpenWork),
        ));
        entries.extend(self.palette_pane_entries());
        if let Some(card) = self.workspace.focused() {
            entries.extend(self.pane_move_palette_entries(card));
        }
        for project in &self.sidebar_model.projects {
            if project.kind == ProjectKind::Folder {
                entries.push((
                    format!("Open workspace: {}", project.name),
                    String::new(),
                    Message::Sidebar(SidebarAction::OpenRepository {
                        project_id: project.id,
                        repository: project.repository.clone(),
                    }),
                ));
            }
            for channel in &project.channels {
                entries.push((
                    format!("Open channel: {} · {}", channel.title, project.name),
                    String::new(),
                    Message::Sidebar(SidebarAction::OpenChannel {
                        channel_id: channel.id,
                    }),
                ));
            }
            for worktree in &project.worktrees {
                entries.push((
                    format!("Open workspace: {} · {}", worktree.label(), project.name),
                    String::new(),
                    Message::Sidebar(SidebarAction::OpenWorktree {
                        project_id: project.id,
                        worktree_id: worktree.id,
                        path: worktree.path.clone(),
                    }),
                ));
            }
        }
        self.filter_palette_entries(entries)
    }

    fn contextual_palette_action_is_current(&self, message: &Message) -> bool {
        self.palette.query.is_some()
            && self.palette.move_pane.is_none()
            && EntryIdentity::capture(message).is_some_and(|identity| {
                self.contextual_palette_entries()
                    .iter()
                    .any(|(_, _, current)| identity.matches(current))
            })
    }

    fn contextual_palette_entries(&self) -> Vec<(String, String, Message)> {
        // These actions name the context the reader can actually see. Their
        // captured payload is checked again before closing the palette, so a
        // delayed row cannot borrow a replacement worktree, branch or terminal.
        if self.update_prepared
            || self.exit.is_some()
            || !self.refresh_ui.focused
            || self.palette.recording.is_some()
            || self.modal_is_open()
            || self.context_inspector_is_overlay()
            || self.chrome_ui.is_open()
        {
            return Vec::new();
        }
        let mut entries = Vec::with_capacity(2);
        if let Some(key) = self.sidebar_state.active_worktree()
            && let Some(project) = self.sidebar_model.project(key.project_id)
            && project.kind == ProjectKind::Git
            && let Some(worktree) = project.worktrees.iter().find(|row| {
                row.id == key.worktree_id && row.path == key.canonical_path && !row.detached
            })
            && let Some(branch) = &worktree.branch
            && !matches!(worktree.deletion, Some(sidebar::RowDeletion::Deleting))
            && !self.project_ui.deleting_directory(&worktree.path)
            && !self.project_ui.closing.contains_key(&project.id)
        {
            entries.push((
                format!("Copy branch: {branch}"),
                project.name.clone(),
                Message::Palette(Action::CopyBranch(
                    project.id,
                    worktree.id,
                    worktree.path.clone(),
                    branch.clone(),
                )),
            ));
        }
        if let Some(card) = self.workspace.focused()
            && self.pane_is_active(card)
            && let Some(state) = self.card_state(card)
            && state.existence.is_open()
            && state.exposure == Exposure::Visible
            && let CardKind::Terminal(session) = &state.kind
            && let Some(CardRuntime::Terminal(runtime)) = self.cards.get(&card)
            && runtime.terminal_id() == session.id
            && (self.terminal_review_card != Some(card)
                || self.terminal_review_identity == Some(session.id))
        {
            let active = self.terminal_review_card != Some(card);
            entries.push((
                if active {
                    "Review visible text"
                } else {
                    "Return to terminal input"
                }
                .into(),
                state.title.clone(),
                Message::Palette(Action::TerminalReview(
                    card,
                    session.id,
                    self.terminal_review_revision,
                    active,
                )),
            ));
        }
        entries
    }

    fn pane_move_palette_entries(&self, card: CardId) -> Vec<(String, String, Message)> {
        self.pane_move_destinations(card)
            .into_iter()
            .map(|channel| {
                (
                    format!(
                        "Move pane to channel: {}",
                        self.pane_move_destination_label(channel)
                    ),
                    String::new(),
                    Message::MovePaneToChannel {
                        card,
                        channel: channel.id,
                    },
                )
            })
            .collect()
    }

    fn filter_palette_entries(
        &self,
        mut entries: Vec<(String, String, Message)>,
    ) -> Vec<(String, String, Message)> {
        let (category, query) = palette_mode_query(
            self.palette.query.as_deref().unwrap_or(""),
            self.palette.move_pane.is_some(),
        );
        entries.retain(|(label, _, message)| {
            category.is_none_or(|category| entry_category(message) == category)
                && match_score(search_label(label, self.palette.move_pane.is_some()), query)
                    .is_some()
        });
        entries.sort_by_cached_key(|(label, _, message)| {
            let recent = entry_key(message)
                .and_then(|key| {
                    self.settings
                        .palette_recents
                        .iter()
                        .position(|recent| *recent == key)
                })
                .unwrap_or(usize::MAX);
            let common = match message {
                Message::RunCommand(Command::NewTerminal) => 0,
                Message::RunCommand(Command::NewThread) => 1,
                Message::RunCommand(Command::OpenSettings) => 2,
                _ => 3,
            };
            (
                match_score(search_label(label, self.palette.move_pane.is_some()), query)
                    .unwrap_or(usize::MAX),
                recent,
                common,
            )
        });
        entries
    }

    pub(super) fn palette_surface(&self) -> Option<Element<'_, Message>> {
        if self.palette.recording.is_some() || self.palette.filtering_key {
            let mut children: Vec<Element<'_, Message>> =
                vec![design::body("Press a shortcut · Escape cancels").into()];
            if let Some(reason) = &self.palette.recording_error {
                children.push(
                    design::body(reason)
                        .class(cosmic::theme::Text::Custom(design::destructive_text))
                        .into(),
                );
            }
            if let Some((_, _, owner)) = &self.palette.reassign {
                children.push(
                    widget::button::text(format!(
                        "Reassign from {}",
                        SettingsField::SHORTCUTS[*owner].label()
                    ))
                    .on_press(Message::Palette(Action::Reassign))
                    .into(),
                );
            }
            return Some(
                widget::container(widget::column::with_children(children).spacing(space::S))
                    .class(cosmic::theme::Container::Card)
                    .padding(20)
                    .width(480)
                    .into(),
            );
        }
        let query = self.palette.query.as_ref()?;
        let scope = self.palette.native_scope;
        let entries = self.palette_entries();
        let contextual_origins: Vec<_> = self
            .contextual_palette_entries()
            .iter()
            .filter_map(|(_, _, message)| EntryIdentity::capture(message))
            .collect();
        let query_origins = contextual_origins.clone();
        let input = crate::ui_controls::field::search(
            if self.palette.move_pane.is_some() {
                "Move pane to channel…"
            } else {
                "Search commands, @ panes, # workspaces, > commands"
            },
            query,
        )
        .id(widget::Id::new("palette.query"))
        .on_input(move |query| Message::Palette(Action::QueryFromNative(scope, query)));
        let barrier = self.terminal_review_input.clone();
        // One ordered native query stream; keep the wrapper tag stable.
        let input = crate::interaction::focused_key(input, move |event| {
            // Protect keys consumed by the focused editor while preserving its
            // typing/navigation repeats. Enter's action ticket records itself.
            if matches!(event, keyboard::Event::KeyPressed { .. })
                && !matches!(event, keyboard::Event::KeyPressed { key: keyboard::Key::Named(keyboard::key::Named::Enter), modifiers, .. } if modifiers.is_empty())
            {
                if !matches!(event, keyboard::Event::KeyPressed { key: keyboard::Key::Named(keyboard::key::Named::Tab), modifiers, .. } if modifiers.control() || modifiers.alt() || modifiers.logo())
                {
                    barrier.capture_consumed_key(event);
                }
            }
            let keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(key),
                modifiers,
                repeat,
                ..
            } = event
            else {
                return None;
            };
            if !modifiers.is_empty() {
                return None;
            }
            match key {
                keyboard::key::Named::ArrowDown => {
                    Some(Message::Palette(Action::NavigateFromNative(scope, false)))
                }
                keyboard::key::Named::ArrowUp => {
                    Some(Message::Palette(Action::NavigateFromNative(scope, true)))
                }
                keyboard::key::Named::Enter => {
                    if *repeat || !barrier.capture_query_submit(event) {
                        return Some(Message::Palette(Action::ConsumedActivation));
                    }
                    Some(Message::Palette(Action::SubmitFromNative(Box::new(
                        NativeQuerySubmit {
                            row: None,
                            scope,
                            event: event.clone(),
                            source: cosmic::iced::runtime::core::keyboard::input::current_source(),
                            contextual_origins: query_origins.clone(),
                        },
                    ))))
                }
                _ => None,
            }
        });
        let selected = self.selected_palette_index(&entries);
        let row_ids = entries
            .iter()
            .filter_map(|(_, _, message)| EntryIdentity::capture(message))
            .map(|identity| identity.widget_id())
            .collect();
        let rows: Vec<Element<'_, Message>> = entries
            .into_iter()
            .enumerate()
            .filter_map(|(index, (label, chord, message))| {
                let identity = EntryIdentity::capture(&message)?;
                let (_, needle) = palette_mode_query(query, self.palette.move_pane.is_some());
                let row = widget::row::with_children(vec![
                    highlighted_label(&label, needle, self.palette.move_pane.is_some()),
                    design::caption(chord)
                        .wrapping(iced::widget::text::Wrapping::None)
                        .into(),
                ])
                .spacing(space::M)
                .align_y(Alignment::Center);
                let on_press = Message::Palette(Action::Run(MessageBox(Box::new(message))));
                let button: Element<'_, Message> = widget::button::custom(
                    widget::container(row)
                        .height(Length::Fill)
                        .align_y(Alignment::Center),
                )
                .id(identity.widget_id())
                .name(label)
                .class(palette_row_style(selected == Some(index)))
                .padding([0, 12])
                .width(Length::Fill)
                .height(ROW_HEIGHT)
                .on_press(on_press.clone())
                .into();
                let button = {
                    let barrier = self.terminal_review_input.clone();
                    let captured = on_press.clone();
                    crate::interaction::focused_key(button, move |event| {
                        let keyboard::Event::KeyPressed {
                            key,
                            modifiers,
                            repeat,
                            ..
                        } = event
                        else {
                            return None;
                        };
                        let activation = matches!(
                            key.as_ref(),
                            keyboard::Key::Named(keyboard::key::Named::Enter)
                        ) || (modifiers.is_empty()
                            && matches!(key.as_ref(), keyboard::Key::Character(" ")));
                        if !activation {
                            return None;
                        }
                        if *repeat || !barrier.capture_query_submit(event) {
                            return Some(Message::Palette(Action::ConsumedActivation));
                        }
                        let Message::Palette(Action::Run(payload)) = &captured else {
                            unreachable!()
                        };
                        Some(Message::Palette(Action::SubmitFromNative(Box::new(
                            NativeQuerySubmit {
                                scope,
                                row: Some(payload.clone()),
                                event: event.clone(),
                                source:
                                    cosmic::iced::runtime::core::keyboard::input::current_source(),
                                contextual_origins: Vec::new(),
                            },
                        ))))
                    })
                };
                Some(row::owned(
                    identity,
                    crate::interaction::keyboard_list_row(button),
                ))
            })
            .collect();
        let rows = if rows.is_empty() {
            widget::column::with_children(vec![
                design::caption(if self.palette.move_pane.is_some() {
                    "No matching channels"
                } else {
                    "No matching commands"
                })
                .into(),
            ])
        } else {
            widget::column::with_children(rows)
        };
        let row_barrier = self.terminal_review_input.clone();
        let results = crate::interaction::focused_key(
            crate::interaction::keyboard_list(
                widget::scrollable(
                    widget::container(rows.spacing(ROW_SPACING)).padding([ROW_FOCUS_OUTSET, 16.0]),
                )
                .id(widget::Id::new("palette.commands"))
                .height(Length::Shrink),
                true,
            ),
            move |event| {
                if matches!(event, keyboard::Event::KeyPressed { .. })
                    && !matches!(
                        event,
                        keyboard::Event::KeyPressed {
                            key: keyboard::Key::Named(keyboard::key::Named::Enter),
                            ..
                        }
                    )
                    && !matches!(event, keyboard::Event::KeyPressed { key, modifiers, .. } if modifiers.is_empty() && matches!(key.as_ref(), keyboard::Key::Character(" ")))
                    && !matches!(event, keyboard::Event::KeyPressed { key: keyboard::Key::Named(keyboard::key::Named::Tab), modifiers, .. } if modifiers.control() || modifiers.alt() || modifiers.logo())
                {
                    row_barrier.capture_consumed_key(event);
                }
                None
            },
        );
        let surface: Element<'_, Message> = widget::container(
            widget::column::with_children(vec![
                input.into(),
                widget::container(results).max_height(360).into(),
                design::caption("↑ ↓ choose · Enter run · Escape close").into(),
            ])
            .spacing(space::S),
        )
        .padding(space::M)
        .width(560)
        .class(cosmic::theme::Container::Card)
        .into();
        let barrier = self.terminal_review_input.clone();
        let surface = crate::interaction::focused_key(surface, move |event| {
            if let keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::Escape),
                repeat,
                ..
            } = event
            {
                // Direct component fixtures retain their old untagged route.
                if cfg!(test)
                    && cosmic::iced::runtime::core::keyboard::input::current_source()
                        .cycle()
                        .is_none()
                {
                    return None;
                }
                return Some(Message::Palette(
                    if !repeat && barrier.claim_native_activation(event) {
                        Action::CloseFromNative(scope)
                    } else {
                        Action::ConsumedActivation
                    },
                ));
            }
            // Capture and retire after focus moves between palette controls,
            // without forwarding an owned release to another native child.
            (matches!(event, keyboard::Event::KeyReleased { .. })
                && barrier.consume_keyboard(event))
            .then_some(Message::Palette(Action::ConsumedActivation))
        });
        Some(native_tab::view(
            surface,
            self.palette_native_owner_is_exposed(),
            self.terminal_review_input.clone(),
            row_ids,
        ))
    }

    pub(super) fn run_workspace_command(
        &mut self,
        command: Command,
        modifiers: keyboard::Modifiers,
    ) -> Task<Message> {
        if self.workspace_inputs_blocked()
            && !(command == Command::ToggleInspector
                && self.context_inspector_is_overlay()
                && !self.modal_is_open())
        {
            return Task::none();
        }
        match command {
            Command::ChatAccount | Command::ChatModel | Command::ChatPermissions => {
                self.open_chat_control(command)
            }
            Command::ToggleProjects => self.update_chrome(chrome_ui::Action::ToggleProjects),
            Command::RenamePane => self.workspace.focused().map_or_else(Task::none, |id| {
                self.update_chrome(chrome_ui::Action::Rename(id))
            }),
            Command::CommandPalette => self.update_palette(Action::Open),
            Command::NewThread => {
                self.update_chrome(chrome_ui::Action::Launch(chrome_ui::NewTab::Thread))
            }
            Command::ToggleInspector => self.update(Message::ToggleContextInspector),
            Command::OpenSettings => self.update(Message::Sidebar(SidebarAction::OpenSettings)),
            Command::NewTerminal => self.update(Message::NewTerminal),
            Command::NewChannel => self.new_project_channel(),
            Command::ClonePane => self.clone_focused_pane(),
            Command::CloseFocusedCard => close_focused_message(&self.workspace)
                .map_or_else(Task::none, |message| self.update(message)),
            Command::FocusProjectSearch => self.update(Message::FocusProjectSearch),
            Command::FindInPane => {
                if self.settings_card_focused() {
                    return iced::widget::operation::focus(crate::settings_widgets::search_id())
                        .map(cosmic::Action::App);
                }
                match self.focused_editable_repository_card() {
                    Some(card) => {
                        self.update(Message::Repository(RepositoryAction::OpenFind { card }))
                    }
                    None => {
                        if let Some(task) = self.open_chat_find() {
                            return task;
                        }
                        if let Some(card) = self.workspace.focused()
                            && matches!(self.cards.get(&card), Some(CardRuntime::Terminal(_)))
                        {
                            return self.handle_terminal_find(terminal_ui::Action::Open(card));
                        }
                        Task::none()
                    }
                }
            }
            Command::FocusAdjacent(direction) => self.update(Message::FocusAdjacent(direction)),
            Command::CyclePaneWidth => self.update(Message::CyclePaneWidth),
            Command::CycleWorkspace(direction) => self.cycle_workspace(direction, modifiers),
            Command::MovePaneLeft | Command::MovePaneRight => {
                let Some(id) = self.workspace.focused() else {
                    return Task::none();
                };
                let order: Vec<_> = self.workspace.order().collect();
                let Some(index) = order.iter().position(|candidate| *candidate == id) else {
                    return Task::none();
                };
                let destination = if command == Command::MovePaneLeft {
                    index.saturating_sub(1)
                } else {
                    (index + 1).min(order.len().saturating_sub(1))
                };
                if destination == index {
                    return Task::none();
                }
                let before = if destination < index {
                    order.get(destination).copied()
                } else {
                    order.get(destination + 1).copied()
                };
                self.reorder_pane(id, before)
            }
            Command::ReopenPane => {
                let scope = self.active_workspace_destination();
                let Some(index) = self
                    .recently_closed
                    .iter()
                    .rposition(|(_, saved_scope)| *saved_scope == scope)
                else {
                    return Task::none();
                };
                let (card, saved_scope) = self.recently_closed[index].clone();
                if let CardKind::Chat(session) = &card.kind
                    && session.launch.session_id.is_some()
                {
                    let task =
                        self.resume_saved_conversation(crate::persistence::SavedConversation {
                            card: card.id,
                            title: card.title.clone(),
                            session: session.clone(),
                            open: false,
                            updated_at_unix_ms: crate::chat::now_ms().unwrap_or(0) as i64,
                        });
                    return task;
                }
                self.recently_closed.remove(index);
                if card.kind.is_utility() {
                    self.open_utility_card(card.kind)
                } else {
                    // Close retires the old card's filing. The matching saved
                    // destination identifies the workspace being reopened in;
                    // channel membership is restored separately on insertion.
                    let owner = match saved_scope {
                        Some(
                            WorkspaceDestination::Worktree(_) | WorkspaceDestination::Folder(_),
                        ) => self.active_workspace.clone(),
                        Some(WorkspaceDestination::Channel(_)) | None => None,
                    };
                    self.duplicate_pane_in_scope(card, owner)
                }
            }
            Command::FocusPane1
            | Command::FocusPane2
            | Command::FocusPane3
            | Command::FocusPane4
            | Command::FocusPane5
            | Command::FocusPane6
            | Command::FocusPane7
            | Command::FocusPane8
            | Command::FocusPane9 => {
                let index = match command {
                    Command::FocusPane1 => 0,
                    Command::FocusPane2 => 1,
                    Command::FocusPane3 => 2,
                    Command::FocusPane4 => 3,
                    Command::FocusPane5 => 4,
                    Command::FocusPane6 => 5,
                    Command::FocusPane7 => 6,
                    Command::FocusPane8 => 7,
                    _ => 8,
                };
                let card = self.workspace.order().nth(index);
                card.map_or_else(Task::none, |id| self.update(Message::FocusCard(id)))
            }
            Command::ReviewToggleViewed
            | Command::ReviewNextFile
            | Command::ReviewPreviousFile
            | Command::ReviewNextUnviewed
            | Command::ReviewNextChange
            | Command::ReviewPreviousChange => {
                let Some(card) = self.workspace.focused() else {
                    return Task::none();
                };
                if matches!(self.cards.get(&card), Some(CardRuntime::Repository(_))) {
                    return self.navigate_local_diff(card, command);
                }
                if !matches!(self.cards.get(&card), Some(CardRuntime::PullRequests(_))) {
                    return Task::none();
                }
                let action = match command {
                    Command::ReviewToggleViewed => {
                        pull_request_review::Action::ToggleViewedAndAdvance
                    }
                    Command::ReviewNextFile => pull_request_review::Action::NextFile,
                    Command::ReviewNextUnviewed => pull_request_review::Action::NextUnviewed,
                    Command::ReviewNextChange => pull_request_review::Action::NextChange,
                    Command::ReviewPreviousChange => pull_request_review::Action::PreviousChange,
                    _ => pull_request_review::Action::PreviousFile,
                };
                self.handle_pull_request_review(card, action)
            }
        }
    }
}

impl SettingsField {
    pub(super) fn shortcut_action(self) -> Option<Command> {
        match self {
            Self::ShortcutToggleProjects => Some(Command::ToggleProjects),
            Self::ShortcutRenamePane => Some(Command::RenamePane),
            Self::ShortcutNewTerminal => Some(Command::NewTerminal),
            Self::ShortcutNewChannel => Some(Command::NewChannel),
            Self::ShortcutClonePane => Some(Command::ClonePane),
            Self::ShortcutCloseFocusedColumn => Some(Command::CloseFocusedCard),
            Self::ShortcutSearchProjects => Some(Command::FocusProjectSearch),
            Self::ShortcutFindInPane => Some(Command::FindInPane),
            Self::ShortcutPreviousColumn => Some(Command::FocusAdjacent(
                crate::workspace::FocusDirection::Left,
            )),
            Self::ShortcutNextColumn => Some(Command::FocusAdjacent(
                crate::workspace::FocusDirection::Right,
            )),
            Self::ShortcutCyclePaneWidth => Some(Command::CyclePaneWidth),
            Self::ShortcutPreviousWorkspace => Some(Command::CycleWorkspace(
                crate::workspace::FocusDirection::Left,
            )),
            Self::ShortcutNextWorkspace => Some(Command::CycleWorkspace(
                crate::workspace::FocusDirection::Right,
            )),
            Self::ShortcutReviewToggleViewed => Some(Command::ReviewToggleViewed),
            Self::ShortcutReviewNextFile => Some(Command::ReviewNextFile),
            Self::ShortcutReviewPreviousFile => Some(Command::ReviewPreviousFile),
            Self::ShortcutReviewNextUnviewed => Some(Command::ReviewNextUnviewed),
            Self::ShortcutReviewNextChange => Some(Command::ReviewNextChange),
            Self::ShortcutReviewPreviousChange => Some(Command::ReviewPreviousChange),
            Self::ShortcutCommandPalette => Some(Command::CommandPalette),
            Self::ShortcutNewThread => Some(Command::NewThread),
            Self::ShortcutChatPermissions => Some(Command::ChatPermissions),
            Self::ShortcutChatModel => Some(Command::ChatModel),
            Self::ShortcutChatAccount => Some(Command::ChatAccount),
            Self::ShortcutToggleInspector => Some(Command::ToggleInspector),
            Self::ShortcutOpenSettings => Some(Command::OpenSettings),
            Self::ShortcutMovePaneLeft => Some(Command::MovePaneLeft),
            Self::ShortcutMovePaneRight => Some(Command::MovePaneRight),
            Self::ShortcutReopenPane => Some(Command::ReopenPane),
            Self::ShortcutFocusPane1 => Some(Command::FocusPane1),
            Self::ShortcutFocusPane2 => Some(Command::FocusPane2),
            Self::ShortcutFocusPane3 => Some(Command::FocusPane3),
            Self::ShortcutFocusPane4 => Some(Command::FocusPane4),
            Self::ShortcutFocusPane5 => Some(Command::FocusPane5),
            Self::ShortcutFocusPane6 => Some(Command::FocusPane6),
            Self::ShortcutFocusPane7 => Some(Command::FocusPane7),
            Self::ShortcutFocusPane8 => Some(Command::FocusPane8),
            Self::ShortcutFocusPane9 => Some(Command::FocusPane9),
            _ => None,
        }
    }
}
impl ShortcutDraft {
    pub(super) fn shortcut_values(&self) -> Vec<(Command, &str)> {
        vec![
            (Command::ToggleProjects, self.toggle_projects.as_str()),
            (Command::RenamePane, self.rename_pane.as_str()),
            (Command::NewTerminal, self.new_terminal.as_str()),
            (Command::NewChannel, self.new_channel.as_str()),
            (Command::ClonePane, self.clone_pane.as_str()),
            (
                Command::CloseFocusedCard,
                self.close_focused_column.as_str(),
            ),
            (Command::FocusProjectSearch, self.search_projects.as_str()),
            (Command::FindInPane, self.find_in_pane.as_str()),
            (
                Command::FocusAdjacent(crate::workspace::FocusDirection::Left),
                self.previous_column.as_str(),
            ),
            (
                Command::FocusAdjacent(crate::workspace::FocusDirection::Right),
                self.next_column.as_str(),
            ),
            (Command::CyclePaneWidth, self.cycle_pane_width.as_str()),
            (
                Command::CycleWorkspace(crate::workspace::FocusDirection::Left),
                self.previous_workspace.as_str(),
            ),
            (
                Command::CycleWorkspace(crate::workspace::FocusDirection::Right),
                self.next_workspace.as_str(),
            ),
            (
                Command::ReviewToggleViewed,
                self.review_toggle_viewed.as_str(),
            ),
            (Command::ReviewNextFile, self.review_next_file.as_str()),
            (
                Command::ReviewPreviousFile,
                self.review_previous_file.as_str(),
            ),
            (
                Command::ReviewNextUnviewed,
                self.review_next_unviewed.as_str(),
            ),
            (Command::ReviewNextChange, self.review_next_change.as_str()),
            (
                Command::ReviewPreviousChange,
                self.review_previous_change.as_str(),
            ),
            (Command::CommandPalette, self.command_palette.as_str()),
            (Command::NewThread, self.new_native_thread.as_str()),
            (Command::ChatPermissions, self.chat_permissions.as_str()),
            (Command::ChatModel, self.chat_model.as_str()),
            (Command::ChatAccount, self.chat_account.as_str()),
            (Command::ToggleInspector, self.toggle_inspector.as_str()),
            (Command::OpenSettings, self.open_settings.as_str()),
            (Command::MovePaneLeft, self.move_pane_left.as_str()),
            (Command::MovePaneRight, self.move_pane_right.as_str()),
            (Command::ReopenPane, self.reopen_pane.as_str()),
            (Command::FocusPane1, self.focus_pane_1.as_str()),
            (Command::FocusPane2, self.focus_pane_2.as_str()),
            (Command::FocusPane3, self.focus_pane_3.as_str()),
            (Command::FocusPane4, self.focus_pane_4.as_str()),
            (Command::FocusPane5, self.focus_pane_5.as_str()),
            (Command::FocusPane6, self.focus_pane_6.as_str()),
            (Command::FocusPane7, self.focus_pane_7.as_str()),
            (Command::FocusPane8, self.focus_pane_8.as_str()),
            (Command::FocusPane9, self.focus_pane_9.as_str()),
        ]
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn palette_highlighted_labels_use_native_ellipsis_and_reflow_after_resize() {
        use iced::advanced::text::{self, Paragraph as _};
        use iced::{Font, Pixels, Size};
        type Paragraph = <cosmic::Renderer as text::Renderer>::Paragraph;
        fn config<T>(content: T, width: f32) -> text::Text<T, Font> {
            text::Text {
                content,
                bounds: Size::new(width, 18.0),
                size: Pixels(13.0),
                line_height: text::LineHeight::Absolute(Pixels(18.0)),
                font: Font::MONOSPACE,
                align_x: text::Alignment::Left,
                align_y: iced::alignment::Vertical::Top,
                shaping: text::Shaping::Advanced,
                wrapping: text::Wrapping::None,
                ellipsize: text::Ellipsize::End(text::EllipsizeHeightLimit::Lines(1)),
            }
        }
        let label =
            "Refactor this very long workspace label without wrapping or clipping the match";
        let spans: Vec<text::Span<'_, (), Font>> = vec![
            text::Span::new("Refactor").color(iced::Color::from_rgb(0.9, 0.2, 0.2)),
            text::Span::new(&label[8..]),
        ];
        for width in [64.0, 180.0] {
            let plain = Paragraph::with_text(config(label, width));
            let mut highlighted = Paragraph::with_spans(config(spans.as_slice(), width));
            assert!(
                highlighted.min_bounds().width <= width + 0.5,
                "rich labels must apply the ellipsis strategy, not only clip their source: {:?}",
                highlighted.min_bounds()
            );
            assert!(highlighted.min_bounds().height <= 18.5);
            assert!((highlighted.min_bounds().width - plain.min_bounds().width).abs() <= 0.5);
            highlighted.resize(Size::new(32.0, 18.0));
            assert!(highlighted.min_bounds().width <= 32.5);
            highlighted.resize(Size::new(1200.0, 18.0));
            assert!(
                highlighted.min_bounds().width > width,
                "resizing restores previously ellipsized text"
            );
            assert!(highlighted.min_bounds().height <= 18.5);
        }
    }

    use super::*;

    // App focus/visibility postludes construct Tokio timers even when a test
    // drops their tasks. A worker also drives timers while the synchronous
    // native clipboard fixture polls a returned task with futures::block_on.
    fn native_palette_test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap()
    }

    /// Run a real scroll operation against the retained native widget tree.
    fn scroll_fixture_step(
        app: &Cagents,
        tree: &mut iced::advanced::widget::Tree,
        palette: bool,
        task: Task<Message>,
    ) -> f32 {
        use iced::advanced::{Layout, layout, widget::Operation};
        use iced::{Size, futures::StreamExt};
        struct Probe {
            id: widget::Id,
            offset: Option<f32>,
        }
        impl Operation for Probe {
            fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn Operation)) {
                visit(self);
            }
            fn scrollable(
                &mut self,
                id: Option<&widget::Id>,
                _: iced::Rectangle,
                _: iced::Rectangle,
                translation: iced::Vector,
                _: &mut dyn iced::advanced::widget::operation::Scrollable,
            ) {
                if id == Some(&self.id) {
                    self.offset = Some(translation.y);
                }
            }
        }
        let renderer = super::super::tests::headless_renderer();
        let mut view = if palette {
            app.palette_surface().unwrap()
        } else {
            app.card_view(app.workspace.focused().unwrap(), true, false)
        };
        tree.diff(view.as_widget_mut());
        let node = view.as_widget_mut().layout(
            tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(800.0, 300.0)),
        );
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        executor.block_on(async {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Widget(mut operation) = action {
                        view.as_widget_mut().operate(
                            tree,
                            Layout::new(&node),
                            &renderer,
                            operation.as_mut(),
                        );
                    }
                }
            }
        });
        let mut probe = Probe {
            id: if palette {
                widget::Id::new("palette.commands")
            } else {
                crate::settings_widgets::content_id()
            },
            offset: None,
        };
        view.as_widget_mut()
            .operate(tree, Layout::new(&node), &renderer, &mut probe);
        probe.offset.expect("the results scrollable is mounted")
    }

    #[test]
    fn palette_query_and_reopen_reveal_the_first_selected_result() {
        use iced::advanced::widget::Tree;
        let mut app = settings_focus_fixture();
        let _ = app.update_palette(Action::Open);
        let mut tree = Tree::new(app.palette_surface().unwrap().as_widget());
        let scroll = || {
            iced::widget::operation::scroll_to(
                widget::Id::new("palette.commands"),
                iced::widget::operation::AbsoluteOffset { x: 0.0, y: 300.0 },
            )
        };
        assert!(scroll_fixture_step(&app, &mut tree, true, scroll()) > 0.0);
        // A space keeps every result, so clamping a shorter result set cannot
        // accidentally make this pass without applying the reveal operation.
        let query = app.update_palette(Action::Query(" ".into()));
        assert_eq!(scroll_fixture_step(&app, &mut tree, true, query), 0.0);
        assert!(scroll_fixture_step(&app, &mut tree, true, scroll()) > 0.0);
        let _ = app.update_palette(Action::Close);
        let reopen = app.update_palette(Action::Open);
        assert_eq!(scroll_fixture_step(&app, &mut tree, true, reopen), 0.0);
    }

    #[test]
    fn settings_category_and_query_changes_reveal_the_new_content_start() {
        use iced::advanced::widget::Tree;
        let mut app = settings_focus_fixture();
        app.settings_category = SettingsCategory::Keyboard;
        let mut tree = Tree::new(
            app.card_view(app.workspace.focused().unwrap(), true, false)
                .as_widget(),
        );
        let scroll = || {
            iced::widget::operation::scroll_to(
                crate::settings_widgets::content_id(),
                iced::widget::operation::AbsoluteOffset { x: 0.0, y: 300.0 },
            )
        };
        assert!(scroll_fixture_step(&app, &mut tree, false, scroll()) > 0.0);
        let category = app.update(Message::SelectSettingsCategory(SettingsCategory::Keyboard));
        assert_eq!(scroll_fixture_step(&app, &mut tree, false, category), 0.0);
        assert!(scroll_fixture_step(&app, &mut tree, false, scroll()) > 0.0);
        let query = app.update(Message::SettingsSearchChanged("keyboard".into()));
        assert_eq!(scroll_fixture_step(&app, &mut tree, false, query), 0.0);
    }

    /// Keep the real Settings pane mounted while the real palette comes and
    /// goes. Both controls must receive the same runtime focus operation.
    fn focus_fixture_view(app: &Cagents) -> Element<'_, Message> {
        widget::column::with_children(vec![
            app.card_view(app.workspace.focused().unwrap(), true, false),
            app.palette_surface()
                .unwrap_or_else(|| widget::Space::new().width(1).height(1).into()),
            app.pane_rename_surface()
                .unwrap_or_else(|| widget::Space::new().width(1).height(1).into()),
        ])
        .into()
    }

    fn focus_fixture_step(
        app: &Cagents,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
        task: Task<Message>,
        type_text: bool,
    ) -> Vec<Message> {
        use iced::advanced::{Layout, Shell, clipboard, layout, mouse};
        use iced::{Event, Rectangle, Size, futures::StreamExt};

        let mut element = focus_fixture_view(app);
        tree.diff(element.as_widget_mut());
        let viewport = Rectangle::with_size(Size::new(1000.0, 1600.0));
        let node = element.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let executor = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let mut messages = Vec::new();
        executor.block_on(async {
            if let Some(mut stream) = iced::runtime::task::into_stream(task) {
                // Apply actual immediate runtime effects. Destination changes
                // also schedule maintenance timers; they are not the focus
                // contract and need not finish before typing into the widget.
                while let Ok(Some(action)) =
                    tokio::time::timeout(Duration::from_millis(250), stream.next()).await
                {
                    match action {
                        iced::runtime::Action::Widget(operation) => {
                            let mut next = Some(operation);
                            while let Some(mut operation) = next.take() {
                                element.as_widget_mut().operate(
                                    tree,
                                    Layout::new(&node),
                                    renderer,
                                    operation.as_mut(),
                                );
                                if let iced::advanced::widget::operation::Outcome::Chain(chained) =
                                    operation.finish()
                                {
                                    next = Some(chained);
                                }
                            }
                        }
                        iced::runtime::Action::Output(cosmic::Action::App(message)) => {
                            messages.push(message);
                        }
                        iced::runtime::Action::Window(_) => {}
                        _ => panic!("unexpected runtime action in focus fixture"),
                    }
                }
            }
        });
        executor.shutdown_background();
        if type_text {
            element.as_widget_mut().update(
                tree,
                &Event::Keyboard(keyboard::Event::KeyPressed {
                    key: keyboard::Key::Character("x".into()),
                    modified_key: keyboard::Key::Character("x".into()),
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyX),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: Some("x".into()),
                    repeat: false,
                }),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                renderer,
                &mut clipboard::Null,
                &mut Shell::new(&mut messages),
                &viewport,
            );
        }
        messages
    }

    #[test]
    fn empty_palette_enter_restores_retained_settings_input_like_escape() {
        use iced::advanced::widget::Tree;

        for dismiss in [Action::Submit, Action::Close] {
            let mut app = super::super::tests::context_inspector_test_app(true);
            app.context_inspector_open = false;
            let card = app.workspace.focused().unwrap();
            app.cards.insert(card, CardRuntime::Settings);
            app.card_state_mut(card).unwrap().kind = CardKind::Settings;
            let renderer = super::super::tests::headless_renderer();
            let mut tree = Tree::new(focus_fixture_view(&app).as_widget());
            let focus = iced::widget::operation::focus(crate::settings_widgets::search_id())
                .map(cosmic::Action::App);
            let initial = focus_fixture_step(&app, &mut tree, &renderer, focus, true);
            assert!(initial.iter().any(|message| matches!(
                message, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::SettingsSearchChanged(value) if value == "x")
            )));

            let open = app.update_palette(Action::Open);
            let _ = app.update_palette(Action::Query("zzqxv978unmatched".into()));
            assert!(app.palette_entries().is_empty());
            let while_open = focus_fixture_step(&app, &mut tree, &renderer, open, true);
            assert!(
                !while_open
                    .iter()
                    .any(|message| matches!(message, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::SettingsSearchChanged(_))))
            );
            assert!(
                while_open.iter().any(|message| matches!(
                    message,
                    Message::Palette(Action::QueryFromNative(..))
                ))
            );

            let dismiss = app.update_palette(dismiss);
            assert!(!app.palette.is_open());
            let after = focus_fixture_step(&app, &mut tree, &renderer, dismiss, true);
            assert!(after.iter().any(|message| matches!(
                message, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::SettingsSearchChanged(value) if value == "x")
            )));
        }
    }

    fn settings_focus_fixture() -> Cagents {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        let card = app.workspace.focused().unwrap();
        app.cards.insert(card, CardRuntime::Settings);
        app.card_state_mut(card).unwrap().kind = CardKind::Settings;
        app
    }

    use cosmic::iced::runtime::core::keyboard::input::{
        self as input_source, PressCycle, Source, Transition,
    };

    fn cycle_key(
        named: keyboard::key::Named,
        code: keyboard::key::Code,
        down: bool,
        repeat: bool,
    ) -> keyboard::Event {
        if down {
            keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(named),
                modified_key: keyboard::Key::Named(named),
                physical_key: keyboard::key::Physical::Code(code),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: None,
                repeat,
            }
        } else {
            keyboard::Event::KeyReleased {
                key: keyboard::Key::Named(named),
                modified_key: keyboard::Key::Named(named),
                physical_key: keyboard::key::Physical::Code(code),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
            }
        }
    }
    fn palette_native_batch(
        app: &Cagents,
        focus: widget::Id,
        events: &[iced::Event],
        sources: &[Source],
    ) -> (Vec<Message>, Vec<iced::event::Status>) {
        use iced::{
            Size,
            advanced::{clipboard, widget::operation},
            mouse,
        };
        let mut renderer = crate::app::tests::headless_renderer();
        let mut ui = iced::runtime::UserInterface::build(
            app.palette_surface().unwrap(),
            Size::new(880.0, 800.0),
            iced::runtime::user_interface::Cache::default(),
            &mut renderer,
        );
        ui.operate(&renderer, &mut operation::focusable::focus::<()>(focus));
        let mut messages = Vec::new();
        let (_, statuses) = ui.update_with_sources(
            events,
            sources,
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        (messages, statuses)
    }
    fn reduce_native_palette(app: &mut Cagents, messages: Vec<Message>) -> Vec<String> {
        messages
            .into_iter()
            .flat_map(|message| clipboard_writes(app.update(message)))
            .collect()
    }
    fn tagged(app: &mut Cagents, event: keyboard::Event, source: Source) -> Vec<String> {
        clipboard_writes(app.update(Message::TaggedKeyEvent {
            event,
            source,
            captured: true,
        }))
    }

    #[test]
    fn native_query_quick_press_release_submits_once_and_old_copy_cannot_act_after_reopen() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use keyboard::key::{Code, Named, Physical};
        let mut app = branch_palette_fixture();
        let press = cycle_key(Named::Enter, Code::Enter, true, false);
        let release = cycle_key(Named::Enter, Code::Enter, false, false);
        let cycle = PressCycle::new(Physical::Code(Code::Enter));
        let sources = [
            Source::tracked(cycle.clone(), Transition::Press),
            Source::tracked(cycle.clone(), Transition::Release),
        ];
        cycle.release(); // real ingress has queued key-up before native batch runs
        let (messages, statuses) = palette_native_batch(
            &app,
            widget::Id::new("palette.query"),
            &[
                iced::Event::Keyboard(press.clone()),
                iced::Event::Keyboard(release.clone()),
            ],
            &sources,
        );
        assert_eq!(statuses, vec![iced::event::Status::Captured; 2]);
        assert!(matches!(
            messages.as_slice(),
            [
                Message::Palette(Action::SubmitFromNative(_)),
                Message::Palette(Action::ConsumedActivation)
            ]
        ));
        assert_eq!(
            reduce_native_palette(&mut app, messages),
            ["feature/日本語-copy"]
        );
        assert!(!app.palette.is_open());
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query(">copy branch".into())));
        assert!(tagged(&mut app, press, sources[0].clone()).is_empty());
        assert!(tagged(&mut app, release, sources[1].clone()).is_empty());
        assert!(
            app.palette.is_open(),
            "delayed subscription copies cannot submit a new palette"
        );
    }

    #[test]
    fn native_query_arrow_repeat_then_enter_resolves_the_ordered_selection_in_both_directions() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use keyboard::key::{Code, Named, Physical};
        for backwards in [false, true] {
            let mut app = branch_palette_fixture();
            drop(app.update_palette(Action::Query(">".into())));
            let entries = app.palette_entries();
            let target = entries
                .iter()
                .position(|(_, _, message)| {
                    matches!(message, Message::Palette(Action::CopyBranch(..)))
                })
                .unwrap();
            let start = app.selected_palette_index(&entries).unwrap();
            let steps = if backwards {
                (start + entries.len() - target) % entries.len()
            } else {
                (target + entries.len() - start) % entries.len()
            };
            assert!(
                steps > 0,
                "start on an ordinary result, navigate into contextual command"
            );
            let (named, code) = if backwards {
                (Named::ArrowUp, Code::ArrowUp)
            } else {
                (Named::ArrowDown, Code::ArrowDown)
            };
            let navigation = PressCycle::new(Physical::Code(code));
            let enter = PressCycle::new(Physical::Code(Code::Enter));
            let mut events: Vec<_> = (0..steps)
                .map(|n| iced::Event::Keyboard(cycle_key(named, code, true, n > 0)))
                .collect();
            let mut sources = vec![Source::tracked(navigation, Transition::Press); steps];
            events.push(iced::Event::Keyboard(cycle_key(
                Named::Enter,
                Code::Enter,
                true,
                false,
            )));
            sources.push(Source::tracked(enter.clone(), Transition::Press));
            enter.release();
            let (messages, statuses) =
                palette_native_batch(&app, widget::Id::new("palette.query"), &events, &sources);
            assert!(
                statuses
                    .iter()
                    .all(|status| *status == iced::event::Status::Captured)
            );
            assert_eq!(
                reduce_native_palette(&mut app, messages),
                ["feature/日本語-copy"]
            );
            assert!(!app.palette.is_open());
        }
    }

    #[test]
    fn native_query_edit_then_enter_does_not_run_the_old_hidden_contextual_result() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use keyboard::key::{Code, Named, Physical};
        let mut app = branch_palette_fixture();
        let select = keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("a".into()),
            modified_key: keyboard::Key::Character("a".into()),
            physical_key: Physical::Code(Code::KeyA),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL,
            text: None,
            repeat: false,
        };
        let edit = keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("z".into()),
            modified_key: keyboard::Key::Character("z".into()),
            physical_key: Physical::Code(Code::KeyZ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some("zzzx-no-palette-result".into()),
            repeat: false,
        };
        let events = [
            iced::Event::Keyboard(select),
            iced::Event::Keyboard(edit),
            iced::Event::Keyboard(cycle_key(Named::Enter, Code::Enter, true, false)),
        ];
        let sources = [Code::KeyA, Code::KeyZ, Code::Enter]
            .map(|code| Source::tracked(PressCycle::new(Physical::Code(code)), Transition::Press));
        let (messages, _) =
            palette_native_batch(&app, widget::Id::new("palette.query"), &events, &sources);
        assert!(matches!(
            messages.as_slice(),
            [
                Message::Palette(Action::QueryFromNative(..)),
                Message::Palette(Action::SubmitFromNative(_))
            ]
        ));
        assert!(
            reduce_native_palette(&mut app, messages).is_empty(),
            "old Copy branch target became hidden before Enter"
        );
        assert!(
            !app.palette.is_open(),
            "empty ordered submit keeps existing close behavior"
        );
    }

    #[test]
    fn native_query_post_submit_edit_and_old_scope_cannot_reopen_or_mutate_palette() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use keyboard::key::{Code, Named, Physical};
        let mut app = branch_palette_fixture();
        let scope = app.palette.native_scope;
        let edit = keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("z".into()),
            modified_key: keyboard::Key::Character("z".into()),
            physical_key: Physical::Code(Code::KeyZ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some("z".into()),
            repeat: false,
        };
        let events = [
            iced::Event::Keyboard(cycle_key(Named::Enter, Code::Enter, true, false)),
            iced::Event::Keyboard(edit),
        ];
        let sources = [Code::Enter, Code::KeyZ]
            .map(|code| Source::tracked(PressCycle::new(Physical::Code(code)), Transition::Press));
        let (messages, _) =
            palette_native_batch(&app, widget::Id::new("palette.query"), &events, &sources);
        assert!(matches!(
            messages.as_slice(),
            [
                Message::Palette(Action::SubmitFromNative(_)),
                Message::Palette(Action::QueryFromNative(..))
            ]
        ));
        assert_eq!(
            reduce_native_palette(&mut app, messages),
            ["feature/日本語-copy"]
        );
        assert!(!app.palette.is_open());
        drop(app.update_palette(Action::Open));
        let query = app.palette.query.clone();
        drop(app.update_palette(Action::QueryFromNative(scope, "old callback".into())));
        drop(app.update_palette(Action::NavigateFromNative(scope, false)));
        assert_eq!(app.palette.query, query);
    }

    #[test]
    fn native_row_query_and_ordinary_rows_share_one_action_ticket_until_fresh_press() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use keyboard::key::{Code, Named, Physical};
        for (query_focus, key, code) in [
            (false, keyboard::Key::Named(Named::Enter), Code::Enter),
            (true, keyboard::Key::Named(Named::Enter), Code::Enter),
            (false, keyboard::Key::Character(" ".into()), Code::Space),
        ] {
            let mut app = branch_palette_fixture();
            let cycle = PressCycle::new(Physical::Code(code));
            let source = Source::tracked(cycle.clone(), Transition::Press);
            let press = keyboard::Event::KeyPressed {
                key: key.clone(),
                modified_key: key.clone(),
                physical_key: Physical::Code(code),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: (code == Code::Space).then(|| " ".into()),
                repeat: false,
            };
            let identity = EntryIdentity::capture(&app.palette_entries()[0].2).unwrap();
            let (messages, _) = palette_native_batch(
                &app,
                identity.widget_id(),
                &[iced::Event::Keyboard(press.clone())],
                &[source.clone()],
            );
            assert_eq!(
                reduce_native_palette(&mut app, messages),
                ["feature/日本語-copy"]
            );
            drop(app.update_palette(Action::Open));
            drop(app.update_palette(Action::Query(">copy branch".into())));
            assert!(tagged(&mut app, press.clone(), source.clone()).is_empty());
            let focus = if query_focus {
                widget::Id::new("palette.query")
            } else {
                identity.widget_id()
            };
            let (messages, _) = palette_native_batch(
                &app,
                focus.clone(),
                &[iced::Event::Keyboard(press.clone())],
                &[source.clone()],
            );
            assert!(matches!(
                messages.as_slice(),
                [Message::Palette(Action::ConsumedActivation)]
            ));
            assert!(reduce_native_palette(&mut app, messages).is_empty());
            assert!(app.palette.is_open());
            cycle.release();
            let release = keyboard::Event::KeyReleased {
                key: key.clone(),
                modified_key: key,
                physical_key: Physical::Code(code),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
            };
            let (messages, _) = palette_native_batch(
                &app,
                focus.clone(),
                &[iced::Event::Keyboard(release)],
                &[Source::tracked(cycle, Transition::Release)],
            );
            drop(reduce_native_palette(&mut app, messages));
            let fresh = Source::tracked(PressCycle::new(Physical::Code(code)), Transition::Press);
            let (messages, _) =
                palette_native_batch(&app, focus, &[iced::Event::Keyboard(press)], &[fresh]);
            assert_eq!(
                reduce_native_palette(&mut app, messages),
                ["feature/日本語-copy"]
            );
        }
    }

    #[test]
    fn native_p1_r1_p2_batch_and_delayed_app_r1_preserve_exact_new_press_ownership() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use keyboard::key::{Code, Named, Physical};
        for query in [false, true] {
            let mut app = branch_palette_fixture();
            let press = cycle_key(Named::Enter, Code::Enter, true, false);
            let release = cycle_key(Named::Enter, Code::Enter, false, false);
            let first = PressCycle::new(Physical::Code(Code::Enter));
            first.release();
            let second = PressCycle::new(Physical::Code(Code::Enter));
            let sources = [
                Source::tracked(first.clone(), Transition::Press),
                Source::tracked(first, Transition::Release),
                Source::tracked(second, Transition::Press),
            ];
            let events = [
                iced::Event::Keyboard(press.clone()),
                iced::Event::Keyboard(release.clone()),
                iced::Event::Keyboard(press.clone()),
            ];
            let focus = if query {
                widget::Id::new("palette.query")
            } else {
                EntryIdentity::capture(&app.palette_entries()[0].2)
                    .unwrap()
                    .widget_id()
            };
            let (messages, statuses) = palette_native_batch(&app, focus, &events, &sources);
            assert_eq!(statuses, vec![iced::event::Status::Captured; 3]);
            assert_eq!(
                reduce_native_palette(&mut app, messages),
                ["feature/日本語-copy"]
            );
            for (event, source) in [press.clone(), release, press.clone()]
                .into_iter()
                .zip(sources.iter().cloned())
            {
                assert!(tagged(&mut app, event, source).is_empty());
            }
            let _native = input_source::scope(sources[2].clone());
            assert!(
                app.terminal_review_input
                    .owns_keyboard(&iced::Event::Keyboard(press))
            );
        }
    }

    #[test]
    fn native_query_and_row_preserve_ordinary_settings_command_and_tab_forwarding() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use keyboard::key::{Code, Named, Physical};
        for query_focus in [false, true] {
            let mut app = crate::app::tests::context_inspector_test_app(true);
            app.refresh_ui.focused = true;
            app.context_inspector_open = false;
            let original = app.workspace.focused().unwrap();
            drop(app.update_palette(Action::Open));
            drop(app.update_palette(Action::Query("open settings".into())));
            assert_eq!(app.palette_entries().len(), 1);
            let identity = EntryIdentity::capture(&app.palette_entries()[0].2).unwrap();
            assert!(matches!(
                identity,
                EntryIdentity::Command(Command::OpenSettings)
            ));
            let focus = if query_focus {
                widget::Id::new("palette.query")
            } else {
                identity.widget_id()
            };
            let enter = Source::tracked(
                PressCycle::new(Physical::Code(Code::Enter)),
                Transition::Press,
            );
            let (messages, statuses) = palette_native_batch(
                &app,
                focus,
                &[iced::Event::Keyboard(cycle_key(
                    Named::Enter,
                    Code::Enter,
                    true,
                    false,
                ))],
                &[enter],
            );
            assert_eq!(statuses, [iced::event::Status::Captured]);
            assert!(reduce_native_palette(&mut app, messages).is_empty());
            assert!(!app.palette.is_open());
            let destination = app.workspace.focused().unwrap();
            assert_ne!(destination, original);
            assert!(matches!(
                app.cards.get(&destination),
                Some(CardRuntime::Settings)
            ));
        }
        let app = branch_palette_fixture();
        let tab = Source::tracked(
            PressCycle::new(Physical::Code(Code::Tab)),
            Transition::Press,
        );
        let (messages, _) = palette_native_batch(
            &app,
            widget::Id::new("palette.query"),
            &[iced::Event::Keyboard(cycle_key(
                Named::Tab,
                Code::Tab,
                true,
                false,
            ))],
            &[tab],
        );
        assert!(
            messages.is_empty(),
            "native Tab traverses without a deferred App message"
        );
    }

    #[test]
    fn native_row_arrows_then_enter_activate_ordered_target_in_both_directions_and_categories() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use keyboard::key::{Code, Named, Physical};
        for target_contextual in [false, true] {
            let mut app = branch_palette_fixture();
            drop(app.update_palette(Action::Query(">".into())));
            let entries = app.palette_entries();
            let contextual = entries
                .iter()
                .position(|(_, _, message)| {
                    matches!(message, Message::Palette(Action::CopyBranch(..)))
                })
                .unwrap();
            let generic = entries
                .iter()
                .position(|(_, _, message)| {
                    matches!(message, Message::RunCommand(Command::OpenSettings))
                })
                .unwrap();
            let (start, target) = if target_contextual {
                (generic, contextual)
            } else {
                (contextual, generic)
            };
            let origin = EntryIdentity::capture(&entries[start].2).unwrap();
            let expected = EntryIdentity::capture(&entries[target].2).unwrap();
            let backwards = target < start;
            let steps = start.abs_diff(target);
            assert!(steps > 0);
            let (named, code) = if backwards {
                (Named::ArrowUp, Code::ArrowUp)
            } else {
                (Named::ArrowDown, Code::ArrowDown)
            };
            let navigation = PressCycle::new(Physical::Code(code));
            let mut events: Vec<_> = (0..steps)
                .map(|n| iced::Event::Keyboard(cycle_key(named, code, true, n > 0)))
                .collect();
            let mut sources = vec![Source::tracked(navigation, Transition::Press); steps];
            events.push(iced::Event::Keyboard(cycle_key(
                Named::Enter,
                Code::Enter,
                true,
                false,
            )));
            sources.push(Source::tracked(
                PressCycle::new(Physical::Code(Code::Enter)),
                Transition::Press,
            ));
            let (messages, statuses) =
                palette_native_batch(&app, origin.widget_id(), &events, &sources);
            assert_eq!(statuses, vec![iced::event::Status::Captured; events.len()]);
            assert!(matches!(
                messages.as_slice(),
                [Message::Palette(Action::SubmitFromNative(_))]
            ));
            let writes = reduce_native_palette(&mut app, messages);
            if target_contextual {
                assert_eq!(writes, ["feature/日本語-copy"]);
            } else {
                assert!(
                    writes.is_empty(),
                    "the originally focused Copy branch row must not run"
                );
                assert_eq!(
                    app.settings.palette_recents.first(),
                    entry_key(&Message::RunCommand(Command::OpenSettings)).as_ref()
                );
            }
            assert!(
                !app.palette.is_open(),
                "ordered Enter executes {:?}",
                expected
            );
        }
    }

    #[test]
    fn native_query_can_navigate_from_contextual_to_generic_before_enter() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use keyboard::key::{Code, Named, Physical};
        let mut app = branch_palette_fixture();
        drop(app.update_palette(Action::Query(">".into())));
        let entries = app.palette_entries();
        let start = entries
            .iter()
            .position(|(_, _, m)| matches!(m, Message::Palette(Action::CopyBranch(..))))
            .unwrap();
        let target = entries
            .iter()
            .position(|(_, _, m)| matches!(m, Message::RunCommand(Command::OpenSettings)))
            .unwrap();
        app.palette.selected = EntryIdentity::capture(&entries[start].2);
        let steps = (target + entries.len() - start) % entries.len();
        assert!(steps > 0);
        let nav = PressCycle::new(Physical::Code(Code::ArrowDown));
        let mut events: Vec<_> = (0..steps)
            .map(|n| {
                iced::Event::Keyboard(cycle_key(Named::ArrowDown, Code::ArrowDown, true, n > 0))
            })
            .collect();
        let mut sources = vec![Source::tracked(nav, Transition::Press); steps];
        events.push(iced::Event::Keyboard(cycle_key(
            Named::Enter,
            Code::Enter,
            true,
            false,
        )));
        sources.push(Source::tracked(
            PressCycle::new(Physical::Code(Code::Enter)),
            Transition::Press,
        ));
        let (messages, _) =
            palette_native_batch(&app, widget::Id::new("palette.query"), &events, &sources);
        assert!(reduce_native_palette(&mut app, messages).is_empty());
        assert!(!app.palette.is_open());
        assert_eq!(
            app.settings.palette_recents.first(),
            entry_key(&Message::RunCommand(Command::OpenSettings)).as_ref()
        );
    }

    #[test]
    fn native_palette_intents_refuse_a_new_cover_without_mutating_or_closing_the_palette() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use keyboard::key::{Code, Named, Physical};
        for row in [false, true] {
            for cover in ["approval", "image", "unfocused", "freeze"] {
                let mut app = branch_palette_fixture();
                drop(app.update_palette(Action::Query("open settings".into())));
                let origin = EntryIdentity::capture(&app.palette_entries()[0].2).unwrap();
                let scope = app.palette.native_scope;
                let press = cycle_key(Named::Enter, Code::Enter, true, false);
                let source = Source::tracked(
                    PressCycle::new(Physical::Code(Code::Enter)),
                    Transition::Press,
                );
                let focus = if row {
                    origin.widget_id()
                } else {
                    widget::Id::new("palette.query")
                };
                let (messages, statuses) = palette_native_batch(
                    &app,
                    focus,
                    &[iced::Event::Keyboard(press.clone())],
                    &[source.clone()],
                );
                assert_eq!(statuses, [iced::event::Status::Captured]);
                match cover {
                    "approval" => open_copy_release_approval(&mut app),
                    "image" => {
                        app.chat_ui.image_viewer = Some(chat_ui::images::Gallery::file(
                            "/press-cycle-cover-fixture.png".into(),
                            None,
                            iced::widget::image::Handle::from_rgba(1, 1, vec![0, 0, 0, 255]),
                        ))
                    }
                    "unfocused" => app.refresh_ui.focused = false,
                    "freeze" => app.update_prepared = true,
                    _ => unreachable!(),
                }
                let query = app.palette.query.clone();
                let selected = app.palette.selected.clone();
                let focused = app.workspace.focused();
                assert!(reduce_native_palette(&mut app, messages).is_empty());
                drop(app.update(Message::Palette(Action::QueryFromNative(
                    scope,
                    "covered edit".into(),
                ))));
                drop(app.update(Message::Palette(Action::NavigateFromNative(scope, false))));
                assert_eq!(app.palette.query, query, "cover={cover}, row={row}");
                assert_eq!(app.palette.selected, selected);
                assert_eq!(app.workspace.focused(), focused);
                assert!(app.palette.is_open());
                // Ownership retirement remains independent of palette admission.
                source.cycle().unwrap().release();
                let release = cycle_key(Named::Enter, Code::Enter, false, false);
                assert!(
                    tagged(
                        &mut app,
                        release,
                        Source::tracked(source.cycle().unwrap().clone(), Transition::Release)
                    )
                    .is_empty()
                );
                let _native = input_source::scope(source.clone());
                assert!(
                    !app.terminal_review_input
                        .owns_keyboard(&iced::Event::Keyboard(press))
                );
            }
        }
    }

    #[test]
    fn native_row_focus_survives_rebuild_and_later_tab_or_accessibility_focus_return() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use iced::{
            Size,
            advanced::{clipboard, widget::operation},
            mouse,
        };
        use keyboard::key::{Code, Named, Physical};
        for return_focus in [false, true] {
            let mut app = branch_palette_fixture();
            drop(app.update_palette(Action::Query(">".into())));
            let entries = app.palette_entries();
            let target = entries
                .iter()
                .position(|(_, _, m)| matches!(m, Message::Palette(Action::CopyBranch(..))))
                .unwrap();
            assert!(entries.len() > 2);
            let backwards = target < 2;
            let start = if backwards { target + 2 } else { target - 2 };
            let origin = EntryIdentity::capture(&entries[start].2).unwrap();
            let mut renderer = crate::app::tests::headless_renderer();
            let mut ui = iced::runtime::UserInterface::build(
                app.palette_surface().unwrap(),
                Size::new(880.0, 800.0),
                iced::runtime::user_interface::Cache::default(),
                &mut renderer,
            );
            ui.operate(
                &renderer,
                &mut operation::focusable::focus::<()>(origin.widget_id()),
            );
            let (named, code) = if backwards {
                (Named::ArrowUp, Code::ArrowUp)
            } else {
                (Named::ArrowDown, Code::ArrowDown)
            };
            let nav = Source::tracked(PressCycle::new(Physical::Code(code)), Transition::Press);
            let mut messages = Vec::new();
            let (_, statuses) = ui.update_with_sources(
                &[iced::Event::Keyboard(cycle_key(named, code, true, false))],
                &[nav.clone()],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(statuses, [iced::event::Status::Captured]);
            assert!(
                messages.is_empty(),
                "row navigation is entirely synchronous native focus"
            );
            let cache = ui.into_cache();
            let mut ui = iced::runtime::UserInterface::build(
                app.palette_surface().unwrap(),
                Size::new(880.0, 800.0),
                cache,
                &mut renderer,
            );
            if return_focus {
                // Tab is captured and traversed synchronously in the palette.
                let tab = Source::tracked(
                    PressCycle::new(Physical::Code(Code::Tab)),
                    Transition::Press,
                );
                let (_, statuses) = ui.update_with_sources(
                    &[iced::Event::Keyboard(cycle_key(
                        Named::Tab,
                        Code::Tab,
                        true,
                        false,
                    ))],
                    &[tab],
                    mouse::Cursor::Unavailable,
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                assert_eq!(statuses, [iced::event::Status::Captured]);
                // An explicit AT-delivered focus operation may leave and return
                // to any row; there is no stale logical navigation anchor.
                ui.operate(
                    &renderer,
                    &mut operation::focusable::focus::<()>(widget::Id::new("palette.query")),
                );
                ui.operate(
                    &renderer,
                    &mut operation::focusable::focus::<()>(origin.widget_id()),
                );
            }
            let arrow_count = if return_focus { 2 } else { 1 };
            let mut events: Vec<_> = (0..arrow_count)
                .map(|_| iced::Event::Keyboard(cycle_key(named, code, true, true)))
                .collect();
            let mut sources = vec![nav; arrow_count];
            events.push(iced::Event::Keyboard(cycle_key(
                Named::Enter,
                Code::Enter,
                true,
                false,
            )));
            sources.push(Source::tracked(
                PressCycle::new(Physical::Code(Code::Enter)),
                Transition::Press,
            ));
            let (_, statuses) = ui.update_with_sources(
                &events,
                &sources,
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            assert_eq!(statuses, vec![iced::event::Status::Captured; events.len()]);
            assert!(matches!(
                messages.as_slice(),
                [Message::Palette(Action::SubmitFromNative(_))]
            ));
            drop(ui);
            assert_eq!(
                reduce_native_palette(&mut app, messages),
                ["feature/日本語-copy"]
            );
        }
    }

    #[test]
    fn palette_custom_style_distinguishes_native_row_focus_from_query_selection() {
        let cosmic::theme::Button::Custom {
            active, hovered, ..
        } = palette_row_style(false)
        else {
            unreachable!()
        };
        let theme = cosmic::theme::active();
        assert_eq!(active(false, &theme).outline_width, 0.0);
        assert!(active(true, &theme).outline_width > 0.0);
        assert!(hovered(true, &theme).outline_width > 0.0);
        let cosmic::theme::Button::Custom { active, .. } = palette_row_style(true) else {
            unreachable!()
        };
        assert_eq!(active(false, &theme).outline_width, 0.0);
        assert!(
            active(false, &theme).background.is_some(),
            "query selection remains visible without pretending to hold row focus"
        );
    }

    // Runtime-tag contract fixture: actual native query widget and actual App
    // reducer. The queue producer itself is covered by runtime NativeEvents units.
    fn native_palette_enter(app: &mut Cagents) -> Task<Message> {
        use keyboard::key::{Code, Named, Physical};
        let cycle = PressCycle::new(Physical::Code(Code::Enter));
        let source = Source::tracked(cycle, Transition::Press);
        let (mut messages, statuses) = palette_native_batch(
            app,
            widget::Id::new("palette.query"),
            &[iced::Event::Keyboard(cycle_key(
                Named::Enter,
                Code::Enter,
                true,
                false,
            ))],
            &[source],
        );
        assert_eq!(statuses, [iced::event::Status::Captured]);
        assert!(matches!(
            messages.as_slice(),
            [Message::Palette(Action::SubmitFromNative(_))]
        ));
        app.update(messages.pop().unwrap())
    }

    #[test]
    fn native_query_submits_workspace_channel_resume_and_workboard_families() {
        for family in ["worktree", "repository", "channel", "resume", "workboard"] {
            let mut app = if family == "repository" {
                inactive_palette_pane(false).0
            } else {
                branch_palette_fixture()
            };
            app.refresh_ui.focused = true;
            let destination = tempfile::tempdir().unwrap();
            let protection = tempfile::tempdir().unwrap();
            let directory = destination.path().canonicalize().unwrap();
            app.worktree_protections
                .use_fixture_runtime(protection.path().to_owned());
            if family == "repository" {
                app.sidebar_model.projects.last_mut().unwrap().repository = directory.clone();
            } else {
                let project = &mut app.sidebar_model.projects[0];
                project.repository = directory.clone();
                project.worktrees[0].path = directory.clone();
            }
            if app.palette.is_open() {
                drop(app.update_palette(Action::Close));
            }
            let expected = match family {
                "worktree" => {
                    let p = &app.sidebar_model.projects[0];
                    let w = &p.worktrees[0];
                    EntryIdentity::Worktree(p.id, w.id, w.path.clone())
                }
                "repository" => {
                    let p = app.sidebar_model.projects.last().unwrap();
                    EntryIdentity::Repository(p.id, p.repository.clone())
                }
                "channel" => {
                    let id = crate::model::ChannelId::new();
                    let p = &mut app.sidebar_model.projects[0];
                    p.channels.push(crate::sidebar::SidebarChannel::new(
                        id,
                        "Native needle".into(),
                        false,
                        None,
                        false,
                        0,
                        false,
                        1,
                    ));
                    Arc::make_mut(&mut app.durable)
                        .channels
                        .push(crate::model::ProjectChannel {
                            id,
                            project_id: p.id,
                            title: "Native needle".into(),
                            title_source: Default::default(),
                            ordinal: 1,
                            shared: false,
                            cards: vec![],
                            focused: None,
                        });
                    EntryIdentity::Channel(id)
                }
                "resume" => EntryIdentity::ResumeConversation,
                "workboard" => EntryIdentity::Workboard,
                _ => unreachable!(),
            };
            drop(app.update_palette(Action::Open));
            let query = match family {
                "worktree" => "#feature",
                "repository" => "#heldworkspace",
                "channel" => "#native needle",
                "resume" => ">resume conversation",
                _ => ">work board",
            };
            drop(app.update_palette(Action::Query(query.into())));
            assert_eq!(
                app.palette.selected.as_ref(),
                Some(&expected),
                "family={family}"
            );
            let message = app
                .palette_entries()
                .into_iter()
                .find(|(_, _, m)| expected.matches(m))
                .unwrap()
                .2;
            let recent = entry_key(&message).unwrap();
            drop(native_palette_enter(&mut app));
            assert!(!app.palette.is_open(), "family={family}");
            assert_eq!(app.settings.palette_recents.first(), Some(&recent));
            match expected {
                EntryIdentity::Worktree(_, _, path) | EntryIdentity::Repository(_, path) => {
                    assert_eq!(app.active_workspace, Some(path), "family={family}");
                    assert_eq!(app.durable.active_channel, None);
                }
                EntryIdentity::Channel(id) => {
                    assert_eq!(app.durable.active_channel, Some(id));
                    assert_eq!(app.active_workspace, Some(directory));
                }
                EntryIdentity::ResumeConversation => assert!(app.resume_ui.open),
                EntryIdentity::Workboard => assert!(matches!(
                    app.cards.get(&app.workspace.focused().unwrap()),
                    Some(CardRuntime::Work)
                )),
                _ => unreachable!(),
            }
        }
    }

    #[test]
    fn native_palette_consumed_editor_and_row_keys_keep_repeats_local_and_hold_ownership_after_close()
     {
        use keyboard::key::{Code, Named, Physical};
        for row in [false, true] {
            let mut app = branch_palette_fixture();
            let origin = EntryIdentity::capture(&app.palette_entries()[0].2).unwrap();
            let focus = if row {
                origin.widget_id()
            } else {
                widget::Id::new("palette.query")
            };
            let nav = Source::tracked(
                PressCycle::new(Physical::Code(Code::ArrowDown)),
                Transition::Press,
            );
            let events = [
                iced::Event::Keyboard(cycle_key(Named::ArrowDown, Code::ArrowDown, true, false)),
                iced::Event::Keyboard(cycle_key(Named::ArrowDown, Code::ArrowDown, true, true)),
            ];
            let (messages, statuses) =
                palette_native_batch(&app, focus.clone(), &events, &[nav.clone(), nav.clone()]);
            assert_eq!(statuses, [iced::event::Status::Captured; 2]);
            assert_eq!(
                messages.len(),
                if row { 0 } else { 2 },
                "native query repeats navigate; row repeats move native focus"
            );
            for (named, code) in [(Named::Tab, Code::Tab), (Named::Home, Code::Home)] {
                let source =
                    Source::tracked(PressCycle::new(Physical::Code(code)), Transition::Press);
                let key = cycle_key(named, code, true, false);
                drop(palette_native_batch(
                    &app,
                    focus.clone(),
                    &[iced::Event::Keyboard(key.clone())],
                    &[source.clone()],
                ));
                let _native = input_source::scope(source);
                assert!(
                    app.terminal_review_input
                        .owns_keyboard(&iced::Event::Keyboard(key)),
                    "row={row}, code={code:?}"
                );
            }
            let letter = keyboard::Event::KeyPressed {
                key: keyboard::Key::Character("q".into()),
                modified_key: keyboard::Key::Character("q".into()),
                physical_key: Physical::Code(Code::KeyQ),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: Some("q".into()),
                repeat: false,
            };
            let letter_source = Source::tracked(
                PressCycle::new(Physical::Code(Code::KeyQ)),
                Transition::Press,
            );
            let (letters, _) = palette_native_batch(
                &app,
                focus.clone(),
                &[iced::Event::Keyboard(letter.clone())],
                &[letter_source.clone()],
            );
            if row {
                assert!(
                    letters.is_empty(),
                    "row letters retain existing native forwarding"
                );
            }
            {
                let _native = input_source::scope(letter_source.clone());
                assert!(
                    app.terminal_review_input
                        .owns_keyboard(&iced::Event::Keyboard(letter.clone())),
                    "every palette-focused consumed cycle is protected before App processing, row={row}"
                );
            }
            drop(app.update_palette(Action::Close));
            let _native = input_source::scope(nav);
            assert!(
                app.terminal_review_input.owns_keyboard(&events[1]),
                "held navigation remains protected after palette closes"
            );
        }
        let app = branch_palette_fixture();
        let source = Source::tracked(
            PressCycle::new(Physical::Code(Code::KeyX)),
            Transition::Press,
        );
        let make = |repeat| {
            iced::Event::Keyboard(keyboard::Event::KeyPressed {
                key: keyboard::Key::Character("x".into()),
                modified_key: keyboard::Key::Character("x".into()),
                physical_key: Physical::Code(Code::KeyX),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: Some("x".into()),
                repeat,
            })
        };
        let (messages, statuses) = palette_native_batch(
            &app,
            widget::Id::new("palette.query"),
            &[make(false), make(true)],
            &[source.clone(), source],
        );
        assert_eq!(statuses, [iced::event::Status::Captured; 2]);
        assert_eq!(
            messages
                .iter()
                .filter(|m| matches!(m, Message::Palette(Action::QueryFromNative(..))))
                .count(),
            2,
            "owned text repeat still reaches native editor"
        );
    }

    #[test]
    fn native_query_move_destination_preserves_captured_selection_and_emits_exact_model_transfer() {
        use crate::model::{BrowserId, BrowserRestoreFidelity, BrowserSession};
        use crate::workspace_model::{
            ModelChannel, ModelEdit,
            views::{Change, Pane, Scope, View},
        };
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.refresh_ui.focused = true;
        app.context_inspector_open = false;
        let source = crate::model::ChannelId::new();
        let destination = crate::model::ChannelId::new();
        let project = ProjectId::new();
        let pane = Pane {
            id: CardId::new(),
            title: "Native move fixture".into(),
            user_title: None,
            pending_terminal_start: false,
            width_fraction: 0.5,
            scope: None,
            kind: CardKind::Browser(BrowserSession {
                id: BrowserId::new(),
                current_url: "https://example.com".parse().unwrap(),
                history: vec![],
                history_index: 0,
                scroll_x: 0.0,
                scroll_y: 0.0,
                fidelity: BrowserRestoreFidelity::UrlOnly,
            }),
        };
        // Descriptor-only projection; every returned task is dropped. No
        // browser/provider/terminal process is launched by this fixture.
        for id in [source, destination] {
            Arc::make_mut(&mut app.durable)
                .channels
                .push(crate::model::ProjectChannel {
                    id,
                    project_id: project,
                    title: "Move fixture".into(),
                    title_source: Default::default(),
                    ordinal: 1,
                    shared: true,
                    cards: vec![],
                    focused: None,
                });
        }
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.workspace_model.channels = [source, destination]
            .into_iter()
            .map(|id| ModelChannel {
                id,
                repository: "/repo/.git".into(),
                title: "Needle destination".into(),
                title_source: Default::default(),
                ordinal: 1,
            })
            .collect();
        let mut from = View::empty(source);
        from.revision = 17;
        from.panes.push(pane.clone());
        app.workspace_model.views = vec![from, View::empty(destination)];
        Arc::make_mut(&mut app.durable).active_channel = Some(source);
        app.acknowledge_view_edit(Scope::Channel(source), None);
        app.acknowledge_view_edit(Scope::Channel(destination), None);
        drop(app.reconcile_shared_views());
        let (tx, rx) = async_channel::unbounded();
        app.workspace_model_tx = tx;
        drop(app.update_palette(Action::MovePane(pane.id)));
        drop(app.update_palette(Action::Query("needle".into())));
        assert_eq!(
            app.palette.selected,
            Some(EntryIdentity::MovePane(pane.id, destination))
        );
        let mut newer = app.workspace_model.channel(destination).unwrap().clone();
        newer.id = crate::model::ChannelId::new();
        newer.title = "Needle".into();
        newer.ordinal += 1;
        app.workspace_model.channels.push(newer);
        drop(native_palette_enter(&mut app));
        assert!(!app.palette.is_open());
        assert!(matches!(rx.try_recv().unwrap(), ModelEdit::ChangeView {
            scope: Scope::Channel(from), expected_revision: 17,
            change: Change::Transfer { id, destination: Scope::Channel(to), .. }, ..
        } if from == source && id == pane.id && to == destination));
        assert!(app.pane_transfer_pending(pane.id));
    }

    #[test]
    fn native_pointer_and_advertised_accessibility_runs_refuse_cover_then_fresh_run_succeeds() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use iced::advanced::widget::Tree;
        use iced::mouse::{Button, Event};
        for accessibility in [false, true] {
            for cover in ["approval", "image"] {
                let mut app = branch_palette_fixture();
                drop(app.update_palette(Action::Query("open settings".into())));
                let identity = EntryIdentity::capture(&app.palette_entries()[0].2).unwrap();
                let capture = |app: &Cagents| {
                    if accessibility {
                        click_current_contextual_palette_row(app, &identity)
                    } else {
                        let renderer = crate::app::tests::headless_renderer();
                        let mut tree = Tree::new(app.palette_surface().unwrap().as_widget());
                        let (point, messages) = palette_pointer_step(
                            app,
                            &mut tree,
                            &renderer,
                            &identity,
                            Event::ButtonPressed(Button::Left),
                            None,
                        );
                        assert!(messages.is_empty());
                        palette_pointer_step(
                            app,
                            &mut tree,
                            &renderer,
                            &identity,
                            Event::ButtonReleased(Button::Left),
                            Some(point),
                        )
                        .1
                    }
                };
                let messages = capture(&app);
                assert!(matches!(
                    messages.as_slice(),
                    [Message::Palette(Action::Run(_))]
                ));
                if cover == "approval" {
                    open_copy_release_approval(&mut app);
                } else {
                    app.chat_ui.image_viewer = Some(chat_ui::images::Gallery::file(
                        "/palette-pointer-cover.png".into(),
                        None,
                        iced::widget::image::Handle::from_rgba(1, 1, vec![0, 0, 0, 255]),
                    ));
                }
                let query = app.palette.query.clone();
                let recents = app.settings.palette_recents.clone();
                let focused = app.workspace.focused();
                for message in messages {
                    let task = app.update(message);
                    // Task::batch retains a stream even when every task is
                    // empty. Check actual effects, including the App postlude.
                    use iced::futures::StreamExt;
                    let effects = runtime.block_on(async {
                        tokio::time::timeout(Duration::from_secs(1), async {
                            match iced::runtime::task::into_stream(task) {
                                Some(stream) => stream.collect::<Vec<_>>().await,
                                None => Vec::new(),
                            }
                        })
                        .await
                        .expect("covered palette effects settle within the fixture deadline")
                    });
                    assert!(
                        effects.is_empty(),
                        "cover refusal emits no focus/dispatch effect: {effects:?}"
                    );
                }
                assert_eq!(app.palette.query, query);
                assert_eq!(app.settings.palette_recents, recents);
                assert_eq!(app.workspace.focused(), focused);
                if cover == "approval" {
                    drop(app.cancel_automation_approval());
                } else {
                    app.chat_ui.image_viewer = None;
                }
                assert!(!app.modal_is_open());
                let fresh = capture(&app);
                drop(reduce_native_palette(&mut app, fresh));
                assert!(!app.palette.is_open());
                assert_eq!(
                    app.settings.palette_recents.first(),
                    entry_key(&Message::RunCommand(Command::OpenSettings)).as_ref()
                );
            }
        }
    }

    #[test]
    fn native_tab_then_enter_activates_newly_focused_pane_in_one_batch_in_both_directions() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use keyboard::key::{Code, Named, Physical};
        for backwards in [false, true] {
            let mut app = branch_palette_fixture();
            drop(app.update_palette(Action::Close));
            let original = app.workspace.focused().unwrap();
            let mut next = app.card_state(original).unwrap().clone();
            next.id = CardId::new();
            next.title = "Second settings".into();
            Arc::make_mut(&mut app.durable).cards.push(next.clone());
            app.cards.insert(next.id, CardRuntime::Settings);
            Arc::make_mut(&mut app.durable).focused =
                Some(if backwards { original } else { next.id });
            app.workspace = Cagents::rebuild_workspace(&app.durable);
            drop(app.update_palette(Action::Open));
            drop(app.update_palette(Action::Query("@".into())));
            let entries = app.palette_entries();
            assert_eq!(entries.len(), 2);
            let expected =
                EntryIdentity::capture(&entries[if backwards { 1 } else { 0 }].2).unwrap();
            let other = EntryIdentity::capture(&entries[if backwards { 0 } else { 1 }].2).unwrap();
            app.palette.selected = Some(other.clone());
            let mut tab = cycle_key(Named::Tab, Code::Tab, true, false);
            if backwards && let keyboard::Event::KeyPressed { modifiers, .. } = &mut tab {
                *modifiers = keyboard::Modifiers::SHIFT;
            }
            let sources = [Code::Tab, Code::Enter].map(|code| {
                Source::tracked(PressCycle::new(Physical::Code(code)), Transition::Press)
            });
            let (messages, statuses) = palette_native_batch(
                &app,
                widget::Id::new("palette.query"),
                &[
                    iced::Event::Keyboard(tab),
                    iced::Event::Keyboard(cycle_key(Named::Enter, Code::Enter, true, false)),
                ],
                &sources,
            );
            assert_eq!(
                statuses,
                [iced::event::Status::Captured; 2],
                "COSMIC cannot enqueue a second global traversal"
            );
            assert!(
                matches!(messages.as_slice(), [Message::Palette(Action::SubmitFromNative(intent))] if intent.row.as_ref().is_some_and(|m| expected.matches(&m.0)))
            );
            assert!(reduce_native_palette(&mut app, messages).is_empty());
            let EntryIdentity::Pane(expected) = expected else {
                unreachable!()
            };
            assert_eq!(app.workspace.focused(), Some(expected));
            assert!(!app.palette.is_open());
        }
    }

    #[test]
    fn native_palette_tab_wraps_to_query_handles_missing_focus_and_declines_disabled_or_modified_chords()
     {
        use keyboard::key::{Code, Named, Physical};
        let app = branch_palette_fixture();
        let press = |named, code| iced::Event::Keyboard(cycle_key(named, code, true, false));
        let sources = [Code::Tab, Code::Enter]
            .map(|code| Source::tracked(PressCycle::new(Physical::Code(code)), Transition::Press));
        let (messages, statuses) = palette_native_batch(
            &app,
            widget::Id::new("no-native-palette-focus"),
            &[
                press(Named::Tab, Code::Tab),
                press(Named::Enter, Code::Enter),
            ],
            &sources,
        );
        assert_eq!(statuses, [iced::event::Status::Captured; 2]);
        assert!(
            matches!(messages.as_slice(), [Message::Palette(Action::SubmitFromNative(intent))] if intent.row.is_none()),
            "missing focus begins with query"
        );
        let app = branch_palette_fixture();
        let row = EntryIdentity::capture(&app.palette_entries()[0].2).unwrap();
        let sources = [Code::Tab, Code::Enter]
            .map(|code| Source::tracked(PressCycle::new(Physical::Code(code)), Transition::Press));
        let (messages, statuses) = palette_native_batch(
            &app,
            row.widget_id(),
            &[
                press(Named::Tab, Code::Tab),
                press(Named::Enter, Code::Enter),
            ],
            &sources,
        );
        assert_eq!(statuses, [iced::event::Status::Captured; 2]);
        assert!(
            matches!(messages.as_slice(), [Message::Palette(Action::SubmitFromNative(intent))] if intent.row.is_none()),
            "last row wraps to query synchronously"
        );
        for modifiers in [
            keyboard::Modifiers::CTRL,
            keyboard::Modifiers::ALT,
            keyboard::Modifiers::LOGO,
        ] {
            let app = branch_palette_fixture();
            let mut tab = cycle_key(Named::Tab, Code::Tab, true, false);
            if let keyboard::Event::KeyPressed {
                modifiers: actual, ..
            } = &mut tab
            {
                *actual = modifiers;
            }
            let (messages, statuses) = palette_native_batch(
                &app,
                widget::Id::new("palette.query"),
                &[iced::Event::Keyboard(tab)],
                &[Source::tracked(
                    PressCycle::new(Physical::Code(Code::Tab)),
                    Transition::Press,
                )],
            );
            assert!(messages.is_empty());
            assert_eq!(statuses, [iced::event::Status::Ignored]);
        }
        let mut app = branch_palette_fixture();
        app.settings_reset_confirm = true;
        let (messages, statuses) = palette_native_batch(
            &app,
            widget::Id::new("palette.query"),
            &[press(Named::Tab, Code::Tab)],
            &[Source::tracked(
                PressCycle::new(Physical::Code(Code::Tab)),
                Transition::Press,
            )],
        );
        assert!(messages.is_empty());
        assert_eq!(
            statuses,
            [iced::event::Status::Ignored],
            "covered palette leaves Tab to visible owner"
        );
    }

    fn branch_palette_fixture() -> Cagents {
        let mut app = settings_focus_fixture();
        app.window_width = 1600.0;
        app.refresh_ui.focused = true;
        let path = PathBuf::from("/palette-copy-worktree");
        let worktree = SidebarWorktree::new(
            crate::model::WorktreeId::new(),
            path.clone(),
            Some("feature/日本語-copy".into()),
            false,
            false,
            None,
            None,
            None,
        );
        app.sidebar_model.projects = vec![SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "Copy fixture".into(),
            PathBuf::from("/palette-copy-repository"),
            vec![worktree],
            Vec::new(),
        )];
        app.sidebar_state.sync_active_worktree(
            &app.sidebar_model,
            &SidebarFocus::Repository { worktree: path },
        );
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query(">copy branch".into())));
        app
    }

    fn clipboard_writes(task: Task<Message>) -> Vec<String> {
        use iced::futures::StreamExt;

        iced::futures::executor::block_on(async {
            let Some(mut stream) = iced::runtime::task::into_stream(task) else {
                return Vec::new();
            };
            let mut writes = Vec::new();
            while let Some(effect) = stream.next().await {
                if let iced::runtime::Action::Clipboard(iced::runtime::clipboard::Action::Write {
                    target,
                    contents,
                }) = effect
                {
                    assert_eq!(target, iced::advanced::clipboard::Kind::Standard);
                    writes.push(contents);
                }
            }
            writes
        })
    }

    fn palette_enter(app: &mut Cagents) -> Task<Message> {
        app.palette_key_event(&keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        })
        .expect("the open palette owns Enter")
    }

    fn assert_contextual_palette_button(app: &Cagents, message: &Message, label: &str) {
        use iced::advanced::{Layout, layout, widget::Tree};
        use iced::{Size, mouse};
        use iced_accessibility::accesskit::{Action as A11yAction, Role};
        let identity = EntryIdentity::capture(message).unwrap();
        let renderer = crate::app::tests::headless_renderer();
        let mut element = app.palette_surface().unwrap();
        let mut tree = Tree::new(element.as_widget());
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, Size::new(880.0, 800.0)),
        );
        let accessible =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let id: iced_accessibility::A11yId = identity.widget_id().into();
        let row = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .find(|node| iced_accessibility::IdEq::eq(node.id(), &id))
            .unwrap();
        assert_eq!(row.node().role(), Role::Button);
        assert_eq!(row.node().label(), Some(label));
        assert!(row.node().supports_action(A11yAction::Focus));
        assert!(row.node().supports_action(A11yAction::Click));
    }

    #[test]
    fn contextual_branch_copy_is_a_named_native_row_and_submit_copies_exact_branch() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        let mut app = branch_palette_fixture();
        let entries = app.palette_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "Copy branch: feature/日本語-copy");
        assert_contextual_palette_button(&app, &entries[0].2, &entries[0].0);
        let task = palette_enter(&mut app);
        assert_eq!(clipboard_writes(task), ["feature/日本語-copy"]);
        assert!(!app.palette.is_open());
    }

    fn click_current_contextual_palette_row(
        app: &Cagents,
        identity: &EntryIdentity,
    ) -> Vec<Message> {
        use iced::advanced::{Layout, Shell, clipboard, layout, widget::Tree};
        use iced::{Event, Rectangle, Size, mouse};
        use iced_accessibility::accesskit::{Action as A11yAction, ActionRequest, NodeId, TreeId};
        let target: iced_accessibility::A11yId = identity.widget_id().into();
        let renderer = crate::app::tests::headless_renderer();
        let mut element = app.palette_surface().unwrap();
        let mut tree = Tree::new(element.as_widget());
        let bounds = Rectangle::with_size(Size::new(880.0, 800.0));
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &layout::Limits::new(Size::ZERO, bounds.size()),
        );
        let accessible =
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), &tree, mouse::Cursor::Unavailable);
        let advertised = accessible
            .root()
            .iter()
            .chain(accessible.children())
            .find(|node| iced_accessibility::IdEq::eq(node.id(), &target))
            .expect("current contextual result is advertised by this actual native tree");
        let target_node: NodeId = advertised.id().clone().into();
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            &mut tree,
            &Event::A11y(
                widget::Id::from(target_node.0),
                ActionRequest {
                    action: A11yAction::Click,
                    target_tree: TreeId::ROOT,
                    target_node,
                    data: None,
                },
            ),
            Layout::new(&node),
            mouse::Cursor::Unavailable,
            &renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &bounds,
        );
        assert_eq!(
            messages.len(),
            1,
            "only the advertised current result owns the AX click"
        );
        assert!(
            matches!(&messages[0], Message::Palette(Action::Run(MessageBox(message)))
            if identity.matches(message))
        );
        messages
    }

    #[test]
    fn current_contextual_palette_accessibility_click_runs_its_captured_branch_copy() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        let mut app = branch_palette_fixture();
        let entry = app.palette_entries().pop().unwrap();
        let identity = EntryIdentity::capture(&entry.2).unwrap();
        let mut messages = click_current_contextual_palette_row(&app, &identity);
        let focus = app.workspace.focused();
        assert_eq!(
            clipboard_writes(app.update(messages.pop().unwrap())),
            ["feature/日本語-copy"]
        );
        assert_eq!(app.workspace.focused(), focus);
        assert!(!app.palette.is_open());
    }

    #[test]
    fn current_terminal_palette_accessibility_click_reviews_and_returns_exact_origin() {
        let (mut app, card, _directory) = terminal_palette_fixture();
        for active in [true, false] {
            if !active {
                drop(app.update_palette(Action::Open));
                drop(app.update_palette(Action::Query(">return to terminal input".into())));
            }
            let entry = app.palette_entries().pop().unwrap();
            let identity = EntryIdentity::capture(&entry.2).unwrap();
            let mut messages = click_current_contextual_palette_row(&app, &identity);
            drop(app.update(messages.pop().unwrap()));
            assert_eq!(app.terminal_review_card, active.then_some(card));
            assert_eq!(app.workspace.focused(), Some(card));
            assert!(!app.palette.is_open());
        }
    }

    #[test]
    fn contextual_palette_query_keeps_native_focus_and_caret_when_result_kind_changes() {
        use iced::advanced::widget::Tree;
        let mut app = branch_palette_fixture();
        drop(app.update_palette(Action::Query(">settings".into())));
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(focus_fixture_view(&app).as_widget());
        let focus = Task::batch([
            iced::widget::operation::focus(widget::Id::new("palette.query"))
                .map(cosmic::Action::App),
            widget::text_input::move_cursor_to(widget::Id::new("palette.query"), 1)
                .map(cosmic::Action::App),
        ]);
        drop(focus_fixture_step(&app, &mut tree, &renderer, focus, false));
        for (index, query) in [">settings", ">copy branch", ">settings"]
            .into_iter()
            .enumerate()
        {
            drop(app.update_palette(Action::Query(query.into())));
            let contextual = matches!(app.palette.selected, Some(EntryIdentity::CopyBranch(..)));
            assert_eq!(contextual, index == 1);
            let messages = focus_fixture_step(&app, &mut tree, &renderer, Task::none(), true);
            let mut expected = query.to_owned();
            expected.insert(index + 1, 'x');
            assert!(
                messages.iter().any(|message| matches!(
                    message, Message::Palette(Action::QueryFromNative(scope, actual))
                        if *scope == app.palette.native_scope && actual == &expected
                )),
                "native query focus and its advanced caret survive result-kind transition {index}"
            );
        }
    }

    fn open_copy_release_approval(app: &mut Cagents) {
        use crate::{
            agent::Provider,
            agent_execution::{Origin, Spec, Surface, Target},
            automations::{
                Definition, DirectoryIdentity, Proposal, ProposalState, Schedule, SchedulePattern,
                SessionMode,
            },
        };
        app.active_workspace = Some(
            app.sidebar_state
                .active_worktree()
                .unwrap()
                .canonical_path
                .clone(),
        );
        let definition = Definition {
            name: "Copy release fixture".into(),
            prompt: "Review only the approved fixture".into(),
            agent: Spec {
                provider: Provider::Codex,
                account: Some("codex/system".parse().unwrap()),
                target: Target {
                    repository: app.sidebar_model.projects[0].repository.clone(),
                    directory: app.active_workspace.clone().unwrap(),
                    placement: None,
                },
                surface: Surface::Chat,
                model: Some("offline".into()),
                reasoning_effort: Some("low".into()),
                permission_mode: None,
            },
            schedule: Schedule {
                timezone: "America/Vancouver".into(),
                pattern: SchedulePattern::Weekdays {
                    hour: 9,
                    minute: 30,
                },
            },
            session_mode: SessionMode::Fresh,
            precheck: None,
        };
        let proposal = Proposal {
            id: uuid::Uuid::new_v4(),
            revision: 1,
            owner: Origin::User,
            requested: definition.clone(),
            definition,
            target_identity: DirectoryIdentity {
                device: 1,
                inode: 2,
            },
            channel_title: None,
            created_ms: 1,
            state: ProposalState::Pending,
        };
        // Existing local reducers install and open the immutable proposal;
        // no daemon read, decision or provider task is polled.
        drop(
            app.update_automation_approval(automation_approval::Action::Read {
                generation: 0,
                result: Arc::new(Ok(vec![proposal.clone()])),
            }),
        );
        drop(
            app.update_automation_approval(automation_approval::Action::Open {
                id: proposal.id,
                revision: proposal.revision,
            }),
        );
        assert!(app.automation_approval_open());
    }

    #[test]
    fn closed_copy_palette_retires_activation_without_a_terminal_before_fresh_terminal_input() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Tree, operation},
        };
        use iced::{Event, Rectangle, Size, mouse};
        let press = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        let release = keyboard::Event::KeyReleased {
            key: keyboard::Key::Named(keyboard::key::Named::Enter),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Enter),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Enter),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
        };
        // listen_with delivers releases whether another native control
        // captured them or left them ignored. Neither status may pin a key.
        for (captured, cover) in [false, true].into_iter().flat_map(|captured| {
            ["none", "approval", "image", "update"].map(|cover| (captured, cover))
        }) {
            let mut app = branch_palette_fixture();
            assert!(
                app.cards
                    .values()
                    .all(|runtime| !matches!(runtime, CardRuntime::Terminal(_)))
            );
            let copy = app.update(Message::KeyEvent {
                event: press.clone(),
                captured: true,
            });
            assert_eq!(clipboard_writes(copy), ["feature/日本語-copy"]);
            assert!(!app.palette.is_open());
            assert!(
                app.terminal_review_input
                    .owns_keyboard(&Event::Keyboard(press.clone()))
            );
            let fresh_shortcut = keyboard::Event::KeyPressed {
                key: keyboard::Key::Character("P".into()),
                modified_key: keyboard::Key::Character("P".into()),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyP),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
                text: None,
                repeat: false,
            };
            match cover {
                "update" => {
                    // Model an already-admitted update without polling the
                    // unrelated disk flush; cancellation below is the real reducer.
                    app.update_prepared = true;
                    drop(app.update(Message::KeyEvent {
                        event: fresh_shortcut.clone(),
                        captured: false,
                    }));
                    assert!(
                        !app.palette.is_open(),
                        "fresh unowned shortcut stays frozen"
                    );
                    assert!(
                        !app.terminal_review_input
                            .owns_keyboard(&Event::Keyboard(fresh_shortcut.clone())),
                        "update freeze must not invent activation ownership"
                    );
                }
                "approval" => open_copy_release_approval(&mut app),
                "image" => {
                    app.chat_ui.image_viewer = Some(chat_ui::images::Gallery::file(
                        "/copy-release-fixture.png".into(),
                        None,
                        iced::widget::image::Handle::from_rgba(1, 1, vec![0, 0, 0, 255]),
                    ));
                }
                _ => {}
            }
            let focus = app.workspace.focused();
            let review_revision = app.terminal_review_revision;
            let mut unrelated_release = release.clone();
            if let keyboard::Event::KeyReleased { physical_key, .. } = &mut unrelated_release {
                *physical_key = keyboard::key::Physical::Code(keyboard::key::Code::NumpadEnter);
            }
            drop(app.update(Message::KeyEvent {
                event: unrelated_release,
                captured,
            }));
            assert!(
                app.terminal_review_input
                    .owns_keyboard(&Event::Keyboard(press.clone())),
                "logically equal but physically different key-up cannot retire Enter"
            );
            let mut repeat = press.clone();
            if let keyboard::Event::KeyPressed { repeat, .. } = &mut repeat {
                *repeat = true;
            }
            drop(app.update(Message::KeyEvent {
                event: repeat,
                captured,
            }));
            assert!(
                app.terminal_review_input
                    .owns_keyboard(&Event::Keyboard(press.clone()))
            );
            drop(app.update(Message::KeyEvent {
                event: release.clone(),
                captured,
            }));
            assert!(
                !app.terminal_review_input
                    .owns_keyboard(&Event::Keyboard(press.clone())),
                "actual App routing must clean key-up after the palette closed under {cover}"
            );
            assert_eq!(app.workspace.focused(), focus);
            assert_eq!(app.terminal_review_revision, review_revision);
            assert!(!app.palette.is_open());
            match cover {
                "update" => {
                    assert!(
                        app.update_prepared,
                        "key cleanup must not unfreeze the update"
                    );
                    let (response, receiver) = std::sync::mpsc::sync_channel(1);
                    drop(app.update(Message::UiUpdate(instance::UiUpdateRequest {
                        action: instance::UiUpdateAction::Cancel,
                        response,
                    })));
                    assert_eq!(receiver.try_recv().unwrap(), "ok");
                    assert!(!app.update_prepared);
                    drop(app.update(Message::KeyEvent {
                        event: fresh_shortcut.clone(),
                        captured: false,
                    }));
                    assert!(app.palette.is_open(), "fresh shortcut works after rollback");
                    assert!(
                        !app.terminal_review_input
                            .owns_keyboard(&Event::Keyboard(fresh_shortcut)),
                        "fresh shortcut is not owned by the old activation"
                    );
                    drop(app.update_palette(Action::Close));
                }
                "approval" => {
                    assert!(
                        app.automation_approval_open(),
                        "owned repeat cannot dismiss approval"
                    );
                    // A genuine new press still belongs to the approval. Its
                    // first button dismisses locally; nothing approves work.
                    drop(app.update(Message::KeyEvent {
                        event: press.clone(),
                        captured: false,
                    }));
                    assert!(
                        !app.automation_approval_open(),
                        "fresh Enter must reach the top owner"
                    );
                }
                "image" => {
                    assert!(app.chat_ui.image_viewer.is_some());
                    app.chat_ui.image_viewer = None;
                }
                _ => {}
            }
            assert!(
                app.palette_key_event(&press).is_none(),
                "a fresh closed-palette Enter is not consumed"
            );

            let (mut fixture, card, _directory) =
                crate::app::tests::live_agent_app("copy-release-terminal");
            let state = fixture.card_state(card).unwrap().clone();
            let runtime = fixture.cards.remove(&card).unwrap();
            let durable = Arc::make_mut(&mut app.durable);
            durable.cards.push(state);
            durable.focused = Some(card);
            app.workspace = Cagents::rebuild_workspace(&app.durable);
            app.cards.insert(card, runtime);
            let focus_id = app.card_keyboard_focus(card);
            let renderer = crate::app::tests::headless_renderer();
            let mut element = app.card_view(card, true, false);
            let mut tree = Tree::new(element.as_widget());
            let bounds = Rectangle::with_size(Size::new(800.0, 480.0));
            let node = element.as_widget_mut().layout(
                &mut tree,
                &renderer,
                &layout::Limits::new(Size::ZERO, bounds.size()),
            );
            element.as_widget_mut().operate(
                &mut tree,
                Layout::new(&node),
                &renderer,
                &mut operation::focusable::focus::<()>(focus_id),
            );
            let mut messages = Vec::new();
            let mut shell = Shell::new(&mut messages);
            element.as_widget_mut().update(
                &mut tree,
                &Event::Keyboard(press.clone()),
                Layout::new(&node),
                mouse::Cursor::Unavailable,
                &renderer,
                &mut clipboard::Null,
                &mut shell,
                &bounds,
            );
            assert!(
                shell.is_event_captured(),
                "fresh Enter is admitted by the mounted terminal widget"
            );
            drop(element);
            assert!(
                !app.terminal_review_input
                    .owns_keyboard(&Event::Keyboard(press.clone())),
                "fresh terminal input is not swallowed into review ownership"
            );
            assert_eq!(app.terminal_review_card, None);
        }
    }

    #[test]
    fn closed_palette_ignores_unrelated_keyboard_without_effects() {
        let mut app = settings_focus_fixture();
        let event = keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("q".into()),
            modified_key: keyboard::Key::Character("q".into()),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyQ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some("q".into()),
            repeat: false,
        };
        let before = (
            app.workspace.focused(),
            app.settings_search.clone(),
            app.terminal_review_revision,
        );
        assert!(app.palette_key_event(&event).is_none());
        drop(app.update(Message::KeyEvent {
            event: event.clone(),
            captured: false,
        }));
        assert_eq!(
            (
                app.workspace.focused(),
                app.settings_search.clone(),
                app.terminal_review_revision
            ),
            before
        );
        assert!(!app.palette.is_open());
        assert!(
            !app.terminal_review_input
                .owns_keyboard(&iced::Event::Keyboard(event))
        );
    }

    #[test]
    fn queued_branch_copy_refuses_changed_context_without_closing_the_palette() {
        for change in 0..13 {
            let mut app = branch_palette_fixture();
            let held = app.palette_entries().pop().unwrap().2;
            let project = &mut app.sidebar_model.projects[0];
            match change {
                0 => project.id = ProjectId::new(),
                1 => project.worktrees[0].id = crate::model::WorktreeId::new(),
                2 => project.worktrees[0].path = "/replacement-worktree".into(),
                3 => project.worktrees[0].branch = Some("replacement-branch".into()),
                4 => project.worktrees[0].detached = true,
                5 => project.kind = ProjectKind::Folder,
                6 => app.device_ui.open = true,
                7 => drop(app.update_palette(Action::Close)),
                8 => project.worktrees[0].deletion = Some(sidebar::RowDeletion::Deleting),
                9 => {
                    let path = project.worktrees[0].path.clone();
                    app.project_ui.begin_deleting(path);
                }
                10 => {
                    app.project_ui
                        .closing
                        .insert(project.id, project.repository.clone());
                }
                11 => app.refresh_ui.focused = false,
                12 => app.palette.recording = Some(SettingsField::ShortcutCommandPalette),
                _ => unreachable!(),
            }
            let open = app.palette.is_open();
            let task = app.update_palette(Action::Run(MessageBox(Box::new(held))));
            assert!(clipboard_writes(task).is_empty(), "case {change}");
            assert_eq!(app.palette.is_open(), open, "case {change}");
        }
    }

    #[test]
    fn held_branch_palette_press_cannot_transfer_to_a_changed_branch_value() {
        use iced::advanced::widget::Tree;
        use iced::mouse::{Button, Event};
        let mut app = branch_palette_fixture();
        let renderer = crate::app::tests::headless_renderer();
        let original = EntryIdentity::capture(&app.palette_entries()[0].2).unwrap();
        let mut tree = Tree::new(app.palette_surface().unwrap().as_widget());
        let (point, messages) = palette_pointer_step(
            &app,
            &mut tree,
            &renderer,
            &original,
            Event::ButtonPressed(Button::Left),
            None,
        );
        assert!(messages.is_empty());
        app.sidebar_model.projects[0].worktrees[0].branch = Some("replacement".into());
        let replacement = EntryIdentity::capture(&app.palette_entries()[0].2).unwrap();
        assert_ne!(original, replacement);
        let (_, messages) = palette_pointer_step(
            &app,
            &mut tree,
            &renderer,
            &replacement,
            Event::ButtonReleased(Button::Left),
            Some(point),
        );
        assert!(messages.is_empty());
        drop(palette_enter(&mut app));
        assert!(
            app.palette.is_open(),
            "Enter cannot adopt a replacement identity"
        );
    }

    fn terminal_palette_fixture() -> (Cagents, CardId, tempfile::TempDir) {
        let (mut app, card, directory) =
            crate::app::tests::live_agent_app("palette-offline-terminal");
        app.refresh_ui.focused = true;
        app.card_state_mut(card).unwrap().exposure = Exposure::Visible;
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query(">review visible text".into())));
        (app, card, directory)
    }

    #[test]
    fn focused_terminal_palette_reviews_and_returns_through_existing_reducer() {
        let (mut app, card, _directory) = terminal_palette_fixture();
        let entries = app.palette_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "Review visible text");
        assert_contextual_palette_button(&app, &entries[0].2, &entries[0].0);
        drop(native_palette_enter(&mut app));
        assert_eq!(app.terminal_review_card, Some(card));
        assert!(!app.palette.is_open());
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query(">return to terminal input".into())));
        let entries = app.palette_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "Return to terminal input");
        assert_contextual_palette_button(&app, &entries[0].2, &entries[0].0);
        drop(native_palette_enter(&mut app));
        assert_eq!(app.terminal_review_card, None);
        assert_eq!(app.workspace.focused(), Some(card));
    }

    #[test]
    fn native_contextual_palette_activation_captures_query_and_row_keys_before_exit() {
        use iced::advanced::{
            Layout, Shell, clipboard, layout,
            widget::{Tree, operation},
        };
        use iced::{Event, Rectangle, Size, mouse};
        for row_focus in [false, true] {
            let keys = if row_focus {
                vec![
                    (
                        keyboard::Key::Named(keyboard::key::Named::Enter),
                        keyboard::key::Code::Enter,
                        keyboard::Modifiers::empty(),
                    ),
                    (
                        keyboard::Key::Named(keyboard::key::Named::Enter),
                        keyboard::key::Code::NumpadEnter,
                        keyboard::Modifiers::CTRL,
                    ),
                    (
                        keyboard::Key::Character(" ".into()),
                        keyboard::key::Code::Space,
                        keyboard::Modifiers::empty(),
                    ),
                ]
            } else {
                vec![(
                    keyboard::Key::Named(keyboard::key::Named::Enter),
                    keyboard::key::Code::Enter,
                    keyboard::Modifiers::empty(),
                )]
            };
            for (key, physical, modifiers) in keys {
                let (mut app, card, _directory) = terminal_palette_fixture();
                app.set_terminal_review_owner(Some(card));
                drop(app.update_palette(Action::Query(">return to terminal input".into())));
                let identity = EntryIdentity::capture(&app.palette_entries()[0].2).unwrap();
                let renderer = crate::app::tests::headless_renderer();
                let mut element = app.palette_surface().unwrap();
                let mut tree = Tree::new(element.as_widget());
                let bounds = Rectangle::with_size(Size::new(880.0, 800.0));
                let node = element.as_widget_mut().layout(
                    &mut tree,
                    &renderer,
                    &layout::Limits::new(Size::ZERO, bounds.size()),
                );
                let id = if row_focus {
                    identity.widget_id()
                } else {
                    widget::Id::new("palette.query")
                };
                element.as_widget_mut().operate(
                    &mut tree,
                    Layout::new(&node),
                    &renderer,
                    &mut operation::focusable::focus::<()>(id),
                );
                let press = keyboard::Event::KeyPressed {
                    key: key.clone(),
                    modified_key: key.clone(),
                    physical_key: keyboard::key::Physical::Code(physical),
                    location: keyboard::Location::Standard,
                    modifiers,
                    text: None,
                    repeat: false,
                };
                let mut messages = Vec::new();
                let mut repeat = press.clone();
                if let keyboard::Event::KeyPressed { repeat, .. } = &mut repeat {
                    *repeat = true;
                }
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Keyboard(repeat.clone()),
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &bounds,
                );
                assert!(
                    matches!(
                        messages.as_slice(),
                        [Message::Palette(Action::ConsumedActivation)]
                    ),
                    "a repeat without its original activation cannot close the palette"
                );
                assert!(
                    !app.terminal_review_input
                        .owns_keyboard(&Event::Keyboard(repeat.clone()))
                );
                messages.clear();
                element.as_widget_mut().update(
                    &mut tree,
                    &Event::Keyboard(press.clone()),
                    Layout::new(&node),
                    mouse::Cursor::Unavailable,
                    &renderer,
                    &mut clipboard::Null,
                    &mut Shell::new(&mut messages),
                    &bounds,
                );
                assert!(
                    app.terminal_review_input
                        .owns_keyboard(&Event::Keyboard(press.clone())),
                    "the actual focused widget records the original activation before publishing"
                );
                drop(element);
                if row_focus {
                    assert!(matches!(
                        messages.as_slice(),
                        [Message::Palette(Action::SubmitFromNative(_))]
                    ));
                    drop(app.update(messages.pop().unwrap()));
                } else {
                    assert!(matches!(
                        messages.as_slice(),
                        [Message::Palette(Action::SubmitFromNative(_))]
                    ));
                    drop(app.update(messages.pop().unwrap()));
                }
                assert_eq!(app.terminal_review_card, None);
                assert!(!app.palette.is_open());
                assert!(app.terminal_review_input.consume_keyboard(&repeat));
                let release = keyboard::Event::KeyReleased {
                    key: key.clone(),
                    modified_key: key.clone(),
                    physical_key: keyboard::key::Physical::Code(physical),
                    location: keyboard::Location::Standard,
                    modifiers,
                };
                assert!(app.terminal_review_input.consume_keyboard(&release));
                assert!(
                    !app.terminal_review_input.consume_keyboard(&press),
                    "fresh input is no longer owned"
                );
            }
        }
    }

    #[test]
    fn queued_terminal_palette_action_rejects_identity_mode_visibility_and_input_owner_changes() {
        for change in 0..8 {
            let (mut app, card, _directory) = terminal_palette_fixture();
            let held = app.palette_entries().pop().unwrap().2;
            match change {
                0 => {
                    if let CardKind::Terminal(session) = &mut app.card_state_mut(card).unwrap().kind
                    {
                        session.id = crate::model::TerminalId::new();
                    }
                }
                1 => app.card_state_mut(card).unwrap().exposure = Exposure::Offscreen,
                2 => app.refresh_ui.focused = false,
                3 => app.device_ui.open = true,
                4 => app.set_terminal_review_owner(Some(card)),
                5 => {
                    app.set_terminal_review_owner(Some(card));
                    app.set_terminal_review_owner(None);
                }
                6 => drop(app.update_palette(Action::Close)),
                7 => app.palette.recording = Some(SettingsField::ShortcutCommandPalette),
                _ => unreachable!(),
            }
            let owner = app.terminal_review_card;
            let revision = app.terminal_review_revision;
            let focused = app.workspace.focused();
            let task = app.update_palette(Action::Run(MessageBox(Box::new(held))));
            assert!(
                iced::runtime::task::into_stream(task).is_none(),
                "case {change}"
            );
            assert_eq!(app.terminal_review_card, owner, "case {change}");
            assert_eq!(app.terminal_review_revision, revision, "case {change}");
            assert_eq!(app.workspace.focused(), focused, "case {change}");
        }
    }

    #[test]
    fn selected_no_op_command_restores_retained_settings_input() {
        use iced::advanced::widget::Tree;

        let mut app = settings_focus_fixture();
        let before = app.workspace.order().collect::<Vec<_>>();
        let renderer = super::super::tests::headless_renderer();
        let mut tree = Tree::new(focus_fixture_view(&app).as_widget());
        let open = app.update_palette(Action::Open);
        let _ = app.update_palette(Action::Query("move pane left".into()));
        assert_eq!(app.palette_entries().len(), 1);
        let _ = focus_fixture_step(&app, &mut tree, &renderer, open, false);
        let submit = app.update_palette(Action::Submit);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), before);
        assert!(!app.palette.is_open());
        let after = focus_fixture_step(&app, &mut tree, &renderer, submit, true);
        assert!(after.iter().any(|message| matches!(
            message, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::SettingsSearchChanged(value) if value == "x")
        )));
    }

    #[test]
    fn palette_command_uses_new_settings_destination_for_restoration() {
        use iced::advanced::widget::Tree;

        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        let original = app.workspace.focused().unwrap();
        let renderer = super::super::tests::headless_renderer();
        let mut tree = Tree::new(focus_fixture_view(&app).as_widget());
        let open = app.update_palette(Action::Open);
        let _ = app.update_palette(Action::Query("open settings".into()));
        assert_eq!(app.palette_entries().len(), 1);
        let _ = focus_fixture_step(&app, &mut tree, &renderer, open, false);
        let submit = app.update_palette(Action::Submit);
        let destination = app.workspace.focused().unwrap();
        assert_ne!(destination, original);
        assert!(matches!(
            app.cards.get(&destination),
            Some(CardRuntime::Settings)
        ));
        let after = focus_fixture_step(&app, &mut tree, &renderer, submit, true);
        assert!(after.iter().any(|message| matches!(
            message, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::SettingsSearchChanged(value) if value == "x")
        )));
    }

    #[test]
    fn palette_command_explicit_rename_focus_wins_over_pane_restoration() {
        use iced::advanced::widget::Tree;

        for click in [false, true] {
            let mut app = settings_focus_fixture();
            let renderer = super::super::tests::headless_renderer();
            let mut tree = Tree::new(focus_fixture_view(&app).as_widget());
            let open = app.update_palette(Action::Open);
            let _ = app.update_palette(Action::Query("rename pane".into()));
            assert_eq!(app.palette_entries().len(), 1);
            let _ = focus_fixture_step(&app, &mut tree, &renderer, open, false);
            let run = if click {
                let (_, _, message) = app.palette_entries().pop().unwrap();
                app.update_palette(Action::Run(MessageBox(Box::new(message))))
            } else {
                app.update_palette(Action::Submit)
            };
            assert!(app.pane_rename_surface().is_some());
            let after = focus_fixture_step(&app, &mut tree, &renderer, run, true);
            assert!(after.iter().any(|message| matches!(
                message, Message::ChromeUi(chrome_ui::Action::RenameChanged(value)) if value == "x"
            )));
            assert!(
                !after
                    .iter()
                    .any(|message| matches!(message, Message::UtilityPaneUi(inner) if matches!(inner.as_ref(), Message::SettingsSearchChanged(_))))
            );
        }
    }

    #[test]
    fn captured_new_tab_shortcut_creates_a_thread_once_and_respects_modals() {
        for captured in [false, true] {
            let mut app = super::super::tests::context_inspector_test_app(true);
            app.context_inspector_open = false;
            let press = |repeat| keyboard::Event::KeyPressed {
                key: keyboard::Key::Character("t".into()),
                modified_key: keyboard::Key::Character("t".into()),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyT),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::CTRL,
                text: None,
                repeat,
            };
            let before = app.workspace.order().count();
            let _ = app.update(Message::KeyEvent {
                event: press(false),
                captured,
            });
            assert_eq!(app.workspace.order().count(), before + 1);
            let card = app.workspace.focused().unwrap();
            assert!(matches!(
                app.card_state(card).unwrap().kind,
                CardKind::Chat(_)
            ));
            let _ = app.update(Message::KeyEvent {
                event: press(true),
                captured,
            });
            assert_eq!(app.workspace.order().count(), before + 1);

            let _ = app.update_palette(Action::Open);
            let _ = app.update(Message::KeyEvent {
                event: press(false),
                captured,
            });
            assert_eq!(app.workspace.order().count(), before + 1);
        }
    }

    #[test]
    fn closing_shortcut_recording_dismisses_the_modal_and_preserves_the_binding() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let before = app
            .settings_draft
            .keyboard_shortcuts
            .command_palette
            .clone();
        let _ = app.update_palette(Action::Record(SettingsField::ShortcutCommandPalette));
        assert!(app.palette.is_open());
        let _ = app.update_palette(Action::Close);
        assert!(!app.palette.is_open());
        assert_eq!(
            app.settings_draft.keyboard_shortcuts.command_palette,
            before
        );
    }
    #[test]
    fn palette_filters_commands_without_changing_workspace() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        let before = app.workspace.order().collect::<Vec<_>>();
        let _ = app.update_palette(Action::Open);
        let _ = app.update_palette(Action::Query("newthr".into()));
        let entries = app.palette_entries();
        assert!(
            entries
                .iter()
                .any(|(label, _, message)| label == "New thread"
                    && matches!(message, Message::RunCommand(Command::NewThread)))
        );
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), before);
        let _ = app.update_palette(Action::Close);
        assert!(!app.palette.is_open());
    }
    #[test]
    fn palette_ranks_useful_matches_and_restores_bounded_recent_commands() {
        assert!(match_score("feature login", "feat") < match_score("alpha-wt-feat", "feat"));
        assert!(match_score("alpha-wt-feat", "feat") < match_score("defeated", "feat"));
        let mut labels = [
            "Investigate task in alpha-wt-refactor-ui",
            "refactor ui · alpha",
            "prepare follow-on refactoring",
        ];
        labels.sort_by_cached_key(|label| match_score(label, "refactor"));
        assert_eq!(labels[0], "refactor ui · alpha");
        let mut app = settings_focus_fixture();
        app.settings.palette_recents = (0..30).map(|i| format!("retired:{i}")).collect();
        for command in [
            Command::NewTerminal,
            Command::OpenSettings,
            Command::OpenSettings,
        ] {
            drop(app.run_palette_command(Message::RunCommand(command)));
        }
        let selected = entry_key(&Message::RunCommand(Command::OpenSettings)).unwrap();
        assert_eq!(app.settings.palette_recents.len(), 24);
        assert_eq!(
            app.settings
                .palette_recents
                .iter()
                .filter(|key| **key == selected)
                .count(),
            1
        );
        drop(app.update_palette(Action::Open));
        let entries = app.palette_entries();
        assert!(matches!(
            entries[0].2,
            Message::RunCommand(Command::OpenSettings)
        ));
        assert!(matches!(
            entries[1].2,
            Message::RunCommand(Command::NewTerminal)
        ));
        for prefix in ['>', '@', '#'] {
            app.palette.query = Some(prefix.to_string());
            assert!(
                app.palette_entries()
                    .iter()
                    .all(|entry| entry_category(&entry.2) == prefix)
            );
        }
    }

    #[test]
    fn palette_finds_inactive_panes_with_workspace_and_unread_status_then_navigates() {
        let mut app = settings_focus_fixture();
        let original = app.workspace.focused().unwrap();
        let mut other = app.card_state(original).unwrap().clone();
        other.id = CardId::new();
        other.title = "Migration agent".into();
        let path = PathBuf::from("/palette-inactive-worktree");
        app.worktree_owners.insert(other.id, path.clone());
        Arc::make_mut(&mut app.durable)
            .card_scopes
            .insert(other.id, path.clone());
        Arc::make_mut(&mut app.durable).cards.push(other.clone());
        app.sidebar_state.observe_agent_update(
            other.id,
            ProjectId::new(),
            crate::model::WorktreeId::new(),
        );
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query("@migration".into())));
        let mut entries = app.palette_entries();
        assert_eq!(entries.len(), 1);
        let (label, status, message) = entries.pop().unwrap();
        assert!(label.contains("palette-inactive-worktree"));
        assert_eq!(status, "• Unread");
        drop(app.run_palette_command(message));
        assert_eq!(app.active_workspace, Some(path));
        assert_eq!(app.workspace.focused(), Some(other.id));
    }

    fn inactive_palette_pane(channel: bool) -> (Cagents, CardId, PathBuf) {
        let mut app = settings_focus_fixture();
        let mut card = app
            .card_state(app.workspace.focused().unwrap())
            .unwrap()
            .clone();
        card.id = CardId::new();
        card.title = "Held pane".into();
        let path = PathBuf::from("/palette-held-workspace");
        let project = ProjectId::new();
        app.sidebar_model.projects.push(
            SidebarProject::new(
                project,
                RepositoryId::new(),
                "Held workspace".into(),
                path.clone(),
                Vec::new(),
                Vec::new(),
            )
            .with_kind(ProjectKind::Folder),
        );
        if channel {
            Arc::make_mut(&mut app.durable)
                .channels
                .push(crate::model::ProjectChannel {
                    id: crate::model::ChannelId::new(),
                    project_id: project,
                    title: "Held channel".into(),
                    title_source: Default::default(),
                    ordinal: 0,
                    shared: false,
                    cards: vec![card.id],
                    focused: Some(card.id),
                });
        } else {
            app.worktree_owners.insert(card.id, path.clone());
            Arc::make_mut(&mut app.durable)
                .card_scopes
                .insert(card.id, path.clone());
        }
        let id = card.id;
        Arc::make_mut(&mut app.durable).cards.push(card);
        (app, id, path)
    }

    #[test]
    fn held_palette_pane_action_cannot_change_scope_after_actual_update_prepare() {
        for channel in [false, true] {
            let (mut app, target, path) = inactive_palette_pane(channel);
            drop(app.update(Message::Palette(Action::Open)));
            drop(app.update(Message::Palette(Action::Query("@held pane".into()))));
            let (_, _, selected) = app.palette_entries().into_iter().find(|entry| {
                matches!(entry.2, Message::Palette(Action::FocusPane(id)) if id == target)
            }).unwrap();
            let held = Message::Palette(Action::Run(MessageBox(Box::new(selected))));
            let before = (
                app.active_workspace.clone(),
                app.durable.active_channel,
                app.workspace.order().collect::<Vec<_>>(),
                app.workspace.focused(),
                app.settings.palette_recents.clone(),
            );
            let (response, _) = std::sync::mpsc::sync_channel(1);
            drop(app.update(Message::UiUpdate(instance::UiUpdateRequest {
                action: instance::UiUpdateAction::Prepare,
                response,
            })));
            assert!(
                app.update_prepared,
                "real Prepare admitted the palette-open window"
            );
            drop(app.update(held.clone()));
            assert_eq!(
                (
                    app.active_workspace.clone(),
                    app.durable.active_channel,
                    app.workspace.order().collect::<Vec<_>>(),
                    app.workspace.focused(),
                    app.settings.palette_recents.clone()
                ),
                before
            );
            assert_eq!(app.palette.query.as_deref(), Some("@held pane"));
            let (response, receiver) = std::sync::mpsc::sync_channel(1);
            drop(app.update(Message::UiUpdate(instance::UiUpdateRequest {
                action: instance::UiUpdateAction::Cancel,
                response,
            })));
            assert_eq!(receiver.try_recv().unwrap(), "ok");
            drop(app.update(held));
            assert_eq!(app.active_workspace.as_ref(), Some(&path));
            assert_eq!(app.workspace.focused(), Some(target));
        }
    }

    #[test]
    fn held_palette_pane_action_refuses_deleting_workspaces_and_closing_projects() {
        for channel in [false, true] {
            for closing in [false, true] {
                let (mut app, target, path) = inactive_palette_pane(channel);
                let before = (
                    app.active_workspace.clone(),
                    app.durable.active_channel,
                    app.workspace.order().collect::<Vec<_>>(),
                    app.workspace.focused(),
                );
                if closing {
                    let project = app.sidebar_model.projects.last().unwrap().id;
                    app.project_ui.closing.insert(project, path.clone());
                } else {
                    app.project_ui.begin_deleting(path);
                }
                drop(app.update(Message::Palette(Action::Run(MessageBox(Box::new(
                    Message::Palette(Action::FocusPane(target)),
                ))))));
                assert_eq!(
                    (
                        app.active_workspace.clone(),
                        app.durable.active_channel,
                        app.workspace.order().collect::<Vec<_>>(),
                        app.workspace.focused()
                    ),
                    before
                );
                let warning = TransientNotice::warning(if closing {
                    "That project is being removed."
                } else {
                    "That worktree is being deleted."
                });
                assert!(app.status.is_none(), "actual update drained the warning");
                assert!(app.notices.recent().any(|entry| entry.notice == warning));
            }
        }
    }

    #[test]
    fn captured_pane_focus_refuses_a_closed_target_and_a_modal() {
        let mut app = settings_focus_fixture();
        let focused = app.workspace.focused();
        let mut other = app.card_state(focused.unwrap()).unwrap().clone();
        other.id = CardId::new();
        Arc::make_mut(&mut app.durable).cards.push(other.clone());
        app.resume_ui.open = true;
        drop(app.update_palette(Action::FocusPane(other.id)));
        assert_eq!(app.workspace.focused(), focused);
        app.resume_ui.open = false;
        app.card_state_mut(other.id).unwrap().existence = project_ui::removed_existence("closed");
        drop(app.update_palette(Action::FocusPane(other.id)));
        assert_eq!(app.workspace.focused(), focused);
    }

    #[test]
    fn pane_rename_survives_runtime_title_updates() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        let id = app.workspace.focused().unwrap();
        let _ = app.update_chrome(chrome_ui::Action::Rename(id));
        let _ = app.update_chrome(chrome_ui::Action::RenameChanged("Migration agent".into()));
        let _ = app.update_chrome(chrome_ui::Action::RenameSubmit);
        assert!(!app.apply_reported_title(id, "fish /some/path".into()));
        assert_eq!(app.card_state(id).unwrap().title, "Migration agent");
    }
    #[test]
    fn card_index_tolerates_insertions_before_the_cached_position() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        let id = app.workspace.focused().unwrap();
        let _ = app.sync_chrome();
        let mut other = app.card_state(id).unwrap().clone();
        other.id = CardId::new();
        Arc::make_mut(&mut app.durable).cards.insert(0, other);
        assert_eq!(app.card_state(id).unwrap().id, id);
        assert_eq!(app.card_state_mut(id).unwrap().id, id);
    }
    #[test]
    fn move_picker_ranks_destinations_and_keeps_category_prefixes_literal() {
        let mut app = settings_focus_fixture();
        let card = app.workspace.focused().unwrap();
        app.palette.move_pane = Some(card);
        let first = crate::model::ChannelId::new();
        let exact = crate::model::ChannelId::new();
        let entry = |label: &str, channel| {
            (
                format!("Move pane to channel: {label}"),
                String::new(),
                Message::MovePaneToChannel { card, channel },
            )
        };
        app.palette.query = Some("refactor".into());
        let entries = app.filter_palette_entries(vec![
            entry("alpha-wt-refactor-ui", first),
            entry("refactor", exact),
        ]);
        assert_eq!(entries.len(), 2);
        assert!(
            matches!(entries[0].2, Message::MovePaneToChannel { card: id, channel }
            if id == card && channel == exact)
        );
        app.palette.query = Some("#release".into());
        let entries =
            app.filter_palette_entries(vec![entry("#release", exact), entry("release", first)]);
        assert_eq!(entries.len(), 1);
        assert!(
            matches!(entries[0].2, Message::MovePaneToChannel { card: id, channel }
            if id == card && channel == exact)
        );
        assert_eq!(
            palette_mode_query("#release", false),
            (Some('#'), "release")
        );
        assert_eq!(palette_mode_query("#release", true), (None, "#release"));
        app.palette.move_pane = None;
        app.palette.query = Some(">move".into());
        let entries = app.filter_palette_entries(vec![entry("refactor", exact)]);
        assert_eq!(
            entries.len(),
            1,
            "normal command search must retain the move verb"
        );
        assert!(
            matches!(entries[0].2, Message::MovePaneToChannel { card: id, channel }
            if id == card && channel == exact)
        );
    }

    #[test]
    fn palette_pane_labels_preserve_channel_title_precedence_across_live_renames() {
        let (mut app, primary, path) = inactive_palette_pane(true);
        let provider = crate::agent::Provider::Omp;
        let session = crate::chat::Session {
            id: crate::model::TerminalId::new(),
            launch: crate::chat::Launch {
                provider,
                recipe: crate::model::LaunchRecipe {
                    program: "/bin/sh".into(),
                    arguments: Vec::new(),
                    working_directory: path.clone(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: None,
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let mut secondary = app.card_state(primary).unwrap().clone();
        secondary.id = CardId::new();
        secondary.title = chat_ui::chat_card_title(provider).into();
        secondary.kind = CardKind::Chat(session.clone());
        let utility = app.workspace.focused().unwrap();
        let durable = Arc::make_mut(&mut app.durable);
        let card = durable
            .cards
            .iter_mut()
            .find(|card| card.id == primary)
            .unwrap();
        card.title = "Provider suggestion".into();
        card.kind = CardKind::Chat(session);
        durable.cards.push(secondary.clone());
        // The empty first channel owns the place label; the first nonempty
        // channel supplies the primary manual title and the secondary default.
        durable.channels[0].title.clear();
        durable.channels[0].cards.push(secondary.id);
        let mut named = durable.channels[0].clone();
        named.id = crate::model::ChannelId::new();
        named.title = "Investigation".into();
        named.title_source = crate::model::ChannelTitleSource::Manual;
        named.cards = vec![utility, primary, secondary.id];
        durable.channels.push(named.clone());
        named.id = crate::model::ChannelId::new();
        named.title = "Later membership".into();
        durable.channels.push(named);
        app.worktree_owners.insert(primary, "/ignored-owner".into());
        let labels = |app: &Cagents| {
            app.palette_pane_entries()
                .into_iter()
                .filter_map(|(label, _, message)| match message {
                    Message::Palette(Action::FocusPane(id))
                        if id == primary || id == secondary.id =>
                    {
                        Some((id, label))
                    }
                    _ => None,
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(
            labels(&app),
            vec![
                (
                    primary,
                    "Focus pane: Investigation ·  · Held workspace".into()
                ),
                (
                    secondary.id,
                    "Focus pane: Investigation ·  · Held workspace".into(),
                ),
            ]
        );
        Arc::make_mut(&mut app.durable).channels[1].title = "Renamed".into();
        app.chrome_ui
            .user_titles
            .insert(secondary.id, "Local title".into());
        assert_eq!(
            labels(&app),
            vec![
                (primary, "Focus pane: Renamed ·  · Held workspace".into()),
                (
                    secondary.id,
                    "Focus pane: Local title ·  · Held workspace".into(),
                ),
            ]
        );
    }

    fn palette_pointer_step(
        app: &Cagents,
        tree: &mut iced::advanced::widget::Tree,
        renderer: &cosmic::Renderer,
        target: &EntryIdentity,
        event: iced::mouse::Event,
        point: Option<iced::Point>,
    ) -> (iced::Point, Vec<Message>) {
        use iced::advanced::{Layout, Shell, clipboard, layout, widget::Operation};
        use iced::{Event, Rectangle, Size, mouse};
        struct Bounds {
            id: widget::Id,
            found: Option<Rectangle>,
        }
        impl Operation for Bounds {
            fn traverse(&mut self, operate: &mut dyn FnMut(&mut dyn Operation)) {
                operate(self);
            }
            fn focusable(
                &mut self,
                id: Option<&widget::Id>,
                bounds: Rectangle,
                _state: &mut dyn iced::advanced::widget::operation::Focusable,
            ) {
                if id == Some(&self.id) {
                    self.found = Some(bounds);
                }
            }
        }
        let mut element = app.palette_surface().unwrap();
        tree.diff(element.as_widget_mut());
        let viewport = Rectangle::with_size(Size::new(1000.0, 1600.0));
        let node = element.as_widget_mut().layout(
            tree,
            renderer,
            &layout::Limits::new(Size::ZERO, viewport.size()),
        );
        let mut bounds = Bounds {
            id: target.widget_id(),
            found: None,
        };
        element
            .as_widget_mut()
            .operate(tree, Layout::new(&node), renderer, &mut bounds);
        let bounds = bounds
            .found
            .expect("actual production row is a native focusable button");
        assert!((bounds.height - ROW_HEIGHT).abs() < 0.5);
        let point = point.unwrap_or_else(|| bounds.center());
        assert!(
            bounds.contains(point),
            "held pointer must remain at the same row position"
        );
        let mut messages = Vec::new();
        element.as_widget_mut().update(
            tree,
            &Event::Mouse(event),
            Layout::new(&node),
            mouse::Cursor::Available(point),
            renderer,
            &mut clipboard::Null,
            &mut Shell::new(&mut messages),
            &viewport,
        );
        (point, messages)
    }

    #[test]
    fn palette_row_accessibility_keeps_actionable_full_labels_after_target_reset() {
        use iced::advanced::{Layout, layout, widget::Tree};
        use iced::{Size, mouse};
        use iced_accessibility::accesskit::{Action as A11yAction, Role};
        fn accessible(
            app: &Cagents,
            tree: &mut Tree,
            renderer: &cosmic::Renderer,
        ) -> iced_accessibility::A11yTree {
            let mut element = app.palette_surface().unwrap();
            tree.diff(element.as_widget_mut());
            let node = element.as_widget_mut().layout(
                tree,
                renderer,
                &layout::Limits::new(Size::ZERO, Size::new(1000.0, 1600.0)),
            );
            element
                .as_widget()
                .a11y_nodes(Layout::new(&node), tree, mouse::Cursor::Unavailable)
        }
        fn assert_button(tree: &iced_accessibility::A11yTree, card: CardId, label: &str) {
            let expected: iced_accessibility::A11yId = EntryIdentity::Pane(card).widget_id().into();
            let node = tree
                .root()
                .iter()
                .chain(tree.children())
                .find(|node| iced_accessibility::IdEq::eq(node.id(), &expected))
                .expect("actual result button remains accessible");
            assert_eq!(node.node().role(), Role::Button);
            assert!(node.node().supports_action(A11yAction::Click));
            assert!(node.node().supports_action(A11yAction::Focus));
            assert!(
                node.node()
                    .label()
                    .is_some_and(|actual| actual.contains(label))
            );
        }
        let (mut app, original, path) = inactive_palette_pane(false);
        app.card_state_mut(original).unwrap().title =
            "Archive held agent with a complete accessible name".into();
        let mut other = app.card_state(original).unwrap().clone();
        other.id = CardId::new();
        other.title = "Unrelated archive held agent".into();
        let replacement = other.id;
        app.worktree_owners.insert(replacement, path.clone());
        Arc::make_mut(&mut app.durable)
            .card_scopes
            .insert(replacement, path);
        Arc::make_mut(&mut app.durable).cards.push(other);
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query("@held".into())));
        assert!(
            matches!(app.palette_entries()[0].2, Message::Palette(Action::FocusPane(id)) if id == original)
        );
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(app.palette_surface().unwrap().as_widget());
        let before = accessible(&app, &mut tree, &renderer);
        assert_button(
            &before,
            original,
            "Archive held agent with a complete accessible name",
        );
        assert_button(&before, replacement, "Unrelated archive held agent");
        app.card_state_mut(replacement).unwrap().title = "Held new agent".into();
        assert_eq!(app.palette_entries().len(), 2);
        assert!(
            matches!(app.palette_entries()[0].2, Message::Palette(Action::FocusPane(id)) if id == replacement)
        );
        let after = accessible(&app, &mut tree, &renderer);
        assert_button(
            &after,
            original,
            "Archive held agent with a complete accessible name",
        );
        assert_button(&after, replacement, "Held new agent");
    }

    #[test]
    fn native_palette_press_cannot_transfer_to_another_target_at_the_same_row() {
        use iced::advanced::widget::Tree;
        use iced::mouse::{Button, Event};
        let (mut app, original, path) = inactive_palette_pane(false);
        app.card_state_mut(original).unwrap().title = "Archive held agent".into();
        let mut other = app.card_state(original).unwrap().clone();
        other.id = CardId::new();
        other.title = "Unrelated archive held agent".into();
        let replacement = other.id;
        app.worktree_owners.insert(replacement, path.clone());
        Arc::make_mut(&mut app.durable)
            .card_scopes
            .insert(replacement, path);
        Arc::make_mut(&mut app.durable).cards.push(other);
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query("@held".into())));
        assert!(
            matches!(app.palette_entries()[0].2, Message::Palette(Action::FocusPane(id)) if id == original)
        );
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(app.palette_surface().unwrap().as_widget());
        let (point, messages) = palette_pointer_step(
            &app,
            &mut tree,
            &renderer,
            &EntryIdentity::Pane(original),
            Event::ButtonPressed(Button::Left),
            None,
        );
        assert!(messages.is_empty());
        app.card_state_mut(replacement).unwrap().title = "Held new agent".into();
        assert_eq!(
            app.palette_entries().len(),
            2,
            "equal child count must exercise positional reuse"
        );
        assert!(
            matches!(app.palette_entries()[0].2, Message::Palette(Action::FocusPane(id)) if id == replacement)
        );
        let (_, messages) = palette_pointer_step(
            &app,
            &mut tree,
            &renderer,
            &EntryIdentity::Pane(replacement),
            Event::ButtonReleased(Button::Left),
            Some(point),
        );
        assert!(
            messages.is_empty(),
            "old press must not activate the rebuilt replacement callback"
        );
        let (_, messages) = palette_pointer_step(
            &app,
            &mut tree,
            &renderer,
            &EntryIdentity::Pane(replacement),
            Event::ButtonPressed(Button::Left),
            Some(point),
        );
        assert!(messages.is_empty());
        let (_, messages) = palette_pointer_step(
            &app,
            &mut tree,
            &renderer,
            &EntryIdentity::Pane(replacement),
            Event::ButtonReleased(Button::Left),
            Some(point),
        );
        assert!(
            matches!(messages.as_slice(), [Message::Palette(Action::Run(MessageBox(message)))]
            if matches!(message.as_ref(), Message::Palette(Action::FocusPane(id)) if *id == replacement))
        );
    }

    #[test]
    fn native_palette_press_survives_status_refresh_of_the_same_target() {
        use iced::advanced::widget::Tree;
        use iced::mouse::{Button, Event};
        let (mut app, target, _) = inactive_palette_pane(false);
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query("@held".into())));
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(app.palette_surface().unwrap().as_widget());
        let (point, messages) = palette_pointer_step(
            &app,
            &mut tree,
            &renderer,
            &EntryIdentity::Pane(target),
            Event::ButtonPressed(Button::Left),
            None,
        );
        assert!(messages.is_empty());
        app.sidebar_state.observe_agent_update(
            target,
            ProjectId::new(),
            crate::model::WorktreeId::new(),
        );
        assert_eq!(app.palette_entries()[0].1, "• Unread");
        let (_, messages) = palette_pointer_step(
            &app,
            &mut tree,
            &renderer,
            &EntryIdentity::Pane(target),
            Event::ButtonReleased(Button::Left),
            Some(point),
        );
        assert!(
            matches!(messages.as_slice(), [Message::Palette(Action::Run(MessageBox(message)))]
            if matches!(message.as_ref(), Message::Palette(Action::FocusPane(id)) if *id == target))
        );
    }

    #[test]
    fn keyboard_selection_keeps_its_pane_when_live_results_rank_ahead() {
        let (mut app, target, path) = inactive_palette_pane(false);
        app.card_state_mut(target).unwrap().title = "Archive held agent".into();
        app.refresh_ui.focused = true;
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query("@held".into())));
        assert_eq!(app.palette.selected, Some(EntryIdentity::Pane(target)));
        let mut fresh = app.card_state(target).unwrap().clone();
        fresh.id = CardId::new();
        fresh.title = "Held new agent".into();
        let fresh_id = fresh.id;
        app.worktree_owners.insert(fresh_id, path.clone());
        let durable = Arc::make_mut(&mut app.durable);
        durable.card_scopes.insert(fresh_id, path.clone());
        durable.cards.push(fresh);
        let entries = app.palette_entries();
        assert!(matches!(entries[0].2, Message::Palette(Action::FocusPane(id)) if id == fresh_id));
        assert_eq!(app.selected_palette_index(&entries), Some(1));
        drop(native_palette_enter(&mut app));
        assert_eq!(app.active_workspace.as_ref(), Some(&path));
        assert_eq!(app.workspace.focused(), Some(target));
        assert_eq!(app.settings.palette_recents[0], format!("pane:{target}"));
    }

    #[test]
    fn keyboard_selection_refuses_a_removed_pane_and_a_replaced_workspace_path() {
        for workspace_path_changed in [false, true] {
            let (mut app, target, _) = inactive_palette_pane(false);
            app.refresh_ui.focused = true;
            drop(app.update_palette(Action::Open));
            drop(app.update_palette(Action::Query(if workspace_path_changed {
                "#heldworkspace".into()
            } else {
                "@held".into()
            })));
            assert!(app.palette.selected.is_some());
            let before = (
                app.active_workspace.clone(),
                app.workspace.focused(),
                app.settings.palette_recents.clone(),
            );
            if workspace_path_changed {
                app.sidebar_model.projects.last_mut().unwrap().repository =
                    PathBuf::from("/replacement-held-workspace");
            } else {
                let mut fresh = app.card_state(target).unwrap().clone();
                fresh.id = CardId::new();
                Arc::make_mut(&mut app.durable)
                    .cards
                    .retain(|card| card.id != target);
                Arc::make_mut(&mut app.durable).cards.push(fresh);
            }
            assert!(
                !app.palette_entries().is_empty(),
                "another live row must not inherit selection"
            );
            drop(native_palette_enter(&mut app));
            assert_eq!(
                (
                    app.active_workspace.clone(),
                    app.workspace.focused(),
                    app.settings.palette_recents.clone()
                ),
                before
            );
            assert!(app.palette.is_open());
            let expected = TransientNotice::warning(
                "That palette result is no longer available. Choose another result.",
            );
            assert!(
                app.notices
                    .recent()
                    .any(|notice| notice.notice.message == expected.message)
            );
        }
    }

    #[test]
    fn worktree_selection_keeps_project_worktree_and_exact_path() {
        let project_id = ProjectId::new();
        let worktree_id = crate::model::WorktreeId::new();
        let original = Message::Sidebar(SidebarAction::OpenWorktree {
            project_id,
            worktree_id,
            path: PathBuf::from("/original-worktree"),
        });
        let selected = EntryIdentity::capture(&original).unwrap();
        assert!(selected.matches(&original));
        for (project_id, worktree_id, path) in [
            (ProjectId::new(), worktree_id, "/original-worktree"),
            (
                project_id,
                crate::model::WorktreeId::new(),
                "/original-worktree",
            ),
            (project_id, worktree_id, "/replacement-worktree"),
        ] {
            assert!(
                !selected.matches(&Message::Sidebar(SidebarAction::OpenWorktree {
                    project_id,
                    worktree_id,
                    path: PathBuf::from(path),
                }))
            );
        }
    }

    #[test]
    fn palette_pane_navigation_preserves_scope_when_original_protection_is_refused() {
        for blocked in [true, false] {
            let destination = tempfile::tempdir().unwrap();
            let runtime = tempfile::tempdir().unwrap();
            let path = destination.path().canonicalize().unwrap();
            let (mut app, target, _) = inactive_palette_pane(false);
            app.sidebar_model.projects.last_mut().unwrap().repository = path.clone();
            app.worktree_owners.insert(target, path.clone());
            Arc::make_mut(&mut app.durable)
                .card_scopes
                .insert(target, path.clone());
            app.worktree_protections
                .use_fixture_runtime(runtime.path().into());
            let exclusive = blocked.then(|| {
                crate::worktree_cleanup::fixture_protection(runtime.path(), &path, true).unwrap()
            });
            let before = (
                app.active_workspace.clone(),
                app.durable.active_channel,
                app.workspace.order().collect::<Vec<_>>(),
                app.workspace.focused(),
            );
            drop(app.update(Message::Palette(Action::FocusPane(target))));
            if blocked {
                assert_eq!(
                    (
                        app.active_workspace.clone(),
                        app.durable.active_channel,
                        app.workspace.order().collect::<Vec<_>>(),
                        app.workspace.focused()
                    ),
                    before
                );
                assert!(
                    app.notices
                        .recent()
                        .any(|entry| entry.notice.message.contains("Cannot open this worktree"))
                );
            } else {
                assert_eq!(app.active_workspace.as_ref(), Some(&path));
                assert_eq!(app.workspace.focused(), Some(target));
                assert!(
                    crate::worktree_cleanup::fixture_protection(runtime.path(), &path, true)
                        .is_err(),
                    "the admitted destination keeps its actual shared protection"
                );
            }
            drop(exclusive);
        }
    }

    fn diagnostic_palette_fixture() -> (Cagents, CardId) {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        // Diagnostics are already-delivered fixture metadata. No server is
        // configured and every returned Task is dropped.
        app.settings.language_servers.clear();
        app.lsp = LspManager::new(Vec::new());
        let card = app.workspace.focused().unwrap();
        let (mut runtime, _) = repository_widget::RepositoryRuntime::open_fixture(
            card,
            "notes.txt",
            "first\nsecond\nthird\n",
        );
        runtime.set_editor_diagnostics(
            [1, 2]
                .into_iter()
                .map(|line| {
                    crate::lsp::LspDiagnostic::new(
                        crate::lsp::LspRange::new(
                            crate::lsp::LspPosition::new(line, 0),
                            crate::lsp::LspPosition::new(line, 3),
                        ),
                        crate::lsp::LspSeverity::Warning,
                        "Fixture diagnostic".into(),
                        None,
                        None,
                    )
                })
                .collect(),
        );
        assert!(runtime.move_cursor_to(crate::lsp::LspPosition::new(0, 0)));
        app.card_state_mut(card).unwrap().kind = CardKind::Repository(runtime.descriptor.clone());
        app.cards
            .insert(card, CardRuntime::Repository(Box::new(runtime)));
        (app, card)
    }

    fn diagnostic_cursor(app: &Cagents, card: CardId) -> Option<crate::lsp::LspPosition> {
        let Some(CardRuntime::Repository(runtime)) = app.cards.get(&card) else {
            panic!("fixture editor is retained")
        };
        runtime.cursor_position()
    }

    fn click_diagnostic_palette_row(app: &Cagents, card: CardId, forward: bool) -> Message {
        use iced::advanced::widget::Tree;
        use iced::mouse::{Button, Event};
        let renderer = crate::app::tests::headless_renderer();
        let identity = EntryIdentity::Diagnostic(card, forward);
        let mut tree = Tree::new(app.palette_surface().unwrap().as_widget());
        let (point, messages) = palette_pointer_step(
            app,
            &mut tree,
            &renderer,
            &identity,
            Event::ButtonPressed(Button::Left),
            None,
        );
        assert!(messages.is_empty());
        let (_, mut messages) = palette_pointer_step(
            app,
            &mut tree,
            &renderer,
            &identity,
            Event::ButtonReleased(Button::Left),
            Some(point),
        );
        assert!(matches!(messages.as_slice(),
            [Message::Palette(Action::Run(MessageBox(message)))]
            if matches!(message.as_ref(), Message::Repository(RepositoryAction::Language {
                card: actual,
                action: repository_widget::EditorLspAction::Diagnostic(direction),
            }) if *actual == card && *direction == forward)));
        messages.pop().unwrap()
    }

    #[test]
    fn diagnostic_palette_rows_mount_and_pointer_and_submit_navigate_the_exact_editor_direction() {
        let (mut app, card) = diagnostic_palette_fixture();
        for (query, forward, line) in [
            ("Next diagnostic", true, 1),
            ("Previous diagnostic", false, 2),
        ] {
            app.refresh_ui.focused = true;
            drop(app.update_palette(Action::Open));
            drop(app.update_palette(Action::Query(format!(">{query}"))));
            assert_eq!(
                app.palette.selected,
                Some(EntryIdentity::Diagnostic(card, forward))
            );
            let message = click_diagnostic_palette_row(&app, card, forward);
            drop(app.update(message));
            assert!(!app.palette.is_open());
            assert_eq!(
                diagnostic_cursor(&app, card),
                Some(crate::lsp::LspPosition::new(line, 0))
            );
        }
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query(">Next diagnostic".into())));
        drop(native_palette_enter(&mut app));
        assert!(!app.palette.is_open());
        assert_eq!(
            diagnostic_cursor(&app, card),
            Some(crate::lsp::LspPosition::new(1, 0))
        );
    }

    #[test]
    fn main_work_hides_diagnostics_and_refuses_queued_rows_until_conversations_returns() {
        let (mut app, card) = diagnostic_palette_fixture();
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query(">Next diagnostic".into())));
        let queued = click_diagnostic_palette_row(&app, card, true);
        drop(app.update_palette(Action::Close));
        let channel = crate::model::ChannelId::new();
        app.workspace_model.identity = uuid::Uuid::new_v4();
        app.workspace_model
            .channels
            .push(crate::workspace_model::ModelChannel {
                id: channel,
                repository: "/repo".into(),
                title: "Diagnostic fixture".into(),
                title_source: Default::default(),
                ordinal: 1,
            });
        drop(app.open_channel_work(channel));
        assert!(app.channel_work_is_main());
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query(">diagnostic".into())));
        assert!(app.palette_entries().is_empty());
        assert_eq!(app.palette.selected, None);
        drop(app.update(Message::Palette(Action::Submit)));
        drop(app.update(queued));
        assert!(app.channel_work_is_main());
        assert_eq!(
            diagnostic_cursor(&app, card),
            Some(crate::lsp::LspPosition::new(0, 0))
        );

        let owner = app.channel_work.view.owner().unwrap();
        drop(app.update_channel_work(crate::channel_work_ui::Action::Close(owner)));
        assert!(!app.channel_work_is_main());
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query(">Next diagnostic".into())));
        let message = click_diagnostic_palette_row(&app, card, true);
        drop(app.update(message));
        assert_eq!(
            diagnostic_cursor(&app, card),
            Some(crate::lsp::LspPosition::new(1, 0))
        );
    }

    #[test]
    fn work_board_entry_remains_an_actionable_typed_palette_row() {
        use iced::advanced::widget::Tree;
        use iced::mouse::{Button, Event};
        let mut app = settings_focus_fixture();
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query(">work board".into())));
        let entries = app.palette_entries();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].0, "Open Work board");
        assert!(EntryIdentity::Workboard.matches(&entries[0].2));
        assert_eq!(app.palette.selected, Some(EntryIdentity::Workboard));
        let renderer = crate::app::tests::headless_renderer();
        let mut tree = Tree::new(app.palette_surface().unwrap().as_widget());
        let (point, messages) = palette_pointer_step(
            &app,
            &mut tree,
            &renderer,
            &EntryIdentity::Workboard,
            Event::ButtonPressed(Button::Left),
            None,
        );
        assert!(messages.is_empty());
        let (_, messages) = palette_pointer_step(
            &app,
            &mut tree,
            &renderer,
            &EntryIdentity::Workboard,
            Event::ButtonReleased(Button::Left),
            Some(point),
        );
        assert!(matches!(messages.as_slice(),
            [Message::Palette(Action::Run(MessageBox(message)))]
            if matches!(message.as_ref(), Message::Sidebar(SidebarAction::OpenWork))));
        drop(app.update(messages.into_iter().next().unwrap()));
        assert!(!app.palette.is_open());
        assert_eq!(
            app.settings.palette_recents.first().map(String::as_str),
            Some("command:open_work"),
        );
        drop(app.update_palette(Action::Open));
        let entries = app.palette_entries();
        assert!(EntryIdentity::Workboard.matches(&entries[0].2));
        assert_eq!(app.palette.selected, Some(EntryIdentity::Workboard));
    }
    #[derive(Default, Debug)]
    struct NativePaletteGeometry {
        focused: Option<(widget::Id, iced::Rectangle)>,
        scroll: Option<(iced::Rectangle, iced::Rectangle, iced::Vector)>,
    }
    impl iced::advanced::widget::Operation for NativePaletteGeometry {
        fn traverse(&mut self, visit: &mut dyn FnMut(&mut dyn iced::advanced::widget::Operation)) {
            visit(self);
        }
        fn focusable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            state: &mut dyn iced::advanced::widget::operation::Focusable,
        ) {
            if state.is_focused()
                && let Some(id) = id
            {
                self.focused = Some((id.clone(), bounds));
            }
        }
        fn scrollable(
            &mut self,
            id: Option<&widget::Id>,
            bounds: iced::Rectangle,
            content: iced::Rectangle,
            translation: iced::Vector,
            _: &mut dyn iced::advanced::widget::operation::Scrollable,
        ) {
            if id == Some(&widget::Id::new("palette.commands")) {
                self.scroll = Some((bounds, content, translation));
            }
        }
    }
    fn native_palette_geometry(
        ui: &mut iced::runtime::UserInterface<'_, Message, cosmic::Theme, cosmic::Renderer>,
        renderer: &cosmic::Renderer,
    ) -> NativePaletteGeometry {
        let mut geometry = NativePaletteGeometry::default();
        ui.operate(renderer, &mut geometry);
        geometry
    }
    fn assert_native_row_ring_visible(geometry: &NativePaletteGeometry, expected: &widget::Id) {
        let (focused, row) = geometry.focused.as_ref().expect("native focus exists");
        assert_eq!(focused, expected);
        let (viewport, _, translation) = geometry.scroll.expect("actual results viewport");
        let ring = iced::Rectangle {
            x: row.x - translation.x - ROW_FOCUS_OUTSET,
            y: row.y - translation.y - ROW_FOCUS_OUTSET,
            width: row.width + 2.0 * ROW_FOCUS_OUTSET,
            height: row.height + 2.0 * ROW_FOCUS_OUTSET,
        };
        // Tiny floating point tolerance; test the full drawn outline, not only
        // its button's unscrolled content-layout rectangle.
        let epsilon = 0.01;
        assert!(
            ring.x >= viewport.x - epsilon
                && ring.y >= viewport.y - epsilon
                && ring.x + ring.width <= viewport.x + viewport.width + epsilon
                && ring.y + ring.height <= viewport.y + viewport.height + epsilon,
            "focused ring must be visible: {geometry:?}; ring={ring:?}"
        );
    }
    fn long_native_palette_fixture() -> Cagents {
        let mut app = branch_palette_fixture();
        drop(app.update_palette(Action::Close));
        let original = app
            .card_state(app.workspace.focused().unwrap())
            .unwrap()
            .clone();
        for index in 1..34 {
            let mut card = original.clone();
            card.id = CardId::new();
            card.title = format!("Focus fixture {index:02}");
            app.cards.insert(card.id, CardRuntime::Settings);
            Arc::make_mut(&mut app.durable).cards.push(card);
        }
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query("@".into())));
        assert_eq!(app.palette_entries().len(), 34);
        app
    }

    #[test]
    fn native_long_palette_rows_reveal_the_entire_focus_ring_before_batched_activation() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use iced::{
            Size,
            advanced::{clipboard, widget::operation},
            mouse,
        };
        use keyboard::key::{Code, Named, Physical};
        for width in [360.0, 880.0] {
            for direction in ["down", "up", "end", "home", "tab", "shift-tab"] {
                let mut app = long_native_palette_fixture();
                let identities: Vec<_> = app
                    .palette_entries()
                    .iter()
                    .map(|(_, _, m)| EntryIdentity::capture(m).unwrap())
                    .collect();
                let last = identities.len() - 1;
                let (named, code, count, start, expected) = match direction {
                    "down" => (
                        Named::ArrowDown,
                        Code::ArrowDown,
                        20,
                        identities[0].widget_id(),
                        20,
                    ),
                    "up" => (
                        Named::ArrowUp,
                        Code::ArrowUp,
                        20,
                        identities[last].widget_id(),
                        last - 20,
                    ),
                    "end" => (Named::End, Code::End, 1, identities[0].widget_id(), last),
                    "home" => (Named::Home, Code::Home, 1, identities[last].widget_id(), 0),
                    "tab" => (
                        Named::Tab,
                        Code::Tab,
                        21,
                        widget::Id::new("palette.query"),
                        20,
                    ),
                    _ => (
                        Named::Tab,
                        Code::Tab,
                        12,
                        widget::Id::new("palette.query"),
                        last - 11,
                    ),
                };
                let mut renderer = crate::app::tests::headless_renderer();
                let mut ui = iced::runtime::UserInterface::build(
                    app.palette_surface().unwrap(),
                    Size::new(width, 700.0),
                    iced::runtime::user_interface::Cache::default(),
                    &mut renderer,
                );
                ui.operate(&renderer, &mut operation::focusable::focus::<()>(start));
                if direction == "home" || direction == "up" {
                    ui.operate(
                        &renderer,
                        &mut operation::scrollable::scroll_to::<()>(
                            widget::Id::new("palette.commands"),
                            operation::scrollable::AbsoluteOffset {
                                x: None,
                                y: Some(f32::MAX),
                            },
                        ),
                    );
                }
                let before = native_palette_geometry(&mut ui, &renderer);
                let (viewport, content, _) = before.scroll.unwrap();
                assert!(
                    viewport.height <= 360.0
                        && content.height > viewport.height + 10.0 * ROW_HEIGHT
                );
                let cycle = PressCycle::new(Physical::Code(code));
                let source = Source::tracked(cycle, Transition::Press);
                let mut events: Vec<_> = (0..count)
                    .map(|index| {
                        let mut key = cycle_key(named, code, true, index > 0);
                        if direction == "shift-tab"
                            && let keyboard::Event::KeyPressed { modifiers, .. } = &mut key
                        {
                            *modifiers = keyboard::Modifiers::SHIFT;
                        }
                        iced::Event::Keyboard(key)
                    })
                    .collect();
                let mut sources = vec![source; count];
                events.push(iced::Event::Keyboard(cycle_key(
                    Named::Enter,
                    Code::Enter,
                    true,
                    false,
                )));
                sources.push(Source::tracked(
                    PressCycle::new(Physical::Code(Code::Enter)),
                    Transition::Press,
                ));
                let mut messages = Vec::new();
                let (_, statuses) = ui.update_with_sources(
                    &events,
                    &sources,
                    mouse::Cursor::Unavailable,
                    &mut renderer,
                    &mut clipboard::Null,
                    &mut messages,
                );
                assert_eq!(
                    statuses,
                    vec![iced::event::Status::Captured; events.len()],
                    "{direction}, width={width}"
                );
                let geometry = native_palette_geometry(&mut ui, &renderer);
                assert_native_row_ring_visible(&geometry, &identities[expected].widget_id());
                let (viewport, content, offset) = geometry.scroll.unwrap();
                if direction == "home" {
                    assert_eq!(offset.y, 0.0);
                }
                if direction == "end" {
                    assert!((offset.y - (content.height - viewport.height)).abs() < 0.01);
                }
                if expected > 10 {
                    assert!(offset.y > 0.0);
                }
                assert!(
                    matches!(messages.as_slice(), [Message::Palette(Action::SubmitFromNative(intent))] if intent.row.as_ref().is_some_and(|m| identities[expected].matches(&m.0)))
                );
                drop(ui);
                assert!(reduce_native_palette(&mut app, messages).is_empty());
                let EntryIdentity::Pane(target) = &identities[expected] else {
                    unreachable!()
                };
                assert_eq!(app.workspace.focused(), Some(*target));
                assert!(!app.palette.is_open());
            }
        }
    }

    #[test]
    fn native_palette_reveal_clamps_short_content_and_preserves_query_caret_and_scroll() {
        use iced::{
            Size,
            advanced::{clipboard, widget::operation},
            mouse,
        };
        use keyboard::key::{Code, Named, Physical};
        let app = branch_palette_fixture();
        let row = EntryIdentity::capture(&app.palette_entries()[0].2).unwrap();
        let mut renderer = crate::app::tests::headless_renderer();
        let mut ui = iced::runtime::UserInterface::build(
            app.palette_surface().unwrap(),
            Size::new(360.0, 700.0),
            iced::runtime::user_interface::Cache::default(),
            &mut renderer,
        );
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(widget::Id::new("palette.query")),
        );
        let mut messages = Vec::new();
        let tab = Source::tracked(
            PressCycle::new(Physical::Code(Code::Tab)),
            Transition::Press,
        );
        ui.update_with_sources(
            &[iced::Event::Keyboard(cycle_key(
                Named::Tab,
                Code::Tab,
                true,
                false,
            ))],
            &[tab],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        let geometry = native_palette_geometry(&mut ui, &renderer);
        assert_native_row_ring_visible(&geometry, &row.widget_id());
        let (viewport, content, offset) = geometry.scroll.unwrap();
        assert!(content.height <= viewport.height + 0.01);
        assert_eq!(offset.y, 0.0);
        drop(ui);
        for end in [false, true] {
            let app = long_native_palette_fixture();
            let mut ui = iced::runtime::UserInterface::build(
                app.palette_surface().unwrap(),
                Size::new(360.0, 700.0),
                iced::runtime::user_interface::Cache::default(),
                &mut renderer,
            );
            ui.operate(
                &renderer,
                &mut operation::focusable::focus::<()>(widget::Id::new("palette.query")),
            );
            ui.operate(
                &renderer,
                &mut operation::scrollable::scroll_to::<()>(
                    widget::Id::new("palette.commands"),
                    operation::scrollable::AbsoluteOffset {
                        x: None,
                        y: Some(500.0),
                    },
                ),
            );
            let before = native_palette_geometry(&mut ui, &renderer)
                .scroll
                .unwrap()
                .2;
            let (named, code) = if end {
                (Named::End, Code::End)
            } else {
                (Named::Home, Code::Home)
            };
            let text = keyboard::Event::KeyPressed {
                key: keyboard::Key::Character("x".into()),
                modified_key: keyboard::Key::Character("x".into()),
                physical_key: Physical::Code(Code::KeyX),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::empty(),
                text: Some("x".into()),
                repeat: false,
            };
            let mut messages = Vec::new();
            ui.update_with_sources(
                &[
                    iced::Event::Keyboard(cycle_key(named, code, true, false)),
                    iced::Event::Keyboard(text),
                ],
                &[
                    Source::tracked(PressCycle::new(Physical::Code(code)), Transition::Press),
                    Source::tracked(
                        PressCycle::new(Physical::Code(Code::KeyX)),
                        Transition::Press,
                    ),
                ],
                mouse::Cursor::Unavailable,
                &mut renderer,
                &mut clipboard::Null,
                &mut messages,
            );
            let geometry = native_palette_geometry(&mut ui, &renderer);
            assert_eq!(
                geometry.focused.unwrap().0,
                widget::Id::new("palette.query")
            );
            assert_eq!(geometry.scroll.unwrap().2, before);
            let expected = if end { "@x" } else { "x@" };
            assert!(
                matches!(messages.as_slice(), [Message::Palette(Action::QueryFromNative(_, value))] if value == expected)
            );
        }
    }

    #[test]
    fn native_input_public_scope_contract_restores_nested_and_empty_sources() {
        use keyboard::key::{Code, Physical};
        let original = input_source::current_source();
        let first = Source::tracked(
            PressCycle::new(Physical::Code(Code::Enter)),
            Transition::Press,
        );
        let second = Source::tracked(
            PressCycle::new(Physical::Code(Code::Space)),
            Transition::Release,
        );
        {
            let _outer = input_source::scope(first.clone());
            assert_eq!(input_source::current_source(), first);
            {
                let _inner = input_source::scope(second.clone());
                assert_eq!(input_source::current_source(), second);
            }
            assert_eq!(input_source::current_source(), first);
            std::thread::spawn(|| {
                assert_eq!(
                    input_source::current_source(),
                    Source::default(),
                    "new thread cannot inherit its caller's active native input"
                );
                let local = Source::tracked(
                    PressCycle::new(Physical::Code(Code::ArrowDown)),
                    Transition::Press,
                );
                {
                    let _local = input_source::scope(local.clone());
                    assert_eq!(input_source::current_source(), local);
                }
                assert_eq!(input_source::current_source(), Source::default());
            })
            .join()
            .expect("bounded public input-scope contract worker");
            assert_eq!(
                input_source::current_source(),
                first,
                "another thread cannot replace the active outer source"
            );
            {
                let _empty = input_source::scope(Source::default());
                assert_eq!(input_source::current_source(), Source::default());
            }
            assert_eq!(input_source::current_source(), first);
        }
        assert_eq!(input_source::current_source(), original);
    }

    #[test]
    fn native_input_public_scope_contract_unwind_restores_outer_source() {
        use keyboard::key::{Code, Physical};
        let original = input_source::current_source();
        let outer = Source::tracked(
            PressCycle::new(Physical::Code(Code::Enter)),
            Transition::Press,
        );
        {
            let _outer = input_source::scope(outer.clone());
            let result = std::panic::catch_unwind(|| {
                let _inner = input_source::scope(Source::tracked(
                    PressCycle::new(Physical::Code(Code::Space)),
                    Transition::Release,
                ));
                panic!("intentional scope-contract unwind");
            });
            assert!(result.is_err());
            assert_eq!(input_source::current_source(), outer);
        }
        assert_eq!(input_source::current_source(), original);
    }
    // Instrumentation delegates an actual native ContextMenu/Popover overlay;
    // it observes real UserInterface overlay delivery without inventing a pass.
    mod native_overlay_audit {
        use super::{Element, Message, Source};
        use cosmic::{Renderer, Theme, iced};
        use iced::advanced::widget::{Operation, Tree, tree};
        use iced::advanced::{Clipboard, Layout, Shell, Widget, layout, mouse, overlay, renderer};
        use iced::{Event, Length, Rectangle, Size, Vector};
        use std::sync::{Arc, Mutex};
        #[derive(Default)]
        pub(super) struct Visits {
            pub created: usize,
            pub keys: Vec<(Source, bool)>,
        }
        struct NativeOverlayAudit<'a> {
            content: Element<'a, Message>,
            visits: Arc<Mutex<Visits>>,
        }
        pub(super) fn view<'a>(
            content: Element<'a, Message>,
            visits: Arc<Mutex<Visits>>,
        ) -> Element<'a, Message> {
            Element::new(NativeOverlayAudit { content, visits })
        }
        impl Widget<Message, Theme, Renderer> for NativeOverlayAudit<'_> {
            fn tag(&self) -> tree::Tag {
                tree::Tag::of::<NativeOverlayAudit<'static>>()
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
                self.content.as_widget_mut().operate(
                    &mut tree.children[0],
                    layout,
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
                destinations: &mut iced::advanced::clipboard::DndDestinationRectangles,
            ) {
                self.content.as_widget().drag_destinations(
                    &tree.children[0],
                    layout,
                    renderer,
                    destinations,
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
                    .map(|inner| {
                        self.visits.lock().unwrap().created += 1;
                        overlay::Element::new(Box::new(AuditedNativeOverlay {
                            inner,
                            visits: self.visits.clone(),
                        }))
                    })
            }
        }

        struct AuditedNativeOverlay<'a> {
            inner: overlay::Element<'a, Message, Theme, Renderer>,
            visits: Arc<Mutex<Visits>>,
        }
        impl overlay::Overlay<Message, Theme, Renderer> for AuditedNativeOverlay<'_> {
            fn layout(&mut self, renderer: &Renderer, bounds: Size) -> layout::Node {
                self.inner.as_overlay_mut().layout(renderer, bounds)
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
                self.inner
                    .as_overlay_mut()
                    .update(event, layout, cursor, renderer, clipboard, shell);
                if matches!(
                    event,
                    Event::Keyboard(iced::keyboard::Event::KeyPressed { .. })
                ) {
                    self.visits.lock().unwrap().keys.push((
                        iced::runtime::core::keyboard::input::current_source(),
                        shell.is_event_captured(),
                    ));
                }
            }
            fn draw(
                &self,
                renderer: &mut Renderer,
                theme: &Theme,
                style: &renderer::Style,
                layout: Layout<'_>,
                cursor: mouse::Cursor,
            ) {
                self.inner
                    .as_overlay()
                    .draw(renderer, theme, style, layout, cursor);
            }
            fn operate(
                &mut self,
                layout: Layout<'_>,
                renderer: &Renderer,
                operation: &mut dyn Operation,
            ) {
                self.inner
                    .as_overlay_mut()
                    .operate(layout, renderer, operation);
            }
            fn mouse_interaction(
                &self,
                layout: Layout<'_>,
                cursor: mouse::Cursor,
                renderer: &Renderer,
            ) -> mouse::Interaction {
                self.inner
                    .as_overlay()
                    .mouse_interaction(layout, cursor, renderer)
            }
            fn overlay<'a>(
                &'a mut self,
                layout: Layout<'a>,
                renderer: &Renderer,
            ) -> Option<overlay::Element<'a, Message, Theme, Renderer>> {
                self.inner.as_overlay_mut().overlay(layout, renderer)
            }
            fn index(&self) -> f32 {
                self.inner.as_overlay().index()
            }
        }
    }
    #[derive(Clone, Copy, PartialEq, Eq)]
    struct RetainedPaletteMenuAction;
    impl widget::menu::Action for RetainedPaletteMenuAction {
        type Message = Message;
        fn message(&self) -> Message {
            Message::DismissNotice(1725)
        }
    }
    fn palette_retained_menu_fixture<'a>(
        app: &'a Cagents,
        palette: bool,
        visits: Arc<std::sync::Mutex<native_overlay_audit::Visits>>,
    ) -> Element<'a, Message> {
        let menu = crate::interaction::context_menu(
            widget::Space::new()
                .width(Length::Fill)
                .height(Length::Fill),
            widget::menu::items(
                &HashMap::new(),
                vec![widget::menu::Item::Button(
                    "Retained pointer action",
                    None,
                    RetainedPaletteMenuAction,
                )],
            ),
        );
        let menu = native_overlay_audit::view(menu, visits);
        // Match the actual app mount: existing workspace/context overlay in
        // the base slot, palette in the front base slot; native overlays remain
        // the runtime's separate overlay-first pass.
        crate::interaction::layer(
            menu,
            if palette {
                app.palette_surface().unwrap()
            } else {
                widget::Space::new().into()
            },
        )
    }
    #[test]
    fn actual_retained_context_menu_ignored_pass_allows_one_palette_tab_per_original_event() {
        let runtime = native_palette_test_runtime();
        let _runtime = runtime.enter();
        use iced::{
            Point, Size,
            advanced::{clipboard, widget::operation},
            mouse,
        };
        use keyboard::key::{Code, Named, Physical};
        let mut app = long_native_palette_fixture();
        drop(app.update_palette(Action::Close));
        let visits: Arc<std::sync::Mutex<native_overlay_audit::Visits>> = Default::default();
        let mut renderer = crate::app::tests::headless_renderer();
        let size = Size::new(880.0, 700.0);
        let mut ui = iced::runtime::UserInterface::build(
            palette_retained_menu_fixture(&app, false, visits.clone()),
            size,
            iced::runtime::user_interface::Cache::default(),
            &mut renderer,
        );
        let mut messages = Vec::new();
        let anchor = Point::new(30.0, 30.0);
        ui.update_with_sources(
            &[
                iced::Event::Mouse(mouse::Event::ButtonReleased(mouse::Button::Right)),
                iced::Event::Window(iced::window::Event::RedrawRequested(Instant::now())),
            ],
            &[Source::default(), Source::default()],
            mouse::Cursor::Available(anchor),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        // Opening occurs in the base pass. Drive a separate native frame so
        // the newly opened real overlay receives its measuring/redraw update
        // before later keyboard events (as in the running native app).
        ui.update_with_sources(
            &[iced::Event::Window(iced::window::Event::RedrawRequested(
                Instant::now(),
            ))],
            &[Source::default()],
            mouse::Cursor::Available(anchor),
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert!(
            visits.lock().unwrap().created > 0,
            "the actual pointer context menu opened before the palette"
        );
        let cache = ui.into_cache();
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query("@".into())));
        let rows: Vec<_> = app
            .palette_entries()
            .iter()
            .map(|(_, _, m)| EntryIdentity::capture(m).unwrap())
            .collect();
        let mut ui = iced::runtime::UserInterface::build(
            palette_retained_menu_fixture(&app, true, visits.clone()),
            size,
            cache,
            &mut renderer,
        );
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(widget::Id::new("palette.query")),
        );
        let tab = Source::tracked(
            PressCycle::new(Physical::Code(Code::Tab)),
            Transition::Press,
        );
        let enter = Source::tracked(
            PressCycle::new(Physical::Code(Code::Enter)),
            Transition::Press,
        );
        let events = [
            iced::Event::Keyboard(cycle_key(Named::Tab, Code::Tab, true, false)),
            iced::Event::Keyboard(cycle_key(Named::Tab, Code::Tab, true, true)),
            iced::Event::Keyboard(cycle_key(Named::Enter, Code::Enter, true, false)),
        ];
        let sources = [tab.clone(), tab.clone(), enter.clone()];
        let (_, statuses) = ui.update_with_sources(
            &events,
            &sources,
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(statuses, [iced::event::Status::Captured; 3]);
        let observed = visits.lock().unwrap();
        assert_eq!(
            observed.keys.len(),
            3,
            "each original key went through an actual retained-menu overlay pass"
        );
        assert!(
            observed.keys.iter().all(|(_, captured)| !captured),
            "this pointer menu declines keyboard ownership; no blanket overlay-modal rule"
        );
        assert_eq!(
            observed
                .keys
                .iter()
                .map(|(source, _)| source.clone())
                .collect::<Vec<_>>(),
            sources
        );
        drop(observed);
        let geometry = native_palette_geometry(&mut ui, &renderer);
        assert_native_row_ring_visible(&geometry, &rows[1].widget_id());
        assert!(
            matches!(messages.as_slice(), [Message::Palette(Action::SubmitFromNative(intent))] if intent.row.as_ref().is_some_and(|m| rows[1].matches(&m.0))),
            "two genuine same-cycle Tab events traverse exactly two controls; no overlay/base double traversal"
        );
        drop(ui);
        assert!(reduce_native_palette(&mut app, messages).is_empty());
        let EntryIdentity::Pane(target) = &rows[1] else {
            unreachable!()
        };
        assert_eq!(app.workspace.focused(), Some(*target));
    }
    fn palette_captured_popup_fixture<'a>(
        app: &'a Cagents,
        popup: bool,
        visits: Arc<std::sync::Mutex<native_overlay_audit::Visits>>,
    ) -> Element<'a, Message> {
        let mut surface = widget::popover(app.palette_surface().unwrap())
            .id(widget::Id::new("palette.capture.popover"))
            .modal(true)
            .position(widget::popover::Position::Point(iced::Point::new(
                30.0, 30.0,
            )));
        if popup {
            let button = widget::button::custom(design::caption("Foreground keyboard owner"))
                .id(widget::Id::new("palette.capture.control"))
                .on_press(Message::Palette(Action::ConsumedActivation));
            surface = surface.popup(crate::interaction::focused_key(button, |event| {
                matches!(event, keyboard::Event::KeyPressed { key: keyboard::Key::Named(keyboard::key::Named::Tab), modifiers, .. } if modifiers.is_empty())
                    .then_some(Message::Palette(Action::ConsumedActivation))
            }));
        }
        native_overlay_audit::view(surface.into(), visits)
    }
    #[test]
    fn actual_captured_popup_pass_preserves_base_palette_focus_and_allows_later_genuine_repeat() {
        use iced::{
            Size,
            advanced::{clipboard, widget::operation},
            mouse,
        };
        use keyboard::key::{Code, Named, Physical};
        let app = long_native_palette_fixture();
        let visits: Arc<std::sync::Mutex<native_overlay_audit::Visits>> = Default::default();
        let mut renderer = crate::app::tests::headless_renderer();
        let size = Size::new(360.0, 700.0);
        let mut ui = iced::runtime::UserInterface::build(
            palette_captured_popup_fixture(&app, false, visits.clone()),
            size,
            iced::runtime::user_interface::Cache::default(),
            &mut renderer,
        );
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(widget::Id::new("palette.query")),
        );
        let before = native_palette_geometry(&mut ui, &renderer);
        let cache = ui.into_cache();
        let mut ui = iced::runtime::UserInterface::build(
            palette_captured_popup_fixture(&app, true, visits.clone()),
            size,
            cache,
            &mut renderer,
        );
        ui.operate(
            &renderer,
            &mut operation::focusable::focus::<()>(widget::Id::new("palette.capture.control")),
        );
        let tab = Source::tracked(
            PressCycle::new(Physical::Code(Code::Tab)),
            Transition::Press,
        );
        let enter = Source::tracked(
            PressCycle::new(Physical::Code(Code::Enter)),
            Transition::Press,
        );
        let mut messages = Vec::new();
        let (_, statuses) = ui.update_with_sources(
            &[
                iced::Event::Keyboard(cycle_key(Named::Tab, Code::Tab, true, false)),
                iced::Event::Keyboard(cycle_key(Named::Enter, Code::Enter, true, false)),
            ],
            &[tab.clone(), enter.clone()],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(statuses, [iced::event::Status::Captured; 2]);
        assert!(
            matches!(
                messages.as_slice(),
                [
                    Message::Palette(Action::ConsumedActivation),
                    Message::Palette(Action::ConsumedActivation)
                ]
            ),
            "foreground controls keep their native key actions; palette emits no Submit"
        );
        let observed = visits.lock().unwrap();
        assert!(observed.created > 0);
        assert_eq!(observed.keys.len(), 2);
        assert_eq!(
            observed
                .keys
                .iter()
                .map(|(source, _)| source.clone())
                .collect::<Vec<_>>(),
            [tab.clone(), enter]
        );
        assert!(observed.keys.iter().all(|(_, captured)| *captured));
        drop(observed);
        let cache = ui.into_cache();
        let mut ui = iced::runtime::UserInterface::build(
            palette_captured_popup_fixture(&app, false, visits),
            size,
            cache,
            &mut renderer,
        );
        let after = native_palette_geometry(&mut ui, &renderer);
        assert_eq!(
            after.focused, before.focused,
            "captured overlay events never traversed the base palette"
        );
        assert_eq!(after.scroll, before.scroll);
        messages.clear();
        let (_, statuses) = ui.update_with_sources(
            &[iced::Event::Keyboard(cycle_key(
                Named::Tab,
                Code::Tab,
                true,
                true,
            ))],
            &[tab],
            mouse::Cursor::Unavailable,
            &mut renderer,
            &mut clipboard::Null,
            &mut messages,
        );
        assert_eq!(statuses, [iced::event::Status::Captured]);
        assert!(messages.is_empty());
        let first = EntryIdentity::capture(&app.palette_entries()[0].2).unwrap();
        assert_native_row_ring_visible(
            &native_palette_geometry(&mut ui, &renderer),
            &first.widget_id(),
        );
    }
    #[test]
    fn owned_initial_keys_preserve_native_escape_and_wait_for_release_before_global_dispatch() {
        use keyboard::key::{Code, Named, Physical};
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.refresh_ui.focused = true;
        app.context_inspector_open = false;
        app.active_workspace = Some("/owned-key-fixture".into());
        let original = app.workspace.focused().unwrap();
        drop(app.update_palette(Action::Open));
        let press = keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("t".into()),
            modified_key: keyboard::Key::Character("t".into()),
            physical_key: Physical::Code(Code::KeyT),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL,
            text: None,
            repeat: false,
        };
        let up = keyboard::Event::KeyReleased {
            key: keyboard::Key::Character("t".into()),
            modified_key: keyboard::Key::Character("t".into()),
            physical_key: Physical::Code(Code::KeyT),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL,
        };
        let quick = PressCycle::new(Physical::Code(Code::KeyT));
        let quick_sources = [
            Source::tracked(quick.clone(), Transition::Press),
            Source::tracked(quick.clone(), Transition::Release),
        ];
        quick.release();
        let (messages, statuses) = palette_native_batch(
            &app,
            widget::Id::new("palette.query"),
            &[
                iced::Event::Keyboard(press.clone()),
                iced::Event::Keyboard(up.clone()),
            ],
            &quick_sources,
        );
        assert_eq!(statuses, [iced::event::Status::Captured; 2]);
        for message in messages {
            drop(app.update(message));
        }
        drop(app.update_palette(Action::Close));
        drop(app.update(Message::TaggedKeyEvent {
            event: press.clone(),
            captured: true,
            source: quick_sources[0].clone(),
        }));
        assert_eq!(
            app.workspace.order().collect::<Vec<_>>(),
            [original],
            "quick native P/R then pointer Close cannot promote delayed P into a global thread"
        );
        drop(app.update(Message::TaggedKeyEvent {
            event: up.clone(),
            captured: true,
            source: quick_sources[1].clone(),
        }));
        drop(app.update_palette(Action::Open));
        let cycle = PressCycle::new(Physical::Code(Code::KeyT));
        let source = Source::tracked(cycle.clone(), Transition::Press);
        let (messages, statuses) = palette_native_batch(
            &app,
            widget::Id::new("palette.query"),
            &[iced::Event::Keyboard(press.clone())],
            &[source.clone()],
        );
        assert_eq!(statuses, [iced::event::Status::Captured]);
        for message in messages {
            drop(app.update(message));
        }
        drop(app.update(Message::TaggedKeyEvent {
            event: press.clone(),
            captured: true,
            source: source.clone(),
        }));
        assert!(app.palette.is_open());
        drop(app.update_palette(Action::Close));
        let mut repeat = press.clone();
        if let keyboard::Event::KeyPressed { repeat, .. } = &mut repeat {
            *repeat = true;
        }
        for event in [repeat, press.clone()] {
            drop(app.update(Message::TaggedKeyEvent {
                event,
                captured: true,
                source: source.clone(),
            }));
            assert_eq!(
                app.workspace.order().collect::<Vec<_>>(),
                [original],
                "owned aliases cannot open a background thread"
            );
        }
        cycle.release();
        let up = keyboard::Event::KeyReleased {
            key: keyboard::Key::Character("t".into()),
            modified_key: keyboard::Key::Character("t".into()),
            physical_key: Physical::Code(Code::KeyT),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::CTRL,
        };
        drop(app.update(Message::TaggedKeyEvent {
            event: up,
            captured: true,
            source: Source::tracked(cycle, Transition::Release),
        }));
        let fresh = Source::tracked(
            PressCycle::new(Physical::Code(Code::KeyT)),
            Transition::Press,
        );
        // Exercise the real root and then its subscription copy, so this
        // positive also composes with a native-owner observer. Lazy Tasks stay
        // unpolled: this creates a draft, never a provider or terminal process.
        let mut renderer = crate::app::tests::headless_renderer();
        let mut ui = iced::runtime::UserInterface::build(
            app.view(),
            iced::Size::new(1600.0, 1000.0),
            iced::runtime::user_interface::Cache::default(),
            &mut renderer,
        );
        let mut native = Vec::new();
        let (_, statuses) = ui.update_with_sources(
            &[iced::Event::Keyboard(press.clone())],
            &[fresh.clone()],
            iced::mouse::Cursor::Unavailable,
            &mut renderer,
            &mut iced::advanced::clipboard::Null,
            &mut native,
        );
        let _cache = ui.into_cache();
        assert_eq!(statuses, [iced::event::Status::Captured]);
        for message in native {
            drop(app.update(message));
        }
        drop(app.update(Message::TaggedKeyEvent {
            event: press,
            captured: true,
            source: fresh,
        }));
        assert_eq!(
            app.workspace.order().count(),
            2,
            "fresh unowned Ctrl+T remains usable"
        );
        assert!(!app.chat_start_pending(app.workspace.focused().unwrap()));

        // Quick key-up can remove key ownership before the subscription copy
        // arrives. The existing weak native-action ticket still identifies it.
        for row_focus in [false, true] {
            drop(app.update_palette(Action::Open));
            let focus = if row_focus {
                EntryIdentity::capture(&app.palette_entries()[0].2)
                    .unwrap()
                    .widget_id()
            } else {
                widget::Id::new("palette.query")
            };
            let escape = cycle_key(Named::Escape, Code::Escape, true, false);
            let escape_up = cycle_key(Named::Escape, Code::Escape, false, false);
            let cycle = PressCycle::new(Physical::Code(Code::Escape));
            let sources = [
                Source::tracked(cycle.clone(), Transition::Press),
                Source::tracked(cycle.clone(), Transition::Release),
            ];
            cycle.release();
            let (messages, statuses) = palette_native_batch(
                &app,
                focus,
                &[
                    iced::Event::Keyboard(escape.clone()),
                    iced::Event::Keyboard(escape_up.clone()),
                ],
                &sources,
            );
            assert_eq!(statuses, [iced::event::Status::Captured; 2]);
            assert_eq!(
                messages
                    .iter()
                    .filter(|message| matches!(
                        message,
                        Message::Palette(Action::CloseFromNative(_))
                    ))
                    .count(),
                1
            );
            for message in messages {
                drop(app.update(message));
            }
            assert!(
                !app.palette.is_open(),
                "fresh native Escape closes from query and row"
            );
            drop(app.update_palette(Action::Open));
            let successor = app.palette.native_scope;
            for (event, source) in [escape, escape_up].into_iter().zip(sources) {
                drop(app.update(Message::TaggedKeyEvent {
                    event,
                    captured: true,
                    source,
                }));
            }
            assert!(
                app.palette.is_open(),
                "old quick-Escape copy cannot close a successor"
            );
            assert_eq!(app.palette.native_scope, successor);
        }
        let escape = cycle_key(Named::Escape, Code::Escape, true, false);
        let held = PressCycle::new(Physical::Code(Code::Escape));
        let source = Source::tracked(held.clone(), Transition::Press);
        let (messages, _) = palette_native_batch(
            &app,
            widget::Id::new("palette.query"),
            &[iced::Event::Keyboard(escape.clone())],
            &[source.clone()],
        );
        // Replace the presentation before reducing its native Close intent.
        drop(app.update_palette(Action::Close));
        drop(app.update_palette(Action::Open));
        app.device_ui.open = true;
        for message in messages {
            drop(app.update(message));
        }
        drop(app.update(Message::TaggedKeyEvent {
            event: escape.clone(),
            captured: true,
            source: source.clone(),
        }));
        assert!(
            app.device_ui.open,
            "stale palette Escape must not answer the covering form"
        );
        held.release();
        let up = cycle_key(Named::Escape, Code::Escape, false, false);
        drop(app.update(Message::TaggedKeyEvent {
            event: up,
            captured: true,
            source: Source::tracked(held.clone(), Transition::Release),
        }));
        let fresh_escape = Source::tracked(
            PressCycle::new(Physical::Code(Code::Escape)),
            Transition::Press,
        );
        drop(app.update(Message::TaggedKeyEvent {
            event: escape.clone(),
            captured: true,
            source: fresh_escape,
        }));
        assert!(
            !app.device_ui.open,
            "a genuinely fresh form Escape still closes the form"
        );
        let (messages, _) = palette_native_batch(
            &app,
            widget::Id::new("palette.query"),
            &[iced::Event::Keyboard(escape)],
            &[source],
        );
        for message in messages {
            drop(app.update(message));
        }
        assert!(
            app.palette.is_open(),
            "an old native Escape copy cannot dismiss the replacement palette"
        );
        let text = keyboard::Event::KeyPressed {
            key: keyboard::Key::Character("q".into()),
            modified_key: keyboard::Key::Character("q".into()),
            physical_key: Physical::Code(Code::KeyQ),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: Some("q".into()),
            repeat: false,
        };
        let (messages, _) = palette_native_batch(
            &app,
            widget::Id::new("palette.query"),
            &[iced::Event::Keyboard(text)],
            &[Source::tracked(
                PressCycle::new(Physical::Code(Code::KeyQ)),
                Transition::Press,
            )],
        );
        for message in messages {
            drop(app.update(message));
        }
        assert_eq!(
            app.palette.query.as_deref(),
            Some("q"),
            "ordinary native query editing remains usable"
        );
    }
}

#[cfg(test)]
mod settings_recorder_tests {
    use super::*;

    #[test]
    fn settings_palette_release_commits_recent_panes_before_modal_capture() {
        for release_key in [false, true] {
            let mut app = super::super::tests::context_inspector_test_app(true);
            app.context_inspector_open = false;
            let first = app.durable.cards[0].clone();
            let mut second = first.clone();
            second.id = CardId::new();
            let mut third = first.clone();
            third.id = CardId::new();
            let ids = [first.id, second.id, third.id];
            let durable = Arc::make_mut(&mut app.durable);
            durable.cards = vec![first, second, third];
            durable.focused = Some(ids[2]);
            app.workspace = Cagents::rebuild_workspace(&app.durable);
            app.tab_history = crate::tab_history::TabHistory::from_recent([ids[2], ids[1], ids[0]]);
            let press = |key: keyboard::Key, physical, modifiers| Message::KeyEvent {
                event: keyboard::Event::KeyPressed {
                    modified_key: key.clone(),
                    key,
                    physical_key: keyboard::key::Physical::Code(physical),
                    location: keyboard::Location::Standard,
                    modifiers,
                    text: None,
                    repeat: false,
                },
                captured: true,
            };
            let tab = || {
                press(
                    keyboard::Key::Named(keyboard::key::Named::Tab),
                    keyboard::key::Code::Tab,
                    keyboard::Modifiers::CTRL,
                )
            };
            drop(app.update(tab()));
            assert_eq!(app.workspace.focused(), Some(ids[1]));
            assert!(app.tab_history.cycle_order().is_some());
            drop(app.update(press(
                keyboard::Key::Character("p".into()),
                keyboard::key::Code::KeyP,
                keyboard::Modifiers::CTRL | keyboard::Modifiers::SHIFT,
            )));
            assert!(app.palette.is_open());
            let event = if release_key {
                keyboard::Event::KeyReleased {
                    key: keyboard::Key::Named(keyboard::key::Named::Control),
                    modified_key: keyboard::Key::Named(keyboard::key::Named::Control),
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::ControlLeft),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                }
            } else {
                keyboard::Event::ModifiersChanged(keyboard::Modifiers::empty())
            };
            drop(app.update(Message::KeyEvent {
                event,
                captured: true,
            }));
            assert!(app.palette.is_open());
            assert!(app.tab_history.cycle_order().is_none());
            drop(app.update(press(
                keyboard::Key::Named(keyboard::key::Named::Escape),
                keyboard::key::Code::Escape,
                keyboard::Modifiers::empty(),
            )));
            assert!(!app.palette.is_open());
            drop(app.update(tab()));
            assert_eq!(app.workspace.focused(), Some(ids[2]));
        }
    }

    #[test]
    fn settings_capture_yields_to_a_current_form_dialog_and_resumes_after_escape() {
        for filtering in [false, true] {
            let mut app = super::super::tests::context_inspector_test_app(true);
            app.context_inspector_open = false;
            let field = SettingsField::ShortcutToggleProjects;
            drop(app.update_palette(if filtering {
                Action::FilterKey
            } else {
                Action::Record(field)
            }));
            let settings = app.settings.clone();
            let draft = app
                .settings_draft
                .keyboard_shortcuts
                .toggle_projects
                .clone();
            app.device_ui.open = true;
            let event = keyboard::Event::KeyPressed {
                key: keyboard::Key::Named(keyboard::key::Named::F8),
                modified_key: keyboard::Key::Named(keyboard::key::Named::F8),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::F8),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::CTRL | keyboard::Modifiers::ALT,
                text: None,
                repeat: false,
            };
            drop(app.update(Message::KeyEvent {
                event: event.clone(),
                captured: false,
            }));
            assert!(app.device_ui.open);
            assert_eq!(app.palette.filtering_key, filtering);
            assert_eq!(app.palette.recording, (!filtering).then_some(field));
            assert!(app.palette.recording_error.is_none());
            assert_eq!(app.settings, settings);
            assert_eq!(app.settings_draft.keyboard_shortcuts.toggle_projects, draft);
            assert!(app.settings_search.is_empty());
            drop(app.update(Message::KeyEvent {
                event: keyboard::Event::KeyPressed {
                    key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
                    physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
                    location: keyboard::Location::Standard,
                    modifiers: keyboard::Modifiers::empty(),
                    text: None,
                    repeat: false,
                },
                captured: false,
            }));
            assert!(!app.device_ui.open, "the visible form owns Escape");
            assert!(app.palette.is_open(), "covered capture was not cancelled");
            drop(app.update(Message::KeyEvent {
                event,
                captured: false,
            }));
            assert!(
                !app.palette.is_open(),
                "exposed capture accepts the fresh chord"
            );
            if filtering {
                assert_eq!(app.settings_search, "Ctrl+Alt+F8");
                assert_eq!(app.settings, settings);
            } else {
                assert!(
                    app.settings
                        .keyboard_shortcuts
                        .toggle_projects
                        .contains(&"Ctrl+Alt+F8".into())
                );
                assert!(app.settings_invalid.is_empty());
            }
        }
    }

    #[test]
    fn settings_recorder_and_key_filter_share_dispatch_character_normalization() {
        let event = |value: &str, physical, modifiers| keyboard::Event::KeyPressed {
            key: keyboard::Key::Character(value.into()),
            modified_key: keyboard::Key::Character(value.into()),
            physical_key: keyboard::key::Physical::Code(physical),
            location: keyboard::Location::Standard,
            modifiers,
            text: None,
            repeat: false,
        };
        let modifiers =
            keyboard::Modifiers::CTRL | keyboard::Modifiers::ALT | keyboard::Modifiers::SHIFT;
        for (value, physical, chord) in [
            ("л", keyboard::key::Code::KeyK, "Ctrl+Alt+Shift+K"),
            ("!", keyboard::key::Code::Digit1, "Ctrl+Alt+Shift+1"),
            (",", keyboard::key::Code::Comma, "Ctrl+Alt+Shift+Comma"),
            ("+", keyboard::key::Code::Equal, "Ctrl+Alt+Shift+Plus"),
        ] {
            let mut app = super::super::tests::context_inspector_test_app(false);
            let field = SettingsField::ShortcutToggleProjects;
            let pressed = event(value, physical, modifiers);
            let binding = settings::parse_shortcut(chord).unwrap();
            assert!(workspace_widget::shortcut_event_matches(binding, &pressed));
            drop(app.update_palette(Action::Record(field)));
            drop(
                app.palette_key_event(&pressed)
                    .expect("recorder owns the key"),
            );
            assert!(app.palette.recording_error.is_none());
            assert!(
                app.settings
                    .keyboard_shortcuts
                    .toggle_projects
                    .iter()
                    .any(|chord| settings::parse_shortcut(chord).ok() == Some(binding))
            );
            drop(app.update_palette(Action::FilterKey));
            drop(
                app.palette_key_event(&pressed)
                    .expect("pressed-key filter owns the key"),
            );
            assert_eq!(app.settings_search, chord);
            assert!(!app.palette.filtering_key);
        }
        let mut app = super::super::tests::context_inspector_test_app(false);
        let field = SettingsField::ShortcutToggleProjects;
        let before = app.settings.keyboard_shortcuts.clone();
        drop(app.update_palette(Action::Record(field)));
        let numpad = event("1", keyboard::key::Code::Numpad1, modifiers);
        drop(app.palette_key_event(&numpad));
        assert!(
            app.palette
                .recording_error
                .as_ref()
                .is_some_and(|reason| reason.contains("number row"))
        );
        assert_eq!(app.settings.keyboard_shortcuts, before);
        assert_eq!(app.palette.recording, Some(field));
        let interrupt = event("с", keyboard::key::Code::KeyC, keyboard::Modifiers::CTRL);
        drop(
            app.palette_key_event(&interrupt)
                .expect("recorder owns the key"),
        );
        assert!(
            app.palette
                .recording_error
                .as_ref()
                .is_some_and(|reason| reason.contains("interrupts"))
        );
        assert_eq!(app.settings.keyboard_shortcuts, before);
        assert_eq!(app.palette.recording, Some(field));
    }

    #[test]
    fn recorder_refuses_shell_controls_and_fixed_chords_before_editing_the_draft() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let field = SettingsField::ShortcutToggleProjects;
        let original = app
            .settings_draft
            .keyboard_shortcuts
            .toggle_projects
            .clone();
        for (chord, reason) in [
            ("Ctrl+C", "interrupts"),
            ("Ctrl+Tab", "Cycle recent panes"),
            ("Ctrl+=", "Increase font size"),
        ] {
            drop(app.update_palette(Action::Record(field)));
            drop(app.record_shortcut(field, chord.into()));
            assert!(
                app.palette
                    .recording_error
                    .as_ref()
                    .is_some_and(|error| error.contains(reason)),
                "{chord}"
            );
            assert_eq!(app.palette.recording, Some(field));
            assert_eq!(
                app.settings_draft.keyboard_shortcuts.toggle_projects,
                original
            );
        }
    }

    #[test]
    fn cancelling_a_recording_cannot_reassign_its_conflict_from_key_filter_mode() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let original = app.settings.keyboard_shortcuts.clone();
        let field = SettingsField::ShortcutToggleProjects;
        let chord = app.settings.keyboard_shortcuts.command_palette[0].clone();
        drop(app.update_palette(Action::Record(field)));
        drop(app.record_shortcut(field, chord));
        assert!(app.palette.reassign.is_some());
        let escape = keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(keyboard::key::Named::Escape),
            modified_key: keyboard::Key::Named(keyboard::key::Named::Escape),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Escape),
            location: keyboard::Location::Standard,
            modifiers: keyboard::Modifiers::empty(),
            text: None,
            repeat: false,
        };
        drop(app.palette_key_event(&escape));
        assert!(app.palette.reassign.is_none());
        drop(app.update_palette(Action::FilterKey));
        assert!(app.palette.recording.is_none());
        assert!(app.palette.reassign.is_none());
        drop(app.update_palette(Action::Reassign));
        assert_eq!(app.settings.keyboard_shortcuts, original);
    }

    #[test]
    fn reassign_preserves_chord_drafts_still_waiting_for_autosave() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let field = SettingsField::ShortcutToggleProjects;
        let chord = app.settings.keyboard_shortcuts.command_palette[0].clone();
        drop(app.queue_setting_input(field, String::new()));
        drop(app.queue_setting_input(
            SettingsField::ShortcutCommandPalette,
            format!("{chord}, Ctrl+Alt+Shift+P"),
        ));
        drop(app.update_palette(Action::Record(field)));
        drop(app.record_shortcut(field, chord.clone()));
        drop(app.update_palette(Action::Reassign));
        assert_eq!(app.settings.keyboard_shortcuts.toggle_projects, [chord]);
        assert_eq!(
            app.settings.keyboard_shortcuts.command_palette,
            ["Ctrl+Alt+Shift+P"]
        );
        assert!(app.settings_pending.is_empty());
        assert!(app.settings_invalid.is_empty());
    }

    #[test]
    fn recorder_reassigns_a_conflicting_optional_chord_atomically() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let field = SettingsField::ShortcutToggleProjects;
        let chord = app.settings.keyboard_shortcuts.command_palette[0].clone();
        drop(app.update_palette(Action::Record(field)));
        drop(app.record_shortcut(field, chord.clone()));
        assert!(app.palette.reassign.is_some());
        drop(app.update_palette(Action::Reassign));
        assert!(app.palette.recording.is_none());
        assert!(app.settings.keyboard_shortcuts.command_palette.is_empty());
        assert!(
            app.settings
                .keyboard_shortcuts
                .toggle_projects
                .contains(&chord)
        );
        assert!(app.settings_invalid.is_empty());
        assert!(app.settings.clone().validate().is_ok());
    }
}
