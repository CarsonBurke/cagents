use super::state::{ClientId, HostState, SharedHost};
use crate::{
    agent::Provider,
    chat::{
        self, Session,
        runtime::{Handle, Update},
    },
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
    pub(super) observations: BTreeMap<ClientId, Observation>,
    pub completion_observed: bool,
}

#[derive(Default)]
pub(super) struct Observation {
    epoch: u64,
    after_generation: u64,
    notified: bool,
}

impl Observation {
    fn acknowledge(&mut self, epoch: u64, generation: u64) {
        if epoch > self.epoch {
            self.epoch = epoch;
            self.after_generation = generation;
            self.notified = false;
        }
    }

    fn accepts(&self, generation: u64) -> bool {
        !self.notified && generation > self.after_generation
    }
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
fn changed(host: &Weak<Mutex<HostState>>, id: TerminalId, update: Update) {
    let Some(host) = host.upgrade() else {
        return;
    };
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let proof = state.proof().clone();
    let HostState { chats, clients, .. } = &mut *state;
    let Some(record) = chats.get_mut(&id) else {
        return;
    };
    for (&client, &visible) in &record.subscribers {
        let message = match update {
            Update::Activity { state, generation } => ServerMessage::ChatActivity {
                chat_id: id,
                state,
                generation,
            },
            Update::Presentation if visible => ServerMessage::ChatChanged { chat_id: id },
            Update::Observed { generation } if !visible => {
                let observation = record.observations.entry(client).or_default();
                if !observation.accepts(generation) {
                    continue;
                }
                ServerMessage::ChatObserved {
                    chat_id: id,
                    epoch: observation.epoch,
                }
            }
            _ => continue,
        };
        if let Some(delivery) = clients.get(&client) {
            let delivered = delivery.push(ServerEnvelope {
                version: PROTOCOL_VERSION,
                request_id: None,
                proof: proof.clone(),
                message,
            });
            if matches!(update, Update::Observed { .. }) && delivered.is_ok() {
                record.observations.entry(client).or_default().notified = true;
            }
        }
    }
    // An old producer callback can consume the shared rearm after an ACK but
    // arrive here with a pre-ACK generation. Keep waiting subscribers armed;
    // only successful delivery in their own epoch coalesces future output.
    if matches!(update, Update::Observed { .. })
        && record.subscribers.iter().any(|(client, visible)| {
            !visible
                && record
                    .observations
                    .get(client)
                    .is_none_or(|observation| !observation.notified)
        })
    {
        record.handle.observe_next_update();
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
    if let Some(proof) = source.as_ref() {
        let handle = {
            let state = host.lock().unwrap_or_else(|p| p.into_inner());
            validate_proof(Some(proof), state.proof())?;
            let record = state
                .terminals
                .get(&session.id)
                .ok_or_else(|| error("The proven source terminal no longer exists"))?;
            if record.handle.is_completed() {
                return Err(unavailable(
                    "The source agent has exited; resume its exact conversation in the terminal first",
                ));
            }
            record.handle.clone()
        };
        // ChatStart already runs off the UI thread. Ask the PTY owner for fresh
        // evidence, rather than trusting a saved hint or a user-entered UUID.
        let deadline = Instant::now() + Duration::from_secs(2);
        let (hint, recipe) = loop {
            let hint = handle.observe_agent().map_err(unavailable)?;
            if let Some(recipe) = hint
                .as_deref()
                .and_then(crate::model::agent_recovery::recipe_from_hint)
            {
                break (hint, recipe);
            }
            if Instant::now() >= deadline || handle.is_completed() {
                return Err(unavailable(
                    "The active agent has no process-bound conversation identity. Resume this exact conversation in the provider CLI, then retry. Shared Codex servers do not expose a TUI-to-thread binding; no recent session or private backend is selected.",
                ));
            }
            thread::sleep(Duration::from_millis(100));
        };
        let provider = Provider::from_program(&recipe.program)
            .ok_or_else(|| error("The foreground process is not a supported agent"))?;
        let id = recipe
            .arguments
            .windows(2)
            .find(|pair| pair[0] == provider.resume_flag())
            .map(|pair| pair[1].clone())
            .ok_or_else(|| error("Missing process-bound conversation identity"))?;
        if provider != session.launch.provider
            || session
                .launch
                .session_id
                .as_ref()
                .is_some_and(|expected| expected != &id)
        {
            return Err(error(
                "The active conversation changed before the handoff; retry from the current terminal",
            ));
        }
        session.launch.recipe = recipe;
        session.launch.session_id = Some(id);
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        validate_proof(Some(proof), state.proof())?;
        let record = state
            .terminals
            .get_mut(&session.id)
            .ok_or_else(|| error("The proven source terminal no longer exists"))?;
        record.session.resume_hint = hint;
    }
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
            if let std::collections::btree_map::Entry::Vacant(entry) =
                record.subscribers.entry(client)
            {
                entry.insert(false);
                record.observations.insert(
                    client,
                    Observation {
                        after_generation: record.handle.observe_next_update(),
                        ..Observation::default()
                    },
                );
            }
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
                    .find(|pair| pair[0] == session.launch.provider.resume_flag())
                    .map(|pair| pair[1].as_str());
                if proven != Some(id) {
                    return Err(error(
                        "Requested conversation differs from the source's exact resume identity",
                    ));
                }
            } else {
                return Err(error(
                    "The source no longer has an exact process-bound conversation identity",
                ));
            }
            if session.launch.provider != Provider::Codex
                && !terminate_source
                && !record.handle.is_completed()
            {
                return Err(error(
                    "This provider requires stopping the CLI before exact native resume",
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
            let current = handle
                .observe_agent()
                .map_err(unavailable)?
                .as_deref()
                .and_then(crate::model::agent_recovery::recipe_from_hint);
            if current.as_ref() != Some(&session.launch.recipe) {
                return Err(error(
                    "The foreground conversation changed during handoff; no native writer was started",
                ));
            }
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
            move |update| changed(&weak, id, update),
        )
        .map_err(unavailable)?;
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
            drop(state);
            let _ = handle.close();
            return Err(unavailable("Daemon shutdown cancelled chat start"));
        }
        let observation = Observation {
            after_generation: handle.observe_next_update(),
            ..Observation::default()
        };
        let record = Record {
            session,
            handle,
            subscribers: BTreeMap::from([(client, false)]),
            observations: BTreeMap::from([(client, observation)]),
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
            if let std::collections::btree_map::Entry::Vacant(entry) =
                record.subscribers.entry(client)
            {
                entry.insert(false);
                record.observations.insert(
                    client,
                    Observation {
                        after_generation: record.handle.observe_next_update(),
                        ..Observation::default()
                    },
                );
            }
            Ok(session_snapshot(record))
        }
        ClientRequest::ChatDetach { chat_id } => {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            attached(&state, client, chat_id)?;
            let record = state.chats.get_mut(&chat_id).expect("checked chat");
            record.subscribers.remove(&client);
            record.observations.remove(&client);
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
        ClientRequest::ChatAcknowledge { chat_id, epoch } => {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            attached(&state, client, chat_id)?;
            let record = state.chats.get_mut(&chat_id).expect("checked chat");
            let observation = record.observations.entry(client).or_default();
            if epoch > observation.epoch {
                observation.acknowledge(epoch, record.handle.observe_next_update());
            }
            Ok(ServerResponse::Accepted)
        }
        ClientRequest::ChatVisibility { chat_id, visible } => {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            attached(&state, client, chat_id)?;
            let record = state.chats.get_mut(&chat_id).expect("checked chat");
            let previous = record.subscribers.insert(client, visible);
            if previous != Some(visible) && !visible {
                record.handle.observe_next_update();
            }
            record
                .handle
                .set_visible(record.subscribers.values().any(|visible| *visible));
            drop(state);
            if visible {
                changed(&Arc::downgrade(host), chat_id, Update::Presentation);
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

pub(super) fn resolve_upgrade(
    host: &SharedHost,
    source: &Session,
) -> Result<Session, ProtocolError> {
    let state = host.lock().unwrap_or_else(|p| p.into_inner());
    let sources = state
        .upgrade_chats
        .get(&source.id)
        .ok_or_else(|| unavailable("No durable upgrade replacement exists for this native chat"))?;
    if !sources.iter().any(|previous| {
        let saved = &previous.source;
        !(source.proof.is_none()
        || saved.proof != source.proof
        // Presentation identities can be stale on hidden cards. The old proof,
        // actor ID and immutable launch bind that descriptor to this lineage;
        // only the daemon's checkpoint chooses the replacement conversation.
        || saved.launch.provider != source.launch.provider
        || saved.launch.recipe != source.launch.recipe
        || saved.terminal.as_ref().map(|terminal| {
            (
                &terminal.id,
                &terminal.launch,
                terminal.continuity.live_proof(),
            )
        }) != source.terminal.as_ref().map(|terminal| {
            (
                &terminal.id,
                &terminal.launch,
                terminal.continuity.live_proof(),
            )
        }))
    }) {
        return Err(error(
            "Native upgrade source does not match the exact former daemon conversation",
        ));
    }
    let record = state
        .chats
        .get(&source.id)
        .ok_or_else(|| unavailable("Restored native chat is no longer hosted"))?;
    if !record.handle.owns_work() {
        return Err(unavailable(
            "Restored native provider has closed; no writer was restarted",
        ));
    }
    let mut session = record.session.clone();
    session.launch.session_id = record.handle.session_id();
    Ok(session)
}

pub(super) fn restore_upgrade(
    host: &SharedHost,
    saved: &crate::upgrade::handoff::Chat,
) -> Result<(), String> {
    let mut session = saved.source.clone();
    session.launch = saved.launch.clone();
    let source = {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        session.proof = Some(state.proof().clone());
        if let Some(terminal) = session.terminal.as_mut() {
            if state.terminals.contains_key(&terminal.id) {
                *terminal = state
                    .descriptor(0, terminal.id)
                    .map_err(|error| error.message)?
                    .session;
            } else {
                terminal.continuity = crate::model::TerminalContinuity::RelaunchAvailable {
                    recipe: session
                        .launch
                        .terminal_recipe()
                        .ok_or("Native exact recipe unavailable")?,
                    last_exit: None,
                };
            }
        }
        state
            .terminals
            .contains_key(&session.id)
            .then(|| state.proof().clone())
    };
    let response = start(host, 0, session, source, false).map_err(|error| error.message)?;
    let id = match response {
        ServerResponse::ChatAttached { session, .. } => session.id,
        _ => return Err("Native restore did not attach its exact session".into()),
    };
    let handle = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .chats
        .get(&id)
        .expect("restored chat")
        .handle
        .clone();
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let snapshot = handle.snapshot(None).expect("unconditional snapshot");
        if let Some(error) = snapshot.error {
            return Err(error);
        }
        if handle.is_completed() {
            return Err(
                "Restored native provider exited before confirming its exact conversation".into(),
            );
        }
        if let Some(actual) = handle.confirmed_session_id() {
            if Some(&actual) != saved.launch.session_id.as_ref() {
                return Err(
                    "Restored provider confirmed a different conversation; checkpoint retained"
                        .into(),
                );
            }
            break;
        }
        if Instant::now() >= deadline {
            return Err(
                "Native provider did not confirm its exact conversation within 30 seconds".into(),
            );
        }
        thread::sleep(Duration::from_millis(20));
    }
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let record = state
        .chats
        .get_mut(&saved.source.id)
        .expect("restored chat");
    record.subscribers.remove(&0);
    record.observations.remove(&0);
    let mut sources = saved.previous_sources.clone();
    sources.push(crate::upgrade::handoff::ChatSource {
        source: saved.source.clone(),
        launch: saved.launch.clone(),
    });
    state.upgrade_chats.insert(saved.source.id, sources);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        chat::{
            Command, Event,
            protocol::{Adapter, ProcessSpec, Step},
        },
        model::{BootId, HostIncarnation, HostSessionId, LaunchRecipe},
    };
    use serde_json::{Value, json};

    fn upgrade_fixture() -> (tempfile::TempDir, SharedHost, crate::upgrade::handoff::Chat) {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let executable = directory.path().join("omp");
        std::fs::write(&executable, r#"#!/usr/bin/env python3
import json, os, sys
identity = sys.argv[sys.argv.index("--resume") + 1]
print(json.dumps({"type": "ready"}), flush=True)
for line in sys.stdin:
    request = json.loads(line)
    command = request["type"]
    if command not in ("negotiate_protocol", "get_state", "get_available_models", "get_available_commands", "get_session_stats", "get_messages_page"):
        sys.exit(7)
    data = {"sessionId": os.environ.get("REPORTED_SESSION", identity), "messages": []}
    print(json.dumps({"type": "response", "id": request["id"], "command": command, "success": True, "data": data}), flush=True)
"#).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let old = HostSessionProof {
            host_session_id: HostSessionId::new("old-session"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("old-incarnation"),
        };
        let current = HostSessionProof {
            host_session_id: HostSessionId::new("new-session"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("new-incarnation"),
        };
        let launch = chat::Launch {
            provider: Provider::Omp,
            recipe: LaunchRecipe {
                program: executable.to_string_lossy().into_owned(),
                arguments: Vec::new(),
                working_directory: directory.path().to_owned(),
                environment: BTreeMap::new(),
            },
            session_id: Some("12345678-1234-4234-8234-123456789abc".into()),
        };
        let source = Session {
            id: TerminalId::new(),
            launch: launch.clone(),
            proof: Some(old),
            terminal: None,
        };
        (
            directory,
            Arc::new(Mutex::new(HostState::new(current))),
            crate::upgrade::handoff::Chat {
                source,
                launch,
                previous_sources: Vec::new(),
            },
        )
    }

    #[test]
    fn native_upgrade_restores_exact_provider_session_and_only_resolves_proven_source() {
        let (_directory, host, saved) = upgrade_fixture();
        restore_upgrade(&host, &saved).unwrap();
        let restored = resolve_upgrade(&host, &saved.source).unwrap();
        assert_eq!(restored.id, saved.source.id);
        assert_eq!(restored.launch.session_id, saved.launch.session_id);
        assert_eq!(
            restored.proof.as_ref(),
            Some(host.lock().unwrap_or_else(|p| p.into_inner()).proof())
        );
        assert_ne!(restored.proof, saved.source.proof);
        let mut wrong_source = saved.source.clone();
        wrong_source.proof = restored.proof;
        assert!(resolve_upgrade(&host, &wrong_source).is_err());
        assert!(restore_upgrade(&host, &saved).is_err());
        let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&saved.source.id]
            .handle
            .clone();
        handle.close().unwrap();
    }

    #[test]
    fn native_upgrade_rejects_provider_substituting_another_conversation() {
        let (_directory, host, mut saved) = upgrade_fixture();
        saved.launch.recipe.environment.insert(
            "REPORTED_SESSION".into(),
            "98765432-1234-4234-8234-123456789abc".into(),
        );
        assert!(restore_upgrade(&host, &saved).is_err());
        assert!(resolve_upgrade(&host, &saved.source).is_err());
        let handle = host.lock().unwrap_or_else(|p| p.into_inner()).chats[&saved.source.id]
            .handle
            .clone();
        assert!(handle.close().is_err());
    }

    struct Echo(bool);
    impl Adapter for Echo {
        fn initialize(&mut self) -> Result<Step, String> {
            Ok(Step::default())
        }
        fn command(&mut self, _: Command) -> Result<Step, String> {
            Ok(Step {
                writes: vec![json!({"tick": true})],
                events: Vec::new(),
            })
        }
        fn receive(&mut self, _: Value) -> Result<Step, String> {
            self.0 = !self.0;
            Ok(Step {
                writes: Vec::new(),
                events: vec![Event::Waiting(self.0)],
            })
        }
    }

    #[test]
    fn delayed_producer_callback_after_rearm_is_fenced_without_reads() {
        acknowledgement_race(false);
    }

    #[test]
    fn delayed_producer_callback_before_rearm_cannot_consume_new_acknowledgement() {
        acknowledgement_race(true);
    }

    fn acknowledgement_race(before_rearm: bool) {
        let proof = HostSessionProof {
            host_session_id: HostSessionId::new("chat-observation"),
            boot_id: BootId::new("boot"),
            host_incarnation: HostIncarnation::new("incarnation"),
        };
        let host = Arc::new(Mutex::new(HostState::new(proof.clone())));
        let chat_id = TerminalId::new();
        let recipe = LaunchRecipe {
            program: "/bin/cat".into(),
            arguments: Vec::new(),
            working_directory: std::env::temp_dir(),
            environment: BTreeMap::new(),
        };
        let spec = ProcessSpec::from(&recipe);
        let mut initial = Step::default();
        let store = chat::runtime::preflight(&spec, &mut initial).unwrap();
        let (observed, pending) = crossbeam_channel::bounded(1);
        let (release, released) = crossbeam_channel::bounded(1);
        let (delivered, completed) = crossbeam_channel::bounded(1);
        let weak = Arc::downgrade(&host);
        let handle = Handle::start(
            spec,
            Box::new(Echo(false)),
            initial,
            store,
            None,
            move |update| {
                let generation = match update {
                    Update::Activity { generation, .. } if before_rearm => Some(generation),
                    Update::Observed { generation } if !before_rearm => Some(generation),
                    _ => None,
                };
                if let Some(generation) = generation {
                    // Pause on either side of consuming the producer rearm, always
                    // after the semantic event and before its unread host callback.
                    if observed.send(generation).is_ok() {
                        let _ = released.recv_timeout(Duration::from_secs(5));
                    }
                }
                changed(&weak, chat_id, update);
                if matches!(update, Update::Observed { .. }) {
                    let _ = delivered.try_send(());
                }
            },
        )
        .unwrap();
        let (first, second) = {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            let first = state.register_client(1);
            let second = state.register_client(2);
            state.chats.insert(
                chat_id,
                Record {
                    session: Session {
                        id: chat_id,
                        launch: chat::Launch {
                            provider: Provider::Claude,
                            recipe,
                            session_id: None,
                        },
                        proof: Some(proof),
                        terminal: None,
                    },
                    handle: handle.clone(),
                    subscribers: BTreeMap::from([(1, false), (2, false)]),
                    observations: BTreeMap::from([
                        (1, Observation::default()),
                        (2, Observation::default()),
                    ]),
                    completion_observed: false,
                },
            );
            (first, second)
        };
        handle.command(Command::Interrupt).unwrap();
        let old_generation = pending.recv_timeout(Duration::from_secs(2)).unwrap();
        // This must not deadlock on the producer's snapshot lock or hydrate any
        // transcript. Only subscriber 1 acknowledges the already-produced event.
        process(
            &host,
            1,
            ClientRequest::ChatAcknowledge { chat_id, epoch: 1 },
        )
        .unwrap();
        release.send(()).unwrap();
        completed.recv_timeout(Duration::from_secs(2)).unwrap();
        handle.command(Command::Interrupt).unwrap();
        let fresh_generation = pending.recv_timeout(Duration::from_secs(2)).unwrap();
        assert!(fresh_generation > old_generation);
        // Reordered and duplicate ACKs cannot advance the source cutoff to this
        // fresh event, nor clear a subscriber's already-delivered observation.
        for epoch in [1, 0] {
            process(&host, 1, ClientRequest::ChatAcknowledge { chat_id, epoch }).unwrap();
        }
        release.send(()).unwrap();
        completed.recv_timeout(Duration::from_secs(2)).unwrap();
        for epoch in [1, 0] {
            process(&host, 1, ClientRequest::ChatAcknowledge { chat_id, epoch }).unwrap();
        }
        changed(
            &Arc::downgrade(&host),
            chat_id,
            Update::Observed {
                generation: fresh_generation + 1,
            },
        );
        // Closing the queues makes missing or surplus observations deterministic.
        first.close_after_drain();
        second.close_after_drain();
        let drain = |queue: &super::super::delivery::DeliveryQueue| {
            let mut epochs = Vec::new();
            while let Some(envelope) = queue.pop() {
                match envelope.message {
                    ServerMessage::ChatObserved { epoch, .. } => epochs.push(epoch),
                    ServerMessage::ChatActivity { .. } => {}
                    other => panic!("hidden acknowledgement requested presentation: {other:?}"),
                }
            }
            epochs
        };
        assert_eq!(
            drain(&first),
            vec![1],
            "old callback or duplicate ACK resurrected unread"
        );
        assert_eq!(
            drain(&second),
            vec![0],
            "one subscriber's ACK must not rearm another"
        );
        assert!(
            process(
                &host,
                1,
                ClientRequest::ChatRead {
                    chat_id,
                    after_generation: None
                }
            )
            .is_err()
        );
        process(&host, 1, ClientRequest::ChatDetach { chat_id }).unwrap();
        {
            let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
            state.unregister_client(2);
            let record = state.chats.get(&chat_id).unwrap();
            assert!(record.subscribers.is_empty());
            assert!(record.observations.is_empty());
        }
        drop(pending);
        drop(release);
        handle.close().unwrap();
    }
}
