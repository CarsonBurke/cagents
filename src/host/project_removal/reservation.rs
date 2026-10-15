//! A removal reserves logical ownership, rather than freezing unrelated I/O.
use super::*;
use crate::{model::{CardId, ChannelId, TerminalId}, workspace_model::{ModelEdit, WorkspaceModel, views::{Change, Pane, Scope}}};
use std::{collections::BTreeMap, path::{Path, PathBuf}, sync::{Arc, Mutex}};

#[derive(Clone)]
pub(crate) struct ReservedProject {
    project: ProjectInstance,
    roots: Vec<PathBuf>,
    protected: Vec<PathBuf>,
    channels: HashSet<ChannelId>,
    panes: HashSet<CardId>,
    sessions: HashSet<TerminalId>,
    protected_sessions: HashSet<TerminalId>,
    infer_directory: bool,
}
pub(crate) type Reservations = Arc<Mutex<BTreeMap<uuid::Uuid, ReservedProject>>>;
pub(crate) struct Reservation { entries: Reservations, token: uuid::Uuid }
impl Reservation {
    pub(super) fn qualifies(&self, source: &HostedSession) -> bool { self.entries.lock().unwrap_or_else(|p| p.into_inner()).get(&self.token).is_some_and(|scope| scope.qualifies(source)) }
}
impl Drop for Reservation {
    fn drop(&mut self) { self.entries.lock().unwrap_or_else(|p| p.into_inner()).remove(&self.token); }
}
impl ReservedProject {
    fn qualifies(&self, source: &HostedSession) -> bool {
        if self.protected_sessions.contains(&source.id()) { return false; }
        let directory = match source { HostedSession::Chat(source) => &source.launch.recipe.working_directory, HostedSession::Terminal(source) => &source.launch.working_directory };
        self.sessions.contains(&source.id()) || (self.infer_directory && self.directory(directory))
    }
    fn scope(&self, scope: &Scope) -> bool {
        match scope { Scope::Channel(channel) => self.channels.contains(channel), Scope::Worktree { repository, path } => *repository == self.project.repository || self.directory(path) }
    }
    fn directory(&self, path: &Path) -> bool {
        let depth = |roots: &[PathBuf]| roots.iter().filter(|root| path.starts_with(root)).map(|root| root.components().count()).max();
        match (depth(&self.roots), depth(&self.protected)) { (Some(own),Some(other)) => own > other, (Some(_),None) => true, _ => false }
    }
    fn pane(&self, pane: &Pane) -> bool {
        self.panes.contains(&pane.id) || pane.scope.as_deref().is_some_and(|path| self.directory(path)) || match &pane.kind {
            crate::model::CardKind::Terminal(session) => self.sessions.contains(&session.id) || self.directory(&session.launch.working_directory),
            crate::model::CardKind::Chat(session) => self.sessions.contains(&session.id) || self.directory(&session.launch.recipe.working_directory),
            crate::model::CardKind::Repository(view) => view.repository == self.project.repository || self.directory(&view.worktree),
            crate::model::CardKind::PullRequests(view) => view.repository == self.project.repository,
            _ => false,
        }
    }
    fn edit(&self, edit: &ModelEdit) -> bool {
        match edit {
            ModelEdit::OpenProject { repository, .. } | ModelEdit::ReopenProject { repository, .. } | ModelEdit::CloseProject { repository }
            | ModelEdit::ResetNavigation { repository } | ModelEdit::ReorderNavigation { repository, .. } | ModelEdit::CreateChannel { repository, .. } => *repository == self.project.repository || self.directory(repository),
            ModelEdit::CloseProjectInstance { project } => project.registry == self.project.registry && project.ordinal == self.project.ordinal,
            ModelEdit::RelocateProject { relocation } => relocation.repository == self.project.repository || relocation.destination == self.project.repository || self.directory(&relocation.old_root) || self.directory(&relocation.new_root),
            ModelEdit::RetitleChannel { id, .. } | ModelEdit::SuggestChannelTitle { id, .. } | ModelEdit::DeleteChannel { id } => self.channels.contains(id),
            ModelEdit::SeedView { scope, panes, .. } => self.scope(scope) || panes.iter().any(|pane| self.pane(pane)),
            ModelEdit::ChangeView { scope, change, .. } => self.scope(scope) || match change {
                Change::Transfer { id, destination, .. } => self.panes.contains(id) || self.scope(destination),
                Change::Insert { pane, .. } | Change::InsertIfEmpty { pane } => self.pane(pane),
                Change::Update { before, pane } => self.pane(before) || self.pane(pane),
                Change::Remove { id } | Change::Move { id, .. } => self.panes.contains(id),
            },
            ModelEdit::AgentsChanged => false,
        }
    }
}
fn entries(host: &SharedHost) -> Reservations { host.lock().unwrap_or_else(|p| p.into_inner()).project_removals.clone() }
fn scopes(host: &SharedHost) -> Vec<ReservedProject> { entries(host).lock().unwrap_or_else(|p| p.into_inner()).values().cloned().collect() }
fn busy() -> ProtocolError { ProtocolError::new(ProtocolErrorCode::InvalidRequest, "This project is being removed. Wait for its removal to finish, or retry after resolving its failure.", true) }

/// Caller holds short exclusive control admission. No slow teardown occurs here.
pub(crate) fn reserve(host: &SharedHost, project: &ProjectInstance, selected: &[SessionClose]) -> Result<Reservation,ProtocolError> {
    let registry = super::super::agent_execution::registry(host).map_err(error)?;
    let model = { let registry = registry.lock().unwrap_or_else(|p| p.into_inner()); registry.require_project_instance(project).map_err(error)?; registry.model().map_err(error)? };
    let mut reserved = ReservedProject { project: project.clone(), roots: Vec::new(), protected: Vec::new(), channels: model.channels.iter().filter(|channel| channel.repository == project.repository).map(|channel| channel.id).collect(), panes: HashSet::new(), sessions: HashSet::new(), protected_sessions: HashSet::new(), infer_directory: !model.projects.iter().any(|other| other.repository != project.repository && other.kind == crate::model::ProjectKind::Git) };
    for current in &model.projects {
        let roots = if current.repository == project.repository { &mut reserved.roots } else { &mut reserved.protected };
        roots.extend(current.primary_checkout.iter().chain(current.checkout.iter()).cloned());
        if current.kind == crate::model::ProjectKind::Folder { roots.push(current.repository.clone()); }
        else if current.repository.file_name().is_some_and(|name| name == ".git") { if let Some(root) = current.repository.parent() { roots.push(root.to_path_buf()); } }
    }
    for view in &model.views {
        let own = match &view.scope { Scope::Channel(channel) => reserved.channels.contains(channel), Scope::Worktree { repository, .. } => *repository == project.repository };
        if let Scope::Worktree { path, .. } = &view.scope { if own { reserved.roots.push(path.clone()); } else { reserved.protected.push(path.clone()); } }
        for pane in &view.panes {
            if own { reserved.panes.insert(pane.id); }
            let ids = if own { &mut reserved.sessions } else { &mut reserved.protected_sessions };
            match &pane.kind {
                crate::model::CardKind::Terminal(session) => { ids.insert(session.id); },
                crate::model::CardKind::Chat(session) => { ids.insert(session.id); if let Some(terminal) = &session.terminal { ids.insert(terminal.id); } },
                _ => {},
            }
        }
    }
    for agent in &model.agents {
        let location = model.agent_locations.iter().find(|location| location.agent == agent.id);
        let repository = location.map(|location| &location.repository).unwrap_or(&agent.spec.target.repository);
        if *repository == project.repository {
            reserved.panes.insert(CardId::from_uuid(agent.id.as_uuid()));
            if let Some(source) = &agent.session { reserved.sessions.insert(source.id()); }
            if let Some(console) = &agent.console { reserved.sessions.insert(console.id); }
        } else {
            reserved.protected.push(location.map(|location| location.logical_directory().to_owned()).unwrap_or_else(|| agent.spec.target.directory.clone()));
            if let Some(source) = &agent.session { reserved.protected_sessions.insert(source.id()); }
            if let Some(console) = &agent.console { reserved.protected_sessions.insert(console.id); }
        }
    }
    // Another Git project may own external linked roots absent from this
    // snapshot. In that case only explicit shared membership can authorize
    // teardown; a guessed path prefix must never broaden ownership.
    if selected.iter().any(|selected| !reserved.qualifies(&selected.source)) { return Err(error("A selected session does not belong to this project's verified shared membership or known directory scope. No sessions were closed; close an unregistered pane explicitly before removing the project.")); }
    reserved.sessions.extend(selected.iter().map(|selected| selected.source.id()));
    let entries = entries(host); let token = uuid::Uuid::new_v4();
    { let mut entries = entries.lock().unwrap_or_else(|p| p.into_inner()); if entries.values().any(|active| active.project == *project) { return Err(busy()); } entries.insert(token,reserved); }
    Ok(Reservation { entries, token })
}

/// Host-only entry points call this before replacing/stopping a source or
/// starting work. The existing control admission must span check and effect.
pub(crate) fn ensure_available(host: &SharedHost, repository: Option<&Path>, directory: Option<&Path>, session: Option<TerminalId>) -> Result<(), ProtocolError> {
    let scopes = scopes(host); if scopes.is_empty() { return Ok(()); }
    let directory = directory.map(|path| path.canonicalize().unwrap_or_else(|_| path.to_owned()));
    if scopes.iter().any(|scope| repository.is_some_and(|repository| repository == scope.project.repository) || directory.as_deref().is_some_and(|directory| scope.directory(directory)) || session.is_some_and(|session| scope.sessions.contains(&session))) { return Err(busy()); }
    Ok(())
}

pub(crate) fn ensure_agent_available(host: &SharedHost, id: crate::agent_execution::AgentId) -> Result<(),ProtocolError> {
    if scopes(host).is_empty() { return Ok(()); }
    let registry = super::super::agent_execution::registry(host).map_err(error)?;
    let (repository, session) = { let registry = registry.lock().unwrap_or_else(|p| p.into_inner()); let agent = registry.agent(id).map_err(error)?; (registry.agent_location(id).map_err(error)?.repository, agent.session.as_ref().map(HostedSession::id)) };
    ensure_available(host,Some(&repository),None,session)
}

/// Called after ordinary control admission and before any public request effect.
pub(crate) fn admit(host: &SharedHost, request: &ClientRequest) -> Result<(), ProtocolError> {
    let active = scopes(host); if active.is_empty() { return Ok(()); }
    let session = match request {
        ClientRequest::Input { terminal_id, .. } | ClientRequest::Paste { terminal_id, .. } | ClientRequest::Key { terminal_id, .. }
        | ClientRequest::ShutdownTerminal { terminal_id } | ClientRequest::CloseTerminalChecked { terminal_id, .. } | ClientRequest::ChatReturnTerminal { chat_id: terminal_id }
        | ClientRequest::ChatCommitTerminal { terminal_id } | ClientRequest::ChatClose { chat_id: terminal_id } | ClientRequest::CompletePendingTerminal { terminal_id, .. } => Some(*terminal_id),
        ClientRequest::CloseTerminalSourceChecked { source, .. } | ClientRequest::ReleaseCompletedTerminal { session: source, .. } => Some(source.id),
        ClientRequest::ChatCommand { chat_id, command } if !matches!(command, crate::chat::Command::Refresh | crate::chat::Command::LoadEarlier) => Some(*chat_id),
        ClientRequest::ChatCloseRetained { session } | ClientRequest::ChatCloseSession {session} => Some(session.id),
        _ => None,
    };
    if session.is_some_and(|id| active.iter().any(|scope| scope.sessions.contains(&id))) { return Err(busy()); }
    match request {
        ClientRequest::EditWorkspaceModel { edit } => {
            if let ModelEdit::ChangeView { operation, .. } = edit {
                let registry = super::super::agent_execution::registry(host).map_err(error)?;
                if registry.lock().unwrap_or_else(|p| p.into_inner()).view_operation_seen(*operation).map_err(error)? { return Ok(()); }
            }
            let edit = edit.clone().canonicalized().map_err(error)?;
            if active.iter().any(|scope| scope.edit(&edit)) { return Err(busy()); }
        },
        ClientRequest::Spawn { session, .. } => ensure_available(host,None,Some(&session.launch.working_directory),Some(session.id))?,
        // ChatAttach may restart a completed provider. Defer matching
        // attachments for this short reservation rather than guessing whether
        // the retained backend has completed while this admission is checked.
        ClientRequest::ChatAttach {session} | ClientRequest::ChatStart { session, .. } | ClientRequest::ChatStartSelected { session, .. } => ensure_available(host,None,Some(&session.launch.recipe.working_directory),Some(session.id))?,
        ClientRequest::ChatRecoverAccount { source, replacement, .. } => { ensure_available(host,None,Some(&source.launch.recipe.working_directory),Some(source.id))?; ensure_available(host,None,Some(&replacement.launch.recipe.working_directory),Some(replacement.id))?; },
        ClientRequest::DeviceOperation { operation: crate::device_service::Operation::Write { root, .. } } => ensure_available(host,Some(root),Some(root),None)?,
        ClientRequest::DeviceOperation { operation: crate::device_service::Operation::Git { root, operation } } if !matches!(operation, crate::device_service::GitOperation::Status | crate::device_service::GitOperation::Worktrees | crate::device_service::GitOperation::Diff { .. }) => ensure_available(host,Some(root),Some(root),None)?,
        ClientRequest::DeviceOperation { operation: crate::device_service::Operation::RemoveMergedWorktree { request } } => ensure_available(host,Some(&request.repository),Some(&request.path),None)?,
        _ => {},
    }
    Ok(())
}
