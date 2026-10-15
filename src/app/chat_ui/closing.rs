//! Hiding a view and ending the conversation are separate operations.
use super::*;

#[derive(Debug, Clone, Copy)]
pub(in crate::app) enum Action {
    End(CardId),
    Cancel(DialogTarget),
    Confirm(DialogTarget),
    Hide(DialogTarget),
    CommitHide(CardId),
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::app) struct DialogTarget {
    card: CardId,
    generation: u64,
}
pub(in crate::app) struct Confirmation {
    pub card: CardId,
    pub tab: bool,
    generation: u64,
}
impl Confirmation {
    pub(in crate::app) fn target(&self) -> DialogTarget {
        DialogTarget {
            card: self.card,
            generation: self.generation,
        }
    }
}

pub(in crate::app) fn draft_key(session: &chat::Session) -> Option<String> {
    let id = session.launch.session_id.as_deref()?.trim();
    if id.is_empty() {
        return None;
    }
    Some(format!("{}:{id}", session.launch.provider.label()))
}

impl Cagents {
    pub(in crate::app) fn reconcile_chat_close_backing_state(&mut self) {
        if self.chat_ui.close_dialog.as_ref().is_some_and(|dialog| {
            !self
                .card_state(dialog.card)
                .is_some_and(|card| card.existence.is_open())
        }) {
            self.chat_ui.close_dialog = None;
        }
    }
    pub(in crate::app) fn remember_conversation_draft(&mut self, card: CardId) {
        let Some(session) = self.chat_return_session(card) else {
            return;
        };
        let Some(key) = draft_key(&session) else {
            return;
        };
        let draft = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(runtime.unsent_text()),
            _ => self
                .chat_ui
                .parked
                .get(&card)
                .map(|runtime| runtime.unsent_text())
                .or_else(|| self.chat_ui.drafts.get(&card).cloned()),
        };
        let Some(draft) = draft else {
            return;
        };
        let model = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(&**runtime),
            _ => self.chat_ui.parked.get(&card).map(|runtime| &**runtime),
        }
        .and_then(|runtime| {
            runtime
                .snapshot
                .selected_model
                .clone()
                .or_else(|| runtime.recorded_model.clone())
        });
        let channel = self
            .durable
            .channels
            .iter()
            .find(|channel| channel.cards.contains(&card))
            .map(|channel| channel.id);
        let durable = Arc::make_mut(&mut self.durable);
        if let Some(channel) = channel {
            durable.conversation_channels.insert(key.clone(), channel);
        }
        if let Some(model) = model {
            durable.conversation_models.insert(key.clone(), model);
        }
        let saved = &mut durable.conversation_drafts;
        if draft.is_empty() {
            saved.remove(&key);
        } else {
            saved.insert(key, draft);
        }
    }
    fn chat_close_needs_confirmation(&self, card: CardId) -> bool {
        let runtime = match self.cards.get(&card) {
            Some(CardRuntime::Chat(runtime)) => Some(&**runtime),
            _ => self.chat_ui.parked.get(&card).map(|runtime| &**runtime),
        };
        self.chat_ui.phases.contains_key(&card)
            || runtime.is_some_and(|runtime| {
                runtime.snapshot.busy
                    || runtime.handle().is_some_and(|handle| {
                        !runtime.visible.load(Ordering::Relaxed) || handle.snapshot().busy
                    })
                    || !runtime.unsent_text().is_empty()
                    || !runtime.attachments.is_empty()
                    || runtime.queued.is_some()
                    || runtime.pending.is_some()
                    || !runtime.followups.is_empty()
            })
            || self
                .chat_ui
                .drafts
                .get(&card)
                .is_some_and(|draft| !draft.is_empty())
    }
    pub(in crate::app) fn request_chat_tab_close(&mut self, card: CardId) -> Task<Message> {
        if self.chat_is_unused(card) {
            return self.begin_chat_close(card);
        }
        if self.chat_close_needs_confirmation(card) {
            return self.open_chat_close_confirmation(card, true);
        }
        self.hide_chat_tab(card)
    }
    fn open_chat_close_confirmation(&mut self, card: CardId, tab: bool) -> Task<Message> {
        if self.chat_ui.close_dialog.is_some() {
            return Task::none();
        }
        self.chat_ui.close_generation = self.chat_ui.close_generation.wrapping_add(1);
        self.chat_ui.close_dialog = Some(Confirmation {
            card,
            tab,
            generation: self.chat_ui.close_generation,
        });
        iced::widget::operation::focus(widget::Id::new("chat-close.cancel"))
            .map(cosmic::Action::App)
    }
    pub(in crate::app) fn cancel_chat_close(&mut self) -> Task<Message> {
        let Some(target) = self.chat_ui.close_dialog.as_ref().map(Confirmation::target) else {
            return Task::none();
        };
        self.update_chat_close(Action::Cancel(target))
    }
    pub(in crate::app) fn update_chat_close(&mut self, action: Action) -> Task<Message> {
        if self.update_prepared && !matches!(action, Action::CommitHide(_)) {
            return Task::none();
        }
        let target = match action {
            Action::Cancel(target) | Action::Confirm(target) | Action::Hide(target) => Some(target),
            _ => None,
        };
        if target.is_some_and(|target| {
            self.chat_ui
                .close_dialog
                .as_ref()
                .is_none_or(|dialog| dialog.target() != target)
        }) {
            return Task::none();
        }
        match action {
            Action::End(card) => {
                if self.chat_close_needs_confirmation(card) {
                    self.open_chat_close_confirmation(card, false)
                } else {
                    self.begin_chat_close(card)
                }
            }
            Action::Cancel(_) => {
                let Some(dialog) = self.chat_ui.close_dialog.take() else {
                    return Task::none();
                };
                let restore =
                    resolve_restore_card(Some(dialog.card), self.workspace.focused(), |card| {
                        self.workspace.order().any(|id| id == card)
                    });
                let target = restore
                    .map(|card| self.card_keyboard_focus(card))
                    .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID));
                iced::widget::operation::focus(target).map(cosmic::Action::App)
            }
            Action::Confirm(_) => {
                let Some(dialog) = self.chat_ui.close_dialog.take() else {
                    return Task::none();
                };
                self.begin_chat_close(dialog.card)
            }
            Action::Hide(_) => {
                let Some(dialog) = self.chat_ui.close_dialog.take() else {
                    return Task::none();
                };
                self.hide_chat_tab(dialog.card)
            }
            Action::CommitHide(card) => {
                let update = match self.workspace.commit_close(card) {
                    Ok(update) => update,
                    Err(error) => {
                        self.status = Some(TransientNotice::warning(error.to_string()));
                        return Task::none();
                    }
                };
                self.remember_conversation_draft(card);
                if let Some(CardRuntime::Chat(runtime)) = self.cards.get_mut(&card) {
                    let _ = runtime.set_visible(false);
                }
                // Keep the descriptor, channel membership, runtime and provider.
                // Selecting its sidebar row reconstructs the view from those.
                self.apply_viewport(update, true, true)
            }
        }
    }
    fn hide_chat_tab(&mut self, card: CardId) -> Task<Message> {
        let handoff = match self.workspace.request_close(card) {
            Ok(handoff) => handoff,
            Err(error) => {
                self.status = Some(TransientNotice::warning(error.to_string()));
                return Task::none();
            }
        };
        workspace_widget::focus_handoff_then(
            handoff,
            widget::Id::new(TOOLBAR_FOCUS_ID),
            |next| self.card_keyboard_focus(next),
            Message::ChatClose(Action::CommitHide(card)),
        )
        .map(cosmic::Action::App)
    }
    pub(in crate::app) fn chat_close_dialog(&self) -> Option<Element<'_, Message>> {
        let dialog = self.chat_ui.close_dialog.as_ref()?;
        let target = dialog.target();
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::title("Close conversation?").into(),
            notice_body(
                if self
                    .chat_return_session(dialog.card)
                    .as_ref()
                    .and_then(draft_key)
                    .is_some()
                {
                    "Closing ends the agent's running turn. Unsent text is saved for this conversation and restored on resume. Attached files must be added again."
                } else if dialog.tab {
                    "This conversation has not received a provider session ID. Closing discards its unsent draft; cancel or hide the tab to keep it."
                } else {
                    "This conversation has not received a provider session ID. Closing discards its unsent draft; cancel to keep it."
                },
            ),
        ];
        if dialog.tab {
            rows.push(
                crate::ui_controls::button::secondary("Hide tab and keep conversation")
                    .on_press(Message::ChatClose(Action::Hide(target)))
                    .into(),
            );
        }
        rows.push(dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel")
                .id(widget::Id::new("chat-close.cancel"))
                .on_press(Message::ChatClose(Action::Cancel(target))),
            crate::ui_controls::button::destructive("Close conversation")
                .id(widget::Id::new("chat-close.confirm"))
                .on_press(Message::ChatClose(Action::Confirm(target))),
        ));
        Some(dialog_ui::overlay(
            dialog_ui::surface(widget::column::with_children(rows).spacing(space::M), 480.0),
            Some(Message::ChatClose(Action::Cancel(target))),
            1.0,
        ))
    }
}

pub(in crate::app) fn recipe_model(recipe: &LaunchRecipe) -> Option<String> {
    recipe
        .arguments
        .iter()
        .enumerate()
        .find_map(|(index, arg)| {
            arg.strip_prefix("--model=").map(str::to_owned).or_else(|| {
                (arg == "--model" || arg == "-m")
                    .then(|| recipe.arguments.get(index + 1).cloned())
                    .flatten()
            })
        })
}
pub(in crate::app) fn clear_model(recipe: &mut LaunchRecipe) {
    let mut index = 0;
    while index < recipe.arguments.len() {
        if recipe.arguments[index] == "--model" || recipe.arguments[index] == "-m" {
            let end = (index + 2).min(recipe.arguments.len());
            recipe.arguments.drain(index..end);
        } else if recipe.arguments[index].starts_with("--model=") {
            recipe.arguments.remove(index);
        } else {
            index += 1;
        }
    }
}
pub(in crate::app) fn set_model(recipe: &mut LaunchRecipe, model: &str) {
    clear_model(recipe);
    recipe
        .arguments
        .extend(["--model".into(), model.to_owned()]);
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Cagents, CardId, tempfile::TempDir) {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let directory = tempfile::tempdir().unwrap();
        drop(app.new_native_chat(Provider::Codex, Some(directory.path().into())));
        let card = app.workspace.focused().unwrap();
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("draft chat");
        };
        runtime.session.launch.session_id = Some("saved-conversation".into());
        if let Some(state) = app.card_state_mut(card) {
            if let CardKind::Chat(session) = &mut state.kind {
                session.launch.session_id = Some("saved-conversation".into());
            }
        }
        (app, card, directory)
    }
    fn respond(app: &mut Cagents, action: fn(DialogTarget) -> Action) {
        let target = app.chat_ui.close_dialog.as_ref().unwrap().target();
        drop(app.update_chat_close(action(target)));
    }
    fn write(app: &mut Cagents, card: CardId, text: &str, busy: bool) {
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("chat");
        };
        runtime.replace_draft(text_editor::Content::with_text(text));
        Arc::make_mut(&mut runtime.snapshot).busy = busy;
    }
    #[test]
    fn prepared_update_blocks_new_end_confirmations_but_admits_hide_completion() {
        let (mut app, card, _directory) = fixture();
        write(&mut app, card, "retained prompt", true);
        app.update_prepared = true;
        drop(app.update(Message::ChatClose(Action::End(card))));
        assert!(app.chat_ui.close_dialog.is_none());
        app.update_prepared = false;
        drop(app.update(Message::ChatClose(Action::End(card))));
        assert!(app.chat_ui.close_dialog.is_some());
        app.update_prepared = true;
        respond(&mut app, Action::Confirm);
        assert!(app.chat_ui.close_dialog.is_some());
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(!super::super::super::user_message_blocked_during_update(
            &Message::ChatClose(Action::CommitHide(card))
        ));
    }

    #[test]
    fn obsolete_confirmation_cannot_end_a_reopened_dialog_even_for_the_same_card() {
        let (mut app, card, _directory) = fixture();
        write(&mut app, card, "keep this prompt", true);
        drop(app.update_chat_close(Action::End(card)));
        let previous = app.chat_ui.close_dialog.as_ref().unwrap().target();
        drop(app.cancel_chat_close());
        drop(app.update_chat_close(Action::End(card)));
        let current = app.chat_ui.close_dialog.as_ref().unwrap().target();
        assert_ne!(previous, current);
        for stale in [
            Action::Confirm(previous),
            Action::Cancel(previous),
            Action::Hide(previous),
        ] {
            drop(app.update_chat_close(stale));
            assert_eq!(app.chat_ui.close_dialog.as_ref().unwrap().target(), current);
            assert!(app.card_state(card).unwrap().existence.is_open());
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
                panic!("chat retained");
            };
            assert_eq!(runtime.unsent_text().trim(), "keep this prompt");
        }
    }

    #[test]
    fn busy_or_drafted_tab_close_confirms_and_cancel_changes_nothing() {
        for (draft, busy) in [
            ("unsent prompt", false),
            ("", true),
            ("draft while working", true),
        ] {
            let (mut app, card, _directory) = fixture();
            write(&mut app, card, draft, busy);
            let task = app.update(Message::CloseCard(card));
            assert!(app.chat_ui.close_dialog.as_ref().unwrap().tab);
            assert_eq!(
                crate::app::tests::focused_element(app.chat_close_dialog().unwrap(), task),
                Some(widget::Id::new("chat-close.cancel"))
            );
            drop(app.cancel_chat_close());
            assert!(app.chat_ui.close_dialog.is_none());
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
                panic!("cancel retained runtime");
            };
            assert_eq!(runtime.unsent_text().trim(), draft);
            assert_eq!(runtime.snapshot.busy, busy);
            assert!(app.workspace.order().any(|id| id == card));
            assert!(app.card_state(card).unwrap().existence.is_open());
        }
    }
    #[test]
    fn a_close_confirmation_disappears_when_its_conversation_was_closed_elsewhere() {
        let (mut app, card, _directory) = fixture();
        write(&mut app, card, "retained", true);
        drop(app.update(Message::CloseCard(card)));
        assert!(app.chat_ui.close_dialog.is_some());
        drop(app.begin_chat_close(card));
        drop(app.update(Message::DismissStatus));
        assert!(app.chat_ui.close_dialog.is_none());
        assert!(!app.modal_is_open());
    }

    #[test]
    fn confirmed_end_saves_draft_for_session_and_recreated_card_restores_it() {
        let (mut app, card, _directory) = fixture();
        write(&mut app, card, "keep across cards", true);
        let session = app.chat_return_session(card).unwrap();
        drop(app.update(Message::CloseCard(card)));
        respond(&mut app, Action::Confirm);
        assert!(!app.card_state(card).unwrap().existence.is_open());
        assert!(!app.cards.contains_key(&card));
        let key = draft_key(&session).unwrap();
        assert_eq!(
            app.persistent_workspace().conversation_drafts[&key],
            "keep across cards"
        );
        drop(app.insert_chat_card(session, None, None));
        let reopened = app.workspace.focused().unwrap();
        assert_ne!(reopened, card);
        assert_eq!(app.chat_ui.drafts[&reopened], "keep across cards");
    }
    #[test]
    fn ending_and_resuming_a_new_card_retains_the_original_channel() {
        let (mut app, card, _directory) = fixture();
        let channel = crate::model::ChannelId::new();
        let project = crate::model::ProjectId::new();
        app.sidebar_model
            .projects
            .push(crate::sidebar::SidebarProject::new(
                project,
                crate::repository::RepositoryId::new(),
                "Original project".into(),
                _directory.path().into(),
                Vec::new(),
                Vec::new(),
            ));
        let durable = Arc::make_mut(&mut app.durable);
        durable.channels.push(crate::model::ProjectChannel {
            id: channel,
            project_id: project,
            title: "Original channel".into(),
            title_source: Default::default(),
            ordinal: 0,
            shared: false,
            cards: vec![card],
            focused: Some(card),
        });
        durable.active_channel = Some(channel);
        write(&mut app, card, "original draft", true);
        let session = app.chat_return_session(card).unwrap();
        drop(app.begin_chat_close(card));
        assert_eq!(
            app.durable
                .conversation_channels
                .get(&draft_key(&session).unwrap()),
            Some(&channel)
        );
        drop(
            app.resume_saved_conversation(crate::persistence::SavedConversation {
                card,
                title: "Resumed conversation".into(),
                session,
                open: false,
                updated_at_unix_ms: 0,
            }),
        );
        let reopened = app.workspace.focused().unwrap();
        assert_ne!(reopened, card);
        assert_eq!(app.durable.active_channel, Some(channel));
        assert!(
            app.durable
                .channels
                .iter()
                .find(|row| row.id == channel)
                .unwrap()
                .cards
                .contains(&reopened)
        );
        assert_eq!(app.chat_ui.drafts[&reopened], "original draft");
    }

    #[test]
    fn hiding_preserves_conversation_membership_runtime_and_draft() {
        let (mut app, card, _directory) = fixture();
        let project = crate::model::ProjectId::new();
        let channel = crate::model::ChannelId::new();
        app.sidebar_model
            .projects
            .push(crate::sidebar::SidebarProject::new(
                project,
                crate::repository::RepositoryId::new(),
                "Project".into(),
                _directory.path().into(),
                Vec::new(),
                Vec::new(),
            ));
        Arc::make_mut(&mut app.durable)
            .channels
            .push(crate::model::ProjectChannel {
                id: channel,
                project_id: project,
                title: "Conversation".into(),
                title_source: Default::default(),
                ordinal: 0,
                shared: false,
                cards: vec![card],
                focused: Some(card),
            });
        Arc::make_mut(&mut app.durable).active_channel = Some(channel);
        write(&mut app, card, "draft survives hide", false);
        drop(app.request_chat_tab_close(card));
        respond(&mut app, Action::Hide);
        drop(app.update_chat_close(Action::CommitHide(card)));
        assert!(!app.workspace.order().any(|id| id == card));
        assert!(app.card_state(card).unwrap().existence.is_open());
        assert!(app.cards.contains_key(&card));
        assert!(app.durable.channels[0].cards.contains(&card));
        drop(app.select_channel(channel));
        assert!(app.workspace.order().any(|id| id == card));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
            panic!("same runtime");
        };
        assert_eq!(runtime.unsent_text().trim(), "draft survives hide");
    }
    #[test]
    fn discovered_model_never_changes_an_issued_session_launch_recipe() {
        let (mut app, card, _directory) = fixture();
        let issued = app.chat_return_session(card).unwrap();
        let handle = Arc::new(ClientHandle::fixture(
            issued.clone(),
            Snapshot {
                session_id: issued.launch.session_id.clone(),
                selected_model: Some("discovered-model".into()),
                ..Snapshot::default()
            },
        ));
        let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
            panic!("chat");
        };
        runtime.connection = Connection::Live(Arc::clone(&handle));
        runtime.snapshot = handle.snapshot();
        runtime.recorded_model = Some("discovered-model".into());
        assert_eq!(
            app.chat_return_session(card).unwrap().launch.recipe,
            issued.launch.recipe
        );
        app.remember_conversation_draft(card);
        let saved = app.persistent_workspace();
        assert_eq!(
            saved.conversation_models[&draft_key(&issued).unwrap()],
            "discovered-model"
        );
        assert_eq!(handle.session().launch.recipe, issued.launch.recipe);
        assert_eq!(
            app.chat_return_session(card).unwrap().launch.recipe,
            issued.launch.recipe
        );
        drop(app.begin_chat_close(card));
        let CardKind::Chat(saved) = &app.card_state(card).unwrap().kind else {
            panic!("chat descriptor");
        };
        assert_eq!(saved.launch.recipe, issued.launch.recipe);
        assert_eq!(handle.session().launch.recipe, issued.launch.recipe);
    }

    #[test]
    fn live_conversation_confirms_without_hydrating_its_stale_idle_snapshot() {
        for visible in [false, true] {
            let (mut app, card, _directory) = fixture();
            let session = app.chat_return_session(card).unwrap();
            let handle = Arc::new(ClientHandle::fixture(
                session.clone(),
                Snapshot {
                    session_id: session.launch.session_id.clone(),
                    busy: true,
                    ..Snapshot::default()
                },
            ));
            let Some(CardRuntime::Chat(runtime)) = app.cards.get_mut(&card) else {
                panic!("chat");
            };
            runtime.connection = Connection::Live(Arc::clone(&handle));
            runtime.visible.store(visible, Ordering::Relaxed);
            assert!(!runtime.snapshot.busy);
            let presentation = Arc::clone(&runtime.snapshot);
            drop(app.update_chat_close(Action::End(card)));
            assert!(app.chat_ui.close_dialog.is_some());
            let Some(CardRuntime::Chat(runtime)) = app.cards.get(&card) else {
                panic!("chat");
            };
            assert!(Arc::ptr_eq(&presentation, &runtime.snapshot));
            assert!(handle.snapshot().busy);
            drop(app.cancel_chat_close());
            assert!(app.cards.contains_key(&card));
            assert!(handle.snapshot().busy);
        }
    }

    #[test]
    fn model_flags_are_replaced_without_retaining_a_seed_models_argument() {
        let (app, card, _directory) = fixture();
        let mut recipe = app.chat_return_session(card).unwrap().launch.recipe;
        recipe
            .arguments
            .extend(["--model=seed".into(), "-m".into(), "obsolete".into()]);
        set_model(&mut recipe, "recorded-model");
        assert_eq!(recipe_model(&recipe).as_deref(), Some("recorded-model"));
        assert_eq!(
            recipe
                .arguments
                .iter()
                .filter(|arg| *arg == "--model")
                .count(),
            1
        );
        assert!(
            !recipe
                .arguments
                .iter()
                .any(|arg| arg == "obsolete" || arg.contains("seed"))
        );
    }
}
