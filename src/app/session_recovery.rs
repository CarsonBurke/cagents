use super::*;
use crate::persistence::TerminalPaneAdoption;

#[derive(Default)]
pub(super) struct State {
    target: Option<CardId>,
    review: Option<Review>,
    preparing: bool,
    pending: bool,
    generation: u64,
    error: Option<String>,
    focus: usize,
    restore_keyboard_nav: Option<bool>,
}

impl State {
    pub(super) fn open(&self) -> bool {
        self.target.is_some()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Review {
    recovery: TerminalPaneAdoption,
    terminal: TerminalSession,
    source_existence: Existence,
    source_title: String,
    target_title: String,
}

impl Review {
    fn from_snapshot(
        state: &WorkspaceState,
        recovery: TerminalPaneAdoption,
    ) -> Result<Self, String> {
        let source = state.cards.iter().find(|card| card.id == recovery.alias);
        let target = state
            .cards
            .iter()
            .find(|card| card.id == recovery.canonical);
        let (Some(source), Some(target)) = (source, target) else {
            return Err("Both terminal panes must remain in this window.".into());
        };
        let (CardKind::Terminal(archived), CardKind::Terminal(live)) = (&source.kind, &target.kind)
        else {
            return Err("The panes no longer hold the reviewed terminal.".into());
        };
        if !matches!(source.existence, Existence::Closed { .. })
            || !target.existence.is_open()
            || archived != live
            || live.id != recovery.terminal
        {
            return Err("The terminal recipe or continuity evidence changed. Review the current save notice.".into());
        }
        Ok(Self {
            recovery,
            terminal: live.clone(),
            source_existence: source.existence.clone(),
            source_title: source.title.clone(),
            target_title: target.title.clone(),
        })
    }
}

#[derive(Clone, Debug)]
pub enum Action {
    Open(CardId),
    Prepared {
        generation: u64,
        result: Result<Review, String>,
    },
    Cancel,
    Confirm,
    Finished(Result<(), String>),
}

impl Cagents {
    pub(super) fn refused_terminal_recovery_target(&self) -> Option<CardId> {
        let health = self.save_health.as_ref()?;
        if !matches!(
            health.health,
            SessionSaveHealth::Healthy | SessionSaveHealth::Recovered { .. }
        ) || self.session.is_none()
        {
            return None;
        }
        health.quarantined.iter().find_map(|write| {
            let WriteSubject::Workspace { card: Some(id) } = write.subject else {
                return None;
            };
            if self.shared_views.defaults.contains_key(&id) || self.chat_is_unused_default(id) {
                return None;
            }
            let target = self.card_state(id)?;
            let CardKind::Terminal(live) = &target.kind else {
                return None;
            };
            (target.existence.is_open()
                && self.durable.cards.iter().any(|source| {
                    source.id != id
                        && matches!(source.existence, Existence::Closed { .. })
                        && matches!(&source.kind, CardKind::Terminal(archived) if archived == live)
                }))
            .then_some(id)
        })
    }

    pub(super) fn session_recovery_action(&self) -> Option<Element<'_, Message>> {
        let card = self.refused_terminal_recovery_target()?;
        Some(
            crate::ui_controls::button::secondary("Recover terminal ownership…")
                .id(widget::Id::new("session-recovery.open"))
                .name("Review recovery of the refused terminal from its closed pane")
                .on_press_maybe(
                    (!self.modal_is_open()).then_some(Message::SessionRecovery(Action::Open(card))),
                )
                .into(),
        )
    }

    fn session_recovery_review_is_current(&self, review: &Review) -> bool {
        self.refused_terminal_recovery_target() == Some(review.recovery.canonical)
            && Review::from_snapshot(&self.durable, review.recovery).as_ref() == Ok(review)
    }

    pub(super) fn update_session_recovery(&mut self, action: Action) -> Task<Message> {
        match action {
            Action::Open(card)
                if !self.modal_is_open()
                    && self.refused_terminal_recovery_target() == Some(card) =>
            {
                let snapshot = self.persistent_workspace();
                let Some(target) = snapshot.cards.iter().find(|state| state.id == card) else {
                    return Task::none();
                };
                let CardKind::Terminal(terminal) = &target.kind else {
                    return Task::none();
                };
                let terminal = terminal.id;
                let store = self.session.as_ref().unwrap().operation_store();
                let state = &mut self.session_recovery;
                state.generation = state.generation.wrapping_add(1);
                state.target = Some(card);
                state.review = None;
                state.error = None;
                state.preparing = true;
                state.pending = false;
                state.focus = 0;
                state.restore_keyboard_nav = Some(self.core.keyboard_nav());
                self.core.set_keyboard_nav(false);
                let generation = state.generation;
                Task::batch([
                    recovery_focus(0),
                    cosmic::task::future(async move {
                        let result = tokio::task::spawn_blocking(move || {
                            let recovery = store
                                .closed_terminal_pane_adoption(&snapshot, card, terminal)
                                .map_err(|error| error.to_string())?
                                .ok_or_else(|| {
                                    "Storage no longer has the exact closed owner in this window."
                                        .to_owned()
                                })?;
                            Review::from_snapshot(&snapshot, recovery)
                        })
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result);
                        Message::SessionRecovery(Action::Prepared { generation, result })
                    }),
                ])
            }
            Action::Prepared { generation, result }
                if self.session_recovery.open()
                    && self.session_recovery.generation == generation =>
            {
                self.session_recovery.preparing = false;
                match result {
                    Ok(review) if self.session_recovery_review_is_current(&review) => {
                        self.session_recovery.review = Some(review)
                    }
                    Ok(_) => {
                        self.session_recovery.error = Some(
                            "The refused pane changed. Cancel and review the current save notice."
                                .into(),
                        )
                    }
                    Err(error) => self.session_recovery.error = Some(error),
                }
                self.session_recovery.focus = 0;
                recovery_focus(0)
            }
            Action::Cancel if self.session_recovery.open() && !self.session_recovery.pending => {
                self.close_session_recovery()
            }
            Action::Confirm if self.session_recovery.open() && !self.session_recovery.pending => {
                let Some(review) = self.session_recovery.review.clone() else {
                    return Task::none();
                };
                if !self.session_recovery_review_is_current(&review) {
                    self.session_recovery.focus = 0;
                    self.session_recovery.error = Some(
                        "The refused pane changed. Cancel and review the current save notice."
                            .into(),
                    );
                    return recovery_focus(0);
                }
                let snapshot = self.persistent_workspace();
                let receipt = self
                    .session
                    .as_ref()
                    .unwrap()
                    .recover_terminal_ownership(snapshot, review.recovery);
                self.session_recovery.pending = true;
                self.session_recovery.error = None;
                cosmic::task::future(async move {
                    let result = match receipt {
                        Ok(receipt) => tokio::task::spawn_blocking(move || {
                            receipt
                                .recv()
                                .map_err(|error| format!("Session writer disconnected: {error}"))
                                .and_then(|result| result)
                        })
                        .await
                        .map_err(|error| error.to_string())
                        .and_then(|result| result),
                        Err(error) => Err(error),
                    };
                    Message::SessionRecovery(Action::Finished(result))
                })
            }
            Action::Finished(result) if self.session_recovery.pending => {
                self.session_recovery.pending = false;
                match result {
                    Ok(()) => self.close_session_recovery(),
                    Err(error) => {
                        self.session_recovery.error = Some(error);
                        self.session_recovery.focus = 0;
                        recovery_focus(0)
                    }
                }
            }
            _ => Task::none(),
        }
    }

    fn close_session_recovery(&mut self) -> Task<Message> {
        self.session_recovery.target = None;
        self.session_recovery.review = None;
        self.session_recovery.preparing = false;
        self.session_recovery.error = None;
        if let Some(restore) = self.session_recovery.restore_keyboard_nav.take() {
            self.core.set_keyboard_nav(restore);
        }
        let target = self
            .workspace
            .focused()
            .map(|card| self.card_keyboard_focus(card))
            .unwrap_or_else(|| widget::Id::new(TOOLBAR_FOCUS_ID));
        iced::widget::operation::focus(target).map(cosmic::Action::App)
    }

    pub(super) fn adopt_session_recovery_keyboard_nav(&mut self, restore: bool) -> bool {
        if !self.session_recovery.open() {
            return false;
        }
        self.session_recovery.restore_keyboard_nav = Some(restore);
        self.core.set_keyboard_nav(false);
        true
    }

    pub(super) fn session_recovery_key_event(
        &mut self,
        event: &keyboard::Event,
        captured: bool,
    ) -> Option<Task<Message>> {
        if !self.session_recovery.open() {
            return None;
        }
        let keyboard::Event::KeyPressed { key, modifiers, .. } = event else {
            return None;
        };
        let keyboard::Key::Named(key) = key else {
            return Some(Task::none());
        };
        use keyboard::key::Named;
        if *key == Named::Escape {
            return Some(self.update_session_recovery(Action::Cancel));
        }
        if self.session_recovery.pending {
            return Some(Task::none());
        }
        if matches!(key, Named::Tab | Named::ArrowLeft | Named::ArrowRight) {
            let count = 1 + usize::from(
                self.session_recovery
                    .review
                    .as_ref()
                    .is_some_and(|review| self.session_recovery_review_is_current(review)),
            );
            let backwards = modifiers.shift() || *key == Named::ArrowLeft;
            self.session_recovery.focus = if backwards {
                (self.session_recovery.focus.min(count - 1) + count - 1) % count
            } else {
                (self.session_recovery.focus + 1) % count
            };
            return Some(recovery_focus(self.session_recovery.focus));
        }
        if *key == Named::Enter
            && !captured
            && !modifiers.control()
            && !modifiers.alt()
            && !modifiers.logo()
        {
            return Some(
                self.update_session_recovery(if self.session_recovery.focus == 1 {
                    Action::Confirm
                } else {
                    Action::Cancel
                }),
            );
        }
        Some(Task::none())
    }

    pub(super) fn session_recovery_dialog(&self) -> Option<Element<'_, Message>> {
        let state = &self.session_recovery;
        if !state.open() {
            return None;
        }
        let mut rows: Vec<Element<'_, Message>> = vec![
            design::title("Recover terminal ownership?").into(),
            notice_body(
                "The live pane has not been saved because its terminal belongs to a closed pane. Recovery preserves the closed pane's history and saves this window's current layout, panes, and drafts together.",
            ),
        ];
        if let Some(review) = &state.review {
            rows.push(notice_detail(format!(
                "Closed pane: “{}”\nLive pane: “{}”",
                review.source_title, review.target_title
            )));
            rows.push(notice_detail("Only this exact closed owner's terminal will move to the live pane. Recovery does not start, stop, or replace the terminal. If either pane's identity or saved state changed, storage refuses the transfer."));
        }
        if state.preparing {
            rows.push(notice_detail("Verifying the saved closed owner…"));
        }
        if state.pending {
            rows.push(notice_detail("Saving the recovered workspace…"));
        }
        if let Some(error) = &state.error {
            rows.push(notice_detail(error));
        }
        rows.push(dialog_ui::confirmation_actions(
            crate::ui_controls::button::secondary("Cancel")
                .id(widget::Id::new("session-recovery.cancel"))
                .on_press_maybe(
                    (!state.pending).then_some(Message::SessionRecovery(Action::Cancel)),
                ),
            crate::ui_controls::button::primary("Recover and save")
                .id(widget::Id::new("session-recovery.confirm"))
                .on_press_maybe(
                    (!state.pending
                        && state
                            .review
                            .as_ref()
                            .is_some_and(|review| self.session_recovery_review_is_current(review)))
                    .then_some(Message::SessionRecovery(Action::Confirm)),
                ),
        ));
        Some(dialog_ui::overlay(
            dialog_ui::surface(widget::column::with_children(rows).spacing(space::M), 600.0),
            (!state.pending).then_some(Message::SessionRecovery(Action::Cancel)),
            1.0,
        ))
    }
}

fn recovery_focus(index: usize) -> Task<Message> {
    iced::widget::operation::focus(widget::Id::new(if index == 0 {
        "session-recovery.cancel"
    } else {
        "session-recovery.confirm"
    }))
    .map(cosmic::Action::App)
}

#[cfg(test)]
mod tests {
    use super::*;
    use cosmic::iced::futures::StreamExt;

    fn fixture() -> (tempfile::TempDir, Cagents, CardId) {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("ownership.sqlite3");
        let (session, _) =
            SessionRuntime::open(&path, crate::model::BootId::new("ui-recovery")).unwrap();
        let health = session.health_receiver();
        let mut app = super::super::tests::context_inspector_test_app(true);
        let mut state = (*app.durable).clone();
        let timeout = Duration::from_secs(5);
        session.snapshot(Arc::new(state.clone()));
        session.flush(timeout).unwrap();
        state.cards[0].title = "Archived shell".into();
        state.cards[0].existence = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 773,
                reason: Some("user closed".into()),
            },
        };
        state.focused = None;
        session.snapshot(Arc::new(state.clone()));
        session.flush(timeout).unwrap();
        let mut live = state.cards[0].clone();
        live.id = CardId::new();
        live.title = "Unsaved shell".into();
        live.existence = Existence::Open;
        let card = live.id;
        state.focused = Some(card);
        state.cards.push(live);
        session.snapshot(Arc::new(state.clone()));
        assert!(session.flush(timeout).is_err());
        let mut save_health = SaveHealthState::new(path);
        loop {
            save_health.apply(health.recv_blocking().unwrap());
            if !save_health.quarantined.is_empty() {
                break;
            }
        }
        app.workspace = Cagents::rebuild_workspace(&state);
        app.workspace.resize_viewport(1200.0);
        app.durable = Arc::new(state);
        app.cards.clear();
        app.cards.insert(card, CardRuntime::Loading);
        app.session = Some(session);
        app.save_health = Some(save_health);
        app.core.set_keyboard_nav(true);
        (directory, app, card)
    }

    #[test]
    fn notices_keyboard_traversal_reaches_the_eligible_ownership_recovery_control() {
        let (_directory, mut app, _card) = fixture();
        drop(app.update_chrome(chrome_ui::Action::ToggleNotices));
        let recovery = widget::Id::new("session-recovery.open");
        let mut reached = false;
        for _ in 0..10 {
            let task = app
                .chrome_key_event(&key(keyboard::key::Named::Tab, false))
                .unwrap();
            if super::super::tests::focused_element(app.session_recovery_action().unwrap(), task)
                == Some(recovery.clone())
            {
                reached = true;
                break;
            }
        }
        assert!(
            reached,
            "the popup's keyboard ring must reach its rendered recovery button"
        );
    }

    #[test]
    fn ownership_recovery_notice_stays_readable_at_the_real_popover_width() {
        let (_directory, app, _card) = fixture();
        let mut element = save_health_views(
            app.save_health.as_ref().unwrap(),
            |card| app.card_state(card).map(|card| card.title.as_str()),
            app.session_recovery_action(),
        )
        .pop()
        .unwrap();
        let mut tree = iced::advanced::widget::Tree::new(&element);
        use iced::advanced::renderer::Headless;
        let renderer = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(cosmic::Renderer::new(
                iced::Font::default(),
                iced::Pixels(14.0),
                Some("tiny-skia"),
            ))
            .unwrap();
        let node = element.as_widget_mut().layout(
            &mut tree,
            &renderer,
            &iced::advanced::layout::Limits::new(
                iced::Size::ZERO,
                iced::Size::new(design::NOTICE_POPOVER_WIDTH - 12.0, f32::INFINITY),
            ),
        );
        assert!(
            node.size().height < 408.0,
            "the quarantine notice must fit the popover instead of losing its text width: {:?}",
            node.size()
        );
    }

    fn reply(task: Task<Message>) -> Message {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let mut stream = iced::runtime::task::into_stream(task).unwrap();
                while let Some(action) = stream.next().await {
                    if let iced::runtime::Action::Output(cosmic::Action::App(
                        message @ Message::SessionRecovery(_),
                    )) = action
                    {
                        return message;
                    }
                }
                panic!("recovery task produced no receipt");
            })
    }

    fn key(key: keyboard::key::Named, shift: bool) -> keyboard::Event {
        keyboard::Event::KeyPressed {
            key: keyboard::Key::Named(key.clone()),
            modified_key: keyboard::Key::Named(key),
            physical_key: keyboard::key::Physical::Code(keyboard::key::Code::Tab),
            location: keyboard::Location::Standard,
            modifiers: if shift {
                keyboard::Modifiers::SHIFT
            } else {
                keyboard::Modifiers::empty()
            },
            text: None,
            repeat: false,
        }
    }

    #[test]
    fn ownership_recovery_confirmation_names_the_exact_owner_and_cancel_changes_no_work() {
        let (_directory, mut app, card) = fixture();
        let before = Arc::clone(&app.durable);
        let quarantine = app.save_health.as_ref().unwrap().quarantined.clone();
        let preparation = reply(app.update_session_recovery(Action::Open(card)));
        assert!(app.modal_is_open());
        assert!(!app.core.keyboard_nav());
        drop(app.update_session_recovery(Action::Confirm));
        assert!(!app.session_recovery.pending);
        let navigation = app
            .session_recovery_key_event(&key(keyboard::key::Named::Tab, false), false)
            .unwrap();
        assert_eq!(
            super::super::tests::focused_element(
                app.session_recovery_dialog().unwrap(),
                navigation
            ),
            Some(widget::Id::new("session-recovery.cancel"))
        );
        let focused = app.update(preparation);
        assert_eq!(
            super::super::tests::focused_element(app.session_recovery_dialog().unwrap(), focused),
            Some(widget::Id::new("session-recovery.cancel"))
        );
        let review = app.session_recovery.review.clone().unwrap();
        assert_eq!(review.source_title, "Archived shell");
        assert_eq!(review.target_title, "Unsaved shell");
        let navigation = app
            .session_recovery_key_event(&key(keyboard::key::Named::Tab, false), false)
            .unwrap();
        assert_eq!(
            super::super::tests::focused_element(
                app.session_recovery_dialog().unwrap(),
                navigation
            ),
            Some(widget::Id::new("session-recovery.confirm"))
        );
        let navigation = app
            .session_recovery_key_event(&key(keyboard::key::Named::Tab, true), false)
            .unwrap();
        assert_eq!(
            super::super::tests::focused_element(
                app.session_recovery_dialog().unwrap(),
                navigation
            ),
            Some(widget::Id::new("session-recovery.cancel"))
        );
        drop(app.session_recovery_key_event(&key(keyboard::key::Named::Escape, false), false));
        assert!(!app.modal_is_open());
        assert!(app.core.keyboard_nav());
        assert_eq!(app.durable, before);
        assert_eq!(app.save_health.as_ref().unwrap().quarantined, quarantine);
        let session = app.session.as_mut().unwrap();
        assert!(session.flush(Duration::from_secs(5)).is_err());
        assert!(
            session
                .operation_store()
                .load_card(card, None)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            session
                .operation_store()
                .load_card(review.recovery.alias, None)
                .unwrap()
                .unwrap()
                .kind,
            before.cards[0].kind
        );
        session.shutdown();
    }

    #[test]
    fn ownership_recovery_confirmation_saves_current_drafts_and_keeps_the_barrier_until_commit() {
        let (_directory, mut app, card) = fixture();
        let preparation = reply(app.update_session_recovery(Action::Open(card)));
        drop(app.update(preparation));
        app.chat_ui = chat_ui::State::with_drafts(HashMap::from([(
            card,
            "latest unsent work\nnot the reviewed snapshot".into(),
        )]));
        let task = app.update_session_recovery(Action::Confirm);
        assert!(app.session_recovery.pending);
        assert!(app.save_health.as_ref().unwrap().window_state_refused());
        drop(app.update_session_recovery(Action::Cancel));
        assert!(
            app.modal_is_open(),
            "an enqueued transaction cannot be cancelled as if it never happened"
        );
        let receipt = reply(task);
        assert!(matches!(
            receipt,
            Message::SessionRecovery(Action::Finished(Ok(())))
        ));
        drop(app.update(receipt));
        assert!(!app.modal_is_open());
        assert!(app.core.keyboard_nav());
        let session = app.session.as_mut().unwrap();
        session.flush(Duration::from_secs(5)).unwrap();
        let saved = session
            .operation_store()
            .load_workspace(app.durable.id, None)
            .unwrap()
            .unwrap();
        assert_eq!(
            saved.chat_drafts[&card],
            "latest unsent work\nnot the reviewed snapshot"
        );
        assert_eq!(saved.focused, Some(card));
        session.shutdown();
    }

    #[test]
    fn ownership_recovery_fences_stale_preparation_review_and_filtered_default_panes() {
        let (_directory, mut app, card) = fixture();
        let preparation = reply(app.update_session_recovery(Action::Open(card)));
        drop(app.update_session_recovery(Action::Cancel));
        drop(app.update(preparation.clone()));
        assert!(!app.modal_is_open());
        let newer = reply(app.update_session_recovery(Action::Open(card)));
        drop(app.update(preparation));
        assert!(app.session_recovery.preparing);
        drop(app.update(newer));
        let CardKind::Terminal(terminal) = &mut Arc::make_mut(&mut app.durable).cards[1].kind
        else {
            unreachable!()
        };
        terminal.resume_hint = Some("changed after review".into());
        app.session_recovery.focus = 1;
        drop(app.update_session_recovery(Action::Confirm));
        assert!(!app.session_recovery.pending);
        assert!(app.session_recovery.error.is_some());
        assert_eq!(app.session_recovery.focus, 0);
        drop(app.session_recovery_key_event(&key(keyboard::key::Named::Enter, false), false));
        assert!(!app.modal_is_open());
        let original = match &app.durable.cards[0].kind {
            CardKind::Terminal(session) => session.clone(),
            _ => unreachable!(),
        };
        Arc::make_mut(&mut app.durable).cards[1].kind = CardKind::Terminal(original.clone());
        app.shared_views.defaults.insert(card, original);
        assert!(app.refused_terminal_recovery_target().is_none());
        drop(app.update_session_recovery(Action::Open(card)));
        assert!(!app.modal_is_open());
        let session = app.session.as_mut().unwrap();
        assert!(
            session
                .operation_store()
                .load_card(card, None)
                .unwrap()
                .is_none()
        );
        assert!(session.flush(Duration::from_secs(5)).is_err());
        session.shutdown();
    }
}
