//! What a connection that authenticated as an agent session may do. It reads
//! every terminal, but changes only those its lineage controls: its own
//! session, the terminals it started and the sessions of the agents it
//! delegated to. Typing into a delegated agent's session, an agent console or
//! a provider CLI acts as the person at it, who can approve tool requests, so
//! only an agent whose own approvals are not limited may do it. By the same
//! rule it may move the panes of the sessions it controls into another
//! channel of their project, making that channel for the move if it asks.
//! Anything else outside the terminal surface stays with the user.
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
    /// Sends input to a terminal's program, as a person typing, pasting or
    /// clicking at it would.
    Input(TerminalId),
    Spawn(&'a TerminalSession),
    /// Moves a pane to another channel. The handler authorizes the pane it
    /// reads, at the view revision it moves it at, with [`authorize_move`].
    Place,
    /// Belongs to the user alone, or carries an authority of its own.
    Refused,
}

/// Classified without a fallback arm, so every new request has to choose.
fn access(request: &ClientRequest) -> Access<'_> {
    use ClientRequest as R;
    match request {
        R::Connect { .. }
        | R::Authenticate { .. }
        | R::BuildInfo
        | R::StartupState
        | R::List
        | R::ListPage { .. }
        | R::ListResidentTerminals
        | R::WatchTerminals
        | R::WatchWorkspaceModel
        | R::ReadWorkspaceModel
        | R::ReadWorkspaceView { .. }
        | R::ReadWorkspaceSnapshot
        | R::ReadWorkspaceMetadata { .. }
        | R::ReadTerminalRemovalBasis { .. }
        | R::ReadCapturedTerminalRemovalBasis { .. }
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
        | R::WatchWork
        // A linked computer's session resolves paths on this device as it
        // starts agents; nothing else of a device's files is open to it.
        | R::DeviceOperation {
            operation:
                crate::device_service::Operation::Home
                | crate::device_service::Operation::Discover { .. },
        }
        | R::PeerChallenge
        | R::PeerAuthenticate { .. }
        // Admitted only so its handler can explain that it was removed.
        | R::RetiredChannelWork => Access::Open,
        R::Input { terminal_id, .. }
        | R::Paste { terminal_id, .. }
        | R::Key { terminal_id, .. }
        | R::Mouse { terminal_id, .. } => Access::Input(*terminal_id),
        R::Focus { terminal_id, .. }
        | R::Scroll { terminal_id, .. }
        | R::Resize { terminal_id, .. }
        | R::SetTheme { terminal_id, .. }
        | R::ClearTerminalScrollback { terminal_id }
        | R::ShutdownTerminal { terminal_id }
        | R::CloseTerminalChecked { terminal_id, .. } => Access::Control(*terminal_id),
        R::ReleaseCompletedTerminal { session, .. } | R::CloseTerminalSource { session, .. } => {
            Access::Control(session.id)
        }
        R::Spawn { session, .. } => Access::Spawn(session),
        R::MovePane { .. } => Access::Place,
        R::CompletePendingTerminal { .. }
        | R::RetryPendingTerminalSave { .. }
        | R::DeviceOperation { .. }
        | R::DeviceTrust { .. }
        | R::SignDeviceChallenge { .. }
        | R::Agent { .. }
        | R::Automation { .. }
        | R::Shutdown
        | R::ShutdownV1 { .. }
        | R::PrepareUpgrade { .. }
        | R::UpgradeImpact { .. }
        | R::PrepareConfirmedUpgrade { .. }
        | R::ResolveUpgradeChat { .. }
        | R::EditWorkspaceModel { .. }
        | R::ApplyWorkspaceEdit { .. }
        | R::RefreshAgentActivity { .. }
        | R::ObserveReviewRequests { .. }
        | R::WatchNotifications { .. }
        | R::NotificationPresence { .. }
        | R::ReadNotifications
        | R::NotificationPreferencesChanged { .. }
        | R::AcknowledgeNotification { .. }
        | R::SendTestNotification
        | R::ChatStart { .. }
        | R::ChatStartInPane { .. }
        | R::ChatRecoverAccount { .. }
        | R::ChatAttach { .. }
        | R::ChatDetach { .. }
        | R::ChatCommand { .. }
        | R::ChatDeliver { .. }
        | R::ChatVisibility { .. }
        | R::ChatRead { .. }
        | R::ChatClose { .. }
        | R::ChatCloseRetained { .. }
        | R::ChatLostProcesses { .. }
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
        | R::WatchConversationPullRequests { .. }
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
        return Ok(());
    }
    // A peer capability acts only on the connection whose proof it answered.
    if super::super::device_trust::is_peer_capability(authority)
        && !host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .peers
            .issued_to(authority.token().unwrap_or_default(), client_id)
    {
        return Err(denied(
            "This device capability was issued to another connection",
        ));
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
            state
                .agent_client_authorities
                .insert(client_id, authority.clone());
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
    let Some(lineage) = acting_for(host, client_id) else {
        return Ok(());
    };
    match access(request) {
        Access::Open => Ok(()),
        Access::Refused if own_peer_request(host, client_id, request) => Ok(()),
        Access::Refused => Err(denied(
            "An agent session's connection may only read terminals and change its own",
        )),
        Access::Spawn(session) => fresh(host, session),
        Access::Place => Ok(()),
        Access::Control(terminal) | Access::Input(terminal) => {
            let control = controls(host, lineage, terminal).map_err(internal)?;
            let Some(control) = control else {
                return Err(denied(format!(
                    "Terminal {terminal} belongs to another session; message it with `cagents send` instead"
                )));
            };
            match access(request) {
                Access::Input(terminal) => typing_into(host, client_id, terminal, control),
                _ => Ok(()),
            }
        }
    }
}

/// Refuses input from an agent whose own approvals are limited into a
/// terminal that reads it as its person's, where whatever it types, such as
/// an `/answer` or a key approving a tool request, could approve work the
/// agent may not approve itself: a delegated agent's session or console, any
/// agent's console, or a provider CLI's terminal, its own included.
fn typing_into(
    host: &SharedHost,
    client_id: ClientId,
    terminal: TerminalId,
    control: Control,
) -> Result<(), ProtocolError> {
    // What it types there, and what it may do instead.
    let (target, instead) = match control {
        Control::Delegated { agent, console } => {
            let name = agent
                .name
                .clone()
                .unwrap_or_else(|| format!("agent {}", agent.id));
            let surface = if console { "console" } else { "session" };
            (
                format!("{name}'s {surface}"),
                "Use `cagents agent message` instead.",
            )
        }
        Control::Own => {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let Some(launch) = state
                .terminals
                .get(&terminal)
                .map(|record| record.execution_launch())
            else {
                return Ok(());
            };
            let target = if crate::agent_execution::console_agent(launch).is_some() {
                "its own console".to_owned()
            } else if let Some((provider, _)) = crate::agent::Provider::launched(launch) {
                format!("{}'s terminal {terminal}", provider.label())
            } else {
                return Ok(());
            };
            (target, "A person can type there in cagents.")
        }
    };
    let authority = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_client_authorities
        .get(&client_id)
        .cloned();
    // A connection acting for an agent without its authority recorded counts
    // as limited, never as a person.
    let ceiling = match authority {
        Some(authority) => super::caller_ceiling(host, &authority).map_err(denied)?,
        None => Some(crate::agent_execution::permission::Ceiling::unknown()),
    };
    if ceiling.as_ref().is_none_or(|ceiling| ceiling.may_approve()) {
        return Ok(());
    }
    Err(denied(format!(
        "This agent can't type into {target} because its approvals are limited. {instead}"
    )))
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

/// What an agent's request to move a pane is about.
pub(in crate::host) enum Mover {
    /// A person's connection, which may move any pane.
    Person,
    /// A connection acting for an agent session, whose live session is
    /// `session`: its own pane is the one showing it.
    Agent {
        lineage: Lineage,
        session: TerminalId,
    },
}

/// Who a connection moves panes for. An agent connection whose issuing
/// session is no longer live moves nothing.
pub(in crate::host) fn mover(
    host: &SharedHost,
    client_id: ClientId,
) -> Result<Mover, ProtocolError> {
    let Some(lineage) = acting_for(host, client_id) else {
        return Ok(Mover::Person);
    };
    let authority = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .agent_client_authorities
        .get(&client_id)
        .cloned()
        .ok_or_else(|| denied("This connection's agent authority is no longer recorded"))?;
    match super::caller(host, &authority).map_err(denied)? {
        (_, Some(session)) => Ok(Mover::Agent { lineage, session }),
        (_, None) => Err(denied(
            "Only a live agent session can move its pane; ask the person to move it",
        )),
    }
}

/// Refuses moving `pane` for an agent that does not control its session: its
/// own session, a terminal it started, or the session or console of an
/// agent it delegated to, as for terminal control. A pane that shows no
/// session, such as a browser or editor, is never an agent's to move.
pub(in crate::host) fn authorize_move(
    host: &SharedHost,
    mover: &Mover,
    pane: &crate::workspace_model::views::Pane,
) -> Result<(), ProtocolError> {
    let Mover::Agent { lineage, .. } = mover else {
        return Ok(());
    };
    let session = match &pane.kind {
        crate::model::CardKind::Chat(session) => Some(session.id),
        crate::model::CardKind::Terminal(session) => Some(session.id),
        _ => None,
    };
    let controlled = match session {
        Some(terminal) => controls(host, *lineage, terminal)
            .map_err(internal)?
            .is_some(),
        None => false,
    };
    if controlled {
        return Ok(());
    }
    Err(denied(format!(
        "Pane {} belongs to another session. An agent moves only its own pane and those of the terminals and agents it started; message that session with `cagents send`, or ask the person to move it",
        pane.id
    )))
}

/// A linked computer's session sends its agent and automation requests on the
/// connection it proved, carrying the capability that proof issued; each is
/// then held to that capability's scope as it is handled. Any other authority
/// on that connection is refused, as on a local session's.
fn own_peer_request(host: &SharedHost, client_id: ClientId, request: &ClientRequest) -> bool {
    let (ClientRequest::Agent { authority, .. } | ClientRequest::Automation { authority, .. }) =
        request
    else {
        return false;
    };
    authority.token().is_some_and(|token| {
        host.lock()
            .unwrap_or_else(|p| p.into_inner())
            .peers
            .issued_to(token, client_id)
    })
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

/// Why a lineage may change a terminal.
enum Control {
    /// It is the lineage's own session, or a terminal it started.
    Own,
    /// It is the session, or the console, of an agent the lineage delegated to.
    Delegated {
        agent: Box<crate::agent_execution::Agent>,
        console: bool,
    },
}

/// Whether `lineage` may change `terminal`, and why: it is that lineage's own
/// session, one it started, or the session or console of an agent it
/// delegated to.
fn controls(
    host: &SharedHost,
    lineage: Lineage,
    terminal: TerminalId,
) -> Result<Option<Control>, String> {
    let registry = registry(host)?;
    let registry = registry.lock().unwrap_or_else(|p| p.into_inner());
    let target = registry
        .session_lineage(terminal)?
        .unwrap_or(Lineage(terminal));
    if target == lineage || registry.terminal_owner(target)? == Some(lineage) {
        return Ok(Some(Control::Own));
    }
    for agent in registry.agents(Some(&Origin::Child { parent: lineage }))? {
        let sessions = agent
            .session
            .as_ref()
            .map(|session| (session.id(), false))
            .into_iter()
            .chain(agent.console.as_ref().map(|console| (console.id, true)));
        for (session, console) in sessions.collect::<Vec<_>>() {
            if session == terminal
                || registry
                    .session_lineage(session)?
                    .unwrap_or(Lineage(session))
                    == target
            {
                return Ok(Some(Control::Delegated {
                    agent: Box::new(agent),
                    console,
                }));
            }
        }
    }
    Ok(None)
}

fn denied(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::PermissionDenied, message, false)
}

fn internal(message: String) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::Internal, message, false)
}
