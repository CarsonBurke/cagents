use super::*;
use crate::control::ControlClient;

/// How long the observer waits before reconnecting.
///
/// A window can open before any daemon exists, and the daemon can be replaced
/// under it. Neither is an error, so the observer retries rather than giving up
/// - and it retries slowly, because the only thing it can miss in the meantime
/// is an inventory it re-reads in full on the next connection.
const RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Watches the daemon for terminals this window does not own.
///
/// Every message is a complete set of observed sessions: the full inventory on
/// connect, and one session per creation edge afterwards. The receiver decides
/// what to do with them, so a reconnection after a missed edge is a resynchronise
/// rather than a special case.
pub(super) fn observe_hosted_inventory(
    sender: async_channel::Sender<Vec<crate::terminal::ipc::TerminalDescriptor>>,
) {
    let observer = std::thread::Builder::new()
        .name("cagents-terminal-inventory".into())
        .spawn(move || {
            while !sender.is_closed() {
                if let Err(error) = observe_inventory(&sender) {
                    tracing::debug!(%error, "terminal inventory observation ended");
                }
                std::thread::sleep(RECONNECT_DELAY);
            }
        });
    if let Err(error) = observer {
        tracing::warn!(
            %error,
            "could not start the terminal inventory observer; terminals created by other clients \
             will only be adopted when this window is restarted"
        );
    }
}

fn observe_inventory(
    sender: &async_channel::Sender<Vec<crate::terminal::ipc::TerminalDescriptor>>,
) -> Result<(), String> {
    // Never start a daemon merely to watch it: with no daemon there is nothing
    // to adopt, and the window starts one itself as soon as it hosts a terminal.
    let socket = crate::terminal::ipc::daemon_socket_path().map_err(|error| error.to_string())?;
    let mut client = ControlClient::connect(&socket).map_err(|error| error.to_string())?;
    client
        .watch_terminals()
        .map_err(|error| error.to_string())?;
    // Subscribe first, then list. A terminal created between the two arrives as
    // a buffered edge; listing first would lose it.
    let listed = client.list().map_err(|error| error.to_string())?;
    sender
        .send_blocking(listed)
        .map_err(|error| error.to_string())?;
    loop {
        let created = client
            .next_created_terminal()
            .map_err(|error| error.to_string())?;
        sender
            .send_blocking(vec![created])
            .map_err(|error| error.to_string())?;
    }
}

/// Whether `card` claims `terminal`.
///
/// Existence is deliberately not consulted. A closed card still claims its
/// terminal, because the user's close is an instruction: the daemon may keep
/// hosting that process, and a later observation of it must not reopen the card
/// the user shut.
fn card_claims_terminal(card: &CardState, terminal: TerminalId) -> bool {
    match &card.kind {
        CardKind::Terminal(session) => session.id == terminal,
        CardKind::Chat(session) => {
            session.id == terminal
                || session
                    .terminal
                    .as_ref()
                    .is_some_and(|hosted| hosted.id == terminal)
        }
        CardKind::Browser(_)
        | CardKind::PullRequests(_)
        | CardKind::Repository(_)
        | CardKind::Settings
        | CardKind::Accounts => false,
    }
}

impl Cagents {
    pub(super) fn observe_hosted_descriptors(
        &mut self,
        terminals: Vec<crate::terminal::ipc::TerminalDescriptor>,
    ) -> Task<Message> {
        for terminal in &terminals {
            if !terminal.upgrade_relaunches.is_empty() {
                self.relaunched_terminals.insert(terminal.session.id);
            } else {
                self.relaunched_terminals.remove(&terminal.session.id);
            }
        }
        self.observe_hosted_terminals(
            terminals
                .into_iter()
                .map(|terminal| terminal.session)
                .collect(),
        )
    }
    /// Adopts every observed hosted terminal this window has no card for.
    ///
    /// The daemon owns the terminal inventory, so a terminal spawned by the CLI
    /// - with or without a window running - belongs on screen exactly like one
    /// this window spawned itself. Adoption attaches to the existing session; it
    /// never starts a second process.
    pub(super) fn observe_hosted_terminals(
        &mut self,
        terminals: Vec<TerminalSession>,
    ) -> Task<Message> {
        // A start that has not reported back may be about to claim one of these
        // identities: recovery can replace a terminal ID mid-start, so the card
        // does not hold the identity the daemon already published. Deciding now
        // would race it, so the observation waits for a quiet moment instead.
        if self.terminal_starts_in_flight() > 0 {
            self.pending_adoptions.extend(terminals);
            return Task::none();
        }
        let mut tasks = Vec::new();
        for session in terminals {
            if !self.adoptable(&session) {
                continue;
            }
            tasks.push(self.adopt_hosted_terminal(session));
        }
        if tasks.is_empty() {
            return Task::none();
        }
        self.persist();
        tasks.push(self.lifecycle_sweep());
        Task::batch(tasks)
    }

    /// Releases one in-flight terminal start and decides the observations it
    /// held back, then lets the next restored agent attach.
    pub(super) fn finish_terminal_start(&mut self) -> Task<Message> {
        let remaining = self.terminal_starts_in_flight().saturating_sub(1);
        self.terminal_starts
            .store(remaining, std::sync::atomic::Ordering::Relaxed);
        if remaining > 0 {
            return Task::none();
        }
        let adopted = if self.pending_adoptions.is_empty() {
            Task::none()
        } else {
            let pending = std::mem::take(&mut self.pending_adoptions);
            self.observe_hosted_terminals(pending)
        };
        Task::batch([adopted, self.attach_next_restored_agent()])
    }

    pub(super) fn terminal_starts_in_flight(&self) -> usize {
        self.terminal_starts
            .load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Counts a terminal start from the start path, which only holds `&self`.
    pub(super) fn begin_terminal_start(&self) {
        let started = self.terminal_starts_in_flight().saturating_add(1);
        self.terminal_starts
            .store(started, std::sync::atomic::Ordering::Relaxed);
    }

    fn adoptable(&self, session: &TerminalSession) -> bool {
        // Only a terminal the daemon still hosts can be attached. An exited one
        // would have to be relaunched, and relaunching is a user's decision.
        session.continuity.live_proof().is_some()
            // A terminal in the middle of a native-chat handoff is already this
            // window's, even before the card that will hold it is written.
            && !self.pending_terminal_handoffs.contains_key(&session.id)
            && !self
                .durable
                .cards
                .iter()
                .any(|card| card_claims_terminal(card, session.id))
    }

    fn adopt_hosted_terminal(&mut self, session: TerminalSession) -> Task<Message> {
        let directory = session.launch.working_directory.clone();
        let matched = self.worktree_scope_for(&directory);
        // Where a user would look for it: the worktree that owns the launch
        // directory, and otherwise the workspace they are looking at.
        let scope = matched.clone().or_else(|| self.active_workspace.clone());
        let card_id = CardId::new();
        let card = CardState {
            id: card_id,
            generation: self.durable.generation,
            title: directory
                .file_name()
                .and_then(|name| normalize_card_name(&name.to_string_lossy()))
                .unwrap_or_else(|| "Terminal".into()),
            width_fraction: self.settings.default_column_fraction,
            existence: Existence::Open,
            // Adoption is not a user action, so it claims no camera and no
            // focus. The card enters cold and offscreen and is hydrated by the
            // ordinary lifecycle sweep, which is also what admits it when the
            // strip scrolls it into view.
            exposure: Exposure::Offscreen,
            residency: Residency::Cold,
            kind: CardKind::Terminal(session),
        };
        let durable = Arc::make_mut(&mut self.durable);
        durable.cards.push(card.clone());
        if matched.is_none()
            && let Some(scope) = &scope
        {
            // No worktree owns this directory, so discovery cannot re-derive
            // the filing. Record it, or the user finds the card somewhere else
            // after a restart.
            durable.card_scopes.insert(card_id, scope.clone());
        }
        self.cards.insert(card_id, CardRuntime::Loading);
        self.lifecycle.register(
            card_id,
            Exposure::Offscreen,
            Residency::Cold,
            Instant::now(),
        );
        // A channel strip shows exactly its own cards, and an adopted terminal
        // is not one of them. Another worktree's strip is rebuilt from the
        // durable cards when the user navigates there.
        if self.durable.active_channel.is_some() || scope != self.active_workspace {
            return Task::none();
        }
        let index = self.workspace.cards().len();
        match self.workspace.add_card(
            WorkspaceCard::new(
                card_id,
                card.title,
                CardWidth::Fraction(card.width_fraction),
            )
            .expect("adopted terminal card has a normalized title and a valid width"),
            index,
            false,
        ) {
            Ok(update) => self.apply_viewport(update, true, false),
            Err(error) => {
                self.status = Some(TransientNotice::error(error.to_string()));
                Task::none()
            }
        }
    }

    /// The known worktree that owns `directory`.
    pub(super) fn worktree_scope_for(&self, directory: &Path) -> Option<PathBuf> {
        worktree_ui::worktree_root(
            directory,
            self.known_worktree_roots
                .iter()
                .map(PathBuf::as_path)
                .chain(self.sidebar_model.projects.iter().flat_map(|project| {
                    project
                        .worktrees
                        .iter()
                        .map(|worktree| worktree.path.as_path())
                })),
        )
        .map(Path::to_path_buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        model::{BootId, HostIncarnation, HostSessionId, TerminalId, WorktreeId},
        repository::RepositoryId,
    };

    fn hosted(directory: &str) -> TerminalSession {
        TerminalSession {
            recovery_origin: None,
            id: TerminalId::new(),
            launch: LaunchRecipe {
                program: "/bin/sh".into(),
                arguments: Vec::new(),
                working_directory: directory.into(),
                environment: Default::default(),
            },
            transcript_path: None,
            resume_hint: None,
            continuity: TerminalContinuity::DetachedSameBoot {
                host_session_id: HostSessionId::new("host"),
                boot_id: BootId::new("boot"),
                host_incarnation: HostIncarnation::new("incarnation"),
            },
        }
    }

    fn register_worktrees(app: &mut Cagents, paths: &[&str]) {
        app.sidebar_model.projects = vec![SidebarProject::new(
            ProjectId::new(),
            RepositoryId::new(),
            "repo".into(),
            paths[0].into(),
            paths
                .iter()
                .map(|path| {
                    SidebarWorktree::new(
                        WorktreeId::new(),
                        (*path).into(),
                        None,
                        false,
                        false,
                        None,
                        None,
                        None,
                    )
                })
                .collect(),
            Vec::new(),
        )];
        app.known_worktree_roots = paths.iter().map(PathBuf::from).collect();
    }

    fn cards_for(app: &Cagents, terminal: TerminalId) -> Vec<CardState> {
        app.durable
            .cards
            .iter()
            .filter(|card| card_claims_terminal(card, terminal))
            .cloned()
            .collect()
    }

    #[test]
    fn a_hosted_terminal_no_card_owns_becomes_one_card_in_its_own_worktree() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo", "/repo/worktree"]);
        let _ = app.select_worktree("/repo".into(), false);
        let session = hosted("/repo/worktree/src");

        let _ = app.observe_hosted_terminals(vec![session.clone()]);

        let adopted = cards_for(&app, session.id);
        assert_eq!(adopted.len(), 1);
        // Attaching, never spawning: the daemon already owns this process.
        assert!(matches!(
            terminal_start_mode(&session),
            Ok(TerminalStartMode::Attach(_))
        ));
        assert_eq!(
            app.card_worktree(&adopted[0]),
            Some(Path::new("/repo/worktree"))
        );
        assert!(
            !app.workspace.order().any(|card| card == adopted[0].id),
            "another worktree's strip shows it, not the selected one"
        );
        let _ = app.select_worktree("/repo/worktree".into(), false);
        assert!(app.workspace.order().any(|card| card == adopted[0].id));

        // The observer resynchronizes by re-listing, so the same terminal is
        // observed repeatedly and must not accumulate cards.
        let _ = app.observe_hosted_terminals(vec![session.clone()]);
        assert_eq!(cards_for(&app, session.id).len(), 1);
    }

    #[test]
    fn a_card_the_user_closed_is_never_re_adopted() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo"]);
        let _ = app.select_worktree("/repo".into(), false);
        let session = hosted("/repo");
        let _ = app.observe_hosted_terminals(vec![session.clone()]);
        let card = cards_for(&app, session.id)[0].id;
        let closed = Existence::Closed {
            tombstone: crate::model::Tombstone {
                operation_id: None,
                closed_at_unix_ms: 1,
                reason: Some("closed by user".into()),
            },
        };
        if let Some(state) = app.card_state_mut(card) {
            state.existence = closed.clone();
        }

        let _ = app.observe_hosted_terminals(vec![session]);

        let remaining = cards_for(&app, card_terminal(&app, card));
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].existence, closed);
    }

    #[test]
    fn an_observation_waits_for_an_outstanding_start_to_claim_its_identity() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo"]);
        let _ = app.select_worktree("/repo".into(), false);
        let session = hosted("/repo");
        app.begin_terminal_start();

        let _ = app.observe_hosted_terminals(vec![session.clone()]);
        assert!(cards_for(&app, session.id).is_empty());
        assert_eq!(app.pending_adoptions.len(), 1);

        let _ = app.finish_terminal_start();
        assert_eq!(cards_for(&app, session.id).len(), 1);
        assert!(app.pending_adoptions.is_empty());
    }

    #[test]
    fn an_exited_terminal_is_left_for_the_user_to_relaunch() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo"]);
        let _ = app.select_worktree("/repo".into(), false);
        let mut session = hosted("/repo");
        session.continuity = TerminalContinuity::RelaunchAvailable {
            recipe: session.launch.clone(),
            last_exit: None,
        };

        let _ = app.observe_hosted_terminals(vec![session.clone()]);

        assert!(cards_for(&app, session.id).is_empty());
    }

    #[test]
    fn a_terminal_outside_every_worktree_stays_filed_under_the_workspace_it_joined() {
        let mut app = super::super::tests::context_inspector_test_app(false);
        register_worktrees(&mut app, &["/repo", "/repo/worktree"]);
        let _ = app.select_worktree("/repo".into(), false);
        let session = hosted("/tmp/elsewhere");

        let _ = app.observe_hosted_terminals(vec![session.clone()]);

        let adopted = cards_for(&app, session.id);
        assert_eq!(adopted.len(), 1);
        assert_eq!(
            app.durable
                .card_scopes
                .get(&adopted[0].id)
                .map(PathBuf::as_path),
            Some(Path::new("/repo"))
        );
        assert!(app.workspace.order().any(|card| card == adopted[0].id));
        // Navigating away and back must find it in the same place, which is
        // what the durable filing is for.
        let _ = app.select_worktree("/repo/worktree".into(), false);
        assert!(!app.workspace.order().any(|card| card == adopted[0].id));
        let _ = app.select_worktree("/repo".into(), false);
        assert!(app.workspace.order().any(|card| card == adopted[0].id));
    }

    fn card_terminal(app: &Cagents, card: CardId) -> TerminalId {
        match &app.card_state(card).expect("card exists").kind {
            CardKind::Terminal(session) => session.id,
            _ => panic!("adopted card is a terminal"),
        }
    }
}
