use super::*;
use workspace_widget::GlobalShortcut as Command;

#[derive(Debug, Default)]
pub(super) struct State {
    recording: Option<SettingsField>,
    recording_error: Option<String>,
    reassign: Option<(SettingsField, String, usize)>,
    filtering_key: bool,
    query: Option<String>,
    selected: usize,
    panes_only: bool,
}
impl State {
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
    OpenPanes,
    ClosePane(CardId),
    Close,
    Query(String),
    Run(MessageBox),
    Submit,
    FocusPane(CardId),
    RunPane(CardId, Command),
    RunChatPane(chat_ui::Target, Command),
}
// Box breaks the recursive Message/Action representation.
#[derive(Debug, Clone)]
pub struct MessageBox(pub Box<Message>);

const ROW_HEIGHT: f32 = 40.0;
const ROW_SPACING: f32 = space::XXS;

/// Category prefixes are parsed before scoring and never participate in matching.
fn palette_query(query: &str) -> (Option<char>, &str) {
    let query = query.trim();
    match query.chars().next() {
        Some(prefix @ ('>' | '@' | '#')) => (Some(prefix), query[1..].trim_start()),
        _ => (None, query),
    }
}

fn search_label(label: &str) -> &str {
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

fn entry_key(message: &Message) -> String {
    match message {
        Message::RunCommand(command) => format!("command:{}", command.id()),
        Message::Palette(Action::FocusPane(id)) | Message::FocusCard(id) => format!("pane:{id}"),
        Message::Sidebar(SidebarAction::OpenWorktree { worktree_id, .. }) => {
            format!("worktree:{worktree_id}")
        }
        Message::Sidebar(SidebarAction::OpenChannel { channel_id }) => {
            format!("channel:{channel_id}")
        }
        Message::Sidebar(SidebarAction::OpenRepository { project_id, .. }) => {
            format!("folder:{project_id}")
        }
        Message::CloseCard(_) => format!("command:{}", Command::CloseFocusedCard.id()),
        _ => "command:resume_conversation".into(),
    }
}

/// The sidebar and palette share match ranking; palette callers pass display
/// labels, while the shared scorer consumes case-folded text.
fn match_score(label: &str, query: &str) -> Option<usize> {
    crate::search_match::score(&label.to_lowercase(), &query.to_lowercase())
}

fn highlighted_label(label: &str, query: &str) -> Element<'static, Message> {
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
    let searchable = search_label(label);
    let offset = label.len() - searchable.len();
    let folded: Vec<_> = searchable
        .char_indices()
        .flat_map(|(index, ch)| {
            ch.to_lowercase()
                .map(move |folded| (folded, offset + index..offset + index + ch.len_utf8()))
        })
        .collect();
    let needle = query.to_lowercase().chars().collect::<Vec<_>>();
    let mut matches = Vec::new();
    if !needle.is_empty() {
        let contiguous = folded
            .windows(needle.len())
            .enumerate()
            .filter(|(_, window)| window.iter().map(|(ch, _)| *ch).eq(needle.iter().copied()))
            .min_by_key(|(index, _)| {
                if *index == 0 {
                    0
                } else if !folded[index - 1].0.is_alphanumeric() {
                    1
                } else {
                    2
                }
            });
        if let Some((_, window)) = contiguous {
            matches.extend(window.iter().map(|(_, range)| range.clone()));
        } else {
            let mut cursor = 0;
            for needle in needle {
                if let Some(index) = folded[cursor..].iter().position(|(ch, _)| *ch == needle) {
                    cursor += index;
                    matches.push(folded[cursor].1.clone());
                    cursor += 1;
                }
            }
        }
    }
    let mut spans: Vec<Span<'static, ()>> = Vec::new();
    let mut start = 0;
    let mut highlighted = false;
    for (index, _) in label.char_indices() {
        let next = matches.iter().any(|range| range.contains(&index));
        if next != highlighted {
            if start < index {
                let span = Span::new(label[start..index].to_owned());
                spans.push(if highlighted {
                    span.color(design::accent_color(&cosmic::theme::active()))
                        .font(iced::Font {
                            weight: iced::font::Weight::Bold,
                            ..cosmic::font::default()
                        })
                } else {
                    span
                });
            }
            start = index;
            highlighted = next;
        }
    }
    let span = Span::new(label[start..].to_owned());
    spans.push(if highlighted {
        span.color(design::accent_color(&cosmic::theme::active()))
            .font(iced::Font {
                weight: iced::font::Weight::Bold,
                ..cosmic::font::default()
            })
    } else {
        span
    });
    widget::container(
        iced::widget::rich_text(spans)
            .size(design::TEXT_BODY)
            .font(cosmic::font::default())
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
    fn appearance(selected: bool, hover: bool, theme: &cosmic::Theme) -> widget::button::Style {
        let mut accent = design::accent_color(theme);
        accent.a = if selected { 0.18 } else { 0.08 };
        widget::button::Style {
            background: (selected || hover).then_some(iced::Background::Color(accent)),
            border_radius: design::RADIUS_S.into(),
            ..design::button_style()
        }
    }
    cosmic::theme::Button::Custom {
        active: Box::new(move |_, theme| appearance(selected, false, theme)),
        disabled: Box::new(move |theme| appearance(selected, false, theme)),
        hovered: Box::new(move |_, theme| appearance(selected, true, theme)),
        pressed: Box::new(move |_, theme| appearance(true, true, theme)),
    }
}

fn project_command_index(command: Command) -> Option<usize> {
    match command {
        Command::FocusProject1 => Some(0),
        Command::FocusProject2 => Some(1),
        Command::FocusProject3 => Some(2),
        Command::FocusProject4 => Some(3),
        Command::FocusProject5 => Some(4),
        Command::FocusProject6 => Some(5),
        Command::FocusProject7 => Some(6),
        Command::FocusProject8 => Some(7),
        Command::FocusProject9 => Some(8),
        _ => None,
    }
}

fn settings_command_category(command: Command) -> Option<SettingsCategory> {
    match command {
        Command::SettingsGeneral => Some(SettingsCategory::General),
        Command::SettingsAppearance => Some(SettingsCategory::Appearance),
        Command::SettingsTerminal => Some(SettingsCategory::Terminal),
        Command::SettingsLinks => Some(SettingsCategory::Links),
        Command::SettingsKeyboard => Some(SettingsCategory::Keyboard),
        Command::SettingsNativeChat => Some(SettingsCategory::NativeChat),
        Command::SettingsEditor => Some(SettingsCategory::Editor),
        Command::SettingsAccounts => Some(SettingsCategory::Accounts),
        Command::SettingsNotifications => Some(SettingsCategory::Notifications),
        Command::SettingsAbout => Some(SettingsCategory::About),
        _ => None,
    }
}

impl Cagents {
    pub(super) fn update_palette(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Record(field) => {
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
                let Some((field, chord, owner)) = self.palette.reassign.take() else { return Task::none(); };
                if self.palette.recording != Some(field) { return Task::none(); }
                let binding = settings::parse_shortcut(&chord).expect("recorded chord");
                let mut next = self.settings.clone();
                let drafts = self.settings_draft.keyboard_shortcuts.shortcut_values();
                let owner_action = SettingsField::SHORTCUTS[owner].shortcut_action().expect("shortcut action");
                let remaining = drafts.iter().find(|(action, _)| *action == owner_action).expect("owner draft").1;
                let mut remaining = settings::parse_chords(remaining);
                remaining.retain(|chord| settings::parse_shortcut(chord).ok() != Some(binding));
                next.keyboard_shortcuts.set_action(owner, remaining.clone());
                let index = SettingsField::SHORTCUTS.iter().position(|candidate| *candidate == field).expect("shortcut");
                let action = field.shortcut_action().expect("shortcut action");
                let mine = drafts.iter().find(|(candidate, _)| *candidate == action).expect("edited draft").1;
                let mut mine = settings::parse_chords(mine);
                if !mine.iter().any(|chord| settings::parse_shortcut(chord).ok() == Some(binding)) { mine.push(chord); }
                next.keyboard_shortcuts.set_action(index, mine.clone());
                let attempted = next.clone();
                match next.validate() {
                    Ok(next) => {
                        let previous = std::mem::replace(&mut self.settings, next);
                        self.settings_draft.set(SettingsField::SHORTCUTS[owner], settings::format_chords(&remaining));
                        self.settings_draft.set(field, settings::format_chords(&mine));
                        self.settings_pending.remove(&field);
                        self.settings_pending.remove(&SettingsField::SHORTCUTS[owner]);
                        self.settings_invalid.remove(&field);
                        self.settings_invalid.remove(&SettingsField::SHORTCUTS[owner]);
                        self.retry_shortcut_drafts();
                        self.palette.recording = None;
                        self.palette.recording_error = None;
                        self.refresh_settings_feedback();
                        Task::batch([self.save_settings_task(false), self.synchronize_settings(&previous)])
                    }
                    Err(_) => {
                        self.palette.recording_error = if remaining.is_empty() && settings::shortcut_required(owner) {
                            Some(format!("{} requires at least one shortcut. Record another shortcut there before reassigning this one.", SettingsField::SHORTCUTS[owner].label()))
                        } else {
                            refusal(field, &attempted).or_else(|| refusal(SettingsField::SHORTCUTS[owner], &attempted)).or_else(|| Some("Reassignment could not be saved. Check the other settings.".into()))
                        };
                        Task::none()
                    }
                }
            }
            Action::ClosePane(card) => self.run_palette_command(Message::CloseCard(card)),
            Action::Open | Action::OpenPanes => {
                if self.workspace_inputs_blocked() {
                    return Task::none();
                }
                self.palette.panes_only = matches!(action, Action::OpenPanes);
                self.chrome_ui.close();
                self.palette.query = Some(String::new());
                self.palette.selected = 0;
                iced::widget::operation::focus(widget::Id::new("palette.query"))
                    .map(cosmic::Action::App)
                    .chain(self.reveal_palette_selection())
            }
            Action::Close => {
                self.palette.recording = None;
                self.palette.filtering_key = false;
                self.palette.query = None;
                self.palette.reassign = None;
                self.palette.recording_error = None;
                self.workspace.focused().map_or_else(Task::none, |card| {
                    iced::widget::operation::focus(self.card_keyboard_focus(card))
                        .map(cosmic::Action::App)
                })
            }
            Action::Query(query) => {
                self.palette.query = Some(query);
                self.palette.selected = 0;
                self.reveal_palette_selection()
            }
            Action::Run(MessageBox(message)) => self.run_palette_command(*message),
            Action::RunChatPane(target, command) => {
                let current = match command {
                    Command::ForkConversation => self.chat_fork_target(target.card),
                    Command::ReviewConversationChanges => self.chat_changes_target(target.card),
                    _ => None,
                };
                if current != Some(target) {
                    self.status = Some(TransientNotice::warning("This conversation changed; open its menu again"));
                    return Task::none();
                }
                self.update_palette(Action::RunPane(target.card, command))
            }
            Action::RunPane(id, command) => match self.pane_command_message(id, command) {
                Some(message) => self.update(message),
                None => {
                    self.status = Some(TransientNotice::warning(
                        "This pane command is no longer available",
                    ));
                    Task::none()
                }
            },
            Action::FocusPane(id) => {
                let Some(card) = self.card_state(id).filter(|card| card.existence.is_open()) else {
                    return Task::none();
                };
                let channel = self
                    .durable
                    .channels
                    .iter()
                    .find(|channel| channel.cards.contains(&id))
                    .map(|channel| channel.id);
                let path = self.card_worktree(card).map(Path::to_path_buf).or_else(|| self.active_workspace.clone());
                let navigation = if self.workspace.order().any(|candidate| candidate == id) {
                    Task::none()
                } else if let Some(channel) = channel {
                    self.select_channel(channel)
                } else if let Some(path) = path {
                    self.select_worktree(path, false)
                } else {
                    Task::none()
                };
                navigation.chain(self.update(Message::FocusCard(id)))
            }
            Action::Submit => {
                let entry = self
                    .palette_entries()
                    .into_iter()
                    .nth(self.palette.selected);
                match entry {
                    Some((_, _, message)) => self.run_palette_command(message),
                    None => self.update_palette(Action::Close),
                }
            }
        }
    }

    fn run_palette_command(&mut self, message: Message) -> Task<Message> {
        let key = entry_key(&message);
        self.settings
            .palette_recents
            .retain(|recent| recent != &key);
        self.settings.palette_recents.insert(0, key);
        self.settings.palette_recents.truncate(24);
        self.palette.query = None;
        let command = self.update(message);
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
        if self.palette.recording.is_some() || self.palette.filtering_key {
            if is_escape_press(event) {
                self.palette.recording = None;
                self.palette.filtering_key = false;
                self.palette.reassign = None;
                self.palette.recording_error = None;
                return Some(Task::none());
            }
            if let keyboard::Event::KeyPressed { key, modifiers, .. } = event {
                let name = match key {
                    keyboard::Key::Character(value) if value.chars().count() == 1 => {
                        Some(value.to_uppercase())
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
                        return Some(self.update(Message::SettingsSearchChanged(chord)));
                    }
                    return Some(self.record_shortcut(self.palette.recording.expect("recording field"), chord));
                }
            }
            return Some(Task::none());
        }
        self.palette.query.as_ref()?;
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
            let count = self.palette_entries().len();
            match key {
                keyboard::key::Named::ArrowDown if count > 0 => {
                    self.palette.selected = (self.palette.selected + 1) % count;
                    return Some(self.reveal_palette_selection());
                }
                keyboard::key::Named::ArrowUp if count > 0 => {
                    self.palette.selected = (self.palette.selected + count - 1) % count;
                    return Some(self.reveal_palette_selection());
                }
                keyboard::key::Named::Enter => return Some(self.update_palette(Action::Submit)),
                _ => {}
            }
        }
        Some(Task::none())
    }

    fn record_shortcut(&mut self, field: SettingsField, chord: String) -> Task<Message> {
        self.palette.reassign = None;
        let action = field.shortcut_action().expect("shortcut field");
        let value = self.settings_draft.keyboard_shortcuts.shortcut_values().into_iter().find(|(candidate, _)| *candidate == action).expect("shortcut draft").1;
        let mut chords = settings::parse_chords(value);
        if !chords.iter().any(|existing| settings::parse_shortcut(existing).ok() == settings::parse_shortcut(&chord).ok()) {
            chords.push(chord.clone());
        }
        let value = settings::format_chords(&chords);
        match settings_update(&self.settings, field, &value) {
            SettingsDecision::Invalid(reason) => {
                self.palette.recording_error = Some(reason);
                if let Ok(binding) = settings::parse_shortcut(&chord)
                    && settings::shortcut_chord_refusal(binding).is_none()
                    && let Some(owner) = self.settings.keyboard_shortcuts.actions().iter().position(|chords| chords.iter().any(|candidate| settings::parse_shortcut(candidate).ok() == Some(binding)))
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

    fn reveal_palette_selection(&self) -> Task<Message> {
        crate::interaction::reveal_list_selection(
            widget::Id::new("palette.commands"),
            self.palette.selected,
            ROW_HEIGHT,
            ROW_SPACING,
        )
    }

    fn pane_attention_label(&self, card: CardId) -> &'static str {
        use sidebar::AttentionState;
        let unread = self.sidebar_state.has_card_update(card);
        match self.sidebar_state.card_attention(card) {
            Some(AttentionState::Working) if unread => "Working · Unread",
            Some(AttentionState::Working) => "Working",
            Some(AttentionState::Waiting) => "Waiting for input",
            Some(AttentionState::Failed) => self
                .sidebar_state
                .card_outcome(card)
                .map_or("Failed", crate::agent::ActivityOutcome::label),
            Some(AttentionState::Cancelled) => "Cancelled",
            Some(AttentionState::Unread)
                if matches!(
                    self.sidebar_state.card_outcome(card),
                    Some(crate::agent::ActivityOutcome::Cancelled)
                ) =>
            {
                "Cancelled · Unread"
            }
            Some(AttentionState::Unread) => "Unread",
            None => "Idle",
        }
    }

    fn palette_entries(&self) -> Vec<(String, String, Message)> {
        let (category, query) = palette_query(self.palette.query.as_deref().unwrap_or(""));
        let mut entries: Vec<_> = if !self.palette.panes_only && category.is_none_or(|category| category == '>') {
            let context = self.command_context(self.workspace.focused());
            workspace_widget::GLOBAL_SHORTCUT_DESCRIPTORS
            .iter()
            .filter(|descriptor| descriptor.action != Command::CommandPalette)
            .filter(|descriptor| descriptor.available(&context))
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
            .collect()
        } else { Vec::new() };
        let current_order: HashMap<_, _> = self
            .workspace
            .order()
            .enumerate()
            .map(|(index, id)| (id, index))
            .collect();
        if category.is_none_or(|category| category == '@') {
        for card in self.durable.cards.iter().filter(|card| {
            card.existence.is_open()
                && (!self.palette.panes_only || current_order.contains_key(&card.id))
        }) {
            let place = self
                .durable
                .channels
                .iter()
                .find(|channel| channel.cards.contains(&card.id))
                .map(|channel| channel.title.clone())
                .or_else(|| {
                    self.card_worktree(card).map(|path| {
                        path.file_name()
                            .unwrap_or(path.as_os_str())
                            .to_string_lossy()
                            .into_owned()
                    })
                })
                .unwrap_or_else(|| "Global".into());
            let status = if self.sidebar_state.has_card_update(card.id) {
                "● Unread"
            } else {
                match self
                    .sidebar_state
                    .card_activity(card.id)
                    .map(|activity| activity.state)
                {
                    Some(crate::agent::ActivityState::Working) => "◌ Working",
                    Some(crate::agent::ActivityState::Waiting) => "! Waiting",
                    _ => "○ Idle",
                }
            };
            entries.push((
                if self.palette.panes_only {
                    self.card_display_title(card).to_owned()
                } else {
                    format!("Focus pane: {} · {}", self.card_display_title(card), place)
                },
                status.into(),
                Message::Palette(Action::FocusPane(card.id)),
            ));
        }
        }
        if !self.palette.panes_only && category.is_none_or(|category| category == '#') {
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
        }

        entries.retain(|(label, _, message)| {
            category.is_none_or(|category| entry_category(message) == category)
                && match_score(search_label(label), query).is_some()
        });
        if self.palette.panes_only && query.is_empty() {
            entries.sort_by_key(|(_, _, message)| match message {
                Message::Palette(Action::FocusPane(id)) => {
                    current_order.get(id).copied().unwrap_or(usize::MAX)
                }
                _ => usize::MAX,
            });
        } else {
            entries.sort_by_cached_key(|(label, _, message)| {
                let recent = self
                    .settings
                    .palette_recents
                    .iter()
                    .position(|key| key == &entry_key(message))
                    .unwrap_or(usize::MAX);
                let common = match message {
                    Message::RunCommand(Command::NewThread) => 0,
                    Message::RunCommand(Command::NewTerminal) => 1,
                    Message::RunCommand(Command::OpenSettings) => 2,
                    _ => 3,
                };
                (
                    match_score(search_label(label), query).unwrap_or(usize::MAX),
                    recent,
                    common,
                )
            });
        }
        entries
    }

    pub(super) fn palette_surface(&self) -> Option<Element<'_, Message>> {
        if self.palette.recording.is_some() || self.palette.filtering_key {
            let mut children: Vec<Element<'_, Message>> = vec![design::body("Press a shortcut · Escape cancels").into()];
            if let Some(reason) = &self.palette.recording_error {
                children.push(design::body(reason).class(cosmic::theme::Text::Custom(design::destructive_text)).into());
            }
            if let Some((_, _, owner)) = &self.palette.reassign {
                children.push(widget::button::text(format!("Reassign from {}", SettingsField::SHORTCUTS[*owner].label())).on_press(Message::Palette(Action::Reassign)).into());
            }
            return Some(widget::container(widget::column::with_children(children).spacing(space::S)).class(cosmic::theme::Container::Card).padding(20).width(480).into());
        }
        let query = self.palette.query.as_ref()?;
        let input = crate::ui_controls::field::search(
            if self.palette.panes_only {
                "Search open panes"
            } else {
                "Search · > commands · @ panes · # workspaces"
            },
            query,
        )
        .id(widget::Id::new("palette.query"))
        .on_input(|query| Message::Palette(Action::Query(query)));
        let entries = self.palette_entries();
        let rows: Vec<Element<'_, Message>> = entries
            .into_iter()
            .enumerate()
            .map(|(index, (label, chord, message))| {
                let pane = match &message {
                    Message::Palette(Action::FocusPane(id)) => Some(*id),
                    _ => None,
                };
                let (_, needle) = palette_query(query);
                let label = highlighted_label(&label, needle);
                let mut children = Vec::new();
                if let Some(card) = pane {
                    children.push(
                        self.card_icon(card)
                            .icon()
                            .size(design::ROW_ICON_SIZE)
                            .into(),
                    );
                    if let Some(indicator) = sidebar::outcome_indicator(
                        self.sidebar_state
                            .card_activity(card)
                            .map(|activity| activity.state),
                        self.sidebar_state.has_card_update(card),
                        self.sidebar_state.card_outcome(card),
                    ) {
                        children.push(indicator);
                    }
                }
                children.push(label);
                children.push(
                    design::caption(chord)
                        .wrapping(iced::widget::text::Wrapping::None)
                        .into(),
                );
                let row = widget::row::with_children(children)
                    .spacing(space::M)
                    .align_y(Alignment::Center);
                let button: Element<'_, Message> = widget::button::custom(
                    widget::container(row)
                        .height(Length::Fill)
                        .align_y(Alignment::Center),
                )
                .class(palette_row_style(index == self.palette.selected))
                .padding([0, 12])
                .width(Length::Fill)
                .height(ROW_HEIGHT)
                .on_press(Message::Palette(Action::Run(MessageBox(Box::new(message)))))
                .into();
                if self.palette.panes_only {
                    if let Some(card) = pane {
                        return widget::row::with_children(vec![
                            button,
                            crate::ui_controls::icon_button(Icon::Close, "Close pane")
                                .id(widget::Id::new(format!("pane-list.close.{card}")))
                                .on_press(Message::Palette(Action::ClosePane(card)))
                                .into(),
                        ])
                        .align_y(Alignment::Center)
                        .into();
                    }
                }
                button
            })
            .collect();
        let rows = if rows.is_empty() {
            widget::column::with_children(vec![
                design::caption(if self.palette.panes_only {
                    "No matching panes"
                } else {
                    "No matching commands"
                })
                .into(),
            ])
        } else {
            widget::column::with_children(rows)
        };
        Some(
            widget::container(
                widget::column::with_children(vec![
                    input.into(),
                    widget::container(
                        widget::scrollable(
                            widget::container(rows.spacing(ROW_SPACING)).padding([0, 16]),
                        )
                        .id(widget::Id::new("palette.commands"))
                        .height(Length::Shrink),
                    )
                    .max_height(360)
                    .into(),
                    design::caption("↑ ↓ choose · Enter run · Escape close").into(),
                ])
                .spacing(space::S),
            )
            .padding(space::M)
            .width(560)
            .class(cosmic::theme::Container::Card)
            .into(),
        )
    }

    fn command_project(&self) -> Option<&SidebarProject> {
        if let Some(channel) = self.durable.active_channel.and_then(|id| {
            self.durable
                .channels
                .iter()
                .find(|channel| channel.id == id)
        }) {
            return self.sidebar_model.project(channel.project_id);
        }
        let selected = self.active_workspace.as_deref().and_then(|path| {
            self.sidebar_model
                .projects
                .iter()
                .filter(|project| self.project_owns_active_scope(project.id, path))
                .max_by_key(|project| {
                    std::iter::once(project.repository.as_path())
                        .chain(
                            project
                                .worktrees
                                .iter()
                                .map(|worktree| worktree.path.as_path()),
                        )
                        .filter(|root| path.starts_with(root))
                        .map(|root| root.components().count())
                        .max()
                        .unwrap_or(0)
                })
        });
        selected.or_else(|| {
            (self.sidebar_model.projects.len() == 1).then(|| &self.sidebar_model.projects[0])
        })
    }

    /// Availability is shared by palette listing and execution. A stale menu
    /// or shortcut cannot turn an unavailable command into a silent no-op.
    fn command_context(&self, card: Option<CardId>) -> workspace_widget::CommandContext {
        let pane_index =
            card.and_then(|id| self.workspace.order().position(|candidate| candidate == id));
        let state = card.and_then(|id| self.card_state(id));
        workspace_widget::CommandContext {
            focused: if card == self.workspace.focused() {
                self.focused_pane()
            } else {
                match card.and_then(|id| self.cards.get(&id)) {
                    Some(CardRuntime::Terminal(_))
                        if state.is_some_and(|state| ended_terminal(Some(state)).is_none()) =>
                    {
                        workspace_widget::FocusedPane::Terminal
                    }
                    Some(CardRuntime::Chat(_)) => workspace_widget::FocusedPane::Chat,
                    Some(CardRuntime::PullRequests(_)) => workspace_widget::FocusedPane::Review,
                    Some(CardRuntime::Repository(runtime)) if runtime.shows_diff() => {
                        workspace_widget::FocusedPane::Review
                    }
                    _ => workspace_widget::FocusedPane::Other,
                }
            },
            pane_index,
            pane_count: self.workspace.order().count(),
            project_count: self.sidebar_model.projects.len(),
            sidebar_has_rows: !sidebar::visible_selection_rows(
                &self.sidebar_model,
                &self.sidebar_state,
                self.projects_sidebar_mode() == SidebarMode::Compact,
            )
            .is_empty(),
            active_git_project: self
                .command_project()
                .is_some_and(|project| project.kind != ProjectKind::Folder),
            pane_has_path: card
                .is_some_and(|id| matches!(self.cards.get(&id), Some(CardRuntime::Repository(_))))
                || state.and_then(super::worktree_ui::card_directory).is_some(),
            pane_revealable: pane_index.is_some() && self.active_workspace_destination().is_some(),
            pane_cloneable: !self.focused_repository_is_saving() && state.is_some_and(|state| {
                state.existence.is_open()
                    && !state.kind.is_utility()
                    && !self.project_ui.deleting_card(state)
            }),
            can_fork: card.and_then(|id| self.chat_fork_target(id)).is_some(),
            chat_account: card.is_some_and(|id| self.chat_control_available(id, Command::ChatAccount)),
            chat_model: card.is_some_and(|id| self.chat_control_available(id, Command::ChatModel)),
            chat_permissions: card.is_some_and(|id| self.chat_control_available(id, Command::ChatPermissions)),
            can_switch: card
                .is_some_and(|id| self.chat_switch_available(id) && self.chat_switch_ready(id)),
            can_review: card.and_then(|id| self.chat_changes_target(id)).is_some(),
            is_pull_request: card.is_some_and(|id| matches!(self.cards.get(&id), Some(CardRuntime::PullRequests(_)))) ,
            can_find: card.is_some_and(|id| match self.cards.get(&id) {
                Some(CardRuntime::Repository(runtime)) => runtime.editor_focus_target(id).is_some(),
                Some(CardRuntime::Chat(_) | CardRuntime::Settings | CardRuntime::Terminal(_)) => {
                    true
                }
                _ => false,
            }),
            daemon_can_restart: !crate::device::is_remote(),
            can_reopen: self
                .recently_closed
                .iter()
                .any(|(_, scope)| *scope == self.active_workspace_destination()),
        }
    }

    pub(super) fn command_available(&self, command: Command) -> bool {
        workspace_widget::shortcut_descriptor(command)
            .available(&self.command_context(self.workspace.focused()))
    }

    pub(super) fn pane_command_available(&self, card: CardId, command: Command) -> bool {
        workspace_widget::shortcut_descriptor(command).available(&self.command_context(Some(card)))
    }

    /// Pane menus carry their original target, independently of current focus.
    pub(super) fn pane_command_message(&self, id: CardId, command: Command) -> Option<Message> {
        if !self.pane_command_available(id, command) {
            return None;
        }
        Some(match command {
            Command::RenamePane => Message::ChromeUi(chrome_ui::Action::Rename(id)),
            Command::ClonePane => Message::ChromeUi(chrome_ui::Action::Clone(id)),
            Command::CloseFocusedCard => Message::CloseCard(id),
            Command::CloseOtherPanes => Message::ChromeUi(chrome_ui::Action::CloseOthers(id)),
            Command::ClosePanesToRight => Message::ChromeUi(chrome_ui::Action::CloseToRight(id)),
            Command::CopyPath => Message::ChromeUi(chrome_ui::Action::CopyPath(id)),
            Command::RevealInProjects => Message::ChromeUi(chrome_ui::Action::Reveal(id)),
            Command::SwitchPane => Message::ChatUi(chat_ui::Action::Switch(id)),
            Command::ForkConversation => Message::ChatUi(chat_ui::Action::BranchPicker(
                self.chat_fork_target(id)?,
                true,
            )),
            Command::ReviewConversationChanges => Message::ChatUi(chat_ui::Action::ChangesPicker(
                self.chat_changes_target(id)?,
                true,
            )),
            _ => return None,
        })
    }

    fn run_registered_command(&mut self, command: Command) -> Task<Message> {
        if let Some(index) = project_command_index(command) {
            let Some(project) = self.sidebar_model.projects.get(index) else {
                return Task::none();
            };
            return self.update(Message::Sidebar(SidebarAction::OpenRepository {
                project_id: project.id,
                repository: project.repository.clone(),
            }));
        }
        if let Some(category) = settings_command_category(command) {
            let open = self.update(Message::Sidebar(SidebarAction::OpenSettings));
            return Task::batch([open, self.update(Message::SelectSettingsCategory(category))]);
        }
        if let Some(message) = self
            .workspace
            .focused()
            .and_then(|id| self.pane_command_message(id, command))
        {
            return self.update(message);
        }
        match command {
            Command::NewBrowser => self.update(Message::NewBrowser),
            Command::OpenFolder => self.update(Message::OpenRepository),
            Command::NewWorktree => {
                let Some(project) = self.command_project() else {
                    return Task::none();
                };
                self.update(Message::Sidebar(SidebarAction::NewWorktree {
                    project_id: project.id,
                    repository: project.repository.clone(),
                }))
            }
            Command::ResumeConversation => {
                self.update(Message::ResumeConversation(resume_ui::Action::Open))
            }
            Command::FocusSidebarTree => {
                self.chrome_ui.projects_collapsed = false;
                let overlay = self.sync_context_overlay_state();
                let rows = sidebar::visible_selection_rows(
                    &self.sidebar_model,
                    &self.sidebar_state,
                    self.projects_sidebar_mode() == SidebarMode::Compact,
                );
                let row = self
                    .sidebar_state
                    .selected_rows(&rows)
                    .first()
                    .copied()
                    .or_else(|| rows.first().copied());
                Task::batch([
                    overlay,
                    row.map_or_else(Task::none, |row| {
                        sidebar::focus_and_reveal(sidebar::selection_row_focus_id(row))
                            .map(cosmic::Action::App)
                    }),
                ])
            }
            Command::RestartDaemon => self.update(Message::DaemonHealthUi(
                daemon_health_ui::Action::OpenRequested,
            )),
            Command::ToggleTheme => {
                let theme = if self
                    .settings
                    .app_theme
                    .resolve(self.core.system_theme())
                    .theme_type
                    .is_dark()
                {
                    AppTheme::Light
                } else {
                    AppTheme::Dark
                };
                self.apply_setting_input(SettingsField::AppTheme, theme.key().into())
            }
            _ => Task::none(),
        }
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
        if !self.command_available(command) {
            self.status = Some(TransientNotice::warning(format!(
                "{} is unavailable in the current context",
                workspace_widget::shortcut_descriptor(command).label
            )));
            return Task::none();
        }
        match command {
            Command::NewBrowser
            | Command::OpenFolder
            | Command::NewWorktree
            | Command::CloseOtherPanes
            | Command::ClosePanesToRight
            | Command::CopyPath
            | Command::RevealInProjects
            | Command::ForkConversation
            | Command::FocusSidebarTree
            | Command::RestartDaemon
            | Command::ToggleTheme
            | Command::ResumeConversation
            | Command::SwitchPane
            | Command::ReviewConversationChanges
            | Command::FocusProject1
            | Command::FocusProject2
            | Command::FocusProject3
            | Command::FocusProject4
            | Command::FocusProject5
            | Command::FocusProject6
            | Command::FocusProject7
            | Command::FocusProject8
            | Command::FocusProject9
            | Command::SettingsGeneral
            | Command::SettingsAppearance
            | Command::SettingsTerminal
            | Command::SettingsLinks
            | Command::SettingsKeyboard
            | Command::SettingsNativeChat
            | Command::SettingsEditor
            | Command::SettingsAccounts
            | Command::SettingsNotifications
            | Command::SettingsAbout => self.run_registered_command(command),
            Command::ChatAccount | Command::ChatModel | Command::ChatPermissions => {
                self.open_chat_control(command)
            }
            Command::NextAttention => self.cycle_attention(false),
            Command::PreviousAttention => self.cycle_attention(true),
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
                        self.update(Message::FocusProjectSearch)
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
            Command::FocusLastPane => {
                let card = self.workspace.order().last();
                card.map_or_else(Task::none, |id| self.update(Message::FocusCard(id)))
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
            Self::ShortcutNewBrowser => Some(Command::NewBrowser),
            Self::ShortcutOpenFolder => Some(Command::OpenFolder),
            Self::ShortcutNewWorktree => Some(Command::NewWorktree),
            Self::ShortcutCloseOtherPanes => Some(Command::CloseOtherPanes),
            Self::ShortcutClosePanesToRight => Some(Command::ClosePanesToRight),
            Self::ShortcutCopyPath => Some(Command::CopyPath),
            Self::ShortcutRevealInProjects => Some(Command::RevealInProjects),
            Self::ShortcutForkConversation => Some(Command::ForkConversation),
            Self::ShortcutFocusSidebarTree => Some(Command::FocusSidebarTree),
            Self::ShortcutRestartDaemon => Some(Command::RestartDaemon),
            Self::ShortcutToggleTheme => Some(Command::ToggleTheme),
            Self::ShortcutResumeConversation => Some(Command::ResumeConversation),
            Self::ShortcutSwitchPane => Some(Command::SwitchPane),
            Self::ShortcutReviewConversationChanges => Some(Command::ReviewConversationChanges),
            Self::ShortcutFocusProject1 => Some(Command::FocusProject1),
            Self::ShortcutFocusProject2 => Some(Command::FocusProject2),
            Self::ShortcutFocusProject3 => Some(Command::FocusProject3),
            Self::ShortcutFocusProject4 => Some(Command::FocusProject4),
            Self::ShortcutFocusProject5 => Some(Command::FocusProject5),
            Self::ShortcutFocusProject6 => Some(Command::FocusProject6),
            Self::ShortcutFocusProject7 => Some(Command::FocusProject7),
            Self::ShortcutFocusProject8 => Some(Command::FocusProject8),
            Self::ShortcutFocusProject9 => Some(Command::FocusProject9),
            Self::ShortcutSettingsGeneral => Some(Command::SettingsGeneral),
            Self::ShortcutSettingsAppearance => Some(Command::SettingsAppearance),
            Self::ShortcutSettingsTerminal => Some(Command::SettingsTerminal),
            Self::ShortcutSettingsLinks => Some(Command::SettingsLinks),
            Self::ShortcutSettingsKeyboard => Some(Command::SettingsKeyboard),
            Self::ShortcutSettingsNativeChat => Some(Command::SettingsNativeChat),
            Self::ShortcutSettingsEditor => Some(Command::SettingsEditor),
            Self::ShortcutSettingsAccounts => Some(Command::SettingsAccounts),
            Self::ShortcutSettingsNotifications => Some(Command::SettingsNotifications),
            Self::ShortcutSettingsAbout => Some(Command::SettingsAbout),

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
            Self::ShortcutNextAttention => Some(Command::NextAttention),
            Self::ShortcutPreviousAttention => Some(Command::PreviousAttention),
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
            Self::ShortcutFocusLastPane => Some(Command::FocusLastPane),
            _ => None,
        }
    }
}
impl ShortcutDraft {
    pub(super) fn shortcut_values(&self) -> Vec<(Command, &str)> {
        vec![
            (Command::ToggleProjects, self.toggle_projects.as_str()),
            (Command::RenamePane, self.rename_pane.as_str()),
            (Command::NewBrowser, self.new_browser.as_str()),
            (Command::OpenFolder, self.open_folder.as_str()),
            (Command::NewWorktree, self.new_worktree.as_str()),
            (Command::CloseOtherPanes, self.close_other_panes.as_str()),
            (
                Command::ClosePanesToRight,
                self.close_panes_to_right.as_str(),
            ),
            (Command::CopyPath, self.copy_path.as_str()),
            (Command::RevealInProjects, self.reveal_in_projects.as_str()),
            (Command::ForkConversation, self.fork_conversation.as_str()),
            (Command::FocusSidebarTree, self.focus_sidebar_tree.as_str()),
            (Command::RestartDaemon, self.restart_daemon.as_str()),
            (Command::ToggleTheme, self.toggle_theme.as_str()),
            (
                Command::ResumeConversation,
                self.resume_conversation.as_str(),
            ),
            (Command::SwitchPane, self.switch_pane.as_str()),
            (
                Command::ReviewConversationChanges,
                self.review_conversation_changes.as_str(),
            ),
            (Command::FocusProject1, self.focus_project_1.as_str()),
            (Command::FocusProject2, self.focus_project_2.as_str()),
            (Command::FocusProject3, self.focus_project_3.as_str()),
            (Command::FocusProject4, self.focus_project_4.as_str()),
            (Command::FocusProject5, self.focus_project_5.as_str()),
            (Command::FocusProject6, self.focus_project_6.as_str()),
            (Command::FocusProject7, self.focus_project_7.as_str()),
            (Command::FocusProject8, self.focus_project_8.as_str()),
            (Command::FocusProject9, self.focus_project_9.as_str()),
            (Command::SettingsGeneral, self.settings_general.as_str()),
            (
                Command::SettingsAppearance,
                self.settings_appearance.as_str(),
            ),
            (Command::SettingsTerminal, self.settings_terminal.as_str()),
            (Command::SettingsLinks, self.settings_links.as_str()),
            (Command::SettingsKeyboard, self.settings_keyboard.as_str()),
            (
                Command::SettingsNativeChat,
                self.settings_native_chat.as_str(),
            ),
            (Command::SettingsEditor, self.settings_editor.as_str()),
            (Command::SettingsAccounts, self.settings_accounts.as_str()),
            (
                Command::SettingsNotifications,
                self.settings_notifications.as_str(),
            ),
            (Command::SettingsAbout, self.settings_about.as_str()),
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
            (Command::FocusLastPane, self.focus_last_pane.as_str()),
        ]
    }
}

#[cfg(test)]
mod tests {

    #[test]
    fn pane_list_uses_shared_attention_ranking_and_preserves_unread_work() {
        use crate::agent::{Activity, ActivityOutcome, ActivityState, Provider};
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let card = app.workspace.focused().unwrap();
        app.sidebar_state.set_card_activity(
            card,
            Activity {
                provider: Provider::Codex,
                state: ActivityState::Working,
            },
        );
        app.sidebar_state.observe_card_update(card);
        assert_eq!(app.pane_attention_label(card), "Working · Unread");
        app.sidebar_state.set_card_outcome(
            card,
            Some(ActivityOutcome::Failed {
                reason: "exit3".into(),
            }),
        );
        assert_eq!(app.pane_attention_label(card), "Failed");
        app.sidebar_state.set_card_outcome(
            card,
            Some(ActivityOutcome::Interrupted {
                reason: "no final result".into(),
            }),
        );
        assert_eq!(app.pane_attention_label(card), "Interrupted");
        app.sidebar_state.set_card_activity(
            card,
            Activity {
                provider: Provider::Codex,
                state: ActivityState::Waiting,
            },
        );
        assert_eq!(app.pane_attention_label(card), "Waiting for input");
        app.sidebar_state.set_card_activity(
            card,
            Activity {
                provider: Provider::Codex,
                state: ActivityState::Idle,
            },
        );
        app.sidebar_state
            .set_card_outcome(card, Some(ActivityOutcome::Cancelled));
        assert_eq!(app.pane_attention_label(card), "Cancelled · Unread");
    }

    #[test]
    fn pane_list_filters_selects_and_focuses_the_chosen_pane() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let cards: Vec<_> = (1..=20)
            .map(|index| {
                let mut card = crate::app::tests::terminal_card_state();
                card.title = format!("Pane {index}");
                card
            })
            .collect();
        let chosen = cards[13].id;
        Arc::make_mut(&mut app.durable).cards = cards;
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(900.0);
        drop(app.update_palette(Action::OpenPanes));
        assert_eq!(app.palette_entries().len(), 20);
        drop(app.update_palette(Action::Query("pane14".into())));
        let entries = app.palette_entries();
        assert_eq!(entries.len(), 1);
        assert!(matches!(entries[0].2, Message::FocusCard(id) if id == chosen));
        drop(app.update_palette(Action::Submit));
        assert!(!app.palette.is_open());
        assert_eq!(app.workspace.focused(), Some(chosen));
        drop(app.run_workspace_command(Command::FocusLastPane, keyboard::Modifiers::empty()));
        assert_eq!(app.workspace.focused(), app.workspace.order().last());
    }
    use super::*;

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
    fn pane_picker_stays_in_the_active_workspace_and_preserves_strip_order() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let mut cards = (1..=20)
            .map(|index| {
                let mut card = super::super::tests::terminal_card_state();
                card.title = format!("Pane {index}");
                card
            })
            .collect::<Vec<_>>();
        let hidden = cards.pop().unwrap();
        let chosen = cards[13].id;
        Arc::make_mut(&mut app.durable).cards = cards;
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.workspace.resize_viewport(900.0);
        Arc::make_mut(&mut app.durable).cards.push(hidden.clone());
        drop(app.update_palette(Action::OpenPanes));
        let entries = app.palette_entries();
        assert_eq!(entries.len(), 19);
        assert!(!entries.iter().any(
            |entry| matches!(entry.2, Message::Palette(Action::FocusPane(id)) if id == hidden.id)
        ));
        assert_eq!(
            entries
                .iter()
                .filter_map(|entry| match entry.2 {
                    Message::Palette(Action::FocusPane(id)) => Some(id),
                    _ => None,
                })
                .collect::<Vec<_>>(),
            app.workspace.order().collect::<Vec<_>>()
        );
        drop(app.update_palette(Action::Query("pane14".into())));
        assert_eq!(app.palette_entries().len(), 1);
        drop(app.update_palette(Action::Submit));
        assert_eq!(app.workspace.focused(), Some(chosen));
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query("@Pane20".into())));
        assert_eq!(
            app.palette_entries().len(),
            1,
            "the global palette still covers inactive panes"
        );
    }

    #[test]
    fn scorer_orders_exact_prefix_boundary_substring_and_subsequence() {
        let labels = [
            "refactor",
            "refactor ui · alpha",
            "alpha refactor",
            "arefactor",
            "Investigate task in alpha-wt-refactor-ui",
            "r e f a c t o r",
        ];
        let scores: Vec<_> = labels
            .iter()
            .map(|label| match_score(label, "refactor").unwrap())
            .collect();
        assert!(scores[0] < scores[1]);
        assert!(scores[1] < scores[2]);
        assert!(scores[2] < scores[3]);
        assert!(scores[3] < scores[5]);
        assert!(scores[1] < scores[4]);
        assert_eq!(match_score("migration", "refactor"), None);
        assert!(match_score("Réfactor UI", "réfactor").is_some());
    }

    #[test]
    fn refactor_workspace_ranks_before_the_loose_channel_match() {
        let mut app = settings_focus_fixture();
        let project_id = ProjectId::new();
        let worktree_id = crate::model::WorktreeId::new();
        let mut project = SidebarProject::new(
            project_id,
            crate::repository::RepositoryId::new(),
            "alpha".into(),
            "/alpha".into(),
            vec![sidebar::SidebarWorktree::new(
                worktree_id,
                "/alpha/refactor ui".into(),
                None,
                false,
                false,
                None,
                None,
                None,
            )],
            Vec::new(),
        );
        project.channels.push(sidebar::SidebarChannel::new(
            crate::model::ChannelId::new(),
            "Investigate task in alpha-wt-refactor-ui".into(),
            false,
            None,
            false,
            0,
            false,
            1,
        ));
        app.sidebar_model.projects = vec![project];
        drop(app.update_palette(Action::Open));
        drop(app.update_palette(Action::Query("refactor".into())));
        let entries = app.palette_entries();
        assert!(
            matches!(entries[0].2, Message::Sidebar(SidebarAction::OpenWorktree { worktree_id: id, .. }) if id == worktree_id)
        );
    }

    #[test]
    fn empty_query_lists_recent_commands_in_mru_order_and_prefixes_filter() {
        let mut app = settings_focus_fixture();
        app.settings.palette_recents = vec![
            entry_key(&Message::RunCommand(Command::OpenSettings)),
            entry_key(&Message::RunCommand(Command::NewTerminal)),
        ];
        let _ = app.update_palette(Action::Open);
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
    fn selecting_a_command_updates_bounded_mru_without_duplicate_entries() {
        let mut app = settings_focus_fixture();
        app.settings.palette_recents = (0..30).map(|index| format!("retired:{index}")).collect();
        drop(app.run_palette_command(Message::RunCommand(Command::OpenSettings)));
        let selected = entry_key(&Message::RunCommand(Command::OpenSettings));
        assert_eq!(app.settings.palette_recents.first(), Some(&selected));
        assert_eq!(app.settings.palette_recents.len(), 24);
        drop(app.run_palette_command(Message::RunCommand(Command::OpenSettings)));
        assert_eq!(app.settings.palette_recents.len(), 24);
        assert_eq!(app.settings.palette_recents.iter().filter(|key| **key == selected).count(), 1);
        drop(app.update_palette(Action::Open));
        assert!(matches!(app.palette_entries()[0].2, Message::RunCommand(Command::OpenSettings)));
    }

    #[test]
    fn panes_in_an_inactive_workspace_can_be_found_and_focused() {
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
        let _ = app.update_palette(Action::Open);
        let _ = app.update_palette(Action::Query("@migration".into()));
        let entries = app.palette_entries();
        assert_eq!(entries.len(), 1);
        assert!(entries[0].0.contains("palette-inactive-worktree"));
        assert!(matches!(entries[0].2, Message::Palette(Action::FocusPane(id)) if id == other.id));
        let _ = app.update_palette(Action::FocusPane(other.id));
        assert_eq!(app.active_workspace, Some(path));
        assert_eq!(app.workspace.focused(), Some(other.id));
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
                message, Message::SettingsSearchChanged(value) if value == "x"
            )));

            let open = app.update_palette(Action::Open);
            let _ = app.update_palette(Action::Query("zzqxv978unmatched".into()));
            assert!(app.palette_entries().is_empty());
            let while_open = focus_fixture_step(&app, &mut tree, &renderer, open, true);
            assert!(
                !while_open
                    .iter()
                    .any(|message| matches!(message, Message::SettingsSearchChanged(_)))
            );
            assert!(
                while_open
                    .iter()
                    .any(|message| matches!(message, Message::Palette(Action::Query(_))))
            );

            let dismiss = app.update_palette(dismiss);
            assert!(!app.palette.is_open());
            let after = focus_fixture_step(&app, &mut tree, &renderer, dismiss, true);
            assert!(after.iter().any(|message| matches!(
                message, Message::SettingsSearchChanged(value) if value == "x"
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

    #[test]
    fn a_global_command_targets_the_pane_with_native_keyboard_focus() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        let first = app.workspace.focused().unwrap();
        let mut second = app.card_state(first).unwrap().clone();
        second.id = CardId::new();
        let target = second.id;
        Arc::make_mut(&mut app.durable).cards.push(second);
        app.workspace = Cagents::rebuild_workspace(&app.durable);
        app.cards.insert(target, CardRuntime::Loading);
        app.input_trace = crate::input_trace::InputTrace::with_keyboard_region(crate::input_trace::KeyboardRegion { pane: workspace_widget::FocusedPane::Other, card: Some(target) });
        app.sidebar_model.projects.push(SidebarProject::new(ProjectId::new(), RepositoryId::new(), "Project".into(), std::env::temp_dir(), Vec::new(), Vec::new()));
        assert_eq!(app.workspace.focused(), Some(first));
        drop(app.update(Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: keyboard::Key::Character("f".into()), modified_key: keyboard::Key::Character("f".into()), physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyF), location: keyboard::Location::Standard, modifiers: keyboard::Modifiers::CTRL, text: None, repeat: false,
            }, captured: true,
        }));
        assert_eq!(app.workspace.focused(), Some(target));
        assert!(app.sidebar_state.controls_expanded());
    }

    #[test]
    fn find_without_a_pane_target_focuses_project_search() {
        let mut app = super::super::tests::context_inspector_test_app(true);
        app.context_inspector_open = false;
        app.cards.remove(&app.workspace.focused().unwrap());
        app.sidebar_model.projects.push(SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "Project".into(),
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
        ));
        assert!(!app.sidebar_state.controls_expanded());
        drop(app.run_workspace_command(Command::FindInPane, keyboard::Modifiers::CTRL));
        assert!(
            app.sidebar_state.controls_expanded(),
            "fallback must expose and focus project search"
        );
    }

    #[test]
    fn unavailable_commands_are_hidden_and_dismissal_restores_settings_input() {
        use iced::advanced::widget::Tree;

        let mut app = settings_focus_fixture();
        let before = app.workspace.order().collect::<Vec<_>>();
        let renderer = super::super::tests::headless_renderer();
        let mut tree = Tree::new(focus_fixture_view(&app).as_widget());
        let open = app.update_palette(Action::Open);
        let _ = app.update_palette(Action::Query("move pane left".into()));
        assert!(app.palette_entries().is_empty());
        let _ = focus_fixture_step(&app, &mut tree, &renderer, open, false);
        let submit = app.update_palette(Action::Submit);
        assert_eq!(app.workspace.order().collect::<Vec<_>>(), before);
        assert!(!app.palette.is_open());
        let after = focus_fixture_step(&app, &mut tree, &renderer, submit, true);
        assert!(after.iter().any(|message| matches!(
            message, Message::SettingsSearchChanged(value) if value == "x"
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
        assert!(matches!(
            app.palette_entries()[0].2,
            Message::RunCommand(Command::OpenSettings)
        ));
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
            message, Message::SettingsSearchChanged(value) if value == "x"
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
                    .any(|message| matches!(message, Message::SettingsSearchChanged(_)))
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
}

#[cfg(test)]
mod settings_recorder_tests {
    use super::*;

    #[test]
    fn recorder_refuses_shell_controls_and_fixed_chords_before_editing_the_draft() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        let field = SettingsField::ShortcutToggleProjects;
        let original = app.settings_draft.keyboard_shortcuts.toggle_projects.clone();
        for (chord, reason) in [("Ctrl+C", "interrupts"), ("Ctrl+Tab", "Cycle recent panes"), ("Ctrl+=", "Increase font size")] {
            drop(app.update_palette(Action::Record(field)));
            drop(app.record_shortcut(field, chord.into()));
            assert!(app.palette.recording_error.as_ref().is_some_and(|error| error.contains(reason)), "{chord}");
            assert_eq!(app.palette.recording, Some(field));
            assert_eq!(app.settings_draft.keyboard_shortcuts.toggle_projects, original);
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
            location: keyboard::Location::Standard, modifiers: keyboard::Modifiers::empty(), text: None, repeat: false,
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
        drop(app.queue_setting_input(SettingsField::ShortcutCommandPalette, format!("{chord}, Ctrl+Alt+Shift+P")));
        drop(app.update_palette(Action::Record(field)));
        drop(app.record_shortcut(field, chord.clone()));
        drop(app.update_palette(Action::Reassign));
        assert_eq!(app.settings.keyboard_shortcuts.toggle_projects, [chord]);
        assert_eq!(app.settings.keyboard_shortcuts.command_palette, ["Ctrl+Alt+Shift+P"]);
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
        assert!(app.settings.keyboard_shortcuts.toggle_projects.contains(&chord));
        assert!(app.settings_invalid.is_empty());
        assert!(app.settings.clone().validate().is_ok());
    }
}
