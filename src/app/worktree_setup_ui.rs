//! Interactive, scoped setup after a journaled worktree has fully settled.
use super::*;
use crate::{
    model::OperationId,
    sidebar::WorktreeSetupStatus,
    worktree_setup::{SetupPlan, SetupPreparation},
};

pub(super) use crate::repository::WorktreeSetupIntent as SetupIntent;

static INVENTORY_PERMITS: std::sync::LazyLock<tokio::sync::Semaphore> =
    std::sync::LazyLock::new(|| tokio::sync::Semaphore::new(2));

#[derive(Debug)]
pub(super) struct Review {
    pub request: OperationId,
    creation: OperationId,
    path: PathBuf,
    expected: SetupIntent,
    current: SetupIntent,
    remote: Option<PathBuf>,
    active: bool,
    loading: bool,
    trusted: bool,
    pub(super) approved: bool,
    error: Option<String>,
    token: CancellationToken,
}
#[derive(Debug, Clone)]
pub(super) struct ReviewSnapshot {
    creation: OperationId,
    expected: SetupIntent,
    current: SetupIntent,
    trusted: bool,
    active: bool,
    error: Option<String>,
}
#[derive(Debug, Clone)]
pub(super) enum ReviewCompletion {
    Local {
        creation: OperationId,
        path: PathBuf,
        intent: SetupIntent,
    },
    Remote {
        path: PathBuf,
        run_id: OperationId,
        setup: Option<crate::worktree_control::Setup>,
        warnings: Vec<String>,
        skipped: bool,
    },
}

impl Drop for Review {
    fn drop(&mut self) {
        self.token.cancel();
    }
}

#[derive(Default)]
pub(super) struct State {
    pending: HashMap<PathBuf, (OperationId, CancellationToken)>,
    statuses: HashMap<PathBuf, WorktreeSetupStatus>,
    terminals: HashMap<PathBuf, (u64, TerminalId)>,
    recovered: HashMap<PathBuf, (OperationId, SetupIntent)>,
    authorized_runs: std::sync::Mutex<HashSet<OperationId>>,
    expected_runs: HashMap<PathBuf, OperationId>,
    review_requests: HashMap<PathBuf, OperationId>,
    receipt_requests: HashMap<ProjectId, (OperationId, PathBuf, Instant)>,
    receipt_paths: HashMap<ProjectId, Vec<PathBuf>>,
    receipt_tokens: HashMap<ProjectId, CancellationToken>,
    daemon_receipts: HashMap<PathBuf, (ProjectId, OperationId)>,
    uncertain_receipts: HashSet<PathBuf>,
}

impl Drop for State {
    fn drop(&mut self) {
        for (_, token) in self.pending.values() {
            token.cancel();
        }
        for token in self.receipt_tokens.values() {
            token.cancel();
        }
    }
}

impl State {
    fn has_authorized_run(&self, session: &TerminalSession) -> bool {
        session
            .launch
            .environment
            .get("CAGENTS_WORKTREE_SETUP_ID")
            .and_then(|value| value.parse::<OperationId>().ok())
            .is_some_and(|run| {
                self.authorized_runs
                    .lock()
                    .expect("setup launch authorization lock")
                    .contains(&run)
            })
    }
    pub(super) fn authorized_run(&self, session: &TerminalSession) -> Option<OperationId> {
        let run = session
            .launch
            .environment
            .get("CAGENTS_WORKTREE_SETUP_ID")?
            .parse()
            .ok()?;
        self.authorized_runs
            .lock()
            .expect("setup launch authorization lock")
            .remove(&run)
            .then_some(run)
    }
    pub(super) fn run_snapshot(&self) -> HashMap<PathBuf, OperationId> {
        self.expected_runs.clone()
    }
    pub(super) fn current_run(&self, path: &Path) -> Option<OperationId> {
        self.expected_runs.get(path).copied()
    }
    #[cfg(test)]
    pub(super) fn recovered_creation(&self, path: &Path) -> Option<OperationId> {
        self.recovered.get(path).map(|(creation, _)| *creation)
    }
    pub fn preparing(&self, path: &Path) -> bool {
        self.pending.contains_key(path)
    }
    pub fn protected_directories(&self) -> impl Iterator<Item = &PathBuf> {
        self.pending.keys()
    }
    pub fn active(&self) -> bool {
        !self.pending.is_empty()
            || self
                .statuses
                .values()
                .any(|status| *status == WorktreeSetupStatus::Running)
    }
}

fn terminal_status(session: &TerminalSession, open: bool, starting: bool) -> WorktreeSetupStatus {
    let ended = |exit: &crate::model::TerminalExit| {
        if exit.code == Some(0) && exit.signal.is_none() {
            WorktreeSetupStatus::Succeeded
        } else {
            WorktreeSetupStatus::Failed(match (exit.code, exit.signal.as_deref()) {
                (Some(code), _) => format!("Setup exited with status {code}"),
                (_, Some(signal)) => format!("Setup ended with signal {signal}"),
                _ => "Setup ended without a successful exit status".into(),
            })
        }
    };
    match &session.continuity {
        TerminalContinuity::Exited { last_exit }
        | TerminalContinuity::RelaunchAvailable {
            last_exit: Some(last_exit),
            ..
        } => ended(last_exit),
        TerminalContinuity::Failed { typed_error, .. } => {
            WorktreeSetupStatus::Failed(typed_error.message.clone())
        }
        _ if !open => WorktreeSetupStatus::Failed("The setup terminal was closed".into()),
        TerminalContinuity::RelaunchAvailable {
            last_exit: None, ..
        } if !starting => WorktreeSetupStatus::Interrupted(
            "Setup was interrupted. Review its files and terminal output before resuming.".into(),
        ),
        _ => WorktreeSetupStatus::Running,
    }
}

/// Runs on the terminal start worker, immediately before asking the daemon to
/// start a deliberately reviewed setup. The daemon repeats this at child start.
pub(super) fn verify_setup_launch(launch: &LaunchRecipe) -> Result<(), String> {
    let creation = launch
        .environment
        .get("CAGENTS_WORKTREE_SETUP_CREATION_ID")
        .ok_or("The setup launch has no recorded creation generation")?
        .parse::<OperationId>()
        .map_err(|error| error.to_string())?;
    let service = RepositoryService::system("git");
    let token = CancellationToken::new();
    let admin = service
        .git_dir(&launch.working_directory, &token)
        .map_err(|error| error.to_string())?;
    if !crate::repository::worktree_setup_marker_matches(&admin, creation)
        .map_err(|error| error.to_string())?
    {
        return Err("The setup destination was replaced after preparation; review its current checkout explicitly".into());
    }
    Ok(())
}

fn preparation(
    intent: SetupIntent,
    destination: &Path,
    creation: OperationId,
    token: &CancellationToken,
) -> Result<SetupPreparation, String> {
    if intent.skip {
        return Ok(SetupPreparation {
            skipped: true,
            ..Default::default()
        });
    }
    // Keep automatic cleanup fenced if the user selects another scope while
    // the background copy is still preparing this new checkout.
    let _protection =
        crate::worktree_cleanup::protect(destination).map_err(|error| error.to_string())?;
    let primary = intent
        .primary
        .ok_or("The primary checkout is unavailable for setup")?;
    let plan = intent
        .plan
        .ok_or("The project's setup configuration is unavailable")?;
    crate::worktree_setup::prepare_for_creation(
        &primary,
        destination,
        &plan,
        false,
        creation,
        token,
    )
    .map_err(|error| error.to_string())
}


/// Hold the original creation lease from the last intent read through every
/// copy and its progress update. A superseding Resume or Skip must win before
/// preparation starts or wait until all of these effects have settled.
fn prepare_recorded_setup(
    store: Option<&crate::persistence::StateStore>,
    intent: SetupIntent,
    destination: &Path,
    creation: OperationId,
    token: &CancellationToken,
) -> Result<SetupPreparation, String> {
    if intent.skip {
        return preparation(intent, destination, creation, token);
    }
    let store = store.ok_or("The setup journal is unavailable")?;
    let service = RepositoryService::system("git");
    let guard = service.worktree_setup_guard(store, creation, Some(&intent))
        .map_err(|error| error.to_string())?;
    let result = preparation(intent.clone(), destination, creation, token)?;
    let mut complete = intent.clone();
    complete.copy_done = true;
    service.update_worktree_setup_intent_with_guard(
        store, creation, Some(&intent), complete, &guard,
    ).map_err(|error| error.to_string())?;
    // The host obtains the same lease for its independently verified launch.
    // Never retain it while waiting for the StartWorktreeSetup RPC.
    drop(guard);
    Ok(result)
}

impl Cagents {
    pub(super) fn start_worktree_setup(
        &mut self,
        operation: OperationId,
        destination: PathBuf,
        intent: SetupIntent,
    ) -> Task<Message> {
        let run = intent.run_id.unwrap_or(operation);
        self.setup_ui.expected_runs.insert(destination.clone(), run);
        let token = CancellationToken::new();
        self.setup_ui
            .recovered
            .insert(destination.clone(), (operation, intent.clone()));
        self.setup_ui
            .pending
            .insert(destination.clone(), (run, token.clone()));
        let store = self.session.as_ref().map(SessionRuntime::operation_store);
        self.setup_ui
            .statuses
            .insert(destination.clone(), WorktreeSetupStatus::Preparing);
        cosmic::task::future(async move {
            let worktree = destination.clone();
            let result = tokio::task::spawn_blocking(move || {
                prepare_recorded_setup(store.as_deref(), intent, &destination, operation, &token)
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::WorktreeSetupPrepared {
                operation: run,
                worktree,
                result,
            }
        })
    }

    pub(super) fn finish_worktree_setup(
        &mut self,
        operation: OperationId,
        worktree: PathBuf,
        result: Result<SetupPreparation, String>,
    ) -> Task<Message> {
        if !self
            .setup_ui
            .pending
            .get(&worktree)
            .is_some_and(|(pending, _)| *pending == operation)
        {
            return Task::none();
        }
        self.setup_ui.pending.remove(&worktree);
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                // A delayed same-run terminal must not overwrite failure of
                // the fresh generation/intent check with its old exit status.
                self.setup_ui.uncertain_receipts.insert(worktree.clone());
                self.setup_ui
                    .statuses
                    .insert(worktree, WorktreeSetupStatus::Interrupted(error.clone()));
                self.status = Some(TransientNotice::warning(format!(
                    "Worktree created; {error}"
                )));
                self.refresh_worktree_setup_rows();
                return Task::none();
            }
        };
        self.setup_ui.uncertain_receipts.remove(&worktree);
        if prepared.skipped {
            self.setup_ui
                .statuses
                .insert(worktree, WorktreeSetupStatus::Skipped);
            self.refresh_worktree_setup_rows();
            return Task::none();
        }
        if !prepared.skipped_paths.is_empty() || !prepared.missing_patterns.is_empty() {
            self.status = Some(TransientNotice::warning(format!(
                "Worktree setup copied {} files; skipped {} existing or unsafe paths and {} missing patterns.",
                prepared.copied.len(),
                prepared.skipped_paths.len(),
                prepared.missing_patterns.len()
            )));
        }
        let Some(command) = prepared.command else {
            self.setup_ui
                .statuses
                .insert(worktree, WorktreeSetupStatus::Succeeded);
            self.refresh_worktree_setup_rows();
            return Task::none();
        };
        if self.refuse_in_deleting_worktree(&worktree) {
            self.setup_ui.statuses.insert(
                worktree,
                WorktreeSetupStatus::Failed("The worktree is being removed".into()),
            );
            self.refresh_worktree_setup_rows();
            return Task::none();
        }
        let started_at = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|elapsed| elapsed.as_millis().min(u64::MAX as u128) as u64)
            .unwrap_or_default();
        // Keep the existing window lease through guarded scope selection and
        // pane publication. The selection lifecycle callback retires this entry.
        self.setup_ui
            .pending
            .insert(worktree.clone(), (operation, CancellationToken::new()));
        self.setup_ui
            .authorized_runs
            .lock()
            .expect("setup launch authorization lock")
            .insert(operation);
        let Some(creation) = self
            .setup_ui
            .recovered
            .get(&worktree)
            .map(|(creation, _)| *creation)
        else {
            return self.worktree_setup_selection_finished(operation, worktree, false);
        };
        let launch = LaunchRecipe {
            program: "/bin/sh".into(),
            arguments: vec!["-c".into(), command],
            working_directory: worktree.clone(),
            environment: [
                ("CAGENTS_WORKTREE_SETUP".into(), "1".into()),
                ("CAGENTS_WORKTREE_SETUP_ID".into(), operation.to_string()),
                (
                    "CAGENTS_WORKTREE_SETUP_CREATION_ID".into(),
                    creation.to_string(),
                ),
                (
                    "CAGENTS_WORKTREE_SETUP_STARTED_AT".into(),
                    started_at.to_string(),
                ),
            ]
            .into(),
            unset_environment: Default::default(),
        };
        let session = fresh_terminal_session(launch);
        let terminal = session.id;
        self.setup_ui
            .terminals
            .insert(worktree.clone(), (started_at, terminal));
        self.setup_ui
            .statuses
            .insert(worktree.clone(), WorktreeSetupStatus::Running);
        // The new worktree owns this card even if another channel was selected
        // while the bounded copy operation completed.
        let select = self.select_worktree(worktree.clone(), false);
        let start = self.insert_terminal_card(session, None);
        let _ = self.worktree_setup_selection_finished(operation, worktree, true);
        self.refresh_worktree_setup_rows();
        Task::batch([select, start])
    }

    pub(super) fn worktree_setup_selection_finished(
        &mut self,
        operation: OperationId,
        worktree: PathBuf,
        committed: bool,
    ) -> Task<Message> {
        if !self
            .setup_ui
            .pending
            .get(&worktree)
            .is_some_and(|(run, _)| *run == operation)
        {
            return Task::none();
        }
        self.setup_ui.pending.remove(&worktree);
        if !committed {
            self.setup_ui
                .authorized_runs
                .lock()
                .expect("setup launch authorization lock")
                .remove(&operation);
            self.setup_ui.statuses.insert(worktree, WorktreeSetupStatus::Interrupted("Setup was not opened because its destination selection changed or could not be protected. Review setup to resume.".into()));
        }
        self.refresh_worktree_setup_rows();
        Task::none()
    }

    pub(super) fn refresh_worktree_setup_rows(&mut self) {
        for card in &self.durable.cards {
            let CardKind::Terminal(session) = &card.kind else {
                continue;
            };
            if session
                .launch
                .environment
                .get("CAGENTS_WORKTREE_SETUP")
                .map(String::as_str)
                != Some("1")
            {
                continue;
            }
            let path = &session.launch.working_directory;
            if self.setup_ui.pending.contains_key(path)
                || self.setup_ui.uncertain_receipts.contains(path)
            {
                continue;
            }
            // A saved terminal proves its own history, not that the current
            // checkout inherited the same creation. Wait for a verified local
            // journal or daemon receipt even on this window's first inventory.
            let Some(run) = self.setup_ui.expected_runs.get(path) else {
                self.setup_ui.statuses.insert(path.clone(), WorktreeSetupStatus::Interrupted(
                    "The saved setup terminal has no verified creation receipt for this checkout. Review setup before continuing.".into(),
                ));
                continue;
            };
            // A stale retained terminal from a previous run cannot stand in
            // for the run referenced by the durable creation intent.
            if session.launch.environment.get("CAGENTS_WORKTREE_SETUP_ID")
                .is_none_or(|value| value != &run.to_string())
            {
                continue;
            }
            let started_at = session
                .launch
                .environment
                .get("CAGENTS_WORKTREE_SETUP_STARTED_AT")
                .and_then(|value| value.parse::<u64>().ok())
                .unwrap_or_default();
            if self
                .setup_ui
                .terminals
                .get(path)
                .is_some_and(|(known_at, known)| {
                    *known_at > started_at || (*known_at == started_at && *known != session.id)
                })
            {
                continue;
            }
            self.setup_ui
                .terminals
                .insert(path.clone(), (started_at, session.id));
            // A new deliberate start first waits for its durable pane
            // reservation. Restored pending panes have no in-memory permit.
            let starting = self.terminal_start_in_flight(card.id)
                || (matches!(self.cards.get(&card.id), Some(CardRuntime::Loading))
                    && self.setup_ui.has_authorized_run(session));
            let status = if !starting
                && session.continuity.live_proof().is_some()
                && matches!(self.cards.get(&card.id), Some(CardRuntime::Failed(_)))
            {
                WorktreeSetupStatus::Interrupted("Setup terminal status is unavailable. Review its existing session before resuming.".into())
            } else {
                terminal_status(session, card.existence.is_open(), starting)
            };
            self.setup_ui.statuses.insert(path.clone(), status);
        }
        let roots = self
            .sidebar_model
            .projects
            .iter()
            .flat_map(|project| {
                project
                    .worktrees
                    .iter()
                    .map(|worktree| worktree.path.clone())
            })
            .collect::<HashSet<_>>();
        self.setup_ui.statuses.retain(|path, _| {
            roots.contains(path)
                || self.setup_ui.pending.contains_key(path)
                || self.setup_ui.recovered.contains_key(path)
        });
        self.setup_ui
            .terminals
            .retain(|path, _| roots.contains(path) || self.setup_ui.pending.contains_key(path));
        for worktree in self
            .sidebar_model
            .projects
            .iter_mut()
            .flat_map(|project| &mut project.worktrees)
        {
            worktree.setup_status = self.setup_ui.statuses.get(&worktree.path).cloned();
        }
    }
}

impl Cagents {
    pub(super) fn setup_review_on_top(&self) -> bool {
        #[cfg(feature = "integrated-browser")]
        if self.browser_import.open() {
            return false;
        }
        self.project_ui.setup_review.is_some()
            && !self.update_prepared
            && self.update_ui.confirmation.is_none()
            && !self.session_recovery.open()
            && !self.daemon_health_ui.confirmation
            && !self.automation_approval_open()
            && self.transient_work_close.is_none()
            && !self.automation_ui.draft_dialog_open()
            && !self.device_ui.open
            && self.chat_ui.image_viewer.is_none()
            && self.terminal_paste.is_none()
            && !self.terminal_close_dialog_visible()
            && !self.settings_reset_confirm
            && self.sidebar_state.pending_discard().is_none()
            && self.dirty_repository_close.is_none()
            && self.sidebar_selection_ui.dialog.is_none()
    }
    pub(super) fn forget_worktree_setup(&mut self, path: &Path) {
        self.setup_ui.recovered.remove(path);
        self.setup_ui.statuses.remove(path);
        self.setup_ui.terminals.remove(path);
        self.setup_ui.expected_runs.remove(path);
        self.setup_ui.review_requests.remove(path);
        self.setup_ui.daemon_receipts.remove(path);
        self.setup_ui.uncertain_receipts.remove(path);
    }
    pub(super) fn mark_unverified_worktree_setup(&mut self, path: PathBuf) {
        if self.setup_ui.pending.contains_key(&path) {
            return;
        }
        self.setup_ui.uncertain_receipts.insert(path.clone());
        self.setup_ui.statuses.insert(path, WorktreeSetupStatus::Interrupted("The saved setup receipt does not verify this checkout generation. Inspect its current files and configuration before running setup.".into()));
    }

    pub(super) fn recover_worktree_setup(
        &mut self,
        creation: OperationId,
        path: PathBuf,
        intent: SetupIntent,
    ) {
        self.setup_ui.uncertain_receipts.remove(&path);
        let status = restored_setup_status(&intent);
        if let Some(run) = intent.run_id {
            self.setup_ui.expected_runs.insert(path.clone(), run);
        }
        self.setup_ui
            .recovered
            .insert(path.clone(), (creation, intent));
        // An inventory pass may have seen an older run before the journal
        // was restored. Re-arbitrate using this durable run identity.
        self.setup_ui.terminals.remove(&path);
        self.setup_ui.statuses.insert(path, status);
        self.refresh_worktree_setup_rows();
    }

    pub(super) fn review_worktree_setup(&mut self, path: PathBuf) -> Task<Message> {
        if self.modal_is_open()
            || self.update_prepared
            || self.setup_ui.preparing(&path)
            || matches!(
                self.setup_ui.statuses.get(&path),
                Some(WorktreeSetupStatus::Running | WorktreeSetupStatus::Preparing)
            )
        {
            return Task::none();
        }
        let local = self.setup_ui.recovered.get(&path).cloned();
        let repository = self
            .sidebar_model
            .projects
            .iter()
            .find(|project| project.worktrees.iter().any(|row| row.path == path))
            .map(|project| project.repository.clone());
        let remote = if local.is_none() { repository } else { None };
        if local.is_none() && remote.is_none() {
            self.status = Some(TransientNotice::warning(
                "The worktree is no longer registered in this project.",
            ));
            return Task::none();
        }
        let (creation, intent) =
            local.unwrap_or_else(|| (OperationId::new(), SetupIntent::default()));
        let request = OperationId::new();
        self.invalidate_setup_inventory_for_path(&path);
        let token = CancellationToken::new();
        self.setup_ui.review_requests.insert(path.clone(), request);
        let store = self.session.as_ref().map(SessionRuntime::operation_store);
        self.worktree_dialog = None;
        self.project_ui.show_setup_review(Review {
            request,
            creation,
            path: path.clone(),
            expected: intent.clone(),
            current: intent.clone(),
            remote: remote.clone(),
            active: false,
            loading: true,
            trusted: false,
            approved: false,
            error: None,
            token: token.clone(),
        });
        let load = cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                if token.is_cancelled() {
                    return Err("Setup review was cancelled".to_owned());
                }
                if let Some(repository) = remote {
                    return daemon_setup_review(&repository, &path);
                }
                let store = store.ok_or("The setup journal is unavailable")?;
                let report = RepositoryService::system("git")
                    .worktree_setup_creation(&store, creation)
                    .map_err(|error| error.to_string())?
                    .ok_or("The setup creation is no longer recorded")?;
                let intent = report
                    .request
                    .and_then(|request| request.setup_intent)
                    .ok_or("The saved setup intent is unavailable")?;
                let primary = intent
                    .primary
                    .as_ref()
                    .ok_or("The primary checkout is unavailable")?;
                let plan =
                    crate::worktree_setup::load(primary).map_err(|error| error.to_string())?;
                let trusted = match &plan {
                    Some(plan) => crate::worktree_setup::is_trusted(primary, plan, &token)
                        .map_err(|error| error.to_string())?,
                    None => true,
                };
                let current = SetupIntent {
                    plan,
                    skip: false,
                    copy_done: false,
                    ..intent.clone()
                };
                Ok(ReviewSnapshot {
                    creation,
                    expected: intent,
                    current,
                    trusted,
                    active: false,
                    error: None,
                })
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::ProjectUi(project_ui::Action::SetupReviewLoaded { request, result })
        });
        Task::batch([
            load,
            iced::widget::operation::focus(widget::Id::new("dialog.setup.cancel"))
                .map(cosmic::Action::App),
        ])
    }

    pub(super) fn setup_review_loaded(
        &mut self,
        request: OperationId,
        result: Result<ReviewSnapshot, String>,
    ) {
        let Some(review) = self
            .project_ui
            .setup_review
            .as_mut()
            .filter(|review| review.request == request)
        else {
            return;
        };
        review.loading = false;
        match result {
            Ok(snapshot) => {
                self.setup_ui.uncertain_receipts.remove(&review.path);
                review.creation = snapshot.creation;
                review.expected = snapshot.expected;
                review.current = snapshot.current;
                review.trusted = snapshot.trusted;
                review.active = snapshot.active;
                review.error = snapshot.error;
                if let Some(run) = review.expected.run_id {
                    self.setup_ui.expected_runs.insert(review.path.clone(), run);
                }
            }
            Err(error) => review.error = Some(error),
        }
    }

    pub(super) fn confirm_setup_review(
        &mut self,
        request: OperationId,
        skip: bool,
    ) -> Task<Message> {
        if !self.setup_review_on_top() {
            return Task::none();
        }
        let Some(review) = self.project_ui.setup_review.as_mut() else {
            return Task::none();
        };
        if review.request != request
            || review.loading
            || review.active
            || (!skip && (review.error.is_some() || (!review.trusted && !review.approved)))
        {
            return Task::none();
        }
        let store = self.session.as_ref().map(SessionRuntime::operation_store);
        if review.remote.is_none() && store.is_none() {
            review.error = Some("The setup journal is unavailable".into());
            return Task::none();
        }
        review.loading = true;
        let request = review.request;
        let creation = review.creation;
        let path = review.path.clone();
        let expected = review.expected.clone();
        let mut intent = review.current.clone();
        intent.run_id = Some(OperationId::new());
        intent.copy_done = false;
        intent.skip = skip;
        if intent.plan.is_none() {
            intent.skip = true;
        }
        let approve = review.approved;
        let token = review.token.clone();
        let remote = review.remote.clone();
        cosmic::task::future(async move {
            let result = tokio::task::spawn_blocking(move || {
                if token.is_cancelled() {
                    return Err("Setup review was cancelled".to_owned());
                }
                if let Some(repository) = remote {
                    let fingerprint = intent.plan.as_ref().map(|plan| plan.fingerprint.clone());
                    let reply =
                        setup_control_request(crate::worktree_control::Operation::ResumeSetup {
                            repository,
                            path: path.clone(),
                            expected_run_id: expected.run_id,
                            expected_plan_fingerprint: fingerprint,
                            trust: approve,
                            skip: intent.skip,
                        })?;
                    let crate::worktree_control::Reply::SetupPrepared {
                        path: returned,
                        setup,
                        warnings,
                        run_id,
                    } = reply
                    else {
                        return Err("Unexpected worktree setup recovery response".into());
                    };
                    if returned != path {
                        return Err("The daemon resumed a different worktree setup".into());
                    }
                    validate_resumed_setup(&path, expected.run_id, run_id, setup.as_ref())?;
                    return Ok(ReviewCompletion::Remote {
                        path,
                        run_id,
                        setup,
                        warnings,
                        skipped: intent.skip,
                    });
                }
                let store = store.ok_or("The setup journal is unavailable")?;
                verify_local_setup_resume(&store, creation, &path, &expected, &token)?;
                if !intent.skip
                    && let (Some(primary), Some(plan)) = (&intent.primary, &intent.plan)
                {
                    if approve {
                        crate::worktree_setup::trust(primary, plan, &token)
                            .map_err(|error| error.to_string())?;
                    }
                }
                RepositoryService::system("git")
                    .update_worktree_setup_intent_guarded(&store, creation, Some(&expected), intent.clone())
                    .map_err(|error| error.to_string())?;
                Ok(ReviewCompletion::Local {
                    creation,
                    path,
                    intent,
                })
            })
            .await
            .map_err(|error| error.to_string())
            .and_then(|result| result);
            Message::ProjectUi(project_ui::Action::SetupReviewReady { request, result })
        })
    }

    pub(super) fn setup_review_ready(
        &mut self,
        request: OperationId,
        result: Result<ReviewCompletion, String>,
    ) -> Task<Message> {
        if let Ok(completion) = &result {
            let path = match completion {
                ReviewCompletion::Local { path, .. } | ReviewCompletion::Remote { path, .. } => {
                    path
                }
            };
            if self.setup_ui.review_requests.get(path) != Some(&request) {
                return Task::none();
            }
            self.invalidate_setup_inventory_for_path(path);
        }
        if !self
            .project_ui
            .setup_review
            .as_ref()
            .is_some_and(|review| review.request == request)
        {
            if let Ok(completion) = result {
                self.retain_setup_completion(completion);
            }
            return Task::none();
        }
        match result {
            Ok(ReviewCompletion::Local {
                creation,
                path,
                intent,
            }) => {
                if !self.setup_review_on_top() {
                    self.recover_worktree_setup(creation, path, intent.clone());
                    if let Some(review) = &mut self.project_ui.setup_review {
                        review.expected = intent.clone();
                        review.current = intent;
                        review.loading = false;
                    }
                    return Task::none();
                }
                self.project_ui.setup_review = None;
                self.start_worktree_setup(creation, path, intent)
            }
            Ok(remote @ ReviewCompletion::Remote { .. }) => {
                // The host already admitted this deliberate resume and owns
                // its terminal. Shared-model observation installs its pane;
                // inserting a second local terminal would duplicate effects.
                self.project_ui.setup_review = None;
                self.retain_setup_completion(remote);
                Task::none()
            }
            Err(error) => {
                let review = self.project_ui.setup_review.as_mut().unwrap();
                review.loading = false;
                review.error = Some(error);
                Task::none()
            }
        }
    }

    fn retain_setup_completion(&mut self, completion: ReviewCompletion) {
        match completion {
            ReviewCompletion::Local {
                creation,
                path,
                intent,
            } => self.recover_worktree_setup(creation, path, intent),
            ReviewCompletion::Remote {
                path,
                run_id,
                setup,
                warnings,
                skipped,
            } => {
                self.setup_ui.uncertain_receipts.remove(&path);
                self.setup_ui.expected_runs.insert(path.clone(), run_id);
                self.setup_ui.terminals.remove(&path);
                let status = if skipped {
                    WorktreeSetupStatus::Skipped
                } else if let Some(setup) = setup {
                    if let Some(run) = setup.run_id {
                        self.setup_ui.expected_runs.insert(path.clone(), run);
                    }
                    match (setup.error, setup.terminal) {
                        (Some(error), _) => WorktreeSetupStatus::Interrupted(error),
                        (_, Some(_)) => WorktreeSetupStatus::Running,
                        _ => WorktreeSetupStatus::Succeeded,
                    }
                } else {
                    WorktreeSetupStatus::Succeeded
                };
                self.setup_ui.statuses.insert(path, status);
                if !warnings.is_empty() {
                    self.status = Some(TransientNotice::warning(warnings.join("; ")));
                }
                self.refresh_worktree_setup_rows();
            }
        }
    }
}

fn setup_control_request(
    operation: crate::worktree_control::Operation,
) -> Result<crate::worktree_control::Reply, String> {
    match crate::device_service::request(crate::device_service::Operation::Worktree { operation })?
    {
        crate::device_service::Reply::Worktree(reply) => Ok(reply),
        _ => Err("Unexpected selected-device worktree response".into()),
    }
}

fn validate_resumed_setup(
    path: &Path,
    previous: Option<OperationId>,
    run: OperationId,
    setup: Option<&crate::worktree_control::Setup>,
) -> Result<(), String> {
    if previous == Some(run) {
        return Err("The daemon returned a stale setup run receipt".into());
    }
    if let Some(setup) = setup {
        if setup.run_id != Some(run) {
            return Err("The setup result has a different run identity".into());
        }
        if let Some(terminal) = &setup.terminal {
            let marked = terminal
                .launch
                .environment
                .get("CAGENTS_WORKTREE_SETUP")
                .map(String::as_str)
                == Some("1")
                && terminal
                    .launch
                    .environment
                    .get("CAGENTS_WORKTREE_SETUP_ID")
                    .is_some_and(|id| id == &run.to_string());
            if terminal.launch.working_directory != path || !marked {
                return Err(
                    "The setup terminal does not match its worktree and run receipt".into(),
                );
            }
        }
    }
    Ok(())
}

fn daemon_setup_review(repository: &Path, path: &Path) -> Result<ReviewSnapshot, String> {
    let reply = setup_control_request(crate::worktree_control::Operation::SetupIntent {
        repository: repository.to_owned(),
        path: path.to_owned(),
    })?;
    decode_daemon_setup_review(reply)
}

fn decode_daemon_setup_review(
    reply: crate::worktree_control::Reply,
) -> Result<ReviewSnapshot, String> {
    let crate::worktree_control::Reply::SetupIntent {
        creation_id,
        primary,
        intent,
        current_plan,
        plan_error,
        trusted,
        active,
    } = reply
    else {
        return Err("Unexpected worktree setup intent response".into());
    };
    let expected: SetupIntent = serde_json::from_str(&intent)
        .map_err(|error| format!("Invalid setup intent from the daemon: {error}"))?;
    let plan = current_plan
        .map(|plan| {
            serde_json::from_str::<SetupPlan>(&plan)
                .map_err(|error| format!("Invalid setup configuration from the daemon: {error}"))
        })
        .transpose()?;
    let current = SetupIntent {
        primary: Some(primary),
        plan,
        skip: false,
        copy_done: false,
        ..expected.clone()
    };
    Ok(ReviewSnapshot {
        creation: creation_id,
        expected,
        current,
        trusted,
        active,
        error: plan_error.or_else(|| {
            active.then(|| {
                "Setup is still active. Reconnect to its terminal before attempting recovery."
                    .into()
            })
        }),
    })
}

/// A stale saved creation is not authority to set up a different checkout at
/// the same path, or to duplicate a still-hosted copy of its previous recipe.
fn verify_local_setup_resume(
    store: &crate::persistence::StateStore,
    creation: OperationId,
    path: &Path,
    expected: &SetupIntent,
    token: &CancellationToken,
) -> Result<(), String> {
    let service = RepositoryService::system("git");
    let report = service
        .worktree_setup_creation(store, creation)
        .map_err(|error| error.to_string())?
        .ok_or("The setup creation is no longer recorded")?;
    let request = report
        .request
        .ok_or("The setup creation request is unavailable")?;
    if request.destination != path || request.setup_intent.as_ref() != Some(expected) {
        return Err("The saved setup intent changed. Reopen its review.".into());
    }
    let identity = service
        .worktree_setup_creation_identity(store, creation)
        .map_err(|error| error.to_string())?
        .ok_or("The setup creation identity is no longer recorded")?;
    let common = service
        .common_dir(&request.repository, token)
        .map_err(|error| error.to_string())?;
    let admin = service
        .git_dir(path, token)
        .map_err(|error| error.to_string())?;
    if !identity
        .matches_checkout(&common, &admin)
        .map_err(|error| error.to_string())?
    {
        return Err("This checkout no longer matches the recorded Git administrative identity. Create or configure setup for the current checkout explicitly.".into());
    }
    let records = service
        .list_worktrees(&request.repository, token)
        .map_err(|error| error.to_string())?;
    if !records.iter().any(|record| {
        record.path == path
            && !record.bare
            && record.branch.as_deref() == Some(request.branch.full_ref().as_str())
    }) {
        return Err("This checkout no longer matches the recorded creation".into());
    }
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client = crate::control::ControlClient::connect(&socket)
        .map_err(|error| format!("Cannot verify the previous setup terminal: {error}"))?;
    let sessions = client.list().map_err(|error| error.to_string())?;
    if sessions
        .iter()
        .any(|row| setup_run_is_live(&row.session, path, expected.run_id))
    {
        return Err("The previous setup is still hosted. Reconnect to that terminal instead of running its commands again.".into());
    }
    Ok(())
}

fn setup_run_is_live(session: &TerminalSession, path: &Path, run: Option<OperationId>) -> bool {
    session.launch.working_directory == path
        && session.continuity.live_proof().is_some()
        && run.is_some_and(|run| {
            session
                .launch
                .environment
                .get("CAGENTS_WORKTREE_SETUP_ID")
                .is_some_and(|value| value == &run.to_string())
        })
}

fn restored_setup_status(intent: &SetupIntent) -> WorktreeSetupStatus {
    setup_summary_status(
        intent.skip,
        intent.copy_done,
        intent
            .plan
            .as_ref()
            .is_some_and(|plan| !plan.commands.is_empty()),
    )
}

fn setup_summary_status(skip: bool, copy_done: bool, has_commands: bool) -> WorktreeSetupStatus {
    if skip {
        WorktreeSetupStatus::Skipped
    } else if copy_done && !has_commands {
        WorktreeSetupStatus::Succeeded
    } else {
        WorktreeSetupStatus::Interrupted(
            "Setup completion was not confirmed. Review files and terminal output before resuming."
                .into(),
        )
    }
}

impl Cagents {
    fn invalidate_setup_inventory_for_path(&mut self, path: &Path) {
        let projects = self
            .sidebar_model
            .projects
            .iter()
            .filter(|project| project.worktrees.iter().any(|row| row.path == path))
            .map(|project| project.id)
            .collect::<Vec<_>>();
        for project in projects {
            self.invalidate_worktree_setup_receipts(project);
        }
    }

    pub(super) fn dismiss_worktree_setup_review(&mut self) {
        if let Some(review) = self.project_ui.setup_review.take() {
            self.invalidate_setup_inventory_for_path(&review.path);
        }
    }

    pub(super) fn invalidate_worktree_setup_receipts(&mut self, project: ProjectId) {
        self.setup_ui.receipt_requests.remove(&project);
        if let Some(token) = self.setup_ui.receipt_tokens.remove(&project) {
            token.cancel();
        }
    }

    pub(super) fn refresh_all_worktree_setup_receipts(&mut self) -> Task<Message> {
        let projects = self
            .sidebar_model
            .projects
            .iter()
            .filter(|project| project.kind == ProjectKind::Git)
            .map(|project| project.id)
            .collect::<Vec<_>>();
        Task::batch(
            projects
                .into_iter()
                .map(|project| self.request_worktree_setup_receipts(project, true)),
        )
    }

    pub(super) fn request_worktree_setup_receipts(
        &mut self,
        project: ProjectId,
        force: bool,
    ) -> Task<Message> {
        let Some(row) = self
            .sidebar_model
            .project(project)
            .filter(|row| row.kind == ProjectKind::Git)
        else {
            return Task::none();
        };
        let repository = row.repository.clone();
        let mut paths = row
            .worktrees
            .iter()
            .map(|row| row.path.clone())
            .collect::<Vec<_>>();
        paths.sort();
        if !force
            && self.setup_ui.receipt_paths.get(&project) == Some(&paths)
            && self
                .setup_ui
                .receipt_requests
                .get(&project)
                .is_some_and(|(_, old, at)| {
                    old == &repository && at.elapsed() < Duration::from_secs(30)
                })
        {
            return Task::none();
        }
        let request = OperationId::new();
        self.setup_ui
            .receipt_requests
            .insert(project, (request, repository.clone(), Instant::now()));
        self.setup_ui.receipt_paths.insert(project, paths);
        let token = CancellationToken::new();
        if let Some(previous) = self.setup_ui.receipt_tokens.insert(project, token.clone()) {
            previous.cancel();
        }
        cosmic::task::future(async move {
            let source = repository.clone();
            // Waiting consumes no blocking-pool thread. Superseded refreshes
            // are discarded before starting Git or opening the journal store.
            let permit = INVENTORY_PERMITS.acquire().await;
            let result = if permit.is_err() || token.is_cancelled() {
                Err("Setup inventory refresh was superseded".into())
            } else {
                tokio::task::spawn_blocking(move || {
                    if token.is_cancelled() {
                        return Err("Setup inventory refresh was superseded".into());
                    }
                    match setup_control_request(crate::worktree_control::Operation::SetupIntents {
                        repository: source,
                    })? {
                        crate::worktree_control::Reply::SetupIntents { intents, warnings } => {
                            Ok((intents, warnings))
                        }
                        _ => Err("Unexpected worktree setup receipt inventory response".into()),
                    }
                })
                .await
                .map_err(|error| error.to_string())
                .and_then(|result| result)
            };
            drop(permit);
            Message::WorktreeSetupReceipts {
                project,
                repository,
                request,
                result,
            }
        })
    }

    pub(super) fn finish_worktree_setup_receipts(
        &mut self,
        project: ProjectId,
        repository: PathBuf,
        request: OperationId,
        result: Result<(Vec<crate::worktree_control::SetupReceipt>, Vec<String>), String>,
    ) -> Task<Message> {
        if !self
            .setup_ui
            .receipt_requests
            .get(&project)
            .is_some_and(|(current, source, _)| *current == request && source == &repository)
        {
            return Task::none();
        }
        let Some(row) = self
            .sidebar_model
            .project(project)
            .filter(|row| row.repository == repository)
        else {
            return Task::none();
        };
        let known = row
            .worktrees
            .iter()
            .map(|row| row.path.clone())
            .collect::<HashSet<_>>();
        let (receipts, warnings) = match result {
            Ok(result) => result,
            Err(error) => {
                self.status = Some(TransientNotice::warning(format!(
                    "Could not refresh worktree setup recovery: {error}"
                )));
                return Task::none();
            }
        };
        let mut seen = HashSet::new();
        if receipts
            .iter()
            .any(|receipt| !seen.insert(receipt.path.clone()))
        {
            self.status = Some(TransientNotice::warning(
                "The daemon returned ambiguous worktree setup receipts.",
            ));
            return Task::none();
        }
        // Old terminal cards are not evidence that a replacement checkout
        // inherited its creation. Preserve the receipt for an explicit review,
        // but suppress terminal arbitration until the daemon verifies a row.
        let omitted = self
            .setup_ui
            .daemon_receipts
            .iter()
            .filter(|(path, (owner, _))| *owner == project && !seen.contains(*path))
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        for path in omitted {
            if self.setup_ui.recovered.contains_key(&path)
                || self.setup_ui.pending.contains_key(&path)
            {
                continue;
            }
            if known.contains(&path) {
                self.setup_ui.uncertain_receipts.insert(path.clone());
                self.setup_ui.statuses.insert(path, WorktreeSetupStatus::Interrupted("Setup creation identity could not be verified for this checkout. Review its files and configuration before running setup.".into()));
            } else if warnings.is_empty() {
                self.setup_ui.daemon_receipts.remove(&path);
                self.setup_ui.uncertain_receipts.remove(&path);
                self.setup_ui.statuses.remove(&path);
                self.setup_ui.expected_runs.remove(&path);
                self.setup_ui.terminals.remove(&path);
            }
        }
        for receipt in receipts {
            if !known.contains(&receipt.path) || self.setup_ui.pending.contains_key(&receipt.path) {
                continue;
            }
            // An explicit review/resume in this window owns its newer run
            // until its completion; an older background inventory cannot
            // overwrite that run with metadata sampled before the review.
            if self
                .project_ui
                .setup_review
                .as_ref()
                .is_some_and(|review| review.path == receipt.path)
            {
                continue;
            }
            if self.setup_ui.recovered.contains_key(&receipt.path) {
                continue;
            }
            let previous = self.setup_ui.expected_runs.get(&receipt.path).copied();
            if let Some(run) = receipt.run_id {
                self.setup_ui
                    .expected_runs
                    .insert(receipt.path.clone(), run);
                if previous != Some(run) {
                    self.setup_ui.terminals.remove(&receipt.path);
                }
            }
            self.setup_ui.uncertain_receipts.remove(&receipt.path);
            self.setup_ui
                .daemon_receipts
                .insert(receipt.path.clone(), (project, receipt.creation_id));
            self.setup_ui.statuses.insert(
                receipt.path,
                setup_summary_status(receipt.skip, receipt.copy_done, receipt.has_commands),
            );
        }
        if !warnings.is_empty() {
            self.status = Some(TransientNotice::warning(warnings.join("; ")));
        }
        self.refresh_worktree_setup_rows();
        Task::none()
    }
}

pub(super) fn review_view(review: &Review) -> Element<'_, Message> {
    let request = review.request;
    let mut rows: Vec<Element<'_, Message>> = vec![
        design::title("Review interrupted worktree setup").into(),
        super::notice_detail(review.path.display().to_string()),
        super::notice_body(
            "Earlier commands may have run before the interruption. Inspect the files and terminal output before running setup again. Existing files will be preserved.",
        ),
    ];
    if review.loading {
        rows.push(super::notice_detail("Loading setup…"));
    }
    if let Some(plan) = &review.current.plan {
        for command in &plan.commands {
            rows.push(super::notice_detail(command.clone()));
        }
        if !plan.copy.is_empty() {
            rows.push(super::notice_detail(format!(
                "Copy: {}",
                plan.copy.join(", ")
            )));
        }
        if !review.trusted && !review.loading {
            rows.push(
                widget::checkbox(review.approved)
                    .label("Trust these exact project commands")
                    .on_toggle(move |value| {
                        Message::ProjectUi(project_ui::Action::SetupReviewTrust {
                            request,
                            approved: value,
                        })
                    })
                    .into(),
            );
        }
    } else if !review.loading {
        rows.push(super::notice_detail(
            "This project has no setup configuration.",
        ));
    }
    if let Some(error) = &review.error {
        rows.push(super::notice_detail(error.clone()));
    }
    rows.push(
        crate::ui_controls::button::secondary("Skip setup")
            .on_press_maybe(
                (!review.loading && !review.active).then_some(Message::ProjectUi(
                    project_ui::Action::ConfirmSetupReview {
                        request: review.request,
                        skip: true,
                    },
                )),
            )
            .into(),
    );
    let footer = super::dialog_ui::confirmation_actions(
        crate::ui_controls::button::secondary("Cancel")
            .id(widget::Id::new("dialog.setup.cancel"))
            .on_press(Message::ProjectUi(project_ui::Action::Dismiss)),
        crate::ui_controls::button::primary("Resume setup").on_press_maybe(
            (!review.loading
                && !review.active
                && review.error.is_none()
                && (review.trusted || review.approved))
                .then_some(Message::ProjectUi(project_ui::Action::ConfirmSetupReview {
                    request: review.request,
                    skip: false,
                })),
        ),
    );
    super::dialog_ui::surface(
        widget::column::with_children(vec![
            widget::container(
                widget::scrollable(widget::column::with_children(rows).spacing(space::M))
                    .height(iced::Length::Shrink),
            )
            .max_height(420.0)
            .into(),
            footer,
        ])
        .spacing(space::M),
        560.0,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn daemon_created_setup_terminal_markers_drive_the_worktree_row() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let project = ProjectId::new();
        let path = PathBuf::from("/repo/setup");
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            vec![SidebarWorktree::from_durable(
                crate::model::WorktreeId::new(),
                path.clone(),
                Some("setup".into()),
            )],
            Vec::new(),
        ));
        let run = OperationId::new();
        let mut session = fresh_terminal_session(LaunchRecipe::shell(path.clone()).unwrap());
        session.launch.environment.insert("CAGENTS_WORKTREE_SETUP_ID".into(), run.to_string());
        session
            .launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
        session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 0,
            },
        };
        Arc::make_mut(&mut app.durable).cards[0].kind = CardKind::Terminal(session);
        app.refresh_worktree_setup_rows();
        assert!(matches!(app.sidebar_model.projects[0].worktrees[0].setup_status,
            Some(WorktreeSetupStatus::Interrupted(_))), "a retained successful terminal alone cannot prove the current checkout generation");
        let inventory = OperationId::new();
        app.setup_ui.receipt_requests.insert(project, (inventory, "/repo".into(), Instant::now()));
        drop(app.finish_worktree_setup_receipts(project, "/repo".into(), inventory, Ok((vec![crate::worktree_control::SetupReceipt {
            path: path.clone(), creation_id: OperationId::new(), run_id: Some(run),
            skip: false, copy_done: true, has_commands: true,
        }], Vec::new()))));
        assert_eq!(app.sidebar_model.projects[0].worktrees[0].setup_status,
            Some(WorktreeSetupStatus::Succeeded), "the verified current receipt can promote its exact terminal result");
        app.setup_ui.pending.insert(path.clone(), (run, CancellationToken::new()));
        drop(app.finish_worktree_setup(run, path.clone(), Err("The checkout generation changed".into())));
        assert!(matches!(app.sidebar_model.projects[0].worktrees[0].setup_status,
            Some(WorktreeSetupStatus::Interrupted(_))), "old success must not overwrite a failed current preparation check");
        // Reusing a checkout path must not resurrect the retained old setup
        // result instead of showing the newly reserved terminal.
        let mut next = app.durable.cards[0].clone();
        next.id = CardId::new();
        let mut session =
            fresh_terminal_session(LaunchRecipe::shell("/repo/setup".into()).unwrap());
        session
            .launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
        session
            .launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP_STARTED_AT".into(), "10".into());
        next.kind = CardKind::Terminal(session);
        let next_run = OperationId::new();
        let CardKind::Terminal(session) = &mut next.kind else { unreachable!() };
        session.launch.environment.insert("CAGENTS_WORKTREE_SETUP_ID".into(), next_run.to_string());
        Arc::make_mut(&mut app.durable).cards.push(next);
        let inventory = OperationId::new();
        app.setup_ui.receipt_requests.insert(project, (inventory, "/repo".into(), Instant::now()));
        drop(app.finish_worktree_setup_receipts(project, "/repo".into(), inventory, Ok((vec![crate::worktree_control::SetupReceipt {
            path, creation_id: OperationId::new(), run_id: Some(next_run),
            skip: false, copy_done: true, has_commands: true,
        }], Vec::new()))));
        assert!(matches!(app.sidebar_model.projects[0].worktrees[0].setup_status,
            Some(WorktreeSetupStatus::Interrupted(_))));
    }

    #[test]
    fn durable_run_identity_wins_over_an_old_run_with_a_later_clock_timestamp() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let path = PathBuf::from("/repo/setup");
        let current = OperationId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            vec![SidebarWorktree::from_durable(
                crate::model::WorktreeId::new(),
                path.clone(),
                Some("setup".into()),
            )],
            vec![],
        ));
        let mut old = fresh_terminal_session(LaunchRecipe::shell(path.clone()).unwrap());
        old.launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
        old.launch.environment.insert(
            "CAGENTS_WORKTREE_SETUP_ID".into(),
            OperationId::new().to_string(),
        );
        old.launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP_STARTED_AT".into(), "100".into());
        old.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(9),
                signal: None,
                occurred_at_unix_ms: 100,
            },
        };
        Arc::make_mut(&mut app.durable).cards[0].kind = CardKind::Terminal(old);
        let mut next = app.durable.cards[0].clone();
        next.id = CardId::new();
        let mut session = fresh_terminal_session(LaunchRecipe::shell(path.clone()).unwrap());
        session
            .launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
        session
            .launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP_ID".into(), current.to_string());
        session
            .launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP_STARTED_AT".into(), "10".into());
        session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 10,
            },
        };
        let expected = session.id;
        next.kind = CardKind::Terminal(session);
        Arc::make_mut(&mut app.durable).cards.push(next);
        app.refresh_worktree_setup_rows();
        assert_eq!(app.setup_ui.terminals[&path].0, 100);
        app.recover_worktree_setup(
            current,
            path.clone(),
            SetupIntent {
                run_id: Some(current),
                ..Default::default()
            },
        );
        assert_eq!(app.setup_ui.statuses[&path], WorktreeSetupStatus::Succeeded);
        assert_eq!(app.setup_ui.terminals[&path].1, expected);
    }

    #[test]
    fn recovered_setup_is_visible_and_never_replays_commands() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let path = PathBuf::from("/repo/recovered");
        let creation = OperationId::new();
        let intent = SetupIntent {
            primary: Some("/repo".into()),
            plan: Some(SetupPlan {
                copy: vec![".env".into()],
                commands: vec!["must-not-run".into()],
                fingerprint: "a".repeat(64),
            }),
            run_id: Some(creation),
            ..Default::default()
        };
        app.recover_worktree_setup(creation, path.clone(), intent.clone());
        assert!(matches!(
            app.setup_ui.statuses[&path],
            WorktreeSetupStatus::Interrupted(_)
        ));
        assert!(app.setup_ui.pending.is_empty());
        assert!(app.setup_ui.authorized_runs.lock().unwrap().is_empty());
        assert!(!app.setup_ui.active());
        let mut copied = intent.clone();
        copied.copy_done = true;
        app.recover_worktree_setup(creation, path.clone(), copied.clone());
        assert!(
            matches!(
                app.setup_ui.statuses[&path],
                WorktreeSetupStatus::Interrupted(_)
            ),
            "copies do not prove command completion"
        );
        copied.plan.as_mut().unwrap().commands.clear();
        app.recover_worktree_setup(creation, path.clone(), copied);
        assert_eq!(app.setup_ui.statuses[&path], WorktreeSetupStatus::Succeeded);
        let mut skipped = intent;
        skipped.skip = true;
        app.recover_worktree_setup(creation, path.clone(), skipped);
        assert_eq!(app.setup_ui.statuses[&path], WorktreeSetupStatus::Skipped);
    }

    #[test]
    fn setup_review_user_actions_require_the_exposed_modal_and_no_update_freeze() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let id = OperationId::new();
        app.project_ui.setup_review = Some(Review {
            request: id,
            creation: id,
            path: "/repo".into(),
            expected: SetupIntent::default(),
            current: SetupIntent::default(),
            remote: None,
            active: false,
            loading: false,
            trusted: true,
            approved: false,
            error: None,
            token: CancellationToken::new(),
        });
        assert!(app.setup_review_on_top());
        let stale = OperationId::new();
        drop(
            app.handle_project_action(project_ui::Action::SetupReviewTrust {
                request: stale,
                approved: true,
            }),
        );
        assert!(!app.project_ui.setup_review.as_ref().unwrap().approved);
        let task = app.confirm_setup_review(stale, true);
        assert!(cosmic::iced::runtime::task::into_stream(task).is_none());
        assert!(!app.project_ui.setup_review.as_ref().unwrap().loading);
        for frozen in [false, true] {
            app.update_prepared = frozen;
            app.settings_reset_confirm = !frozen;
            assert!(!app.setup_review_on_top());
            drop(
                app.handle_project_action(project_ui::Action::SetupReviewTrust {
                    request: id,
                    approved: true,
                }),
            );
            assert!(!app.project_ui.setup_review.as_ref().unwrap().approved);
            let task = app.confirm_setup_review(id, true);
            assert!(cosmic::iced::runtime::task::into_stream(task).is_none());
            assert!(!app.project_ui.setup_review.as_ref().unwrap().loading);
        }
        assert!(user_message_blocked_during_update(&Message::ProjectUi(
            project_ui::Action::SetupReviewTrust {
                request: id,
                approved: true
            }
        )));
        assert!(user_message_blocked_during_update(&Message::ProjectUi(
            project_ui::Action::ConfirmSetupReview {
                request: id,
                skip: false
            }
        )));
        assert!(!user_message_blocked_during_update(&Message::ProjectUi(
            project_ui::Action::SetupReviewLoaded {
                request: id,
                result: Err("fixture".into())
            }
        )));
    }

    #[test]
    fn setup_launch_authorization_is_consumed_once_and_is_not_restored() {
        let state = State::default();
        let run = OperationId::new();
        let mut session = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
        session
            .launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP_ID".into(), run.to_string());
        assert!(state.authorized_run(&session).is_none());
        state.authorized_runs.lock().unwrap().insert(run);
        assert!(state.has_authorized_run(&session));
        assert_eq!(state.authorized_run(&session), Some(run));
        assert!(!state.has_authorized_run(&session));
        assert!(state.authorized_run(&session).is_none());
    }

    #[test]
    fn restored_pending_setup_is_interrupted_until_explicitly_started() {
        let session = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
        assert!(matches!(
            terminal_status(&session, true, false),
            WorktreeSetupStatus::Interrupted(_)
        ));
        assert_eq!(
            terminal_status(&session, true, true),
            WorktreeSetupStatus::Running
        );
    }

    #[test]
    fn stale_selection_callbacks_cannot_retire_a_newer_setup_path_guard() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let path = PathBuf::from("/repo/setup");
        let old = OperationId::new();
        let current = OperationId::new();
        app.setup_ui
            .pending
            .insert(path.clone(), (current, CancellationToken::new()));
        app.setup_ui.recovered.insert(
            path.clone(),
            (
                current,
                SetupIntent {
                    run_id: Some(current),
                    ..Default::default()
                },
            ),
        );
        app.setup_ui.authorized_runs.lock().unwrap().insert(current);
        drop(app.worktree_setup_selection_finished(old, path.clone(), false));
        assert!(app.setup_ui.preparing(&path));
        drop(app.worktree_setup_selection_finished(current, path.clone(), false));
        assert!(!app.setup_ui.preparing(&path));
        assert!(!app.setup_ui.active());
        assert!(app.setup_ui.authorized_runs.lock().unwrap().is_empty());
        assert!(matches!(
            app.setup_ui.statuses[&path],
            WorktreeSetupStatus::Interrupted(_)
        ));
    }

    #[test]
    fn daemon_inventory_exposes_copy_finished_without_a_setup_terminal_as_interrupted() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let path = PathBuf::from("/repo/setup");
        let project = ProjectId::new();
        let request = OperationId::new();
        let run = OperationId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            vec![SidebarWorktree::from_durable(
                crate::model::WorktreeId::new(),
                path.clone(),
                Some("setup".into()),
            )],
            vec![],
        ));
        app.setup_ui
            .receipt_requests
            .insert(project, (request, "/repo".into(), Instant::now()));
        let receipt = crate::worktree_control::SetupReceipt {
            path: path.clone(),
            creation_id: OperationId::new(),
            run_id: Some(run),
            skip: false,
            copy_done: true,
            has_commands: true,
        };
        drop(app.finish_worktree_setup_receipts(
            project,
            "/repo".into(),
            request,
            Ok((vec![receipt], vec![])),
        ));
        assert!(matches!(
            app.sidebar_model.projects[0].worktrees[0].setup_status,
            Some(WorktreeSetupStatus::Interrupted(_))
        ));
        assert_eq!(app.setup_ui.expected_runs[&path], run);
        assert!(
            !app.setup_ui.recovered.contains_key(&path),
            "daemon receipts must use daemon-owned review"
        );
        assert!(app.durable.cards.is_empty());
    }

    #[test]
    fn omitted_daemon_creation_receipt_suppresses_old_successful_terminal_evidence() {
        let mut app = crate::app::tests::context_inspector_test_app(true);
        let path = PathBuf::from("/repo/setup");
        let project = ProjectId::new();
        let run = OperationId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            vec![SidebarWorktree::from_durable(
                crate::model::WorktreeId::new(),
                path.clone(),
                Some("setup".into()),
            )],
            vec![],
        ));
        let mut session = fresh_terminal_session(LaunchRecipe::shell(path.clone()).unwrap());
        session
            .launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
        session
            .launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP_ID".into(), run.to_string());
        session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 0,
            },
        };
        Arc::make_mut(&mut app.durable).cards[0].kind = CardKind::Terminal(session);
        app.setup_ui
            .daemon_receipts
            .insert(path.clone(), (project, OperationId::new()));
        app.setup_ui.expected_runs.insert(path.clone(), run);
        app.refresh_worktree_setup_rows();
        assert_eq!(app.setup_ui.statuses[&path], WorktreeSetupStatus::Succeeded);
        for warnings in [vec!["Creation generation no longer matches".into()], vec![]] {
            let request = OperationId::new();
            app.setup_ui
                .receipt_requests
                .insert(project, (request, "/repo".into(), Instant::now()));
            drop(app.finish_worktree_setup_receipts(
                project,
                "/repo".into(),
                request,
                Ok((vec![], warnings)),
            ));
            assert!(matches!(
                app.setup_ui.statuses[&path],
                WorktreeSetupStatus::Interrupted(_)
            ));
        }
        let receipt = crate::worktree_control::SetupReceipt {
            path: path.clone(),
            creation_id: OperationId::new(),
            run_id: Some(OperationId::new()),
            skip: true,
            copy_done: false,
            has_commands: false,
        };
        let request = OperationId::new();
        app.setup_ui
            .receipt_requests
            .insert(project, (request, "/repo".into(), Instant::now()));
        drop(app.finish_worktree_setup_receipts(
            project,
            "/repo".into(),
            request,
            Ok((vec![receipt], vec![])),
        ));
        assert_eq!(app.setup_ui.statuses[&path], WorktreeSetupStatus::Skipped);
        assert!(!app.setup_ui.uncertain_receipts.contains(&path));
    }

    #[test]
    fn a_metadata_inventory_sampled_before_resume_cannot_overwrite_its_receipt() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let path = PathBuf::from("/repo/setup");
        let project = ProjectId::new();
        let inventory = OperationId::new();
        let review = OperationId::new();
        let run = OperationId::new();
        app.sidebar_model.projects.push(SidebarProject::new(
            project,
            RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            vec![SidebarWorktree::from_durable(
                crate::model::WorktreeId::new(),
                path.clone(),
                Some("setup".into()),
            )],
            vec![],
        ));
        app.setup_ui
            .receipt_requests
            .insert(project, (inventory, "/repo".into(), Instant::now()));
        app.setup_ui.review_requests.insert(path.clone(), review);
        drop(app.setup_review_ready(
            review,
            Ok(ReviewCompletion::Remote {
                path: path.clone(),
                run_id: run,
                setup: None,
                warnings: vec![],
                skipped: true,
            }),
        ));
        let old = crate::worktree_control::SetupReceipt {
            path: path.clone(),
            creation_id: OperationId::new(),
            run_id: Some(OperationId::new()),
            skip: false,
            copy_done: true,
            has_commands: true,
        };
        drop(app.finish_worktree_setup_receipts(
            project,
            "/repo".into(),
            inventory,
            Ok((vec![old], vec![])),
        ));
        assert_eq!(app.setup_ui.expected_runs[&path], run);
        assert_eq!(app.setup_ui.statuses[&path], WorktreeSetupStatus::Skipped);
    }

    #[test]
    fn a_late_daemon_resume_receipt_cannot_replace_a_newer_review_run() {
        let mut app = crate::app::tests::context_inspector_test_app(false);
        let path = PathBuf::from("/repo/setup");
        app.sidebar_model.projects.push(SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "repo".into(),
            "/repo".into(),
            vec![SidebarWorktree::from_durable(
                crate::model::WorktreeId::new(),
                path.clone(),
                Some("setup".into()),
            )],
            vec![],
        ));
        let first_request = OperationId::new();
        let second_request = OperationId::new();
        let first_run = OperationId::new();
        let second_run = OperationId::new();
        app.setup_ui
            .review_requests
            .insert(path.clone(), second_request);
        // Both dialogs were dismissed; only the last review retains authority
        // to publish a completion into this window's status cache.
        drop(app.setup_review_ready(
            second_request,
            Ok(ReviewCompletion::Remote {
                path: path.clone(),
                run_id: second_run,
                setup: None,
                warnings: vec![],
                skipped: true,
            }),
        ));
        drop(app.setup_review_ready(
            first_request,
            Ok(ReviewCompletion::Remote {
                path: path.clone(),
                run_id: first_run,
                setup: None,
                warnings: vec![],
                skipped: false,
            }),
        ));
        assert_eq!(app.setup_ui.expected_runs[&path], second_run);
        assert_eq!(app.setup_ui.statuses[&path], WorktreeSetupStatus::Skipped);
    }

    #[test]
    fn daemon_setup_review_validates_typed_replies_and_current_configuration() {
        let run = OperationId::new();
        let intent = SetupIntent {
            run_id: Some(run),
            ..Default::default()
        };
        let plan = SetupPlan {
            copy: vec![".env".into()],
            commands: vec!["printf reviewed".into()],
            fingerprint: "a".repeat(64),
        };
        let reply = crate::worktree_control::Reply::SetupIntent {
            creation_id: OperationId::new(),
            primary: "/repo".into(),
            intent: serde_json::to_string(&intent).unwrap(),
            current_plan: Some(serde_json::to_string(&plan).unwrap()),
            plan_error: None,
            trusted: false,
            active: true,
        };
        let review = decode_daemon_setup_review(reply).unwrap();
        assert_eq!(review.expected.run_id, Some(run));
        assert_eq!(review.current.primary.as_deref(), Some(Path::new("/repo")));
        assert_eq!(review.current.plan, Some(plan));
        assert!(review.active);
        assert!(review.error.unwrap().contains("still active"));
        assert!(
            decode_daemon_setup_review(crate::worktree_control::Reply::StaleLock {
                unlocked: false
            })
            .is_err()
        );
        assert!(
            decode_daemon_setup_review(crate::worktree_control::Reply::SetupIntent {
                creation_id: OperationId::new(),
                primary: "/repo".into(),
                intent: "invalid".into(),
                current_plan: None,
                plan_error: None,
                trusted: false,
                active: false
            })
            .is_err()
        );
    }

    #[test]
    fn resumed_terminal_receipts_must_match_the_new_run_and_destination() {
        let path = Path::new("/repo/setup");
        let previous = OperationId::new();
        let current = OperationId::new();
        let mut setup = crate::worktree_control::Setup {
            command: "printf setup".into(),
            run_id: Some(current),
            terminal: None,
            error: None,
        };
        assert!(validate_resumed_setup(path, Some(previous), current, Some(&setup)).is_ok());
        assert!(validate_resumed_setup(path, Some(current), current, Some(&setup)).is_err());
        setup.run_id = Some(previous);
        assert!(validate_resumed_setup(path, Some(previous), current, Some(&setup)).is_err());
        setup.run_id = Some(current);
        let mut session = fresh_terminal_session(LaunchRecipe::shell(path.into()).unwrap());
        session
            .launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP".into(), "1".into());
        session
            .launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP_ID".into(), current.to_string());
        setup.terminal = Some(session);
        assert!(validate_resumed_setup(path, Some(previous), current, Some(&setup)).is_ok());
        assert!(
            validate_resumed_setup(
                Path::new("/repo/other"),
                Some(previous),
                current,
                Some(&setup)
            )
            .is_err()
        );
    }

    #[test]
    fn a_live_previous_run_is_not_authority_to_launch_another_setup() {
        let path = Path::new("/repo/setup");
        let run = OperationId::new();
        let mut session = fresh_terminal_session(LaunchRecipe::shell(path.into()).unwrap());
        session
            .launch
            .environment
            .insert("CAGENTS_WORKTREE_SETUP_ID".into(), run.to_string());
        session.continuity = TerminalContinuity::Live {
            host_session_id: crate::model::HostSessionId::new("fixture"),
            boot_id: crate::model::BootId::new("fixture"),
            host_incarnation: crate::model::HostIncarnation::new("fixture"),
        };
        assert!(setup_run_is_live(&session, path, Some(run)));
        assert!(!setup_run_is_live(&session, path, Some(OperationId::new())));
        assert!(!setup_run_is_live(
            &session,
            Path::new("/repo/other"),
            Some(run)
        ));
        session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(1),
                signal: None,
                occurred_at_unix_ms: 0,
            },
        };
        assert!(!setup_run_is_live(&session, path, Some(run)));
    }

    #[test]
    fn superseded_setup_skip_prevents_old_gui_preparation_from_copying() {
        use crate::process::{ProcessExecutor, SystemProcessExecutor};
        let root = tempfile::tempdir().unwrap();
        let primary = root.path().join("project");
        std::fs::create_dir(&primary).unwrap();
        let token = CancellationToken::new();
        let git_service = RepositoryService::system("git");
        let git = |arguments: &[&str]| {
            let command = git_service.git_base(&primary, false)
                .args(arguments)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null");
            SystemProcessExecutor.execute(&command, &token)
                .and_then(|output| output.require_success(&command.program)).unwrap();
        };
        git(&["init", "--quiet", "-b", "main"]);
        std::fs::write(primary.join("tracked"), "initial").unwrap();
        git(&["add", "tracked"]);
        git(&["-c", "user.name=Setup Contract", "-c", "user.email=setup@example.invalid", "commit", "-qm", "initial"]);
        std::fs::create_dir(primary.join(".cagents")).unwrap();
        std::fs::write(primary.join(".cagents/worktree.toml"), "copy = [\".env\"]\n").unwrap();
        std::fs::write(primary.join(".env"), "preserve").unwrap();
        let service = RepositoryService::system("git");
        let plan = crate::worktree_setup::load(&primary).unwrap().unwrap();
        let mut request = service.resolve_worktree_request(&crate::repository::WorktreeInput {
            repository: primary.clone(), input: "topic".into(), base: "main".into(),
            managed_directory: ".worktrees/".into(), destination: String::new(), operation_id: OperationId::new(),
        }, &token).unwrap();
        let original = SetupIntent {primary: Some(primary), plan: Some(plan), skip: false,
            run_id: Some(OperationId::new()), copy_done: false};
        request.setup_intent = Some(original.clone());
        let store = crate::persistence::StateStore::open(root.path().join("journal.sqlite3"), crate::model::BootId::new("gui-prepare-test")).unwrap();
        let (progress, _) = async_channel::bounded(8);
        let report = service.create_worktree_journaled(&store, &request, 0, &token, &progress);
        assert_eq!(report.state, crate::repository::WorktreeOperationState::Created, "{}", report.detail);
        let mut skipped = original.clone();
        skipped.run_id = Some(OperationId::new()); skipped.skip = true; skipped.copy_done = true;
        service.update_worktree_setup_intent_guarded(&store, request.operation_id, Some(&original), skipped.clone()).unwrap();
        assert!(prepare_recorded_setup(Some(&store), original, &request.destination, request.operation_id, &token).is_err());
        assert!(!request.destination.join(".env").exists(), "a superseded run must not copy ignored files after an explicit skip");
        assert_eq!(service.worktree_setup_creation(&store, request.operation_id).unwrap().unwrap().request.unwrap().setup_intent, Some(skipped));
    }

    #[test]
    fn skipped_setup_needs_no_filesystem_or_project_configuration() {
        let result = prepare_recorded_setup(
            None,
            SetupIntent {
                primary: None,
                plan: None,
                skip: true,
                ..Default::default()
            },
            Path::new("/missing/worktree"),
            OperationId::new(),
            &CancellationToken::new(),
        )
        .unwrap();
        assert!(result.skipped);
        assert!(result.command.is_none());
    }
    #[test]
    fn setup_row_reports_success_failure_running_and_explicit_close() {
        let mut session = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
        assert_eq!(
            terminal_status(&session, true, true),
            WorktreeSetupStatus::Running
        );
        assert!(matches!(
            terminal_status(&session, false, false),
            WorktreeSetupStatus::Failed(_)
        ));
        session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(0),
                signal: None,
                occurred_at_unix_ms: 0,
            },
        };
        assert_eq!(
            terminal_status(&session, true, true),
            WorktreeSetupStatus::Succeeded
        );
        session.continuity = TerminalContinuity::Exited {
            last_exit: crate::model::TerminalExit {
                code: Some(7),
                signal: None,
                occurred_at_unix_ms: 0,
            },
        };
        assert_eq!(
            terminal_status(&session, true, true),
            WorktreeSetupStatus::Failed("Setup exited with status 7".into())
        );
    }
}
