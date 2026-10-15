//! Interactive, scoped setup after a journaled worktree has fully settled.
use super::*;
use crate::{model::OperationId, sidebar::WorktreeSetupStatus, worktree_setup::{SetupPlan, SetupPreparation}};

#[derive(Debug, Clone)]
pub(super) struct SetupIntent {
    pub primary: Option<PathBuf>,
    pub plan: Option<SetupPlan>,
    pub skip: bool,
}

#[derive(Default)]
pub(super) struct State {
    pending: HashMap<PathBuf, (OperationId, CancellationToken)>,
    statuses: HashMap<PathBuf, WorktreeSetupStatus>,
    terminals: HashMap<PathBuf, (u64, TerminalId)>,
}

impl Drop for State {
    fn drop(&mut self) { for (_, token) in self.pending.values() { token.cancel(); } }
}

impl State {
    pub fn preparing(&self, path: &Path) -> bool { self.pending.contains_key(path) }
    pub fn protected_directories(&self) -> impl Iterator<Item = &PathBuf> { self.pending.keys() }
    pub fn active(&self) -> bool {
        !self.pending.is_empty() || self.statuses.values().any(|status| *status == WorktreeSetupStatus::Running)
    }
}

fn terminal_status(session: &TerminalSession, open: bool) -> WorktreeSetupStatus {
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
        let token = CancellationToken::new();
        self.setup_ui.pending.insert(destination.clone(), (operation, token.clone()));
        self.setup_ui.statuses.insert(destination.clone(), WorktreeSetupStatus::Preparing);
        cosmic::task::future(async move {
            let worktree = destination.clone();
            let result = tokio::task::spawn_blocking(move || preparation(intent, &destination, &token))
                .await.map_err(|error| error.to_string()).and_then(|result| result);
            Message::WorktreeSetupPrepared { operation, worktree, result }
        })
    }

    pub(super) fn finish_worktree_setup(&mut self, operation: OperationId, worktree: PathBuf, result: Result<SetupPreparation, String>) -> Task<Message> {
        if !self.setup_ui.pending.get(&worktree).is_some_and(|(pending, _)| *pending == operation) { return Task::none(); }
        self.setup_ui.pending.remove(&worktree);
        let prepared = match result {
            Ok(prepared) => prepared,
            Err(error) => {
                self.setup_ui.statuses.insert(worktree, WorktreeSetupStatus::Failed(error.clone()));
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
            let started_at = session.launch.environment.get("CAGENTS_WORKTREE_SETUP_STARTED_AT").and_then(|value| value.parse::<u64>().ok()).unwrap_or_default();
            if self.setup_ui.terminals.get(path).is_some_and(|(known_at, known)| *known_at > started_at || (*known_at == started_at && *known != session.id)) { continue; }
            self.setup_ui.terminals.insert(path.clone(), (started_at, session.id));
            self.setup_ui.statuses.insert(path.clone(), terminal_status(session, card.existence.is_open()));
        }
        let roots = self.sidebar_model.projects.iter().flat_map(|project| project.worktrees.iter().map(|worktree| worktree.path.clone())).collect::<HashSet<_>>();
        self.setup_ui.statuses.retain(|path, _| roots.contains(path) || self.setup_ui.pending.contains_key(path));
        self.setup_ui.terminals.retain(|path, _| roots.contains(path) || self.setup_ui.pending.contains_key(path));
        for worktree in self.sidebar_model.projects.iter_mut().flat_map(|project| &mut project.worktrees) {
            worktree.setup_status = self.setup_ui.statuses.get(&worktree.path).cloned();
        }
    }
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
        assert_eq!(app.sidebar_model.projects[0].worktrees[0].setup_status, Some(WorktreeSetupStatus::Running));
    }

    #[test]
    fn skipped_setup_needs_no_filesystem_or_project_configuration() {
        let result = preparation(SetupIntent { primary: None, plan: None, skip: true }, Path::new("/missing/worktree"), &CancellationToken::new()).unwrap();
        assert!(result.skipped);
        assert!(result.command.is_none());
    }
    #[test]
    fn setup_row_reports_success_failure_running_and_explicit_close() {
        let mut session = fresh_terminal_session(LaunchRecipe::shell("/repo".into()).unwrap());
        assert_eq!(terminal_status(&session, true), WorktreeSetupStatus::Running);
        assert!(matches!(terminal_status(&session, false), WorktreeSetupStatus::Failed(_)));
        session.continuity = TerminalContinuity::Exited { last_exit: crate::model::TerminalExit { code: Some(0), signal: None, occurred_at_unix_ms: 0 } };
        assert_eq!(terminal_status(&session, true), WorktreeSetupStatus::Succeeded);
        session.continuity = TerminalContinuity::Exited { last_exit: crate::model::TerminalExit { code: Some(7), signal: None, occurred_at_unix_ms: 0 } };
        assert_eq!(terminal_status(&session, true), WorktreeSetupStatus::Failed("Setup exited with status 7".into()));
    }
}
