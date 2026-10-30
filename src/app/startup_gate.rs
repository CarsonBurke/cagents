//! A launched window is ready only after its saved conversations and workspace
//! have actually recovered. Opening a native surface alone cannot finish an update.
use super::*;

#[derive(Default)]
pub(super) struct State {
    opened: Option<Instant>,
    reported: bool,
    attempt: Option<Attempt>,
    qualified: Option<SaveWitness>,
    incomplete: bool,
    pending_save: Option<uuid::Uuid>,
}

struct Attempt {
    nonce: uuid::Uuid,
    deadline: Instant,
    saving: Option<SaveWitness>,
}

/// Captured only after the production persistence path has submitted its
/// payload. This owns no launch permission and does not freeze the UI.
struct SaveWitness {
    submission: crate::session::WindowSubmissionFence,
    durable: std::sync::Weak<WorkspaceState>,
    payload: Arc<WorkspaceState>,
    chat_targets: HashMap<CardId, chat_ui::Target>,
    settings_generation: u64,
    settings_path: PathBuf,
    settings: AppSettings,
    view: WindowView,
    window: Option<window::Id>,
    view_generation: u64,
    active_workspace: Option<PathBuf>,
    worktree_focus: HashMap<PathBuf, CardId>,
    order: Vec<CardId>,
    focused: Option<CardId>,
    camera_offset: f32,
}

impl SaveWitness {
    fn matches(&self, app: &Cagents) -> bool {
        app.session
            .as_ref()
            .is_some_and(|session| session.window_submission_is_current(&self.submission))
            && std::sync::Weak::ptr_eq(&self.durable, &Arc::downgrade(&app.durable))
            && self.window == app.core.main_window_id()
            && self.view_generation == app.workspace_view_generation
            && self.active_workspace == app.active_workspace
            && self.worktree_focus == app.worktree_focus
            && self.focused == app.workspace.focused()
            && self.order.iter().copied().eq(app.workspace.order())
            && self.camera_offset == app.workspace.camera().offset()
            && self.settings_generation == app.settings_save_generation
            && app
                .settings_path
                .as_ref()
                .is_none_or(|path| path == &self.settings_path)
            && self.settings == app.settings
            && self.view == app.current_window_view()
            && self.chat_targets == app.startup_chat_targets()
            && self.payload.as_ref() == app.startup_window_payload().as_ref()
    }
}

impl State {
    pub(super) fn window_opened(&self) -> bool {
        self.opened.is_some()
    }
    #[cfg(test)]
    pub(super) fn fixture_opened() -> Self {
        Self {
            opened: Some(Instant::now()),
            ..Self::default()
        }
    }
}

impl Cagents {
    fn startup_conversation_count(&self) -> usize {
        self.durable
            .cards
            .iter()
            .filter(|card| {
                card.existence.is_open()
                    && match &card.kind {
                        CardKind::Chat(session) => {
                            !session.is_unstarted()
                                && !self.retained_startup_draft(card.id, session)
                        }
                        CardKind::Terminal(session) => {
                            !terminal_ended(session) && !self.saved_terminal_ready(card.id)
                        }
                        _ => false,
                    }
            })
            .count()
    }

    /// An exact retained local copy is saved data, not a hosted pane admitted
    /// by this window. A pending marker on a shared pane alone cannot waive
    /// its original attachment/readiness requirement.
    fn retained_startup_draft(&self, card: CardId, session: &crate::chat::Session) -> bool {
        self.durable.local_only_cards.contains(&card)
            && self.durable.pending_chat_admissions.get(&card) == Some(session)
            && self.chat_awaiting_admission(card)
    }

    pub(super) fn startup_pane_check(&self) -> Result<bool, String> {
        // A native window opens independently of the observer/SeedView task.
        // Wait for that actual admission boundary before judging saved chats
        // or declaring a formerly shared workspace recovered.
        let mut pending = !self.shared_views_ready_for_startup();
        let mut failures = Vec::new();
        for card in self
            .durable
            .cards
            .iter()
            .filter(|card| card.existence.is_open())
        {
            if let CardKind::Terminal(session) = &card.kind {
                if terminal_ended(session) || self.saved_terminal_ready(card.id) {
                    continue;
                }
                match self.cards.get(&card.id) {
                    Some(CardRuntime::Terminal(surface))
                        if surface.terminal_id() == session.id
                            && session.continuity.live_proof().is_some_and(|proof| {
                                proof.is_complete() && surface.host_proof().as_ref() == Some(&proof)
                            })
                            && !surface.snapshot().detached => {}
                    Some(CardRuntime::Loading) | None => pending = true,
                    _ => failures.push(card.id),
                }
                continue;
            }
            let CardKind::Chat(session) = &card.kind else {
                continue;
            };
            if session.is_unstarted() || self.retained_startup_draft(card.id, session) {
                continue;
            }
            if self.chat_awaiting_admission(card.id) && pending {
                continue;
            }
            if self.chat_start_pending(card.id) {
                pending = true;
                continue;
            }
            match self.cards.get(&card.id) {
                Some(CardRuntime::Chat(runtime)) => match runtime.restored_pane_ready() {
                    Ok(true) => {}
                    Ok(false) => pending = true,
                    Err(_) => failures.push(card.id),
                },
                Some(CardRuntime::Loading) | None => pending = true,
                _ => failures.push(card.id),
            }
        }
        if pending {
            return Ok(false);
        }
        if !failures.is_empty() {
            return Err(format!(
                "recovery-incomplete: {} saved panes could not reconnect (panes {}). Open the affected panes and use their recovery controls. Their drafts and recovery evidence are retained; this window remains available.",
                failures.len(),
                failures
                    .iter()
                    .take(12)
                    .map(ToString::to_string)
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        Ok(true)
    }

    /// Qualification attaches retained owners without waking their offscreen
    /// rendering surface or replaying a terminal launch recipe.
    pub(super) fn qualify_startup_terminals(&mut self) -> Task<Message> {
        for card in &self.durable.cards {
            if card.existence.is_open()
                && matches!(&card.kind, CardKind::Terminal(session) if !terminal_ended(session))
                && matches!(self.cards.get(&card.id), Some(CardRuntime::Loading))
                && self.saved_terminal_current(card.id).is_none()
                && !self.restored_agent_attaches.contains(&card.id)
            {
                self.restored_agent_attaches.push_back(card.id);
            }
        }
        self.attach_next_restored_agent()
    }

    pub(super) fn begin_startup_gate(&mut self) -> Task<Message> {
        self.startup_gate.opened = Some(Instant::now());
        if !instance::ui_launch_handshake_requested() {
            self.startup_gate.reported = true;
            return Task::none();
        }
        let count = self.startup_conversation_count();
        let nonce = uuid::Uuid::new_v4();
        self.startup_gate.attempt = Some(Attempt {
            nonce,
            deadline: self.startup_deadline_at(Instant::now()),
            saving: None,
        });
        let attach = self.qualify_startup_terminals();
        let progress = cosmic::task::future(async move {
            match tokio::task::spawn_blocking(move || instance::report_ui_recovery_progress(count))
                .await
            {
                Ok(result) => Message::StartupProgressFinished(
                    nonce,
                    result.map_err(|error| error.to_string()),
                ),
                Err(error) => Message::StartupProgressFinished(nonce, Err(error.to_string())),
            }
        });
        let deadline = self.startup_watchdog(nonce);
        Task::batch([progress, attach, deadline])
    }

    fn startup_deadline_at(&self, now: Instant) -> Instant {
        now + Duration::from_secs(30 + 30 * self.startup_conversation_count() as u64)
    }

    fn startup_watchdog(&self, nonce: uuid::Uuid) -> Task<Message> {
        let deadline = self.startup_gate.attempt.as_ref().unwrap().deadline;
        cosmic::task::future(async move {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
            Message::StartupDeadline(nonce)
        })
    }

    pub(super) fn startup_progress_finished(
        &mut self,
        nonce: uuid::Uuid,
        result: Result<(), String>,
    ) -> Task<Message> {
        if !self
            .startup_gate
            .attempt
            .as_ref()
            .is_some_and(|attempt| attempt.nonce == nonce)
        {
            return Task::none();
        }
        match result {
            Ok(()) => self.verify_startup_gate(nonce),
            Err(error) => {
                // A closed stdout is not a pane failure, but it must not strand
                // the retained App in an attempt that never began verification.
                self.startup_gate.attempt = None;
                self.startup_gate.reported = true;
                self.startup_gate.incomplete = true;
                self.status = Some(TransientNotice::warning(format!(
                    "The updater could not observe recovery progress: {error}. This window remains available; retry its startup check."
                )));
                Task::none()
            }
        }
    }

    pub(super) fn startup_deadline(&mut self, nonce: uuid::Uuid) -> Task<Message> {
        if !self
            .startup_gate
            .attempt
            .as_ref()
            .is_some_and(|attempt| attempt.nonce == nonce && Instant::now() >= attempt.deadline)
        {
            return Task::none();
        }
        self.finish_startup_attempt(Err("recovery-incomplete: saved conversations are still reconnecting or changing. This window remains available; retry after saving.".into()), None)
    }

    fn start_startup_attempt(&mut self) -> Task<Message> {
        if self.startup_gate.attempt.is_some() || self.startup_gate.opened.is_none() {
            return Task::none();
        }
        let nonce = uuid::Uuid::new_v4();
        self.startup_gate.qualified = None;
        self.startup_gate.attempt = Some(Attempt {
            nonce,
            deadline: self.startup_deadline_at(Instant::now()),
            saving: None,
        });
        let attach = self.qualify_startup_terminals();
        let deadline = self.startup_watchdog(nonce);
        Task::batch([attach, deadline, self.verify_startup_gate(nonce)])
    }

    /// A query is answered by the App immediately. It can start one bounded
    /// fresh qualification, but neither activation nor transport enqueue is ready.
    pub(super) fn check_retained_startup(
        &mut self,
        request: instance::UiStartupCheckRequest,
    ) -> Task<Message> {
        use instance::startup_check::StartupDisposition;
        let current = self
            .startup_gate
            .qualified
            .as_ref()
            .is_some_and(|witness| witness.matches(self))
            && self.startup_pane_check() == Ok(true);
        let disposition = if current {
            StartupDisposition::Ready
        } else if self.startup_gate.incomplete && self.startup_gate.attempt.is_none() {
            StartupDisposition::RecoveryIncomplete
        } else {
            StartupDisposition::Pending
        };
        let _ = request
            .response
            .try_send(request.token.response_line(disposition));
        if current || self.startup_gate.opened.is_none() {
            return Task::none();
        }
        self.start_startup_attempt()
    }

    pub(super) fn verify_startup_gate(&mut self, nonce: uuid::Uuid) -> Task<Message> {
        let Some(attempt) = self
            .startup_gate
            .attempt
            .as_ref()
            .filter(|attempt| attempt.nonce == nonce && attempt.saving.is_none())
        else {
            return Task::none();
        };
        if Instant::now() >= attempt.deadline {
            return self.startup_deadline(nonce);
        }
        if self.startup_gate.pending_save.is_some() {
            return cosmic::task::future(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Message::VerifyStartupPanes(nonce)
            });
        }
        match self.startup_pane_check() {
            Err(reason) => self.finish_startup_attempt(Err(reason), None),
            Ok(false) => cosmic::task::future(async move {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Message::VerifyStartupPanes(nonce)
            }),
            Ok(true) => {
                self.persist();
                let captured = (|| {
                    let settings = self.capture_settings_save()?;
                    let session = self
                        .session
                        .as_ref()
                        .ok_or_else(|| "workspace storage unavailable".to_owned())?;
                    let (submission, flush) = session.request_window_flush()?;
                    let witness = SaveWitness {
                        submission,
                        durable: Arc::downgrade(&self.durable),
                        payload: self.startup_window_payload(),
                        chat_targets: self.startup_chat_targets(),
                        settings_generation: settings.generation,
                        settings_path: settings.path.clone(),
                        settings: settings.desired.clone(),
                        view: settings.view.clone(),
                        window: self.core.main_window_id(),
                        view_generation: self.workspace_view_generation,
                        active_workspace: self.active_workspace.clone(),
                        worktree_focus: self.worktree_focus.clone(),
                        order: self.workspace.order().collect(),
                        focused: self.workspace.focused(),
                        camera_offset: self.workspace.camera().offset(),
                    };
                    Ok::<_, String>((witness, WorkspaceSave { settings, flush }))
                })();
                let (witness, save) = match captured {
                    Ok(captured) => captured,
                    Err(error) => return self.finish_startup_attempt(Err(format!("recovery-incomplete: the recovered workspace could not be saved: {error}")), None),
                };
                self.startup_gate.attempt.as_mut().unwrap().saving = Some(witness);
                self.startup_gate.pending_save = Some(nonce);
                cosmic::task::future(async move {
                    let (settings, workspace) = flush_workspace(Ok(save)).await;
                    Message::StartupPanesSaved(nonce, settings, workspace)
                })
            }
        }
    }

    pub(super) fn startup_panes_saved(
        &mut self,
        nonce: uuid::Uuid,
        settings: Result<SettingsSaved, String>,
        saved: Result<(), String>,
    ) -> Task<Message> {
        if self.startup_gate.pending_save != Some(nonce) {
            return Task::none();
        }
        self.startup_gate.pending_save = None;
        if self
            .startup_gate
            .attempt
            .as_ref()
            .is_some_and(|attempt| attempt.nonce == nonce && Instant::now() >= attempt.deadline)
        {
            return self.startup_deadline(nonce);
        }
        let Some(attempt) = self
            .startup_gate
            .attempt
            .as_mut()
            .filter(|attempt| attempt.nonce == nonce)
        else {
            return Task::none();
        };
        let Some(witness) = attempt.saving.take() else {
            return Task::none();
        };
        let receipt_matches = settings.as_ref().is_ok_and(|saved| {
            saved.generation == witness.settings_generation
                && saved.path == witness.settings_path
                && saved.settings == witness.settings
        });
        let result = settings
            .map(|saved| self.acknowledge_settings_save(saved))
            .and(saved);
        if let Err(error) = result {
            return self.finish_startup_attempt(Err(format!("recovery-incomplete: conversations reconnected, but the recovered workspace could not be saved: {error}. This window remains available; retry saving before updating again.")), None);
        }
        if !receipt_matches || !witness.matches(self) {
            return self.verify_startup_gate(nonce);
        }
        match self.startup_pane_check() {
            Ok(true) => self.finish_startup_attempt(Ok(()), Some(witness)),
            Ok(false) => self.verify_startup_gate(nonce),
            Err(reason) => self.finish_startup_attempt(Err(reason), None),
        }
    }

    fn finish_startup_attempt(
        &mut self,
        result: Result<(), String>,
        witness: Option<SaveWitness>,
    ) -> Task<Message> {
        self.startup_gate.attempt = None;
        self.startup_gate.incomplete = result.is_err();
        self.startup_gate.qualified = if result.is_ok() { witness } else { None };
        if let Err(reason) = &result {
            self.status = Some(TransientNotice::warning(reason.clone()));
        }
        if self.startup_gate.reported {
            return Task::none();
        }
        self.startup_gate.reported = true;
        report(result)
    }
}

fn report(result: Result<(), String>) -> Task<Message> {
    cosmic::task::future(async move {
        let result = tokio::task::spawn_blocking(move || instance::report_ui_startup(&result))
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result.map_err(|error| error.to_string()));
        Message::StartupReported(result)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn app() -> (Cagents, tempfile::TempDir) {
        let directory = tempfile::tempdir().unwrap();
        let (session, _) = SessionRuntime::open(
            directory.path().join("startup.sqlite3"),
            crate::model::BootId::new("startup-save-receipt"),
        )
        .unwrap();
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.session = Some(session);
        app.settings_path = Some(directory.path().join("settings.json"));
        app.startup_gate = State::fixture_opened();
        // These checks exercise reusable qualification; they never write to a
        // process-global launch stdout or open a native/provider connection.
        app.startup_gate.reported = true;
        (app, directory)
    }

    fn next_message(task: Task<Message>) -> Message {
        let mut stream = iced::runtime::task::into_stream(task).unwrap();
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        match runtime.block_on(stream.next()).unwrap() {
            iced::runtime::Action::Output(cosmic::Action::App(message)) => message,
            _ => panic!("startup task must return its App message"),
        }
    }

    pub(super) fn completed_save(app: &mut Cagents) -> Message {
        next_message(app.start_startup_attempt())
    }

    #[test]
    fn reusable_startup_requires_real_matching_save_and_can_recover_after_failure() {
        use instance::startup_check::{StartupCheckToken, StartupDisposition};
        let (mut app, _directory) = app();
        let first = completed_save(&mut app);
        let Message::StartupPanesSaved(nonce, settings, saved) = first else {
            panic!("production App gate must await its real settings and session writer");
        };
        assert!(settings.is_ok() && saved.is_ok());
        drop(app.startup_panes_saved(nonce, settings, saved));
        assert!(app.startup_gate.qualified.is_some());
        let token = StartupCheckToken::new();
        let (response, receiver) = std::sync::mpsc::sync_channel(1);
        drop(app.check_retained_startup(instance::UiStartupCheckRequest { token, response }));
        assert_eq!(
            receiver.recv().unwrap(),
            token.response_line(StartupDisposition::Ready)
        );

        // A definite save failure retains this App, but may not leave its old
        // successful witness available to the next query.
        app.settings_write_blocked = true;
        app.startup_gate.qualified = None;
        drop(app.start_startup_attempt());
        assert!(app.startup_gate.incomplete);
        assert!(app.startup_gate.qualified.is_none());
        assert!(app.startup_gate.attempt.is_none());
        app.settings_write_blocked = false;
        let repaired = completed_save(&mut app);
        drop(app.update(repaired));
        assert!(app.startup_gate.qualified.is_some());
        assert!(!app.startup_gate.incomplete);
        assert!(
            app.startup_gate.reported,
            "initial stdout report remains one-shot"
        );
    }

    #[test]
    fn held_original_app_save_rejects_aba_then_accepts_a_fresh_matching_save() {
        let (mut app, directory) = app();
        drop(app.add_utility_card(CardKind::Settings));
        app.session
            .as_ref()
            .unwrap()
            .flush(Duration::from_secs(5))
            .unwrap();
        let original = app.durable.clone();
        let mut blocker =
            rusqlite::Connection::open(directory.path().join("startup.sqlite3")).unwrap();
        let transaction = blocker
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        // The actual original checkpoint is admitted while this fixture DB's
        // write lock prevents it from completing. B and the same immutable A
        // are then submitted to the real writer before that lock is released.
        let original_task = app.start_startup_attempt();
        let nonce = app.startup_gate.attempt.as_ref().unwrap().nonce;
        let writer = app.session.as_ref().unwrap();
        let mut changed = (*original).clone();
        changed.cards[0].title = "Intervening B".into();
        writer.snapshot(Arc::new(changed));
        writer.snapshot(original.clone());
        transaction.commit().unwrap();
        let completed = next_message(original_task);
        let Message::StartupPanesSaved(actual, settings, saved) = completed else {
            panic!("real held save");
        };
        assert_eq!(actual, nonce);
        assert!(settings.is_ok() && saved.is_ok());
        assert_eq!(app.startup_window_payload().as_ref(), original.as_ref());
        let fresh_task = app.startup_panes_saved(actual, settings, saved);
        assert!(
            app.startup_gate.qualified.is_none(),
            "equality after ABA cannot qualify the original checkpoint"
        );
        let fresh = next_message(fresh_task);
        drop(app.update(fresh));
        assert!(app.startup_gate.qualified.is_some());
    }

    #[test]
    fn newer_settings_receipt_and_stale_callbacks_cannot_finish_another_startup_attempt() {
        let (mut app, _directory) = app();
        let pending = completed_save(&mut app);
        let Message::StartupPanesSaved(nonce, settings, saved) = pending else {
            panic!("production save completion");
        };
        assert!(saved.is_ok());
        // SettingsSave legitimately returns a newer lane receipt when a later
        // capture won. That success must not certify this earlier WindowView.
        app.settings.sidebar_width += 17.0;
        let newer = app.capture_settings_save().unwrap().run().unwrap();
        assert!(newer.generation > settings.as_ref().unwrap().generation);
        let next = app.startup_panes_saved(nonce, Ok(newer), saved);
        assert!(app.startup_gate.qualified.is_none());
        assert_eq!(app.startup_gate.attempt.as_ref().unwrap().nonce, nonce);
        let fresh = next_message(next);
        let Message::StartupPanesSaved(_, settings, saved) = fresh else {
            panic!("changed payload must be saved again");
        };
        let successor = uuid::Uuid::new_v4();
        app.startup_gate.attempt.as_mut().unwrap().nonce = successor;
        drop(app.startup_panes_saved(nonce, settings, saved));
        assert!(app.startup_gate.qualified.is_none());
        assert_eq!(app.startup_gate.attempt.as_ref().unwrap().nonce, successor);
        assert!(app.startup_gate.attempt.as_ref().unwrap().saving.is_some());
    }
}

#[cfg(test)]
mod deadline_tests {
    use super::*;

    #[test]
    fn expired_save_and_failed_initial_progress_leave_reusable_recovery() {
        let directory = tempfile::tempdir().unwrap();
        let (session, _) = SessionRuntime::open(
            directory.path().join("deadline.sqlite3"),
            crate::model::BootId::new("startup-deadline"),
        )
        .unwrap();
        let mut app = super::super::tests::context_inspector_test_app(false);
        app.session = Some(session);
        app.settings_path = Some(directory.path().join("settings.json"));
        app.startup_gate = State::fixture_opened();
        app.startup_gate.reported = true;
        let first = super::tests::completed_save(&mut app);
        let Message::StartupPanesSaved(nonce, settings, saved) = first else {
            panic!("real save");
        };
        app.startup_gate.attempt.as_mut().unwrap().deadline =
            Instant::now() - Duration::from_millis(1);
        drop(app.startup_panes_saved(nonce, settings, saved));
        assert!(app.startup_gate.incomplete);
        assert!(app.startup_gate.qualified.is_none());
        assert!(app.startup_gate.pending_save.is_none());
        assert!(app.startup_gate.attempt.is_none());
        let nonce = uuid::Uuid::new_v4();
        app.startup_gate.attempt = Some(Attempt {
            nonce,
            deadline: Instant::now() + Duration::from_secs(30),
            saving: None,
        });
        drop(app.startup_progress_finished(nonce, Err("original pipe closed".into())));
        assert!(app.startup_gate.attempt.is_none());
        let recovered = super::tests::completed_save(&mut app);
        drop(app.update(recovered));
        assert!(app.startup_gate.qualified.is_some());
    }
}
