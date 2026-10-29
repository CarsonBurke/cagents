//! What a connection that authenticated as an agent session may do. It reads
//! every terminal, but changes only those its lineage controls: its own
//! session, the terminals it started and the sessions of the agents it
//! delegated to. Anything outside the terminal surface stays with the user.
//!
//! The boundary is the `cagents` CLI, which authenticates before it changes a
//! terminal from inside a hosted session. A process that reaches the socket
//! without doing so acts for the user, as everywhere else.
use super::registry;
use crate::{
    agent_execution::{Authority, Lineage, Origin},
    host::state::{ClientId, SharedHost},
    model::{TerminalId, TerminalSession},
    terminal_ipc::{ClientRequest, ProtocolError, ProtocolErrorCode},
};

enum Access<'a> {
    /// Reads, or presentation private to this connection.
    Open,
    /// Changes a terminal that other connections share.
    Control(TerminalId),
    Spawn(&'a TerminalSession),
    /// Belongs to the user alone, or carries an authority of its own.
    Refused,
}

/// Explicit scope for retained setup metadata only. Other requests retain
/// their existing connection policy. A delegation attempt is sticky even if
/// its capability is refused; retrying without it must not become User.
#[derive(Clone, Copy)]
pub(in crate::host) enum AccountPreparationCaller {
    User,
    DelegatedAttempt,
}

/// Classified without a fallback arm, so every new request has to choose.
fn access(request: &ClientRequest) -> Access<'_> {
    use ClientRequest as R;
    match request {
        R::Connect { .. }
        | R::Authenticate { .. }
        | R::BuildInfo
        | R::List
        | R::WatchTerminals
        | R::WatchWorkspaceModel
        | R::ReadWorkspaceModel
        | R::Attach { .. }
        | R::Detach { .. }
        | R::SetVisibility { .. }
        | R::Hibernate { .. }
        | R::Resume { .. }
        | R::Snapshot { .. }
        | R::AcknowledgeRedraw { .. }
        | R::TerminalAcknowledge { .. }
        | R::SearchTerminal { .. }
        | R::CopyTerminalSelection { .. }
        | R::Work(
            crate::work::Request::Board { .. }
            | crate::work::Request::RemovalPlan { .. }
            | crate::work::Request::Snapshots { .. },
        )
        | R::WatchWork => Access::Open,
        R::Input { terminal_id, .. }
        | R::Paste { terminal_id, .. }
        | R::Key { terminal_id, .. }
        | R::Mouse { terminal_id, .. }
        | R::Focus { terminal_id, .. }
        | R::Scroll { terminal_id, .. }
        | R::Resize { terminal_id, .. }
        | R::SetTheme { terminal_id, .. }
        | R::ClearTerminalScrollback { terminal_id }
        | R::ShutdownTerminal { terminal_id }
        | R::CloseTerminalChecked { terminal_id, .. } => Access::Control(*terminal_id),
        R::ReleaseCompletedTerminal { session, .. } => Access::Control(session.id),
        R::Spawn { session, .. } => Access::Spawn(session),
        R::CompletePendingTerminal { .. }
        | R::DeviceOperation { .. }
        | R::Agent { .. }
        | R::Automation { .. }
        | R::ChannelWork { .. }
        | R::Shutdown
        | R::ShutdownV1 { .. }
        | R::PrepareUpgrade { .. }
        | R::UpgradeImpact { .. }
        | R::PrepareConfirmedUpgrade { .. }
        | R::ResolveUpgradeChat { .. }
        | R::EditWorkspaceModel { .. }
        | R::RefreshAgentActivity { .. }
        | R::ObserveReviewRequests { .. }
        | R::WatchNotifications { .. }
        | R::NotificationPresence { .. }
        | R::ReadNotifications
        | R::NotificationPreferencesChanged { .. }
        | R::AcknowledgeNotification { .. }
        | R::SendTestNotification
        | R::ChatStart { .. }
        | R::ChatRecoverAccount { .. }
        | R::ChatAttach { .. }
        | R::ChatDetach { .. }
        | R::ChatCommand { .. }
        | R::ChatVisibility { .. }
        | R::ChatRead { .. }
        | R::ChatClose { .. }
        | R::ChatCloseRetained { .. }
        | R::ChatCapacityInspect
        | R::ChatCapacitySet { .. }
        | R::ChatReleaseInspect { .. }
        | R::ChatRelease { .. }
        | R::ChatList
        | R::ChatAcknowledge { .. }
        | R::ChatPreview { .. }
        | R::ChatCandidates { .. }
        | R::ChatStartSelected { .. }
        | R::ChatReturnTerminal { .. }
        | R::ChatCommitTerminal { .. }
        | R::ChatPrepareResume { .. }
        | R::Work(crate::work::Request::Pin { .. } | crate::work::Request::Remove(_)) => {
            Access::Refused
        }
    }
}

/// Binds the lineage `authority` was issued to to the connection. Without a
/// token the connection goes on acting for the user; a connection that acts
/// for an agent never acts for anyone else.
pub(in crate::host) fn authenticate(
    host: &SharedHost,
    client_id: ClientId,
    authority: &Authority,
) -> Result<(), ProtocolError> {
    if authority.token().is_none() {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.clients.contains_key(&client_id) && !state.agent_clients.contains_key(&client_id) {
            state
                .account_preparation_callers
                .entry(client_id)
                .or_insert(AccountPreparationCaller::User);
        }
        return Ok(());
    }
    {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.clients.contains_key(&client_id) {
            state
                .account_preparation_callers
                .insert(client_id, AccountPreparationCaller::DelegatedAttempt);
        }
    }
    let lineage = match super::origin(host, authority).map_err(denied)? {
        Origin::Child { parent } => parent,
        Origin::User | Origin::Automation { .. } => {
            return Err(denied(
                "Only an agent session's capability can authenticate",
            ));
        }
    };
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    match state.agent_clients.get(&client_id) {
        Some(bound) if *bound != lineage => Err(denied(
            "This connection already acts for another agent session",
        )),
        _ => {
            state.agent_clients.insert(client_id, lineage);
            Ok(())
        }
    }
}

/// Refuses what the agent session a connection acts for may not do.
pub(in crate::host) fn admit(
    host: &SharedHost,
    client_id: ClientId,
    request: &ClientRequest,
) -> Result<(), ProtocolError> {
    if matches!(
        request,
        ClientRequest::DeviceOperation {
            operation: crate::device_service::Operation::AccountPreparation { .. }
        }
    ) {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        return if state.clients.contains_key(&client_id)
            && !state.agent_clients.contains_key(&client_id)
            && matches!(
                state.account_preparation_callers.get(&client_id),
                Some(AccountPreparationCaller::User)
            ) {
            Ok(())
        } else {
            Err(denied(
                "Account preparation requires this connection's explicit User scope",
            ))
        };
    }
    let Some(lineage) = acting_for(host, client_id) else {
        return Ok(());
    };
    match access(request) {
        Access::Open => Ok(()),
        Access::Refused => Err(denied(
            "An agent session's connection may only read terminals and change its own",
        )),
        Access::Spawn(session) => fresh(host, session),
        Access::Control(terminal) => {
            if controls(host, lineage, terminal).map_err(internal)? {
                Ok(())
            } else {
                Err(denied(format!(
                    "Terminal {terminal} belongs to another session; message it with `cagents send` instead"
                )))
            }
        }
    }
}

/// Records that the agent session a connection acts for starts `terminal`,
/// before it exists, so the agent controls it from its first output.
pub(in crate::host) fn adopt(
    host: &SharedHost,
    client_id: ClientId,
    terminal: TerminalId,
) -> Result<(), ProtocolError> {
    let Some(owner) = acting_for(host, client_id) else {
        return Ok(());
    };
    registry(host)
        .map_err(internal)?
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .own_terminal(Lineage(terminal), owner)
        .map_err(internal)
}

fn acting_for(host: &SharedHost, client_id: ClientId) -> Option<Lineage> {
    host.lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_clients
        .get(&client_id)
        .copied()
}

/// An agent may start only a new terminal. One that names a predecessor or an
/// ID some lineage already used would join, and so take over, that lineage.
fn fresh(host: &SharedHost, session: &TerminalSession) -> Result<(), ProtocolError> {
    if session.predecessor.is_some()
        || session.recovery_origin.is_some()
        || session.owned_codex_listener.is_some()
    {
        return Err(denied(
            "An agent session may start a new terminal but not relaunch one",
        ));
    }
    let registry = registry(host).map_err(internal)?;
    if registry
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .lineage_recorded(session.id)
        .map_err(internal)?
    {
        return Err(ProtocolError::new(
            ProtocolErrorCode::TerminalAlreadyExists,
            format!("terminal {} was already used", session.id),
            false,
        ));
    }
    Ok(())
}

/// Whether `lineage` may change `terminal`: it is that lineage's own session,
/// one it started, or the session or console of an agent it delegated to.
fn controls(host: &SharedHost, lineage: Lineage, terminal: TerminalId) -> Result<bool, String> {
    let registry = registry(host)?;
    let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let target = registry
        .session_lineage(terminal)?
        .unwrap_or(Lineage(terminal));
    if target == lineage || registry.terminal_owner(target)? == Some(lineage) {
        return Ok(true);
    }
    for agent in registry.agents(Some(&Origin::Child { parent: lineage }))? {
        let sessions = agent
            .session
            .as_ref()
            .map(|session| session.id())
            .into_iter()
            .chain(agent.console.as_ref().map(|console| console.id));
        for session in sessions {
            if session == terminal
                || registry
                    .session_lineage(session)?
                    .unwrap_or(Lineage(session))
                    == target
            {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn denied(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::PermissionDenied, message, false)
}

fn internal(message: String) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::Internal, message, false)
}
