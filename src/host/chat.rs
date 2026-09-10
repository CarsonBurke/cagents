use super::state::{ClientId, HostState, SharedHost};
use crate::{
    chat::{self, Provider, Session, runtime::Handle},
    model::{HostSessionProof, TerminalId},
    terminal_ipc::{
        ClientRequest, PROTOCOL_VERSION, ProtocolError, ProtocolErrorCode, ServerEnvelope,
        ServerMessage, ServerResponse, validate_proof,
    },
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
    thread,
    time::{Duration, Instant},
};

pub(super) struct Record {
    pub session: Session,
    pub handle: Handle,
    pub subscribers: BTreeMap<ClientId, bool>,
    pub completion_observed: bool,
}
fn error(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::InvalidRequest, message, false)
}
fn unavailable(message: impl Into<String>) -> ProtocolError {
    ProtocolError::new(ProtocolErrorCode::TerminalUnavailable, message, false)
}
fn attached(state: &HostState, client: ClientId, id: TerminalId) -> Result<&Record, ProtocolError> {
    let record = state.chats.get(&id).ok_or_else(|| {
        unavailable("Exact native chat is not hosted; explicit recovery is required")
    })?;
    if !record.subscribers.contains_key(&client) {
        return Err(ProtocolError::new(
            ProtocolErrorCode::PermissionDenied,
            "Client is not attached to this chat",
            false,
        ));
    }
    Ok(record)
}
fn changed(host: &Weak<Mutex<HostState>>, id: TerminalId) {
    let Some(host) = host.upgrade() else {
        return;
    };
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let Some(record) = state.chats.get(&id) else {
        return;
    };
    for (&client, &visible) in &record.subscribers {
        if visible && let Some(delivery) = state.clients.get(&client) {
            let _ = delivery.push(ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: None,
                proof: state.proof().clone(),
                message: ServerMessage::ChatChanged { chat_id: id },
            });
        }
    }
}
fn session_snapshot(record: &Record) -> ServerResponse {
    let snapshot = record
        .handle
        .snapshot(None)
        .expect("unconditional snapshot");
    let mut session = record.session.clone();
    session.launch.session_id = snapshot.session_id.clone();
    ServerResponse::ChatAttached { session, snapshot }
}
fn same_conversation(a: &chat::Launch, b: &chat::Launch) -> bool {
    a.provider == b.provider && a.session_id.is_some() && a.session_id == b.session_id
}
fn start(
    host: &SharedHost,
    client: ClientId,
    mut session: Session,
    source: Option<HostSessionProof>,
    terminate_source: bool,
) -> Result<ServerResponse, ProtocolError> {
    let reservation_id = session.id;
    // A Codex TUI remains live while its native view is detached. Switching
    // that proven source back is an attachment, never a second daemon writer.
    if source.is_some() && !terminate_source && session.launch.provider == Provider::Codex {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
            return Err(unavailable("Daemon is shutting down"));
        }
        validate_proof(source.as_ref(), state.proof())?;
        if let Some(record) = state.chats.get_mut(&session.id)
            && record.handle.owns_work()
        {
            if record.session.launch.provider != session.launch.provider
                || record.session.launch.recipe != session.launch.recipe
                || record.handle.session_id() != session.launch.session_id
                || record.session.terminal.as_ref().map(|terminal| terminal.id) != Some(session.id)
            {
                return Err(error(
                    "Retained chat does not match this exact Codex source",
                ));
            }
            record.subscribers.insert(client, false);
            return Ok(session_snapshot(record));
        }
    }
    // All adapter validation happens before a source terminal can be changed.
    let (spec, mut adapter) = chat::protocol::create(&session.launch).map_err(error)?;
    let mut initial = adapter.initialize().map_err(error)?;
    let store = chat::runtime::preflight(&spec, &mut initial).map_err(error)?;
    if terminate_source && source.is_none() {
        return Err(error("Source termination requires an exact host proof"));
    }
    let mut retired = None;
    let source_handle = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
            return Err(unavailable("Daemon is shutting down"));
        }
        let recovering = if let Some(record) = state.chats.get(&session.id) {
            if record.handle.owns_work() {
                return Err(error(
                    "Chat already owns work; attach instead of starting another writer",
                ));
            }
            validate_proof(session.proof.as_ref(), state.proof())?;
            if record.session.launch.provider != session.launch.provider
                || record.session.launch.recipe != session.launch.recipe
                || record.handle.session_id() != session.launch.session_id
            {
                return Err(error(
                    "Explicit recovery must retain the exact closed chat identity",
                ));
            }
            true
        } else {
            false
        };
        if !recovering && state.chats.len() + state.chat_spawning.len() >= 128 {
            return Err(unavailable("Daemon native chat capacity reached"));
        }
        if state.chat_spawning.contains_key(&session.id) {
            return Err(error("Chat is already starting"));
        }
        for record in state.chats.values() {
            let mut launch = record.session.launch.clone();
            launch.session_id = record.handle.session_id();
            if same_conversation(&launch, &session.launch) && record.handle.owns_work() {
                return Err(error(
                    "This exact conversation already has a daemon writer or unconfirmed backend work; attach to its existing chat",
                ));
            }
        }
        if state
            .chat_spawning
            .values()
            .any(|launch| same_conversation(launch, &session.launch))
        {
            return Err(error("This exact conversation is already starting"));
        }
        if let Some(id) = &session.launch.session_id {
            for (&terminal_id, record) in &state.terminals {
                if record.handle.is_completed() || (source.is_some() && terminal_id == session.id) {
                    continue;
                }
                let hint = record
                    .session
                    .resume_hint
                    .as_deref()
                    .and_then(crate::model::agent_recovery::observed_launch_from_hint);
                let recipe = hint.as_ref().unwrap_or(&record.session.launch);
                if recipe.arguments.iter().any(|arg| {
                    arg == id || arg.split_once('=').is_some_and(|(_, value)| value == id)
                }) {
                    return Err(error(format!(
                        "The exact conversation is already owned by CLI terminal {terminal_id}; convert that proven source rather than launching another writer"
                    )));
                }
            }
        }
        let source_handle = if let Some(source) = source.as_ref() {
            validate_proof(Some(source), state.proof())?;
            let record = state.terminals.get(&session.id).ok_or_else(|| {
                error("Exact source terminal does not exist on the proven daemon")
            })?;
            let hint = record
                .session
                .resume_hint
                .as_deref()
                .and_then(crate::model::agent_recovery::observed_launch_from_hint);
            let recipe = hint.as_ref().unwrap_or(&record.session.launch);
            if recipe != &session.launch.recipe {
                return Err(error(
                    "Source launch identity changed; refresh the exact recovery evidence before converting",
                ));
            }
            let id = session
                .launch
                .session_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .ok_or_else(|| error("Conversion requires an exact provider conversation ID"))?;
            if let Some(exact) = crate::model::agent_recovery::resume_recipe(recipe) {
                let proven = exact
                    .arguments
                    .windows(2)
                    .find(|pair| pair[0] == "--resume" || pair[0] == "resume")
                    .map(|pair| pair[1].as_str());
                if proven != Some(id) {
                    return Err(error(
                        "Requested conversation differs from the source's exact resume identity",
                    ));
                }
            } else if id.len() != 36
                || uuid::Uuid::parse_str(id).is_err()
                || (session.launch.provider != Provider::Codex && !terminate_source)
            {
                return Err(error(
                    "Fresh observed agents require an explicitly chosen full conversation UUID and confirmed CLI stop (Codex same-server join does not stop its TUI)",
                ));
            }
            if session.launch.provider != Provider::Codex
                && !terminate_source
                && !record.handle.is_completed()
            {
                return Err(error(
                    "This provider requires explicit confirmation to stop the CLI before exact native resume",
                ));
            }
            let handle = record.handle.clone();
            session.terminal = Some(record.session.clone());
            Some(handle)
        } else {
            if state.terminals.contains_key(&session.id) && !recovering {
                return Err(error(
                    "Existing source terminal requires exact host proof; it will not be replaced implicitly",
                ));
            }
            None
        };
        if recovering {
            retired = state.chats.remove(&session.id);
        }
        session.proof = Some(state.proof().clone());
        state
            .chat_spawning
            .insert(session.id, session.launch.clone());
        state.inventory_generation += 1;
        source_handle
    };
    // Joining a completed actor must never happen while holding the host lock:
    // its final notification may still be acquiring that same lock.
    drop(retired);
    let result = (|| {
        if let Some(handle) = source_handle {
            if session.launch.provider != Provider::Codex && !handle.is_completed() {
                handle.shutdown().map_err(super::state::send_error)?;
                let deadline = Instant::now() + Duration::from_secs(10);
                while !handle.is_completed() {
                    if Instant::now() >= deadline {
                        return Err(unavailable(
                            "CLI shutdown did not complete in ten seconds. Source evidence is retained; no native writer was started",
                        ));
                    }
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
        {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            if state.shutting_down {
                return Err(unavailable(
                    "Daemon shutdown cancelled chat start; exact source/session evidence remains recoverable",
                ));
            }
        }
        let weak = Arc::downgrade(host);
        let id = session.id;
        let handle = Handle::start(
            spec,
            adapter,
            initial,
            store,
            session.launch.session_id.clone(),
            move || changed(&weak, id),
        )
        .map_err(unavailable)?;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
            drop(state);
            let _ = handle.close();
            return Err(unavailable("Daemon shutdown cancelled chat start"));
        }
        let record = Record {
            session,
            handle,
            subscribers: BTreeMap::from([(client, false)]),
            completion_observed: false,
        };
        let response = session_snapshot(&record);
        state.chats.insert(id, record);
        state.inventory_generation += 1;
        Ok(response)
    })();
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    state.chat_spawning.remove(&reservation_id);
    state.inventory_generation += 1;
    result
}

pub(super) fn process(
    host: &SharedHost,
    client: ClientId,
    request: ClientRequest,
) -> Result<ServerResponse, ProtocolError> {
    match request {
        ClientRequest::ChatList => {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let mut sessions = state
                .chats
                .values()
                .filter(|record| record.handle.owns_work())
                .map(|record| {
                    let mut session = record.session.clone();
                    session.launch.session_id = record.handle.session_id();
                    session
                })
                .collect::<Vec<_>>();
            sessions.extend(state.chat_spawning.iter().map(|(&id, launch)| Session {
                id,
                launch: launch.clone(),
                proof: Some(state.proof().clone()),
                terminal: None,
            }));
            Ok(ServerResponse::ChatListed { sessions })
        }
        ClientRequest::ChatStart {
            session,
            source,
            terminate_source,
        } => start(host, client, session, source, terminate_source),
        ClientRequest::ChatAttach { session } => {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            validate_proof(session.proof.as_ref(), state.proof())?;
            let record = state.chats.get_mut(&session.id).ok_or_else(|| {
                unavailable("Exact native chat not found; no replacement process was spawned")
            })?;
            let actual_id = record.handle.session_id();
            if record.session.launch.provider != session.launch.provider
                || record.session.launch.recipe != session.launch.recipe
                || (session.launch.session_id.is_some() && session.launch.session_id != actual_id)
            {
                return Err(error(
                    "Native chat recovery identity does not match the hosted conversation",
                ));
            }
            record.subscribers.entry(client).or_insert(false);
            Ok(session_snapshot(record))
        }
        ClientRequest::ChatDetach { chat_id } => {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            attached(&state, client, chat_id)?;
            let record = state.chats.get_mut(&chat_id).expect("checked chat");
            record.subscribers.remove(&client);
            record
                .handle
                .set_visible(record.subscribers.values().any(|visible| *visible));
            Ok(ServerResponse::Detached)
        }
        ClientRequest::ChatCommand { chat_id, command } => {
            let handle = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                if state.shutting_down {
                    return Err(unavailable("Daemon is shutting down"));
                }
                attached(&state, client, chat_id)?.handle.clone()
            };
            handle.command(command).map_err(unavailable)?;
            Ok(ServerResponse::Accepted)
        }
        ClientRequest::ChatVisibility { chat_id, visible } => {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            attached(&state, client, chat_id)?;
            let record = state.chats.get_mut(&chat_id).expect("checked chat");
            record.subscribers.insert(client, visible);
            record
                .handle
                .set_visible(record.subscribers.values().any(|visible| *visible));
            drop(state);
            if visible {
                changed(&Arc::downgrade(host), chat_id);
            }
            Ok(ServerResponse::Accepted)
        }
        ClientRequest::ChatRead {
            chat_id,
            after_generation,
        } => {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            let record = attached(&state, client, chat_id)?;
            if !record.subscribers.get(&client).copied().unwrap_or(false) {
                return Err(error(
                    "Hidden chat has no presentation stream; make it visible before reading",
                ));
            }
            Ok(ServerResponse::ChatSnapshot {
                chat_id,
                snapshot: record.handle.snapshot(after_generation),
            })
        }
        ClientRequest::ChatClose { chat_id } => {
            let handle = {
                let state = host.lock().unwrap_or_else(|p| p.into_inner());
                attached(&state, client, chat_id)?.handle.clone()
            };
            handle.close().map_err(unavailable)?;
            let snapshot = handle.snapshot(None).expect("unconditional final snapshot");
            let record = {
                let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
                state.inventory_generation += 1;
                state.chats.remove(&chat_id)
            };
            drop(record);
            Ok(ServerResponse::ChatClosed { chat_id, snapshot })
        }
        _ => Err(error("Not a native chat request")),
    }
}
