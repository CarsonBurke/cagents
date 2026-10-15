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
    sidebar_target: Option<sidebar::commands::CommandTarget>,
    move_pane: Option<CardId>,
    move_destinations: Vec<(crate::model::ChannelId, String)>,
    git_target: Option<sidebar::git_commands::GitTarget>,
    git_captured: bool,
    stash_picker: Option<Command>,
    stash_entries: Vec<crate::repository::StashEntry>,
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
    MovePane(CardId),
    RunGitStash(sidebar::git_commands::GitTarget, Command, crate::repository::StashEntry),
    MovePaneToChannel(CardId, crate::model::ChannelId),
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
const SECTION_HEIGHT: f32 = 26.0;

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
        Message::Palette(Action::FocusPane(_)) | Message::FocusAttention(_) => '@',
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
        Message::Palette(Action::RunGitStash(_, command, _)) => format!("command:{}", command.id()),
        Message::Palette(Action::MovePaneToChannel(_, _)) => format!("command:{}", Command::MovePaneToChannel.id()),
        Message::Palette(Action::FocusPane(id)) | Message::FocusCard(id) | Message::FocusAttention(id) => format!("pane:{id}"),
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
    let accent = design::accent_color(&cosmic::theme::active());
    let font = cosmic::font::default();
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
                    span.color(accent)
                        .font(iced::Font {
                            weight: iced::font::Weight::Bold,
                            ..font
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
        span.color(accent)
            .font(iced::Font {
                weight: iced::font::Weight::Bold,
                ..font
            })
    } else {
        span
    });
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
                self.palette.git_target = None;
                self.palette.git_captured = false;
                self.palette.stash_picker = None;
                self.palette.stash_entries.clear();
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
                self.palette.sidebar_target = None;
                self.palette.git_target = None;
                self.palette.git_captured = false;
                self.palette.stash_picker = None;
                self.palette.stash_entries.clear();
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
            Action::ClosePane(card) => self.run_palette_command(Message::CloseCard(card)),
            Action::Open | Action::OpenPanes => {
                if self.workspace_inputs_blocked() {
                    return Task::none();
                }
                self.palette.sidebar_target = self.capture_sidebar_command_target();
                self.palette.git_target = sidebar::git_commands::GitTarget::capture(&self.sidebar_model, &self.sidebar_state);
                self.palette.git_captured = true;
                self.palette.stash_picker = None;
                self.palette.stash_entries.clear();
                self.palette.panes_only = matches!(action, Action::OpenPanes);
                self.chrome_ui.close();
                self.palette.query = Some(String::new());
                self.palette.selected = 0;
                self.palette.move_pane = None;
                self.palette.move_destinations.clear();
                iced::widget::operation::focus(widget::Id::new("palette.query"))
                    .map(cosmic::Action::App)
                    .chain(self.reveal_palette_selection())
            }
            Action::RunGitStash(target, command, entry) => {
                if self.focused_repository_is_saving() || self.workspace_inputs_blocked() { return Task::none(); }
                match target.stash_action(&self.sidebar_model, &self.sidebar_state, command, &entry) {
                    Some(action) if !crate::device::is_remote() || device_ui::supports_sidebar_action(&action) => {
                        let open = command == Command::GitDropStash;
                        let task = self.update(Message::Sidebar(action));
                        if open { Task::batch([task, self.update(Message::SelectContextTab(ContextTab::Changes))]) } else { task }
                    }
                    _ => { self.status = Some(TransientNotice::warning("The original stash is no longer available")); Task::none() }
                }
            }
            Action::MovePaneToChannel(card, channel) => self.update(Message::MovePaneToChannel { card, channel }),
            Action::MovePane(card) => {
                if self.workspace_inputs_blocked() { return Task::none(); }
                let destinations: Vec<_> = self.pane_move_destinations(card).into_iter()
                    .map(|channel| (channel.id, self.pane_move_destination_label(channel).into_owned())).collect();
                if destinations.is_empty() { return Task::none(); }
                let task = self.update_palette(Action::Open);
                self.palette.move_pane = Some(card);
                self.palette.move_destinations = destinations;
                task
            }
            Action::Close => {
                self.palette.git_target = None;
                self.palette.git_captured = false;
                self.palette.stash_picker = None;
                self.palette.stash_entries.clear();
                self.palette.recording = None;
                self.palette.filtering_key = false;
                self.palette.query = None;
                self.palette.sidebar_target = None;
                self.palette.reassign = None;
                self.palette.recording_error = None;
                self.palette.move_pane = None;
                self.palette.move_destinations.clear();
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
        self.palette.move_pane = None;
        self.palette.move_destinations.clear();
        self.palette.stash_picker = None;
        let command = self.update(message);
        self.palette.sidebar_target = None;
        if self.palette.query.is_none() {
            self.palette.stash_entries.clear();
            self.palette.git_target = None;
            self.palette.git_captured = false;
        }
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
                    keyboard::Key::Character(value) if value.as_str() == " " => Some("Space".to_owned()),
                    keyboard::Key::Character(value) if value.chars().count() == 1 => {
                        Some(value.to_uppercase())
                    }
                    keyboard::Key::Named(name) => match name {
                        keyboard::key::Named::Space => Some("Space".into()),
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
                    return Some(
                        self.record_shortcut(
                            self.palette.recording.expect("recording field"),
                            chord,
                        ),
                    );
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
                    && settings::shortcut_chord_refusal_for(action.id(), binding).is_none()
                    && let Some(owner) =
                        self.settings
                            .keyboard_shortcuts
                            .actions()
                            .iter()
                            .enumerate()
                            .find_map(|(owner, chords)| {
                                (settings::shortcut_binding_conflicts(action.id(), KeyboardShortcuts::action_names()[owner], binding) && chords.iter().any(|candidate| settings::parse_shortcut(candidate).ok() == Some(binding))).then_some(owner)
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

    fn palette_section(&self, message: &Message) -> Option<&'static str> {
        if self.palette.panes_only || self.palette.move_pane.is_some() || self.palette.stash_picker.is_some() || !self.palette.query.as_deref().unwrap_or("").trim().is_empty() {
            return None;
        }
        Some(if self.settings.palette_recents.contains(&entry_key(message)) {
            "Recent"
        } else { match message {
            Message::FocusAttention(_) => "Needs attention",
            Message::RunCommand(_) => "Commands",
            Message::Palette(Action::FocusPane(_)) => "Panes",
            _ => "Workspaces",
        }})
    }

    fn reveal_palette_selection(&self) -> Task<Message> {
        let mut section = None;
        let mut top = 0.0;
        for (index, (_, _, message)) in self.palette_entries().iter().enumerate() {
            let next = self.palette_section(message);
            if next.is_some() && next != section {
                top += SECTION_HEIGHT + ROW_SPACING;
            }
            if index == self.palette.selected { break; }
            top += ROW_HEIGHT + ROW_SPACING;
            section = next;
        }
        crate::interaction::reveal_list_region(widget::Id::new("palette.commands"), top, ROW_HEIGHT)
    }

    fn palette_pane_label(&self, card: &CardState) -> String {
        let title = self.card_chrome.get(&card.id).map_or_else(|| self.card_display_title(card), |chrome| chrome.title.as_str());
        if self.palette.panes_only { return title.to_owned(); }
        let place = self.durable.channels.iter().find(|channel| channel.cards.contains(&card.id)).map(|channel| {
            self.sidebar_model.project(channel.project_id).map_or_else(|| channel.title.clone(), |project| {
                let branch = project.channels.iter().find(|row| row.id == channel.id).and_then(|row| row.worktree.as_ref()).map_or_else(String::new, |(_, branch)| format!(" · {branch}"));
                format!("{}{} · {}", channel.title, branch, project.name)
            })
        }).or_else(|| self.card_worktree(card).map(|path| {
            self.sidebar_model.worktree_at(path).and_then(|key| self.sidebar_model.project(key.project_id).map(|project| {
                let name = project.worktrees.iter().find(|worktree| worktree.id == key.worktree_id).map_or_else(|| path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned(), |worktree| worktree.label().to_owned());
                format!("{name} · {}", project.name)
            })).unwrap_or_else(|| path.file_name().unwrap_or(path.as_os_str()).to_string_lossy().into_owned())
        })).unwrap_or_else(|| "Global".into());
        format!("Focus pane: {title} · {place}")
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
        if let Some(command) = self.palette.stash_picker {
            let Some(target) = self.palette.git_target.as_ref() else { return Vec::new(); };
            if !target.available(&self.sidebar_model, &self.sidebar_state, Command::GitRefresh) { return Vec::new(); }
            let (_, query) = palette_query(self.palette.query.as_deref().unwrap_or(""));
            let mut entries: Vec<_> = self.palette.stash_entries.iter().filter_map(|entry| {
                let label = format!("{} · {}", entry.reference, entry.message);
                match_score(&label, query).map(|score| (score, label, Message::Palette(Action::RunGitStash(target.clone(), command, entry.clone()))))
            }).collect();
            if !query.is_empty() { entries.sort_by_key(|entry| entry.0); }
            return entries.into_iter().map(|(_, label, message)| (label, String::new(), message)).collect();
        }
        if let Some(card) = self.palette.move_pane {
            let (_, query) = palette_query(self.palette.query.as_deref().unwrap_or(""));
            let mut entries: Vec<_> = self.palette.move_destinations.iter().filter_map(|(channel, label)| {
                match_score(label, query).map(|score| (score, label.clone(), Message::Palette(Action::MovePaneToChannel(card, *channel))))
            }).collect();
            if !query.is_empty() { entries.sort_by_key(|entry| entry.0); }
            return entries.into_iter().map(|(_, label, message)| (label, String::new(), message)).collect();
        }
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
        let attention = if !self.palette.panes_only && category.is_none() && query.is_empty() {
            self.sidebar_state.attention_cards()
        } else { Vec::new() };
        for id in &attention {
            if let Some(card) = self.card_state(*id).filter(|card| card.existence.is_open()) {
                entries.push((self.palette_pane_label(card), self.pane_attention_label(*id).to_owned(), Message::FocusAttention(*id)));
            }
        }
        let current_order: HashMap<_, _> = self
            .workspace
            .order()
            .enumerate()
            .map(|(index, id)| (id, index))
            .collect();
        if category.is_none_or(|category| category == '@') {
        for card in self.durable.cards.iter().filter(|card| {
            card.existence.is_open()
                && !attention.contains(&card.id)
                && (!self.palette.panes_only || current_order.contains_key(&card.id))
        }) {
            let status = self.pane_attention_label(card.id);
            entries.push((
                self.palette_pane_label(card),
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
                        format!("Open channel: {} · {}{}", channel.title, project.name, channel.worktree.as_ref().map_or_else(String::new, |(_, branch)| format!(" · {branch}"))),
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
                    if query.is_empty() && category.is_none() {
                        if recent != usize::MAX { 0 } else { match message {
                            Message::FocusAttention(_) => 1,
                            Message::RunCommand(_) => 2,
                            Message::Palette(Action::FocusPane(_)) => 3,
                            _ => 4,
                        }}
                    } else { 0 },
                    recent,
                    // Attention order is authoritative: waiting, failures,
                    // then unread. Keep that order inside its section.
                    match message { Message::FocusAttention(id) => attention.iter().position(|card| card == id).unwrap_or(usize::MAX), _ => common },
                )
            });
        }
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
        let input = crate::ui_controls::field::search(
            if self.palette.stash_picker.is_some() {
                "Choose the exact stash"
            } else if self.palette.move_pane.is_some() {
                "Move pane to channel…"
            } else if self.palette.panes_only {
                "Search open panes"
            } else {
                "Search · > commands · @ panes · # workspaces"
            },
            query,
        )
        .id(widget::Id::new("palette.query"))
        .on_input(|query| Message::Palette(Action::Query(query)));
        let entries = self.palette_entries();
        let mut section = None;
        let rows: Vec<Element<'_, Message>> = entries
            .into_iter()
            .enumerate()
            .flat_map(|(index, (label, chord, message))| {
                let enabled = match &message {
                    Message::Palette(Action::RunGitStash(target, command, entry)) => target.stash_action(&self.sidebar_model, &self.sidebar_state, *command, entry).is_some(),
                    Message::Palette(Action::MovePaneToChannel(card, channel)) => self.can_move_pane_to_channel(*card, *channel),
                    _ => true,
                };
                let next_section = self.palette_section(&message);
                let heading: Option<Element<'_, Message>> = if next_section.is_some() && next_section != section {
                    Some(widget::container(design::caption(next_section.unwrap())).height(SECTION_HEIGHT).align_y(Alignment::Center).padding([0,12]).into())
                } else { None };
                section = next_section;
                let pane = match &message {
                    Message::Palette(Action::FocusPane(id)) | Message::FocusAttention(id) => Some(*id),
                    _ => None,
                };
                let (_, needle) = palette_query(query);
                // Leading ordinals stay visible even when a long OSC title is
                // ellipsized. Ranking continues to use the complete title.
                let display = pane.and_then(|id| self.card_chrome.get(&id)).and_then(|chrome| chrome.compact_title.as_ref().map(|title| label.replacen(&chrome.title, title, 1))).unwrap_or_else(|| label.clone());
                let label = highlighted_label(&display, needle);
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
                .on_press_maybe(enabled.then(|| Message::Palette(Action::Run(MessageBox(Box::new(message))))))
                .into();
                let button = if self.palette.panes_only && pane.is_some() {
                    let card = pane.unwrap();
                    widget::row::with_children(vec![
                            button,
                            crate::ui_controls::icon_button(Icon::Close, "Close pane")
                                .id(widget::Id::new(format!("pane-list.close.{card}")))
                                .on_press(Message::Palette(Action::ClosePane(card)))
                                .into(),
                        ])
                        .align_y(Alignment::Center)
                        .into()
                } else { button };
                heading.into_iter().chain(std::iter::once(button))
            })
            .collect();
        let rows = if rows.is_empty() {
            widget::column::with_children(vec![
                design::caption(if self.palette.stash_picker.is_some() {
                    "No matching stashes"
                } else if self.palette.move_pane.is_some() {
                    "No matching channels"
                } else if self.palette.panes_only {
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

    fn capture_sidebar_command_target(&self) -> Option<sidebar::commands::CommandTarget> {
        let pane = self.input_trace.keyboard_focus()?;
        let Some(widget) = self.input_trace.keyboard_widget_id() else {
            return matches!(pane, workspace_widget::FocusedPane::SidebarRows | workspace_widget::FocusedPane::SidebarFiles | workspace_widget::FocusedPane::SidebarChanges).then_some(sidebar::commands::CommandTarget::Unavailable);
        };
        match pane {
            workspace_widget::FocusedPane::SidebarRows => {
                let visible = sidebar::visible_selection_rows(&self.sidebar_model, &self.sidebar_state, self.projects_sidebar_mode() == SidebarMode::Compact);
                let Some(row) = visible.iter().copied().find(|row| sidebar::selection_row_focus_id(*row) == widget) else { return Some(sidebar::commands::CommandTarget::Unavailable); };
                let selected = self.sidebar_state.selected_rows(&visible);
                Some(sidebar::commands::CommandTarget::Rows(if selected.contains(&row) { selected } else { vec![row] }))
            }
            workspace_widget::FocusedPane::SidebarChanges => {
                let Some(source) = self.sidebar_model.source_control.as_ref() else { return Some(sidebar::commands::CommandTarget::Unavailable); };
                let Some(row) = sidebar::change_row_from_focus_id(source, &source.key, &widget) else { return Some(sidebar::commands::CommandTarget::Unavailable); };
                Some(sidebar::commands::CommandTarget::Changes { key: source.key.clone(), row,
                    selection_revision: self.sidebar_state.change_selection_revision() })
            }
            workspace_widget::FocusedPane::SidebarFiles => {
                let Some(source) = self.sidebar_model.source_control.as_ref() else { return Some(sidebar::commands::CommandTarget::Unavailable); };
                let Some(row) = sidebar::commands::file_rows(source).find(|row| sidebar::file_row_focus_id(&source.key, &row.path) == widget) else { return Some(sidebar::commands::CommandTarget::Unavailable); };
                Some(sidebar::commands::CommandTarget::File { key: source.key.clone(), path: row.path.clone() })
            }
            _ => None,
        }
    }

    fn sidebar_command_target(&self) -> Option<sidebar::commands::CommandTarget> {
        self.palette.sidebar_target.clone().or_else(|| self.capture_sidebar_command_target())
    }

    fn git_command_target(&self) -> Option<sidebar::git_commands::GitTarget> {
        if self.palette.git_captured { self.palette.git_target.clone() }
        else { sidebar::git_commands::GitTarget::capture(&self.sidebar_model, &self.sidebar_state) }
    }

    fn git_command_mask(&self) -> u32 {
        let Some(target) = self.git_command_target() else { return 0; };
        workspace_widget::GLOBAL_SHORTCUT_DESCRIPTORS.iter().filter_map(|descriptor| {
            let bit = workspace_widget::git_command_bit(descriptor.action)?;
            if !target.available(&self.sidebar_model, &self.sidebar_state, descriptor.action) { return None; }
            if crate::device::is_remote() {
                let action = target.action(&self.sidebar_model, &self.sidebar_state, descriptor.action)
                    .or_else(|| target.stashes(&self.sidebar_model, &self.sidebar_state).first().and_then(|entry| target.stash_action(&self.sidebar_model, &self.sidebar_state, descriptor.action, entry)))?;
                if !device_ui::supports_sidebar_action(&action) { return None; }
            }
            Some(bit)
        }).fold(0, |mask, bit| mask | bit)
    }

    fn sidebar_command_mask(&self, target: &sidebar::commands::CommandTarget) -> u64 {
        workspace_widget::GLOBAL_SHORTCUT_DESCRIPTORS.iter().filter_map(|descriptor| {
            let bit = descriptor.action.sidebar_command_bit()?;
            let action = target.resolve_in(&self.sidebar_model, &self.sidebar_state, descriptor.action)?;
            (!crate::device::is_remote() || device_ui::supports_sidebar_action(&action)).then_some(bit)
        }).fold(0, |mask, bit| mask | bit)
    }

    pub(super) fn run_captured_change_command(&mut self, command: Command, key: sidebar::ActiveWorktreeKey,
        row: sidebar::ChangeSelectionRow, selection_revision: u64) -> Task<Message> {
        let target = sidebar::commands::CommandTarget::Changes { key, row, selection_revision };
        match target.resolve_in(&self.sidebar_model, &self.sidebar_state, command) {
            Some(action) if !crate::device::is_remote() || device_ui::supports_sidebar_action(&action) => self.update(Message::Sidebar(action)),
            _ => { self.status = Some(TransientNotice::warning("The original change selection is no longer available")); Task::none() }
        }
    }

    pub(super) fn run_sidebar_registered_command(&mut self, command: Command, action: SidebarAction) -> Task<Message> {
        let valid = sidebar::commands::CommandTarget::from_action(&action)
            .is_some_and(|target| target.accepts(&self.sidebar_model, command, &action));
        if !valid || (crate::device::is_remote() && !device_ui::supports_sidebar_action(&action)) {
            self.status = Some(TransientNotice::warning("The original sidebar target is no longer available"));
            return Task::none();
        }
        self.update(Message::Sidebar(action))
    }

    fn review_command_capabilities(&self, card: CardId) -> u8 {
        let (runtime, mut flags) = match self.cards.get(&card) {
            Some(CardRuntime::Repository(runtime)) if runtime.shows_diff() => (runtime.as_ref(),
                if self.local_diff_file_target(card, true).is_some() { 2 } else { 0 }
                    | if self.local_diff_file_target(card, false).is_some() { 4 } else { 0 }),
            Some(CardRuntime::PullRequests(pane)) => (&pane.runtime,
                if pane.review.can_toggle_selected_viewed() { 1 } else { 0 }
                    | if pane.review.can_step_file(true) { 2 } else { 0 }
                    | if pane.review.can_step_file(false) { 4 } else { 0 }
                    | if pane.review.can_next_unviewed() { 8 } else { 0 }),
            _ => return 0,
        };
        if runtime.can_step_change(true) { flags |= 16; }
        if runtime.can_step_change(false) { flags |= 32; }
        flags
    }

    /// Availability is shared by palette listing and execution. A stale menu
    /// or shortcut cannot turn an unavailable command into a silent no-op.
    fn command_context(&self, card: Option<CardId>) -> workspace_widget::CommandContext {
        let pane_index =
            card.and_then(|id| self.workspace.order().position(|candidate| candidate == id));
        let state = card.and_then(|id| self.card_state(id));
        let save_in_flight = self.focused_repository_is_saving()
            || card.is_some_and(|id| matches!(self.cards.get(&id), Some(CardRuntime::Repository(runtime)) if runtime.is_saving()));
        workspace_widget::CommandContext {
            save_in_flight,
            sidebar_commands: self.sidebar_command_target().as_ref().map(|target| self.sidebar_command_mask(target)),
            git_commands: self.git_command_mask(),
            // The palette's search field owns native keyboard focus. Command
            // availability instead describes the originating pane's content.
            focused: match card.and_then(|id| self.cards.get(&id)) {
                Some(CardRuntime::Terminal(_)) if state.is_some_and(|state| ended_terminal(Some(state)).is_none()) => workspace_widget::FocusedPane::Terminal,
                Some(CardRuntime::Chat(_)) => workspace_widget::FocusedPane::Chat,
                Some(CardRuntime::PullRequests(_)) => workspace_widget::FocusedPane::Review,
                Some(CardRuntime::Repository(runtime)) if runtime.shows_diff() => workspace_widget::FocusedPane::Review,
                Some(CardRuntime::Repository(runtime)) if card.is_some_and(|card| runtime.editor_focus_target(card).is_some()) => workspace_widget::FocusedPane::Editor,
                Some(CardRuntime::Repository(_)) => workspace_widget::FocusedPane::RepositoryPane,
                _ if state.is_some_and(|state| matches!(state.kind, CardKind::Chat(_))) => workspace_widget::FocusedPane::ChatPane,
                _ => workspace_widget::FocusedPane::Other,
            },
            can_navigate_diagnostics: card.is_some_and(|id| matches!(self.cards.get(&id), Some(CardRuntime::Repository(runtime)) if runtime.diagnostic_navigation_available())),
            review_commands: card.map_or(0, |card| self.review_command_capabilities(card)),
            diff_commands: card.map_or(0, |id| match self.cards.get(&id) {
                Some(CardRuntime::Repository(runtime)) => Some(runtime.as_ref()),
                Some(CardRuntime::PullRequests(pane)) => Some(&pane.runtime),
                _ => None,
            }.map_or(0, |runtime| workspace_widget::GLOBAL_SHORTCUT_DESCRIPTORS.iter().filter_map(|descriptor| workspace_widget::diff_command_bit(descriptor.action).filter(|_| runtime.diff_command_available(descriptor.id()))).fold(0, |flags, bit| flags | bit))),
            editor_commands: card.map_or(0, |id| match self.cards.get(&id) { Some(CardRuntime::Repository(runtime)) => runtime.editor_command_capabilities(), _ => 0 }),
            editor_surface_commands: card.map_or(0, |id| match self.cards.get(&id) { Some(CardRuntime::Repository(runtime)) => runtime.editor_surface_command_capabilities(id), Some(CardRuntime::PullRequests(pane)) => pane.runtime.editor_surface_command_capabilities(id), _ => 0 }),
            editor_language_commands: card.map_or(0, |id| match self.cards.get(&id) { Some(CardRuntime::Repository(runtime)) if self.lsp_document(id) == runtime.lsp_document() && self.lsp_document(id).and_then(|doc| self.lsp.document_version(doc)).is_some() => runtime.editor_language_command_capabilities(), _ => 0 }),
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
            pane_revealable: pane_index.is_some() && self.active_workspace_destination().is_some()
                && (!save_in_flight || card == self.workspace.focused()),
            pane_cloneable: !self.focused_repository_is_saving() && state.is_some_and(|state| {
                state.existence.is_open()
                    && !state.kind.is_utility()
                    && !self.project_ui.deleting_card(state)
            }),
            pane_moveable: card.is_some_and(|id| self.pane_has_move_destination(id)),
            pane_external_editor: state.is_some_and(|state| state.existence.is_open()) && card.is_some_and(|id| matches!(self.cards.get(&id), Some(CardRuntime::Repository(runtime)) if runtime.external_editor_command_available())),
            can_fork: card.and_then(|id| self.chat_fork_target(id)).is_some(),
            can_end_conversation: state.is_some_and(|state| state.existence.is_open() && matches!(state.kind, CardKind::Chat(_))),
            chat_account: card.is_some_and(|id| self.chat_control_available(id, Command::ChatAccount)),
            chat_model: card.is_some_and(|id| self.chat_control_available(id, Command::ChatModel)),
            chat_permissions: card.is_some_and(|id| self.chat_control_available(id, Command::ChatPermissions)),
            can_switch: card
                .is_some_and(|id| self.chat_switch_available(id) && self.chat_switch_ready(id)),
            can_review: card.and_then(|id| self.chat_changes_target(id)).is_some(),
            can_find: !self.sidebar_model.projects.is_empty() || card.is_some_and(|id| match self.cards.get(&id) {
                Some(CardRuntime::Repository(runtime)) => runtime.editor_focus_target(id).is_some(),
                Some(CardRuntime::Chat(_) | CardRuntime::Settings | CardRuntime::Terminal(_)) => {
                    true
                }
                _ => false,
            }),
            daemon_can_restart: !crate::device::is_remote(),
            can_search_project: self.sidebar_state.active_worktree().is_some(),
            has_attention: !self.sidebar_state.attention_cards().is_empty(),
            can_cycle_workspace: sidebar::workspace_cycle(&self.sidebar_model, &self.sidebar_state).len() > 1,
            has_unread: self.durable.cards.iter().any(|card| card.existence.is_open() && self.sidebar_state.has_card_update(card.id)),
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
        let mut context = self.command_context(Some(card));
        context.sidebar_commands = None;
        workspace_widget::shortcut_descriptor(command).available(&context)
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
            Command::EndConversation => Message::ChatClose(chat_ui::closing::Action::End(id)),
            Command::MovePaneToChannel => Message::Palette(Action::MovePane(id)),
            Command::CloseOtherPanes => Message::ChromeUi(chrome_ui::Action::CloseOthers(id)),
            Command::ClosePanesToRight => Message::ChromeUi(chrome_ui::Action::CloseToRight(id)),
            Command::OpenInExternalEditor => Message::Repository(match self.cards.get(&id) {
                Some(CardRuntime::Repository(runtime)) => runtime.external_editor_command(id, None)?,
                _ => return None,
            }),
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
        if workspace_widget::editor_surface_command_bit(command).is_some() {
            return self.workspace.focused().and_then(|card| match self.cards.get(&card) {
                Some(CardRuntime::Repository(runtime)) => runtime.registered_surface_command_action(card, command),
                Some(CardRuntime::PullRequests(pane)) => pane.runtime.registered_surface_command_action(card, command),
                _ => None,
            }).map_or_else(Task::none, |action| self.update(Message::Repository(action)));
        }
        if workspace_widget::editor_language_command_bit(command).is_some() {
            return self.workspace.focused().and_then(|card| match self.cards.get(&card) {
                Some(CardRuntime::Repository(runtime)) => runtime.editor_language_command_action(card, command),
                _ => None,
            }).map_or_else(Task::none, |action| self.update(Message::Repository(action)));
        }
        if workspace_widget::git_command_bit(command).is_some() {
            let Some(target) = self.git_command_target() else { return Task::none(); };
            if !target.available(&self.sidebar_model, &self.sidebar_state, command) { return Task::none(); }
            if sidebar::git_commands::is_stash_picker(command) {
                let entries = target.stashes(&self.sidebar_model, &self.sidebar_state).to_vec();
                let task = self.update_palette(Action::Open);
                self.palette.stash_entries = entries;
                self.palette.git_target = Some(target);
                self.palette.stash_picker = Some(command);
                return task;
            }
            let task = target.action(&self.sidebar_model, &self.sidebar_state, command)
                .map_or_else(Task::none, |action| self.update(Message::Sidebar(action)));
            return if matches!(command, Command::GitStashes | Command::GitAmend | Command::GitOutput) {
                Task::batch([task, self.update(Message::SelectContextTab(ContextTab::Changes))])
            } else { task };
        }
        if command.sidebar_command_bit().is_some()
            && let Some(target) = self.sidebar_command_target()
        {
            return match target.resolve_in(&self.sidebar_model, &self.sidebar_state, command) {
                // Check and invoke in the same update: no queued message may
                // reinterpret a selection after its captured revision passed.
                Some(action) if matches!(&target, sidebar::commands::CommandTarget::Changes { .. }) => {
                    if crate::device::is_remote() && !device_ui::supports_sidebar_action(&action) { return Task::none(); }
                    self.update(Message::Sidebar(action))
                }
                Some(action) => self.run_sidebar_registered_command(command, action),
                None => {
                    self.status = Some(TransientNotice::warning("The original sidebar target is no longer available"));
                    Task::none()
                }
            };
        }
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
            Command::EditorUndo | Command::EditorRedo | Command::EditorCut | Command::EditorCopy | Command::EditorPaste | Command::EditorSelectAll | Command::NextDiagnostic | Command::PreviousDiagnostic => self.workspace.focused().and_then(|card| repository_widget::editor_command_action(card, command)).map_or_else(Task::none, |action| self.update(Message::Repository(action))),
            Command::DiffCopy | Command::DiffCopyOld | Command::DiffCopyNew | Command::DiffCopyPatch | Command::DiffCopyLineReference | Command::DiffOpenInEditor | Command::DiffSelectAll => {
                let action = self.workspace.focused().and_then(|card| match self.cards.get(&card) {
                    Some(CardRuntime::Repository(runtime)) => runtime.diff_command_action(card, command.id()),
                    Some(CardRuntime::PullRequests(pane)) => pane.runtime.diff_command_action(card, command.id()),
                    _ => None,
                });
                action.map_or_else(Task::none, |action| self.update(Message::Repository(action)))
            }
            Command::NewBrowser => self.update(Message::NewBrowser),
            Command::MarkAllRead => self.update(Message::Sidebar(SidebarAction::MarkAllRead)),
            Command::SearchInProject => self.update(Message::SelectContextTab(ContextTab::Search)),
            Command::CloneRepository => self.update(Message::Onboarding(onboarding_ui::Action::Clone)),
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
        if command.sidebar_command_bit().is_some() && self.sidebar_command_target().is_some() {
            return self.run_registered_command(command);
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
            | Command::SettingsAbout
            | Command::MarkAllRead
            | Command::SearchInProject
            | Command::CloneRepository
            | Command::EditorUndo
            | Command::EditorRedo
            | Command::EditorCut
            | Command::EditorCopy
            | Command::EditorPaste
            | Command::EditorSelectAll
            | Command::EndConversation
            | Command::MovePaneToChannel
            | Command::DiffCopy
            | Command::DiffCopyOld
            | Command::DiffCopyNew
            | Command::DiffCopyPatch
            | Command::DiffCopyLineReference
            | Command::DiffOpenInEditor
            | Command::DiffSelectAll
            | Command::NextDiagnostic
            | Command::PreviousDiagnostic
            | Command::CopyBranch
            | Command::OpenOrCreatePullRequest
            | Command::NewWorktreeFromBranch
            | Command::MoveProjectUp
            | Command::MoveProjectDown
            | Command::ResetSidebarOrder
            | Command::RevealInFileManager
            | Command::OpenRepositoryUrl
            | Command::OpenInExternalEditor
            | Command::DeleteWorktree
            | Command::RemoveProject
            | Command::DeleteSelectedRows
            | Command::RenameChannel
            | Command::MarkChannelRead
            | Command::DeleteChannel
            | Command::MarkWorktreeRead
            | Command::RenameFile
            | Command::DuplicateFile
            | Command::CopyRelativePath
            | Command::StageSelectedChanges
            | Command::UnstageSelectedChanges
            | Command::DiscardSelectedChanges
            | Command::OpenFile
            | Command::RevealInFiles
            | Command::IgnoreChangePath
            | Command::GitFetch
            | Command::GitPush
            | Command::GitPublish
            | Command::GitSync
            | Command::GitSyncRebase
            | Command::GitStash
            | Command::GitStashUntracked
            | Command::GitStashes
            | Command::GitApplyStash
            | Command::GitPopStash
            | Command::GitDropStash
            | Command::GitCommit
            | Command::GitAmend
            | Command::GitRefresh
            | Command::GitAbort
            | Command::GitContinue
            | Command::GitOutput
            | Command::GitAcceptOurs
            | Command::GitAcceptTheirs
            | Command::EditorComplete
            | Command::GoToDefinition
            | Command::FindReferences
            | Command::ShowHover
            | Command::ShowDiagnostics
            | Command::EditorSave
            | Command::EditorDiscardEdits
            | Command::EditorEdit
            | Command::EditorPreview
            | Command::EditorDiff
            => self.run_registered_command(command),
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
            Self::ShortcutMovePaneToChannel => Some(Command::MovePaneToChannel),
            Self::ShortcutDiffCopy => Some(Command::DiffCopy),
            Self::ShortcutDiffCopyOld => Some(Command::DiffCopyOld),
            Self::ShortcutDiffCopyNew => Some(Command::DiffCopyNew),
            Self::ShortcutDiffCopyPatch => Some(Command::DiffCopyPatch),
            Self::ShortcutDiffCopyLineReference => Some(Command::DiffCopyLineReference),
            Self::ShortcutDiffOpenInEditor => Some(Command::DiffOpenInEditor),
            Self::ShortcutDiffSelectAll => Some(Command::DiffSelectAll),

            Self::ShortcutNextDiagnostic => Some(Command::NextDiagnostic),
            Self::ShortcutPreviousDiagnostic => Some(Command::PreviousDiagnostic),
            Self::ShortcutEndConversation => Some(Command::EndConversation),

            Self::ShortcutEditorUndo => Some(Command::EditorUndo),
            Self::ShortcutEditorRedo => Some(Command::EditorRedo),
            Self::ShortcutEditorCut => Some(Command::EditorCut),
            Self::ShortcutEditorCopy => Some(Command::EditorCopy),
            Self::ShortcutEditorPaste => Some(Command::EditorPaste),
            Self::ShortcutEditorSelectAll => Some(Command::EditorSelectAll),

            Self::ShortcutMarkAllRead => Some(Command::MarkAllRead),
            Self::ShortcutSearchInProject => Some(Command::SearchInProject),
            Self::ShortcutCloneRepository => Some(Command::CloneRepository),
            Self::ShortcutCopyBranch => Some(Command::CopyBranch),
            Self::ShortcutOpenOrCreatePullRequest => Some(Command::OpenOrCreatePullRequest),
            Self::ShortcutNewWorktreeFromBranch => Some(Command::NewWorktreeFromBranch),
            Self::ShortcutMoveProjectUp => Some(Command::MoveProjectUp),
            Self::ShortcutMoveProjectDown => Some(Command::MoveProjectDown),
            Self::ShortcutResetSidebarOrder => Some(Command::ResetSidebarOrder),
            Self::ShortcutRevealInFileManager => Some(Command::RevealInFileManager),
            Self::ShortcutOpenRepositoryUrl => Some(Command::OpenRepositoryUrl),
            Self::ShortcutOpenInExternalEditor => Some(Command::OpenInExternalEditor),
            Self::ShortcutDeleteWorktree => Some(Command::DeleteWorktree),
            Self::ShortcutRemoveProject => Some(Command::RemoveProject),
            Self::ShortcutDeleteSelectedRows => Some(Command::DeleteSelectedRows),
            Self::ShortcutRenameChannel => Some(Command::RenameChannel),
            Self::ShortcutMarkChannelRead => Some(Command::MarkChannelRead),
            Self::ShortcutDeleteChannel => Some(Command::DeleteChannel),
            Self::ShortcutMarkWorktreeRead => Some(Command::MarkWorktreeRead),
            Self::ShortcutRenameFile => Some(Command::RenameFile),
            Self::ShortcutDuplicateFile => Some(Command::DuplicateFile),
            Self::ShortcutCopyRelativePath => Some(Command::CopyRelativePath),
            Self::ShortcutStageSelectedChanges => Some(Command::StageSelectedChanges),
            Self::ShortcutUnstageSelectedChanges => Some(Command::UnstageSelectedChanges),
            Self::ShortcutDiscardSelectedChanges => Some(Command::DiscardSelectedChanges),
            Self::ShortcutOpenFile => Some(Command::OpenFile),
            Self::ShortcutRevealInFiles => Some(Command::RevealInFiles),
            Self::ShortcutIgnoreChangePath => Some(Command::IgnoreChangePath),
            Self::ShortcutGitFetch => Some(Command::GitFetch),
            Self::ShortcutGitPush => Some(Command::GitPush),
            Self::ShortcutGitPublish => Some(Command::GitPublish),
            Self::ShortcutGitSync => Some(Command::GitSync),
            Self::ShortcutGitSyncRebase => Some(Command::GitSyncRebase),
            Self::ShortcutGitStash => Some(Command::GitStash),
            Self::ShortcutGitStashUntracked => Some(Command::GitStashUntracked),
            Self::ShortcutGitStashes => Some(Command::GitStashes),
            Self::ShortcutGitApplyStash => Some(Command::GitApplyStash),
            Self::ShortcutGitPopStash => Some(Command::GitPopStash),
            Self::ShortcutGitDropStash => Some(Command::GitDropStash),
            Self::ShortcutGitCommit => Some(Command::GitCommit),
            Self::ShortcutGitAmend => Some(Command::GitAmend),
            Self::ShortcutGitRefresh => Some(Command::GitRefresh),
            Self::ShortcutGitAbort => Some(Command::GitAbort),
            Self::ShortcutGitContinue => Some(Command::GitContinue),
            Self::ShortcutGitOutput => Some(Command::GitOutput),
            Self::ShortcutGitAcceptOurs => Some(Command::GitAcceptOurs),
            Self::ShortcutGitAcceptTheirs => Some(Command::GitAcceptTheirs),
            Self::ShortcutEditorComplete => Some(Command::EditorComplete),
            Self::ShortcutGoToDefinition => Some(Command::GoToDefinition),
            Self::ShortcutFindReferences => Some(Command::FindReferences),
            Self::ShortcutShowHover => Some(Command::ShowHover),
            Self::ShortcutShowDiagnostics => Some(Command::ShowDiagnostics),
            Self::ShortcutEditorSave => Some(Command::EditorSave),
            Self::ShortcutEditorDiscardEdits => Some(Command::EditorDiscardEdits),
            Self::ShortcutEditorEdit => Some(Command::EditorEdit),
            Self::ShortcutEditorPreview => Some(Command::EditorPreview),
            Self::ShortcutEditorDiff => Some(Command::EditorDiff),


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
            (Command::MovePaneToChannel, self.move_pane_to_channel.as_str()),
            (Command::DiffCopy, self.diff_copy.as_str()),
            (Command::DiffCopyOld, self.diff_copy_old.as_str()),
            (Command::DiffCopyNew, self.diff_copy_new.as_str()),
            (Command::DiffCopyPatch, self.diff_copy_patch.as_str()),
            (Command::DiffCopyLineReference, self.diff_copy_line_reference.as_str()),
            (Command::DiffOpenInEditor, self.diff_open_in_editor.as_str()),
            (Command::DiffSelectAll, self.diff_select_all.as_str()),

            (Command::NextDiagnostic, self.next_diagnostic.as_str()),
            (Command::PreviousDiagnostic, self.previous_diagnostic.as_str()),
            (Command::EndConversation, self.end_conversation.as_str()),

            (Command::EditorUndo, self.editor_undo.as_str()),
            (Command::EditorRedo, self.editor_redo.as_str()),
            (Command::EditorCut, self.editor_cut.as_str()),
            (Command::EditorCopy, self.editor_copy.as_str()),
            (Command::EditorPaste, self.editor_paste.as_str()),
            (Command::EditorSelectAll, self.editor_select_all.as_str()),

            (Command::MarkAllRead, self.mark_all_read.as_str()),
            (Command::SearchInProject, self.search_in_project.as_str()),
            (Command::CloneRepository, self.clone_repository.as_str()),
            (Command::CopyBranch, self.copy_branch.as_str()),
            (Command::OpenOrCreatePullRequest, self.open_or_create_pull_request.as_str()),
            (Command::NewWorktreeFromBranch, self.new_worktree_from_branch.as_str()),
            (Command::MoveProjectUp, self.move_project_up.as_str()),
            (Command::MoveProjectDown, self.move_project_down.as_str()),
            (Command::ResetSidebarOrder, self.reset_sidebar_order.as_str()),
            (Command::RevealInFileManager, self.reveal_in_file_manager.as_str()),
            (Command::OpenRepositoryUrl, self.open_repository_url.as_str()),
            (Command::OpenInExternalEditor, self.open_in_external_editor.as_str()),
            (Command::DeleteWorktree, self.delete_worktree.as_str()),
            (Command::RemoveProject, self.remove_project.as_str()),
            (Command::DeleteSelectedRows, self.delete_selected_rows.as_str()),
            (Command::RenameChannel, self.rename_channel.as_str()),
            (Command::MarkChannelRead, self.mark_channel_read.as_str()),
            (Command::DeleteChannel, self.delete_channel.as_str()),
            (Command::MarkWorktreeRead, self.mark_worktree_read.as_str()),
            (Command::RenameFile, self.rename_file.as_str()),
            (Command::DuplicateFile, self.duplicate_file.as_str()),
            (Command::CopyRelativePath, self.copy_relative_path.as_str()),
            (Command::StageSelectedChanges, self.stage_selected_changes.as_str()),
            (Command::UnstageSelectedChanges, self.unstage_selected_changes.as_str()),
            (Command::DiscardSelectedChanges, self.discard_selected_changes.as_str()),
            (Command::OpenFile, self.open_file.as_str()),
            (Command::RevealInFiles, self.reveal_in_files.as_str()),
            (Command::IgnoreChangePath, self.ignore_change_path.as_str()),
            (Command::GitFetch, self.git_fetch.as_str()),
            (Command::GitPush, self.git_push.as_str()),
            (Command::GitPublish, self.git_publish.as_str()),
            (Command::GitSync, self.git_sync.as_str()),
            (Command::GitSyncRebase, self.git_sync_rebase.as_str()),
            (Command::GitStash, self.git_stash.as_str()),
            (Command::GitStashUntracked, self.git_stash_untracked.as_str()),
            (Command::GitStashes, self.git_stashes.as_str()),
            (Command::GitApplyStash, self.git_apply_stash.as_str()),
            (Command::GitPopStash, self.git_pop_stash.as_str()),
            (Command::GitDropStash, self.git_drop_stash.as_str()),
            (Command::GitCommit, self.git_commit.as_str()),
            (Command::GitAmend, self.git_amend.as_str()),
            (Command::GitRefresh, self.git_refresh.as_str()),
            (Command::GitAbort, self.git_abort.as_str()),
            (Command::GitContinue, self.git_continue.as_str()),
            (Command::GitOutput, self.git_output.as_str()),
            (Command::GitAcceptOurs, self.git_accept_ours.as_str()),
            (Command::GitAcceptTheirs, self.git_accept_theirs.as_str()),
            (Command::EditorComplete, self.editor_complete.as_str()),
            (Command::GoToDefinition, self.go_to_definition.as_str()),
            (Command::FindReferences, self.find_references.as_str()),
            (Command::ShowHover, self.show_hover.as_str()),
            (Command::ShowDiagnostics, self.show_diagnostics.as_str()),
            (Command::EditorSave, self.editor_save.as_str()),
            (Command::EditorDiscardEdits, self.editor_discard_edits.as_str()),
            (Command::EditorEdit, self.editor_edit.as_str()),
            (Command::EditorPreview, self.editor_preview.as_str()),
            (Command::EditorDiff, self.editor_diff.as_str()),

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
        assert!(matches!(entries[0].2, Message::Palette(Action::FocusPane(id)) if id == chosen));
        drop(app.update_palette(Action::Submit));
        assert!(!app.palette.is_open());
        assert_eq!(app.workspace.focused(), Some(chosen));
        drop(app.run_workspace_command(Command::FocusLastPane, keyboard::Modifiers::empty()));
        assert_eq!(app.workspace.focused(), app.workspace.order().last());
    }
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
    fn saving_navigation_is_hidden_and_rejected_while_safe_commands_remain_available() {
        let mut app = settings_focus_fixture();
        let card = app.workspace.focused().unwrap();
        app.sidebar_model.projects.push(SidebarProject::new(ProjectId::new(), crate::repository::RepositoryId::new(), "Project".into(), "/repo".into(), Vec::new(), Vec::new()));
        let blocked = [Command::NewWorktree, Command::NewTerminal, Command::NewThread, Command::OpenSettings, Command::SettingsEditor, Command::FocusProject1];
        for command in blocked { assert!(app.command_available(command), "baseline {command:?}"); }
        let (runtime, _) = RepositoryRuntime::post_persist_failure_fixture(card);
        app.card_state_mut(card).unwrap().kind = CardKind::Repository(runtime.descriptor.clone());
        app.cards.insert(card, CardRuntime::Repository(Box::new(runtime)));
        assert!(app.focused_repository_is_saving());
        let cards = app.durable.cards.len();
        let settings_category = app.settings_category;
        for command in blocked {
            assert!(!app.command_available(command), "saving {command:?}");
            assert!(!app.palette_entries().iter().any(|entry| matches!(entry.2, Message::RunCommand(candidate) if candidate == command)));
            drop(app.update(Message::RunCommand(command)));
            assert_eq!(app.workspace.focused(), Some(card));
            assert_eq!(app.durable.cards.len(), cards);
            assert_eq!(app.settings_category, settings_category);
            assert!(app.notices.recent().any(|entry| entry.notice.message.contains("unavailable")));
        }
        for command in [Command::CopyPath, Command::ToggleTheme, Command::FocusProjectSearch, Command::FindInPane] {
            assert!(app.command_available(command), "non-navigating {command:?}");
        }
        app.sidebar_state.observe_card_update(card);
        assert!(app.command_available(Command::MarkAllRead));
        drop(app.update(Message::RunCommand(Command::MarkAllRead)));
        assert!(!app.sidebar_state.has_card_update(card));
    }

    #[test]
    fn palette_end_conversation_keeps_recovery_available_without_a_ready_runtime() {
        let mut app = settings_focus_fixture();
        let directory = tempfile::tempdir().unwrap();
        drop(app.new_native_chat(crate::agent::Provider::Codex, Some(directory.path().into())));
        let card = app.workspace.focused().unwrap();
        for runtime in [CardRuntime::Loading, CardRuntime::Failed("fixture startup failure".into())] {
            app.cards.insert(card, runtime);
            assert!(app.pane_command_available(card, Command::EndConversation));
            assert!(matches!(app.pane_command_message(card, Command::EndConversation), Some(Message::ChatClose(chat_ui::closing::Action::End(id))) if id == card));
            assert!(!app.pane_command_available(card, Command::ForkConversation));
        }
        app.card_state_mut(card).unwrap().kind = CardKind::Settings;
        assert!(!app.pane_command_available(card, Command::EndConversation));
    }

    #[test]
    fn palette_recents_precede_ranked_attention_without_duplicate_panes() {
        let mut app = settings_focus_fixture();
        let card = app.workspace.focused().unwrap();
        let recent = Message::RunCommand(Command::OpenSettings);
        app.settings.palette_recents = vec![entry_key(&recent)];
        let project = ProjectId::new();
        let worktree = crate::model::WorktreeId::new();
        app.sidebar_state.observe_agent_update(card, project, worktree);
        assert!(app.command_available(Command::MarkAllRead));
        drop(app.update_palette(Action::Open));
        let entries = app.palette_entries();
        assert_eq!(app.palette_section(&entries[0].2), Some("Recent"));
        assert!(matches!(entries[1].2, Message::FocusAttention(id) if id == card));
        assert_eq!(app.palette_section(&entries[1].2), Some("Needs attention"));
        assert_eq!(entries.iter().filter(|(_,_, message)| entry_key(message) == format!("pane:{card}")).count(), 1);
        assert_eq!(entry_category(&entries[1].2), '@');
        drop(app.update_palette(Action::Close));
        drop(app.run_workspace_command(Command::MarkAllRead, keyboard::Modifiers::empty()));
        assert!(!app.sidebar_state.has_card_update(card));
        assert!(!app.command_available(Command::MarkAllRead));
    }

    #[test]
    fn all_registered_actions_have_visible_keyboard_groups_and_matching_drafts() {
        let app = settings_focus_fixture();
        let drafts = app.settings_draft.keyboard_shortcuts.shortcut_values();
        assert_eq!(drafts.len(), SettingsField::SHORTCUTS.len());
        for (index, field) in SettingsField::SHORTCUTS.into_iter().enumerate() {
            let action = field.shortcut_action().unwrap();
            let descriptor = workspace_widget::shortcut_descriptor(action);
            assert!(workspace_widget::SHORTCUT_CATEGORIES.contains(&descriptor.category()));
            assert_eq!(drafts[index].0, action);
            assert_eq!(action.id(), KeyboardShortcuts::action_names()[index]);
        }
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
    fn configured_save_retires_old_chords_and_respects_actual_repository_focus() {
        use workspace_widget::FocusedPane;
        let make = || {
            let mut app = settings_focus_fixture();
            let card = app.workspace.focused().unwrap();
            let mut runtime = RepositoryRuntime::dirty_fixture(card);
            runtime.set_auto_save(false);
            app.card_state_mut(card).unwrap().kind = CardKind::Repository(runtime.descriptor.clone());
            app.cards.insert(card, CardRuntime::Repository(Box::new(runtime)));
            (app, card)
        };
        let press = |app: &mut Cagents, card, pane, key, physical, modifiers| {
            drop(app.update(Message::RoutedKeyEvent {
                event: keyboard::Event::KeyPressed { key: keyboard::Key::Character(key), modified_key: keyboard::Key::Character("s".into()), physical_key: physical, location: keyboard::Location::Standard, modifiers, text: None, repeat: false },
                captured: true, region: crate::input_trace::KeyboardRegion { pane, card: Some(card) }, widget: None,
            }));
        };
        for (pane, expected) in [(FocusedPane::RepositoryPane, true), (FocusedPane::Review, true), (FocusedPane::Editor, true), (FocusedPane::TextInput, false), (FocusedPane::SidebarRows, false), (FocusedPane::SidebarFiles, false), (FocusedPane::SidebarChanges, false), (FocusedPane::Terminal, false), (FocusedPane::Chat, false), (FocusedPane::ChatPane, false), (FocusedPane::OutsideWorkspace, false), (FocusedPane::Other, false)] {
            let (mut app, card) = make();
            assert!(app.command_available(Command::EditorSave));
            press(&mut app, card, pane, "s".into(), keyboard::key::Physical::Code(keyboard::key::Code::KeyS), keyboard::Modifiers::CTRL);
            assert_eq!(app.focused_repository_is_saving(), expected, "{pane:?}");
        }
        let (mut app, card) = make();
        app.settings.keyboard_shortcuts.editor_save.clear();
        assert!(app.settings.clone().validate().is_ok());
        press(&mut app, card, FocusedPane::Editor, "s".into(), keyboard::key::Physical::Code(keyboard::key::Code::KeyS), keyboard::Modifiers::CTRL);
        assert!(!app.focused_repository_is_saving(), "clearing Save retires Ctrl+S");
        app.settings.keyboard_shortcuts.editor_save = vec!["Ctrl+Alt+F9".into()];
        assert!(app.settings.clone().validate().is_ok());
        press(&mut app, card, FocusedPane::Editor, "s".into(), keyboard::key::Physical::Code(keyboard::key::Code::KeyS), keyboard::Modifiers::CTRL);
        assert!(!app.focused_repository_is_saving(), "rebinding Save leaves the old chord retired");
        drop(app.update(Message::RoutedKeyEvent {
            event: keyboard::Event::KeyPressed { key: keyboard::Key::Named(keyboard::key::Named::F9), modified_key: keyboard::Key::Named(keyboard::key::Named::F9), physical_key: keyboard::key::Physical::Code(keyboard::key::Code::F9), location: keyboard::Location::Standard, modifiers: keyboard::Modifiers::CTRL | keyboard::Modifiers::ALT, text: None, repeat: false },
            captured: true, region: crate::input_trace::KeyboardRegion { pane: FocusedPane::Editor, card: Some(card) }, widget: None,
        }));
        assert!(app.focused_repository_is_saving(), "the configured chord uses the actual save controller once");
        let (mut app, card) = make();
        press(&mut app, card, FocusedPane::Editor, "ы".into(), keyboard::key::Physical::Code(keyboard::key::Code::KeyS), keyboard::Modifiers::CTRL);
        assert!(app.focused_repository_is_saving(), "physical KeyS preserves save on a non-Latin layout");
    }

    #[test]
    fn send_to_terminal_setting_updates_live_policy_and_retained_controls() {
        let mut app = settings_focus_fixture();
        let action = "rename_pane";
        assert!(
            app.settings
                .keyboard_shortcuts
                .terminal_passthrough_actions
                .iter()
                .any(|name| name == action)
        );
        for enabled in [false, true] {
            drop(app.update(Message::ShortcutTerminalPassthrough {
                action: action.into(),
                enabled,
            }));
            assert_eq!(
                app.settings
                    .keyboard_shortcuts
                    .terminal_passthrough_actions
                    .iter()
                    .any(|name| name == action),
                enabled
            );
            assert_eq!(
                app.settings_draft
                    .keyboard_shortcuts
                    .terminal_passthrough_actions,
                app.settings.keyboard_shortcuts.terminal_passthrough_actions
            );
            let encoded = toml::to_string(&app.settings.keyboard_shortcuts).unwrap();
            let restored: KeyboardShortcuts = toml::from_str(&encoded).unwrap();
            assert_eq!(
                restored.terminal_passthrough_actions,
                app.settings.keyboard_shortcuts.terminal_passthrough_actions
            );
        }
        let previous = app.settings.keyboard_shortcuts.clone();
        drop(app.update(Message::ShortcutTerminalPassthrough {
            action: "unknown-command".into(),
            enabled: true,
        }));
        assert_eq!(app.settings.keyboard_shortcuts, previous);
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
        app.input_trace = crate::input_trace::InputTrace::with_keyboard_region(
            crate::input_trace::KeyboardRegion {
                pane: workspace_widget::FocusedPane::Other,
                card: Some(target),
            },
        );
        app.sidebar_model.projects.push(SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "Project".into(),
            std::env::temp_dir(),
            Vec::new(),
            Vec::new(),
        ));
        assert_eq!(app.workspace.focused(), Some(first));
        drop(app.update(Message::KeyEvent {
            event: keyboard::Event::KeyPressed {
                key: keyboard::Key::Character("f".into()),
                modified_key: keyboard::Key::Character("f".into()),
                physical_key: keyboard::key::Physical::Code(keyboard::key::Code::KeyF),
                location: keyboard::Location::Standard,
                modifiers: keyboard::Modifiers::CTRL,
                text: None,
                repeat: false,
            },
            captured: true,
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
    #[test]
    fn registered_stash_picker_keeps_its_captured_worktree_after_nested_palette_open() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.sidebar_model = sidebar::tests::panel_model();
        app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
        app.sidebar_model.source_control.as_mut().unwrap().stashes = vec![crate::repository::StashEntry {
            reference: "stash@{2}".into(), oid: crate::repository::Oid::parse(&"a".repeat(40)).unwrap(), message: "selected work".into(),
        }];
        drop(app.update_palette(Action::Open));
        assert!(app.command_available(Command::GitApplyStash));
        drop(app.run_palette_command(Message::RunCommand(Command::GitApplyStash)));
        assert!(app.palette.query.is_some());
        assert!(app.palette.git_captured);
        assert_eq!(app.palette.stash_picker, Some(Command::GitApplyStash));
        assert_eq!(app.palette_entries().len(), 1);
        app.sidebar_model.source_control.as_mut().unwrap().branch = Some(crate::repository::ValidatedBranchName::parse("replacement").unwrap());
        assert!(app.palette_entries().is_empty(), "a branch switch cannot substitute another stash source");
        drop(app.update_palette(Action::Close));
        assert!(!app.palette.git_captured);
    }

    #[test]
    fn stash_enter_keeps_the_shown_candidate_when_a_refresh_replaces_the_same_index() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.sidebar_model = sidebar::tests::panel_model();
        app.sidebar_state = sidebar::tests::panel_state(&app.sidebar_model);
        let original = crate::repository::StashEntry { reference: "stash@{0}".into(), oid: crate::repository::Oid::parse(&"a".repeat(40)).unwrap(), message: "Original".into() };
        app.sidebar_model.source_control.as_mut().unwrap().stashes = vec![original.clone()];
        drop(app.run_workspace_command(Command::GitPopStash, keyboard::Modifiers::empty()));
        let shown = app.palette_entries();
        assert!(matches!(&shown[0].2, Message::Palette(Action::RunGitStash(_, _, entry)) if entry == &original));
        drop(shown);
        let mut replacement = original.clone();
        replacement.oid = crate::repository::Oid::parse(&"b".repeat(40)).unwrap();
        replacement.message = "Replacement".into();
        app.sidebar_model.source_control.as_mut().unwrap().stashes = vec![replacement];
        let after_refresh = app.palette_entries();
        assert!(matches!(&after_refresh[0].2, Message::Palette(Action::RunGitStash(_, _, entry)) if entry == &original), "the selected index still names the originally displayed candidate");
        drop(after_refresh);
        drop(app.update_palette(Action::Submit));
        assert!(app.notices.recent().any(|entry| entry.notice.message.contains("original stash")));
        assert!(app.source_control_mutations.is_empty(), "Enter cannot pop the replacement stash");
    }

    #[test]
    fn sidebar_palette_keeps_original_row_after_focus_and_selection_change() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.sidebar_model = sidebar::tests::panel_model();
        let project = app.sidebar_model.projects[0].id;
        let row = sidebar::SelectionRow::Worktree(project, app.sidebar_model.projects[0].worktrees[0].id);
        let region = crate::input_trace::KeyboardRegion::from(workspace_widget::FocusedPane::SidebarRows);
        let _event = app.input_trace.scoped_keyboard_focus(region, Some(sidebar::selection_row_focus_id(row)));
        drop(app.update_palette(Action::Open));
        assert_eq!(app.palette.sidebar_target, Some(sidebar::commands::CommandTarget::Rows(vec![row])));
        let replacement = app.sidebar_model.projects[0].worktrees[1].id;
        app.sidebar_state.select_rows(sidebar::SelectionRow::Worktree(project, replacement), sidebar::SelectionIntent::Toggle, &[row, sidebar::SelectionRow::Worktree(project, replacement)]);
        assert_eq!(app.sidebar_command_target(), Some(sidebar::commands::CommandTarget::Rows(vec![row])));
        app.sidebar_model.projects[0].worktrees.remove(0);
        assert!(!app.command_available(Command::NewTerminal));
        assert!(!app.command_available(Command::CopyPath));
        drop(app.run_registered_command(Command::CopyPath));
        assert!(app.status.is_some());
    }

    #[test]
    fn sidebar_thread_binding_retains_worktree_and_rejects_other_native_row_targets() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        app.sidebar_model = sidebar::tests::panel_model();
        let project = app.sidebar_model.projects[0].id;
        let worktree = app.sidebar_model.projects[0].worktrees[1].id;
        let channel = crate::model::ChannelId::new();
        app.sidebar_model.projects[0].channels.push(sidebar::SidebarChannel::new(channel, "Example".into(), false, None, false, 0, false, 1));
        let region = crate::input_trace::KeyboardRegion::from(workspace_widget::FocusedPane::SidebarRows);
        let focused = app.workspace.focused();
        let cards = app.durable.cards.len();
        for (row, available) in [
            (sidebar::SelectionRow::Worktree(project, worktree), true),
            (sidebar::SelectionRow::Project(project), false),
            (sidebar::SelectionRow::Channel(channel), false),
            (sidebar::SelectionRow::Worktree(project, WorktreeId::new()), false),
        ] {
            let _event = app.input_trace.scoped_keyboard_focus(region, Some(sidebar::selection_row_focus_id(row)));
            assert_eq!(app.command_available(Command::NewThread), available, "{row:?}");
            if available {
                let target = app.sidebar_command_target().expect("native row target");
                assert!(matches!(target.resolve_in(&app.sidebar_model, &app.sidebar_state, Command::NewThread), Some(SidebarAction::ProjectCommand { project_id, worktree_id: Some(id), command: sidebar::ProjectCommand::NewChannel }) if project_id == project && id == worktree));
            } else {
                drop(app.run_workspace_command(Command::NewThread, keyboard::Modifiers::empty()));
                assert_eq!(app.workspace.focused(), focused);
                assert_eq!(app.durable.cards.len(), cards, "rejected row must not create a background pane");
            }
        }
    }

    #[test]
    fn stale_native_sidebar_identity_does_not_fall_back_to_focused_pane() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let region = crate::input_trace::KeyboardRegion::from(workspace_widget::FocusedPane::SidebarRows);
        let _event = app.input_trace.scoped_keyboard_focus(region, Some(widget::Id::new("removed-sidebar-row")));
        assert_eq!(app.sidebar_command_target(), Some(sidebar::commands::CommandTarget::Unavailable));
        assert!(!app.command_available(Command::CopyPath));
    }

}
