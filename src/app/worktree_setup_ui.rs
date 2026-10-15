//! Interactive, scoped setup after a journaled worktree has fully settled.
use super::*;
use crate::{model::OperationId, sidebar::WorktreeSetupStatus, worktree_setup::{SetupPlan, SetupPreparation}};

pub(super) use crate::repository::WorktreeSetupIntent as SetupIntent;

#[derive(Debug)]
pub(super) struct Review {
    pub request: OperationId,
    creation: OperationId,
    path: PathBuf,
    expected: SetupIntent,
    current: SetupIntent,
    loading: bool,
    trusted: bool,
    pub(super) approved: bool,
    error: Option<String>,
    token: CancellationToken,
}
impl Drop for Review { fn drop(&mut self) { self.token.cancel(); } }

#[derive(Default)]
pub(super) struct State {
    pending: HashMap<PathBuf, (OperationId, CancellationToken)>,
    statuses: HashMap<PathBuf, WorktreeSetupStatus>,
    terminals: HashMap<PathBuf, (u64, TerminalId)>,
    recovered: HashMap<PathBuf, (OperationId, SetupIntent)>,
    authorized_runs: std::sync::Mutex<HashSet<OperationId>>,
}

impl Drop for State {
    fn drop(&mut self) { for (_, token) in self.pending.values() { token.cancel(); } }
}

impl State {
    pub(super) fn authorized_run(&self, session: &TerminalSession) -> Option<OperationId> {
        let run = session.launch.environment.get("CAGENTS_WORKTREE_SETUP_ID")?.parse().ok()?;
        self.authorized_runs.lock().expect("setup launch authorization lock").remove(&run).then_some(run)
    }
    pub fn preparing(&self, path: &Path) -> bool { self.pending.contains_key(path) }
    pub fn protected_directories(&self) -> impl Iterator<Item = &PathBuf> { self.pending.keys() }
    pub fn active(&self) -> bool {
        !self.pending.is_empty() || self.statuses.values().any(|status| *status == WorktreeSetupStatus::Running)
    }
}

fn terminal_status(session: &TerminalSession, open: bool, starting: bool) -> WorktreeSetupStatus {
    let ended = |exit: &crate::model::TerminalExit| {
        if exit.code == Some(0) && exit.signal.is_none() { WorktreeSetupStatus::Succeeded }
        else { WorktreeSetupStatus::Failed(match (exit.code, exit.signal.as_deref()) {
            (Some(code), _) => format!("Setup exited with status {code}"),
            (_, Some(signal)) => format!("Setup ended with signal {signal}"),
            _ => "Setup ended without a successful exit status".into(),
        }) }
    };
    match &session.continuity {
        TerminalContinuity::Exited { last_exit } | TerminalContinuity::RelaunchAvailable { last_exit: Some(last_exit), .. } => ended(last_exit),
        TerminalContinuity::Failed { typed_error, .. } => WorktreeSetupStatus::Failed(typed_error.message.clone()),
        _ if !open => WorktreeSetupStatus::Failed("The setup terminal was closed".into()),
        TerminalContinuity::RelaunchAvailable { last_exit: None, .. } if !starting => WorktreeSetupStatus::Interrupted("Setup was interrupted. Review its files and terminal output before resuming.".into()),
        _ => WorktreeSetupStatus::Running,
    }
}

fn preparation(intent: SetupIntent, destination: &Path, token: &CancellationToken) -> Result<SetupPreparation, String> {
    if intent.skip { return Ok(SetupPreparation { skipped: true, ..Default::default() }); }
    // Keep automatic cleanup fenced if the user selects another scope while
    // the background copy is still preparing this new checkout.
    let _protection = crate::worktree_cleanup::protect(destination).map_err(|error| error.to_string())?;
    let primary = intent.primary.ok_or("The primary checkout is unavailable for setup")?;
    let plan = intent.plan.ok_or("The project's setup configuration is unavailable")?;
    crate::worktree_setup::prepare(&primary, destination, &plan, false, token).map_err(|error| error.to_string())
}

impl Cagents {
    pub(super) fn start_worktree_setup(&mut self, operation: OperationId, destination: PathBuf, intent: SetupIntent) -> Task<Message> {
        let run = intent.run_id.unwrap_or(operation);
        let token = CancellationToken::new();
        self.setup_ui.recovered.insert(destination.clone(), (operation, intent.clone()));
        self.setup_ui.pending.insert(destination.clone(), (run, token.clone()));
        let store = self.session.as_ref().map(SessionRuntime::operation_store);
        self.setup_ui.statuses.insert(destination.clone(), WorktreeSetupStatus::Preparing);
        cosmic::task::future(async move {
            let worktree = destination.clone();
            let result = tokio::task::spawn_blocking(move || {
                let result = preparation(intent.clone(), &destination, &token)?;
                // Copy-only completion is durable before the UI can report it.
                // Shell completion is evidenced by its exact marked terminal.
                if result.command.is_none() && !result.skipped {
                    let store = store.ok_or("The setup journal is unavailable")?;
                    let mut complete = intent.clone(); complete.copy_done = true;
                    RepositoryService::system("git").update_worktree_setup_intent(&store, operation, Some(&intent), complete)
                        .map_err(|error| error.to_string())?;
                }
                Ok(result)
            })
                .await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::WorktreeSetupPrepared { operation: run, worktree, result }
        })
    }

    pub(super) fn finish_worktree_setup(&mut self, operation: OperationId, worktree: PathBuf, result: Result<SetupPreparation, String>) -> Task<Message> {
        if !self.setup_ui.pending.get(&worktree).is_some_and(|(pending, _)| *pending == operation) { return Task::none(); }
        self.setup_ui.pending.remove(&worktree);
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                self.setup_ui.statuses.insert(worktree, WorktreeSetupStatus::Interrupted(error.clone()));
                self.status = Some(TransientNotice::warning(format!("Worktree created; {error}")));
                self.refresh_worktree_setup_rows();
                return Task::none();
            }
        };
        if prepared.skipped {
            self.setup_ui.statuses.insert(worktree, WorktreeSetupStatus::Skipped);
            self.refresh_worktree_setup_rows();
            return Task::none();
        }
        if !prepared.skipped_paths.is_empty() || !prepared.missing_patterns.is_empty() {
            self.status = Some(TransientNotice::warning(format!(
                "Worktree setup copied {} files; skipped {} existing or unsafe paths and {} missing patterns.",
                prepared.copied.len(), prepared.skipped_paths.len(), prepared.missing_patterns.len()
            )));
        }
        let Some(command) = prepared.command else {
            self.setup_ui.statuses.insert(worktree, WorktreeSetupStatus::Succeeded);
            self.refresh_worktree_setup_rows();
            return Task::none();
        };
        if self.refuse_in_deleting_worktree(&worktree) {
            self.setup_ui.statuses.insert(worktree, WorktreeSetupStatus::Failed("The worktree is being removed".into()));
            self.refresh_worktree_setup_rows();
            return Task::none();
        }
        let started_at = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis().min(u64::MAX as u128) as u64).unwrap_or_default();
        self.setup_ui.authorized_runs.lock().expect("setup launch authorization lock").insert(operation);
        let launch = LaunchRecipe {
            program: "/bin/sh".into(), arguments: vec!["-c".into(), command], working_directory: worktree.clone(),
            environment: [("CAGENTS_WORKTREE_SETUP".into(), "1".into()), ("CAGENTS_WORKTREE_SETUP_ID".into(), operation.to_string()), ("CAGENTS_WORKTREE_SETUP_STARTED_AT".into(), started_at.to_string())].into(),
            unset_environment: Default::default(),
        };
        let session = fresh_terminal_session(launch);
        let terminal = session.id;
        self.setup_ui.terminals.insert(worktree.clone(), (started_at, terminal));
        self.setup_ui.statuses.insert(worktree.clone(), WorktreeSetupStatus::Running);
        // The new worktree owns this card even if another channel was selected
        // while the bounded copy operation completed.
        let select = self.select_worktree(worktree, false);
        let start = self.insert_terminal_card(session, None);
        if let Some(card) = self.durable.cards.iter().find(|card| matches!(&card.kind, CardKind::Terminal(session) if session.id == terminal)).map(|card| card.id) {
            self.chrome_ui.user_titles.insert(card, "Worktree setup".into());
            if let Some(state) = self.card_state_mut(card) { state.title = "Worktree setup".into(); }
            let _ = self.workspace.rename_card(card, "Worktree setup");
            self.persist();
        }
        self.refresh_worktree_setup_rows();
        Task::batch([select, start])
    }

    pub(super) fn refresh_worktree_setup_rows(&mut self) {
        for card in &self.durable.cards {
            let CardKind::Terminal(session) = &card.kind else { continue; };
            if session.launch.environment.get("CAGENTS_WORKTREE_SETUP").map(String::as_str) != Some("1") { continue; }
            let path = &session.launch.working_directory;
            if self.setup_ui.pending.contains_key(path) { continue; }
            // A stale retained terminal from a previous run cannot stand in
            // for the run referenced by the durable creation intent.
            if let Some((_, intent)) = self.setup_ui.recovered.get(path)
                && let Some(run) = intent.run_id
                && session.launch.environment.get("CAGENTS_WORKTREE_SETUP_ID").is_none_or(|value| value != &run.to_string()) { continue; }
            let started_at = session.launch.environment.get("CAGENTS_WORKTREE_SETUP_STARTED_AT").and_then(|value| value.parse::<u64>().ok()).unwrap_or_default();
            if self.setup_ui.terminals.get(path).is_some_and(|(known_at, known)| *known_at > started_at || (*known_at == started_at && *known != session.id)) { continue; }
            self.setup_ui.terminals.insert(path.clone(), (started_at, session.id));
            let status = terminal_status(session, card.existence.is_open(), self.terminal_start_in_flight(card.id));
            self.setup_ui.statuses.insert(path.clone(), status);
        }
        let roots = self.sidebar_model.projects.iter().flat_map(|project| project.worktrees.iter().map(|worktree| worktree.path.clone())).collect::<HashSet<_>>();
        self.setup_ui.statuses.retain(|path, _| roots.contains(path) || self.setup_ui.pending.contains_key(path) || self.setup_ui.recovered.contains_key(path));
        self.setup_ui.terminals.retain(|path, _| roots.contains(path) || self.setup_ui.pending.contains_key(path));
        for worktree in self.sidebar_model.projects.iter_mut().flat_map(|project| &mut project.worktrees) {
            worktree.setup_status = self.setup_ui.statuses.get(&worktree.path).cloned();
        }
    }
}

impl Cagents {
    pub(super) fn setup_review_on_top(&self) -> bool {
        #[cfg(feature = "integrated-browser")]
        if self.browser_import.open() { return false; }
        self.project_ui.setup_review.is_some() && !self.update_prepared
            && self.update_ui.confirmation.is_none() && !self.session_recovery.open()
            && !self.daemon_health_ui.confirmation && !self.automation_approval_open()
            && self.transient_work_close.is_none() && !self.automation_ui.draft_dialog_open()
            && !self.device_ui.open && self.chat_ui.image_viewer.is_none()
            && self.terminal_paste.is_none() && !self.terminal_close_dialog_visible()
            && !self.settings_reset_confirm && self.sidebar_state.pending_discard().is_none()
            && self.dirty_repository_close.is_none() && self.sidebar_selection_ui.dialog.is_none()
    }
    pub(super) fn forget_worktree_setup(&mut self, path: &Path) {
        self.setup_ui.recovered.remove(path); self.setup_ui.statuses.remove(path); self.setup_ui.terminals.remove(path);
    }
    pub(super) fn recover_worktree_setup(&mut self, creation: OperationId, path: PathBuf, intent: SetupIntent) {
        let status = if intent.skip { WorktreeSetupStatus::Skipped }
        else if intent.copy_done { WorktreeSetupStatus::Succeeded }
        else { WorktreeSetupStatus::Interrupted("Creation finished before setup completion was confirmed. Review files and terminal output before resuming.".into()) };
        self.setup_ui.recovered.insert(path.clone(), (creation, intent));
        // An inventory pass may have seen an older run before the journal
        // was restored. Re-arbitrate using this durable run identity.
        self.setup_ui.terminals.remove(&path);
        self.setup_ui.statuses.insert(path, status);
        self.refresh_worktree_setup_rows();
    }

    pub(super) fn review_worktree_setup(&mut self, path: PathBuf) -> Task<Message> {
        let Some((creation, intent)) = self.setup_ui.recovered.get(&path).cloned() else {
            self.status = Some(TransientNotice::warning("This setup was created by another client. Review its stopped terminal there; it will not restart automatically."));
            return Task::none();
        };
        let request = OperationId::new();
        let token = CancellationToken::new();
        self.worktree_dialog = None;
        self.project_ui.setup_review = Some(Review { request, creation, path, expected: intent.clone(), current: intent.clone(), loading: true, trusted: false, approved: false, error: None, token: token.clone() });
        let load = cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                if token.is_cancelled() { return Err("Setup review was cancelled".to_owned()); }
                let primary = intent.primary.as_ref().ok_or("The primary checkout is unavailable")?;
                let plan = crate::worktree_setup::load(primary).map_err(|error| error.to_string())?;
                let trusted = match &plan { Some(plan) => crate::worktree_setup::is_trusted(primary, plan, &token).map_err(|error| error.to_string())?, None => true };
                let current = SetupIntent { plan, skip: false, copy_done: false, ..intent };
                Ok((current, trusted))
            }).await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::ProjectUi(project_ui::Action::SetupReviewLoaded { request, result })
        });
        Task::batch([load, iced::widget::operation::focus(widget::Id::new("dialog.setup.cancel")).map(cosmic::Action::App)])
    }

    pub(super) fn setup_review_loaded(&mut self, request: OperationId, result: Result<(SetupIntent, bool), String>) {
        let Some(review) = self.project_ui.setup_review.as_mut().filter(|review| review.request == request) else { return; };
        review.loading = false;
        match result { Ok((intent, trusted)) => { review.current = intent; review.trusted = trusted; }, Err(error) => review.error = Some(error) }
    }

    pub(super) fn confirm_setup_review(&mut self, skip: bool) -> Task<Message> {
        if !self.setup_review_on_top() { return Task::none(); }
        let Some(review) = self.project_ui.setup_review.as_mut() else { return Task::none(); };
        if review.loading || (!skip && (review.error.is_some() || (!review.trusted && !review.approved))) { return Task::none(); }
        let Some(store) = self.session.as_ref().map(SessionRuntime::operation_store) else { review.error = Some("The setup journal is unavailable".into()); return Task::none(); };
        review.loading = true;
        let request = review.request;
        let creation = review.creation;
        let path = review.path.clone();
        let expected = review.expected.clone();
        let mut intent = review.current.clone();
        intent.run_id = Some(OperationId::new()); intent.copy_done = false; intent.skip = skip;
        // No configured effects remain if the project removed its setup file.
        if intent.plan.is_none() { intent.skip = true; }
        let approve = review.approved;
        let token = review.token.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                if token.is_cancelled() { return Err("Setup review was cancelled".to_owned()); }
                if !intent.skip && let (Some(primary), Some(plan)) = (&intent.primary, &intent.plan) {
                    if approve { crate::worktree_setup::trust(primary, plan, &token).map_err(|error| error.to_string())?; }
                    // Even trusted recipes are verified against the exact current file
                    // by prepare. A changed file never inherits this approval.
                }
                RepositoryService::system("git").update_worktree_setup_intent(&store, creation, Some(&expected), intent.clone()).map_err(|error| error.to_string())?;
                Ok((creation, path, intent))
            }).await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::ProjectUi(project_ui::Action::SetupReviewReady { request, result })
        })
    }

    pub(super) fn setup_review_ready(&mut self, request: OperationId, result: Result<(OperationId, PathBuf, SetupIntent), String>) -> Task<Message> {
        if !self.project_ui.setup_review.as_ref().is_some_and(|review| review.request == request) {
            // The deliberate review may have been dismissed while its metadata
            // CAS completed. Retain the new run for another review, never launch.
            if let Ok((creation, path, intent)) = result { self.recover_worktree_setup(creation, path, intent); }
            return Task::none();
        }
        match result {
            Ok((creation, path, intent)) => {
                if !self.setup_review_on_top() {
                    // A higher-priority dialog or update freeze took over
                    // while metadata was saved. Preserve the reviewed intent
                    // but require a new deliberate resume when it is exposed.
                    self.recover_worktree_setup(creation, path, intent.clone());
                    if let Some(review) = &mut self.project_ui.setup_review {
                        review.expected = intent.clone(); review.current = intent; review.loading = false;
                    }
                    return Task::none();
                }
                self.project_ui.setup_review = None;
                self.start_worktree_setup(creation, path, intent)
            }
            Err(error) => { let review = self.project_ui.setup_review.as_mut().unwrap(); review.loading = false; review.error = Some(error); Task::none() }
        }
    }
}

pub(super) fn review_view(review: &Review) -> Element<'_, Message> {
    let mut rows: Vec<Element<'_, Message>> = vec![
        design::title("Review interrupted worktree setup").into(),
        super::notice_detail(review.path.display().to_string()),
        super::notice_body("Earlier commands may have run before the interruption. Inspect the files and terminal output before running setup again. Existing files will be preserved."),
    ];
    if review.loading { rows.push(super::notice_detail("Loading setup…")); }
    if let Some(plan) = &review.current.plan {
        for command in &plan.commands { rows.push(super::notice_detail(command.clone())); }
        if !plan.copy.is_empty() { rows.push(super::notice_detail(format!("Copy: {}", plan.copy.join(", ")))); }
        if !review.trusted && !review.loading {
            rows.push(widget::checkbox("Trust these exact project commands", review.approved)
                .on_toggle(|value| Message::ProjectUi(project_ui::Action::SetupReviewTrust(value))).into());
        }
    } else if !review.loading { rows.push(super::notice_detail("This project has no setup configuration.")); }
    if let Some(error) = &review.error { rows.push(super::notice_detail(error.clone())); }
    rows.push(crate::ui_controls::button::secondary("Skip setup")
        .on_press_maybe((!review.loading).then_some(Message::ProjectUi(project_ui::Action::ConfirmSetupReview { skip: true }))).into());
    let footer = super::dialog_ui::confirmation_actions(
        crate::ui_controls::button::secondary("Cancel").id(widget::Id::new("dialog.setup.cancel"))
            .on_press(Message::ProjectUi(project_ui::Action::Dismiss)),
        crate::ui_controls::button::primary("Resume setup")
            .on_press_maybe((!review.loading && review.error.is_none() && (review.trusted || review.approved))
                .then_some(Message::ProjectUi(project_ui::Action::ConfirmSetupReview { skip: false }))),
    );
    super::dialog_ui::surface(widget::column::with_children(vec![
        widget::container(widget::scrollable(widget::column::with_children(rows).spacing(space::M)).height(iced::Length::Shrink)).max_height(420.0).into(), footer,
    ]).spacing(space::M), 560.0)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn daemon_created_setup_terminal_markers_drive_the_worktree_row() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let project = ProjectId::new();
        let path = PathBuf::from("/repo/setup");
        app.sidebar_model.projects.push(SidebarProject::new(project, RepositoryId::new(), "repo".into(), "/repo".into(),
            vec![SidebarWorktree::from_durable(crate::model::WorktreeId::new(), path.clone(), Some("setup".into()))], Vec::new()));
        let mut session = fresh_terminal_session(LaunchRecipe::shell(path).unwrap());
        session.launch.environment.insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
        session.continuity = TerminalContinuity::Exited { last_exit: crate::model::TerminalExit { code: Some(0), signal: None, occurred_at_unix_ms: 0 } };
        Arc::make_mut(&mut app.durable).cards[0].kind = CardKind::Terminal(session);
        app.refresh_worktree_setup_rows();
        assert_eq!(app.sidebar_model.projects[0].worktrees[0].setup_status, Some(WorktreeSetupStatus::Succeeded));
        // Reusing a checkout path must not resurrect the retained old setup
        // result instead of showing the newly reserved terminal.
        let mut next = app.durable.cards[0].clone(); next.id = CardId::new();
        let mut session = fresh_terminal_session(LaunchRecipe::shell("/repo/setup".into()).unwrap());
        session.launch.environment.insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
        session.launch.environment.insert("CAGENTS_WORKTREE_SETUP_STARTED_AT".into(), "10".into());
        next.kind = CardKind::Terminal(session);
        Arc::make_mut(&mut app.durable).cards.push(next);
        app.refresh_worktree_setup_rows();
        assert!(matches!(app.sidebar_model.projects[0].worktrees[0].setup_status, Some(WorktreeSetupStatus::Interrupted(_))));
    }

    #[test]
    fn durable_run_identity_wins_over_an_old_run_with_a_later_clock_timestamp() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let path = PathBuf::from("/repo/setup");
        let current = OperationId::new();
        let mut old = fresh_terminal_session(LaunchRecipe::shell(path.clone()).unwrap());
        old.launch.environment.insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
        old.launch.environment.insert("CAGENTS_WORKTREE_SETUP_ID".into(), OperationId::new().to_string());
        old.launch.environment.insert("CAGENTS_WORKTREE_SETUP_STARTED_AT".into(), "100".into());
        old.continuity = TerminalContinuity::Exited { last_exit: crate::model::TerminalExit { code: Some(9), signal: None, occurred_at_unix_ms: 100 } };
        Arc::make_mut(&mut app.durable).cards[0].kind = CardKind::Terminal(old);
        let mut next = app.durable.cards[0].clone(); next.id = CardId::new();
        let mut session = fresh_terminal_session(LaunchRecipe::shell(path.clone()).unwrap());
        session.launch.environment.insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
        session.launch.environment.insert("CAGENTS_WORKTREE_SETUP_ID".into(), current.to_string());
        session.launch.environment.insert("CAGENTS_WORKTREE_SETUP_STARTED_AT".into(), "10".into());
        session.continuity = TerminalContinuity::Exited { last_exit: crate::model::TerminalExit { code: Some(0), signal: None, occurred_at_unix_ms: 10 } };
        let expected = session.id;
        next.kind = CardKind::Terminal(session);
        Arc::make_mut(&mut app.durable).cards.push(next);
        app.refresh_worktree_setup_rows();
        app.recover_worktree_setup(current, path.clone(), SetupIntent { run_id: Some(current), ..Default::default() });
        assert_eq!(app.setup_ui.statuses[&path], WorktreeSetupStatus::Succeeded);
        assert_eq!(app.setup_ui.terminals[&path].1, expected);
    }

    #[test]
    fn recovered_setup_is_visible_and_never_replays_commands() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let path = PathBuf::from("/repo/recovered");
        let creation = OperationId::new();
        let intent = SetupIntent { primary: Some("/repo".into()), plan: Some(SetupPlan { copy: vec![".env".into()], commands: vec!["must-not-run".into()], fingerprint: "a".repeat(64) }), run_id: Some(creation), ..Default::default() };
        app.recover_worktree_setup(creation, path.clone(), intent.clone());
        assert!(matches!(app.setup_ui.statuses[&path], WorktreeSetupStatus::Interrupted(_)));
        assert!(app.setup_ui.pending.is_empty());
        assert!(app.setup_ui.authorized_runs.lock().unwrap().is_empty());
        assert!(!app.setup_ui.active());
        let mut copied = intent.clone(); copied.copy_done = true;
        app.recover_worktree_setup(creation, path.clone(), copied);
        assert_eq!(app.setup_ui.statuses[&path], WorktreeSetupStatus::Succeeded);
        let mut skipped = intent; skipped.skip = true;
        app.recover_worktree_setup(creation, path.clone(), skipped);
        assert_eq!(app.setup_ui.statuses[&path], WorktreeSetupStatus::Skipped);
    }

    #[test]
    fn setup_review_user_actions_require_the_exposed_modal_and_no_update_freeze() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let id = OperationId::new();
        app.project_ui.setup_review = Some(Review { request: id, creation: id, path: "/repo".into(),
            expected: SetupIntent::default(), current: SetupIntent::default(), loading: false,
            trusted: true, approved: false, error: None, token: CancellationToken::new() });
        assert!(app.setup_review_on_top());
        for frozen in [false, true] {
            app.update_prepared = frozen; app.settings_reset_confirm = !frozen;
            assert!(!app.setup_review_on_top());
            drop(app.handle_project_action(project_ui::Action::SetupReviewTrust(true)));
            assert!(!app.project_ui.setup_review.as_ref().unwrap().approved);
            let task = app.confirm_setup_review(true);
            assert!(cosmic::iced::runtime::task::into_stream(task).is_none());
            assert!(!app.project_ui.setup_review.as_ref().unwrap().loading);
        }
        assert!(user_message_blocked_during_update(&Message::ProjectUi(project_ui::Action::SetupReviewTrust(true))));
        assert!(user_message_blocked_during_update(&Message::ProjectUi(project_ui::Action::ConfirmSetupReview { skip: false })));
        assert!(!user_message_blocked_during_update(&Message::ProjectUi(project_ui::Action::SetupReviewLoaded { request: id, result: Err("fixture".into()) })));
    }

    #[test]
    fn setup_launch_authorization_is_consumed_once_and_is_not_restored() {
        let state = State::default();
        let run = OperationId::new();
        let mut session = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
        session.launch.environment.insert("CAGENTS_WORKTREE_SETUP_ID".into(), run.to_string());
        assert!(state.authorized_run(&session).is_none());
        state.authorized_runs.lock().unwrap().insert(run);
        assert_eq!(state.authorized_run(&session), Some(run));
        assert!(state.authorized_run(&session).is_none());
    }

    #[test]
    fn restored_pending_setup_is_interrupted_until_explicitly_started() {
        let session = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
        assert!(matches!(terminal_status(&session, true, false), WorktreeSetupStatus::Interrupted(_)));
        assert_eq!(terminal_status(&session, true, true), WorktreeSetupStatus::Running);
    }

    #[test]
    fn skipped_setup_needs_no_filesystem_or_project_configuration() {
        let result = preparation(SetupIntent { primary: None, plan: None, skip: true, ..Default::default() }, Path::new("/missing/worktree"), &CancellationToken::new()).unwrap();
        assert!(result.skipped);
        assert!(result.command.is_none());
    }
    #[test]
    fn setup_row_reports_success_failure_running_and_explicit_close() {
        let mut session = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
        assert_eq!(terminal_status(&session, true, true), WorktreeSetupStatus::Running);
        assert!(matches!(terminal_status(&session, false, false), WorktreeSetupStatus::Failed(_)));
        session.continuity = TerminalContinuity::Exited { last_exit: crate::model::TerminalExit { code: Some(0), signal: None, occurred_at_unix_ms: 0 } };
        assert_eq!(terminal_status(&session, true, true), WorktreeSetupStatus::Succeeded);
        session.continuity = TerminalContinuity::Exited { last_exit: crate::model::TerminalExit { code: Some(7), signal: None, occurred_at_unix_ms: 0 } };
        assert_eq!(terminal_status(&session, true, true), WorktreeSetupStatus::Failed("Setup exited with status 7".into()));
    }
}
