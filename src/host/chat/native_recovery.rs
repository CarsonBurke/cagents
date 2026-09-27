//! Reattach only authenticated, still-owned provider servers. A saved recipe
//! never authorizes launching a replacement for a lost durable server.
use super::*;
use crate::daemon::holder_registry::{self as registry, Kind};

pub(super) fn restore(
    host: &SharedHost,
    client: ClientId,
    source: &Session,
) -> Result<ServerResponse, String> {
    let proof = {
        let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down || state.chat_spawning.contains_key(&source.id) {
            return Err(
                "Native holder adoption is unavailable while another transition is pending".into(),
            );
        }
        if state
            .chats
            .get(&source.id)
            .is_some_and(|record| record.handle.owns_work())
        {
            return Err("The native conversation already has a live actor; attach to it".into());
        }
        if state.chat_handoffs.contains_key(&source.id)
            || state.chats.values().any(|record| {
                record.handle.owns_work()
                    && same_conversation(&record.session.launch, &source.launch)
            })
            || state
                .chat_spawning
                .values()
                .any(|pending| same_conversation(&pending.launch, &source.launch))
            || (!state.chats.contains_key(&source.id)
                && state.chats.len() + state.chat_spawning.len() >= 128)
        {
            return Err("Native conversation ownership or capacity prevents adoption".into());
        }
        state.chat_spawning.insert(source.id, source.clone());
        state.inventory_generation += 1;
        state.proof().clone()
    };
    let result = restore_reserved(host, client, source, &proof);
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    state.chat_spawning.remove(&source.id);
    state.inventory_generation += 1;
    result
}

fn restore_reserved(
    host: &SharedHost,
    client: ClientId,
    source: &Session,
    proof: &HostSessionProof,
) -> Result<ServerResponse, String> {
    let directory = registry::directory(Kind::Native)?;
    let record = registry::evidence(&registry::socket(&directory, source.id), Kind::Native)?
        .ok_or("The durable native holder is unavailable; this conversation was not restarted or resent")?;
    let (recovery, exited) = chat::native_holder::inspect(&record, proof)?;
    if exited {
        return Err("The retained native server exited; explicitly resume the conversation to start another server".into());
    }
    if &recovery.source != source && !recovery.previous_sources.contains(source) {
        return Err("Native holder does not recognize this exact issued descriptor".into());
    }
    let (mut spec, mut adapter) = chat::protocol::codex::create_restored(&recovery)?;
    spec.environment = recovery.execution_environment.0.clone();
    let mut initial = adapter.initialize()?;
    let store = chat::runtime::preflight(&spec, &mut initial)?;
    let (holder, recovery) = chat::native_holder::adopt(&record, source, proof)?;
    let mut session = recovery.source.clone();
    let expected_thread = recovery.confirmed_thread.clone();
    let mut sources = recovery.previous_sources.clone();
    sources.push(recovery.source.clone());
    let weak = Arc::downgrade(host);
    let id = session.id;
    let handle = Handle::start_adopted(
        spec,
        adapter,
        initial,
        store,
        recovery,
        holder,
        move |update| changed(&weak, id, update),
    )?;
    // Installation is still reversible: close drops only this new connection
    // until the exact server confirms the retained thread and the host owns it.
    let confirmed = (|| {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if crate::daemon::termination_requested()
                || host.lock().unwrap_or_else(|p| p.into_inner()).shutting_down
            {
                return Err("Daemon shutdown interrupted native adoption".into());
            }
            if let Some(error) = handle.failure() {
                return Err(error);
            }
            if handle.is_completed() {
                return Err("Retained server disconnected during adoption".into());
            }
            if let Some(actual) = handle.confirmed_session_id() {
                return if Some(actual) == expected_thread {
                    Ok(())
                } else {
                    Err("Retained server confirmed a different conversation".into())
                };
            }
            if Instant::now() >= deadline {
                return Err("Retained server did not confirm its thread within 30 seconds".into());
            }
            thread::sleep(Duration::from_millis(20));
        }
    })();
    if let Err(error) = confirmed {
        let _ = handle.close();
        return Err(error);
    }
    let paired = (|| {
        let state = host.lock().unwrap_or_else(|p| p.into_inner());
        if state.shutting_down {
            return Err("Daemon shutdown interrupted native adoption".to_string());
        }
        let Some(terminal) = &mut session.terminal else {
            return Ok(None);
        };
        // Terminal recovery has already authenticated its own holder lineage.
        // Keep that exact worker through the native holder's acknowledgment;
        // an equal recipe alone cannot authorize a concurrent replacement.
        let current = state
            .terminals
            .get(&terminal.id)
            .filter(|current| {
                current.session.launch == terminal.launch
                    && current.session.owned_codex_listener == terminal.owned_codex_listener
                    && current.session.continuity.live_proof().as_ref() == Some(proof)
            })
            .ok_or("The paired terminal could not be recovered; native ownership was preserved")?;
        *terminal = current.session.clone();
        Ok(Some(current.handle.clone()))
    })();
    let paired = match paired {
        Ok(paired) => paired,
        Err(error) => {
            let _ = handle.close();
            return Err(error);
        }
    };
    if let Err(error) = handle.update_native_source(session.clone()) {
        let _ = handle.close();
        return Err(error);
    }
    let mut state = host.lock().unwrap_or_else(|p| p.into_inner());
    let pair_unchanged = match (&session.terminal, &paired) {
        (None, None) => true,
        (Some(terminal), Some(handle)) => {
            state.terminals.get(&terminal.id).is_some_and(|current| {
                current.session == *terminal && current.handle.same_worker(handle)
            })
        }
        _ => false,
    };
    if state.shutting_down || !pair_unchanged {
        drop(state);
        let _ = handle.close();
        return Err(
            "Native adoption was interrupted before its paired ownership could be installed".into(),
        );
    }
    let observation = Observation {
        after_generation: handle.observe_next_update(),
        ..Observation::default()
    };
    let record = Record {
        session,
        handle,
        source_process: None,
        subscribers: if client == 0 {
            BTreeMap::new()
        } else {
            BTreeMap::from([(client, false)])
        },
        observations: if client == 0 {
            BTreeMap::new()
        } else {
            BTreeMap::from([(client, observation)])
        },
        completion_observed: false,
        returning_terminal: false,
    };
    let response = session_snapshot(&record);
    record.handle.commit_native_installation();
    let retired = state.chats.insert(id, record);
    state.upgrade_chats.insert(
        id,
        sources
            .into_iter()
            .map(|source| crate::upgrade::handoff::ChatSource {
                launch: source.launch.clone(),
                source,
            })
            .collect(),
    );
    state.inventory_generation += 1;
    drop(state);
    drop(retired);
    Ok(response)
}

pub(in crate::host) fn restore_orphaned_native(host: &SharedHost) -> Result<(), String> {
    let proof = host
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .proof()
        .clone();
    let directory = registry::directory(Kind::Native)?;
    for record in registry::discover(&directory, &proof.boot_id, Kind::Native)? {
        if host
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .chats
            .contains_key(&record.terminal)
        {
            continue;
        }
        let result = chat::native_holder::inspect(&record, &proof)
            .and_then(|(recovery, _)| restore(host, 0, &recovery.source));
        if let Err(error) = result {
            tracing::warn!(session = %record.terminal, %error, "retained native conversation could not be reattached; holder evidence preserved");
        }
    }
    Ok(())
}
