use super::*;
use crate::agent::Provider;
use crate::chat;
use crate::persistence::SavedConversation;
use std::io::{Read, Seek};

#[derive(Default)]
pub(super) struct State {
    pub open: bool,
    generation: u64,
    query: String,
    entries: Vec<SavedConversation>,
    loading: bool,
    pub(super) error: Option<String>,
    limit: usize,
    selected: usize,
}

#[derive(Debug, Clone)]
pub enum Action {
    Open,
    Close,
    Query(String),
    Loaded(u64, Result<Vec<SavedConversation>, String>),
    Resume(CardId),
    More,
    Submit,
}

pub(super) fn focus_id() -> widget::Id {
    widget::Id::new("conversation-resume.search")
}

fn same_conversation(a: &chat::Session, b: &chat::Session) -> bool {
    a.launch.provider == b.launch.provider
        && a.launch.session_id == b.launch.session_id
        && a.launch.recipe.working_directory == b.launch.recipe.working_directory
        && crate::account::home_variable(a.launch.provider).is_none_or(|variable| {
            a.launch.recipe.environment.get(variable) == b.launch.recipe.environment.get(variable)
        })
}

/// Read bounded transcript metadata, never the complete conversation body.
fn claude_title(session: &chat::Session) -> Option<String> {
    if session.launch.provider != Provider::Claude {
        return None;
    }
    let recipe = &session.launch.recipe;
    let home = recipe
        .environment
        .get("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .or_else(|| {
            recipe
                .environment
                .get("HOME")
                .map(|home| PathBuf::from(home).join(".claude"))
        })?;
    let home = if home.is_absolute() {
        home
    } else {
        recipe.working_directory.join(home)
    };
    let path = crate::agent::claude_transcript(
        &home,
        &recipe
            .working_directory
            .canonicalize()
            .unwrap_or_else(|_| recipe.working_directory.clone()),
        session.launch.session_id.as_deref()?,
    )
    .ok()?;
    let mut file = std::fs::File::open(path).ok()?;
    let mut title = None;
    let mut summary = None;
    let mut first_prompt = None;
    let mut read_part = |bytes: Vec<u8>| {
        for line in bytes.split(|byte| *byte == b'\n') {
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(line) else {
                continue;
            };
            if matches!(
                value.get("type").and_then(|v| v.as_str()),
                Some("custom-title" | "summary")
            ) {
                if let Some(text) = value
                    .get("customTitle")
                    .or_else(|| value.get("summary"))
                    .and_then(|v| v.as_str())
                {
                    if value["type"] == "custom-title" {
                        title = crate::account::clean_label(text);
                    } else {
                        summary = crate::account::clean_label(text);
                    }
                }
            } else if first_prompt.is_none()
                && value.get("type").and_then(|v| v.as_str()) == Some("user")
                && value.get("isMeta").and_then(|v| v.as_bool()) != Some(true)
                && value.get("isSidechain").and_then(|v| v.as_bool()) != Some(true)
            {
                let content = &value["message"]["content"];
                let text = content.as_str().or_else(|| {
                    content
                        .as_array()?
                        .iter()
                        .find_map(|part| part.get("text")?.as_str())
                });
                first_prompt = text.and_then(crate::account::clean_label);
            }
        }
    };
    let mut head = Vec::new();
    (&mut file).take(128 * 1024).read_to_end(&mut head).ok()?;
    read_part(head);
    if file.metadata().ok()?.len() > 128 * 1024 {
        file.seek(std::io::SeekFrom::End(-64 * 1024)).ok()?;
        let mut tail = Vec::new();
        (&mut file).take(64 * 1024).read_to_end(&mut tail).ok()?;
        read_part(tail);
    }
    title.or(summary).or(first_prompt)
}

impl Cagents {
    pub(super) fn resume_key_event(&mut self, event: &keyboard::Event) -> Option<Task<Message>> {
        if !self.resume_ui.open {
            return None;
        }
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return None;
        };
        if !modifiers.is_empty() {
            return None;
        }
        match key {
            keyboard::Key::Named(keyboard::key::Named::ArrowDown) => {
                let count = self.resume_entries().len().min(self.resume_ui.limit);
                self.resume_ui.selected =
                    (self.resume_ui.selected + 1).min(count.saturating_sub(1));
                Some(self.reveal_resume_selection())
            }
            keyboard::Key::Named(keyboard::key::Named::ArrowUp) => {
                self.resume_ui.selected = self.resume_ui.selected.saturating_sub(1);
                Some(self.reveal_resume_selection())
            }
            _ => None,
        }
    }

    fn reveal_resume_selection(&self) -> Task<Message> {
        let count = self.resume_entries().len().min(self.resume_ui.limit);
        let content_height = count as f32 * 104.0;
        let y = (self.resume_ui.selected as f32 * 104.0 - 180.0)
            .max(0.0)
            .min((content_height - 420.0).max(0.0));
        iced::widget::scrollable::scroll_to(
            widget::Id::new("conversation-resume.results"),
            iced::widget::scrollable::AbsoluteOffset {
                x: None,
                y: Some(y),
            },
        )
        .map(cosmic::Action::App)
    }
    pub(super) fn update_resume(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Open => {
                if self.modal_is_open() {
                    return Task::none();
                }
                self.chrome_ui.close();
                self.resume_ui.open = true;
                self.resume_ui.generation = self.resume_ui.generation.wrapping_add(1);
                self.resume_ui.query.clear();
                self.resume_ui.entries.clear();
                self.resume_ui.error = None;
                self.resume_ui.loading = true;
                self.resume_ui.limit = 100;
                self.resume_ui.selected = 0;
                let generation = self.resume_ui.generation;
                let store = self.session.as_ref().map(SessionRuntime::operation_store);
                let current = self.durable.cards.clone();
                Task::batch([
                    iced::widget::operation::focus(focus_id()).map(cosmic::Action::App),
                    cosmic::task::future(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            let mut entries = store
                                .map_or(Ok(Vec::new()), |store| store.saved_conversations())
                                .map_err(|error| error.to_string())?;
                            for card in current {
                                if let CardKind::Chat(session) = card.kind
                                    && session.launch.session_id.is_some()
                                {
                                    let updated_at_unix_ms = entries
                                        .iter()
                                        .find(|entry| entry.card == card.id)
                                        .map_or_else(
                                            || chat::now_ms().unwrap_or(0) as i64,
                                            |entry| entry.updated_at_unix_ms,
                                        );
                                    entries.retain(|entry| entry.card != card.id);
                                    entries.insert(
                                        0,
                                        SavedConversation {
                                            card: card.id,
                                            title: card.title,
                                            session,
                                            open: card.existence.is_open(),
                                            updated_at_unix_ms,
                                        },
                                    );
                                }
                            }
                            entries.sort_by_key(|entry| {
                                (
                                    std::cmp::Reverse(entry.open),
                                    std::cmp::Reverse(entry.updated_at_unix_ms),
                                )
                            });
                            let mut seen = HashSet::new();
                            entries.retain(|entry| {
                                let launch = &entry.session.launch;
                                seen.insert((
                                    launch.provider,
                                    launch.session_id.clone(),
                                    launch.recipe.working_directory.clone(),
                                    crate::account::home_variable(launch.provider)
                                        .and_then(|variable| {
                                            launch.recipe.environment.get(variable)
                                        })
                                        .cloned(),
                                ))
                            });
                            entries
                                .sort_by_key(|entry| std::cmp::Reverse(entry.updated_at_unix_ms));
                            for entry in &mut entries {
                                if entry.title
                                    == chat_ui::chat_card_title(entry.session.launch.provider)
                                    && let Some(title) = claude_title(&entry.session)
                                {
                                    entry.title = title;
                                }
                            }
                            Ok(entries)
                        })
                        .await
                        .unwrap_or_else(|error| Err(error.to_string()));
                        Message::ResumeConversation(Action::Loaded(generation, result))
                    }),
                ])
            }
            Action::Close => {
                self.resume_ui.open = false;
                self.resume_ui.generation = self.resume_ui.generation.wrapping_add(1);
                self.workspace.focused().map_or_else(Task::none, |card| {
                    iced::widget::operation::focus(self.card_keyboard_focus(card))
                        .map(cosmic::Action::App)
                })
            }
            Action::Query(query) => {
                self.resume_ui.query = query;
                self.resume_ui.limit = 100;
                self.resume_ui.selected = 0;
                self.reveal_resume_selection()
            }
            Action::More => {
                self.resume_ui.limit += 100;
                Task::none()
            }
            Action::Loaded(generation, result) => {
                if !self.resume_ui.open || generation != self.resume_ui.generation {
                    return Task::none();
                }
                self.resume_ui.loading = false;
                match result {
                    Ok(entries) => self.resume_ui.entries = entries,
                    Err(error) => self.resume_ui.error = Some(error),
                }
                Task::none()
            }
            Action::Resume(card) => {
                if !self.resume_ui.open {
                    return Task::none();
                }
                let Some(entry) = self
                    .resume_ui
                    .entries
                    .iter()
                    .find(|entry| entry.card == card)
                    .cloned()
                else {
                    return Task::none();
                };
                self.resume_saved_conversation(entry)
            }
            Action::Submit => {
                let card = self
                    .resume_entries()
                    .get(self.resume_ui.selected)
                    .map(|entry| entry.card);
                card.map_or_else(Task::none, |card| self.update_resume(Action::Resume(card)))
            }
        }
    }

    fn existing_conversation_card(&self, session: &chat::Session) -> Option<CardId> {
        self.durable
            .cards
            .iter()
            .find(|card| {
                card.existence.is_open()
                    && match &card.kind {
                        CardKind::Chat(current) => same_conversation(current, session),
                        CardKind::Terminal(current) => chat_ui::conversion(current)
                            .is_ok_and(|current| same_conversation(&current, session)),
                        _ => false,
                    }
            })
            .map(|card| card.id)
    }

    pub(super) fn resume_saved_conversation(&mut self, entry: SavedConversation) -> Task<Message> {
        self.resume_ui.error = None;
        let existing = self.existing_conversation_card(&entry.session);
        if let Some(card) = existing {
            self.resume_ui.open = false;
            let scope = if let Some(channel) = self
                .durable
                .channels
                .iter()
                .find(|channel| channel.cards.contains(&card))
                .map(|channel| channel.id)
            {
                self.select_channel(channel)
            } else {
                let owner = self
                    .card_state(card)
                    .and_then(|card| self.card_worktree(card))
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| entry.session.launch.recipe.working_directory.clone());
                self.select_worktree(owner, false)
            };
            return Task::batch([scope, cosmic::task::message(Message::FocusCard(card))]);
        }
        if entry.open {
            self.resume_error("This conversation is open outside this window. Return to its original window before resuming.");
            return Task::none();
        }
        let mut session = entry.session;
        if matches!(
            session.native_continuity,
            chat::NativeContinuity::Durable { .. }
        ) {
            self.resume_error("This Codex conversation needs its retained native server. Reopening a closed server is not yet supported.");
            return Task::none();
        }
        if !session.launch.recipe.working_directory.is_dir() {
            self.resume_error("This conversation's project folder is unavailable. Restore the folder before resuming.");
            return Task::none();
        }
        session.reset_owned_codex_listener();
        session.id = TerminalId::new();
        session.proof = None;
        session.terminal = None;
        self.resume_ui.open = false;
        let scope = self.select_worktree(session.launch.recipe.working_directory.clone(), false);
        let insert = self.insert_chat_card(session, None, None);
        if let Some(card) = self.workspace.focused() {
            self.apply_reported_title(card, entry.title);
        }
        Task::batch([scope, insert])
    }

    fn resume_error(&mut self, error: &str) {
        self.resume_ui.error = Some(error.into());
        if !self.resume_ui.open {
            self.status = Some(TransientNotice::error(error));
        }
    }

    pub(super) fn resume_dialog(&self) -> Option<Element<'_, Message>> {
        if !self.resume_ui.open {
            return None;
        }
        let state = &self.resume_ui;
        let entries = self.resume_entries();
        let mut rows: Vec<Element<'_, Message>> = Vec::new();
        for (index, entry) in entries.iter().take(state.limit).enumerate() {
            let label = format!(
                "{} · {}{}",
                entry.session.launch.provider.label(),
                self.resume_account_label(&entry.session),
                if entry.open { " · Open" } else { "" }
            );
            let date = chat::local_time(entry.updated_at_unix_ms / 1000)
                .map(|time| {
                    format!(
                        "{:04}-{:02}-{:02} {}",
                        time.year + 1900,
                        time.month + 1,
                        time.day,
                        time.clock()
                    )
                })
                .unwrap_or_default();
            let detail = format!(
                "{} · {}",
                entry.session.launch.recipe.working_directory.display(),
                date
            );
            let mut contents = widget::column::with_children(vec![
                design::body(&entry.title)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
                design::caption(label)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
                design::caption(detail)
                    .wrapping(iced::widget::text::Wrapping::None)
                    .ellipsize(iced::widget::text::Ellipsize::End(
                        EllipsizeHeightLimit::Lines(1),
                    ))
                    .width(Length::Fill)
                    .into(),
            ])
            .spacing(4);
            let unsupported = !entry.open
                && self.existing_conversation_card(&entry.session).is_none()
                && matches!(
                    entry.session.native_continuity,
                    chat::NativeContinuity::Durable { .. }
                );
            if unsupported {
                contents =
                    contents.push(design::caption("Saved Codex server cannot be reopened yet"));
            }
            rows.push(
                widget::button::custom(contents)
                    .name(format!("Resume {}", entry.title))
                    .width(Length::Fill)
                    .height(Length::Fixed(96.0))
                    .class(cosmic::theme::Button::ListItem([8.0; 4]))
                    .selected(index == state.selected)
                    .on_press_maybe(
                        (!unsupported)
                            .then_some(Message::ResumeConversation(Action::Resume(entry.card))),
                    )
                    .into(),
            );
        }
        if entries.len() > state.limit {
            rows.push(
                widget::button::standard("Show more conversations")
                    .on_press(Message::ResumeConversation(Action::More))
                    .into(),
            );
        }
        if rows.is_empty() {
            rows.push(
                design::caption(if state.loading {
                    "Loading saved conversations…"
                } else if state.entries.is_empty() {
                    "No saved conversations yet."
                } else {
                    "No matching conversations."
                })
                .into(),
            );
        }
        let mut content = widget::column::with_children(vec![
            widget::text::title3("Resume conversation").into(),
            design::caption(
                "Return to a saved conversation with its original project and account.",
            )
            .into(),
            widget::text_input("Search conversations, projects, or accounts…", &state.query)
                .id(focus_id())
                .on_input(|query| Message::ResumeConversation(Action::Query(query)))
                .on_submit(|_| Message::ResumeConversation(Action::Submit))
                .into(),
            widget::scrollable(widget::column::with_children(rows).spacing(8))
                .id(widget::Id::new("conversation-resume.results"))
                .height(Length::Fixed(420.0))
                .into(),
        ])
        .spacing(12);
        if let Some(error) = &state.error {
            content = content.push(design::caption(error));
        }
        content = content.push(
            widget::button::standard("Cancel").on_press(Message::ResumeConversation(Action::Close)),
        );
        Some(dialog_ui::overlay(
            dialog_ui::surface(content, 800.0),
            Some(Message::ResumeConversation(Action::Close)),
            1.0,
        ))
    }

    fn resume_entries(&self) -> Vec<&SavedConversation> {
        let query = self.resume_ui.query.to_lowercase();
        self.resume_ui
            .entries
            .iter()
            .filter(|entry| {
                let recipe = &entry.session.launch.recipe;
                format!(
                    "{} {} {} {}",
                    entry.title,
                    entry.session.launch.provider.label(),
                    recipe.working_directory.display(),
                    self.resume_account_label(&entry.session)
                )
                .to_lowercase()
                .contains(&query)
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn saved(directory: &Path) -> SavedConversation {
        let mut session = chat::fresh_session(
            Provider::Claude,
            TerminalId::new(),
            directory.into(),
            &crate::account::Account::system(Provider::Claude),
        )
        .unwrap();
        session.launch.session_id = Some("f0e49307-af96-4edd-b23f-60b185fe4fea".into());
        SavedConversation {
            card: CardId::new(),
            title: "Review peripheral Rust modules".into(),
            session,
            open: false,
            updated_at_unix_ms: 1,
        }
    }

    #[test]
    fn closed_conversation_resumes_exact_identity_in_its_original_project() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let entry = saved(directory.path());
        let original = entry.session.clone();
        let _ = app.resume_saved_conversation(entry);
        let card = app.card_state(app.workspace.focused().unwrap()).unwrap();
        let CardKind::Chat(session) = &card.kind else {
            panic!("native conversation")
        };
        assert_ne!(session.id, original.id);
        assert_eq!(session.launch, original.launch);
        assert!(session.proof.is_none());
        assert!(session.terminal.is_none());
        assert_eq!(app.active_workspace.as_deref(), Some(directory.path()));
        assert_eq!(card.title, "Review peripheral Rust modules");
        // An already-open conversation focuses its original pane instead of
        // creating another provider writer.
        let entry = SavedConversation {
            card: card.id,
            title: card.title.clone(),
            session: session.clone(),
            open: true,
            updated_at_unix_ms: 1,
        };
        let before = app.durable.cards.len();
        let _ = app.resume_saved_conversation(entry);
        assert_eq!(app.durable.cards.len(), before);
    }

    #[test]
    fn picker_search_and_stale_results_do_not_change_the_workspace() {
        let directory = tempfile::tempdir().unwrap();
        let mut app = crate::app::tests::context_inspector_test_app(false);
        app.resume_ui.open = true;
        app.resume_ui.generation = 2;
        app.resume_ui.limit = 100;
        let _ = app.update_resume(Action::Loaded(1, Ok(vec![saved(directory.path())])));
        assert!(app.resume_ui.entries.is_empty());
        let _ = app.update_resume(Action::Loaded(2, Ok(vec![saved(directory.path())])));
        let _ = app.update_resume(Action::Query("PERIPHERAL".into()));
        assert_eq!(app.resume_entries().len(), 1);
        let _ = app.update_resume(Action::Query("not this conversation".into()));
        assert!(app.resume_entries().is_empty());
        let _ = app.update_resume(Action::Submit);
        assert!(app.durable.cards.is_empty());
        let _ = app.update_resume(Action::Close);
        let _ = app.update_resume(Action::Loaded(2, Ok(vec![saved(directory.path())])));
        assert!(!app.resume_ui.open);
        assert!(app.durable.cards.is_empty());
    }

    #[test]
    fn transcript_title_keeps_custom_name_and_ignores_sidechains() {
        let directory = tempfile::tempdir().unwrap();
        let mut entry = saved(directory.path());
        let home = directory.path().join("claude");
        entry
            .session
            .launch
            .recipe
            .environment
            .insert("CLAUDE_CONFIG_DIR".into(), home.display().to_string());
        let encoded: String = directory
            .path()
            .to_str()
            .unwrap()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let project = home.join("projects").join(encoded);
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(format!("{}.jsonl", entry.session.launch.session_id.as_ref().unwrap())),
            concat!("{\"type\":\"user\",\"isSidechain\":true,\"message\":{\"content\":\"Wrong subagent\"}}\n",
                "{\"type\":\"user\",\"message\":{\"content\":[{\"type\":\"text\",\"text\":\"Review Rust\"}]}}\n",
                "{\"type\":\"custom-title\",\"customTitle\":\"My saved review\"}\n",
                "{\"type\":\"summary\",\"summary\":\"Generated summary\"}\n")).unwrap();
        assert_eq!(
            claude_title(&entry.session).as_deref(),
            Some("My saved review")
        );
    }
}
