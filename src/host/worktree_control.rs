//! First-class authenticated worktree operations. Blocking Git/copy/SQL never
//! borrows HostState or holds the global control writer.
use super::{
    agent_execution,
    state::{ClientId, SharedHost},
};
use crate::{
    agent_execution::{Authority, HostedSession, Origin},
    model::{ProjectKind, TerminalId},
    persistence::StateStore,
    repository::{CancellationToken, RepositoryService},
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
    workspace_model::ModelEdit,
    worktree_control::{
        Operation, Reply,
        service::{self, Execution},
    },
};
use std::{
    os::unix::fs::MetadataExt,
    path::{Path, PathBuf},
    sync::Arc,
};

fn refused(error: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, error, false)
}

fn not_started(error: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::WorktreeNotStarted, error, false)
}

/// RAII ownership outside global admission. Registration happens under a short
/// read fence; shutdown/upgrade's writer sees this until publication finishes.
pub(super) struct Active {
    host: SharedHost,
}
impl Active {
    fn enter(host: &SharedHost) -> Result<Self, ProtocolError> {
        super::client::ensure_control_open(host)?;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        state.worktree_mutations = state
            .worktree_mutations
            .checked_add(1)
            .ok_or_else(|| refused("Too many active worktree operations"))?;
        Ok(Self { host: host.clone() })
    }
}
impl Drop for Active {
    fn drop(&mut self) {
        let mut state = self.host.lock().unwrap_or_else(|p| p.into_inner());
        state.worktree_mutations -= 1;
    }
}

/// Exact pending pane membership, observed from the already-applied model.
/// No SQL or filesystem work runs in this HostState cancellation hook.
pub(super) struct SetupStart {
    scope: crate::workspace_model::views::Scope,
    pane: crate::workspace_model::views::Pane,
    withdrawn: Arc<std::sync::atomic::AtomicBool>,
    authority: crate::terminal::holder::setup::Authority,
}
impl SetupStart {
    pub(super) fn observe(&self, model: &crate::workspace_model::WorkspaceModel) {
        let present = model
            .views
            .iter()
            .find(|view| view.scope == self.scope)
            .is_some_and(|view| {
                view.panes.iter().any(|pane| {
                    pane.id == self.pane.id
                        && pane.scope == self.pane.scope
                        && match (&pane.kind, &self.pane.kind) {
                            (
                                crate::model::CardKind::Terminal(actual),
                                crate::model::CardKind::Terminal(expected),
                            ) => actual.id == expected.id && actual.launch == expected.launch,
                            _ => false,
                        }
                })
            });
        if !present {
            // Linearize withdrawal against the exact same commit CAS.
            // HostState only cancels; the original startup guard publishes
            // its resulting no-launch disposition outside this lock.
            let _ = self.authority.cancel_before_commit();
            self.withdrawn
                .store(true, std::sync::atomic::Ordering::Release);
        }
    }
}
struct SetupPaneReservation {
    host: SharedHost,
    pane: crate::model::CardId,
    withdrawn: Arc<std::sync::atomic::AtomicBool>,
}
impl Drop for SetupPaneReservation {
    fn drop(&mut self) {
        let mut state = self.host.lock().unwrap_or_else(|p| p.into_inner());
        if state
            .setup_starts
            .get(&self.pane)
            .is_some_and(|start| Arc::ptr_eq(&start.withdrawn, &self.withdrawn))
        {
            state.setup_starts.remove(&self.pane);
        }
    }
}

enum LiveHandle {
    Chat(crate::chat::runtime::Handle),
    Terminal(crate::terminal::TerminalHandle),
}
impl LiveHandle {
    fn pin(
        &self,
    ) -> Result<
        (
            crate::agent_execution::DirectoryIdentity,
            Arc<std::fs::File>,
        ),
        String,
    > {
        match self {
            Self::Chat(handle) => handle.pinned_launch_directory(),
            Self::Terminal(handle) => handle.pinned_launch_directory(),
        }
    }
    fn current(&self, host: &SharedHost, id: TerminalId) -> bool {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        match self {
            Self::Chat(handle) => state
                .chats
                .get(&id)
                .is_some_and(|record| record.handle.same_actor(handle)),
            Self::Terminal(handle) => state
                .terminals
                .get(&id)
                .is_some_and(|record| record.handle.same_worker(handle)),
        }
    }
}
struct Caller {
    authority: Authority,
    origin: Origin,
    issuing: Option<crate::model::TerminalId>,
    source: Option<crate::model::LaunchRecipe>,
    accepted_project: Option<PathBuf>,
    accepted_agent: Option<crate::model::AgentId>,
    accepted_target: Option<crate::agent_execution::Target>,
    accepted_placement: Option<crate::agent_execution::Placement>,
    root: Option<crate::fs_safety::Root>,
    common: Option<crate::fs_safety::Root>,
    handle: Option<LiveHandle>,
    pin: Option<(
        crate::agent_execution::DirectoryIdentity,
        Arc<std::fs::File>,
    )>,
}
fn same_named(root: &crate::fs_safety::Root) -> Result<(), String> {
    let held = root.metadata().map_err(|e| e.to_string())?;
    let actual = std::fs::symlink_metadata(root.path()).map_err(|e| e.to_string())?;
    if !actual.is_dir() || (held.dev(), held.ino()) != (actual.dev(), actual.ino()) {
        return Err(
            "Original repository authority was replaced; no new effects are authorized".into(),
        );
    }
    Ok(())
}
fn same_directory(
    left: &crate::fs_safety::Root,
    right: &crate::fs_safety::Root,
) -> Result<bool, String> {
    let left = left.metadata().map_err(|e| e.to_string())?;
    let right = right.metadata().map_err(|e| e.to_string())?;
    Ok((left.dev(), left.ino()) == (right.dev(), right.ino()))
}
/// Bind to the accepted registered project at admission. This is not a claim
/// about a historical Git-common inode at provider launch: Root1770 attests
/// the launch directory only. Once accepted, both roots remain held and named.
fn verify_project_scope(accepted: &Path, selected: &crate::fs_safety::Root) -> Result<(), String> {
    // Agent targets are accepted canonical project registry keys (the Git
    // common directory), not a fresh interpretation of a checkout's .git.
    let expected = crate::fs_safety::Root::open(accepted).map_err(|error| error.to_string())?;
    same_named(&expected)?;
    same_named(selected)?;
    if !same_directory(&expected, selected)? {
        return Err(
            "The selected repository is outside this agent's accepted project scope".into(),
        );
    }
    Ok(())
}

impl Caller {
    fn resolve(host: &SharedHost, client: ClientId, authority: Authority) -> Result<Self, String> {
        let (origin, issuing) =
            agent_execution::terminals::worktree_caller(host, client, &authority)
                .map_err(|error| error.message)?;
        let source = {
            let registry = agent_execution::registry(host)?;
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            registry
                .agent_authority_source(&authority)?
                .map(|source| match source {
                    HostedSession::Chat(source) => source.launch.recipe,
                    HostedSession::Terminal(source) => source.launch,
                })
        };
        if source.is_none() && (origin != Origin::User || authority.token().is_some()) {
            return Err(
                "Worktree operations require the exact live issuing session or the user".into(),
            );
        }
        // The live launch-directory pin is necessary provenance, not project
        // scope. Only a previously accepted managed-agent target grants this
        // new worktree surface to an agent. Legacy scripts keep their existing
        // capabilities but cannot acquire project mutation by changing .git.
        let accepted = if let Some(id) = issuing {
            let registry = agent_execution::registry(host)?;
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            Some(registry.agent_for_session(id)?.ok_or(
                "This issuing session has no accepted project scope for worktree operations; ask the user or an agent already bound to that project to perform this request",
            )?)
        } else {
            None
        };
        let accepted_project = accepted
            .as_ref()
            .map(|agent| agent.spec.target.repository.clone());
        let accepted_agent = accepted.as_ref().map(|agent| agent.id);
        let accepted_target = accepted.as_ref().map(|agent| agent.spec.target.clone());
        let accepted_placement = accepted.as_ref().map(|agent| agent.placement.clone());
        let handle = if let Some(id) = issuing {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            Some(if let Some(record) = state.chats.get(&id) {
                LiveHandle::Chat(record.handle.clone())
            } else if let Some(record) = state.terminals.get(&id) {
                LiveHandle::Terminal(record.handle.clone())
            } else {
                return Err("The exact issuing actor is unavailable".into());
            })
        } else {
            None
        };
        let pin = handle.as_ref().map(LiveHandle::pin).transpose()?;
        let (root, common) = match &source {
            Some(source) => {
                let root = crate::fs_safety::Root::open(&source.working_directory)
                    .map_err(|e| e.to_string())?;
                let actual = crate::agent_execution::DirectoryIdentity::from_metadata(
                    &root.metadata().map_err(|error| error.to_string())?,
                );
                if pin.as_ref().is_none_or(|(expected, _)| *expected != actual) {
                    return Err("The issuing checkout pathname no longer names the provider's original launch directory".into());
                }
                let common = RepositoryService::system("git")
                    .common_dir(&root.process_path(), &CancellationToken::new())
                    .map_err(|e| e.to_string())?;
                (
                    Some(root),
                    Some(crate::fs_safety::Root::open(&common).map_err(|e| e.to_string())?),
                )
            }
            None => (None, None),
        };
        let caller = Self {
            authority,
            origin,
            issuing,
            source,
            accepted_project,
            accepted_agent,
            accepted_target,
            accepted_placement,
            root,
            common,
            handle,
            pin,
        };
        caller.verify(host, client)?;
        Ok(caller)
    }
    fn verify(&self, host: &SharedHost, client: ClientId) -> Result<(), String> {
        let current = agent_execution::terminals::worktree_caller(host, client, &self.authority)
            .map_err(|error| error.message)?;
        if current != (self.origin.clone(), self.issuing) {
            return Err("The issuing worktree caller changed".into());
        }
        if let Some(id) = self.issuing {
            let registry = agent_execution::registry(host)?;
            let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
            let accepted = registry
                .agent_for_session(id)?
                .ok_or("The issuing session no longer has an accepted project scope")?;
            if self.accepted_agent != Some(accepted.id)
                || self.accepted_target.as_ref() != Some(&accepted.spec.target)
                || self.accepted_placement.as_ref() != Some(&accepted.placement)
                || self.accepted_project.as_ref() != Some(&accepted.spec.target.repository)
            {
                return Err("The issuing session's accepted project scope changed".into());
            }
        }
        if let Some(handle) = &self.handle {
            if !self.issuing.is_some_and(|id| handle.current(host, id)) {
                return Err("The issuing actor was replaced".into());
            }
            let current = handle.pin()?;
            if self
                .pin
                .as_ref()
                .is_none_or(|(expected, _)| *expected != current.0)
            {
                return Err("The original issuing launch directory changed".into());
            }
        }
        if let (Some(root), Some(common), Some(_source)) = (&self.root, &self.common, &self.source)
        {
            same_named(root)?;
            same_named(common)?;
            let actual = crate::agent_execution::DirectoryIdentity::from_metadata(
                &root.metadata().map_err(|error| error.to_string())?,
            );
            if self
                .pin
                .as_ref()
                .is_none_or(|(expected, _)| *expected != actual)
            {
                return Err(
                    "Original issuing launch directory no longer matches its retained authority"
                        .into(),
                );
            }
            let selected = RepositoryService::system("git")
                .common_dir(&root.process_path(), &CancellationToken::new())
                .map_err(|e| e.to_string())?;
            let selected = crate::fs_safety::Root::open(&selected).map_err(|e| e.to_string())?;
            if !same_directory(common, &selected)? {
                return Err("The issuing checkout now resolves to another repository".into());
            }
            let accepted = self
                .accepted_project
                .as_ref()
                .ok_or("The issuing session has no accepted project scope")?;
            verify_project_scope(accepted, common)?;
        }
        Ok(())
    }
    fn permits(&self, selected: &crate::fs_safety::Root) -> Result<(), String> {
        same_named(selected)?;
        if let Some(common) = &self.common
            && !same_directory(common, selected)?
        {
            return Err("An agent may manage worktrees only in its issuing repository".into());
        }
        Ok(())
    }
}

/// Root Git mutations in the selected original common descriptor. The durable
/// journal retains ordinary canonical paths, never a process-local /proc path.
/// Every command selecting that common root is pinned, including future Git
/// effect verbs. Checkout status keeps its own worktree authority.
struct PinnedGit {
    common: Arc<crate::fs_safety::Root>,
}
impl crate::process::ProcessExecutor for PinnedGit {
    fn execute(
        &self,
        command: &crate::process::CommandSpec,
        cancellation: &CancellationToken,
    ) -> Result<crate::process::ProcessOutput, crate::process::ProcessFailure> {
        use std::ffi::OsStr;
        let mut anchored = command.clone();
        let at = anchored.arguments.windows(2).position(|pair| {
            pair[0] == OsStr::new("-C") && Path::new(&pair[1]) == self.common.path()
        });
        if let Some(at) = at {
            same_named(&self.common).map_err(|error| {
                crate::process::process_failure(
                    command,
                    crate::process::ProcessFailureKind::Io,
                    &error,
                    false,
                )
            })?;
            anchored.arguments[at + 1] = self.common.process_path().into_os_string();
        }
        let mut output = crate::process::ProcessExecutor::execute(
            &crate::process::SystemProcessExecutor,
            &anchored,
            cancellation,
        )?;
        // Git can report its /proc descriptor locator for these two discovery
        // queries. Canonical durable journal paths must remain ordinary paths;
        // normalize only a successful single-directory result that physically
        // equals this retained common root. Never rewrite arbitrary output.
        if at.is_some()
            && output.success
            && anchored
                .arguments
                .iter()
                .any(|arg| matches!(arg.to_str(), Some("--git-common-dir" | "--git-dir")))
        {
            if let Ok(text) = std::str::from_utf8(&output.stdout) {
                let path = Path::new(text.trim_end_matches('\n'));
                if let Ok(metadata) = std::fs::metadata(path) {
                    if let Ok(held) = self.common.metadata() {
                        if metadata.is_dir()
                            && (metadata.dev(), metadata.ino()) == (held.dev(), held.ino())
                        {
                            use std::os::unix::ffi::OsStrExt;
                            output.stdout = self.common.path().as_os_str().as_bytes().to_vec();
                            output.stdout.push(b'\n');
                        }
                    }
                }
            }
        }
        Ok(output)
    }
}
fn selected(operation: &mut Operation) -> Option<&mut PathBuf> {
    match operation {
        Operation::ResolveProject { .. } => None,
        Operation::ResolveRepository { path } => Some(path),
        Operation::Create { repository, .. }
        | Operation::List { repository }
        | Operation::Remove { repository, .. }
        | Operation::SetupIntents { repository }
        | Operation::ReviewSetup { repository, .. }
        | Operation::ResumeSetup { repository, .. } => Some(repository),
    }
}
fn journal() -> Result<StateStore, String> {
    let dirs = crate::instance::project_dirs().ok_or("The data directory is unavailable")?;
    StateStore::open(
        dirs.data_dir().join("worktree-operations.sqlite3"),
        crate::daemon::read_boot_id().map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())
}
fn live_sessions(host: &SharedHost, path: &Path) -> Result<Vec<TerminalId>, String> {
    let within = |directory: &Path| directory == path || directory.starts_with(path);
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let mut sessions = std::collections::BTreeSet::new();
    for (id, record) in &state.chats {
        if !record.handle.is_completed() && within(&record.session.launch.recipe.working_directory)
        {
            sessions.insert(*id);
        }
    }
    for (id, record) in &state.terminals {
        if !record.handle.is_completed()
            && (within(record.current_directory())
                || within(&record.execution_launch().working_directory))
        {
            sessions.insert(*id);
        }
    }
    Ok(sessions.into_iter().collect())
}

/// Called after the ordinary startup/account/removal admission checks under a
/// short shared fence. The caller drops that fence before this blocking work.
pub(super) fn process(
    host: &SharedHost,
    client: ClientId,
    authority: Authority,
    mut operation: Operation,
    active: Option<Active>,
) -> Result<Reply, ProtocolError> {
    let _active = active;
    let caller = Caller::resolve(host, client, authority).map_err(not_started)?;
    let common = if let Some(path) = selected(&mut operation) {
        let common = RepositoryService::system("git")
            .common_dir(path, &CancellationToken::new())
            .map_err(|e| not_started(e.to_string()))?;
        let root = crate::fs_safety::Root::open(&common).map_err(|e| not_started(e.to_string()))?;
        caller.permits(&root).map_err(not_started)?;
        *path = common;
        Some(Arc::new(root))
    } else {
        None
    };
    caller.verify(host, client).map_err(not_started)?;
    let store = journal().map_err(not_started)?;
    let execution = match &common {
        Some(common) => service::execute(
            &RepositoryService::new(
                "git",
                Arc::new(PinnedGit {
                    common: common.clone(),
                }),
            ),
            operation,
            &store,
            |path| live_sessions(host, path),
        ),
        None => service::execute(
            &RepositoryService::system("git"),
            operation,
            &store,
            |path| live_sessions(host, path),
        ),
    }
    .map_err(refused)?;
    match execution {
        Execution::Remove(removal) => {
            caller.verify(host, client).map_err(refused)?;
            if let Some(common) = &common {
                caller.permits(common).map_err(refused)?;
            }
            let reservation = super::worktree_removal::Reservation::install(host, removal.path())?;
            reservation.ensure_unoccupied(host).map_err(refused)?;
            reservation.ensure_no_processes().map_err(refused)?;
            let result = removal.remove(
                &store,
                |_| reservation.ensure_unoccupied(host),
                || {
                    caller.verify(host, client)?;
                    if let Some(common) = &common {
                        caller.permits(common)?;
                    }
                    reservation.ensure_no_processes()
                },
            );
            // Both success and uncertain Git effects require fresh inventory;
            // a failed response never proves the checkout is unchanged.
            if let Some(common) = &common {
                let _ = notify_inventory(host, client, common.path());
            }
            Ok(match result {
                Ok(removed) => Reply::Removed(removed),
                Err(failure) => Reply::RemovalFailed(failure),
            })
        }
        Execution::Reply { mut reply, setup } => {
            if let Reply::ResolvedRepository { repository } = &reply {
                let resolved = RepositoryService::system("git")
                    .common_dir(repository, &CancellationToken::new())
                    .map_err(|error| refused(error.to_string()))?;
                let resolved = crate::fs_safety::Root::open(&resolved)
                    .map_err(|error| refused(error.to_string()))?;
                caller.permits(&resolved).map_err(refused)?;
            }
            // Preserve a typed committed/uncertain creation even when a later
            // permission/publication step fails. It must never invite replay.
            let post = caller.verify(host, client).and_then(|_| {
                common
                    .as_ref()
                    .map_or(Ok(()), |common| caller.permits(common))
            });
            if let Err(error) = post {
                if matches!(
                    reply,
                    Reply::Created(_)
                        | Reply::CreationFailed(crate::worktree_control::CreationFailure {
                            may_have_effects: true,
                            ..
                        })
                ) {
                    // Pure invalidation of the existing original project does
                    // not reopen it or grant another setup launch after loss
                    // of permission, and must still expose committed effects.
                    if let Some(common) = &common {
                        let _ = notify_inventory(host, client, common.path());
                    }
                }
                if !record_failure(&mut reply, error.clone()) {
                    return Err(refused(error));
                }
                return Ok(reply);
            }
            if let Some(common) = &common
                && matches!(
                    reply,
                    Reply::Created(_)
                        | Reply::CreationFailed(crate::worktree_control::CreationFailure {
                            may_have_effects: true,
                            ..
                        })
                        | Reply::SetupPrepared { .. }
                )
            {
                if let Err(error) = publish_project(host, client, common.path()) {
                    record_failure(
                        &mut reply,
                        format!("Checkout retained; sidebar publication needs review: {error}"),
                    );
                }
            }
            if let Some(prepared) = setup {
                if let Some(recipe) = &prepared.recipe {
                    let terminal = TerminalId::new();
                    let result = match &mut reply {
                        Reply::Created(created) => &mut created.setup,
                        Reply::SetupPrepared { setup, .. } => setup,
                        _ => unreachable!("prepared setup reply"),
                    };
                    if let Some(result) = result {
                        result.attempted_terminal = Some(terminal);
                    }
                    let session = crate::model::TerminalSession {
                        id: terminal,
                        owned_codex_listener: None,
                        recovery_origin: None,
                        predecessor: None,
                        launch: recipe.clone(),
                        transcript_path: None,
                        resume_hint: None,
                        continuity: crate::model::TerminalContinuity::RelaunchAvailable {
                            recipe: recipe.clone(),
                            last_exit: None,
                        },
                    };
                    let mut published_pane = None;
                    let mut pane_reservation = None;
                    let accepted = prepared.custody.start(
                        &RepositoryService::system("git"),
                        &store,
                        terminal,
                        recipe,
                        |authority| {
                            caller.verify(host, client)?;
                            if let Origin::Child { parent } = caller.origin {
                                agent_execution::registry(host)?
                                    .lock()
                                    .unwrap_or_else(|p| p.into_inner())
                                    .own_terminal(
                                        crate::agent_execution::Lineage(terminal),
                                        parent,
                                    )?;
                            }
                            let gate = host
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .control_gate
                                .clone();
                            let _shared = gate.read().unwrap_or_else(|p| p.into_inner());
                            super::client::ensure_control_open(host).map_err(|e| e.message)?;
                            super::account_lifecycle::ensure_launch(
                                host,
                                &recipe.working_directory,
                                &recipe.environment,
                                &crate::account::launch_credential_overrides(recipe),
                                true,
                            )
                            .map_err(|e| e.message)?;
                            super::worktree_removal::ensure_outside(
                                host,
                                &recipe.working_directory,
                            )
                            .map_err(|e| e.message)?;
                            drop(_shared);
                            let common = common
                                .as_ref()
                                .ok_or("Setup has no retained repository authority")?;
                            super::client::ensure_control_open(host)
                                .map_err(|error| error.message)?;
                            published_pane = Some(publish_setup_pane(
                                host,
                                client,
                                common.path(),
                                &session,
                                None,
                            )?);
                            let pane = published_pane.as_ref().expect("published exact setup pane");
                            let withdrawn = Arc::new(std::sync::atomic::AtomicBool::new(false));
                            let registration = gate.read().unwrap_or_else(|p| p.into_inner());
                            super::client::ensure_control_open(host)
                                .map_err(|error| error.message)?;
                            host.lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .setup_starts
                                .insert(
                                    pane.id,
                                    SetupStart {
                                        scope: crate::workspace_model::views::Scope::Worktree {
                                            repository: common.path().to_owned(),
                                            path: recipe.working_directory.clone(),
                                        },
                                        pane: pane.clone(),
                                        withdrawn: withdrawn.clone(),
                                        authority: authority.clone(),
                                    },
                                );
                            pane_reservation = Some(SetupPaneReservation {
                                host: host.clone(),
                                pane: pane.id,
                                withdrawn: withdrawn.clone(),
                            });
                            let mut authority = authority;
                            authority.pane_withdrawal = Some((pane.id, withdrawn));
                            drop(registration);
                            // A close before token registration is seen in this
                            // fresh model read; later closes cancel via the
                            // accepted-edit hook. Neither gate nor HostState
                            // is held through this SQLite/view hydration.
                            let latest = agent_execution::registry(host)?
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .model()?;
                            if let Some(start) = host
                                .lock()
                                .unwrap_or_else(|p| p.into_inner())
                                .setup_starts
                                .get(&pane.id)
                            {
                                start.observe(&latest);
                            }
                            let _shared = gate.read().unwrap_or_else(|p| p.into_inner());
                            super::client::ensure_control_open(host)
                                .map_err(|error| error.message)?;
                            super::state::spawn_reviewed_setup(
                                host,
                                client,
                                session.clone(),
                                authority,
                            )
                            .map(|descriptor| descriptor.session)
                            .map_err(|error| error.message)
                        },
                    );
                    match accepted {
                        Ok(session) => {
                            set_setup_terminal(&mut reply, session.clone());
                            if let Some(common) = &common
                                && let Err(error) = publish_setup_pane(
                                    host,
                                    client,
                                    common.path(),
                                    &session,
                                    published_pane.as_ref(),
                                )
                            {
                                record_failure(
                                    &mut reply,
                                    format!(
                                        "Setup actor {} is retained, but its pane publication needs review: {error}",
                                        session.id
                                    ),
                                );
                            }
                        }
                        Err(error) => {
                            record_failure(
                                &mut reply,
                                format!(
                                    "Setup actor {terminal} was not confirmed: {error}. Keep this checkout and wait for its exact retained holder cleanup before reviewing another run"
                                ),
                            );
                        }
                    }
                }
            }
            Ok(reply)
        }
    }
}
fn record_failure(reply: &mut Reply, message: String) -> bool {
    match reply {
        Reply::Created(created) => {
            created.error.get_or_insert(message);
            true
        }
        Reply::CreationFailed(failure) => {
            failure.error.push_str(&format!("; {message}"));
            true
        }
        Reply::SetupPrepared { error, .. } => {
            error.get_or_insert(message);
            true
        }
        _ => false,
    }
}
fn set_setup_terminal(reply: &mut Reply, terminal: crate::model::TerminalSession) {
    let setup = match reply {
        Reply::Created(created) => &mut created.setup,
        Reply::SetupPrepared { setup, .. } => setup,
        _ => return,
    };
    if let Some(setup) = setup {
        setup.terminal = Some(terminal);
    }
}
fn publish_project(host: &SharedHost, client: ClientId, repository: &Path) -> Result<(), String> {
    let primary = RepositoryService::system("git")
        .managed_primary_checkout(repository, &CancellationToken::new())
        .map_err(|e| e.to_string())?
        .ok_or("The primary checkout is unavailable")?;
    let registry = agent_execution::registry(host)?;
    let (edit, applied) = {
        let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let model = registry.model()?;
        let existing = model
            .projects
            .iter()
            .find(|project| project.repository == repository);
        let edit = ModelEdit::ReopenProject {
            repository: repository.to_owned(),
            name: existing
                .map(|project| project.name.clone())
                .unwrap_or_else(|| crate::workspace_model::repository_display_name(&primary)),
            kind: existing
                .map(|project| project.kind)
                .unwrap_or(ProjectKind::Git),
        };
        let applied = registry.apply(&edit)?;
        (edit, applied)
    };
    // Never acquire Registry/SQLite while HostState is held. Even a no-op
    // project upsert carries the worktree inventory invalidation for watchers.
    if applied.changed {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .announce_model_edit(client, applied.model.sequence, &edit);
    }
    notify_inventory(host, client, repository)
}
fn notify_inventory(host: &SharedHost, client: ClientId, repository: &Path) -> Result<(), String> {
    let registry = agent_execution::registry(host)?;
    let sequence = registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .model()?
        .sequence;
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .announce_model_edit(
            client,
            sequence,
            &ModelEdit::WorktreesChanged {
                repository: repository.to_owned(),
            },
        );
    Ok(())
}

pub(super) fn admit(host: &SharedHost, mutates: bool) -> Result<Option<Active>, ProtocolError> {
    if mutates {
        Active::enter(host).map(Some)
    } else {
        Ok(None)
    }
}

fn publish_setup_pane(
    host: &SharedHost,
    client: ClientId,
    repository: &Path,
    session: &crate::model::TerminalSession,
    before: Option<&crate::workspace_model::views::Pane>,
) -> Result<crate::workspace_model::views::Pane, String> {
    use crate::workspace_model::views::{Change, Pane, Scope};
    let scope = Scope::Worktree {
        repository: repository.to_owned(),
        path: session.launch.working_directory.clone(),
    };
    let pane = Pane {
        id: before
            .map(|pane| pane.id)
            .unwrap_or_else(crate::model::CardId::new),
        title: "Worktree setup".into(),
        user_title: None,
        pending_terminal_start: false,
        width_fraction: 1.0,
        kind: crate::model::CardKind::Terminal(session.clone()),
        scope: Some(session.launch.working_directory.clone()),
    };
    let registry = agent_execution::registry(host)?;
    let (edit, applied) = {
        let mut registry = registry.lock().unwrap_or_else(|p| p.into_inner());
        let mut pane = pane.clone();
        let current = if let Some(before) = before {
            let model = registry.model()?;
            let current = model
                .views
                .iter()
                .find(|view| view.scope == scope)
                .and_then(|view| {
                    view.panes
                        .iter()
                        .find(|candidate| candidate.id == before.id)
                })
                .cloned()
                .ok_or("The original setup pane was withdrawn; it was not reopened")?;
            if !matches!(&current.kind, crate::model::CardKind::Terminal(terminal) if terminal.id == session.id && terminal.launch == session.launch)
            {
                return Err("Setup pane now belongs to another source".into());
            }
            pane.title = current.title.clone();
            pane.user_title = current.user_title.clone();
            pane.width_fraction = current.width_fraction;
            Some(current)
        } else {
            None
        };
        let edit = ModelEdit::ChangeView {
            expected_revision: registry.view_revision(&scope)?,
            scope,
            operation: uuid::Uuid::new_v4(),
            change: current
                .as_ref()
                .map(|before| Change::Update {
                    before: before.clone(),
                    pane: pane.clone(),
                })
                .unwrap_or_else(|| Change::Insert {
                    pane: pane.clone(),
                    before: None,
                }),
        };
        let applied = registry.apply(&edit)?;
        (edit, applied)
    };
    if applied.changed {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .announce_model_edit(client, applied.model.sequence, &edit);
    }
    Ok(pane)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::process::{ProcessExecutor, SystemProcessExecutor};

    #[test]
    fn unbound_script_scope_and_authenticated_user_downgrade_are_refused() {
        // A remote attachment fixture observes identity only: it launches no
        // provider/process and must not be closed or signalled as a process.
        let directory = tempfile::tempdir().unwrap();
        let proof = crate::model::HostSessionProof {
            host_session_id: crate::model::HostSessionId::new("worktree-caller-fixture"),
            host_incarnation: crate::model::HostIncarnation::new("worktree-caller-fixture"),
            boot_id: crate::model::BootId::new("worktree-caller-fixture"),
        };
        let source = crate::chat::Session {
            id: TerminalId::new(),
            launch: crate::chat::Launch {
                provider: crate::agent::Provider::Claude,
                recipe: crate::model::LaunchRecipe {
                    program: "fixture-no-process".into(),
                    arguments: Vec::new(),
                    working_directory: directory.path().to_owned(),
                    environment: Default::default(),
                    unset_environment: Default::default(),
                },
                session_id: None,
            },
            proof: Some(proof.clone()),
            terminal: None,
            owned_codex_listener: None,
            native_continuity: Default::default(),
        };
        let mut registry =
            crate::host::model::Registry::at(&directory.path().join("registry.sqlite3")).unwrap();
        let authority = registry
            .register_agent_authority(&HostedSession::Chat(source.clone()))
            .unwrap();
        let mut state = crate::host::state::HostState::new(proof);
        state.install_model(registry);
        let (handle, _peer) = crate::chat::runtime::Handle::fixture_remote_attachment_pair();
        state.chats.insert(
            source.id,
            crate::host::chat::Record {
                execution_instance: uuid::Uuid::new_v4(),
                console_recipe: None,
                paired_terminal: None,
                session: source,
                handle,
                source_process: None,
                subscribers: Default::default(),
                observations: Default::default(),
                completion_observed: false,
                returning_terminal: false,
            },
        );
        let client = 41;
        state.register_client(client);
        let host = Arc::new(crate::host::state::HostLock::new(state));
        let error = Caller::resolve(&host, client, authority.clone())
            .err()
            .expect("unbound script must not gain project scope");
        assert!(error.contains("no accepted project scope"), "{error}");
        agent_execution::terminals::authenticate(&host, client, &authority).unwrap();
        let error = Caller::resolve(&host, client, Authority::default())
            .err()
            .expect("authenticated socket must not fall back to User");
        assert!(
            error.contains("cannot change its authenticated worktree caller"),
            "{error}"
        );
    }

    #[test]
    fn accepted_project_refuses_an_original_checkout_redirected_to_another_repo() {
        if crate::repository::test_fixture::isolated_fixture_case(concat!(
            module_path!(),
            "::accepted_project_refuses_an_original_checkout_redirected_to_another_repo"
        )) {
            return;
        }
        let fixture = tempfile::tempdir_in("/var/tmp").unwrap();
        let a = fixture.path().join("a");
        let b = fixture.path().join("b");
        let common_a = fixture.path().join("a.git");
        let common_b = fixture.path().join("b.git");
        for (checkout, common) in [(&a, &common_a), (&b, &common_b)] {
            let output = SystemProcessExecutor
                .execute(
                    &crate::worktree_branch::git_command(fixture.path())
                        .args(["init", "-q", "--separate-git-dir"])
                        .arg(common)
                        .arg(checkout)
                        .timeout(std::time::Duration::from_secs(10)),
                    &CancellationToken::new(),
                )
                .unwrap();
            assert!(
                output.success,
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
        let original_directory = crate::fs_safety::Root::open(&a).unwrap();
        let service = RepositoryService::system("git");
        let original_common = service
            .common_dir(
                &original_directory.process_path(),
                &CancellationToken::new(),
            )
            .unwrap();
        let held = crate::fs_safety::Root::open(&original_common).unwrap();
        verify_project_scope(&common_a, &held).unwrap();
        assert!(
            verify_project_scope(&a, &held).is_err(),
            "an accepted Folder key is not Git-common authority"
        );
        let before = original_directory.metadata().unwrap();
        std::fs::write(a.join(".git"), format!("gitdir: {}\n", common_b.display())).unwrap();
        let after = original_directory.metadata().unwrap();
        assert_eq!(
            (before.dev(), before.ino()),
            (after.dev(), after.ino()),
            "the original launch directory itself did not change"
        );
        let redirected = service
            .common_dir(
                &original_directory.process_path(),
                &CancellationToken::new(),
            )
            .unwrap();
        let redirected = crate::fs_safety::Root::open(&redirected).unwrap();
        assert!(
            verify_project_scope(&common_a, &redirected)
                .unwrap_err()
                .contains("accepted project scope")
        );
        assert!(
            verify_project_scope(&a, &redirected).is_err(),
            "redirecting an accepted Folder checkout must not grant its new repository"
        );
        verify_project_scope(&common_a, &held).unwrap();
        let parked = fixture.path().join("parked-a.git");
        std::fs::rename(&common_a, &parked).unwrap();
        std::fs::rename(&common_b, &common_a).unwrap();
        let command =
            crate::worktree_branch::git_command(&common_a).args(["rev-parse", "--git-common-dir"]);
        let failure = PinnedGit {
            common: Arc::new(held),
        }
        .execute(&command, &CancellationToken::new())
        .unwrap_err();
        assert!(
            !failure.started,
            "replacement common must be rejected before Git starts"
        );
    }
}
